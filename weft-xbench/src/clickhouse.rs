//! The `ClickHouse` arms: knots stored as `Float64` or as `Decimal64(s)`, on a server given by URL.
//!
//! `ClickHouse` is a server, so these arms talk to one over its HTTP interface on loopback, in
//! `RowBinary` both ways (the client format its own drivers use), and the client side of each call
//! is inside the timing as it is for a `ClickHouse` user. Each load creates a fresh database with
//! one `MergeTree` table `knots (seq Int64, t Int64 /* epoch ns */, v <type>) ORDER BY t`, inserted in
//! one `INSERT`, with `fsync_after_insert` and `fsync_part_directory` on so the load is durable the
//! way a WeftDB seal and `DuckDB`'s `CHECKPOINT` are.
//!
//! `ClickHouse`'s own gap filling (`ORDER BY … WITH FILL … INTERPOLATE`) fills from the previous
//! row's expression, not along a line between neighbours, so it does not compute this benchmark's
//! answer. The interpolation query is therefore the same SQL as the `DuckDB` arm's: a `numbers()`
//! grid, one `ASOF LEFT JOIN` for the last knot at or before each instant and another for the first
//! at or after it, and the line between them, with the first and last segments extended past the
//! data. `ClickHouse`'s `ASOF JOIN` needs an equality condition beside the inequality, so both sides
//! carry a constant key. Downsampling is a `GROUP BY` on the epoch-aligned bucket.
//!
//! In the `Decimal64` arm the knots are stored exactly and `min` and `max` stay exact; the line is
//! computed in `Float64`, since `ClickHouse` refuses `Decimal × Float64` without a cast, and `avg`
//! is read back as `Float64`.

use std::{
	sync::atomic::{AtomicU64, Ordering}, time::Duration
};

use bigdecimal::{BigDecimal, ToPrimitive};

use crate::{weft::Arm, ArmInfo, BucketRow, Dataset, Output};

/// Counter distinguishing databases of arms alive at once.
static NEXT_DB: AtomicU64 = AtomicU64::new(0);

/// The widest `Decimal` `ClickHouse` stores in an `Int64` (`Decimal64`).
const DECIMAL_WIDTH: u8 = 18;

/// The grid reconstruction, with `{db}` the database. Parameters: `start`, `end` (inclusive) and
/// `step`, in epoch nanoseconds. The same query as `duck::INTERPOLATE_SQL`, in `ClickHouse`'s
/// dialect: duplicate instants keep their last value (`argMax` over `seq`), as splimes does.
/// `join_use_nulls` makes the joined columns `Nullable`; every grid instant has a segment, so the
/// result is unwrapped to plain `Float64`, which `RowBinary` sends without a null flag per row.
const INTERPOLATE_SQL: &str = "
WITH
k AS (SELECT 0 AS c, t, argMax(v, seq) AS v FROM {db}.knots GROUP BY t),
g AS (SELECT 0 AS c, {start:Int64} + toInt64(number) * {step:Int64} AS t FROM numbers(intDiv({end:Int64} - {start:Int64}, {step:Int64}) + 1)),
lo AS (SELECT g.t AS t, k.t AS t0, k.v AS v0 FROM g ASOF LEFT JOIN k ON g.c = k.c AND g.t >= k.t),
hi AS (SELECT g.t AS t, k.t AS t1, k.v AS v1 FROM g ASOF LEFT JOIN k ON g.c = k.c AND g.t <= k.t),
ends AS (SELECT
	(SELECT t FROM k ORDER BY t LIMIT 1) AS f0t, (SELECT v FROM k ORDER BY t LIMIT 1) AS f0v,
	(SELECT t FROM k ORDER BY t LIMIT 1 OFFSET 1) AS f1t, (SELECT v FROM k ORDER BY t LIMIT 1 OFFSET 1) AS f1v,
	(SELECT t FROM k ORDER BY t DESC LIMIT 1 OFFSET 1) AS l0t, (SELECT v FROM k ORDER BY t DESC LIMIT 1 OFFSET 1) AS l0v,
	(SELECT t FROM k ORDER BY t DESC LIMIT 1) AS l1t, (SELECT v FROM k ORDER BY t DESC LIMIT 1) AS l1v),
seg AS (SELECT lo.t AS t,
	if(lo.t0 IS NULL, f0t, if(hi.t1 IS NULL, l0t, lo.t0)) AS a_t,
	toFloat64(if(lo.t0 IS NULL, f0v, if(hi.t1 IS NULL, l0v, lo.v0))) AS a_v,
	if(lo.t0 IS NULL, f1t, if(hi.t1 IS NULL, l1t, hi.t1)) AS b_t,
	toFloat64(if(lo.t0 IS NULL, f1v, if(hi.t1 IS NULL, l1v, hi.v1))) AS b_v
	FROM lo JOIN hi ON lo.t = hi.t CROSS JOIN ends)
SELECT assumeNotNull(if(b_t = a_t, a_v, a_v + (b_v - a_v) * (toFloat64(t - a_t) / toFloat64(b_t - a_t)))) AS v
FROM seg ORDER BY t
SETTINGS join_use_nulls = 1
FORMAT RowBinary
";

/// Count, min, max and avg per epoch-aligned bucket of `{width:Int64}` nanoseconds. Every row
/// counts, duplicates included, as in `weft_reduce::reduce`.
const DOWNSAMPLE_SQL: &str = "
SELECT intDiv(t, {width:Int64}) * {width:Int64} AS b, toUInt64(count()) AS n,
	toFloat64(min(v)), toFloat64(max(v)), toFloat64(avg(v))
FROM {db}.knots GROUP BY b ORDER BY b
FORMAT RowBinary
";

/// The column type the knots are stored as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChValue {
	/// `Float64`.
	Double,
	/// `Decimal64(s)`, with `s` the dataset's largest number of fractional digits.
	Decimal,
}

/// A `ClickHouse` HTTP endpoint.
#[derive(Clone)]
pub struct Server {
	url: String,
	agent: ureq::Agent,
}

impl std::fmt::Debug for Server {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Server").field("url", &self.url).finish_non_exhaustive()
	}
}

impl Server {
	/// The server at `url` (`http://127.0.0.1:8123`).
	#[must_use]
	pub fn new(url: &str) -> Self {
		let config = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(3600))).build();
		Self { url: url.trim_end_matches('/').to_string(), agent: ureq::Agent::new_with_config(config) }
	}

	/// Run `sql` with `params` (`{name:Type}` placeholders), sending `body` after it, and return
	/// the response body. A non-2xx answer is an error carrying the server's message.
	fn query(&self, sql: &str, params: &[(&str, String)], body: &[u8]) -> anyhow::Result<Vec<u8>> {
		let mut request = self.agent.post(&self.url);
		// With a body, the statement goes in the URL and the body is the INSERT's data.
		let payload = if body.is_empty() {
			sql.as_bytes()
		} else {
			request = request.query("query", sql);
			body
		};
		for (name, value) in params {
			request = request.query(format!("param_{name}"), value);
		}
		let mut response = request.send(payload)?;
		let status = response.status();
		let bytes = response.body_mut().with_config().limit(u64::MAX).read_to_vec()?;
		anyhow::ensure!(status.is_success(), "clickhouse {status}: {}", String::from_utf8_lossy(&bytes).trim());
		Ok(bytes)
	}

	/// The server's version.
	///
	/// # Errors
	///
	/// If the server cannot be reached.
	pub fn version(&self) -> anyhow::Result<String> {
		Ok(String::from_utf8(self.query("SELECT version() FORMAT TSV", &[], &[])?)?.trim().to_string())
	}
}

/// A loaded database, dropped from the server on drop.
struct Db {
	server: Server,
	name: String,
}

impl Drop for Db {
	fn drop(&mut self) {
		let _ = self.server.query(&format!("DROP DATABASE IF EXISTS {} SYNC", self.name), &[], &[]);
	}
}

/// `ClickHouse` with the knots stored as [`ChValue`].
pub struct ClickHouse {
	value: ChValue,
	server: Server,
	db: Option<Db>,
}

impl std::fmt::Debug for ClickHouse {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("ClickHouse").field("value", &self.value).field("server", &self.server).field("loaded", &self.db.is_some()).finish()
	}
}

impl ClickHouse {
	/// `clickhouse-double`.
	#[must_use]
	pub const fn double(server: Server) -> Self {
		Self { value: ChValue::Double, server, db: None }
	}

	/// `clickhouse-decimal`.
	#[must_use]
	pub const fn decimal(server: Server) -> Self {
		Self { value: ChValue::Decimal, server, db: None }
	}

	/// Run `f` with the loaded database on tokio's blocking pool, as the `DuckDB` arms do, so the
	/// hand-off is in every arm's timing alike.
	async fn on_db<T: Send + 'static>(&mut self, f: impl FnOnce(&Server, &str) -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
		let db = self.db.take().ok_or_else(|| anyhow::anyhow!("clickhouse: nothing loaded"))?;
		let (db, result) = tokio::task::spawn_blocking(move || {
			let result = f(&db.server, &db.name);
			(db, result)
		})
		.await?;
		self.db = Some(db);
		result
	}
}

/// The smallest scale that holds every value exactly.
fn decimal_scale(values: &[BigDecimal]) -> anyhow::Result<u8> {
	let digits = values.iter().map(|v| v.normalized().fractional_digit_count().max(0)).max().unwrap_or(0);
	u8::try_from(digits).ok().filter(|s| *s <= DECIMAL_WIDTH).ok_or_else(|| anyhow::anyhow!("values need {digits} fractional digits, more than Decimal64 holds"))
}

/// `v`'s exact `Decimal64(scale)` mantissa, refusing one that does not fit.
fn decimal_mantissa(v: &BigDecimal, scale: u8) -> anyhow::Result<i64> {
	// `with_scale` rounds silently; a value that changes when rescaled has more digits than `scale`.
	let rescaled = v.with_scale(i64::from(scale));
	anyhow::ensure!(&rescaled == v, "value {v} has more than {scale} fractional digits");
	let (mantissa, _) = rescaled.into_bigint_and_exponent();
	mantissa.to_i64().filter(|m| m.unsigned_abs() < 10u64.pow(u32::from(DECIMAL_WIDTH))).ok_or_else(|| anyhow::anyhow!("value {v} does not fit Decimal64({scale})"))
}

/// The `RowBinary` rows `(seq, t, v)`, with `v` already encoded as 8 little-endian bytes.
fn row_binary(timestamps: &[i64], values: &[[u8; 8]]) -> anyhow::Result<Vec<u8>> {
	let mut out = Vec::with_capacity(timestamps.len() * 24);
	for (seq, (t, v)) in timestamps.iter().zip(values).enumerate() {
		out.extend_from_slice(&i64::try_from(seq)?.to_le_bytes());
		out.extend_from_slice(&t.to_le_bytes());
		out.extend_from_slice(v);
	}
	Ok(out)
}

/// Little-endian 8-byte words from a `RowBinary` answer.
fn words(bytes: &[u8]) -> impl Iterator<Item = [u8; 8]> + '_ {
	bytes.chunks_exact(8).map(|c| <[u8; 8]>::try_from(c).unwrap_or_default())
}

fn parse_grid(bytes: &[u8]) -> anyhow::Result<Vec<f64>> {
	anyhow::ensure!(bytes.len() % 8 == 0, "clickhouse: a grid answer of {} bytes is not whole Float64s", bytes.len());
	Ok(words(bytes).map(f64::from_le_bytes).collect())
}

fn parse_buckets(bytes: &[u8]) -> anyhow::Result<Vec<BucketRow>> {
	anyhow::ensure!(bytes.len() % 40 == 0, "clickhouse: a bucket answer of {} bytes is not whole rows", bytes.len());
	Ok(bytes.chunks_exact(40)
		.map(|row| {
			let mut w = words(row);
			let mut next = || w.next().unwrap_or_default();
			BucketRow { start_ns: i64::from_le_bytes(next()), count: u64::from_le_bytes(next()), min: f64::from_le_bytes(next()), max: f64::from_le_bytes(next()), avg: f64::from_le_bytes(next()) }
		})
		.collect())
}

fn load_db(server: Server, name: String, column_type: &str, body: &[u8]) -> anyhow::Result<Db> {
	server.query(&format!("CREATE DATABASE {name}"), &[], &[])?;
	// From here on, dropping `db` removes the database whatever fails next.
	let db = Db { server, name };
	db.server.query(&format!("CREATE TABLE {}.knots (seq Int64, t Int64, v {column_type}) ENGINE = MergeTree ORDER BY t SETTINGS fsync_after_insert = 1, fsync_part_directory = 1", db.name), &[], &[])?;
	db.server.query(&format!("INSERT INTO {}.knots FORMAT RowBinary", db.name), &[], body)?;
	Ok(db)
}

impl Arm for ClickHouse {
	fn info(&self) -> ArmInfo {
		let version = self.server.version().unwrap_or_else(|_| "unknown".into());
		let (arm, value_type, note) = match self.value {
			ChValue::Double => ("clickhouse-double", "Float64", ""),
			ChValue::Decimal => ("clickhouse-decimal", "Decimal64(s)", "; stored exactly, the interpolation line computed in Float64"),
		};
		ArmInfo { arm: arm.into(), engine: format!("clickhouse {version}"), value_type: value_type.into(), method: format!("linear interpolation in SQL: numbers() grid, two ASOF LEFT JOINs, first/last segment extended; downsample via GROUP BY on the epoch bucket; MergeTree ORDER BY t, one fsync'd INSERT; server over loopback HTTP, RowBinary both ways{note}") }
	}

	async fn load(&mut self, ds: &Dataset) -> anyhow::Result<()> {
		self.db = None;
		let timestamps = ds.points.iter().map(|p| p.timestamp.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("instant {} is outside the nanosecond range", p.timestamp))).collect::<anyhow::Result<Vec<i64>>>()?;
		let (column_type, values) = match self.value {
			ChValue::Double => ("Float64".to_string(), ds.points.iter().map(|p| p.value.to_f64().filter(|v| v.is_finite()).map(f64::to_le_bytes).ok_or_else(|| anyhow::anyhow!("value {} has no finite f64 image", p.value))).collect::<anyhow::Result<Vec<_>>>()?),
			ChValue::Decimal => {
				let values: Vec<BigDecimal> = ds.points.iter().map(|p| p.value.clone()).collect();
				let scale = decimal_scale(&values)?;
				(format!("Decimal64({scale})"), values.iter().map(|v| decimal_mantissa(v, scale).map(i64::to_le_bytes)).collect::<anyhow::Result<Vec<_>>>()?)
			}
		};
		let body = row_binary(&timestamps, &values)?;
		let name = format!("weft_xbench_{}_{}", std::process::id(), NEXT_DB.fetch_add(1, Ordering::Relaxed));
		let server = self.server.clone();
		self.db = Some(tokio::task::spawn_blocking(move || load_db(server, name, &column_type, &body)).await??);
		Ok(())
	}

	fn storage_bytes(&self) -> Option<u64> {
		let db = self.db.as_ref()?;
		let bytes = db.server.query("SELECT sum(bytes_on_disk) FROM system.parts WHERE database = {db:String} AND table = 'knots' AND active FORMAT TSV", &[("db", db.name.clone())], &[]).ok()?;
		String::from_utf8(bytes).ok()?.trim().parse().ok()
	}

	async fn interpolate(&mut self, ds: &Dataset, _knots: Vec<splimes::Point>) -> anyhow::Result<Output> {
		let start = ds.start.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("grid start out of range"))?;
		let end = ds.end.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("grid end out of range"))?;
		let step = ds.resolution.step().num_nanoseconds().ok_or_else(|| anyhow::anyhow!("grid step out of range"))?;
		let bytes = self.on_db(move |server, db| server.query(&INTERPOLATE_SQL.replace("{db}", db), &[("start", start.to_string()), ("end", end.to_string()), ("step", step.to_string())], &[])).await?;
		Ok(Output::Float(parse_grid(&bytes)?))
	}

	async fn downsample(&mut self, ds: &Dataset, _knots: Vec<splimes::Point>) -> anyhow::Result<Vec<BucketRow>> {
		let width = ds.bucket.step().num_nanoseconds().ok_or_else(|| anyhow::anyhow!("bucket width out of range"))?;
		let bytes = self.on_db(move |server, db| server.query(&DOWNSAMPLE_SQL.replace("{db}", db), &[("width", width.to_string())], &[])).await?;
		parse_buckets(&bytes)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn dec(s: &str) -> BigDecimal {
		s.parse().expect("decimal")
	}

	#[test]
	fn a_decimal_mantissa_is_exact_or_refused() {
		assert_eq!(decimal_scale(&[dec("1.50"), dec("2.125"), dec("3")]).expect("fits"), 3);
		assert_eq!(decimal_mantissa(&dec("-1.25"), 3).expect("fits"), -1250);
		assert!(decimal_mantissa(&dec("1.2345"), 3).is_err(), "rescaling would round");
		assert!(decimal_mantissa(&dec("1000000000000000"), 3).is_err(), "19 digits do not fit Decimal64(3)");
	}

	#[test]
	fn row_binary_round_trips_through_the_parsers() {
		let body = row_binary(&[7, -3], &[1.5f64.to_le_bytes(), (-2i64).to_le_bytes()]).expect("encodes");
		assert_eq!(body.len(), 48);
		assert_eq!(i64::from_le_bytes(body[8..16].try_into().expect("8 bytes")), 7);
		assert_eq!(parse_grid(&body[16..24]).expect("parses"), vec![1.5]);
		assert!(parse_grid(&body[..5]).is_err());
		let row: Vec<u8> = [60i64.to_le_bytes(), 2u64.to_le_bytes(), 0.5f64.to_le_bytes(), 1.5f64.to_le_bytes(), 1.0f64.to_le_bytes()].concat();
		assert_eq!(parse_buckets(&row).expect("parses"), vec![BucketRow { start_ns: 60, count: 2, min: 0.5, max: 1.5, avg: 1.0 }]);
	}
}
