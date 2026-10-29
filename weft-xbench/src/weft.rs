//! The WeftDB arms: in-memory, and stored with exact-decimal or `f64` output in two segment layouts.

use std::{
	future::Future, path::{Path, PathBuf}, sync::atomic::{AtomicU64, Ordering}
};

use bigdecimal::BigDecimal;
use splimes::{Interpolator, Spline};
use weft_bench::{adapter::SystemAdapter, WeftAdapter, DEFAULT_INGEST_BATCH_ROWS};
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weftdb::SegmentStore;

use crate::{ArmInfo, BucketRow, Dataset, Output, DOWNSAMPLE_AGGREGATIONS};

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

	/// Whether the arm has a downsample of its own. An arm that would run the same reduction as
	/// another arm over the same store reports `false` instead of a duplicate figure.
	fn downsamples(&self) -> bool {
		true
	}

	/// Load `ds` into fresh storage, replacing whatever an earlier call loaded. This is the
	/// timed load phase.
	///
	/// # Errors
	///
	/// If the engine refuses the data.
	async fn load(&mut self, ds: &Dataset) -> anyhow::Result<()>;

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
		ArmInfo { arm: "weftdb-memory".into(), engine: report.engine, value_type: "BigDecimal".into(), method: format!("{}; downsample via weft_reduce::reduce", report.method) }
	}

	fn loads(&self) -> bool {
		false
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

/// A loaded store and the directory it lives in, removed on drop (the store first).
struct Stored {
	store: SegmentStore,
	dir: PathBuf,
	first: i64,
	last: i64,
}

impl Drop for Stored {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.dir);
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
	/// The read window's bounds and a margin that takes in every stored row.
	fn window(&self, ds: &Dataset) -> anyhow::Result<(i64, i64, i64)> {
		let (lo, hi) = (nanos(ds.start)?, nanos(ds.end)?);
		Ok((lo, hi, lo.saturating_sub(self.first).max(self.last.saturating_sub(hi)).max(0)))
	}

	async fn interpolate(&self, ds: &Dataset, out: ValueOut) -> anyhow::Result<Output> {
		let (lo, hi, margin) = self.window(ds)?;
		let interpolator = Interpolator::new(Spline::Linear, ds.resolution);
		Ok(match out {
			ValueOut::Decimal => Output::Decimal(self.store.interpolate_range(ASPECT, lo, hi, margin, interpolator).await?.into_points()),
			ValueOut::F64 => Output::Float(self.store.interpolate_range_f64(ASPECT, lo, hi, margin, interpolator).await?.into_parts().1),
		})
	}

	async fn downsample(&self, ds: &Dataset) -> anyhow::Result<Vec<BucketRow>> {
		BucketRow::from_weft(&self.store.downsample_range(ASPECT, self.first, self.last, ds.bucket, &DOWNSAMPLE_AGGREGATIONS).await?)
	}
}

impl Arm for WeftStore {
	fn info(&self) -> ArmInfo {
		let (value_type, call) = match self.out {
			ValueOut::Decimal => ("BigDecimal", "interpolate_range"),
			ValueOut::F64 => ("f64", "interpolate_range_f64"),
		};
		let layout = if self.batch_rows == usize::MAX { "one segment per load".to_string() } else { format!("{}-row segments", self.batch_rows) };
		ArmInfo { arm: self.name.into(), engine: "weftdb SegmentStore + splimes".into(), value_type: value_type.into(), method: format!("interpolation-on-read via SegmentStore::{call} (Backend::Auto), downsample via SegmentStore::downsample_range; {layout}") }
	}

	fn downsamples(&self) -> bool {
		self.downsamples
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
		self.stored = Some(Stored { store: seal(&dir, physical_type, &timestamps, &values, self.batch_rows).await?, dir, first, last });
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
