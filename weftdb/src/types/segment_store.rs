//! On-disk Storage v2 segment store (roadmap **Phase 4.3**).
//!
//! This closes the Phase-4.3 loop. The pieces were in place: `weft-physical-type`
//! seals a batch into a typed columnar [`Segment`] under a declared
//! [`AspectSchema`] (no silent downcast — hard constraint #4), frames it to a
//! checksummed `.weftseg` byte layout, and describes it with a
//! [`SegmentDescriptor`]; [`SegmentIndexStore`](crate::SegmentIndexStore) persists
//! those descriptors in the libSQL control plane and prunes a query to the segments
//! it must open. [`SegmentStore`] wires them together against the filesystem:
//!
//! - **seal** ([`SegmentStore::seal`] / [`seal_nullable`](SegmentStore::seal_nullable))
//!   encodes a batch under the aspect's schema, writes the sealed `.weftseg` frame to
//!   `segments/<aspect>-<id>.weftseg`, and records its descriptor (with the *realized*
//!   on-disk byte length and path) in the index — one atomic-feeling operation that
//!   leaves a measurement segment on disk and a catalog row pointing at it.
//! - **read** ([`SegmentStore::read_time_range`]) prunes the index by time *first*
//!   (a SQL `WHERE` over min/max ts — no `.weftseg` touched), then opens **only** the
//!   selected files, decodes them, and keeps the rows inside the window. A bounded
//!   range query over a long-lived aspect reads a few segment files, not all of them
//!   — the realized payoff of every per-segment stat the earlier slices built.
//!
//! Boundary (hard constraint #3): the control plane holds *metadata* (the index DB);
//! the measurement bytes live in the `.weftseg` files WeftDB owns. libSQL never stores a
//! measurement. This slice seals single-block segments via
//! [`AspectSchema::seal`] **and** paged segments via
//! [`AspectSchema::seal_paged`] — the read path dispatches on the recorded frame
//! version, so a paged segment skips pages *within* the file too. The
//! catalog/`metadata.db` registry is a later slice.
//!
//! Aspect names become file names, so every method that takes one and declares, seals,
//! reads or maintains it first checks it with [`aspect_name::validate`] and fails with an
//! [`InvalidAspectName`](crate::InvalidAspectName) for a name that could point a path
//! outside `segments/`. The path builder also checks that each frame path it returns is a
//! direct child of `segments/`.
//!
//! **Concurrency** (docs/design/crash-consistency.md section 5.1, slice S7). Each aspect
//! has two locks. Its *commit* lock is held around every control-plane transaction of the
//! aspect and guards its id allocator: a seal takes its id from the allocator, writes its
//! frame with the lock released, then inserts its row (a plain `INSERT`, which cannot
//! replace another), raises the persisted allocator (`aspect_seq`) and folds the rollup
//! under it again; maintenance row rewrites, member deletes and rollup rebuilds run under
//! it too. So ids are never shared or reissued (not after a delete, a crash or a restart),
//! and no rollup update is lost to a concurrent one. Its *maintenance* lock is held for a
//! whole maintenance operation, so two never interleave on one aspect. A per-aspect
//! maintenance entry point waits for it up to [`SegmentStore::maintenance_wait`] and then
//! fails with [`MaintenanceBusy`]; a store-wide sweep skips or waits for a busy aspect as
//! its [`MaintenanceWait`] says.

use std::{
	collections::HashMap, ffi::OsStr, future::Future, path::{Component, Path, PathBuf}, sync::{
		atomic::{AtomicBool, AtomicU64, Ordering}, OnceLock
	}, time::Duration
};

use anyhow::{bail, Context, Result};
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use splimes::{Point, Resolution};
use weft_physical_type::{merge_newer_wins, split_index, AspectSchema, FrameOptions, PagedSegment, Segment, SegmentDescriptor, SplitDecision, SplitPolicy, TimeUnit, PAGED_SEGMENT_FORMAT_VERSION};
use weft_reduce::{Aggregation, Bucket, PartialReduction};

use crate::{
	aspect_name, types::{
		aspect_locks::{AspectLocks, AspectState, CommitGuard, MaintGuard}, durable::{
			create_dir_all_durable, fault::{self, FaultPoint}, LockHolder, RealFs, RootLock, RootLockError, StoreFs, SyncPolicy, WritePoints
		}, index_txn::{IndexOp, IndexRow, IndexTxn, IndexTxnError, TxnApplied, TxnErrorKind}
	}, AspectCatalog, AspectMetadata, AspectMetadataStore, CatalogStore, PartialSidecar, PartialSidecarPolicy, SegmentIndexStore, SnapshotReport, SIDECAR_AGGREGATIONS
};

/// The `(database, subject)` namespace a [`SegmentStore`] opened with the plain
/// [`open`](SegmentStore::open) constructor declares its aspect schemas under.
const DEFAULT_DATABASE: &str = "default";
/// See [`DEFAULT_DATABASE`].
const DEFAULT_SUBJECT: &str = "default";

/// A filesystem-backed store of sealed `.weftseg` segments with a libSQL segment
/// index over them.
///
/// Open one with [`SegmentStore::open`] (it lays out `segments/` and a
/// `segment_index.db` under the given root); seal batches with
/// [`seal`](SegmentStore::seal); read a time range with
/// [`read_time_range`](SegmentStore::read_time_range).
/// When a freshly sealed segment should carry a **persisted timestamp checkpoint index**.
///
/// The index makes a point lookup on a *sorted, irregular* column resume from the nearest
/// checkpoint instead of decoding the whole timestamp column — measured **~3.45× faster**
/// on a 100k-row single-block frame for **+0.41% bytes** (`weft-physical-type`'s
/// `benches/dodsearch.rs`). It is a **storage-for-latency trade**, so it is **off by
/// default**: `DISABLED` writes byte-for-byte the frames WeftDB has always written.
///
/// Enable per-deployment with `WEFT_SEGMENT_CHECKPOINT_STRIDE` (rows between checkpoints;
/// ~1024 is the sweet spot — stride barely moves the speed but does move the size) and
/// optionally `WEFT_SEGMENT_CHECKPOINT_MIN_ROWS` (default [`DEFAULT_CHECKPOINT_MIN_ROWS`];
/// a small segment decodes trivially, so indexing it is pure cost).
///
/// Whether making this the *default* is owner-gated — it changes the headline bytes/point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CheckpointPolicy {
	/// Rows between checkpoints; `None` disables the index entirely.
	pub stride: Option<usize>,
	/// Segments with fewer rows than this are never checkpointed.
	pub min_rows: usize,
	/// Ceiling on the timestamp-codec override a checkpointed frame may pay
	/// (`blocked / best`; `1.0` = only when free). See
	/// [`DEFAULT_CHECKPOINT_MAX_CODEC_OVERHEAD`].
	pub max_codec_overhead: f64,
}

/// Row floor below which a checkpoint index is not worth its bytes.
pub const DEFAULT_CHECKPOINT_MIN_ROWS: usize = 8_192;

/// Ceiling on the timestamp-codec override ratio a checkpointed seal will accept: `1.25`
/// (a quarter more timestamp bytes than the best codec would write).
///
/// A checkpointed frame must encode its dods with the range-decodable per-block codec
/// rather than the smallest one. Measured, that override is ~free where the per-block
/// codec already wins (+1% on bounded jitter) but **~3.5× on a Gorilla-shaped
/// scattered-jitter column and ~14× on an RLE-shaped long-constant-run column** — and
/// both of those are *irregular*, so a shape-only test would happily checkpoint them
/// (`weft-physical-type`'s `benches/dodsearch.rs::report_codec_override_cost`). Without
/// this ceiling, enabling `WEFT_SEGMENT_CHECKPOINT_STRIDE` would silently bloat exactly
/// those columns. Override with `WEFT_SEGMENT_CHECKPOINT_MAX_CODEC_OVERHEAD`.
pub const DEFAULT_CHECKPOINT_MAX_CODEC_OVERHEAD: f64 = 1.25;

impl CheckpointPolicy {
	/// Never write a checkpoint index — the default, and byte-for-byte the historical
	/// frame layout.
	pub const DISABLED: Self = Self { stride: None, min_rows: DEFAULT_CHECKPOINT_MIN_ROWS, max_codec_overhead: DEFAULT_CHECKPOINT_MAX_CODEC_OVERHEAD };

	/// Read the policy from the environment: `WEFT_SEGMENT_CHECKPOINT_STRIDE` (absent, zero
	/// or unparseable → [`DISABLED`](Self::DISABLED)), `WEFT_SEGMENT_CHECKPOINT_MIN_ROWS`
	/// (→ [`DEFAULT_CHECKPOINT_MIN_ROWS`]) and `WEFT_SEGMENT_CHECKPOINT_MAX_CODEC_OVERHEAD`
	/// (→ [`DEFAULT_CHECKPOINT_MAX_CODEC_OVERHEAD`]; a non-finite or `< 1.0` value is
	/// ignored, since a ratio below 1.0 could never be met).
	#[must_use]
	pub fn from_env() -> Self {
		let stride = std::env::var("WEFT_SEGMENT_CHECKPOINT_STRIDE").ok().and_then(|v| v.trim().parse::<usize>().ok()).filter(|&s| s > 0);
		let min_rows = std::env::var("WEFT_SEGMENT_CHECKPOINT_MIN_ROWS").ok().and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(DEFAULT_CHECKPOINT_MIN_ROWS);
		let max_codec_overhead = std::env::var("WEFT_SEGMENT_CHECKPOINT_MAX_CODEC_OVERHEAD").ok().and_then(|v| v.trim().parse::<f64>().ok()).filter(|r| r.is_finite() && *r >= 1.0).unwrap_or(DEFAULT_CHECKPOINT_MAX_CODEC_OVERHEAD);
		Self { stride, min_rows, max_codec_overhead }
	}

	/// The stride to seal a segment with, or `None` to write the ordinary frame.
	///
	/// Three gates, all of which must pass: the policy is enabled, the segment's *shape*
	/// benefits (`benefits_from_checkpoints` — sorted and irregular) and is big enough,
	/// and the *codec override* the checkpointed frame would force is affordable
	/// (`checkpoint_codec_overhead` within [`max_codec_overhead`](Self::max_codec_overhead)).
	/// The last gate is what stops an irregular-but-Gorilla/RLE-shaped column from being
	/// silently bloated for a lookup win that is not worth those bytes.
	#[must_use]
	pub fn stride_for(self, row_count: usize, benefits: bool, codec_overhead: f64) -> Option<usize> {
		self.stride.filter(|_| benefits && row_count >= self.min_rows && codec_overhead <= self.max_codec_overhead)
	}
}

/// When a freshly sealed segment writes its value column in the **bit-sliced
/// (bit-plane-major)** codec instead of the size-selected one.
///
/// The bit-sliced layout stores the same *code* as the linear bit-pack with its bits permuted,
/// so it never wins on size — it is a **decode-latency** trade. Its decoder reads `u64` plane
/// words and walks only the set bits, so a small-magnitude column's empty high bit-planes are
/// skipped wholesale (measured ~5.7x the linear per-block unpack at the primitive level,
/// `weft-physical-type`'s `benches/bitunpack.rs`).
///
/// It is therefore **off by default**: `DISABLED` writes byte-for-byte the frames WeftDB has
/// always written. Enable per-deployment with `WEFT_SEGMENT_TRANSPOSED_MAX_OVERHEAD` — the
/// ceiling on how much larger the transposed value block may be than the size-selected codec
/// (`1.0` = only when free; `1.05` = up to 5% larger). A column where a *different* codec
/// family won the size race (FOR on a clustered column) is refused rather than bloated, the
/// same shape as [`CheckpointPolicy`]'s `max_codec_overhead` gate.
///
/// Whether making this the default is owner-gated — it changes the headline bytes/point.
///
/// **Needs the `bitsliced-codec` cargo feature**, which is off by default and pending patent
/// review. Without it the codec is not compiled in: [`from_env`](Self::from_env) logs a warning
/// and returns [`DISABLED`](Self::DISABLED), any other policy is ignored by the frame writer,
/// and reading a segment that uses the codec fails with an error naming the feature.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransposedPolicy {
	/// Ceiling on the transposed block's size overhead against the size-selected codec, or
	/// `None` to never write it.
	pub max_overhead: Option<f64>,
}

impl TransposedPolicy {
	/// Never write the transposed value codec — the default, and byte-for-byte the historical
	/// frame layout.
	pub const DISABLED: Self = Self { max_overhead: None };

	/// Read the policy from `WEFT_SEGMENT_TRANSPOSED_MAX_OVERHEAD` (absent, unparseable,
	/// non-finite or non-positive → [`DISABLED`](Self::DISABLED)). A value **below 1.0 is
	/// meaningful**: it adopts the bit-sliced layout only where it is also a strict size win
	/// (see `weft_physical_type::ColumnEncoding::transposed_overhead`).
	///
	/// Without the `bitsliced-codec` feature a set variable is ignored, with one warning per
	/// process, and the result is always [`DISABLED`](Self::DISABLED).
	#[must_use]
	pub fn from_env() -> Self {
		let raw = std::env::var("WEFT_SEGMENT_TRANSPOSED_MAX_OVERHEAD").ok();
		if !cfg!(feature = "bitsliced-codec") {
			if raw.is_some() {
				static WARNED: std::sync::Once = std::sync::Once::new();
				WARNED.call_once(|| tracing::warn!("WEFT_SEGMENT_TRANSPOSED_MAX_OVERHEAD is set, but this build does not include the bit-sliced value codec (cargo feature `bitsliced-codec`); ignoring it"));
			}
			return Self::DISABLED;
		}
		Self { max_overhead: raw.and_then(|v| v.trim().parse::<f64>().ok()).filter(|r| r.is_finite() && *r > 0.0) }
	}
}

/// Lift a stored integer epoch in `unit` to an absolute instant — the inverse of the
/// ingest path's `epoch_in_unit`. `None` if the epoch falls outside the representable
/// range (only reachable for coarse units at absurd magnitudes).
const fn instant_from_epoch(epoch: i64, unit: TimeUnit) -> Option<DateTime<Utc>> {
	match unit {
		TimeUnit::Seconds => DateTime::<Utc>::from_timestamp(epoch, 0),
		TimeUnit::Millis => DateTime::<Utc>::from_timestamp_millis(epoch),
		TimeUnit::Micros => DateTime::<Utc>::from_timestamp_micros(epoch),
		TimeUnit::Nanos => Some(DateTime::<Utc>::from_timestamp_nanos(epoch)),
	}
}

/// Decode one already-read segment frame's `[start, end]` window and fold it into its
/// own [`PartialReduction`] — the per-segment half of
/// [`SegmentStore::downsample_range`], factored out so it can run on the blocking pool.
///
/// `Ok(None)` when the segment contributes no present rows in the window (a pruned-in
/// segment can still be empty after the value/window filter), which the caller skips.
/// Pure and synchronous: it takes the frame bytes and returns the partial, so it holds
/// no store state and the whole call is `spawn_blocking`-safe.
fn segment_partial(descriptor: &SegmentDescriptor, bytes: &[u8], start: i64, end: i64, unit: TimeUnit, resolution: Resolution, aggregations: &[Aggregation]) -> Result<Option<PartialReduction>> {
	let (ts, vs) = if descriptor.format_version == PAGED_SEGMENT_FORMAT_VERSION { weft_physical_type::weftseg::read_paged_segment_range(bytes, start, end).map_err(|e| anyhow::anyhow!("decoding paged segment {}: {e}", descriptor.path))? } else { weft_physical_type::weftseg::read_segment_range(bytes, start, end).map_err(|e| anyhow::anyhow!("decoding segment {}: {e}", descriptor.path))? };
	// Null rows carry no value to reduce; present rows lift to absolute instants.
	let mut points: Vec<Point> = Vec::with_capacity(ts.len());
	for (t, v) in ts.into_iter().zip(vs) {
		if let Some(value) = v {
			if start <= t && t <= end {
				points.push(Point::new(instant_from_epoch(t, unit).ok_or_else(|| anyhow::anyhow!("segment {} holds epoch {t} outside the representable instant range", descriptor.path))?, value));
			}
		}
	}
	if points.is_empty() {
		return Ok(None);
	}
	let partial = weft_reduce::reduce_partial(&points, resolution, None, None, aggregations).map_err(|e| anyhow::anyhow!("reducing segment {}: {e}", descriptor.path))?;
	Ok(Some(partial))
}

/// Whether the inclusive query window `[start, end]` fully covers `descriptor`'s segment
/// — every row of the segment falls inside it, so no in-window trimming is needed. This is
/// the precondition for serving the segment from its persisted partial sidecar: the stored
/// partial is the segment's *whole* contribution, so it is only substitutable when the
/// query does not cut inside the segment. An empty segment (no min/max) is never "covered"
/// (it has nothing to serve and no sidecar anyway).
fn window_covers_segment(descriptor: &SegmentDescriptor, start: i64, end: i64) -> bool {
	descriptor.min_ts.zip(descriptor.max_ts).is_some_and(|(min_ts, max_ts)| start <= min_ts && max_ts <= end)
}

/// [`SegmentStore::open_scoped`] found its root already open: by another process, or
/// by another [`SegmentStore`] in this one that has not been dropped yet.
///
/// A store root is single-process (design section 5.5, OPEN step 1): Turso already
/// refuses a second process its database files, but `segments/` has no such guard, and
/// two owners would race each other's frame names, reaper and recovery. The root's
/// `LOCK` file turns that into this error, which names the holder, before any database
/// is opened. It arrives inside an [`anyhow::Error`]; `downcast_ref::<StoreLocked>()`
/// recovers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreLocked {
	/// The store root that is in use.
	pub root: PathBuf,
	/// The process holding it, as recorded in the root's `LOCK.holder` file, or `None`
	/// when that record could not be read (for example because the holder had not
	/// written it yet).
	pub holder: Option<LockHolder>,
}

impl std::fmt::Display for StoreLocked {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let root = self.root.display();
		match &self.holder {
			Some(holder) => write!(f, "segment store root {root} is in use by pid {} (session {}): a root can be open in only one process, and only once in it; stop that process, or close its store, and retry", holder.pid, holder.session),
			None => write!(f, "segment store root {root} is in use by another process (its pid could not be read from {}): a root can be open in only one process, and only once in it", self.root.join(crate::types::durable::lock::HOLDER_FILE).display()),
		}
	}
}

impl std::error::Error for StoreLocked {}

/// The environment variable that chooses what a store does after an ambiguous COMMIT.
///
/// Unset or `poison` write-poisons the store (design section 5.1). `exit` logs and exits
/// the process with [`AMBIGUOUS_COMMIT_EXIT_CODE`], for deployments whose supervisor
/// restarts the server; the restart's recovery then settles the commit. Read once, when
/// the store opens.
pub const AMBIGUOUS_COMMIT_ENV: &str = "WEFT_ON_AMBIGUOUS_COMMIT";

/// The status a process exits with after an ambiguous COMMIT under
/// `WEFT_ON_AMBIGUOUS_COMMIT=exit`: 70, `EX_SOFTWARE` in sysexits.h.
pub const AMBIGUOUS_COMMIT_EXIT_CODE: i32 = 70;

/// A write to a [`SegmentStore`] that is write-poisoned.
///
/// An earlier control-plane COMMIT failed in a way that may still have committed (any
/// COMMIT error except a conflict Turso found while validating the transaction, which it
/// rolls back before writing anything), so that transaction may or may not be durable,
/// and the store refuses every write until the process restarts (design section 5.1,
/// poison). Writes are refused because each one would build on a state nobody knows: a seal
/// would take the next id after a row that might not exist, a maintenance swap would
/// replace members that might already be gone. Reads go on serving what is committed.
/// The restart's open replays Turso's log, which is the authority on whether the
/// transaction committed. It arrives inside an [`anyhow::Error`];
/// `downcast_ref::<Poisoned>()` recovers it, and
/// [`SegmentStore::poisoned`] reports the same state without a write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Poisoned {
	/// What poisoned the store: the failed transaction and its error.
	pub reason: String,
}

impl std::fmt::Display for Poisoned {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "the segment store refuses writes until it is restarted (it is write-poisoned by {}); reads still work, and the restart's recovery settles the transaction", self.reason)
	}
}

impl std::error::Error for Poisoned {}

/// How long a maintenance entry point waits for a busy aspect by default: 30 seconds.
///
/// A per-aspect maintenance entry point of a [`SegmentStore`] waits this long for an
/// aspect another maintenance operation holds before it gives up with [`MaintenanceBusy`]
/// (design section 5.3, M1), unless changed with [`SegmentStore::with_maintenance_wait`].
pub const DEFAULT_MAINTENANCE_WAIT: Duration = Duration::from_secs(30);

/// A maintenance operation on an aspect did not start because another one held the aspect
/// for longer than the caller would wait.
///
/// Every maintenance operation (a reconcile, a split, an overlap merge, a squash, a
/// compaction) holds its aspect's maintenance lock from its first read to its last write,
/// so two of them never interleave on one aspect: a reconcile that had read a segment
/// could otherwise write it back after a squash had merged it away, bringing back a
/// member's stale rows. A per-aspect entry point waits for the lock up to
/// [`SegmentStore::maintenance_wait`] and then fails with this error, having read and
/// written nothing; try again once the other operation is done. `weft-server` answers it
/// with `409 Conflict`. It arrives inside an [`anyhow::Error`];
/// `downcast_ref::<MaintenanceBusy>()` recovers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceBusy {
	/// The aspect another maintenance operation held.
	pub aspect: String,
	/// How long the caller waited for it.
	pub waited: Duration,
}

impl std::fmt::Display for MaintenanceBusy {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "aspect {:?} is busy with another maintenance operation (waited {:?} for it); retry once that operation finishes", self.aspect, self.waited)
	}
}

impl std::error::Error for MaintenanceBusy {}

/// An aspect's maintenance lock, held by [`SegmentStore::hold_maintenance`] until it drops.
#[cfg(feature = "fault-injection")]
#[doc(hidden)]
pub struct MaintenanceHold {
	/// Held for its drop, which releases the lock.
	_held: MaintGuard,
}

/// How a store-wide maintenance sweep treats an aspect another operation holds.
///
/// Maintenance operations on one aspect take turns (see [`MaintenanceBusy`]). Either way,
/// an aspect the sweep could not take is left as it is and listed in the sweep's `busy`
/// field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceWait {
	/// Pass over it at once. What a background daemon wants: a tick never queues behind
	/// an operator's request or a long pass, and the next tick tries the aspect again.
	Skip,
	/// Wait for it, up to this long in all, for the whole sweep rather than per aspect.
	/// What an operator's request wants: it waits out a daemon pass in progress instead
	/// of leaving the aspect unmaintained.
	Wait(Duration),
}

impl MaintenanceWait {
	/// When a sweep that starts now stops waiting for busy aspects; `None` for
	/// [`Skip`](Self::Skip).
	fn deadline(self) -> Option<tokio::time::Instant> {
		match self {
			Self::Skip => None,
			Self::Wait(wait) => Some(tokio::time::Instant::now() + wait),
		}
	}
}

/// What a store does once a COMMIT is ambiguous; see [`AMBIGUOUS_COMMIT_ENV`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnAmbiguousCommit {
	/// Write-poison the store and keep serving reads.
	Poison,
	/// Log and exit the process with [`AMBIGUOUS_COMMIT_EXIT_CODE`].
	Exit,
}

impl OnAmbiguousCommit {
	/// Read the choice from [`AMBIGUOUS_COMMIT_ENV`].
	fn from_env() -> Self {
		Self::parse(std::env::var(AMBIGUOUS_COMMIT_ENV).ok().as_deref())
	}

	/// The choice `value` names. An unknown value poisons, the safe default that keeps
	/// reads up, and says so.
	fn parse(value: Option<&str>) -> Self {
		match value.map(str::trim) {
			Some(exit) if exit.eq_ignore_ascii_case("exit") => Self::Exit,
			None | Some("") => Self::Poison,
			Some(poison) if poison.eq_ignore_ascii_case("poison") => Self::Poison,
			Some(other) => {
				tracing::warn!(value = other, "{AMBIGUOUS_COMMIT_ENV} is neither `poison` nor `exit`; an ambiguous COMMIT will write-poison the store");
				Self::Poison
			}
		}
	}
}

/// A store's write poison: set once, by the first ambiguous COMMIT, and never cleared.
#[derive(Debug, Default)]
struct StorePoison {
	/// Checked on every write, so it is the one thing a write reads.
	poisoned: AtomicBool,
	/// Why, from the first poisoning; later ones keep it.
	reason: OnceLock<String>,
}

impl StorePoison {
	/// Poison the store for `reason`. Returns whether this call poisoned it (the first).
	fn set(&self, reason: String) -> bool {
		let first = self.reason.set(reason).is_ok();
		self.poisoned.store(true, Ordering::Release);
		first
	}

	/// The poison, if the store is poisoned.
	fn get(&self) -> Option<Poisoned> {
		self.poisoned.load(Ordering::Acquire).then(|| Poisoned { reason: self.reason.get().cloned().unwrap_or_default() })
	}
}

/// The file a stored frame `path` names under the store root `root` (design section 4:
/// `path` keeps being written root-joined, and readers resolve it against the current
/// root).
///
/// A path under `root` is used as it is. Any other path was written under a root that
/// has since moved: the store was relocated, restored into another root, or is mounted
/// somewhere else. The frame then lives at the same place below the current root's
/// `segments/`: `root/segments/` joined with the components after the last component
/// named `segments` (the last, so that a root which itself sits under a `segments`
/// directory resolves correctly). A path with no `segments` component, or one whose
/// tail is not plain names (a `..`, say), is used as it is: there is nothing safe to
/// resolve it to, and opening it fails or succeeds just as it did before resolution.
fn resolve_frame_path(root: &Path, stored: &str) -> PathBuf {
	let stored = Path::new(stored);
	if stored.starts_with(root) {
		return stored.to_path_buf();
	}
	let components: Vec<Component<'_>> = stored.components().collect();
	let Some(last_segments) = components.iter().rposition(|component| component.as_os_str() == "segments") else { return stored.to_path_buf() };
	let tail = &components[last_segments + 1..];
	if tail.is_empty() || !tail.iter().all(|component| matches!(component, Component::Normal(_))) {
		return stored.to_path_buf();
	}
	tail.iter().fold(root.join("segments"), |path, component| path.join(component))
}

/// The [`IndexOp::SeqBump`] that persists the allocator `commit` holds for `aspect`: past
/// every id handed out so far, including those of seals still writing their frames.
fn seq_bump(aspect: &str, commit: &CommitGuard) -> IndexOp {
	let state = commit.state();
	IndexOp::SeqBump { aspect: aspect.to_string(), next_id: state.next_id, epoch: state.epoch }
}

/// The aspect and id a legacy-named file carries: a frame `{aspect}-{id}.weftseg` or its
/// sidecar `{aspect}-{id}.weftpart`. The id is the digits after the last `-`, so an aspect
/// name that itself contains `-` still parses exactly. `None` for any other name.
fn legacy_file_id(name: &str) -> Option<(&str, u64)> {
	let stem = name.strip_suffix(".weftseg").or_else(|| name.strip_suffix(".weftpart"))?;
	let (aspect, id) = stem.rsplit_once('-')?;
	if aspect.is_empty() || id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
		return None;
	}
	Some((aspect, id.parse().ok()?))
}

/// The largest id each aspect's legacy-named files directly under `segments` carry (see
/// [`legacy_file_id`]). A sidecar counts as well as a frame: one left behind by a deleted
/// segment would otherwise match the frame of a seal that reused its id.
async fn legacy_file_ids(segments: PathBuf) -> Result<HashMap<String, u64>> {
	let mut ids: HashMap<String, u64> = HashMap::new();
	let mut entries = tokio::fs::read_dir(&segments).await.with_context(|| format!("listing {}", segments.display()))?;
	while let Some(entry) = entries.next_entry().await.with_context(|| format!("listing {}", segments.display()))? {
		let name = entry.file_name();
		if let Some((aspect, id)) = name.to_str().and_then(legacy_file_id) {
			let max = ids.entry(aspect.to_string()).or_insert(id);
			*max = (*max).max(id);
		}
	}
	Ok(ids)
}

/// Join `file_name` onto `dir`, refusing a result that is not a file directly inside
/// `dir`.
///
/// Frame names come from aspect names, which [`aspect_name::validate`] already keeps free
/// of separators and dot segments. This is the second line of defence: whatever the name,
/// the store never writes, reads or deletes a frame anywhere but its own `segments/`
/// directory.
///
/// # Errors
///
/// When the joined path's parent is not `dir`, or its last component is not `file_name`
/// (an absolute or drive-prefixed name replaces `dir`; a `..` or a separator moves it).
fn contained_file(dir: &Path, file_name: &str) -> Result<PathBuf> {
	let path = dir.join(file_name);
	if path.parent() != Some(dir) || path.file_name() != Some(OsStr::new(file_name)) {
		anyhow::bail!("refusing segment file {}: it is not directly inside {}", path.display(), dir.display());
	}
	Ok(path)
}

pub struct SegmentStore {
	/// The store root; sealed frames live in `root/segments/`.
	root: PathBuf,
	/// The libSQL segment index (`root/segment_index.db`).
	index: SegmentIndexStore,
	/// The libSQL per-aspect segment-set rollup (`root/metadata.db`) — one
	/// materialized [`AspectMetadata`] row per aspect, folded forward on every seal,
	/// so the aspect-wide summary is an O(1) read rather than a full index scan.
	metadata: AspectMetadataStore,
	/// The libSQL aspect-schema catalog (`root/aspect_catalog.db`) — the declared
	/// [`AspectSchema`] for each aspect, so a seal need not be handed the schema.
	catalog: AspectCatalog,
	/// Whether new seals carry a timestamp checkpoint index (off unless configured).
	checkpoints: CheckpointPolicy,
	/// Whether new seals write the transposed (decode-fast) value codec (off unless
	/// configured). See [`TransposedPolicy`].
	transposed: TransposedPolicy,
	/// Whether new seals materialize a per-segment partial-reduction sidecar (off unless
	/// configured). See [`PartialSidecar`].
	partials: PartialSidecarPolicy,
	/// The libSQL DB/subject registry (`root/catalog.db`) — the hierarchy above the
	/// aspect schemas. The store registers its own `(database, subject)` here on open,
	/// so the control plane can enumerate what a root holds.
	registry: CatalogStore,
	/// The database namespace this store's aspect schemas are declared under.
	database: String,
	/// The subject namespace this store's aspect schemas are declared under. A store
	/// is scoped to one subject, so its flat aspect keys (which name the `.weftseg`
	/// files and index rows) are unique within it.
	subject: String,
	/// Set by an ambiguous `segment_index.db` COMMIT; every write checks it and refuses
	/// while it is set (see [`Poisoned`]).
	poison: StorePoison,
	/// Whether an ambiguous COMMIT poisons the store or exits the process.
	on_ambiguous: OnAmbiguousCommit,
	/// Each aspect's commit lock (around every control-plane transaction of the aspect,
	/// guarding its id allocator) and maintenance lock (around a whole maintenance
	/// operation); see `aspect_locks`.
	locks: AspectLocks,
	/// The largest id each aspect's legacy-named files in `segments/` carry, read once,
	/// when the first allocator is seeded. Every id handed out after that comes from an
	/// allocator seeded above it, so the one listing stays a valid floor for an aspect
	/// seeded later.
	legacy_ids: tokio::sync::OnceCell<HashMap<String, u64>>,
	/// How long a per-aspect maintenance entry point waits for an aspect another
	/// maintenance operation holds.
	maintenance_wait: Duration,
	/// Segment-index transaction attempts that lost an MVCC conflict; see
	/// [`index_conflicts`](SegmentStore::index_conflicts).
	index_conflicts: AtomicU64,
	/// The root's `LOCK`, held for the store's lifetime. Declared last so that it drops
	/// last: the next opener cannot take the root while a database above is still
	/// closing.
	_root_lock: RootLock,
}

/// The directories `create_dir_all(dir)` will create: `dir` and each missing ancestor,
/// deepest first, up to the first that exists.
async fn missing_dirs(dir: &Path) -> Vec<PathBuf> {
	let mut missing = Vec::new();
	for ancestor in dir.ancestors().filter(|a| !a.as_os_str().is_empty()) {
		if !matches!(tokio::fs::try_exists(ancestor).await, Ok(false)) {
			break;
		}
		missing.push(ancestor.to_path_buf());
	}
	missing
}

/// Take `root`'s `LOCK` (design section 5.5, OPEN step 1). The acquire opens a file and
/// can briefly wait out a lock that a child being spawned still shares, so it runs on a
/// blocking thread.
async fn lock_root(root: &Path) -> Result<RootLock> {
	let owned = root.to_path_buf();
	let acquired = tokio::task::spawn_blocking(move || RootLock::acquire(&owned)).await.context("taking the store root lock")?;
	match acquired {
		Ok(lock) => Ok(lock),
		Err(RootLockError::Held { root, holder }) => Err(StoreLocked { root, holder }.into()),
		Err(e @ RootLockError::Io { .. }) => Err(e.into()),
	}
}

/// Make the store layout's directory entries durable (design section 5.5, OPEN step 4):
/// fsync `segments/` and the root, which hold the frames and the control-plane database
/// files with their `-log`s; every directory in `created` together with the parent of
/// the topmost one, so a root this open created is itself reachable after a power cut;
/// and the root's parent on every open.
///
/// The root's parent is synced even when this open did not create the root, because the
/// open that did may have died before reaching this point (a failed probe, a lost lock
/// race, a SIGKILL), and no later open would know the root's entry was never made
/// durable. A root several levels deep created by such an open still leaves the levels
/// above its parent unsynced; that needs both a crash before this step and a power cut
/// before the kernel writes the directories back on its own.
///
/// A root this open did not create may sit in a parent the server cannot read (a
/// service account's store under a `0711` directory, say). Opening that parent for the
/// fsync then fails with `PermissionDenied`, which is logged and skipped rather than
/// refusing a store that opened before S3.
///
/// Once is enough: Turso truncates an MVCC `-log` in place on checkpoint instead of
/// recreating it (`turso_core` `logical_log.rs`), so an entry fsynced here stays durable.
async fn sync_layout(fs: &dyn StoreFs, root: &Path, segments_dir: &Path, created: &[PathBuf]) -> Result<()> {
	let root_parent = parent_dir(root);
	let created_root = created.iter().any(|dir| dir == root);
	for dir in layout_dirs(root, segments_dir, created) {
		match fs.sync_dir(&dir).await {
			Ok(()) => {}
			Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && !created_root && root_parent.as_ref() == Some(&dir) => {
				tracing::warn!(dir = %dir.display(), root = %root.display(), error = %e, "cannot open the store root's parent to fsync it; the root predates this open, so its directory entry is left to the filesystem");
			}
			Err(e) => return Err(e).with_context(|| format!("fsyncing directory {}", dir.display())),
		}
	}
	Ok(())
}

/// The directory holding `path`'s entry: its parent, with the working directory standing
/// in for the empty parent of a bare relative name. `None` for a filesystem root.
fn parent_dir(path: &Path) -> Option<PathBuf> {
	path.parent().map(|parent| if parent.as_os_str().is_empty() { PathBuf::from(".") } else { parent.to_path_buf() })
}

/// The directories [`sync_layout`] fsyncs, deepest first: `segments/`, the root, each
/// directory in `created` and the existing parent of the topmost one, then the root's
/// parent.
fn layout_dirs(root: &Path, segments_dir: &Path, created: &[PathBuf]) -> Vec<PathBuf> {
	let mut dirs = vec![segments_dir.to_path_buf(), root.to_path_buf()];
	let anchor = created.last().and_then(|top| parent_dir(top));
	for dir in created.iter().cloned().chain(anchor).chain(parent_dir(root)) {
		if !dirs.contains(&dir) {
			dirs.push(dir);
		}
	}
	dirs
}

impl SegmentStore {
	/// Open (creating if absent) a segment store rooted at `root` under the `default`
	/// database/subject namespace: ensures `root/segments/` exists and opens the
	/// `root/segment_index.db` and `root/aspect_catalog.db` control-plane DBs.
	///
	/// Use [`open_scoped`](SegmentStore::open_scoped) to place the store's declared
	/// schemas under a named `(database, subject)` instead.
	///
	/// # Errors
	///
	/// As [`open_scoped`](SegmentStore::open_scoped): a [`StoreLocked`] error if the
	/// root is already open, and any failure opening the layout or the databases.
	pub async fn open(root: impl AsRef<Path>) -> Result<Self> {
		Self::open_scoped(root, DEFAULT_DATABASE, DEFAULT_SUBJECT).await
	}

	/// Open (creating if absent) a segment store rooted at `root` whose declared aspect
	/// schemas live under the `(database, subject)` namespace.
	///
	/// The segment files and index rows are keyed by the flat aspect name, which is
	/// unique within one subject; the aspect-schema catalog records the full
	/// `(database, subject, aspect)` triple so a reopened store recovers the encoding
	/// it sealed under.
	///
	/// The open follows design section 5.5:
	///
	/// 1. take the root's `LOCK`, before any database is opened, so one store (in one
	///    process) owns the root;
	/// 2. open the four control-plane databases, each refusing to open unless it runs
	///    MVCC and a new connection syncs FULL, and each syncing its MVCC header;
	/// 3. fsync `segments/`, the root, any directory this open created above it and the
	///    root's parent, so the database files and their `-log` files keep their
	///    directory entries through a power cut (Turso truncates a `-log` in place, so one
	///    fsync covers it for good);
	/// 4. register the store's `(database, subject)` scope in one catalog transaction,
	///    after the fsyncs, so its commit lands in a `-log` whose entry is already
	///    durable.
	///
	/// The lock is held until the store drops.
	///
	/// # Errors
	///
	/// A [`StoreLocked`] error if the root is already open, in another process or in
	/// this one; an error if a database cannot run MVCC or does not sync FULL, or if a
	/// directory fsync fails; and any filesystem error creating the layout or libSQL
	/// failure opening the databases.
	pub async fn open_scoped(root: impl AsRef<Path>, database: &str, subject: &str) -> Result<Self> {
		Self::open_scoped_on(&RealFs, root.as_ref(), database, subject).await
	}

	/// [`open_scoped`](SegmentStore::open_scoped), with its directory fsyncs going through
	/// `fs`, so a test can see which directories the open syncs and fail one.
	async fn open_scoped_on(fs: &dyn StoreFs, root: &Path, database: &str, subject: &str) -> Result<Self> {
		let root = root.to_path_buf();
		let segments_dir = root.join("segments");
		let created = missing_dirs(&segments_dir).await;
		tokio::fs::create_dir_all(&segments_dir).await.with_context(|| format!("creating segments dir {}", segments_dir.display()))?;
		let root_lock = lock_root(&root).await?;
		let index_path = root.join("segment_index.db");
		let index = SegmentIndexStore::open(&index_path.to_string_lossy()).await?;
		let metadata_path = root.join("metadata.db");
		let metadata = AspectMetadataStore::open(&metadata_path.to_string_lossy()).await?;
		let catalog_path = root.join("aspect_catalog.db");
		let catalog = AspectCatalog::open(&catalog_path.to_string_lossy()).await?;
		let registry_path = root.join("catalog.db");
		let registry = CatalogStore::open(&registry_path.to_string_lossy()).await?;
		sync_layout(fs, &root, &segments_dir, &created).await?;
		// Record this store's place in the hierarchy so the control plane can enumerate
		// the databases/subjects a root holds (idempotent, and atomic).
		registry.register_scope(database, subject).await?;
		let checkpoints = CheckpointPolicy::from_env();
		let partials = PartialSidecarPolicy::from_env();
		let transposed = TransposedPolicy::from_env();
		Ok(Self { root, index, metadata, catalog, registry, database: database.to_string(), subject: subject.to_string(), checkpoints, partials, transposed, poison: StorePoison::default(), on_ambiguous: OnAmbiguousCommit::from_env(), locks: AspectLocks::default(), legacy_ids: tokio::sync::OnceCell::new(), maintenance_wait: DEFAULT_MAINTENANCE_WAIT, index_conflicts: AtomicU64::new(0), _root_lock: root_lock })
	}

	/// The store's write poison, or `None` while it accepts writes.
	///
	/// A store poisons itself when a `segment_index.db` COMMIT fails in a way that may
	/// still have committed, because that transaction may or may not be durable (see
	/// [`Poisoned`]); a conflict Turso finds while validating the commit is not one of
	/// them, since it rolls the transaction back before writing anything. From then on every
	/// write fails with that [`Poisoned`] error and reads go on working, until the
	/// process restarts. `weft-server`'s `/ready` reports it as `poisoned` and
	/// `restart_required`.
	#[must_use]
	pub fn poisoned(&self) -> Option<Poisoned> {
		self.poison.get()
	}

	/// Write-poison the store for `reason`, as an ambiguous COMMIT would, without staging
	/// one. The hook for tests of the layers above (`weft-server`'s `/ready`), which
	/// cannot reach the store's commit path; only `fault-injection` builds have it.
	#[cfg(feature = "fault-injection")]
	#[doc(hidden)]
	pub fn inject_poison(&self, reason: impl Into<String>) {
		self.poison.set(reason.into());
	}

	/// Hold `aspect`'s maintenance lock until the returned guard drops, as a maintenance
	/// operation in progress would, without running one. The hook for tests of the layers
	/// above (`weft-server`'s `409 Conflict` and its daemon's skip), which cannot reach the
	/// lock; only `fault-injection` builds have it.
	///
	/// # Errors
	///
	/// [`MaintenanceBusy`] if another operation holds the aspect for longer than
	/// [`maintenance_wait`](Self::maintenance_wait).
	#[cfg(feature = "fault-injection")]
	#[doc(hidden)]
	pub async fn hold_maintenance(&self, aspect: &str) -> Result<MaintenanceHold> {
		Ok(MaintenanceHold { _held: self.maintain(aspect).await? })
	}

	/// Refuse a write while the store is poisoned. Every write entry point calls this
	/// first, before it reads or writes anything.
	fn writable(&self) -> Result<()> {
		match self.poison.get() {
			Some(poisoned) => Err(poisoned.into()),
			None => Ok(()),
		}
	}

	/// Commit `txn` to the segment index, unless the store is poisoned. An ambiguous
	/// failure poisons the store (or, under `WEFT_ON_AMBIGUOUS_COMMIT=exit`, exits the
	/// process) before the error is returned, so no later write builds on it.
	///
	/// The poison is checked again before every retry of a conflict, so a transaction
	/// waiting out a conflict while another writer poisons the store gives up with
	/// [`Poisoned`] instead of committing after it. An attempt already running when the
	/// poison is set is not stopped, and its COMMIT can still land: like every write
	/// entry point, the poison refuses what starts after it.
	///
	/// # Errors
	///
	/// [`Poisoned`] if the store already is, or became so while the transaction waited to
	/// retry a conflict; otherwise the transaction's [`IndexTxnError`].
	pub(crate) async fn commit_index(&self, txn: &IndexTxn) -> Result<TxnApplied> {
		self.writable()?;
		let result = self.index.apply_while(txn, || self.poison.get().is_none()).await;
		// Every attempt before the last lost a conflict, and so did the last one when the
		// conflict is what the transaction failed with.
		let lost = match &result {
			Ok(applied) => applied.attempts.saturating_sub(1),
			Err(e) if e.kind == TxnErrorKind::Retryable => e.attempts,
			Err(e) => e.attempts.saturating_sub(1),
		};
		if lost > 0 {
			self.index_conflicts.fetch_add(u64::from(lost), Ordering::Relaxed);
		}
		match result {
			Ok(applied) => Ok(applied),
			Err(e) => {
				if e.is_ambiguous() {
					self.on_ambiguous_commit(&e);
				} else if e.kind == TxnErrorKind::Retryable {
					// The conflict may have gone unretried because the store was poisoned
					// meanwhile; then the poison is the reason to report.
					self.writable()?;
				}
				Err(e.into())
			}
		}
	}

	/// React to an ambiguous COMMIT as [`AMBIGUOUS_COMMIT_ENV`] chose.
	fn on_ambiguous_commit(&self, error: &IndexTxnError) {
		let root = self.root.display();
		match self.on_ambiguous {
			OnAmbiguousCommit::Exit => {
				tracing::error!(%root, %error, "a segment_index COMMIT is ambiguous; exiting with status {AMBIGUOUS_COMMIT_EXIT_CODE} as {AMBIGUOUS_COMMIT_ENV}=exit asks, so that the restart's recovery settles it");
				eprintln!("weftdb: segment store {root}: {error}; exiting with status {AMBIGUOUS_COMMIT_EXIT_CODE} ({AMBIGUOUS_COMMIT_ENV}=exit)");
				std::process::exit(AMBIGUOUS_COMMIT_EXIT_CODE);
			}
			OnAmbiguousCommit::Poison => {
				if self.poison.set(error.to_string()) {
					tracing::error!(%root, %error, "a segment_index COMMIT is ambiguous; the store refuses writes until the process restarts, and reads go on");
				}
			}
		}
	}

	/// How many segment-index transaction attempts have lost an MVCC conflict (or found
	/// the database busy) since this store opened, whether the transaction was then
	/// retried or failed.
	///
	/// The store's own writers do not conflict with each other: the writers of one aspect
	/// take turns under its commit lock, and those of different aspects touch different
	/// rows. So this stays at zero unless something else contends for
	/// `segment_index.db`, and a count that rises is a conflict nothing explains.
	#[must_use]
	pub fn index_conflicts(&self) -> u64 {
		self.index_conflicts.load(Ordering::Relaxed)
	}

	/// How long a per-aspect maintenance entry point waits for an aspect another
	/// maintenance operation holds before it fails with [`MaintenanceBusy`]
	/// ([`DEFAULT_MAINTENANCE_WAIT`] unless changed with
	/// [`with_maintenance_wait`](Self::with_maintenance_wait)).
	#[must_use]
	pub const fn maintenance_wait(&self) -> Duration {
		self.maintenance_wait
	}

	/// Override how long a per-aspect maintenance entry point waits for a busy aspect; see
	/// [`maintenance_wait`](Self::maintenance_wait). `Duration::ZERO` makes it take the
	/// aspect only if it is free.
	#[must_use]
	pub const fn with_maintenance_wait(mut self, wait: Duration) -> Self {
		self.maintenance_wait = wait;
		self
	}

	/// Take `aspect`'s commit lock, seeding its id allocator the first time.
	async fn commit_lock(&self, aspect: &str) -> Result<CommitGuard> {
		self.locks.commit(aspect, || self.seed_allocator(aspect)).await
	}

	/// Where `aspect`'s id allocator starts: above every id it can have used, which is the
	/// largest of its persisted `aspect_seq.next_id`, one past its largest indexed id, and
	/// one past the largest id a legacy-named file of it in `segments/` carries. The last
	/// covers a frame whose seal crashed before committing it, which nothing in the index
	/// records.
	async fn seed_allocator(&self, aspect: &str) -> Result<AspectState> {
		let seed = self.index.allocator_seed(aspect).await?;
		let legacy = self.legacy_ids.get_or_try_init(|| legacy_file_ids(self.root.join("segments"))).await?.get(aspect).copied();
		let past = |id: Option<u64>| id.map_or(0, |id| id.saturating_add(1));
		let next_id = seed.next_id.unwrap_or(0).max(past(seed.max_id)).max(past(legacy));
		Ok(AspectState { next_id, epoch: seed.epoch })
	}

	/// Hand out a segment id for `aspect` that no segment, live or deleted, and no frame
	/// on disk has had, and that will never be handed out again, even if the caller never
	/// commits it.
	async fn allocate_id(&self, aspect: &str) -> Result<u64> {
		self.commit_lock(aspect).await?.allocate()
	}

	/// Take `aspect`'s maintenance lock for a per-aspect maintenance entry point, waiting
	/// up to [`maintenance_wait`](Self::maintenance_wait).
	async fn maintain(&self, aspect: &str) -> Result<MaintGuard> {
		match self.locks.maint_within(aspect, self.maintenance_wait).await {
			Some(held) => Ok(held),
			None => Err(MaintenanceBusy { aspect: aspect.to_string(), waited: self.maintenance_wait }.into()),
		}
	}

	/// Take `aspect`'s maintenance lock for a store-wide sweep: at once or not at all with
	/// no `deadline` ([`MaintenanceWait::Skip`]), else waiting until `deadline`.
	async fn sweep_maintain(&self, aspect: &str, deadline: Option<tokio::time::Instant>) -> Option<MaintGuard> {
		match deadline {
			None => self.locks.try_maint(aspect),
			Some(deadline) => self.locks.maint_within(aspect, deadline.saturating_duration_since(tokio::time::Instant::now())).await,
		}
	}

	/// Commit a freshly sealed frame's row (generation 0) under `aspect`'s commit lock:
	/// a plain insert of its allocated id, which no other row can hold, with the
	/// persisted allocator raised past it in the same transaction; then, under the same
	/// lock, fold it into the rollup, which no other writer of the aspect can be reading
	/// or writing meanwhile.
	async fn publish_seal(&self, aspect: &str, descriptor: &SegmentDescriptor) -> Result<()> {
		let commit = self.commit_lock(aspect).await?;
		self.commit_index(&IndexTxn::new(vec![IndexOp::InsertNew { aspect: aspect.to_string(), row: IndexRow::legacy(descriptor.clone()) }, seq_bump(aspect, &commit)])).await?;
		self.metadata.record_seal(aspect, descriptor).await?;
		drop(commit);
		Ok(())
	}

	/// Commit the row (generation 0) of a maintenance output that took a freshly
	/// allocated id, a split suffix, under `aspect`'s commit lock: a plain insert with the
	/// persisted allocator raised past it, as a seal commits. The caller rebuilds the
	/// rollup once its operation is done.
	async fn insert_descriptor(&self, aspect: &str, descriptor: &SegmentDescriptor) -> Result<()> {
		let commit = self.commit_lock(aspect).await?;
		self.commit_index(&IndexTxn::new(vec![IndexOp::InsertNew { aspect: aspect.to_string(), row: IndexRow::legacy(descriptor.clone()) }, seq_bump(aspect, &commit)])).await?;
		drop(commit);
		Ok(())
	}

	/// Record `descriptor` as `aspect`'s legacy row (generation 0) under the aspect's
	/// commit lock, replacing any row with its id: an in-place rewrite by maintenance.
	async fn put_descriptor(&self, aspect: &str, descriptor: &SegmentDescriptor) -> Result<()> {
		let commit = self.commit_lock(aspect).await?;
		let txn = IndexTxn::new(vec![IndexOp::Upsert { aspect: aspect.to_string(), row: IndexRow::legacy(descriptor.clone()) }]);
		self.commit_index(&txn).await?;
		drop(commit);
		Ok(())
	}

	/// Remove `aspect`'s row for segment `id`, if there is one, under the aspect's commit
	/// lock.
	async fn remove_descriptor(&self, aspect: &str, id: u64) -> Result<()> {
		let commit = self.commit_lock(aspect).await?;
		let txn = IndexTxn::new(vec![IndexOp::Delete { aspect: aspect.to_string(), id }]);
		self.commit_index(&txn).await?;
		drop(commit);
		Ok(())
	}

	/// The file holding `descriptor`'s frame: its recorded path, resolved against this
	/// store's root (see [`resolve_frame_path`]), so that a root that was moved or
	/// restored elsewhere still reads its own frames.
	fn frame_path(&self, descriptor: &SegmentDescriptor) -> PathBuf {
		resolve_frame_path(&self.root, &descriptor.path)
	}

	/// Read `descriptor`'s frame from [`frame_path`](Self::frame_path).
	async fn read_frame(&self, descriptor: &SegmentDescriptor) -> Result<Vec<u8>> {
		let path = self.frame_path(descriptor);
		tokio::fs::read(&path).await.with_context(|| format!("reading segment {}", path.display()))
	}

	/// Override this store's [`CheckpointPolicy`] (the env-read default is
	/// [`CheckpointPolicy::DISABLED`]), for a caller that wants the timestamp checkpoint
	/// index without setting an environment variable.
	#[must_use]
	pub const fn with_checkpoint_policy(mut self, policy: CheckpointPolicy) -> Self {
		self.checkpoints = policy;
		self
	}

	/// The checkpoint policy new seals are written under.
	#[must_use]
	pub const fn checkpoint_policy(&self) -> CheckpointPolicy {
		self.checkpoints
	}

	/// Override this store's [`TransposedPolicy`] (the env-read default is
	/// [`TransposedPolicy::DISABLED`]), for a caller that wants the decode-fast transposed
	/// value codec without setting an environment variable.
	#[must_use]
	pub const fn with_transposed_policy(mut self, policy: TransposedPolicy) -> Self {
		self.transposed = policy;
		self
	}

	/// The transposed-value-codec policy new seals are written under.
	#[must_use]
	pub const fn transposed_policy(&self) -> TransposedPolicy {
		self.transposed
	}

	/// The [`FrameOptions`] a fresh seal of a `row_count`-row segment writes under, folding
	/// this store's checkpoint and transposed policies into the one options value both frame
	/// writers take. `benefits`/`codec_overhead` are the segment's checkpoint gates (see
	/// [`CheckpointPolicy::stride_for`]).
	fn frame_options(&self, row_count: usize, benefits: bool, codec_overhead: f64) -> FrameOptions {
		FrameOptions { checkpoint_stride: self.checkpoints.stride_for(row_count, benefits, codec_overhead), transposed_max_overhead: self.transposed.max_overhead }
	}

	/// Override this store's [`PartialSidecarPolicy`] (the env-read default is
	/// [`PartialSidecarPolicy::DISABLED`]), for a caller that wants per-segment partial
	/// sidecars without setting an environment variable.
	#[must_use]
	pub const fn with_partial_sidecar_policy(mut self, policy: PartialSidecarPolicy) -> Self {
		self.partials = policy;
		self
	}

	/// The partial-sidecar policy new seals are written under.
	#[must_use]
	pub const fn partial_sidecar_policy(&self) -> PartialSidecarPolicy {
		self.partials
	}

	/// The control-plane index backing this store, for pruning/accounting queries
	/// ([`prune_by_time`](SegmentIndexStore::prune_by_time),
	/// [`load_index`](SegmentIndexStore::load_index), …).
	///
	/// For reads. Its writers ([`insert`](SegmentIndexStore::insert),
	/// [`delete`](SegmentIndexStore::delete)) bypass this store's write poison: they
	/// write even while it is [`poisoned`](Self::poisoned), and an ambiguous COMMIT in
	/// them does not poison it. Write through this store's own entry points.
	#[must_use]
	pub const fn index(&self) -> &SegmentIndexStore {
		&self.index
	}

	/// The aspect-schema catalog backing this store.
	///
	/// For reads. Its writer ([`declare`](AspectCatalog::declare)) bypasses this store's
	/// write poison; declare through [`SegmentStore::declare`] instead.
	#[must_use]
	pub const fn catalog(&self) -> &AspectCatalog {
		&self.catalog
	}

	/// The per-aspect segment-set rollup store backing this store, for the materialized
	/// aspect-wide summary ([`get`](AspectMetadataStore::get),
	/// [`list_aspects`](AspectMetadataStore::list_aspects)).
	///
	/// For reads. Its writers ([`put`](AspectMetadataStore::put),
	/// [`record_seal`](AspectMetadataStore::record_seal),
	/// [`remove`](AspectMetadataStore::remove)) bypass this store's write poison; rebuild
	/// a rollup through [`rebuild_aspect_metadata`](Self::rebuild_aspect_metadata) instead.
	#[must_use]
	pub const fn metadata(&self) -> &AspectMetadataStore {
		&self.metadata
	}

	/// The DB/subject registry backing this store, for enumerating the databases and
	/// subjects a root holds ([`list_databases`](CatalogStore::list_databases),
	/// [`list_subjects`](CatalogStore::list_subjects)).
	#[must_use]
	pub const fn registry(&self) -> &CatalogStore {
		&self.registry
	}

	/// The database namespace this store's aspect schemas are declared under (the
	/// upper half of its `(database, subject)` scope — fixed at
	/// [`open_scoped`](SegmentStore::open_scoped), `default` for
	/// [`open`](SegmentStore::open)).
	#[must_use]
	pub fn database(&self) -> &str {
		&self.database
	}

	/// The subject namespace this store's aspect schemas are declared under (the
	/// lower half of its `(database, subject)` scope).
	#[must_use]
	pub fn subject(&self) -> &str {
		&self.subject
	}

	/// The store root — the directory holding `segments/` and the four control-plane
	/// DBs. The natural base for a default [`backup_control_plane`](SegmentStore::backup_control_plane)
	/// destination.
	#[must_use]
	pub fn root(&self) -> &Path {
		&self.root
	}

	/// The aspects [`declare`](SegmentStore::declare)d in this store's
	/// `(database, subject)` scope, in name order.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn list_declared_aspects(&self) -> Result<Vec<String>> {
		self.catalog.list_aspects(&self.database, &self.subject).await
	}

	/// Take an online, consistent snapshot of this store's **four control-plane
	/// databases** (`segment_index.db`, `metadata.db`, `aspect_catalog.db`, `catalog.db`)
	/// into `dest_dir`, via Turso's `VACUUM INTO` (roadmap **Phase 7.4**). Each copy is
	/// reopened and verified to match its source table-for-table before the call returns.
	///
	/// The `.weftseg` measurement frames under `segments/` are **not** part of this backup
	/// — this is the control-plane (catalog/index/metadata) snapshot only, per the storage
	/// boundary (hard-constraint #3). `dest_dir` must not exist yet: the backup is built
	/// beside it and appears under that name, with a `MANIFEST.json`, only once it is
	/// complete and durable (see [`backup_control_plane_via`](SegmentStore::backup_control_plane_via)).
	/// Its parent is created if absent.
	///
	/// See [`snapshot_and_verify`](crate::snapshot_and_verify) for the consistency scope
	/// of the per-file verification (the row-count match assumes a quiescent source).
	///
	/// # Errors
	///
	/// `dest_dir` already exists, a filesystem error building or publishing the backup,
	/// or any per-database backup/verify failure (libSQL error, a missing table, or a
	/// source/copy mismatch). An error before the backup is published removes what it
	/// built. One after it (only the parent's fsync is left by then) leaves the complete
	/// backup under `dest_dir`; see
	/// [`backup_control_plane_via`](SegmentStore::backup_control_plane_via).
	pub async fn backup_control_plane(&self, dest_dir: impl AsRef<Path>) -> Result<ControlPlaneBackup> {
		self.backup_control_plane_with_verify(dest_dir, crate::VerifyMode::default()).await
	}

	/// Take the same four-database control-plane snapshot as
	/// [`backup_control_plane`](SegmentStore::backup_control_plane), under an explicit
	/// [`VerifyMode`](crate::VerifyMode).
	///
	/// [`VerifyMode::SourceMatch`](crate::VerifyMode::SourceMatch) (the default) verifies
	/// each copy against a fresh source read and assumes a **quiescent** store — the
	/// maintenance-window shape. [`VerifyMode::SnapshotOnly`](crate::VerifyMode::SnapshotOnly)
	/// verifies each copy on its own terms and never re-reads the source, so it is the
	/// mode an **online** backup taken against a live, ingesting store must use: a
	/// concurrent seal committing between a vacuum and its verification would otherwise
	/// be reported as a spurious mismatch.
	///
	/// # Errors
	///
	/// As [`backup_control_plane`](SegmentStore::backup_control_plane).
	pub async fn backup_control_plane_with_verify(&self, dest_dir: impl AsRef<Path>, mode: crate::VerifyMode) -> Result<ControlPlaneBackup> {
		self.backup_control_plane_via(&RealFs, dest_dir, mode).await
	}

	/// [`backup_control_plane_with_verify`](SegmentStore::backup_control_plane_with_verify),
	/// with every directory operation and fsync going through `fs` (the crash tests pass
	/// a `SimFs`).
	///
	/// The backup becomes visible only once it is complete and durable (design section
	/// 9): it is built in `<parent>/.partial-{label}-{nonce}/`, where each database is
	/// vacuumed, verified (including its expected table set) and fsynced; then a
	/// `MANIFEST.json` recording each file's size, tables and rows is written and synced,
	/// the build directory is fsynced, renamed to `dest_dir`, and the parent fsynced.
	///
	/// What a failure leaves depends on when it strikes:
	///
	/// - An error before the rename removes the build directory again, so nothing is
	///   left. The removal is best effort: if it fails, it is logged, the backup's own
	///   error is returned, and the directory is left as a crash would leave it.
	/// - A crash before the rename leaves the `.partial-*` directory. It is never counted
	///   or restored, and [`sweep_backup_staging`](crate::sweep_backup_staging) removes it
	///   once stale. weft-server's backup daemon runs that sweep; where the daemon is not
	///   enabled, such a directory stays until an operator removes it.
	/// - After the rename only the parent's fsync is left. An error there (or an injected
	///   fault at `B-renamed`) is returned although the complete backup is already under
	///   `dest_dir`: it counts toward retention, it restores, and its label is taken. It
	///   is only not yet known to survive power loss, which the next backup's fsync of
	///   the same parent makes it. A crash there leaves the same state.
	///
	/// # Errors
	///
	/// As [`backup_control_plane`](SegmentStore::backup_control_plane), or an injected
	/// fault at a `B-*` point.
	pub async fn backup_control_plane_via(&self, fs: &dyn StoreFs, dest_dir: impl AsRef<Path>, mode: crate::VerifyMode) -> Result<ControlPlaneBackup> {
		let dir = dest_dir.as_ref().to_path_buf();
		let (base, label) = crate::types::backup::split_dir(&dir).with_context(|| format!("choosing where to build backup {}", dir.display()))?;
		if fs.metadata(&dir).await.is_ok() {
			bail!("backup destination {} already exists (a backup is published under a fresh name)", dir.display());
		}
		create_dir_all_durable(fs, &base).await.with_context(|| format!("creating backup base {}", base.display()))?;
		let partial = base.join(crate::types::backup::staging_name(crate::PARTIAL_PREFIX, &label));
		fs.create_dir(&partial).await.with_context(|| format!("creating backup build directory {}", partial.display()))?;

		let [mut segment_index, mut metadata, mut aspect_catalog, mut registry] = match self.build_backup(fs, &partial, &dir, mode).await {
			Ok(reports) => reports,
			Err(err) => {
				// An error, unlike a crash, can clean up after itself. The build directory
				// may hold three full database copies, and the sweep that removes a crash's
				// leftovers runs only in weft-server's backup daemon, so a deployment that
				// takes only manual backups would otherwise collect them unseen.
				if let Err(cleanup) = fs.remove_dir_all(&partial).await {
					tracing::warn!(dir = %partial.display(), error = %cleanup, cause = %format!("{err:#}"), "could not remove a failed backup's build directory; the backup daemon's sweep removes it once stale, if the daemon is enabled");
				}
				return Err(err);
			}
		};
		fault::hit(FaultPoint::BRenamed).await?;
		fs.sync_dir(&base).await.with_context(|| format!("syncing backup base {}", base.display()))?;

		for (report, name) in [(&mut segment_index, "segment_index.db"), (&mut metadata, "metadata.db"), (&mut aspect_catalog, "aspect_catalog.db"), (&mut registry, "catalog.db")] {
			report.dest = dir.join(name);
		}
		Ok(ControlPlaneBackup { dir, segment_index, metadata, aspect_catalog, registry })
	}

	/// The steps of [`backup_control_plane_via`](SegmentStore::backup_control_plane_via)
	/// from the freshly created build directory `partial` up to its rename to `dir`:
	/// everything an error can still undo by removing `partial`. Returns the four
	/// snapshots' reports in [`CONTROL_PLANE_FILES`](crate::CONTROL_PLANE_FILES) order,
	/// still naming the files under `partial`.
	async fn build_backup(&self, fs: &dyn StoreFs, partial: &Path, dir: &Path, mode: crate::VerifyMode) -> Result<[SnapshotReport; 4]> {
		fault::hit(FaultPoint::BPartialCreated).await?;

		let segment_index = snapshot_into(fs, 0, self.index.backup_to_with(&partial.join("segment_index.db"), mode)).await.context("backing up segment_index.db")?;
		let metadata = snapshot_into(fs, 1, self.metadata.backup_to_with(&partial.join("metadata.db"), mode)).await.context("backing up metadata.db")?;
		let aspect_catalog = snapshot_into(fs, 2, self.catalog.backup_to_with(&partial.join("aspect_catalog.db"), mode)).await.context("backing up aspect_catalog.db")?;
		let registry = snapshot_into(fs, 3, self.registry.backup_to_with(&partial.join("catalog.db"), mode)).await.context("backing up catalog.db")?;
		// A control-plane backup links no frames (the whole-store backup, S16, links them
		// here); the point keeps the crash matrix's `B-*` sequence whole.
		fault::hit(FaultPoint::BLinks).await?;

		let created_ms = u64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis()).unwrap_or(u64::MAX);
		let manifest = crate::BackupManifest::new(created_ms, &[("segment_index.db", &segment_index), ("metadata.db", &metadata), ("aspect_catalog.db", &aspect_catalog), ("catalog.db", &registry)]);
		let manifest_bytes = serde_json::to_vec_pretty(&manifest).context("encoding the backup manifest")?;
		fs.create_new_write(&partial.join(crate::BACKUP_MANIFEST), manifest_bytes, SyncPolicy::Full, WritePoints::NONE).await.context("writing the backup manifest")?;
		fault::hit(FaultPoint::BManifest).await?;
		fs.sync_dir(partial).await.with_context(|| format!("syncing backup build directory {}", partial.display()))?;

		// Publish. The rename is the instant the backup appears under its label, whole.
		if fs.metadata(dir).await.is_ok() {
			bail!("backup destination {} appeared while the backup was being built", dir.display());
		}
		fs.rename(partial, dir).await.with_context(|| format!("publishing backup {} -> {}", partial.display(), dir.display()))?;
		Ok([segment_index, metadata, aspect_catalog, registry])
	}

	/// Declare `aspect`'s [`AspectSchema`] in this store's catalog, so later
	/// [`seal_declared`](SegmentStore::seal_declared) calls need not be handed the
	/// schema. Idempotent on the aspect (a re-declaration overwrites).
	///
	/// The name also names the aspect's segment files, so it must pass
	/// [`aspect_name::validate`] (no path separators, no leading `.`, no control
	/// characters, …); a name that does not is refused before anything is written.
	///
	/// # Errors
	///
	/// An [`InvalidAspectName`](crate::InvalidAspectName) when `aspect` is not a valid
	/// name; otherwise propagates any libSQL write failure.
	pub async fn declare(&self, aspect: &str, schema: &AspectSchema) -> Result<()> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		self.catalog.declare(&self.database, &self.subject, aspect, schema).await
	}

	/// The declared [`AspectSchema`] for `aspect`, or [`None`] if it has not been
	/// [`declare`](SegmentStore::declare)d in this store.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn schema_for(&self, aspect: &str) -> Result<Option<AspectSchema>> {
		self.catalog.get(&self.database, &self.subject, aspect).await
	}

	/// The declared schema for `aspect`, or an error naming the aspect if it has not
	/// been declared.
	async fn require_schema(&self, aspect: &str) -> Result<AspectSchema> {
		self.schema_for(aspect).await?.ok_or_else(|| anyhow::anyhow!("aspect {aspect:?} has no declared schema in {}/{}", self.database, self.subject))
	}

	/// Seal a dense `(timestamp, value)` batch under `aspect`'s **declared** schema —
	/// the catalog-backed counterpart of [`seal`](SegmentStore::seal) that looks the
	/// encoding up rather than taking it as a parameter.
	///
	/// # Errors
	///
	/// Returns an error if `aspect` has no declared schema; otherwise as
	/// [`seal`](SegmentStore::seal).
	pub async fn seal_declared(&self, aspect: &str, timestamps: &[i64], values: &[BigDecimal]) -> Result<SegmentDescriptor> {
		self.writable()?;
		let schema = self.require_schema(aspect).await?;
		self.seal(aspect, &schema, timestamps, values).await
	}

	/// Seal a **nullable** batch under `aspect`'s declared schema — the catalog-backed
	/// counterpart of [`seal_nullable`](SegmentStore::seal_nullable).
	///
	/// # Errors
	///
	/// Returns an error if `aspect` has no declared schema; otherwise as
	/// [`seal_nullable`](SegmentStore::seal_nullable).
	pub async fn seal_declared_nullable(&self, aspect: &str, timestamps: &[i64], values: &[Option<BigDecimal>]) -> Result<SegmentDescriptor> {
		self.writable()?;
		let schema = self.require_schema(aspect).await?;
		self.seal_nullable(aspect, &schema, timestamps, values).await
	}

	/// Seal a dense batch into a **paged** segment under `aspect`'s declared schema —
	/// the catalog-backed counterpart of [`seal_paged`](SegmentStore::seal_paged).
	///
	/// # Errors
	///
	/// Returns an error if `aspect` has no declared schema; otherwise as
	/// [`seal_paged`](SegmentStore::seal_paged).
	pub async fn seal_declared_paged(&self, aspect: &str, timestamps: &[i64], values: &[BigDecimal], rows_per_page: usize) -> Result<SegmentDescriptor> {
		self.writable()?;
		let schema = self.require_schema(aspect).await?;
		self.seal_paged(aspect, &schema, timestamps, values, rows_per_page).await
	}

	/// Seal a dense `(timestamp, value)` batch into a `.weftseg` file under `aspect`'s
	/// declared `schema` and record it in the index, returning the descriptor.
	///
	/// The batch is encoded under exactly the schema's declared
	/// [`PhysicalType`](weft_physical_type::PhysicalType): an unrepresentable value or
	/// one whose error exceeds the schema tolerance fails the seal rather than
	/// downcasting silently (hard constraint #4). The segment claims the next
	/// monotonic id for the aspect.
	///
	/// # Errors
	///
	/// Propagates a [`weft_physical_type::SealError`] (length mismatch, unrepresentable
	/// value, tolerance exceeded), a filesystem write error, or a libSQL index
	/// failure.
	pub async fn seal(&self, aspect: &str, schema: &AspectSchema, timestamps: &[i64], values: &[BigDecimal]) -> Result<SegmentDescriptor> {
		self.writable()?;
		let segment = schema.seal(timestamps, values).map_err(|e| anyhow::anyhow!("seal failed: {e}"))?;
		self.persist(aspect, &segment).await
	}

	/// Seal a **nullable** batch (a dense timestamp column and a `&[Option<BigDecimal>]`
	/// value column) into a `.weftseg` file and record it — the quality-column seal.
	///
	/// Present values are encoded densely under the declared encoding; `None` rows
	/// become cleared bits in the segment's quality mask. Enforcement mirrors
	/// [`seal`](SegmentStore::seal).
	///
	/// # Errors
	///
	/// As [`seal`](SegmentStore::seal).
	pub async fn seal_nullable(&self, aspect: &str, schema: &AspectSchema, timestamps: &[i64], values: &[Option<BigDecimal>]) -> Result<SegmentDescriptor> {
		self.writable()?;
		let segment = schema.seal_nullable(timestamps, values).map_err(|e| anyhow::anyhow!("seal failed: {e}"))?;
		self.persist(aspect, &segment).await
	}

	/// Seal a dense batch into a **paged** `.weftseg` segment (intra-segment page
	/// subdivision) and record it. Rows are partitioned into pages of `rows_per_page`,
	/// each independently encoded with its own min/max stats, so a later
	/// [`read_time_range`](SegmentStore::read_time_range) skips pages *within* the
	/// file, not just whole files.
	///
	/// # Errors
	///
	/// As [`seal`](SegmentStore::seal), plus a [`weft_physical_type::SealError::EmptyPageSize`]
	/// if `rows_per_page` is zero.
	pub async fn seal_paged(&self, aspect: &str, schema: &AspectSchema, timestamps: &[i64], values: &[BigDecimal], rows_per_page: usize) -> Result<SegmentDescriptor> {
		self.writable()?;
		let segment = schema.seal_paged(timestamps, values, rows_per_page).map_err(|e| anyhow::anyhow!("paged seal failed: {e}"))?;
		self.persist_paged(aspect, &segment).await
	}

	/// Seal a **nullable** batch into a paged `.weftseg` segment and record it — the
	/// quality-column paged seal.
	///
	/// # Errors
	///
	/// As [`seal_paged`](SegmentStore::seal_paged).
	pub async fn seal_paged_nullable(&self, aspect: &str, schema: &AspectSchema, timestamps: &[i64], values: &[Option<BigDecimal>], rows_per_page: usize) -> Result<SegmentDescriptor> {
		self.writable()?;
		let segment = schema.seal_paged_nullable(timestamps, values, rows_per_page).map_err(|e| anyhow::anyhow!("paged seal failed: {e}"))?;
		self.persist_paged(aspect, &segment).await
	}

	/// Write a freshly sealed segment to disk and record its descriptor. Shared by
	/// the dense and nullable seal paths.
	///
	/// The id comes from the aspect's allocator, under its commit lock, so concurrent seals
	/// never share an id or a frame name, and no seal takes the id of a deleted segment or
	/// of a frame a crashed seal left behind. The frame is written outside the lock, so
	/// seals of one aspect write their frames in parallel; the row, the allocator's raise
	/// and the rollup fold then commit under it ([`publish_seal`](Self::publish_seal)).
	async fn persist(&self, aspect: &str, segment: &Segment) -> Result<SegmentDescriptor> {
		aspect_name::validate(aspect)?;
		let id = self.allocate_id(aspect).await?;
		// Both opt-in layouts are applied here: the timestamp checkpoint index only when
		// configured AND the segment's shape actually benefits (sorted + irregular + big
		// enough), and the transposed value codec only when configured AND within its overhead
		// ceiling. With neither configured this writes the historical frame byte-for-byte, and
		// every combination reads identically.
		let bytes = segment.write_to_with(&self.frame_options(segment.stats.row_count, segment.benefits_from_checkpoints(), segment.checkpoint_codec_overhead()));
		let path = self.segment_path(aspect, id)?;
		tokio::fs::write(&path, &bytes).await.with_context(|| format!("writing segment {}", path.display()))?;
		fault::hit(FaultPoint::SFrameWritten).await?;
		let descriptor = SegmentDescriptor::of_segment(id, path.to_string_lossy().into_owned(), bytes.len() as u64, segment);
		self.publish_seal(aspect, &descriptor).await?;
		// Per-segment partial sidecar (opt-in) — a pure read accelerator, so decode only
		// when the policy actually wants one, and never fail the seal on a sidecar error.
		if self.partials.base_for(segment.stats.row_count).is_some() {
			let (ts, vs) = segment.decode_nullable();
			if let Err(e) = self.write_partial_sidecar(aspect, &descriptor, ts, vs).await {
				tracing::warn!(aspect, id = descriptor.id, error = %e, "failed to write partial sidecar; downsample will fall back to a full decode");
			}
		}
		Ok(descriptor)
	}

	/// Write a freshly sealed **paged** segment to disk (format-version-3 frame) and
	/// record its descriptor. The paged analogue of [`persist`](SegmentStore::persist), with
	/// the same id allocation and commit.
	async fn persist_paged(&self, aspect: &str, segment: &PagedSegment) -> Result<SegmentDescriptor> {
		aspect_name::validate(aspect)?;
		let id = self.allocate_id(aspect).await?;
		// As `persist`, though the checkpoint win is far smaller here — page pruning already
		// bounds a probe's decode to `rows_per_page`.
		let bytes = segment.write_to_with(&self.frame_options(segment.stats.row_count, segment.benefits_from_checkpoints(), segment.checkpoint_codec_overhead()));
		let path = self.segment_path(aspect, id)?;
		tokio::fs::write(&path, &bytes).await.with_context(|| format!("writing segment {}", path.display()))?;
		fault::hit(FaultPoint::SFrameWritten).await?;
		let descriptor = SegmentDescriptor::of_paged_segment(id, path.to_string_lossy().into_owned(), bytes.len() as u64, segment);
		self.publish_seal(aspect, &descriptor).await?;
		// As `persist`: opt-in per-segment partial sidecar, never failing the seal.
		if self.partials.base_for(segment.stats.row_count).is_some() {
			let (ts, vs) = segment.decode_nullable();
			if let Err(e) = self.write_partial_sidecar(aspect, &descriptor, ts, vs).await {
				tracing::warn!(aspect, id = descriptor.id, error = %e, "failed to write partial sidecar; downsample will fall back to a full decode");
			}
		}
		Ok(descriptor)
	}

	/// The on-disk path a freshly sealed segment of the given `aspect`/`id` takes.
	///
	/// # Errors
	///
	/// As [`aspect_file_path`](Self::aspect_file_path).
	fn segment_path(&self, aspect: &str, id: u64) -> Result<PathBuf> {
		self.aspect_file_path(aspect, id, "weftseg")
	}

	/// The on-disk path of the partial-reduction sidecar beside an `aspect`/`id` segment
	/// (`{aspect}-{id}.weftpart`, alongside the `.weftseg`).
	///
	/// # Errors
	///
	/// As [`aspect_file_path`](Self::aspect_file_path).
	fn sidecar_path(&self, aspect: &str, id: u64) -> Result<PathBuf> {
		self.aspect_file_path(aspect, id, "weftpart")
	}

	/// The path of the `{aspect}-{id}.{extension}` file under `segments/`: the one place
	/// an aspect name becomes a path.
	///
	/// # Errors
	///
	/// An [`InvalidAspectName`](crate::InvalidAspectName) when `aspect` fails
	/// [`aspect_name::validate`], or an error when the joined path is somehow not a
	/// direct child of `segments/` (defence in depth behind the name check).
	fn aspect_file_path(&self, aspect: &str, id: u64, extension: &str) -> Result<PathBuf> {
		aspect_name::validate(aspect)?;
		contained_file(&self.root.join("segments"), &format!("{aspect}-{id}.{extension}"))
	}

	/// Materialize a per-segment partial-reduction sidecar for a just-sealed segment, when
	/// the [`PartialSidecarPolicy`] calls for one. Decodes the segment's rows, folds the
	/// present ones into a [`PartialReduction`] at `base` over [`SIDECAR_AGGREGATIONS`], and
	/// writes the `.weftpart` frame beside the `.weftseg`. A no-op (returns `Ok(())`) when the
	/// policy is disabled/too-small, the segment has no declared time unit, or it holds no
	/// present rows.
	///
	/// A sidecar is a *pure acceleration*: a failure to write one must never fail the seal,
	/// so the persist paths call this and log-and-ignore any error rather than propagating.
	async fn write_partial_sidecar(&self, aspect: &str, descriptor: &SegmentDescriptor, timestamps: Vec<i64>, values: Vec<Option<BigDecimal>>) -> Result<()> {
		let Some(base) = self.partials.base_for(descriptor.row_count) else { return Ok(()) };
		let Some(unit) = descriptor.time_unit else { return Ok(()) };
		let mut points: Vec<Point> = Vec::with_capacity(timestamps.len());
		for (t, v) in timestamps.into_iter().zip(values) {
			if let Some(value) = v {
				points.push(Point::new(instant_from_epoch(t, unit).ok_or_else(|| anyhow::anyhow!("segment {} holds epoch {t} outside the representable instant range", descriptor.path))?, value));
			}
		}
		if points.is_empty() {
			return Ok(());
		}
		let partial = weft_reduce::reduce_partial(&points, base, None, None, &SIDECAR_AGGREGATIONS).map_err(|e| anyhow::anyhow!("reducing segment {} for its partial sidecar: {e}", descriptor.path))?;
		// Materialize the coarser rollup tiers the policy declares (each re-keyed from the
		// tier below), so a coarse downsample folds a coarse tier rather than the whole base.
		let sidecar = PartialSidecar::materialize(base, descriptor, partial, &self.partials.tiers()).map_err(|e| anyhow::anyhow!("building rollup tiers for segment {}: {e}", descriptor.path))?;
		let bytes = sidecar.to_bytes()?;
		let path = self.sidecar_path(aspect, descriptor.id)?;
		tokio::fs::write(&path, &bytes).await.with_context(|| format!("writing partial sidecar {}", path.display()))?;
		Ok(())
	}

	/// Load the partial-reduction sidecar for `descriptor`'s segment, or [`None`] when no
	/// sidecar exists **or the one on disk is stale** (its staleness stamp does not match
	/// the live descriptor — a rewritten segment automatically invalidates its old sidecar,
	/// see [`PartialSidecar::matches`]). A malformed/foreign sidecar is treated as absent
	/// too, so a downsample is always correct, at worst un-accelerated.
	///
	/// # Errors
	///
	/// Propagates a filesystem read error other than "not found".
	pub async fn load_partial_sidecar(&self, aspect: &str, descriptor: &SegmentDescriptor) -> Result<Option<PartialSidecar>> {
		let path = self.sidecar_path(aspect, descriptor.id)?;
		let bytes = match tokio::fs::read(&path).await {
			Ok(bytes) => bytes,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
			Err(e) => return Err(anyhow::Error::new(e).context(format!("reading partial sidecar {}", path.display()))),
		};
		// A malformed or version-mismatched frame is not an error the caller must handle —
		// it just means "no usable sidecar", so the read falls back to a full decode.
		let Ok(sidecar) = PartialSidecar::from_bytes(&bytes) else { return Ok(None) };
		Ok(sidecar.matches(descriptor).then_some(sidecar))
	}

	/// Delete the partial sidecar for `aspect`/`id`, if one exists. Best-effort by intent —
	/// a missing sidecar is success, since the caller's aim is only that no stale/orphaned
	/// `.weftpart` is left behind.
	///
	/// # Errors
	///
	/// Propagates a filesystem error other than "not found".
	async fn remove_sidecar(&self, aspect: &str, id: u64) -> Result<()> {
		let path = self.sidecar_path(aspect, id)?;
		match tokio::fs::remove_file(&path).await {
			Ok(()) => Ok(()),
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
			Err(e) => Err(anyhow::Error::new(e).context(format!("removing partial sidecar {}", path.display()))),
		}
	}

	/// Keep the partial sidecar consistent with a segment whose bytes were just **rewritten
	/// in place** (a reconcile sort, a split re-seal): regenerate it from the new rows when
	/// the policy still wants one (restoring the read acceleration the rewrite would
	/// otherwise have stranded — the old sidecar's staleness stamp no longer matches), or
	/// drop any now-stale sidecar when it does not. Purely an accelerator, so a maintenance
	/// error is logged, never propagated — the reconcile itself must not fail on it.
	async fn refresh_sidecar_after_rewrite(&self, aspect: &str, descriptor: &SegmentDescriptor, timestamps: Vec<i64>, values: Vec<Option<BigDecimal>>) {
		let result = if self.partials.base_for(descriptor.row_count).is_some() {
			// Overwrites any stale sidecar at this id with one matching the new bytes.
			self.write_partial_sidecar(aspect, descriptor, timestamps, values).await
		} else {
			self.remove_sidecar(aspect, descriptor.id).await
		};
		if let Err(e) = result {
			tracing::warn!(aspect, id = descriptor.id, error = %e, "failed to refresh partial sidecar after a segment rewrite; downsample will fall back to a full decode");
		}
	}

	/// Read every row of `aspect` whose timestamp falls in the inclusive range
	/// `[start, end]`, **opening only the segment files that overlap it**.
	///
	/// The index is pruned by time first (a SQL `WHERE` — no file touched for a
	/// disjoint segment); only the surviving descriptors' `.weftseg` files are opened,
	/// decoded, and filtered to the window. Returns parallel `(timestamps, values)`
	/// vectors with `None` at every null row, in segment-seal then in-segment order.
	///
	/// # Errors
	///
	/// Propagates a libSQL prune failure, a filesystem read error, or a
	/// [`weft_physical_type::weftseg::WeftSegError`] for a corrupt/unreadable `.weftseg`.
	pub async fn read_time_range(&self, aspect: &str, start: i64, end: i64) -> Result<(Vec<i64>, Vec<Option<BigDecimal>>)> {
		aspect_name::validate(aspect)?;
		let descriptors = self.index.prune_by_time(aspect, start, end).await?;
		let mut timestamps = Vec::new();
		let mut values = Vec::new();
		for descriptor in &descriptors {
			let bytes = self.read_frame(descriptor).await?;
			// A paged frame (v3) decodes through PagedSegment::read_time_range, which
			// skips pages *within* the file; a single-block frame decodes whole.
			// Windowed read: a regular block-coded frame decodes only the row window (closed-form
			// index range + per-present-row value read) — the paged variant additionally skips whole
			// pages disjoint from the window — instead of the whole segment; any other shape falls
			// back to a full decode. Already filtered to `[start, end]`.
			let (ts, vs) = if descriptor.format_version == PAGED_SEGMENT_FORMAT_VERSION { weft_physical_type::weftseg::read_paged_segment_range(&bytes, start, end).map_err(|e| anyhow::anyhow!("decoding paged segment {}: {e}", descriptor.path))? } else { weft_physical_type::weftseg::read_segment_range(&bytes, start, end).map_err(|e| anyhow::anyhow!("decoding segment {}: {e}", descriptor.path))? };
			for (t, v) in ts.into_iter().zip(vs) {
				if start <= t && t <= end {
					timestamps.push(t);
					values.push(v);
				}
			}
		}
		Ok((timestamps, values))
	}

	/// **Cross-segment downsample** — reduce `aspect`'s `[start, end]` rows into
	/// grid-aligned buckets **without ever materializing the range**.
	///
	/// The distributed shape of [`weft_reduce::reduce`]: the index is pruned by time, then
	/// each surviving segment is read, windowed, and folded into its own
	/// [`PartialReduction`]; the partials are merged and finished once. Only one segment's
	/// rows are in memory at a time, so a range far larger than RAM still reduces — and
	/// with a `sketch_p*` aggregation the per-bucket state is bounded too, which is the
	/// scenario `DdSketch`'s exact mergeability exists for.
	///
	/// Identical to reading the whole range and reducing it in one pass (merging is exact
	/// for every reduction), which is what the test asserts. Timestamps are the aspect's
	/// declared [`TimeUnit`] epochs, lifted to absolute instants for bucketing.
	///
	/// # Errors
	///
	/// If `aspect` has no declared schema, if a segment cannot be read or decoded, if a
	/// stored epoch falls outside the representable instant range, or if the reduction
	/// fails.
	pub async fn downsample_range(&self, aspect: &str, start: i64, end: i64, resolution: Resolution, aggregations: &[Aggregation]) -> Result<Vec<Bucket>> {
		aspect_name::validate(aspect)?;
		let schema = self.require_schema(aspect).await?;
		let descriptors = self.index.prune_by_time(aspect, start, end).await?;

		// A persisted per-segment partial can *substitute* for decoding a segment only when
		// it can answer this exact query: every requested reduction must be one the sidecar
		// materializes (the exact percentiles / TWA need the whole bucket, so a query asking
		// for them decodes), one of the sidecar's materialized grids (its base or a coarser
		// rollup tier) must equal or nest in the requested resolution (`partial_for` picks the
		// coarsest that does, re-keying from the fewest buckets), and the window must cover the
		// whole segment (no in-window trimming). When all three hold the stored partial IS the
		// segment's contribution, so the read skips the value-column decode entirely.
		let sidecar_eligible = aggregations.iter().all(|a| a.is_sidecar_materializable());

		// A single pruned segment has nothing to overlap with, so the offload below is
		// pure cost: measured 35.7ms vs 21.0ms inline on a 200k-row single-segment frame
		// (the `spawn_blocking` hand-off, ~1.7x slower, for zero parallelism). One
		// segment is a common shape — a young aspect, or a tight window that prunes to
		// one frame — so it keeps the inline path.
		if descriptors.len() == 1 {
			let descriptor = &descriptors[0];
			// The sidecar fast path: no file decode at all when a matching partial serves it —
			// directly when a materialized grid equals the requested resolution, or re-keyed
			// from the coarsest tier that nests in it.
			if sidecar_eligible && window_covers_segment(descriptor, start, end) {
				if let Some(sidecar) = self.load_partial_sidecar(aspect, descriptor).await? {
					if let Some(partial) = sidecar.partial_for(resolution).map_err(|e| anyhow::anyhow!("re-keying the sidecar of {aspect:?}: {e}"))? {
						return partial.finish(resolution, aggregations).map_err(|e| anyhow::anyhow!("finishing downsample of {aspect:?}: {e}"));
					}
				}
			}
			let bytes = self.read_frame(descriptor).await?;
			let partial = segment_partial(descriptor, &bytes, start, end, schema.timestamp_unit, resolution, aggregations)?;
			return partial.map_or_else(|| Ok(Vec::new()), |p| p.finish(resolution, aggregations).map_err(|e| anyhow::anyhow!("finishing downsample of {aspect:?}: {e}")));
		}

		// Each pruned segment folds into its own `PartialReduction` **concurrently**: the
		// file read is async I/O and the decode + reduce is CPU-bound, so each segment's
		// CPU half runs on the blocking pool and the reads overlap rather than queueing
		// behind one another. A segment served from its sidecar skips the decode (and the
		// `spawn_blocking` hand-off) altogether — just a small async read of the partial.
		// Peak memory is unchanged in kind — a segment's rows are dropped at its partial —
		// but now bounded by the in-flight segment count rather than by one, which is the
		// deliberate trade for the parallelism.
		let tasks = descriptors.into_iter().map(|descriptor| {
			let aggregations = aggregations.to_vec();
			let unit = schema.timestamp_unit;
			async move {
				if sidecar_eligible && window_covers_segment(&descriptor, start, end) {
					if let Some(sidecar) = self.load_partial_sidecar(aspect, &descriptor).await? {
						if let Some(partial) = sidecar.partial_for(resolution).map_err(|e| anyhow::anyhow!("re-keying a sidecar of {aspect:?}: {e}"))? {
							return Ok::<Option<PartialReduction>, anyhow::Error>(Some(partial));
						}
					}
				}
				let bytes = self.read_frame(&descriptor).await?;
				tokio::task::spawn_blocking(move || segment_partial(&descriptor, &bytes, start, end, unit, resolution, &aggregations)).await.context("segment reduce task panicked")?
			}
		});
		// `try_join_all` preserves input order, so the merge below folds partials in
		// descriptor order on every run — merging is exact and order-independent for
		// every reduction, but a deterministic order keeps results reproducible.
		let partials = futures::future::try_join_all(tasks).await?;

		let mut merged: Option<PartialReduction> = None;
		for partial in partials.into_iter().flatten() {
			match merged.as_mut() {
				Some(m) => m.merge(partial).map_err(|e| anyhow::anyhow!("merging a segment partial of {aspect:?}: {e}"))?,
				None => merged = Some(partial),
			}
		}
		merged.map_or_else(|| Ok(Vec::new()), |m| m.finish(resolution, aggregations).map_err(|e| anyhow::anyhow!("finishing downsample of {aspect:?}: {e}")))
	}

	/// **Point lookup** (roadmap Phase 4.6 read-planner): the present value of
	/// `aspect` at exactly timestamp `t`, or [`None`] when no present row carries it.
	///
	/// The index is pruned by time first — a point lookup is
	/// [`prune_by_time`](SegmentIndexStore::prune_by_time) with `start == end == t`,
	/// so only the `.weftseg` files whose `[min_ts, max_ts]` spans `t` are opened. Each
	/// opened segment resolves the instant with its own persisted per-segment order
	/// signal: a `time_sorted` segment binary-searches the timestamp column, an
	/// out-of-order one linear-scans it (see
	/// [`Segment::value_at`](weft_physical_type::Segment::value_at)). Candidates are
	/// pruned in seal-id order, so when overlapping segments each carry a present
	/// value at `t` the most recently sealed one wins (last-writer-wins) — the natural
	/// read-your-writes answer once out-of-order reconciliation (Phase 4.6) can leave
	/// two segments spanning one instant.
	///
	/// # Errors
	///
	/// Propagates a libSQL prune failure, a filesystem read error, or a
	/// [`weft_physical_type::weftseg::WeftSegError`] for a corrupt/unreadable `.weftseg`.
	pub async fn read_point(&self, aspect: &str, t: i64) -> Result<Option<BigDecimal>> {
		aspect_name::validate(aspect)?;
		let descriptors = self.index.prune_by_time(aspect, t, t).await?;
		let mut found = None;
		for descriptor in &descriptors {
			let bytes = self.read_frame(descriptor).await?;
			// Streaming single-value read: prunes/skips the pages and value-column blocks a point
			// lookup does not touch, unpacking only the one block covering `t` on a per-block codec
			// (roadmap Phase 4/6). Equal to `…read_from(&bytes)?.value_at(t)` for every frame.
			let hit = if descriptor.format_version == PAGED_SEGMENT_FORMAT_VERSION { weft_physical_type::weftseg::read_paged_segment_point(&bytes, t).map_err(|e| anyhow::anyhow!("decoding paged segment {}: {e}", descriptor.path))? } else { weft_physical_type::weftseg::read_segment_point(&bytes, t).map_err(|e| anyhow::anyhow!("decoding segment {}: {e}", descriptor.path))? };
			if hit.is_some() {
				found = hit;
			}
		}
		Ok(found)
	}

	/// **Batch point lookup** (roadmap Phase 4/6): the present value of `aspect` at each
	/// instant in `ts`, returned aligned to `ts` (`None` where no present row carries it).
	///
	/// The batch analogue of [`read_point`](SegmentStore::read_point): the index is pruned
	/// **once** by the batch's whole `[min(ts), max(ts)]` span, and each surviving segment is
	/// opened **once** and its timestamp column decoded **once** for the whole batch (via the
	/// streaming [`read_segment_points`](weft_physical_type::weftseg::read_segment_points) /
	/// [`read_paged_segment_points`](weft_physical_type::weftseg::read_paged_segment_points)), so
	/// looking up `N` instants that share segments pays one file read + one timestamp decode per
	/// segment rather than `N`. As with `read_point`, candidates are merged in seal-id order so
	/// the most recently sealed present value wins per instant (last-writer-wins). An empty `ts`
	/// yields an empty vector without touching the index.
	///
	/// # Errors
	///
	/// Propagates a libSQL prune failure, a filesystem read error, or a
	/// [`weft_physical_type::weftseg::WeftSegError`] for a corrupt/unreadable `.weftseg`.
	pub async fn read_points(&self, aspect: &str, ts: &[i64]) -> Result<Vec<Option<BigDecimal>>> {
		aspect_name::validate(aspect)?;
		if ts.is_empty() {
			return Ok(Vec::new());
		}
		// Prune the index once by the batch's whole span (safe: every instant lies within it).
		let (lo, hi) = ts.iter().fold((i64::MAX, i64::MIN), |(lo, hi), &t| (lo.min(t), hi.max(t)));
		let descriptors = self.index.prune_by_time(aspect, lo, hi).await?;
		let mut found = vec![None; ts.len()];
		for descriptor in &descriptors {
			let bytes = self.read_frame(descriptor).await?;
			let hits = if descriptor.format_version == PAGED_SEGMENT_FORMAT_VERSION { weft_physical_type::weftseg::read_paged_segment_points(&bytes, ts).map_err(|e| anyhow::anyhow!("decoding paged segment {}: {e}", descriptor.path))? } else { weft_physical_type::weftseg::read_segment_points(&bytes, ts).map_err(|e| anyhow::anyhow!("decoding segment {}: {e}", descriptor.path))? };
			// Later (higher seal-id) segments override earlier ones per instant — last-writer-wins.
			for (slot, hit) in found.iter_mut().zip(hits) {
				if hit.is_some() {
					*slot = hit;
				}
			}
		}
		Ok(found)
	}

	/// **Out-of-order reconciliation** (roadmap Phase 4.6): rewrite a single
	/// out-of-order segment into a time-sorted one, in place at its own id.
	///
	/// The first bounded slice of the QuestDB-O3-style reconciliation path — a
	/// per-segment sort rather than the full staging-window cross-segment merge. It
	/// reads segment `id`, stable-sorts its rows by timestamp (equal timestamps keep
	/// their original order, so [`read_point`](SegmentStore::read_point)'s
	/// first-present-of-a-run answer is preserved), and re-seals the sorted rows
	/// under the aspect's declared schema to the **same id and file** (the index's
	/// `INSERT OR REPLACE` on `(aspect, id)` swaps the descriptor, the deterministic
	/// path overwrites the frame). The reconciled segment is `time_sorted`, so it
	/// drops out of the `unsorted_segments` order-health count and a point lookup over
	/// it binary-searches. The frame kind is preserved (a paged segment re-seals
	/// paged at its own page height). The materialized rollup is rebuilt from the
	/// index afterward (a reconcile is not a fresh seal, so it must not fold forward).
	///
	/// Returns `true` when a rewrite happened, `false` when the segment was already
	/// sorted (a no-op).
	///
	/// # Errors
	///
	/// Returns an error if `aspect` has no declared schema or no segment `id`;
	/// propagates a filesystem/read error, a re-seal failure, or a libSQL failure.
	/// A [`MaintenanceBusy`] error if another maintenance operation holds the aspect for
	/// longer than [`maintenance_wait`](Self::maintenance_wait).
	pub async fn reconcile_segment(&self, aspect: &str, id: u64) -> Result<bool> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		let held = self.maintain(aspect).await?;
		self.reconcile_segment_held(&held, id).await
	}

	/// [`reconcile_segment`](Self::reconcile_segment) under the maintenance lock `held`.
	async fn reconcile_segment_held(&self, held: &MaintGuard, id: u64) -> Result<bool> {
		self.writable()?;
		let aspect = held.aspect();
		aspect_name::validate(aspect)?;
		let descriptor = self.index.all(aspect).await?.into_iter().find(|d| d.id == id).ok_or_else(|| anyhow::anyhow!("aspect {aspect:?} has no segment {id}"))?;
		if descriptor.time_sorted {
			return Ok(false);
		}
		let schema = self.require_schema(aspect).await?;
		let bytes = self.read_frame(&descriptor).await?;
		let paged = descriptor.format_version == PAGED_SEGMENT_FORMAT_VERSION;
		let (rows_per_page, timestamps, values) = if paged {
			let segment = PagedSegment::read_from(&bytes).map_err(|e| anyhow::anyhow!("decoding paged segment {}: {e}", descriptor.path))?;
			let rows_per_page = segment.rows_per_page;
			let (ts, vs) = segment.decode_nullable();
			(Some(rows_per_page), ts, vs)
		} else {
			let segment = Segment::read_from(&bytes).map_err(|e| anyhow::anyhow!("decoding segment {}: {e}", descriptor.path))?;
			let (ts, vs) = segment.decode_nullable();
			(None, ts, vs)
		};
		// Stable sort by timestamp — equal timestamps keep their ingest order.
		let mut rows: Vec<(i64, Option<BigDecimal>)> = timestamps.into_iter().zip(values).collect();
		rows.sort_by_key(|(t, _)| *t);
		let (sorted_ts, sorted_vs): (Vec<i64>, Vec<Option<BigDecimal>>) = rows.into_iter().unzip();
		fault::hit(FaultPoint::MPlanned).await?;
		// Re-seal the sorted rows in the original frame kind, to the same id/file.
		let path = self.segment_path(aspect, id)?;
		let new_descriptor = if let Some(rows_per_page) = rows_per_page {
			let segment = schema.seal_paged_nullable(&sorted_ts, &sorted_vs, rows_per_page).map_err(|e| anyhow::anyhow!("re-seal failed: {e}"))?;
			let new_bytes = segment.write_to();
			tokio::fs::write(&path, &new_bytes).await.with_context(|| format!("writing reconciled segment {}", path.display()))?;
			SegmentDescriptor::of_paged_segment(id, path.to_string_lossy().into_owned(), new_bytes.len() as u64, &segment)
		} else {
			let segment = schema.seal_nullable(&sorted_ts, &sorted_vs).map_err(|e| anyhow::anyhow!("re-seal failed: {e}"))?;
			let new_bytes = segment.write_to();
			tokio::fs::write(&path, &new_bytes).await.with_context(|| format!("writing reconciled segment {}", path.display()))?;
			SegmentDescriptor::of_segment(id, path.to_string_lossy().into_owned(), new_bytes.len() as u64, &segment)
		};
		self.put_descriptor(aspect, &new_descriptor).await?;
		// The rewrite changed the segment's bytes, staling any sidecar — regenerate it from
		// the reconciled rows (or drop it if the policy no longer wants one).
		self.refresh_sidecar_after_rewrite(aspect, &new_descriptor, sorted_ts, sorted_vs).await;
		// A reconcile replaces a segment rather than adding one, so the O(1) fold would
		// double-count — recompute the rollup from the durable index instead.
		self.rebuild_aspect_metadata(aspect).await?;
		Ok(true)
	}

	/// Re-seal a nullable `(timestamps, values)` batch into the `.weftseg` file for
	/// `aspect`'s existing segment `id`, in the frame kind selected by `rows_per_page`
	/// (`Some` → paged, `None` → single-block), replacing its descriptor in the
	/// control-plane index.
	///
	/// The write-half shared by the split and merge paths: [`reconcile_segment`] inlines
	/// the same logic against a single id. It does **not** touch the materialized rollup —
	/// a caller that changes the segment set rebuilds it once at the end. A new segment
	/// (a split suffix) goes through [`reseal_nullable_fresh`](Self::reseal_nullable_fresh)
	/// instead.
	async fn reseal_nullable_at(&self, aspect: &str, schema: &AspectSchema, id: u64, timestamps: &[i64], values: &[Option<BigDecimal>], rows_per_page: Option<usize>) -> Result<SegmentDescriptor> {
		let descriptor = self.write_resealed(aspect, schema, id, timestamps, values, rows_per_page).await?;
		self.put_descriptor(aspect, &descriptor).await?;
		// Keep the sidecar consistent with the freshly written bytes at this id.
		self.refresh_sidecar_after_rewrite(aspect, &descriptor, timestamps.to_vec(), values.to_vec()).await;
		Ok(descriptor)
	}

	/// [`reseal_nullable_at`](Self::reseal_nullable_at) for a new segment: `id` comes
	/// from [`allocate_id`](Self::allocate_id), and its row is inserted, never replacing
	/// one, with the allocator raised past it.
	async fn reseal_nullable_fresh(&self, aspect: &str, schema: &AspectSchema, id: u64, timestamps: &[i64], values: &[Option<BigDecimal>], rows_per_page: Option<usize>) -> Result<SegmentDescriptor> {
		let descriptor = self.write_resealed(aspect, schema, id, timestamps, values, rows_per_page).await?;
		self.insert_descriptor(aspect, &descriptor).await?;
		self.refresh_sidecar_after_rewrite(aspect, &descriptor, timestamps.to_vec(), values.to_vec()).await;
		Ok(descriptor)
	}

	/// Seal a nullable `(timestamps, values)` batch in the frame kind `rows_per_page`
	/// selects and write it to the `.weftseg` file for `aspect`/`id`, returning its
	/// descriptor; nothing is indexed.
	async fn write_resealed(&self, aspect: &str, schema: &AspectSchema, id: u64, timestamps: &[i64], values: &[Option<BigDecimal>], rows_per_page: Option<usize>) -> Result<SegmentDescriptor> {
		let path = self.segment_path(aspect, id)?;
		let descriptor = if let Some(rows_per_page) = rows_per_page {
			let segment = schema.seal_paged_nullable(timestamps, values, rows_per_page).map_err(|e| anyhow::anyhow!("split re-seal failed: {e}"))?;
			let new_bytes = segment.write_to();
			tokio::fs::write(&path, &new_bytes).await.with_context(|| format!("writing split segment {}", path.display()))?;
			SegmentDescriptor::of_paged_segment(id, path.to_string_lossy().into_owned(), new_bytes.len() as u64, &segment)
		} else {
			let segment = schema.seal_nullable(timestamps, values).map_err(|e| anyhow::anyhow!("split re-seal failed: {e}"))?;
			let new_bytes = segment.write_to();
			tokio::fs::write(&path, &new_bytes).await.with_context(|| format!("writing split segment {}", path.display()))?;
			SegmentDescriptor::of_segment(id, path.to_string_lossy().into_owned(), new_bytes.len() as u64, &segment)
		};
		Ok(descriptor)
	}

	/// **Split a sorted segment at a timestamp boundary** (roadmap Phase 4.6 — the
	/// physical mechanism of the split-not-rewrite reconciliation path).
	///
	/// Partitions segment `id` of `aspect` at `boundary` into a **prefix** (rows with
	/// timestamp strictly `< boundary`, kept at the original `id`) and a **suffix**
	/// (rows at or after `boundary`, moved to a freshly-allocated segment id), following
	/// the [`split_index`](weft_physical_type::split_index) partition point. Because the
	/// input is time-sorted, the prefix's every timestamp is `< boundary ≤` the suffix's
	/// every timestamp, so the two results are internally sorted **and disjoint in time**
	/// — the split adds no cross-segment overlap, and a point/range read still opens
	/// exactly one of them for any instant. Both keep the input's frame kind (a paged
	/// segment splits into two paged segments at its own page height).
	///
	/// This is the primitive `QuestDB`'s partition split is built on: once a large cold
	/// prefix is carved into its own segment, later late-data merges touch only the hot
	/// suffix and never rewrite the cold prefix again, bounding write amplification over
	/// the segment's lifetime. Wiring [`SplitPolicy::decide`](weft_physical_type::SplitPolicy)
	/// into [`reconcile_overlaps`](SegmentStore::reconcile_overlaps) to *choose* a split
	/// over a full rewrite is the next slice; this slice ships the mechanism it calls.
	///
	/// The suffix segment is written **before** the prefix is rewritten, so a crash
	/// mid-split can at worst leave the suffix rows duplicated in the not-yet-shrunk
	/// prefix (a cross-segment overlap [`reconcile_overlaps`](Self::reconcile_overlaps) repairs), never lost.
	///
	/// Returns `Some(suffix_id)` — the new segment's id — when a split happened, or
	/// `None` when the split was degenerate (`boundary` falls before the first or after
	/// the last row, so every row lands on one side and there is nothing to carve). A
	/// degenerate split writes nothing.
	///
	/// # Errors
	///
	/// Returns an error if `aspect` has no declared schema or no segment `id`, if `id`
	/// is **not time-sorted** (split requires a sorted segment — reconcile it first, so
	/// the [`split_index`] precondition holds), or propagates a filesystem/decode/re-seal/
	/// libSQL failure.
	/// A [`MaintenanceBusy`] error if another maintenance operation holds the aspect for
	/// longer than [`maintenance_wait`](Self::maintenance_wait).
	pub async fn split_segment(&self, aspect: &str, id: u64, boundary: i64) -> Result<Option<u64>> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		let held = self.maintain(aspect).await?;
		self.split_segment_held(&held, id, boundary).await
	}

	/// [`split_segment`](Self::split_segment) under the maintenance lock `held`.
	async fn split_segment_held(&self, held: &MaintGuard, id: u64, boundary: i64) -> Result<Option<u64>> {
		self.writable()?;
		let aspect = held.aspect();
		aspect_name::validate(aspect)?;
		let descriptor = self.index.all(aspect).await?.into_iter().find(|d| d.id == id).ok_or_else(|| anyhow::anyhow!("aspect {aspect:?} has no segment {id}"))?;
		if !descriptor.time_sorted {
			return Err(anyhow::anyhow!("segment {id} of aspect {aspect:?} is out of order; reconcile it before splitting"));
		}
		let schema = self.require_schema(aspect).await?;
		// Read once, capturing the paged page height so each half re-seals in kind.
		let bytes = self.read_frame(&descriptor).await?;
		let (rows_per_page, timestamps, values) = if descriptor.format_version == PAGED_SEGMENT_FORMAT_VERSION {
			let segment = PagedSegment::read_from(&bytes).map_err(|e| anyhow::anyhow!("decoding paged segment {}: {e}", descriptor.path))?;
			let rows_per_page = segment.rows_per_page;
			let (ts, vs) = segment.decode_nullable();
			(Some(rows_per_page), ts, vs)
		} else {
			let segment = Segment::read_from(&bytes).map_err(|e| anyhow::anyhow!("decoding segment {}: {e}", descriptor.path))?;
			let (ts, vs) = segment.decode_nullable();
			(None, ts, vs)
		};
		let k = split_index(&timestamps, boundary);
		if k == 0 || k == timestamps.len() {
			// Every row is on one side — nothing to carve.
			return Ok(None);
		}
		let (prefix_ts, suffix_ts) = timestamps.split_at(k);
		let (prefix_vs, suffix_vs) = values.split_at(k);
		// Suffix first (new id), then rewrite the prefix in place: a crash between the
		// two duplicates rows rather than dropping them. The suffix's id comes from the
		// allocator seals use, so it is never one a seal or a deleted segment had.
		let suffix_id = self.allocate_id(aspect).await?;
		self.reseal_nullable_fresh(aspect, &schema, suffix_id, suffix_ts, suffix_vs, rows_per_page).await?;
		self.reseal_nullable_at(aspect, &schema, id, prefix_ts, prefix_vs, rows_per_page).await?;
		// A split turns one segment into two; the O(1) rollup fold would miscount, so
		// rebuild it from the durable index.
		self.rebuild_aspect_metadata(aspect).await?;
		Ok(Some(suffix_id))
	}

	/// **Reconcile every out-of-order segment** of `aspect` (roadmap Phase 4.6),
	/// returning the number rewritten. The natural trigger is a non-zero
	/// [`unsorted_segments`](AspectStorageStats::unsorted_segments) — after this call
	/// it is zero and every point lookup over the aspect binary-searches.
	///
	/// Each out-of-order segment is reconciled in place by
	/// [`reconcile_segment`](SegmentStore::reconcile_segment); already-sorted segments
	/// are skipped. Segments are visited in seal-id order.
	///
	/// # Errors
	///
	/// As [`reconcile_segment`](SegmentStore::reconcile_segment).
	pub async fn reconcile_aspect(&self, aspect: &str) -> Result<usize> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		let held = self.maintain(aspect).await?;
		self.reconcile_aspect_held(&held).await
	}

	/// [`reconcile_aspect`](Self::reconcile_aspect) under the maintenance lock `held`.
	async fn reconcile_aspect_held(&self, held: &MaintGuard) -> Result<usize> {
		self.writable()?;
		aspect_name::validate(held.aspect())?;
		let unsorted_ids: Vec<u64> = self.index.all(held.aspect()).await?.into_iter().filter(|d| !d.time_sorted).map(|d| d.id).collect();
		let mut reconciled = 0;
		for id in unsorted_ids {
			if self.reconcile_segment_held(held, id).await? {
				reconciled += 1;
			}
		}
		Ok(reconciled)
	}

	/// **Threshold-triggered reconciliation** (roadmap Phase 4.6): run
	/// [`reconcile_aspect`](SegmentStore::reconcile_aspect) **only** when the aspect's
	/// out-of-order backlog has grown past `threshold` out-of-order segments,
	/// otherwise leave it untouched.
	///
	/// This is the trigger primitive an automatic background reconcile/compaction
	/// pass keys on, modeled on the QuestDB-style split-count squash policy — that
	/// engine does not rewrite on every late row; it accumulates split partitions and squashes only
	/// once their number crosses `cairo.o3.last.partition.max.splits` (default 20). The
	/// `unsorted_segments` order-health count is WeftDB's analogue: below the threshold the
	/// backlog is cheap enough to answer with a per-segment linear scan, so the write
	/// amplification of a rewrite is not yet worth paying; at or above it the read cost
	/// dominates and a pass is triggered. *(src: automatic squash past a split
	/// threshold — <https://questdb.com/docs/concepts/partitions/>)*
	///
	/// The count is read from the durable index (the same signal
	/// [`aspect_stats`](SegmentStore::aspect_stats) reports), so a caller can poll this
	/// cheaply and only pay the rewrite when it fires.
	///
	/// Returns `Some(reconciled)` — the number of segments rewritten sorted — when the
	/// pass fired (the backlog was `>= threshold`), and `None` when it did not (the
	/// backlog was below `threshold`, so nothing was read or rewritten). A `threshold`
	/// of 0 is clamped to 1: a pass over a fully-ordered aspect would rewrite nothing,
	/// so the smallest meaningful trigger is "at least one out-of-order segment".
	///
	/// # Errors
	///
	/// As [`reconcile_aspect`](SegmentStore::reconcile_aspect); also propagates the
	/// index read behind the backlog count.
	pub async fn reconcile_aspect_if_unsorted_exceeds(&self, aspect: &str, threshold: usize) -> Result<Option<usize>> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		let held = self.maintain(aspect).await?;
		self.reconcile_aspect_if_unsorted_exceeds_held(&held, threshold).await
	}

	/// [`reconcile_aspect_if_unsorted_exceeds`](Self::reconcile_aspect_if_unsorted_exceeds)
	/// under the maintenance lock `held`.
	async fn reconcile_aspect_if_unsorted_exceeds_held(&self, held: &MaintGuard, threshold: usize) -> Result<Option<usize>> {
		self.writable()?;
		aspect_name::validate(held.aspect())?;
		let threshold = threshold.max(1);
		let unsorted = self.index.load_index(held.aspect()).await?.unsorted_count();
		if unsorted < threshold {
			return Ok(None);
		}
		Ok(Some(self.reconcile_aspect_held(held).await?))
	}

	/// **Store-wide threshold sweep** (roadmap Phase 4.6): apply the
	/// [`reconcile_aspect_if_unsorted_exceeds`](SegmentStore::reconcile_aspect_if_unsorted_exceeds)
	/// trigger to **every** declared aspect, reconciling only those whose out-of-order
	/// backlog is at or above `threshold`.
	///
	/// This is the tick an automatic background reconcile daemon calls: on a timer it
	/// sweeps the store's aspects, pays the rewrite only for the ones over threshold,
	/// and leaves the rest untouched. Aspects are visited in declared-name order; a
	/// `threshold` of 0 clamps to 1 (as for the single-aspect trigger). Returns a
	/// [`ReconcileSweep`] summary — how many aspects were scanned, how many fired, and
	/// the total segments rewritten — the numbers a daemon logs and exports per tick.
	///
	/// A per-aspect failure (as [`reconcile_aspect_if_unsorted_exceeds`](SegmentStore::reconcile_aspect_if_unsorted_exceeds))
	/// is recorded in [`ReconcileSweep::failed`] and the sweep continues with the next
	/// aspect, so one bad aspect cannot stall maintenance of every aspect after it.
	/// An aspect another maintenance operation holds is skipped or waited for as `wait`
	/// says, and listed in [`ReconcileSweep::busy`] if the sweep could not take it.
	///
	/// # Errors
	///
	/// Propagates only the aspect-list read; per-aspect failures are reported in
	/// [`ReconcileSweep::failed`].
	pub async fn reconcile_all_over_threshold(&self, threshold: usize, wait: MaintenanceWait) -> Result<ReconcileSweep> {
		self.writable()?;
		let aspects = self.list_declared_aspects().await?;
		let deadline = wait.deadline();
		let mut sweep = ReconcileSweep { aspects_scanned: aspects.len(), ..ReconcileSweep::default() };
		for aspect in &aspects {
			let Some(held) = self.sweep_maintain(aspect, deadline).await else {
				sweep.busy.push(aspect.clone());
				continue;
			};
			match self.reconcile_aspect_if_unsorted_exceeds_held(&held, threshold).await {
				Ok(Some(reconciled)) => {
					sweep.aspects_reconciled += 1;
					sweep.segments_reconciled += reconciled;
				}
				Ok(None) => {}
				Err(err) => sweep.failed.push((aspect.clone(), err)),
			}
		}
		Ok(sweep)
	}

	/// **Hot/cold threshold reconciliation** (roadmap Phase 4.6): reconcile an aspect's
	/// out-of-order segments the way `QuestDB` squashes partitions — **cold** (sealed,
	/// no-longer-appended) segments are rewritten on every pass, while the **hot tail**
	/// (the most-recently sealed segment, still the append target) is deferred until the
	/// aspect's out-of-order backlog reaches `threshold`.
	///
	/// The plain [`reconcile_aspect_if_unsorted_exceeds`](SegmentStore::reconcile_aspect_if_unsorted_exceeds)
	/// gate is all-or-nothing: below the threshold it leaves *every* out-of-order
	/// segment alone, so a cold segment that will never be appended again waits behind
	/// the hot tail's split budget. That over-defers — a cold segment's rewrite is a
	/// one-time cost that a later append cannot undo, so paying it eagerly is strictly
	/// cheaper than carrying its linear-scan point-lookup cost. Only the hot tail is
	/// worth deferring: rewriting the actively-growing segment on every tick would
	/// rewrite the same bytes repeatedly. This method reconciles the cold segments
	/// unconditionally and gates only the hot tail on the backlog, matching `QuestDB`'s
	/// "squash non-active partitions each commit, defer the active partition until the
	/// split threshold" policy. *(src: <https://questdb.com/docs/concepts/partitions/>)*
	///
	/// The **hot tail** is the segment with the largest id — ids are assigned
	/// monotonically on seal, so the newest is the append target. An already-sorted hot
	/// tail needs no rewrite regardless. The backlog is the aspect's out-of-order
	/// segment count observed at entry (before any rewrite this pass), so the hot-tail
	/// decision does not shift as cold segments drop out of the count.
	///
	/// `threshold` is clamped to 1 (as for the plain trigger). Returns a
	/// [`HotColdReconcile`] splitting the rewrite count into cold vs hot.
	///
	/// # Errors
	///
	/// As [`reconcile_segment`](SegmentStore::reconcile_segment); also propagates the
	/// index read behind the segment list.
	pub async fn reconcile_aspect_hot_cold(&self, aspect: &str, threshold: usize) -> Result<HotColdReconcile> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		let held = self.maintain(aspect).await?;
		self.reconcile_aspect_hot_cold_held(&held, threshold).await
	}

	/// [`reconcile_aspect_hot_cold`](Self::reconcile_aspect_hot_cold) under the
	/// maintenance lock `held`.
	async fn reconcile_aspect_hot_cold_held(&self, held: &MaintGuard, threshold: usize) -> Result<HotColdReconcile> {
		self.writable()?;
		aspect_name::validate(held.aspect())?;
		let threshold = threshold.max(1);
		let descriptors = self.index.all(held.aspect()).await?;
		let hot_tail_id = descriptors.iter().map(|d| d.id).max();
		let unsorted: Vec<u64> = descriptors.iter().filter(|d| !d.time_sorted).map(|d| d.id).collect();
		let hot_fires = unsorted.len() >= threshold;
		let mut out = HotColdReconcile::default();
		for id in unsorted {
			if Some(id) == hot_tail_id {
				// The hot tail is deferred until the backlog reaches the threshold.
				if hot_fires && self.reconcile_segment_held(held, id).await? {
					out.hot_reconciled += 1;
				}
			} else if self.reconcile_segment_held(held, id).await? {
				// A cold segment is always worth reconciling.
				out.cold_reconciled += 1;
			}
		}
		Ok(out)
	}

	/// **Store-wide hot/cold reconcile sweep** (roadmap Phase 4.6): apply
	/// [`reconcile_aspect_hot_cold`](SegmentStore::reconcile_aspect_hot_cold) to **every**
	/// declared aspect — eagerly reconciling each aspect's cold segments while deferring
	/// its hot tail until that aspect's own backlog reaches `threshold`.
	///
	/// This is the hot/cold analogue of
	/// [`reconcile_all_over_threshold`](SegmentStore::reconcile_all_over_threshold): the
	/// tick a background daemon calls when it wants cold segments cleaned up on every
	/// sweep without rewriting each aspect's actively-appended tail on every tick.
	/// Aspects are visited in declared-name order; a `threshold` of 0 clamps to 1.
	/// Returns a [`HotColdSweep`] — aspects scanned, aspects that rewrote at least one
	/// segment, and the cold/hot rewrite split — the numbers a daemon logs and exports.
	///
	/// A per-aspect failure (as [`reconcile_aspect_hot_cold`](SegmentStore::reconcile_aspect_hot_cold))
	/// is recorded in [`HotColdSweep::failed`] and the sweep continues with the next aspect.
	/// An aspect another maintenance operation holds is skipped or waited for as `wait`
	/// says, and listed in [`HotColdSweep::busy`] if the sweep could not take it.
	///
	/// # Errors
	///
	/// Propagates only the aspect-list read; per-aspect failures are reported in
	/// [`HotColdSweep::failed`].
	pub async fn reconcile_all_hot_cold(&self, threshold: usize, wait: MaintenanceWait) -> Result<HotColdSweep> {
		self.writable()?;
		let aspects = self.list_declared_aspects().await?;
		let deadline = wait.deadline();
		let mut sweep = HotColdSweep { aspects_scanned: aspects.len(), ..HotColdSweep::default() };
		for aspect in &aspects {
			let Some(held) = self.sweep_maintain(aspect, deadline).await else {
				sweep.busy.push(aspect.clone());
				continue;
			};
			match self.reconcile_aspect_hot_cold_held(&held, threshold).await {
				Ok(outcome) if outcome.total() > 0 => {
					sweep.aspects_reconciled += 1;
					sweep.cold_reconciled += outcome.cold_reconciled;
					sweep.hot_reconciled += outcome.hot_reconciled;
				}
				Ok(_) => {}
				Err(err) => sweep.failed.push((aspect.clone(), err)),
			}
		}
		Ok(sweep)
	}

	/// **Cross-segment out-of-order merge** (roadmap Phase 4.6): collapse every group
	/// of time-**overlapping** segments of `aspect` into a single time-sorted segment,
	/// resolving late data that re-entered an already-covered window.
	///
	/// Where [`reconcile_segment`](SegmentStore::reconcile_segment) fixes *intra*-segment
	/// disorder (the [`unsorted_segments`](AspectStorageStats::unsorted_segments) signal),
	/// this fixes *cross*-segment overlap (the
	/// [`overlapping_segments`](AspectStorageStats::overlapping_segments) signal): two
	/// internally-sorted segments whose windows intersect. Each connected component of
	/// overlapping segments (transitive time overlap) is merged into one segment at the
	/// component's **lowest id** and the other members are dropped (index row + file),
	/// so afterwards [`SegmentIndex::overlapping_count`](weft_physical_type::SegmentIndex::overlapping_count)
	/// is zero and a point/range read over the merged window opens a single segment.
	///
	/// **Merge semantics — newer wins (upsert).** Members are folded in ascending seal
	/// id (oldest → newest) with [`merge_newer_wins`], so at any timestamp two members
	/// share, the more-recently-sealed value supersedes the older one — the same
	/// last-writer-wins answer [`read_point`](SegmentStore::read_point) already gives
	/// across overlapping segments. This *dedups* cross-segment duplicate timestamps
	/// (a range read no longer returns the superseded row), which is the intended
	/// reconciliation/upsert behaviour. Rows at a timestamp carried by only one member
	/// — including that member's own internal duplicates — are preserved.
	///
	/// Each overlap component is either fully rewritten into one single-block segment
	/// (a paged member is compacted to single-block) or, when a large **cold prefix**
	/// dominates, **split** (per [`SplitPolicy`]) into an untouched-going-forward prefix
	/// segment plus a merged hot-suffix segment — see
	/// [`reconcile_overlaps_with_policy`](SegmentStore::reconcile_overlaps_with_policy).
	/// Non-overlapping segments are left untouched. The materialized rollup is rebuilt
	/// from the durable index afterward. This entry point uses
	/// [`SplitPolicy::questdb_default`] (a 50 MiB split floor), so components of small
	/// segments always take the full-rewrite path.
	///
	/// Returns the net reduction in segment count across all components — `members − 1`
	/// per fully-rewritten component and `members − 2` per split one (a split keeps two
	/// segments); zero when no two segments overlap (a no-op).
	///
	/// # Errors
	///
	/// Returns an error if `aspect` has no declared schema; propagates a filesystem
	/// read/write error, a decode/re-seal failure, or a libSQL failure.
	/// A [`MaintenanceBusy`] error if another maintenance operation holds the aspect for
	/// longer than [`maintenance_wait`](Self::maintenance_wait).
	pub async fn reconcile_overlaps(&self, aspect: &str) -> Result<usize> {
		self.reconcile_overlaps_with_policy(aspect, SplitPolicy::questdb_default()).await
	}

	/// **Cross-segment overlap merge with an explicit split policy** (roadmap Phase 4.6
	/// — the split-not-rewrite path). As [`reconcile_overlaps`](SegmentStore::reconcile_overlaps),
	/// but `policy` governs whether each overlap component is fully rewritten or split.
	///
	/// **Merge semantics — newer wins (upsert).** Every component's members are folded
	/// oldest → newest with [`merge_newer_wins`], so at any shared timestamp the
	/// more-recently-sealed value supersedes the older one (dedup / last-writer-wins),
	/// exactly as [`reconcile_overlaps`](SegmentStore::reconcile_overlaps) documents. The
	/// *logical* result is identical regardless of `policy`; only the on-disk layout
	/// differs.
	///
	/// **Split-not-rewrite.** A component's **cold prefix** is the run of merged rows
	/// before its second-earliest member starts — those timestamps are carried by a
	/// single member, so they saw no cross-segment overlap and need no merge. When that
	/// prefix both clears the policy's [`min_split_bytes`](SplitPolicy::min_split_bytes)
	/// floor and outweighs the hot suffix ([`SplitPolicy::decide`] → [`Split`](SplitDecision::Split)),
	/// the component is laid out as **two** segments — the cold prefix re-sealed at the
	/// lowest id and the merged hot suffix at a fresh id — instead of one. The two are
	/// disjoint in time (prefix < boundary ≤ suffix), so [`overlapping_count`](weft_physical_type::SegmentIndex::overlapping_count)
	/// still lands at zero, and a **later** late arrival that re-enters only the hot
	/// window forms an overlap component with the suffix alone: the cold prefix is never
	/// pulled back in and never rewritten again, bounding write amplification over the
	/// series' lifetime the way `QuestDB`'s partition split does. *(This pass still reads
	/// and rewrites the prefix once to carve it; the saving is amortized over subsequent
	/// reconciles — <https://questdb.com/docs/concepts/partitions/>)*
	///
	/// Byte sizes for the decision are estimated from the component's on-disk bytes and
	/// row counts (uniform per-row encoding), with the merged tail treated as new data
	/// via `decide(prefix, suffix, 0)`.
	///
	/// # Errors
	///
	/// As [`reconcile_overlaps`](SegmentStore::reconcile_overlaps).
	pub async fn reconcile_overlaps_with_policy(&self, aspect: &str, policy: SplitPolicy) -> Result<usize> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		let held = self.maintain(aspect).await?;
		self.reconcile_overlaps_held(&held, policy).await
	}

	/// [`reconcile_overlaps_with_policy`](Self::reconcile_overlaps_with_policy) under the
	/// maintenance lock `held`.
	async fn reconcile_overlaps_held(&self, held: &MaintGuard, policy: SplitPolicy) -> Result<usize> {
		self.writable()?;
		let aspect = held.aspect();
		aspect_name::validate(aspect)?;
		let descriptors = self.index.all(aspect).await?;
		// Components of transitively time-overlapping segments: sort spans by (min_ts,
		// max_ts), sweep, and start a new component whenever a span begins after the
		// running max end of the current one.
		let mut spans: Vec<(u64, i64, i64)> = descriptors.iter().filter_map(|d| d.time_range().map(|(lo, hi)| (d.id, lo, hi))).collect();
		spans.sort_by_key(|&(_, lo, hi)| (lo, hi));
		let mut components: Vec<Vec<u64>> = Vec::new();
		let mut running_max_hi = i64::MIN;
		for (id, lo, hi) in spans {
			match components.last_mut() {
				Some(component) if lo <= running_max_hi => {
					component.push(id);
					running_max_hi = running_max_hi.max(hi);
				}
				_ => {
					components.push(vec![id]);
					running_max_hi = hi;
				}
			}
		}
		let schema = self.require_schema(aspect).await?;
		let mut removed = 0;
		let mut changed = false;
		for mut component in components {
			if component.len() < 2 {
				continue;
			}
			changed = true;
			// Fold members oldest → newest so the most-recently-sealed value wins a tie.
			component.sort_unstable();
			let mut merged: Vec<(i64, Option<BigDecimal>)> = Vec::new();
			let mut component_bytes = 0_u64;
			let mut component_rows = 0_usize;
			// The second-earliest member start: rows before it are the cold prefix (carried
			// by one member, no cross-segment overlap). `starts` is never empty — every
			// component member has a time range (components are built from `time_range`).
			let mut starts: Vec<i64> = Vec::with_capacity(component.len());
			for &id in &component {
				let descriptor = descriptors.iter().find(|d| d.id == id).ok_or_else(|| anyhow::anyhow!("aspect {aspect:?} lost segment {id} mid-merge"))?;
				component_bytes += descriptor.byte_len;
				component_rows += descriptor.row_count;
				if let Some((lo, _)) = descriptor.time_range() {
					starts.push(lo);
				}
				let (ts, vs) = self.decode_all(descriptor).await?;
				let mut rows: Vec<(i64, Option<BigDecimal>)> = ts.into_iter().zip(vs).collect();
				// Each member may be internally out of order; sort before merging.
				rows.sort_by_key(|(t, _)| *t);
				merged = merge_newer_wins(&merged, &rows);
			}
			starts.sort_unstable();
			let cold_boundary = starts.get(1).copied().unwrap_or(i64::MIN);
			let (all_ts, all_vs): (Vec<i64>, Vec<Option<BigDecimal>>) = merged.into_iter().unzip();
			// The cold prefix: merged rows strictly before the second member's start.
			let prefix_len = all_ts.partition_point(|&t| t < cold_boundary);
			// Estimate prefix/suffix bytes from the component's uniform per-row size.
			let bytes_per_row = if component_rows > 0 { component_bytes / component_rows as u64 } else { 0 };
			let prefix_bytes = bytes_per_row.saturating_mul(prefix_len as u64);
			let suffix_bytes = bytes_per_row.saturating_mul((all_ts.len() - prefix_len) as u64);
			let split = prefix_len > 0 && prefix_len < all_ts.len() && policy.decide(prefix_bytes, suffix_bytes, 0) == SplitDecision::Split;
			let target = component[0];
			if split {
				// Carve the cold prefix into the lowest id and the hot suffix into a fresh
				// id from the allocator seals use; the two are disjoint so no cross-segment
				// overlap remains. Write the suffix first so a crash duplicates rather than
				// drops rows.
				let suffix_id = self.allocate_id(aspect).await?;
				self.reseal_nullable_fresh(aspect, &schema, suffix_id, &all_ts[prefix_len..], &all_vs[prefix_len..], None).await?;
				self.reseal_nullable_at(aspect, &schema, target, &all_ts[..prefix_len], &all_vs[..prefix_len], None).await?;
			} else {
				// Full rewrite: the whole merged component into the lowest id.
				self.reseal_nullable_at(aspect, &schema, target, &all_ts, &all_vs, None).await?;
			}
			// Drop the other members: control-plane row then the file (and its sidecar).
			for &id in component.iter().skip(1) {
				self.remove_descriptor(aspect, id).await?;
				let victim = self.segment_path(aspect, id)?;
				tokio::fs::remove_file(&victim).await.with_context(|| format!("removing merged-away segment {}", victim.display()))?;
				// A merged-away segment's sidecar is now orphaned — drop it too.
				self.remove_sidecar(aspect, id).await?;
			}
			// Net segment reduction: a full rewrite keeps 1, a split keeps 2 (so a
			// two-member split reduces the count by zero while still changing the set).
			removed += component.len() - if split { 2 } else { 1 };
		}
		if changed {
			// A merge/split changed the segment set; recompute the rollup from the index.
			self.rebuild_aspect_metadata(aspect).await?;
		}
		Ok(removed)
	}

	/// **Store-wide cross-segment overlap merge** (roadmap Phase 4.6): apply
	/// [`reconcile_overlaps`](SegmentStore::reconcile_overlaps) to **every** declared
	/// aspect, merging each aspect's time-overlap groups.
	///
	/// The store-wide counterpart to the per-aspect merge — the tick a background
	/// daemon calls to keep cross-segment overlap from accumulating store-wide. Aspects
	/// are visited in declared-name order. Returns an [`OverlapSweep`] — how many
	/// aspects were scanned, how many actually merged anything, and the total segments
	/// removed — the numbers a daemon logs and exports.
	///
	/// A per-aspect failure (as [`reconcile_overlaps`](SegmentStore::reconcile_overlaps))
	/// is recorded in [`OverlapSweep::failed`] and the sweep continues with the next aspect.
	/// An aspect another maintenance operation holds is skipped or waited for as `wait`
	/// says, and listed in [`OverlapSweep::busy`] if the sweep could not take it.
	///
	/// # Errors
	///
	/// Propagates only the aspect-list read; per-aspect failures are reported in
	/// [`OverlapSweep::failed`].
	pub async fn reconcile_all_overlaps(&self, wait: MaintenanceWait) -> Result<OverlapSweep> {
		self.writable()?;
		let aspects = self.list_declared_aspects().await?;
		let deadline = wait.deadline();
		let mut sweep = OverlapSweep { aspects_scanned: aspects.len(), ..OverlapSweep::default() };
		for aspect in &aspects {
			let Some(held) = self.sweep_maintain(aspect, deadline).await else {
				sweep.busy.push(aspect.clone());
				continue;
			};
			match self.reconcile_overlaps_held(&held, SplitPolicy::questdb_default()).await {
				Ok(removed) if removed > 0 => {
					sweep.aspects_reconciled += 1;
					sweep.segments_removed += removed;
				}
				Ok(_) => {}
				Err(err) => sweep.failed.push((aspect.clone(), err)),
			}
		}
		Ok(sweep)
	}

	/// **Store-wide overlap merge with an explicit split policy** (roadmap Phase 4.6 —
	/// the split-not-rewrite path). As [`reconcile_all_overlaps`](SegmentStore::reconcile_all_overlaps),
	/// but every aspect's merge runs under `policy` via
	/// [`reconcile_overlaps_with_policy`](SegmentStore::reconcile_overlaps_with_policy),
	/// so a dominant cold prefix is split off rather than fully rewritten.
	///
	/// An aspect is counted as reconciled when it **carried cross-segment overlap**
	/// before the pass (its [`overlapping_segments`](AspectStorageStats::overlapping_segments)
	/// was non-zero — exactly the aspects the merge acts on), rather than by the net
	/// removed count: a two-member split changes the layout while leaving the segment
	/// count unchanged, so a `removed > 0` test would miss it. `segments_removed` remains
	/// the honest **net** reduction (zero for a pure split). For the full-rewrite path
	/// (the default 50 MiB floor) this counts identically to
	/// [`reconcile_all_overlaps`](SegmentStore::reconcile_all_overlaps), since there
	/// overlap-present ⟺ a segment is removed.
	///
	/// A per-aspect failure (the pre-pass overlap read or the merge itself) is recorded in
	/// [`OverlapSweep::failed`] and the sweep continues with the next aspect.
	/// An aspect another maintenance operation holds is skipped or waited for as `wait`
	/// says, and listed in [`OverlapSweep::busy`] if the sweep could not take it.
	///
	/// # Errors
	///
	/// As [`reconcile_all_overlaps`](SegmentStore::reconcile_all_overlaps).
	pub async fn reconcile_all_overlaps_with_policy(&self, policy: SplitPolicy, wait: MaintenanceWait) -> Result<OverlapSweep> {
		self.writable()?;
		let aspects = self.list_declared_aspects().await?;
		let deadline = wait.deadline();
		let mut sweep = OverlapSweep { aspects_scanned: aspects.len(), ..OverlapSweep::default() };
		for aspect in &aspects {
			let Some(held) = self.sweep_maintain(aspect, deadline).await else {
				sweep.busy.push(aspect.clone());
				continue;
			};
			let pass = async {
				let had_overlap = self.index.load_index(aspect).await?.overlapping_count() > 0;
				let removed = self.reconcile_overlaps_held(&held, policy).await?;
				anyhow::Ok((had_overlap, removed))
			};
			match pass.await {
				Ok((true, removed)) => {
					sweep.aspects_reconciled += 1;
					sweep.segments_removed += removed;
				}
				Ok((false, _)) => {}
				Err(err) => sweep.failed.push((aspect.clone(), err)),
			}
		}
		Ok(sweep)
	}

	/// **Squash all of an aspect's segments into one** (roadmap Phase 4.6 — the squash
	/// half of the split-not-rewrite path).
	///
	/// Repeated late arrivals under the split path accumulate small time-disjoint
	/// segments (a growing pile of carved-off cold prefixes). Left unchecked that
	/// fragments an aspect and multiplies the per-segment read/prune overhead. Squash is
	/// the QuestDB-style bound on that fragmentation: it folds **every** segment of
	/// `aspect` — in ascending seal id, [`merge_newer_wins`] so any residual shared
	/// timestamp still resolves last-writer-wins — into a single time-sorted single-block
	/// segment at the lowest id, dropping the rest. After it, the aspect is one segment
	/// with no cross-segment overlap. The natural trigger is a segment count past a
	/// threshold (see [`squash_aspect_if_exceeds`](SegmentStore::squash_aspect_if_exceeds));
	/// squashing trades the split path's low write amplification for a low segment count,
	/// so it is meant to fire rarely, not every commit.
	///
	/// Returns the number of segments removed (`count − 1`); zero when the aspect has
	/// fewer than two segments (nothing to squash).
	///
	/// # Errors
	///
	/// Returns an error if `aspect` has no declared schema; propagates a filesystem
	/// read/write error, a decode/re-seal failure, or a libSQL failure.
	/// A [`MaintenanceBusy`] error if another maintenance operation holds the aspect for
	/// longer than [`maintenance_wait`](Self::maintenance_wait).
	pub async fn squash_aspect(&self, aspect: &str) -> Result<usize> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		let held = self.maintain(aspect).await?;
		self.squash_aspect_held(&held).await
	}

	/// [`squash_aspect`](Self::squash_aspect) under the maintenance lock `held`.
	async fn squash_aspect_held(&self, held: &MaintGuard) -> Result<usize> {
		self.writable()?;
		let aspect = held.aspect();
		aspect_name::validate(aspect)?;
		let descriptors = self.index.all(aspect).await?;
		if descriptors.len() < 2 {
			return Ok(0);
		}
		let schema = self.require_schema(aspect).await?;
		let mut ids: Vec<u64> = descriptors.iter().map(|d| d.id).collect();
		ids.sort_unstable();
		// Fold oldest → newest so the most-recently-sealed value wins any residual tie.
		let mut merged: Vec<(i64, Option<BigDecimal>)> = Vec::new();
		for &id in &ids {
			let descriptor = descriptors.iter().find(|d| d.id == id).ok_or_else(|| anyhow::anyhow!("aspect {aspect:?} lost segment {id} mid-squash"))?;
			let (ts, vs) = self.decode_all(descriptor).await?;
			let mut rows: Vec<(i64, Option<BigDecimal>)> = ts.into_iter().zip(vs).collect();
			rows.sort_by_key(|(t, _)| *t);
			merged = merge_newer_wins(&merged, &rows);
		}
		let (all_ts, all_vs): (Vec<i64>, Vec<Option<BigDecimal>>) = merged.into_iter().unzip();
		let target = ids[0];
		self.reseal_nullable_at(aspect, &schema, target, &all_ts, &all_vs, None).await?;
		for &id in ids.iter().skip(1) {
			self.remove_descriptor(aspect, id).await?;
			let victim = self.segment_path(aspect, id)?;
			tokio::fs::remove_file(&victim).await.with_context(|| format!("removing squashed-away segment {}", victim.display()))?;
			// The squashed-away segment's sidecar is now orphaned — drop it too.
			self.remove_sidecar(aspect, id).await?;
		}
		self.rebuild_aspect_metadata(aspect).await?;
		Ok(ids.len() - 1)
	}

	/// **Threshold-triggered squash** (roadmap Phase 4.6): run
	/// [`squash_aspect`](SegmentStore::squash_aspect) **only** when `aspect` has more than
	/// `max_segments` segments, otherwise leave it untouched.
	///
	/// The trigger a background compaction pass keys on to cap split-path fragmentation,
	/// modeled on `QuestDB`'s `cairo.o3.last.partition.max.splits` squash threshold: below
	/// the cap the fragmentation is cheap enough to tolerate, so the squash's write cost
	/// is not yet worth paying; above it the read/prune overhead of many segments
	/// dominates and a squash fires. A `max_segments` of 0 clamps to 1 (an aspect can
	/// never squash below a single segment).
	///
	/// Returns `Some(removed)` when the squash fired (the count was `> max_segments`) and
	/// `None` when it held. The count is read from the durable index, so a caller can
	/// poll cheaply and pay the rewrite only when it fires.
	///
	/// # Errors
	///
	/// As [`squash_aspect`](SegmentStore::squash_aspect); also propagates the segment-count read.
	pub async fn squash_aspect_if_exceeds(&self, aspect: &str, max_segments: usize) -> Result<Option<usize>> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		let held = self.maintain(aspect).await?;
		self.squash_aspect_if_exceeds_held(&held, max_segments).await
	}

	/// [`squash_aspect_if_exceeds`](Self::squash_aspect_if_exceeds) under the maintenance
	/// lock `held`.
	async fn squash_aspect_if_exceeds_held(&self, held: &MaintGuard, max_segments: usize) -> Result<Option<usize>> {
		self.writable()?;
		aspect_name::validate(held.aspect())?;
		let max_segments = max_segments.max(1);
		if self.index.count(held.aspect()).await? <= max_segments {
			return Ok(None);
		}
		Ok(Some(self.squash_aspect_held(held).await?))
	}

	/// **Store-wide threshold squash** (roadmap Phase 4.6): apply the
	/// [`squash_aspect_if_exceeds`](SegmentStore::squash_aspect_if_exceeds) trigger to
	/// **every** declared aspect, squashing only those whose segment count exceeds
	/// `max_segments`.
	///
	/// The tick a background squash daemon calls to cap split-path fragmentation
	/// store-wide: on a timer it sweeps the aspects, pays the squash only for the ones
	/// over the cap, and leaves the rest untouched. Aspects are visited in declared-name
	/// order; a `max_segments` of 0 clamps to 1. Returns a [`SquashSweep`] — how many
	/// aspects were scanned, how many were squashed, and the total segments removed.
	///
	/// A per-aspect failure (as [`squash_aspect_if_exceeds`](SegmentStore::squash_aspect_if_exceeds))
	/// is recorded in [`SquashSweep::failed`] and the sweep continues with the next aspect.
	/// An aspect another maintenance operation holds is skipped or waited for as `wait`
	/// says, and listed in [`SquashSweep::busy`] if the sweep could not take it.
	///
	/// # Errors
	///
	/// Propagates only the aspect-list read; per-aspect failures are reported in
	/// [`SquashSweep::failed`].
	pub async fn squash_all_over_threshold(&self, max_segments: usize, wait: MaintenanceWait) -> Result<SquashSweep> {
		self.writable()?;
		let aspects = self.list_declared_aspects().await?;
		let deadline = wait.deadline();
		let mut sweep = SquashSweep { aspects_scanned: aspects.len(), ..SquashSweep::default() };
		for aspect in &aspects {
			let Some(held) = self.sweep_maintain(aspect, deadline).await else {
				sweep.busy.push(aspect.clone());
				continue;
			};
			match self.squash_aspect_if_exceeds_held(&held, max_segments).await {
				Ok(Some(removed)) => {
					sweep.aspects_squashed += 1;
					sweep.segments_removed += removed;
				}
				Ok(None) => {}
				Err(err) => sweep.failed.push((aspect.clone(), err)),
			}
		}
		Ok(sweep)
	}

	/// **Compact an aspect toward a target segment size** (roadmap Phase 4.6 — the
	/// size-aware counterpart of [`squash_aspect`](SegmentStore::squash_aspect)).
	///
	/// [`squash_aspect`](SegmentStore::squash_aspect) folds *every* segment into one, which
	/// the `downsample_range` segment-count bench showed over-corrects: at a fixed row count
	/// a single large segment reads **slower** than a handful of mid-sized ones (one
	/// 200k-row segment measured ~32.8 ms vs ~9.7 ms for sixteen ~12.5k-row segments — a
	/// single segment offers no cross-segment parallelism and decodes its whole column in
	/// one shot, while too many segments pay a per-segment fixed cost). This compaction
	/// instead coalesces **consecutive** (seal-id-ordered) segments into groups whose
	/// combined row count first reaches `target_rows`, re-sealing each multi-segment group
	/// into one segment at the group's lowest id and leaving already-large segments
	/// untouched — folding an over-fragmented aspect toward ~`target_rows`-sized segments
	/// rather than a single giant one.
	///
	/// Grouping is by ascending seal id (≈ time order for append-mostly ingest); each group
	/// merges oldest→newest via [`merge_newer_wins`] so a residual shared timestamp still
	/// resolves last-writer-wins, and the merged rows are time-sorted so every resealed
	/// segment is sorted. A `target_rows` of 0 clamps to 1, so every segment forms its own
	/// singleton group and the call is a no-op. A single-segment group is never rewritten.
	///
	/// Returns the number of segments removed (`Σ (group_len − 1)`); zero when nothing
	/// coalesced (fewer than two segments, or every segment already ≥ `target_rows`).
	///
	/// # Errors
	///
	/// As [`squash_aspect`](SegmentStore::squash_aspect).
	pub async fn squash_aspect_to_target_rows(&self, aspect: &str, target_rows: usize) -> Result<usize> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		let held = self.maintain(aspect).await?;
		self.squash_aspect_to_target_rows_held(&held, target_rows).await
	}

	/// [`squash_aspect_to_target_rows`](Self::squash_aspect_to_target_rows) under the
	/// maintenance lock `held`.
	async fn squash_aspect_to_target_rows_held(&self, held: &MaintGuard, target_rows: usize) -> Result<usize> {
		self.writable()?;
		let aspect = held.aspect();
		aspect_name::validate(aspect)?;
		let target_rows = target_rows.max(1);
		let descriptors = self.index.all(aspect).await?;
		if descriptors.len() < 2 {
			return Ok(0);
		}
		let schema = self.require_schema(aspect).await?;
		let mut ids: Vec<u64> = descriptors.iter().map(|d| d.id).collect();
		ids.sort_unstable();
		// Greedily group consecutive ids until a group's cumulative row count first reaches
		// the target; a trailing partial group is kept as-is. A group that stays a singleton
		// (a segment already ≥ target, or the lone tail) is skipped below — never rewritten.
		let mut groups: Vec<Vec<u64>> = Vec::new();
		let mut group: Vec<u64> = Vec::new();
		let mut group_rows = 0usize;
		for &id in &ids {
			let rows = descriptors.iter().find(|d| d.id == id).map_or(0, |d| d.row_count);
			group.push(id);
			group_rows += rows;
			if group_rows >= target_rows {
				groups.push(std::mem::take(&mut group));
				group_rows = 0;
			}
		}
		if !group.is_empty() {
			groups.push(group);
		}
		let mut removed = 0usize;
		for group in groups {
			if group.len() < 2 {
				continue;
			}
			// Fold oldest → newest so the most-recently-sealed value wins any residual tie.
			let mut merged: Vec<(i64, Option<BigDecimal>)> = Vec::new();
			for &id in &group {
				let descriptor = descriptors.iter().find(|d| d.id == id).ok_or_else(|| anyhow::anyhow!("aspect {aspect:?} lost segment {id} mid-compaction"))?;
				let (ts, vs) = self.decode_all(descriptor).await?;
				let mut rows: Vec<(i64, Option<BigDecimal>)> = ts.into_iter().zip(vs).collect();
				rows.sort_by_key(|(t, _)| *t);
				merged = merge_newer_wins(&merged, &rows);
			}
			let (all_ts, all_vs): (Vec<i64>, Vec<Option<BigDecimal>>) = merged.into_iter().unzip();
			let target = group[0];
			self.reseal_nullable_at(aspect, &schema, target, &all_ts, &all_vs, None).await?;
			for &id in group.iter().skip(1) {
				self.remove_descriptor(aspect, id).await?;
				let victim = self.segment_path(aspect, id)?;
				tokio::fs::remove_file(&victim).await.with_context(|| format!("removing compacted-away segment {}", victim.display()))?;
				self.remove_sidecar(aspect, id).await?;
				removed += 1;
			}
		}
		// Only the resealed target ids remain of each group; a segment-set change means the
		// O(1) rollup fold would be wrong, so rebuild it once from the durable index.
		if removed > 0 {
			self.rebuild_aspect_metadata(aspect).await?;
		}
		Ok(removed)
	}

	/// **Store-wide size-targeted compaction** (roadmap Phase 4.6): apply
	/// [`squash_aspect_to_target_rows`](SegmentStore::squash_aspect_to_target_rows) to
	/// **every** declared aspect, coalescing each toward ~`target_rows`-sized segments.
	///
	/// The tick a background compaction daemon calls to hold fragmentation near the
	/// read-optimal segment size store-wide — the size-aware counterpart of
	/// [`squash_all_over_threshold`](SegmentStore::squash_all_over_threshold) (which folds
	/// each over-threshold aspect all the way to one). Aspects are visited in declared-name
	/// order. Returns a [`SquashSweep`]: how many aspects were scanned, how many actually
	/// coalesced at least one segment, and the total segments removed.
	///
	/// A per-aspect failure (as [`squash_aspect_to_target_rows`](SegmentStore::squash_aspect_to_target_rows))
	/// is recorded in [`SquashSweep::failed`] and the sweep continues with the next aspect.
	/// An aspect another maintenance operation holds is skipped or waited for as `wait`
	/// says, and listed in [`SquashSweep::busy`] if the sweep could not take it.
	///
	/// # Errors
	///
	/// Propagates only the aspect-list read; per-aspect failures are reported in
	/// [`SquashSweep::failed`].
	pub async fn squash_all_to_target_rows(&self, target_rows: usize, wait: MaintenanceWait) -> Result<SquashSweep> {
		self.writable()?;
		let aspects = self.list_declared_aspects().await?;
		let deadline = wait.deadline();
		let mut sweep = SquashSweep { aspects_scanned: aspects.len(), ..SquashSweep::default() };
		for aspect in &aspects {
			let Some(held) = self.sweep_maintain(aspect, deadline).await else {
				sweep.busy.push(aspect.clone());
				continue;
			};
			match self.squash_aspect_to_target_rows_held(&held, target_rows).await {
				Ok(removed) if removed > 0 => {
					sweep.aspects_squashed += 1;
					sweep.segments_removed += removed;
				}
				Ok(_) => {}
				Err(err) => sweep.failed.push((aspect.clone(), err)),
			}
		}
		Ok(sweep)
	}

	/// **Fragmentation-gated** size-targeted compaction of one aspect (roadmap Phase 4.6).
	///
	/// The cheap-first guard the background daemon uses so it does not scan every aspect's
	/// segment list on every tick. Reads the **O(1)** per-aspect rollup
	/// ([`AspectMetadataStore`](super::AspectMetadataStore)) for the segment count and total
	/// rows, computes the minimum segments needed to hold that many rows at `target_rows`
	/// (`ideal = ⌈total_rows / target_rows⌉`), and runs
	/// [`squash_aspect_to_target_rows`](SegmentStore::squash_aspect_to_target_rows) **only when
	/// the aspect carries more segments than that** — i.e. it is genuinely over-fragmented.
	/// A well-sized aspect returns `None` without ever reading its segment descriptors, so a
	/// converged aspect costs one control-plane rollup read per tick, not a full index scan.
	///
	/// Returns `Some(removed)` when the compaction ran, `None` when the aspect held (already
	/// as compact as the target allows, unknown, or empty). `target_rows` clamps to 1.
	///
	/// # Errors
	///
	/// As [`squash_aspect_to_target_rows`](SegmentStore::squash_aspect_to_target_rows); also
	/// propagates the rollup read.
	pub async fn squash_aspect_to_target_rows_if_fragmented(&self, aspect: &str, target_rows: usize) -> Result<Option<usize>> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		let held = self.maintain(aspect).await?;
		self.squash_aspect_to_target_rows_if_fragmented_held(&held, target_rows).await
	}

	/// [`squash_aspect_to_target_rows_if_fragmented`](Self::squash_aspect_to_target_rows_if_fragmented)
	/// under the maintenance lock `held`.
	async fn squash_aspect_to_target_rows_if_fragmented_held(&self, held: &MaintGuard, target_rows: usize) -> Result<Option<usize>> {
		self.writable()?;
		aspect_name::validate(held.aspect())?;
		let target_rows = target_rows.max(1);
		let Some(meta) = self.metadata.get(held.aspect()).await? else { return Ok(None) };
		// Minimum segments to hold total_rows at the target; a fully-compacted aspect sits at
		// exactly this count (or below), so more than this means there is fragmentation to fold.
		let ideal = meta.total_rows.div_ceil(target_rows as u64).max(1);
		if (meta.segment_count as u64) <= ideal {
			return Ok(None);
		}
		Ok(Some(self.squash_aspect_to_target_rows_held(held, target_rows).await?))
	}

	/// **Fragmentation-gated store-wide** size-targeted compaction (roadmap Phase 4.6).
	///
	/// The tick the background compaction daemon actually calls: applies
	/// [`squash_aspect_to_target_rows_if_fragmented`](SegmentStore::squash_aspect_to_target_rows_if_fragmented)
	/// to every declared aspect, so a converged store costs one O(1) rollup read per aspect
	/// per tick and rewrites nothing. Unlike [`squash_all_to_target_rows`](SegmentStore::squash_all_to_target_rows)
	/// (which unconditionally scans and coalesces every aspect — the "force" form a manual
	/// caller wants), this skips aspects already at or below their ideal segment count.
	/// Returns a [`SquashSweep`].
	///
	/// A per-aspect failure (as [`squash_aspect_to_target_rows_if_fragmented`](SegmentStore::squash_aspect_to_target_rows_if_fragmented))
	/// is recorded in [`SquashSweep::failed`] and the sweep continues with the next aspect.
	/// An aspect another maintenance operation holds is skipped or waited for as `wait`
	/// says, and listed in [`SquashSweep::busy`] if the sweep could not take it.
	///
	/// # Errors
	///
	/// Propagates only the aspect-list read; per-aspect failures are reported in
	/// [`SquashSweep::failed`].
	pub async fn squash_all_to_target_rows_if_fragmented(&self, target_rows: usize, wait: MaintenanceWait) -> Result<SquashSweep> {
		self.writable()?;
		let aspects = self.list_declared_aspects().await?;
		let deadline = wait.deadline();
		let mut sweep = SquashSweep { aspects_scanned: aspects.len(), ..SquashSweep::default() };
		for aspect in &aspects {
			let Some(held) = self.sweep_maintain(aspect, deadline).await else {
				sweep.busy.push(aspect.clone());
				continue;
			};
			match self.squash_aspect_to_target_rows_if_fragmented_held(&held, target_rows).await {
				Ok(Some(removed)) if removed > 0 => {
					sweep.aspects_squashed += 1;
					sweep.segments_removed += removed;
				}
				Ok(_) => {}
				Err(err) => sweep.failed.push((aspect.clone(), err)),
			}
		}
		Ok(sweep)
	}

	/// Read every present row of `aspect` whose **value** falls in the inclusive range
	/// `[lo, hi]`, **opening only the segment files whose value span overlaps it**.
	///
	/// The value column has no SQL ordering (the `BigDecimal` bounds are stored as
	/// text), so the pruning runs through the resident
	/// [`SegmentIndex`](weft_physical_type::SegmentIndex): it is loaded from the
	/// control plane and pruned by value, and only the surviving descriptors' files
	/// are opened. Within each opened segment the rows are filtered to those whose
	/// value is present and in `[lo, hi]`. Returns parallel `(timestamps, values)`
	/// vectors, in segment-seal then in-segment order.
	///
	/// # Errors
	///
	/// Propagates a libSQL read failure, a filesystem read error, or a
	/// [`weft_physical_type::weftseg::WeftSegError`] for a corrupt/unreadable `.weftseg`.
	pub async fn read_value_range(&self, aspect: &str, lo: &BigDecimal, hi: &BigDecimal) -> Result<(Vec<i64>, Vec<BigDecimal>)> {
		aspect_name::validate(aspect)?;
		let index = self.index.load_index(aspect).await?;
		let mut timestamps = Vec::new();
		let mut values = Vec::new();
		for descriptor in index.prune_by_value(lo, hi) {
			let (ts, vs) = self.decode_all(descriptor).await?;
			for (t, v) in ts.into_iter().zip(vs) {
				if let Some(value) = v {
					if lo <= &value && &value <= hi {
						timestamps.push(t);
						values.push(value);
					}
				}
			}
		}
		Ok((timestamps, values))
	}

	/// Read and fully decode one segment file (frame-version aware), returning all
	/// rows aligned `(timestamps, values)` with `None` at every null row.
	async fn decode_all(&self, descriptor: &SegmentDescriptor) -> Result<(Vec<i64>, Vec<Option<BigDecimal>>)> {
		let bytes = self.read_frame(descriptor).await?;
		if descriptor.format_version == PAGED_SEGMENT_FORMAT_VERSION {
			let segment = PagedSegment::read_from(&bytes).map_err(|e| anyhow::anyhow!("decoding paged segment {}: {e}", descriptor.path))?;
			Ok(segment.decode_nullable())
		} else {
			let segment = Segment::read_from(&bytes).map_err(|e| anyhow::anyhow!("decoding segment {}: {e}", descriptor.path))?;
			Ok(segment.decode_nullable())
		}
	}

	/// Number of sealed segments recorded for `aspect`.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn segment_count(&self, aspect: &str) -> Result<usize> {
		self.index.count(aspect).await
	}

	/// Realized storage accounting for `aspect` — the north-star **bytes/point** term
	/// (priority #1 in the commercial thesis) measured over the segments actually on
	/// disk, plus the segment/row counts and the covered time span.
	///
	/// Computed from the resident [`SegmentIndex`](weft_physical_type::SegmentIndex)
	/// (the descriptors' recorded framed byte lengths and row counts), so it reflects
	/// the realized `.weftseg` files including their header/index/checksum overhead,
	/// not an advisory column estimate. No segment file is opened.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn aspect_stats(&self, aspect: &str) -> Result<AspectStorageStats> {
		let index = self.index.load_index(aspect).await?;
		Ok(AspectStorageStats { segment_count: index.len(), total_rows: index.total_rows(), total_bytes: index.total_bytes(), bytes_per_point: index.bytes_per_point(), time_range: index.time_range(), unsorted_segments: index.unsorted_count(), overlapping_segments: index.overlapping_count() })
	}

	/// The **materialized** segment-set rollup for `aspect` — the same aspect-wide
	/// summary as [`aspect_stats`](SegmentStore::aspect_stats), but read as a single
	/// `metadata.db` row (O(1)) rather than scanning the whole segment index. An aspect
	/// with no sealed segments yields the empty rollup
	/// ([`AspectMetadata::default`]). The rollup also carries the aspect-wide value
	/// span, which the per-segment-scan stats do not.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn aspect_metadata(&self, aspect: &str) -> Result<AspectMetadata> {
		Ok(self.metadata.get(aspect).await?.unwrap_or_default())
	}

	/// Re-derive `aspect`'s materialized `metadata.db` rollup from the durable segment
	/// index and write it back, returning the reconciled rollup — the **authoritative**
	/// recovery path.
	///
	/// Where each seal folds one descriptor forward (O(1), but assumes it never sees the
	/// same segment twice), this recomputes the rollup from scratch via
	/// [`AspectMetadata::from_index`] over the whole index, so it is correct regardless
	/// of how the row got out of step (a crash between the index insert and the rollup
	/// fold, a re-seal of an id, a metadata.db restored from an older snapshot). The
	/// segment index is the source of truth; this makes the rollup match it.
	///
	/// It reads the index and writes the rollup under the aspect's commit lock, where a
	/// seal commits its row and folds it in, so a seal committing meanwhile is either in
	/// the index it reads or folded into the rollup after it writes, never lost.
	///
	/// # Errors
	///
	/// Propagates any libSQL read or write failure.
	pub async fn rebuild_aspect_metadata(&self, aspect: &str) -> Result<AspectMetadata> {
		self.writable()?;
		let commit = self.commit_lock(aspect).await?;
		let index = self.index.load_index(aspect).await?;
		let meta = AspectMetadata::from_index(&index);
		self.metadata.put(aspect, &meta).await?;
		drop(commit);
		Ok(meta)
	}

	/// Re-derive **every** aspect's rollup from the durable segment index, returning the
	/// number of aspects reconciled. Rebuilds the whole `metadata.db` from the index —
	/// the recovery path for a lost or stale rollup DB beside an intact index.
	///
	/// The aspect set comes from the index itself
	/// ([`SegmentIndexStore::list_aspects`]), so it reconciles exactly the aspects that
	/// have segments.
	///
	/// # Errors
	///
	/// Propagates any libSQL read or write failure.
	pub async fn rebuild_all_metadata(&self) -> Result<usize> {
		self.writable()?;
		let aspects = self.index.list_aspects().await?;
		let count = aspects.len();
		for aspect in aspects {
			self.rebuild_aspect_metadata(&aspect).await?;
		}
		Ok(count)
	}

	/// Aggregate every aspect's materialized rollup into one **store-wide** summary —
	/// the subject-wide north-star **bytes/point** (priority #1 in the commercial
	/// thesis) across all of this store's aspects, plus the rolled-up segment/row/null
	/// counts and the union of the aspects' time spans.
	///
	/// Built from the per-aspect `metadata.db` rows (one O(1) read per aspect), so it
	/// never opens a segment file or scans the segment index. The value span is
	/// deliberately omitted — a min/max across aspects of unrelated physical meaning
	/// would not be a useful number.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn store_stats(&self) -> Result<StoreStorageStats> {
		let aspects = self.metadata.list_aspects().await?;
		let mut stats = StoreStorageStats { aspect_count: aspects.len(), ..StoreStorageStats::default() };
		for aspect in &aspects {
			let meta = self.metadata.get(aspect).await?.unwrap_or_default();
			stats.segment_count += meta.segment_count;
			stats.total_rows += meta.total_rows;
			stats.total_nulls += meta.total_nulls;
			stats.total_bytes += meta.total_bytes;
			stats.unsorted_segments += meta.unsorted_segments;
			if let Some((lo, hi)) = meta.time_range {
				stats.time_range = Some(match stats.time_range {
					Some((slo, shi)) => (slo.min(lo), shi.max(hi)),
					None => (lo, hi),
				});
			}
		}
		Ok(stats)
	}

	/// **Store-wide cross-segment overlap count** (roadmap Phase 4.6): the total
	/// number of segments across every aspect whose time window overlaps another
	/// segment *in the same aspect* — the store-wide form of
	/// [`AspectStorageStats::overlapping_segments`](AspectStorageStats::overlapping_segments).
	///
	/// Unlike [`store_stats`](SegmentStore::store_stats) — which reads O(1)
	/// per-aspect rollups — a cross-segment property has no incremental fold, so this
	/// **scans each aspect's segment index** ([`SegmentIndex::overlapping_count`](weft_physical_type::SegmentIndex::overlapping_count))
	/// and sums the results. It is a deliberately separate call so `store_stats` keeps
	/// its no-scan guarantee; a caller pays the scan only when it wants this signal.
	/// Overlaps are always within an aspect (segments of different aspects never share
	/// a measurement stream), so the store-wide total is the plain sum of per-aspect
	/// counts.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure (the aspect list or a per-aspect index load).
	pub async fn store_overlapping_segments(&self) -> Result<usize> {
		let aspects = self.metadata.list_aspects().await?;
		let mut total = 0;
		for aspect in &aspects {
			total += self.index.load_index(aspect).await?.overlapping_count();
		}
		Ok(total)
	}
}

/// The outcome of a store-wide threshold reconcile sweep, returned by
/// [`SegmentStore::reconcile_all_over_threshold`] — the per-tick numbers an
/// automatic background reconcile daemon logs and exports.
///
/// Not `Clone`/`PartialEq`: [`failed`](Self::failed) carries the per-aspect
/// [`anyhow::Error`]s, which are neither.
#[derive(Debug, Default)]
pub struct ReconcileSweep {
	/// Number of declared aspects the sweep visited.
	pub aspects_scanned: usize,
	/// Number of aspects whose backlog was at or above the threshold and were
	/// therefore reconciled this sweep.
	pub aspects_reconciled: usize,
	/// Total out-of-order segments rewritten sorted across every reconciled aspect.
	pub segments_reconciled: usize,
	/// Aspects whose pass failed, each with its error, in visit (declared-name) order.
	/// The sweep records the failure and moves on, so one unreadable aspect (a torn or
	/// truncated frame, a transient I/O error) cannot stop maintenance of every aspect
	/// after it in name order. Empty when every aspect succeeded. A failed aspect counts
	/// only toward `aspects_scanned`, even if its pass rewrote some segments before the
	/// error.
	pub failed: Vec<(String, anyhow::Error)>,
	/// Aspects the sweep left alone because another maintenance operation held them (see
	/// [`MaintenanceWait`]), in visit order. Empty when it took every aspect. A busy
	/// aspect counts only toward `aspects_scanned`; a later sweep maintains it.
	pub busy: Vec<String>,
}

/// The cold-vs-hot split of a hot/cold reconcile pass over one aspect, returned by
/// [`SegmentStore::reconcile_aspect_hot_cold`]. A cold segment (any out-of-order
/// segment that is not the aspect's most-recently-sealed one) is always reconciled;
/// the hot tail is reconciled only when the aspect's backlog reached the threshold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HotColdReconcile {
	/// Cold (non-hot-tail) out-of-order segments rewritten sorted — always paid.
	pub cold_reconciled: usize,
	/// Hot-tail segments rewritten this pass (0 or 1 for a single aspect): non-zero
	/// only when the backlog reached the threshold and the tail was out of order.
	pub hot_reconciled: usize,
}

impl HotColdReconcile {
	/// Total segments rewritten this pass — cold plus hot.
	#[must_use]
	pub const fn total(&self) -> usize {
		self.cold_reconciled + self.hot_reconciled
	}
}

/// The outcome of a store-wide hot/cold reconcile sweep, returned by
/// [`SegmentStore::reconcile_all_hot_cold`] — the per-tick numbers a background
/// reconcile daemon in hot/cold mode logs and exports.
///
/// Not `Clone`/`PartialEq`: [`failed`](Self::failed) carries the per-aspect
/// [`anyhow::Error`]s, which are neither.
#[derive(Debug, Default)]
pub struct HotColdSweep {
	/// Number of declared aspects the sweep visited.
	pub aspects_scanned: usize,
	/// Number of aspects that rewrote at least one segment (cold or hot) this sweep.
	pub aspects_reconciled: usize,
	/// Total cold segments rewritten across every aspect.
	pub cold_reconciled: usize,
	/// Total hot-tail segments rewritten across every aspect.
	pub hot_reconciled: usize,
	/// Aspects whose pass failed, each with its error, in visit (declared-name) order.
	/// The sweep records the failure and moves on, so one unreadable aspect (a torn or
	/// truncated frame, a transient I/O error) cannot stop maintenance of every aspect
	/// after it in name order. Empty when every aspect succeeded. A failed aspect counts
	/// only toward `aspects_scanned`, even if its pass rewrote some segments before the
	/// error.
	pub failed: Vec<(String, anyhow::Error)>,
	/// Aspects the sweep left alone because another maintenance operation held them (see
	/// [`MaintenanceWait`]), in visit order. Empty when it took every aspect. A busy
	/// aspect counts only toward `aspects_scanned`; a later sweep maintains it.
	pub busy: Vec<String>,
}

impl HotColdSweep {
	/// Total segments rewritten across the sweep — cold plus hot.
	#[must_use]
	pub const fn segments_reconciled(&self) -> usize {
		self.cold_reconciled + self.hot_reconciled
	}
}

/// The outcome of a store-wide cross-segment overlap merge sweep, returned by
/// [`SegmentStore::reconcile_all_overlaps`] — the per-tick numbers a background
/// reconcile daemon in overlaps mode logs and exports.
///
/// Not `Clone`/`PartialEq`: [`failed`](Self::failed) carries the per-aspect
/// [`anyhow::Error`]s, which are neither.
#[derive(Debug, Default)]
pub struct OverlapSweep {
	/// Number of declared aspects the sweep visited.
	pub aspects_scanned: usize,
	/// Number of aspects that merged at least one overlap group this sweep.
	pub aspects_reconciled: usize,
	/// Total segments removed by merging across every aspect (sum of per-component
	/// `members − 1`).
	pub segments_removed: usize,
	/// Aspects whose pass failed, each with its error, in visit (declared-name) order.
	/// The sweep records the failure and moves on, so one unreadable aspect (a torn or
	/// truncated frame, a transient I/O error) cannot stop maintenance of every aspect
	/// after it in name order. Empty when every aspect succeeded. A failed aspect counts
	/// only toward `aspects_scanned`, even if its pass rewrote some segments before the
	/// error.
	pub failed: Vec<(String, anyhow::Error)>,
	/// Aspects the sweep left alone because another maintenance operation held them (see
	/// [`MaintenanceWait`]), in visit order. Empty when it took every aspect. A busy
	/// aspect counts only toward `aspects_scanned`; a later sweep maintains it.
	pub busy: Vec<String>,
}

/// The outcome of a store-wide squash sweep, returned by
/// [`SegmentStore::squash_all_over_threshold`] — the per-tick numbers a background
/// squash daemon logs and exports.
///
/// Not `Clone`/`PartialEq`: [`failed`](Self::failed) carries the per-aspect
/// [`anyhow::Error`]s, which are neither.
#[derive(Debug, Default)]
pub struct SquashSweep {
	/// Number of declared aspects the sweep visited.
	pub aspects_scanned: usize,
	/// Number of aspects that were squashed this sweep (their segment count exceeded the
	/// threshold).
	pub aspects_squashed: usize,
	/// Total segments removed by squashing across every aspect (sum of per-aspect
	/// `count − 1`).
	pub segments_removed: usize,
	/// Aspects whose pass failed, each with its error, in visit (declared-name) order.
	/// The sweep records the failure and moves on, so one unreadable aspect (a torn or
	/// truncated frame, a transient I/O error) cannot stop maintenance of every aspect
	/// after it in name order. Empty when every aspect succeeded. A failed aspect counts
	/// only toward `aspects_scanned`, even if its pass rewrote some segments before the
	/// error.
	pub failed: Vec<(String, anyhow::Error)>,
	/// Aspects the sweep left alone because another maintenance operation held them (see
	/// [`MaintenanceWait`]), in visit order. Empty when it took every aspect. A busy
	/// aspect counts only toward `aspects_scanned`; a later sweep maintains it.
	pub busy: Vec<String>,
}

/// Await one database's snapshot into a backup's build directory, fsync the copy through
/// `fs`, and hit `B-vacuum(k)`.
///
/// `VACUUM INTO` already fsyncs its output (`turso_core`'s `finalize_vacuum_into_output`),
/// and verifying the copy does not write to it, but Turso writes it behind
/// [`StoreFs`]'s back. The backup's durability rests on this fsync instead, which costs
/// next to nothing on a file that is already clean.
async fn snapshot_into(fs: &dyn StoreFs, k: u32, snapshot: impl Future<Output = Result<SnapshotReport>>) -> Result<SnapshotReport> {
	let report = snapshot.await?;
	fs.sync_file(&report.dest).await.with_context(|| format!("syncing {}", report.dest.display()))?;
	fault::hit(FaultPoint::BVacuum(k)).await?;
	Ok(report)
}

/// The outcome of a [`SegmentStore::backup_control_plane`] run: the verified snapshot of
/// each of the store's four control-plane databases (roadmap Phase 7.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlPlaneBackup {
	/// The directory the four snapshot files were written to.
	pub dir: PathBuf,
	/// Verified snapshot of `segment_index.db` (the per-segment data-skipping index).
	pub segment_index: crate::SnapshotReport,
	/// Verified snapshot of `metadata.db` (the per-aspect segment-set rollup).
	pub metadata: crate::SnapshotReport,
	/// Verified snapshot of `aspect_catalog.db` (the per-aspect declared schema).
	pub aspect_catalog: crate::SnapshotReport,
	/// Verified snapshot of `catalog.db` (the database/subject registry).
	pub registry: crate::SnapshotReport,
}

impl ControlPlaneBackup {
	/// The four snapshot reports, in the order they were taken.
	#[must_use]
	pub const fn reports(&self) -> [&crate::SnapshotReport; 4] {
		[&self.segment_index, &self.metadata, &self.aspect_catalog, &self.registry]
	}

	/// Total rows verified across all four control-plane databases.
	#[must_use]
	pub fn total_rows(&self) -> i64 {
		self.reports().iter().map(|r| r.rows).sum()
	}

	/// Total on-disk size of the four snapshot files in bytes — the whole control-plane
	/// backup's footprint.
	#[must_use]
	pub fn total_bytes(&self) -> u64 {
		self.reports().iter().map(|r| r.bytes).sum()
	}
}

/// A store-wide aggregate over every aspect's materialized rollup, surfaced by
/// [`SegmentStore::store_stats`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreStorageStats {
	/// Number of aspects with a materialized rollup in this store.
	pub aspect_count: usize,
	/// Total sealed segments across every aspect.
	pub segment_count: usize,
	/// Total rows (present and null) across every aspect.
	pub total_rows: u64,
	/// Total null rows across every aspect.
	pub total_nulls: u64,
	/// Total realized on-disk bytes across every aspect's `.weftseg` frames.
	pub total_bytes: u64,
	/// Total out-of-order segments across every aspect — the store-wide order-health
	/// signal (see [`AspectStorageStats::unsorted_segments`]). Zero when every sealed
	/// segment in the store admits ordered access.
	pub unsorted_segments: usize,
	/// The inclusive `(min, max)` timestamp span the union of all aspects covers, or
	/// [`None`] when the store holds no non-empty segment.
	pub time_range: Option<(i64, i64)>,
}

impl StoreStorageStats {
	/// The store-wide cost term: total framed bytes over total rows across every aspect.
	/// Zero when the store holds no rows.
	#[must_use]
	pub fn bytes_per_point(&self) -> f64 {
		if self.total_rows == 0 {
			return 0.0;
		}
		#[allow(clippy::cast_precision_loss)]
		let n = self.total_rows as f64;
		#[allow(clippy::cast_precision_loss)]
		let total = self.total_bytes as f64;
		total / n
	}
}

/// Realized storage accounting for one aspect's sealed segments, surfaced by
/// [`SegmentStore::aspect_stats`].
#[derive(Debug, Clone, PartialEq)]
pub struct AspectStorageStats {
	/// Number of sealed segments recorded for the aspect.
	pub segment_count: usize,
	/// Total rows (present and null) across every segment.
	pub total_rows: u64,
	/// Total realized on-disk bytes across every `.weftseg` frame.
	pub total_bytes: u64,
	/// The north-star cost term: realized framed bytes per stored point. Zero when
	/// the aspect holds no rows.
	pub bytes_per_point: f64,
	/// The inclusive `(min, max)` timestamp span covered by the aspect, or [`None`]
	/// when it holds no non-empty segment.
	pub time_range: Option<(i64, i64)>,
	/// The number of sealed segments whose timestamps are **not** monotonic
	/// non-decreasing — an order-health signal (an out-of-order segment forces a
	/// linear scan on a point lookup; roadmap Phase 4.6). Zero when every segment
	/// admits ordered access, which a `require_sorted` ingest keeps true by
	/// construction. See [`SegmentIndex::unsorted_count`](weft_physical_type::SegmentIndex::unsorted_count).
	pub unsorted_segments: usize,
	/// The number of sealed segments whose time span **overlaps at least one other
	/// segment's** — the *cross-segment* order-health signal (roadmap Phase 4.6),
	/// distinct from [`unsorted_segments`](AspectStorageStats::unsorted_segments)
	/// (which counts *intra*-segment disorder). A non-zero count means late data
	/// re-entered an already-covered window, so a point lookup may have to consult
	/// more than one segment; these are the cross-segment reconciliation candidates.
	/// See [`SegmentIndex::overlapping_count`](weft_physical_type::SegmentIndex::overlapping_count).
	pub overlapping_segments: usize,
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use serial_test::serial;
	use tempfile::TempDir;
	use weft_physical_type::{timestamp::TimeUnit, PhysicalType};

	use super::*;
	use crate::types::index_txn::TxnPoints;

	fn bd(s: &str) -> BigDecimal {
		BigDecimal::from_str(s).expect("parses")
	}

	/// A lossless f64 schema in seconds — the common dense case.
	fn schema() -> AspectSchema {
		AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)
	}

	#[tokio::test]
	async fn seal_writes_a_file_and_indexes_it() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		let ts: Vec<i64> = (0..5).map(|i| 100 + i * 10).collect();
		let vs: Vec<BigDecimal> = vec![bd("1.5"), bd("2.25"), bd("3.0"), bd("4.5"), bd("5.0")];
		let descriptor = store.seal("temp", &schema(), &ts, &vs).await.expect("seals");
		let on_disk = tokio::fs::read(&descriptor.path).await.expect("file exists");
		let count = store.segment_count("temp").await.expect("counts");
		drop(store);
		// The descriptor points at a real file of the recorded length.
		assert_eq!(descriptor.id, 0);
		assert_eq!(on_disk.len() as u64, descriptor.byte_len);
		assert_eq!(descriptor.row_count, 5);
		assert_eq!(count, 1);
		// The file round-trips back to the original data.
		let (rt, rv) = Segment::read_from(&on_disk).expect("decodes").decode();
		assert_eq!(rt, ts);
		assert_eq!(rv, vs);
	}

	/// Every `.weftseg`/`.weftpart` file anywhere under `dir`.
	fn frames_under(dir: &Path) -> Vec<PathBuf> {
		let mut found = Vec::new();
		for entry in std::fs::read_dir(dir).expect("reads dir").map(|entry| entry.expect("dir entry")) {
			let path = entry.path();
			if path.is_dir() {
				found.extend(frames_under(&path));
			} else if path.extension().is_some_and(|ext| ext == "weftseg" || ext == "weftpart") {
				found.push(path);
			}
		}
		found
	}

	/// Regression: an aspect name is part of a frame's file name, so a name that leaves
	/// `segments/` — a `..` traversal or an absolute path — must be refused by `declare` and
	/// by every seal, and no frame may be written anywhere.
	#[tokio::test]
	async fn path_escaping_aspect_names_are_refused_and_write_nothing() {
		let dir = TempDir::new().expect("tempdir");
		let root = dir.path().join("store");
		let store = SegmentStore::open(&root).await.expect("opens");
		let absolute = dir.path().join("abs").to_string_lossy().into_owned();
		for name in ["../x", "../../x", absolute.as_str()] {
			assert!(store.declare(name, &schema()).await.is_err(), "declare({name:?}) must be refused");
			assert!(store.seal(name, &schema(), &[0, 10], &[bd("1"), bd("2")]).await.is_err(), "seal({name:?}) must be refused");
			assert!(store.seal_paged(name, &schema(), &[0, 10], &[bd("1"), bd("2")], 1).await.is_err(), "seal_paged({name:?}) must be refused");
		}
		let declared = store.list_declared_aspects().await.expect("lists");
		drop(store);
		assert!(declared.is_empty(), "nothing was declared: {declared:?}");
		let frames = frames_under(dir.path());
		assert!(frames.is_empty(), "no frame was written anywhere: {frames:?}");
	}

	/// A name declared before names were checked cannot be sealed, read or maintained (a
	/// typed error, not a panic), and a store-wide sweep reports it in `failed` while it
	/// still maintains every other aspect.
	#[tokio::test]
	async fn a_previously_declared_unsafe_name_errors_on_use_and_sweeps_report_it() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// Written straight into the catalog, as an older version would have accepted it.
		store.catalog().declare(store.database(), store.subject(), "../x", &schema()).await.expect("raw declare");
		store.declare("ok", &schema()).await.expect("declares");
		// Out of order and twice, so the sweeps below have work to do on the valid aspect.
		store.seal_declared("ok", &[10, 0], &[bd("1"), bd("2")]).await.expect("seals");
		store.seal_declared("ok", &[20, 30], &[bd("3"), bd("4")]).await.expect("seals");

		let results = [store.seal_declared("../x", &[0], &[bd("1")]).await.map(|_| ()), store.read_time_range("../x", 0, 10).await.map(|_| ()), store.read_point("../x", 0).await.map(|_| ()), store.downsample_range("../x", 0, 10, Resolution::Seconds, &[Aggregation::Avg]).await.map(|_| ()), store.reconcile_aspect("../x").await.map(|_| ()), store.reconcile_overlaps("../x").await.map(|_| ()), store.squash_aspect("../x").await.map(|_| ())];
		let reconcile = store.reconcile_all_over_threshold(1, MaintenanceWait::Skip).await;
		let squash = store.squash_all_over_threshold(1, MaintenanceWait::Skip).await;
		drop(store);
		for result in results {
			let err = result.expect_err("an unsafe name is refused");
			let invalid = err.downcast_ref::<crate::InvalidAspectName>().unwrap_or_else(|| panic!("a typed InvalidAspectName, got: {err:#}"));
			assert_eq!(invalid.name(), "../x");
		}
		let reconcile = reconcile.expect("the sweep runs despite the unsafe declaration");
		assert_eq!(reconcile.aspects_scanned, 2, "both declared aspects are visited");
		assert_eq!(reconcile.segments_reconciled, 1, "the valid aspect is still maintained");
		let squash = squash.expect("the sweep runs despite the unsafe declaration");
		assert_eq!(squash.segments_removed, 1, "the valid aspect is still squashed");
		for failed in [&reconcile.failed, &squash.failed] {
			assert_eq!(failed.len(), 1, "only the unsafe aspect fails: {failed:?}");
			assert_eq!(failed[0].0, "../x");
			assert!(failed[0].1.downcast_ref::<crate::InvalidAspectName>().is_some(), "reported as an InvalidAspectName: {:#}", failed[0].1);
		}
		assert!(frames_under(dir.path()).iter().all(|path| path.starts_with(dir.path().join("segments"))), "every frame stays under segments/");
	}

	#[test]
	fn contained_file_refuses_paths_outside_the_directory() {
		let dir = Path::new("store").join("segments");
		assert_eq!(contained_file(&dir, "a-0.weftseg").expect("inside"), dir.join("a-0.weftseg"));
		let absolute = std::env::temp_dir().join("abs-0.weftseg").to_string_lossy().into_owned();
		for escaping in ["../a-0.weftseg", "x/../../a-0.weftseg", "sub/a-0.weftseg", "a/.", "..", absolute.as_str()] {
			assert!(contained_file(&dir, escaping).is_err(), "{escaping:?} must be refused");
		}
	}

	#[tokio::test]
	#[serial(backup_fault_points)]
	async fn backup_then_restore_round_trips_a_working_store() {
		// The Phase 7.4 drill: back the control plane up, restore it into a fresh root
		// beside the original's segment frames, and prove the restored store still
		// answers — a backup that has never been restored is not yet a backup.
		let dir = TempDir::new().unwrap();
		let schema = AspectSchema::new(PhysicalType::F64, "0".parse().unwrap(), TimeUnit::Seconds);
		let root = dir.path().join("live");
		let store = SegmentStore::open(&root).await.expect("opens");
		store.declare("price", &schema).await.expect("declares");
		store.seal("price", &schema, &[0_i64, 10, 20], &["1".parse().unwrap(), "2".parse().unwrap(), "3".parse().unwrap()]).await.expect("seals");
		let backup_dir = dir.path().join("backup");
		let backup = store.backup_control_plane_with_verify(&backup_dir, crate::VerifyMode::SnapshotOnly).await.expect("backs up");
		let live_stats = store.aspect_stats("price").await.expect("live stats");
		drop(store);

		// Restore into a fresh root, carrying the measurement frames across (the backup is
		// control-plane only, per hard-constraint #3).
		let restored_root = dir.path().join("restored");
		tokio::fs::create_dir_all(restored_root.join("segments")).await.unwrap();
		let mut frames = tokio::fs::read_dir(root.join("segments")).await.unwrap();
		while let Some(entry) = frames.next_entry().await.unwrap() {
			tokio::fs::copy(entry.path(), restored_root.join("segments").join(entry.file_name())).await.unwrap();
		}
		let report = crate::restore_control_plane(&backup_dir, &restored_root).await.expect("restores");
		assert_eq!(report.restored.len(), 4, "all four control-plane DBs restored");
		assert_eq!(report.total_rows(), backup.total_rows(), "the restored control plane holds the backed-up rows");
		// The live root is gone, as after a disk loss: the restored index still names the
		// frames by their old absolute paths, so the reads below pass only because readers
		// resolve those against the restored root.
		tokio::fs::remove_dir_all(&root).await.unwrap();

		// The restored store opens and still knows the aspect, its schema and its segments.
		let reopened = SegmentStore::open(&restored_root).await.expect("restored store opens");
		let aspects = reopened.list_declared_aspects().await.expect("lists aspects");
		let stats = reopened.aspect_stats("price").await.expect("restored stats");
		let (times, values) = reopened.read_time_range("price", 0, 20).await.expect("reads back");
		drop(reopened);
		assert_eq!(aspects, vec!["price".to_string()], "the declared aspect survived the restore");
		assert_eq!(stats.segment_count, live_stats.segment_count, "same segment count as the live store");
		assert_eq!(times, vec![0_i64, 10, 20], "the measurements read back through the restored control plane");
		assert_eq!(values.into_iter().flatten().count(), 3, "every value materialized from the restored store");
	}

	#[tokio::test]
	#[serial(backup_fault_points)]
	async fn restore_refuses_to_clobber_an_existing_control_plane() {
		let dir = TempDir::new().unwrap();
		let schema = AspectSchema::new(PhysicalType::F64, "0".parse().unwrap(), TimeUnit::Seconds);
		let root = dir.path().join("live");
		let store = SegmentStore::open(&root).await.expect("opens");
		store.declare("price", &schema).await.expect("declares");
		let backup_dir = dir.path().join("backup");
		store.backup_control_plane_with_verify(&backup_dir, crate::VerifyMode::SnapshotOnly).await.expect("backs up");
		drop(store);

		// Restoring back over the live root must refuse rather than half-overwrite it.
		let err = crate::restore_control_plane(&backup_dir, &root).await.unwrap_err();
		assert!(err.to_string().contains("refusing to overwrite"), "got: {err}");

		// An incomplete backup dir is refused too, and writes nothing.
		let partial = dir.path().join("partial");
		tokio::fs::create_dir_all(&partial).await.unwrap();
		tokio::fs::copy(backup_dir.join("catalog.db"), partial.join("catalog.db")).await.unwrap();
		let fresh = dir.path().join("fresh");
		let err = crate::restore_control_plane(&partial, &fresh).await.unwrap_err();
		assert!(err.to_string().contains("is missing"), "got: {err}");
		assert!(!fresh.join("catalog.db").exists(), "a refused restore writes nothing");
	}

	/// A store with one declared aspect and one sealed segment, so every control-plane
	/// database holds rows.
	async fn populated_store(root: &Path) -> SegmentStore {
		let store = SegmentStore::open(root).await.expect("opens");
		store.declare("price", &schema()).await.expect("declares");
		store.seal("price", &schema(), &[0_i64, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.expect("seals");
		store
	}

	/// The names directly under `dir`, sorted.
	fn names_in(dir: &Path) -> Vec<String> {
		let mut names: Vec<String> = std::fs::read_dir(dir).expect("lists").map(|e| e.expect("entry").file_name().to_string_lossy().into_owned()).collect();
		names.sort();
		names
	}

	#[tokio::test]
	#[serial(backup_fault_points)]
	async fn a_published_backup_carries_a_manifest_and_leaves_no_build_directory() {
		let dir = TempDir::new().unwrap();
		let store = populated_store(&dir.path().join("live")).await;
		let base = dir.path().join("backups");
		let backup = store.backup_control_plane_with_verify(base.join("nightly"), crate::VerifyMode::SnapshotOnly).await.expect("backs up");
		let again = store.backup_control_plane_with_verify(base.join("nightly"), crate::VerifyMode::SnapshotOnly).await;
		drop(store);

		assert_eq!(names_in(&base), vec!["nightly".to_string()], "the backup was built aside and renamed into place, leaving nothing else");
		let manifest = crate::BackupManifest::read(&backup.dir).await.expect("reads").expect("a published backup has a manifest");
		assert_eq!(manifest.format, crate::MANIFEST_FORMAT);
		for (report, file) in backup.reports().into_iter().zip(&manifest.files) {
			assert_eq!(report.dest, backup.dir.join(&file.name), "the reports name the published files");
			assert_eq!(std::fs::metadata(&report.dest).unwrap().len(), file.bytes, "{}: the manifest records the size", file.name);
			assert_eq!((report.rows, &report.table_names), (file.rows, &file.tables), "{}", file.name);
			for table in crate::expected_tables(&file.name).unwrap() {
				assert!(file.tables.iter().any(|t| t == table), "{}: holds {table}", file.name);
			}
		}
		assert!(again.is_err(), "a backup never replaces an existing one");
		assert_eq!(names_in(&base), vec!["nightly".to_string()], "and a refused backup builds nothing");
	}

	/// The regression for an empty snapshot verifying: a pre-manifest backup whose
	/// `catalog.db` is an empty database (a crash mid-`VACUUM INTO`, say) restored as a
	/// valid control plane with no databases or subjects in it.
	#[tokio::test]
	#[serial(backup_fault_points)]
	async fn restore_rejects_an_empty_catalog_snapshot() {
		let dir = TempDir::new().unwrap();
		let store = populated_store(&dir.path().join("live")).await;
		let backup_dir = dir.path().join("backup");
		store.backup_control_plane_with_verify(&backup_dir, crate::VerifyMode::SnapshotOnly).await.expect("backs up");
		drop(store);
		// Make it a pre-manifest backup, then empty its catalog.
		let _ = std::fs::remove_file(backup_dir.join("MANIFEST.json"));
		std::fs::remove_file(backup_dir.join("catalog.db")).unwrap();
		let empty = turso::Builder::new_local(backup_dir.join("catalog.db").to_str().unwrap()).build().await.unwrap();
		drop(empty.connect().unwrap());
		drop(empty);

		let fresh = dir.path().join("fresh");
		let err = crate::restore_control_plane(&backup_dir, &fresh).await.expect_err("an empty catalog.db is not a backup of one");
		assert!(format!("{err:#}").contains("missing the table(s)"), "got: {err:#}");
		for name in crate::CONTROL_PLANE_FILES {
			assert!(!fresh.join(name).exists(), "a failed restore leaves no control-plane file under its final name: {name}");
		}
	}

	#[tokio::test]
	#[serial(backup_fault_points)]
	async fn a_legacy_backup_without_a_manifest_still_restores() {
		let dir = TempDir::new().unwrap();
		let root = dir.path().join("live");
		let store = populated_store(&root).await;
		let backup_dir = dir.path().join("backup");
		let backup = store.backup_control_plane_with_verify(&backup_dir, crate::VerifyMode::SnapshotOnly).await.expect("backs up");
		drop(store);
		let _ = std::fs::remove_file(backup_dir.join("MANIFEST.json"));
		assert!(crate::is_complete_backup(&backup_dir).await.unwrap(), "four databases and no manifest: a complete pre-manifest backup");

		let restored_root = dir.path().join("restored");
		tokio::fs::create_dir_all(restored_root.join("segments")).await.unwrap();
		for entry in std::fs::read_dir(root.join("segments")).unwrap() {
			let entry = entry.unwrap();
			std::fs::copy(entry.path(), restored_root.join("segments").join(entry.file_name())).unwrap();
		}
		let report = crate::restore_control_plane(&backup_dir, &restored_root).await.expect("a pre-manifest backup restores");
		assert_eq!(report.total_rows(), backup.total_rows());
		assert!(!names_in(&restored_root).iter().any(|n| Path::new(n).extension().is_some_and(|ext| ext == "tmp")), "the staged copies were renamed into place");
		let reopened = SegmentStore::open(&restored_root).await.expect("opens");
		let (times, _) = reopened.read_time_range("price", 0, 20).await.expect("reads");
		assert_eq!(times, vec![0_i64, 10, 20]);
	}

	#[tokio::test]
	#[serial(backup_fault_points)]
	async fn restore_refuses_a_staging_directory_and_a_backup_changed_since_its_manifest() {
		let dir = TempDir::new().unwrap();
		let store = populated_store(&dir.path().join("live")).await;
		let base = dir.path().join("backups");
		let backup = store.backup_control_plane_with_verify(base.join("backup-1"), crate::VerifyMode::SnapshotOnly).await.expect("backs up");
		drop(store);

		// A build directory can hold all four databases and even a manifest; its name
		// still says it was never published.
		let staged = base.join(".partial-backup-1-0123456789abcdef");
		std::fs::rename(&backup.dir, &staged).unwrap();
		let err = crate::restore_control_plane(&staged, &dir.path().join("a")).await.unwrap_err();
		assert!(format!("{err:#}").contains("unfinished"), "got: {err:#}");
		std::fs::rename(&staged, &backup.dir).unwrap();

		// A file that no longer matches the manifest is refused before anything is copied.
		let catalog = backup.dir.join("catalog.db");
		let mut bytes = std::fs::read(&catalog).unwrap();
		bytes.extend_from_slice(&[0; 4096]);
		std::fs::write(&catalog, bytes).unwrap();
		let fresh = dir.path().join("b");
		let err = crate::restore_control_plane(&backup.dir, &fresh).await.unwrap_err();
		assert!(format!("{err:#}").contains("manifest recorded"), "got: {err:#}");
		assert!(!fresh.exists(), "nothing was written");
	}

	/// A reported backup survives power loss whole: every image a power cut right after
	/// the call returns could leave holds the backup under its label, complete and
	/// restorable, and no build directory. The live store's own databases are trusted
	/// (Turso commits them FULL); everything the backup writes, including the snapshots
	/// Turso vacuums into it, counts only once made durable through `StoreFs`.
	#[tokio::test]
	#[serial(backup_fault_points)]
	async fn a_power_cut_after_a_reported_backup_leaves_it_complete() {
		fn live_database(rel: &Path) -> bool {
			rel.parent() == Some(Path::new("")) && crate::durable::SimFs::turso_file(rel)
		}
		let dir = TempDir::new().unwrap();
		let root = dir.path().join("live");
		let store = populated_store(&root).await;
		let sim = crate::durable::SimFs::exempting(&root, live_database).unwrap();
		let backup = store.backup_control_plane_via(&sim, root.join("backups/backup-1"), crate::VerifyMode::SnapshotOnly).await.expect("backs up");
		drop(store);

		for seed in 0..16 {
			let image = TempDir::new().unwrap();
			sim.power_cut(seed, image.path()).unwrap();
			let base = image.path().join("backups");
			assert!(base.is_dir(), "seed {seed}: the backup base the backup created survives");
			assert_eq!(names_in(&base), vec!["backup-1".to_string()], "seed {seed}: the backup is under its label, and only there");
			let published = base.join("backup-1");
			let manifest = crate::BackupManifest::read(&published).await.unwrap_or_else(|e| panic!("seed {seed}: {e:#}"));
			assert!(manifest.is_some(), "seed {seed}: the manifest survives with the backup");
			let restored = crate::restore_control_plane(&published, &image.path().join("restored")).await.unwrap_or_else(|e| panic!("seed {seed}: the backup restores: {e:#}"));
			assert_eq!(restored.total_rows(), backup.total_rows(), "seed {seed}");
		}
	}

	/// A reported restore survives power loss whole: every image a power cut right after
	/// the call returns could leave holds all four databases under their final names,
	/// each complete, and no `.tmp` copy. The restore root is new, so its own entry must
	/// be durable too. Nothing in the restore root is exempt: Turso's reopen to verify a
	/// copy is trusted only if it leaves the synced bytes as they were.
	#[tokio::test]
	#[serial(backup_fault_points)]
	async fn a_power_cut_after_a_reported_restore_leaves_every_database_complete() {
		let dir = TempDir::new().unwrap();
		let store = populated_store(&dir.path().join("live")).await;
		let backup_dir = dir.path().join("backup");
		store.backup_control_plane_with_verify(&backup_dir, crate::VerifyMode::SnapshotOnly).await.expect("backs up");
		drop(store);

		let restores = dir.path().join("restores");
		std::fs::create_dir(&restores).unwrap();
		let sim = crate::durable::SimFs::new(&restores).unwrap();
		let root = restores.join("restored");
		crate::restore_control_plane_with(&sim, &backup_dir, &root).await.expect("restores");

		let mut files: Vec<String> = crate::CONTROL_PLANE_FILES.iter().map(ToString::to_string).collect();
		files.sort();
		assert_eq!(names_in(&root), files, "the restore left exactly the four databases");
		for seed in 0..16 {
			let image = TempDir::new().unwrap();
			sim.power_cut(seed, image.path()).unwrap();
			let restored = image.path().join("restored");
			assert!(restored.is_dir(), "seed {seed}: the new restore root survives");
			assert_eq!(names_in(&restored), files, "seed {seed}: every database is under its final name, and no `.tmp` copy is left");
			for name in crate::CONTROL_PLANE_FILES {
				assert!(std::fs::read(restored.join(name)).unwrap() == std::fs::read(root.join(name)).unwrap(), "seed {seed}: {name} is complete");
			}
		}
	}

	/// The `B-*` points, in the order a backup reaches them.
	const BACKUP_POINTS: [FaultPoint; 8] = [FaultPoint::BPartialCreated, FaultPoint::BVacuum(0), FaultPoint::BVacuum(1), FaultPoint::BVacuum(2), FaultPoint::BVacuum(3), FaultPoint::BLinks, FaultPoint::BManifest, FaultPoint::BRenamed];

	/// The regression for `backup-vacuum-into-partial-dest`,
	/// `backup-partial-dir-counts-toward-retention` and `verify-sidecar-litter`: a backup
	/// that stopped part-way left a directory under the backup's own name holding some of
	/// its databases (and the verification's litter), which retention counted and a drill
	/// could pick. Now a crash at any point leaves nothing incomplete that retention
	/// counts ([`crate::is_complete_backup`], what the daemon lists by) or that restores,
	/// and the sweep removes what it does leave.
	///
	/// The crash is a backup parked at the point and dropped there. Unlike an error, that
	/// runs none of the backup's own cleanup, so what is left is what a process crash
	/// leaves (the OS keeps every write; power loss is the `SimFs` tests' job).
	#[tokio::test]
	#[serial(backup_fault_points)]
	async fn a_crash_at_any_backup_point_leaves_nothing_incomplete_counted_or_restorable() {
		use std::sync::Arc;

		use tokio::sync::Notify;

		use crate::durable::fault::{arm, hits, reached, FaultAction};

		let dir = TempDir::new().unwrap();
		let store = Arc::new(populated_store(&dir.path().join("live")).await);
		let base = dir.path().join("backups");
		for (n, point) in BACKUP_POINTS.into_iter().enumerate() {
			let label = format!("backup-{n}");
			let armed = arm(point, FaultAction::Pause(Arc::new(Notify::new())));
			let before = hits(point);
			let backup = tokio::spawn({
				let (store, dest) = (store.clone(), base.join(&label));
				async move { store.backup_control_plane_with_verify(dest, crate::VerifyMode::SnapshotOnly).await }
			});
			tokio::time::timeout(std::time::Duration::from_secs(30), reached(point, before + 1)).await.unwrap_or_else(|_| panic!("the backup never reached {point}"));
			backup.abort();
			assert!(backup.await.expect_err("the backup is dropped at the point").is_cancelled(), "{point}");
			drop(armed);

			// Stopped after the rename, the backup is whole under its label (only the base
			// fsync had not run), so it rightly counts. Stopped anywhere before, its build
			// directory is all there is, and it does not.
			let expected: Vec<String> = if point == FaultPoint::BRenamed { vec![label.clone()] } else { Vec::new() };
			let names = names_in(&base);
			if point != FaultPoint::BRenamed {
				assert!(matches!(names.as_slice(), [only] if only.starts_with(&format!(".partial-{label}-"))), "{point}: the crash leaves its build directory and nothing else: {names:?}");
			}
			let mut complete = Vec::new();
			for name in names {
				let path = base.join(&name);
				let restored = crate::restore_control_plane(&path, &dir.path().join(format!("restore-{point}-{name}"))).await;
				if crate::is_complete_backup(&path).await.unwrap() {
					restored.unwrap_or_else(|e| panic!("{point}: {name} counts as a backup, so it restores: {e:#}"));
					complete.push(name);
				} else {
					assert!(restored.is_err(), "{point}: {name} does not count as a backup, so it must not restore either");
				}
			}
			assert_eq!(complete, expected, "{point}");

			let swept = crate::sweep_backup_staging(&RealFs, &base, std::time::Duration::ZERO).await.unwrap();
			assert!(swept.failed.is_empty(), "{point}: {:?}", swept.failed);
			assert_eq!(names_in(&base), expected, "{point}: the sweep removes everything that is not a backup");
		}
		drop(store);
	}

	/// An error, unlike a crash, cleans up after itself. Stopped by an error anywhere
	/// before it is published, a backup leaves nothing under the base, so a deployment
	/// without the backup daemon (whose sweep removes what a crash leaves) does not
	/// collect hidden database copies. Stopped by one after the publishing rename, it is
	/// already whole under its label, which stays taken.
	#[tokio::test]
	#[serial(backup_fault_points)]
	async fn an_error_at_any_backup_point_removes_what_it_built() {
		use crate::durable::fault::{arm, injected_point, FaultAction};

		let dir = TempDir::new().unwrap();
		let store = populated_store(&dir.path().join("live")).await;
		let base = dir.path().join("backups");
		for (n, point) in BACKUP_POINTS.into_iter().enumerate() {
			let label = format!("backup-{n}");
			let armed = arm(point, FaultAction::ReturnErr);
			let err = store.backup_control_plane_with_verify(base.join(&label), crate::VerifyMode::SnapshotOnly).await.expect_err("the fault stops the backup");
			drop(armed);
			assert_eq!(err.chain().find_map(|cause| cause.downcast_ref::<std::io::Error>()).and_then(injected_point), Some(point), "{point}: {err:#}");

			if point == FaultPoint::BRenamed {
				assert_eq!(names_in(&base), [label.clone()], "{point}: published before the error");
				assert!(crate::is_complete_backup(&base.join(&label)).await.unwrap(), "{point}");
				let again = store.backup_control_plane_with_verify(base.join(&label), crate::VerifyMode::SnapshotOnly).await;
				assert!(again.is_err(), "{point}: the label is taken");
			} else {
				assert!(names_in(&base).is_empty(), "{point}: the failed backup removed its build directory: {:?}", names_in(&base));
			}
		}
		drop(store);
	}

	#[tokio::test]
	#[serial(backup_fault_points)]
	async fn backup_control_plane_snapshots_and_verifies_every_db() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open_scoped(dir.path(), "market", "btc").await.expect("opens");
		// Declare a schema and seal two aspects, so all four control-plane DBs hold rows:
		// aspect_catalog (declare), segment_index + metadata (seal), catalog (open registers).
		store.declare("price", &schema()).await.expect("declares");
		for aspect in ["price", "volume"] {
			let ts: Vec<i64> = (0..4).map(|i| 100 + i * 10).collect();
			let vs: Vec<BigDecimal> = (0..4).map(|i| bd(&format!("{}.5", i + 1))).collect();
			store.seal(aspect, &schema(), &ts, &vs).await.expect("seals");
		}

		let backup_dir = dir.path().join("backup-run-1");
		let backup = store.backup_control_plane(&backup_dir).await.expect("backs up");

		// Every control-plane file was written and each verified a non-empty table set.
		assert_eq!(backup.dir, backup_dir);
		for name in ["segment_index.db", "metadata.db", "aspect_catalog.db", "catalog.db"] {
			assert!(backup_dir.join(name).exists(), "{name} was written");
		}
		assert!(backup.segment_index.rows >= 2, "two sealed segments indexed");
		assert!(backup.metadata.rows >= 2, "two aspect rollups");
		assert!(backup.aspect_catalog.rows >= 1, "at least the declared schema");
		assert!(backup.registry.rows >= 1, "the (database, subject) registration");
		assert_eq!(backup.total_rows(), backup.reports().iter().map(|r| r.rows).sum::<i64>());
		assert!(backup.total_bytes() > 0, "the snapshot files have a non-zero footprint");
		assert_eq!(backup.total_bytes(), backup.reports().iter().map(|r| r.bytes).sum::<u64>());

		// The source stays fully usable after an online backup.
		let live = store.segment_count("price").await.expect("counts");
		assert_eq!(live, 1);
		drop(store);

		// The snapshot copies reopen as independent stores holding the same data.
		let restored = SegmentStore::open_scoped(&backup_dir, "market", "btc").await.expect("reopens copy");
		assert_eq!(restored.segment_count("price").await.expect("counts"), 1);
		assert_eq!(restored.segment_count("volume").await.expect("counts"), 1);
		let mut aspects = restored.metadata().list_aspects().await.expect("lists");
		aspects.sort();
		assert_eq!(aspects, vec!["price".to_string(), "volume".to_string()]);
	}

	#[tokio::test]
	async fn read_time_range_opens_only_overlapping_segments() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// Three sealed segments over [0,40], [100,140], [200,240].
		for base in [0_i64, 100, 200] {
			let ts: Vec<i64> = (0..5).map(|i| base + i * 10).collect();
			let vs: Vec<BigDecimal> = (0..5).map(|i| bd(&format!("{}", base + i))).collect();
			store.seal("a", &schema(), &ts, &vs).await.expect("seals");
		}
		let count = store.segment_count("a").await.expect("counts");
		// A window inside the middle segment returns only its rows.
		let (ts, vs) = store.read_time_range("a", 110, 130).await.expect("reads");
		// A window straddling the first two segments returns rows from both.
		let (ts2, _) = store.read_time_range("a", 30, 110).await.expect("reads");
		// A window in a gap returns nothing.
		let (ts3, vs3) = store.read_time_range("a", 50, 90).await.expect("reads");
		drop(store);
		assert_eq!(count, 3);
		assert_eq!(ts, vec![110, 120, 130]);
		assert_eq!(vs, vec![Some(bd("101")), Some(bd("102")), Some(bd("103"))]);
		assert_eq!(ts2, vec![30, 40, 100, 110]);
		assert!(ts3.is_empty());
		assert!(vs3.is_empty());
	}

	#[tokio::test]
	async fn seal_assigns_monotonic_ids_and_distinct_files() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		let d0 = store.seal("a", &schema(), &[0_i64], &[bd("1")]).await.expect("seals");
		let d1 = store.seal("a", &schema(), &[10_i64], &[bd("2")]).await.expect("seals");
		drop(store);
		assert_eq!(d0.id, 0);
		assert_eq!(d1.id, 1);
		assert_ne!(d0.path, d1.path, "each segment gets its own file");
	}

	#[tokio::test]
	async fn nullable_seal_round_trips_through_disk() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		let ts = vec![10_i64, 20, 30, 40];
		let vs = vec![Some(bd("1.5")), None, Some(bd("3.5")), None];
		let descriptor = store.seal_nullable("a", &schema(), &ts, &vs).await.expect("seals");
		let (rt, rv) = store.read_time_range("a", 0, 100).await.expect("reads");
		drop(store);
		assert_eq!(descriptor.null_count, 2);
		assert_eq!(rt, ts);
		assert_eq!(rv, vs);
	}

	#[tokio::test]
	async fn declared_tolerance_is_enforced_on_seal() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// A zero-tolerance ScaledI64{scale:0} cannot represent a fractional value: the
		// seal must fail rather than downcast silently, and nothing is recorded.
		let strict = AspectSchema::new(PhysicalType::ScaledI64 { scale: 0 }, bd("0"), TimeUnit::Seconds);
		let err = store.seal("a", &strict, &[0_i64], &[bd("1.5")]).await;
		let count = store.segment_count("a").await.expect("counts");
		drop(store);
		assert!(err.is_err(), "lossy seal past a zero tolerance must fail");
		assert_eq!(count, 0, "a failed seal records nothing");
	}

	#[tokio::test]
	async fn store_reopens_and_sees_prior_segments() {
		let dir = TempDir::new().expect("tempdir");
		let first = SegmentStore::open(dir.path()).await.expect("opens");
		first.seal("a", &schema(), &[0_i64, 10], &[bd("1"), bd("2")]).await.expect("seals");
		drop(first);
		// A fresh store over the same root sees the persisted segment and its file.
		let reopened = SegmentStore::open(dir.path()).await.expect("reopens");
		let count = reopened.segment_count("a").await.expect("counts");
		let (ts, _) = reopened.read_time_range("a", 0, 100).await.expect("reads");
		// The next seal continues the id sequence rather than colliding.
		let d = reopened.seal("a", &schema(), &[20_i64], &[bd("3")]).await.expect("seals");
		drop(reopened);
		assert_eq!(count, 1);
		assert_eq!(ts, vec![0, 10]);
		assert_eq!(d.id, 1);
	}

	#[tokio::test]
	async fn paged_seal_reads_back_with_page_skipping() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// 12 rows at ts 0,10,..,110; 4 rows/page ⇒ pages [0,30],[40,70],[80,110].
		let ts: Vec<i64> = (0..12).map(|i| i * 10).collect();
		let vs: Vec<BigDecimal> = (0..12).map(BigDecimal::from).collect();
		let descriptor = store.seal_paged("a", &schema(), &ts, &vs, 4).await.expect("seals paged");
		// The descriptor records the paged frame version.
		assert_eq!(descriptor.format_version, PAGED_SEGMENT_FORMAT_VERSION);
		// A window inside the middle page returns only its rows (other pages skipped).
		let (wts, wvs) = store.read_time_range("a", 45, 65).await.expect("reads");
		// The whole span decodes to every row.
		let (allts, _) = store.read_time_range("a", i64::MIN, i64::MAX).await.expect("reads");
		drop(store);
		assert_eq!(wts, vec![50, 60]);
		assert_eq!(wvs, vec![Some(bd("5")), Some(bd("6"))]);
		assert_eq!(allts, ts);
	}

	#[tokio::test]
	async fn single_and_paged_segments_read_together() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// A single-block segment over [0,40] and a paged one over [100,140].
		store.seal("a", &schema(), &[0_i64, 10, 20, 30, 40], &(0..5).map(BigDecimal::from).collect::<Vec<_>>()).await.expect("seals single");
		let pts: Vec<i64> = (0..5).map(|i| 100 + i * 10).collect();
		let pvs: Vec<BigDecimal> = (0..5).map(|i| bd(&format!("{}", 100 + i))).collect();
		let paged = store.seal_paged("a", &schema(), &pts, &pvs, 2).await.expect("seals paged");
		// The two frames carry different versions but the same aspect read path.
		assert_eq!(paged.format_version, PAGED_SEGMENT_FORMAT_VERSION);
		let count = store.segment_count("a").await.expect("counts");
		// A window straddling both segments returns rows from each, despite the
		// different on-disk frames.
		let (ts, _) = store.read_time_range("a", 30, 110).await.expect("reads");
		drop(store);
		assert_eq!(count, 2);
		assert_eq!(ts, vec![30, 40, 100, 110]);
	}

	#[tokio::test]
	async fn read_value_range_prunes_by_value_span() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// Three segments with disjoint value spans [0,4], [100,104], [200,204].
		for base in [0_i64, 100, 200] {
			let ts: Vec<i64> = (0..5).map(|i| base + i * 10).collect();
			let vs: Vec<BigDecimal> = (0..5).map(|i| BigDecimal::from(base + i)).collect();
			store.seal("a", &schema(), &ts, &vs).await.expect("seals");
		}
		// A value window inside the middle segment returns only its in-range rows.
		let (ts, vs) = store.read_value_range("a", &bd("101"), &bd("103")).await.expect("reads");
		// A window covering everything returns all rows.
		let (allts, _) = store.read_value_range("a", &bd("0"), &bd("204")).await.expect("reads");
		// A window in a value gap returns nothing.
		let (gts, gvs) = store.read_value_range("a", &bd("50"), &bd("60")).await.expect("reads");
		drop(store);
		assert_eq!(vs, vec![bd("101"), bd("102"), bd("103")]);
		assert_eq!(ts, vec![110, 120, 130]);
		assert_eq!(allts.len(), 15);
		assert!(gts.is_empty());
		assert!(gvs.is_empty());
	}

	#[tokio::test]
	async fn read_point_finds_the_value_at_an_exact_instant() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// Two disjoint sealed segments over [0,40] and [100,140].
		for base in [0_i64, 100] {
			let ts: Vec<i64> = (0..5).map(|i| base + i * 10).collect();
			let vs: Vec<BigDecimal> = (0..5).map(|i| BigDecimal::from(base + i)).collect();
			store.seal("a", &schema(), &ts, &vs).await.expect("seals");
		}
		// A hit in each segment; the index prunes to the one file that spans the instant.
		let hit_first = store.read_point("a", 20).await.expect("reads");
		let hit_second = store.read_point("a", 120).await.expect("reads");
		// An off-grid instant inside a segment span, one in the inter-segment gap, and
		// one past the aspect entirely — all misses.
		let off_grid = store.read_point("a", 25).await.expect("reads");
		let in_gap = store.read_point("a", 70).await.expect("reads");
		let beyond = store.read_point("a", 500).await.expect("reads");
		// An undeclared/empty aspect is a clean miss.
		let empty = store.read_point("none", 0).await.expect("reads");
		drop(store);
		assert_eq!(hit_first, Some(bd("2")));
		assert_eq!(hit_second, Some(bd("102")));
		assert_eq!(off_grid, None);
		assert_eq!(in_gap, None);
		assert_eq!(beyond, None);
		assert_eq!(empty, None);
	}

	#[tokio::test]
	async fn read_points_matches_per_instant_and_batches_across_segments() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// Two disjoint sealed segments over [0,40] and [100,140] (as read_point's test).
		for base in [0_i64, 100] {
			let ts: Vec<i64> = (0..5).map(|i| base + i * 10).collect();
			let vs: Vec<BigDecimal> = (0..5).map(|i| BigDecimal::from(base + i)).collect();
			store.seal("a", &schema(), &ts, &vs).await.expect("seals");
		}
		// A scrambled batch spanning both segments, with a repeat, off-grid/gap/out-of-range misses.
		let batch = [120_i64, 20, 25, 70, 120, 500, 0, 140];
		let got = store.read_points("a", &batch).await.expect("reads");
		assert_eq!(got.len(), batch.len());
		// Each slot equals the single-instant read at that instant.
		for (k, &t) in batch.iter().enumerate() {
			assert_eq!(got[k], store.read_point("a", t).await.expect("reads"), "batch slot {k} (t={t})");
		}
		// Spot-check the values: 20 -> 2, 120 -> 102, 0 -> 0, 140 -> 104; misses are None.
		assert_eq!(got, vec![Some(bd("102")), Some(bd("2")), None, None, Some(bd("102")), None, Some(bd("0")), Some(bd("104"))]);
		// An empty batch and an undeclared aspect are clean.
		assert!(store.read_points("a", &[]).await.expect("reads").is_empty());
		assert_eq!(store.read_points("none", &[0, 1]).await.expect("reads"), vec![None, None]);
	}

	#[tokio::test]
	async fn read_point_reads_paged_and_out_of_order_segments() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// A paged segment over [0,110] (page skipping applies to the point lookup).
		let pts: Vec<i64> = (0..12).map(|i| i * 10).collect();
		let pvs: Vec<BigDecimal> = (0..12).map(BigDecimal::from).collect();
		store.seal_paged("a", &schema(), &pts, &pvs, 4).await.expect("seals paged");
		// An out-of-order single-block segment over [200,240] (forces a linear scan).
		store.seal("a", &schema(), &[200_i64, 230, 210, 240, 220], &(0..5).map(|i| bd(&format!("{}", 200 + i))).collect::<Vec<_>>()).await.expect("seals ooo");
		let unsorted = store.aspect_stats("a").await.expect("stats").unsorted_segments;
		let paged_hit = store.read_point("a", 50).await.expect("reads");
		let ooo_hit = store.read_point("a", 210).await.expect("reads");
		let miss = store.read_point("a", 205).await.expect("reads");
		drop(store);
		assert_eq!(unsorted, 1, "the second segment is out of order");
		assert_eq!(paged_hit, Some(bd("5")));
		assert_eq!(ooo_hit, Some(bd("202")));
		assert_eq!(miss, None);
	}

	#[tokio::test]
	async fn reconcile_sorts_an_out_of_order_segment_in_place() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		// One out-of-order segment.
		let d = store.seal("a", &schema(), &[100_i64, 130, 110, 120], &[bd("1"), bd("4"), bd("2"), bd("3")]).await.expect("seals ooo");
		assert_eq!(store.aspect_stats("a").await.expect("stats").unsorted_segments, 1);
		// The point lookup answers correctly even before reconciliation (linear scan).
		assert_eq!(store.read_point("a", 110).await.expect("reads"), Some(bd("2")));
		// Reconcile: the segment is rewritten sorted at the same id.
		let changed = store.reconcile_segment("a", d.id).await.expect("reconciles");
		let stats = store.aspect_stats("a").await.expect("stats");
		// The rows are unchanged in content, now in timestamp order, and still found.
		let (ts, vs) = store.read_time_range("a", 0, 1000).await.expect("reads");
		let still = store.read_point("a", 110).await.expect("reads");
		// A second reconcile is a no-op.
		let again = store.reconcile_segment("a", d.id).await.expect("reconciles");
		drop(store);
		assert!(changed, "an out-of-order segment is rewritten");
		assert_eq!(stats.segment_count, 1, "reconcile replaces, it does not add a segment");
		assert_eq!(stats.unsorted_segments, 0, "the segment is now sorted");
		assert_eq!(ts, vec![100, 110, 120, 130]);
		assert_eq!(vs, vec![Some(bd("1")), Some(bd("2")), Some(bd("3")), Some(bd("4"))]);
		assert_eq!(still, Some(bd("2")));
		assert!(!again, "a sorted segment reconciles to a no-op");
	}

	/// Reconciling a segment rewrites its bytes, which would strand a sidecar built from the
	/// old bytes (its staleness stamp no longer matches). The rewrite must **regenerate** the
	/// sidecar so the read acceleration survives — proven by deleting the reconciled frame and
	/// showing a downsample still serves from the sidecar.
	#[tokio::test]
	async fn reconcile_regenerates_the_partial_sidecar() {
		use splimes::{Point, Resolution};
		use weft_reduce::{reduce, Aggregation};

		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens").with_partial_sidecar_policy(PartialSidecarPolicy::at(Resolution::Hours, 1));
		store.declare("a", &schema()).await.expect("declares");
		// Out-of-order rows across two hour buckets (schema is SECONDS): hour 0 = [0,3600),
		// hour 1 = [3600,7200). Scrambled so reconcile actually rewrites (and re-codecs) it.
		let ts = vec![3720_i64, 0, 3600, 120, 60, 3660];
		let vs = vec![bd("6"), bd("1"), bd("4"), bd("3"), bd("2"), bd("5")];
		let all: Vec<Point> = ts.iter().zip(&vs).map(|(t, v)| Point::new(DateTime::<Utc>::from_timestamp(*t, 0).expect("instant"), v.clone())).collect();
		let desc = store.seal("a", &schema(), &ts, &vs).await.expect("seals out-of-order");
		assert!(store.load_partial_sidecar("a", &desc).await.expect("reads").is_some(), "a sidecar is written at seal");

		// Reconcile rewrites the segment sorted at the same id → new bytes → the old sidecar
		// stamp is stale, so the reconcile must regenerate it.
		assert!(store.reconcile_segment("a", desc.id).await.expect("reconciles"), "the out-of-order segment is rewritten");
		let new_desc = store.index().all("a").await.expect("index").into_iter().find(|d| d.id == desc.id).expect("segment still present");
		assert!(store.load_partial_sidecar("a", &new_desc).await.expect("reads").is_some_and(|s| s.matches(&new_desc)), "the sidecar was regenerated to match the reconciled bytes");

		// The decisive proof: delete the reconciled frame; a downsample must still succeed,
		// which is only possible if the regenerated sidecar's stamp matches (a stale one
		// would be rejected, forcing a decode of the now-missing frame).
		let aggs = [Aggregation::Min, Aggregation::Max, Aggregation::Sum, Aggregation::First, Aggregation::Last];
		std::fs::remove_file(&new_desc.path).expect("removes the reconciled frame");
		let served = store.downsample_range("a", i64::MIN, i64::MAX, Resolution::Hours, &aggs).await.expect("serves from the regenerated sidecar");
		assert_eq!(served, reduce(&all, Resolution::Hours, None, None, &aggs).expect("reduces"), "the regenerated sidecar reproduces the downsample with no frame on disk");
	}

	#[tokio::test]
	async fn split_segment_carves_a_prefix_and_suffix() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		// One sorted single-block segment over [10,50].
		let d = store.seal("a", &schema(), &[10_i64, 20, 30, 40, 50], &[bd("1"), bd("2"), bd("3"), bd("4"), bd("5")]).await.expect("seals");
		// Split at 35: prefix [10,20,30] stays at d.id, suffix [40,50] to a new id.
		let suffix_id = store.split_segment("a", d.id, 35).await.expect("splits").expect("a real split");
		let stats = store.aspect_stats("a").await.expect("stats");
		// Every row survives, in order, split across the two disjoint segments.
		let (ts, vs) = store.read_time_range("a", 0, 1000).await.expect("reads");
		let prefix_hit = store.read_point("a", 20).await.expect("reads");
		let suffix_hit = store.read_point("a", 40).await.expect("reads");
		drop(store);
		assert_ne!(suffix_id, d.id, "the suffix takes a fresh id");
		assert_eq!(stats.segment_count, 2, "a split turns one segment into two");
		assert_eq!(stats.unsorted_segments, 0, "both halves are internally sorted");
		assert_eq!(stats.overlapping_segments, 0, "prefix < boundary <= suffix — disjoint in time");
		assert_eq!(stats.total_rows, 5, "no row is lost or duplicated");
		assert_eq!(ts, vec![10, 20, 30, 40, 50]);
		assert_eq!(vs, vec![Some(bd("1")), Some(bd("2")), Some(bd("3")), Some(bd("4")), Some(bd("5"))]);
		assert_eq!(prefix_hit, Some(bd("2")));
		assert_eq!(suffix_hit, Some(bd("4")));
	}

	#[tokio::test]
	async fn split_segment_is_a_noop_at_the_edges() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		let d = store.seal("a", &schema(), &[10_i64, 20, 30], &[bd("1"), bd("2"), bd("3")]).await.expect("seals");
		// Boundary before the first row and after the last row both leave every row on
		// one side — nothing to carve.
		let before = store.split_segment("a", d.id, 5).await.expect("splits");
		let after = store.split_segment("a", d.id, 100).await.expect("splits");
		// A boundary equal to the first timestamp is at-or-after it → whole segment is the
		// suffix → still degenerate.
		let at_first = store.split_segment("a", d.id, 10).await.expect("splits");
		let count = store.segment_count("a").await.expect("counts");
		drop(store);
		assert_eq!(before, None);
		assert_eq!(after, None);
		assert_eq!(at_first, None);
		assert_eq!(count, 1, "a degenerate split writes no new segment");
	}

	#[tokio::test]
	async fn split_segment_rejects_an_out_of_order_segment() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		// An out-of-order segment violates split_index's sorted precondition.
		let d = store.seal("a", &schema(), &[30_i64, 10, 20], &[bd("3"), bd("1"), bd("2")]).await.expect("seals ooo");
		let err = store.split_segment("a", d.id, 15).await.expect_err("out-of-order split is rejected");
		let count = store.segment_count("a").await.expect("counts");
		drop(store);
		assert!(err.to_string().contains("out of order"), "the error names the cause: {err}");
		assert_eq!(count, 1, "a rejected split leaves the store untouched");
	}

	#[tokio::test]
	async fn split_segment_preserves_a_paged_nullable_frame() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		// A paged, nullable, sorted segment over [0,70] at page height 3 (a null at 30).
		let ts: Vec<i64> = (0..8).map(|i| i * 10).collect();
		let vs: Vec<Option<BigDecimal>> = (0..8).map(|i| if i == 3 { None } else { Some(BigDecimal::from(i)) }).collect();
		let d = store.seal_paged_nullable("a", &schema(), &ts, &vs, 3).await.expect("seals paged nullable");
		let suffix_id = store.split_segment("a", d.id, 35).await.expect("splits").expect("a real split");
		let stats = store.aspect_stats("a").await.expect("stats");
		let (rts, rvs) = store.read_time_range("a", 0, 1000).await.expect("reads");
		let null_hit = store.read_point("a", 30).await.expect("reads");
		let suffix_hit = store.read_point("a", 40).await.expect("reads");
		// Both halves keep the paged frame version (they re-seal at the source page height).
		let prefix_bytes = std::fs::read(dir.path().join("segments").join(format!("a-{}.weftseg", d.id))).expect("prefix file");
		let suffix_bytes = std::fs::read(dir.path().join("segments").join(format!("a-{suffix_id}.weftseg"))).expect("suffix file");
		drop(store);
		assert_eq!(stats.segment_count, 2);
		assert_eq!(stats.total_rows, 8, "the null row is preserved across the split");
		assert_eq!(rts, ts);
		assert_eq!(rvs, vs);
		assert_eq!(null_hit, None, "the null at 30 stays null (present-bit cleared)");
		assert_eq!(suffix_hit, Some(bd("4")));
		assert!(PagedSegment::read_from(&prefix_bytes).is_ok(), "prefix keeps the paged frame");
		assert!(PagedSegment::read_from(&suffix_bytes).is_ok(), "suffix keeps the paged frame");
	}

	#[tokio::test]
	async fn reconcile_aspect_clears_the_unsorted_count() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		// One ordered, two out-of-order (one of them paged), plus a nullable row.
		store.seal("a", &schema(), &[0_i64, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.expect("ordered");
		store.seal("a", &schema(), &[100_i64, 130, 110], &[bd("4"), bd("5"), bd("6")]).await.expect("ooo single");
		store.seal_paged_nullable("a", &schema(), &[200_i64, 240, 210, 230], &[Some(bd("7")), None, Some(bd("9")), Some(bd("8"))], 2).await.expect("ooo paged");
		assert_eq!(store.aspect_stats("a").await.expect("stats").unsorted_segments, 2);
		let reconciled = store.reconcile_aspect("a").await.expect("reconciles");
		let stats = store.aspect_stats("a").await.expect("stats");
		// The paged segment stayed paged and sorted; its rows read back in order.
		let (ts, vs) = store.read_time_range("a", 200, 240).await.expect("reads");
		let null_hit = store.read_point("a", 240).await.expect("reads");
		drop(store);
		assert_eq!(reconciled, 2, "both out-of-order segments were rewritten");
		assert_eq!(stats.segment_count, 3, "reconcile replaces in place");
		assert_eq!(stats.unsorted_segments, 0);
		assert_eq!(ts, vec![200, 210, 230, 240]);
		assert_eq!(vs, vec![Some(bd("7")), Some(bd("9")), Some(bd("8")), None]);
		assert_eq!(null_hit, None, "the null row stays null after reconciliation");
	}

	#[tokio::test]
	async fn threshold_reconcile_holds_below_the_threshold_and_fires_at_it() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		// One out-of-order segment — backlog of 1.
		store.seal("a", &schema(), &[100_i64, 130, 110], &[bd("1"), bd("3"), bd("2")]).await.expect("ooo one");
		assert_eq!(store.aspect_stats("a").await.expect("stats").unsorted_segments, 1);
		// Threshold 2 is not met: the pass holds, nothing is rewritten.
		let held = store.reconcile_aspect_if_unsorted_exceeds("a", 2).await.expect("polls");
		assert_eq!(held, None, "below the threshold the backlog is left alone");
		assert_eq!(store.aspect_stats("a").await.expect("stats").unsorted_segments, 1, "still out of order");
		// A second out-of-order segment pushes the backlog to 2 — the threshold now fires.
		store.seal("a", &schema(), &[200_i64, 240, 210], &[bd("4"), bd("6"), bd("5")]).await.expect("ooo two");
		assert_eq!(store.aspect_stats("a").await.expect("stats").unsorted_segments, 2);
		let fired = store.reconcile_aspect_if_unsorted_exceeds("a", 2).await.expect("fires");
		let after = store.aspect_stats("a").await.expect("stats").unsorted_segments;
		drop(store);
		assert_eq!(fired, Some(2), "at the threshold both out-of-order segments are rewritten");
		assert_eq!(after, 0, "the backlog is cleared once the pass fires");
	}

	#[tokio::test]
	async fn threshold_reconcile_clamps_zero_to_one_and_no_ops_when_ordered() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		// A fully-ordered aspect: even threshold 0 (clamped to 1) must not fire.
		store.seal("a", &schema(), &[0_i64, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.expect("ordered");
		let clamped = store.reconcile_aspect_if_unsorted_exceeds("a", 0).await.expect("polls");
		assert_eq!(clamped, None, "threshold 0 clamps to 1, and an ordered aspect never fires");
		// Add an out-of-order segment: threshold 0 (clamped to 1) now fires on the backlog of 1.
		store.seal("a", &schema(), &[100_i64, 130, 110], &[bd("4"), bd("6"), bd("5")]).await.expect("ooo");
		let fired = store.reconcile_aspect_if_unsorted_exceeds("a", 0).await.expect("fires");
		let after = store.aspect_stats("a").await.expect("stats").unsorted_segments;
		drop(store);
		assert_eq!(fired, Some(1), "clamped threshold 1 fires on the single out-of-order segment");
		assert_eq!(after, 0);
	}

	#[tokio::test]
	async fn threshold_sweep_reconciles_only_aspects_over_the_threshold() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares a");
		store.declare("b", &schema()).await.expect("declares b");
		// aspect a: two out-of-order segments (backlog 2).
		store.seal("a", &schema(), &[100_i64, 130, 110], &[bd("1"), bd("3"), bd("2")]).await.expect("a ooo1");
		store.seal("a", &schema(), &[200_i64, 240, 210], &[bd("4"), bd("6"), bd("5")]).await.expect("a ooo2");
		// aspect b: one out-of-order segment (backlog 1).
		store.seal("b", &schema(), &[100_i64, 130, 110], &[bd("7"), bd("9"), bd("8")]).await.expect("b ooo1");
		// Sweep at threshold 2: only aspect a fires; b holds below the threshold.
		let sweep = store.reconcile_all_over_threshold(2, MaintenanceWait::Skip).await.expect("sweeps");
		let a_after = store.aspect_stats("a").await.expect("stats a").unsorted_segments;
		let b_after = store.aspect_stats("b").await.expect("stats b").unsorted_segments;
		// A second sweep at threshold 1 now clears b too.
		let sweep2 = store.reconcile_all_over_threshold(1, MaintenanceWait::Skip).await.expect("sweeps again");
		let b_final = store.aspect_stats("b").await.expect("stats b").unsorted_segments;
		drop(store);
		assert_eq!(sweep.aspects_scanned, 2);
		assert_eq!(sweep.aspects_reconciled, 1, "only aspect a is over threshold 2");
		assert_eq!(sweep.segments_reconciled, 2, "both of a's out-of-order segments rewritten");
		assert_eq!(a_after, 0, "a is reconciled");
		assert_eq!(b_after, 1, "b held below threshold 2");
		assert_eq!(sweep2.aspects_reconciled, 1, "the second sweep fires on b");
		assert_eq!(sweep2.segments_reconciled, 1);
		assert_eq!(b_final, 0, "b is reconciled by the threshold-1 sweep");
	}

	#[tokio::test]
	async fn hot_cold_reconciles_cold_segments_but_defers_the_hot_tail() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		// Two out-of-order segments: id 0 (cold) and id 1 (the hot tail).
		store.seal("a", &schema(), &[100_i64, 130, 110], &[bd("1"), bd("3"), bd("2")]).await.expect("cold ooo");
		store.seal("a", &schema(), &[200_i64, 240, 210], &[bd("4"), bd("6"), bd("5")]).await.expect("hot ooo");
		assert_eq!(store.aspect_stats("a").await.expect("stats").unsorted_segments, 2);
		// Threshold 3 is above the backlog of 2: the cold segment is reconciled anyway,
		// the hot tail is deferred.
		let outcome = store.reconcile_aspect_hot_cold("a", 3).await.expect("reconciles");
		let after = store.aspect_stats("a").await.expect("stats").unsorted_segments;
		// The cold segment now binary-searches; the hot tail still linear-scans but reads correctly.
		let cold_hit = store.read_point("a", 110).await.expect("reads");
		let hot_hit = store.read_point("a", 210).await.expect("reads");
		drop(store);
		assert_eq!(outcome.cold_reconciled, 1, "the cold segment is reconciled below threshold");
		assert_eq!(outcome.hot_reconciled, 0, "the hot tail is deferred below threshold");
		assert_eq!(outcome.total(), 1);
		assert_eq!(after, 1, "only the hot tail remains out of order");
		assert_eq!(cold_hit, Some(bd("2")));
		assert_eq!(hot_hit, Some(bd("5")));
	}

	#[tokio::test]
	async fn hot_cold_reconciles_the_hot_tail_once_the_backlog_reaches_threshold() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		store.seal("a", &schema(), &[100_i64, 130, 110], &[bd("1"), bd("3"), bd("2")]).await.expect("cold ooo");
		store.seal("a", &schema(), &[200_i64, 240, 210], &[bd("4"), bd("6"), bd("5")]).await.expect("hot ooo");
		// Threshold 2 == the backlog: the hot tail fires alongside the cold segment.
		let outcome = store.reconcile_aspect_hot_cold("a", 2).await.expect("reconciles");
		let after = store.aspect_stats("a").await.expect("stats").unsorted_segments;
		drop(store);
		assert_eq!(outcome.cold_reconciled, 1);
		assert_eq!(outcome.hot_reconciled, 1, "the hot tail fires at the threshold");
		assert_eq!(after, 0, "the whole aspect is now ordered");
	}

	#[tokio::test]
	async fn hot_cold_leaves_a_sorted_hot_tail_alone() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		// Cold out-of-order segment, then a sorted hot tail.
		store.seal("a", &schema(), &[100_i64, 130, 110], &[bd("1"), bd("3"), bd("2")]).await.expect("cold ooo");
		store.seal("a", &schema(), &[200_i64, 210, 220], &[bd("4"), bd("5"), bd("6")]).await.expect("sorted tail");
		// Even at threshold 1 (which would fire on the backlog of 1) the sorted tail is a no-op.
		let outcome = store.reconcile_aspect_hot_cold("a", 1).await.expect("reconciles");
		let after = store.aspect_stats("a").await.expect("stats").unsorted_segments;
		drop(store);
		assert_eq!(outcome.cold_reconciled, 1, "the cold segment is reconciled");
		assert_eq!(outcome.hot_reconciled, 0, "a sorted hot tail needs no rewrite");
		assert_eq!(after, 0);
	}

	#[tokio::test]
	async fn hot_cold_sweep_defers_hot_tails_below_threshold_across_aspects() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares a");
		store.declare("b", &schema()).await.expect("declares b");
		// aspect a: cold + hot out-of-order (backlog 2). aspect b: a lone out-of-order
		// segment — which is itself the hot tail (backlog 1).
		store.seal("a", &schema(), &[100_i64, 130, 110], &[bd("1"), bd("3"), bd("2")]).await.expect("a cold");
		store.seal("a", &schema(), &[200_i64, 240, 210], &[bd("4"), bd("6"), bd("5")]).await.expect("a hot");
		store.seal("b", &schema(), &[100_i64, 130, 110], &[bd("7"), bd("9"), bd("8")]).await.expect("b hot only");
		// Sweep at threshold 2: a's cold segment is reconciled, a's hot tail fires (backlog 2);
		// b's only segment is its hot tail and stays deferred (backlog 1 < 2).
		let sweep = store.reconcile_all_hot_cold(2, MaintenanceWait::Skip).await.expect("sweeps");
		let a_after = store.aspect_stats("a").await.expect("stats a").unsorted_segments;
		let b_after = store.aspect_stats("b").await.expect("stats b").unsorted_segments;
		drop(store);
		assert_eq!(sweep.aspects_scanned, 2);
		assert_eq!(sweep.aspects_reconciled, 1, "only a rewrote a segment");
		assert_eq!(sweep.cold_reconciled, 1, "a's cold segment");
		assert_eq!(sweep.hot_reconciled, 1, "a's hot tail at threshold 2");
		assert_eq!(sweep.segments_reconciled(), 2);
		assert_eq!(a_after, 0, "a fully reconciled");
		assert_eq!(b_after, 1, "b's lone hot tail is deferred below the threshold");
	}

	#[tokio::test]
	async fn reconcile_overlaps_merges_a_pair_newer_wins() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		// Two internally-sorted segments whose windows overlap at 10 and 20.
		store.seal("a", &schema(), &[0_i64, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.expect("older");
		store.seal("a", &schema(), &[10_i64, 20, 30], &[bd("4"), bd("5"), bd("6")]).await.expect("newer");
		assert_eq!(store.aspect_stats("a").await.expect("stats").overlapping_segments, 2);
		let removed = store.reconcile_overlaps("a").await.expect("merges");
		let stats = store.aspect_stats("a").await.expect("stats");
		// The two segments are now one; the overlap is gone and the merged rows are sorted.
		let (ts, vs) = store.read_time_range("a", 0, 1000).await.expect("reads");
		let hit10 = store.read_point("a", 10).await.expect("reads");
		drop(store);
		assert_eq!(removed, 1, "one segment was merged away");
		assert_eq!(stats.segment_count, 1);
		assert_eq!(stats.overlapping_segments, 0, "no cross-segment overlap remains");
		assert_eq!(stats.unsorted_segments, 0);
		// Newer wins at the shared instants 10 and 20; unique instants 0 and 30 survive.
		assert_eq!(ts, vec![0, 10, 20, 30]);
		assert_eq!(vs, vec![Some(bd("1")), Some(bd("4")), Some(bd("5")), Some(bd("6"))]);
		assert_eq!(hit10, Some(bd("4")), "the newer value supersedes the older at 10");
	}

	#[tokio::test]
	async fn reconcile_overlaps_splits_a_dominant_cold_prefix() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		// A long cold base [0..100] and a small late batch [90,100,110] that re-enters
		// only its tail (newer wins at 90 and 100).
		let base_ts: Vec<i64> = (0..=10).map(|i| i * 10).collect();
		let base_vs: Vec<BigDecimal> = (0..=10).map(BigDecimal::from).collect();
		store.seal("a", &schema(), &base_ts, &base_vs).await.expect("base");
		store.seal("a", &schema(), &[90_i64, 100, 110], &[bd("900"), bd("1000"), bd("1100")]).await.expect("late");
		assert_eq!(store.aspect_stats("a").await.expect("stats").overlapping_segments, 2);
		// A tiny-floor policy makes the dominant cold prefix ([0..80], 9 rows) split off
		// from the hot suffix ([90,100,110], 3 rows) rather than a full rewrite.
		let removed = store.reconcile_overlaps_with_policy("a", SplitPolicy::new(1)).await.expect("splits");
		let stats = store.aspect_stats("a").await.expect("stats");
		let (ts, vs) = store.read_time_range("a", 0, 1000).await.expect("reads");
		let cold_hit = store.read_point("a", 50).await.expect("reads");
		let hot_hit = store.read_point("a", 90).await.expect("reads");
		drop(store);
		assert_eq!(removed, 0, "a two-member split keeps two segments — net count unchanged");
		assert_eq!(stats.segment_count, 2, "cold prefix + hot suffix");
		assert_eq!(stats.overlapping_segments, 0, "the two halves are disjoint in time");
		assert_eq!(stats.unsorted_segments, 0);
		assert_eq!(stats.total_rows, 12, "9 cold + 3 hot, deduped on the shared 90/100");
		// The logical data matches a full merge: newer wins at 90 and 100.
		assert_eq!(ts, vec![0, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110]);
		assert_eq!(vs.last(), Some(&Some(bd("1100"))));
		assert_eq!(cold_hit, Some(bd("5")), "cold prefix value preserved");
		assert_eq!(hot_hit, Some(bd("900")), "newer wins in the hot suffix at 90");
	}

	#[tokio::test]
	async fn split_reconcile_leaves_the_cold_prefix_untouched_on_a_later_merge() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		let base_ts: Vec<i64> = (0..=10).map(|i| i * 10).collect();
		let base_vs: Vec<BigDecimal> = (0..=10).map(BigDecimal::from).collect();
		store.seal("a", &schema(), &base_ts, &base_vs).await.expect("base");
		store.seal("a", &schema(), &[90_i64, 100, 110], &[bd("900"), bd("1000"), bd("1100")]).await.expect("late");
		// First reconcile splits: cold prefix [0..80] stays at id 0, hot suffix gets a new id.
		store.reconcile_overlaps_with_policy("a", SplitPolicy::new(1)).await.expect("splits");
		// The cold-prefix segment on disk after the split (id 0).
		let cold_path = dir.path().join("segments").join("a-0.weftseg");
		let cold_before = std::fs::read(&cold_path).expect("cold prefix file");
		// A second late arrival re-enters only the hot window [90,110]; it must NOT pull
		// the cold prefix back in.
		store.seal("a", &schema(), &[105_i64, 115], &[bd("1050"), bd("1150")]).await.expect("later late");
		let removed = store.reconcile_overlaps_with_policy("a", SplitPolicy::new(1)).await.expect("merges the hot tail");
		let stats = store.aspect_stats("a").await.expect("stats");
		let (ts, _vs) = store.read_time_range("a", 0, 1000).await.expect("reads");
		let cold_after = std::fs::read(&cold_path).expect("cold prefix file still present");
		let cold_hit = store.read_point("a", 50).await.expect("reads");
		drop(store);
		// The cold prefix file is byte-identical — it was never rewritten by the second
		// reconcile (the amortized split-not-rewrite win).
		assert_eq!(cold_before, cold_after, "the cold prefix is untouched by the later merge");
		assert_eq!(stats.overlapping_segments, 0, "no overlap remains after the second reconcile");
		assert_eq!(cold_hit, Some(bd("5")), "cold data intact");
		// Every distinct timestamp survives across both reconciles (110 superseded by the
		// second batch is still present; 105 and 115 are new).
		assert_eq!(ts, vec![0, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 105, 110, 115]);
		let _ = removed;
	}

	#[tokio::test]
	async fn reconcile_overlaps_default_policy_full_rewrites_small_segments() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		// The same cold-base + late-tail shape, but under the default 50 MiB split floor
		// these tiny segments never clear it → a single fully-rewritten segment.
		let base_ts: Vec<i64> = (0..=10).map(|i| i * 10).collect();
		let base_vs: Vec<BigDecimal> = (0..=10).map(BigDecimal::from).collect();
		store.seal("a", &schema(), &base_ts, &base_vs).await.expect("base");
		store.seal("a", &schema(), &[90_i64, 100, 110], &[bd("900"), bd("1000"), bd("1100")]).await.expect("late");
		let removed = store.reconcile_overlaps("a").await.expect("merges");
		let stats = store.aspect_stats("a").await.expect("stats");
		drop(store);
		assert_eq!(removed, 1, "default policy full-rewrites: one segment merged away");
		assert_eq!(stats.segment_count, 1, "no split under the 50 MiB floor");
		assert_eq!(stats.overlapping_segments, 0);
	}

	#[tokio::test]
	async fn reconcile_overlaps_merges_a_transitive_chain() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		// A [0,20], B [10,30], C [25,40] — A–B overlap, B–C overlap, so all one component.
		store.seal("a", &schema(), &[0_i64, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.expect("A");
		store.seal("a", &schema(), &[10_i64, 20, 30], &[bd("4"), bd("5"), bd("6")]).await.expect("B");
		store.seal("a", &schema(), &[25_i64, 30, 40], &[bd("7"), bd("8"), bd("9")]).await.expect("C");
		let removed = store.reconcile_overlaps("a").await.expect("merges");
		let stats = store.aspect_stats("a").await.expect("stats");
		let (ts, _vs) = store.read_time_range("a", 0, 1000).await.expect("reads");
		let hit30 = store.read_point("a", 30).await.expect("reads");
		drop(store);
		assert_eq!(removed, 2, "three segments collapse to one");
		assert_eq!(stats.segment_count, 1);
		assert_eq!(stats.overlapping_segments, 0);
		// Distinct timestamps across the chain: 0,10,20,25,30,40 (10,20 from B win; 30 from C wins).
		assert_eq!(ts, vec![0, 10, 20, 25, 30, 40]);
		// C = [25→7, 30→8, 40→9], so C's value at 30 is 8; it supersedes B's 30→6.
		assert_eq!(hit30, Some(bd("8")), "C (newest) wins at 30");
	}

	#[tokio::test]
	async fn reconcile_overlaps_leaves_disjoint_segments_untouched() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		// Two disjoint windows plus one overlapping pair.
		store.seal("a", &schema(), &[0_i64, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.expect("disjoint");
		store.seal("a", &schema(), &[100_i64, 110, 120], &[bd("4"), bd("5"), bd("6")]).await.expect("pair lo");
		store.seal("a", &schema(), &[110_i64, 120, 130], &[bd("7"), bd("8"), bd("9")]).await.expect("pair hi");
		assert_eq!(store.aspect_stats("a").await.expect("stats").overlapping_segments, 2, "only the [100,120]/[110,130] pair overlaps");
		let removed = store.reconcile_overlaps("a").await.expect("merges");
		let stats = store.aspect_stats("a").await.expect("stats");
		// The disjoint segment survives; the overlapping pair merges to one → 2 segments.
		let (ts0, _) = store.read_time_range("a", 0, 50).await.expect("reads disjoint");
		drop(store);
		assert_eq!(removed, 1, "only the overlapping pair merged");
		assert_eq!(stats.segment_count, 2);
		assert_eq!(stats.overlapping_segments, 0);
		assert_eq!(ts0, vec![0, 10, 20], "the disjoint segment is unchanged");
	}

	#[tokio::test]
	async fn reconcile_all_overlaps_sweeps_every_aspect() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares a");
		store.declare("b", &schema()).await.expect("declares b");
		// a: an overlapping pair. b: disjoint segments.
		store.seal("a", &schema(), &[0_i64, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.expect("a older");
		store.seal("a", &schema(), &[10_i64, 20, 30], &[bd("4"), bd("5"), bd("6")]).await.expect("a newer");
		store.seal("b", &schema(), &[0_i64, 10, 20], &[bd("7"), bd("8"), bd("9")]).await.expect("b lo");
		store.seal("b", &schema(), &[100_i64, 110, 120], &[bd("1"), bd("2"), bd("3")]).await.expect("b hi");
		let sweep = store.reconcile_all_overlaps(MaintenanceWait::Skip).await.expect("sweeps");
		let a_after = store.aspect_stats("a").await.expect("stats a").overlapping_segments;
		let b_after = store.aspect_stats("b").await.expect("stats b").overlapping_segments;
		drop(store);
		assert_eq!(sweep.aspects_scanned, 2);
		assert_eq!(sweep.aspects_reconciled, 1, "only a had an overlap to merge");
		assert_eq!(sweep.segments_removed, 1);
		assert_eq!(a_after, 0);
		assert_eq!(b_after, 0, "b never overlapped");
	}

	#[tokio::test]
	async fn reconcile_all_overlaps_with_policy_splits_and_counts_by_overlap() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares a");
		store.declare("b", &schema()).await.expect("declares b");
		// a: a dominant cold prefix + late tail (splits under a tiny floor, net removed 0).
		let base_ts: Vec<i64> = (0..=10).map(|i| i * 10).collect();
		let base_vs: Vec<BigDecimal> = (0..=10).map(BigDecimal::from).collect();
		store.seal("a", &schema(), &base_ts, &base_vs).await.expect("a base");
		store.seal("a", &schema(), &[90_i64, 100, 110], &[bd("900"), bd("1000"), bd("1100")]).await.expect("a late");
		// b: no overlap.
		store.seal("b", &schema(), &[0_i64, 10], &[bd("7"), bd("8")]).await.expect("b lo");
		store.seal("b", &schema(), &[100_i64, 110], &[bd("1"), bd("2")]).await.expect("b hi");
		let sweep = store.reconcile_all_overlaps_with_policy(SplitPolicy::new(1), MaintenanceWait::Skip).await.expect("sweeps");
		let a_stats = store.aspect_stats("a").await.expect("stats a");
		drop(store);
		// a is counted as reconciled even though its split left the segment count
		// unchanged (net removed 0) — counting keys on the pre-pass overlap.
		assert_eq!(sweep.aspects_scanned, 2);
		assert_eq!(sweep.aspects_reconciled, 1, "only a carried overlap");
		assert_eq!(sweep.segments_removed, 0, "a two-member split removes no segment net");
		assert_eq!(a_stats.segment_count, 2, "a split into cold prefix + hot suffix");
		assert_eq!(a_stats.overlapping_segments, 0);
	}

	#[tokio::test]
	async fn squash_folds_disjoint_segments_into_one() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		// Four time-disjoint segments (as repeated split carve-offs would leave).
		store.seal("a", &schema(), &[0_i64, 10], &[bd("0"), bd("1")]).await.expect("s0");
		store.seal("a", &schema(), &[20_i64, 30], &[bd("2"), bd("3")]).await.expect("s1");
		store.seal("a", &schema(), &[40_i64, 50], &[bd("4"), bd("5")]).await.expect("s2");
		store.seal("a", &schema(), &[60_i64, 70], &[bd("6"), bd("7")]).await.expect("s3");
		let removed = store.squash_aspect("a").await.expect("squashes");
		let stats = store.aspect_stats("a").await.expect("stats");
		let (ts, vs) = store.read_time_range("a", 0, 1000).await.expect("reads");
		let hit = store.read_point("a", 50).await.expect("reads");
		drop(store);
		assert_eq!(removed, 3, "four segments squashed to one");
		assert_eq!(stats.segment_count, 1);
		assert_eq!(stats.unsorted_segments, 0);
		assert_eq!(stats.overlapping_segments, 0);
		assert_eq!(ts, vec![0, 10, 20, 30, 40, 50, 60, 70], "every row preserved in order");
		assert_eq!(vs.len(), 8);
		assert_eq!(hit, Some(bd("5")));
	}

	#[tokio::test]
	async fn squash_all_over_threshold_sweeps_only_aspects_over_the_cap() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares a");
		store.declare("b", &schema()).await.expect("declares b");
		// a: 3 disjoint segments (over a cap of 2). b: 1 segment (under).
		store.seal("a", &schema(), &[0_i64, 10], &[bd("0"), bd("1")]).await.expect("a0");
		store.seal("a", &schema(), &[20_i64, 30], &[bd("2"), bd("3")]).await.expect("a1");
		store.seal("a", &schema(), &[40_i64, 50], &[bd("4"), bd("5")]).await.expect("a2");
		store.seal("b", &schema(), &[0_i64, 10], &[bd("7"), bd("8")]).await.expect("b0");
		let sweep = store.squash_all_over_threshold(2, MaintenanceWait::Skip).await.expect("sweeps");
		let a_count = store.segment_count("a").await.expect("a count");
		let b_count = store.segment_count("b").await.expect("b count");
		drop(store);
		assert_eq!(sweep.aspects_scanned, 2);
		assert_eq!(sweep.aspects_squashed, 1, "only a exceeded the cap of 2");
		assert_eq!(sweep.segments_removed, 2, "a's 3 segments squashed to 1");
		assert_eq!(a_count, 1);
		assert_eq!(b_count, 1, "b was under the cap and untouched");
	}

	#[tokio::test]
	async fn squash_is_a_noop_below_two_segments() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		store.seal("a", &schema(), &[0_i64, 10], &[bd("0"), bd("1")]).await.expect("s0");
		let removed = store.squash_aspect("a").await.expect("no-op");
		drop(store);
		assert_eq!(removed, 0, "a single segment has nothing to squash");
	}

	#[tokio::test]
	async fn squash_if_exceeds_gates_on_the_segment_count() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		store.seal("a", &schema(), &[0_i64, 10], &[bd("0"), bd("1")]).await.expect("s0");
		store.seal("a", &schema(), &[20_i64, 30], &[bd("2"), bd("3")]).await.expect("s1");
		store.seal("a", &schema(), &[40_i64, 50], &[bd("4"), bd("5")]).await.expect("s2");
		// Three segments, cap 3 → holds (count is not > 3).
		let held = store.squash_aspect_if_exceeds("a", 3).await.expect("gate");
		// Cap 2 → fires (3 > 2), squashing to one.
		let fired = store.squash_aspect_if_exceeds("a", 2).await.expect("gate");
		let count = store.segment_count("a").await.expect("count");
		drop(store);
		assert_eq!(held, None, "at or below the cap the squash holds");
		assert_eq!(fired, Some(2), "above the cap it squashes 3 → 1 (2 removed)");
		assert_eq!(count, 1);
	}

	#[tokio::test]
	async fn target_rows_compaction_coalesces_toward_the_target_not_to_one() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		// Six time-disjoint 2-row segments; target 4 groups them in pairs (2+2 reaches 4),
		// so three pairs collapse to three segments — NOT to one, the whole point vs squash.
		for seg in 0..6_i64 {
			let base = seg * 100;
			store.seal("a", &schema(), &[base, base + 10], &[bd(&seg.to_string()), bd(&(seg + 10).to_string())]).await.expect("seals");
		}
		let removed = store.squash_aspect_to_target_rows("a", 4).await.expect("compacts");
		let count = store.segment_count("a").await.expect("count");
		let (ts, _) = store.read_time_range("a", 0, 10_000).await.expect("reads");
		let hit = store.read_point("a", 510).await.expect("reads"); // seg 5's second row → value 15
		drop(store);
		assert_eq!(removed, 3, "three pairs each drop one segment");
		assert_eq!(count, 3, "six segments folded toward the target become three, not one");
		assert_eq!(ts.len(), 12, "every row preserved");
		assert_eq!(ts, vec![0, 10, 100, 110, 200, 210, 300, 310, 400, 410, 500, 510], "rows stay time-sorted across the compaction");
		assert_eq!(hit, Some(bd("15")), "a point still resolves after compaction");
	}

	#[tokio::test]
	async fn target_rows_compaction_leaves_already_large_segments_untouched() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		// A 4-row segment already at/over target 3 (its own singleton group, untouched) then
		// two 1-row segments that coalesce into one.
		store.seal("a", &schema(), &[0_i64, 1, 2, 3], &[bd("0"), bd("1"), bd("2"), bd("3")]).await.expect("big");
		store.seal("a", &schema(), &[100_i64], &[bd("4")]).await.expect("small0");
		store.seal("a", &schema(), &[200_i64], &[bd("5")]).await.expect("small1");
		let removed = store.squash_aspect_to_target_rows("a", 3).await.expect("compacts");
		let count = store.segment_count("a").await.expect("count");
		let (ts, _) = store.read_time_range("a", 0, 10_000).await.expect("reads");
		drop(store);
		assert_eq!(removed, 1, "only the two small segments merged; the large one was left alone");
		assert_eq!(count, 2, "the untouched big segment + the merged pair");
		assert_eq!(ts, vec![0, 1, 2, 3, 100, 200], "all rows preserved");
	}

	#[tokio::test]
	async fn squash_all_to_target_rows_sweeps_every_aspect() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares a");
		store.declare("b", &schema()).await.expect("declares b");
		// a: six 2-row segments (fragmented → coalesces in pairs at target 4). b: one 2-row
		// segment (nothing to coalesce).
		for seg in 0..6_i64 {
			let base = seg * 100;
			store.seal("a", &schema(), &[base, base + 10], &[bd("0"), bd("1")]).await.expect("a seg");
		}
		store.seal("b", &schema(), &[0_i64, 10], &[bd("7"), bd("8")]).await.expect("b0");
		let sweep = store.squash_all_to_target_rows(4, MaintenanceWait::Skip).await.expect("sweeps");
		let a_count = store.segment_count("a").await.expect("a count");
		let b_count = store.segment_count("b").await.expect("b count");
		drop(store);
		assert_eq!(sweep.aspects_scanned, 2);
		assert_eq!(sweep.aspects_squashed, 1, "only a was fragmented enough to coalesce");
		assert_eq!(sweep.segments_removed, 3, "a's six segments folded to three (three pairs)");
		assert_eq!(a_count, 3);
		assert_eq!(b_count, 1, "a single-segment aspect is untouched");
	}

	#[tokio::test]
	async fn fragmentation_gate_skips_well_sized_aspects() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		// Six 2-row segments = 12 rows. Target 6 → ideal ⌈12/6⌉ = 2 segments.
		for seg in 0..6_i64 {
			let base = seg * 100;
			store.seal("a", &schema(), &[base, base + 10], &[bd("1"), bd("2")]).await.expect("seals");
		}
		// segment_count 6 > ideal 2 → the gate fires and coalesces toward the target.
		let first = store.squash_aspect_to_target_rows_if_fragmented("a", 6).await.expect("gated");
		let after = store.segment_count("a").await.expect("count");
		// Now 2 segments of 6 rows = the ideal → a second gated call holds without a rescan.
		let second = store.squash_aspect_to_target_rows_if_fragmented("a", 6).await.expect("gated");
		drop(store);
		assert_eq!(first, Some(4), "an over-fragmented aspect coalesces (6 → 2, 4 removed)");
		assert_eq!(after, 2);
		assert_eq!(second, None, "a well-sized aspect (at its ideal segment count) is skipped");
	}

	#[tokio::test]
	async fn gated_store_sweep_only_touches_fragmented_aspects() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("frag", &schema()).await.expect("declares frag");
		store.declare("tidy", &schema()).await.expect("declares tidy");
		// frag: four 2-row segments (8 rows, ideal 2 at target 4 → over-fragmented).
		for seg in 0..4_i64 {
			let base = seg * 100;
			store.seal("frag", &schema(), &[base, base + 10], &[bd("1"), bd("2")]).await.expect("frag seg");
		}
		// tidy: one 4-row segment already at its ideal.
		store.seal("tidy", &schema(), &[0_i64, 1, 2, 3], &[bd("1"), bd("2"), bd("3"), bd("4")]).await.expect("tidy seg");
		let sweep = store.squash_all_to_target_rows_if_fragmented(4, MaintenanceWait::Skip).await.expect("gated sweep");
		let frag_count = store.segment_count("frag").await.expect("frag count");
		let tidy_count = store.segment_count("tidy").await.expect("tidy count");
		drop(store);
		assert_eq!(sweep.aspects_scanned, 2);
		assert_eq!(sweep.aspects_squashed, 1, "only the fragmented aspect coalesced");
		assert_eq!(sweep.segments_removed, 2, "frag's four segments folded to two");
		assert_eq!(frag_count, 2);
		assert_eq!(tidy_count, 1, "the tidy aspect was skipped by the gate");
	}

	#[tokio::test]
	async fn target_rows_compaction_is_a_noop_at_zero_and_below_two_segments() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		store.seal("a", &schema(), &[0_i64, 10], &[bd("0"), bd("1")]).await.expect("s0");
		store.seal("a", &schema(), &[20_i64, 30], &[bd("2"), bd("3")]).await.expect("s1");
		store.seal("a", &schema(), &[40_i64, 50], &[bd("4"), bd("5")]).await.expect("s2");
		// target 0 clamps to 1 → every segment closes its own singleton group → no rewrite.
		let zero = store.squash_aspect_to_target_rows("a", 0).await.expect("no-op");
		let count_after_zero = store.segment_count("a").await.expect("count");
		// A single-segment aspect has nothing to coalesce.
		store.declare("b", &schema()).await.expect("declares b");
		store.seal("b", &schema(), &[0_i64], &[bd("9")]).await.expect("b0");
		let one = store.squash_aspect_to_target_rows("b", 1000).await.expect("no-op");
		drop(store);
		assert_eq!(zero, 0, "target 0 clamps to 1 and coalesces nothing");
		assert_eq!(count_after_zero, 3, "the three segments are untouched");
		assert_eq!(one, 0, "a single segment has nothing to compact");
	}

	/// The six aspects of the sweep-isolation fixture, in name order. The second,
	/// [`SWEEP_BAD`], carries the truncated frame, so four healthy aspects sort after it.
	const SWEEP_ASPECTS: [&str; 6] = ["s1", "s2", "s3", "s4", "s5", "s6"];
	/// The fixture aspect whose lowest-id frame is truncated on disk.
	const SWEEP_BAD: &str = "s2";

	/// Build the sweep-isolation fixture under `dir`: six aspects, each with three 3-row
	/// segments that are internally out of order **and** transitively time-overlapping
	/// (`[100,130]`, `[120,150]`, `[140,170]`), so every store-wide sweep (reconcile,
	/// hot/cold, overlap, squash, compact) has work on every aspect. The [`SWEEP_BAD`]
	/// aspect's lowest-id frame is then cut to half its length, the shape a torn write or
	/// a short copy leaves, so any sweep that decodes it fails on that aspect. Returns the
	/// store and the truncated frame's path.
	async fn sweep_isolation_fixture(dir: &Path) -> (SegmentStore, String) {
		let store = SegmentStore::open(dir).await.expect("opens");
		let mut bad_frame = String::new();
		for aspect in SWEEP_ASPECTS {
			store.declare(aspect, &schema()).await.expect("declares");
			let first = store.seal(aspect, &schema(), &[100_i64, 130, 110], &[bd("1"), bd("3"), bd("2")]).await.expect("seals 0");
			store.seal(aspect, &schema(), &[120_i64, 150, 125], &[bd("4"), bd("6"), bd("5")]).await.expect("seals 1");
			store.seal(aspect, &schema(), &[140_i64, 170, 145], &[bd("7"), bd("9"), bd("8")]).await.expect("seals 2");
			if aspect == SWEEP_BAD {
				let file = std::fs::OpenOptions::new().write(true).open(&first.path).expect("opens frame");
				let len = file.metadata().expect("stats frame").len();
				file.set_len(len / 2).expect("truncates frame");
				bad_frame = first.path;
			}
		}
		(store, bad_frame)
	}

	/// The healthy fixture aspects (every one but [`SWEEP_BAD`]) whose stats fail
	/// `maintained` after a sweep — empty when the sweep reached all five.
	async fn unmaintained_healthy(store: &SegmentStore, maintained: fn(&AspectStorageStats) -> bool) -> Vec<&'static str> {
		let mut out = Vec::new();
		for aspect in SWEEP_ASPECTS.into_iter().filter(|aspect| *aspect != SWEEP_BAD) {
			if !maintained(&store.aspect_stats(aspect).await.expect("stats")) {
				out.push(aspect);
			}
		}
		out
	}

	/// Assert a sweep's `failed` list names exactly [`SWEEP_BAD`], and that its error is
	/// the truncated frame's decode rather than some unrelated failure.
	fn assert_only_the_bad_aspect_failed(failed: &[(String, anyhow::Error)], bad_frame: &str) {
		let names: Vec<&str> = failed.iter().map(|(aspect, _)| aspect.as_str()).collect();
		assert_eq!(names, vec![SWEEP_BAD], "only the truncated aspect fails: {failed:?}");
		let error = format!("{:#}", failed[0].1);
		assert!(error.contains(bad_frame), "the failure is the truncated frame's decode: {error}");
	}

	/// Crash-consistency S4: a truncated frame in one aspect must not stop the store-wide
	/// reconcile sweeps (threshold and hot/cold) at that aspect. Before S4 the first
	/// per-aspect error was propagated with `?`, so `s3`..`s6` were never reconciled.
	#[tokio::test]
	async fn store_wide_reconcile_sweeps_isolate_a_truncated_frame() {
		let dir = TempDir::new().expect("tempdir");
		let (store, bad_frame) = sweep_isolation_fixture(dir.path()).await;
		let sweep = store.reconcile_all_over_threshold(1, MaintenanceWait::Skip).await.expect("a bad aspect does not fail the sweep");
		let missed = unmaintained_healthy(&store, |stats| stats.unsorted_segments == 0).await;
		let bad = store.aspect_stats(SWEEP_BAD).await.expect("bad stats");
		drop(store);
		assert_eq!(sweep.aspects_scanned, 6);
		assert_eq!(sweep.aspects_reconciled, 5, "every healthy aspect is reconciled");
		assert_eq!(sweep.segments_reconciled, 15, "three segments in each of five aspects");
		assert_only_the_bad_aspect_failed(&sweep.failed, &bad_frame);
		assert_eq!(missed, Vec::<&str>::new(), "healthy aspects left out of order");
		assert_eq!(bad.unsorted_segments, 3, "the bad aspect is left as it was");

		let dir = TempDir::new().expect("tempdir");
		let (store, bad_frame) = sweep_isolation_fixture(dir.path()).await;
		let sweep = store.reconcile_all_hot_cold(1, MaintenanceWait::Skip).await.expect("a bad aspect does not fail the sweep");
		let missed = unmaintained_healthy(&store, |stats| stats.unsorted_segments == 0).await;
		drop(store);
		assert_eq!(sweep.aspects_scanned, 6);
		assert_eq!(sweep.aspects_reconciled, 5);
		assert_eq!(sweep.cold_reconciled, 10, "two cold segments in each of five aspects");
		assert_eq!(sweep.hot_reconciled, 5, "threshold 1 fires every healthy hot tail");
		assert_only_the_bad_aspect_failed(&sweep.failed, &bad_frame);
		assert_eq!(missed, Vec::<&str>::new(), "healthy aspects left out of order");
	}

	/// Crash-consistency S4: the store-wide overlap merges (default and split policy) keep
	/// going past an aspect whose frame is truncated.
	#[tokio::test]
	async fn store_wide_overlap_sweeps_isolate_a_truncated_frame() {
		let dir = TempDir::new().expect("tempdir");
		let (store, bad_frame) = sweep_isolation_fixture(dir.path()).await;
		let sweep = store.reconcile_all_overlaps(MaintenanceWait::Skip).await.expect("a bad aspect does not fail the sweep");
		let missed = unmaintained_healthy(&store, |stats| stats.overlapping_segments == 0 && stats.segment_count == 1).await;
		let bad = store.aspect_stats(SWEEP_BAD).await.expect("bad stats");
		drop(store);
		assert_eq!(sweep.aspects_scanned, 6);
		assert_eq!(sweep.aspects_reconciled, 5, "every healthy aspect is merged");
		assert_eq!(sweep.segments_removed, 10, "each healthy 3-member component merges to one");
		assert_only_the_bad_aspect_failed(&sweep.failed, &bad_frame);
		assert_eq!(missed, Vec::<&str>::new(), "healthy aspects left overlapping");
		assert_eq!((bad.segment_count, bad.overlapping_segments), (3, 3), "the bad aspect is left as it was");

		let dir = TempDir::new().expect("tempdir");
		let (store, bad_frame) = sweep_isolation_fixture(dir.path()).await;
		let sweep = store.reconcile_all_overlaps_with_policy(SplitPolicy::new(1), MaintenanceWait::Skip).await.expect("a bad aspect does not fail the sweep");
		let missed = unmaintained_healthy(&store, |stats| stats.overlapping_segments == 0).await;
		drop(store);
		assert_eq!(sweep.aspects_scanned, 6);
		assert_eq!(sweep.aspects_reconciled, 5);
		assert_only_the_bad_aspect_failed(&sweep.failed, &bad_frame);
		assert_eq!(missed, Vec::<&str>::new(), "healthy aspects left overlapping");
	}

	/// Crash-consistency S4: the store-wide squash keeps going past an aspect whose frame
	/// is truncated.
	#[tokio::test]
	async fn store_wide_squash_sweep_isolates_a_truncated_frame() {
		let dir = TempDir::new().expect("tempdir");
		let (store, bad_frame) = sweep_isolation_fixture(dir.path()).await;
		let sweep = store.squash_all_over_threshold(2, MaintenanceWait::Skip).await.expect("a bad aspect does not fail the sweep");
		let missed = unmaintained_healthy(&store, |stats| stats.segment_count == 1).await;
		let bad = store.aspect_stats(SWEEP_BAD).await.expect("bad stats");
		drop(store);
		assert_eq!(sweep.aspects_scanned, 6);
		assert_eq!(sweep.aspects_squashed, 5, "every healthy aspect is squashed");
		assert_eq!(sweep.segments_removed, 10, "each healthy aspect folds 3 segments to 1");
		assert_only_the_bad_aspect_failed(&sweep.failed, &bad_frame);
		assert_eq!(missed, Vec::<&str>::new(), "healthy aspects left unsquashed");
		assert_eq!(bad.segment_count, 3, "the bad aspect is left as it was");
	}

	/// Crash-consistency S4: the store-wide size-targeted compactions (the forced sweep and
	/// the daemon's fragmentation-gated one) keep going past an aspect whose frame is
	/// truncated.
	#[tokio::test]
	async fn store_wide_compact_sweeps_isolate_a_truncated_frame() {
		let dir = TempDir::new().expect("tempdir");
		let (store, bad_frame) = sweep_isolation_fixture(dir.path()).await;
		let sweep = store.squash_all_to_target_rows(6, MaintenanceWait::Skip).await.expect("a bad aspect does not fail the sweep");
		let missed = unmaintained_healthy(&store, |stats| stats.segment_count == 2).await;
		let bad = store.aspect_stats(SWEEP_BAD).await.expect("bad stats");
		drop(store);
		assert_eq!(sweep.aspects_scanned, 6);
		assert_eq!(sweep.aspects_squashed, 5, "every healthy aspect is compacted");
		assert_eq!(sweep.segments_removed, 5, "each healthy aspect pairs its first two 3-row segments");
		assert_only_the_bad_aspect_failed(&sweep.failed, &bad_frame);
		assert_eq!(missed, Vec::<&str>::new(), "healthy aspects left uncompacted");
		assert_eq!(bad.segment_count, 3, "the bad aspect is left as it was");

		let dir = TempDir::new().expect("tempdir");
		let (store, bad_frame) = sweep_isolation_fixture(dir.path()).await;
		let sweep = store.squash_all_to_target_rows_if_fragmented(6, MaintenanceWait::Skip).await.expect("a bad aspect does not fail the sweep");
		let missed = unmaintained_healthy(&store, |stats| stats.segment_count == 2).await;
		drop(store);
		assert_eq!(sweep.aspects_scanned, 6);
		assert_eq!(sweep.aspects_squashed, 5);
		assert_eq!(sweep.segments_removed, 5);
		assert_only_the_bad_aspect_failed(&sweep.failed, &bad_frame);
		assert_eq!(missed, Vec::<&str>::new(), "healthy aspects left uncompacted");
	}

	#[tokio::test]
	async fn reconcile_overlaps_is_a_noop_without_overlap() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		store.seal("a", &schema(), &[0_i64, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.expect("s0");
		store.seal("a", &schema(), &[100_i64, 110, 120], &[bd("4"), bd("5"), bd("6")]).await.expect("s1");
		let removed = store.reconcile_overlaps("a").await.expect("no-op");
		let count = store.segment_count("a").await.expect("count");
		drop(store);
		assert_eq!(removed, 0, "disjoint segments need no merge");
		assert_eq!(count, 2);
	}

	#[tokio::test]
	async fn reconcile_missing_segment_errors() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		let err = store.reconcile_segment("a", 7).await;
		drop(store);
		assert!(err.is_err(), "reconciling an absent segment id is an error");
	}

	#[tokio::test]
	async fn read_point_last_writer_wins_across_overlapping_segments() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// Two segments both spanning ts=20 with different values there; the later seal wins.
		store.seal("a", &schema(), &[10_i64, 20, 30], &[bd("1"), bd("2"), bd("3")]).await.expect("first");
		store.seal("a", &schema(), &[20_i64, 40], &[bd("99"), bd("4")]).await.expect("second");
		let at_20 = store.read_point("a", 20).await.expect("reads");
		// A value only the first segment carries is still found.
		let at_10 = store.read_point("a", 10).await.expect("reads");
		drop(store);
		assert_eq!(at_20, Some(bd("99")), "the more recently sealed segment wins at the shared instant");
		assert_eq!(at_10, Some(bd("1")));
	}

	#[tokio::test]
	async fn declared_schema_seals_without_passing_it() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// Declare the aspect's schema once, then seal without re-supplying it.
		store.declare("temp", &schema()).await.expect("declares");
		let ts = vec![0_i64, 10, 20];
		let vs = vec![bd("1.5"), bd("2.5"), bd("3.5")];
		let descriptor = store.seal_declared("temp", &ts, &vs).await.expect("seals");
		let (rt, rv) = store.read_time_range("temp", 0, 100).await.expect("reads");
		let got = store.schema_for("temp").await.expect("looks up");
		drop(store);
		assert_eq!(descriptor.row_count, 3);
		assert_eq!(rt, ts);
		assert_eq!(rv, vec![Some(bd("1.5")), Some(bd("2.5")), Some(bd("3.5"))]);
		assert_eq!(got, Some(schema()));
	}

	#[tokio::test]
	async fn seal_declared_without_declaration_fails() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// No declaration ⇒ the catalog-backed seal must refuse and record nothing.
		let err = store.seal_declared("temp", &[0_i64], &[bd("1")]).await;
		let count = store.segment_count("temp").await.expect("counts");
		let missing = store.schema_for("temp").await.expect("looks up");
		drop(store);
		assert!(err.is_err(), "an undeclared aspect cannot be sealed by lookup");
		assert_eq!(count, 0, "the refused seal records nothing");
		assert_eq!(missing, None);
	}

	#[tokio::test]
	async fn declared_schema_survives_reopen() {
		let dir = TempDir::new().expect("tempdir");
		let declared = AspectSchema::new(PhysicalType::ScaledI64 { scale: 2 }, bd("0.005"), TimeUnit::Millis);
		let first = SegmentStore::open_scoped(dir.path(), "market", "BTCUSD").await.expect("opens");
		first.declare("price", &declared).await.expect("declares");
		drop(first);
		// A reopened store under the same scope recovers the encoding it sealed under,
		// and can seal by lookup.
		let reopened = SegmentStore::open_scoped(dir.path(), "market", "BTCUSD").await.expect("reopens");
		let got = reopened.schema_for("price").await.expect("looks up");
		let descriptor = reopened.seal_declared("price", &[0_i64, 1000], &[bd("100.25"), bd("100.50")]).await.expect("seals");
		drop(reopened);
		assert_eq!(got, Some(declared));
		assert_eq!(descriptor.row_count, 2);
	}

	#[tokio::test]
	async fn open_registers_its_database_and_subject() {
		let dir = TempDir::new().expect("tempdir");
		// Scoped stores opened over one root in turn (a root is open once at a time)
		// populate the shared catalog.db hierarchy.
		drop(SegmentStore::open_scoped(dir.path(), "market", "BTCUSD").await.expect("opens"));
		drop(SegmentStore::open_scoped(dir.path(), "iot", "sensor-7").await.expect("opens"));
		// Re-opening the same scope is idempotent — no duplicate rows.
		let market = SegmentStore::open_scoped(dir.path(), "market", "BTCUSD").await.expect("opens");
		let dbs = market.registry().list_databases().await.expect("lists");
		let market_subjects = market.registry().list_subjects("market").await.expect("lists");
		drop(market);
		assert_eq!(dbs, vec!["iot".to_string(), "market".to_string()]);
		assert_eq!(market_subjects, vec!["BTCUSD".to_string()]);
	}

	#[tokio::test]
	async fn list_declared_aspects_scopes_to_the_store() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open_scoped(dir.path(), "d", "s").await.expect("opens");
		store.declare("temp", &schema()).await.expect("declares");
		store.declare("humidity", &schema()).await.expect("declares");
		drop(store);
		// A sibling subject's declaration is not listed here.
		let sibling = SegmentStore::open_scoped(dir.path(), "d", "other").await.expect("opens");
		sibling.declare("pressure", &schema()).await.expect("declares");
		drop(sibling);
		let store = SegmentStore::open_scoped(dir.path(), "d", "s").await.expect("reopens");
		let aspects = store.list_declared_aspects().await.expect("lists");
		drop(store);
		assert_eq!(aspects, vec!["humidity".to_string(), "temp".to_string()]);
	}

	#[tokio::test]
	async fn scopes_isolate_declarations() {
		let dir = TempDir::new().expect("tempdir");
		// Stores over the same root but different subjects share the catalog DB; a
		// declaration under one subject is invisible to the other. A root is open once at
		// a time, so the two scopes take turns.
		let a = SegmentStore::open_scoped(dir.path(), "d", "subject-a").await.expect("opens");
		a.declare("temp", &schema()).await.expect("declares");
		drop(a);
		let b = SegmentStore::open_scoped(dir.path(), "d", "subject-b").await.expect("opens");
		let seen_by_b = b.schema_for("temp").await.expect("looks up");
		drop(b);
		let a = SegmentStore::open_scoped(dir.path(), "d", "subject-a").await.expect("reopens");
		let seen_by_a = a.schema_for("temp").await.expect("looks up");
		drop(a);
		assert_eq!(seen_by_a, Some(schema()));
		assert_eq!(seen_by_b, None, "a sibling subject does not see the declaration");
	}

	#[tokio::test]
	async fn the_directories_to_fsync_are_the_layout_the_created_ancestors_and_the_roots_parent() {
		let dir = TempDir::new().expect("tempdir");
		let paths = |names: &[&str]| -> Vec<PathBuf> { names.iter().map(|n| dir.path().join(n)).collect() };

		// An existing root: `segments/`, the root, and the root's parent.
		std::fs::create_dir(dir.path().join("existing")).unwrap();
		let segments = dir.path().join("existing/segments");
		let created = missing_dirs(&segments).await;
		assert_eq!(created, paths(&["existing/segments"]));
		let mut expected = paths(&["existing/segments", "existing"]);
		expected.push(dir.path().to_path_buf());
		assert_eq!(layout_dirs(&dir.path().join("existing"), &segments, &created), expected);
		assert_eq!(layout_dirs(&dir.path().join("existing"), &segments, &[]), expected, "with nothing created, the root's parent is still synced");

		// A root two levels below an existing directory: every created directory, then the
		// existing parent that holds the topmost one.
		let segments = dir.path().join("a/b/segments");
		let created = missing_dirs(&segments).await;
		assert_eq!(created, paths(&["a/b/segments", "a/b", "a"]));
		let mut expected = paths(&["a/b/segments", "a/b", "a"]);
		expected.push(dir.path().to_path_buf());
		assert_eq!(layout_dirs(&dir.path().join("a/b"), &segments, &created), expected);

		// A bare relative root's parent is the working directory, created or not.
		let created = [PathBuf::from("store/segments"), PathBuf::from("store")];
		let expected = vec![PathBuf::from("store/segments"), PathBuf::from("store"), PathBuf::from(".")];
		assert_eq!(layout_dirs(Path::new("store"), Path::new("store/segments"), &created), expected);
		assert_eq!(layout_dirs(Path::new("store"), Path::new("store/segments"), &[]), expected);
	}

	/// A [`StoreFs`] that records every directory fsync before passing it to [`RealFs`],
	/// and can fail the one for `refuse` with an error of the given kind instead.
	#[derive(Debug, Default)]
	struct RecordingFs {
		synced: std::sync::Mutex<Vec<PathBuf>>,
		refuse: Option<(PathBuf, std::io::ErrorKind)>,
	}

	impl RecordingFs {
		fn refusing(dir: PathBuf, kind: std::io::ErrorKind) -> Self {
			Self { synced: std::sync::Mutex::default(), refuse: Some((dir, kind)) }
		}

		fn synced(&self) -> Vec<PathBuf> {
			self.synced.lock().expect("not poisoned").clone()
		}
	}

	#[async_trait::async_trait]
	impl StoreFs for RecordingFs {
		async fn create_new_write(&self, path: &Path, bytes: Vec<u8>, policy: crate::types::durable::SyncPolicy, points: crate::types::durable::WritePoints) -> std::io::Result<()> {
			RealFs.create_new_write(path, bytes, policy, points).await
		}

		async fn sync_file(&self, path: &Path) -> std::io::Result<()> {
			RealFs.sync_file(path).await
		}

		async fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
			self.synced.lock().expect("not poisoned").push(dir.to_path_buf());
			match &self.refuse {
				Some((refused, kind)) if refused == dir => Err(std::io::Error::from(*kind)),
				_ => RealFs.sync_dir(dir).await,
			}
		}

		async fn hard_link(&self, src: &Path, dst: &Path) -> std::io::Result<()> {
			RealFs.hard_link(src, dst).await
		}

		async fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
			RealFs.rename(from, to).await
		}

		async fn remove_file(&self, path: &Path) -> std::io::Result<()> {
			RealFs.remove_file(path).await
		}

		async fn create_dir(&self, path: &Path) -> std::io::Result<()> {
			RealFs.create_dir(path).await
		}

		async fn remove_dir_all(&self, path: &Path) -> std::io::Result<()> {
			RealFs.remove_dir_all(path).await
		}

		async fn copy_new(&self, src: &Path, dst: &Path, policy: crate::types::durable::SyncPolicy) -> std::io::Result<u64> {
			RealFs.copy_new(src, dst, policy).await
		}

		async fn read_dir(&self, dir: &Path) -> std::io::Result<Vec<crate::types::durable::FsEntry>> {
			RealFs.read_dir(dir).await
		}

		async fn metadata(&self, path: &Path) -> std::io::Result<crate::types::durable::FsMetadata> {
			RealFs.metadata(path).await
		}
	}

	/// Design section 5.5, OPEN step 4: the open itself fsyncs every directory that holds
	/// one of its entries, and does so before it registers its scope.
	#[tokio::test]
	async fn an_open_fsyncs_its_directories_before_it_registers_its_scope() {
		let dir = TempDir::new().expect("tempdir");
		let paths = |names: &[&str]| -> Vec<PathBuf> { names.iter().map(|n| dir.path().join(n)).collect() };
		let root = dir.path().join("a/b");

		// A fresh nested root: everything it created, and the directory that held the top.
		let fs = RecordingFs::default();
		drop(SegmentStore::open_scoped_on(&fs, &root, "market", "BTCUSD").await.expect("opens a fresh nested root"));
		let mut expected = paths(&["a/b/segments", "a/b", "a"]);
		expected.push(dir.path().to_path_buf());
		assert_eq!(fs.synced(), expected);

		// Reopening it still syncs the root's parent: the open that created the root may
		// have died before its own fsyncs, and this one cannot tell.
		let fs = RecordingFs::default();
		drop(SegmentStore::open_scoped_on(&fs, &root, "market", "BTCUSD").await.expect("reopens"));
		assert_eq!(fs.synced(), paths(&["a/b/segments", "a/b", "a"]));

		// A failed fsync fails the open before the scope is registered.
		let fresh = dir.path().join("c");
		let fs = RecordingFs::refusing(fresh.join("segments"), std::io::ErrorKind::Other);
		let err = SegmentStore::open_scoped_on(&fs, &fresh, "market", "BTCUSD").await.err().expect("a failed directory fsync fails the open");
		assert!(format!("{err:#}").contains(&format!("fsyncing directory {}", fresh.join("segments").display())), "{err:#}");
		assert_eq!(registered_scopes(&fresh).await, (Vec::new(), Vec::new()), "the scope is registered only after the layout is durable");
	}

	/// The root's parent is synced on every open, but a store whose root predates the open
	/// still opens when that parent cannot be opened for reading. A root the open created
	/// is another matter: its entry is the open's own to make durable.
	#[tokio::test]
	async fn an_unreadable_parent_is_skipped_only_for_a_root_the_open_did_not_create() {
		let dir = TempDir::new().expect("tempdir");
		let root = dir.path().join("store");
		let fs = RecordingFs::refusing(dir.path().to_path_buf(), std::io::ErrorKind::PermissionDenied);
		let err = SegmentStore::open_scoped_on(&fs, &root, "market", "BTCUSD").await.err().expect("a root this open created needs its parent synced");
		assert!(format!("{err:#}").contains(&format!("fsyncing directory {}", dir.path().display())), "{err:#}");

		let fs = RecordingFs::refusing(dir.path().to_path_buf(), std::io::ErrorKind::PermissionDenied);
		drop(SegmentStore::open_scoped_on(&fs, &root, "market", "BTCUSD").await.expect("the existing root opens"));
		assert_eq!(fs.synced(), vec![root.join("segments"), root.clone(), dir.path().to_path_buf()], "the parent was tried");

		let fs = RecordingFs::refusing(dir.path().to_path_buf(), std::io::ErrorKind::Other);
		assert!(SegmentStore::open_scoped_on(&fs, &root, "market", "BTCUSD").await.is_err(), "any other error syncing the parent still fails the open");
	}

	/// The database and subject rows a root's catalog holds, read without opening a
	/// store (which would register a scope of its own).
	async fn registered_scopes(root: &Path) -> (Vec<String>, Vec<String>) {
		let catalog = CatalogStore::open(&root.join("catalog.db").to_string_lossy()).await.expect("opens the catalog");
		let databases = catalog.list_databases().await.expect("lists databases");
		let subjects = catalog.list_subjects("market").await.expect("lists subjects");
		drop(catalog);
		(databases, subjects)
	}

	#[tokio::test]
	async fn a_second_open_of_a_root_is_refused_until_the_first_store_drops() {
		let dir = TempDir::new().expect("tempdir");
		let first = SegmentStore::open_scoped(dir.path(), "market", "BTCUSD").await.expect("opens");
		// The same scope and a different one are refused alike: the root is the unit.
		for (database, subject) in [("market", "BTCUSD"), ("iot", "sensor-7")] {
			let err = SegmentStore::open_scoped(dir.path(), database, subject).await.err().unwrap_or_else(|| panic!("{database}/{subject}: a second open of a held root must be refused"));
			let locked = err.downcast_ref::<StoreLocked>().unwrap_or_else(|| panic!("{database}/{subject}: expected StoreLocked, got {err:#}"));
			assert_eq!(locked.root, dir.path());
			let holder = locked.holder.as_ref().expect("the holder's record is readable");
			assert_eq!(holder.pid, std::process::id(), "the error names the holding process");
			assert!(err.to_string().contains(&format!("in use by pid {}", std::process::id())), "{err}");
		}
		drop(first);
		assert_eq!(registered_scopes(dir.path()).await.0, vec!["market".to_string()], "a refused open registers nothing");

		let second = SegmentStore::open_scoped(dir.path(), "iot", "sensor-7").await.expect("the root is free once the first store drops");
		drop(second);
	}

	/// Hold the database at `path` open in this process in a mode that cannot run MVCC:
	/// multiprocess WAL. Turso shares one instance per file within a process, so a
	/// store that opens the same file gets this instance and its switch to MVCC fails.
	///
	/// Unix only: Turso 0.8's Windows backend (`WindowsIO`) has no multiprocess WAL
	/// (`supports_shared_wal_coordination` is false), so the build fails there.
	#[cfg(unix)]
	async fn hold_without_mvcc(path: &Path) -> turso::Database {
		let db = turso::Builder::new_local(&path.to_string_lossy()).experimental_multiprocess_wal(true).build().await.expect("opens in multiprocess WAL mode");
		let conn = db.connect().expect("connects");
		conn.execute("CREATE TABLE IF NOT EXISTS placeholder (x INTEGER)", turso::params![]).await.expect("creates the file's schema");
		drop(conn);
		db
	}

	/// Every COMMIT in the control plane relies on MVCC: `BEGIN CONCURRENT`, and a log
	/// that is fsynced before COMMIT returns. Before S3 a failed switch to MVCC was
	/// discarded with `.ok()`, and the store opened and committed in WAL mode.
	///
	/// Unix only, as [`hold_without_mvcc`]: on Windows nothing in Turso 0.8 can pin a file
	/// out of MVCC. The decision itself is tested on every platform in
	/// `durable::control_plane`, and the open's synchronous probe by
	/// `an_open_fails_closed_when_a_new_connection_does_not_sync_full`.
	#[cfg(unix)]
	#[tokio::test]
	async fn an_open_fails_closed_when_a_database_cannot_run_mvcc() {
		for file in crate::CONTROL_PLANE_FILES {
			let dir = TempDir::new().expect("tempdir");
			let held = hold_without_mvcc(&dir.path().join(file)).await;
			let err = SegmentStore::open(dir.path()).await.err().unwrap_or_else(|| panic!("{file}: the store opened although {file} cannot run MVCC"));
			let message = format!("{err:#}");
			assert!(message.contains(file), "{file}: the error names the database: {message}");
			assert!(message.contains("MVCC"), "{file}: the error names the missing journal mode: {message}");
			drop(held);
			let store = SegmentStore::open(dir.path()).await.unwrap_or_else(|e| panic!("{file}: the root opens once nothing pins it out of MVCC: {e:#}"));
			drop(store);
		}
	}

	/// Each of the four control-plane databases refuses to open unless a new connection
	/// syncs FULL. Turso 0.8 always does, so the probe's test override plays one that
	/// does not, for one file at a time.
	#[tokio::test]
	async fn an_open_fails_closed_when_a_new_connection_does_not_sync_full() {
		use crate::types::durable::control_plane::NEW_CONNECTION_OVERRIDE;

		for file in crate::CONTROL_PLANE_FILES {
			let dir = TempDir::new().expect("tempdir");
			let err = NEW_CONNECTION_OVERRIDE.scope((file, "PRAGMA synchronous=NORMAL"), SegmentStore::open(dir.path())).await.err().unwrap_or_else(|| panic!("{file}: the store opened although a new connection to {file} syncs NORMAL"));
			let message = format!("{err:#}");
			assert!(message.contains(&format!("{} reports PRAGMA synchronous=1, not FULL", dir.path().join(file).display())), "{file}: {message}");
			let store = SegmentStore::open(dir.path()).await.unwrap_or_else(|e| panic!("{file}: the root opens once new connections are FULL: {e:#}"));
			drop(store);
		}
	}

	/// Set only in the child process `run_open_child` starts: the root to open.
	const OPEN_CHILD_ROOT: &str = "WEFT_TEST_OPEN_CHILD_ROOT";

	/// The body the re-executed child of
	/// `register_scope_is_atomic_across_a_crash_between_its_rows` runs: open
	/// `market/BTCUSD` at the root it is given, under the fault the parent put in
	/// `WEFT_FAULT`. In a normal test run the variable is unset and this does nothing.
	#[tokio::test]
	async fn open_scoped_child() {
		let Some(root) = std::env::var_os(OPEN_CHILD_ROOT) else { return };
		crate::types::durable::fault::suppress_core_dump();
		// Returns only when the point is armed with `err` rather than `abort`.
		let err = SegmentStore::open_scoped(PathBuf::from(root), "market", "BTCUSD").await.err().expect("the open stops at the armed fault");
		assert!(format!("{err:#}").contains("injected fault at O-scope-database-inserted"), "{err:#}");
	}

	/// Run [`open_scoped_child`] at `root` in a fresh process with `WEFT_FAULT=<spec>`.
	/// A child, because fault points are process-global and every concurrently running
	/// test's open passes this one.
	async fn run_open_child(root: &Path, spec: &str) -> std::process::Output {
		let exe = std::env::current_exe().expect("finds the test binary");
		tokio::process::Command::new(exe).args(["types::segment_store::tests::open_scoped_child", "--exact", "--nocapture", "--test-threads=1"]).env(OPEN_CHILD_ROOT, root).env(crate::types::durable::fault::FAULT_ENV, spec).output().await.expect("runs the child")
	}

	/// Window open-scoped-register-database-then-subject: an open registers its
	/// database and subject in one catalog transaction, so a crash between the two
	/// inserts leaves neither row. Before S3 they were two commits, and a crash between
	/// them left a database without the subject the open was for.
	#[tokio::test]
	async fn register_scope_is_atomic_across_a_crash_between_its_rows() {
		for spec in ["O-scope-database-inserted:err", "O-scope-database-inserted:abort"] {
			let dir = TempDir::new().expect("tempdir");
			let out = run_open_child(dir.path(), spec).await;
			let stderr = String::from_utf8_lossy(&out.stderr);
			if spec.ends_with(":abort") {
				assert!(!out.status.success(), "{spec}: the child aborted: {out:?}");
				#[cfg(unix)]
				{
					use std::os::unix::process::ExitStatusExt;
					assert_eq!(out.status.signal(), Some(6), "{spec}: killed by SIGABRT: {out:?}");
				}
				assert!(stderr.contains("aborting at fault point O-scope-database-inserted"), "{spec}: {stderr}");
			} else {
				assert!(out.status.success(), "{spec}: the child saw the injected error: {out:?}");
				assert!(String::from_utf8_lossy(&out.stdout).contains("1 passed"), "{spec}: the child really ran the open: {out:?}");
			}
			assert_eq!(registered_scopes(dir.path()).await, (Vec::new(), Vec::new()), "{spec}: a crash between the two inserts registers neither row");

			let store = SegmentStore::open_scoped(dir.path(), "market", "BTCUSD").await.expect("the next open succeeds");
			drop(store);
			assert_eq!(registered_scopes(dir.path()).await, (vec!["market".to_string()], vec!["BTCUSD".to_string()]), "{spec}: the next open registers the whole scope");
		}
	}

	#[tokio::test]
	async fn aspect_stats_report_realized_storage() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// Two sealed segments, 5 rows each, over [0,40] and [100,140].
		let mut expected_bytes = 0_u64;
		for base in [0_i64, 100] {
			let ts: Vec<i64> = (0..5).map(|i| base + i * 10).collect();
			let vs: Vec<BigDecimal> = (0..5).map(|i| BigDecimal::from(base + i)).collect();
			let d = store.seal("a", &schema(), &ts, &vs).await.expect("seals");
			expected_bytes += d.byte_len;
		}
		let stats = store.aspect_stats("a").await.expect("stats");
		// An empty aspect reports zeroes.
		let empty = store.aspect_stats("none").await.expect("stats");
		drop(store);
		assert_eq!(stats.segment_count, 2);
		assert_eq!(stats.total_rows, 10);
		assert_eq!(stats.total_bytes, expected_bytes);
		assert_eq!(stats.time_range, Some((0, 140)));
		// Both seals were in order, so the aspect is fully sorted.
		assert_eq!(stats.unsorted_segments, 0);
		#[allow(clippy::cast_precision_loss)]
		let expected_bpp = expected_bytes as f64 / 10.0;
		assert!((stats.bytes_per_point - expected_bpp).abs() < f64::EPSILON);
		// Empty aspect.
		assert_eq!(empty.segment_count, 0);
		assert_eq!(empty.total_rows, 0);
		assert_eq!(empty.time_range, None);
		assert_eq!(empty.unsorted_segments, 0);
		assert!((empty.bytes_per_point - 0.0).abs() < f64::EPSILON);
	}

	#[tokio::test]
	async fn aspect_stats_counts_out_of_order_segments() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// One ordered seal, then one whose timestamps step backwards.
		store.seal("a", &schema(), &[0_i64, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.expect("ordered");
		assert_eq!(store.aspect_stats("a").await.expect("stats").unsorted_segments, 0);
		store.seal("a", &schema(), &[100_i64, 130, 110], &[bd("4"), bd("5"), bd("6")]).await.expect("out of order");
		let stats = store.aspect_stats("a").await.expect("stats");
		drop(store);
		assert_eq!(stats.segment_count, 2);
		assert_eq!(stats.unsorted_segments, 1, "one of the two segments is out of order");
		assert_eq!(stats.overlapping_segments, 0, "the two segments cover disjoint windows");
	}

	#[tokio::test]
	async fn aspect_stats_counts_cross_segment_overlap() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// Two internally-sorted segments whose time windows overlap: [0,20] and [10,30].
		store.seal("a", &schema(), &[0_i64, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.expect("first");
		store.seal("a", &schema(), &[10_i64, 20, 30], &[bd("4"), bd("5"), bd("6")]).await.expect("overlapping");
		let stats = store.aspect_stats("a").await.expect("stats");
		drop(store);
		assert_eq!(stats.unsorted_segments, 0, "both segments are internally sorted");
		assert_eq!(stats.overlapping_segments, 2, "their time windows overlap (late data re-entered a window)");
	}

	#[tokio::test]
	async fn aspect_metadata_matches_aspect_stats() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// A single-block and a paged segment over disjoint windows.
		store.seal("a", &schema(), &[0_i64, 10, 20, 30, 40], &(0..5).map(BigDecimal::from).collect::<Vec<_>>()).await.expect("seals single");
		let pts: Vec<i64> = (0..6).map(|i| 100 + i * 10).collect();
		let pvs: Vec<BigDecimal> = (0..6).map(|i| BigDecimal::from(100 + i)).collect();
		store.seal_paged("a", &schema(), &pts, &pvs, 2).await.expect("seals paged");
		let stats = store.aspect_stats("a").await.expect("stats");
		let meta = store.aspect_metadata("a").await.expect("metadata");
		// An aspect with no segments yields the empty rollup.
		let empty = store.aspect_metadata("none").await.expect("metadata");
		drop(store);
		// The materialized rollup agrees with the scan-derived stats on every shared field.
		assert_eq!(meta.segment_count, stats.segment_count);
		assert_eq!(meta.total_rows, stats.total_rows);
		assert_eq!(meta.total_bytes, stats.total_bytes);
		assert_eq!(meta.time_range, stats.time_range);
		assert!((meta.bytes_per_point() - stats.bytes_per_point).abs() < f64::EPSILON);
		// And it additionally carries the aspect-wide value span.
		assert_eq!(meta.total_rows, 11);
		assert_eq!(meta.value_range, Some((bd("0"), bd("105"))));
		assert_eq!(empty, AspectMetadata::default());
	}

	#[tokio::test]
	async fn store_stats_aggregate_every_aspect() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// Aspect "temp": 2 rows over [0,10]; aspect "humidity": 3 rows over [100,120].
		store.seal("temp", &schema(), &[0_i64, 10], &[bd("1"), bd("2")]).await.expect("seals");
		store.seal("humidity", &schema(), &[100_i64, 110, 120], &[bd("5"), bd("6"), bd("7")]).await.expect("seals");
		let stats = store.store_stats().await.expect("store stats");
		// The per-aspect rollups it sums.
		let temp = store.aspect_metadata("temp").await.expect("metadata");
		let humidity = store.aspect_metadata("humidity").await.expect("metadata");
		// An empty store aggregates to zeroes.
		let empty_dir = TempDir::new().expect("tempdir");
		let empty_store = SegmentStore::open(empty_dir.path()).await.expect("opens");
		let empty = empty_store.store_stats().await.expect("store stats");
		drop(store);
		drop(empty_store);
		assert_eq!(stats.aspect_count, 2);
		assert_eq!(stats.segment_count, 2);
		assert_eq!(stats.total_rows, 5);
		assert_eq!(stats.total_bytes, temp.total_bytes + humidity.total_bytes);
		// The time span is the union across aspects.
		assert_eq!(stats.time_range, Some((0, 120)));
		#[allow(clippy::cast_precision_loss)]
		let expected_bpp = stats.total_bytes as f64 / 5.0;
		assert!((stats.bytes_per_point() - expected_bpp).abs() < f64::EPSILON);
		// Both aspects sealed in order, so the store-wide order-health count is clean,
		// and it equals the sum of the per-aspect rollups.
		assert_eq!(stats.unsorted_segments, 0);
		assert_eq!(stats.unsorted_segments, temp.unsorted_segments + humidity.unsorted_segments);
		// Empty store.
		assert_eq!(empty, StoreStorageStats::default());
		assert_eq!(empty.unsorted_segments, 0);
		assert!((empty.bytes_per_point() - 0.0).abs() < f64::EPSILON);
	}

	#[tokio::test]
	async fn store_stats_sums_out_of_order_segments_across_aspects() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// "a": one ordered + one out-of-order; "b": one out-of-order. Store-wide total = 2.
		store.seal("a", &schema(), &[0_i64, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.expect("ordered");
		store.seal("a", &schema(), &[100_i64, 130, 110], &[bd("4"), bd("5"), bd("6")]).await.expect("ooo");
		store.seal("b", &schema(), &[0_i64, 40, 20], &[bd("7"), bd("8"), bd("9")]).await.expect("ooo");
		let stats = store.store_stats().await.expect("store stats");
		drop(store);
		assert_eq!(stats.aspect_count, 2);
		assert_eq!(stats.segment_count, 3);
		assert_eq!(stats.unsorted_segments, 2);
	}

	#[tokio::test]
	async fn store_overlapping_segments_sums_across_aspects() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// "a": two internally-sorted but time-overlapping segments ([0,20], [10,30]) → 2.
		store.seal("a", &schema(), &[0_i64, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.expect("a first");
		store.seal("a", &schema(), &[10_i64, 20, 30], &[bd("4"), bd("5"), bd("6")]).await.expect("a overlap");
		// "b": two disjoint windows ([0,20], [100,120]) → 0.
		store.seal("b", &schema(), &[0_i64, 10, 20], &[bd("7"), bd("8"), bd("9")]).await.expect("b first");
		store.seal("b", &schema(), &[100_i64, 110, 120], &[bd("1"), bd("2"), bd("3")]).await.expect("b disjoint");
		let total = store.store_overlapping_segments().await.expect("overlap total");
		// store_stats stays O(1) and is unaffected.
		let stats = store.store_stats().await.expect("store stats");
		drop(store);
		assert_eq!(total, 2, "only aspect a's two segments overlap; b's are disjoint");
		assert_eq!(stats.unsorted_segments, 0, "every segment is internally sorted");
	}

	#[tokio::test]
	async fn rebuild_reconciles_a_diverged_rollup() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// Seal three segments, then corrupt the rollup directly behind the store's back.
		for base in [0_i64, 100, 200] {
			let ts: Vec<i64> = (0..5).map(|i| base + i * 10).collect();
			let vs: Vec<BigDecimal> = (0..5).map(|i| BigDecimal::from(base + i)).collect();
			store.seal("a", &schema(), &ts, &vs).await.expect("seals");
		}
		// Force a wrong rollup, then remove another aspect's row entirely.
		store.metadata().put("a", &AspectMetadata { segment_count: 99, total_rows: 1, total_nulls: 7, total_bytes: 3, unsorted_segments: 42, time_range: Some((-5, -1)), value_range: Some((bd("-9"), bd("-8"))) }).await.expect("clobbers");
		let diverged = store.aspect_metadata("a").await.expect("metadata");
		// Rebuilding from the durable index restores the truth.
		let reconciled = store.rebuild_aspect_metadata("a").await.expect("rebuilds");
		let stats = store.aspect_stats("a").await.expect("stats");
		drop(store);
		assert_eq!(diverged.segment_count, 99, "the rollup was clobbered");
		assert_eq!(reconciled.segment_count, 3);
		assert_eq!(reconciled.total_rows, stats.total_rows);
		assert_eq!(reconciled.total_bytes, stats.total_bytes);
		assert_eq!(reconciled.time_range, Some((0, 240)));
		assert_eq!(reconciled.value_range, Some((bd("0"), bd("204"))));
		// The clobbered order-health count (42) is corrected back to the truth (0 —
		// all three seals were in order), matching the index-derived stats.
		assert_eq!(diverged.unsorted_segments, 42, "the clobber wrote a wrong order count");
		assert_eq!(reconciled.unsorted_segments, stats.unsorted_segments);
		assert_eq!(reconciled.unsorted_segments, 0);
	}

	#[tokio::test]
	async fn rebuild_all_recovers_every_aspect_from_the_index() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.seal("temp", &schema(), &[0_i64, 10], &[bd("1"), bd("2")]).await.expect("seals");
		store.seal("humidity", &schema(), &[0_i64, 10, 20], &[bd("5"), bd("6"), bd("7")]).await.expect("seals");
		// Wipe the whole rollup DB, as if metadata.db were lost beside an intact index.
		store.metadata().remove("temp").await.expect("removes");
		store.metadata().remove("humidity").await.expect("removes");
		let before = store.metadata().list_aspects().await.expect("lists");
		let rebuilt = store.rebuild_all_metadata().await.expect("rebuilds");
		let after = store.metadata().list_aspects().await.expect("lists");
		let temp = store.aspect_metadata("temp").await.expect("metadata");
		let humidity = store.aspect_metadata("humidity").await.expect("metadata");
		drop(store);
		assert!(before.is_empty(), "the rollup DB was wiped");
		assert_eq!(rebuilt, 2, "both aspects reconciled from the index");
		assert_eq!(after, vec!["humidity".to_string(), "temp".to_string()]);
		assert_eq!(temp.total_rows, 2);
		assert_eq!(humidity.total_rows, 3);
		assert_eq!(humidity.value_range, Some((bd("5"), bd("7"))));
	}

	#[tokio::test]
	async fn aspect_metadata_survives_reopen() {
		let dir = TempDir::new().expect("tempdir");
		let first = SegmentStore::open(dir.path()).await.expect("opens");
		first.seal("a", &schema(), &[0_i64, 10], &[bd("1"), bd("2")]).await.expect("seals");
		let recorded = first.aspect_metadata("a").await.expect("metadata");
		drop(first);
		// A reopened store reads the persisted rollup, then folds the next seal forward.
		let reopened = SegmentStore::open(dir.path()).await.expect("reopens");
		let before = reopened.aspect_metadata("a").await.expect("metadata");
		reopened.seal("a", &schema(), &[20_i64, 30], &[bd("3"), bd("4")]).await.expect("seals");
		let after = reopened.aspect_metadata("a").await.expect("metadata");
		drop(reopened);
		assert_eq!(before, recorded, "the rollup persists across reopen");
		assert_eq!(after.segment_count, 2);
		assert_eq!(after.total_rows, 4);
		assert_eq!(after.time_range, Some((0, 30)));
		assert_eq!(after.value_range, Some((bd("1"), bd("4"))));
	}

	/// The policy decides on shape + size, and `DISABLED` is the default — the guarantee
	/// that an unconfigured deployment's bytes are byte-for-byte what they always were.
	#[test]
	fn checkpoint_policy_gates_on_shape_size_and_codec_overhead() {
		assert_eq!(CheckpointPolicy::DISABLED.stride_for(1_000_000, true, 1.0), None, "disabled never checkpoints, whatever the shape");
		let p = CheckpointPolicy { stride: Some(1024), min_rows: 8_192, max_codec_overhead: 1.25 };
		assert_eq!(p.stride_for(10_000, true, 1.01), Some(1024), "a large sorted-irregular segment whose override is ~free is checkpointed");
		assert_eq!(p.stride_for(10_000, false, 1.0), None, "a regular (or out-of-order) segment gains nothing — O(1) closed form already");
		assert_eq!(p.stride_for(100, true, 1.0), None, "a small segment decodes trivially; the index would be pure cost");
		// The gate the measured Gorilla/RLE blow-ups demand: irregular is not enough.
		assert_eq!(p.stride_for(10_000, true, 3.5), None, "a Gorilla-shaped column (3.5x override) is refused despite being irregular");
		assert_eq!(p.stride_for(10_000, true, 14.0), None, "an RLE-shaped column (14x override) is refused");
		assert_eq!(p.stride_for(10_000, true, 1.25), Some(1024), "the ceiling is inclusive");
	}

	/// End-to-end through the real store: an enabled policy changes the bytes on disk and
	/// nothing else — every read still returns exactly the same data.
	#[tokio::test]
	async fn checkpointed_seal_reads_identically_and_only_changes_the_bytes() {
		// A sorted IRREGULAR column above the row floor — the shape the index is for.
		let mut t = 0_i64;
		let ts: Vec<i64> = (0..12_000)
			.map(|i: i64| {
				t += 1 + (i * 7) % 29;
				t
			})
			.collect();
		let vs: Vec<BigDecimal> = (0..12_000).map(|i| bd(&format!("{}", 100 + i % 400))).collect();

		let policy = CheckpointPolicy { stride: Some(1024), min_rows: 8_192, max_codec_overhead: 1.25 };
		let plain_dir = TempDir::new().expect("tempdir");
		let plain = SegmentStore::open(plain_dir.path()).await.expect("opens");
		assert_eq!(plain.checkpoint_policy(), CheckpointPolicy::DISABLED, "the store is unconfigured by default");
		let plain_desc = plain.seal("temp", &schema(), &ts, &vs).await.expect("seals");

		let cp_dir = TempDir::new().expect("tempdir");
		let cp = SegmentStore::open(cp_dir.path()).await.expect("opens").with_checkpoint_policy(policy);
		let cp_desc = cp.seal("temp", &schema(), &ts, &vs).await.expect("seals");

		// The index costs real bytes — the whole trade, asserted rather than assumed.
		assert!(cp_desc.byte_len > plain_desc.byte_len, "the checkpointed frame is larger ({} vs {})", cp_desc.byte_len, plain_desc.byte_len);

		// ...and buys identical answers: point reads (present + absent) and a range read.
		for probe in [ts[0], ts[5_000], ts[11_999], ts[3] + 1, -1] {
			assert_eq!(cp.read_point("temp", probe).await.expect("reads"), plain.read_point("temp", probe).await.expect("reads"), "point read at {probe} must match the plain store");
		}
		let batch = vec![ts[10], ts[8_000], 999_999_999, ts[11_000]];
		assert_eq!(cp.read_points("temp", &batch).await.expect("reads"), plain.read_points("temp", &batch).await.expect("reads"), "batch read must match");
		assert_eq!(cp.read_time_range("temp", ts[100], ts[200]).await.expect("reads"), plain.read_time_range("temp", ts[100], ts[200]).await.expect("reads"), "range read must match");
	}

	/// End-to-end through the real store: an enabled [`TransposedPolicy`] seals the value
	/// column in the transposed (decode-fast) codec, an unconfigured store does not, and both
	/// stores answer every read identically. The store is the layer that makes the codec
	/// reachable from a deployment, so this is the test that it is actually wired up.
	#[tokio::test]
	#[cfg(feature = "bitsliced-codec")]
	async fn transposed_seal_reads_identically_and_only_changes_the_bytes() {
		use weft_physical_type::frame_value_codec;

		// A zero-straddling small-magnitude two-decimal column spanning several 1024-lane
		// tiles: the bit-pack family wins the size race, so the transposed layout is admitted
		// near parity. Scale 2 keeps it off the F64 fast path.
		let ts: Vec<i64> = (0..2_500).map(|i| 100 + i * 10).collect();
		let vs: Vec<BigDecimal> = (0..2_500_i64).map(|i| BigDecimal::new((((i * 37) % 1_001) - 500).into(), 2)).collect();
		let scaled = AspectSchema::new(PhysicalType::ScaledI64 { scale: 2 }, bd("0"), TimeUnit::Seconds);

		let plain_dir = TempDir::new().expect("tempdir");
		let plain = SegmentStore::open(plain_dir.path()).await.expect("opens");
		assert_eq!(plain.transposed_policy(), TransposedPolicy::DISABLED, "the store is unconfigured by default");
		let plain_desc = plain.seal("temp", &scaled, &ts, &vs).await.expect("seals");

		let tr_dir = TempDir::new().expect("tempdir");
		let tr = SegmentStore::open(tr_dir.path()).await.expect("opens").with_transposed_policy(TransposedPolicy { max_overhead: Some(1.05) });
		let tr_desc = tr.seal("temp", &scaled, &ts, &vs).await.expect("seals");

		// The frames really do differ in the codec they realized.
		let plain_bytes = tokio::fs::read(&plain_desc.path).await.expect("reads plain frame");
		let tr_bytes = tokio::fs::read(&tr_desc.path).await.expect("reads transposed frame");
		assert_eq!(frame_value_codec(&tr_bytes).expect("codec"), "scaled_transposed", "the configured store sealed the transposed codec");
		assert_ne!(frame_value_codec(&plain_bytes).expect("codec"), "scaled_transposed", "the unconfigured store did not");

		// ...and answer identically: point reads (present, off-grid, out-of-range), a batch,
		// and a range spanning a tile boundary.
		for probe in [ts[0], ts[1_024], ts[2_499], ts[3] + 1, -1] {
			assert_eq!(tr.read_point("temp", probe).await.expect("reads"), plain.read_point("temp", probe).await.expect("reads"), "point read at {probe} must match the plain store");
		}
		let batch = vec![ts[10], ts[1_023], 999_999_999, ts[2_048]];
		assert_eq!(tr.read_points("temp", &batch).await.expect("reads"), plain.read_points("temp", &batch).await.expect("reads"), "batch read must match");
		assert_eq!(tr.read_time_range("temp", ts[1_020], ts[1_030]).await.expect("reads"), plain.read_time_range("temp", ts[1_020], ts[1_030]).await.expect("reads"), "range read across a tile boundary must match");
	}

	/// End-to-end through the real store: an enabled partial-sidecar policy writes a
	/// `.weftpart` beside each sealed `.weftseg`, the sidecar matches the exact segment bytes,
	/// and its stored partial finishes to the same buckets a fresh reduction of the
	/// segment's rows would — the invariant the cross-segment downsample will lean on. An
	/// unconfigured store writes no sidecar.
	#[tokio::test]
	async fn seal_writes_a_matching_partial_sidecar_when_configured() {
		use splimes::{Point, Resolution};
		use weft_reduce::reduce_partial;

		// The shared schema is SECONDS; one sample a minute so the minute-base sidecar has
		// one bucket per sample and an hour of data spans a handful of base buckets.
		let ts: Vec<i64> = (0..180).map(|i| i * 60).collect();
		let vs: Vec<BigDecimal> = (0..180).map(|i| bd(&format!("{}", (i * 13) % 71))).collect();
		let points: Vec<Point> = ts.iter().zip(&vs).map(|(t, v)| Point::new(DateTime::<Utc>::from_timestamp(*t, 0).expect("instant"), v.clone())).collect();

		// A configured store: base = Minutes, floor = 1 so this small fixture qualifies.
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens").with_partial_sidecar_policy(PartialSidecarPolicy::at(Resolution::Minutes, 1));
		store.declare("temp", &schema()).await.expect("declares");
		let descriptor = store.seal("temp", &schema(), &ts, &vs).await.expect("seals");

		let sidecar = store.load_partial_sidecar("temp", &descriptor).await.expect("reads sidecar").expect("a sidecar was written");
		assert_eq!(sidecar.base, Resolution::Minutes);
		assert!(sidecar.matches(&descriptor), "the sidecar's stamp matches the sealed segment");

		// The stored partial finishes to exactly what reducing the segment's rows directly
		// at the base resolution produces — so a downsample can merge it in place of a decode.
		let expected = reduce_partial(&points, Resolution::Minutes, None, None, &SIDECAR_AGGREGATIONS).expect("partial").finish(Resolution::Minutes, &SIDECAR_AGGREGATIONS).expect("finishes");
		let got = sidecar.partial.finish(Resolution::Minutes, &SIDECAR_AGGREGATIONS).expect("finishes");
		assert_eq!(got, expected, "the stored partial equals a fresh reduction of the segment");

		// A stale descriptor (a rewrite would change byte_len) is rejected → falls back.
		let mut stale = descriptor.clone();
		stale.byte_len += 1;
		assert!(store.load_partial_sidecar("temp", &stale).await.expect("reads").is_none(), "a mismatched stamp is treated as no sidecar");

		// An unconfigured store writes nothing beside the segment.
		let plain_dir = TempDir::new().expect("tempdir");
		let plain = SegmentStore::open(plain_dir.path()).await.expect("opens");
		assert_eq!(plain.partial_sidecar_policy(), PartialSidecarPolicy::DISABLED, "off by default");
		let plain_desc = plain.seal("temp", &schema(), &ts, &vs).await.expect("seals");
		assert!(plain.load_partial_sidecar("temp", &plain_desc).await.expect("reads").is_none(), "no sidecar without a policy");
	}

	/// The cross-segment downsample must equal reading the whole range and reducing it in
	/// one pass — for every reduction, across many segments, including the sketch.
	#[tokio::test]
	async fn downsample_range_equals_a_single_pass_over_the_whole_range() {
		use splimes::{Point, Resolution};
		use weft_reduce::{reduce, Aggregation};

		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// `downsample_range` reads the declared unit from the catalog to lift epochs.
		store.declare("temp", &schema()).await.expect("declares");
		// Eight separate seals => eight segments the reduction must span, with buckets
		// straddling segment boundaries (each seal covers 90 minutes at hour resolution).
		let mut all: Vec<Point> = Vec::new();
		for seg in 0..8_i64 {
			// The shared test `schema()` declares SECONDS, so the epochs are seconds: one
			// sample a minute, 90 minutes per seal, straddling hour buckets.
			let ts: Vec<i64> = (0..90).map(|i| (seg * 90 + i) * 60).collect();
			let vs: Vec<BigDecimal> = (0..90).map(|i| bd(&format!("{}", (seg * 7 + i) % 53))).collect();
			for (t, v) in ts.iter().zip(&vs) {
				all.push(Point::new(DateTime::<Utc>::from_timestamp(*t, 0).expect("instant"), v.clone()));
			}
			store.seal("temp", &schema(), &ts, &vs).await.expect("seals");
		}
		let aggs = [Aggregation::Min, Aggregation::Max, Aggregation::Avg, Aggregation::Sum, Aggregation::First, Aggregation::Last, Aggregation::P99, Aggregation::Twa, Aggregation::SketchP99];

		let cross = store.downsample_range("temp", i64::MIN, i64::MAX, Resolution::Hours, &aggs).await.expect("downsamples");
		let single = reduce(&all, Resolution::Hours, None, None, &aggs).expect("reduces");
		assert!(cross.len() > 1, "the fixture must span several buckets, got {}", cross.len());
		assert_eq!(cross, single, "a cross-segment downsample must equal the single pass exactly");

		// A window narrows the result and still matches a single pass over the same window.
		let (w_start, w_end) = (60_i64 * 100, 60_i64 * 300);
		let windowed = store.downsample_range("temp", w_start, w_end, Resolution::Hours, &aggs).await.expect("downsamples");
		let expected: Vec<Point> = all.iter().filter(|p| (w_start..=w_end).contains(&p.timestamp.timestamp())).cloned().collect();
		assert_eq!(windowed, reduce(&expected, Resolution::Hours, None, None, &aggs).expect("reduces"), "a windowed cross-segment downsample must match");

		// An empty window and an undeclared aspect behave sanely.
		assert!(store.downsample_range("temp", -10_000, -5_000, Resolution::Hours, &aggs).await.expect("downsamples").is_empty(), "a window with no rows yields no buckets");
		assert!(store.downsample_range("nope", 0, 1, Resolution::Hours, &aggs).await.is_err(), "an undeclared aspect is an error");

		// A window pruning to exactly ONE segment takes the inline fast path (which skips
		// the `spawn_blocking` hand-off that buys nothing without a second segment to
		// overlap) — it must equal the single pass just as the concurrent branch does.
		let (s_start, s_end) = (0_i64, 89 * 60);
		let one = store.downsample_range("temp", s_start, s_end, Resolution::Hours, &aggs).await.expect("downsamples");
		let in_one: Vec<Point> = all.iter().filter(|p| (s_start..=s_end).contains(&p.timestamp.timestamp())).cloned().collect();
		assert_eq!(one, reduce(&in_one, Resolution::Hours, None, None, &aggs).expect("reduces"), "the single-segment fast path must equal the single pass");
	}

	/// The sidecar **consumption** path: with a partial sidecar written per segment at the
	/// query's resolution, a full-history downsample of materializable reductions merges the
	/// stored partials instead of decoding — proven by **deleting every `.weftseg`** and
	/// showing the answer is unchanged (the value column was never read). Fallbacks stay
	/// correct: a non-materializable reduction (exact `p99`), a resolution other than the
	/// sidecar base, and a window that cuts inside a segment all decode, so they break once
	/// the frames are gone.
	#[tokio::test]
	async fn downsample_range_serves_materializable_queries_from_sidecars() {
		use splimes::{Point, Resolution};
		use weft_reduce::{reduce, Aggregation};

		let dir = TempDir::new().expect("tempdir");
		// Sidecars at HOUR base (the query resolution), floor 1 so every seal qualifies.
		let store = SegmentStore::open(dir.path()).await.expect("opens").with_partial_sidecar_policy(PartialSidecarPolicy::at(Resolution::Hours, 1));
		store.declare("temp", &schema()).await.expect("declares");
		let mut all: Vec<Point> = Vec::new();
		let mut seg_paths: Vec<String> = Vec::new();
		for seg in 0..8_i64 {
			let ts: Vec<i64> = (0..90).map(|i| (seg * 90 + i) * 60).collect();
			let vs: Vec<BigDecimal> = (0..90).map(|i| bd(&format!("{}", (seg * 7 + i) % 53))).collect();
			for (t, v) in ts.iter().zip(&vs) {
				all.push(Point::new(DateTime::<Utc>::from_timestamp(*t, 0).expect("instant"), v.clone()));
			}
			seg_paths.push(store.seal("temp", &schema(), &ts, &vs).await.expect("seals").path);
		}
		// Only materializable reductions — so the sidecars can serve the whole query.
		let aggs = [Aggregation::Min, Aggregation::Max, Aggregation::Avg, Aggregation::Sum, Aggregation::First, Aggregation::Last, Aggregation::SketchP99];
		let expected = reduce(&all, Resolution::Hours, None, None, &aggs).expect("reduces");
		// Baseline while the frames still exist (multi + single-segment sidecar paths).
		assert_eq!(store.downsample_range("temp", i64::MIN, i64::MAX, Resolution::Hours, &aggs).await.expect("downsamples"), expected, "the sidecar-served answer matches the single pass while frames exist");

		// Delete every `.weftseg`: the index (libSQL) still prunes, and a sidecar-served
		// downsample never opens a frame, so the answer must be unchanged.
		for path in &seg_paths {
			std::fs::remove_file(path).expect("removes the .weftseg frame");
		}
		let served = store.downsample_range("temp", i64::MIN, i64::MAX, Resolution::Hours, &aggs).await.expect("downsamples from sidecars alone");
		assert_eq!(served, expected, "with every value-column frame deleted, the sidecars alone reproduce the whole downsample");

		// The single-segment sidecar branch, frames still gone: a window covering exactly one
		// segment is served by that segment's sidecar.
		let one = store.downsample_range("temp", 0, 89 * 60, Resolution::Hours, &aggs).await.expect("single-segment sidecar");
		let in_one: Vec<Point> = all.iter().filter(|p| (0..=89 * 60).contains(&p.timestamp.timestamp())).cloned().collect();
		assert_eq!(one, reduce(&in_one, Resolution::Hours, None, None, &aggs).expect("reduces"), "the single-segment sidecar path matches");

		// Fallbacks that must DECODE — now that the frames are gone they error, proving they
		// do not (and must not) take the sidecar path:
		// (a) an exact percentile is not materializable;
		assert!(store.downsample_range("temp", i64::MIN, i64::MAX, Resolution::Hours, &[Aggregation::P99]).await.is_err(), "an exact p99 must decode, so it fails without frames");
		// (b) a resolution other than the sidecar base cannot be served yet (no re-bucketing);
		assert!(store.downsample_range("temp", i64::MIN, i64::MAX, Resolution::Minutes, &aggs).await.is_err(), "a non-base resolution must decode, so it fails without frames");
		// (c) a window cutting inside a segment cannot use the whole-segment partial.
		assert!(store.downsample_range("temp", 30 * 60, 200 * 60, Resolution::Hours, &aggs).await.is_err(), "a segment-splitting window must decode, so it fails without frames");
	}

	/// The **re-bucketing** path: a sidecar materialized at a fine base (MINUTES) answers a
	/// coarser downsample (HOURS) by re-keying its base buckets — again proven by deleting
	/// every `.weftseg` and showing the coarse answer is unchanged. A resolution FINER than
	/// the base (seconds) cannot be served and must decode.
	#[tokio::test]
	async fn downsample_range_rebuckets_a_fine_base_to_a_coarser_resolution() {
		use splimes::{Point, Resolution};
		use weft_reduce::{reduce, Aggregation};

		let dir = TempDir::new().expect("tempdir");
		// Sidecars at MINUTE base — finer than the HOUR query, so the roll-up re-keys.
		let store = SegmentStore::open(dir.path()).await.expect("opens").with_partial_sidecar_policy(PartialSidecarPolicy::at(Resolution::Minutes, 1));
		store.declare("temp", &schema()).await.expect("declares");
		let mut all: Vec<Point> = Vec::new();
		let mut seg_paths: Vec<String> = Vec::new();
		for seg in 0..6_i64 {
			let ts: Vec<i64> = (0..120).map(|i| (seg * 120 + i) * 60).collect();
			let vs: Vec<BigDecimal> = (0..120).map(|i| bd(&format!("{}", (seg * 5 + i) % 47))).collect();
			for (t, v) in ts.iter().zip(&vs) {
				all.push(Point::new(DateTime::<Utc>::from_timestamp(*t, 0).expect("instant"), v.clone()));
			}
			seg_paths.push(store.seal("temp", &schema(), &ts, &vs).await.expect("seals").path);
		}
		let aggs = [Aggregation::Min, Aggregation::Max, Aggregation::Avg, Aggregation::Sum, Aggregation::First, Aggregation::Last, Aggregation::SketchP99];

		// Delete every frame: an HOUR downsample must now come purely from re-keyed MINUTE
		// sidecars, and still equal a direct HOUR reduction of the data.
		for path in &seg_paths {
			std::fs::remove_file(path).expect("removes frame");
		}
		let hourly = store.downsample_range("temp", i64::MIN, i64::MAX, Resolution::Hours, &aggs).await.expect("rebuckets from minute sidecars");
		assert_eq!(hourly, reduce(&all, Resolution::Hours, None, None, &aggs).expect("reduces"), "a minute-base sidecar re-keys to hours without touching a frame");

		// The exact base (MINUTES) is served directly; a FINER resolution (seconds) cannot be
		// re-keyed from a minute base, so with frames gone it must fail.
		assert_eq!(store.downsample_range("temp", i64::MIN, i64::MAX, Resolution::Minutes, &aggs).await.expect("serves base"), reduce(&all, Resolution::Minutes, None, None, &aggs).expect("reduces"), "the exact base resolution is served directly");
		assert!(store.downsample_range("temp", i64::MIN, i64::MAX, Resolution::Seconds, &aggs).await.is_err(), "a resolution finer than the base must decode, so it fails without frames");
	}

	/// The single-segment **fast path** in isolation: an aspect with exactly one sealed
	/// segment never reaches the concurrent branch, so its correctness is proven here
	/// rather than inferred from the multi-segment equality test above.
	#[tokio::test]
	async fn downsample_range_single_segment_equals_a_single_pass() {
		use splimes::{Point, Resolution};
		use weft_reduce::{reduce, Aggregation};

		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("temp", &schema()).await.expect("declares");
		// One seal => one segment => the inline branch.
		let ts: Vec<i64> = (0..180).map(|i| i * 60).collect();
		let vs: Vec<BigDecimal> = (0..180).map(|i| bd(&format!("{}", (i * 13) % 71))).collect();
		store.seal("temp", &schema(), &ts, &vs).await.expect("seals");
		let all: Vec<Point> = ts.iter().zip(&vs).map(|(t, v)| Point::new(DateTime::<Utc>::from_timestamp(*t, 0).expect("instant"), v.clone())).collect();

		let aggs = [Aggregation::Min, Aggregation::Max, Aggregation::Sum, Aggregation::First, Aggregation::Last, Aggregation::P99, Aggregation::SketchP99];
		let cross = store.downsample_range("temp", i64::MIN, i64::MAX, Resolution::Hours, &aggs).await.expect("downsamples");
		// The significant-`Drop` store is released before the assertions so it does not
		// hold the control-plane connection open to the end of the scope.
		drop(store);
		assert!(cross.len() > 1, "the fixture must span several buckets, got {}", cross.len());
		assert_eq!(cross, reduce(&all, Resolution::Hours, None, None, &aggs).expect("reduces"), "the single-segment fast path must equal the single pass exactly");
	}

	/// The codec-override gate, end-to-end on a real store: an **irregular** column whose
	/// timestamps favour RLE must NOT be checkpointed, because forcing the range-decodable
	/// per-block codec would inflate its timestamp block ~14x for a lookup win not worth
	/// those bytes. Shape alone would have accepted it.
	#[tokio::test]
	async fn an_irregular_but_rle_shaped_column_is_refused_by_the_codec_gate() {
		// Long constant runs then a jump: irregular (so the shape gate passes), but RLE is
		// dramatically the best timestamp codec.
		let ts: Vec<i64> = (0..12_000_i64).map(|i| 1_000_000 + (i / 100) * 5_000).collect();
		let vs: Vec<BigDecimal> = (0..12_000).map(|i| bd(&format!("{}", i % 300))).collect();
		let policy = CheckpointPolicy { stride: Some(1024), min_rows: 8_192, max_codec_overhead: 1.25 };

		let plain_dir = TempDir::new().expect("tempdir");
		let plain = SegmentStore::open(plain_dir.path()).await.expect("opens");
		let plain_desc = plain.seal("temp", &schema(), &ts, &vs).await.expect("seals");

		let cp_dir = TempDir::new().expect("tempdir");
		let cp = SegmentStore::open(cp_dir.path()).await.expect("opens").with_checkpoint_policy(policy);
		let cp_desc = cp.seal("temp", &schema(), &ts, &vs).await.expect("seals");

		assert_eq!(cp_desc.byte_len, plain_desc.byte_len, "an RLE-shaped column must be byte-for-byte identical — the codec gate refused to checkpoint it");
		// Raising the ceiling lets it through, proving the gate (not the shape) is what refused.
		let loose_dir = TempDir::new().expect("tempdir");
		let loose = SegmentStore::open(loose_dir.path()).await.expect("opens").with_checkpoint_policy(CheckpointPolicy { max_codec_overhead: 100.0, ..policy });
		let loose_desc = loose.seal("temp", &schema(), &ts, &vs).await.expect("seals");
		assert!(loose_desc.byte_len > plain_desc.byte_len, "with the ceiling lifted the same column IS checkpointed ({} vs {}) — and pays for it", loose_desc.byte_len, plain_desc.byte_len);
	}

	/// A *regular* column must not be checkpointed even when the policy is on: it already
	/// resolves O(1) closed-form, so an index would be bytes for nothing.
	#[tokio::test]
	async fn a_regular_column_is_never_checkpointed_even_when_enabled() {
		let ts: Vec<i64> = (0..12_000).map(|i| i * 10).collect();
		let vs: Vec<BigDecimal> = (0..12_000).map(|i| bd(&format!("{}", i % 500))).collect();
		let policy = CheckpointPolicy { stride: Some(1024), min_rows: 8_192, max_codec_overhead: 1.25 };

		let plain_dir = TempDir::new().expect("tempdir");
		let plain = SegmentStore::open(plain_dir.path()).await.expect("opens");
		let plain_desc = plain.seal("temp", &schema(), &ts, &vs).await.expect("seals");

		let cp_dir = TempDir::new().expect("tempdir");
		let cp = SegmentStore::open(cp_dir.path()).await.expect("opens").with_checkpoint_policy(policy);
		let cp_desc = cp.seal("temp", &schema(), &ts, &vs).await.expect("seals");

		assert_eq!(cp_desc.byte_len, plain_desc.byte_len, "a regular column must be byte-for-byte identical — no index written");
	}

	/// The aspects and segments both the checked-in pre-v2 fixture
	/// (`tests/fixtures/pre_v2_store`) and the relocation test hold: `price` with a dense,
	/// a nullable, a paged and an out-of-order segment (the last overlapping the first, at
	/// timestamps the first does not carry), and `temp` with one dense segment.
	async fn seal_legacy_layout(store: &SegmentStore) {
		let schema = schema();
		for aspect in ["price", "temp"] {
			store.declare(aspect, &schema).await.expect("declares");
		}
		let dense_ts: Vec<i64> = (0..10).map(|i| i * 10).collect();
		let dense_vs: Vec<BigDecimal> = (0..10).map(|i| bd(&format!("{i}.5"))).collect();
		store.seal("price", &schema, &dense_ts, &dense_vs).await.expect("seals the dense segment");
		store.seal_nullable("price", &schema, &[100, 110, 120, 130, 140, 150], &[Some(bd("7.25")), None, Some(bd("-3")), None, Some(bd("12")), Some(bd("0.125"))]).await.expect("seals the nullable segment");
		let paged_ts: Vec<i64> = (0..12).map(|i| 200 + i * 10).collect();
		let paged_vs: Vec<BigDecimal> = (0..12).map(|i| BigDecimal::from(100 - i * 3)).collect();
		store.seal_paged("price", &schema, &paged_ts, &paged_vs, 4).await.expect("seals the paged segment");
		store.seal("price", &schema, &[55, 45, 65], &[bd("9"), bd("8"), bd("7")]).await.expect("seals the out-of-order segment");
		store.seal("temp", &schema, &[0, 60, 120, 180], &[bd("20.5"), bd("21"), bd("21.5"), bd("22")]).await.expect("seals temp");
	}

	/// Every read a store serves over [`seal_legacy_layout`]'s aspects, rendered as text:
	/// the whole range, single and batched points, a downsample its sidecars can answer and
	/// one only the frames can (P50), and a value range. Each frame-reading path of the
	/// store is on it, so two stores whose texts match read the same bytes the same way.
	async fn fixture_reads(store: &SegmentStore) -> String {
		use std::fmt::Write as _;

		let show = |vs: &[Option<BigDecimal>]| vs.iter().map(|v| v.as_ref().map_or_else(|| "null".to_string(), BigDecimal::to_plain_string)).collect::<Vec<_>>().join(",");
		let instants = [0_i64, 45, 55, 60, 110, 120, 230, 310, 999];
		let mut out = String::new();
		for aspect in ["price", "temp"] {
			let (ts, vs) = store.read_time_range(aspect, i64::MIN, i64::MAX).await.expect("reads the range");
			writeln!(out, "{aspect} range {ts:?} [{}]", show(&vs)).expect("formats");
			let mut points = Vec::new();
			for &t in &instants {
				points.push(store.read_point(aspect, t).await.expect("reads a point"));
			}
			writeln!(out, "{aspect} points {instants:?} [{}]", show(&points)).expect("formats");
			let batch = store.read_points(aspect, &instants).await.expect("reads points");
			writeln!(out, "{aspect} batch [{}]", show(&batch)).expect("formats");
			for aggregations in [&[Aggregation::Min, Aggregation::Max, Aggregation::Sum, Aggregation::Last][..], &[Aggregation::P50][..]] {
				for bucket in store.downsample_range(aspect, i64::MIN, i64::MAX, Resolution::Minutes, aggregations).await.expect("downsamples") {
					write!(out, "{aspect} bucket {} n={}", bucket.timestamp.timestamp(), bucket.count).expect("formats");
					for (name, value) in &bucket.values {
						write!(out, " {name}={}", value.to_plain_string()).expect("formats");
					}
					writeln!(out).expect("formats");
				}
			}
			let (vts, vvs) = store.read_value_range(aspect, &bd("-5"), &bd("50")).await.expect("reads a value range");
			writeln!(out, "{aspect} values {vts:?} [{}]", vvs.iter().map(BigDecimal::to_plain_string).collect::<Vec<_>>().join(",")).expect("formats");
		}
		out
	}

	/// Recursively copy the directory `src` to `dst`.
	fn copy_tree(src: &Path, dst: &Path) {
		std::fs::create_dir_all(dst).expect("creates the copy");
		for entry in std::fs::read_dir(src).expect("lists the source") {
			let entry = entry.expect("reads an entry");
			let to = dst.join(entry.file_name());
			if entry.file_type().expect("reads the entry type").is_dir() {
				copy_tree(&entry.path(), &to);
			} else {
				std::fs::copy(entry.path(), &to).expect("copies a file");
			}
		}
	}

	/// Every file under `dir` (recursively) with its bytes, keyed by its path below `dir`.
	fn tree_bytes(dir: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
		fn walk(base: &Path, dir: &Path, out: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
			for entry in std::fs::read_dir(dir).expect("lists") {
				let path = entry.expect("reads an entry").path();
				if path.is_dir() {
					walk(base, &path, out);
				} else {
					out.insert(path.strip_prefix(base).expect("under the base").to_path_buf(), std::fs::read(&path).expect("reads a file"));
				}
			}
		}
		let mut out = std::collections::BTreeMap::new();
		walk(dir, dir, &mut out);
		out
	}

	/// What `root`'s `segment_index.db` holds besides frames' data, as text: every table
	/// with its columns (name, type, NOT NULL, default, primary-key position), every
	/// `segment_index` row's identity and write-once columns, and every `store_meta` row.
	/// Read with a connection of its own, so the store must be closed.
	async fn index_schema(root: &Path) -> String {
		use std::fmt::Write as _;

		async fn texts(conn: &turso::Connection, sql: &str) -> Vec<Vec<String>> {
			let mut rows = conn.query(sql, ()).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
			let mut out = Vec::new();
			while let Some(row) = rows.next().await.expect("reads a row") {
				out.push((0..row.column_count()).map(|i| format!("{:?}", row.get_value(i).expect("reads a value"))).collect());
			}
			out
		}
		let db = turso::Builder::new_local(&root.join("segment_index.db").to_string_lossy()).build().await.expect("opens the index");
		let conn = db.connect().expect("connects");
		let mut out = String::new();
		for table in texts(&conn, "SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name").await {
			let name = table[0].trim_start_matches("Text(\"").trim_end_matches("\")").to_string();
			let columns: Vec<String> = texts(&conn, &format!("PRAGMA table_info({name})")).await.into_iter().map(|c| c[1..].join(" ")).collect();
			writeln!(out, "table {name}: {}", columns.join(" | ")).expect("formats");
		}
		for row in texts(&conn, "SELECT aspect, id, path, gen, prec, frame_crc, commit_epoch FROM segment_index ORDER BY aspect, id").await {
			writeln!(out, "row {}", row.join(" ")).expect("formats");
		}
		for row in texts(&conn, "SELECT key, value FROM store_meta ORDER BY key").await {
			writeln!(out, "meta {}", row.join(" ")).expect("formats");
		}
		drop(conn);
		drop(db);
		out
	}

	/// The checked-in pre-v2 store (`tests/fixtures/pre_v2_store/root`, written by WeftDB
	/// before layout v2: absolute frame paths into a root that no longer exists,
	/// `{aspect}-{id}` frame names, v3 sidecars) migrates in place on open, and the
	/// migration is idempotent: three opens in a row leave the same tables, columns, rows
	/// and `store_meta` (one `store_uuid`), rewrite no frame, and every open reads
	/// exactly what the pre-v2 build read when it wrote the fixture
	/// (`expected_reads.txt`). That last part also needs the frame paths resolved
	/// against the current root.
	#[tokio::test]
	async fn a_pre_v2_store_migrates_idempotently_and_reads_identically() {
		let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pre_v2_store");
		let expected_reads = std::fs::read_to_string(fixture.join("expected_reads.txt")).expect("reads the expected reads");
		let dir = TempDir::new().expect("tempdir");
		let root = dir.path().join("store");
		copy_tree(&fixture.join("root"), &root);
		let frames_before = tree_bytes(&root.join("segments"));

		let mut schemas = Vec::new();
		for open in 1..=3 {
			let store = SegmentStore::open(&root).await.unwrap_or_else(|e| panic!("open {open}: {e:#}"));
			let reads = fixture_reads(&store).await;
			drop(store);
			assert_eq!(reads, expected_reads, "open {open} reads what the pre-v2 build read");
			schemas.push(index_schema(&root).await);
		}
		assert_eq!(tree_bytes(&root.join("segments")), frames_before, "the migration rewrites no frame or sidecar");
		assert_eq!(schemas[1], schemas[0], "the second open changes nothing the first did not");
		assert_eq!(schemas[2], schemas[0], "nor does the third");

		let schema = &schemas[0];
		for table in ["aspect_metadata", "aspect_seq", "frame_journal", "ingest_ledger", "segment_changes", "segment_index", "segment_quarantine", "store_meta"] {
			assert!(schema.contains(&format!("table {table}: ")), "{table} exists:\n{schema}");
		}
		assert!(schema.contains("| Text(\"gen\") Text(\"INTEGER\") Integer(1) Text(\"0\") Integer(0) | Text(\"prec\") Text(\"INTEGER\") Integer(0) Null Integer(0) | Text(\"frame_crc\") Text(\"INTEGER\") Integer(0) Null Integer(0) | Text(\"commit_epoch\") Text(\"INTEGER\") Integer(0) Null Integer(0)\n"), "segment_index gains gen (NOT NULL DEFAULT 0), prec, frame_crc and commit_epoch:\n{schema}");
		let rows: Vec<&str> = schema.lines().filter(|line| line.starts_with("row ")).collect();
		assert_eq!(rows.len(), 5, "every legacy row is kept:\n{schema}");
		for row in rows {
			assert!(row.contains("/var/tmp/weft-pre-v2-fixture/root/segments/"), "the stored path is left as the pre-v2 build wrote it: {row}");
			assert!(row.ends_with(" Integer(0) Null Null Null"), "a legacy row is generation 0 with no prec, frame_crc or commit_epoch: {row}");
		}
		assert!(schema.contains("meta Text(\"layout_version\") Text(\"2\")\n"), "{schema}");
		let uuid = schema.lines().find_map(|line| line.strip_prefix("meta Text(\"store_uuid\") Text(\"")).and_then(|rest| rest.strip_suffix("\")")).unwrap_or_else(|| panic!("a store_uuid is recorded:\n{schema}"));
		assert!(uuid::Uuid::parse_str(uuid).is_ok(), "the store_uuid is a UUID: {uuid}");
	}

	#[test]
	fn a_frame_path_resolves_against_the_current_root() {
		let root = Path::new("/srv/weft/store");
		let resolve = |stored: &str| resolve_frame_path(root, stored);
		// Under the current root: used as it is.
		assert_eq!(resolve("/srv/weft/store/segments/price-0.weftseg"), root.join("segments/price-0.weftseg"));
		// Written under a root that moved: the tail after `segments` lands in this root's.
		assert_eq!(resolve("/old/place/segments/price-0.weftseg"), root.join("segments/price-0.weftseg"));
		assert_eq!(resolve("/old/place/segments/quarantine/price-0.weftseg"), root.join("segments/quarantine/price-0.weftseg"));
		// The last `segments` component counts: a root that itself sits under one.
		assert_eq!(resolve("/data/segments/old-root/segments/temp-3.weftseg"), root.join("segments/temp-3.weftseg"));
		// A relative root resolves the same way.
		assert_eq!(resolve_frame_path(Path::new("store"), "store/segments/a-1.weftseg"), Path::new("store/segments/a-1.weftseg"));
		assert_eq!(resolve_frame_path(Path::new("store"), "elsewhere/segments/a-1.weftseg"), Path::new("store/segments/a-1.weftseg"));
		// Nothing safe to resolve to: kept as it is.
		assert_eq!(resolve("/old/place/frames/price-0.weftseg"), Path::new("/old/place/frames/price-0.weftseg"));
		assert_eq!(resolve("/old/place/segments"), Path::new("/old/place/segments"));
		assert_eq!(resolve("/old/segments/../../etc/passwd"), Path::new("/old/segments/../../etc/passwd"));
		// Component-wise, not textual: a sibling whose name extends the root's is not under it.
		assert_eq!(resolve("/srv/weft/store-old/segments/a-1.weftseg"), root.join("segments/a-1.weftseg"));
	}

	#[test]
	fn the_ambiguous_commit_choice_reads_poison_or_exit() {
		assert_eq!(OnAmbiguousCommit::parse(None), OnAmbiguousCommit::Poison);
		assert_eq!(OnAmbiguousCommit::parse(Some("")), OnAmbiguousCommit::Poison);
		assert_eq!(OnAmbiguousCommit::parse(Some("poison")), OnAmbiguousCommit::Poison);
		assert_eq!(OnAmbiguousCommit::parse(Some(" EXIT ")), OnAmbiguousCommit::Exit);
		assert_eq!(OnAmbiguousCommit::parse(Some("exit")), OnAmbiguousCommit::Exit);
		assert_eq!(OnAmbiguousCommit::parse(Some("abort")), OnAmbiguousCommit::Poison, "an unknown value keeps the safe default");
	}

	/// A transaction that upserts `descriptor` under `aspect` and then, after its COMMIT
	/// has executed, hits `S-commit-phantom`: armed with an error, that is a COMMIT whose
	/// caller sees a failure although the transaction is durable.
	fn phantom_upsert(aspect: &str, descriptor: SegmentDescriptor) -> IndexTxn {
		IndexTxn::new(vec![IndexOp::Upsert { aspect: aspect.to_string(), row: IndexRow::legacy(descriptor) }]).with_points(TxnPoints { phantom: Some(FaultPoint::SCommitPhantom), ..TxnPoints::NONE })
	}

	/// Write a frame of `rows` for `aspect`'s segment `id` under `root` the way a seal
	/// does, without indexing it, and return its descriptor.
	fn write_frame(root: &Path, aspect: &str, id: u64, rows: &[(i64, &str)]) -> SegmentDescriptor {
		let (ts, vs): (Vec<i64>, Vec<BigDecimal>) = rows.iter().map(|(t, v)| (*t, bd(v))).unzip();
		let segment = schema().seal(&ts, &vs).expect("encodes");
		let bytes = segment.write_to();
		let path = root.join("segments").join(format!("{aspect}-{id}.weftseg"));
		std::fs::write(&path, &bytes).expect("writes the frame");
		SegmentDescriptor::of_segment(id, path.to_string_lossy().into_owned(), bytes.len() as u64, &segment)
	}

	/// The poison, end to end: a COMMIT that executes but reports an error (a phantom
	/// fault after it) is ambiguous, so the store poisons itself. Every write entry point
	/// then fails with `Poisoned` before touching anything, while every read keeps
	/// serving, including the phantom transaction's row, which did commit. A restart
	/// clears the poison.
	#[tokio::test]
	#[serial(index_txn_fault_points)]
	async fn a_phantom_commit_poisons_writes_and_leaves_reads_up() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("price", &schema()).await.expect("declares");
		store.seal("price", &schema(), &[0, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.expect("seals");
		assert_eq!(store.poisoned(), None, "a fresh store accepts writes");

		let phantom = write_frame(dir.path(), "price", 1, &[(30, "4"), (40, "5")]);
		let armed = fault::arm(FaultPoint::SCommitPhantom, fault::FaultAction::ReturnErr);
		let err = store.commit_index(&phantom_upsert("price", phantom)).await.expect_err("the phantom fault reports a committed transaction as failed");
		drop(armed);
		let txn_error = err.downcast_ref::<IndexTxnError>().expect("the transaction's own error");
		assert!(txn_error.is_ambiguous(), "{txn_error}");
		let poisoned = store.poisoned().expect("an ambiguous COMMIT poisons the store");
		assert!(poisoned.reason.contains("injected fault at S-commit-phantom"), "{poisoned}");

		// Every write entry point, each with arguments under which it would write
		// something: frames for the seals, metadata.db for the rollup rebuilds, a squash
		// of the two segments. The refusal must come first, so no file of the root
		// changes (a seal refused only by `commit_index` would leave its frame behind, and
		// a rollup rebuild never reaches `commit_index` at all).
		let schema = schema();
		let policy = SplitPolicy { min_split_bytes: 0 };
		let (ts, vs, nullable) = ([50_i64, 60], [bd("6"), bd("7")], [Some(bd("6")), None]);
		let before = tree_bytes(dir.path());
		let refused: Vec<(&str, Result<()>)> = vec![("declare", store.declare("temp", &schema).await), ("seal", store.seal("price", &schema, &ts, &vs).await.map(drop)), ("seal_nullable", store.seal_nullable("price", &schema, &ts, &nullable).await.map(drop)), ("seal_paged", store.seal_paged("price", &schema, &ts, &vs, 1).await.map(drop)), ("seal_paged_nullable", store.seal_paged_nullable("price", &schema, &ts, &nullable, 1).await.map(drop)), ("seal_declared", store.seal_declared("price", &ts, &vs).await.map(drop)), ("seal_declared_nullable", store.seal_declared_nullable("price", &ts, &nullable).await.map(drop)), ("seal_declared_paged", store.seal_declared_paged("price", &ts, &vs, 1).await.map(drop)), ("reconcile_segment", store.reconcile_segment("price", 0).await.map(drop)), ("split_segment", store.split_segment("price", 0, 10).await.map(drop)), ("reconcile_aspect", store.reconcile_aspect("price").await.map(drop)), ("reconcile_aspect_if_unsorted_exceeds", store.reconcile_aspect_if_unsorted_exceeds("price", 1).await.map(drop)), ("reconcile_all_over_threshold", store.reconcile_all_over_threshold(1, MaintenanceWait::Skip).await.map(drop)), ("reconcile_aspect_hot_cold", store.reconcile_aspect_hot_cold("price", 1).await.map(drop)), ("reconcile_all_hot_cold", store.reconcile_all_hot_cold(1, MaintenanceWait::Skip).await.map(drop)), ("reconcile_overlaps", store.reconcile_overlaps("price").await.map(drop)), ("reconcile_overlaps_with_policy", store.reconcile_overlaps_with_policy("price", policy).await.map(drop)), ("reconcile_all_overlaps", store.reconcile_all_overlaps(MaintenanceWait::Skip).await.map(drop)), ("reconcile_all_overlaps_with_policy", store.reconcile_all_overlaps_with_policy(policy, MaintenanceWait::Skip).await.map(drop)), ("squash_aspect", store.squash_aspect("price").await.map(drop)), ("squash_aspect_if_exceeds", store.squash_aspect_if_exceeds("price", 1).await.map(drop)), ("squash_all_over_threshold", store.squash_all_over_threshold(1, MaintenanceWait::Skip).await.map(drop)), ("squash_aspect_to_target_rows", store.squash_aspect_to_target_rows("price", 10).await.map(drop)), ("squash_all_to_target_rows", store.squash_all_to_target_rows(10, MaintenanceWait::Skip).await.map(drop)), ("squash_aspect_to_target_rows_if_fragmented", store.squash_aspect_to_target_rows_if_fragmented("price", 10).await.map(drop)), ("squash_all_to_target_rows_if_fragmented", store.squash_all_to_target_rows_if_fragmented(10, MaintenanceWait::Skip).await.map(drop)), ("rebuild_aspect_metadata", store.rebuild_aspect_metadata("price").await.map(drop)), ("rebuild_all_metadata", store.rebuild_all_metadata().await.map(drop)), ("commit_index", store.commit_index(&IndexTxn::new(vec![IndexOp::Delete { aspect: "price".to_string(), id: 0 }])).await.map(drop))];
		let after = tree_bytes(dir.path());
		for (write, result) in &refused {
			let err = result.as_ref().err().unwrap_or_else(|| panic!("{write} is refused"));
			assert_eq!(err.downcast_ref::<Poisoned>(), Some(&poisoned), "{write} is refused as poisoned: {err:#}");
		}
		let changed: Vec<&PathBuf> = before.keys().chain(after.keys()).filter(|path| before.get(*path) != after.get(*path)).collect();
		assert!(changed.is_empty(), "no refused write changed a file of the root (frames, metadata.db, the index): {changed:?}");

		let (times, values) = store.read_time_range("price", i64::MIN, i64::MAX).await.expect("a poisoned store still reads");
		let point = store.read_point("price", 40).await.expect("reads a point");
		let count = store.segment_count("price").await.expect("counts");
		let declared = store.list_declared_aspects().await.expect("lists");
		drop(store);
		assert_eq!(times, vec![0, 10, 20, 30, 40], "the phantom transaction did commit, and its row reads back");
		assert_eq!(values.into_iter().flatten().map(|v| v.to_plain_string()).collect::<Vec<_>>(), vec!["1", "2", "3", "4", "5"]);
		assert_eq!(point, Some(bd("5")));
		assert_eq!(count, 2, "no refused write added a segment");
		assert_eq!(declared, vec!["price".to_string()], "the refused declare declared nothing");
		assert_eq!(names_in(&dir.path().join("segments")), vec!["price-0.weftseg", "price-1.weftseg"], "no refused write left a frame behind");

		let reopened = SegmentStore::open(dir.path()).await.expect("reopens");
		let healthy = reopened.poisoned();
		let sealed = reopened.seal("price", &schema, &[50], &[bd("6")]).await;
		drop(reopened);
		assert_eq!(healthy, None, "the restart clears the poison");
		assert_eq!(sealed.expect("the restarted store writes again").id, 2);
	}

	/// The poison is checked again before each retry. A guarded transaction that lost a
	/// conflict (a winner holds an uncommitted change to its row) waits out its backoff;
	/// another writer poisons the store meanwhile, and the transaction gives up with
	/// `Poisoned` instead of starting another attempt, which could commit after the
	/// poison. `S-txn-begun`, paused, holds the first attempt until the poison is set and
	/// counts the attempts: a second one would park there for good, and the timeout
	/// would fail the test.
	#[tokio::test]
	#[serial(index_txn_fault_points)]
	async fn a_transaction_waiting_to_retry_stops_at_the_poison() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		let base = IndexRow { desc: write_frame(dir.path(), "price", 0, &[(0, "1")]), gen: 1, prec: Some(1), frame_crc: Some(7), commit_epoch: Some(1) };
		store.commit_index(&IndexTxn::new(vec![IndexOp::InsertNew { aspect: "price".to_string(), row: base.clone() }])).await.expect("commits the base row");
		let winner = store.index().database().connect().expect("connects");
		winner.execute("BEGIN CONCURRENT", ()).await.expect("begins");
		winner.execute("UPDATE segment_index SET frame_crc = 8 WHERE aspect = 'price' AND id = 0", ()).await.expect("the winner takes the row");

		let loser = IndexTxn::new(vec![IndexOp::ReplaceExpected { aspect: "price".to_string(), expected: base.version(), row: IndexRow { gen: 2, ..base.clone() } }]).with_points(TxnPoints { begun: Some(FaultPoint::STxnBegun), ..TxnPoints::NONE });
		let resume = std::sync::Arc::new(tokio::sync::Notify::new());
		let armed = fault::arm(FaultPoint::STxnBegun, fault::FaultAction::Pause(resume.clone()));
		let hits = fault::hits(FaultPoint::STxnBegun);
		let poison_meanwhile = async {
			fault::reached(FaultPoint::STxnBegun, hits + 1).await;
			store.poison.set("a phantom commit elsewhere".to_string());
			resume.notify_one();
		};
		let raced = tokio::time::timeout(std::time::Duration::from_secs(30), async { tokio::join!(store.commit_index(&loser), poison_meanwhile) }).await;
		let attempts = fault::hits(FaultPoint::STxnBegun) - hits;
		drop(armed);
		winner.execute("ROLLBACK", ()).await.expect("the winner gives up");
		drop(winner);
		let rows = store.index().rows("price").await.expect("reads");
		drop(store);
		let (result, ()) = raced.expect("the transaction stopped instead of starting a second attempt");
		let err = result.expect_err("the transaction did not commit");
		assert_eq!(err.downcast_ref::<Poisoned>().map(|poisoned| poisoned.reason.as_str()), Some("a phantom commit elsewhere"), "{err:#}");
		assert_eq!(attempts, 1, "no attempt started once the store was poisoned");
		assert_eq!(rows, vec![base], "the row is as it was");
	}

	/// Set only in the child process `an_ambiguous_commit_exits_when_asked_to` starts:
	/// the root to open.
	const AMBIGUOUS_CHILD_ROOT: &str = "WEFT_TEST_AMBIGUOUS_CHILD_ROOT";

	/// The body that child runs: commit a phantom transaction with `S-commit-phantom`
	/// armed by the parent's `WEFT_FAULT` and `WEFT_ON_AMBIGUOUS_COMMIT=exit` set, so the
	/// store exits the process. In a normal test run the variable is unset and this does
	/// nothing.
	#[tokio::test]
	async fn ambiguous_commit_child() {
		let Some(root) = std::env::var_os(AMBIGUOUS_CHILD_ROOT) else { return };
		crate::types::durable::fault::suppress_core_dump();
		let root = PathBuf::from(root);
		let store = SegmentStore::open(&root).await.expect("opens");
		let phantom = write_frame(&root, "price", 7, &[(70, "7")]);
		let result = store.commit_index(&phantom_upsert("price", phantom)).await;
		panic!("the ambiguous COMMIT returned instead of exiting the process: {result:?}");
	}

	/// `WEFT_ON_AMBIGUOUS_COMMIT=exit`: an ambiguous COMMIT logs and exits the process
	/// with status 70 instead of poisoning the store. The restarted store has the
	/// transaction (the fault fired after its COMMIT) and accepts writes.
	#[tokio::test]
	async fn an_ambiguous_commit_exits_when_asked_to() {
		let dir = TempDir::new().expect("tempdir");
		let exe = std::env::current_exe().expect("finds the test binary");
		let out = tokio::process::Command::new(exe).args(["types::segment_store::tests::ambiguous_commit_child", "--exact", "--nocapture", "--test-threads=1"]).env(AMBIGUOUS_CHILD_ROOT, dir.path()).env(fault::FAULT_ENV, "S-commit-phantom:err").env(AMBIGUOUS_COMMIT_ENV, "exit").output().await.expect("runs the child");
		let stderr = String::from_utf8_lossy(&out.stderr);
		assert_eq!(out.status.code(), Some(AMBIGUOUS_COMMIT_EXIT_CODE), "the child exits with status 70: {out:?}");
		assert!(stderr.contains("exiting with status 70 (WEFT_ON_AMBIGUOUS_COMMIT=exit)") && stderr.contains("injected fault at S-commit-phantom"), "it says why: {stderr}");

		let store = SegmentStore::open(dir.path()).await.expect("the restart opens the store");
		let poisoned = store.poisoned();
		let ids: Vec<u64> = store.index().all("price").await.expect("reads").iter().map(|d| d.id).collect();
		let point = store.read_point("price", 70).await.expect("reads the committed row");
		let sealed = store.seal("price", &schema(), &[80], &[bd("8")]).await;
		drop(store);
		assert_eq!(poisoned, None);
		assert_eq!(ids, vec![7], "the transaction committed before the process exited");
		assert_eq!(point, Some(bd("7")));
		assert!(sealed.is_ok(), "the restarted store accepts writes: {sealed:?}");
	}

	/// Readers resolve a frame against the store's current root rather than the absolute
	/// path the index recorded, so a root that was moved (or restored somewhere else, or
	/// mounted at another path) still serves every read once the old location is gone.
	/// The maintenance operations read frames through the same resolution.
	#[tokio::test]
	async fn a_relocated_root_serves_every_read() {
		let dir = TempDir::new().expect("tempdir");
		let original = dir.path().join("original");
		let store = SegmentStore::open(&original).await.expect("opens").with_partial_sidecar_policy(PartialSidecarPolicy::at(Resolution::Minutes, 1));
		seal_legacy_layout(&store).await;
		let before = fixture_reads(&store).await;
		let (range_ts, range_vs) = store.read_time_range("price", i64::MIN, i64::MAX).await.expect("reads");
		drop(store);

		let moved = dir.path().join("moved");
		copy_tree(&original, &moved);
		std::fs::remove_dir_all(&original).expect("removes the original root");

		let store = SegmentStore::open(&moved).await.expect("the moved root opens");
		let after = fixture_reads(&store).await;
		// Segment 3 is out of order (reconcile reads it), segment 0 is sorted (split reads
		// it), and squash decodes every segment the aspect has.
		let reconciled = store.reconcile_segment("price", 3).await.expect("reconciles a frame of the moved root");
		let split = store.split_segment("price", 0, 50).await.expect("splits a frame of the moved root");
		let squashed = store.squash_aspect("price").await.expect("squashes the moved root's frames");
		let (squashed_ts, squashed_vs) = store.read_time_range("price", i64::MIN, i64::MAX).await.expect("reads the squashed aspect");
		drop(store);
		assert_eq!(after, before, "the moved root reads exactly what the original did");
		assert!(reconciled);
		assert!(split.is_some());
		assert_eq!(squashed, 4, "five segments (after the split) squash into one");
		// No two segments share a timestamp, so the squash is their union in time order.
		let mut union: Vec<(i64, Option<BigDecimal>)> = range_ts.into_iter().zip(range_vs).collect();
		union.sort_by_key(|(t, _)| *t);
		assert_eq!(squashed_ts.into_iter().zip(squashed_vs).collect::<Vec<_>>(), union);
	}

	/// Batch `i` of the concurrency tests: ten rows from `i * 1000` on, ten apart, each
	/// valued by its own timestamp, so no two batches share a timestamp or a value.
	fn numbered_batch(i: usize) -> (Vec<i64>, Vec<BigDecimal>) {
		let base = i64::try_from(i).expect("a small batch number") * 1000;
		((0..10).map(|r| base + r * 10).collect(), (0..10).map(|r| BigDecimal::from(base + r * 10)).collect())
	}

	/// Seal [`numbered_batch`]es `0..count` into `aspect` at once, each from a task of its
	/// own, and return every seal's result in batch order.
	async fn seal_concurrently(store: &std::sync::Arc<SegmentStore>, aspect: &str, count: usize) -> Vec<Result<SegmentDescriptor>> {
		let tasks: Vec<_> = (0..count)
			.map(|i| {
				let (store, aspect) = (store.clone(), aspect.to_string());
				tokio::spawn(async move {
					let (ts, vs) = numbered_batch(i);
					store.seal(&aspect, &schema(), &ts, &vs).await
				})
			})
			.collect();
		let mut results = Vec::with_capacity(count);
		for task in tasks {
			results.push(task.await.expect("a seal task joins"));
		}
		results
	}

	/// The batches of `0..count` that do not read back from `aspect` exactly as sealed.
	async fn unreadable_batches(store: &SegmentStore, aspect: &str, count: usize) -> Vec<usize> {
		let mut lost = Vec::new();
		for i in 0..count {
			let (ts, vs) = numbered_batch(i);
			let (read_ts, read_vs) = store.read_time_range(aspect, ts[0], ts[ts.len() - 1]).await.expect("reads the batch's window");
			if read_ts != ts || read_vs != vs.into_iter().map(Some).collect::<Vec<_>>() {
				lost.push(i);
			}
		}
		lost
	}

	/// The error of every seal in `results` that failed, by batch number.
	fn failed_seals(results: &[Result<SegmentDescriptor>]) -> Vec<String> {
		results.iter().enumerate().filter_map(|(i, result)| result.as_ref().err().map(|e| format!("seal {i}: {e:#}"))).collect()
	}

	/// Crash-consistency S7, window race-next-id-collision: 64 seals of one aspect at once
	/// each get an id of their own, and every batch reads back. A seal used to take
	/// `MAX(id) + 1`, read with no lock held, so concurrent seals took the same id: the
	/// later one's truncating write replaced the earlier one's frame and its
	/// `INSERT OR REPLACE` its row, and an acknowledged batch was gone (or the second
	/// writer of the row lost an MVCC conflict and the seal failed).
	#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
	async fn concurrent_seals_of_one_aspect_take_distinct_ids_and_all_read_back() {
		const SEALS: usize = 64;
		let dir = TempDir::new().expect("tempdir");
		let store = std::sync::Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
		let results = seal_concurrently(&store, "price", SEALS).await;
		let ids: std::collections::BTreeSet<u64> = results.iter().filter_map(|result| result.as_ref().ok().map(|d| d.id)).collect();
		let lost = unreadable_batches(&store, "price", SEALS).await;
		let count = store.segment_count("price").await.expect("counts");
		drop(store);
		assert_eq!(failed_seals(&results), Vec::<String>::new(), "every seal succeeds");
		assert_eq!(ids.len(), SEALS, "every seal took an id of its own: {ids:?}");
		assert_eq!(lost, Vec::<usize>::new(), "every acknowledged batch reads back exactly as sealed");
		assert_eq!(count, SEALS);
	}

	/// Crash-consistency S7, window race-metadata-rollup-lost-update: after 200 seals of
	/// one aspect at once, the materialized rollup equals the one derived from the index.
	/// Each seal used to fold itself into the rollup with an unlocked get-then-put, so two
	/// seals read the same rollup and one fold was lost (or the second put lost an MVCC
	/// conflict and failed the seal after its row had committed).
	#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
	async fn concurrent_seals_keep_the_rollup_equal_to_the_index() {
		const SEALS: usize = 200;
		let dir = TempDir::new().expect("tempdir");
		let store = std::sync::Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
		let results = seal_concurrently(&store, "price", SEALS).await;
		let rollup = store.aspect_metadata("price").await.expect("reads the rollup");
		let derived = AspectMetadata::from_index(&store.index().load_index("price").await.expect("loads the index"));
		drop(store);
		assert_eq!(failed_seals(&results), Vec::<String>::new(), "every seal succeeds");
		assert_eq!(rollup, derived, "the rollup is exactly what the index says");
		assert_eq!(derived.segment_count, SEALS);
	}

	/// Crash-consistency S7, window race-max-id-reused-between-delete-and-unlink: an id is
	/// never handed out twice, not even once the segment that had it is gone. A squash
	/// keeps its lowest member and deletes the rest, the highest id among them; the next
	/// seal used to take `MAX(id) + 1`, that same id, so a seal racing the squash's unlink
	/// lost its frame, and a leftover sidecar of the deleted segment could match it. The
	/// allocator is persisted in `aspect_seq`, so a reopened store, whose rows and frames
	/// no longer show the deleted ids, does not reissue them either.
	#[tokio::test]
	async fn an_id_freed_by_deleting_the_highest_segment_is_never_reissued() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("price", &schema()).await.expect("declares");
		for i in 0..3 {
			let (ts, vs) = numbered_batch(i);
			store.seal("price", &schema(), &ts, &vs).await.expect("seals");
		}
		let removed = store.squash_aspect("price").await.expect("squashes");
		let (ts, vs) = numbered_batch(3);
		let after_squash = store.seal("price", &schema(), &ts, &vs).await.expect("seals after the squash").id;
		let removed_again = store.squash_aspect("price").await.expect("squashes again");
		drop(store);
		let store = SegmentStore::open(dir.path()).await.expect("reopens");
		let (ts, vs) = numbered_batch(4);
		let after_reopen = store.seal("price", &schema(), &ts, &vs).await.expect("seals after the reopen").id;
		let ids: Vec<u64> = store.index().all("price").await.expect("reads").iter().map(|d| d.id).collect();
		let lost = unreadable_batches(&store, "price", 5).await;
		drop(store);
		assert_eq!(removed, 2, "the squash deleted segments 1 and 2");
		assert_eq!(after_squash, 3, "the seal after the squash does not reuse id 2 (or 1)");
		assert_eq!(removed_again, 1, "the second squash deleted segment 3, the highest");
		assert_eq!(after_reopen, 4, "nor does the first seal after a reopen, although no row or frame has an id above 0");
		assert_eq!(ids, vec![0, 4]);
		assert_eq!(lost, Vec::<usize>::new(), "every batch reads back");
	}

	/// Set only in the child process
	/// `a_seal_does_not_overwrite_the_frame_of_a_seal_that_crashed_before_its_commit`
	/// starts: the root to open.
	const ORPHAN_CHILD_ROOT: &str = "WEFT_TEST_ORPHAN_CHILD_ROOT";

	/// The rows the child's seal writes and never commits.
	const ORPHAN_ROWS: [(i64, &str); 2] = [(100, "7"), (110, "8")];

	/// The body that child runs: seal [`ORPHAN_ROWS`] into `price` with `S-frame-written`
	/// armed by the parent's `WEFT_FAULT`, so the seal stops after writing its frame and
	/// before committing it. It returns only when the point is armed with `err` rather than
	/// `abort`. In a normal test run the variable is unset and this does nothing.
	#[tokio::test]
	async fn orphan_child() {
		let Some(root) = std::env::var_os(ORPHAN_CHILD_ROOT) else { return };
		crate::types::durable::fault::suppress_core_dump();
		let store = SegmentStore::open(PathBuf::from(root)).await.expect("opens");
		let (ts, vs): (Vec<i64>, Vec<BigDecimal>) = ORPHAN_ROWS.iter().map(|(t, v)| (*t, bd(v))).unzip();
		let err = store.seal("price", &schema(), &ts, &vs).await.expect_err("the seal stops at the armed fault");
		drop(store);
		assert!(format!("{err:#}").contains("injected fault at S-frame-written"), "{err:#}");
	}

	/// Crash-consistency S7, window seal-orphan-frame-before-index-commit: a seal that
	/// crashed after writing its frame and before committing it leaves that frame behind
	/// with no row, and the next seal after the restart takes an id above it rather than
	/// truncating it. The orphan's id used to be `MAX(id) + 1` again (nothing had committed
	/// it), so the next seal overwrote the frame in place. The crash is an injected error
	/// (after which the child exits and the store is reopened here) and a real abort.
	#[tokio::test]
	async fn a_seal_does_not_overwrite_the_frame_of_a_seal_that_crashed_before_its_commit() {
		for spec in ["S-frame-written:err", "S-frame-written:abort"] {
			let dir = TempDir::new().expect("tempdir");
			let store = SegmentStore::open(dir.path()).await.expect("opens");
			let first = store.seal("price", &schema(), &[0, 10], &[bd("1"), bd("2")]).await.expect("seals");
			drop(store);

			let exe = std::env::current_exe().expect("finds the test binary");
			let out = tokio::process::Command::new(exe).args(["types::segment_store::tests::orphan_child", "--exact", "--nocapture", "--test-threads=1"]).env(ORPHAN_CHILD_ROOT, dir.path()).env(fault::FAULT_ENV, spec).output().await.expect("runs the child");
			if spec.ends_with(":abort") {
				#[cfg(unix)]
				{
					use std::os::unix::process::ExitStatusExt;
					assert_eq!(out.status.signal(), Some(6), "{spec}: the child was killed by SIGABRT: {out:?}");
				}
				assert!(!out.status.success(), "{spec}: the child aborted: {out:?}");
			} else {
				assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).contains("1 passed"), "{spec}: the child's seal stopped at the injected error: {out:?}");
			}
			let segments = dir.path().join("segments");
			let orphans: Vec<String> = names_in(&segments).into_iter().filter(|name| name != "price-0.weftseg").collect();
			let [orphan] = orphans.as_slice() else { panic!("{spec}: the crashed seal left exactly one frame behind: {orphans:?}") };
			let orphan_bytes = std::fs::read(segments.join(orphan)).expect("reads the orphan");

			let store = SegmentStore::open(dir.path()).await.expect("reopens");
			let next = store.seal("price", &schema(), &[200, 210, 220], &[bd("3"), bd("4"), bd("5")]).await.expect("seals after the crash");
			let (ts, vs) = store.read_time_range("price", i64::MIN, i64::MAX).await.expect("reads");
			drop(store);
			assert_eq!(first.id, 0);
			assert_ne!(segments.join(orphan), PathBuf::from(&next.path), "{spec}: the next seal took a frame name of its own");
			assert_eq!(std::fs::read(segments.join(orphan)).expect("reads the orphan again"), orphan_bytes, "{spec}: the orphan frame is as the crash left it");
			assert_eq!(ts, vec![0, 10, 200, 210, 220], "{spec}: both committed seals read back, and the orphan's rows are not visible");
			assert_eq!(vs.into_iter().flatten().map(|v| v.to_plain_string()).collect::<Vec<_>>(), vec!["1", "2", "3", "4", "5"], "{spec}");
		}
	}

	/// Set only in the child process
	/// `a_reconcile_racing_a_squash_cannot_resurrect_a_merged_member` starts: the root to
	/// open.
	const RACE_CHILD_ROOT: &str = "WEFT_TEST_RACE_CHILD_ROOT";

	/// The body that child runs, in a process of its own because it arms `M-planned`
	/// in-process (a point every reconcile passes) with a pause.
	///
	/// Segment 1 is out of order and shares timestamp 20 with segment 2, which is newer.
	/// A reconcile of segment 1 reads it, sorts it and parks at `M-planned`; a squash of
	/// the aspect is started meanwhile and given a second to finish; then the reconcile
	/// goes on. The squash must not have merged segment 1 away underneath the reconcile,
	/// which would then write it back: the old row and file of a member the squash had
	/// merged, its stale value at timestamp 20 winning over segment 2's again (its id is
	/// above the squash's target), and every row it shares with the target read twice.
	#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
	async fn race_child() {
		let Some(root) = std::env::var_os(RACE_CHILD_ROOT) else { return };
		let store = std::sync::Arc::new(SegmentStore::open(PathBuf::from(root)).await.expect("opens"));
		store.declare("price", &schema()).await.expect("declares");
		store.seal("price", &schema(), &[0, 10], &[bd("1"), bd("2")]).await.expect("seals segment 0");
		store.seal("price", &schema(), &[30, 20], &[bd("30"), bd("20")]).await.expect("seals segment 1, out of order");
		store.seal("price", &schema(), &[20, 40], &[bd("200"), bd("400")]).await.expect("seals segment 2");

		let resume = std::sync::Arc::new(tokio::sync::Notify::new());
		let armed = fault::arm(FaultPoint::MPlanned, fault::FaultAction::Pause(resume.clone()));
		let hits = fault::hits(FaultPoint::MPlanned);
		let reconcile = tokio::spawn({
			let store = store.clone();
			async move { store.reconcile_segment("price", 1).await }
		});
		tokio::time::timeout(std::time::Duration::from_secs(30), fault::reached(FaultPoint::MPlanned, hits + 1)).await.expect("the reconcile reaches M-planned");
		drop(armed);
		let mut squash = tokio::spawn({
			let store = store.clone();
			async move { store.squash_aspect("price").await }
		});
		// Unserialized, the squash runs to completion here; serialized, it waits for the
		// reconcile and this times out.
		let early = tokio::time::timeout(std::time::Duration::from_secs(1), &mut squash).await;
		let squashed_first = early.is_ok();
		resume.notify_one();
		let reconciled = reconcile.await.expect("the reconcile joins");
		let squashed = match early {
			Ok(joined) => joined,
			Err(_) => squash.await,
		}
		.expect("the squash joins");

		let ids: Vec<u64> = store.index().all("price").await.expect("reads").iter().map(|d| d.id).collect();
		let (ts, vs) = store.read_time_range("price", i64::MIN, i64::MAX).await.expect("reads");
		let at_20 = store.read_point("price", 20).await.expect("reads a point");
		let rollup = store.aspect_metadata("price").await.expect("reads the rollup");
		let derived = AspectMetadata::from_index(&store.index().load_index("price").await.expect("loads the index"));
		drop(store);
		eprintln!("squash finished while the reconcile was parked: {squashed_first}");
		assert!(reconciled.as_ref().is_ok_and(|rewrote| *rewrote), "the reconcile rewrote segment 1: {reconciled:?}");
		assert!(squashed.is_ok(), "the squash succeeded: {squashed:?}");
		assert_eq!(ids, vec![0], "one segment is left, and no merged member came back");
		assert_eq!(ts, vec![0, 10, 20, 30, 40], "every timestamp reads once");
		assert_eq!(vs.into_iter().flatten().map(|v| v.to_plain_string()).collect::<Vec<_>>(), vec!["1", "2", "200", "30", "400"], "the newer segment's value wins at 20");
		assert_eq!(at_20, Some(bd("200")), "a point read agrees");
		assert_eq!(rollup, derived, "the rollup matches the index");
		assert!(!squashed_first, "the squash waited for the reconcile instead of running while it was parked");
	}

	/// Crash-consistency S7, window race-reconcile-resurrects-merged-member: a reconcile
	/// paused after reading its segment, and a squash of the same aspect started meanwhile,
	/// leave the aspect squashed with no merged member written back (see [`race_child`],
	/// which runs in a child process).
	#[tokio::test]
	async fn a_reconcile_racing_a_squash_cannot_resurrect_a_merged_member() {
		let dir = TempDir::new().expect("tempdir");
		let exe = std::env::current_exe().expect("finds the test binary");
		let out = tokio::process::Command::new(exe).args(["types::segment_store::tests::race_child", "--exact", "--nocapture", "--test-threads=1"]).env(RACE_CHILD_ROOT, dir.path()).output().await.expect("runs the child");
		assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).contains("1 passed"), "the race left the aspect squashed, with no merged member written back: {}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
	}

	/// Crash-consistency S7: seals of different aspects never conflict, and those of one
	/// aspect take turns under its commit lock instead of conflicting. Eight aspects, sixteen
	/// seals each, all at once: the store counts every segment-index attempt that lost an
	/// MVCC conflict, retried or not, and the count stays at zero; every seal succeeds,
	/// each aspect's ids are distinct and every batch reads back.
	#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
	async fn eight_aspects_sealing_at_once_lose_no_mvcc_conflict() {
		const ASPECTS: usize = 8;
		const SEALS: usize = 16;
		let dir = TempDir::new().expect("tempdir");
		let store = std::sync::Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
		let sealing: Vec<_> = (0..ASPECTS)
			.map(|a| {
				let store = store.clone();
				tokio::spawn(async move { seal_concurrently(&store, &format!("aspect{a}"), SEALS).await })
			})
			.collect();
		let mut outcomes = Vec::new();
		for (a, task) in sealing.into_iter().enumerate() {
			let results = task.await.expect("an aspect's seals join");
			let aspect = format!("aspect{a}");
			let ids: std::collections::BTreeSet<u64> = results.iter().filter_map(|result| result.as_ref().ok().map(|d| d.id)).collect();
			outcomes.push((aspect.clone(), failed_seals(&results), ids.len(), unreadable_batches(&store, &aspect, SEALS).await));
		}
		let conflicts = store.index_conflicts();
		drop(store);
		for (aspect, failed, distinct, lost) in outcomes {
			assert_eq!(failed, Vec::<String>::new(), "{aspect}: every seal succeeds");
			assert_eq!(distinct, SEALS, "{aspect}: every seal took an id of its own");
			assert_eq!(lost, Vec::<usize>::new(), "{aspect}: every batch reads back");
		}
		assert_eq!(conflicts, 0, "no segment-index attempt lost an MVCC conflict");
	}

	/// Crash-consistency S7: a split's suffix and an overlap split's suffix take their ids
	/// from the allocator seals use, so neither reuses the id of a segment a squash or a
	/// merge deleted. They used to take `MAX(id) + 1`: here the split's suffix would have
	/// taken id 1 (deleted by the squash) and the overlap split's id 3.
	#[tokio::test]
	async fn split_suffixes_take_never_used_ids() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("price", &schema()).await.expect("declares");
		for i in 0..3 {
			let (ts, vs) = numbered_batch(i);
			store.seal("price", &schema(), &ts, &vs).await.expect("seals");
		}
		store.squash_aspect("price").await.expect("squashes ids 1 and 2 into 0");
		let split = store.split_segment("price", 0, 1500).await.expect("splits segment 0");
		// A late batch overwriting the end of the split's suffix, then an overlap merge
		// under a floor small enough to split off the suffix's cold prefix.
		let late = store.seal("price", &schema(), &[2080, 2090, 2100], &[bd("-1"), bd("-2"), bd("-3")]).await.expect("seals a late batch").id;
		let removed = store.reconcile_overlaps_with_policy("price", SplitPolicy::new(1)).await.expect("merges the overlap");
		let ids: Vec<u64> = store.index().all("price").await.expect("reads").iter().map(|d| d.id).collect();
		let (ts, vs) = store.read_time_range("price", 2000, 2100).await.expect("reads the merged window");
		let next = store.seal("price", &schema(), &[5000], &[bd("5")]).await.expect("seals").id;
		drop(store);
		assert_eq!(split, Some(3), "the split's suffix takes the next never-used id");
		assert_eq!(late, 4);
		assert_eq!(removed, 0, "a two-member split keeps two segments");
		assert_eq!(ids, vec![0, 3, 5], "the overlap split kept its prefix at 3 and put its suffix at a fresh id, 5");
		assert_eq!(ts, vec![2000, 2010, 2020, 2030, 2040, 2050, 2060, 2070, 2080, 2090, 2100]);
		let tail: Vec<String> = vs.into_iter().flatten().skip(8).map(|v| v.to_plain_string()).collect();
		assert_eq!(tail, vec!["-1", "-2", "-3"], "the late batch wins where it overlaps");
		assert_eq!(next, 6, "and the next seal goes on from there");
	}

	/// Crash-consistency S7: every public maintenance entry point takes its aspect's
	/// maintenance lock. While another operation holds the aspect, each per-aspect entry
	/// point waits up to the store's `maintenance_wait` and then fails with
	/// `MaintenanceBusy`, and each store-wide sweep under `MaintenanceWait::Skip` lists
	/// the aspect in `busy`, all without changing a file. A seal is not maintenance and
	/// does not wait. Once the holder lets go within a `MaintenanceWait::Wait`, the sweep
	/// takes the aspect and maintains it.
	#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
	async fn a_busy_aspect_is_refused_skipped_or_waited_for() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens").with_maintenance_wait(std::time::Duration::from_millis(100));
		store.declare("price", &schema()).await.expect("declares");
		// Out of order and overlapping, so every operation would have work to do.
		store.seal("price", &schema(), &[30, 10, 20], &[bd("3"), bd("1"), bd("2")]).await.expect("seals");
		store.seal("price", &schema(), &[25, 15, 35], &[bd("5"), bd("4"), bd("6")]).await.expect("seals");
		let held = store.locks.try_maint("price").expect("the aspect is free");

		let started = std::time::Instant::now();
		let waited = store.reconcile_aspect("price").await.expect_err("the aspect is busy");
		let elapsed = started.elapsed();
		let store = store.with_maintenance_wait(std::time::Duration::ZERO);
		let before = tree_bytes(dir.path());
		let policy = SplitPolicy::new(1);
		let per_aspect: Vec<(&str, Result<()>)> = vec![("reconcile_segment", store.reconcile_segment("price", 0).await.map(drop)), ("split_segment", store.split_segment("price", 1, 20).await.map(drop)), ("reconcile_aspect", store.reconcile_aspect("price").await.map(drop)), ("reconcile_aspect_if_unsorted_exceeds", store.reconcile_aspect_if_unsorted_exceeds("price", 1).await.map(drop)), ("reconcile_aspect_hot_cold", store.reconcile_aspect_hot_cold("price", 1).await.map(drop)), ("reconcile_overlaps", store.reconcile_overlaps("price").await.map(drop)), ("reconcile_overlaps_with_policy", store.reconcile_overlaps_with_policy("price", policy).await.map(drop)), ("squash_aspect", store.squash_aspect("price").await.map(drop)), ("squash_aspect_if_exceeds", store.squash_aspect_if_exceeds("price", 1).await.map(drop)), ("squash_aspect_to_target_rows", store.squash_aspect_to_target_rows("price", 10).await.map(drop)), ("squash_aspect_to_target_rows_if_fragmented", store.squash_aspect_to_target_rows_if_fragmented("price", 10).await.map(drop))];
		let skip = MaintenanceWait::Skip;
		let sweeps: Vec<(&str, Result<Vec<String>>)> = vec![("reconcile_all_over_threshold", store.reconcile_all_over_threshold(1, skip).await.map(|s| s.busy)), ("reconcile_all_hot_cold", store.reconcile_all_hot_cold(1, skip).await.map(|s| s.busy)), ("reconcile_all_overlaps", store.reconcile_all_overlaps(skip).await.map(|s| s.busy)), ("reconcile_all_overlaps_with_policy", store.reconcile_all_overlaps_with_policy(policy, skip).await.map(|s| s.busy)), ("squash_all_over_threshold", store.squash_all_over_threshold(1, skip).await.map(|s| s.busy)), ("squash_all_to_target_rows", store.squash_all_to_target_rows(10, skip).await.map(|s| s.busy)), ("squash_all_to_target_rows_if_fragmented", store.squash_all_to_target_rows_if_fragmented(10, skip).await.map(|s| s.busy))];
		let after = tree_bytes(dir.path());
		let sealed = tokio::time::timeout(std::time::Duration::from_secs(30), store.seal("price", &schema(), &[50, 40], &[bd("8"), bd("7")])).await;

		let store = std::sync::Arc::new(store);
		let release = tokio::spawn(async move {
			tokio::time::sleep(std::time::Duration::from_millis(100)).await;
			drop(held);
		});
		let waited_for = store.reconcile_all_over_threshold(1, MaintenanceWait::Wait(std::time::Duration::from_secs(30))).await.expect("sweeps");
		release.await.expect("the holder lets go");
		let unsorted = store.aspect_stats("price").await.expect("reads the stats").unsorted_segments;
		drop(store);

		assert_eq!(waited.downcast_ref::<MaintenanceBusy>(), Some(&MaintenanceBusy { aspect: "price".to_string(), waited: std::time::Duration::from_millis(100) }), "{waited:#}");
		assert!(elapsed >= std::time::Duration::from_millis(100), "the entry point waited for the aspect first: {elapsed:?}");
		for (entry, result) in &per_aspect {
			let err = result.as_ref().err().unwrap_or_else(|| panic!("{entry} is refused while the aspect is busy"));
			assert_eq!(err.downcast_ref::<MaintenanceBusy>().map(|busy| busy.aspect.as_str()), Some("price"), "{entry}: {err:#}");
		}
		for (sweep, result) in &sweeps {
			let busy = result.as_ref().unwrap_or_else(|e| panic!("{sweep} sweeps: {e:#}"));
			assert_eq!(busy, &vec!["price".to_string()], "{sweep} skips the busy aspect and says so");
		}
		let changed: Vec<&PathBuf> = before.keys().chain(after.keys()).filter(|path| before.get(*path) != after.get(*path)).collect();
		assert!(changed.is_empty(), "no refused or skipped operation changed a file: {changed:?}");
		assert!(sealed.is_ok_and(|sealed| sealed.is_ok()), "a seal does not wait for maintenance");
		assert_eq!(waited_for.busy, Vec::<String>::new(), "a waiting sweep takes the aspect once it is let go");
		assert_eq!(waited_for.aspects_reconciled, 1);
		assert_eq!(unsorted, 0, "and maintains it");
	}

	/// The aspect and id of a legacy-named file, and nothing for any other name.
	#[test]
	fn legacy_file_names_parse_to_their_aspect_and_id() {
		assert_eq!(legacy_file_id("price-12.weftseg"), Some(("price", 12)));
		assert_eq!(legacy_file_id("price-12.weftpart"), Some(("price", 12)));
		assert_eq!(legacy_file_id("sensor-a-7.weftseg"), Some(("sensor-a", 7)), "the id follows the last `-`");
		assert_eq!(legacy_file_id("sensor-7-3.weftseg"), Some(("sensor-7", 3)));
		for other in ["price.weftseg", "price-.weftseg", "-3.weftseg", "price-1x.weftseg", "price-1.weftseg.tmp", "price-1.txt", "price~g1~p1.weftseg", "LOCK"] {
			assert_eq!(legacy_file_id(other), None, "{other}");
		}
	}
}
