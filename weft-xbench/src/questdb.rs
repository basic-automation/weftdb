//! The `QuestDB` arms: knots stored as `DOUBLE` or as `DECIMAL(18, s)`, on a server given by
//! `host:port`.
//!
//! `QuestDB` 10 speaks **QWP**, a columnar binary protocol over WebSocket that supersedes both ILP
//! for writes and PG Wire for reads, so these arms use it both ways through `questdb-rs`, `QuestDB`'s
//! own client: an arm on PG Wire would be timing a path `QuestDB` has deprecated (#393). Table
//! creation, the applied-yet check and cleanup go over the HTTP `/exec` endpoint on the same port.
//!
//! Each load creates a fresh WAL table `(seq LONG, t TIMESTAMP_NS, v <type>) TIMESTAMP(t)`,
//! partitioned by year, the coarsest partitioning a WAL table allows (one partition for every
//! synthetic dataset, five for the four-year UCI series), sends every row over QWP, waits for the
//! server's acknowledgement, and then waits until the WAL has been applied, so the rows are visible
//! to the queries that follow. Open-source `QuestDB` acknowledges receipt, not durability (a durable
//! acknowledgement is an Enterprise feature), so the reference server runs with
//! `cairo.commit.mode=sync`, under which each commit is synced to disk, and the arm records the mode
//! it found.
//!
//! The interpolation uses only `QuestDB`'s native time joins. An `LT JOIN` of the deduplicated knots
//! onto themselves pairs each knot with the one before it: one row per segment, keyed by the
//! segment's start. An `ASOF JOIN` from the grid then finds, for each instant, the segment that
//! starts at the last knot at or before it, whose end is the next knot; past the last knot that is
//! the last segment, extended, which is splimes' rule. Before the first knot the first segment
//! stands in. Times are carried as `LONG` nanoseconds; only the join keys are timestamps.
//!
//! Two shapes were tried first and are recorded in `DECISIONS.md`: `lead`/`lag` on the right of an
//! `ASOF JOIN` came back shifted on `QuestDB` 10.1 (a knot's own time read as the next knot's), and
//! neighbours attached by `row_number()` self-joins on `rn ± 1` ran as a nested loop that hit the
//! server's 60 s query timeout on the smallest dataset. Duplicate instants keep their last value
//! (`last(v)` per instant), as splimes does. Downsampling is the native `SAMPLE BY … ALIGN TO
//! CALENDAR` in nanoseconds, which is epoch-aligned like `weft_reduce`'s buckets.

use std::{
	sync::atomic::{AtomicU64, Ordering}, time::{Duration, Instant}
};

use bigdecimal::{BigDecimal, ToPrimitive};
use questdb::{
	egress::{column::ColumnView, reader::Reader}, ingress::{AckLevel, DecimalView, Sender, TimestampNanos}
};

use crate::{weft::Arm, ArmInfo, BucketRow, Dataset, Output};

/// Counter distinguishing tables of arms alive at once.
static NEXT_TABLE: AtomicU64 = AtomicU64::new(0);

/// The widest `DECIMAL` whose mantissa fits the `i64` [`DecimalView`] is built from here.
const DECIMAL_WIDTH: u8 = 18;

/// Rows per QWP flush. One frame per load would exceed the client's frame cap on the larger
/// datasets; 100,000 rows is a few megabytes.
const FLUSH_ROWS: usize = 100_000;

/// How long a load may wait for its acknowledgement and for the WAL to be applied.
const APPLY_TIMEOUT: Duration = Duration::from_secs(600);

/// The grid reconstruction, with `{table}`, `{start}`, `{n}` (grid length) and `{step}` (ns)
/// substituted. See the module notes for the shape.
const INTERPOLATE_SQL: &str = "
WITH
k AS ((SELECT t, last(v)::double v FROM {table} GROUP BY t ORDER BY t) TIMESTAMP(t)),
s AS (SELECT b.t st, b.t::long sl, b.v sv, a.t::long el, a.v ev FROM k a LT JOIN k b),
seg AS ((SELECT * FROM s WHERE sl IS NOT NULL ORDER BY st) TIMESTAMP(st)),
f AS (SELECT sl fs, sv fsv, el fe, ev fev FROM seg LIMIT 1),
g AS ((SELECT ({start} + (x - 1) * {step})::timestamp_ns gt FROM long_sequence({n})) TIMESTAMP(gt)),
j AS (SELECT g.gt::long gl, seg.sl, seg.sv, seg.el, seg.ev FROM g ASOF JOIN seg)
SELECT CASE
	WHEN sl IS NULL THEN fsv + (fev - fsv) * ((gl - fs)::double / (fe - fs)::double)
	ELSE sv + (ev - sv) * ((gl - sl)::double / (el - sl)::double)
END v
FROM j CROSS JOIN f ORDER BY gl
";

/// Count, min, max and avg per epoch-aligned bucket of `{width}`, a `SAMPLE BY` interval. The mean is
/// taken in `DOUBLE`, as every other arm's is: `QuestDB`'s `avg` of a `DECIMAL(18, s)` column returns
/// a decimal rounded to the column's `s` places (2.804 for 2.80357… at `s = 3`), which no `f64`
/// reference matches. `min` and `max` stay exact.
const DOWNSAMPLE_SQL: &str = "SELECT t::long b, count() n, min(v)::double, max(v)::double, avg(v::double) FROM {table} SAMPLE BY {width} ALIGN TO CALENDAR";

/// `width_ns` as a `SAMPLE BY` interval in the largest unit that divides it exactly: `QuestDB`
/// refuses a nanosecond count as large as a minute's.
fn sample_interval(width_ns: i64) -> String {
	const UNITS: [(i64, &str); 6] = [(86_400_000_000_000, "d"), (3_600_000_000_000, "h"), (60_000_000_000, "m"), (1_000_000_000, "s"), (1_000_000, "T"), (1_000, "U")];
	UNITS.iter().find(|(ns, _)| width_ns % ns == 0).map_or_else(|| format!("{width_ns}n"), |(ns, unit)| format!("{}{unit}", width_ns / ns))
}

/// The column type the knots are stored as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QdbValue {
	/// `DOUBLE`.
	Double,
	/// `DECIMAL(18, s)`, with `s` the dataset's largest number of fractional digits.
	Decimal,
}

/// A `QuestDB` server: QWP over WebSocket and HTTP `/exec` on one port.
#[derive(Clone)]
pub struct Server {
	addr: String,
	agent: ureq::Agent,
}

impl std::fmt::Debug for Server {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Server").field("addr", &self.addr).finish_non_exhaustive()
	}
}

impl Server {
	/// The server at `addr` (`127.0.0.1:9000`).
	#[must_use]
	pub fn new(addr: &str) -> Self {
		let addr = addr.trim_start_matches("http://").trim_end_matches('/').to_string();
		let config = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(APPLY_TIMEOUT)).build();
		Self { addr, agent: ureq::Agent::new_with_config(config) }
	}

	/// The QWP connection string for this server.
	fn conf(&self) -> String {
		format!("ws::addr={};", self.addr)
	}

	/// Run `sql` over HTTP `/exec` and return its `dataset` rows; an error answer is an error.
	fn exec(&self, sql: &str) -> anyhow::Result<Vec<serde_json::Value>> {
		let mut response = self.agent.get(&format!("http://{}/exec", self.addr)).query("query", sql).call()?;
		let body: serde_json::Value = serde_json::from_slice(&response.body_mut().read_to_vec()?)?;
		if let Some(error) = body.get("error") {
			anyhow::bail!("questdb: {error} in {sql}");
		}
		Ok(body.get("dataset").and_then(serde_json::Value::as_array).cloned().unwrap_or_default())
	}

	/// The first cell of `sql`'s first row as a string.
	fn scalar(&self, sql: &str) -> anyhow::Result<String> {
		let rows = self.exec(sql)?;
		let cell = rows.first().and_then(|r| r.get(0)).ok_or_else(|| anyhow::anyhow!("questdb: no rows from {sql}"))?;
		Ok(cell.as_str().map_or_else(|| cell.to_string(), str::to_string))
	}

	/// The server's version and commit mode, e.g. `10.1.0 (commit mode sync)`.
	///
	/// # Errors
	///
	/// If the server cannot be reached.
	pub fn version(&self) -> anyhow::Result<String> {
		let build = self.scalar("SELECT build()")?;
		let version = build.split("QuestDB ").nth(1).and_then(|s| s.split(',').next()).unwrap_or(&build).trim().to_string();
		let mode = self.scalar("SELECT value FROM (SHOW PARAMETERS) WHERE property_path = 'cairo.commit.mode'").unwrap_or_else(|_| "unknown".into());
		Ok(format!("{version} (commit mode {mode})"))
	}
}

/// A loaded table and the reader its queries go through; the table is dropped on drop.
struct Db {
	server: Server,
	table: String,
	reader: Reader,
}

impl Drop for Db {
	fn drop(&mut self) {
		let _ = self.server.exec(&format!("DROP TABLE IF EXISTS {}", self.table));
	}
}

/// `QuestDB` with the knots stored as [`QdbValue`].
pub struct QuestDb {
	value: QdbValue,
	server: Server,
	db: Option<Db>,
}

impl std::fmt::Debug for QuestDb {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("QuestDb").field("value", &self.value).field("server", &self.server).field("loaded", &self.db.is_some()).finish()
	}
}

impl QuestDb {
	/// `questdb-double`.
	#[must_use]
	pub const fn double(server: Server) -> Self {
		Self { value: QdbValue::Double, server, db: None }
	}

	/// `questdb-decimal`.
	#[must_use]
	pub const fn decimal(server: Server) -> Self {
		Self { value: QdbValue::Decimal, server, db: None }
	}

	/// Run `f` with the loaded table on tokio's blocking pool (the client is synchronous), as the
	/// `DuckDB` arms do, so the hand-off is in every arm's timing alike.
	async fn on_db<T: Send + 'static>(&mut self, f: impl FnOnce(&mut Db) -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
		let mut db = self.db.take().ok_or_else(|| anyhow::anyhow!("questdb: nothing loaded"))?;
		let (db, result) = tokio::task::spawn_blocking(move || {
			let result = f(&mut db);
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
	u8::try_from(digits).ok().filter(|s| *s <= DECIMAL_WIDTH).ok_or_else(|| anyhow::anyhow!("values need {digits} fractional digits, more than DECIMAL({DECIMAL_WIDTH}) holds"))
}

/// `v`'s exact mantissa at `scale`, refusing one that does not fit.
fn decimal_mantissa(v: &BigDecimal, scale: u8) -> anyhow::Result<i64> {
	// `with_scale` rounds silently; a value that changes when rescaled has more digits than `scale`.
	let rescaled = v.with_scale(i64::from(scale));
	anyhow::ensure!(&rescaled == v, "value {v} has more than {scale} fractional digits");
	let (mantissa, _) = rescaled.into_bigint_and_exponent();
	mantissa.to_i64().filter(|m| m.unsigned_abs() < 10u64.pow(u32::from(DECIMAL_WIDTH))).ok_or_else(|| anyhow::anyhow!("value {v} does not fit DECIMAL({DECIMAL_WIDTH}, {scale})"))
}

/// A stored value, ready for its QWP column.
enum Value {
	Double(f64),
	Decimal(i64),
}

/// Wait until every transaction the table's WAL has sequenced is applied, so a query sees the rows.
fn wait_applied(server: &Server, table: &str) -> anyhow::Result<()> {
	let deadline = Instant::now() + APPLY_TIMEOUT;
	let sql = format!("SELECT writerTxn, sequencerTxn, suspended FROM wal_tables() WHERE name = '{table}'");
	loop {
		let rows = server.exec(&sql)?;
		if let Some(row) = rows.first() {
			anyhow::ensure!(row.get(2).and_then(serde_json::Value::as_bool) != Some(true), "questdb: the WAL of {table} is suspended");
			let (writer, sequencer) = (row.get(0).and_then(serde_json::Value::as_i64), row.get(1).and_then(serde_json::Value::as_i64));
			if writer.is_some() && writer == sequencer && sequencer > Some(0) {
				return Ok(());
			}
		}
		anyhow::ensure!(Instant::now() < deadline, "questdb: {table} was not applied within {APPLY_TIMEOUT:?}");
		std::thread::sleep(Duration::from_millis(1));
	}
}

fn load_table(server: Server, table: String, column_type: &str, scale: u8, timestamps: &[i64], values: &[Value]) -> anyhow::Result<Db> {
	server.exec(&format!("CREATE TABLE {table} (seq LONG, t TIMESTAMP_NS, v {column_type}) TIMESTAMP(t) PARTITION BY YEAR WAL"))?;
	// From here on, dropping `db` removes the table whatever fails next.
	let reader = Reader::from_conf(server.conf())?;
	let db = Db { server, table, reader };
	let mut sender = Sender::from_conf(db.server.conf())?;
	let mut buffer = sender.new_buffer();
	for (seq, (t, v)) in timestamps.iter().zip(values).enumerate() {
		buffer.table(db.table.as_str())?.column_i64("seq", i64::try_from(seq)?)?;
		match v {
			Value::Double(v) => buffer.column_f64("v", *v)?,
			Value::Decimal(m) => buffer.column_dec64("v", DecimalView::try_new_scaled(u32::from(scale), m.to_be_bytes().to_vec())?)?,
		};
		buffer.at(TimestampNanos::new(*t))?;
		if (seq + 1) % FLUSH_ROWS == 0 {
			sender.flush(&mut buffer)?;
		}
	}
	if !buffer.is_empty() {
		sender.flush(&mut buffer)?;
	}
	sender.wait(AckLevel::Ok, APPLY_TIMEOUT)?;
	drop(sender);
	wait_applied(&db.server, &db.table)?;
	Ok(db)
}

/// Every `DOUBLE` of column `col` across the cursor's batches.
fn doubles(reader: &mut Reader, sql: &str) -> anyhow::Result<Vec<f64>> {
	let mut cursor = reader.execute(sql)?;
	let mut out = Vec::new();
	while let Some(view) = cursor.next_batch()? {
		match view.column(0) {
			Ok(ColumnView::Double(c)) => out.extend((0..c.len()).map(|i| if c.is_null(i) { f64::NAN } else { c.value(i) })),
			other => anyhow::bail!("questdb: expected a DOUBLE column, got {:?}", other.map(|c| c.kind())),
		}
	}
	Ok(out)
}

fn buckets(reader: &mut Reader, sql: &str) -> anyhow::Result<Vec<BucketRow>> {
	let mut cursor = reader.execute(sql)?;
	let mut out = Vec::new();
	while let Some(view) = cursor.next_batch()? {
		let (Ok(ColumnView::Long(b)), Ok(ColumnView::Long(n)), Ok(ColumnView::Double(mn)), Ok(ColumnView::Double(mx)), Ok(ColumnView::Double(av))) = (view.column(0), view.column(1), view.column(2), view.column(3), view.column(4)) else {
			anyhow::bail!("questdb: unexpected downsample columns");
		};
		out.extend((0..b.len()).map(|i| BucketRow { start_ns: b.value(i), count: n.value(i).unsigned_abs(), min: mn.value(i), max: mx.value(i), avg: av.value(i) }));
	}
	Ok(out)
}

impl Arm for QuestDb {
	fn info(&self) -> ArmInfo {
		let version = self.server.version().unwrap_or_else(|_| "unknown".into());
		let (arm, value_type, note) = match self.value {
			QdbValue::Double => ("questdb-double", "DOUBLE", ""),
			QdbValue::Decimal => ("questdb-decimal", "DECIMAL(18, s)", "; stored exactly, the interpolation line computed in DOUBLE"),
		};
		ArmInfo { arm: arm.into(), engine: format!("questdb {version}"), value_type: value_type.into(), method: format!("linear interpolation in SQL: long_sequence() grid, native LT JOIN into segments, then ASOF JOIN from the grid, first/last segment extended; downsample via SAMPLE BY … ALIGN TO CALENDAR; WAL table partitioned by year, load acknowledged over QWP and timed until applied; QWP both ways over loopback WebSocket{note}") }
	}

	async fn load(&mut self, ds: &Dataset) -> anyhow::Result<()> {
		self.db = None;
		let timestamps = ds.points.iter().map(|p| p.timestamp.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("instant {} is outside the nanosecond range", p.timestamp))).collect::<anyhow::Result<Vec<i64>>>()?;
		let (column_type, scale, values) = match self.value {
			QdbValue::Double => ("DOUBLE".to_string(), 0, ds.points.iter().map(|p| p.value.to_f64().filter(|v| v.is_finite()).map(Value::Double).ok_or_else(|| anyhow::anyhow!("value {} has no finite f64 image", p.value))).collect::<anyhow::Result<Vec<_>>>()?),
			QdbValue::Decimal => {
				let values: Vec<BigDecimal> = ds.points.iter().map(|p| p.value.clone()).collect();
				let scale = decimal_scale(&values)?;
				(format!("DECIMAL({DECIMAL_WIDTH}, {scale})"), scale, values.iter().map(|v| decimal_mantissa(v, scale).map(Value::Decimal)).collect::<anyhow::Result<Vec<_>>>()?)
			}
		};
		let table = format!("weft_xbench_{}_{}", std::process::id(), NEXT_TABLE.fetch_add(1, Ordering::Relaxed));
		let server = self.server.clone();
		self.db = Some(tokio::task::spawn_blocking(move || load_table(server, table, &column_type, scale, &timestamps, &values)).await??);
		Ok(())
	}

	fn storage_bytes(&self) -> Option<u64> {
		let db = self.db.as_ref()?;
		db.server.scalar(&format!("SELECT sum(diskSize) FROM table_partitions('{}')", db.table)).ok()?.parse().ok()
	}

	async fn interpolate(&mut self, ds: &Dataset, _knots: Vec<splimes::Point>) -> anyhow::Result<Output> {
		let start = ds.start.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("grid start out of range"))?;
		let end = ds.end.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("grid end out of range"))?;
		let step = ds.resolution.step().num_nanoseconds().ok_or_else(|| anyhow::anyhow!("grid step out of range"))?;
		let n = (end - start) / step + 1;
		Ok(Output::Float(self.on_db(move |db| doubles(&mut db.reader, &INTERPOLATE_SQL.replace("{table}", &db.table).replace("{start}", &start.to_string()).replace("{step}", &step.to_string()).replace("{n}", &n.to_string()))).await?))
	}

	async fn downsample(&mut self, ds: &Dataset, _knots: Vec<splimes::Point>) -> anyhow::Result<Vec<BucketRow>> {
		let width = ds.bucket.step().num_nanoseconds().ok_or_else(|| anyhow::anyhow!("bucket width out of range"))?;
		self.on_db(move |db| buckets(&mut db.reader, &DOWNSAMPLE_SQL.replace("{table}", &db.table).replace("{width}", &sample_interval(width)))).await
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
	}

	#[test]
	fn a_bucket_width_is_named_in_its_largest_exact_unit() {
		assert_eq!(sample_interval(60_000_000_000), "1m");
		assert_eq!(sample_interval(3_600_000_000_000), "1h");
		assert_eq!(sample_interval(90_000_000_000), "90s");
		assert_eq!(sample_interval(10), "10n");
	}

	#[test]
	fn a_bare_or_http_address_gives_the_same_endpoints() {
		assert_eq!(Server::new("http://127.0.0.1:9000/").conf(), "ws::addr=127.0.0.1:9000;");
		assert_eq!(Server::new("127.0.0.1:9000").conf(), "ws::addr=127.0.0.1:9000;");
	}
}
