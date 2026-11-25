//! The WeftDB arms: in-memory, and stored with exact-decimal or `f64` output in two segment layouts.

use std::{
	collections::BTreeMap, future::Future, path::{Path, PathBuf}, sync::atomic::{AtomicU64, Ordering}
};

use bigdecimal::{BigDecimal, ToPrimitive};
use splimes::{Interpolator, Spline};
use weft_bench::{adapter::SystemAdapter, WeftAdapter, DEFAULT_INGEST_BATCH_ROWS};
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weft_reduce::Aggregation;
use weftdb::{IngestBatch, Resolution, SegmentStore, SeriesId, SeriesSelector, TagSet};

use crate::{tsbs, ArmInfo, BucketRow, Dataset, Output, DOWNSAMPLE_AGGREGATIONS};

/// A benchmark arm: something that can load a dataset, reconstruct its grid and reduce it into
/// buckets.
#[allow(async_fn_in_trait, reason = "the runner dispatches statically; no arm crosses threads as a trait object")]
pub trait Arm {
	/// The arm's report, recorded with each result.
	fn info(&self) -> ArmInfo;

	/// Whether the arm has a load phase (the in-memory arm does not).
	fn loads(&self) -> bool {
		true
	}

	/// Whether the arm's load is a result of its own. An arm that loads exactly as another arm of
	/// the same engine does (the native and client-side arms load as the portable one) reports
	/// `false` instead of a duplicate figure.
	fn reports_load(&self) -> bool {
		true
	}

	/// Whether the arm has a downsample of its own. An arm that would run the same reduction as
	/// another arm over the same store reports `false` instead of a duplicate figure.
	fn downsamples(&self) -> bool {
		true
	}

	/// Release whatever an earlier [`load`](Arm::load) stored: drop its database, table or store.
	/// The runner calls it before starting each load's clock, so a load's time is the load alone
	/// and not also the teardown of the last one (`IoTDB`'s `DROP DATABASE` takes a second).
	fn unload(&mut self);

	/// Load `ds` into fresh storage, replacing whatever an earlier call loaded. This is the
	/// timed load phase.
	///
	/// # Errors
	///
	/// If the engine refuses the data.
	async fn load(&mut self, ds: &Dataset) -> anyhow::Result<()>;

	/// Reconnect to what [`load`](Arm::load) stored after the engine restarted, and wait until it
	/// answers; an in-process arm closes its files and opens them again. Called before a
	/// post-restart rep, outside the clock (#146).
	///
	/// # Errors
	///
	/// If the engine does not come back, or nothing is loaded.
	async fn reopen(&mut self) -> anyhow::Result<()>;

	/// Bytes the loaded data occupies on disk, read after [`load`](Arm::load); `None` for an arm
	/// with no storage.
	fn storage_bytes(&self) -> Option<u64> {
		None
	}

	/// Reconstruct `ds`'s grid from what [`load`](Arm::load) stored. This is the timed
	/// interpolation phase; `knots` is a fresh copy of the points for arms that take them per call.
	///
	/// # Errors
	///
	/// If the engine fails or nothing is loaded.
	async fn interpolate(&mut self, ds: &Dataset, knots: Vec<splimes::Point>) -> anyhow::Result<Output>;

	/// Reduce what is loaded into `ds.bucket` buckets: count, min, max and avg. This is the timed
	/// downsample phase.
	///
	/// # Errors
	///
	/// If the engine fails or nothing is loaded.
	async fn downsample(&mut self, ds: &Dataset, knots: Vec<splimes::Point>) -> anyhow::Result<Vec<BucketRow>>;
}

/// WeftDB's in-process paths: interpolation straight to splimes, downsampling with
/// `weft_reduce::reduce`. The reference arm.
#[derive(Debug, Default)]
pub struct WeftMemory;

impl Arm for WeftMemory {
	fn info(&self) -> ArmInfo {
		let report = WeftAdapter::new().adapter_info();
		ArmInfo { arm: "weftdb-memory".into(), engine: report.engine, value_type: "BigDecimal".into(), method: format!("{}; downsample via weft_reduce::reduce", report.method), class: Some(crate::ArmClass::Native), own_answer: None }
	}

	fn loads(&self) -> bool {
		false
	}

	fn unload(&mut self) {}

	/// Nothing is stored, so nothing reopens.
	fn reopen(&mut self) -> impl Future<Output = anyhow::Result<()>> {
		std::future::ready(Ok(()))
	}

	/// Nothing to load: the points go to splimes on every call.
	fn load(&mut self, _ds: &Dataset) -> impl Future<Output = anyhow::Result<()>> {
		std::future::ready(Ok(()))
	}

	async fn interpolate(&mut self, ds: &Dataset, mut knots: Vec<splimes::Point>) -> anyhow::Result<Output> {
		Ok(Output::Decimal(WeftAdapter::new().interpolate_range(&mut knots, ds.start, ds.end, ds.resolution, Spline::Linear).await?))
	}

	/// The reduction is synchronous; it runs inside the returned future's construction, which the
	/// runner times like any other call.
	fn downsample(&mut self, ds: &Dataset, knots: Vec<splimes::Point>) -> impl Future<Output = anyhow::Result<Vec<BucketRow>>> {
		std::future::ready(weft_reduce::reduce(&knots, ds.bucket, None, None, &DOWNSAMPLE_AGGREGATIONS).map_err(anyhow::Error::from).and_then(|b| BucketRow::from_weft(&b)))
	}
}

/// The value type a stored arm returns from interpolation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueOut {
	/// `SegmentStore::interpolate_range`: exact-decimal points.
	Decimal,
	/// `SegmentStore::interpolate_range_f64`: binary floats.
	F64,
}

/// The aspect every stored arm keeps its knots under.
const ASPECT: &str = "xbench";

/// Counter distinguishing store directories.
static NEXT_STORE: AtomicU64 = AtomicU64::new(0);

/// A loaded store and the directory it lives in, removed on drop.
struct Stored {
	/// `None` only between closing the store and opening it again in [`Arm::reopen`]: a root
	/// opens once per process, so the old handle must go first.
	store: Option<SegmentStore>,
	dir: PathBuf,
	first: i64,
	last: i64,
}

impl Drop for Stored {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.dir);
	}
}

/// A TSBS `cpu-only` file in a WeftDB `SegmentStore`, tagged as TSBS tags it, answering TSBS's
/// tag-grouped devops queries (TAG-12, #341).
///
/// Each of the ten [`tsbs::CPU_METRICS`] is an aspect (`cpu_<metric>`, exact `ScaledI64` at scale
/// 0, nanosecond timestamps), and each host is one series of it, tagged `hostname=<host>`.
/// The answers are compared with [`tsbs::TsbsCpu`]'s reference; this is not yet a timed arm.
pub struct TsbsWeft {
	/// `None` only while dropping, so the directory goes after the store has closed.
	store: Option<SegmentStore>,
	dir: PathBuf,
	hosts: Vec<String>,
}

impl Drop for TsbsWeft {
	fn drop(&mut self) {
		self.store = None;
		let _ = std::fs::remove_dir_all(&self.dir);
	}
}

impl TsbsWeft {
	/// The aspect a CPU metric is stored under.
	#[must_use]
	pub fn aspect(metric: &str) -> String {
		format!("cpu_{metric}")
	}

	/// Store every host's readings of `cpu` in a fresh store under the system temp directory: one
	/// tagged ingest per host and metric.
	///
	/// # Errors
	///
	/// If the store cannot be opened or refuses a declaration or an ingest.
	pub async fn load(cpu: &tsbs::TsbsCpu) -> anyhow::Result<Self> {
		let dir = std::env::temp_dir().join(format!("weft-xbench-tsbs-{}-{}", std::process::id(), NEXT_STORE.fetch_add(1, Ordering::Relaxed)));
		std::fs::create_dir_all(&dir)?;
		// Built before the store opens, so a refusal below still removes the directory.
		let mut loaded = Self { store: None, dir, hosts: cpu.hosts.keys().cloned().collect() };
		let store = SegmentStore::open(&loaded.dir).await?;
		let schema = AspectSchema::new(PhysicalType::ScaledI64 { scale: 0 }, BigDecimal::from(0), TimeUnit::Nanos);
		for metric in tsbs::CPU_METRICS {
			store.declare(&Self::aspect(metric), &schema).await?;
		}
		for (host, readings) in &cpu.hosts {
			let tags = TagSet::from_pairs([("hostname", host.as_str())])?;
			let timestamps: Vec<i64> = readings.iter().map(|r| r.ts_ns).collect();
			for (m, metric) in tsbs::CPU_METRICS.iter().enumerate() {
				let values = readings.iter().map(|r| BigDecimal::from(r.values[m])).collect();
				store.ingest(&Self::aspect(metric), &IngestBatch::dense(host.clone(), timestamps.clone(), values).with_groups(tags.clone())).await?;
			}
		}
		loaded.store = Some(store);
		Ok(loaded)
	}

	fn store(&self) -> anyhow::Result<&SegmentStore> {
		self.store.as_ref().ok_or_else(|| anyhow::anyhow!("the TSBS store is closed"))
	}

	async fn series(&self, aspect: &str, host: &str) -> anyhow::Result<SeriesId> {
		let selector = SeriesSelector::exact([("hostname", Some(host))])?;
		self.store()?.resolve_series(aspect, &selector).await?.ok_or_else(|| anyhow::anyhow!("no series hostname={host} in {aspect}"))
	}

	/// [`tsbs::TsbsCpu::single_groupby`]'s question, answered by the store: each host's per-minute
	/// MAX from `downsample_range_of`, merged across the hosts by bucket.
	///
	/// # Errors
	///
	/// As the reference, or if the store fails.
	pub async fn single_groupby(&self, metrics: usize, hosts: &[&str], start_ns: i64, end_ns: i64) -> anyhow::Result<Vec<tsbs::MinuteMax>> {
		anyhow::ensure!((1..=tsbs::CPU_METRICS.len()).contains(&metrics), "a single-groupby takes 1 to {} metrics, not {metrics}", tsbs::CPU_METRICS.len());
		anyhow::ensure!(!hosts.is_empty(), "a single-groupby names at least one host");
		anyhow::ensure!(start_ns < end_ns, "the window [{start_ns}, {end_ns}) is empty");
		let mut buckets: BTreeMap<i64, Vec<Option<i64>>> = BTreeMap::new();
		for host in hosts {
			for (m, metric) in tsbs::CPU_METRICS[..metrics].iter().enumerate() {
				let aspect = Self::aspect(metric);
				let series = self.series(&aspect, host).await?;
				// The store's range is inclusive; TSBS's window is half-open.
				for bucket in self.store()?.downsample_range_of(&aspect, series, start_ns, end_ns - 1, Resolution::Minutes, &[Aggregation::Max]).await? {
					let minute_ns = nanos(bucket.timestamp)?;
					let max = bucket.values.get(Aggregation::Max.as_str()).ok_or_else(|| anyhow::anyhow!("{aspect} bucket {minute_ns} has no max"))?;
					let max = max.to_i64().ok_or_else(|| anyhow::anyhow!("{aspect} bucket {minute_ns}: max {max} is not an i64"))?;
					let slot = &mut buckets.entry(minute_ns).or_insert_with(|| vec![None; metrics])[m];
					*slot = Some(slot.map_or(max, |s| s.max(max)));
				}
			}
		}
		buckets.into_iter().map(|(minute_ns, max)| Ok(tsbs::MinuteMax { minute_ns, max: max.into_iter().collect::<Option<Vec<i64>>>().ok_or_else(|| anyhow::anyhow!("bucket {minute_ns} has a metric with no rows"))? })).collect()
	}

	/// [`tsbs::TsbsCpu::lastpoint`]'s question, answered by the store: each host's latest
	/// instant and its ten values, hosts in name order.
	///
	/// Each series is answered by [`SegmentStore::read_latest_of`], which reads only the
	/// series' latest instant, the way TSBS's generated `lastpoint` SQL takes one row per host.
	///
	/// # Errors
	///
	/// If the store fails, or a host's ten series disagree on their latest instant.
	pub async fn lastpoint(&self) -> anyhow::Result<Vec<(String, tsbs::CpuReading)>> {
		let mut out = Vec::with_capacity(self.hosts.len());
		for host in &self.hosts {
			let mut reading = tsbs::CpuReading { ts_ns: i64::MIN, values: [0; 10] };
			for (m, metric) in tsbs::CPU_METRICS.iter().enumerate() {
				let aspect = Self::aspect(metric);
				let series = self.series(&aspect, host).await?;
				let Some((last_ts, Some(last_v))) = self.store()?.read_latest_of(&aspect, series).await? else { anyhow::bail!("{aspect} hostname={host} has no latest value") };
				anyhow::ensure!(m == 0 || last_ts == reading.ts_ns, "{host}'s latest {metric} is at {last_ts}, not {}", reading.ts_ns);
				reading.ts_ns = last_ts;
				reading.values[m] = last_v.to_i64().ok_or_else(|| anyhow::anyhow!("{aspect} hostname={host}: {last_v} is not an i64"))?;
			}
			out.push((host.clone(), reading));
		}
		Ok(out)
	}

	/// The store's answer to one instance of `query`, for comparison with
	/// [`tsbs::TsbsCpu::answer`].
	///
	/// # Errors
	///
	/// As [`Self::single_groupby`] and [`Self::lastpoint`].
	pub async fn answer(&self, query: tsbs::TsbsQuery, instance: &tsbs::QueryInstance) -> anyhow::Result<tsbs::TsbsAnswer> {
		match query {
			tsbs::TsbsQuery::SingleGroupby { metrics, .. } => {
				let hosts: Vec<&str> = instance.hosts.iter().map(String::as_str).collect();
				Ok(tsbs::TsbsAnswer::Minutes(self.single_groupby(metrics, &hosts, instance.start_ns, instance.end_ns).await?))
			}
			tsbs::TsbsQuery::Lastpoint => Ok(tsbs::TsbsAnswer::Latest(self.lastpoint().await?)),
		}
	}
}

/// WeftDB interpolating and reducing on read from a `SegmentStore`.
///
/// The store is EVI-11's: the narrowest lossless value type and nanosecond timestamps, read with a
/// margin wide enough to take every stored row, so its grid equals the in-memory one. Only the
/// segment size and the output type vary between arms.
pub struct WeftStore {
	name: &'static str,
	out: ValueOut,
	batch_rows: usize,
	downsamples: bool,
	stored: Option<Stored>,
}

impl std::fmt::Debug for WeftStore {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("WeftStore").field("name", &self.name).field("out", &self.out).field("batch_rows", &self.batch_rows).field("downsamples", &self.downsamples).field("loaded", &self.stored.is_some()).finish()
	}
}

impl WeftStore {
	/// `weftdb-stored`: 1,024-row segments (the ingest workload's batch, EVI-11's layout), exact
	/// decimals out.
	#[must_use]
	pub const fn stored() -> Self {
		Self { name: "weftdb-stored", out: ValueOut::Decimal, batch_rows: DEFAULT_INGEST_BATCH_ROWS, downsamples: true, stored: None }
	}

	/// `weftdb-stored-f64`: the same store, `f64` out. Its downsample would repeat
	/// `weftdb-stored`'s exactly, so it reports none.
	#[must_use]
	pub const fn stored_f64() -> Self {
		Self { name: "weftdb-stored-f64", out: ValueOut::F64, batch_rows: DEFAULT_INGEST_BATCH_ROWS, downsamples: false, stored: None }
	}

	/// `weftdb-stored-bulk`: one segment per load, the shape of a single bulk transaction, exact
	/// decimals out.
	#[must_use]
	pub const fn stored_bulk() -> Self {
		Self { name: "weftdb-stored-bulk", out: ValueOut::Decimal, batch_rows: usize::MAX, downsamples: true, stored: None }
	}

	fn loaded(&self) -> anyhow::Result<&Stored> {
		self.stored.as_ref().ok_or_else(|| anyhow::anyhow!("{}: nothing loaded", self.name))
	}
}

/// `t` as a nanosecond epoch.
fn nanos(t: chrono::DateTime<chrono::Utc>) -> anyhow::Result<i64> {
	t.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("instant {t} is outside the nanosecond range"))
}

/// Total size of the files under `dir`.
fn dir_bytes(dir: &Path) -> u64 {
	std::fs::read_dir(dir).map_or(0, |entries| {
		entries.flatten()
			.map(|e| match e.metadata() {
				Ok(m) if m.is_dir() => dir_bytes(&e.path()),
				Ok(m) => m.len(),
				Err(_) => 0,
			})
			.sum()
	})
}

/// Open a store in `dir`, declare the aspect and seal the rows in `batch_rows`-row segments.
async fn seal(dir: &Path, physical_type: PhysicalType, timestamps: &[i64], values: &[BigDecimal], batch_rows: usize) -> anyhow::Result<SegmentStore> {
	let store = SegmentStore::open(dir).await?;
	store.declare(ASPECT, &AspectSchema::new(physical_type, BigDecimal::from(0), TimeUnit::Nanos)).await?;
	for (ts, vs) in timestamps.chunks(batch_rows).zip(values.chunks(batch_rows)) {
		store.seal_declared(ASPECT, ts, vs).await?;
	}
	Ok(store)
}

impl Stored {
	fn store(&self) -> anyhow::Result<&SegmentStore> {
		self.store.as_ref().ok_or_else(|| anyhow::anyhow!("{}: the store did not reopen", self.dir.display()))
	}

	/// The read window's bounds and a margin that takes in every stored row.
	fn window(&self, ds: &Dataset) -> anyhow::Result<(i64, i64, i64)> {
		let (lo, hi) = (nanos(ds.start)?, nanos(ds.end)?);
		Ok((lo, hi, lo.saturating_sub(self.first).max(self.last.saturating_sub(hi)).max(0)))
	}

	async fn interpolate(&self, ds: &Dataset, out: ValueOut) -> anyhow::Result<Output> {
		let (lo, hi, margin) = self.window(ds)?;
		let interpolator = Interpolator::new(Spline::Linear, ds.resolution);
		Ok(match out {
			ValueOut::Decimal => Output::Decimal(self.store()?.interpolate_range(ASPECT, lo, hi, margin, interpolator).await?.into_points()),
			ValueOut::F64 => Output::Float(self.store()?.interpolate_range_f64(ASPECT, lo, hi, margin, interpolator).await?.into_parts().1),
		})
	}

	async fn downsample(&self, ds: &Dataset) -> anyhow::Result<Vec<BucketRow>> {
		BucketRow::from_weft(&self.store()?.downsample_range(ASPECT, self.first, self.last, ds.bucket, &DOWNSAMPLE_AGGREGATIONS).await?)
	}
}

impl Arm for WeftStore {
	fn info(&self) -> ArmInfo {
		let (value_type, call) = match self.out {
			ValueOut::Decimal => ("BigDecimal", "interpolate_range"),
			ValueOut::F64 => ("f64", "interpolate_range_f64"),
		};
		let layout = if self.batch_rows == usize::MAX { "one segment per load".to_string() } else { format!("{}-row segments", self.batch_rows) };
		ArmInfo { arm: self.name.into(), engine: "weftdb SegmentStore + splimes".into(), value_type: value_type.into(), method: format!("interpolation-on-read via SegmentStore::{call} (Backend::Auto), downsample via SegmentStore::downsample_range; {layout}"), class: Some(crate::ArmClass::Native), own_answer: None }
	}

	fn downsamples(&self) -> bool {
		self.downsamples
	}

	fn unload(&mut self) {
		self.stored = None;
	}

	async fn reopen(&mut self) -> anyhow::Result<()> {
		let name = self.name;
		let stored = self.stored.as_mut().ok_or_else(|| anyhow::anyhow!("{name}: nothing loaded"))?;
		stored.store = None;
		stored.store = Some(SegmentStore::open(&stored.dir).await?);
		Ok(())
	}

	async fn load(&mut self, ds: &Dataset) -> anyhow::Result<()> {
		// Dropping the old store removes its directory; the new one seals from scratch.
		self.stored = None;
		let timestamps = ds.points.iter().map(|p| nanos(p.timestamp)).collect::<anyhow::Result<Vec<i64>>>()?;
		let values: Vec<BigDecimal> = ds.points.iter().map(|p| p.value.clone()).collect();
		// The narrowest type that holds every value exactly, so the stored knots are the request's.
		let physical_type = weft_physical_type::recommend_encoding(&values, &BigDecimal::from(0)).physical_type;
		let dir = std::env::temp_dir().join(format!("weft-xbench-{}-{}-{}", self.name, std::process::id(), NEXT_STORE.fetch_add(1, Ordering::Relaxed)));
		std::fs::create_dir_all(&dir)?;
		let (first, last) = (timestamps[0], timestamps[timestamps.len() - 1]);
		self.stored = Some(Stored { store: Some(seal(&dir, physical_type, &timestamps, &values, self.batch_rows).await?), dir, first, last });
		Ok(())
	}

	fn storage_bytes(&self) -> Option<u64> {
		self.stored.as_ref().map(|s| dir_bytes(&s.dir))
	}

	async fn interpolate(&mut self, ds: &Dataset, _knots: Vec<splimes::Point>) -> anyhow::Result<Output> {
		self.loaded()?.interpolate(ds, self.out).await
	}

	async fn downsample(&mut self, ds: &Dataset, _knots: Vec<splimes::Point>) -> anyhow::Result<Vec<BucketRow>> {
		self.loaded()?.downsample(ds).await
	}
}

#[cfg(test)]
mod tests {
	use rand::{RngExt as _, SeedableRng};
	use rand_chacha::ChaCha8Rng;

	use super::*;

	/// `hosts` hosts with seeded readings every ten seconds for two hours from 2016-01-01, the
	/// cadence `tsbs_generate_data --log-interval=10s` writes, including some values far enough
	/// apart that a MAX taken over the wrong host or bucket shows.
	fn generated(hosts: usize) -> tsbs::TsbsCpu {
		let mut rng = ChaCha8Rng::seed_from_u64(341);
		let mut cpu = tsbs::TsbsCpu::default();
		for h in 0..hosts {
			let readings = (0..720_i64).map(|step| tsbs::CpuReading { ts_ns: 1_451_606_400_000_000_000 + step * 10_000_000_000, values: std::array::from_fn(|_| rng.random_range(0..100) + if rng.random_range(0..50) == 0 { 1_000 } else { 0 }) }).collect();
			cpu.hosts.insert(format!("host_{h}"), readings);
		}
		cpu
	}

	#[tokio::test]
	async fn the_store_answers_every_tsbs_query_as_the_reference_does() {
		let cpu = generated(10);
		let weft = TsbsWeft::load(&cpu).await.expect("loads");
		let t0 = 1_451_606_400_000_000_000_i64;
		let eight = ["host_0", "host_1", "host_2", "host_3", "host_5", "host_6", "host_8", "host_9"];
		for query in tsbs::TsbsQuery::ALL {
			match query {
				tsbs::TsbsQuery::SingleGroupby { metrics, hosts, hours } => {
					// A window that starts and ends inside a minute, as TSBS's random windows do,
					// clipped to the generated two hours for the 12-hour variants.
					let start = t0 + 25_000_000_000;
					let end = (start + i64::from(hours) * 3_600_000_000_000).min(t0 + 7_200_000_000_000) - 15_000_000_000;
					let named = &eight[..hosts];
					let want = cpu.single_groupby(metrics, named, start, end).expect("reference");
					assert!(want.len() > 50, "{}: a real answer, not an empty one", query.name());
					assert_eq!(weft.single_groupby(metrics, named, start, end).await.expect("store"), want, "{}", query.name());
				}
				tsbs::TsbsQuery::Lastpoint => {
					let want: Vec<(String, tsbs::CpuReading)> = cpu.lastpoint().into_iter().map(|(h, r)| (h.to_string(), r.clone())).collect();
					assert_eq!(weft.lastpoint().await.expect("store"), want);
				}
			}
		}
	}

	#[tokio::test]
	async fn a_wrong_host_or_metric_is_refused_not_answered_empty() {
		let weft = TsbsWeft::load(&generated(2)).await.expect("loads");
		assert!(weft.single_groupby(1, &["host_7"], 0, 1).await.expect_err("unknown host").to_string().contains("hostname=host_7"));
		assert!(weft.single_groupby(11, &["host_0"], 0, 1).await.is_err());
		assert!(weft.single_groupby(1, &["host_0"], 5, 5).await.is_err());
		drop(weft);
	}
}
