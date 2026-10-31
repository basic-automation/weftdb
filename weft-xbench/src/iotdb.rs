//! The Apache `IoTDB` arm: knots stored as a `DOUBLE` field of a table-model table, on a server given
//! by the URL of its REST service.
//!
//! `IoTDB` has no decimal type (its numeric types are `INT32`, `INT64`, `FLOAT` and `DOUBLE`), so there
//! is one arm, `iotdb-double`, and the report says there is no exact-decimal counterpart.
//!
//! The arm speaks the table-model REST API (`/rest/table/v1`), the HTTP path `IoTDB` ships; its native
//! clients speak Thrift, which has no Rust client. Each load creates a fresh database holding one
//! table `knots (v DOUBLE FIELD)` with no tags, one device, and sends the rows in time order as
//! `insertTablet` requests of [`ROWS_PER_INSERT`] rows. In time order because `IoTDB` keeps rows that
//! arrive behind ones already written in separate "unsequence" files that every query must merge.
//! The reference server runs with `wal_mode=SYNC`, so a write is answered once its WAL entry is on
//! disk, and with `timestamp_precision=ns`, which must be set before the server first starts.
//!
//! The interpolation is the `QuestDB` arm's shape in `IoTDB`'s SQL: `lead` pairs each knot with the
//! next into one row per segment, keyed by its start; `date_bin_gapfill`, with the grid's start as
//! its origin and the grid's span as its `WHERE` range, emits one row per grid instant (`IoTDB` has no
//! series generator and no `VALUES`); an `ASOF LEFT JOIN` from the grid then finds, for each instant,
//! the segment that starts at the last knot at or before it. Past the last knot that is the last
//! segment, extended, which is splimes' rule; before the first knot the first segment stands in.
//! Downsampling is the native `date_bin`, which is epoch-aligned like `weft_reduce`'s buckets.
//!
//! A device and a timestamp are `IoTDB`'s key, so a second write at the same instant replaces the
//! first. For interpolation that is splimes' rule (the last value wins); for downsampling a
//! duplicated instant counts once where `weft_reduce` counts every row. The benchmark's datasets
//! have no duplicate instants; the report states the difference.

use std::{
	sync::atomic::{AtomicU64, Ordering}, time::Duration
};

use bigdecimal::ToPrimitive;

use crate::{weft::Arm, ArmInfo, BucketRow, Dataset, Output};

/// Counter distinguishing databases of arms alive at once.
static NEXT_DB: AtomicU64 = AtomicU64::new(0);

/// The table every load writes its knots to.
const TABLE: &str = "knots";

/// Rows per `insertTablet` request: about 3 MB of JSON.
const ROWS_PER_INSERT: usize = 100_000;

/// The user the reference server's REST service accepts; `root`/`root` is `IoTDB`'s default.
const USER: (&str, &str) = ("root", "root");

/// The grid reconstruction, with `{start}`, `{end}` (inclusive) and `{step}` in epoch nanoseconds,
/// and `{lo}`/`{hi}` the grid's span widened by a step each way. See the module notes for the shape.
/// `date_bin_gapfill` left out the bucket at `{end}` when no knot fell in it, whether bounded by
/// `<= end` or `< end + step`, and `<= end + step` emitted one past it; the grid is therefore filled
/// over the widened span and cut back to `[start, end]`. Times are cast to `INT64` for the arithmetic.
const INTERPOLATE_SQL: &str = "
WITH
s AS (SELECT time st, v sv, lead(time) OVER (ORDER BY time) et, lead(v) OVER (ORDER BY time) ev FROM knots),
seg AS (SELECT st, CAST(st AS INT64) sl, sv, CAST(et AS INT64) el, ev FROM s WHERE et IS NOT NULL),
f AS (SELECT sl fs, sv fsv, el fe, ev fev FROM seg ORDER BY st LIMIT 1),
g AS (SELECT gt FROM (SELECT date_bin_gapfill({step}ns, time, {start}) gt FROM knots WHERE time >= {lo} AND time <= {hi} GROUP BY 1) WHERE gt >= {start} AND gt <= {end}),
j AS (SELECT g.gt, CAST(g.gt AS INT64) gl, seg.sl, seg.sv, seg.el, seg.ev FROM g ASOF LEFT JOIN seg ON g.gt >= seg.st)
SELECT CASE
	WHEN sl IS NULL THEN fsv + (fev - fsv) * (CAST(gl - fs AS DOUBLE) / CAST(fe - fs AS DOUBLE))
	ELSE sv + (ev - sv) * (CAST(gl - sl AS DOUBLE) / CAST(el - sl AS DOUBLE))
END v
FROM j CROSS JOIN f ORDER BY gt
";

/// Count, min, max and avg per epoch-aligned bucket of `{width}` nanoseconds.
const DOWNSAMPLE_SQL: &str = "SELECT CAST(date_bin({width}ns, time) AS INT64) b, count(*) n, min(v) mn, max(v) mx, avg(v) av FROM knots GROUP BY 1 ORDER BY 1";

/// An `IoTDB` REST service.
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
	/// The REST service at `url` (`http://127.0.0.1:18080`).
	#[must_use]
	pub fn new(url: &str) -> Self {
		let config = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(3600))).build();
		Self { url: url.trim_end_matches('/').to_string(), agent: ureq::Agent::new_with_config(config) }
	}

	/// POST `body` to the table API's `endpoint` and return the JSON answer; a failure status, in
	/// HTTP or in the answer's `code`, is an error carrying the server's message.
	fn post(&self, endpoint: &str, body: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
		let auth = base64_basic(USER.0, USER.1);
		let mut response = self.agent.post(&format!("{}/rest/table/v1/{endpoint}", self.url)).header("Authorization", &auth).header("Content-Type", "application/json").send(&serde_json::to_vec(body)?[..])?;
		let status = response.status();
		let bytes = response.body_mut().with_config().limit(u64::MAX).read_to_vec()?;
		let answer: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("iotdb {status}: {}", String::from_utf8_lossy(&bytes).trim()))?;
		let code = answer.get("code").and_then(serde_json::Value::as_i64);
		anyhow::ensure!(status.is_success() && code.is_none_or(|c| c == 200), "iotdb {status}: {}", answer.get("message").and_then(serde_json::Value::as_str).unwrap_or("no message"));
		Ok(answer)
	}

	fn execute(&self, db: Option<&str>, sql: &str) -> anyhow::Result<()> {
		self.post("nonQuery", &serde_json::json!({ "database": db, "sql": sql }))?;
		Ok(())
	}

	/// The rows of `sql` against `db`, each an array of cells.
	fn query(&self, db: &str, sql: &str, row_limit: usize) -> anyhow::Result<Vec<serde_json::Value>> {
		let answer = self.post("query", &serde_json::json!({ "database": db, "sql": sql, "row_limit": row_limit }))?;
		Ok(answer.get("values").and_then(serde_json::Value::as_array).cloned().unwrap_or_default())
	}

	/// The version the server reports, e.g. `2.0.11`.
	///
	/// # Errors
	///
	/// If the server cannot be reached.
	pub fn version(&self) -> anyhow::Result<String> {
		let rows = self.query("information_schema", "SHOW VERSION", 1)?;
		rows.first().and_then(|r| r.get(0)).and_then(serde_json::Value::as_str).map(str::to_string).ok_or_else(|| anyhow::anyhow!("iotdb: SHOW VERSION returned no version"))
	}
}

/// An HTTP Basic `Authorization` value.
fn base64_basic(user: &str, password: &str) -> String {
	const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
	let input = format!("{user}:{password}");
	let mut out = String::from("Basic ");
	for chunk in input.as_bytes().chunks(3) {
		let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
		for i in 0..4 {
			out.push(if i <= chunk.len() { char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]) } else { '=' });
		}
	}
	out
}

/// A loaded database, dropped from the server on drop.
struct Db {
	server: Server,
	name: String,
}

impl Drop for Db {
	fn drop(&mut self) {
		let _ = self.server.execute(None, &format!("DROP DATABASE IF EXISTS {}", self.name));
	}
}

/// Apache `IoTDB` with the knots stored as a `DOUBLE` field.
pub struct IoTdb {
	server: Server,
	db: Option<Db>,
}

impl std::fmt::Debug for IoTdb {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("IoTdb").field("server", &self.server).field("loaded", &self.db.is_some()).finish()
	}
}

impl IoTdb {
	/// `iotdb-double`.
	#[must_use]
	pub const fn double(server: Server) -> Self {
		Self { server, db: None }
	}

	/// Run `f` with the loaded database on tokio's blocking pool, as the `DuckDB` arms do, so the
	/// hand-off is in every arm's timing alike.
	async fn on_db<T: Send + 'static>(&mut self, f: impl FnOnce(&Server, &str) -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
		let db = self.db.take().ok_or_else(|| anyhow::anyhow!("iotdb: nothing loaded"))?;
		let (db, result) = tokio::task::spawn_blocking(move || {
			let result = f(&db.server, &db.name);
			(db, result)
		})
		.await?;
		self.db = Some(db);
		result
	}
}

fn load_db(server: Server, name: String, timestamps: &[i64], values: &[f64]) -> anyhow::Result<Db> {
	server.execute(None, &format!("CREATE DATABASE {name}"))?;
	// From here on, dropping `db` removes the database whatever fails next.
	let db = Db { server, name };
	db.server.execute(Some(&db.name), &format!("CREATE TABLE {TABLE} (v DOUBLE FIELD)"))?;
	for (t, v) in timestamps.chunks(ROWS_PER_INSERT).zip(values.chunks(ROWS_PER_INSERT)) {
		// `serde_json` writes the shortest form that reads back to the same `f64`.
		let rows: Vec<[f64; 1]> = v.iter().map(|v| [*v]).collect();
		db.server.post("insertTablet", &serde_json::json!({ "database": db.name, "table": TABLE, "column_names": ["v"], "column_categories": ["FIELD"], "data_types": ["DOUBLE"], "timestamps": t, "values": rows }))?;
	}
	Ok(db)
}

fn f64_cell(cell: Option<&serde_json::Value>) -> anyhow::Result<f64> {
	match cell {
		Some(serde_json::Value::Null) => Ok(f64::NAN),
		Some(c) => c.as_f64().ok_or_else(|| anyhow::anyhow!("iotdb: expected a number, got {c}")),
		None => anyhow::bail!("iotdb: a row is short of columns"),
	}
}

fn i64_cell(cell: Option<&serde_json::Value>) -> anyhow::Result<i64> {
	cell.and_then(serde_json::Value::as_i64).ok_or_else(|| anyhow::anyhow!("iotdb: expected an integer, got {cell:?}"))
}

impl Arm for IoTdb {
	fn info(&self) -> ArmInfo {
		let version = self.server.version().unwrap_or_else(|_| "unknown".into());
		ArmInfo { arm: "iotdb-double".into(), engine: format!("iotdb {version}"), value_type: "DOUBLE field".into(), method: format!("linear interpolation in SQL: lead() into segments, date_bin_gapfill() grid, ASOF LEFT JOIN from the grid, first/last segment extended; downsample via date_bin; table model, one device; insertTablet of {ROWS_PER_INSERT} rows in time order, answered once the WAL entry is synced (wal_mode=SYNC); table REST API (JSON) on loopback") }
	}

	async fn load(&mut self, ds: &Dataset) -> anyhow::Result<()> {
		self.db = None;
		let timestamps = ds.points.iter().map(|p| p.timestamp.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("instant {} is outside the nanosecond range", p.timestamp))).collect::<anyhow::Result<Vec<i64>>>()?;
		let values = ds.points.iter().map(|p| p.value.to_f64().filter(|v| v.is_finite()).ok_or_else(|| anyhow::anyhow!("value {} has no finite f64 image", p.value))).collect::<anyhow::Result<Vec<f64>>>()?;
		let name = format!("weft_xbench_{}_{}", std::process::id(), NEXT_DB.fetch_add(1, Ordering::Relaxed));
		let server = self.server.clone();
		self.db = Some(tokio::task::spawn_blocking(move || load_db(server, name, &timestamps, &values)).await??);
		Ok(())
	}

	/// The database's TsFile bytes once flushed: `FLUSH` writes the memtable out, as the server
	/// would on its own once it filled, and `information_schema.table_disk_usage` then reports the
	/// table's files. Taken after the timed loads, so the flush is not in the load time.
	fn storage_bytes(&self) -> Option<u64> {
		let db = self.db.as_ref()?;
		db.server.execute(None, &format!("FLUSH {}", db.name)).ok()?;
		let rows = db.server.query("information_schema", &format!("SELECT CAST(sum(size_in_bytes) AS INT64) FROM table_disk_usage WHERE database = '{}'", db.name), 1).ok()?;
		rows.first()?.get(0)?.as_u64()
	}

	async fn interpolate(&mut self, ds: &Dataset, _knots: Vec<splimes::Point>) -> anyhow::Result<Output> {
		let start = ds.start.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("grid start out of range"))?;
		let end = ds.end.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("grid end out of range"))?;
		let step = ds.resolution.step().num_nanoseconds().ok_or_else(|| anyhow::anyhow!("grid step out of range"))?;
		let sql = INTERPOLATE_SQL.replace("{start}", &start.to_string()).replace("{end}", &end.to_string()).replace("{step}", &step.to_string()).replace("{lo}", &(start - step).to_string()).replace("{hi}", &(end + step).to_string());
		let rows = ds.grid_len();
		Ok(Output::Float(self.on_db(move |server, db| server.query(db, &sql, rows)?.iter().map(|r| f64_cell(r.get(0))).collect()).await?))
	}

	async fn downsample(&mut self, ds: &Dataset, _knots: Vec<splimes::Point>) -> anyhow::Result<Vec<BucketRow>> {
		let width = ds.bucket.step().num_nanoseconds().ok_or_else(|| anyhow::anyhow!("bucket width out of range"))?;
		let sql = DOWNSAMPLE_SQL.replace("{width}", &width.to_string());
		let rows = ds.points.len();
		self.on_db(move |server, db| server.query(db, &sql, rows)?.iter().map(|r| Ok(BucketRow { start_ns: i64_cell(r.get(0))?, count: i64_cell(r.get(1))?.unsigned_abs(), min: f64_cell(r.get(2))?, max: f64_cell(r.get(3))?, avg: f64_cell(r.get(4))? })).collect()).await
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn basic_auth_is_base64_of_user_colon_password() {
		assert_eq!(base64_basic("root", "root"), "Basic cm9vdDpyb290");
		assert_eq!(base64_basic("a", "b"), "Basic YTpi");
		assert_eq!(base64_basic("ab", "c"), "Basic YWI6Yw==");
	}
}
