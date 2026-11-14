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
//! **Concurrency** (basicautomation.io/projects/weftdb/docs/crash-consistency section 5.1, slice S7). Each aspect
//! has two locks. Its *commit* lock is held around every control-plane transaction of the
//! aspect and guards its id allocator: a seal takes its id from the allocator, writes its
//! frame with the lock released, then inserts its row (a plain `INSERT`, which cannot
//! replace another), raises the persisted allocator (`aspect_seq`) and folds the rollup
//! into `segment_index.db` in the same transaction; maintenance row rewrites, member
//! deletes and rollup rebuilds run under it too. So ids are never shared or reissued
//! (not after a delete, a crash or a restart), and no rollup update is lost to a
//! concurrent one. Its *maintenance* lock is held for a
//! whole maintenance operation, so two never interleave on one aspect. A per-aspect
//! maintenance entry point waits for it up to [`SegmentStore::maintenance_wait`] and then
//! fails with [`MaintenanceBusy`]; a store-wide sweep skips or waits for a busy aspect as
//! its [`MaintenanceWait`] says.
//!
//! **Write-once maintenance** (crash-consistency design sections 5.3-5.4, slices S8 and
//! S9). No maintenance operation ([`reconcile_segment`](SegmentStore::reconcile_segment),
//! [`split_segment`](SegmentStore::split_segment), the overlap merge
//! [`reconcile_overlaps`](SegmentStore::reconcile_overlaps), [`squash_aspect`](SegmentStore::squash_aspect)
//! and the compaction [`squash_aspect_to_target_rows`](SegmentStore::squash_aspect_to_target_rows),
//! and every pass built on them) rewrites or deletes a frame. Each plans a swap of its
//! input rows (its members) for its outputs, which go to new frames named
//! `{enc(aspect)}~g{gen}~p{prec}.weftseg` after a per-aspect generation that is never
//! handed out twice and the largest adoption order of their members; they are journaled as
//! pending in `frame_journal`, written and fsynced, with `segments/` fsynced after them.
//! One `segment_index.db` transaction then replaces the members' rows, if they are still
//! the versions read, deletes the members no output takes the id of, and journals every
//! member's frame as retired: a merge's members are swapped for its outputs at once, or
//! not at all. Every read pins a reclaim epoch first, and the reaper unlinks a retired
//! frame only once no read that may still open it is running (nor a backup holds it),
//! then deletes its journal row; [`reap`](SegmentStore::reap) runs it on demand. The open
//! replays the journal before it returns.
//!
//! **Precedence** (release plan D17). Where two segments share a timestamp the higher id
//! wins, so an output takes a member's id: the `j`-th output in time order the `j`-th
//! smallest member id. A merge's members are an overlap component or a run of
//! consecutive ids, and a seal that commits after the merge read the index has a higher id
//! than its members (always from S10, which hands a seal its id at commit; until then, not
//! when the seal's id was handed out before a member's was), so the outputs outrank
//! exactly what their members did. Only an output beyond the members' count (a split's
//! suffix) takes a fresh id, and only where no newer segment overlaps it. Outputs are cut
//! only at timestamp boundaries, so a run of equal timestamps is never split between two.

use std::{
	collections::{HashMap, HashSet}, ffi::OsStr, future::Future, path::{Component, Path, PathBuf}, sync::{
		atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering}, Arc, Mutex, OnceLock, PoisonError, RwLock
	}, time::{Duration, Instant}
};

use anyhow::{bail, Context, Result};
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use splimes::{Point, Resolution};
use tracing::Instrument as _;
use weft_physical_type::{merge_newer_wins, split_index, AspectSchema, FrameOptions, PagedSegment, PhysicalValue, Segment, SegmentDescriptor, SegmentIndex, SplitDecision, SplitPolicy, TimeUnit};
use weft_reduce::{Aggregation, Bucket, PartialReduction};

use crate::{
	aspect_name, types::{
		aspect_locks::{AspectLocks, AspectState, CommitGuard, MaintGuard}, durable::{
			create_dir_all_durable, fault::{self, FaultPoint}, poison_global, write_new_durable, LockHolder, RealFs, RootLock, RootLockError, StoreFs, SyncPolicy, WritePoints
		}, frame_name, frame_read, index_txn::{IndexOp, IndexRow, IndexTxn, IndexTxnError, RowVersion, TxnApplied, TxnErrorKind, TxnPoints}, ingest::{IngestBatch, IngestPhase, IngestReceipt, LedgerEntry}, migrations, quarantine::{classify_decode_failure, is_newer_format, QuarantineClass, QuarantineRecord, QuarantinedRow, RecoveryReport, QUARANTINE_TTL_MS}, reaper::{LiveFrames, ReadPin, Reaper}, segment_index::SegmentChange, store_format::{self, StoreFormat, StoreScope}
	}, AspectCatalog, AspectMetadata, AspectMetadataStore, CatalogStore, PartialSidecar, PartialSidecarPolicy, SegmentIndexStore, SnapshotReport, StoreError, CONTROL_PLANE_FILES, SIDECAR_AGGREGATIONS, SUPPORTED_LAYOUT
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
/// so it rarely wins on size — it was meant as a **decode-latency** trade. Its decoder rebuilds
/// eight lanes per plane byte with branch-free table spreads, and each tile's width header
/// drops a small-magnitude column's empty high bit-planes. The trade did not pay: the ~5.7x
/// primitive-level win once measured in `weft-physical-type`'s `benches/bitunpack.rs` was
/// against the old per-bit linear decoder, the word-wise linear unpack is now faster, and end
/// to end the codec is byte-neutral and reads no faster (`benches/transposed_read.rs`).
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
/// ingest path's unit arithmetic (`TimeUnit::from_nanos`, which truncates toward minus
/// infinity). `None` if the epoch falls outside the representable
/// range (only reachable for coarse units at absurd magnitudes).
const fn instant_from_epoch(epoch: i64, unit: TimeUnit) -> Option<DateTime<Utc>> {
	match unit {
		TimeUnit::Seconds => DateTime::<Utc>::from_timestamp(epoch, 0),
		TimeUnit::Millis => DateTime::<Utc>::from_timestamp_millis(epoch),
		TimeUnit::Micros => DateTime::<Utc>::from_timestamp_micros(epoch),
		TimeUnit::Nanos => Some(DateTime::<Utc>::from_timestamp_nanos(epoch)),
	}
}

/// The present in-window rows of a physically-decoded window as `(epoch nanoseconds,
/// mantissas, scale)`, when every present value is a `ScaledI64` at one shared scale and every
/// instant fits in epoch nanoseconds. `None` sends the caller to the `BigDecimal` path.
fn scaled_rows(ts: &[i64], vs: &[Option<PhysicalValue>], start: i64, end: i64, unit: TimeUnit) -> Option<(Vec<i64>, Vec<i64>, u8)> {
	let per_unit: i64 = unit.nanos_per_unit();
	let (mut nanos, mut mantissas) = (Vec::with_capacity(ts.len()), Vec::with_capacity(ts.len()));
	let mut shared_scale: Option<u8> = None;
	for (&t, v) in ts.iter().zip(vs) {
		let Some(value) = v else { continue };
		if t < start || t > end {
			continue;
		}
		let PhysicalValue::ScaledI64 { mantissa, scale } = *value else { return None };
		if *shared_scale.get_or_insert(scale) != scale {
			return None;
		}
		nanos.push(t.checked_mul(per_unit)?);
		mantissas.push(mantissa);
	}
	Some((nanos, mantissas, shared_scale.unwrap_or(0)))
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
	// Read the window in its physical encoding, so a ScaledI64 column can be reduced on its
	// stored mantissas without building a BigDecimal per row.
	let (ts, vs) = frame_read::read_range_physical(descriptor.path.as_str(), bytes, start, end)?;
	// Integer-native path: every present in-window value is ScaledI64 at one scale and every
	// requested reduction is a streaming one. `reduce_partial_scaled` then builds the same
	// mergeable partial `reduce_partial` would, from integer accumulators.
	if let Some((nanos, mantissas, scale)) = scaled_rows(&ts, &vs, start, end, unit) {
		if nanos.is_empty() {
			return Ok(None);
		}
		if let Some(partial) = weft_reduce::reduce_partial_scaled(&nanos, &mantissas, u32::from(scale), resolution, None, None, aggregations).map_err(|e| anyhow::anyhow!("reducing segment {}: {e}", descriptor.path))? {
			return Ok(Some(partial));
		}
	}
	// Null rows carry no value to reduce; present rows lift to absolute instants.
	let mut points: Vec<Point> = Vec::with_capacity(ts.len());
	for (t, v) in ts.into_iter().zip(vs) {
		if let Some(value) = v {
			if start <= t && t <= end {
				points.push(Point::new(instant_from_epoch(t, unit).ok_or_else(|| anyhow::anyhow!("segment {} holds epoch {t} outside the representable instant range", descriptor.path))?, value.to_logical()));
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
			Some(holder) => write!(f, "segment store root {root} is in use by {holder}: a root can be open in only one process, and only once in it; stop that process, or close its store, and retry"),
			None => write!(f, "segment store root {root} is in use by another process (its pid could not be read from {}): a root can be open in only one process, and only once in it", self.root.join(crate::types::durable::lock::HOLDER_FILE).display()),
		}
	}
}

impl std::error::Error for StoreLocked {}

/// The environment variable with which `weft-server` chooses what happens after an
/// ambiguous COMMIT.
///
/// Unset or `poison` leaves the store write-poisoned (design section 5.1) and serving
/// reads. `exit` makes the server log and exit with [`AMBIGUOUS_COMMIT_EXIT_CODE`] once
/// the store is poisoned, for deployments whose supervisor restarts it; the restart's
/// recovery then settles the commit. The library never reads it and never exits the
/// process (release plan C-1): a store only poisons itself, and an embedder that wants to
/// exit waits for that with [`SegmentStore::wait_until_poisoned`].
pub const AMBIGUOUS_COMMIT_ENV: &str = "WEFT_ON_AMBIGUOUS_COMMIT";

/// The status `weft-server` exits with once its store is poisoned under
/// `WEFT_ON_AMBIGUOUS_COMMIT=exit`: 70, `EX_SOFTWARE` in sysexits.h.
pub const AMBIGUOUS_COMMIT_EXIT_CODE: i32 = 70;

/// How a [`SegmentStore`] is configured: everything a deployment tunes, passed in rather
/// than read from the environment (release plan C-1).
///
/// The library reads no `WEFT_*` variable on its own: [`SegmentStore::open`] and
/// [`open_scoped`](SegmentStore::open_scoped) use [`default`](Self::default), which
/// writes byte-for-byte the frames WeftDB has always written. An embedder that wants the
/// environment's settings asks for them explicitly with [`from_env`](Self::from_env)
/// (what `weft-server` does) and opens with
/// [`open_with_options`](SegmentStore::open_with_options).
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct SegmentStoreOptions {
	/// Whether new seals carry a timestamp checkpoint index.
	pub checkpoints: CheckpointPolicy,
	/// Whether new seals materialize per-segment partial-reduction sidecars.
	pub partials: PartialSidecarPolicy,
	/// Whether new seals write the bit-sliced value codec.
	pub transposed: TransposedPolicy,
	/// The per-aspect circuit breaker store-wide maintenance sweeps run under
	/// (ROB-19). Unlike the seal policies above this one is **on** by default
	/// ([`MaintenanceBreaker::default`]): decision 17's scheduler is on by
	/// default, and the breaker is part of it.
	pub breaker: MaintenanceBreaker,
	/// The per-aspect decode breaker (ROB-22): consecutive decode failures on
	/// an aspect trip this breaker independently of the maintenance breaker, so
	/// a decode crash loop on a damaged frame stops the retries without affecting
	/// the maintenance breaker's sweep-level tripped-aspect list.
	pub decode_breaker: MaintenanceBreaker,
	/// The most series one aspect may register (TAG-7):
	/// [`DEFAULT_MAX_SERIES_PER_ASPECT`] unless changed. An ingest that would register
	/// one more is refused with [`StoreError::SeriesLimit`](crate::StoreError::SeriesLimit);
	/// series already registered stay writable. Series 0, the empty tag set, is implicit
	/// and does not count.
	pub max_series_per_aspect: usize,
	/// The most buckets one stored downsample may produce (ROB-13, 1.0 plan
	/// decision 12): [`DEFAULT_MAX_DOWNSAMPLE_BUCKETS`] unless changed. A
	/// downsample whose fold reaches one bucket more is refused with
	/// [`StoreError::DownsampleTooLarge`](crate::StoreError::DownsampleTooLarge),
	/// so the partial state a caller can make the store hold is bounded here,
	/// for every caller of the library — the HTTP handlers' own ceilings
	/// (ROB-12) refuse the request earlier where they can name its grid.
	pub max_downsample_buckets: usize,
	/// The most instants one batch point lookup may name (ROB-13, 1.0 plan
	/// decision 12's `at-multi` number): [`DEFAULT_MAX_POINT_BATCH`] unless
	/// changed. A wider batch is refused with
	/// [`StoreError::PointBatchTooLarge`](crate::StoreError::PointBatchTooLarge)
	/// before the index is pruned. The single point lookup names one instant
	/// and is bounded by construction.
	pub max_point_batch: usize,
	/// Whether range reads ([`SegmentStore::read_time_range`], and so the interpolation
	/// read path) memory-map their `.weftseg` frames instead of reading them into a heap
	/// buffer (roadmap Phase 5, memory-mapped GPU I/O). Off by default, and only honoured
	/// in a build with the `mmap-reads` cargo feature; without it the reads stay buffered.
	/// The rows returned are the same either way. See the `mapped` module for the safety
	/// argument and its one residual risk (a file truncated by another process while mapped).
	pub mmap_reads: bool,
	/// The bytes every seal must leave available on the store's filesystem (ROB-17,
	/// [`DiskGuard`](crate::DiskGuard)): an ingest or seal that would leave less is
	/// refused with [`StoreError::InsufficientStorage`](crate::StoreError::InsufficientStorage)
	/// before any frame is written. `0`, the library default, turns the guard off;
	/// [`from_env`](Self::from_env) reads `WEFT_DISK_RESERVE_BYTES` and falls back to
	/// [`DEFAULT_DISK_RESERVE_BYTES`](crate::DEFAULT_DISK_RESERVE_BYTES).
	pub disk_reserve_bytes: u64,
}

/// The per-aspect series cap `WEFT_MAX_SERIES_PER_ASPECT`'s value names: a positive
/// whole number, or [`DEFAULT_MAX_SERIES_PER_ASPECT`] when it is unset or anything
/// else (the server's strict configuration check refuses a malformed value at
/// startup, so this leniency only reaches library embedders).
fn max_series_per_aspect_from(value: Option<&str>) -> usize {
	value.and_then(|v| v.trim().parse::<usize>().ok()).filter(|limit| *limit > 0).unwrap_or(DEFAULT_MAX_SERIES_PER_ASPECT)
}

/// The default per-aspect series cap, [`SegmentStoreOptions::max_series_per_aspect`]:
/// 100,000 series.
pub const DEFAULT_MAX_SERIES_PER_ASPECT: usize = 100_000;

/// The default stored-downsample bucket cap,
/// [`SegmentStoreOptions::max_downsample_buckets`]: 1,000,000 buckets
/// (1.0 plan decision 12).
pub const DEFAULT_MAX_DOWNSAMPLE_BUCKETS: usize = 1_000_000;

/// The default batch point-lookup cap, [`SegmentStoreOptions::max_point_batch`]:
/// 10,000 instants (1.0 plan decision 12).
pub const DEFAULT_MAX_POINT_BATCH: usize = 10_000;

impl Default for SegmentStoreOptions {
	fn default() -> Self {
		Self { checkpoints: CheckpointPolicy::DISABLED, partials: PartialSidecarPolicy::DISABLED, transposed: TransposedPolicy::DISABLED, breaker: MaintenanceBreaker::default(), decode_breaker: MaintenanceBreaker::default(), max_series_per_aspect: DEFAULT_MAX_SERIES_PER_ASPECT, max_downsample_buckets: DEFAULT_MAX_DOWNSAMPLE_BUCKETS, max_point_batch: DEFAULT_MAX_POINT_BATCH, mmap_reads: false, disk_reserve_bytes: 0 }
	}
}

impl SegmentStoreOptions {
	/// The options the environment describes: [`CheckpointPolicy::from_env`],
	/// [`PartialSidecarPolicy::from_env`], [`TransposedPolicy::from_env`],
	/// [`MaintenanceBreaker::from_env`], `WEFT_MAX_SERIES_PER_ASPECT`
	/// ([`max_series_per_aspect`](Self::max_series_per_aspect); absent or not a
	/// positive whole number → [`DEFAULT_MAX_SERIES_PER_ASPECT`]) and
	/// `WEFT_SEGMENT_MMAP_READS` ([`mmap_reads`](Self::mmap_reads); on for
	/// `1`/`true`/`on`/`yes`, any case, off otherwise) and
	/// `WEFT_DISK_RESERVE_BYTES` ([`disk_reserve_bytes`](Self::disk_reserve_bytes);
	/// a non-negative whole number, else
	/// [`DEFAULT_DISK_RESERVE_BYTES`](crate::DEFAULT_DISK_RESERVE_BYTES)), each as lenient
	/// as it documents. Explicit: nothing in the library calls it.
	#[must_use]
	pub fn from_env() -> Self {
		Self { checkpoints: CheckpointPolicy::from_env(), partials: PartialSidecarPolicy::from_env(), transposed: TransposedPolicy::from_env(), breaker: MaintenanceBreaker::from_env(), decode_breaker: MaintenanceBreaker::decode_from_env(), max_series_per_aspect: max_series_per_aspect_from(std::env::var("WEFT_MAX_SERIES_PER_ASPECT").ok().as_deref()), max_downsample_buckets: DEFAULT_MAX_DOWNSAMPLE_BUCKETS, max_point_batch: DEFAULT_MAX_POINT_BATCH, mmap_reads: mapped::mmap_reads_from(std::env::var("WEFT_SEGMENT_MMAP_READS").ok().as_deref()), disk_reserve_bytes: crate::disk_guard::disk_reserve_bytes_from(std::env::var("WEFT_DISK_RESERVE_BYTES").ok().as_deref()) }
	}

	/// These options with `policy` for checkpoints.
	#[must_use]
	pub const fn with_checkpoints(mut self, policy: CheckpointPolicy) -> Self {
		self.checkpoints = policy;
		self
	}

	/// These options with `policy` for partial sidecars.
	#[must_use]
	pub const fn with_partials(mut self, policy: PartialSidecarPolicy) -> Self {
		self.partials = policy;
		self
	}

	/// These options with `policy` for the bit-sliced value codec.
	#[must_use]
	pub const fn with_transposed(mut self, policy: TransposedPolicy) -> Self {
		self.transposed = policy;
		self
	}

	/// These options with `breaker` for the maintenance circuit breaker.
	#[must_use]
	pub const fn with_breaker(mut self, breaker: MaintenanceBreaker) -> Self {
		self.breaker = breaker;
		self
	}

	/// These options with `limit` as the per-aspect series cap (see
	/// [`max_series_per_aspect`](Self::max_series_per_aspect)).
	#[must_use]
	pub const fn with_max_series_per_aspect(mut self, limit: usize) -> Self {
		self.max_series_per_aspect = limit;
		self
	}

	/// These options with `limit` as the stored-downsample bucket cap (see
	/// [`max_downsample_buckets`](Self::max_downsample_buckets)).
	#[must_use]
	pub const fn with_max_downsample_buckets(mut self, limit: usize) -> Self {
		self.max_downsample_buckets = limit;
		self
	}

	/// These options with `limit` as the batch point-lookup cap (see
	/// [`max_point_batch`](Self::max_point_batch)).
	#[must_use]
	pub const fn with_max_point_batch(mut self, limit: usize) -> Self {
		self.max_point_batch = limit;
		self
	}

	/// These options with memory-mapped range reads on or off (see
	/// [`mmap_reads`](Self::mmap_reads)).
	#[must_use]
	pub const fn with_mmap_reads(mut self, on: bool) -> Self {
		self.mmap_reads = on;
		self
	}

	/// These options with `bytes` as the disk guard's reserve (see
	/// [`disk_reserve_bytes`](Self::disk_reserve_bytes); `0` turns the guard off).
	#[must_use]
	pub const fn with_disk_reserve_bytes(mut self, bytes: u64) -> Self {
		self.disk_reserve_bytes = bytes;
		self
	}
}

/// What a [`SegmentStore`]'s open found and did (release plan C-5).
///
/// For the embedder to report: whether it created the store, whether and from which
/// layout it migrated it, and whether recovery must stay report-only.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OpenReport {
	/// This open created the store: no control-plane database existed, or the marker
	/// was still the one a new store starts with (an open creating the store stopped
	/// before it finished, and this one finished it).
	pub created_new: bool,
	/// The layout an existing store was in before this open, when this open moved it to
	/// a newer one (1 for a store a pre-marker WeftDB wrote).
	pub migrated_from: Option<u32>,
	/// The migrations this open applied, in order.
	pub applied: Vec<String>,
	/// The store's layout is newer than this build's ([`SUPPORTED_LAYOUT`]) but its write
	/// floor lets this build write it. The open ran no migrations, and recovery (S12) may
	/// only report what it finds: it must not quarantine, adopt or purge anything a newer
	/// WeftDB might understand.
	pub recovery_report_only: bool,
}

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

/// Consecutive failed store-wide sweep passes on one aspect after which that
/// aspect's maintenance circuit breaker trips (ROB-19).
pub const DEFAULT_MAINTENANCE_BREAKER_FAILURES: u32 = 3;

/// How long a tripped maintenance circuit breaker leaves its aspect alone before
/// a store-wide sweep may try it once more (ROB-19).
pub const DEFAULT_MAINTENANCE_BREAKER_COOLDOWN: Duration = Duration::from_secs(300);

/// Consecutive decode failures for an aspect after which the decode breaker trips
/// (ROB-22). Same threshold as the maintenance breaker (3): a single transient I/O
/// blip must not skip a whole tick's maintenance, but two more attempts at the same
/// damaged frame should stop the retry loop without waiting for the maintenance
/// breaker's cooldown.
pub const DEFAULT_DECODE_BREAKER_FAILURES: u32 = 3;

/// How long a tripped decode breaker leaves its aspect alone before a background
/// pass may try a decode once more (ROB-22).
pub const DEFAULT_DECODE_BREAKER_COOLDOWN: Duration = Duration::from_secs(300);

/// The per-aspect **circuit breaker** for store-wide maintenance sweeps (ROB-19,
/// release plan decision 17's maintenance scheduler).
///
/// A sweep already isolates a failing aspect — it records the failure and
/// maintains the rest — but without a breaker it retries that aspect on *every*
/// sweep: a permanently damaged frame is re-read and re-failed on every daemon
/// tick forever, paying the decode cost and a failure log line each time. The
/// breaker counts an aspect's **consecutive** failed sweep passes; at
/// [`failure_threshold`](Self::failure_threshold) the breaker **trips** and
/// sweeps skip the aspect (listing it in the sweep's `tripped` field) until
/// [`cooldown`](Self::cooldown) has passed. The first sweep after the cooldown
/// is a half-open trial: a pass that succeeds (or finds nothing to do) closes
/// the breaker and clears the count; a pass that fails trips it again for a
/// fresh cooldown. Any successful pass resets the consecutive count, so an
/// aspect that fails only intermittently never trips. A busy aspect is neither
/// a failure nor a success — another operation is maintaining it, so its count
/// is left alone. Per-aspect entry points (the operator's `reconcile`,
/// `squash`, `compact` endpoints) are never gated: the breaker governs only the
/// store-wide sweeps the scheduler runs, and an operator can always work on a
/// tripped aspect directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceBreaker {
	/// Consecutive failed sweep passes that trip the breaker; `0` disables the
	/// breaker (sweeps attempt every aspect on every pass, as before ROB-19).
	pub failure_threshold: u32,
	/// How long a tripped breaker skips its aspect before allowing one trial pass.
	pub cooldown: Duration,
}

impl Default for MaintenanceBreaker {
	fn default() -> Self {
		Self { failure_threshold: DEFAULT_MAINTENANCE_BREAKER_FAILURES, cooldown: DEFAULT_MAINTENANCE_BREAKER_COOLDOWN }
	}
}

impl MaintenanceBreaker {
	/// The breaker the environment describes: `WEFT_MAINTENANCE_BREAKER_FAILURES`
	/// (absent or unparseable → [`DEFAULT_MAINTENANCE_BREAKER_FAILURES`]; `0`
	/// disables the breaker) and `WEFT_MAINTENANCE_BREAKER_COOLDOWN_SECS` (absent
	/// or unparseable → [`DEFAULT_MAINTENANCE_BREAKER_COOLDOWN`]). Lenient, like
	/// the other policies: nothing in the library calls it — `weft-server` does,
	/// through [`SegmentStoreOptions::from_env`].
	#[must_use]
	pub fn from_env() -> Self {
		let failure_threshold = std::env::var("WEFT_MAINTENANCE_BREAKER_FAILURES").ok().and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(DEFAULT_MAINTENANCE_BREAKER_FAILURES);
		let cooldown = std::env::var("WEFT_MAINTENANCE_BREAKER_COOLDOWN_SECS").ok().and_then(|v| v.trim().parse::<u64>().ok()).map_or(DEFAULT_MAINTENANCE_BREAKER_COOLDOWN, Duration::from_secs);
		Self { failure_threshold, cooldown }
	}

	/// The decode breaker the environment describes (ROB-22):
	/// `WEFT_DECODE_BREAKER_FAILURES` and `WEFT_DECODE_BREAKER_COOLDOWN_SECS`.
	/// Same fallback behaviour as [`from_env`](Self::from_env).
	#[must_use]
	pub fn decode_from_env() -> Self {
		let failure_threshold = std::env::var("WEFT_DECODE_BREAKER_FAILURES").ok().and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(DEFAULT_DECODE_BREAKER_FAILURES);
		let cooldown = std::env::var("WEFT_DECODE_BREAKER_COOLDOWN_SECS").ok().and_then(|v| v.trim().parse::<u64>().ok()).map_or(DEFAULT_DECODE_BREAKER_COOLDOWN, Duration::from_secs);
		Self { failure_threshold, cooldown }
	}
}

/// One aspect's circuit-breaker state, held in the store's breaker map.
#[derive(Debug, Default)]
struct AspectBreakerState {
	/// Failed sweep passes since the last successful one.
	consecutive_failures: u32,
	/// While this instant is in the future the breaker is open and sweeps skip
	/// the aspect; once it passes, the next sweep's pass is the half-open trial.
	open_until: Option<Instant>,
}

/// A store's write poison: set once, by the first ambiguous COMMIT, and never cleared.
#[derive(Debug)]
struct StorePoison {
	/// Checked on every write, so it is the one thing a write reads.
	poisoned: AtomicBool,
	/// Why, from the first poisoning; later ones keep it.
	reason: OnceLock<String>,
	/// Tells watchers once the store is poisoned.
	tx: tokio::sync::watch::Sender<Option<Poisoned>>,
}

impl Default for StorePoison {
	fn default() -> Self {
		Self { poisoned: AtomicBool::new(false), reason: OnceLock::new(), tx: tokio::sync::watch::Sender::new(None) }
	}
}

impl StorePoison {
	/// Poison the store for `reason`. Returns whether this call poisoned it (the first).
	fn set(&self, reason: String) -> bool {
		let first = self.reason.set(reason).is_ok();
		self.poisoned.store(true, Ordering::Release);
		if first {
			self.tx.send_replace(self.get());
		}
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
/// The frame lives at the same place below the current root's `segments/` as below the
/// root it was written under: `root/segments/` joined with the components after the last
/// component named `segments` (the last, so that a root which itself sits under a
/// `segments` directory resolves correctly). The stored path is split on both `/` and `\`
/// (release plan, D-S6), so a store written on Windows (`C:\…\segments\x-1.weftseg`)
/// reads on Unix and the other way round. So a root that was moved, restored into another
/// root, or mounted somewhere else still reads its own frames, and one that was not reads
/// them where they always were.
///
/// # Errors
///
/// A path with no `segments` component, or whose tail is not plain names (a `..` that
/// would lead out of `segments/`, a drive or root prefix): there is nothing safe to
/// resolve it to, and the read is refused rather than made outside the store. A layout-1
/// store recording such a path is refused at its migration to layout 2
/// ([`StoreError::UnsafeLegacyPath`]); nothing is ever quarantined for it.
fn resolve_frame_path(root: &Path, stored: &str) -> Result<PathBuf> {
	let parts: Vec<&str> = stored.split(['/', '\\']).filter(|part| !part.is_empty() && *part != ".").collect();
	let Some(last_segments) = parts.iter().rposition(|part| *part == "segments") else { bail!("refusing to read frame {stored:?}: its path has no segments/ component to resolve under {}", root.display()) };
	let tail = &parts[last_segments + 1..];
	let plain = |part: &&str| *part != ".." && matches!(Path::new(part).components().collect::<Vec<_>>().as_slice(), [Component::Normal(_)]);
	if tail.is_empty() || !tail.iter().all(plain) {
		bail!("refusing to read frame {stored:?}: its path leads out of segments/");
	}
	Ok(tail.iter().fold(root.join("segments"), |path, part| path.join(part)))
}

/// Refuse the outputs of `members` that would take fresh ids (rule P2 of the release
/// plan's D17), given the aspect's `rows`, when one of `rows` of the members' series with
/// an id above every member overlaps one of their spans `extra`: such an output takes an
/// id above every row, so at the timestamps they share it would outrank that newer row
/// (design section 5.3, M2; `split_segment`'s rule). Precedence only matters within a
/// series (a read targets one series, tags D4; TAG-6), so a newer row of another series
/// never refuses. Rows without a time range, and outputs without one, overlap nothing.
fn refuse_outranking_outputs(aspect: &str, members: &[IndexRow], extra: impl IntoIterator<Item = Option<(i64, i64)>>, rows: &[IndexRow]) -> Result<()> {
	let Some(newest_member) = members.iter().map(|member| member.desc.id).max() else { return Ok(()) };
	let series_id = members.first().map_or(0, |member| member.series_id);
	for (lo, hi) in extra.into_iter().flatten() {
		if let Some(newer) = rows.iter().find(|row| row.series_id == series_id && row.desc.id > newest_member && row.desc.time_range().is_some_and(|(min, max)| min <= hi && lo <= max)) {
			let ids: Vec<u64> = members.iter().map(|member| member.desc.id).collect();
			bail!("refusing to swap segment(s) {ids:?} of aspect {aspect:?}: segment {} is newer and overlaps [{lo}, {hi}], where an output under a new id would outrank it", newer.desc.id);
		}
	}
	Ok(())
}

/// The segment-index transaction error in `e`'s chain, if a transaction is what failed.
fn txn_error(e: &anyhow::Error) -> Option<&IndexTxnError> {
	e.chain().find_map(|cause| cause.downcast_ref::<IndexTxnError>())
}

/// Whether `e` is a segment-index transaction's failed precondition.
fn is_conflict(e: &anyhow::Error) -> bool {
	txn_error(e).is_some_and(|e| e.kind == TxnErrorKind::Conflict)
}

/// Whether `e` is a segment-index transaction that may have committed although it failed
/// (which poisoned the store).
fn is_ambiguous(e: &anyhow::Error) -> bool {
	txn_error(e).is_some_and(IndexTxnError::is_ambiguous)
}

/// Wrap a failed seal so the [`anyhow::Error`] reads exactly as it always did **and** still
/// carries the typed [`SealError`](weft_physical_type::SealError) in its chain.
///
/// The old form was `anyhow!("seal failed: {e}")`, which flattened the typed error into a
/// string and left the HTTP layer to re-recognise the failure by substring. `anyhow`'s
/// `Display` prints only the outermost context, so the same text goes into the context and
/// the typed error stays underneath it as the source: the message a log or a CLI prints is
/// unchanged, and `err.downcast_ref::<SealError>()` now finds it (X-ERR's classifier).
fn seal_failed(error: weft_physical_type::SealError) -> anyhow::Error {
	let shown = error.to_string();
	anyhow::Error::from(error).context(format!("seal failed: {shown}"))
}

/// [`seal_failed`] for the paged seals, whose message has always said `paged seal failed`.
fn paged_seal_failed(error: weft_physical_type::SealError) -> anyhow::Error {
	let shown = error.to_string();
	anyhow::Error::from(error).context(format!("paged seal failed: {shown}"))
}

/// One encoded ingest chunk, in whichever frame shape the batch asked
/// for (D-S14): a single-block [`Segment`], or a [`PagedSegment`] when the
/// batch carries `rows_per_page`. The two shapes share everything the
/// ingest commit needs — stats, frame bytes under a [`FrameOptions`], and
/// a descriptor — so the encode/write/commit path below is written once
/// over this sum rather than once per shape.
enum IngestSegment {
	/// A single-block frame (the shape every pre-D-S14 ingest sealed).
	Single(Segment),
	/// A paged frame, sealed at the batch's page height.
	Paged(PagedSegment),
}

impl IngestSegment {
	/// The segment's row count (present and null rows).
	const fn row_count(&self) -> usize {
		match self {
			Self::Single(segment) => segment.stats.row_count,
			Self::Paged(segment) => segment.stats.row_count,
		}
	}

	/// Whether the segment's shape benefits from timestamp checkpoints.
	fn benefits_from_checkpoints(&self) -> bool {
		match self {
			Self::Single(segment) => segment.benefits_from_checkpoints(),
			Self::Paged(segment) => segment.benefits_from_checkpoints(),
		}
	}

	/// The segment's checkpoint codec overhead estimate.
	fn checkpoint_codec_overhead(&self) -> f64 {
		match self {
			Self::Single(segment) => segment.checkpoint_codec_overhead(),
			Self::Paged(segment) => segment.checkpoint_codec_overhead(),
		}
	}

	/// The frame bytes under `options`.
	fn write_to_with(&self, options: &FrameOptions) -> Vec<u8> {
		match self {
			Self::Single(segment) => segment.write_to_with(options),
			Self::Paged(segment) => segment.write_to_with(options),
		}
	}

	/// The descriptor for this frame at `path`, with id 0 (the commit
	/// assigns the real id).
	fn descriptor(&self, path: String, byte_len: u64) -> SegmentDescriptor {
		match self {
			Self::Single(segment) => SegmentDescriptor::of_segment(0, path, byte_len, segment),
			Self::Paged(segment) => SegmentDescriptor::of_paged_segment(0, path, byte_len, segment),
		}
	}
}

/// The most bytes a series-key envelope ([`tag_frame`]) adds to a frame carrying `key`:
/// the envelope's 16 bytes (a 12-byte header and a 4-byte CRC trailer) plus one
/// extension entry, charged as the envelope writer sizes it (6 bytes of id, flag and
/// length prefix) plus the key itself. A canonical key is at most
/// [`MAX_SERIES_KEY_BYTES`](weft_physical_type::tags::MAX_SERIES_KEY_BYTES) long, so this
/// is at most 1,046 bytes.
pub(crate) fn series_envelope_overhead(key: &crate::SeriesKey) -> u64 {
	(16 + 6_u64).saturating_add(u64::try_from(key.as_bytes().len()).unwrap_or(u64::MAX))
}

/// The encoded-frame target an ingest of `groups` cuts its rows to:
/// [`MAX_ENCODED_FRAME_BYTES`](crate::MAX_ENCODED_FRAME_BYTES) for an untagged batch,
/// and that less [`series_envelope_overhead`] for a tagged one, so that the frame on
/// disk, envelope included, still fits the target. Only tagged batches pay it.
pub(crate) fn ingest_frame_target(groups: &crate::TagSet) -> u64 {
	if groups.is_empty() {
		crate::MAX_ENCODED_FRAME_BYTES
	} else {
		crate::MAX_ENCODED_FRAME_BYTES.saturating_sub(series_envelope_overhead(groups.series_key()))
	}
}

/// Wrap the complete frame `bytes` for series `key` (tags D3): `None` (series 0, the
/// empty tag set) returns the bytes untouched, so an untagged frame is byte-identical to
/// one sealed before tags; `Some` wraps them in a v7 envelope with one
/// [`EXT_SERIES_KEY`](weft_physical_type::EXT_SERIES_KEY) extension, `critical: true`,
/// whose payload is the canonical key. The frame records the key, never the id: an id
/// means nothing outside its store, while the key can be bound again after a restore.
///
/// The envelope ends in its own CRC, so the trailer the durable write reads as
/// `frame_crc`, and the `byte_len` a row records, are the wrapped frame's.
///
/// # Errors
///
/// The envelope writer's refusal of a frame it cannot read, which a frame this build
/// just encoded never is.
pub(crate) fn tag_frame(bytes: Vec<u8>, key: Option<&crate::SeriesKey>) -> Result<Vec<u8>> {
	match key {
		Some(key) if !key.is_empty() => {
			let extension = weft_physical_type::FrameExtension { id: weft_physical_type::EXT_SERIES_KEY, critical: true, payload: key.as_bytes().to_vec() };
			weft_physical_type::wrap_frame_envelope(&bytes, &[extension]).map_err(|e| anyhow::anyhow!("wrapping a frame in its series-key envelope failed: {e}"))
		}
		_ => Ok(bytes),
	}
}

/// Encode one ingest chunk under `schema`, in the batch's frame shape:
/// paged when `rows_per_page` is set, single-block otherwise. The error
/// is the shape's seal failure, worded as the direct seal paths word it.
fn encode_ingest_chunk(schema: &AspectSchema, timestamps: &[i64], values: &[Option<BigDecimal>], rows_per_page: Option<usize>) -> Result<IngestSegment> {
	rows_per_page.map_or_else(|| schema.seal_nullable(timestamps, values).map(IngestSegment::Single).map_err(seal_failed), |rows_per_page| schema.seal_paged_nullable(timestamps, values, rows_per_page).map(IngestSegment::Paged).map_err(paged_seal_failed))
}

/// Cut an ingest batch's rows into chunks of at most `target` bytes of encoded
/// frame ([`ingest_frame_target`]: [`MAX_ENCODED_FRAME_BYTES`](crate::MAX_ENCODED_FRAME_BYTES),
/// less the series-key envelope for a tagged batch), by a conservative per-row
/// estimate (D-S13): each row is charged
/// 24 bytes plus its value's bounded plain-form length (at most
/// [`MAX_PLAIN_DIGITS`](weft_physical_type::MAX_PLAIN_DIGITS) digits, so
/// the estimate can be computed arithmetically, without materialising the
/// plain form). A cut never divides a run of equal timestamps: the chunk
/// holding a run holds all of it, so a chunk may exceed the estimate —
/// the caller proves each chunk's real frame against the target, and a
/// single timestamp run is the smallest cut there is.
fn chunk_ingest_rows(timestamps: &[i64], values: &[Option<BigDecimal>], target: u64) -> Vec<(Vec<i64>, Vec<Option<BigDecimal>>)> {
	let target = usize::try_from(target).unwrap_or(usize::MAX);
	let mut chunks: Vec<(Vec<i64>, Vec<Option<BigDecimal>>)> = Vec::new();
	let mut start = 0usize;
	while start < timestamps.len() {
		let mut end = start;
		let mut estimate = 0usize;
		while end < timestamps.len() {
			let row_cost = 24 + values[end].as_ref().map_or(0, weft_physical_type::plain_digit_count);
			// A cut is allowed only at a timestamp boundary: the next row
			// starts a new timestamp. A run of equal timestamps is charged
			// row by row like everything else, but never divided — so a
			// run that alone exceeds the target stays whole.
			if end > start && estimate.saturating_add(row_cost) > target && timestamps[end] != timestamps[end - 1] {
				break;
			}
			estimate = estimate.saturating_add(row_cost);
			end += 1;
		}
		chunks.push((timestamps[start..end].to_vec(), values[start..end].to_vec()));
		start = end;
	}
	chunks
}

/// The span of `timestamps`, or `None` when it is empty.
fn span_of(timestamps: &[i64]) -> Option<(i64, i64)> {
	let lo = timestamps.iter().min()?;
	let hi = timestamps.iter().max()?;
	Some((*lo, *hi))
}

/// `rows` partitioned by series (TAG-6): one run per series id present, in ascending
/// series order, each keeping the order `rows` had. An untagged aspect is one run.
fn by_series(rows: Vec<IndexRow>) -> Vec<Vec<IndexRow>> {
	let mut series: std::collections::BTreeMap<u64, Vec<IndexRow>> = std::collections::BTreeMap::new();
	for row in rows {
		series.entry(row.series_id).or_default().push(row);
	}
	series.into_values().collect()
}

/// The aspect's [`overlapping_count`](weft_physical_type::SegmentIndex::overlapping_count)
/// per series (TAG-6): the sum over one [`SegmentIndex`](weft_physical_type::SegmentIndex)
/// per series of `rows`, so two series sharing a window do not count as overlap (a read
/// targets one series, tags D4). An untagged aspect counts exactly as its whole index.
fn series_overlapping_count(rows: &[IndexRow]) -> usize {
	let mut series: std::collections::BTreeMap<u64, weft_physical_type::SegmentIndex> = std::collections::BTreeMap::new();
	for row in rows {
		series.entry(row.series_id).or_default().push(row.desc.clone());
	}
	series.values().map(weft_physical_type::SegmentIndex::overlapping_count).sum()
}

/// The connected components of `rows` that overlap in time, transitively, within each
/// series (TAG-6: rows are partitioned by `series_id` first, so a component never mixes
/// series, invariant I12): per series the spans are swept in `(min_ts, max_ts)` order,
/// and a new component starts whenever a span begins after the running end of the
/// current one. So no row of the series outside a component overlaps its span (the
/// closure property design section 5.3's M2 relies on, per series: a read targets one
/// series, tags D4). A row without a time range is in none.
fn overlap_components(rows: Vec<IndexRow>) -> Vec<Vec<IndexRow>> {
	by_series(rows).into_iter().flat_map(series_overlap_components).collect()
}

/// [`overlap_components`] of the rows of one series.
fn series_overlap_components(rows: Vec<IndexRow>) -> Vec<Vec<IndexRow>> {
	let mut spanned: Vec<(i64, i64, IndexRow)> = rows.into_iter().filter_map(|row| row.desc.time_range().map(|(lo, hi)| (lo, hi, row))).collect();
	spanned.sort_by_key(|(lo, hi, _)| (*lo, *hi));
	let mut components: Vec<Vec<IndexRow>> = Vec::new();
	let mut running_max_hi = i64::MIN;
	for (lo, hi, row) in spanned {
		match components.last_mut() {
			Some(component) if lo <= running_max_hi => {
				component.push(row);
				running_max_hi = running_max_hi.max(hi);
			}
			_ => {
				components.push(vec![row]);
				running_max_hi = hi;
			}
		}
	}
	components
}

/// Fold the rows of `frames`, the members of a merge in ascending id order (oldest to
/// newest), into one time-sorted run, the newest value winning at a timestamp two members
/// share: each member's rows are sorted stably first (it may be out of order), then merged
/// with [`merge_newer_wins`], which supersedes the older member's whole run of a shared
/// timestamp and keeps a run unique to one member as it is.
fn fold_newest_wins(frames: Vec<DecodedFrame>) -> (Vec<i64>, Vec<Option<BigDecimal>>) {
	let mut merged: Vec<(i64, Option<BigDecimal>)> = Vec::new();
	for frame in frames {
		let mut rows: Vec<(i64, Option<BigDecimal>)> = frame.timestamps.into_iter().zip(frame.values).collect();
		rows.sort_by_key(|(t, _)| *t);
		merged = merge_newer_wins(&merged, &rows);
	}
	merged.into_iter().unzip()
}

/// Whether two runs of rows are the same, row for row: equal timestamps, and values equal
/// in their **plain form**, scale included (`-3` and `-3.0` differ), nulls equal to nulls.
/// `BigDecimal`'s own `==` ignores scale, and the store keeps the scale a value was
/// written with, so a numeric comparison could keep a frame that reads back different
/// text from the fold (Phase 4.6 skip-identical, critique F23).
fn same_rows(held_timestamps: &[i64], held_values: &[Option<BigDecimal>], offered_timestamps: &[i64], offered_values: &[Option<BigDecimal>]) -> bool {
	held_timestamps == offered_timestamps
		&& held_values.len() == offered_values.len()
		&& held_values.iter().zip(offered_values).all(|(left, right)| match (left, right) {
			(None, None) => true,
			// Same digits at the same scale is the same plain form; otherwise the plain forms
			// decide (`1E+2` and `100` read back alike).
			(Some(left), Some(right)) => left.as_bigint_and_scale() == right.as_bigint_and_scale() || left.to_plain_string() == right.to_plain_string(),
			_ => false,
		})
}

/// Rows per page of a maintenance merge's output once it holds more than one page.
///
/// A merge writes outputs far larger than the seals it folds (a squash to 65,536-row
/// segments over 1,024-row seals), and a single-block frame decodes its value column from
/// the start, so after a squash an hour's read decoded up to a whole segment: 0.19 →
/// 0.50 ms on 10M UCI rows (#507). A paged frame skips the pages outside the window. At
/// 4,096 rows an hour read 0.24 ms before the squash and 0.24 ms after, and the year window
/// got faster (17.8 → 14.1 ms), for 1.341 → 1.357 B/pt, still below the 1.414 of the
/// 1,024-row seals it replaced. 1,024-row pages cost 1.404 B/pt for no faster read, and
/// 16,384-row pages left an hour 1.3× slower.
pub(crate) const MERGE_ROWS_PER_PAGE: usize = 4_096;

/// The frame kind of a maintenance merge's output of `rows` rows: paged at
/// [`MERGE_ROWS_PER_PAGE`] once it holds more than one page, single-block below that,
/// where there is nothing to skip.
pub(crate) const fn merge_rows_per_page(rows: usize) -> Option<usize> {
	if rows > MERGE_ROWS_PER_PAGE {
		Some(MERGE_ROWS_PER_PAGE)
	} else {
		None
	}
}

/// How many independent plans one swap transaction may carry: the default of
/// `WEFT_MAINT_PLANS_PER_TXN` (tags amendment A5). Every caller today swaps one plan at a
/// time, so per-series maintenance (TAG-6) loops one series at a time and never nears it.
const MAX_PLANS_PER_TXN: usize = 32;

/// The rows of one output a swap plans, in time order, and the frame kind it is written
/// in (`rows_per_page` for a paged frame).
struct PlanOutput {
	timestamps: Vec<i64>,
	values: Vec<Option<BigDecimal>>,
	rows_per_page: Option<usize>,
}

/// One maintenance swap (design section 5.3, M2): the input rows it replaces, as M1 read
/// them, ascending by id, and the outputs that replace them, ascending and disjoint in
/// time.
///
/// **Precedence** (release plan D17, without a change to the `segment_index` key). Output
/// `j` in time order takes the `j`-th smallest member id (P1), with a new generation; the
/// members left over are deleted. When there are more outputs than members (P2: a split of
/// one segment), each extra output takes a fresh id inside the swap transaction, and only
/// if no row with an id above every member overlaps its span; otherwise the swap is
/// refused. A merge's members are an overlap component (no other row overlaps their span)
/// or a run of consecutive ids, and a row committed after the snapshot has a higher id
/// (see the module documentation for the seals of the S7-S10 interim), so outputs under
/// member ids outrank exactly what their members did.
///
/// A **keep** plan (Phase 4.6 skip-identical, design 9b) writes no output at all: its
/// lowest-id member already holds exactly the rows the fold would write, so it stays as
/// it is, under an expected-version precondition, and only the other members are deleted
/// and retired.
pub(crate) struct SwapPlan {
	members: Vec<IndexRow>,
	outputs: Vec<PlanOutput>,
	keep: bool,
}

impl SwapPlan {
	/// A plan replacing `members` with the time-sorted rows `timestamps` and `values`, cut
	/// into one output per stretch between the timestamps `cuts` (an output starts at each
	/// cut), every output in the frame kind `rows_per_page`.
	///
	/// A cut is a timestamp, so it falls between two distinct timestamps and never inside
	/// a run of equal ones (release plan D17): a run split across two outputs would have
	/// its first part superseded by the higher-id output's at the next merge. Stretches
	/// with no rows are dropped, but a plan always has an output.
	fn cut(mut members: Vec<IndexRow>, timestamps: Vec<i64>, values: Vec<Option<BigDecimal>>, rows_per_page: Option<usize>, cuts: &[i64]) -> Self {
		members.sort_by_key(|member| member.desc.id);
		let mut cuts = cuts.to_vec();
		cuts.sort_unstable();
		cuts.dedup();
		let (mut timestamps, mut values) = (timestamps, values);
		let mut outputs = Vec::with_capacity(cuts.len() + 1);
		for &cut in cuts.iter().rev() {
			let at = timestamps.partition_point(|&t| t < cut);
			let tail = PlanOutput { timestamps: timestamps.split_off(at), values: values.split_off(at.min(values.len())), rows_per_page };
			if !tail.timestamps.is_empty() {
				outputs.push(tail);
			}
		}
		if !timestamps.is_empty() || outputs.is_empty() {
			outputs.push(PlanOutput { timestamps, values, rows_per_page });
		}
		outputs.reverse();
		Self { members, outputs, keep: false }
	}

	/// A delete-only plan over `members` (Phase 4.6 skip-identical): the lowest-id member
	/// is kept as it is, since its rows are already what a fold of every member gives, and
	/// the others are deleted. No frame is written.
	fn keep(mut members: Vec<IndexRow>) -> Self {
		members.sort_by_key(|member| member.desc.id);
		Self { members, outputs: Vec::new(), keep: true }
	}

	/// How many segments the swap removes from the aspect: its members less its outputs
	/// (the kept member counting as one).
	const fn removes(&self) -> usize {
		self.members.len().saturating_sub(self.outputs.len() + self.keep as usize)
	}

	/// The spans of the outputs that take fresh ids (P2), in time order.
	fn extra_spans(&self) -> impl Iterator<Item = Option<(i64, i64)>> + '_ {
		self.outputs.iter().skip(self.members.len()).map(|output| span_of(&output.timestamps))
	}
}

/// What one plan's swap committed: each output's id, in the outputs' time order.
pub(crate) struct Swapped {
	ids: Vec<u64>,
}

/// A plan on its way through a swap: its members and the frame names they retire, the
/// series and adoption order of its outputs, their fresh ids (P2) once allocated, and the
/// outputs once written.
pub(crate) struct PreparedPlan {
	members: Vec<IndexRow>,
	/// Whether this is a keep plan ([`SwapPlan::keep`]): no outputs, its first member kept.
	keep: bool,
	retired: Vec<String>,
	series_id: u64,
	prec: u64,
	fresh: Vec<u64>,
	outputs: Vec<WrittenOutput>,
}

impl PreparedPlan {
	/// The names of the plan's outputs.
	fn output_names(&self) -> Vec<String> {
		self.outputs.iter().map(|output| output.name.clone()).collect()
	}

	/// How many members keep their row: one per output, or the kept member of a keep
	/// plan. The rest are deleted.
	const fn taken(&self) -> usize {
		if self.keep {
			1
		} else {
			self.outputs.len()
		}
	}

	/// How many of the plan's outputs take fresh ids.
	const fn extra(&self) -> usize {
		self.outputs.len().saturating_sub(self.members.len())
	}

	/// The id each output takes, in time order: the members' ids in ascending order (P1),
	/// then the fresh ones (P2).
	fn output_ids(&self) -> Vec<u64> {
		if self.keep {
			return self.members.iter().take(1).map(|member| member.desc.id).collect();
		}
		self.members.iter().map(|member| member.desc.id).chain(self.fresh.iter().copied()).take(self.outputs.len()).collect()
	}

	/// The members no output takes the id of, which the swap deletes: their frame
	/// paths, whose sidecars a merge removes by frame name.
	fn deleted_frames(&self) -> Vec<String> {
		self.members.iter().skip(self.taken()).map(|member| member.desc.path.clone()).collect()
	}

	/// Step M5's ops for this plan, committing in aspect epoch `epoch` (design section 5.3):
	/// `ReplaceExpected` on the member whose id each output takes (against the version M1
	/// read) or `InsertNew` under a fresh id, one `DeleteEveryExpected` for the members left over,
	/// `JournalRetire` for every retired member's frame, and `ClearJournal` for every output's
	/// pending row. A keep plan has no outputs: its kept member gets an unchanged
	/// `ReplaceExpected` instead, a pure precondition, and is not retired.
	fn ops(&self, aspect: &str, epoch: u64, created_ms: i64, ops: &mut Vec<IndexOp>) {
		if self.keep {
			// The kept member is rewritten as itself: the op changes nothing, but it is a
			// precondition on the version M1 read, so a racing change to it is a `Conflict`
			// and the other members are not deleted under a row that no longer folds them.
			if let Some(kept) = self.members.first() {
				ops.push(IndexOp::ReplaceExpected { aspect: aspect.to_string(), expected: kept.version(), row: kept.clone() });
			}
		}
		for (j, (output, id)) in self.outputs.iter().zip(self.output_ids()).enumerate() {
			let row = output.row(id, epoch, self.series_id);
			match self.members.get(j) {
				Some(member) => ops.push(IndexOp::ReplaceExpected { aspect: aspect.to_string(), expected: member.version(), row }),
				None => ops.push(IndexOp::InsertNew { aspect: aspect.to_string(), row }),
			}
		}
		let deleted: Vec<RowVersion> = self.members.iter().skip(self.taken()).map(IndexRow::version).collect();
		if !deleted.is_empty() {
			ops.push(IndexOp::DeleteEveryExpected { aspect: aspect.to_string(), expected: deleted });
		}
		for name in &self.retired {
			ops.push(IndexOp::JournalRetire { aspect: aspect.to_string(), name: name.clone(), retire_epoch: epoch, created_ms });
		}
		for output in &self.outputs {
			ops.push(IndexOp::ClearJournal { name: output.name.clone() });
		}
	}
}

/// The outputs of a swap that step M3 journaled as pending and that are neither swapped in
/// nor discarded yet: held from the pending commit until the swap's outcome settles each
/// one (release plan D-S9; the robustness track's `PendingOutputs`, ROB-21).
///
/// The swap settles them on every path it foresees: swapped in, discarded (unlinked,
/// `segments/` fsynced, pending rows deleted) when it is certain not to commit, or left to
/// the next open's journal replay after an ambiguous COMMIT. Whatever the guard still holds
/// when it drops (the operation panicked, or its future was dropped at an await between M3
/// and M5, a request cancelled say) goes to the reaper ([`Reaper::abandon`]), which runs
/// over the aspect at the start of its next swap, at the end of every swap and in
/// [`SegmentStore::reap`]; the open's replay (R6) is the backstop. The guard never unlinks
/// anything itself: a future dropped while its swap's COMMIT was in flight may have
/// committed, and only the reaper, which first reads which frames live rows reference,
/// tells a swapped-in output from an abandoned one.
#[must_use = "dropping the guard hands the outputs to the reaper"]
pub(crate) struct PendingOutputs<'a> {
	store: &'a SegmentStore,
	aspect: String,
	names: Vec<String>,
}

impl PendingOutputs<'_> {
	/// `names` were swapped in: their pending rows went with the swap.
	fn swapped(&mut self, names: &[String]) {
		self.names.retain(|name| !names.contains(name));
	}

	/// The swap's COMMIT was ambiguous, which poisoned the store: the next open's journal
	/// replay settles every output still held, whichever way the COMMIT went.
	fn left_for_replay(&mut self) {
		self.names.clear();
	}

	/// Discard `names` now, outputs of a swap certain not to commit them
	/// ([`discard_outputs`](SegmentStore::discard_outputs)). What could not be discarded
	/// stays held, for the reaper.
	async fn discard(&mut self, names: &[String]) {
		if self.store.discard_outputs(&self.aspect, names).await {
			self.swapped(names);
		}
	}

	/// Discard every output still held.
	async fn discard_all(&mut self) {
		let names = self.names.clone();
		self.discard(&names).await;
	}
}

impl Drop for PendingOutputs<'_> {
	fn drop(&mut self) {
		if self.names.is_empty() {
			return;
		}
		tracing::warn!(aspect = self.aspect, frames = ?self.names, "a maintenance operation stopped with outputs it had not swapped in; the reaper settles them");
		self.store.reaper.abandon(&self.aspect, std::mem::take(&mut self.names));
	}
}

/// The [`IndexOp::SeqBump`] that persists the allocator `commit` holds for `aspect`: past
/// every id handed out so far, and past every generation a frame this commit names, in
/// `epoch`. A seal raises the generations past its own frame (S10); a swap past its outputs'.
fn seq_bump(aspect: &str, commit: &CommitGuard, next_gen: u64, epoch: u64) -> IndexOp {
	let state = commit.state();
	IndexOp::SeqBump { aspect: aspect.to_string(), next_id: state.next_id, next_gen, epoch }
}

/// What [`SegmentStore::adopt_orphan`] did with one orphan frame.
enum Adoption {
	/// The frame's row committed under this aspect.
	Adopted(String),
	/// The frame's aspect has no declared schema in this store's scope.
	Undeclared(String),
	/// The frame's name names no aspect, or it holds no rows.
	Skipped,
}

/// The series key a frame's `SERIES_KEY` extension carries (TAG-9): the empty
/// key for a frame with no such extension (an untagged frame), otherwise its
/// payload decoded through [`SeriesKey::from_canonical`](crate::SeriesKey::from_canonical).
///
/// # Errors
///
/// The reason the frame cannot be bound, when it carries more than one
/// `SERIES_KEY` extension, an empty payload (an untagged frame has no envelope
/// at all), or a payload that is not a canonical series key.
fn frame_series_key(info: &weft_physical_type::FrameInfo) -> std::result::Result<crate::SeriesKey, String> {
	let mut payloads = info.extensions.iter().filter(|ext| ext.id == weft_physical_type::EXT_SERIES_KEY);
	let Some(first) = payloads.next() else {
		return Ok(crate::SeriesKey::default());
	};
	if payloads.next().is_some() {
		return Err("the frame carries more than one SERIES_KEY extension".to_string());
	}
	if first.payload.is_empty() {
		return Err("the SERIES_KEY extension is empty, but an untagged frame carries no envelope".to_string());
	}
	crate::SeriesKey::from_canonical(&first.payload).map_err(|error| format!("the SERIES_KEY payload is not a canonical series key: {error}"))
}

/// The aspect and id a legacy-named file carries: a frame `{aspect}-{id}.weftseg` or its
/// sidecar `{aspect}-{id}.weftpart`. The id is the digits after the last `-`, so an aspect
/// name that itself contains `-` still parses exactly. `None` for any other name.
pub(crate) fn legacy_file_id(name: &str) -> Option<(&str, u64)> {
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

/// The highest generation each write-once frame stem under `segments` carries
/// (see [`frame_name::parse`]): the floor of each aspect's generation counter, so that a
/// frame a crashed maintenance operation left behind is never named again. A quarantined
/// frame is still on disk, just relocated to `segments/quarantine/` by D-S12 recovery, so
/// that directory counts too: otherwise a moved stray's generation could be reissued.
async fn frame_gens_on_disk(fs: &dyn StoreFs, segments: &Path) -> Result<HashMap<String, u64>> {
	let mut gens: HashMap<String, u64> = HashMap::new();
	for dir in [segments.to_path_buf(), segments.join("quarantine")] {
		let entries = match fs.read_dir(&dir).await {
			Ok(entries) => entries,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
			Err(error) => return Err(error).with_context(|| format!("listing {}", dir.display())),
		};
		for entry in entries {
			if let Some(parsed) = entry.name.to_str().and_then(frame_name::parse) {
				let max = gens.entry(parsed.stem).or_insert(parsed.gen);
				*max = (*max).max(parsed.gen);
			}
		}
	}
	Ok(gens)
}

/// The current time in milliseconds since the Unix epoch, as the journal records it.
fn now_ms() -> i64 {
	i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis()).unwrap_or(i64::MAX)
}

/// One output a maintenance swap plans (design section 5.3, M2): the rows it holds, the
/// frame kind it is written in (`rows_per_page` for a paged frame), and its name's
/// generation and adoption order.
pub(crate) struct PlannedOutput {
	gen: u64,
	prec: u64,
	timestamps: Vec<i64>,
	values: Vec<Option<BigDecimal>>,
	rows_per_page: Option<usize>,
	/// The series key of the plan's series, when it is not series 0: the output is
	/// written in a v7 envelope carrying it ([`tag_frame`]), exactly as the ingest that
	/// sealed its members wrote them, so no committed row of a tagged series ever names
	/// an untagged frame.
	series_key: Option<crate::SeriesKey>,
}

/// A maintenance output encoded off the async workers: its frame bytes, its descriptor
/// (without its id), and the rows it was encoded from, handed back for its sidecar.
type EncodedOutput = (Vec<u8>, SegmentDescriptor, Vec<i64>, Vec<Option<BigDecimal>>);

/// A segment an ingest has written: its descriptor, generation, frame CRC and rows.
pub(crate) type WrittenSegment = (SegmentDescriptor, u64, u32, Vec<i64>, Vec<Option<BigDecimal>>);

/// What a segment's partial sidecar is built from and named by: the frame's generation,
/// adoption order and CRC, and its rows.
pub(crate) struct SidecarSource {
	pub(crate) gen: u64,
	pub(crate) prec: u64,
	pub(crate) frame_crc: u32,
	pub(crate) timestamps: Vec<i64>,
	pub(crate) values: Vec<Option<BigDecimal>>,
}

/// An output a maintenance swap has written and synced (M4): its frame's file name, its
/// generation, adoption order and CRC, its descriptor (whose `id` the swap sets), and its
/// rows, which its sidecar is built from once it is swapped in.
pub(crate) struct WrittenOutput {
	name: String,
	gen: u64,
	prec: u64,
	frame_crc: u32,
	descriptor: SegmentDescriptor,
	timestamps: Vec<i64>,
	values: Vec<Option<BigDecimal>>,
}

impl WrittenOutput {
	/// The output's row under `id`, committed in aspect epoch `epoch`, for series
	/// `series_id`.
	fn row(&self, id: u64, epoch: u64, series_id: u64) -> IndexRow {
		IndexRow { desc: SegmentDescriptor { id, ..self.descriptor.clone() }, gen: self.gen, prec: Some(self.prec), frame_crc: Some(self.frame_crc), commit_epoch: Some(epoch), series_id }
	}
}

/// The rows and frame kind of a frame read for maintenance.
pub(crate) struct DecodedFrame {
	rows_per_page: Option<usize>,
	timestamps: Vec<i64>,
	values: Vec<Option<BigDecimal>>,
}

/// What a store-wide [`SegmentStore::reap`] did.
///
/// Not `Clone`/`PartialEq`: [`failed`](Self::failed) carries the per-aspect
/// [`anyhow::Error`]s, which are neither.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct ReapSweep {
	/// Whether the sweep was cancelled before it finished (see
	/// [`MaintenanceCancel`](crate::MaintenanceCancel)). `true` means a cancellation request
	/// was received and the sweep stopped before the next aspect could start.
	pub cancelled: bool,
	/// Aspects whose journal the pass worked through.
	pub aspects_reaped: usize,
	/// Retired frames (and abandoned maintenance outputs) unlinked.
	pub frames_unlinked: usize,
	/// Retired frames left for a later pass: a read that may still open them is running,
	/// or a backup holds the reaper off, or the filesystem asked to retry the unlink later
	/// (a Windows sharing violation).
	pub frames_waiting: usize,
	/// Aspects another maintenance operation held, left for a later pass.
	pub busy: Vec<String>,
	/// Aspects whose pass failed, with the error.
	pub failed: Vec<(String, anyhow::Error)>,
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
pub(crate) fn contained_file(dir: &Path, file_name: &str) -> Result<PathBuf> {
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
	/// The `metadata.db` store (`root/metadata.db`): since S11 the segment-set
	/// rollup lives in `segment_index.db`, and this holds the rows-mode legacy
	/// catalog instead (its rollup table is the pre-S11 home, no longer written).
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
	/// Set by an ambiguous `segment_index.db` COMMIT; every write checks it, and the
	/// process-wide poison, and refuses while either is set (see [`Poisoned`]).
	poison: StorePoison,
	/// The filesystem every durable store write goes through: the `STORE_FORMAT` marker
	/// and the directory fsyncs.
	fs: Arc<dyn StoreFs>,
	/// The root's `STORE_FORMAT` marker, as this store last wrote or read it.
	format: RwLock<StoreFormat>,
	/// Serialises the marker writes of [`ensure_floor`](Self::ensure_floor).
	marker_writer: tokio::sync::Mutex<()>,
	/// The highest floor [`ensure_floor`](Self::ensure_floor) has made sure of: both
	/// floors of the marker are at least this.
	floor_ensured: AtomicU32,
	/// The store's identity, from its marker (and `store_meta`).
	store_uuid: String,
	/// What the open found and did.
	report: OpenReport,
	/// What the last recovery pass found and did (startup, or the fsck endpoint):
	/// what `/ready` reports, counts only.
	recovery: RwLock<RecoveryReport>,
	/// Whether any suspect quarantine record exists right now: reads of a store
	/// holding one are incomplete, and their responses carry `Weft-Incomplete`.
	suspect_present: AtomicBool,
	/// Whether the previous process that held this store shut down cleanly: the
	/// `clean_shutdown` flag in `store_meta` as this open found it, before this
	/// open reset it. A store no process ever marked clean reads `false`.
	previous_clean_shutdown: bool,
	/// Each aspect's commit lock (around every control-plane transaction of the aspect,
	/// guarding its id allocator) and maintenance lock (around a whole maintenance
	/// operation); see `aspect_locks`.
	locks: AspectLocks,
	/// The largest id each aspect's legacy-named files in `segments/` carry, read once,
	/// when the first allocator is seeded. Every id handed out after that comes from an
	/// allocator seeded above it, so the one listing stays a valid floor for an aspect
	/// seeded later.
	legacy_ids: tokio::sync::OnceCell<HashMap<String, u64>>,
	/// The largest generation each write-once frame stem in `segments/` carries, read once,
	/// when the first generation counter is seeded; a valid floor for every aspect for
	/// the same reason as `legacy_ids`.
	frame_gens: tokio::sync::OnceCell<HashMap<String, u64>>,
	/// The reaper: reader pins, the backup hold, the `segments/` directory syncer, and the
	/// unlinking of retired frames (`reaper`).
	reaper: Reaper,
	/// How long a per-aspect maintenance entry point waits for an aspect another
	/// maintenance operation holds.
	maintenance_wait: Duration,
	/// The per-aspect circuit breaker store-wide maintenance sweeps run under
	/// (ROB-19): its policy, from the open options.
	maintenance_breaker: MaintenanceBreaker,
	/// The per-aspect decode breaker (ROB-22): shares the same breaker-state map
	/// as the maintenance breaker but with its own threshold and cooldown, so a
	/// decode crash loop on a damaged frame stops the retries without affecting
	/// the maintenance breaker's sweep-level tripped-aspect list.
	decode_breaker: MaintenanceBreaker,
	/// The most series one aspect may register, from the open options
	/// ([`SegmentStoreOptions::max_series_per_aspect`]).
	max_series_per_aspect: usize,
	/// The most buckets one stored downsample may produce, from the open
	/// options ([`SegmentStoreOptions::max_downsample_buckets`]) (ROB-13).
	max_downsample_buckets: usize,
	/// The most instants one batch point lookup may name, from the open
	/// options ([`SegmentStoreOptions::max_point_batch`]) (ROB-13).
	max_point_batch: usize,
	/// Whether range reads memory-map their frames, from the open options
	/// ([`SegmentStoreOptions::mmap_reads`]); always buffered without the `mmap-reads` feature.
	mmap_reads: bool,
	/// The disk guard every seal consults (ROB-17), from the open options
	/// ([`SegmentStoreOptions::disk_reserve_bytes`]).
	disk_guard: crate::DiskGuard,
	/// Whether the disk guard is refusing, so the store logs the change of state once
	/// rather than every refused write (ROB-17).
	disk_refusals: crate::RefusalLatch,
	/// Each aspect's circuit-breaker state (consecutive sweep failures and, once
	/// tripped, when the cooldown ends), guarded by a plain mutex: it is only
	/// ever held for a map touch, never across an await.
	breaker_state: Mutex<HashMap<String, AspectBreakerState>>,
	/// Each aspect's decode-breaker state (ROB-22), separate from the maintenance
	/// breaker so decode failures on a damaged frame do not affect the sweep-level
	/// tripped-aspect list.
	decode_breaker_state: Mutex<HashMap<String, AspectBreakerState>>,
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
/// service account's store under a `0711` directory, say), or on a filesystem that will
/// not fsync it: a read-only mount (`EROFS`), or a FUSE or Docker Desktop bind mount that
/// rejects a directory fsync (`EINVAL`, `ENOTSUP`) (release plan C-3). Syncing that parent
/// then fails, which is logged and skipped rather than refusing a store that opened
/// before S3. Any other directory, and the parent of a root this open created, must sync.
///
/// Once is enough: Turso truncates an MVCC `-log` in place on checkpoint instead of
/// recreating it (`turso_core` `logical_log.rs`), so an entry fsynced here stays durable.
async fn sync_layout(fs: &dyn StoreFs, root: &Path, segments_dir: &Path, created: &[PathBuf]) -> Result<()> {
	let root_parent = parent_dir(root);
	let created_root = created.iter().any(|dir| dir == root);
	for dir in layout_dirs(root, segments_dir, created) {
		match fs.sync_dir(&dir).await {
			Ok(()) => {}
			Err(e) if parent_sync_is_optional(&e) && !created_root && root_parent.as_ref() == Some(&dir) => {
				tracing::warn!(dir = %dir.display(), root = %root.display(), error = %e, "cannot fsync the store root's parent; the root predates this open, so its directory entry is left to the filesystem");
			}
			Err(e) => return Err(e).with_context(|| format!("fsyncing directory {}", dir.display())),
		}
	}
	Ok(())
}

/// Whether an error fsyncing the parent of a root that predates the open may be skipped:
/// the parent cannot be read (`PermissionDenied`), or its filesystem refuses a directory
/// fsync (`EROFS`, `EINVAL`, `ENOTSUP`/`EOPNOTSUPP`). See [`sync_layout`].
fn parent_sync_is_optional(e: &std::io::Error) -> bool {
	use std::io::ErrorKind;
	matches!(e.kind(), ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem | ErrorKind::InvalidInput | ErrorKind::Unsupported)
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

/// Whether any of the four control-plane databases exists under `root`: a root without
/// them (and without a marker) is a new store.
async fn control_plane_present(root: &Path) -> bool {
	for file in CONTROL_PLANE_FILES {
		if matches!(tokio::fs::try_exists(root.join(file)).await, Ok(true)) {
			return true;
		}
	}
	false
}

/// `format` with its floors raised to what `migrations` need and `migrating_to` set to
/// their layout, or `format` itself when there are none: the marker an open writes before
/// it runs them (freeze design §4.3, step 5). Floors only ever go up.
fn raised_for(format: &StoreFormat, migrations: &[&migrations::Migration]) -> StoreFormat {
	migrations::floors(migrations.iter().copied()).map_or_else(|| format.clone(), |needs| StoreFormat { min_read_layout: format.min_read_layout.max(needs.min_read), min_write_layout: format.min_write_layout.max(needs.min_write), migrating_to: Some(format.migrating_to.unwrap_or(0).max(needs.layout)), ..format.clone() })
}

/// The marker an open leaves once its migrations are done (freeze design §4.3, steps 9
/// and 10): every registered migration applied, so the registry's layout and floors,
/// nothing migrating, `last_written_layout` this build's, and `store_uuid` the store's.
/// A `newer` store keeps its layout, floors and migration state; only
/// `last_written_layout` says this build wrote it, so the newer WeftDB re-derives what
/// its older-writer-safe migrations maintain.
fn settled_format(format: &StoreFormat, newer: bool, store_uuid: String) -> StoreFormat {
	if newer {
		return StoreFormat { last_written_layout: SUPPORTED_LAYOUT, store_uuid, ..format.clone() };
	}
	let registry = migrations::floors(migrations::REGISTRY).unwrap_or(migrations::Floors { layout: SUPPORTED_LAYOUT, min_read: SUPPORTED_LAYOUT, min_write: SUPPORTED_LAYOUT });
	StoreFormat { layout_version: format.layout_version.max(registry.layout), min_read_layout: format.min_read_layout.max(registry.min_read), min_write_layout: format.min_write_layout.max(registry.min_write), last_written_layout: SUPPORTED_LAYOUT, migrating_to: None, applied_through: migrations::REGISTRY.last().map(|migration| migration.id.to_string()), store_uuid, ..format.clone() }
}

/// Mirror `format` into `index`'s `store_meta` (freeze design §4.3, step 9), writing only
/// the keys whose recorded value differs, in one transaction.
async fn mirror_into_store_meta(index: &SegmentIndexStore, name: &str, format: &StoreFormat) -> Result<()> {
	let wanted = [("layout_version", Some(format.layout_version.to_string())), ("min_read_layout", Some(format.min_read_layout.to_string())), ("min_write_layout", Some(format.min_write_layout.to_string())), ("last_written_layout", Some(format.last_written_layout.to_string())), ("applied_through", format.applied_through.clone()), ("store_uuid", Some(format.store_uuid.clone()))];
	let mut ops = Vec::new();
	for (key, value) in wanted {
		let Some(value) = value else { continue };
		if index.meta(key).await.with_context(|| format!("{name}: reading store_meta {key}"))?.as_deref() != Some(value.as_str()) {
			ops.push(IndexOp::MetaSet { key: key.to_string(), value });
		}
	}
	if !ops.is_empty() {
		index.apply(&IndexTxn::new(ops)).await.with_context(|| format!("{name}: mirroring the store marker into store_meta"))?;
	}
	Ok(())
}

/// A store's four control-plane databases, opened with pragmas and probes only: no DDL.
struct ControlPlaneDbs {
	index: SegmentIndexStore,
	metadata: AspectMetadataStore,
	catalog: AspectCatalog,
	registry: CatalogStore,
}

impl ControlPlaneDbs {
	/// Open the databases at `names` (the root's [`CONTROL_PLANE_FILES`], in that order),
	/// the index first: it refuses a write floor newer than this build before anything is
	/// written to it or opened beside it.
	async fn open(names: &[String; 4]) -> Result<Self> {
		let index = SegmentIndexStore::open_unmigrated(&names[0]).await?;
		let metadata = AspectMetadataStore::open_unmigrated(&names[1]).await?;
		let catalog = AspectCatalog::open_unmigrated(&names[2]).await?;
		let registry = CatalogStore::open_unmigrated(&names[3]).await?;
		Ok(Self { index, metadata, catalog, registry })
	}

	/// The databases as the migration registry takes them, named by `names`.
	fn control_plane<'a>(&'a self, names: &'a [String; 4]) -> migrations::ControlPlane<'a> {
		migrations::ControlPlane { index: Some(migrations::Db { db: self.index.database(), name: &names[0] }), metadata: Some(migrations::Db { db: self.metadata.database(), name: &names[1] }), aspect_catalog: Some(migrations::Db { db: self.catalog.database(), name: &names[2] }), catalog: Some(migrations::Db { db: self.registry.database(), name: &names[3] }) }
	}
}

/// The marker `index`'s `store_meta` mirrors, for a root whose own `STORE_FORMAT` is gone
/// (deleted, or a control plane restored without it), so that the open keeps the store's
/// layout and floors rather than taking it for layout 1 and lowering them. `None` unless a
/// WeftDB that writes markers recorded it, which always records `min_write_layout`: a
/// development store of S6 or S7 records only `layout_version` and `store_uuid`, and is
/// layout 1 like every store written before the marker.
///
/// # Errors
///
/// A failed read, or a layout key that does not hold a layout number.
async fn recorded_format(index: &SegmentIndexStore, name: &str, scope: StoreScope) -> Result<Option<StoreFormat>> {
	let text = |key: &'static str| async move { index.meta(key).await.with_context(|| format!("{name}: reading store_meta {key}")) };
	let layout = |key: &'static str| async move { text(key).await?.map_or_else(|| Ok(None), |value| value.trim().parse::<u32>().map(Some).with_context(|| format!("{name}: store_meta records {key} {value:?}, which is not a layout number"))) };
	let Some(min_write_layout) = layout("min_write_layout").await? else { return Ok(None) };
	let layout_version = layout("layout_version").await?.unwrap_or(min_write_layout);
	let min_read_layout = layout("min_read_layout").await?.unwrap_or(min_write_layout);
	let last_written_layout = layout("last_written_layout").await?.unwrap_or(layout_version);
	let applied_through = text("applied_through").await?;
	let store_uuid = text("store_uuid").await?.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
	Ok(Some(StoreFormat { kind: store_format::STORE_KIND.to_string(), layout_version, min_read_layout, min_write_layout, last_written_layout, migrating_to: None, applied_through, store_uuid, scope }))
}

/// The process-wide poison ([`poison_global`]) as the [`Poisoned`] error every write, and
/// every open, fails with while it is set.
fn process_poisoned() -> Option<Poisoned> {
	poison_global().get().map(|reason| Poisoned { reason: format!("the process is write-poisoned: {reason}") })
}

mod maint;
mod mapped;
mod open;
mod read;
mod seal;
mod stats;

impl SegmentStore {
	/// The durability class `/ready` reports (design section 2, via the directory
	/// sync it rests on): `full` where a seal fsyncs its file and its directory
	/// entry, and `process-crash-only` where the platform cannot fsync a directory
	/// at all (Windows: [`StoreFs::sync_dir`](crate::types::durable::StoreFs::sync_dir)
	/// is a no-op there), so a power cut, but not a process crash, can lose the entry.
	#[must_use]
	pub const fn durability_class() -> &'static str {
		if cfg!(windows) {
			"process-crash-only"
		} else {
			"full"
		}
	}

	/// What the last recovery pass found and did, counts only.
	#[must_use]
	pub fn recovery_report(&self) -> RecoveryReport {
		self.recovery.read().unwrap_or_else(PoisonError::into_inner).clone()
	}

	/// Whether the previous process that held this store shut down cleanly (see the
	/// field documentation on the struct): `false` for a store no process ever
	/// marked clean, and for every open after a crash.
	#[must_use]
	pub const fn previous_clean_shutdown(&self) -> bool {
		self.previous_clean_shutdown
	}

	/// Whether any suspect quarantine record exists right now: reads of such a
	/// store are incomplete, and their responses carry `Weft-Incomplete`.
	#[must_use]
	pub fn has_suspect_segments(&self) -> bool {
		self.suspect_present.load(Ordering::Relaxed)
	}

	/// Record that this process is shutting down cleanly, so the next open's
	/// `/ready` can report `previous_clean_shutdown: true`. The server calls this
	/// after serving stops; a process that dies any other way leaves the flag at
	/// the `0` its open wrote, and the next open reports the crash.
	///
	/// # Errors
	///
	/// Propagates a control-plane write failure.
	pub async fn mark_clean_shutdown(&self) -> Result<()> {
		self.commit_index(&IndexTxn::new(vec![IndexOp::MetaSet { key: "clean_shutdown".to_string(), value: "1".to_string() }])).await?;
		Ok(())
	}

	/// Shut the store down cleanly (D-S14; crash-consistency §5.5's
	/// shutdown sequence, plan C9's drain order): run the reaper over
	/// whatever the process's swaps retired, then record the clean
	/// shutdown [`mark_clean_shutdown`](Self::mark_clean_shutdown) writes.
	///
	/// The steps before this one are the server's: serving drains first
	/// (in-flight ingests finish or the grace window ends), then the
	/// daemons stop on their shared watch. What remains store-side is
	/// exactly these two: retired frames are reclaimed while the process
	/// still holds the root, and the flag the next open reports is set
	/// last, so a failure mid-shutdown still reads as a crash to that
	/// open. A reaper failure does not block the flag: litter left behind
	/// is what the next open's recovery classifies, while a missing flag
	/// would misreport a clean stop as a crash.
	///
	/// # Errors
	///
	/// Propagates a failure of the clean-shutdown commit itself (a
	/// reaper failure is logged, not propagated — see above).
	pub async fn shutdown(&self) -> Result<()> {
		match self.reap(MaintenanceWait::Skip).await {
			Ok(sweep) => {
				if !sweep.busy.is_empty() {
					tracing::debug!(busy = ?sweep.busy, "store shutdown: the reaper skipped busy aspects; their retired frames stay for the next open's recovery");
				}
			}
			Err(error) => {
				tracing::warn!(error = %error, "store shutdown: the reaper failed; retired frames stay for the next open's recovery");
			}
		}
		self.mark_clean_shutdown().await
	}

	/// The quarantine records, of one aspect for `Some`, or of every aspect for `None`.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn quarantine_list(&self, aspect: Option<&str>) -> Result<Vec<QuarantineRecord>> {
		self.index.quarantine(aspect).await
	}

	/// Release one quarantine record back into reads: its file moves from
	/// `segments/quarantine/` to `segments/`, and its index row is re-inserted
	/// exactly as quarantine recorded it. An operator decision, for either class:
	/// a corrupt file released this way will fail its next fsck again, and a
	/// suspect file released this way reads again on a build that understands it.
	///
	/// # Errors
	///
	/// A record that does not exist, a record with no row behind it (crash litter
	/// has no segment to restore), a taken segment id, a missing quarantined file
	/// (the row is still restored: the next fsck classifies the missing file), or
	/// a control-plane or rollup failure.
	pub async fn quarantine_release(&self, aspect: &str, id: u64, gen: u64) -> Result<QuarantineRecord> {
		let record = self.quarantine_list(Some(aspect)).await?.into_iter().find(|record| record.id == id && record.gen == gen).ok_or_else(|| anyhow::anyhow!("no quarantine record for {aspect} segment {id} generation {gen}"))?;
		let Some(json) = record.descriptor_json.as_deref() else {
			bail!("quarantine record for {aspect} segment {id} has no row to restore: it quarantined crash litter, not an indexed segment");
		};
		let row = QuarantinedRow::from_json(json).ok_or_else(|| anyhow::anyhow!("quarantine record for {aspect} segment {id} holds a row this build cannot read"))?;
		let restored = IndexRow { desc: row.desc, gen: row.gen, prec: row.prec, frame_crc: row.frame_crc, commit_epoch: row.commit_epoch, series_id: row.series_id };
		self.commit_index(&IndexTxn::new(vec![IndexOp::QuarantineDelete { aspect: aspect.to_string(), id, gen }, IndexOp::InsertNew { aspect: aspect.to_string(), row: restored }])).await?;
		let segments = self.root.join("segments");
		let from = contained_file(&segments.join("quarantine"), &record.name)?;
		let to = contained_file(&segments, &record.name)?;
		if tokio::fs::try_exists(&from).await.unwrap_or(false) {
			self.fs.rename(&from, &to).await.with_context(|| format!("moving quarantined {} back to {}", from.display(), to.display()))?;
			self.sync_segments().await?;
		}
		self.rebuild_aspect_metadata(aspect).await?;
		self.refresh_suspect_present().await?;
		Ok(record)
	}

	/// Purge one quarantine record, of either class: its file under
	/// `segments/quarantine/` is unlinked (a missing file counts as already gone)
	/// and its record deleted. The operator's explicit purge: unlike the TTL purge,
	/// this one also purges suspect records, which nothing automatic ever does.
	///
	/// # Errors
	///
	/// A record that does not exist, or a control-plane or filesystem failure.
	pub async fn quarantine_purge(&self, aspect: &str, id: u64, gen: u64) -> Result<QuarantineRecord> {
		let record = self.quarantine_list(Some(aspect)).await?.into_iter().find(|record| record.id == id && record.gen == gen).ok_or_else(|| anyhow::anyhow!("no quarantine record for {aspect} segment {id} generation {gen}"))?;
		self.purge_record(&record).await?;
		Ok(record)
	}

	/// Purge every corrupt quarantine record whose TTL has passed
	/// ([`QUARANTINE_TTL_MS`]): unlink its file and delete its record. Suspect
	/// records are never purged here, however old they are — only an operator
	/// purge ([`quarantine_purge`](Self::quarantine_purge)) removes one.
	///
	/// # Errors
	///
	/// Propagates a control-plane or filesystem failure.
	pub async fn purge_expired_quarantine(&self) -> Result<usize> {
		let now_ms = now_ms();
		let mut purged = 0;
		for record in self.quarantine_list(None).await? {
			if record.class == QuarantineClass::Corrupt && record.quarantined_ms.saturating_add(QUARANTINE_TTL_MS) <= now_ms {
				self.purge_record(&record).await?;
				purged += 1;
			}
		}
		self.refresh_suspect_present().await?;
		Ok(purged)
	}

	/// Unlink one quarantine record's file and delete its record, in that order:
	/// a record whose file is already gone still gets deleted, and a record whose
	/// unlink fails keeps its record, so the next purge retries it.
	async fn purge_record(&self, record: &QuarantineRecord) -> Result<()> {
		let path = contained_file(&self.root.join("segments").join("quarantine"), &record.name)?;
		self.fs.remove_file(&path).await.with_context(|| format!("unlinking quarantined {}", path.display()))?;
		self.commit_index(&IndexTxn::new(vec![IndexOp::QuarantineDelete { aspect: record.aspect.clone(), id: record.id, gen: record.gen }])).await?;
		Ok(())
	}

	/// Recompute [`has_suspect_segments`](Self::has_suspect_segments) from the
	/// records on disk, after a quarantine change.
	async fn refresh_suspect_present(&self) -> Result<()> {
		let suspect = self.quarantine_list(None).await?.iter().any(|record| record.class == QuarantineClass::Suspect);
		self.suspect_present.store(suspect, Ordering::Relaxed);
		Ok(())
	}

	/// What [`adopt_orphan`](Self::adopt_orphan) would do with `name`, without
	/// committing anything: the aspect, generation, precision and decoded frame
	/// to adopt, or the [`Adoption::Undeclared`] / [`Adoption::Skipped`]
	/// outcome. A report-only pass counts through this, so it counts exactly
	/// what a real pass adopts.
	async fn plan_adoption(&self, name: &str, bytes: &[u8]) -> Result<std::result::Result<(String, u64, Option<u64>, frame_read::FullFrame), Adoption>> {
		let (aspect, gen, prec) = match frame_name::parse(name) {
			Some(parsed) => match parsed.aspect() {
				Some(aspect) => (aspect, parsed.gen, Some(parsed.prec)),
				None => return Ok(Err(Adoption::Skipped)),
			},
			None => match legacy_file_id(name) {
				Some((aspect, _)) => (aspect.to_string(), 0, None),
				None => return Ok(Err(Adoption::Skipped)),
			},
		};
		if self.schema_for(&aspect).await?.is_none() {
			return Ok(Err(Adoption::Undeclared(aspect)));
		}
		let frame = frame_read::read_full(name, bytes)?;
		if frame.descriptor(0, String::new(), 0).row_count == 0 {
			return Ok(Err(Adoption::Skipped));
		}
		Ok(Ok((aspect, gen, prec, frame)))
	}

	/// The outcome [`adopt_orphan`](Self::adopt_orphan) would report for
	/// `name`, with nothing committed (a report-only pass).
	async fn would_adopt(&self, name: &str, bytes: &[u8]) -> Result<Adoption> {
		Ok(match self.plan_adoption(name, bytes).await? {
			Ok((aspect, ..)) => Adoption::Adopted(aspect),
			Err(outcome) => outcome,
		})
	}

	/// Whether [`quarantine_orphan`](Self::quarantine_orphan) can file `name`:
	/// its name names an aspect, write-once or legacy.
	fn orphan_names_aspect(name: &str) -> bool {
		frame_name::parse(name).and_then(|parsed| parsed.aspect()).is_some() || legacy_file_id(name).is_some()
	}

	/// Adopt one orphan frame (R8, TAG-9): rebuild its descriptor from the
	/// frame itself and insert its row under the aspect's next id, raising
	/// the allocator past it in the same transaction, under the aspect's
	/// commit lock. A tagged frame (`series_key` is `Some` non-empty key)
	/// resolves its key in the frame's aspect — the catalog's id, or the
	/// aspect's next series id registered with
	/// [`RegisterSeries`](IndexOp::RegisterSeries) and
	/// [`SeedSeries`](IndexOp::SeedSeries) — in that same transaction, so the
	/// row never commits without its catalog row. Recovery does not apply the
	/// per-aspect series cap: the data already exists, and refusing it would
	/// leave it invisible. With `clear_journal` the transaction also clears
	/// the frame's journal row (a held frame released by a later pass).
	///
	/// Returns [`Adoption::Undeclared`] when the frame's aspect has no
	/// declared schema in this store's scope (its data has no home here yet),
	/// and [`Adoption::Skipped`] when its name names no aspect or it holds no
	/// rows. The caller has already classified the frame through
	/// `describe_frame` and read `series_key` from it with [`frame_series_key`].
	async fn adopt_orphan(&self, name: &str, bytes: &[u8], series_key: Option<crate::SeriesKey>, clear_journal: bool) -> Result<Adoption> {
		let (aspect, gen, prec, frame) = match self.plan_adoption(name, bytes).await? {
			Ok(plan) => plan,
			Err(outcome) => return Ok(outcome),
		};
		let mut commit = self.commit_lock(&aspect).await?;
		let mut ops: Vec<IndexOp> = Vec::with_capacity(5);
		let series_id = match &series_key {
			Some(key) => self.resolve_series_key(&aspect, key, &mut ops).await?,
			None => crate::SeriesId::UNTAGGED,
		};
		let id = commit.allocate()?;
		let path = self.root.join("segments").join(name).to_string_lossy().into_owned();
		let desc = frame.descriptor(id, path, bytes.len() as u64);
		let trailer = bytes.len().checked_sub(4).map(|body| u32::from_le_bytes([bytes[body], bytes[body + 1], bytes[body + 2], bytes[body + 3]]));
		let row = IndexRow { desc, gen, prec, frame_crc: trailer, commit_epoch: None, series_id: series_id.0 };
		ops.push(IndexOp::InsertNew { aspect: aspect.clone(), row });
		ops.push(seq_bump(&aspect, &commit, gen.saturating_add(1), commit.state().epoch));
		if clear_journal {
			ops.push(IndexOp::ClearJournal { name: name.to_string() });
		}
		self.commit_index(&IndexTxn::new(ops)).await?;
		drop(commit);
		Ok(Adoption::Adopted(aspect))
	}

	/// Quarantine one indexed row's frame under `class`: the record and the
	/// row delete commit together, then the file (when it exists) moves to
	/// `segments/quarantine/`. Returns whether a file moved.
	async fn quarantine_indexed_row(&self, aspect: &str, row: &IndexRow, file_name: &str, path: &Path, class: QuarantineClass, reason: String) -> Result<bool> {
		let desc = &row.desc;
		let record = QuarantineRecord { aspect: aspect.to_string(), id: desc.id, gen: row.gen, name: file_name.to_string(), class, reason, min_ts: desc.min_ts, max_ts: desc.max_ts, series_id: row.series_id, quarantined_ms: now_ms(), descriptor_json: Some(QuarantinedRow { desc: desc.clone(), gen: row.gen, prec: row.prec, frame_crc: row.frame_crc, commit_epoch: row.commit_epoch, series_id: row.series_id }.to_json()) };
		self.commit_index(&IndexTxn::new(vec![IndexOp::QuarantineInsert { record }, IndexOp::DeleteExpected { aspect: aspect.to_string(), expected: row.version() }])).await?;
		if !tokio::fs::try_exists(path).await.unwrap_or(false) {
			return Ok(false);
		}
		let quarantine_dir = self.root.join("segments").join("quarantine");
		tokio::fs::create_dir_all(&quarantine_dir).await.with_context(|| format!("creating {}", quarantine_dir.display()))?;
		let to = contained_file(&quarantine_dir, file_name)?;
		self.fs.rename(path, &to).await.with_context(|| format!("quarantining {} to {}", path.display(), to.display()))?;
		Ok(true)
	}

	/// Quarantine one orphan frame under `class`: its record commits with
	/// `extra` in one transaction, then the file moves to
	/// `segments/quarantine/`. Returns whether a file moved: `false` when the
	/// name names no aspect, and nothing is done.
	async fn quarantine_orphan(&self, name: &str, path: &Path, class: QuarantineClass, reason: String, extra: Vec<IndexOp>) -> Result<bool> {
		let (aspect, id, gen) = match frame_name::parse(name).and_then(|parsed| parsed.aspect().map(|aspect| (aspect, parsed.gen))) {
			Some((aspect, gen)) => (aspect, 0, gen),
			None => match legacy_file_id(name) {
				Some((aspect, id)) => (aspect.to_string(), id, 0),
				None => return Ok(false),
			},
		};
		let record = QuarantineRecord { aspect, id, gen, name: name.to_string(), class, reason, min_ts: None, max_ts: None, series_id: 0, quarantined_ms: now_ms(), descriptor_json: None };
		let mut ops = vec![IndexOp::QuarantineInsert { record }];
		ops.extend(extra);
		self.commit_index(&IndexTxn::new(ops)).await?;
		let quarantine_dir = self.root.join("segments").join("quarantine");
		tokio::fs::create_dir_all(&quarantine_dir).await.with_context(|| format!("creating {}", quarantine_dir.display()))?;
		let to = contained_file(&quarantine_dir, name)?;
		self.fs.rename(path, &to).await.with_context(|| format!("quarantining orphan {} to {}", path.display(), to.display()))?;
		Ok(true)
	}

	/// Startup recovery and the fsck pass, in one: classify every indexed frame
	/// and every orphan under `segments/`, under the C5 policy.
	///
	/// For each indexed row, in aspect order:
	/// - a row whose path escapes `segments/` is left exactly where it is and
	///   counted as skipped: recovery never moves a file it cannot contain
	///   (the migration precheck already refuses such a store at open, so this
	///   is defence in depth for a row written after the open);
	/// - a row whose file is missing is corrupt-quarantined: the record is
	///   written and the row deleted, and there is no file to move;
	/// - a row whose file fails to decode is quarantined under
	///   [`classify_decode_failure`]: corrupt files move to
	///   `segments/quarantine/` and their rows are deleted, suspect files move
	///   the same way and are never TTL-purged, and a newer-format cause also
	///   sets `incompatible_store` on the report;
	/// - a row whose file decodes is re-bound to its frame's key where it
	///   differs: its generation and adoption order from the frame's file name,
	///   and the frame's own CRC trailer when the row's `frame_crc` is unset or
	///   disagrees with it. An aspect re-bind moves the row to the aspect the
	///   frame's name names. A row that already matches its frame is untouched;
	/// - TAG-9: the frame's `SERIES_KEY` (none for an untagged frame) must be
	///   the key the catalog holds under the row's series id. A malformed,
	///   empty or repeated payload in a frame that decoded is suspect (C5); a
	///   key that differs is rebound with one `ReplaceExpected` to the key's
	///   id, registered in the same transaction when the catalog lacks it; a
	///   row moving aspect has its key resolved again in the target aspect
	///   rather than carrying a foreign id. Counted in `series_rebound`.
	///
	/// Before any row is checked, for each aspect the index or the series
	/// catalog names, R10 seeds `aspect_seq.next_series_id` past the largest
	/// series id any row holds or the catalog registered, so a rebind that
	/// registers a key never reuses an id. Each aspect's rollup is rebuilt,
	/// after the orphan pass, when the pass changed its rows. Orphan `.weftseg` files — no index row and no
	/// journal entry claims them — are counted; one that fails to decode is
	/// quarantined the same way (a decodable orphan is left where it is: with
	/// no row behind it there is no id to adopt it under outside a restore).
	/// Held journal rows — tagged frames a restore could not bind because their
	/// aspect was undeclared — are adopted through the same path once the
	/// aspect is declared, the journal row cleared in the adopting
	/// transaction (TAG-9); one whose frame no longer decodes or whose key is
	/// malformed is quarantined with its journal row cleared, and one whose
	/// frame is gone or holds no rows is cleared (`held_cleared`). Finally the TTL purge runs over pre-existing corrupt
	/// records.
	///
	/// With `report_only` the pass counts everything it would do and changes
	/// nothing: no record, no row delete, no move, no seed, no purge, no rollup
	/// rebuild. That is the pass a newer-layout store gets at open, where
	/// recovery must not quarantine, adopt or purge anything a newer WeftDB
	/// might understand — and the pass the fsck endpoint never uses, since an
	/// operator asking for fsck is asking for the repair.
	///
	/// The report replaces the store's last one, so `/ready` reports the latest
	/// pass, and `has_suspect_segments` follows the records left behind.
	///
	/// # Errors
	///
	/// A control-plane failure, a filesystem failure listing or moving a file, or
	/// a rollup rebuild failure. A failure part-way leaves the store consistent —
	/// every quarantine commits its record and its row delete together — but the
	/// report is not stored, so `/ready` keeps reporting the previous pass.
	pub async fn fsck(&self, report_only: bool) -> Result<RecoveryReport> {
		self.fsck_inner(report_only, false).await
	}

	/// [`fsck`](Self::fsck) in restore mode (D-S16, design section 6 R8): the
	/// pass a root carrying a restore intent marker gets at its first open.
	/// Orphans are classified through
	/// [`describe_frame`](weft_physical_type::describe_frame) (tags A3): a
	/// decodable orphan is **adopted** — descriptor rebuilt from the frame,
	/// fresh id above every restored id. One carrying a `SERIES_KEY` extension
	/// is adopted into the series its key names, registered in the same
	/// transaction when the catalog lacks it (TAG-9); only when its aspect is
	/// not declared is it journaled `held` — never reaped — for a later pass
	/// to adopt. A malformed or repeated `SERIES_KEY` payload is suspect.
	/// Invalid orphans quarantine as in a normal pass.
	async fn fsck_restore(&self) -> Result<RecoveryReport> {
		self.fsck_inner(false, true).await
	}

	async fn fsck_inner(&self, report_only: bool, restore_mode: bool) -> Result<RecoveryReport> {
		let mut pass = FsckPass { report_only, restore_mode, segments_dir: self.root.join("segments"), report: RecoveryReport { report_only, ..RecoveryReport::default() }, journal_names: self.index.journal(None).await?.into_iter().map(|entry| entry.name).collect(), live_names: HashSet::new(), moved: false, changed_aspects: Vec::new() };
		// R10 seeds every aspect the index or the series catalog names: an aspect whose
		// rows were all quarantined keeps its catalog rows, and their ids must not be
		// handed out again if its allocator row is lost.
		let mut aspects: std::collections::BTreeSet<String> = self.index.list_aspects().await?.into_iter().collect();
		aspects.extend(self.index.series_aspects().await?);
		// R10 runs before any row is checked: a row rebound below may register its key,
		// and `resolve_series_key` takes the id from `next_series_id`, which must already
		// sit past every id the catalog and the rows hold — in every aspect, since a row
		// can move to an aspect that sorts later.
		self.fsck_seed_series(&mut pass, &aspects).await?;
		for aspect in aspects {
			let mut aspect_changed = false;
			for row in &self.index.rows(&aspect).await? {
				aspect_changed |= self.fsck_check_row(&mut pass, &aspect, row).await?;
			}
			if aspect_changed {
				pass.changed_aspects.push(aspect);
			}
		}
		self.fsck_release_held(&mut pass).await?;
		self.fsck_orphans(&mut pass).await?;
		// Rollups are rebuilt after the orphan pass, so adopted frames count.
		for aspect in &pass.changed_aspects {
			if !pass.report_only {
				self.rebuild_aspect_metadata(aspect).await?;
			}
		}
		if pass.moved {
			self.sync_segments().await?;
		}
		if !pass.report_only {
			pass.report.purged = self.purge_expired_quarantine().await?;
		}
		*self.recovery.write().unwrap_or_else(PoisonError::into_inner) = pass.report.clone();
		self.refresh_suspect_present().await?;
		Ok(pass.report)
	}

	/// R10 of [`fsck_inner`](Self::fsck_inner): seed each aspect's series allocator past
	/// every id its catalog and rows hold.
	async fn fsck_seed_series(&self, pass: &mut FsckPass, aspects: &std::collections::BTreeSet<String>) -> Result<()> {
		for aspect in aspects {
			let mut max_series_id = self.index.max_registered_series_id(aspect).await?;
			for row in self.index.rows(aspect).await? {
				max_series_id = max_series_id.max(row.series_id);
			}
			if !pass.report_only {
				self.commit_index(&IndexTxn::new(vec![IndexOp::SeedSeries { aspect: aspect.clone(), next_series_id: max_series_id.saturating_add(1).max(1) }])).await?;
			}
			pass.report.seeded += 1;
		}
		Ok(())
	}

	/// Check one index row of `aspect` against its frame: quarantine a missing or
	/// undecodable frame, re-bind a row whose frame says otherwise. Whether the aspect
	/// changed.
	async fn fsck_check_row(&self, pass: &mut FsckPass, aspect: &str, row: &IndexRow) -> Result<bool> {
		let mut aspect_changed = false;
		pass.report.checked += 1;
		let desc = &row.desc;
		let Some(file_name) = Path::new(&desc.path).file_name().and_then(|name| name.to_str()) else {
			pass.report.skipped_outside_segments += 1;
			return Ok(aspect_changed);
		};
		// Containment: the row's file must be a direct child of segments/.
		let Ok(path) = contained_file(&pass.segments_dir, file_name) else {
			pass.report.skipped_outside_segments += 1;
			return Ok(aspect_changed);
		};
		// The name is claimed by a row whatever spelling its path
		// uses, so the orphan pass below never treats a live frame
		// as unclaimed — a restored root's rows name the old root's
		// paths, and their frames are live, not orphans (D-S16).
		pass.live_names.insert(file_name.to_string());
		if path != Path::new(&desc.path) {
			// The row names its file through a different spelling (a
			// symlinked parent, a redundant separator): recovery acts
			// only on the contained form, and leaves such a row alone
			// rather than moving a file it did not resolve itself.
			pass.report.skipped_outside_segments += 1;
			return Ok(aspect_changed);
		}
		let bytes = match tokio::fs::read(&path).await {
			Ok(bytes) => bytes,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
				let record = QuarantineRecord { aspect: aspect.to_string(), id: desc.id, gen: row.gen, name: file_name.to_string(), class: QuarantineClass::Corrupt, reason: "missing: the indexed frame is not under segments/".to_string(), min_ts: desc.min_ts, max_ts: desc.max_ts, series_id: row.series_id, quarantined_ms: now_ms(), descriptor_json: Some(QuarantinedRow { desc: desc.clone(), gen: row.gen, prec: row.prec, frame_crc: row.frame_crc, commit_epoch: row.commit_epoch, series_id: row.series_id }.to_json()) };
				if !pass.report_only {
					self.commit_index(&IndexTxn::new(vec![IndexOp::QuarantineInsert { record }, IndexOp::DeleteExpected { aspect: aspect.to_string(), expected: row.version() }])).await?;
					aspect_changed = true;
				}
				pass.report.quarantined += 1;
				return Ok(aspect_changed);
			}
			Err(error) => return Err(error).with_context(|| format!("reading segment {}", path.display())),
		};
		match frame_read::read_full(file_name, &bytes) {
			Ok(frame) => {
				aspect_changed |= self.fsck_rebind_row(pass, aspect, row, (file_name, &path), &bytes, &frame).await?;
			}
			Err(error) => {
				let source = error.downcast_ref::<frame_read::FrameFaultError>().map(|fault| fault.source().clone());
				let (class, newer) = source.as_ref().map_or((QuarantineClass::Corrupt, false), |source| (classify_decode_failure(&bytes, source), is_newer_format(source)));
				if newer {
					pass.report.incompatible_store = true;
				}
				if !pass.report_only {
					pass.moved |= self.quarantine_indexed_row(aspect, row, file_name, &path, class, format!("{error:#}")).await?;
					aspect_changed = true;
				}
				match class {
					QuarantineClass::Corrupt => pass.report.quarantined += 1,
					QuarantineClass::Suspect => pass.report.suspected += 1,
				}
			}
		}
		Ok(aspect_changed)
	}

	/// A row whose frame decoded: re-bind it to what the frame's name, CRC and series key
	/// say, or quarantine it suspect when the key cannot be bound. Whether the aspect changed.
	async fn fsck_rebind_row(&self, pass: &mut FsckPass, aspect: &str, row: &IndexRow, (file_name, path): (&str, &Path), bytes: &[u8], frame: &frame_read::FullFrame) -> Result<bool> {
		let mut aspect_changed = false;
		// TAG-9: the frame's own series key. A SERIES_KEY payload that is
		// malformed, empty or repeated, in a frame whose CRC verified and
		// whose body decoded, is not damage but a frame this build cannot
		// bind: suspect under C5, never purged.
		let frame_key = match weft_physical_type::describe_frame(bytes) {
			Ok(info) => frame_series_key(&info),
			Err(error) => Err(format!("describing the frame: {error:?}")),
		};
		let frame_key = match frame_key {
			Ok(key) => key,
			Err(reason) => {
				if !pass.report_only {
					pass.moved |= self.quarantine_indexed_row(aspect, row, file_name, path, QuarantineClass::Suspect, reason).await?;
					aspect_changed = true;
				}
				pass.report.suspected += 1;
				return Ok(aspect_changed);
			}
		};
		let (mut rebound, mut changed, new_aspect) = Self::rebound_from_frame(aspect, row, file_name, bytes, frame);
		// TAG-9: the row's series must be the one the frame's key names in
		// the aspect the row ends up in. Catalogs are per aspect (D2), so a
		// row moving to another aspect has its key resolved again there
		// rather than carrying a foreign id; a row staying put is rebound
		// when its catalog key differs from the frame's.
		let target = new_aspect.clone().unwrap_or_else(|| aspect.to_string());
		let mut series_ops: Vec<IndexOp> = Vec::new();
		let mut series_lock = None;
		let catalog_matches = new_aspect.is_none() && self.index.series_key_of(aspect, crate::SeriesId(row.series_id)).await?.as_ref() == Some(&frame_key);
		if !catalog_matches {
			let resolved = if pass.report_only {
				self.index.series_id_for(&target, &frame_key).await?
			} else {
				series_lock = Some(self.commit_lock(&target).await?);
				Some(self.resolve_series_key(&target, &frame_key, &mut series_ops).await?)
			};
			if resolved != Some(crate::SeriesId(row.series_id)) {
				if let Some(id) = resolved {
					rebound.series_id = id.0;
				}
				pass.report.series_rebound += 1;
				changed = true;
			}
		}
		if changed {
			if !pass.report_only {
				// Same-aspect re-bind is one guarded replace: a
				// DeleteExpected first would remove the row the
				// ReplaceExpected then has to match, and the
				// transaction fails with "matched 0". Only a
				// move to a different aspect deletes then
				// inserts.
				// The series registration, if any, commits first in the
				// same transaction, so the row never names an id its
				// catalog lacks.
				let mut ops = series_ops;
				match new_aspect.as_deref() {
					Some(new_aspect) if new_aspect != aspect => ops.extend([IndexOp::DeleteExpected { aspect: aspect.to_string(), expected: row.version() }, IndexOp::InsertNew { aspect: new_aspect.to_string(), row: rebound }]),
					_ => ops.push(IndexOp::ReplaceExpected { aspect: aspect.to_string(), expected: row.version(), row: rebound }),
				}
				self.commit_index(&IndexTxn::new(ops)).await?;
				aspect_changed = true;
			}
			pass.report.rebound += 1;
		}
		drop(series_lock);
		Ok(aspect_changed)
	}

	/// What a row's frame says the row should be, from its name and its CRC trailer:
	/// the re-bound row, whether it differs, and the aspect the frame's name names when
	/// that is not `aspect`.
	///
	/// A row whose recorded length differs from the frame on disk describes some other
	/// version of the file (a pre-0.2 store rewrote reconciled frames in place, and a crash
	/// between the rewrite and the index commit left the old row behind). The frame
	/// decoded, so it is the data: the row's descriptor is rebuilt from the frame itself,
	/// keeping its id and path, so maintenance reads (which check the length) work again.
	fn rebound_from_frame(aspect: &str, row: &IndexRow, file_name: &str, bytes: &[u8], frame: &frame_read::FullFrame) -> (IndexRow, bool, Option<String>) {
		let computed_crc = bytes.len().checked_sub(4).map(|body| {
			let trailer = &bytes[body..];
			u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]])
		});
		let mut rebound = row.clone();
		let mut changed = false;
		let mut new_aspect: Option<String> = None;
		if let Some(parsed) = frame_name::parse(file_name) {
			if let Some(frame_aspect) = parsed.aspect() {
				if frame_aspect != aspect {
					// Identity mismatch: re-bind the row to the
					// frame's key. The file stays where it is;
					// only the row moves, to the aspect the
					// frame's own name names.
					new_aspect = Some(frame_aspect);
					changed = true;
				}
			}
			if parsed.gen != row.gen {
				rebound.gen = parsed.gen;
				changed = true;
			}
			if parsed.prec != row.prec.unwrap_or(0) && !(row.prec.is_none() && parsed.prec == 0) {
				rebound.prec = Some(parsed.prec);
				changed = true;
			}
		}
		if row.frame_crc != computed_crc {
			rebound.frame_crc = computed_crc;
			changed = true;
		}
		let on_disk = bytes.len() as u64;
		if row.desc.byte_len != on_disk {
			rebound.desc = frame.descriptor(row.desc.id, row.desc.path.clone(), on_disk);
			changed = true;
		}
		(rebound, changed, new_aspect)
	}

	/// TAG-9 of [`fsck_inner`](Self::fsck_inner): adopt, keep, quarantine or clear each
	/// frame an earlier pass journaled held.
	async fn fsck_release_held(&self, pass: &mut FsckPass) -> Result<()> {
		// TAG-9: release the frames an earlier pass journaled held — tagged
		// orphans whose aspect was not declared then — through the same adoption
		// path, clearing the journal row in the adopting transaction. A frame
		// whose aspect is still undeclared stays held; one whose key cannot be
		// bound is quarantined suspect and its journal row cleared.
		for entry in self.index.journal(None).await? {
			if entry.state != crate::types::reaper::JournalState::Held {
				continue;
			}
			let Ok(path) = contained_file(&pass.segments_dir, &entry.name) else { continue };
			let clear = vec![IndexOp::ClearJournal { name: entry.name.clone() }];
			let bytes = match tokio::fs::read(&path).await {
				Ok(bytes) => bytes,
				Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
					// Nothing left to adopt: the row is cleared, not kept forever.
					if !pass.report_only {
						self.commit_index(&IndexTxn::new(clear)).await?;
					}
					pass.report.held_cleared += 1;
					continue;
				}
				Err(error) => return Err(error).with_context(|| format!("reading held frame {}", path.display())),
			};
			// A held frame that no longer decodes (it decayed since it was held)
			// is quarantined under its decode class, and its journal row cleared
			// in the same transaction; one that decodes but whose key cannot be
			// bound is suspect.
			let key = match weft_physical_type::describe_frame(&bytes) {
				Ok(info) if info.decodable() => frame_series_key(&info).map_err(|reason| (QuarantineClass::Suspect, reason)),
				_ => Err(match frame_read::read_full(&entry.name, &bytes) {
					Err(error) => {
						let source = error.downcast_ref::<frame_read::FrameFaultError>().map(|fault| fault.source().clone());
						let class = source.as_ref().map_or(QuarantineClass::Corrupt, |source| {
							if is_newer_format(source) {
								pass.report.incompatible_store = true;
							}
							classify_decode_failure(&bytes, source)
						});
						(class, format!("{error:#}"))
					}
					Ok(_) => (QuarantineClass::Suspect, "describing the frame: not decodable".to_string()),
				}),
			};
			match key {
				Ok(key) => {
					let tagged = !key.is_empty();
					let outcome = if pass.report_only { self.would_adopt(&entry.name, &bytes).await? } else { self.adopt_orphan(&entry.name, &bytes, tagged.then_some(key), true).await? };
					match outcome {
						Adoption::Adopted(aspect) => {
							pass.report.adopted += 1;
							if !pass.report_only && !pass.changed_aspects.contains(&aspect) {
								pass.changed_aspects.push(aspect);
							}
						}
						// Its aspect is still undeclared: it stays held.
						Adoption::Undeclared(_) => {}
						// No rows, or a name that names no aspect: nothing will ever adopt it.
						Adoption::Skipped => {
							if !pass.report_only {
								self.commit_index(&IndexTxn::new(clear)).await?;
							}
							pass.report.held_cleared += 1;
						}
					}
				}
				Err((class, reason)) => {
					if Self::orphan_names_aspect(&entry.name) {
						if !pass.report_only {
							pass.moved |= self.quarantine_orphan(&entry.name, &path, class, format!("held: {reason}"), clear).await?;
						}
						match class {
							QuarantineClass::Corrupt => pass.report.quarantined += 1,
							QuarantineClass::Suspect => pass.report.suspected += 1,
						}
					} else {
						if !pass.report_only {
							self.commit_index(&IndexTxn::new(clear)).await?;
						}
						pass.report.held_cleared += 1;
					}
				}
			}
		}
		Ok(())
	}

	/// The orphan pass of [`fsck_inner`](Self::fsck_inner): files no row and no journal
	/// entry claims.
	async fn fsck_orphans(&self, pass: &mut FsckPass) -> Result<()> {
		// Orphans: files no row and no journal entry claims.
		let mut entries = self.fs.read_dir(&pass.segments_dir).await.with_context(|| format!("listing {}", pass.segments_dir.display()))?;
		if pass.restore_mode {
			// R8's order: legacy-named orphans first, in id order; then
			// write-once orphans in ascending (prec, gen) order, so
			// happens-before is preserved among the adopted.
			let key = |name: &str| -> (u8, u64, u64) {
				if let Some((_, id)) = legacy_file_id(name) {
					(0, id, 0)
				} else if let Some(parsed) = frame_name::parse(name) {
					(1, parsed.prec, parsed.gen)
				} else {
					(2, 0, 0)
				}
			};
			entries.sort_by_key(|a| key(&a.name.to_string_lossy()));
		}
		for entry in entries {
			if entry.is_dir {
				continue;
			}
			let name = entry.name.to_string_lossy().into_owned();
			if !name.ends_with(".weftseg") || pass.live_names.contains(&name) || pass.journal_names.contains(&name) {
				continue;
			}
			pass.report.orphans += 1;
			let Ok(path) = contained_file(&pass.segments_dir, &name) else { continue };
			let Ok(bytes) = tokio::fs::read(&path).await else { continue };
			if pass.restore_mode && !pass.report_only {
				if let Ok(info) = weft_physical_type::describe_frame(&bytes) {
					if info.decodable() {
						match frame_series_key(&info) {
							Ok(key) => {
								let tagged = !key.is_empty();
								match self.adopt_orphan(&name, &bytes, tagged.then_some(key), false).await? {
									Adoption::Adopted(aspect) => {
										pass.report.adopted += 1;
										if !pass.changed_aspects.contains(&aspect) {
											pass.changed_aspects.push(aspect);
										}
									}
									// A tagged frame whose aspect is not declared has no
									// catalog to bind its key in yet: it is journaled held,
									// never reaped, and a later pass adopts it once the
									// aspect is declared.
									Adoption::Undeclared(aspect) if tagged => {
										self.commit_index(&IndexTxn::new(vec![IndexOp::JournalHeld { aspect, name: name.clone(), created_ms: now_ms() }])).await?;
										pass.report.held += 1;
									}
									Adoption::Undeclared(_) | Adoption::Skipped => {}
								}
							}
							Err(reason) => {
								if Self::orphan_names_aspect(&name) {
									pass.moved |= self.quarantine_orphan(&name, &path, QuarantineClass::Suspect, format!("orphan: no index row claims this frame: {reason}"), Vec::new()).await?;
									pass.report.suspected += 1;
								}
							}
						}
						continue;
					}
				}
			}
			if let Err(error) = frame_read::read_full(&name, &bytes) {
				let source = error.downcast_ref::<frame_read::FrameFaultError>().map(|fault| fault.source().clone());
				let (class, newer) = source.as_ref().map_or((QuarantineClass::Corrupt, false), |source| (classify_decode_failure(&bytes, source), is_newer_format(source)));
				if newer {
					pass.report.incompatible_store = true;
				}
				if !Self::orphan_names_aspect(&name) {
					continue;
				}
				if !pass.report_only {
					pass.moved |= self.quarantine_orphan(&name, &path, class, format!("orphan: no index row claims this frame: {error:#}"), Vec::new()).await?;
				}
				match class {
					QuarantineClass::Corrupt => pass.report.quarantined += 1,
					QuarantineClass::Suspect => pass.report.suspected += 1,
				}
			}
		}
		Ok(())
	}
}

/// The state one [`SegmentStore::fsck_inner`] pass carries between its steps.
struct FsckPass {
	report_only: bool,
	restore_mode: bool,
	segments_dir: PathBuf,
	report: RecoveryReport,
	/// The frames the reaper's journal names: never orphans.
	journal_names: HashSet<String>,
	/// The frames an index row claims: never orphans.
	live_names: HashSet<String>,
	/// Whether a frame was moved, so `segments/` needs a sync.
	moved: bool,
	/// The aspects whose rollups need rebuilding.
	changed_aspects: Vec<String>,
}

impl SegmentStore {
	/// the subject-wide north-star **bytes/point** (priority #1 in the commercial
	/// thesis) across all of this store's aspects, plus the rolled-up segment/row/null
	/// counts and the union of the aspects' time spans.
	///
	/// Built from the per-aspect `segment_index.db` rollup rows (one O(1) read per aspect), so it
	/// never opens a segment file or scans the segment index. The value span is
	/// deliberately omitted — a min/max across aspects of unrelated physical meaning
	/// would not be a useful number.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn store_stats(&self) -> Result<StoreStorageStats> {
		let aspects = self.index.rollup_aspects().await?;
		let mut stats = StoreStorageStats { aspect_count: aspects.len(), ..StoreStorageStats::default() };
		for aspect in &aspects {
			let meta = self.index.rollup(aspect).await?.unwrap_or_default();
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
	/// segment *of the same series of the same aspect* (TAG-6) — the store-wide form of
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
		let aspects = self.index.list_aspects().await?;
		let mut total = 0;
		for aspect in &aspects {
			total += series_overlapping_count(&self.index.rows(aspect).await?);
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
	/// Whether the sweep was cancelled before it finished (see
	/// [`MaintenanceCancel`](crate::MaintenanceCancel)). `true` means a cancellation request
	/// was received and the sweep stopped before the next aspect could start.
	pub cancelled: bool,
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
	/// Aspects the sweep did not attempt because their maintenance circuit breaker
	/// is tripped (see [`MaintenanceBreaker`]), in visit order. A tripped aspect
	/// counts only toward `aspects_scanned`; a sweep after the breaker's cooldown
	/// retries it once.
	pub tripped: Vec<String>,
	/// Breakers this sweep tripped (ROB-21): aspects whose consecutive failures
	/// reached the threshold during this sweep, so the store opened their breaker
	/// (logged once at `WARN` there). A sweep that only *skips* already-tripped
	/// aspects counts none here — those are in [`tripped`](Self::tripped) — so this
	/// is the trip-event count the `weft_maintenance_breaker_trips_total` metric
	/// reports, not a re-count of skips.
	pub breaker_trips: usize,
}

/// The cold-vs-hot split of a hot/cold reconcile pass over one aspect, returned by
/// [`SegmentStore::reconcile_aspect_hot_cold`].
///
/// A cold segment (any out-of-order segment that is not the aspect's most-recently-sealed one) is
/// always reconciled; the hot tail is reconciled only when the aspect's backlog reached the threshold.
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
	/// Whether the sweep was cancelled before it finished (see
	/// [`MaintenanceCancel`](crate::MaintenanceCancel)). `true` means a cancellation request
	/// was received and the sweep stopped before the next aspect could start.
	pub cancelled: bool,
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
	/// Aspects the sweep did not attempt because their maintenance circuit breaker
	/// is tripped (see [`MaintenanceBreaker`]), in visit order. A tripped aspect
	/// counts only toward `aspects_scanned`; a sweep after the breaker's cooldown
	/// retries it once.
	pub tripped: Vec<String>,
	/// Breakers this sweep tripped (ROB-21): aspects whose consecutive failures
	/// reached the threshold during this sweep, so the store opened their breaker
	/// (logged once at `WARN` there). A sweep that only *skips* already-tripped
	/// aspects counts none here — those are in [`tripped`](Self::tripped) — so this
	/// is the trip-event count the `weft_maintenance_breaker_trips_total` metric
	/// reports, not a re-count of skips.
	pub breaker_trips: usize,
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
	/// Whether the sweep was cancelled before it finished (see
	/// [`MaintenanceCancel`](crate::MaintenanceCancel)). `true` means a cancellation request
	/// was received and the sweep stopped before the next aspect could start.
	pub cancelled: bool,
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
	/// Aspects the sweep did not attempt because their maintenance circuit breaker
	/// is tripped (see [`MaintenanceBreaker`]), in visit order. A tripped aspect
	/// counts only toward `aspects_scanned`; a sweep after the breaker's cooldown
	/// retries it once.
	pub tripped: Vec<String>,
	/// Breakers this sweep tripped (ROB-21): aspects whose consecutive failures
	/// reached the threshold during this sweep, so the store opened their breaker
	/// (logged once at `WARN` there). A sweep that only *skips* already-tripped
	/// aspects counts none here — those are in [`tripped`](Self::tripped) — so this
	/// is the trip-event count the `weft_maintenance_breaker_trips_total` metric
	/// reports, not a re-count of skips.
	pub breaker_trips: usize,
}

/// The outcome of a store-wide squash sweep, returned by
/// [`SegmentStore::squash_all_over_threshold`] — the per-tick numbers a background
/// squash daemon logs and exports.
///
/// Not `Clone`/`PartialEq`: [`failed`](Self::failed) carries the per-aspect
/// [`anyhow::Error`]s, which are neither.
#[derive(Debug, Default)]
pub struct SquashSweep {
	/// Whether the sweep was cancelled before it finished (see
	/// [`MaintenanceCancel`](crate::MaintenanceCancel)). `true` means a cancellation request
	/// was received and the sweep stopped before the next aspect could start.
	pub cancelled: bool,
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
	/// Aspects the sweep did not attempt because their maintenance circuit breaker
	/// is tripped (see [`MaintenanceBreaker`]), in visit order. A tripped aspect
	/// counts only toward `aspects_scanned`; a sweep after the breaker's cooldown
	/// retries it once.
	pub tripped: Vec<String>,
	/// Breakers this sweep tripped (ROB-21): aspects whose consecutive failures
	/// reached the threshold during this sweep, so the store opened their breaker
	/// (logged once at `WARN` there). A sweep that only *skips* already-tripped
	/// aspects counts none here — those are in [`tripped`](Self::tripped) — so this
	/// is the trip-event count the `weft_maintenance_breaker_trips_total` metric
	/// reports, not a re-count of skips.
	pub breaker_trips: usize,
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
	/// Verified snapshot of `metadata.db` (the rows-mode legacy catalog; the
	/// segment-set rollup lives in `segment_index.db` since S11).
	pub metadata: crate::SnapshotReport,
	/// Verified snapshot of `aspect_catalog.db` (the per-aspect declared schema).
	pub aspect_catalog: crate::SnapshotReport,
	/// Verified snapshot of `catalog.db` (the database/subject registry).
	pub registry: crate::SnapshotReport,
}

/// The outcome of a [`SegmentStore::backup_store`] run (D-S16): the control
/// plane plus every frame it references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WholeStoreBackup {
	/// The directory the backup was published under.
	pub dir: PathBuf,
	/// Verified snapshot of `segment_index.db`.
	pub segment_index: crate::SnapshotReport,
	/// Verified snapshot of `metadata.db`.
	pub metadata: crate::SnapshotReport,
	/// Verified snapshot of `aspect_catalog.db`.
	pub aspect_catalog: crate::SnapshotReport,
	/// Verified snapshot of `catalog.db`.
	pub registry: crate::SnapshotReport,
	/// The frames carried, as the manifest records them.
	pub frames: Vec<crate::ManifestFrame>,
	/// Bytes carried by hard links (no extra space while the store keeps them).
	pub linked_bytes: u64,
	/// Bytes carried by copies.
	pub copied_bytes: u64,
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
		let n = crate::types::num::to_f64(self.total_rows);
		let total = crate::types::num::to_f64(self.total_bytes);
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
	/// segment's** of the same series (TAG-6: each series is counted on its own, so two
	/// series sharing a window are not overlap) — the *cross-segment* order-health signal (roadmap Phase 4.6),
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
	use weft_physical_type::{timestamp::TimeUnit, PhysicalType, PAGED_SEGMENT_FORMAT_VERSION};

	use super::*;

	/// `WEFT_MAX_SERIES_PER_ASPECT` (TAG-11): a positive whole number sets the cap;
	/// unset, zero, or anything else keeps the default.
	#[test]
	fn max_series_per_aspect_reads_a_positive_whole_number() {
		assert_eq!(max_series_per_aspect_from(Some("7")), 7);
		assert_eq!(max_series_per_aspect_from(Some(" 12 ")), 12);
		for other in [None, Some(""), Some("0"), Some("-1"), Some("many")] {
			assert_eq!(max_series_per_aspect_from(other), DEFAULT_MAX_SERIES_PER_ASPECT, "{other:?}");
		}
	}
	use crate::{types::index_txn::TxnPoints, STORE_FORMAT_FILE};

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
		// The file round-trips back to the original data, through the
		// frame-reader API the store's read sites use (TAG-4).
		let (rt, rv) = frame_read::read_full(&descriptor.path, &on_disk).expect("decodes").decode_nullable();
		assert_eq!(rt, ts);
		assert_eq!(rv, vs.into_iter().map(Some).collect::<Vec<_>>());
	}

	/// D-S13: the same batch twice commits once. The first ingest returns
	/// its receipt; the second — and a third after a restart, answered from
	/// the ledger — return the same receipt marked duplicate, and the store
	/// holds exactly one segment with the batch's rows.
	#[tokio::test]
	async fn ingest_commits_once_for_the_same_batch_twice() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("price", &schema()).await.expect("declares");
		let batch = IngestBatch::dense("batch-1", vec![0, 10, 20], vec![bd("1"), bd("2"), bd("3")]);
		let first = store.ingest("price", &batch).await.expect("ingests");
		assert!(!first.duplicate, "the first commit is not a duplicate");
		assert_eq!(first.segment_ids.len(), 1, "one segment for a small batch");
		assert_eq!(first.series_ids, vec![0], "the empty group is series 0");
		assert_eq!((first.row_count, first.min_ts, first.max_ts), (3, Some(0), Some(20)));
		let second = store.ingest("price", &batch).await.expect("replays");
		assert!(second.duplicate, "the replay is marked duplicate");
		assert_eq!(
			{
				let mut r = second.clone();
				r.duplicate = false;
				r
			},
			first,
			"a replay returns the recorded receipt"
		);
		assert_eq!(store.segment_count("price").await.expect("counts"), 1, "the same batch twice commits once");
		let entry = store.index.ledger_entry("price", "batch-1").await.expect("reads the ledger").expect("a ledger row");
		assert_eq!(entry.fingerprint, first.fingerprint);
		assert_eq!(entry.series_ids, vec![0], "the ledger records series_ids (C1)");
		assert_eq!((entry.id_lo, entry.id_hi), (Some(first.segment_ids[0]), Some(first.segment_ids[0])));
		let (ts, vs) = store.read_time_range("price", 0, 20).await.expect("reads");
		assert_eq!(ts, vec![0, 10, 20]);
		assert_eq!(vs, vec![Some(bd("1")), Some(bd("2")), Some(bd("3"))]);
		drop(store);
		// After a restart the replay is answered from the ledger, not memory.
		let reopened = SegmentStore::open(dir.path()).await.expect("reopens");
		let third = reopened.ingest("price", &batch).await.expect("replays after restart");
		assert!(third.duplicate);
		assert_eq!(third.segment_ids, first.segment_ids);
		assert_eq!(reopened.segment_count("price").await.expect("counts"), 1);
	}

	/// D-S14: a paged ingest batch seals a paged (format v6) frame through
	/// the same entry point, and replays like any other keyed batch.
	#[tokio::test]
	async fn ingest_with_rows_per_page_seals_a_paged_frame() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("price", &schema()).await.expect("declares");
		let batch = IngestBatch::dense("paged-1", vec![0, 10, 20, 30], vec![bd("1"), bd("2"), bd("3"), bd("4")]).with_rows_per_page(Some(2));
		let first = store.ingest("price", &batch).await.expect("ingests");
		assert!(!first.duplicate);
		let descriptors = store.index.all("price").await.expect("descriptors");
		assert_eq!(descriptors.len(), 1);
		assert_eq!(descriptors[0].format_version, weft_physical_type::PAGED_SEGMENT_FORMAT_VERSION, "the paged batch sealed a paged frame");
		assert_eq!(descriptors[0].id, first.segment_ids[0]);
		let second = store.ingest("price", &batch).await.expect("replays");
		assert!(second.duplicate, "the paged batch replays under its key");
		assert_eq!(second.segment_ids, first.segment_ids);
		let (ts, vs) = store.read_time_range("price", 0, 30).await.expect("reads");
		drop(store);
		assert_eq!(ts, vec![0, 10, 20, 30]);
		assert_eq!(vs.len(), 4);
	}

	/// D-S13: a different batch under a recorded key is a typed conflict,
	/// and nothing more is written.
	#[tokio::test]
	async fn ingest_refuses_a_different_batch_under_a_recorded_key() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("price", &schema()).await.expect("declares");
		store.ingest("price", &IngestBatch::dense("k", vec![0], vec![bd("1")])).await.expect("ingests");
		let err = store.ingest("price", &IngestBatch::dense("k", vec![0], vec![bd("2")])).await.expect_err("a different batch under the key conflicts");
		assert!(matches!(err.downcast_ref::<StoreError>(), Some(StoreError::IdempotencyConflict { aspect, key, .. }) if aspect == "price" && key == "k"), "{err:#}");
		assert_eq!(store.segment_count("price").await.expect("counts"), 1, "the conflict wrote nothing");
		// A different key for the same rows is a new batch, and commits.
		let other = store.ingest("price", &IngestBatch::dense("other", vec![0], vec![bd("1")])).await.expect("a new key commits");
		assert!(!other.duplicate);
		assert_eq!(store.segment_count("price").await.expect("counts"), 2);
	}

	/// D-S13: the ingest refusals are typed (`#[non_exhaustive]`
	/// [`StoreError`] variants), including X-ERR's `UnknownAspect` from
	/// `require_schema`, which every declared-schema entry point now shares.
	#[tokio::test]
	async fn ingest_refusals_are_typed() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		let batch = IngestBatch::dense("k", vec![0], vec![bd("1")]);
		let err = store.ingest("undeclared", &batch).await.expect_err("no declared schema");
		assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::UnknownAspect { aspect: "undeclared".to_string() }), "{err:#}");
		let err = store.seal_declared("undeclared", &[0], &[bd("1")]).await.expect_err("seal_declared shares require_schema");
		assert!(matches!(err.downcast_ref::<StoreError>(), Some(StoreError::UnknownAspect { .. })), "{err:#}");
		store.declare("price", &schema()).await.expect("declares");
		// TAG-8: the library gate is open, but a reserved `__` key is still refused.
		let reserved = batch.clone().with_groups(crate::TagSet::from(crate::SeriesKey::from_canonical(b"__dim\x1fs").expect("a stored reserved key")));
		let err = store.ingest("price", &reserved).await.expect_err("a reserved tag key");
		assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::ReservedTagKey { aspect: "price".to_string(), key: "__dim".to_string() }), "{err:#}");
		let err = store.ingest("price", &IngestBatch::dense("k", vec![], vec![])).await.expect_err("an empty batch");
		assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::EmptyIngestBatch { aspect: "price".to_string() }), "{err:#}");
		let err = store.ingest("price", &IngestBatch::new("k", vec![0, 10], vec![Some(bd("1"))])).await.expect_err("mismatched columns");
		assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::IngestLengthMismatch { timestamps: 2, values: 1 }), "{err:#}");
		assert_eq!(store.segment_count("price").await.expect("counts"), 0, "every refusal wrote nothing");
	}

	/// D-S13: the frame-target cut never divides a run of equal timestamps
	/// and never loses or reorders a row.
	#[test]
	fn chunk_ingest_rows_never_splits_a_timestamp_run() {
		// Each value's plain form is ~200k digits, so the estimate charges
		// ~200 KB a row and a 128 MiB chunk holds a few hundred rows. The
		// exponent form keeps the mantissa tiny, so the arithmetic digit
		// count is cheap.
		let big = bd("1e200000");
		let timestamps: Vec<i64> = (0..2_000).map(|i| i / 3).collect();
		let values: Vec<Option<BigDecimal>> = (0..2_000).map(|_| Some(big.clone())).collect();
		let chunks = chunk_ingest_rows(&timestamps, &values, crate::MAX_ENCODED_FRAME_BYTES);
		assert!(chunks.len() >= 3, "a ~400 MB batch is cut: {} chunk(s)", chunks.len());
		let mut flat_ts: Vec<i64> = Vec::new();
		for (chunk_ts, chunk_vs) in &chunks {
			assert_eq!(chunk_ts.len(), chunk_vs.len());
			assert!(chunk_ts.len() <= 700, "a chunk holds {} rows, over the estimate's few hundred", chunk_ts.len());
			if let (Some(last), Some(_)) = (flat_ts.last(), chunk_ts.first()) {
				assert_ne!(last, chunk_ts.first().expect("non-empty"), "a chunk boundary divides a run of equal timestamps");
			}
			flat_ts.extend(chunk_ts.iter().copied());
		}
		assert_eq!(flat_ts, timestamps, "the cut loses and reorders nothing");
	}

	/// D-S10: a seal writes a write-once frame named by its generation, not by its
	/// id, and the id is assigned by the commit that publishes it: two seals of one
	/// aspect take consecutive ids in commit order under generation-named frames
	/// that no later seal reuses, and the frame and sidecar builders name those
	/// frames through `aspect_name::encode` directly under `segments/`.
	#[tokio::test]
	async fn a_seal_is_write_once_with_its_id_assigned_at_commit() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// The builders themselves: an encoded name, contained in `segments/`.
		let frame = store.segment_path("Room-A.temp", 3, 3).expect("frame path");
		assert_eq!(frame, dir.path().join("segments").join("%52oom%2D%41%2Etemp~g3~p3.weftseg"), "the frame name encodes the aspect");
		let sidecar = store.sidecar_path("Room-A.temp", 3, 3).expect("sidecar path");
		assert_eq!(sidecar, dir.path().join("segments").join("%52oom%2D%41%2Etemp~g3~p3.weftpart"), "the sidecar shares the frame's encoded stem");
		assert!(store.segment_path("../x", 1, 1).is_err(), "a traversing aspect is refused by the builder");
		// Two seals: ids in commit order, frames named by generation, never by id.
		let first = store.seal("price", &schema(), &[0, 10], &[bd("1"), bd("2")]).await.expect("seals");
		let second = store.seal("price", &schema(), &[20, 30], &[bd("3"), bd("4")]).await.expect("seals");
		assert_eq!((first.id, second.id), (0, 1), "ids are assigned in commit order");
		let first_file = std::path::Path::new(&first.path).file_name().and_then(|name| name.to_str()).unwrap_or_default().to_string();
		let second_file = std::path::Path::new(&second.path).file_name().and_then(|name| name.to_str()).unwrap_or_default().to_string();
		assert_eq!(first_file, "price~g1~p1.weftseg", "the first seal's frame is generation-named");
		assert_eq!(second_file, "price~g2~p2.weftseg", "the second seal's frame is generation-named");
		assert!(!first_file.contains("-0") && !second_file.contains("-1"), "no frame is named by its id");
		// The rows read back under the committed ids, and the frames they name are
		// the ones on disk: the row's path is the frame's, not an id-derived alias.
		let descriptors = store.index().all("price").await.expect("reads the index");
		assert_eq!(descriptors.iter().map(|desc| desc.id).collect::<Vec<_>>(), vec![0, 1]);
		assert_eq!(descriptors.iter().map(|desc| desc.path.clone()).collect::<Vec<_>>(), vec![first.path.clone(), second.path.clone()]);
		let (ts, vs) = store.read_time_range("price", i64::MIN, i64::MAX).await.expect("reads");
		drop(store);
		assert_eq!(ts, vec![0, 10, 20, 30]);
		assert_eq!(vs.into_iter().flatten().map(|v| v.to_plain_string()).collect::<Vec<_>>(), vec!["1", "2", "3", "4"]);
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

	/// D-S16: a whole-store backup links the frames its snapshot references,
	/// its manifest embeds the store's `STORE_FORMAT` and allocators, a
	/// control-plane restore refuses it, and `restore_store` round-trips it
	/// into a fresh root that opens and reads — with the live root gone.
	#[tokio::test]
	#[serial(backup_fault_points)]
	async fn whole_store_backup_links_frames_and_restores() {
		let dir = TempDir::new().unwrap();
		let root = dir.path().join("live");
		let store = populated_store(&root).await;
		let backup_dir = dir.path().join("whole");
		let backup = store.backup_store(&backup_dir, crate::BackupMode::Linked).await.expect("whole-store backup");
		assert_eq!(backup.frames.len(), 1, "one sealed frame is carried");
		assert!(backup.frames[0].linked, "same-filesystem frames are linked");
		assert_eq!(backup.linked_bytes, backup.frames[0].bytes);
		assert_eq!(backup.copied_bytes, 0);
		#[cfg(unix)]
		{
			use std::os::unix::fs::MetadataExt;
			let live = std::fs::metadata(root.join("segments").join(&backup.frames[0].name)).unwrap();
			let linked = std::fs::metadata(backup_dir.join("segments").join(&backup.frames[0].name)).unwrap();
			assert_eq!(live.ino(), linked.ino(), "a linked frame shares the store frame's inode");
		}
		let manifest = crate::BackupManifest::read(&backup_dir).await.expect("reads").expect("a manifest");
		assert!(manifest.is_whole_store());
		assert_eq!(manifest.store_format.as_ref().expect("embedded").store_uuid, store.store_uuid(), "the manifest embeds the store's STORE_FORMAT");
		assert_eq!(manifest.allocators.len(), 1, "the aspect's allocator is recorded");
		assert_eq!(manifest.frames.len(), 1);
		let uuid = store.store_uuid().to_string();
		drop(store);
		// A control-plane restore refuses a whole-store backup.
		let err = crate::restore_control_plane(&backup_dir, &dir.path().join("cp")).await.unwrap_err();
		assert!(err.to_string().contains("whole-store"), "got: {err}");
		// The whole-store restore, then the live root is gone (disk loss).
		let restored_root = dir.path().join("restored");
		let report = crate::restore_store(&backup_dir, &restored_root).await.expect("restores");
		assert_eq!(report.frames, 1);
		assert_eq!(report.intent.store_uuid, uuid);
		assert!(restored_root.join(crate::RESTORE_INTENT_FILE).exists(), "the intent marker is published with the root (C-7)");
		tokio::fs::remove_dir_all(&root).await.unwrap();
		let reopened = SegmentStore::open(&restored_root).await.expect("restored store opens");
		assert!(!restored_root.join(crate::RESTORE_INTENT_FILE).exists(), "the first open completes the restore and removes the intent marker");
		let (times, values) = reopened.read_time_range("price", i64::MIN, i64::MAX).await.expect("reads back");
		assert_eq!(times, vec![0, 10, 20]);
		assert_eq!(values.into_iter().flatten().map(|v| v.to_plain_string()).collect::<Vec<_>>(), vec!["1", "2", "3"]);
		drop(reopened);
	}

	/// D-S16: a backup whose embedded `STORE_FORMAT` floor is newer than this
	/// build is refused before anything is staged.
	#[tokio::test]
	#[serial(backup_fault_points)]
	async fn whole_store_restore_refuses_a_newer_floor() {
		let dir = TempDir::new().unwrap();
		let root = dir.path().join("live");
		let store = populated_store(&root).await;
		let backup_dir = dir.path().join("whole");
		store.backup_store(&backup_dir, crate::BackupMode::Copy).await.expect("whole-store backup");
		drop(store);
		let manifest_path = backup_dir.join(crate::BACKUP_MANIFEST);
		let mut json: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
		json["store_format"]["min_write_layout"] = serde_json::json!(99);
		std::fs::write(&manifest_path, serde_json::to_vec(&json).unwrap()).unwrap();
		let err = crate::restore_store(&backup_dir, &dir.path().join("restored")).await.unwrap_err();
		assert!(format!("{err:#}").contains("newer"), "got: {err:#}");
		assert!(!dir.path().join("restored").exists(), "a refused restore publishes nothing");
	}

	/// D-S16, R8, TAG-9: at the restored root's first open, a post-backup
	/// orphan is adopted via `describe_frame` under a fresh id, and an orphan
	/// carrying a canonical `SERIES_KEY` extension is adopted into the series its
	/// key names — registered in the catalog in the same transaction — rather
	/// than held.
	#[tokio::test]
	#[serial(backup_fault_points)]
	async fn restore_mode_adopts_series_key_frames_into_their_series() {
		let dir = TempDir::new().unwrap();
		let root = dir.path().join("live");
		let store = populated_store(&root).await;
		let backup_dir = dir.path().join("whole");
		store.backup_store(&backup_dir, crate::BackupMode::Linked).await.expect("whole-store backup");
		drop(store);
		let restored_root = dir.path().join("restored");
		crate::restore_store(&backup_dir, &restored_root).await.expect("restores");
		// Two post-backup frames appear beside the restored ones (the
		// restore-beside-newer-frames case): one plain, one SERIES_KEY.
		write_frame(&restored_root, "price", 7, &[(30, "4"), (40, "5")]);
		std::fs::write(restored_root.join("segments").join("price-8.weftseg"), tagged_frame(&[50, 60], b"a\x1f1")).unwrap();

		let reopened = SegmentStore::open(&restored_root).await.expect("restored store opens");
		let report = reopened.recovery_report();
		assert_eq!(report.adopted, 2, "both orphans are adopted: {report:?}");
		assert_eq!(report.held, 0, "a tagged orphan of a declared aspect is not held: {report:?}");
		let (times, _) = reopened.read_time_range("price", i64::MIN, i64::MAX).await.expect("reads");
		// Reads target one series (TAG-8): series 0 serves the plain frame's rows, and
		// the tagged frame's rows are read from the series its key names.
		assert_eq!(times, vec![0, 10, 20, 30, 40], "the plain adopted frame's rows are series 0's");
		let key = crate::SeriesKey::from_canonical(b"a\x1f1").unwrap();
		let id = reopened.index().series_id_for("price", &key).await.expect("catalog").expect("the key is registered");
		let (times, _) = reopened.read_time_range_of("price", id, i64::MIN, i64::MAX).await.expect("reads");
		assert_eq!(times, vec![50, 60], "the tagged adopted frame's rows are readable in their series");
		assert_ne!(id, crate::SeriesId::UNTAGGED);
		let rows = reopened.index().rows("price").await.expect("rows");
		let tagged: Vec<_> = rows.iter().filter(|row| row.desc.path.ends_with("price-8.weftseg")).collect();
		assert_eq!(tagged.len(), 1);
		assert_eq!(tagged[0].series_id, id.0, "the adopted row names the key's series");
		assert!(reopened.index().journal(None).await.expect("journal").is_empty(), "nothing is held");
		// The rollup is rebuilt after the orphan pass, so it counts both adopted frames.
		let metadata = reopened.aspect_metadata("price").await.expect("metadata");
		assert_eq!(metadata.total_rows, 7, "{metadata:?}");
		assert_eq!(metadata.time_range.map(|(_, max)| max), Some(60), "{metadata:?}");
		drop(reopened);
		// A second open finds the frame indexed and rebinds nothing.
		let reopened = SegmentStore::open(&restored_root).await.expect("reopens");
		let report = reopened.recovery_report();
		assert_eq!((report.rebound, report.series_rebound, report.orphans), (0, 0, 0), "{report:?}");
		drop(reopened);
	}

	/// TAG-9: a tagged orphan of an aspect the restored store has not declared
	/// is held at the restore; once the aspect is declared, the next open
	/// adopts it into its series and clears the journal row.
	#[tokio::test]
	#[serial(backup_fault_points)]
	async fn a_held_frame_is_adopted_on_the_next_open() {
		let dir = TempDir::new().unwrap();
		let root = dir.path().join("live");
		let store = populated_store(&root).await;
		let backup_dir = dir.path().join("whole");
		store.backup_store(&backup_dir, crate::BackupMode::Linked).await.expect("whole-store backup");
		drop(store);
		let restored_root = dir.path().join("restored");
		crate::restore_store(&backup_dir, &restored_root).await.expect("restores");
		std::fs::write(restored_root.join("segments").join("volume-1.weftseg"), tagged_frame(&[50, 60], b"a\x1f1")).unwrap();

		let reopened = SegmentStore::open(&restored_root).await.expect("restored store opens");
		let report = reopened.recovery_report();
		assert_eq!((report.adopted, report.held), (0, 1), "the undeclared aspect's tagged orphan is held: {report:?}");
		reopened.declare("volume", &schema()).await.expect("declares");
		drop(reopened);

		let reopened = SegmentStore::open(&restored_root).await.expect("reopens");
		let report = reopened.recovery_report();
		assert_eq!((report.adopted, report.held), (1, 0), "the held frame is released: {report:?}");
		assert!(reopened.index().journal(None).await.expect("journal").is_empty(), "the journal row is cleared with the adoption");
		let key = crate::SeriesKey::from_canonical(b"a\x1f1").unwrap();
		let id = reopened.index().series_id_for("volume", &key).await.expect("catalog").expect("registered");
		let rows = reopened.index().rows("volume").await.expect("rows");
		assert_eq!(rows.len(), 1);
		assert_eq!(rows[0].series_id, id.0);
		drop(reopened);
	}

	/// Restore a populated store, hold a tagged `volume-1.weftseg` orphan (its
	/// aspect undeclared at the restore), then declare `volume`. Returns the
	/// restored root, the held frame's path and the still-open store.
	async fn restored_with_held_frame(dir: &TempDir) -> (PathBuf, PathBuf, SegmentStore) {
		let root = dir.path().join("live");
		let store = populated_store(&root).await;
		let backup_dir = dir.path().join("whole");
		store.backup_store(&backup_dir, crate::BackupMode::Linked).await.expect("whole-store backup");
		drop(store);
		let restored_root = dir.path().join("restored");
		crate::restore_store(&backup_dir, &restored_root).await.expect("restores");
		let held = restored_root.join("segments").join("volume-1.weftseg");
		std::fs::write(&held, tagged_frame(&[50, 60], b"a\x1f1")).unwrap();
		let reopened = SegmentStore::open(&restored_root).await.expect("restored store opens");
		assert_eq!(reopened.recovery_report().held, 1);
		reopened.declare("volume", &schema()).await.expect("declares");
		(restored_root, held, reopened)
	}

	/// TAG-9: a held frame whose `SERIES_KEY` payload is malformed is quarantined
	/// suspect on the next open, its journal row cleared in the same pass.
	#[tokio::test]
	#[serial(backup_fault_points)]
	async fn a_held_frame_with_a_malformed_key_is_suspect() {
		let dir = TempDir::new().unwrap();
		let (root, held, store) = restored_with_held_frame(&dir).await;
		drop(store);
		std::fs::write(&held, tagged_frame(&[50, 60], b"volume|a=1")).unwrap();
		let store = SegmentStore::open(&root).await.expect("reopens");
		let report = store.recovery_report();
		assert_eq!((report.suspected, report.adopted, report.held_cleared), (1, 0, 0), "{report:?}");
		assert!(store.index().journal(None).await.expect("journal").is_empty(), "the journal row is cleared");
		drop(store);
		assert!(root.join("segments").join("quarantine").join("volume-1.weftseg").exists(), "the frame moved to quarantine");
		assert!(!held.exists());
	}

	/// TAG-9: a held frame that decayed (its CRC no longer verifies) is
	/// quarantined under its decode class rather than kept held forever.
	#[tokio::test]
	#[serial(backup_fault_points)]
	async fn a_decayed_held_frame_is_quarantined() {
		let dir = TempDir::new().unwrap();
		let (root, held, store) = restored_with_held_frame(&dir).await;
		drop(store);
		let mut bytes = std::fs::read(&held).unwrap();
		let middle = bytes.len() / 2;
		bytes[middle] ^= 0xff;
		std::fs::write(&held, bytes).unwrap();
		let store = SegmentStore::open(&root).await.expect("reopens");
		let report = store.recovery_report();
		assert_eq!(report.quarantined + report.suspected, 1, "{report:?}");
		assert_eq!(report.adopted, 0, "{report:?}");
		assert!(store.index().journal(None).await.expect("journal").is_empty(), "the journal row is cleared");
		drop(store);
		assert!(!held.exists(), "the frame left segments/");
	}

	/// TAG-9: a held row whose frame is gone is cleared and counted, not kept.
	#[tokio::test]
	#[serial(backup_fault_points)]
	async fn a_held_row_without_its_frame_is_cleared() {
		let dir = TempDir::new().unwrap();
		let (root, held, store) = restored_with_held_frame(&dir).await;
		drop(store);
		std::fs::remove_file(&held).unwrap();
		let store = SegmentStore::open(&root).await.expect("reopens");
		let report = store.recovery_report();
		assert_eq!((report.held_cleared, report.adopted), (1, 0), "{report:?}");
		assert_eq!(store.index().journal(None).await.expect("journal"), [] as [crate::types::reaper::JournalEntry; 0]);
		drop(store);
	}

	/// TAG-9: a report-only pass over a held row counts the adoption it would
	/// make and leaves the journal row, the catalog and the index untouched.
	#[tokio::test]
	#[serial(backup_fault_points)]
	async fn report_only_leaves_a_held_row_alone() {
		let dir = TempDir::new().unwrap();
		let (_, _, store) = restored_with_held_frame(&dir).await;
		let journal = store.index().journal(None).await.expect("journal");
		assert_eq!(journal.len(), 1, "the row is held before the pass");
		let report = store.fsck(true).await.expect("fsck");
		assert_eq!((report.adopted, report.held_cleared), (1, 0), "{report:?}");
		assert_eq!(store.index().journal(None).await.expect("journal").len(), 1, "the journal row stays");
		assert!(store.index().rows("volume").await.expect("rows").is_empty(), "nothing is indexed");
		assert!(store.index().series_of("volume").await.expect("catalog").is_empty(), "nothing is registered");
	}

	/// TAG-9: a restore-mode orphan whose `SERIES_KEY` payload is malformed is
	/// quarantined suspect, not adopted.
	#[tokio::test]
	#[serial(backup_fault_points)]
	async fn a_restore_orphan_with_a_malformed_key_is_suspect() {
		let dir = TempDir::new().unwrap();
		let root = dir.path().join("live");
		let store = populated_store(&root).await;
		let backup_dir = dir.path().join("whole");
		store.backup_store(&backup_dir, crate::BackupMode::Linked).await.expect("whole-store backup");
		drop(store);
		let restored_root = dir.path().join("restored");
		crate::restore_store(&backup_dir, &restored_root).await.expect("restores");
		std::fs::write(restored_root.join("segments").join("price-8.weftseg"), tagged_frame(&[50, 60], b"price|a=1")).unwrap();
		let store = SegmentStore::open(&restored_root).await.expect("restored store opens");
		let report = store.recovery_report();
		assert_eq!((report.suspected, report.adopted, report.held), (1, 0, 0), "{report:?}");
		let records = store.quarantine_list(None).await.expect("records");
		drop(store);
		assert_eq!(records.len(), 1);
		assert_eq!(records[0].class, QuarantineClass::Suspect);
	}

	/// TAG-9: a `SERIES_KEY` extension marked non-critical is still the frame's
	/// key: the row is rebound to it, not refused.
	#[tokio::test]
	async fn a_non_critical_series_key_is_accepted() {
		let dir = TempDir::new().unwrap();
		let store = populated_store(dir.path()).await;
		let rows = store.index().rows("price").await.expect("rows");
		let values: Vec<BigDecimal> = [0i64, 10, 20].iter().map(|ts| bd(&(ts / 10).to_string())).collect();
		let segment = schema().seal(&[0, 10, 20], &values).expect("encodes");
		let bytes = weft_physical_type::wrap_frame_envelope(&segment.write_to(), &[weft_physical_type::FrameExtension { id: weft_physical_type::EXT_SERIES_KEY, critical: false, payload: b"a\x1f1".to_vec() }]).expect("wraps");
		std::fs::write(&rows[0].desc.path, bytes).unwrap();
		let report = store.fsck(false).await.expect("fsck");
		assert_eq!((report.series_rebound, report.suspected, report.quarantined), (1, 0, 0), "{report:?}");
		let key = crate::SeriesKey::from_canonical(b"a\x1f1").unwrap();
		assert!(store.index().series_id_for("price", &key).await.expect("catalog").is_some());
		drop(store);
	}

	/// TAG-9: an untagged store's fsck rebinds nothing and registers nothing.
	#[tokio::test]
	async fn an_untagged_store_sees_no_series_work() {
		let dir = TempDir::new().unwrap();
		let store = populated_store(dir.path()).await;
		let report = store.fsck(false).await.expect("fsck");
		assert_eq!((report.series_rebound, report.rebound, report.suspected, report.quarantined), (0, 0, 0, 0), "{report:?}");
		assert_eq!(store.index().series_of("price").await.expect("catalog"), [] as [(crate::SeriesId, weft_physical_type::SeriesKey); 0]);
		assert!(store.index().rows("price").await.expect("rows").iter().all(|row| row.series_id == 0));
		drop(store);
	}

	/// TAG-9, R10: with the allocator row lost but the catalog intact, a row
	/// rebound during recovery registers its key past the catalog's ids rather
	/// than colliding with one (the seed runs before any row is checked).
	#[tokio::test]
	async fn a_rebind_after_a_lost_allocator_does_not_reuse_a_catalog_id() {
		let dir = TempDir::new().unwrap();
		let root = dir.path().join("store");
		let store = populated_store(&root).await;
		let taken = crate::SeriesKey::from_canonical(b"a\x1f1").unwrap();
		store.commit_index(&IndexTxn::new(vec![IndexOp::RegisterSeries { aspect: "price".to_string(), series_id: crate::SeriesId(1), series_key: taken.clone() }, IndexOp::SeedSeries { aspect: "price".to_string(), next_series_id: 2 }])).await.expect("registers");
		retag_indexed_frame(&store, b"b\x1f2").await;
		drop(store);
		on_raw_index(&root, &["DELETE FROM aspect_seq"]).await;
		let store = SegmentStore::open(&root).await.expect("recovery succeeds");
		let report = store.recovery_report();
		assert_eq!(report.series_rebound, 1, "{report:?}");
		let key = crate::SeriesKey::from_canonical(b"b\x1f2").unwrap();
		let id = store.index().series_id_for("price", &key).await.expect("catalog").expect("registered");
		assert_eq!(id, crate::SeriesId(2), "past the catalog's id 1");
		assert_eq!(store.index().series_id_for("price", &taken).await.expect("catalog"), Some(crate::SeriesId(1)));
		assert_eq!(store.index().rows("price").await.expect("rows")[0].series_id, 2);
		drop(store);
	}

	/// A v7 frame of `[(ts, ts / 10)]` rows wrapped with one critical
	/// `SERIES_KEY` extension carrying `payload`.
	fn tagged_frame(timestamps: &[i64], payload: &[u8]) -> Vec<u8> {
		let values: Vec<BigDecimal> = timestamps.iter().map(|ts| bd(&(ts / 10).to_string())).collect();
		let segment = schema().seal(timestamps, &values).expect("encodes");
		weft_physical_type::wrap_frame_envelope(&segment.write_to(), &[weft_physical_type::FrameExtension { id: weft_physical_type::EXT_SERIES_KEY, critical: true, payload: payload.to_vec() }]).expect("wraps")
	}

	/// Rewrite the populated store's one indexed frame as a tagged frame
	/// carrying `payload`, keeping its rows, and return the row.
	async fn retag_indexed_frame(store: &SegmentStore, payload: &[u8]) -> IndexRow {
		let rows = store.index().rows("price").await.expect("rows");
		assert_eq!(rows.len(), 1);
		std::fs::write(&rows[0].desc.path, tagged_frame(&[0, 10, 20], payload)).unwrap();
		rows[0].clone()
	}

	/// TAG-9, C5: an indexed frame whose CRC verifies and whose body decodes
	/// but whose `SERIES_KEY` payload is not canonical is suspect: quarantined,
	/// never purged, its row deleted.
	#[tokio::test]
	async fn a_malformed_series_key_payload_is_suspect() {
		let dir = TempDir::new().unwrap();
		let store = populated_store(dir.path()).await;
		retag_indexed_frame(&store, b"price|a=1").await;
		let report = store.fsck(false).await.expect("fsck");
		assert_eq!((report.suspected, report.quarantined, report.series_rebound), (1, 0, 0), "{report:?}");
		assert!(store.index().rows("price").await.expect("rows").is_empty(), "the row is deleted");
		let records = store.quarantine_list(None).await.expect("records");
		drop(store);
		assert_eq!(records.len(), 1);
		assert_eq!(records[0].class, QuarantineClass::Suspect);
		assert!(records[0].reason.contains("SERIES_KEY"), "{}", records[0].reason);
	}

	/// TAG-9: an indexed row whose series id names a different key than its
	/// frame's `SERIES_KEY` is rebound to the frame's series, registered in the
	/// same transaction.
	#[tokio::test]
	async fn a_series_key_mismatch_rebinds_the_row() {
		let dir = TempDir::new().unwrap();
		let store = populated_store(dir.path()).await;
		let before = retag_indexed_frame(&store, b"a\x1f1").await;
		assert_eq!(before.series_id, 0);
		let report = store.fsck(false).await.expect("fsck");
		assert_eq!((report.series_rebound, report.rebound, report.suspected), (1, 1, 0), "{report:?}");
		let key = crate::SeriesKey::from_canonical(b"a\x1f1").unwrap();
		let id = store.index().series_id_for("price", &key).await.expect("catalog").expect("registered");
		let rows = store.index().rows("price").await.expect("rows");
		assert_eq!(rows.len(), 1);
		assert_eq!(rows[0].series_id, id.0, "the row names the frame's series");
		assert_eq!(rows[0].desc.id, before.desc.id, "only the series binding changed");
		let again = store.fsck(false).await.expect("fsck");
		drop(store);
		assert_eq!((again.series_rebound, again.rebound), (0, 0), "a second pass finds nothing to rebind: {again:?}");
	}

	/// TAG-9, critique E19: a row moved back to the aspect its frame's name
	/// names does not carry the series id it held in the wrong aspect — its
	/// frame's key is resolved again in the target aspect's own catalog.
	#[tokio::test]
	async fn an_aspect_rebind_resolves_the_key_in_the_target_aspect() {
		let dir = TempDir::new().unwrap();
		let store = populated_store(dir.path()).await;
		let row = retag_indexed_frame(&store, b"a\x1f1").await;
		let key = crate::SeriesKey::from_canonical(b"a\x1f1").unwrap();
		// Misfile the row under `volume`, as series 3 of volume's catalog.
		let misfiled = IndexRow { series_id: 3, ..row.clone() };
		store.commit_index(&IndexTxn::new(vec![IndexOp::RegisterSeries { aspect: "volume".to_string(), series_id: crate::SeriesId(3), series_key: key.clone() }, IndexOp::SeedSeries { aspect: "volume".to_string(), next_series_id: 4 }, IndexOp::DeleteExpected { aspect: "price".to_string(), expected: row.version() }, IndexOp::InsertNew { aspect: "volume".to_string(), row: misfiled }])).await.expect("misfiles");
		let report = store.fsck(false).await.expect("fsck");
		assert_eq!((report.rebound, report.series_rebound), (1, 1), "{report:?}");
		assert!(store.index().rows("volume").await.expect("rows").is_empty(), "the row left the wrong aspect");
		let id = store.index().series_id_for("price", &key).await.expect("catalog").expect("registered in price");
		assert_eq!(id, crate::SeriesId(1), "price's catalog hands out its own first id, not volume's 3");
		let rows = store.index().rows("price").await.expect("rows");
		drop(store);
		assert_eq!(rows.len(), 1);
		assert_eq!(rows[0].series_id, id.0);
	}

	/// TAG-9: a report-only pass counts the series rebind it would make and
	/// changes nothing: no catalog row, no row rewrite.
	#[tokio::test]
	async fn report_only_rebinds_nothing() {
		let dir = TempDir::new().unwrap();
		let store = populated_store(dir.path()).await;
		let before = retag_indexed_frame(&store, b"a\x1f1").await;
		let report = store.fsck(true).await.expect("fsck");
		assert_eq!(report.series_rebound, 1, "{report:?}");
		let key = crate::SeriesKey::from_canonical(b"a\x1f1").unwrap();
		assert_eq!(store.index().series_id_for("price", &key).await.expect("catalog"), None, "no series is registered");
		let rows = store.index().rows("price").await.expect("rows");
		drop(store);
		assert_eq!(rows, vec![before], "the row is untouched");
	}

	/// TAG-9: a report-only pass leaves a malformed `SERIES_KEY` frame in place.
	#[tokio::test]
	async fn report_only_leaves_a_malformed_series_key_frame_alone() {
		let dir = TempDir::new().unwrap();
		let store = populated_store(dir.path()).await;
		let before = retag_indexed_frame(&store, b"price|a=1").await;
		let report = store.fsck(true).await.expect("fsck");
		assert_eq!(report.suspected, 1, "{report:?}");
		assert_eq!(store.index().rows("price").await.expect("rows"), vec![before]);
		assert_eq!(store.quarantine_list(None).await.expect("records"), [] as [crate::types::quarantine::QuarantineRecord; 0]);
		drop(store);
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
		drop(reopened);
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
	#[cfg_attr(windows, ignore = "Windows file locks are mandatory: this test images or reads the files of a store that is still open, which Turso holds locked (ERROR_LOCK_VIOLATION). Linux and macOS CI run it; Windows crash coverage is the process-kill tests")]
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
				assert_eq!(names_in(&base), std::slice::from_ref(&label), "{point}: published before the error");
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
		// Declare a schema and seal two aspects, so the control-plane DBs hold rows:
		// aspect_catalog (declare), segment_index (seal, including the S11 rollup rows
		// folded into its seal transactions), catalog (open registers). `metadata.db`
		// holds no rollup rows any more: since S11 the rollup lives in
		// `segment_index.db`, and `metadata.db` carries only the rows-mode legacy
		// catalog, which this fixture never imports.
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
		assert_eq!(backup.metadata.rows, 0, "no rollup rows in metadata.db since S11");
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
		let mut aspects = restored.index().rollup_aspects().await.expect("lists");
		drop(restored);
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
		assert_eq!(ts3, [] as [i64; 0]);
		assert_eq!(vs3, [] as [Option<bigdecimal::BigDecimal>; 0]);
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

	/// Quality pruning on the store's point path (roadmap Phase 4.3): a sealed segment that
	/// carries no value at all is dropped by the index prune, so a point read never opens its
	/// file. The corrupt bytes left in its place are what prove it — the read can only come
	/// back clean if that segment was skipped instead of read. A range read, whose contract
	/// includes null rows, still opens it and reports the damage; a downsample, whose contract
	/// is the buckets, does not.
	#[tokio::test]
	async fn point_reads_prune_an_all_null_segment_without_opening_it() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		// A downsample resolves the aspect's schema before it reads anything, so the fixture has
		// to declare it: without this line `downsample_range` failed on "no declared schema"
		// instead of on the file, which is exactly the vacuous pass the old `is_err()` assertion
		// was accepting here. `read_time_range` never looked at the schema, which is why the same
		// vacuity hid behind its corrupt-file assertion.
		store.declare("a", &schema()).await.expect("declares");
		let _live = store.seal("a", &schema(), &[0_i64, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.expect("seals the value segment");
		let nulls = store.seal_nullable("a", &schema(), &[30_i64, 40], &[None, None]).await.expect("seals the all-null segment");
		assert_eq!(store.segment_count("a").await.expect("counts"), 2, "both segments are indexed");
		// The all-null segment's file becomes bytes no reader can parse.
		tokio::fs::write(dir.path().join(&nulls.path), b"not a weftseg frame").await.expect("replaces the null segment file");

		let at = store.read_point("a", 10).await.expect("reads a point in the live segment");
		let many = store.read_points("a", &[10, 30, 40]).await.expect("reads a batch across both");
		let ranged = store.read_time_range("a", 0, 100).await;
		let downsampled = store.downsample_range("a", 0, 100, Resolution::Seconds, &[Aggregation::Avg]).await;
		drop(store);
		assert_eq!(at, Some(bd("2")));
		assert_eq!(many, vec![Some(bd("2")), None, None], "the null instants answer None without their segment being read");
		assert!(ranged.is_err(), "a range read still reads null rows, so it must hit the corrupt file: {ranged:?}");
		// The schema is declared and the live segment reads clean, so the only thing left for a
		// range read to fail on is the frame the point read skipped.
		let buckets = downsampled.expect("a downsample answers from the live segment alone");
		assert_eq!(buckets.len(), 3, "the live segment's three instants are three one-second buckets, and the null segment adds none");
	}

	/// The bucket-level claim the prune rests on, stated where the merge happens: an all-null
	/// segment folded in must leave every bucket of every aggregation exactly as it was,
	/// including a window the null segment covers on its own (a bucket holding nothing is
	/// dropped, not emitted empty). The reference is the single-pass `reduce` over the live
	/// rows only, so a prune that changed a bucket would show as a mismatch rather than as an
	/// assertion the fixture was built to satisfy.
	#[tokio::test]
	async fn an_all_null_segment_changes_no_bucket_of_a_downsample() {
		use splimes::Point;
		use weft_reduce::{reduce, Aggregation};

		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("temp", &schema()).await.expect("declares");
		let live: Vec<(i64, BigDecimal)> = (0..40).map(|i| (i * 15, bd(&format!("{}", i % 7)))).collect();
		let (ts, vs): (Vec<i64>, Vec<BigDecimal>) = live.iter().cloned().unzip();
		store.seal("temp", &schema(), &ts, &vs).await.expect("seals the live segment");
		// The null segment covers 600..=899, a stretch of the window no live row reaches.
		store.seal_nullable("temp", &schema(), &[600_i64, 660, 720, 780, 840], &[None, None, None, None, None]).await.expect("seals the all-null segment");
		assert_eq!(store.segment_count("temp").await.expect("counts"), 2, "both segments are indexed");

		let aggs = [Aggregation::Min, Aggregation::Max, Aggregation::Avg, Aggregation::Sum, Aggregation::First, Aggregation::Last, Aggregation::P99];
		let points: Vec<Point> = live.iter().map(|(t, v)| Point::new(DateTime::<Utc>::from_timestamp(*t, 0).expect("instant"), v.clone())).collect();
		for resolution in [Resolution::Minutes, Resolution::Hours] {
			let pruned = store.downsample_range("temp", 0, 1_000, resolution, &aggs).await.expect("downsamples");
			let single_pass = reduce(&points, resolution, None, None, &aggs).expect("reduces");
			assert!(!pruned.is_empty(), "the fixture must produce buckets");
			assert_eq!(pruned, single_pass, "the all-null segment must not move a bucket at {resolution:?}");
		}
		// A window the null segment covers alone answers empty, not a run of empty buckets.
		assert!(store.downsample_range("temp", 600, 899, Resolution::Minutes, &aggs).await.expect("downsamples the null-only window").is_empty(), "a window of nothing but nulls yields no buckets");
		drop(store);
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

	/// TAG-4: a frame wrapped in the v7 envelope — a framing no legacy
	/// layout-specific reader understands — still reads through the store's
	/// public read sites, because they go through the frame-reader API and
	/// let the frame's own header choose the reader.
	#[tokio::test]
	async fn an_envelope_wrapped_frame_reads_through_the_store() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		let descriptor = store.seal("a", &schema(), &[0_i64, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.expect("seals");
		// Rewrite the sealed frame in place, wrapped verbatim in an envelope.
		let plain = tokio::fs::read(&descriptor.path).await.expect("reads the frame");
		let wrapped = weft_physical_type::wrap_frame_envelope(&plain, &[]).expect("wraps");
		tokio::fs::write(&descriptor.path, &wrapped).await.expect("rewrites the frame");
		let (ts, vs) = store.read_time_range("a", 0, 100).await.expect("the enveloped frame reads");
		let hit = store.read_point("a", 10).await.expect("the enveloped frame answers a point read");
		drop(store);
		assert_eq!(ts, vec![0, 10, 20]);
		assert_eq!(vs, vec![Some(bd("1")), Some(bd("2")), Some(bd("3"))]);
		assert_eq!(hit, Some(bd("2")));
	}

	/// TAG-4: a frame written by a newer build fails a store read with a
	/// **typed** error — the `FrameFaultError` under the context classifies
	/// it `Undecodable { newer_format: true }`, so a caller acts on the
	/// class (C5) rather than parsing the sentence.
	#[tokio::test]
	async fn a_newer_format_frame_fails_a_store_read_with_a_typed_error() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		let descriptor = store.seal("a", &schema(), &[0_i64, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.expect("seals");
		// Restamp the frame's own version field past this build's registry
		// and re-stamp its trailer, so the reader reaches the version check.
		let mut bytes = tokio::fs::read(&descriptor.path).await.expect("reads the frame");
		let bumped = weft_physical_type::FRAME_VERSION_MAX_KNOWN + 1;
		bytes[7..9].copy_from_slice(&bumped.to_le_bytes());
		let body = bytes.len() - 4;
		let checksum = weft_physical_type::crc32(&bytes[..body]);
		bytes[body..].copy_from_slice(&checksum.to_le_bytes());
		tokio::fs::write(&descriptor.path, &bytes).await.expect("rewrites the frame");
		let error = store.read_time_range("a", 0, 100).await.expect_err("a newer frame does not decode");
		drop(store);
		let fault = error.downcast_ref::<crate::FrameFaultError>().expect("the fault class travels under the context");
		assert!(matches!(fault.fault(), weft_physical_type::FrameFault::Undecodable { newer_format: true }), "a newer frame is not a corrupt file: {fault:?}");
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
		assert_eq!(gts, [] as [i64; 0]);
		assert_eq!(gvs, [] as [bigdecimal::BigDecimal; 0]);
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
		assert_eq!(store.read_points("a", &[]).await.expect("reads"), [] as [Option<bigdecimal::BigDecimal>; 0]);
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
		assert!(store.load_partial_sidecar("a", &desc, None).await.expect("reads").is_some(), "a sidecar is written at seal");

		// Reconcile rewrites the segment sorted at the same id → new bytes → the old sidecar
		// stamp is stale, so the reconcile must regenerate it.
		assert!(store.reconcile_segment("a", desc.id).await.expect("reconciles"), "the out-of-order segment is rewritten");
		let new_desc = store.index().all("a").await.expect("index").into_iter().find(|d| d.id == desc.id).expect("segment still present");
		assert!(store.load_partial_sidecar("a", &new_desc, None).await.expect("reads").is_some_and(|s| s.matches(&new_desc, None)), "the sidecar was regenerated to match the reconciled bytes");

		// The decisive proof: delete the reconciled frame; a downsample must still succeed,
		// which is only possible if the regenerated sidecar's stamp matches (a stale one
		// would be rejected, forcing a decode of the now-missing frame).
		let aggs = [Aggregation::Min, Aggregation::Max, Aggregation::Sum, Aggregation::First, Aggregation::Last];
		std::fs::remove_file(&new_desc.path).expect("removes the reconciled frame");
		let served = store.downsample_range("a", i64::MIN, i64::MAX, Resolution::Hours, &aggs).await.expect("serves from the regenerated sidecar");
		drop(store);
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
		let halves = store.index().all("a").await.expect("reads the index");
		let half = |id: u64| halves.iter().find(|h| h.id == id).map(|h| std::fs::read(&h.path).expect("reads a half's frame")).expect("the half is indexed");
		let (prefix_bytes, suffix_bytes) = (half(d.id), half(suffix_id));
		drop(store);
		assert_eq!(stats.segment_count, 2);
		assert_eq!(stats.total_rows, 8, "the null row is preserved across the split");
		assert_eq!(rts, ts);
		assert_eq!(rvs, vs);
		assert_eq!(null_hit, None, "the null at 30 stays null (present-bit cleared)");
		assert_eq!(suffix_hit, Some(bd("4")));
		// Both halves still decode as paged frames through the frame reader
		// (TAG-4): a paged frame is the one that reports a page height.
		assert_eq!(frame_read::read_full("prefix", &prefix_bytes).expect("prefix decodes").rows_per_page(), Some(3), "prefix keeps the paged frame");
		assert_eq!(frame_read::read_full("suffix", &suffix_bytes).expect("suffix decodes").rows_per_page(), Some(3), "suffix keeps the paged frame");
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
		let base_values: Vec<BigDecimal> = (0..=10).map(BigDecimal::from).collect();
		store.seal("a", &schema(), &base_ts, &base_values).await.expect("base");
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
		// The cold-prefix segment on disk after a split (id 0): a write-once frame.
		async fn cold_frame(store: &SegmentStore) -> PathBuf {
			PathBuf::from(store.index().all("a").await.expect("reads").into_iter().find(|d| d.id == 0).map(|d| d.path).expect("the cold prefix"))
		}
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("a", &schema()).await.expect("declares");
		let base_ts: Vec<i64> = (0..=10).map(|i| i * 10).collect();
		let base_values: Vec<BigDecimal> = (0..=10).map(BigDecimal::from).collect();
		store.seal("a", &schema(), &base_ts, &base_values).await.expect("base");
		store.seal("a", &schema(), &[90_i64, 100, 110], &[bd("900"), bd("1000"), bd("1100")]).await.expect("late");
		// First reconcile splits: cold prefix [0..80] stays at id 0, hot suffix takes id 1.
		store.reconcile_overlaps_with_policy("a", SplitPolicy::new(1)).await.expect("splits");
		let cold_path = cold_frame(&store).await;
		let cold_before = std::fs::read(&cold_path).expect("cold prefix file");
		// A second late arrival re-enters only the hot window [90,110]; it must NOT pull
		// the cold prefix back in.
		store.seal("a", &schema(), &[105_i64, 115], &[bd("1050"), bd("1150")]).await.expect("later late");
		let removed = store.reconcile_overlaps_with_policy("a", SplitPolicy::new(1)).await.expect("merges the hot tail");
		let stats = store.aspect_stats("a").await.expect("stats");
		let (ts, _vs) = store.read_time_range("a", 0, 1000).await.expect("reads");
		let cold_after = std::fs::read(&cold_path).expect("cold prefix file still present");
		let cold_path_after = cold_frame(&store).await;
		let cold_hit = store.read_point("a", 50).await.expect("reads");
		drop(store);
		// The cold prefix file is byte-identical, and still the one its row names — it was
		// never rewritten by the second reconcile (the amortized split-not-rewrite win).
		assert_eq!(cold_path_after, cold_path, "segment 0 still names the frame the first merge wrote");
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
		let base_values: Vec<BigDecimal> = (0..=10).map(BigDecimal::from).collect();
		store.seal("a", &schema(), &base_ts, &base_values).await.expect("base");
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
		let base_values: Vec<BigDecimal> = (0..=10).map(BigDecimal::from).collect();
		store.seal("a", &schema(), &base_ts, &base_values).await.expect("a base");
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

	/// A merge that holds more than a page writes a paged frame (#507), so a small window
	/// after it skips the pages it does not touch, and it reads the same rows as before;
	/// one that fits in a page stays single-block.
	#[tokio::test]
	async fn a_merge_past_a_page_writes_a_paged_frame() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		for aspect in ["big", "small"] {
			store.declare(aspect, &schema()).await.expect("declares");
		}
		let rows = 2_000_i64;
		for seg in 0..3 {
			let ts: Vec<i64> = (seg * rows..(seg + 1) * rows).map(|i| i * 10).collect();
			let vs: Vec<BigDecimal> = (seg * rows..(seg + 1) * rows).map(|i| bd(&i.to_string())).collect();
			store.seal("big", &schema(), &ts, &vs).await.expect("seals");
		}
		store.seal("small", &schema(), &[0_i64, 10], &[bd("0"), bd("1")]).await.expect("seals");
		store.seal("small", &schema(), &[20_i64, 30], &[bd("2"), bd("3")]).await.expect("seals");
		let window = (4_090 * 10, 4_110 * 10);
		let before = store.read_time_range("big", window.0, window.1).await.expect("reads");
		assert_eq!(store.squash_aspect_to_target_rows("big", 10_000).await.expect("compacts"), 2);
		assert_eq!(store.squash_aspect_to_target_rows("small", 10).await.expect("compacts"), 1);
		let mut kinds = Vec::new();
		for aspect in ["big", "small"] {
			let rows = store.index.rows(aspect).await.expect("rows");
			assert_eq!(rows.len(), 1, "{aspect} is one segment");
			let bytes = store.read_frame(&rows[0].desc).await.expect("reads the frame");
			kinds.push(frame_read::read_full("merged", &bytes).expect("decodes").rows_per_page());
		}
		let after = store.read_time_range("big", window.0, window.1).await.expect("reads");
		let every = store.read_time_range("big", 0, i64::MAX).await.expect("reads");
		drop(store);
		assert_eq!(kinds, vec![Some(MERGE_ROWS_PER_PAGE), None], "6,000 merged rows are paged, 4 are not");
		assert_eq!(after, before, "a window across a page boundary reads the same rows");
		assert_eq!(after.0.len(), 21);
		assert_eq!(every.0, (0..3 * rows).map(|i| i * 10).collect::<Vec<_>>(), "every row is kept, in order");
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

	/// ROB-19: the per-aspect circuit breaker trips an aspect whose sweep passes
	/// keep failing, and later sweeps skip it — listing it in `tripped`, recording
	/// no new failure — while every other aspect is maintained as usual. The
	/// per-aspect entry point is never gated: an operator can still attempt the
	/// tripped aspect directly.
	#[tokio::test]
	async fn maintenance_breaker_trips_a_failing_aspect_and_sweeps_the_rest() {
		let dir = TempDir::new().expect("tempdir");
		let (store, bad_frame) = sweep_isolation_fixture(dir.path()).await;
		let store = store.with_maintenance_breaker(MaintenanceBreaker { failure_threshold: 2, cooldown: Duration::from_secs(60) });

		// First sweep: the bad aspect fails (one consecutive failure), the five
		// healthy aspects reconcile. Below the threshold of 2, nothing trips.
		let sweep = store.reconcile_all_over_threshold(1, MaintenanceWait::Skip).await.expect("sweeps");
		assert_eq!(sweep.aspects_reconciled, 5, "every healthy aspect is reconciled");
		assert_only_the_bad_aspect_failed(&sweep.failed, &bad_frame);
		assert!(sweep.tripped.is_empty(), "nothing has tripped yet: {:?}", sweep.tripped);
		assert_eq!(sweep.breaker_trips, 0, "one failure is below the threshold, so no trip event");
		assert!(!store.breaker_tripped(SWEEP_BAD), "one failure is below the threshold");

		// Second sweep: the bad aspect fails again and trips. The healthy aspects
		// have nothing left to reconcile, which still counts as success.
		let sweep = store.reconcile_all_over_threshold(1, MaintenanceWait::Skip).await.expect("sweeps");
		assert_only_the_bad_aspect_failed(&sweep.failed, &bad_frame);
		assert_eq!(sweep.breaker_trips, 1, "the second consecutive failure is the trip event");
		assert!(store.breaker_tripped(SWEEP_BAD), "the second consecutive failure trips the breaker");

		// Third sweep: the tripped aspect is not attempted at all — no failure is
		// recorded for it, it is listed in `tripped`, and the sweep is clean.
		let sweep = store.reconcile_all_over_threshold(1, MaintenanceWait::Skip).await.expect("sweeps");
		drop(store);
		assert_eq!(sweep.aspects_scanned, 6);
		assert!(sweep.failed.is_empty(), "a tripped aspect records no new failure: {:?}", sweep.failed);
		assert_eq!(sweep.tripped, vec![SWEEP_BAD.to_string()], "the tripped aspect is named");
		assert_eq!(sweep.breaker_trips, 0, "skipping an already-tripped aspect is not a trip event");

		// The breaker gates only the store-wide sweeps: the per-aspect entry point
		// still attempts the aspect (and still meets the truncated frame).
		let dir = TempDir::new().expect("tempdir");
		let (store, _bad_frame) = sweep_isolation_fixture(dir.path()).await;
		let store = store.with_maintenance_breaker(MaintenanceBreaker { failure_threshold: 1, cooldown: Duration::from_secs(60) });
		let _ = store.reconcile_all_over_threshold(1, MaintenanceWait::Skip).await.expect("sweeps");
		assert!(store.breaker_tripped(SWEEP_BAD));
		let direct = store.reconcile_aspect(SWEEP_BAD).await;
		drop(store);
		assert!(direct.is_err(), "the per-aspect pass is attempted, not gated: {direct:?}");
	}

	/// ROB-19: once a tripped breaker's cooldown passes, the next sweep retries
	/// the aspect once (the half-open trial); a trial that succeeds closes the
	/// breaker and the aspect is maintained again.
	#[tokio::test]
	async fn maintenance_breaker_half_open_trial_closes_on_success() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens").with_maintenance_breaker(MaintenanceBreaker { failure_threshold: 1, cooldown: Duration::from_millis(50) });
		store.declare("a", &schema()).await.expect("declares");
		// One out-of-order segment; keep its frame's bytes so the damage can be undone.
		let first = store.seal("a", &schema(), &[100_i64, 130, 110], &[bd("1"), bd("3"), bd("2")]).await.expect("seals");
		let original = std::fs::read(&first.path).expect("reads frame");
		let file = std::fs::OpenOptions::new().write(true).open(&first.path).expect("opens frame");
		file.set_len(original.len() as u64 / 2).expect("truncates frame");
		drop(file);

		// The failing pass trips the breaker at a threshold of one failure.
		let sweep = store.reconcile_all_over_threshold(1, MaintenanceWait::Skip).await.expect("sweeps");
		assert_eq!(sweep.failed.len(), 1, "the damaged aspect fails");
		assert!(store.breaker_tripped("a"), "one failure trips a threshold-1 breaker");

		// Repair the frame and let the cooldown pass: the trial pass reconciles
		// the aspect and closes the breaker.
		std::fs::write(&first.path, &original).expect("restores frame");
		tokio::time::sleep(Duration::from_millis(60)).await;
		let sweep = store.reconcile_all_over_threshold(1, MaintenanceWait::Skip).await.expect("sweeps");
		let stats = store.aspect_stats("a").await.expect("stats");
		drop(store);
		assert!(sweep.failed.is_empty(), "the trial pass succeeds: {:?}", sweep.failed);
		assert!(sweep.tripped.is_empty(), "the cooldown passed, so the aspect was tried");
		assert_eq!(sweep.aspects_reconciled, 1, "the trial pass reconciles the aspect");
		assert_eq!(stats.unsorted_segments, 0, "the aspect is maintained again");
	}

	/// ROB-19: a failure threshold of 0 disables the breaker — a failing aspect
	/// is attempted (and fails) on every sweep, as before the breaker existed.
	#[tokio::test]
	async fn maintenance_breaker_disabled_never_trips() {
		let dir = TempDir::new().expect("tempdir");
		let (store, bad_frame) = sweep_isolation_fixture(dir.path()).await;
		let store = store.with_maintenance_breaker(MaintenanceBreaker { failure_threshold: 0, cooldown: Duration::from_secs(60) });
		for _ in 0..3 {
			let sweep = store.reconcile_all_over_threshold(1, MaintenanceWait::Skip).await.expect("sweeps");
			assert_only_the_bad_aspect_failed(&sweep.failed, &bad_frame);
			assert!(sweep.tripped.is_empty(), "a disabled breaker trips nothing");
			assert!(!store.breaker_tripped(SWEEP_BAD));
		}
		drop(store);
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
	/// and can fail the one for `refuse` with an error of the given kind instead. It also
	/// records every `STORE_FORMAT` marker renamed into place, with the control-plane
	/// databases that existed beside it at that moment.
	#[derive(Debug, Default)]
	struct RecordingFs {
		synced: std::sync::Mutex<Vec<PathBuf>>,
		refuse: Option<(PathBuf, std::io::ErrorKind)>,
		markers: std::sync::Mutex<Vec<(StoreFormat, Vec<&'static str>)>>,
	}

	impl RecordingFs {
		fn refusing(dir: PathBuf, kind: std::io::ErrorKind) -> Self {
			Self { refuse: Some((dir, kind)), ..Self::default() }
		}

		fn synced(&self) -> Vec<PathBuf> {
			self.synced.lock().expect("not poisoned").clone()
		}

		fn markers(&self) -> Vec<(StoreFormat, Vec<&'static str>)> {
			self.markers.lock().expect("not poisoned").clone()
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
			RealFs.rename(from, to).await?;
			if to.file_name() == Some(OsStr::new(STORE_FORMAT_FILE)) {
				let dir = to.parent().expect("a marker has a directory");
				let format = serde_json::from_slice(&std::fs::read(to)?).expect("the renamed marker parses");
				let databases = CONTROL_PLANE_FILES.into_iter().filter(|file| dir.join(file).exists()).collect();
				self.markers.lock().expect("not poisoned").push((format, databases));
			}
			Ok(())
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

		// A fresh nested root: the root twice for its STORE_FORMAT marker (written before
		// anything else, then in its final form), then everything it created and the
		// directory that held the top.
		let fs = Arc::new(RecordingFs::default());
		drop(SegmentStore::open_on(fs.clone(), &root, "market", "BTCUSD", SegmentStoreOptions::default()).await.expect("opens a fresh nested root"));
		let mut expected = paths(&["a/b", "a/b", "a/b/segments", "a/b", "a"]);
		expected.push(dir.path().to_path_buf());
		assert_eq!(fs.synced(), expected);

		// Reopening it still syncs the root's parent: the open that created the root may
		// have died before its own fsyncs, and this one cannot tell. Its marker is already
		// final, so it is not rewritten.
		let fs = Arc::new(RecordingFs::default());
		drop(SegmentStore::open_on(fs.clone(), &root, "market", "BTCUSD", SegmentStoreOptions::default()).await.expect("reopens"));
		assert_eq!(fs.synced(), paths(&["a/b/segments", "a/b", "a"]));

		// A failed fsync fails the open before the scope is registered.
		let fresh = dir.path().join("c");
		let fs = Arc::new(RecordingFs::refusing(fresh.join("segments"), std::io::ErrorKind::Other));
		let err = SegmentStore::open_on(fs, &fresh, "market", "BTCUSD", SegmentStoreOptions::default()).await.err().expect("a failed directory fsync fails the open");
		assert!(format!("{err:#}").contains(&format!("fsyncing directory {}", fresh.join("segments").display())), "{err:#}");
		assert_eq!(registered_scopes(&fresh).await, (Vec::new(), Vec::new()), "the scope is registered only after the layout is durable");
	}

	/// The root's parent is synced on every open, but a store whose root predates the open
	/// still opens when that parent cannot be opened for reading, or sits on a filesystem
	/// that refuses a directory fsync: read-only (`EROFS`), or a FUSE or Docker Desktop
	/// mount (`EINVAL`, `ENOTSUP`) (release plan C-3). A root the open created is another
	/// matter: its entry is the open's own to make durable.
	#[tokio::test]
	async fn an_unsyncable_parent_is_skipped_only_for_a_root_the_open_did_not_create() {
		use std::io::ErrorKind;

		for kind in [ErrorKind::PermissionDenied, ErrorKind::ReadOnlyFilesystem, ErrorKind::InvalidInput, ErrorKind::Unsupported] {
			let dir = TempDir::new().expect("tempdir");
			let root = dir.path().join("store");
			let fs = Arc::new(RecordingFs::refusing(dir.path().to_path_buf(), kind));
			let err = SegmentStore::open_on(fs, &root, "market", "BTCUSD", SegmentStoreOptions::default()).await.err().unwrap_or_else(|| panic!("{kind:?}: a root this open created needs its parent synced"));
			assert!(format!("{err:#}").contains(&format!("fsyncing directory {}", dir.path().display())), "{kind:?}: {err:#}");

			let fs = Arc::new(RecordingFs::refusing(dir.path().to_path_buf(), kind));
			drop(SegmentStore::open_on(fs.clone(), &root, "market", "BTCUSD", SegmentStoreOptions::default()).await.unwrap_or_else(|e| panic!("{kind:?}: the existing root opens: {e:#}")));
			assert_eq!(fs.synced(), vec![root.join("segments"), root.clone(), dir.path().to_path_buf()], "{kind:?}: the parent was tried");

			let fs = Arc::new(RecordingFs::refusing(dir.path().to_path_buf(), ErrorKind::Other));
			assert!(SegmentStore::open_on(fs, &root, "market", "BTCUSD", SegmentStoreOptions::default()).await.is_err(), "{kind:?}: any other error syncing the parent still fails the open");
		}
	}

	/// The database and subject rows a root's catalog holds, read without opening a
	/// store (which would register a scope of its own).
	async fn registered_scopes(root: &Path) -> (Vec<String>, Vec<String>) {
		let catalog = CatalogStore::open_unmigrated(&root.join("catalog.db").to_string_lossy()).await.expect("opens the catalog");
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
		let expected_bpp = crate::types::num::to_f64(expected_bytes) / 10.0;
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
		let expected_bpp = crate::types::num::to_f64(stats.total_bytes) / 5.0;
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
		// Force a wrong rollup, then remove another aspect's row entirely: the
		// clobber writes the `segment_index.db` rollup row directly, behind the
		// store's back, the way a stale pre-S11 copy or a bad restore would.
		store.index().apply(&IndexTxn::new(vec![IndexOp::PutRollup { aspect: "a".to_string(), meta: AspectMetadata { segment_count: 99, total_rows: 1, total_nulls: 7, total_bytes: 3, unsorted_segments: 42, time_range: Some((-5, -1)), value_range: Some((bd("-9"), bd("-8"))) } }])).await.expect("clobbers");
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
		// Wipe the whole rollup table, as if the `segment_index.db` rollup rows were
		// lost beside an intact index.
		let conn = store.index().database().connect().expect("connects");
		conn.execute("DELETE FROM aspect_metadata", ()).await.expect("wipes the rollup rows");
		drop(conn);
		let before = store.index().rollup_aspects().await.expect("lists");
		let rebuilt = store.rebuild_all_metadata().await.expect("rebuilds");
		let after = store.index().rollup_aspects().await.expect("lists");
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
	async fn aspect_metadata_backfilled_on_open_after_rollup_wipe() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.seal("temp", &schema(), &[0_i64, 10], &[bd("1"), bd("2")]).await.expect("seals");
		store.seal_nullable("humidity", &schema(), &[0_i64, 10, 20], &[Some(bd("5")), None, Some(bd("7"))]).await.expect("seals nullable");
		let temp = store.aspect_metadata("temp").await.expect("metadata");
		let humidity = store.aspect_metadata("humidity").await.expect("metadata");
		// Wipe the whole rollup table, as a pre-S11 store has no rows at all.
		let conn = store.index().database().connect().expect("connects");
		conn.execute("DELETE FROM aspect_metadata", ()).await.expect("wipes the rollup rows");
		drop(conn);
		drop(store);
		// Reopen backfills exactly the missing rows: no explicit rebuild call.
		let store = SegmentStore::open(dir.path()).await.expect("reopens");
		let temp_after = store.aspect_metadata("temp").await.expect("metadata");
		let humidity_after = store.aspect_metadata("humidity").await.expect("metadata");
		drop(store);
		assert_eq!(temp_after.total_rows, temp.total_rows);
		assert_eq!(temp_after.total_nulls, temp.total_nulls);
		assert_eq!(humidity_after.total_rows, humidity.total_rows);
		assert_eq!(humidity_after.total_nulls, 1);
		assert_eq!(humidity_after.value_range, humidity.value_range);
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

	/// D-S11: a seal folds its segment into the rollup row in `segment_index.db`
	/// itself, in the seal's transaction — not into `metadata.db` afterwards. The
	/// rollup the index store reads back equals the rollup the index derives, and
	/// the pre-S11 `metadata.db` copy is never written.
	#[tokio::test]
	async fn seal_folds_the_rollup_into_segment_index_db() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.seal("a", &schema(), &[0_i64, 10], &[bd("1"), bd("2")]).await.expect("seals");
		store.seal("a", &schema(), &[20_i64, 30], &[bd("3"), bd("4")]).await.expect("seals");
		let rollup = store.index().rollup("a").await.expect("reads the index rollup");
		let derived = crate::AspectMetadata::from_index(&store.index().load_index("a").await.expect("loads the index"));
		let legacy = store.metadata().get("a").await.expect("reads the legacy copy");
		drop(store);
		assert_eq!(rollup, Some(derived), "the segment_index.db rollup is the index's own summary");
		assert_eq!(legacy, None, "the seal wrote no rollup row into metadata.db");
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
		drop(plain);
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
		drop(plain);
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

		let sidecar = store.load_partial_sidecar("temp", &descriptor, None).await.expect("reads sidecar").expect("a sidecar was written");
		assert_eq!(sidecar.base, Resolution::Minutes);
		assert!(sidecar.matches(&descriptor, None), "the sidecar's stamp matches the sealed segment");

		// The stored partial finishes to exactly what reducing the segment's rows directly
		// at the base resolution produces — so a downsample can merge it in place of a decode.
		let expected = reduce_partial(&points, Resolution::Minutes, None, None, &SIDECAR_AGGREGATIONS).expect("partial").finish(Resolution::Minutes, &SIDECAR_AGGREGATIONS).expect("finishes");
		let got = sidecar.partial.finish(Resolution::Minutes, &SIDECAR_AGGREGATIONS).expect("finishes");
		assert_eq!(got, expected, "the stored partial equals a fresh reduction of the segment");

		// A stale descriptor (a rewrite would change byte_len) is rejected → falls back.
		let mut stale = descriptor.clone();
		stale.byte_len += 1;
		assert!(store.load_partial_sidecar("temp", &stale, None).await.expect("reads").is_none(), "a mismatched stamp is treated as no sidecar");
		drop(store);

		// An unconfigured store writes nothing beside the segment.
		let plain_dir = TempDir::new().expect("tempdir");
		let plain = SegmentStore::open(plain_dir.path()).await.expect("opens");
		assert_eq!(plain.partial_sidecar_policy(), PartialSidecarPolicy::DISABLED, "off by default");
		let plain_desc = plain.seal("temp", &schema(), &ts, &vs).await.expect("seals");
		assert!(plain.load_partial_sidecar("temp", &plain_desc, None).await.expect("reads").is_none(), "no sidecar without a policy");
		drop(plain);
	}

	/// `scaled_rows` admits a window to the integer path only when every present in-window
	/// value is a `ScaledI64` at one scale and every instant fits in epoch nanoseconds.
	#[test]
	fn scaled_rows_takes_one_scale_scaled_columns_and_declines_the_rest() {
		let scaled = |m: i64, scale: u8| Some(PhysicalValue::ScaledI64 { mantissa: m, scale });
		// In-window present rows of one scale lift to nanoseconds; nulls and out-of-window rows drop.
		let ts = [10_i64, 20, 30, 40];
		let vs = [scaled(125, 2), None, scaled(-5, 2), scaled(7, 2)];
		assert_eq!(scaled_rows(&ts, &vs, 10, 30, TimeUnit::Seconds), Some((vec![10_000_000_000, 30_000_000_000], vec![125, -5], 2)));
		assert_eq!(scaled_rows(&ts, &vs, 50, 60, TimeUnit::Millis), Some((Vec::new(), Vec::new(), 0)));
		// Mixed scales, a non-scaled value, and an instant past the nanosecond range all decline.
		assert_eq!(scaled_rows(&ts[..2], &[scaled(1, 2), scaled(1, 3)], 0, 100, TimeUnit::Seconds), None);
		assert_eq!(scaled_rows(&ts[..1], &[Some(PhysicalValue::F64(1.5))], 0, 100, TimeUnit::Seconds), None);
		assert_eq!(scaled_rows(&[i64::MAX / 10], &[scaled(1, 0)], 0, i64::MAX, TimeUnit::Seconds), None);
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
		drop(store);
		let in_one: Vec<Point> = all.iter().filter(|p| (s_start..=s_end).contains(&p.timestamp.timestamp())).cloned().collect();
		assert_eq!(one, reduce(&in_one, Resolution::Hours, None, None, &aggs).expect("reduces"), "the single-segment fast path must equal the single pass");
	}

	/// The integer path end to end: on a `ScaledI64` aspect every frame's window goes through
	/// `scaled_rows` into `reduce_partial_scaled`, and the merged downsample must still equal
	/// `read_time_range` followed by one `reduce` — value for value and exponent for exponent —
	/// across overlapping, out-of-order, nullable and paged segments, and again after
	/// maintenance has rewritten them.
	#[tokio::test]
	async fn downsample_range_on_scaled_segments_takes_the_integer_path_and_matches_a_single_pass() {
		use splimes::{Point, Resolution};
		use weft_reduce::{reduce, Aggregation};

		const ASPECT: &str = "px";

		/// `downsample_range` over `[start, end]` against `read_time_range` + one `reduce`.
		async fn assert_single_pass(store: &SegmentStore, start: i64, end: i64, aggs: &[Aggregation], label: &str) {
			let (ts, vs) = store.read_time_range(ASPECT, start, end).await.expect("reads");
			let points: Vec<Point> = ts.iter().zip(vs).filter_map(|(t, v)| v.map(|v| Point::new(DateTime::<Utc>::from_timestamp_millis(*t).expect("instant"), v))).collect();
			let single = reduce(&points, Resolution::Minutes, None, None, aggs).expect("reduces");
			let cross = store.downsample_range(ASPECT, start, end, Resolution::Minutes, aggs).await.expect("downsamples");
			assert!(cross.len() > 1, "{label}: the fixture must span several buckets, got {}", cross.len());
			assert_eq!(cross, single, "{label}: the downsample must equal the single pass");
			// `BigDecimal` equality ignores the exponent, so compare the representation too.
			for (c, s) in cross.iter().zip(&single) {
				for (name, value) in &c.values {
					assert_eq!(Some(value.as_bigint_and_exponent()), s.values.get(name).map(BigDecimal::as_bigint_and_exponent), "{label}: {name} at {} must carry the same digits and exponent", c.timestamp);
				}
			}
		}

		/// Every frame of the aspect is admitted to the integer path rather than the fallback.
		async fn assert_integer_path(store: &SegmentStore, aggs: &[Aggregation]) {
			let descriptors = store.index.prune_by_time(ASPECT, i64::MIN, i64::MAX).await.expect("prunes");
			assert!(descriptors.len() > 1, "the fixture must span several segments");
			for descriptor in &descriptors {
				let bytes = store.read_frame(descriptor).await.expect("reads the frame");
				// TAG-4: the frame is read through the frame-reader API, which
				// chooses its reader from the frame's own header — never from
				// the index row's `format_version` copy.
				let (ts, vs) = frame_read::read_range_physical(descriptor.path.as_str(), &bytes, i64::MIN, i64::MAX).expect("decodes");
				let (nanos, mantissas, scale) = scaled_rows(&ts, &vs, i64::MIN, i64::MAX, TimeUnit::Millis).expect("a ScaledI64 frame is admitted to the integer path");
				assert_eq!(scale, 4, "the declared scale reaches the reducer");
				assert!(weft_reduce::reduce_partial_scaled(&nanos, &mantissas, u32::from(scale), Resolution::Minutes, None, None, aggs).expect("reduces").is_some(), "every requested reduction is integer-native");
			}
		}

		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		let scaled = AspectSchema::new(PhysicalType::ScaledI64 { scale: 4 }, bd("0"), TimeUnit::Millis);
		store.declare(ASPECT, &scaled).await.expect("declares");
		// Four-decimal values, negative and whole ones included, so the integer `avg` quotient
		// has digits and an exponent to disagree with `BigDecimal`'s on.
		let value = |i: i64| if i % 13 == 0 { bd(&format!("{}", i % 50 - 25)) } else { bd(&format!("{}.{:04}", (i * 37) % 211 - 100, (i * 7919).rem_euclid(10_000))) };
		let base = 1_700_000_000_000_i64;
		// A sorted segment, and a second interleaved with it (distinct instants, overlapping windows).
		let a_ts: Vec<i64> = (0..300).map(|i| base + i * 7_000).collect();
		store.seal(ASPECT, &scaled, &a_ts, &(0..300).map(value).collect::<Vec<_>>()).await.expect("seals a");
		let b_ts: Vec<i64> = (100..400).map(|i| base + 3_500 + i * 7_000).collect();
		store.seal(ASPECT, &scaled, &b_ts, &(100..400).map(|i| value(i + 1_000)).collect::<Vec<_>>()).await.expect("seals b");
		// An out-of-order segment.
		let c_ts: Vec<i64> = (0..200).rev().map(|i| base + 3_000_000 + i * 11_000).collect();
		store.seal(ASPECT, &scaled, &c_ts, &(0..200).rev().map(|i| value(i + 2_000)).collect::<Vec<_>>()).await.expect("seals c");
		// A nullable segment, and a paged nullable one overlapping its tail.
		let d_ts: Vec<i64> = (0..250).map(|i| base + 6_000_000 + i * 5_000).collect();
		let d_values: Vec<Option<BigDecimal>> = (0..250).map(|i| (i % 5 != 0).then(|| value(i + 3_000))).collect();
		store.seal_nullable(ASPECT, &scaled, &d_ts, &d_values).await.expect("seals d");
		let e_ts: Vec<i64> = (0..300).map(|i| base + 7_000_001 + i * 3_000).collect();
		let e_values: Vec<Option<BigDecimal>> = (0..300).map(|i| (i % 7 != 3).then(|| value(i + 4_000))).collect();
		let paged = store.seal_paged_nullable(ASPECT, &scaled, &e_ts, &e_values, 32).await.expect("seals e");
		assert_eq!(paged.format_version, PAGED_SEGMENT_FORMAT_VERSION, "e is a paged frame");
		let stats = store.aspect_stats(ASPECT).await.expect("stats");
		assert!(stats.unsorted_segments >= 1 && stats.overlapping_segments >= 4, "the fixture holds unsorted and overlapping segments: {stats:?}");

		// Streaming reductions and the sketch take the integer path; exact percentiles and TWA
		// fall back, and must agree on the same scaled store too.
		let integer = [Aggregation::Min, Aggregation::Max, Aggregation::Avg, Aggregation::Sum, Aggregation::First, Aggregation::Last, Aggregation::SketchP99];
		let fallback = [Aggregation::Avg, Aggregation::P99, Aggregation::Twa];
		let whole = (i64::MIN, i64::MAX);
		// A window that trims inside the first, second and nullable segments.
		let window = (base + 1_000_000, base + 6_500_000);

		assert_integer_path(&store, &integer).await;
		for (start, end) in [whole, window] {
			assert_single_pass(&store, start, end, &integer, "sealed, integer path").await;
			assert_single_pass(&store, start, end, &fallback, "sealed, fallback").await;
		}

		// Maintenance sorts the out-of-order segment and merges the overlapping ones; the
		// rewritten frames must still be admitted and still agree.
		store.reconcile_aspect(ASPECT).await.expect("reconciles");
		store.reconcile_overlaps(ASPECT).await.expect("merges");
		let stats = store.aspect_stats(ASPECT).await.expect("stats");
		assert_eq!((stats.unsorted_segments, stats.overlapping_segments), (0, 0), "maintenance left no unsorted or overlapping segment: {stats:?}");
		assert_integer_path(&store, &integer).await;
		for (start, end) in [whole, window] {
			assert_single_pass(&store, start, end, &integer, "maintained, integer path").await;
			assert_single_pass(&store, start, end, &fallback, "maintained, fallback").await;
		}
		drop(store);
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
		drop(store);
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
		drop(store);
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
		drop(plain);

		let cp_dir = TempDir::new().expect("tempdir");
		let cp = SegmentStore::open(cp_dir.path()).await.expect("opens").with_checkpoint_policy(policy);
		let cp_desc = cp.seal("temp", &schema(), &ts, &vs).await.expect("seals");
		drop(cp);

		assert_eq!(cp_desc.byte_len, plain_desc.byte_len, "an RLE-shaped column must be byte-for-byte identical — the codec gate refused to checkpoint it");
		// Raising the ceiling lets it through, proving the gate (not the shape) is what refused.
		let loose_dir = TempDir::new().expect("tempdir");
		let loose = SegmentStore::open(loose_dir.path()).await.expect("opens").with_checkpoint_policy(CheckpointPolicy { max_codec_overhead: 100.0, ..policy });
		let loose_desc = loose.seal("temp", &schema(), &ts, &vs).await.expect("seals");
		drop(loose);
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
		drop(plain);

		let cp_dir = TempDir::new().expect("tempdir");
		let cp = SegmentStore::open(cp_dir.path()).await.expect("opens").with_checkpoint_policy(policy);
		let cp_desc = cp.seal("temp", &schema(), &ts, &vs).await.expect("seals");
		drop(cp);

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
		let dense_values: Vec<BigDecimal> = (0..10).map(|i| bd(&format!("{i}.5"))).collect();
		store.seal("price", &schema, &dense_ts, &dense_values).await.expect("seals the dense segment");
		store.seal_nullable("price", &schema, &[100, 110, 120, 130, 140, 150], &[Some(bd("7.25")), None, Some(bd("-3")), None, Some(bd("12")), Some(bd("0.125"))]).await.expect("seals the nullable segment");
		let paged_ts: Vec<i64> = (0..12).map(|i| 200 + i * 10).collect();
		let paged_values: Vec<BigDecimal> = (0..12).map(|i| BigDecimal::from(100 - i * 3)).collect();
		store.seal_paged("price", &schema, &paged_ts, &paged_values, 4).await.expect("seals the paged segment");
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
	/// `segment_index` row's identity and write-once columns, every `store_meta` row and
	/// every `store_migrations` record. Read with a connection of its own, so the store
	/// must be closed.
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
		for row in texts(&conn, "SELECT aspect, id, path, gen, prec, frame_crc, commit_epoch, series_id FROM segment_index ORDER BY aspect, id").await {
			writeln!(out, "row {}", row.join(" ")).expect("formats");
		}
		for row in texts(&conn, "SELECT key, value FROM store_meta ORDER BY key").await {
			writeln!(out, "meta {}", row.join(" ")).expect("formats");
		}
		for row in texts(&conn, "SELECT id, layout, applied_ms FROM store_migrations ORDER BY id").await {
			writeln!(out, "migration {}", row.join(" ")).expect("formats");
		}
		drop(conn);
		drop(db);
		out
	}

	/// The checked-in pre-v2 store (`tests/fixtures/pre_v2_store/root`, written by WeftDB
	/// before layout v2: absolute frame paths into a root that no longer exists,
	/// `{aspect}-{id}` frame names, v3 sidecars, no `STORE_FORMAT`) is layout 1, and
	/// upgrades in place through the migration registry (FRE-12a): the first open applies
	/// `0001_baseline` and `0002_s6_s7`, records both in `store_migrations`, mirrors its
	/// marker into `store_meta` and leaves the marker at layout 2. The migration is
	/// idempotent: three opens in a row leave the same tables, columns, rows, `store_meta`
	/// (one `store_uuid`) and records, rewrite no frame, and every open reads exactly what
	/// the pre-v2 build read when it wrote the fixture (`expected_reads.txt`). That last
	/// part also needs the frame paths resolved against the current root.
	#[tokio::test]
	async fn a_pre_v2_store_migrates_idempotently_and_reads_identically() {
		let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pre_v2_store");
		let expected_reads = std::fs::read_to_string(fixture.join("expected_reads.txt")).expect("reads the expected reads");
		let dir = TempDir::new().expect("tempdir");
		let root = dir.path().join("store");
		copy_tree(&fixture.join("root"), &root);
		let frames_before = tree_bytes(&root.join("segments"));
		assert!(!root.join(STORE_FORMAT_FILE).exists(), "the fixture predates the marker");

		let mut schemas = Vec::new();
		let mut reports = Vec::new();
		let mut formats = Vec::new();
		for open in 1..=3 {
			let store = SegmentStore::open(&root).await.unwrap_or_else(|e| panic!("open {open}: {e:#}"));
			let reads = fixture_reads(&store).await;
			reports.push(store.open_report().clone());
			formats.push((store.store_format(), store.store_uuid().to_string()));
			drop(store);
			assert_eq!(reads, expected_reads, "open {open} reads what the pre-v2 build read");
			schemas.push(index_schema(&root).await);
		}
		assert_eq!(tree_bytes(&root.join("segments")), frames_before, "the migration rewrites no frame or sidecar");
		assert_eq!(schemas[1], schemas[0], "the second open changes nothing the first did not");
		assert_eq!(schemas[2], schemas[0], "nor does the third");

		let migrated = OpenReport { created_new: false, migrated_from: Some(1), applied: vec!["0001_baseline".into(), "0002_s6_s7".into(), "0003_fre14_legacy_catalog".into(), "0004_d_s12".into(), "0005_d_s13".into(), "0006_tag5_series".into(), "0007_series_time_index".into(), "0008_upsert_keys".into()], recovery_report_only: false };
		let settled = OpenReport { created_new: false, migrated_from: None, applied: Vec::new(), recovery_report_only: false };
		assert_eq!(reports, vec![migrated, settled.clone(), settled], "the first open migrates from layout 1 through the registry, and the later ones find nothing pending");
		let (format, uuid) = formats[0].clone();
		assert_eq!(format, StoreFormat { kind: crate::types::store_format::STORE_KIND.into(), layout_version: 2, min_read_layout: 2, min_write_layout: 2, last_written_layout: 2, migrating_to: None, applied_through: Some("0008_upsert_keys".into()), store_uuid: uuid.clone(), scope: StoreScope { database: "default".into(), subject: "default".into() } });
		assert!(formats.iter().all(|seen| seen == &formats[0]), "the marker and the store's identity stay put: {formats:?}");
		assert_eq!(crate::types::store_format::read_marker(&root).await.expect("reads the marker"), Some(format));

		let schema = &schemas[0];
		for table in ["aspect_metadata", "aspect_seq", "frame_journal", "ingest_ledger", "segment_changes", "segment_index", "segment_quarantine", "series", "store_meta", "store_migrations"] {
			assert!(schema.contains(&format!("table {table}: ")), "{table} exists:\n{schema}");
		}
		assert!(schema.contains("| Text(\"gen\") Text(\"INTEGER\") Integer(1) Text(\"0\") Integer(0) | Text(\"prec\") Text(\"INTEGER\") Integer(0) Null Integer(0) | Text(\"frame_crc\") Text(\"INTEGER\") Integer(0) Null Integer(0) | Text(\"commit_epoch\") Text(\"INTEGER\") Integer(0) Null Integer(0) | Text(\"series_id\") Text(\"INTEGER\") Integer(1) Text(\"0\") Integer(0)\n"), "segment_index gains gen (NOT NULL DEFAULT 0), prec, frame_crc, commit_epoch and series_id (NOT NULL DEFAULT 0):\n{schema}");
		assert!(schema.contains("| Text(\"next_series_id\") Text(\"INTEGER\") Integer(1) Text(\"1\") Integer(0)\n"), "aspect_seq has next_series_id (NOT NULL DEFAULT 1):\n{schema}");
		let rows: Vec<&str> = schema.lines().filter(|line| line.starts_with("row ")).collect();
		assert_eq!(rows.len(), 5, "every legacy row is kept:\n{schema}");
		for row in rows {
			assert!(row.contains("/var/tmp/weft-pre-v2-fixture/root/segments/"), "the stored path is left as the pre-v2 build wrote it: {row}");
			assert!(row.ends_with(" Integer(0) Null Null Null Integer(0)"), "a legacy row is generation 0 and series 0, with no prec, frame_crc or commit_epoch: {row}");
		}
		for (key, value) in [("applied_through", "0008_upsert_keys"), ("last_written_layout", "2"), ("layout_version", "2"), ("min_read_layout", "2"), ("min_write_layout", "2"), ("store_uuid", uuid.as_str())] {
			assert!(schema.contains(&format!("meta Text(\"{key}\") Text(\"{value}\")\n")), "store_meta mirrors the marker's {key}:\n{schema}");
		}
		assert!(uuid::Uuid::parse_str(&uuid).is_ok(), "the store_uuid is a UUID: {uuid}");
		let migrations: Vec<&str> = schema.lines().filter_map(|line| line.strip_prefix("migration ")).collect();
		assert_eq!(migrations.len(), 8, "{schema}");
		assert!(migrations[0].starts_with("Text(\"0001_baseline\") Integer(1) ") && migrations[1].starts_with("Text(\"0002_s6_s7\") Integer(2) ") && migrations[2].starts_with("Text(\"0003_fre14_legacy_catalog\") Integer(2) ") && migrations[3].starts_with("Text(\"0004_d_s12\") Integer(2) ") && migrations[4].starts_with("Text(\"0005_d_s13\") Integer(2) ") && migrations[5].starts_with("Text(\"0006_tag5_series\") Integer(2) ") && migrations[6].starts_with("Text(\"0007_series_time_index\") Integer(2) ") && migrations[7].starts_with("Text(\"0008_upsert_keys\") Integer(2) "), "{migrations:?}");
	}

	/// FRE-13: a pre-v2 (layout-1) store upgrades to layout-2, serves existing data
	/// correctly, accepts new writes after the migration, and the upgrade is idempotent
	/// on subsequent opens.
	#[tokio::test]
	async fn a_layout_1_store_upgrades_to_layout_2_and_accepts_new_writes() {
		let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pre_v2_store");
		let expected_reads = std::fs::read_to_string(fixture.join("expected_reads.txt")).expect("reads the expected reads");
		let dir = TempDir::new().expect("tempdir");
		let root = dir.path().join("store");
		copy_tree(&fixture.join("root"), &root);
		assert!(!root.join(STORE_FORMAT_FILE).exists(), "the fixture predates the marker");

		// First open: migrates layout-1 → layout-2
		let store = SegmentStore::open(&root).await.unwrap_or_else(|e| panic!("first open: {e:#}"));
		let fresh_reads = fixture_reads(&store).await;
		assert_eq!(fresh_reads, expected_reads, "migrated store reads exactly what the pre-v2 build read");
		let report = store.open_report();
		assert_eq!(report.migrated_from, Some(1), "first open migrates from layout 1");
		assert_eq!(report.applied.len(), 8, "all migrations applied");
		assert!(!report.created_new, "not a new store");
		let format = store.store_format();
		assert_eq!(format.layout_version, 2, "upgraded to layout 2");
		assert_eq!(format.min_read_layout, 2);
		assert_eq!(format.min_write_layout, 2);
		assert_eq!(format.last_written_layout, 2);
		assert_eq!(format.applied_through, Some("0008_upsert_keys".into()));
		let uuid = store.store_uuid().to_string();

		// Write new data into the now-layout-2 store. The timestamps sit outside the
		// fixture's range on purpose: the series keeps every row it is given, so a
		// re-used timestamp would read back as two points, not one replaced point.
		let sch = schema();
		let new_ts: Vec<i64> = vec![500, 510, 520];
		let new_vals: Vec<BigDecimal> = vec![bd("55"), bd("66"), bd("77")];
		store.seal("price", &sch, &new_ts, &new_vals).await.expect("seals new data after migration");
		let (ts, vs) = store.read_time_range("price", 500, 600).await.expect("reads the just-written range");
		let expected_new_read: Vec<Option<BigDecimal>> = vec![Some(bd("55")), Some(bd("66")), Some(bd("77"))];
		assert_eq!(ts, new_ts, "new timestamps readable");
		assert_eq!(vs, expected_new_read, "new values readable");
		drop(store);

		// Second open: settled — the marker says layout 2, no migration runs
		let store2 = SegmentStore::open(&root).await.unwrap_or_else(|e| panic!("second open: {e:#}"));
		let report2 = store2.open_report();
		assert_eq!(report2.migrated_from, None, "second open finds no pending migration");
		assert_eq!(report2.applied.len(), 0, "no new migrations");
		assert!(!report2.created_new);

		// Existing fixture data is still correct (spot-check)
		assert_eq!(store2.read_point("price", 0).await.expect("reads point 0"), Some(bd("0.5")));
		assert_eq!(store2.read_point("price", 50).await.expect("reads point 50"), Some(bd("5.5")));
		assert_eq!(store2.read_point("price", 150).await.expect("reads point 150"), Some(bd("0.125")));
		assert_eq!(store2.read_point("temp", 0).await.expect("reads point 0"), Some(bd("20.5")));
		assert_eq!(store2.read_point("temp", 180).await.expect("reads point 180"), Some(bd("22")));

		// New data persisted through reopen
		let (ts2, vs2) = store2.read_time_range("price", 500, 600).await.expect("reads new data after reopen");
		assert_eq!(ts2, new_ts, "new timestamps persisted");
		assert_eq!(vs2, expected_new_read, "new values persisted");

		let format2 = store2.store_format();
		assert_eq!(format2.layout_version, 2, "still layout 2");
		assert_eq!(format2.applied_through, Some("0008_upsert_keys".into()));
		assert_eq!(store2.store_uuid().to_string(), uuid, "store uuid unchanged");
		drop(store2);

		// Third open: also settled (idempotent on re-reopen)
		let store3 = SegmentStore::open(&root).await.unwrap_or_else(|e| panic!("third open: {e:#}"));
		let report3 = store3.open_report();
		assert_eq!(report3.migrated_from, None);
		assert_eq!(report3.applied.len(), 0);
		let format3 = store3.store_format();
		assert_eq!(format3.layout_version, 2, "layout 2 persists");
		assert_eq!(store3.store_uuid().to_string(), uuid, "uuid stable");
		drop(store3);

		// On-disk marker matches
		let marker = crate::types::store_format::read_marker(&root).await.expect("reads the marker");
		assert_eq!(marker.map(|f| f.layout_version), Some(2), "on-disk marker is layout 2");
	}

	/// Every file under `root` with its bytes, but the root's `LOCK` and `LOCK.holder`,
	/// which every open, even a refused one, takes and rewrites.
	fn files_but_the_lock(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
		let mut files = tree_bytes(root);
		files.remove(Path::new(crate::types::durable::LOCK_FILE));
		files.remove(Path::new(crate::types::durable::lock::HOLDER_FILE));
		files
	}

	/// Overwrite `root`'s marker with `format`, as another WeftDB would have written it.
	fn put_marker(root: &Path, format: &StoreFormat) {
		std::fs::write(root.join(STORE_FORMAT_FILE), serde_json::to_vec(format).expect("encodes")).expect("writes the marker");
	}

	/// FRE-12a's gate: a store whose marker says only a WeftDB that knows layout 99 may
	/// write it is refused with `IncompatibleLayout` before anything is opened, and every
	/// file of the root but `LOCK` and `LOCK.holder` is byte for byte what it was: no
	/// database was opened (no MVCC switch, no header sync, no DDL) and nothing was
	/// created (a root holding only such a marker gains no `segments/` and no database).
	/// An unreadable marker is refused the same way, with `UnreadableStoreFormat`.
	#[tokio::test]
	async fn the_gate_refuses_a_newer_write_floor_before_opening_anything() {
		let dir = TempDir::new().expect("tempdir");
		let root = dir.path().join("store");
		let store = SegmentStore::open(&root).await.expect("creates the store");
		store.seal("price", &schema(), &[0, 10], &[bd("1"), bd("2")]).await.expect("seals");
		let format = store.store_format();
		drop(store);
		let newer = StoreFormat { layout_version: 99, min_read_layout: 99, min_write_layout: 99, last_written_layout: 99, applied_through: Some("0099_future".into()), ..format };
		put_marker(&root, &newer);
		let before = files_but_the_lock(&root);
		let err = SegmentStore::open(&root).await.err().expect("a store this build may not write is refused");
		let after = files_but_the_lock(&root);
		assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::IncompatibleLayout { min_write: 99, supported: SUPPORTED_LAYOUT }), "{err:#}");
		assert!(before == after, "the refused open changed {:?}", before.keys().chain(after.keys()).filter(|path| before.get(*path) != after.get(*path)).collect::<Vec<_>>());

		let bare = dir.path().join("bare");
		std::fs::create_dir(&bare).expect("creates a root");
		put_marker(&bare, &newer);
		let err = SegmentStore::open_scoped(&bare, "market", "BTCUSD").await.err().expect("refused");
		assert!(matches!(err.downcast_ref::<StoreError>(), Some(StoreError::IncompatibleLayout { min_write: 99, .. })), "{err:#}");
		assert_eq!(names_in(&bare), vec!["LOCK", "LOCK.holder", "STORE_FORMAT"], "nothing but the lock beside the marker");

		std::fs::write(root.join(STORE_FORMAT_FILE), b"{\"kind\":\"weftdb-store\",").expect("tears the marker");
		let before = files_but_the_lock(&root);
		let err = SegmentStore::open(&root).await.err().expect("an unreadable marker is refused");
		let after = files_but_the_lock(&root);
		assert!(matches!(err.downcast_ref::<StoreError>(), Some(StoreError::UnreadableStoreFormat { path, .. }) if path == &root.join(STORE_FORMAT_FILE)), "{err:#}");
		assert!(before == after, "the refused open changed nothing");
	}

	/// FRE-12a: a new store writes its marker, at this build's layout and floors and
	/// migrating to it, before any control-plane database exists; once the databases are
	/// open and every migration has applied, the marker is rewritten in its final form.
	#[tokio::test]
	async fn a_new_store_writes_its_marker_before_any_database_exists() {
		let dir = TempDir::new().expect("tempdir");
		let root = dir.path().join("store");
		let fs = Arc::new(RecordingFs::default());
		let store = SegmentStore::open_on(fs.clone(), &root, "market", "BTCUSD", SegmentStoreOptions::default()).await.expect("creates the store");
		let report = store.open_report().clone();
		let (format, uuid) = (store.store_format(), store.store_uuid().to_string());
		drop(store);
		let markers = fs.markers();
		assert_eq!(markers.len(), 2, "the first marker, then the final one: {markers:?}");
		let (first, databases) = &markers[0];
		assert!(databases.is_empty(), "no control-plane database existed when the first marker was in place: {databases:?}");
		assert_eq!((first.layout_version, first.min_read_layout, first.min_write_layout, first.migrating_to, first.applied_through.as_deref()), (SUPPORTED_LAYOUT, SUPPORTED_LAYOUT, SUPPORTED_LAYOUT, Some(SUPPORTED_LAYOUT), None));
		let (last, databases) = &markers[1];
		assert_eq!(databases, &CONTROL_PLANE_FILES.to_vec(), "the final marker follows the databases");
		assert_eq!(last, &format);
		assert_eq!(format, StoreFormat { migrating_to: None, applied_through: Some("0008_upsert_keys".into()), ..first.clone() }, "the final marker is the first, settled");
		assert_eq!((format.store_uuid.as_str(), &format.scope), (uuid.as_str(), &StoreScope { database: "market".into(), subject: "BTCUSD".into() }));
		assert_eq!(report, OpenReport { created_new: true, migrated_from: None, applied: vec!["0001_baseline".into(), "0002_s6_s7".into(), "0003_fre14_legacy_catalog".into(), "0004_d_s12".into(), "0005_d_s13".into(), "0006_tag5_series".into(), "0007_series_time_index".into(), "0008_upsert_keys".into()], recovery_report_only: false });
		let reopened = SegmentStore::open_scoped(&root, "market", "BTCUSD").await.expect("reopens");
		let again = (reopened.open_report().created_new, reopened.store_uuid().to_string());
		drop(reopened);
		assert_eq!(again, (false, uuid), "a reopen is not a new store, and keeps its identity");
	}

	/// Every table, column and index of `root`'s four control-plane databases, with every
	/// `store_meta` and `store_migrations` row, as text. The store must be closed.
	async fn control_plane_schema(root: &Path) -> String {
		use std::fmt::Write as _;

		let mut out = String::new();
		for file in CONTROL_PLANE_FILES {
			let db = turso::Builder::new_local(&root.join(file).to_string_lossy()).build().await.expect("opens the database");
			let conn = db.connect().expect("connects");
			let mut rows = conn.query("SELECT type, name, sql FROM sqlite_schema ORDER BY type, name", ()).await.expect("reads the schema");
			while let Some(row) = rows.next().await.expect("reads a row") {
				writeln!(out, "{file}: {:?} {:?} {:?}", row.get_value(0).expect("type"), row.get_value(1).expect("name"), row.get_value(2).expect("sql")).expect("formats");
			}
			drop(rows);
			for sql in ["SELECT key, value FROM store_meta ORDER BY key", "SELECT id, layout, applied_ms FROM store_migrations ORDER BY id"] {
				if let Ok(mut rows) = conn.query(sql, ()).await {
					while let Some(row) = rows.next().await.expect("reads a row") {
						writeln!(out, "{file}: {:?} {:?} {:?}", row.get_value(0).expect("a value"), row.get_value(1).expect("a value"), row.get_value(2).ok()).expect("formats");
					}
				}
			}
			drop(conn);
			drop(db);
		}
		out
	}

	/// FRE-12a: running the migrations again is a no-op. Every registered migration run a
	/// second time, directly, over a store that has them all leaves every database's
	/// schema and every record exactly as it was, and the next open finds nothing pending.
	#[tokio::test]
	async fn running_the_migrations_twice_is_a_no_op() {
		let dir = TempDir::new().expect("tempdir");
		let root = dir.path().join("store");
		let store = SegmentStore::open(&root).await.expect("creates the store");
		store.declare("price", &schema()).await.expect("declares");
		store.seal("price", &schema(), &[0, 10], &[bd("1"), bd("2")]).await.expect("seals");
		drop(store);
		let before = control_plane_schema(&root).await;

		let names = CONTROL_PLANE_FILES.map(|file| root.join(file).to_string_lossy().into_owned());
		let index = SegmentIndexStore::open_unmigrated(&names[0]).await.expect("opens");
		let metadata = AspectMetadataStore::open_unmigrated(&names[1]).await.expect("opens");
		let catalog = AspectCatalog::open_unmigrated(&names[2]).await.expect("opens");
		let registry = CatalogStore::open_unmigrated(&names[3]).await.expect("opens");
		let all: Vec<&migrations::Migration> = migrations::REGISTRY.iter().collect();
		let ran = migrations::run(&migrations::ControlPlane { index: Some(migrations::Db { db: index.database(), name: &names[0] }), metadata: Some(migrations::Db { db: metadata.database(), name: &names[1] }), aspect_catalog: Some(migrations::Db { db: catalog.database(), name: &names[2] }), catalog: Some(migrations::Db { db: registry.database(), name: &names[3] }) }, &all).await.expect("runs every migration again");
		drop((index, metadata, catalog, registry));
		let after = control_plane_schema(&root).await;

		let store = SegmentStore::open(&root).await.expect("reopens");
		let applied = store.open_report().applied.clone();
		let reads = store.read_time_range("price", i64::MIN, i64::MAX).await.expect("reads");
		drop(store);
		assert_eq!(ran, vec!["0001_baseline", "0002_s6_s7", "0003_fre14_legacy_catalog", "0004_d_s12", "0005_d_s13", "0006_tag5_series", "0007_series_time_index", "0008_upsert_keys"]);
		assert!(before.contains("store_migrations") && before.contains("0002_s6_s7"), "{before}");
		assert_eq!(after, before, "a second run of every migration changed nothing");
		assert!(applied.is_empty(), "nothing is pending: {applied:?}");
		assert_eq!(reads.0, vec![0, 10]);
		assert_eq!(control_plane_schema(&root).await, before, "nor did the reopen");
	}

	/// A [`StoreFs`] over a [`SimFs`](crate::durable::SimFs) that, right after every step
	/// of every marker write (the temporary file's synced write, its rename over the
	/// marker, the root's fsync), cuts the power with each of `seeds` and keeps the
	/// images.
	#[derive(Debug)]
	struct MarkerCuttingFs {
		sim: crate::durable::SimFs,
		images: PathBuf,
		seeds: &'static [u64],
		taken: std::sync::Mutex<Vec<PathBuf>>,
	}

	impl MarkerCuttingFs {
		fn cut(&self) {
			let mut taken = self.taken.lock().expect("not poisoned");
			for seed in self.seeds {
				let image = self.images.join(format!("cut-{}-{seed}", taken.len()));
				self.sim.power_cut(*seed, &image).expect("cuts the power");
				taken.push(image);
			}
		}

		fn is_marker_step(&self, path: &Path) -> bool {
			path.parent() == Some(self.sim.root()) && path.file_name().is_some_and(|name| name.to_string_lossy().starts_with(STORE_FORMAT_FILE))
		}
	}

	#[async_trait::async_trait]
	impl StoreFs for MarkerCuttingFs {
		async fn create_new_write(&self, path: &Path, bytes: Vec<u8>, policy: crate::types::durable::SyncPolicy, points: crate::types::durable::WritePoints) -> std::io::Result<()> {
			self.sim.create_new_write(path, bytes, policy, points).await?;
			if self.is_marker_step(path) {
				self.cut();
			}
			Ok(())
		}

		async fn sync_file(&self, path: &Path) -> std::io::Result<()> {
			self.sim.sync_file(path).await
		}

		async fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
			self.sim.sync_dir(dir).await?;
			if dir == self.sim.root() {
				self.cut();
			}
			Ok(())
		}

		async fn hard_link(&self, src: &Path, dst: &Path) -> std::io::Result<()> {
			self.sim.hard_link(src, dst).await
		}

		async fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
			self.sim.rename(from, to).await?;
			if self.is_marker_step(to) {
				self.cut();
			}
			Ok(())
		}

		async fn remove_file(&self, path: &Path) -> std::io::Result<()> {
			self.sim.remove_file(path).await
		}

		async fn create_dir(&self, path: &Path) -> std::io::Result<()> {
			self.sim.create_dir(path).await
		}

		async fn remove_dir_all(&self, path: &Path) -> std::io::Result<()> {
			self.sim.remove_dir_all(path).await
		}

		async fn copy_new(&self, src: &Path, dst: &Path, policy: crate::types::durable::SyncPolicy) -> std::io::Result<u64> {
			self.sim.copy_new(src, dst, policy).await
		}

		async fn read_dir(&self, dir: &Path) -> std::io::Result<Vec<crate::types::durable::FsEntry>> {
			self.sim.read_dir(dir).await
		}

		async fn metadata(&self, path: &Path) -> std::io::Result<crate::types::durable::FsMetadata> {
			self.sim.metadata(path).await
		}
	}

	/// The registered migrations a store image's `segment_index.db` records as applied,
	/// read raw (none when the database or the table is missing).
	async fn recorded_migrations(root: &Path) -> Vec<String> {
		let path = root.join("segment_index.db");
		if !path.exists() {
			return Vec::new();
		}
		let db = turso::Builder::new_local(&path.to_string_lossy()).build().await.expect("opens the image's index");
		let conn = db.connect().expect("connects");
		let mut ids = Vec::new();
		if let Ok(mut rows) = conn.query("SELECT id FROM store_migrations ORDER BY id", ()).await {
			while let Some(row) = rows.next().await.expect("reads a row") {
				ids.push(row.get_value(0).expect("an id").as_text().cloned().expect("text"));
			}
		}
		drop(conn);
		drop(db);
		ids
	}

	/// The tables of `root`'s `segment_index.db`, by name, read raw.
	async fn index_tables(root: &Path) -> Vec<String> {
		let db = turso::Builder::new_local(&root.join("segment_index.db").to_string_lossy()).build().await.expect("opens the index");
		let conn = db.connect().expect("connects");
		let mut rows = conn.query("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name", ()).await.expect("reads the schema");
		let mut names = Vec::new();
		while let Some(row) = rows.next().await.expect("reads a row") {
			names.push(row.get_value(0).expect("a name").as_text().cloned().expect("text"));
		}
		drop(rows);
		drop(conn);
		drop(db);
		names
	}

	/// The column names of `table` in `root`'s `segment_index.db`, read raw.
	async fn index_columns(root: &Path, table: &str) -> Vec<String> {
		let db = turso::Builder::new_local(&root.join("segment_index.db").to_string_lossy()).build().await.expect("opens the index");
		let conn = db.connect().expect("connects");
		let names = migrations::column_names(&conn, table).await.expect("lists the columns");
		drop(conn);
		drop(db);
		names
	}

	/// FRE-12a's floor-first rule under power loss: the power is cut (with several seeds)
	/// right after each step of every marker write an open makes, for a new store and for
	/// the layout-1 fixture, and in every image the marker's floors (layout 1's when there
	/// is no marker yet) are at least what every migration the image's `store_migrations`
	/// records needs. A marker written only after the migrations, as one in its final form
	/// would be, fails this at the first cut. Every image then opens, settles its marker,
	/// and reads what the store held.
	#[tokio::test]
	#[cfg_attr(windows, ignore = "Windows file locks are mandatory: this test images or reads the files of a store that is still open, which Turso holds locked (ERROR_LOCK_VIOLATION). Linux and macOS CI run it; Windows crash coverage is the process-kill tests")]
	async fn a_power_cut_at_any_marker_write_never_leaves_floors_below_a_committed_migration() {
		// Sixteen seeds, so that the floor invariant by itself catches a marker write that
		// skips the root's fsync (with three, only the cut count below did).
		const SEEDS: &[u64] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
		let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pre_v2_store");
		let expected_reads = std::fs::read_to_string(fixture.join("expected_reads.txt")).expect("reads the expected reads");
		for legacy in [false, true] {
			let dir = TempDir::new().expect("tempdir");
			let root = dir.path().join("store");
			if legacy {
				copy_tree(&fixture.join("root"), &root);
			} else {
				std::fs::create_dir(&root).expect("creates the root");
			}
			let images = dir.path().join("images");
			let sim = crate::durable::SimFs::exempting(&root, crate::durable::SimFs::turso_file).expect("simulates the root");
			let fs = Arc::new(MarkerCuttingFs { sim, images, seeds: SEEDS, taken: std::sync::Mutex::default() });
			drop(SegmentStore::open_on(fs.clone(), &root, "default", "default", SegmentStoreOptions::default()).await.expect("opens"));
			let taken = fs.taken.lock().expect("not poisoned").clone();
			assert!(taken.len() >= 6 * SEEDS.len(), "legacy {legacy}: both marker writes were cut at each step: {}", taken.len());
			for image in taken {
				let marker = crate::types::store_format::read_marker(&image).await.unwrap_or_else(|e| panic!("{}: the marker is whole or absent, never torn: {e}", image.display()));
				let (min_read, min_write) = marker.as_ref().map_or((crate::LEGACY_LAYOUT, crate::LEGACY_LAYOUT), |m| (m.min_read_layout, m.min_write_layout));
				for id in recorded_migrations(&image).await {
					let migration = migrations::REGISTRY.iter().find(|m| m.id == id).unwrap_or_else(|| panic!("an unknown migration {id}"));
					assert!(min_read >= migration.min_read_after && min_write >= migration.min_write_after, "{} (legacy {legacy}): {id} committed, needing floors {}/{}, but the marker {marker:?} holds {min_read}/{min_write}", image.display(), migration.min_read_after, migration.min_write_after);
				}
				let store = SegmentStore::open(&image).await.unwrap_or_else(|e| panic!("{}: the image opens: {e:#}", image.display()));
				let settled = store.store_format();
				let reads = if legacy { Some(fixture_reads(&store).await) } else { None };
				drop(store);
				assert_eq!((settled.layout_version, settled.min_write_layout, settled.migrating_to, settled.applied_through.as_deref()), (SUPPORTED_LAYOUT, SUPPORTED_LAYOUT, None, Some("0008_upsert_keys")), "{}", image.display());
				if let Some(reads) = reads {
					assert_eq!(reads, expected_reads, "{}: the image reads what the fixture holds", image.display());
				}
			}
		}
	}

	/// FRE-12a: a store whose layout is newer than this build's, but whose write floor
	/// this build meets, opens without running a single migration (here, not even one its
	/// `store_migrations` lacks), says recovery must only report, and records only that
	/// this build wrote it (`last_written_layout`), so the newer WeftDB re-derives what it
	/// must. It keeps working.
	#[tokio::test]
	async fn a_newer_layout_this_build_may_write_opens_without_migrating() {
		let dir = TempDir::new().expect("tempdir");
		let root = dir.path().join("store");
		let store = SegmentStore::open(&root).await.expect("creates the store");
		store.seal("price", &schema(), &[0, 10], &[bd("1"), bd("2")]).await.expect("seals");
		let format = store.store_format();
		drop(store);
		let newer = StoreFormat { layout_version: 3, min_read_layout: 3, min_write_layout: 2, last_written_layout: 3, applied_through: Some("0009_future".into()), ..format.clone() };
		put_marker(&root, &newer);
		// A pending migration and a table it would create, so that a migration run would
		// show.
		let db = turso::Builder::new_local(&root.join("segment_index.db").to_string_lossy()).build().await.expect("opens raw");
		let conn = db.connect().expect("connects");
		conn.execute("DELETE FROM store_migrations WHERE id = '0002_s6_s7'", ()).await.expect("forgets a migration");
		conn.execute("DROP TABLE segment_changes", ()).await.expect("drops a table it creates");
		drop(conn);
		drop(db);

		let store = SegmentStore::open(&root).await.expect("a newer layout this build may write opens");
		let report = store.open_report().clone();
		let settled = store.store_format();
		let sealed = store.seal("price", &schema(), &[20], &[bd("3")]).await.map(|d| d.id);
		let (ts, _) = store.read_time_range("price", i64::MIN, i64::MAX).await.expect("reads");
		drop(store);
		let schema_after = index_schema(&root).await;
		assert_eq!(report, OpenReport { created_new: false, migrated_from: None, applied: Vec::new(), recovery_report_only: true });
		assert_eq!(settled, StoreFormat { last_written_layout: SUPPORTED_LAYOUT, ..newer }, "only last_written_layout changes");
		assert_eq!(crate::types::store_format::read_marker(&root).await.expect("reads"), Some(settled));
		assert!(!schema_after.contains("table segment_changes"), "no migration ran:\n{schema_after}");
		assert!(schema_after.contains("meta Text(\"layout_version\") Text(\"3\")") && schema_after.contains("meta Text(\"last_written_layout\") Text(\"2\")") && schema_after.contains("meta Text(\"min_write_layout\") Text(\"2\")"), "store_meta mirrors the marker:\n{schema_after}");
		assert_eq!(sealed.expect("the store takes writes"), 1);
		assert_eq!(ts, vec![0, 10, 20]);
	}

	/// Run each of `statements` on `root`'s `segment_index.db`, on a raw connection of its
	/// own (the store must be closed).
	async fn on_raw_index(root: &Path, statements: &[&str]) {
		let db = turso::Builder::new_local(&root.join("segment_index.db").to_string_lossy()).build().await.expect("opens raw");
		let conn = db.connect().expect("connects");
		for sql in statements {
			conn.execute(*sql, ()).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
		}
		drop(conn);
		drop(db);
	}

	/// A root whose `STORE_FORMAT` is gone, but whose databases a WeftDB that writes
	/// markers wrote, is judged by the copy of the marker its `store_meta` holds, not
	/// taken for layout 1:
	///
	/// - a settled store gets its marker back as it was (identity and floors), and nothing
	///   is migrated;
	/// - a newer layout this build may write opens report-only, and neither its layout nor
	///   its read floor is lowered, in the marker or in `store_meta`;
	/// - a write floor of 99 is refused with `IncompatibleLayout` before anything is
	///   written: every file of the root but `LOCK` and `LOCK.holder` is byte for byte
	///   what it was, and no marker is planted; nor is a stale layout-1 marker raised.
	#[tokio::test]
	async fn a_root_that_lost_its_marker_is_judged_by_store_meta() {
		let dir = TempDir::new().expect("tempdir");
		let root = dir.path().join("store");
		let store = SegmentStore::open(&root).await.expect("creates the store");
		store.seal("price", &schema(), &[0, 10], &[bd("1"), bd("2")]).await.expect("seals");
		let format = store.store_format();
		drop(store);

		std::fs::remove_file(root.join(STORE_FORMAT_FILE)).expect("loses the marker");
		let store = SegmentStore::open(&root).await.expect("reopens");
		let (report, restored, uuid) = (store.open_report().clone(), store.store_format(), store.store_uuid().to_string());
		drop(store);
		assert_eq!(report, OpenReport { created_new: false, migrated_from: None, applied: Vec::new(), recovery_report_only: false });
		assert_eq!(restored, format, "the marker is rebuilt from store_meta as it was");
		assert_eq!(uuid, format.store_uuid);
		assert_eq!(crate::types::store_format::read_marker(&root).await.expect("reads"), Some(format.clone()), "and written back");

		on_raw_index(&root, &["UPDATE store_meta SET value = '3' WHERE key IN ('layout_version', 'min_read_layout', 'last_written_layout')"]).await;
		std::fs::remove_file(root.join(STORE_FORMAT_FILE)).expect("loses the marker");
		let store = SegmentStore::open(&root).await.expect("a newer layout this build may write opens");
		let (report, restored) = (store.open_report().clone(), store.store_format());
		let sealed = store.seal("price", &schema(), &[20], &[bd("3")]).await.map(|d| d.id);
		drop(store);
		let schema_after = index_schema(&root).await;
		assert_eq!(report, OpenReport { created_new: false, migrated_from: None, applied: Vec::new(), recovery_report_only: true });
		assert_eq!(restored, StoreFormat { layout_version: 3, min_read_layout: 3, last_written_layout: SUPPORTED_LAYOUT, ..format.clone() }, "the newer layout and read floor are kept");
		assert_eq!(crate::types::store_format::read_marker(&root).await.expect("reads"), Some(restored));
		assert!(schema_after.contains("meta Text(\"layout_version\") Text(\"3\")") && schema_after.contains("meta Text(\"min_read_layout\") Text(\"3\")"), "store_meta is not lowered:\n{schema_after}");
		assert_eq!(sealed.expect("the store takes writes"), 1);

		on_raw_index(&root, &["UPDATE store_meta SET value = '99' WHERE key = 'min_write_layout'"]).await;
		std::fs::remove_file(root.join(STORE_FORMAT_FILE)).expect("loses the marker");
		let before = files_but_the_lock(&root);
		let err = SegmentStore::open(&root).await.err().expect("a store this build may not write is refused");
		let after = files_but_the_lock(&root);
		assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::IncompatibleLayout { min_write: 99, supported: SUPPORTED_LAYOUT }), "{err:#}");
		assert!(before == after, "the refused open changed {:?}", before.keys().chain(after.keys()).filter(|path| before.get(*path) != after.get(*path)).collect::<Vec<_>>());
		assert!(!root.join(STORE_FORMAT_FILE).exists(), "no marker was planted");

		// A stale layout-1 marker over the same control plane passes the marker's gate,
		// and store_meta's floor still refuses the store before the marker is raised.
		put_marker(&root, &StoreFormat { layout_version: 1, min_read_layout: 1, min_write_layout: 1, last_written_layout: 1, migrating_to: None, applied_through: None, ..format });
		let before = files_but_the_lock(&root);
		let err = SegmentStore::open(&root).await.err().expect("refused by store_meta");
		assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::IncompatibleLayout { min_write: 99, supported: SUPPORTED_LAYOUT }), "{err:#}");
		assert!(files_but_the_lock(&root) == before, "the stale marker was not raised");
	}

	/// A root holding only the marker of a layout newer than this build's (its write floor
	/// this build meets) and none of its databases: a newer WeftDB began creating the store.
	/// This build cannot create that layout, so it refuses the root with
	/// `NewerStoreWithoutDatabases`, creating nothing: no database, no `segments/`.
	#[tokio::test]
	async fn a_newer_marker_without_databases_is_refused_untouched() {
		let dir = TempDir::new().expect("tempdir");
		let root = dir.path().join("store");
		std::fs::create_dir(&root).expect("creates the root");
		let newer = StoreFormat { layout_version: 3, min_read_layout: 3, min_write_layout: 2, last_written_layout: 3, migrating_to: Some(3), ..StoreFormat::new_store(StoreScope { database: "default".into(), subject: "default".into() }) };
		put_marker(&root, &newer);
		let before = files_but_the_lock(&root);
		let err = SegmentStore::open(&root).await.err().expect("refused");
		assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::NewerStoreWithoutDatabases { layout: 3, supported: SUPPORTED_LAYOUT }), "{err:#}");
		assert_eq!(names_in(&root), vec!["LOCK", "LOCK.holder", "STORE_FORMAT"], "nothing but the lock beside the marker");
		assert!(files_but_the_lock(&root) == before, "the marker is untouched");
	}

	/// A development store of S6 or S7 (no marker, no `store_migrations`, a `store_meta`
	/// with only `layout_version` 2 and its `store_uuid`, `segment_index` without
	/// `series_id` and `aspect_seq` without `next_series_id`) is layout 1 to the registry:
	/// it is migrated from layout 1 through both migrations, gains both columns with their
	/// declarations, keeps its identity, and seals and reads.
	#[tokio::test]
	async fn an_s6_era_store_gains_the_series_columns_and_keeps_its_identity() {
		let dir = TempDir::new().expect("tempdir");
		let root = dir.path().join("store");
		let store = SegmentStore::open(&root).await.expect("creates the store");
		store.seal("price", &schema(), &[0, 10], &[bd("1"), bd("2")]).await.expect("seals");
		let uuid = store.store_uuid().to_string();
		drop(store);
		std::fs::remove_file(root.join(STORE_FORMAT_FILE)).expect("removes the marker");
		on_raw_index(&root, &["DROP TABLE store_migrations", "DELETE FROM store_meta WHERE key <> 'store_uuid'", "INSERT INTO store_meta (key, value) VALUES ('layout_version', '2')", "DROP INDEX idx_segment_index_series_time", "ALTER TABLE segment_index DROP COLUMN series_id", "ALTER TABLE aspect_seq DROP COLUMN next_series_id"]).await;
		let (tables, index_columns, seq_columns) = (index_tables(&root).await, index_columns(&root, "segment_index").await, index_columns(&root, "aspect_seq").await);
		assert!(!tables.contains(&"store_migrations".to_string()) && index_columns.contains(&"gen".to_string()) && !index_columns.contains(&"series_id".to_string()) && !seq_columns.contains(&"next_series_id".to_string()), "the S6/S7 shape: {tables:?} {index_columns:?} {seq_columns:?}");

		let store = SegmentStore::open(&root).await.expect("an S6-era store opens");
		let (report, kept) = (store.open_report().clone(), store.store_uuid().to_string());
		let sealed = store.seal("price", &schema(), &[20], &[bd("3")]).await.map(|d| d.id);
		let (ts, _) = store.read_time_range("price", i64::MIN, i64::MAX).await.expect("reads");
		drop(store);
		let schema = index_schema(&root).await;
		assert_eq!(report, OpenReport { created_new: false, migrated_from: Some(1), applied: vec!["0001_baseline".into(), "0002_s6_s7".into(), "0003_fre14_legacy_catalog".into(), "0004_d_s12".into(), "0005_d_s13".into(), "0006_tag5_series".into(), "0007_series_time_index".into(), "0008_upsert_keys".into()], recovery_report_only: false });
		assert_eq!(kept, uuid, "the S6 store_uuid is kept");
		assert!(schema.contains("| Text(\"series_id\") Text(\"INTEGER\") Integer(1) Text(\"0\") Integer(0)\n"), "segment_index gains series_id (NOT NULL DEFAULT 0):\n{schema}");
		assert!(schema.contains("| Text(\"next_series_id\") Text(\"INTEGER\") Integer(1) Text(\"1\") Integer(0)\n"), "aspect_seq gains next_series_id (NOT NULL DEFAULT 1):\n{schema}");
		assert_eq!(sealed.expect("the store seals"), 1);
		assert_eq!(ts, vec![0, 10, 20]);
	}

	/// TAG-5: a layout-2 store written before the series catalog (its registry through
	/// `0005_d_s13`, no `series` table) gains the catalog at its next open with nothing
	/// else changed: only `0006_tag5_series` (and `0007_series_time_index` after it, whose
	/// index the pre-TAG-5 shape lacks too) applies, the layout and both floors stay 2
	/// (so it is not reported as migrated from an older layout), the store keeps its
	/// identity, and its rows and reads are what they were.
	#[tokio::test]
	async fn a_layout_2_store_gains_the_series_catalog_and_keeps_its_identity() {
		let dir = TempDir::new().expect("tempdir");
		let root = dir.path().join("store");
		let store = SegmentStore::open(&root).await.expect("creates the store");
		store.seal("price", &schema(), &[0, 10], &[bd("1"), bd("2")]).await.expect("seals");
		let (format, before) = (store.store_format(), store.read_time_range("price", i64::MIN, i64::MAX).await.expect("reads"));
		drop(store);
		put_marker(&root, &StoreFormat { applied_through: Some("0005_d_s13".into()), ..format.clone() });
		on_raw_index(&root, &["DROP TABLE series", "DROP INDEX idx_segment_index_series_time", "DELETE FROM store_migrations WHERE id IN ('0006_tag5_series', '0007_series_time_index', '0008_upsert_keys')", "UPDATE store_meta SET value = '0005_d_s13' WHERE key = 'applied_through'"]).await;
		assert!(!index_tables(&root).await.contains(&"series".to_string()), "the pre-TAG-5 shape");
		let rows_before: Vec<String> = index_schema(&root).await.lines().filter(|line| line.starts_with("row ")).map(str::to_string).collect();

		let store = SegmentStore::open(&root).await.expect("a pre-TAG-5 layout-2 store opens");
		let (report, settled, uuid) = (store.open_report().clone(), store.store_format(), store.store_uuid().to_string());
		let after = store.read_time_range("price", i64::MIN, i64::MAX).await.expect("reads");
		let catalog = store.index().series_of("price").await.expect("the catalog reads");
		drop(store);
		assert_eq!(report, OpenReport { created_new: false, migrated_from: None, applied: vec!["0006_tag5_series".into(), "0007_series_time_index".into(), "0008_upsert_keys".into()], recovery_report_only: false });
		assert_eq!(settled, StoreFormat { applied_through: Some("0008_upsert_keys".into()), ..format.clone() }, "layout, floors and identity unchanged");
		assert_eq!((settled.layout_version, settled.min_read_layout, settled.min_write_layout), (2, 2, 2), "the catalog is a layout-2 migration (DECISIONS: TAG-5)");
		assert_eq!(uuid, format.store_uuid, "the store keeps its identity");
		assert_eq!(after, before, "reads are unchanged");
		assert!(catalog.is_empty(), "the migration registers no series: series 0 is implicit");
		let schema = index_schema(&root).await;
		assert!(schema.contains("table series: Text(\"aspect\") Text(\"TEXT\") Integer(1) Null Integer(1) | Text(\"series_key\") Text(\"TEXT\") Integer(1) Null Integer(2) | Text(\"series_id\") Text(\"INTEGER\") Integer(1) Null Integer(0)\n"), "series(aspect, series_key, series_id), keyed on (aspect, series_key):\n{schema}");
		let rows_after: Vec<String> = schema.lines().filter(|line| line.starts_with("row ")).map(str::to_string).collect();
		assert_eq!(rows_after, rows_before, "no segment_index row is rewritten");
		assert!(schema.contains("meta Text(\"applied_through\") Text(\"0008_upsert_keys\")\n"), "{schema}");
	}

	/// TAG-5, R10: the repair pass seeds each aspect's series allocator past the ids its
	/// catalog registered as well as the ids its rows hold, and seeds an aspect the
	/// catalog names even when the index holds none of its rows. So an allocator row
	/// lost after every row of a series was quarantined cannot hand that id out again.
	#[tokio::test]
	async fn r10_seeds_next_series_id_past_the_catalog_as_well_as_the_rows() {
		use crate::{
			types::index_txn::{IndexOp, IndexTxn}, SeriesId, TagSet
		};

		let dir = TempDir::new().expect("tempdir");
		let root = dir.path().join("store");
		let store = SegmentStore::open(&root).await.expect("creates the store");
		store.seal("price", &schema(), &[0, 10], &[bd("1"), bd("2")]).await.expect("seals series 0");
		let key = TagSet::from_pairs([("host", "a")]).expect("valid tags").series_key().clone();
		let register = |aspect: &str, id: u64| [IndexOp::RegisterSeries { aspect: aspect.to_string(), series_id: SeriesId(id), series_key: key.clone() }, IndexOp::SeedSeries { aspect: aspect.to_string(), next_series_id: id + 1 }];
		store.commit_index(&IndexTxn::new(register("price", 5).into_iter().chain(register("ghost", 3)).collect())).await.expect("registers");
		drop(store);
		// Lose every allocator row: only the rows and the catalog are left to seed from.
		on_raw_index(&root, &["DELETE FROM aspect_seq"]).await;

		let store = SegmentStore::open(&root).await.expect("reopens");
		let report = store.fsck(false).await.expect("fsck");
		let (price, ghost) = (store.index().next_series_id("price").await.expect("reads"), store.index().next_series_id("ghost").await.expect("reads"));
		let ghost_rows = store.index().rows("ghost").await.expect("reads");
		drop(store);
		assert!(ghost_rows.is_empty(), "ghost has catalog rows only");
		assert_eq!(report.seeded, 2, "both the indexed aspect and the catalog-only one are seeded: {report:?}");
		assert_eq!(price, 6, "past the catalog's id 5, though every row of price is series 0");
		assert_eq!(ghost, 4, "past the catalog's id 3, though the index holds no ghost row");
	}

	/// An open that stopped part way leaves its marker unsettled, and the next open reports
	/// what it finished from that: a new store whose creation stopped after its databases
	/// existed but before its migrations were recorded (the first marker of a new store)
	/// is reported as created, not migrated from its own layout; a layout-1 store whose
	/// upgrade stopped after the floor raise is reported as migrated from layout 1.
	#[tokio::test]
	async fn an_unfinished_open_is_reported_as_what_it_was() {
		let dir = TempDir::new().expect("tempdir");
		let root = dir.path().join("store");
		let store = SegmentStore::open(&root).await.expect("creates the store");
		let format = store.store_format();
		drop(store);
		put_marker(&root, &StoreFormat { migrating_to: Some(SUPPORTED_LAYOUT), applied_through: None, ..format.clone() });
		on_raw_index(&root, &["DELETE FROM store_migrations"]).await;
		let store = SegmentStore::open(&root).await.expect("finishes the creation");
		let (report, uuid) = (store.open_report().clone(), store.store_uuid().to_string());
		drop(store);
		assert_eq!(report, OpenReport { created_new: true, migrated_from: None, applied: vec!["0001_baseline".into(), "0002_s6_s7".into(), "0003_fre14_legacy_catalog".into(), "0004_d_s12".into(), "0005_d_s13".into(), "0006_tag5_series".into(), "0007_series_time_index".into(), "0008_upsert_keys".into()], recovery_report_only: false });
		assert_eq!(uuid, format.store_uuid);

		let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pre_v2_store");
		let legacy = dir.path().join("legacy");
		copy_tree(&fixture.join("root"), &legacy);
		let raised = StoreFormat { layout_version: 1, min_read_layout: 2, min_write_layout: 2, last_written_layout: 1, migrating_to: Some(2), applied_through: None, ..format };
		put_marker(&legacy, &raised);
		let store = SegmentStore::open(&legacy).await.expect("finishes the upgrade");
		let report = store.open_report().clone();
		drop(store);
		assert_eq!(report, OpenReport { created_new: false, migrated_from: Some(1), applied: vec!["0001_baseline".into(), "0002_s6_s7".into(), "0003_fre14_legacy_catalog".into(), "0004_d_s12".into(), "0005_d_s13".into(), "0006_tag5_series".into(), "0007_series_time_index".into(), "0008_upsert_keys".into()], recovery_report_only: false });
	}

	/// FRE-12a's `ensure_floor`: raising a floor writes the marker first, fsynced, and
	/// returns the op that raises `store_meta` inside the caller's transaction; after the
	/// first call for a level nothing is written. A level newer than this build's is
	/// refused.
	#[tokio::test]
	async fn ensure_floor_raises_the_marker_first_and_store_meta_in_the_callers_transaction() {
		let dir = TempDir::new().expect("tempdir");
		let root = dir.path().join("store");
		let store = SegmentStore::open(&root).await.expect("creates the store");
		assert_eq!(store.ensure_floor(SUPPORTED_LAYOUT).await.expect("ensures"), None, "a fresh store already has this build's floors");
		// Play a store whose floors are below the level a write needs: what the first
		// layout-3 frame feature will find on a layout-2 store.
		let lowered = StoreFormat { min_read_layout: 1, min_write_layout: 1, ..store.store_format() };
		*store.format.write().expect("not poisoned") = lowered.clone();
		store.floor_ensured.store(1, Ordering::Release);
		store.commit_index(&IndexTxn::new(vec![IndexOp::MetaSet { key: "min_read_layout".into(), value: "1".into() }, IndexOp::MetaSet { key: "min_write_layout".into(), value: "1".into() }])).await.expect("lowers store_meta");
		put_marker(&root, &lowered);

		let op = store.ensure_floor(2).await.expect("raises the floor");
		let marker = crate::types::store_format::read_marker(&root).await.expect("reads").expect("a marker");
		let meta_before = (store.index().meta("min_read_layout").await.expect("reads"), store.index().meta("min_write_layout").await.expect("reads"));
		store.commit_index(&IndexTxn::new(op.clone().into_iter().collect())).await.expect("the caller's transaction commits");
		let meta_after = (store.index().meta("min_read_layout").await.expect("reads"), store.index().meta("min_write_layout").await.expect("reads"));
		let again = store.ensure_floor(2).await.expect("ensures again");
		let too_new = store.ensure_floor(SUPPORTED_LAYOUT + 1).await.expect_err("a newer level is refused").to_string();
		drop(store);
		assert_eq!(op, Some(IndexOp::RaiseFloor { level: 2 }));
		assert_eq!((marker.min_read_layout, marker.min_write_layout), (2, 2), "the marker is raised before the op is handed back");
		assert_eq!(meta_before, (Some("1".into()), Some("1".into())), "store_meta waits for the caller's transaction");
		assert_eq!(meta_after, (Some("2".into()), Some("2".into())));
		assert_eq!(again, None, "the floor is cached after the first call");
		assert!(too_new.contains(&format!("writes layouts up to {SUPPORTED_LAYOUT}")), "{too_new}");
	}

	/// Rewrite every `segment_index` row's path under `root` with `rewrite(aspect, id,
	/// path)`, and insert `extra` rows (aspect, id, path) copied from the first row.
	async fn rewrite_index_paths(root: &Path, rewrite: impl Fn(&str, i64, &str) -> String, extra: &[(&str, i64, &str)]) {
		let db = turso::Builder::new_local(&root.join("segment_index.db").to_string_lossy()).build().await.expect("opens raw");
		let conn = db.connect().expect("connects");
		let mut rows = conn.query("SELECT aspect, id, path FROM segment_index", ()).await.expect("reads");
		let mut all = Vec::new();
		while let Some(row) = rows.next().await.expect("reads a row") {
			all.push((row.get_value(0).expect("aspect").as_text().cloned().expect("text"), *row.get_value(1).expect("id").as_integer().expect("integer"), row.get_value(2).expect("path").as_text().cloned().expect("text")));
		}
		drop(rows);
		for (aspect, id, path) in &all {
			conn.execute("UPDATE segment_index SET path = ? WHERE aspect = ? AND id = ?", turso::params![rewrite(aspect, *id, path), aspect.clone(), *id]).await.expect("rewrites a path");
		}
		for (aspect, id, path) in extra {
			let (from_aspect, from_id, _) = &all[0];
			conn.execute("INSERT INTO segment_index SELECT ?, ?, ?, format_version, physical_type, time_unit, row_count, null_count, time_sorted, min_ts, max_ts, min_value, max_value, byte_len FROM segment_index WHERE aspect = ? AND id = ?", turso::params![aspect.to_string(), *id, path.to_string(), from_aspect.clone(), *from_id]).await.expect("inserts a row");
		}
		drop(conn);
		drop(db);
	}

	/// Release plan D-S6 / FRE-13: a layout-1 store whose index records a frame outside
	/// `segments/` (written through the aspect-name traversal of pre-1.0 builds: an aspect
	/// named `../evil` sealed into `root/evil-0.weftseg`) is refused at its migration to
	/// layout 2 with `UnsafeLegacyPath`, naming the aspect, before any migration applies
	/// and before a marker is written: no `STORE_FORMAT`, no `store_meta` or
	/// `store_migrations`, and nothing moved or quarantined. Once the row is gone the store
	/// upgrades. The same store with its paths in Windows form
	/// (`C:\…\segments\price-0.weftseg`) upgrades and reads exactly what the fixture holds.
	#[tokio::test]
	async fn an_escaped_layout_one_path_is_refused_and_windows_paths_resolve() {
		let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pre_v2_store");
		let expected_reads = std::fs::read_to_string(fixture.join("expected_reads.txt")).expect("reads the expected reads");
		let dir = TempDir::new().expect("tempdir");

		let root = dir.path().join("escaped");
		copy_tree(&fixture.join("root"), &root);
		std::fs::copy(root.join("segments/price-0.weftseg"), root.join("evil-0.weftseg")).expect("plants the escaped frame");
		rewrite_index_paths(&root, |_, _, path| path.to_string(), &[("../evil", 0, "/var/tmp/weft-pre-v2-fixture/root/segments/../evil-0.weftseg")]).await;
		let segments_before = tree_bytes(&root.join("segments"));
		for attempt in 1..=2 {
			let err = SegmentStore::open(&root).await.err().unwrap_or_else(|| panic!("attempt {attempt}: an escaped frame path is refused"));
			assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::UnsafeLegacyPath { aspects: vec!["../evil".into()] }), "attempt {attempt}: {err:#}");
			assert!(format!("{err:#}").contains("\"../evil\""), "the error names the aspect: {err:#}");
		}
		assert_eq!(tree_bytes(&root.join("segments")), segments_before, "nothing under segments/ was moved, quarantined or rewritten");
		assert!(root.join("evil-0.weftseg").exists() && !root.join("segments/quarantine").exists(), "the escaped frame is left where it is");
		assert!(!root.join(STORE_FORMAT_FILE).exists(), "no marker was written");
		let tables = index_tables(&root).await;
		assert!(tables.contains(&"segment_index".to_string()) && !tables.iter().any(|table| ["store_meta", "store_migrations", "aspect_seq"].contains(&table.as_str())), "no migration applied, 0001 included: {tables:?}");
		let db = turso::Builder::new_local(&root.join("segment_index.db").to_string_lossy()).build().await.expect("opens raw");
		db.connect().expect("connects").execute("DELETE FROM segment_index WHERE aspect = '../evil'", ()).await.expect("drops the escaped row");
		drop(db);
		let store = SegmentStore::open(&root).await.expect("upgrades once the escaped row is gone");
		let reads = fixture_reads(&store).await;
		drop(store);
		assert_eq!(reads, expected_reads);

		let root = dir.path().join("windows");
		copy_tree(&fixture.join("root"), &root);
		rewrite_index_paths(&root, |aspect, id, _| format!("C:\\weft\\pre-v2\\root\\segments\\{aspect}-{id}.weftseg"), &[]).await;
		let store = SegmentStore::open(&root).await.expect("a store written on Windows upgrades");
		let reads = fixture_reads(&store).await;
		drop(store);
		assert_eq!(reads, expected_reads, "and reads its frames through the split on \\");
	}

	/// Set only in the child process `a_process_wide_poison_refuses_every_store_write`
	/// starts: the root to open.
	const GLOBAL_POISON_CHILD_ROOT: &str = "WEFT_TEST_GLOBAL_POISON_CHILD_ROOT";

	/// The body that child runs (the process-wide poison cannot be unset, so it runs in a
	/// process of its own): poison the process, as the panic hook does for a panic inside
	/// a control-plane write, and check that an open store refuses writes, keeps reading
	/// and wakes its poison watchers, and that no store opens any more, the same root
	/// reopened or a new one, with nothing of either root touched. In a normal test run
	/// the variable is unset and this does nothing.
	#[tokio::test]
	async fn global_poison_child() {
		let Some(root) = std::env::var_os(GLOBAL_POISON_CHILD_ROOT) else { return };
		let root = PathBuf::from(root);
		let reason = "the process is write-poisoned: a panic inside a control-plane write";
		let store = SegmentStore::open(root.join("before")).await.expect("opens");
		store.seal("price", &schema(), &[0], &[bd("1")]).await.expect("seals before the poison");
		assert!(poison_global().set("a panic inside a control-plane write"));
		assert!(!poison_global().set("a second one"), "the first poisoning is the one kept");
		let woken = tokio::time::timeout(std::time::Duration::from_secs(10), store.wait_until_poisoned()).await.expect("watchers wake");
		let seal = store.seal("price", &schema(), &[10], &[bd("2")]).await.expect_err("a seal is refused");
		let declare = store.declare("temp", &schema()).await.expect_err("a declare is refused");
		for err in [seal, declare] {
			assert_eq!(err.downcast_ref::<Poisoned>().map(|poisoned| poisoned.reason.as_str()), Some(reason), "refused as poisoned: {err:#}");
		}
		assert!(store.poisoned().is_some());
		let (ts, _) = store.read_time_range("price", i64::MIN, i64::MAX).await.expect("reads go on");
		assert_eq!(ts, vec![0]);
		drop(store);

		let before = tree_bytes(&root.join("before"));
		for which in ["before", "after"] {
			let err = SegmentStore::open(root.join(which)).await.err().unwrap_or_else(|| panic!("{which}: a poisoned process opens no store"));
			assert_eq!(err.downcast_ref::<Poisoned>().map(|poisoned| poisoned.reason.as_str()), Some(reason), "{which}: {err:#}");
		}
		assert!(tree_bytes(&root.join("before")) == before, "the refused reopen touched nothing, not even LOCK");
		assert!(!root.join("after").exists(), "nor did it create a new root");
		println!("global poison child: {}", woken.reason);
	}

	/// Robustness track ROB-2: the process-wide poison (`poison_global`) refuses every
	/// write of every store while reads go on, and every open, which writes.
	#[tokio::test]
	async fn a_process_wide_poison_refuses_every_store_write() {
		let dir = TempDir::new().expect("tempdir");
		let exe = std::env::current_exe().expect("finds the test binary");
		let out = tokio::process::Command::new(exe).args(["types::segment_store::tests::global_poison_child", "--exact", "--nocapture", "--test-threads=1"]).env(GLOBAL_POISON_CHILD_ROOT, dir.path()).output().await.expect("runs the child");
		let stdout = String::from_utf8_lossy(&out.stdout);
		assert!(out.status.success(), "{out:?}");
		assert!(stdout.contains("global poison child: the process is write-poisoned: a panic inside a control-plane write") && stdout.contains("1 passed"), "the child ran: {stdout}");
	}

	#[test]
	fn a_frame_path_resolves_against_the_current_root() {
		let root = Path::new("/srv/weft/store");
		let resolve = |stored: &str| resolve_frame_path(root, stored).unwrap_or_else(|e| panic!("{stored}: {e:#}"));
		// Under the current root: the same file.
		assert_eq!(resolve("/srv/weft/store/segments/price-0.weftseg"), root.join("segments/price-0.weftseg"));
		// Written under a root that moved: the tail after `segments` lands in this root's.
		assert_eq!(resolve("/old/place/segments/price-0.weftseg"), root.join("segments/price-0.weftseg"));
		assert_eq!(resolve("/old/place/segments/quarantine/price-0.weftseg"), root.join("segments/quarantine/price-0.weftseg"));
		// The last `segments` component counts: a root that itself sits under one.
		assert_eq!(resolve("/data/segments/old-root/segments/temp-3.weftseg"), root.join("segments/temp-3.weftseg"));
		// A relative root resolves the same way.
		assert_eq!(resolve_frame_path(Path::new("store"), "store/segments/a-1.weftseg").expect("resolves"), Path::new("store/segments/a-1.weftseg"));
		assert_eq!(resolve_frame_path(Path::new("store"), "elsewhere/segments/a-1.weftseg").expect("resolves"), Path::new("store/segments/a-1.weftseg"));
		// Component-wise, not textual: a sibling whose name extends the root's is not under it.
		assert_eq!(resolve("/srv/weft/store-old/segments/a-1.weftseg"), root.join("segments/a-1.weftseg"));
		// Split on both separators (release plan, D-S6): a store written on Windows reads
		// here, and one written here on Windows.
		assert_eq!(resolve("C:\\weft\\old-root\\segments\\price-0.weftseg"), root.join("segments/price-0.weftseg"));
		assert_eq!(resolve("\\\\server\\share\\store\\segments\\quarantine\\price-0.weftseg"), root.join("segments/quarantine/price-0.weftseg"));
		assert_eq!(resolve("D:/mixed\\segments/price-0.weftseg"), root.join("segments/price-0.weftseg"));
		// Nothing safe to resolve to: refused, never read where it points.
		for escaped in ["/old/place/frames/price-0.weftseg", "/old/place/segments", "/old/place/segments/", "/old/segments/../../etc/passwd", "/srv/weft/store/segments/../price-0.weftseg", "C:\\old\\segments\\..\\x-0.weftseg", "price-0.weftseg", ""] {
			let err = resolve_frame_path(root, escaped).expect_err(escaped).to_string();
			assert!(err.starts_with(&format!("refusing to read frame {escaped:?}")), "{escaped}: {err}");
		}
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
	#[cfg_attr(windows, ignore = "Windows file locks are mandatory: this test images or reads the files of a store that is still open, which Turso holds locked (ERROR_LOCK_VIOLATION). Linux and macOS CI run it; Windows crash coverage is the process-kill tests")]
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
		// something: frames for the seals, segment_index.db for the rollup rebuilds, a squash
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
		assert_eq!(names_in(&dir.path().join("segments")), vec!["price-1.weftseg", "price~g1~p1.weftseg"], "no refused write left a frame behind");

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
		let base = IndexRow { desc: write_frame(dir.path(), "price", 0, &[(0, "1")]), gen: 1, prec: Some(1), frame_crc: Some(7), commit_epoch: Some(1), series_id: 0 };
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

	/// Set only in the child process `an_ambiguous_commit_poisons_and_never_exits_the_process`
	/// starts: the root to open.
	const AMBIGUOUS_CHILD_ROOT: &str = "WEFT_TEST_AMBIGUOUS_CHILD_ROOT";

	/// The body that child runs: commit a phantom transaction with `S-commit-phantom` armed
	/// by the parent's `WEFT_FAULT`, with `WEFT_ON_AMBIGUOUS_COMMIT=exit` set as a
	/// deployment that wants the process to exit would set it. The store must poison
	/// itself and return the ambiguous error, and the child goes on to report it: the
	/// library never exits the process (release plan C-1). In a normal test run the
	/// variable is unset and this does nothing.
	#[tokio::test]
	async fn ambiguous_commit_child() {
		let Some(root) = std::env::var_os(AMBIGUOUS_CHILD_ROOT) else { return };
		crate::types::durable::fault::suppress_core_dump();
		let root = PathBuf::from(root);
		let store = SegmentStore::open(&root).await.expect("opens");
		let phantom = write_frame(&root, "price", 7, &[(70, "7")]);
		let err = store.commit_index(&phantom_upsert("price", phantom)).await.expect_err("the phantom COMMIT reports an error");
		assert!(err.downcast_ref::<IndexTxnError>().is_some_and(IndexTxnError::is_ambiguous), "{err:#}");
		let poisoned = tokio::time::timeout(std::time::Duration::from_secs(10), store.wait_until_poisoned()).await.expect("the poison is announced to its watchers");
		assert_eq!(store.poisoned(), Some(poisoned.clone()));
		println!("child still running, poisoned by: {}", poisoned.reason);
	}

	/// Release plan C-1: the library never calls `process::exit`, whatever the environment
	/// says. Under `WEFT_ON_AMBIGUOUS_COMMIT=exit` an ambiguous COMMIT still only poisons
	/// the store and wakes its poison watchers (an embedder such as `weft-server` exits on
	/// that itself); the process lives on. The restarted store has the transaction (the
	/// fault fired after its COMMIT) and accepts writes.
	#[tokio::test]
	async fn an_ambiguous_commit_poisons_and_never_exits_the_process() {
		let dir = TempDir::new().expect("tempdir");
		let exe = std::env::current_exe().expect("finds the test binary");
		let out = tokio::process::Command::new(exe).args(["types::segment_store::tests::ambiguous_commit_child", "--exact", "--nocapture", "--test-threads=1"]).env(AMBIGUOUS_CHILD_ROOT, dir.path()).env(fault::FAULT_ENV, "S-commit-phantom:err").env(AMBIGUOUS_COMMIT_ENV, "exit").output().await.expect("runs the child");
		let stdout = String::from_utf8_lossy(&out.stdout);
		assert!(out.status.success(), "the child was not exited by the library: {out:?}");
		assert!(stdout.contains("child still running, poisoned by: ") && stdout.contains("injected fault at S-commit-phantom") && stdout.contains("1 passed"), "the child saw the poison and finished: {stdout}");

		let store = SegmentStore::open(dir.path()).await.expect("the restart opens the store");
		let poisoned = store.poisoned();
		let ids: Vec<u64> = store.index().all("price").await.expect("reads").iter().map(|d| d.id).collect();
		let point = store.read_point("price", 70).await.expect("reads the committed row");
		let sealed = store.seal("price", &schema(), &[80], &[bd("8")]).await;
		drop(store);
		assert_eq!(poisoned, None);
		assert_eq!(ids, vec![7], "the transaction committed");
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
		let (range_ts, range_values) = store.read_time_range("price", i64::MIN, i64::MAX).await.expect("reads");
		drop(store);

		let moved = dir.path().join("moved");
		copy_tree(&original, &moved);
		std::fs::remove_dir_all(&original).expect("removes the original root");

		let store = SegmentStore::open(&moved).await.expect("the moved root opens");
		let after = fixture_reads(&store).await;
		// Segment 3 is out of order (reconcile reads it), segment 0 is sorted (split reads
		// it; at 70, so its suffix [70, 90] overlaps nothing newer, which segment 3's
		// [45, 65] would), and squash decodes every segment the aspect has.
		let reconciled = store.reconcile_segment("price", 3).await.expect("reconciles a frame of the moved root");
		let split = store.split_segment("price", 0, 70).await.expect("splits a frame of the moved root");
		let squashed = store.squash_aspect("price").await.expect("squashes the moved root's frames");
		let (squashed_ts, squashed_values) = store.read_time_range("price", i64::MIN, i64::MAX).await.expect("reads the squashed aspect");
		drop(store);
		assert_eq!(after, before, "the moved root reads exactly what the original did");
		assert!(reconciled);
		assert!(split.is_some());
		assert_eq!(squashed, 4, "five segments (after the split) squash into one");
		// No two segments share a timestamp, so the squash is their union in time order.
		let mut union: Vec<(i64, Option<BigDecimal>)> = range_ts.into_iter().zip(range_values).collect();
		union.sort_by_key(|(t, _)| *t);
		assert_eq!(squashed_ts.into_iter().zip(squashed_values).collect::<Vec<_>>(), union);
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
			let (read_ts, read_values) = store.read_time_range(aspect, ts[0], ts[ts.len() - 1]).await.expect("reads the batch's window");
			if read_ts != ts || read_values != vs.into_iter().map(Some).collect::<Vec<_>>() {
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
			let first_file = std::path::Path::new(&first.path).file_name().and_then(|name| name.to_str()).unwrap_or_default().to_string();
			let orphans: Vec<String> = names_in(&segments).into_iter().filter(|name| *name != first_file).collect();
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

	/// Crash-consistency S7 and S9: a split's suffix takes its id from the allocator seals
	/// use, so it never reuses the id of a segment a squash or a merge deleted; it used to
	/// take `MAX(id) + 1`, here id 1 (deleted by the squash). An overlap split's outputs
	/// take its members' ids (S9, release plan D17 P1), so its suffix takes the second
	/// member's (4, the late batch) and allocates none; before S9 it took a fresh id (5),
	/// and before S7 `MAX(id) + 1` (3).
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
		assert_eq!(ids, vec![0, 3, 4], "the overlap split kept its prefix at 3 and put its suffix at its other member's id, 4");
		assert_eq!(ts, vec![2000, 2010, 2020, 2030, 2040, 2050, 2060, 2070, 2080, 2090, 2100]);
		let tail: Vec<String> = vs.into_iter().flatten().skip(8).map(|v| v.to_plain_string()).collect();
		assert_eq!(tail, vec!["-1", "-2", "-3"], "the late batch wins where it overlaps");
		assert_eq!(next, 5, "and the next seal goes on from there, the overlap split having allocated no id");
	}

	/// Crash-consistency S7: every public maintenance entry point takes its aspect's
	/// maintenance lock. While another operation holds the aspect, each per-aspect entry
	/// point waits up to the store's `maintenance_wait` and then fails with
	/// `MaintenanceBusy`, and each store-wide sweep under `MaintenanceWait::Skip` lists
	/// the aspect in `busy`, all without changing a file. A seal is not maintenance and
	/// does not wait. Once the holder lets go within a `MaintenanceWait::Wait`, the sweep
	/// takes the aspect and maintains it.
	#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
	#[cfg_attr(windows, ignore = "Windows file locks are mandatory: this test images or reads the files of a store that is still open, which Turso holds locked (ERROR_LOCK_VIOLATION). Linux and macOS CI run it; Windows crash coverage is the process-kill tests")]
	async fn a_busy_aspect_is_refused_skipped_or_waited_for() {
		let dir = TempDir::new().expect("tempdir");
		let mut store = std::sync::Arc::new(SegmentStore::open(dir.path()).await.expect("opens").with_maintenance_wait(std::time::Duration::from_millis(100)));
		store.declare("price", &schema()).await.expect("declares");
		// Out of order and overlapping, so every operation would have work to do.
		store.seal("price", &schema(), &[30, 10, 20], &[bd("3"), bd("1"), bd("2")]).await.expect("seals");
		store.seal("price", &schema(), &[25, 15, 35], &[bd("5"), bd("4"), bd("6")]).await.expect("seals");
		let held = store.locks.try_maint("price").expect("the aspect is free");

		let started = std::time::Instant::now();
		let waited = store.reconcile_aspect("price").await.expect_err("the aspect is busy");
		let elapsed = started.elapsed();
		std::sync::Arc::get_mut(&mut store).expect("no other handle to the store yet").maintenance_wait = std::time::Duration::ZERO;
		let before = tree_bytes(dir.path());
		let policy = SplitPolicy::new(1);
		let per_aspect: Vec<(&str, Result<()>)> = vec![("reconcile_segment", store.reconcile_segment("price", 0).await.map(drop)), ("split_segment", store.split_segment("price", 1, 20).await.map(drop)), ("reconcile_aspect", store.reconcile_aspect("price").await.map(drop)), ("reconcile_aspect_if_unsorted_exceeds", store.reconcile_aspect_if_unsorted_exceeds("price", 1).await.map(drop)), ("reconcile_aspect_hot_cold", store.reconcile_aspect_hot_cold("price", 1).await.map(drop)), ("reconcile_overlaps", store.reconcile_overlaps("price").await.map(drop)), ("reconcile_overlaps_with_policy", store.reconcile_overlaps_with_policy("price", policy).await.map(drop)), ("squash_aspect", store.squash_aspect("price").await.map(drop)), ("squash_aspect_if_exceeds", store.squash_aspect_if_exceeds("price", 1).await.map(drop)), ("squash_aspect_to_target_rows", store.squash_aspect_to_target_rows("price", 10).await.map(drop)), ("squash_aspect_to_target_rows_if_fragmented", store.squash_aspect_to_target_rows_if_fragmented("price", 10).await.map(drop))];
		let skip = MaintenanceWait::Skip;
		let sweeps: Vec<(&str, Result<Vec<String>>)> = vec![("reconcile_all_over_threshold", store.reconcile_all_over_threshold(1, skip).await.map(|s| s.busy)), ("reconcile_all_hot_cold", store.reconcile_all_hot_cold(1, skip).await.map(|s| s.busy)), ("reconcile_all_overlaps", store.reconcile_all_overlaps(skip).await.map(|s| s.busy)), ("reconcile_all_overlaps_with_policy", store.reconcile_all_overlaps_with_policy(policy, skip).await.map(|s| s.busy)), ("squash_all_over_threshold", store.squash_all_over_threshold(1, skip).await.map(|s| s.busy)), ("squash_all_to_target_rows", store.squash_all_to_target_rows(10, skip).await.map(|s| s.busy)), ("squash_all_to_target_rows_if_fragmented", store.squash_all_to_target_rows_if_fragmented(10, skip).await.map(|s| s.busy))];
		let after = tree_bytes(dir.path());
		let sealed = tokio::time::timeout(std::time::Duration::from_secs(30), store.seal("price", &schema(), &[50, 40], &[bd("8"), bd("7")])).await;

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
		assert_eq!(waited_for.busy, Vec::<String>::new(), "a store sweep takes the aspect once it is let go");
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

	/// D-S12: startup recovery quarantines a frame damaged after its seal: the row
	/// is gone from reads, the file is under `segments/quarantine/`, and the record
	/// says corrupt with the segment's time span and series id.
	#[tokio::test]
	async fn startup_recovery_quarantines_a_corrupt_frame() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		let descriptor = store.seal("price", &schema(), &[0, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.expect("seals");
		let frame = PathBuf::from(&descriptor.path);
		drop(store);
		// Damage a value byte mid-frame, leaving the trailer stale: CRC fails.
		let mut bytes = std::fs::read(&frame).expect("reads frame");
		let middle = bytes.len() / 2;
		bytes[middle] ^= 0xff;
		std::fs::write(&frame, &bytes).expect("damages frame");
		let store = SegmentStore::open(dir.path()).await.expect("reopens");
		let report = store.recovery_report();
		assert_eq!(report.checked, 1, "one row examined");
		assert_eq!(report.quarantined, 1, "the damaged frame quarantines as corrupt");
		assert_eq!(report.suspected, 0);
		assert!(!report.incompatible_store);
		let records = store.quarantine_list(Some("price")).await.expect("lists");
		assert_eq!(records.len(), 1);
		assert_eq!(records[0].class, crate::QuarantineClass::Corrupt);
		assert_eq!((records[0].min_ts, records[0].max_ts), (Some(0), Some(20)));
		assert_eq!(records[0].series_id, 0);
		assert_eq!(store.segment_count("price").await.expect("counts"), 0, "the row is gone from reads");
		assert!(dir.path().join("segments/quarantine").join(frame.file_name().unwrap()).exists(), "the file moved under segments/quarantine/");
		assert!(!frame.exists(), "the file no longer sits in segments/");
	}

	/// D-S12: a frame whose CRC verifies but whose version this build predates is
	/// suspect, never auto-purged, and makes the report incompatible: the version
	/// field is re-stamped with its trailer, so the CRC verifies while the version
	/// check is what fails.
	#[tokio::test]
	async fn startup_recovery_suspects_a_newer_format_frame_and_never_ttl_purges_it() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		let descriptor = store.seal("price", &schema(), &[0, 10], &[bd("1"), bd("2")]).await.expect("seals");
		let frame = PathBuf::from(&descriptor.path);
		drop(store);
		let mut bytes = std::fs::read(&frame).expect("reads frame");
		// A frame header is 7 magic bytes, then the u16 version; re-stamp the
		// trailer so the CRC verifies and the version check is what fails.
		bytes[7..9].copy_from_slice(&99u16.to_le_bytes());
		let body = bytes.len() - 4;
		let checksum = weft_physical_type::crc32(&bytes[..body]);
		bytes[body..].copy_from_slice(&checksum.to_le_bytes());
		std::fs::write(&frame, &bytes).expect("rewrites frame");
		let store = SegmentStore::open(dir.path()).await.expect("reopens");
		let report = store.recovery_report();
		assert_eq!(report.suspected, 1, "a newer-format frame is suspect, not corrupt");
		assert_eq!(report.quarantined, 0);
		assert!(report.incompatible_store, "a newer-format cause reports incompatible_store");
		assert!(store.has_suspect_segments());
		// Behaviour 1: a suspect frame is excluded from reads — the row is gone
		// and the file is moved to quarantine/, so reads return nothing.
		assert_eq!(store.segment_count("price").await.expect("counts"), 0, "the suspect row is gone from segment_count");
		let (ts, vs) = store.read_time_range("price", 0, 20).await.expect("reads range");
		assert!(ts.is_empty(), "a suspect frame returns no timestamps from read_time_range");
		assert!(vs.is_empty(), "a suspect frame returns no values from read_time_range");
		// Behaviour 2: TTL purge must not touch a suspect record, however old it is: age it by
		// rewriting its record, then purge.
		let mut record = store.quarantine_list(Some("price")).await.expect("lists").into_iter().next().expect("one record");
		record.quarantined_ms = 0;
		store.commit_index(&crate::types::index_txn::IndexTxn::new(vec![crate::types::index_txn::IndexOp::QuarantineInsert { record }])).await.expect("ages record");
		assert_eq!(store.purge_expired_quarantine().await.expect("purges"), 0, "suspect records never TTL-purge");
		assert_eq!(store.quarantine_list(Some("price")).await.expect("lists").len(), 1);
	}

	/// ROB-22/C5: in a mix of a good frame and a suspect frame for the same
	/// aspect, recovery quarantines only the suspect one, leaves the good one
	/// readable, and the suspect file is never TTL-purged.
	#[tokio::test]
	async fn mixed_good_and_suspect_frames_survive_recovery_with_good_data_readable() {
		let dir = TempDir::new().expect("tempdir");
		let frame = {
			let store = SegmentStore::open(dir.path()).await.expect("opens");
			// A good segment at [0, 10].
			let _good = store.seal("price", &schema(), &[0, 10], &[bd("1"), bd("2")]).await.expect("seals good");
			// A second segment at [20, 30] that recovery will suspect.
			let desc = store.seal("price", &schema(), &[20, 30], &[bd("3"), bd("4")]).await.expect("seals second");
			let frame = PathBuf::from(&desc.path);
			drop(store);
			frame
		};
		let mut bytes = std::fs::read(&frame).expect("reads frame");
		bytes[7..9].copy_from_slice(&99u16.to_le_bytes());
		let body = bytes.len() - 4;
		let checksum = weft_physical_type::crc32(&bytes[..body]);
		bytes[body..].copy_from_slice(&checksum.to_le_bytes());
		std::fs::write(&frame, &bytes).expect("rewrites frame as suspect");
		let store = SegmentStore::open(dir.path()).await.expect("reopens");
		let report = store.recovery_report();
		assert_eq!(report.suspected, 1, "the newer-format frame is suspect");
		assert_eq!(report.quarantined, 0);
		assert!(store.has_suspect_segments());
		// Behaviour 1: only the good frame's data is readable.
		assert_eq!(store.segment_count("price").await.expect("counts"), 1, "only the good segment remains");
		let (ts, vs) = store.read_time_range("price", 0, 40).await.expect("reads range");
		assert_eq!(ts, vec![0, 10], "only the good frame's timestamps appear");
		assert_eq!(vs, vec![Some(bd("1")), Some(bd("2"))], "only the good frame's values appear");
		let records = store.quarantine_list(Some("price")).await.expect("lists");
		assert_eq!(records.len(), 1);
		assert_eq!(records[0].class, crate::QuarantineClass::Suspect);
		// Behaviour 2: TTL purge ignores the suspect record; age it then check.
		let mut aged = records[0].clone();
		aged.quarantined_ms = 0;
		store.commit_index(&crate::types::index_txn::IndexTxn::new(vec![crate::types::index_txn::IndexOp::QuarantineInsert { record: aged }])).await.expect("ages record");
		assert_eq!(store.purge_expired_quarantine().await.expect("purges"), 0, "suspect records never TTL-purge — the good frame stays readable");
		let (ts, vs) = store.read_time_range("price", 0, 40).await.expect("reads range after purge attempt");
		assert_eq!(ts, vec![0, 10], "the good data is still there after the purge attempt");
		assert_eq!(vs, vec![Some(bd("1")), Some(bd("2"))]);
		drop(store);
	}

	/// D-S12: a missing indexed file quarantines as corrupt and reads as gone, and
	/// its record carries the row back for a release.
	#[tokio::test]
	async fn startup_recovery_quarantines_a_missing_frame() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		let descriptor = store.seal("price", &schema(), &[0, 10], &[bd("1"), bd("2")]).await.expect("seals");
		let frame = PathBuf::from(&descriptor.path);
		drop(store);
		std::fs::remove_file(&frame).expect("deletes frame");
		let store = SegmentStore::open(dir.path()).await.expect("reopens");
		assert_eq!(store.recovery_report().quarantined, 1);
		let records = store.quarantine_list(Some("price")).await.expect("lists");
		assert_eq!(records.len(), 1);
		assert!(records[0].descriptor_json.is_some(), "the record carries the row back");
		assert_eq!(store.segment_count("price").await.expect("counts"), 0);
	}

	/// D-S12: report-only recovery counts but changes nothing — no record, no row
	/// delete, no move — while the repair pass on the same damage quarantines.
	#[tokio::test]
	async fn report_only_recovery_counts_without_changing_anything() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		let descriptor = store.seal("price", &schema(), &[0, 10], &[bd("1"), bd("2")]).await.expect("seals");
		let frame = PathBuf::from(&descriptor.path);
		let mut bytes = std::fs::read(&frame).expect("reads frame");
		let middle = bytes.len() / 2;
		bytes[middle] ^= 0xff;
		std::fs::write(&frame, &bytes).expect("damages frame");
		let report = store.fsck(true).await.expect("report-only fsck");
		assert!(report.report_only);
		assert_eq!(report.quarantined, 1, "report-only counts the corrupt frame");
		assert_eq!(store.segment_count("price").await.expect("counts"), 1, "report-only deletes no row");
		assert_eq!(store.quarantine_list(Some("price")).await.expect("lists").len(), 0, "report-only writes no record");
		assert!(frame.exists(), "report-only moves no file");
		let report = store.fsck(false).await.expect("repair fsck");
		assert_eq!(report.quarantined, 1, "the repair pass quarantines the same frame");
		assert_eq!(store.segment_count("price").await.expect("counts"), 0);
	}

	/// D-S12: fsck binds a row's `frame_crc` when it is unset, and the next pass
	/// finds nothing to re-bind.
	#[tokio::test]
	async fn fsck_binds_an_unset_frame_crc() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.seal("price", &schema(), &[0, 10], &[bd("1"), bd("2")]).await.expect("seals");
		let rows = store.index().rows("price").await.expect("rows");
		assert_eq!(rows.len(), 1);
		// Clear the bound CRC through a guarded replace, to simulate a row from
		// before binding, then fsck must re-bind it to the frame's trailer.
		let mut row = rows.into_iter().next().expect("row");
		let expected = row.version();
		row.frame_crc = None;
		store.commit_index(&crate::types::index_txn::IndexTxn::new(vec![crate::types::index_txn::IndexOp::ReplaceExpected { aspect: "price".to_string(), expected, row }])).await.expect("clears crc");
		let report = store.fsck(false).await.expect("fsck");
		assert_eq!(report.rebound, 1, "the unset CRC is re-bound to the frame's trailer");
		let rows = store.index().rows("price").await.expect("rows");
		assert!(rows[0].frame_crc.is_some());
		let report = store.fsck(false).await.expect("fsck again");
		drop(store);
		assert_eq!(report.rebound, 0, "a bound row is untouched");
	}

	/// A pre-0.2 store rewrote a frame in place and crashed before the index
	/// commit, leaving a row whose length (and range) describe the old file.
	/// fsck rebuilds the row from the frame that decodes, so the length-checked
	/// maintenance reads work again, and keeps the data.
	#[tokio::test]
	async fn fsck_rebuilds_a_row_whose_length_is_stale() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		let sealed = store.seal("price", &schema(), &[0, 10], &[bd("1"), bd("2")]).await.expect("seals");
		let other_dir = TempDir::new().expect("tempdir");
		let other = SegmentStore::open(other_dir.path()).await.expect("opens");
		let bigger = other.seal("price", &schema(), &[0, 10, 20, 30], &[bd("1"), bd("2"), bd("3"), bd("4")]).await.expect("seals");
		// Rewrite the frame in place with a different, valid frame: the row is now stale.
		std::fs::copy(&bigger.path, &sealed.path).expect("rewrites in place");
		drop(other);
		let row = store.index().rows("price").await.expect("rows").into_iter().next().expect("row");
		assert!(store.read_bound_frame(&row).await.is_err(), "the stale row fails the length check");
		let report = store.fsck(false).await.expect("fsck");
		assert_eq!(report.rebound, 1);
		assert_eq!(report.quarantined, 0, "a frame that decodes is kept");
		let row = store.index().rows("price").await.expect("rows").into_iter().next().expect("row");
		assert_eq!(row.desc.byte_len, std::fs::metadata(&sealed.path).expect("stat").len());
		assert_eq!((row.desc.row_count, row.desc.max_ts), (4, Some(30)), "the range comes from the frame");
		assert_eq!(row.desc.id, sealed.id);
		store.read_bound_frame(&row).await.expect("the rebuilt row reads");
		let report = store.fsck(false).await.expect("fsck again");
		drop(store);
		assert_eq!(report.rebound, 0);
	}

	/// D-S12: a clean shutdown marks the flag the next open reports, and a
	/// second open after no clean shutdown reports the crash.
	#[tokio::test]
	async fn clean_shutdown_flag_survives_one_reopen_only() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		assert!(!store.previous_clean_shutdown(), "no process ever marked this store clean");
		store.mark_clean_shutdown().await.expect("marks clean");
		drop(store);
		let store = SegmentStore::open(dir.path()).await.expect("reopens");
		assert!(store.previous_clean_shutdown(), "the clean shutdown is reported once");
		drop(store);
		let store = SegmentStore::open(dir.path()).await.expect("reopens again");
		assert!(!store.previous_clean_shutdown(), "an open without a clean shutdown resets the flag");
		drop(store);
	}

	/// D-S14: `shutdown()` is the closing sequence in one call — the
	/// reaper pass, then the clean-shutdown commit the next open reports.
	#[tokio::test]
	async fn shutdown_marks_the_store_clean_for_the_next_open() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		assert!(!store.previous_clean_shutdown(), "no process ever marked this store clean");
		store.shutdown().await.expect("shuts down");
		drop(store);
		let store = SegmentStore::open(dir.path()).await.expect("reopens");
		assert!(store.previous_clean_shutdown(), "shutdown() committed the clean-shutdown flag");
		drop(store);
	}

	/// D-S12: the TTL purge removes an expired corrupt record and its file, and
	/// leaves a fresh corrupt record alone.
	#[tokio::test]
	async fn ttl_purge_removes_only_expired_corrupt_records() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		let first = store.seal("price", &schema(), &[0, 10], &[bd("1"), bd("2")]).await.expect("seals");
		let second = store.seal("price", &schema(), &[20, 30], &[bd("3"), bd("4")]).await.expect("seals");
		for descriptor in [&first, &second] {
			let frame = PathBuf::from(&descriptor.path);
			let mut bytes = std::fs::read(&frame).expect("reads frame");
			let middle = bytes.len() / 2;
			bytes[middle] ^= 0xff;
			std::fs::write(&frame, &bytes).expect("damages frame");
		}
		let report = store.fsck(false).await.expect("fsck");
		assert_eq!(report.quarantined, 2);
		// Age only the first record past the TTL.
		let mut records = store.quarantine_list(Some("price")).await.expect("lists");
		assert_eq!(records.len(), 2);
		records[0].quarantined_ms = 0;
		store.commit_index(&crate::types::index_txn::IndexTxn::new(vec![crate::types::index_txn::IndexOp::QuarantineInsert { record: records[0].clone() }])).await.expect("ages record");
		let purged = store.purge_expired_quarantine().await.expect("purges");
		assert_eq!(purged, 1, "only the expired record purges");
		let remaining = store.quarantine_list(Some("price")).await.expect("lists");
		drop(store);
		assert_eq!(remaining.len(), 1);
		assert!(!dir.path().join("segments/quarantine").join(Path::new(&first.path).file_name().unwrap()).exists(), "the purged file is unlinked");
		assert!(dir.path().join("segments/quarantine").join(Path::new(&second.path).file_name().unwrap()).exists(), "the fresh record's file stays");
	}

	/// ROB-22/C5: a corrupt frame is excluded from reads immediately and, when its
	/// TTL has passed, is purged (the file unlinked and the record deleted).
	#[tokio::test]
	async fn corrupt_frame_is_excluded_from_reads_and_ttl_purged() {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		let descriptor = store.seal("price", &schema(), &[0, 10], &[bd("1"), bd("2")]).await.expect("seals");
		let frame = PathBuf::from(&descriptor.path);
		drop(store);
		let mut bytes = std::fs::read(&frame).expect("reads frame");
		let middle = bytes.len() / 2;
		bytes[middle] ^= 0xff;
		std::fs::write(&frame, &bytes).expect("damages frame");
		let store = SegmentStore::open(dir.path()).await.expect("reopens");
		let report = store.recovery_report();
		assert_eq!(report.quarantined, 1, "the damaged frame quarantines as corrupt");
		assert_eq!(report.suspected, 0);
		// Behaviour 1: the corrupt frame is gone from reads.
		assert_eq!(store.segment_count("price").await.expect("counts"), 0, "no segments readable");
		let (ts, vs) = store.read_time_range("price", 0, 20).await.expect("reads");
		assert!(ts.is_empty(), "no data returned from a corrupt frame");
		assert!(vs.is_empty(), "no values returned from a corrupt frame");
		let records = store.quarantine_list(Some("price")).await.expect("lists");
		assert_eq!(records.len(), 1);
		assert_eq!(records[0].class, crate::QuarantineClass::Corrupt);
		// Behaviour 3: TTL purge removes an expired corrupt record.
		let mut aged = records[0].clone();
		aged.quarantined_ms = 0;
		store.commit_index(&crate::types::index_txn::IndexTxn::new(vec![crate::types::index_txn::IndexOp::QuarantineInsert { record: aged }])).await.expect("ages record");
		let purged = store.purge_expired_quarantine().await.expect("purges");
		assert_eq!(purged, 1, "the expired corrupt record purges");
		let remaining = store.quarantine_list(Some("price")).await.expect("lists");
		drop(store);
		assert_eq!(remaining.len(), 0, "the record is deleted");
		assert!(!dir.path().join("segments/quarantine").join(Path::new(&descriptor.path).file_name().unwrap()).exists(), "the purged file is unlinked");
	}
}

/// Crash-consistency S8: the write-once reconcile and split, their journal, reaper and
/// reader pins.
#[cfg(test)]
mod swap_tests;

/// A sidecar-served downsample keeps the column's decimal scale.
#[cfg(test)]
mod sidecar_scale_tests;

/// TAG-7: tagged batches seal series-key envelopes under catalog-resolved series ids.
#[cfg(test)]
mod tag_seal_tests;

/// TAG-6: every maintenance pass plans within one series.
#[cfg(test)]
mod tag_maint_tests;

/// TAG-8: every store read targets one series; selectors resolve to one.
#[cfg(test)]
mod tag_read_tests;

/// Phase 4.6: declared upsert keys bound the tags an ingest may carry.
#[cfg(test)]
mod upsert_keys_tests;

/// ROB-13: the stored downsample and the batch point lookup are bounded in
/// the store, at decision 12's numbers.
#[cfg(test)]
mod read_limits_tests;

/// ROB-17: a seal below the disk guard's reserve writes nothing.
#[cfg(test)]
mod disk_guard_tests;

/// Phase 5 memory-mapped GPU I/O: a mapped range read returns exactly the buffered read's rows.
#[cfg(test)]
mod mapped_read_tests;

/// ROB-21a: cooperative between-aspect maintenance cancellation.
#[cfg(test)]
mod cancel_tests;

/// Issue #443: overlapping reads across pruned segments — a range read merges with newer-wins.
#[cfg(test)]
mod overlap_read_tests;
