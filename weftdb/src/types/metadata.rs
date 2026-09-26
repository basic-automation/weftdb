//! libSQL per-aspect `metadata.db` (roadmap **Phase 4.3**, segment-set rollup).
//!
//! The Storage v2 layout the roadmap names has four pieces: `catalog.db`, a per-aspect
//! `metadata.db`, the `segments/*.weftseg` files, and `segment_index.db`. Three of those
//! four already exist: the
//! [`CatalogStore`](crate::CatalogStore) (databases/subjects), the
//! [`AspectCatalog`](crate::AspectCatalog) (per-aspect schema), and the
//! [`SegmentIndexStore`](crate::SegmentIndexStore) (one row *per segment* — the
//! data-skipping inputs). The missing layer is the per-aspect `metadata.db`: a
//! **materialized rollup over an aspect's whole segment set**, beside the per-segment
//! index.
//!
//! The segment index can already answer "how many rows / bytes / what time span does
//! this aspect cover?" — but only by scanning *every* descriptor row
//! ([`SegmentStore::aspect_stats`](crate::SegmentStore) loads the full resident index
//! to do exactly that). [`AspectMetadataStore`] persists that answer as a single row
//! per aspect, so the aspect-wide summary is an O(1) lookup rather than an O(segments)
//! scan — the natural home for segment-set metadata the roadmap layout reserves a DB
//! for.
//!
//! Boundary (hard constraint #3): this is **control-plane** metadata only — counts,
//! byte totals, and min/max bounds *about* an aspect's segments, never a measurement.
//! The measurement bytes live in the `.weftseg` files WeftDB owns. The `BigDecimal` value
//! bounds round-trip through their plain-text form (hard constraint #4 — no silent
//! float downcast, even in the rollup). It uses the same MVCC write path the rest of
//! the control plane does (`BEGIN CONCURRENT`).

use std::str::FromStr;

use anyhow::{bail, Result};
use bigdecimal::BigDecimal;
use turso::{Builder, Value};
use weft_physical_type::{SegmentDescriptor, SegmentIndex};

/// The materialized segment-set rollup for one aspect — the aspect-wide summary the
/// [`AspectMetadataStore`] persists and returns.
///
/// Derive one from a resident [`SegmentIndex`] with [`AspectMetadata::from_index`]
/// (the authoritative, idempotent path), or fold a single freshly sealed segment into
/// an existing rollup with [`AspectMetadata::folded`] (the O(1) incremental path the
/// store uses on each seal).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AspectMetadata {
	/// Number of sealed segments recorded for the aspect.
	pub segment_count: usize,
	/// Total rows (present and null) across every segment.
	pub total_rows: u64,
	/// Total null rows across every segment.
	pub total_nulls: u64,
	/// Total realized on-disk bytes across every `.weftseg` frame.
	pub total_bytes: u64,
	/// The number of sealed segments whose timestamps are **not** monotonic
	/// non-decreasing — the aspect's order-health signal (an out-of-order segment
	/// forces a linear scan on a point lookup; roadmap Phase 4.6). Zero when every
	/// segment admits ordered access.
	pub unsorted_segments: usize,
	/// The inclusive `(min, max)` timestamp span the aspect covers, or [`None`] when it
	/// holds no non-empty segment.
	pub time_range: Option<(i64, i64)>,
	/// The inclusive `(min, max)` value span the aspect covers, or [`None`] when it
	/// holds no value-bearing segment.
	pub value_range: Option<(BigDecimal, BigDecimal)>,
}

impl AspectMetadata {
	/// Derive the rollup from a resident [`SegmentIndex`] — the authoritative summary
	/// over every recorded descriptor. Idempotent: the same index always yields the
	/// same rollup, so re-deriving after any change keeps the materialized row exact.
	#[must_use]
	pub fn from_index(index: &SegmentIndex) -> Self {
		let mut meta = Self { segment_count: index.len(), total_rows: index.total_rows(), total_bytes: index.total_bytes(), unsorted_segments: index.unsorted_count(), time_range: index.time_range(), ..Self::default() };
		meta.total_nulls = index.iter().map(|d| d.null_count as u64).sum();
		meta.value_range = value_span(index.iter());
		meta
	}

	/// Fold a single freshly sealed segment's `descriptor` into this rollup, returning
	/// the updated rollup — the O(1) incremental update for the seal path (one segment
	/// added, counts and bounds extended).
	///
	/// This assumes `descriptor` is a *new* segment (a fresh, monotonic id), the way
	/// [`SegmentStore`](crate::SegmentStore) seals: folding the same descriptor twice
	/// would double-count. When in doubt, re-derive with [`from_index`](AspectMetadata::from_index).
	#[must_use]
	pub fn folded(mut self, descriptor: &SegmentDescriptor) -> Self {
		self.segment_count += 1;
		self.total_rows += descriptor.row_count as u64;
		self.total_nulls += descriptor.null_count as u64;
		self.total_bytes += descriptor.byte_len;
		self.unsorted_segments += usize::from(!descriptor.time_sorted);
		if let Some((lo, hi)) = descriptor.time_range() {
			self.time_range = Some(match self.time_range {
				Some((slo, shi)) => (slo.min(lo), shi.max(hi)),
				None => (lo, hi),
			});
		}
		if let (Some(lo), Some(hi)) = (descriptor.min_value.clone(), descriptor.max_value.clone()) {
			self.value_range = Some(match self.value_range.take() {
				Some((slo, shi)) => (slo.min(lo), shi.max(hi)),
				None => (lo, hi),
			});
		}
		self
	}

	/// Aspect-wide storage cost in **bytes per point**: the total framed bytes over the
	/// total rows (the north-star cost term). Zero when the aspect holds no rows.
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

/// The inclusive `(min, max)` value span across a sequence of descriptors, or [`None`]
/// when none carries a value bound.
fn value_span<'a>(descriptors: impl Iterator<Item = &'a SegmentDescriptor>) -> Option<(BigDecimal, BigDecimal)> {
	let mut span: Option<(BigDecimal, BigDecimal)> = None;
	for descriptor in descriptors {
		if let (Some(lo), Some(hi)) = (descriptor.min_value.clone(), descriptor.max_value.clone()) {
			span = Some(match span {
				Some((slo, shi)) => (slo.min(lo), shi.max(hi)),
				None => (lo, hi),
			});
		}
	}
	span
}

/// A durable, libSQL-backed store of per-aspect segment-set rollups.
///
/// Open one with [`AspectMetadataStore::open`] (a file path) or
/// [`AspectMetadataStore::open_in_memory`] (tests); materialize an aspect's rollup with
/// [`put`](AspectMetadataStore::put) (or fold a fresh seal in with
/// [`record_seal`](AspectMetadataStore::record_seal)); read it back with
/// [`get`](AspectMetadataStore::get).
pub struct AspectMetadataStore {
	db: turso::Database,
}

impl AspectMetadataStore {
	/// Open (creating if absent) the `metadata.db` at `path`, enabling MVCC and ensuring
	/// the `aspect_metadata` table exists.
	///
	/// # Errors
	///
	/// Propagates any libSQL connection or DDL failure.
	pub async fn open(path: &str) -> Result<Self> {
		let db = Builder::new_local(path).build().await?;
		let conn = db.connect()?;
		// Match the control plane's MVCC write path (Turso 0.6, no AUTOINCREMENT).
		conn.execute("PRAGMA journal_mode=experimental_mvcc", turso::params![]).await.ok();
		conn.execute("PRAGMA busy_timeout=600000", turso::params![]).await.ok();
		conn.execute(
			"CREATE TABLE IF NOT EXISTS aspect_metadata (
				aspect TEXT NOT NULL,
				segment_count INTEGER NOT NULL,
				total_rows INTEGER NOT NULL,
				total_nulls INTEGER NOT NULL,
				total_bytes INTEGER NOT NULL,
				unsorted_segments INTEGER NOT NULL DEFAULT 0,
				min_ts INTEGER,
				max_ts INTEGER,
				min_value TEXT,
				max_value TEXT,
				PRIMARY KEY (aspect)
			)",
			turso::params![],
		)
		.await?;
		// Migrate a pre-existing metadata.db created before the order-health column:
		// CREATE TABLE IF NOT EXISTS above is a no-op on it, so add the column here.
		// A duplicate-column error (fresh table already has it) is expected and ignored.
		conn.execute("ALTER TABLE aspect_metadata ADD COLUMN unsorted_segments INTEGER NOT NULL DEFAULT 0", turso::params![]).await.ok();
		Ok(Self { db })
	}

	/// Open an ephemeral in-memory store (`:memory:`) for tests.
	///
	/// # Errors
	///
	/// Propagates any libSQL connection or DDL failure.
	pub async fn open_in_memory() -> Result<Self> {
		Self::open(":memory:").await
	}

	/// Snapshot this `metadata.db` to `dest` (a fresh file) via Turso's `VACUUM INTO`,
	/// verifying the copy opens and its rows match. The online, consistent control-plane
	/// backup primitive (roadmap Phase 7.4) — see
	/// [`snapshot_and_verify`](crate::snapshot_and_verify) for the consistency scope.
	///
	/// # Errors
	///
	/// Propagates a connection failure or any backup/verify failure.
	pub async fn backup_to(&self, dest: &std::path::Path) -> Result<crate::SnapshotReport> {
		self.backup_to_with(dest, crate::VerifyMode::default()).await
	}

	/// Snapshot this database to `dest` under an explicit [`VerifyMode`](crate::VerifyMode).
	///
	/// [`VerifyMode::SnapshotOnly`](crate::VerifyMode::SnapshotOnly) verifies the copy
	/// without re-reading the source, so it is the mode an **online** backup (the backup
	/// daemon) must use while writers are still committing.
	///
	/// # Errors
	///
	/// Propagates a connection failure or any backup/verify failure.
	pub async fn backup_to_with(&self, dest: &std::path::Path, mode: crate::VerifyMode) -> Result<crate::SnapshotReport> {
		let conn = self.db.connect()?;
		crate::types::backup::snapshot_with_verify(&conn, dest, mode).await
	}

	/// Materialize (or overwrite) the rollup for `aspect`, returning the number of
	/// control-plane rows the write actually affected.
	///
	/// `INSERT OR REPLACE` makes the call idempotent on the aspect key, so re-deriving
	/// the rollup from the segment index and writing it back keeps the row exact.
	///
	/// The returned count is libSQL's `Statement::n_change()` — see
	/// [`SegmentIndexStore::insert`](crate::SegmentIndexStore::insert) for why the write
	/// path reports it. Also emitted as a `rows_changed` field on a
	/// `control_plane.metadata.put` debug span.
	///
	/// # Errors
	///
	/// Propagates any libSQL write failure.
	pub async fn put(&self, aspect: &str, meta: &AspectMetadata) -> Result<u64> {
		let span = tracing::debug_span!("control_plane.metadata.put", aspect, rows_changed = tracing::field::Empty);
		let _guard = span.enter();
		let (min_ts, max_ts) = meta.time_range.map_or((Value::Null, Value::Null), |(lo, hi)| (Value::Integer(lo), Value::Integer(hi)));
		let (min_value, max_value) = meta.value_range.as_ref().map_or((Value::Null, Value::Null), |(lo, hi)| (Value::Text(lo.to_plain_string()), Value::Text(hi.to_plain_string())));
		let conn = self.db.connect()?;
		conn.execute("BEGIN CONCURRENT", turso::params![]).await?;
		let res = conn
			.execute(
				"INSERT OR REPLACE INTO aspect_metadata
				(aspect, segment_count, total_rows, total_nulls, total_bytes, unsorted_segments, min_ts, max_ts, min_value, max_value)
				VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
				turso::params![aspect.to_string(), i64::try_from(meta.segment_count).unwrap_or(i64::MAX), i64::try_from(meta.total_rows).unwrap_or(i64::MAX), i64::try_from(meta.total_nulls).unwrap_or(i64::MAX), i64::try_from(meta.total_bytes).unwrap_or(i64::MAX), i64::try_from(meta.unsorted_segments).unwrap_or(i64::MAX), min_ts, max_ts, min_value, max_value],
			)
			.await;
		match res {
			Ok(changed) => {
				conn.execute("COMMIT", turso::params![]).await?;
				span.record("rows_changed", changed);
				Ok(changed)
			}
			Err(e) => {
				conn.execute("ROLLBACK", turso::params![]).await.ok();
				bail!("aspect_metadata put failed: {e}")
			}
		}
	}

	/// Fold a freshly sealed segment's `descriptor` into `aspect`'s rollup — read the
	/// current row (an empty rollup if none), [`fold`](AspectMetadata::folded) the
	/// descriptor in, and write it back. The O(1)-per-seal incremental update the
	/// segment store uses.
	///
	/// # Errors
	///
	/// Propagates any libSQL read or write failure.
	pub async fn record_seal(&self, aspect: &str, descriptor: &SegmentDescriptor) -> Result<AspectMetadata> {
		let updated = self.get(aspect).await?.unwrap_or_default().folded(descriptor);
		self.put(aspect, &updated).await?;
		Ok(updated)
	}

	/// The materialized rollup for `aspect`, or [`None`] if none has been recorded.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure, or a value bound that cannot be decoded.
	pub async fn get(&self, aspect: &str) -> Result<Option<AspectMetadata>> {
		let conn = self.db.connect()?;
		let mut rows = conn
			.query(
				"SELECT segment_count, total_rows, total_nulls, total_bytes, min_ts, max_ts, min_value, max_value, unsorted_segments
				FROM aspect_metadata WHERE aspect = ?",
				turso::params![aspect.to_string()],
			)
			.await?;
		match rows.next().await? {
			Some(row) => Ok(Some(Self::decode_row(&row)?)),
			None => Ok(None),
		}
	}

	/// Every aspect with a materialized rollup, in name order.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn list_aspects(&self) -> Result<Vec<String>> {
		let conn = self.db.connect()?;
		let mut rows = conn.query("SELECT aspect FROM aspect_metadata ORDER BY aspect", turso::params![]).await?;
		let mut out = Vec::new();
		while let Some(row) = rows.next().await? {
			if let Value::Text(s) = row.get_value(0)? {
				out.push(s);
			}
		}
		Ok(out)
	}

	/// Remove `aspect`'s rollup (idempotent — removing an absent aspect is a no-op).
	///
	/// # Errors
	///
	/// Propagates any libSQL write failure.
	pub async fn remove(&self, aspect: &str) -> Result<()> {
		let conn = self.db.connect()?;
		conn.execute("BEGIN CONCURRENT", turso::params![]).await?;
		let res = conn.execute("DELETE FROM aspect_metadata WHERE aspect = ?", turso::params![aspect.to_string()]).await;
		match res {
			Ok(_) => {
				conn.execute("COMMIT", turso::params![]).await?;
				Ok(())
			}
			Err(e) => {
				conn.execute("ROLLBACK", turso::params![]).await.ok();
				bail!("aspect_metadata remove failed: {e}")
			}
		}
	}

	/// Decode one row (columns in the query's fixed order) into an [`AspectMetadata`].
	fn decode_row(row: &turso::Row) -> Result<AspectMetadata> {
		let segment_count = usize::try_from(*row.get_value(0)?.as_integer().unwrap_or(&0)).unwrap_or(0);
		let total_rows = u64::try_from(*row.get_value(1)?.as_integer().unwrap_or(&0)).unwrap_or(0);
		let total_nulls = u64::try_from(*row.get_value(2)?.as_integer().unwrap_or(&0)).unwrap_or(0);
		let total_bytes = u64::try_from(*row.get_value(3)?.as_integer().unwrap_or(&0)).unwrap_or(0);
		let min_ts = Self::opt_integer(row, 4);
		let max_ts = Self::opt_integer(row, 5);
		let time_range = match (min_ts, max_ts) {
			(Some(lo), Some(hi)) => Some((lo, hi)),
			_ => None,
		};
		let min_value = Self::opt_decimal(row, 6)?;
		let max_value = Self::opt_decimal(row, 7)?;
		let value_range = match (min_value, max_value) {
			(Some(lo), Some(hi)) => Some((lo, hi)),
			_ => None,
		};
		let unsorted_segments = usize::try_from(*row.get_value(8)?.as_integer().unwrap_or(&0)).unwrap_or(0);
		Ok(AspectMetadata { segment_count, total_rows, total_nulls, total_bytes, unsorted_segments, time_range, value_range })
	}

	/// Read a nullable integer column, distinguishing SQL `NULL` from `0`.
	fn opt_integer(row: &turso::Row, idx: usize) -> Option<i64> {
		match row.get_value(idx) {
			Ok(Value::Integer(n)) => Some(n),
			_ => None,
		}
	}

	/// Read a nullable `BigDecimal` column stored as its plain-text form.
	fn opt_decimal(row: &turso::Row, idx: usize) -> Result<Option<BigDecimal>> {
		match row.get_value(idx) {
			Ok(Value::Text(s)) => Ok(Some(BigDecimal::from_str(&s)?)),
			_ => Ok(None),
		}
	}
}

#[cfg(test)]
mod tests {
	use weft_physical_type::{timestamp::TimeUnit, Segment};

	use super::*;

	fn bd(s: &str) -> BigDecimal {
		BigDecimal::from_str(s).expect("parses")
	}

	/// A sealed single-block segment over `[base, base+90]`, values `base..base+10`.
	fn sealed(base: i64) -> (SegmentDescriptor, u64) {
		let ts: Vec<i64> = (0..10).map(|i| base + i * 10).collect();
		let vs: Vec<BigDecimal> = (0..10).map(|i| BigDecimal::from(base + i)).collect();
		let seg = Segment::build(&ts, &vs, TimeUnit::Seconds, &bd("0")).expect("builds");
		let len = seg.write_to().len() as u64;
		(SegmentDescriptor::of_segment(0, "s.weftseg", len, &seg), len)
	}

	#[tokio::test]
	async fn put_then_get_round_trips() {
		let store = AspectMetadataStore::open_in_memory().await.expect("opens");
		let meta = AspectMetadata { segment_count: 2, total_rows: 20, total_nulls: 3, total_bytes: 256, unsorted_segments: 1, time_range: Some((0, 190)), value_range: Some((bd("0"), bd("19.5"))) };
		store.put("temp", &meta).await.expect("puts");
		let got = store.get("temp").await.expect("gets");
		let missing = store.get("absent").await.expect("gets");
		drop(store);
		assert_eq!(got, Some(meta));
		assert_eq!(missing, None);
	}

	/// `put` reports libSQL's affected-row count (`Statement::n_change()`), so the write
	/// path can tell a real rollup mutation from a no-op. `INSERT OR REPLACE` on the
	/// aspect key affects exactly one row whether it inserts or overwrites — the count is
	/// the write's own report, not an inference from whether a row already existed.
	#[tokio::test]
	async fn put_reports_its_affected_row_count() {
		let store = AspectMetadataStore::open_in_memory().await.expect("opens");
		let meta = AspectMetadata { segment_count: 1, total_rows: 10, total_nulls: 0, total_bytes: 128, unsorted_segments: 0, time_range: Some((0, 90)), value_range: Some((bd("0"), bd("9"))) };
		let first = store.put("temp", &meta).await.expect("puts");
		// Overwriting the same aspect key replaces the row rather than adding one.
		let overwrite = store.put("temp", &meta).await.expect("re-puts");
		let other = store.put("humidity", &meta).await.expect("puts a second aspect");
		let rows = store.list_aspects().await.expect("lists").len();
		drop(store);
		assert_eq!(first, 1, "a fresh rollup write affects one row");
		assert_eq!(overwrite, 1, "an INSERT OR REPLACE on the same key still affects one row");
		assert_eq!(other, 1);
		assert_eq!(rows, 2, "the overwrite did not add a row");
	}

	#[tokio::test]
	async fn from_index_matches_a_fold_chain() {
		let store = AspectMetadataStore::open_in_memory().await.expect("opens");
		// Build a two-segment index and the equivalent fold chain; they must agree.
		let mut index = SegmentIndex::new();
		let mut folded = AspectMetadata::default();
		for (i, base) in [0_i64, 100].into_iter().enumerate() {
			let (mut d, _) = sealed(base);
			d.id = i as u64;
			index.push(d.clone());
			folded = folded.folded(&d);
		}
		let derived = AspectMetadata::from_index(&index);
		store.put("a", &derived).await.expect("puts");
		let got = store.get("a").await.expect("gets");
		drop(store);
		assert_eq!(derived, folded, "from_index and a fold chain agree");
		assert_eq!(derived.segment_count, 2);
		assert_eq!(derived.total_rows, 20);
		assert_eq!(derived.time_range, Some((0, 190)));
		assert_eq!(derived.value_range, Some((bd("0"), bd("109"))));
		assert_eq!(got, Some(derived));
	}

	#[tokio::test]
	async fn record_seal_accumulates_incrementally() {
		let store = AspectMetadataStore::open_in_memory().await.expect("opens");
		let (first, len0) = sealed(0);
		let (second, len1) = sealed(100);
		// First seal creates the rollup; second folds in.
		let after_first = store.record_seal("a", &first).await.expect("records");
		let after_second = store.record_seal("a", &second).await.expect("records");
		let got = store.get("a").await.expect("gets");
		drop(store);
		assert_eq!(after_first.segment_count, 1);
		assert_eq!(after_first.total_rows, 10);
		assert_eq!(after_first.total_bytes, len0);
		assert_eq!(after_second.segment_count, 2);
		assert_eq!(after_second.total_rows, 20);
		assert_eq!(after_second.total_bytes, len0 + len1);
		assert_eq!(after_second.time_range, Some((0, 190)));
		assert_eq!(after_second.value_range, Some((bd("0"), bd("109"))));
		assert_eq!(got, Some(after_second));
	}

	#[tokio::test]
	async fn nullable_segment_counts_nulls_and_skips_empty_value_span() {
		let store = AspectMetadataStore::open_in_memory().await.expect("opens");
		// Two nulls among four rows; the value span still spans the present values.
		let ts = vec![10_i64, 20, 30, 40];
		let vs = vec![Some(bd("1.25")), None, Some(bd("3.75")), None];
		let seg = Segment::build_nullable(&ts, &vs, TimeUnit::Seconds, &bd("0")).expect("builds");
		let d = SegmentDescriptor::of_segment(0, "n.weftseg", seg.write_to().len() as u64, &seg);
		let meta = store.record_seal("a", &d).await.expect("records");
		let got = store.get("a").await.expect("gets");
		drop(store);
		assert_eq!(meta.total_rows, 4);
		assert_eq!(meta.total_nulls, 2);
		assert_eq!(meta.value_range, Some((bd("1.25"), bd("3.75"))));
		assert_eq!(got, Some(meta));
	}

	#[tokio::test]
	async fn empty_segment_records_no_bounds() {
		let store = AspectMetadataStore::open_in_memory().await.expect("opens");
		let empty = Segment::build(&[], &[], TimeUnit::Seconds, &bd("0")).expect("builds");
		let d = SegmentDescriptor::of_segment(0, "empty.weftseg", empty.write_to().len() as u64, &empty);
		let meta = store.record_seal("a", &d).await.expect("records");
		let got = store.get("a").await.expect("gets");
		drop(store);
		assert_eq!(meta.segment_count, 1);
		assert_eq!(meta.total_rows, 0);
		assert_eq!(meta.time_range, None, "an empty segment contributes no time bound");
		assert_eq!(meta.value_range, None);
		assert_eq!(got, Some(meta));
	}

	#[tokio::test]
	async fn list_and_remove() {
		let store = AspectMetadataStore::open_in_memory().await.expect("opens");
		let (d, _) = sealed(0);
		store.record_seal("temp", &d).await.expect("records");
		store.record_seal("humidity", &d).await.expect("records");
		let listed = store.list_aspects().await.expect("lists");
		store.remove("temp").await.expect("removes");
		// Removing an absent aspect is a no-op.
		store.remove("absent").await.expect("idempotent");
		let after = store.list_aspects().await.expect("lists");
		drop(store);
		assert_eq!(listed, vec!["humidity".to_string(), "temp".to_string()]);
		assert_eq!(after, vec!["humidity".to_string()]);
	}

	#[tokio::test]
	async fn store_reopens_and_sees_prior_rollup() {
		let dir = tempfile::TempDir::new().expect("tempdir");
		let path = dir.path().join("metadata.db");
		let path = path.to_string_lossy().into_owned();
		let (d, _) = sealed(0);
		let first = AspectMetadataStore::open(&path).await.expect("opens");
		let recorded = first.record_seal("a", &d).await.expect("records");
		drop(first);
		let reopened = AspectMetadataStore::open(&path).await.expect("reopens");
		let got = reopened.get("a").await.expect("gets");
		drop(reopened);
		assert_eq!(got, Some(recorded));
	}

	#[tokio::test]
	async fn bytes_per_point_over_rows() {
		let meta = AspectMetadata { segment_count: 1, total_rows: 10, total_nulls: 0, total_bytes: 250, unsorted_segments: 0, time_range: Some((0, 90)), value_range: Some((bd("0"), bd("9"))) };
		let empty = AspectMetadata::default();
		assert!((meta.bytes_per_point() - 25.0).abs() < f64::EPSILON);
		assert!((empty.bytes_per_point() - 0.0).abs() < f64::EPSILON);
	}
}
