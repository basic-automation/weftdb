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
use dsp_physical_type::{AspectSchema, PagedSegment, Segment, SegmentDescriptor, PAGED_SEGMENT_FORMAT_VERSION};

use crate::{AspectCatalog, CatalogStore, SegmentIndexStore};

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
pub struct SegmentStore {
	/// The store root; sealed frames live in `root/segments/`.
	root: PathBuf,
	/// The libSQL segment index (`root/segment_index.db`).
	index: SegmentIndexStore,
	/// The libSQL aspect-schema catalog (`root/aspect_catalog.db`) — the declared
	/// [`AspectSchema`] for each aspect, so a seal need not be handed the schema.
	catalog: AspectCatalog,
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
		let catalog_path = root.join("aspect_catalog.db");
		let catalog = AspectCatalog::open(&catalog_path.to_string_lossy()).await?;
		let registry_path = root.join("catalog.db");
		let registry = CatalogStore::open(&registry_path.to_string_lossy()).await?;
		// Record this store's place in the hierarchy so the control plane can enumerate
		// the databases/subjects a root holds (idempotent).
		registry.register_database(database).await?;
		registry.register_subject(database, subject).await?;
		Ok(Self { root, index, catalog, registry, database: database.to_string(), subject: subject.to_string() })
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

	/// The DB/subject registry backing this store, for enumerating the databases and
	/// subjects a root holds ([`list_databases`](CatalogStore::list_databases),
	/// [`list_subjects`](CatalogStore::list_subjects)).
	#[must_use]
	pub const fn registry(&self) -> &CatalogStore {
		&self.registry
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
		let bytes = segment.write_to();
		let path = self.segment_path(aspect, id);
		tokio::fs::write(&path, &bytes).await.with_context(|| format!("writing segment {}", path.display()))?;
		let descriptor = SegmentDescriptor::of_segment(id, path.to_string_lossy().into_owned(), bytes.len() as u64, segment);
		self.index.insert(aspect, &descriptor).await?;
		Ok(descriptor)
	}

	/// Write a freshly sealed **paged** segment to disk (format-version-3 frame) and
	/// record its descriptor. The paged analogue of [`persist`](SegmentStore::persist).
	async fn persist_paged(&self, aspect: &str, segment: &PagedSegment) -> Result<SegmentDescriptor> {
		let id = self.index.next_id(aspect).await?;
		let bytes = segment.write_to();
		let path = self.segment_path(aspect, id);
		tokio::fs::write(&path, &bytes).await.with_context(|| format!("writing segment {}", path.display()))?;
		let descriptor = SegmentDescriptor::of_paged_segment(id, path.to_string_lossy().into_owned(), bytes.len() as u64, segment);
		self.index.insert(aspect, &descriptor).await?;
		Ok(descriptor)
	}

	/// The on-disk path a freshly sealed segment of the given `aspect`/`id` takes.
	fn segment_path(&self, aspect: &str, id: u64) -> PathBuf {
		self.root.join("segments").join(format!("{aspect}-{id}.dspseg"))
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
			let (ts, vs) = if descriptor.format_version == PAGED_SEGMENT_FORMAT_VERSION {
				let segment = PagedSegment::read_from(&bytes).map_err(|e| anyhow::anyhow!("decoding paged segment {}: {e}", descriptor.path))?;
				segment.read_time_range(start, end)
			} else {
				let segment = Segment::read_from(&bytes).map_err(|e| anyhow::anyhow!("decoding segment {}: {e}", descriptor.path))?;
				segment.decode_nullable()
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
		Ok(AspectStorageStats { segment_count: index.len(), total_rows: index.total_rows(), total_bytes: index.total_bytes(), bytes_per_point: index.bytes_per_point(), time_range: index.time_range() })
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
		#[allow(clippy::cast_precision_loss)]
		let expected_bpp = expected_bytes as f64 / 10.0;
		assert!((stats.bytes_per_point - expected_bpp).abs() < f64::EPSILON);
		// Empty aspect.
		assert_eq!(empty.segment_count, 0);
		assert_eq!(empty.total_rows, 0);
		assert_eq!(empty.time_range, None);
		assert!((empty.bytes_per_point - 0.0).abs() < f64::EPSILON);
	}
}
