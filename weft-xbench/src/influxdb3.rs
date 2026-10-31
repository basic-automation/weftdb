//! The `InfluxDB` 3 Core arm: knots stored as a `float` field, on a server given by URL.
//!
//! `InfluxDB` 3 has no decimal type (fields are float, integer, unsigned, string or boolean), so there
//! is one arm, `influxdb3-double`, and the report says there is no exact-decimal counterpart.
//!
//! Writes are line protocol to `/api/v3/write_lp` with nanosecond precision. With `no_sync` left at
//! its default (`false`) the server answers only once the write-ahead log file holding the rows has
//! been persisted to its object store, a local directory on the reference host: durable, as a WeftDB
//! seal is. Each load gets a fresh database, dropped afterwards. Reads are SQL to `/api/v3/query_sql`
//! with `format=parquet`, columnar like the other engines' binary paths, decoded with the `parquet`
//! crate already in the workspace.
//!
//! The query engine is `DataFusion`, which has no `ASOF JOIN`, and `InfluxDB`'s own
//! `date_bin_gapfill` with `interpolate()` fills between bucket aggregates, not between the knots. The
//! interpolation therefore merges the grid with the knots into one ordered stream and reads, for
//! every grid row, the last knot at or before it and the first after it (running `max` and `min` of
//! the knot instants, then their values by equality join); the first and last segments stand in past the data. Downsampling
//! is the native `date_bin`.
//!
//! A series and a timestamp are `InfluxDB`'s key, so a second write at the same instant replaces the
//! first. For interpolation that is splimes' rule (the last value wins); for downsampling it means a
//! duplicated instant counts once where `weft_reduce` counts every row. The benchmark's datasets have
//! no duplicate instants; the report states the difference.

use std::{
	fmt::Write as _, sync::atomic::{AtomicU64, Ordering}, time::Duration
};

use arrow::array::{Array, Float64Array, Int64Array};
use bigdecimal::ToPrimitive;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

use crate::{weft::Arm, ArmInfo, BucketRow, Dataset, Output};

/// Counter distinguishing databases of arms alive at once.
static NEXT_DB: AtomicU64 = AtomicU64::new(0);

/// The measurement every load writes its knots to.
const TABLE: &str = "knots";

/// The largest write body sent at once. The server refuses a request over 10 MiB (its default
/// `--max-http-request-size`), and the larger datasets' line protocol is several times that.
const MAX_WRITE_BYTES: usize = 8 << 20;

/// The grid reconstruction, with `{start}`, `{end}` (inclusive) and `{step}` in epoch nanoseconds.
/// `o` orders a knot before a grid row at the same instant, so the knot counts as "at or before".
/// Knot instants only increase, so the last knot at or before a row is a running `max` of the knot
/// instants, and the first after it a running `min` over the stream reversed; their values come
/// back by equality joins on the instant, which `InfluxDB` keeps unique. Both windows are linear.
/// `last_value`/`first_value … IGNORE NULLS` gave the same answers but re-scanned their frames:
/// 6 min 25 s at the `medium` scale against 0.79 s for this.
const INTERPOLATE_SQL: &str = "
WITH
k AS (SELECT CAST(time AS BIGINT) t, v FROM knots),
e AS (SELECT
	(SELECT t FROM k ORDER BY t LIMIT 1) f0t, (SELECT v FROM k ORDER BY t LIMIT 1) f0v,
	(SELECT t FROM k ORDER BY t LIMIT 1 OFFSET 1) f1t, (SELECT v FROM k ORDER BY t LIMIT 1 OFFSET 1) f1v,
	(SELECT t FROM k ORDER BY t DESC LIMIT 1 OFFSET 1) l0t, (SELECT v FROM k ORDER BY t DESC LIMIT 1 OFFSET 1) l0v,
	(SELECT t FROM k ORDER BY t DESC LIMIT 1) l1t, (SELECT v FROM k ORDER BY t DESC LIMIT 1) l1v),
u AS (SELECT t, 0 o, t kt FROM k UNION ALL SELECT value t, 1 o, CAST(NULL AS BIGINT) kt FROM generate_series({start}, {end}, {step})),
w AS (SELECT t, o,
	max(kt) OVER (ORDER BY t, o ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) lt,
	min(kt) OVER (ORDER BY t DESC, o DESC ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) nt
	FROM u),
n AS (SELECT w.t, w.lt, kl.v lv, w.nt, kn.v nv FROM w LEFT JOIN k kl ON kl.t = w.lt LEFT JOIN k kn ON kn.t = w.nt WHERE w.o = 1),
s AS (SELECT n.t,
	CASE WHEN lt IS NULL THEN f0t WHEN nt IS NULL THEN l0t ELSE lt END a_t,
	CASE WHEN lt IS NULL THEN f0v WHEN nt IS NULL THEN l0v ELSE lv END a_v,
	CASE WHEN lt IS NULL THEN f1t WHEN nt IS NULL THEN l1t ELSE nt END b_t,
	CASE WHEN lt IS NULL THEN f1v WHEN nt IS NULL THEN l1v ELSE nv END b_v
	FROM e CROSS JOIN n)
SELECT CASE WHEN b_t = a_t OR t = a_t THEN a_v ELSE a_v + (b_v - a_v) * (CAST(t - a_t AS DOUBLE) / CAST(b_t - a_t AS DOUBLE)) END v
FROM s ORDER BY t
";

/// Count, min, max and avg per epoch-aligned bucket of `{width}` nanoseconds.
const DOWNSAMPLE_SQL: &str = "SELECT CAST(date_bin(INTERVAL '{width} nanoseconds', time, TIMESTAMP '1970-01-01T00:00:00Z') AS BIGINT) b, CAST(count(*) AS BIGINT) n, min(v) mn, max(v) mx, avg(v) av FROM knots GROUP BY b ORDER BY b";

/// An `InfluxDB` 3 HTTP endpoint.
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
	/// The server at `url` (`http://127.0.0.1:8181`).
	#[must_use]
	pub fn new(url: &str) -> Self {
		let config = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(3600))).build();
		Self { url: url.trim_end_matches('/').to_string(), agent: ureq::Agent::new_with_config(config) }
	}

	/// The response body of a request, or an error carrying the server's message.
	fn checked(response: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> anyhow::Result<Vec<u8>> {
		let mut response = response?;
		let status = response.status();
		let bytes = response.body_mut().with_config().limit(u64::MAX).read_to_vec()?;
		anyhow::ensure!(status.is_success(), "influxdb3 {status}: {}", String::from_utf8_lossy(&bytes).trim());
		Ok(bytes)
	}

	/// The product and version the server reports, e.g. `InfluxDB 3 Core 3.12.2`.
	///
	/// # Errors
	///
	/// If the server cannot be reached.
	pub fn version(&self) -> anyhow::Result<String> {
		let ping: serde_json::Value = serde_json::from_slice(&Self::checked(self.agent.get(&format!("{}/ping", self.url)).call())?)?;
		Ok(format!("{} {}", ping.get("product_name").and_then(serde_json::Value::as_str).unwrap_or("InfluxDB 3"), ping.get("version").and_then(serde_json::Value::as_str).unwrap_or("unknown")))
	}

	fn create_db(&self, db: &str) -> anyhow::Result<()> {
		Self::checked(self.agent.post(&format!("{}/api/v3/configure/database", self.url)).header("Content-Type", "application/json").send(&serde_json::to_vec(&serde_json::json!({ "db": db }))?[..]))?;
		Ok(())
	}

	fn drop_db(&self, db: &str) -> anyhow::Result<()> {
		Self::checked(self.agent.delete(&format!("{}/api/v3/configure/database", self.url)).query("db", db).call())?;
		Ok(())
	}

	fn write(&self, db: &str, body: &[u8]) -> anyhow::Result<()> {
		Self::checked(self.agent.post(&format!("{}/api/v3/write_lp", self.url)).query("db", db).query("precision", "nanosecond").send(body))?;
		Ok(())
	}

	/// `sql` against `db`, as the Parquet file the server sends.
	fn query(&self, db: &str, sql: &str) -> anyhow::Result<bytes::Bytes> {
		Ok(bytes::Bytes::from(Self::checked(self.agent.post(&format!("{}/api/v3/query_sql", self.url)).header("Content-Type", "application/json").send(&serde_json::to_vec(&serde_json::json!({ "db": db, "q": sql, "format": "parquet" }))?[..]))?))
	}
}

/// A loaded database, dropped from the server on drop.
struct Db {
	server: Server,
	name: String,
}

impl Drop for Db {
	fn drop(&mut self) {
		let _ = self.server.drop_db(&self.name);
	}
}

/// `InfluxDB` 3 Core with the knots stored as a `float` field.
pub struct Influx {
	server: Server,
	db: Option<Db>,
}

impl std::fmt::Debug for Influx {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Influx").field("server", &self.server).field("loaded", &self.db.is_some()).finish()
	}
}

impl Influx {
	/// `influxdb3-double`.
	#[must_use]
	pub const fn double(server: Server) -> Self {
		Self { server, db: None }
	}

	/// Run `f` with the loaded database on tokio's blocking pool, as the `DuckDB` arms do, so the
	/// hand-off is in every arm's timing alike.
	async fn on_db<T: Send + 'static>(&mut self, f: impl FnOnce(&Server, &str) -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
		let db = self.db.take().ok_or_else(|| anyhow::anyhow!("influxdb3: nothing loaded"))?;
		let (db, result) = tokio::task::spawn_blocking(move || {
			let result = f(&db.server, &db.name);
			(db, result)
		})
		.await?;
		self.db = Some(db);
		result
	}
}

/// The line-protocol body: one `knots seq=<i>i,v=<f> <ns>` line per knot.
fn line_protocol(timestamps: &[i64], values: &[f64]) -> anyhow::Result<String> {
	let mut out = String::with_capacity(timestamps.len() * 48);
	for (seq, (t, v)) in timestamps.iter().zip(values).enumerate() {
		// `{v:?}` is the shortest form that reads back to the same `f64`.
		writeln!(out, "{TABLE} seq={seq}i,v={v:?} {t}")?;
	}
	Ok(out)
}

/// Every record batch of a Parquet answer.
fn batches(parquet: bytes::Bytes) -> anyhow::Result<Vec<arrow::record_batch::RecordBatch>> {
	if parquet.is_empty() {
		return Ok(Vec::new());
	}
	Ok(ParquetRecordBatchReaderBuilder::try_new(parquet)?.build()?.collect::<Result<Vec<_>, _>>()?)
}

fn column<'a, T: 'static>(batch: &'a arrow::record_batch::RecordBatch, name: &str) -> anyhow::Result<&'a T> {
	batch.column_by_name(name).and_then(|c| c.as_any().downcast_ref::<T>()).ok_or_else(|| anyhow::anyhow!("influxdb3: column {name} missing or of an unexpected type"))
}

fn parse_grid(parquet: bytes::Bytes) -> anyhow::Result<Vec<f64>> {
	let mut out = Vec::new();
	for batch in batches(parquet)? {
		let v = column::<Float64Array>(&batch, "v")?;
		out.extend((0..v.len()).map(|i| if v.is_null(i) { f64::NAN } else { v.value(i) }));
	}
	Ok(out)
}

fn parse_buckets(parquet: bytes::Bytes) -> anyhow::Result<Vec<BucketRow>> {
	let mut out = Vec::new();
	for batch in batches(parquet)? {
		let (b, n) = (column::<Int64Array>(&batch, "b")?, column::<Int64Array>(&batch, "n")?);
		let (mn, mx, av) = (column::<Float64Array>(&batch, "mn")?, column::<Float64Array>(&batch, "mx")?, column::<Float64Array>(&batch, "av")?);
		out.extend((0..b.len()).map(|i| BucketRow { start_ns: b.value(i), count: n.value(i).unsigned_abs(), min: mn.value(i), max: mx.value(i), avg: av.value(i) }));
	}
	Ok(out)
}

/// `body` cut at line ends into pieces of at most `max` bytes (a single longer line stands alone).
fn chunks(body: &str, max: usize) -> Vec<&str> {
	let mut out = Vec::new();
	let mut rest = body;
	while rest.len() > max {
		let cut = rest[..max].rfind('\n').map_or_else(|| rest.find('\n').map_or(rest.len(), |i| i + 1), |i| i + 1);
		out.push(&rest[..cut]);
		rest = &rest[cut..];
	}
	if !rest.is_empty() {
		out.push(rest);
	}
	out
}

fn load_db(server: Server, name: String, body: &str) -> anyhow::Result<Db> {
	server.create_db(&name)?;
	// From here on, dropping `db` removes the database whatever fails next.
	let db = Db { server, name };
	// Every write is answered at the server's next WAL flush (once a second by default), so the
	// pieces go concurrently, as a bulk loader's batches would, and share that flush instead of
	// waiting for one each in turn.
	std::thread::scope(|scope| {
		let writes: Vec<_> = chunks(body, MAX_WRITE_BYTES).into_iter().map(|piece| scope.spawn(|| db.server.write(&db.name, piece.as_bytes()))).collect();
		writes.into_iter().try_for_each(|w| w.join().map_err(|_| anyhow::anyhow!("influxdb3: a write thread panicked"))?)
	})?;
	Ok(db)
}

impl Arm for Influx {
	fn info(&self) -> ArmInfo {
		let version = self.server.version().unwrap_or_else(|_| "unknown".into());
		ArmInfo { arm: "influxdb3-double".into(), engine: format!("influxdb3 {version}"), value_type: "float field".into(), method: "linear interpolation in SQL: generate_series() grid merged with the knots, running max/min of the knot instants joined back to their values, first/last segment extended; downsample via date_bin; line-protocol writes of at most 8 MiB sent concurrently, each answered once the WAL file is persisted (no_sync=false); SQL over HTTP read as Parquet, on loopback".into() }
	}

	fn unload(&mut self) {
		self.db = None;
	}

	async fn load(&mut self, ds: &Dataset) -> anyhow::Result<()> {
		self.db = None;
		let timestamps = ds.points.iter().map(|p| p.timestamp.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("instant {} is outside the nanosecond range", p.timestamp))).collect::<anyhow::Result<Vec<i64>>>()?;
		let values = ds.points.iter().map(|p| p.value.to_f64().filter(|v| v.is_finite()).ok_or_else(|| anyhow::anyhow!("value {} has no finite f64 image", p.value))).collect::<anyhow::Result<Vec<f64>>>()?;
		let body = line_protocol(&timestamps, &values)?;
		let name = format!("weft_xbench_{}_{}", std::process::id(), NEXT_DB.fetch_add(1, Ordering::Relaxed));
		let server = self.server.clone();
		self.db = Some(tokio::task::spawn_blocking(move || load_db(server, name, &body)).await??);
		Ok(())
	}

	/// Not measured: the rows sit in the write-ahead log until the server snapshots them to Parquet
	/// on its own schedule, and the server reports neither size for one database.
	fn storage_bytes(&self) -> Option<u64> {
		None
	}

	async fn interpolate(&mut self, ds: &Dataset, _knots: Vec<splimes::Point>) -> anyhow::Result<Output> {
		let start = ds.start.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("grid start out of range"))?;
		let end = ds.end.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("grid end out of range"))?;
		let step = ds.resolution.step().num_nanoseconds().ok_or_else(|| anyhow::anyhow!("grid step out of range"))?;
		let sql = INTERPOLATE_SQL.replace("{start}", &start.to_string()).replace("{end}", &end.to_string()).replace("{step}", &step.to_string());
		Ok(Output::Float(self.on_db(move |server, db| parse_grid(server.query(db, &sql)?)).await?))
	}

	async fn downsample(&mut self, ds: &Dataset, _knots: Vec<splimes::Point>) -> anyhow::Result<Vec<BucketRow>> {
		let width = ds.bucket.step().num_nanoseconds().ok_or_else(|| anyhow::anyhow!("bucket width out of range"))?;
		let sql = DOWNSAMPLE_SQL.replace("{width}", &width.to_string());
		self.on_db(move |server, db| parse_buckets(server.query(db, &sql)?)).await
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_body_is_cut_only_at_line_ends_and_under_the_cap() {
		let body = "aaaa\nbb\ncccccc\nd\n";
		let pieces = chunks(body, 8);
		assert_eq!(pieces.concat(), body);
		assert!(pieces.iter().all(|p| p.ends_with('\n')));
		assert_eq!(pieces, ["aaaa\nbb\n", "cccccc\n", "d\n"]);
		assert_eq!(chunks("a-very-long-line\nx\n", 4), ["a-very-long-line\n", "x\n"], "a line longer than the cap goes alone");
	}

	#[test]
	fn line_protocol_is_one_line_per_knot_with_exact_floats() {
		assert_eq!(line_protocol(&[10, 20], &[1.5, 0.1]).expect("formats"), "knots seq=0i,v=1.5 10\nknots seq=1i,v=0.1 20\n");
	}
}
