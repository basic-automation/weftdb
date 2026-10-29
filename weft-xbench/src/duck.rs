//! The `DuckDB` arms: knots stored as `DOUBLE` or as `DECIMAL(18, s)`.
//!
//! `DuckDB` stores the knots as `(seq BIGINT, t BIGINT /* epoch ns */, v <type>)` in an on-disk
//! database file and answers in SQL. It has no interpolation function, so the interpolation query is
//! the one a `DuckDB` user would write: `range()` names the grid, one `ASOF JOIN` finds the last knot
//! at or before each instant and another the first knot at or after it, and the value is the line
//! between them. Outside the data, the first or last segment's line is extended, which is splimes'
//! `Linear` rule. Downsampling is a `GROUP BY` on the epoch-aligned bucket, the grid `weft_reduce`
//! uses.
//!
//! In the `DECIMAL` arm the knots are stored exactly, and `min` and `max` stay exact. The
//! interpolation line is computed in `DOUBLE`, because `DuckDB`'s `DECIMAL × DOUBLE` is `DOUBLE`.
//! `avg` is whatever `DuckDB` returns for a `DECIMAL` average. Both are read back as `DOUBLE`.

use std::{
	path::PathBuf, sync::atomic::{AtomicU64, Ordering}
};

use bigdecimal::{BigDecimal, ToPrimitive};
use duckdb::{params, types::Value, Connection};

use crate::{weft::Arm, ArmInfo, BucketRow, Dataset, Output};

/// Counter distinguishing database files of arms alive at once.
static NEXT_DB: AtomicU64 = AtomicU64::new(0);

/// The widest `DECIMAL` `DuckDB` stores in an `i64`.
const DECIMAL_WIDTH: u8 = 18;

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
SELECT CAST(CASE WHEN b_t = a_t THEN a_v
	ELSE a_v + (b_v - a_v) * (CAST(t - a_t AS DOUBLE) / CAST(b_t - a_t AS DOUBLE)) END AS DOUBLE) AS v
FROM seg ORDER BY t
";

/// Count, min, max and avg per epoch-aligned bucket. Parameters: the bucket width in nanoseconds,
/// twice. Every row counts, duplicates included, as in `weft_reduce::reduce`.
const DOWNSAMPLE_SQL: &str = "
SELECT (t // ?) * ? AS b, count(*) AS n,
	CAST(min(v) AS DOUBLE), CAST(max(v) AS DOUBLE), CAST(avg(v) AS DOUBLE)
FROM knots GROUP BY b ORDER BY b
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

/// The column type the knots are stored as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuckValue {
	/// `DOUBLE`.
	Double,
	/// `DECIMAL(18, s)`, with `s` the dataset's largest number of fractional digits.
	Decimal,
}

/// `DuckDB` with the knots stored as [`DuckValue`].
pub struct Duck {
	value: DuckValue,
	db: Option<Db>,
}

impl std::fmt::Debug for Duck {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Duck").field("value", &self.value).field("loaded", &self.db.is_some()).finish()
	}
}

impl Duck {
	/// `duckdb-double`.
	#[must_use]
	pub const fn double() -> Self {
		Self { value: DuckValue::Double, db: None }
	}

	/// `duckdb-decimal`.
	#[must_use]
	pub const fn decimal() -> Self {
		Self { value: DuckValue::Decimal, db: None }
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

/// The smallest scale that holds every value exactly.
fn decimal_scale(values: &[BigDecimal]) -> anyhow::Result<u8> {
	let digits = values.iter().map(|v| v.normalized().fractional_digit_count().max(0)).max().unwrap_or(0);
	u8::try_from(digits).ok().filter(|s| *s <= DECIMAL_WIDTH).ok_or_else(|| anyhow::anyhow!("values need {digits} fractional digits, more than DECIMAL({DECIMAL_WIDTH}) holds"))
}

/// `v` as a `DECIMAL(18, scale)` value, refusing one that does not fit exactly.
fn decimal_value(v: &BigDecimal, scale: u8) -> anyhow::Result<Value> {
	// `with_scale` rounds silently; a value that changes when rescaled has more digits than `scale`.
	let rescaled = v.with_scale(i64::from(scale));
	anyhow::ensure!(&rescaled == v, "value {v} has more than {scale} fractional digits");
	let (mantissa, _) = rescaled.into_bigint_and_exponent();
	let mantissa = mantissa.to_i128().filter(|m| m.unsigned_abs() < 10u128.pow(u32::from(DECIMAL_WIDTH))).ok_or_else(|| anyhow::anyhow!("value {v} does not fit DECIMAL({DECIMAL_WIDTH}, {scale})"))?;
	Ok(Value::Decimal(duckdb::types::Decimal::new(DECIMAL_WIDTH, scale, mantissa)?))
}

/// Create the database file at `path` with a `knots` table of `column_type`, append `rows` and
/// checkpoint.
fn load_file(path: PathBuf, column_type: &str, rows: Vec<(i64, Value)>) -> anyhow::Result<Db> {
	let db = Db { conn: Connection::open(&path)?, path };
	db.conn.execute_batch(&format!("CREATE TABLE knots (seq BIGINT, t BIGINT, v {column_type})"))?;
	let mut appender = db.conn.appender("knots")?;
	for (seq, (t, v)) in rows.into_iter().enumerate() {
		appender.append_row(params![i64::try_from(seq)?, t, v])?;
	}
	appender.flush()?;
	drop(appender);
	// Durable, like a WeftDB seal: the rows reach the database file, not just the WAL.
	db.conn.execute_batch("CHECKPOINT")?;
	Ok(db)
}

fn query_grid(conn: &Connection, start: i64, end: i64, step: i64) -> anyhow::Result<Vec<f64>> {
	let mut stmt = conn.prepare_cached(INTERPOLATE_SQL)?;
	let values = stmt.query_map(params![start, end, step], |row| row.get::<_, f64>(0))?.collect::<Result<Vec<f64>, _>>()?;
	Ok(values)
}

fn query_buckets(conn: &Connection, width: i64) -> anyhow::Result<Vec<BucketRow>> {
	let mut stmt = conn.prepare_cached(DOWNSAMPLE_SQL)?;
	let rows = stmt.query_map(params![width, width], |row| Ok(BucketRow { start_ns: row.get(0)?, count: row.get::<_, i64>(1)?.unsigned_abs(), min: row.get(2)?, max: row.get(3)?, avg: row.get(4)? }))?;
	Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

impl Duck {
	/// Run `f` on the loaded connection on tokio's blocking pool, as splimes runs behind the WeftDB
	/// arms, so the hand-off is in every arm's timing alike.
	async fn on_db<T: Send + 'static>(&mut self, f: impl FnOnce(&Connection) -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
		let db = self.db.take().ok_or_else(|| anyhow::anyhow!("duckdb: nothing loaded"))?;
		let (db, result) = tokio::task::spawn_blocking(move || {
			let result = f(&db.conn);
			(db, result)
		})
		.await?;
		self.db = Some(db);
		result
	}
}

impl Arm for Duck {
	fn info(&self) -> ArmInfo {
		let version = duckdb_version().unwrap_or_else(|_| "unknown".into());
		let (arm, value_type, note) = match self.value {
			DuckValue::Double => ("duckdb-double", "DOUBLE", ""),
			DuckValue::Decimal => ("duckdb-decimal", "DECIMAL(18, s)", "; stored exactly, the interpolation line computed in DOUBLE (DECIMAL × DOUBLE is DOUBLE)"),
		};
		ArmInfo { arm: arm.into(), engine: format!("duckdb {version}"), value_type: value_type.into(), method: format!("linear interpolation in SQL: range() grid, two ASOF JOINs, first/last segment extended; downsample via GROUP BY on the epoch bucket; on-disk database{note}") }
	}

	async fn load(&mut self, ds: &Dataset) -> anyhow::Result<()> {
		self.db = None;
		let timestamps = ds.points.iter().map(|p| p.timestamp.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("instant {} is outside the nanosecond range", p.timestamp))).collect::<anyhow::Result<Vec<i64>>>()?;
		let (column_type, values) = match self.value {
			DuckValue::Double => ("DOUBLE".to_string(), ds.points.iter().map(|p| p.value.to_f64().filter(|v| v.is_finite()).map(Value::Double).ok_or_else(|| anyhow::anyhow!("value {} has no finite f64 image", p.value))).collect::<anyhow::Result<Vec<_>>>()?),
			DuckValue::Decimal => {
				let values: Vec<BigDecimal> = ds.points.iter().map(|p| p.value.clone()).collect();
				let scale = decimal_scale(&values)?;
				(format!("DECIMAL({DECIMAL_WIDTH}, {scale})"), values.iter().map(|v| decimal_value(v, scale)).collect::<anyhow::Result<Vec<_>>>()?)
			}
		};
		let rows: Vec<(i64, Value)> = timestamps.into_iter().zip(values).collect();
		let path = std::env::temp_dir().join(format!("weft-xbench-duck-{}-{}.db", std::process::id(), NEXT_DB.fetch_add(1, Ordering::Relaxed)));
		self.db = Some(tokio::task::spawn_blocking(move || load_file(path, &column_type, rows)).await??);
		Ok(())
	}

	fn storage_bytes(&self) -> Option<u64> {
		let db = self.db.as_ref()?;
		let file = std::fs::metadata(&db.path).map(|m| m.len()).ok()?;
		Some(file + std::fs::metadata(db.path.with_extension("db.wal")).map_or(0, |m| m.len()))
	}

	async fn interpolate(&mut self, ds: &Dataset, _knots: Vec<splimes::Point>) -> anyhow::Result<Output> {
		let start = ds.start.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("grid start out of range"))?;
		let end = ds.end.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("grid end out of range"))?;
		let step = ds.resolution.step().num_nanoseconds().ok_or_else(|| anyhow::anyhow!("grid step out of range"))?;
		Ok(Output::Float(self.on_db(move |conn| query_grid(conn, start, end, step)).await?))
	}

	async fn downsample(&mut self, ds: &Dataset, _knots: Vec<splimes::Point>) -> anyhow::Result<Vec<BucketRow>> {
		let width = ds.bucket.step().num_nanoseconds().ok_or_else(|| anyhow::anyhow!("bucket width out of range"))?;
		self.on_db(move |conn| query_buckets(conn, width)).await
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn dec(s: &str) -> BigDecimal {
		s.parse().expect("decimal")
	}

	#[test]
	fn the_scale_is_the_most_fractional_digits_after_normalizing() {
		assert_eq!(decimal_scale(&[dec("1.50"), dec("2.125"), dec("3")]).expect("fits"), 3);
		assert_eq!(decimal_scale(&[dec("100"), dec("2E+2")]).expect("fits"), 0);
		assert!(decimal_scale(&[dec("0.0000000000000000001")]).is_err(), "19 fractional digits do not fit DECIMAL(18)");
	}

	#[test]
	fn a_decimal_value_is_its_exact_mantissa_or_refused() {
		assert_eq!(decimal_value(&dec("-1.25"), 3).expect("fits"), Value::Decimal(duckdb::types::Decimal::new(18, 3, -1250).expect("valid")));
		assert!(decimal_value(&dec("1.2345"), 3).is_err(), "rescaling would round");
		assert!(decimal_value(&dec("1000000000000000"), 3).is_err(), "19 digits do not fit DECIMAL(18, 3)");
	}
}
