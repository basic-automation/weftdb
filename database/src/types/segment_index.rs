//! libSQL-backed `segment_index` control plane (roadmap **Phase 4.3**).
//!
//! Phase 4.3 sealed measurement data into typed columnar `.dspseg` segments and
//! gave each a [`SegmentDescriptor`](dsp_physical_type::SegmentDescriptor) — the
//! resident index row (min/max ts/value, row/null counts, byte length, path) that
//! lets a query prune segments without opening them. This module makes that index
//! **durable**: it persists descriptors into a libSQL `segment_index` table and
//! answers a time-range query with a SQL `WHERE` over the integer `min_ts`/`max_ts`
//! columns, returning only the descriptors a query must open.
//!
//! This is squarely a **control-plane** component (hard constraint #3): libSQL owns
//! catalog/metadata; the measurement hot path stays in the `.dspseg` segments. The
//! store holds *metadata about* segments — never the measurements themselves. The
//! `BigDecimal` value bounds round-trip through their plain-text form (hard
//! constraint #4 — no silent float downcast, even in the catalog), and the
//! `PhysicalType`/`TimeUnit` metadata round-trip as JSON so a `ScaledI64 { scale }`
//! reconstructs faithfully.
//!
//! A single `segment_index.db` can index many aspects: every row is scoped by an
//! `aspect` key, so [`SegmentIndexStore::prune_by_time`] and the accessors all take
//! the aspect they operate on. It uses the same MVCC-concurrent write path the rest
//! of the control plane does (`BEGIN CONCURRENT` for inserts, `BEGIN IMMEDIATE` for
//! the one-time DDL).

use std::str::FromStr;

use anyhow::{bail, Result};
use bigdecimal::BigDecimal;
use dsp_physical_type::{SegmentDescriptor, SegmentIndex};
use turso::{Builder, Value};

/// A durable, libSQL-backed index of sealed segments, scoped by aspect.
///
/// Open one with [`SegmentIndexStore::open`] (a file path) or
/// [`SegmentIndexStore::open_in_memory`] (tests); record a freshly sealed segment
/// with [`insert`](SegmentIndexStore::insert); prune a query with
/// [`prune_by_time`](SegmentIndexStore::prune_by_time).
pub struct SegmentIndexStore {
	db: turso::Database,
}

impl SegmentIndexStore {
	/// Open (creating if absent) the `segment_index.db` at `path`, enabling MVCC and
	/// ensuring the `segment_index` table exists.
	///
	/// # Errors
	///
	/// Propagates any libSQL connection or DDL failure.
	pub async fn open(path: &str) -> Result<Self> {
		let db = Builder::new_local(path).build().await?;
		Self::configure_and_wireframe(&db).await?;
		Ok(Self { db })
	}

	/// Open an ephemeral in-memory store — the path `:memory:` — for tests.
	///
	/// # Errors
	///
	/// Propagates any libSQL connection or DDL failure.
	pub async fn open_in_memory() -> Result<Self> {
		Self::open(":memory:").await
	}

	/// Enable MVCC and create the `segment_index` table if it does not exist.
	async fn configure_and_wireframe(db: &turso::Database) -> Result<()> {
		let conn = db.connect()?;
		// Match the control plane's MVCC write path (Turso 0.6, no AUTOINCREMENT).
		conn.execute("PRAGMA journal_mode=experimental_mvcc", turso::params![]).await.ok();
		conn.execute("PRAGMA busy_timeout=600000", turso::params![]).await.ok();
		conn.execute(
			"CREATE TABLE IF NOT EXISTS segment_index (
				aspect TEXT NOT NULL,
				id INTEGER NOT NULL,
				path TEXT NOT NULL,
				format_version INTEGER NOT NULL,
				physical_type TEXT,
				time_unit TEXT,
				row_count INTEGER NOT NULL,
				null_count INTEGER NOT NULL,
				time_sorted INTEGER NOT NULL,
				min_ts INTEGER,
				max_ts INTEGER,
				min_value TEXT,
				max_value TEXT,
				byte_len INTEGER NOT NULL,
				PRIMARY KEY (aspect, id)
			)",
			turso::params![],
		)
		.await?;
		// A covering range index over the time span turns prune_by_time into an index scan.
		conn.execute("CREATE INDEX IF NOT EXISTS idx_segment_index_time ON segment_index(aspect, min_ts, max_ts)", turso::params![]).await.ok();
		Ok(())
	}

	/// Record a sealed segment's descriptor under `aspect`.
	///
	/// Replaces any existing row with the same `(aspect, id)` (a re-seal of the same
	/// segment id overwrites), so the call is idempotent on segment identity.
	///
	/// # Errors
	///
	/// Propagates any libSQL write failure, or a metadata-serialization failure.
	pub async fn insert(&self, aspect: &str, descriptor: &SegmentDescriptor) -> Result<()> {
		let conn = self.db.connect()?;
		conn.execute("BEGIN CONCURRENT", turso::params![]).await?;
		let physical_type = match &descriptor.physical_type {
			Some(pt) => Value::Text(serde_json::to_string(pt)?),
			None => Value::Null,
		};
		let time_unit = match &descriptor.time_unit {
			Some(tu) => Value::Text(serde_json::to_string(tu)?),
			None => Value::Null,
		};
		let res = conn
			.execute(
				"INSERT OR REPLACE INTO segment_index
				(aspect, id, path, format_version, physical_type, time_unit, row_count, null_count, time_sorted, min_ts, max_ts, min_value, max_value, byte_len)
				VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
				turso::params![aspect.to_string(), i64::try_from(descriptor.id).unwrap_or(i64::MAX), descriptor.path.clone(), i64::from(descriptor.format_version), physical_type, time_unit, i64::try_from(descriptor.row_count).unwrap_or(i64::MAX), i64::try_from(descriptor.null_count).unwrap_or(i64::MAX), i64::from(descriptor.time_sorted), descriptor.min_ts.map_or(Value::Null, Value::Integer), descriptor.max_ts.map_or(Value::Null, Value::Integer), descriptor.min_value.as_ref().map_or(Value::Null, |v| Value::Text(v.to_plain_string())), descriptor.max_value.as_ref().map_or(Value::Null, |v| Value::Text(v.to_plain_string())), i64::try_from(descriptor.byte_len).unwrap_or(i64::MAX),],
			)
			.await;
		match res {
			Ok(_) => {
				conn.execute("COMMIT", turso::params![]).await?;
				Ok(())
			}
			Err(e) => {
				conn.execute("ROLLBACK", turso::params![]).await.ok();
				bail!("segment_index insert failed: {e}")
			}
		}
	}

	/// **Data skipping at the control plane** (roadmap Phase 4.4): the descriptors
	/// for `aspect` whose segments may hold a row in the inclusive time range
	/// `[start, end]` — the ones a query must open. The pruning runs as a SQL
	/// `WHERE min_ts <= end AND start <= max_ts` over the indexed integer columns, so
	/// disjoint segments are skipped without their rows ever leaving libSQL. Ordered
	/// by segment `id` (seal order).
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure, or a row that cannot be decoded back into
	/// a [`SegmentDescriptor`].
	pub async fn prune_by_time(&self, aspect: &str, start: i64, end: i64) -> Result<Vec<SegmentDescriptor>> {
		let conn = self.db.connect()?;
		// A NULL-spanned (empty) segment can never overlap, and the inequalities reject it.
		let rows = conn
			.query(
				"SELECT id, path, format_version, physical_type, time_unit, row_count, null_count, time_sorted, min_ts, max_ts, min_value, max_value, byte_len
				FROM segment_index
				WHERE aspect = ? AND min_ts IS NOT NULL AND max_ts IS NOT NULL AND min_ts <= ? AND ? <= max_ts
				ORDER BY id",
				turso::params![aspect.to_string(), end, start],
			)
			.await?;
		Self::collect(rows).await
	}

	/// Every descriptor recorded under `aspect`, in seal (`id`) order.
	///
	/// Use this to rebuild the resident [`SegmentIndex`] for finer in-memory pruning
	/// (value/quality), via [`load_index`](SegmentIndexStore::load_index).
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure, or an undecodable row.
	pub async fn all(&self, aspect: &str) -> Result<Vec<SegmentDescriptor>> {
		let conn = self.db.connect()?;
		let rows = conn
			.query(
				"SELECT id, path, format_version, physical_type, time_unit, row_count, null_count, time_sorted, min_ts, max_ts, min_value, max_value, byte_len
				FROM segment_index WHERE aspect = ? ORDER BY id",
				turso::params![aspect.to_string()],
			)
			.await?;
		Self::collect(rows).await
	}

	/// Load every descriptor for `aspect` into a resident
	/// [`SegmentIndex`](dsp_physical_type::SegmentIndex) — the in-memory model whose
	/// value/quality pruning complements this store's SQL time pruning.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure, or an undecodable row.
	pub async fn load_index(&self, aspect: &str) -> Result<SegmentIndex> {
		let mut index = SegmentIndex::new();
		for descriptor in self.all(aspect).await? {
			index.push(descriptor);
		}
		Ok(index)
	}

	/// Number of segments indexed under `aspect`.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn count(&self, aspect: &str) -> Result<usize> {
		let conn = self.db.connect()?;
		let mut rows = conn.query("SELECT COUNT(*) FROM segment_index WHERE aspect = ?", turso::params![aspect.to_string()]).await?;
		let row = rows.next().await?.ok_or_else(|| anyhow::anyhow!("COUNT returned no row"))?;
		let n = *row.get_value(0)?.as_integer().unwrap_or(&0);
		Ok(usize::try_from(n).unwrap_or(0))
	}

	/// The next unused segment `id` for `aspect`: one past the current maximum, or
	/// `0` when the aspect has no segments yet. The id a fresh seal should claim so
	/// segment ids stay monotonic within an aspect.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn next_id(&self, aspect: &str) -> Result<u64> {
		let conn = self.db.connect()?;
		let mut rows = conn.query("SELECT MAX(id) FROM segment_index WHERE aspect = ?", turso::params![aspect.to_string()]).await?;
		let row = rows.next().await?.ok_or_else(|| anyhow::anyhow!("MAX returned no row"))?;
		// MAX over no rows is SQL NULL → start at 0; otherwise one past the maximum.
		match row.get_value(0)? {
			Value::Integer(max) => Ok(u64::try_from(max).unwrap_or(0).saturating_add(1)),
			_ => Ok(0),
		}
	}

	/// Decode a result set (the full descriptor column list, in the fixed order the
	/// queries select) into [`SegmentDescriptor`]s.
	async fn collect(mut rows: turso::Rows) -> Result<Vec<SegmentDescriptor>> {
		let mut out = Vec::new();
		while let Some(row) = rows.next().await? {
			out.push(Self::decode_row(&row)?);
		}
		Ok(out)
	}

	/// Decode one row (columns in the queries' fixed order) back into a descriptor.
	fn decode_row(row: &turso::Row) -> Result<SegmentDescriptor> {
		let id = u64::try_from(*row.get_value(0)?.as_integer().unwrap_or(&0)).unwrap_or(0);
		let path = row.get_value(1)?.as_text().cloned().unwrap_or_default();
		let format_version = u16::try_from(*row.get_value(2)?.as_integer().unwrap_or(&0)).unwrap_or(0);
		let physical_type = match row.get_value(3)? {
			Value::Text(s) => Some(serde_json::from_str(&s)?),
			_ => None,
		};
		let time_unit = match row.get_value(4)? {
			Value::Text(s) => Some(serde_json::from_str(&s)?),
			_ => None,
		};
		let row_count = usize::try_from(*row.get_value(5)?.as_integer().unwrap_or(&0)).unwrap_or(0);
		let null_count = usize::try_from(*row.get_value(6)?.as_integer().unwrap_or(&0)).unwrap_or(0);
		let time_sorted = *row.get_value(7)?.as_integer().unwrap_or(&1) != 0;
		let min_ts = Self::opt_integer(row, 8);
		let max_ts = Self::opt_integer(row, 9);
		let min_value = Self::opt_decimal(row, 10)?;
		let max_value = Self::opt_decimal(row, 11)?;
		let byte_len = u64::try_from(*row.get_value(12)?.as_integer().unwrap_or(&0)).unwrap_or(0);
		Ok(SegmentDescriptor { id, path, format_version, physical_type, time_unit, row_count, null_count, time_sorted, min_ts, max_ts, min_value, max_value, byte_len })
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
	use dsp_physical_type::{timestamp::TimeUnit, Segment};

	use super::*;

	fn bd(s: &str) -> BigDecimal {
		BigDecimal::from_str(s).expect("parses")
	}

	/// A sealed single-block segment over `[base, base+90]`, values 0..10.
	fn sealed(base: i64) -> (Segment, u64) {
		let ts: Vec<i64> = (0..10).map(|i| base + i * 10).collect();
		let vs: Vec<BigDecimal> = (0..10).map(BigDecimal::from).collect();
		let seg = Segment::build(&ts, &vs, TimeUnit::Seconds, &bd("0")).expect("builds");
		let len = seg.write_to().len() as u64;
		(seg, len)
	}

	#[tokio::test]
	async fn insert_then_read_round_trips_a_descriptor() {
		let store = SegmentIndexStore::open_in_memory().await.expect("opens");
		let (seg, len) = sealed(100);
		let d = SegmentDescriptor::of_segment(1, "segments/1.dspseg", len, &seg);
		store.insert("temp", &d).await.expect("inserts");
		let all = store.all("temp").await.expect("reads");
		let count = store.count("temp").await.expect("counts");
		drop(store);
		assert_eq!(all.len(), 1);
		// The descriptor round-trips field-for-field through libSQL.
		assert_eq!(all[0], d);
		assert_eq!(count, 1);
	}

	#[tokio::test]
	async fn prune_by_time_skips_disjoint_segments_in_sql() {
		let store = SegmentIndexStore::open_in_memory().await.expect("opens");
		for (i, base) in [0_i64, 100, 200].into_iter().enumerate() {
			let (seg, len) = sealed(base);
			let d = SegmentDescriptor::of_segment(i as u64, format!("s{i}.dspseg"), len, &seg);
			store.insert("aspect", &d).await.expect("inserts");
		}
		let ids = |ds: &[SegmentDescriptor]| ds.iter().map(|d| d.id).collect::<Vec<_>>();
		// Each sealed() spans [base, base+90].
		let mid = store.prune_by_time("aspect", 120, 150).await.expect("prunes");
		let straddle = store.prune_by_time("aspect", 50, 150).await.expect("prunes");
		let all = store.prune_by_time("aspect", 0, 290).await.expect("prunes");
		let gap = store.prune_by_time("aspect", 91, 99).await.expect("prunes");
		drop(store);
		assert_eq!(ids(&mid), vec![1]);
		assert_eq!(ids(&straddle), vec![0, 1]);
		assert_eq!(ids(&all), vec![0, 1, 2]);
		assert!(gap.is_empty());
	}

	#[tokio::test]
	async fn aspects_are_isolated() {
		let store = SegmentIndexStore::open_in_memory().await.expect("opens");
		let (seg, len) = sealed(0);
		store.insert("a", &SegmentDescriptor::of_segment(0, "a0.dspseg", len, &seg)).await.expect("inserts");
		store.insert("b", &SegmentDescriptor::of_segment(0, "b0.dspseg", len, &seg)).await.expect("inserts");
		store.insert("b", &SegmentDescriptor::of_segment(1, "b1.dspseg", len, &seg)).await.expect("inserts");
		let count_a = store.count("a").await.expect("counts");
		let count_b = store.count("b").await.expect("counts");
		// Pruning never crosses aspects.
		let prune_a = store.prune_by_time("a", 0, 90).await.expect("prunes").len();
		drop(store);
		assert_eq!(count_a, 1);
		assert_eq!(count_b, 2);
		assert_eq!(prune_a, 1);
	}

	#[tokio::test]
	async fn reinsert_same_id_replaces() {
		let store = SegmentIndexStore::open_in_memory().await.expect("opens");
		let (seg, len) = sealed(0);
		store.insert("a", &SegmentDescriptor::of_segment(0, "old.dspseg", len, &seg)).await.expect("inserts");
		store.insert("a", &SegmentDescriptor::of_segment(0, "new.dspseg", len, &seg)).await.expect("re-inserts");
		let all = store.all("a").await.expect("reads");
		drop(store);
		assert_eq!(all.len(), 1, "same (aspect, id) replaces, not duplicates");
		assert_eq!(all[0].path, "new.dspseg");
	}

	#[tokio::test]
	async fn nullable_and_metadata_round_trip() {
		let store = SegmentIndexStore::open_in_memory().await.expect("opens");
		// A nullable segment with two nulls and a scaled encoding to exercise the
		// JSON metadata + plain-text decimal round trip.
		let ts = vec![10_i64, 20, 30, 40];
		let vs = vec![Some(bd("1.25")), None, Some(bd("3.75")), None];
		let seg = Segment::build_nullable(&ts, &vs, TimeUnit::Micros, &bd("0")).expect("builds");
		let d = SegmentDescriptor::of_segment(5, "n.dspseg", seg.write_to().len() as u64, &seg);
		store.insert("a", &d).await.expect("inserts");
		let back = store.all("a").await.expect("reads").into_iter().next().expect("one row");
		drop(store);
		assert_eq!(back, d);
		assert_eq!(back.null_count, 2);
		assert_eq!(back.physical_type, d.physical_type);
		assert_eq!(back.time_unit, Some(TimeUnit::Micros));
		assert_eq!(back.min_value, Some(bd("1.25")));
	}

	#[tokio::test]
	async fn load_index_rebuilds_resident_pruning() {
		let store = SegmentIndexStore::open_in_memory().await.expect("opens");
		// Values offset by base ⇒ disjoint value spans [0,9], [100,109], [200,209].
		for (i, base) in [0_i64, 100, 200].into_iter().enumerate() {
			let ts: Vec<i64> = (0..10).map(|v| base + v * 10).collect();
			let vs: Vec<BigDecimal> = (0..10).map(|v| BigDecimal::from(base + v)).collect();
			let seg = Segment::build(&ts, &vs, TimeUnit::Seconds, &bd("0")).expect("builds");
			store.insert("a", &SegmentDescriptor::of_segment(i as u64, format!("s{i}.dspseg"), seg.write_to().len() as u64, &seg)).await.expect("inserts");
		}
		let index = store.load_index("a").await.expect("loads");
		drop(store);
		assert_eq!(index.len(), 3);
		assert_eq!(index.total_rows(), 30);
		// The resident index answers value pruning the SQL store does not.
		let hit = index.prune_by_value(&bd("102"), &bd("108"));
		assert_eq!(hit.len(), 1, "values 102..108 only in the second segment");
		assert_eq!(hit[0].id, 1);
	}

	#[tokio::test]
	async fn empty_segment_never_overlaps() {
		let store = SegmentIndexStore::open_in_memory().await.expect("opens");
		let empty = Segment::build(&[], &[], TimeUnit::Seconds, &bd("0")).expect("builds");
		let d = SegmentDescriptor::of_segment(0, "empty.dspseg", empty.write_to().len() as u64, &empty);
		store.insert("a", &d).await.expect("inserts");
		// Stored, but its NULL span is excluded from every time prune.
		let count = store.count("a").await.expect("counts");
		let pruned = store.prune_by_time("a", i64::MIN, i64::MAX).await.expect("prunes");
		drop(store);
		assert_eq!(count, 1);
		assert!(pruned.is_empty());
	}

	#[tokio::test]
	async fn next_id_is_monotonic_per_aspect() {
		let store = SegmentIndexStore::open_in_memory().await.expect("opens");
		let (seg, len) = sealed(0);
		// An empty aspect starts at 0.
		let first = store.next_id("a").await.expect("next_id");
		store.insert("a", &SegmentDescriptor::of_segment(first, "a0.dspseg", len, &seg)).await.expect("inserts");
		// After inserting id 0, the next is 1; a different aspect is independent.
		let second = store.next_id("a").await.expect("next_id");
		let other = store.next_id("b").await.expect("next_id");
		store.insert("a", &SegmentDescriptor::of_segment(second, "a1.dspseg", len, &seg)).await.expect("inserts");
		let third = store.next_id("a").await.expect("next_id");
		drop(store);
		assert_eq!(first, 0);
		assert_eq!(second, 1);
		assert_eq!(third, 2);
		assert_eq!(other, 0, "a fresh aspect starts at 0");
	}
}
