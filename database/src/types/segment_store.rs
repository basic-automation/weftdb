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

use crate::SegmentIndexStore;

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
}

impl SegmentStore {
	/// Open (creating if absent) a segment store rooted at `root`: ensures
	/// `root/segments/` exists and opens the `root/segment_index.db` control-plane
	/// index.
	///
	/// # Errors
	///
	/// Propagates a filesystem error creating the layout, or any libSQL failure
	/// opening the index.
	pub async fn open(root: impl AsRef<Path>) -> Result<Self> {
		let root = root.as_ref().to_path_buf();
		let segments_dir = root.join("segments");
		tokio::fs::create_dir_all(&segments_dir).await.with_context(|| format!("creating segments dir {}", segments_dir.display()))?;
		let index_path = root.join("segment_index.db");
		let index = SegmentIndexStore::open(&index_path.to_string_lossy()).await?;
		Ok(Self { root, index })
	}

	/// The control-plane index backing this store, for pruning/accounting queries
	/// ([`prune_by_time`](SegmentIndexStore::prune_by_time),
	/// [`load_index`](SegmentIndexStore::load_index), …).
	#[must_use]
	pub const fn index(&self) -> &SegmentIndexStore {
		&self.index
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

	/// Number of sealed segments recorded for `aspect`.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn segment_count(&self, aspect: &str) -> Result<usize> {
		self.index.count(aspect).await
	}
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
}
