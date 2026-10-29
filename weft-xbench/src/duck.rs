//! The `DuckDB` `DOUBLE` arm.
//!
//! `DuckDB` stores the knots as `(t BIGINT /* epoch ns */, v DOUBLE)` in an on-disk database file
//! and reconstructs the grid in SQL. `DuckDB` has no interpolation function, so the query is the one
//! a `DuckDB` user would write: `range()` names the grid, one `ASOF JOIN` finds the last knot at or
//! before each instant and another the first knot at or after it, and the value is the line
//! between them. Outside the data the first or last segment's line is extended, which is splimes'
//! `Linear` rule, so the two engines answer the same question.

use std::{
	path::PathBuf, sync::atomic::{AtomicU64, Ordering}
};

use bigdecimal::ToPrimitive;
use duckdb::{params, Connection};

use crate::{weft::Arm, ArmInfo, Dataset, Output};

/// Counter distinguishing database files of arms alive at once.
static NEXT_DB: AtomicU64 = AtomicU64::new(0);

/// The grid reconstruction. Parameters: first grid instant, last grid instant (inclusive), step,
/// all in epoch nanoseconds.
///
/// `lo`/`hi` are the knots either side; at an exact hit both are the knot itself. Before the
/// first knot `lo` is missing and the first segment (`f0`, `f1`) stands in for both ends; after
/// the last knot the same holds for `l0`, `l1`. Duplicate instants are collapsed to their last
/// value first, matching splimes, which keeps the last of equal timestamps.
const INTERPOLATE_SQL: &str = "
WITH k AS (
	SELECT t, last(v ORDER BY seq) AS v FROM knots GROUP BY t
),
g AS (
	SELECT range AS t FROM range(?::BIGINT, ?::BIGINT + 1, ?::BIGINT)
),
lo AS (
	SELECT g.t, k.t AS t0, k.v AS v0 FROM g ASOF LEFT JOIN k ON g.t >= k.t
),
hi AS (
	SELECT g.t, k.t AS t1, k.v AS v1 FROM g ASOF LEFT JOIN k ON g.t <= k.t
),
ends AS (
	SELECT
		(SELECT t FROM k ORDER BY t LIMIT 1) AS f0t, (SELECT v FROM k ORDER BY t LIMIT 1) AS f0v,
		(SELECT t FROM k ORDER BY t LIMIT 1 OFFSET 1) AS f1t, (SELECT v FROM k ORDER BY t LIMIT 1 OFFSET 1) AS f1v,
		(SELECT t FROM k ORDER BY t DESC LIMIT 1 OFFSET 1) AS l0t, (SELECT v FROM k ORDER BY t DESC LIMIT 1 OFFSET 1) AS l0v,
		(SELECT t FROM k ORDER BY t DESC LIMIT 1) AS l1t, (SELECT v FROM k ORDER BY t DESC LIMIT 1) AS l1v
),
seg AS (
	SELECT lo.t,
		CASE WHEN lo.t0 IS NULL THEN ends.f0t WHEN hi.t1 IS NULL THEN ends.l0t ELSE lo.t0 END AS a_t,
		CASE WHEN lo.t0 IS NULL THEN ends.f0v WHEN hi.t1 IS NULL THEN ends.l0v ELSE lo.v0 END AS a_v,
		CASE WHEN lo.t0 IS NULL THEN ends.f1t WHEN hi.t1 IS NULL THEN ends.l1t ELSE hi.t1 END AS b_t,
		CASE WHEN lo.t0 IS NULL THEN ends.f1v WHEN hi.t1 IS NULL THEN ends.l1v ELSE hi.v1 END AS b_v
	FROM lo JOIN hi USING (t), ends
)
SELECT CASE WHEN b_t = a_t THEN a_v
	ELSE a_v + (b_v - a_v) * (CAST(t - a_t AS DOUBLE) / CAST(b_t - a_t AS DOUBLE)) END AS v
FROM seg ORDER BY t
";

/// A database file and its connection, removed on drop (connection first).
struct Db {
	conn: Connection,
	path: PathBuf,
}

impl Drop for Db {
	fn drop(&mut self) {
		let _ = std::fs::remove_file(&self.path);
		let _ = std::fs::remove_file(self.path.with_extension("db.wal"));
	}
}

/// `DuckDB` with the knots as `DOUBLE`.
#[derive(Default)]
pub struct DuckDouble {
	db: Option<Db>,
}

impl std::fmt::Debug for DuckDouble {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("DuckDouble").field("loaded", &self.db.is_some()).finish()
	}
}

/// The `DuckDB` library version linked into this binary.
///
/// # Errors
///
/// If an in-memory database cannot be opened.
pub fn duckdb_version() -> anyhow::Result<String> {
	let conn = Connection::open_in_memory()?;
	Ok(conn.query_row("SELECT version()", [], |row| row.get::<_, String>(0))?)
}

/// Create the database file at `path`, append `rows` as `(seq, t, v)` and checkpoint.
fn load_file(path: PathBuf, rows: &[(i64, f64)]) -> anyhow::Result<Db> {
	let db = Db { conn: Connection::open(&path)?, path };
	db.conn.execute_batch("CREATE TABLE knots (seq BIGINT, t BIGINT, v DOUBLE)")?;
	let mut appender = db.conn.appender("knots")?;
	for (seq, &(t, v)) in rows.iter().enumerate() {
		appender.append_row(params![i64::try_from(seq)?, t, v])?;
	}
	appender.flush()?;
	drop(appender);
	// Durable, like a WeftDB seal: the rows reach the database file, not just the WAL.
	db.conn.execute_batch("CHECKPOINT")?;
	Ok(db)
}

/// Run the grid reconstruction over `[start, end]` every `step` nanoseconds.
fn query_grid(conn: &Connection, start: i64, end: i64, step: i64) -> anyhow::Result<Vec<f64>> {
	let mut stmt = conn.prepare_cached(INTERPOLATE_SQL)?;
	let values = stmt.query_map(params![start, end, step], |row| row.get::<_, f64>(0))?.collect::<Result<Vec<f64>, _>>()?;
	Ok(values)
}

impl Arm for DuckDouble {
	fn info(&self) -> ArmInfo {
		let version = duckdb_version().unwrap_or_else(|_| "unknown".into());
		ArmInfo { arm: "duckdb-double".into(), engine: format!("duckdb {version}"), value_type: "DOUBLE".into(), method: "linear interpolation in SQL: range() grid, two ASOF JOINs, first/last segment extended (on-disk database)".into() }
	}

	// DuckDB's calls are synchronous, so both phases run on tokio's blocking pool, as splimes does
	// behind the WeftDB arms; the hand-off is in every arm's timing alike.
	async fn load(&mut self, ds: &Dataset) -> anyhow::Result<()> {
		self.db = None;
		let rows = ds.points.iter().map(|p| {
			let t = p.timestamp.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("instant {} is outside the nanosecond range", p.timestamp))?;
			let v = p.value.to_f64().filter(|v| v.is_finite()).ok_or_else(|| anyhow::anyhow!("value {} has no finite f64 image", p.value))?;
			Ok((t, v))
		});
		let rows = rows.collect::<anyhow::Result<Vec<(i64, f64)>>>()?;
		let path = std::env::temp_dir().join(format!("weft-xbench-duck-{}-{}.db", std::process::id(), NEXT_DB.fetch_add(1, Ordering::Relaxed)));
		self.db = Some(tokio::task::spawn_blocking(move || load_file(path, &rows)).await??);
		Ok(())
	}

	async fn interpolate(&mut self, ds: &Dataset, _knots: Vec<splimes::Point>) -> anyhow::Result<Output> {
		let db = self.db.take().ok_or_else(|| anyhow::anyhow!("duckdb-double: nothing loaded"))?;
		let start = ds.start.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("grid start out of range"))?;
		let end = ds.end.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("grid end out of range"))?;
		let step = ds.resolution.step().num_nanoseconds().ok_or_else(|| anyhow::anyhow!("grid step out of range"))?;
		let (db, values) = tokio::task::spawn_blocking(move || {
			let values = query_grid(&db.conn, start, end, step);
			(db, values)
		})
		.await?;
		self.db = Some(db);
		Ok(Output::Float(values?))
	}
}
