//! The `TimescaleDB` arms: knots stored as `double precision` or as `numeric`, in a hypertable on a
//! `PostgreSQL` server given by URL.
//!
//! `TimescaleDB` is a `PostgreSQL` extension, so these arms speak the `PostgreSQL` wire protocol
//! through `tokio-postgres`, the async client a Rust user would reach for, over loopback. Each load
//! creates a fresh schema with one hypertable `knots (seq bigint, t bigint /* epoch ns */, v <type>)`
//! and fills it with one text `COPY`, which commits with the server's default
//! `synchronous_commit = on`: durable, as a WeftDB seal is.
//!
//! The hypertable's chunk interval is the dataset's whole span, so every load is one chunk: each
//! dataset fits in memory many times over, which is the size TimescaleDB's guidance picks chunks
//! for, and it is the counterpart of `weftdb-stored-bulk`'s one segment. A fixed one-day interval
//! was tried first and is the wrong configuration here: the four-year UCI series became 420 chunks,
//! and each per-instant index lookup walked all of them, 58× slower on the same query.
//!
//! `TimescaleDB` has a native linear fill, `time_bucket_gapfill(…)` with `interpolate(…)`, but it
//! interpolates between *bucket aggregates* at bucket boundaries, not between the knots themselves.
//! The datasets' knots are jittered off the grid, so it computes a different answer, and it does not
//! extend the first and last segments past the data. The interpolation query is therefore the SQL
//! a `PostgreSQL` user writes without `ASOF JOIN`: for each grid instant from `generate_series`, one
//! `LATERAL … ORDER BY t DESC LIMIT 1` for the knot at or before it and one `ORDER BY t LIMIT 1` for
//! the knot at or after it, both answered from the hypertable's index on `t`. Duplicate instants keep
//! their last value (highest `seq`), as splimes does. Downsampling uses the native `time_bucket`.
//!
//! In the `numeric` arm the knots are stored exactly and `min` and `max` stay exact; the line and
//! `avg` are read back as `double precision`. The hypertable is left in the row store: the
//! columnstore (compression) is a separate configuration, not this arm's.

use std::sync::atomic::{AtomicU64, Ordering};

use bigdecimal::ToPrimitive;
use futures_util::SinkExt;
use tokio_postgres::{Client, NoTls, Statement};

use crate::{weft::Arm, ArmInfo, BucketRow, Dataset, Output};

/// Counter distinguishing schemas of arms alive at once.
static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

/// The grid reconstruction. Parameters: first grid instant, last grid instant (inclusive), step,
/// all in epoch nanoseconds. Before the first knot `lo` is missing and the first segment (`f0`,
/// `f1`, the first two distinct instants) stands in; after the last, `l0`, `l1` do.
const INTERPOLATE_SQL: &str = "
WITH
f0 AS (SELECT t, v FROM knots ORDER BY t, seq DESC LIMIT 1),
f1 AS (SELECT k.t, k.v FROM knots k, f0 WHERE k.t > f0.t ORDER BY k.t, k.seq DESC LIMIT 1),
l1 AS (SELECT t, v FROM knots ORDER BY t DESC, seq DESC LIMIT 1),
l0 AS (SELECT k.t, k.v FROM knots k, l1 WHERE k.t < l1.t ORDER BY k.t DESC, k.seq DESC LIMIT 1),
g AS (SELECT generate_series($1::bigint, $2::bigint, $3::bigint) AS t),
seg AS (
	SELECT g.t,
		CASE WHEN lo.t IS NULL THEN (SELECT t FROM f0) WHEN hi.t IS NULL THEN (SELECT t FROM l0) ELSE lo.t END AS a_t,
		CASE WHEN lo.t IS NULL THEN (SELECT v FROM f0) WHEN hi.t IS NULL THEN (SELECT v FROM l0) ELSE lo.v END AS a_v,
		CASE WHEN lo.t IS NULL THEN (SELECT t FROM f1) WHEN hi.t IS NULL THEN (SELECT t FROM l1) ELSE hi.t END AS b_t,
		CASE WHEN lo.t IS NULL THEN (SELECT v FROM f1) WHEN hi.t IS NULL THEN (SELECT v FROM l1) ELSE hi.v END AS b_v
	FROM g
	LEFT JOIN LATERAL (SELECT k.t, k.v FROM knots k WHERE k.t <= g.t ORDER BY k.t DESC, k.seq DESC LIMIT 1) lo ON true
	LEFT JOIN LATERAL (SELECT k.t, k.v FROM knots k WHERE k.t >= g.t ORDER BY k.t, k.seq DESC LIMIT 1) hi ON true
)
SELECT CASE WHEN b_t = a_t THEN a_v::float8
	ELSE a_v::float8 + (b_v::float8 - a_v::float8) * ((t - a_t)::float8 / (b_t - a_t)::float8) END
FROM seg ORDER BY t
";

/// Count, min, max and avg per epoch-aligned bucket. Parameter: the bucket width in nanoseconds.
/// Every row counts, duplicates included, as in `weft_reduce::reduce`.
const DOWNSAMPLE_SQL: &str = "
SELECT time_bucket($1::bigint, t) AS b, count(*)::bigint, min(v)::float8, max(v)::float8, avg(v)::float8
FROM knots GROUP BY b ORDER BY b
";

/// The column type the knots are stored as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsValue {
	/// `double precision`.
	Double,
	/// `numeric`, unconstrained: every value exactly as given.
	Numeric,
}

/// A loaded schema and the connection that owns it; the schema is dropped on drop.
struct Db {
	client: Client,
	url: String,
	schema: String,
	/// The two queries, prepared on first use; the first timed rep pays the round trip, as a
	/// client's first query does.
	interpolate: Option<Statement>,
	downsample: Option<Statement>,
}

impl Drop for Db {
	/// `Drop` cannot await, so the cleanup runs on a thread of its own with its own connection, and
	/// is waited for, so a run leaves no schema behind.
	fn drop(&mut self) {
		let (url, schema) = (self.url.clone(), self.schema.clone());
		let _ = std::thread::spawn(move || {
			let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
			runtime.block_on(async {
				if let Ok((client, connection)) = tokio_postgres::connect(&url, NoTls).await {
					tokio::spawn(connection);
					let _ = client.batch_execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE")).await;
				}
			});
		})
		.join();
	}
}

/// `TimescaleDB` with the knots stored as [`TsValue`].
pub struct Timescale {
	value: TsValue,
	url: String,
	db: Option<Db>,
}

impl std::fmt::Debug for Timescale {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Timescale").field("value", &self.value).field("loaded", &self.db.is_some()).finish_non_exhaustive()
	}
}

/// A connection to `url`, its driver task spawned on the current runtime.
async fn connect(url: &str) -> anyhow::Result<Client> {
	let (client, connection) = tokio_postgres::connect(url, NoTls).await?;
	tokio::spawn(connection);
	Ok(client)
}

/// `PostgreSQL` and `TimescaleDB` versions of the server at `url` (`postgresql://…`).
///
/// # Errors
///
/// If the server cannot be reached or has no `timescaledb` extension.
pub async fn version(url: &str) -> anyhow::Result<String> {
	let client = connect(url).await?;
	let row = client.query_one("SELECT current_setting('server_version'), (SELECT extversion FROM pg_extension WHERE extname = 'timescaledb')", &[]).await?;
	let extension: Option<String> = row.get(1);
	Ok(format!("{} (PostgreSQL {})", extension.ok_or_else(|| anyhow::anyhow!("the server at {url} has no timescaledb extension"))?, row.get::<_, String>(0)))
}

impl Timescale {
	/// `timescaledb-double`.
	#[must_use]
	pub fn double(url: &str) -> Self {
		Self { value: TsValue::Double, url: url.to_string(), db: None }
	}

	/// `timescaledb-numeric`.
	#[must_use]
	pub fn numeric(url: &str) -> Self {
		Self { value: TsValue::Numeric, url: url.to_string(), db: None }
	}

	fn loaded(&mut self) -> anyhow::Result<&mut Db> {
		self.db.as_mut().ok_or_else(|| anyhow::anyhow!("timescaledb: nothing loaded"))
	}
}

/// The `COPY … FROM STDIN` text body: one `seq\tt\tv` line per knot.
fn copy_text(timestamps: &[i64], values: &[String]) -> String {
	let mut out = String::with_capacity(timestamps.len() * 40);
	for (seq, (t, v)) in timestamps.iter().zip(values).enumerate() {
		out.push_str(&format!("{seq}\t{t}\t{v}\n"));
	}
	out
}

impl Arm for Timescale {
	fn info(&self) -> ArmInfo {
		// `info` is synchronous and called outside any timing; the version is read on a thread of
		// its own so it works from inside the runtime too.
		let url = self.url.clone();
		let version = std::thread::spawn(move || tokio::runtime::Builder::new_current_thread().enable_all().build().ok().and_then(|rt| rt.block_on(version(&url)).ok())).join().ok().flatten().unwrap_or_else(|| "unknown".into());
		let (arm, value_type, note) = match self.value {
			TsValue::Double => ("timescaledb-double", "double precision", ""),
			TsValue::Numeric => ("timescaledb-numeric", "numeric", "; stored exactly, the interpolation line computed in double precision"),
		};
		ArmInfo { arm: arm.into(), engine: format!("timescaledb {version}"), value_type: value_type.into(), method: format!("linear interpolation in SQL: generate_series() grid, two index-backed LATERAL LIMIT 1 lookups, first/last segment extended; downsample via time_bucket; hypertable on t (one chunk per load, row store), one text COPY committed with synchronous_commit on; server over loopback via tokio-postgres{note}") }
	}

	fn unload(&mut self) {
		self.db = None;
	}

	async fn load(&mut self, ds: &Dataset) -> anyhow::Result<()> {
		self.db = None;
		let timestamps = ds.points.iter().map(|p| p.timestamp.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("instant {} is outside the nanosecond range", p.timestamp))).collect::<anyhow::Result<Vec<i64>>>()?;
		let (column_type, values) = match self.value {
			TsValue::Double => ("double precision", ds.points.iter().map(|p| p.value.to_f64().filter(|v| v.is_finite()).map(|v| format!("{v:?}")).ok_or_else(|| anyhow::anyhow!("value {} has no finite f64 image", p.value))).collect::<anyhow::Result<Vec<_>>>()?),
			// `BigDecimal`'s plain form is exact and `numeric` parses it without rounding.
			TsValue::Numeric => ("numeric", ds.points.iter().map(|p| p.value.to_plain_string()).collect()),
		};
		// One chunk for the whole load (see the module notes).
		let chunk = timestamps.iter().max().zip(timestamps.iter().min()).map_or(1, |(last, first)| last.saturating_sub(*first).saturating_add(1)).max(1);
		let body = copy_text(&timestamps, &values);
		let client = connect(&self.url).await?;
		let schema = format!("weft_xbench_{}_{}", std::process::id(), NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed));
		client.batch_execute(&format!("CREATE SCHEMA {schema}")).await?;
		// From here on, dropping `db` removes the schema whatever fails next.
		let db = Db { client, url: self.url.clone(), schema, interpolate: None, downsample: None };
		db.client.batch_execute(&format!("SET search_path = {}, public; CREATE TABLE knots (seq bigint NOT NULL, t bigint NOT NULL, v {column_type} NOT NULL); SELECT create_hypertable('knots', by_range('t', {chunk}::bigint))", db.schema)).await?;
		let sink = db.client.copy_in("COPY knots (seq, t, v) FROM STDIN").await?;
		futures_util::pin_mut!(sink);
		sink.send(bytes::Bytes::from(body)).await?;
		sink.finish().await?;
		self.db = Some(db);
		Ok(())
	}

	fn storage_bytes(&self) -> Option<u64> {
		let db = self.db.as_ref()?;
		let (url, schema) = (db.url.clone(), db.schema.clone());
		std::thread::spawn(move || {
			let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
			rt.block_on(async {
				let client = connect(&url).await.ok()?;
				let row = client.query_one(&format!("SELECT hypertable_size('{schema}.knots')"), &[]).await.ok()?;
				u64::try_from(row.get::<_, i64>(0)).ok()
			})
		})
		.join()
		.ok()
		.flatten()
	}

	async fn interpolate(&mut self, ds: &Dataset, _knots: Vec<splimes::Point>) -> anyhow::Result<Output> {
		let start = ds.start.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("grid start out of range"))?;
		let end = ds.end.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("grid end out of range"))?;
		let step = ds.resolution.step().num_nanoseconds().ok_or_else(|| anyhow::anyhow!("grid step out of range"))?;
		let db = self.loaded()?;
		let statement = if let Some(statement) = &db.interpolate {
			statement.clone()
		} else {
			let statement = db.client.prepare(INTERPOLATE_SQL).await?;
			db.interpolate = Some(statement.clone());
			statement
		};
		let rows = db.client.query(&statement, &[&start, &end, &step]).await?;
		Ok(Output::Float(rows.iter().map(|r| r.get::<_, f64>(0)).collect()))
	}

	async fn downsample(&mut self, ds: &Dataset, _knots: Vec<splimes::Point>) -> anyhow::Result<Vec<BucketRow>> {
		let width = ds.bucket.step().num_nanoseconds().ok_or_else(|| anyhow::anyhow!("bucket width out of range"))?;
		let db = self.loaded()?;
		let statement = if let Some(statement) = &db.downsample {
			statement.clone()
		} else {
			let statement = db.client.prepare(DOWNSAMPLE_SQL).await?;
			db.downsample = Some(statement.clone());
			statement
		};
		let rows = db.client.query(&statement, &[&width]).await?;
		Ok(rows.iter().map(|r| BucketRow { start_ns: r.get(0), count: r.get::<_, i64>(1).unsigned_abs(), min: r.get(2), max: r.get(3), avg: r.get(4) }).collect())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_copy_body_is_one_tab_separated_line_per_knot() {
		assert_eq!(copy_text(&[10, -5], &["1.5".into(), "-2".into()]), "0\t10\t1.5\n1\t-5\t-2\n");
	}
}
