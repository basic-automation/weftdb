//! On-disk Storage v2 segment store (roadmap **Phase 4.3**).
//!
//! This closes the Phase-4.3 loop. The pieces were in place: `dsp-physical-type`
//! seals a batch into a typed columnar [`Segment`] under a declared
//! [`AspectSchema`] (no silent downcast — hard constraint #4), frames it to a
//! checksummed `.dspseg` byte layout, and describes it with a
//! [`SegmentDescriptor`]; [`SegmentIndexStore`](crate::SegmentIndexStore) persists
//! those descriptors in the libSQL control plane and prunes a query to the segments
//! it must open. [`SegmentStore`] wires them together against the filesystem:
//!
//! - **seal** ([`SegmentStore::seal`] / [`seal_nullable`](SegmentStore::seal_nullable))
//!   encodes a batch under the aspect's schema, writes the sealed `.dspseg` frame to
//!   `segments/<aspect>-<id>.dspseg`, and records its descriptor (with the *realized*
//!   on-disk byte length and path) in the index — one atomic-feeling operation that
//!   leaves a measurement segment on disk and a catalog row pointing at it.
//! - **read** ([`SegmentStore::read_time_range`]) prunes the index by time *first*
//!   (a SQL `WHERE` over min/max ts — no `.dspseg` touched), then opens **only** the
//!   selected files, decodes them, and keeps the rows inside the window. A bounded
//!   range query over a long-lived aspect reads a few segment files, not all of them
//!   — the realized payoff of every per-segment stat the earlier slices built.
//!
//! Boundary (hard constraint #3): the control plane holds *metadata* (the index DB);
//! the measurement bytes live in the `.dspseg` files DSP owns. libSQL never stores a
//! measurement. This slice seals single-block segments via
//! [`AspectSchema::seal`] **and** paged segments via
//! [`AspectSchema::seal_paged`] — the read path dispatches on the recorded frame
//! version, so a paged segment skips pages *within* the file too. The
//! catalog/`metadata.db` registry is a later slice.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use dsp_physical_type::{merge_newer_wins, split_index, AspectSchema, PagedSegment, Segment, SegmentDescriptor, SplitDecision, SplitPolicy, TimeUnit, PAGED_SEGMENT_FORMAT_VERSION};
use dsp_reduce::{Aggregation, Bucket, PartialReduction};
use splimes::{Point, Resolution};

use crate::{AspectCatalog, AspectMetadata, AspectMetadataStore, CatalogStore, PartialSidecar, PartialSidecarPolicy, SegmentIndexStore, SIDECAR_AGGREGATIONS};

/// The `(database, subject)` namespace a [`SegmentStore`] opened with the plain
/// [`open`](SegmentStore::open) constructor declares its aspect schemas under.
const DEFAULT_DATABASE: &str = "default";
/// See [`DEFAULT_DATABASE`].
const DEFAULT_SUBJECT: &str = "default";

/// A filesystem-backed store of sealed `.dspseg` segments with a libSQL segment
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
/// on a 100k-row single-block frame for **+0.41% bytes** (`dsp-physical-type`'s
/// `benches/dodsearch.rs`). It is a **storage-for-latency trade**, so it is **off by
/// default**: `DISABLED` writes byte-for-byte the frames DSP has always written.
///
/// Enable per-deployment with `DSP_SEGMENT_CHECKPOINT_STRIDE` (rows between checkpoints;
/// ~1024 is the sweet spot — stride barely moves the speed but does move the size) and
/// optionally `DSP_SEGMENT_CHECKPOINT_MIN_ROWS` (default [`DEFAULT_CHECKPOINT_MIN_ROWS`];
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
/// (`dsp-physical-type`'s `benches/dodsearch.rs::report_codec_override_cost`). Without
/// this ceiling, enabling `DSP_SEGMENT_CHECKPOINT_STRIDE` would silently bloat exactly
/// those columns. Override with `DSP_SEGMENT_CHECKPOINT_MAX_CODEC_OVERHEAD`.
pub const DEFAULT_CHECKPOINT_MAX_CODEC_OVERHEAD: f64 = 1.25;

impl CheckpointPolicy {
	/// Never write a checkpoint index — the default, and byte-for-byte the historical
	/// frame layout.
	pub const DISABLED: Self = Self { stride: None, min_rows: DEFAULT_CHECKPOINT_MIN_ROWS, max_codec_overhead: DEFAULT_CHECKPOINT_MAX_CODEC_OVERHEAD };

	/// Read the policy from the environment: `DSP_SEGMENT_CHECKPOINT_STRIDE` (absent, zero
	/// or unparseable → [`DISABLED`](Self::DISABLED)), `DSP_SEGMENT_CHECKPOINT_MIN_ROWS`
	/// (→ [`DEFAULT_CHECKPOINT_MIN_ROWS`]) and `DSP_SEGMENT_CHECKPOINT_MAX_CODEC_OVERHEAD`
	/// (→ [`DEFAULT_CHECKPOINT_MAX_CODEC_OVERHEAD`]; a non-finite or `< 1.0` value is
	/// ignored, since a ratio below 1.0 could never be met).
	#[must_use]
	pub fn from_env() -> Self {
		let stride = std::env::var("DSP_SEGMENT_CHECKPOINT_STRIDE").ok().and_then(|v| v.trim().parse::<usize>().ok()).filter(|&s| s > 0);
		let min_rows = std::env::var("DSP_SEGMENT_CHECKPOINT_MIN_ROWS").ok().and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(DEFAULT_CHECKPOINT_MIN_ROWS);
		let max_codec_overhead = std::env::var("DSP_SEGMENT_CHECKPOINT_MAX_CODEC_OVERHEAD").ok().and_then(|v| v.trim().parse::<f64>().ok()).filter(|r| r.is_finite() && *r >= 1.0).unwrap_or(DEFAULT_CHECKPOINT_MAX_CODEC_OVERHEAD);
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
	let (ts, vs) = if descriptor.format_version == PAGED_SEGMENT_FORMAT_VERSION {
		dsp_physical_type::dspseg::read_paged_segment_range(bytes, start, end).map_err(|e| anyhow::anyhow!("decoding paged segment {}: {e}", descriptor.path))?
	} else {
		dsp_physical_type::dspseg::read_segment_range(bytes, start, end).map_err(|e| anyhow::anyhow!("decoding segment {}: {e}", descriptor.path))?
	};
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
	let partial = dsp_reduce::reduce_partial(&points, resolution, None, None, aggregations).map_err(|e| anyhow::anyhow!("reducing segment {}: {e}", descriptor.path))?;
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
	/// is scoped to one subject, so its flat aspect keys (which name the `.dspseg`
	/// files and index rows) are unique within it.
	subject: String,
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
	/// Propagates a filesystem error creating the layout, or any libSQL failure
	/// opening the index or catalog.
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
	/// # Errors
	///
	/// Propagates a filesystem error creating the layout, or any libSQL failure
	/// opening the index or catalog.
	pub async fn open_scoped(root: impl AsRef<Path>, database: &str, subject: &str) -> Result<Self> {
		let root = root.as_ref().to_path_buf();
		let segments_dir = root.join("segments");
		tokio::fs::create_dir_all(&segments_dir).await.with_context(|| format!("creating segments dir {}", segments_dir.display()))?;
		let index_path = root.join("segment_index.db");
		let index = SegmentIndexStore::open(&index_path.to_string_lossy()).await?;
		let metadata_path = root.join("metadata.db");
		let metadata = AspectMetadataStore::open(&metadata_path.to_string_lossy()).await?;
		let catalog_path = root.join("aspect_catalog.db");
		let catalog = AspectCatalog::open(&catalog_path.to_string_lossy()).await?;
		let registry_path = root.join("catalog.db");
		let registry = CatalogStore::open(&registry_path.to_string_lossy()).await?;
		// Record this store's place in the hierarchy so the control plane can enumerate
		// the databases/subjects a root holds (idempotent).
		registry.register_database(database).await?;
		registry.register_subject(database, subject).await?;
		let checkpoints = CheckpointPolicy::from_env();
		let partials = PartialSidecarPolicy::from_env();
		Ok(Self { root, index, metadata, catalog, registry, database: database.to_string(), subject: subject.to_string(), checkpoints, partials })
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
	#[must_use]
	pub const fn index(&self) -> &SegmentIndexStore {
		&self.index
	}

	/// The aspect-schema catalog backing this store.
	#[must_use]
	pub const fn catalog(&self) -> &AspectCatalog {
		&self.catalog
	}

	/// The per-aspect segment-set rollup store backing this store, for the materialized
	/// aspect-wide summary ([`get`](AspectMetadataStore::get),
	/// [`list_aspects`](AspectMetadataStore::list_aspects)).
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
	/// The `.dspseg` measurement frames under `segments/` are **not** part of this backup
	/// — this is the control-plane (catalog/index/metadata) snapshot only, per the storage
	/// boundary (hard-constraint #3). `dest_dir` is created if absent; each destination
	/// file must not already exist (`VACUUM INTO` needs a fresh file), so back up into a
	/// fresh (e.g. timestamped) directory.
	///
	/// See [`snapshot_and_verify`](crate::snapshot_and_verify) for the consistency scope
	/// of the per-file verification (the row-count match assumes a quiescent source).
	///
	/// # Errors
	///
	/// Propagates a filesystem error creating `dest_dir`, or any per-database backup/verify
	/// failure (bad/existing destination, libSQL error, or a source/copy mismatch).
	pub async fn backup_control_plane(&self, dest_dir: impl AsRef<Path>) -> Result<ControlPlaneBackup> {
		let dir = dest_dir.as_ref().to_path_buf();
		tokio::fs::create_dir_all(&dir).await.with_context(|| format!("creating backup dir {}", dir.display()))?;
		let segment_index = self.index.backup_to(&dir.join("segment_index.db")).await.context("backing up segment_index.db")?;
		let metadata = self.metadata.backup_to(&dir.join("metadata.db")).await.context("backing up metadata.db")?;
		let aspect_catalog = self.catalog.backup_to(&dir.join("aspect_catalog.db")).await.context("backing up aspect_catalog.db")?;
		let registry = self.registry.backup_to(&dir.join("catalog.db")).await.context("backing up catalog.db")?;
		Ok(ControlPlaneBackup { dir, segment_index, metadata, aspect_catalog, registry })
	}

	/// Declare `aspect`'s [`AspectSchema`] in this store's catalog, so later
	/// [`seal_declared`](SegmentStore::seal_declared) calls need not be handed the
	/// schema. Idempotent on the aspect (a re-declaration overwrites).
	///
	/// # Errors
	///
	/// Propagates any libSQL write failure.
	pub async fn declare(&self, aspect: &str, schema: &AspectSchema) -> Result<()> {
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
		let schema = self.require_schema(aspect).await?;
		self.seal_paged(aspect, &schema, timestamps, values, rows_per_page).await
	}

	/// Seal a dense `(timestamp, value)` batch into a `.dspseg` file under `aspect`'s
	/// declared `schema` and record it in the index, returning the descriptor.
	///
	/// The batch is encoded under exactly the schema's declared
	/// [`PhysicalType`](dsp_physical_type::PhysicalType): an unrepresentable value or
	/// one whose error exceeds the schema tolerance fails the seal rather than
	/// downcasting silently (hard constraint #4). The segment claims the next
	/// monotonic id for the aspect.
	///
	/// # Errors
	///
	/// Propagates a [`dsp_physical_type::SealError`] (length mismatch, unrepresentable
	/// value, tolerance exceeded), a filesystem write error, or a libSQL index
	/// failure.
	pub async fn seal(&self, aspect: &str, schema: &AspectSchema, timestamps: &[i64], values: &[BigDecimal]) -> Result<SegmentDescriptor> {
		let segment = schema.seal(timestamps, values).map_err(|e| anyhow::anyhow!("seal failed: {e}"))?;
		self.persist(aspect, &segment).await
	}

	/// Seal a **nullable** batch (a dense timestamp column and a `&[Option<BigDecimal>]`
	/// value column) into a `.dspseg` file and record it — the quality-column seal.
	///
	/// Present values are encoded densely under the declared encoding; `None` rows
	/// become cleared bits in the segment's quality mask. Enforcement mirrors
	/// [`seal`](SegmentStore::seal).
	///
	/// # Errors
	///
	/// As [`seal`](SegmentStore::seal).
	pub async fn seal_nullable(&self, aspect: &str, schema: &AspectSchema, timestamps: &[i64], values: &[Option<BigDecimal>]) -> Result<SegmentDescriptor> {
		let segment = schema.seal_nullable(timestamps, values).map_err(|e| anyhow::anyhow!("seal failed: {e}"))?;
		self.persist(aspect, &segment).await
	}

	/// Seal a dense batch into a **paged** `.dspseg` segment (intra-segment page
	/// subdivision) and record it. Rows are partitioned into pages of `rows_per_page`,
	/// each independently encoded with its own min/max stats, so a later
	/// [`read_time_range`](SegmentStore::read_time_range) skips pages *within* the
	/// file, not just whole files.
	///
	/// # Errors
	///
	/// As [`seal`](SegmentStore::seal), plus a [`dsp_physical_type::SealError::EmptyPageSize`]
	/// if `rows_per_page` is zero.
	pub async fn seal_paged(&self, aspect: &str, schema: &AspectSchema, timestamps: &[i64], values: &[BigDecimal], rows_per_page: usize) -> Result<SegmentDescriptor> {
		let segment = schema.seal_paged(timestamps, values, rows_per_page).map_err(|e| anyhow::anyhow!("paged seal failed: {e}"))?;
		self.persist_paged(aspect, &segment).await
	}

	/// Seal a **nullable** batch into a paged `.dspseg` segment and record it — the
	/// quality-column paged seal.
	///
	/// # Errors
	///
	/// As [`seal_paged`](SegmentStore::seal_paged).
	pub async fn seal_paged_nullable(&self, aspect: &str, schema: &AspectSchema, timestamps: &[i64], values: &[Option<BigDecimal>], rows_per_page: usize) -> Result<SegmentDescriptor> {
		let segment = schema.seal_paged_nullable(timestamps, values, rows_per_page).map_err(|e| anyhow::anyhow!("paged seal failed: {e}"))?;
		self.persist_paged(aspect, &segment).await
	}

	/// Write a freshly sealed segment to disk and record its descriptor. Shared by
	/// the dense and nullable seal paths.
	async fn persist(&self, aspect: &str, segment: &Segment) -> Result<SegmentDescriptor> {
		let id = self.index.next_id(aspect).await?;
		// Checkpoint the timestamp column only when configured AND the segment's shape
		// actually benefits (sorted + irregular + big enough) — otherwise write the
		// historical frame byte-for-byte. Either frame reads identically.
		let bytes = match self.checkpoints.stride_for(segment.stats.row_count, segment.benefits_from_checkpoints(), segment.checkpoint_codec_overhead()) {
			Some(stride) => segment.write_to_checkpointed(stride),
			None => segment.write_to(),
		};
		let path = self.segment_path(aspect, id);
		tokio::fs::write(&path, &bytes).await.with_context(|| format!("writing segment {}", path.display()))?;
		let descriptor = SegmentDescriptor::of_segment(id, path.to_string_lossy().into_owned(), bytes.len() as u64, segment);
		self.index.insert(aspect, &descriptor).await?;
		self.metadata.record_seal(aspect, &descriptor).await?;
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
	/// record its descriptor. The paged analogue of [`persist`](SegmentStore::persist).
	async fn persist_paged(&self, aspect: &str, segment: &PagedSegment) -> Result<SegmentDescriptor> {
		let id = self.index.next_id(aspect).await?;
		// As `persist`, though the paged win is far smaller — page pruning already bounds
		// a probe's decode to `rows_per_page`.
		let bytes = match self.checkpoints.stride_for(segment.stats.row_count, segment.benefits_from_checkpoints(), segment.checkpoint_codec_overhead()) {
			Some(stride) => segment.write_to_checkpointed(stride),
			None => segment.write_to(),
		};
		let path = self.segment_path(aspect, id);
		tokio::fs::write(&path, &bytes).await.with_context(|| format!("writing segment {}", path.display()))?;
		let descriptor = SegmentDescriptor::of_paged_segment(id, path.to_string_lossy().into_owned(), bytes.len() as u64, segment);
		self.index.insert(aspect, &descriptor).await?;
		self.metadata.record_seal(aspect, &descriptor).await?;
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
	fn segment_path(&self, aspect: &str, id: u64) -> PathBuf {
		self.root.join("segments").join(format!("{aspect}-{id}.dspseg"))
	}

	/// The on-disk path of the partial-reduction sidecar beside an `aspect`/`id` segment
	/// (`{aspect}-{id}.dspart`, alongside the `.dspseg`).
	fn sidecar_path(&self, aspect: &str, id: u64) -> PathBuf {
		self.root.join("segments").join(format!("{aspect}-{id}.dspart"))
	}

	/// Materialize a per-segment partial-reduction sidecar for a just-sealed segment, when
	/// the [`PartialSidecarPolicy`] calls for one. Decodes the segment's rows, folds the
	/// present ones into a [`PartialReduction`] at `base` over [`SIDECAR_AGGREGATIONS`], and
	/// writes the `.dspart` frame beside the `.dspseg`. A no-op (returns `Ok(())`) when the
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
		let partial = dsp_reduce::reduce_partial(&points, base, None, None, &SIDECAR_AGGREGATIONS).map_err(|e| anyhow::anyhow!("reducing segment {} for its partial sidecar: {e}", descriptor.path))?;
		// Materialize the coarser rollup tiers the policy declares (each re-keyed from the
		// tier below), so a coarse downsample folds a coarse tier rather than the whole base.
		let sidecar = PartialSidecar::materialize(base, descriptor, partial, &self.partials.tiers()).map_err(|e| anyhow::anyhow!("building rollup tiers for segment {}: {e}", descriptor.path))?;
		let bytes = sidecar.to_bytes()?;
		let path = self.sidecar_path(aspect, descriptor.id);
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
		let path = self.sidecar_path(aspect, descriptor.id);
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
	/// `.dspart` is left behind.
	///
	/// # Errors
	///
	/// Propagates a filesystem error other than "not found".
	async fn remove_sidecar(&self, aspect: &str, id: u64) -> Result<()> {
		let path = self.sidecar_path(aspect, id);
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
	/// disjoint segment); only the surviving descriptors' `.dspseg` files are opened,
	/// decoded, and filtered to the window. Returns parallel `(timestamps, values)`
	/// vectors with `None` at every null row, in segment-seal then in-segment order.
	///
	/// # Errors
	///
	/// Propagates a libSQL prune failure, a filesystem read error, or a
	/// [`dsp_physical_type::dspseg::DspSegError`] for a corrupt/unreadable `.dspseg`.
	pub async fn read_time_range(&self, aspect: &str, start: i64, end: i64) -> Result<(Vec<i64>, Vec<Option<BigDecimal>>)> {
		let descriptors = self.index.prune_by_time(aspect, start, end).await?;
		let mut timestamps = Vec::new();
		let mut values = Vec::new();
		for descriptor in &descriptors {
			let bytes = tokio::fs::read(&descriptor.path).await.with_context(|| format!("reading segment {}", descriptor.path))?;
			// A paged frame (v3) decodes through PagedSegment::read_time_range, which
			// skips pages *within* the file; a single-block frame decodes whole.
			// Windowed read: a regular block-coded frame decodes only the row window (closed-form
			// index range + per-present-row value read) — the paged variant additionally skips whole
			// pages disjoint from the window — instead of the whole segment; any other shape falls
			// back to a full decode. Already filtered to `[start, end]`.
			let (ts, vs) = if descriptor.format_version == PAGED_SEGMENT_FORMAT_VERSION {
				dsp_physical_type::dspseg::read_paged_segment_range(&bytes, start, end).map_err(|e| anyhow::anyhow!("decoding paged segment {}: {e}", descriptor.path))?
			} else {
				dsp_physical_type::dspseg::read_segment_range(&bytes, start, end).map_err(|e| anyhow::anyhow!("decoding segment {}: {e}", descriptor.path))?
			};
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
	/// The distributed shape of [`dsp_reduce::reduce`]: the index is pruned by time, then
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
			let bytes = tokio::fs::read(&descriptor.path).await.with_context(|| format!("reading segment {}", descriptor.path))?;
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
				let bytes = tokio::fs::read(&descriptor.path).await.with_context(|| format!("reading segment {}", descriptor.path))?;
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
	/// so only the `.dspseg` files whose `[min_ts, max_ts]` spans `t` are opened. Each
	/// opened segment resolves the instant with its own persisted per-segment order
	/// signal: a `time_sorted` segment binary-searches the timestamp column, an
	/// out-of-order one linear-scans it (see
	/// [`Segment::value_at`](dsp_physical_type::Segment::value_at)). Candidates are
	/// pruned in seal-id order, so when overlapping segments each carry a present
	/// value at `t` the most recently sealed one wins (last-writer-wins) — the natural
	/// read-your-writes answer once out-of-order reconciliation (Phase 4.6) can leave
	/// two segments spanning one instant.
	///
	/// # Errors
	///
	/// Propagates a libSQL prune failure, a filesystem read error, or a
	/// [`dsp_physical_type::dspseg::DspSegError`] for a corrupt/unreadable `.dspseg`.
	pub async fn read_point(&self, aspect: &str, t: i64) -> Result<Option<BigDecimal>> {
		let descriptors = self.index.prune_by_time(aspect, t, t).await?;
		let mut found = None;
		for descriptor in &descriptors {
			let bytes = tokio::fs::read(&descriptor.path).await.with_context(|| format!("reading segment {}", descriptor.path))?;
			// Streaming single-value read: prunes/skips the pages and value-column blocks a point
			// lookup does not touch, unpacking only the one block covering `t` on a per-block codec
			// (roadmap Phase 4/6). Equal to `…read_from(&bytes)?.value_at(t)` for every frame.
			let hit = if descriptor.format_version == PAGED_SEGMENT_FORMAT_VERSION {
				dsp_physical_type::dspseg::read_paged_segment_point(&bytes, t).map_err(|e| anyhow::anyhow!("decoding paged segment {}: {e}", descriptor.path))?
			} else {
				dsp_physical_type::dspseg::read_segment_point(&bytes, t).map_err(|e| anyhow::anyhow!("decoding segment {}: {e}", descriptor.path))?
			};
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
	/// streaming [`read_segment_points`](dsp_physical_type::dspseg::read_segment_points) /
	/// [`read_paged_segment_points`](dsp_physical_type::dspseg::read_paged_segment_points)), so
	/// looking up `N` instants that share segments pays one file read + one timestamp decode per
	/// segment rather than `N`. As with `read_point`, candidates are merged in seal-id order so
	/// the most recently sealed present value wins per instant (last-writer-wins). An empty `ts`
	/// yields an empty vector without touching the index.
	///
	/// # Errors
	///
	/// Propagates a libSQL prune failure, a filesystem read error, or a
	/// [`dsp_physical_type::dspseg::DspSegError`] for a corrupt/unreadable `.dspseg`.
	pub async fn read_points(&self, aspect: &str, ts: &[i64]) -> Result<Vec<Option<BigDecimal>>> {
		if ts.is_empty() {
			return Ok(Vec::new());
		}
		// Prune the index once by the batch's whole span (safe: every instant lies within it).
		let (lo, hi) = ts.iter().fold((i64::MAX, i64::MIN), |(lo, hi), &t| (lo.min(t), hi.max(t)));
		let descriptors = self.index.prune_by_time(aspect, lo, hi).await?;
		let mut found = vec![None; ts.len()];
		for descriptor in &descriptors {
			let bytes = tokio::fs::read(&descriptor.path).await.with_context(|| format!("reading segment {}", descriptor.path))?;
			let hits = if descriptor.format_version == PAGED_SEGMENT_FORMAT_VERSION {
				dsp_physical_type::dspseg::read_paged_segment_points(&bytes, ts).map_err(|e| anyhow::anyhow!("decoding paged segment {}: {e}", descriptor.path))?
			} else {
				dsp_physical_type::dspseg::read_segment_points(&bytes, ts).map_err(|e| anyhow::anyhow!("decoding segment {}: {e}", descriptor.path))?
			};
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
	pub async fn reconcile_segment(&self, aspect: &str, id: u64) -> Result<bool> {
		let descriptor = self.index.all(aspect).await?.into_iter().find(|d| d.id == id).ok_or_else(|| anyhow::anyhow!("aspect {aspect:?} has no segment {id}"))?;
		if descriptor.time_sorted {
			return Ok(false);
		}
		let schema = self.require_schema(aspect).await?;
		let bytes = tokio::fs::read(&descriptor.path).await.with_context(|| format!("reading segment {}", descriptor.path))?;
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
		// Re-seal the sorted rows in the original frame kind, to the same id/file.
		let path = self.segment_path(aspect, id);
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
		self.index.insert(aspect, &new_descriptor).await?;
		// The rewrite changed the segment's bytes, staling any sidecar — regenerate it from
		// the reconciled rows (or drop it if the policy no longer wants one).
		self.refresh_sidecar_after_rewrite(aspect, &new_descriptor, sorted_ts, sorted_vs).await;
		// A reconcile replaces a segment rather than adding one, so the O(1) fold would
		// double-count — recompute the rollup from the durable index instead.
		self.rebuild_aspect_metadata(aspect).await?;
		Ok(true)
	}

	/// Re-seal a nullable `(timestamps, values)` batch into the `.dspseg` file for
	/// `aspect`/`id`, in the frame kind selected by `rows_per_page` (`Some` → paged,
	/// `None` → single-block), recording the new descriptor in the control-plane index.
	///
	/// The write-half shared by the split path: [`reconcile_segment`] inlines the same
	/// logic against a single id, this one targets an arbitrary id so a split can write
	/// its prefix and suffix through one code path. It does **not** touch the
	/// materialized rollup — a caller that changes the segment set rebuilds it once at
	/// the end.
	async fn reseal_nullable_at(&self, aspect: &str, schema: &AspectSchema, id: u64, timestamps: &[i64], values: &[Option<BigDecimal>], rows_per_page: Option<usize>) -> Result<SegmentDescriptor> {
		let path = self.segment_path(aspect, id);
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
		self.index.insert(aspect, &descriptor).await?;
		// Keep the sidecar consistent with the freshly written bytes at this id.
		self.refresh_sidecar_after_rewrite(aspect, &descriptor, timestamps.to_vec(), values.to_vec()).await;
		Ok(descriptor)
	}

	/// **Split a sorted segment at a timestamp boundary** (roadmap Phase 4.6 — the
	/// physical mechanism of the split-not-rewrite reconciliation path).
	///
	/// Partitions segment `id` of `aspect` at `boundary` into a **prefix** (rows with
	/// timestamp strictly `< boundary`, kept at the original `id`) and a **suffix**
	/// (rows at or after `boundary`, moved to a freshly-allocated segment id), following
	/// the [`split_index`](dsp_physical_type::split_index) partition point. Because the
	/// input is time-sorted, the prefix's every timestamp is `< boundary ≤` the suffix's
	/// every timestamp, so the two results are internally sorted **and disjoint in time**
	/// — the split adds no cross-segment overlap, and a point/range read still opens
	/// exactly one of them for any instant. Both keep the input's frame kind (a paged
	/// segment splits into two paged segments at its own page height).
	///
	/// This is the primitive `QuestDB`'s partition split is built on: once a large cold
	/// prefix is carved into its own segment, later late-data merges touch only the hot
	/// suffix and never rewrite the cold prefix again, bounding write amplification over
	/// the segment's lifetime. Wiring [`SplitPolicy::decide`](dsp_physical_type::SplitPolicy)
	/// into [`reconcile_overlaps`](SegmentStore::reconcile_overlaps) to *choose* a split
	/// over a full rewrite is the next slice; this slice ships the mechanism it calls.
	///
	/// The suffix segment is written **before** the prefix is rewritten, so a crash
	/// mid-split can at worst leave the suffix rows duplicated in the not-yet-shrunk
	/// prefix (a cross-segment overlap [`reconcile_overlaps`] repairs), never lost.
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
	pub async fn split_segment(&self, aspect: &str, id: u64, boundary: i64) -> Result<Option<u64>> {
		let descriptor = self.index.all(aspect).await?.into_iter().find(|d| d.id == id).ok_or_else(|| anyhow::anyhow!("aspect {aspect:?} has no segment {id}"))?;
		if !descriptor.time_sorted {
			return Err(anyhow::anyhow!("segment {id} of aspect {aspect:?} is out of order; reconcile it before splitting"));
		}
		let schema = self.require_schema(aspect).await?;
		// Read once, capturing the paged page height so each half re-seals in kind.
		let bytes = tokio::fs::read(&descriptor.path).await.with_context(|| format!("reading segment {}", descriptor.path))?;
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
		// two duplicates rows rather than dropping them.
		let suffix_id = self.index.next_id(aspect).await?;
		self.reseal_nullable_at(aspect, &schema, suffix_id, suffix_ts, suffix_vs, rows_per_page).await?;
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
		let unsorted_ids: Vec<u64> = self.index.all(aspect).await?.into_iter().filter(|d| !d.time_sorted).map(|d| d.id).collect();
		let mut reconciled = 0;
		for id in unsorted_ids {
			if self.reconcile_segment(aspect, id).await? {
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
	/// `unsorted_segments` order-health count is DSP's analogue: below the threshold the
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
		let threshold = threshold.max(1);
		let unsorted = self.index.load_index(aspect).await?.unsorted_count();
		if unsorted < threshold {
			return Ok(None);
		}
		Ok(Some(self.reconcile_aspect(aspect).await?))
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
	/// # Errors
	///
	/// As [`reconcile_aspect_if_unsorted_exceeds`](SegmentStore::reconcile_aspect_if_unsorted_exceeds);
	/// also propagates the aspect-list read.
	pub async fn reconcile_all_over_threshold(&self, threshold: usize) -> Result<ReconcileSweep> {
		let aspects = self.list_declared_aspects().await?;
		let mut sweep = ReconcileSweep { aspects_scanned: aspects.len(), ..ReconcileSweep::default() };
		for aspect in &aspects {
			if let Some(reconciled) = self.reconcile_aspect_if_unsorted_exceeds(aspect, threshold).await? {
				sweep.aspects_reconciled += 1;
				sweep.segments_reconciled += reconciled;
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
		let threshold = threshold.max(1);
		let descriptors = self.index.all(aspect).await?;
		let hot_tail_id = descriptors.iter().map(|d| d.id).max();
		let unsorted: Vec<u64> = descriptors.iter().filter(|d| !d.time_sorted).map(|d| d.id).collect();
		let hot_fires = unsorted.len() >= threshold;
		let mut out = HotColdReconcile::default();
		for id in unsorted {
			if Some(id) == hot_tail_id {
				// The hot tail is deferred until the backlog reaches the threshold.
				if hot_fires && self.reconcile_segment(aspect, id).await? {
					out.hot_reconciled += 1;
				}
			} else if self.reconcile_segment(aspect, id).await? {
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
	/// # Errors
	///
	/// As [`reconcile_aspect_hot_cold`](SegmentStore::reconcile_aspect_hot_cold); also
	/// propagates the aspect-list read.
	pub async fn reconcile_all_hot_cold(&self, threshold: usize) -> Result<HotColdSweep> {
		let aspects = self.list_declared_aspects().await?;
		let mut sweep = HotColdSweep { aspects_scanned: aspects.len(), ..HotColdSweep::default() };
		for aspect in &aspects {
			let outcome = self.reconcile_aspect_hot_cold(aspect, threshold).await?;
			if outcome.total() > 0 {
				sweep.aspects_reconciled += 1;
				sweep.cold_reconciled += outcome.cold_reconciled;
				sweep.hot_reconciled += outcome.hot_reconciled;
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
	/// so afterwards [`SegmentIndex::overlapping_count`](dsp_physical_type::SegmentIndex::overlapping_count)
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
	/// disjoint in time (prefix < boundary ≤ suffix), so [`overlapping_count`](dsp_physical_type::SegmentIndex::overlapping_count)
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
				},
				_ => {
					components.push(vec![id]);
					running_max_hi = hi;
				},
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
				// id; the two are disjoint so no cross-segment overlap remains. Write the
				// suffix first so a crash duplicates rather than drops rows.
				let suffix_id = self.index.next_id(aspect).await?;
				self.reseal_nullable_at(aspect, &schema, suffix_id, &all_ts[prefix_len..], &all_vs[prefix_len..], None).await?;
				self.reseal_nullable_at(aspect, &schema, target, &all_ts[..prefix_len], &all_vs[..prefix_len], None).await?;
			} else {
				// Full rewrite: the whole merged component into the lowest id.
				self.reseal_nullable_at(aspect, &schema, target, &all_ts, &all_vs, None).await?;
			}
			// Drop the other members: control-plane row then the file (and its sidecar).
			for &id in component.iter().skip(1) {
				self.index.delete(aspect, id).await?;
				let victim = self.segment_path(aspect, id);
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
	/// # Errors
	///
	/// As [`reconcile_overlaps`](SegmentStore::reconcile_overlaps); also propagates the
	/// aspect-list read.
	pub async fn reconcile_all_overlaps(&self) -> Result<OverlapSweep> {
		let aspects = self.list_declared_aspects().await?;
		let mut sweep = OverlapSweep { aspects_scanned: aspects.len(), ..OverlapSweep::default() };
		for aspect in &aspects {
			let removed = self.reconcile_overlaps(aspect).await?;
			if removed > 0 {
				sweep.aspects_reconciled += 1;
				sweep.segments_removed += removed;
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
	/// # Errors
	///
	/// As [`reconcile_all_overlaps`](SegmentStore::reconcile_all_overlaps).
	pub async fn reconcile_all_overlaps_with_policy(&self, policy: SplitPolicy) -> Result<OverlapSweep> {
		let aspects = self.list_declared_aspects().await?;
		let mut sweep = OverlapSweep { aspects_scanned: aspects.len(), ..OverlapSweep::default() };
		for aspect in &aspects {
			let had_overlap = self.index.load_index(aspect).await?.overlapping_count() > 0;
			let removed = self.reconcile_overlaps_with_policy(aspect, policy).await?;
			if had_overlap {
				sweep.aspects_reconciled += 1;
				sweep.segments_removed += removed;
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
	pub async fn squash_aspect(&self, aspect: &str) -> Result<usize> {
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
			self.index.delete(aspect, id).await?;
			let victim = self.segment_path(aspect, id);
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
		let max_segments = max_segments.max(1);
		if self.index.count(aspect).await? <= max_segments {
			return Ok(None);
		}
		Ok(Some(self.squash_aspect(aspect).await?))
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
	/// # Errors
	///
	/// As [`squash_aspect_if_exceeds`](SegmentStore::squash_aspect_if_exceeds); also
	/// propagates the aspect-list read.
	pub async fn squash_all_over_threshold(&self, max_segments: usize) -> Result<SquashSweep> {
		let aspects = self.list_declared_aspects().await?;
		let mut sweep = SquashSweep { aspects_scanned: aspects.len(), ..SquashSweep::default() };
		for aspect in &aspects {
			if let Some(removed) = self.squash_aspect_if_exceeds(aspect, max_segments).await? {
				sweep.aspects_squashed += 1;
				sweep.segments_removed += removed;
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
				self.index.delete(aspect, id).await?;
				let victim = self.segment_path(aspect, id);
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
	/// # Errors
	///
	/// As [`squash_aspect_to_target_rows`](SegmentStore::squash_aspect_to_target_rows); also
	/// propagates the aspect-list read.
	pub async fn squash_all_to_target_rows(&self, target_rows: usize) -> Result<SquashSweep> {
		let aspects = self.list_declared_aspects().await?;
		let mut sweep = SquashSweep { aspects_scanned: aspects.len(), ..SquashSweep::default() };
		for aspect in &aspects {
			let removed = self.squash_aspect_to_target_rows(aspect, target_rows).await?;
			if removed > 0 {
				sweep.aspects_squashed += 1;
				sweep.segments_removed += removed;
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
		let target_rows = target_rows.max(1);
		let Some(meta) = self.metadata.get(aspect).await? else { return Ok(None) };
		// Minimum segments to hold total_rows at the target; a fully-compacted aspect sits at
		// exactly this count (or below), so more than this means there is fragmentation to fold.
		let ideal = meta.total_rows.div_ceil(target_rows as u64).max(1);
		if (meta.segment_count as u64) <= ideal {
			return Ok(None);
		}
		Ok(Some(self.squash_aspect_to_target_rows(aspect, target_rows).await?))
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
	/// # Errors
	///
	/// As [`squash_aspect_to_target_rows_if_fragmented`](SegmentStore::squash_aspect_to_target_rows_if_fragmented);
	/// also propagates the aspect-list read.
	pub async fn squash_all_to_target_rows_if_fragmented(&self, target_rows: usize) -> Result<SquashSweep> {
		let aspects = self.list_declared_aspects().await?;
		let mut sweep = SquashSweep { aspects_scanned: aspects.len(), ..SquashSweep::default() };
		for aspect in &aspects {
			if let Some(removed) = self.squash_aspect_to_target_rows_if_fragmented(aspect, target_rows).await? {
				if removed > 0 {
					sweep.aspects_squashed += 1;
					sweep.segments_removed += removed;
				}
			}
		}
		Ok(sweep)
	}

	/// Read every present row of `aspect` whose **value** falls in the inclusive range
	/// `[lo, hi]`, **opening only the segment files whose value span overlaps it**.
	///
	/// The value column has no SQL ordering (the `BigDecimal` bounds are stored as
	/// text), so the pruning runs through the resident
	/// [`SegmentIndex`](dsp_physical_type::SegmentIndex): it is loaded from the
	/// control plane and pruned by value, and only the surviving descriptors' files
	/// are opened. Within each opened segment the rows are filtered to those whose
	/// value is present and in `[lo, hi]`. Returns parallel `(timestamps, values)`
	/// vectors, in segment-seal then in-segment order.
	///
	/// # Errors
	///
	/// Propagates a libSQL read failure, a filesystem read error, or a
	/// [`dsp_physical_type::dspseg::DspSegError`] for a corrupt/unreadable `.dspseg`.
	pub async fn read_value_range(&self, aspect: &str, lo: &BigDecimal, hi: &BigDecimal) -> Result<(Vec<i64>, Vec<BigDecimal>)> {
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
		let bytes = tokio::fs::read(&descriptor.path).await.with_context(|| format!("reading segment {}", descriptor.path))?;
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
	/// Computed from the resident [`SegmentIndex`](dsp_physical_type::SegmentIndex)
	/// (the descriptors' recorded framed byte lengths and row counts), so it reflects
	/// the realized `.dspseg` files including their header/index/checksum overhead,
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
	/// # Errors
	///
	/// Propagates any libSQL read or write failure.
	pub async fn rebuild_aspect_metadata(&self, aspect: &str) -> Result<AspectMetadata> {
		let index = self.index.load_index(aspect).await?;
		let meta = AspectMetadata::from_index(&index);
		self.metadata.put(aspect, &meta).await?;
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
	/// **scans each aspect's segment index** ([`SegmentIndex::overlapping_count`](dsp_physical_type::SegmentIndex::overlapping_count))
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileSweep {
	/// Number of declared aspects the sweep visited.
	pub aspects_scanned: usize,
	/// Number of aspects whose backlog was at or above the threshold and were
	/// therefore reconciled this sweep.
	pub aspects_reconciled: usize,
	/// Total out-of-order segments rewritten sorted across every reconciled aspect.
	pub segments_reconciled: usize,
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HotColdSweep {
	/// Number of declared aspects the sweep visited.
	pub aspects_scanned: usize,
	/// Number of aspects that rewrote at least one segment (cold or hot) this sweep.
	pub aspects_reconciled: usize,
	/// Total cold segments rewritten across every aspect.
	pub cold_reconciled: usize,
	/// Total hot-tail segments rewritten across every aspect.
	pub hot_reconciled: usize,
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OverlapSweep {
	/// Number of declared aspects the sweep visited.
	pub aspects_scanned: usize,
	/// Number of aspects that merged at least one overlap group this sweep.
	pub aspects_reconciled: usize,
	/// Total segments removed by merging across every aspect (sum of per-component
	/// `members − 1`).
	pub segments_removed: usize,
}

/// The outcome of a store-wide squash sweep, returned by
/// [`SegmentStore::squash_all_over_threshold`] — the per-tick numbers a background
/// squash daemon logs and exports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SquashSweep {
	/// Number of declared aspects the sweep visited.
	pub aspects_scanned: usize,
	/// Number of aspects that were squashed this sweep (their segment count exceeded the
	/// threshold).
	pub aspects_squashed: usize,
	/// Total segments removed by squashing across every aspect (sum of per-aspect
	/// `count − 1`).
	pub segments_removed: usize,
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
	/// Total realized on-disk bytes across every aspect's `.dspseg` frames.
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
	/// Total realized on-disk bytes across every `.dspseg` frame.
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
	/// construction. See [`SegmentIndex::unsorted_count`](dsp_physical_type::SegmentIndex::unsorted_count).
	pub unsorted_segments: usize,
	/// The number of sealed segments whose time span **overlaps at least one other
	/// segment's** — the *cross-segment* order-health signal (roadmap Phase 4.6),
	/// distinct from [`unsorted_segments`](AspectStorageStats::unsorted_segments)
	/// (which counts *intra*-segment disorder). A non-zero count means late data
	/// re-entered an already-covered window, so a point lookup may have to consult
	/// more than one segment; these are the cross-segment reconciliation candidates.
	/// See [`SegmentIndex::overlapping_count`](dsp_physical_type::SegmentIndex::overlapping_count).
	pub overlapping_segments: usize,
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use dsp_physical_type::{timestamp::TimeUnit, PhysicalType};
	use tempfile::TempDir;

	use super::*;

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

	#[tokio::test]
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
		use dsp_reduce::{reduce, Aggregation};
		use splimes::{Point, Resolution};

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
		let prefix_bytes = std::fs::read(dir.path().join("segments").join(format!("a-{}.dspseg", d.id))).expect("prefix file");
		let suffix_bytes = std::fs::read(dir.path().join("segments").join(format!("a-{suffix_id}.dspseg"))).expect("suffix file");
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
		let sweep = store.reconcile_all_over_threshold(2).await.expect("sweeps");
		let a_after = store.aspect_stats("a").await.expect("stats a").unsorted_segments;
		let b_after = store.aspect_stats("b").await.expect("stats b").unsorted_segments;
		// A second sweep at threshold 1 now clears b too.
		let sweep2 = store.reconcile_all_over_threshold(1).await.expect("sweeps again");
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
		let sweep = store.reconcile_all_hot_cold(2).await.expect("sweeps");
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
		let cold_path = dir.path().join("segments").join("a-0.dspseg");
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
		let sweep = store.reconcile_all_overlaps().await.expect("sweeps");
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
		let sweep = store.reconcile_all_overlaps_with_policy(SplitPolicy::new(1)).await.expect("sweeps");
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
		let sweep = store.squash_all_over_threshold(2).await.expect("sweeps");
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
		let sweep = store.squash_all_to_target_rows(4).await.expect("sweeps");
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
		let sweep = store.squash_all_to_target_rows_if_fragmented(4).await.expect("gated sweep");
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
		// Two scoped stores over one root populate the shared catalog.db hierarchy.
		let market = SegmentStore::open_scoped(dir.path(), "market", "BTCUSD").await.expect("opens");
		let _iot = SegmentStore::open_scoped(dir.path(), "iot", "sensor-7").await.expect("opens");
		// Re-opening the same scope is idempotent — no duplicate rows.
		let _again = SegmentStore::open_scoped(dir.path(), "market", "BTCUSD").await.expect("opens");
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
		// A sibling subject's declaration is not listed here.
		let sibling = SegmentStore::open_scoped(dir.path(), "d", "other").await.expect("opens");
		sibling.declare("pressure", &schema()).await.expect("declares");
		let aspects = store.list_declared_aspects().await.expect("lists");
		drop(store);
		drop(sibling);
		assert_eq!(aspects, vec!["humidity".to_string(), "temp".to_string()]);
	}

	#[tokio::test]
	async fn scopes_isolate_declarations() {
		let dir = TempDir::new().expect("tempdir");
		// Two stores over the same root but different subjects share the catalog DB;
		// a declaration under one subject is invisible to the other.
		let a = SegmentStore::open_scoped(dir.path(), "d", "subject-a").await.expect("opens");
		a.declare("temp", &schema()).await.expect("declares");
		let b = SegmentStore::open_scoped(dir.path(), "d", "subject-b").await.expect("opens");
		let seen_by_b = b.schema_for("temp").await.expect("looks up");
		let seen_by_a = a.schema_for("temp").await.expect("looks up");
		drop(a);
		drop(b);
		assert_eq!(seen_by_a, Some(schema()));
		assert_eq!(seen_by_b, None, "a sibling subject does not see the declaration");
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

	/// End-to-end through the real store: an enabled partial-sidecar policy writes a
	/// `.dspart` beside each sealed `.dspseg`, the sidecar matches the exact segment bytes,
	/// and its stored partial finishes to the same buckets a fresh reduction of the
	/// segment's rows would — the invariant the cross-segment downsample will lean on. An
	/// unconfigured store writes no sidecar.
	#[tokio::test]
	async fn seal_writes_a_matching_partial_sidecar_when_configured() {
		use dsp_reduce::reduce_partial;
		use splimes::{Point, Resolution};

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
		use dsp_reduce::{reduce, Aggregation};
		use splimes::{Point, Resolution};

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
	/// stored partials instead of decoding — proven by **deleting every `.dspseg`** and
	/// showing the answer is unchanged (the value column was never read). Fallbacks stay
	/// correct: a non-materializable reduction (exact `p99`), a resolution other than the
	/// sidecar base, and a window that cuts inside a segment all decode, so they break once
	/// the frames are gone.
	#[tokio::test]
	async fn downsample_range_serves_materializable_queries_from_sidecars() {
		use dsp_reduce::{reduce, Aggregation};
		use splimes::{Point, Resolution};

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

		// Delete every `.dspseg`: the index (libSQL) still prunes, and a sidecar-served
		// downsample never opens a frame, so the answer must be unchanged.
		for path in &seg_paths {
			std::fs::remove_file(path).expect("removes the .dspseg frame");
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
	/// every `.dspseg` and showing the coarse answer is unchanged. A resolution FINER than
	/// the base (seconds) cannot be served and must decode.
	#[tokio::test]
	async fn downsample_range_rebuckets_a_fine_base_to_a_coarser_resolution() {
		use dsp_reduce::{reduce, Aggregation};
		use splimes::{Point, Resolution};

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
		use dsp_reduce::{reduce, Aggregation};
		use splimes::{Point, Resolution};

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
}
