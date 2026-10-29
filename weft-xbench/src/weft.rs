//! The WeftDB arms: in-memory, stored (exact decimals out) and stored `f64`.

use std::{
	path::PathBuf, sync::atomic::{AtomicU64, Ordering}
};

use bigdecimal::BigDecimal;
use splimes::{Interpolator, Spline};
use weft_bench::{adapter::SystemAdapter, StoredWeftAdapter, WeftAdapter};
use weft_physical_type::{AspectSchema, TimeUnit};
use weftdb::SegmentStore;

use crate::{ArmInfo, Dataset, Output};

/// A benchmark arm: something that can load a dataset and reconstruct its grid.
#[allow(async_fn_in_trait, reason = "the runner dispatches statically; no arm crosses threads as a trait object")]
pub trait Arm {
	/// The arm's report, recorded with each result.
	fn info(&self) -> ArmInfo;

	/// Whether the arm has a load phase (the in-memory arm does not).
	fn loads(&self) -> bool {
		true
	}

	/// Load `ds` into fresh storage, replacing whatever an earlier call loaded. This is the
	/// timed load phase.
	///
	/// # Errors
	///
	/// If the engine refuses the data.
	async fn load(&mut self, ds: &Dataset) -> anyhow::Result<()>;

	/// Reconstruct `ds`'s grid from what [`load`](Arm::load) stored. This is the timed
	/// interpolation phase; `knots` is a fresh copy of the points for arms that take them per call.
	///
	/// # Errors
	///
	/// If the engine fails or nothing is loaded.
	async fn interpolate(&mut self, ds: &Dataset, knots: Vec<splimes::Point>) -> anyhow::Result<Output>;
}

/// WeftDB's in-process reconstruction: the points go straight to splimes. The reference arm.
#[derive(Debug, Default)]
pub struct WeftMemory;

impl Arm for WeftMemory {
	fn info(&self) -> ArmInfo {
		let report = WeftAdapter::new().adapter_info();
		ArmInfo { arm: "weftdb-memory".into(), engine: report.engine, value_type: "BigDecimal".into(), method: report.method }
	}

	fn loads(&self) -> bool {
		false
	}

	/// Nothing to load: the points go to splimes on every call.
	fn load(&mut self, _ds: &Dataset) -> impl std::future::Future<Output = anyhow::Result<()>> {
		std::future::ready(Ok(()))
	}

	async fn interpolate(&mut self, ds: &Dataset, mut knots: Vec<splimes::Point>) -> anyhow::Result<Output> {
		Ok(Output::Decimal(WeftAdapter::new().interpolate_range(&mut knots, ds.start, ds.end, ds.resolution, Spline::Linear).await?))
	}
}

/// WeftDB interpolating on read from a `SegmentStore` (EVI-11's adapter), exact decimals out.
#[derive(Debug, Default)]
pub struct WeftStored {
	adapter: Option<StoredWeftAdapter>,
}

impl Arm for WeftStored {
	fn info(&self) -> ArmInfo {
		let report = StoredWeftAdapter::new().adapter_info();
		ArmInfo { arm: "weftdb-stored".into(), engine: report.engine, value_type: "BigDecimal".into(), method: report.method }
	}

	async fn load(&mut self, ds: &Dataset) -> anyhow::Result<()> {
		// Dropping the old adapter removes its store; a fresh one seals from scratch.
		// Replacing the old adapter drops it, which removes its store; the new one seals from scratch.
		self.adapter.insert(StoredWeftAdapter::new()).prepare(&ds.points).await
	}

	async fn interpolate(&mut self, ds: &Dataset, mut knots: Vec<splimes::Point>) -> anyhow::Result<Output> {
		Ok(Output::Decimal(self.adapter.as_ref().ok_or_else(|| anyhow::anyhow!("weftdb-stored: nothing loaded"))?.interpolate_range(&mut knots, ds.start, ds.end, ds.resolution, Spline::Linear).await?))
	}
}

/// The aspect the `f64` arm stores its knots under.
const F64_ASPECT: &str = "xbench_f64";

/// Counter distinguishing the `f64` arm's store directories.
static NEXT_F64_STORE: AtomicU64 = AtomicU64::new(0);

/// A store and the directory it lives in, removed on drop (the store first).
struct F64Store {
	store: SegmentStore,
	dir: PathBuf,
	first: i64,
	last: i64,
}

impl Drop for F64Store {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.dir);
	}
}

/// WeftDB interpolating on read with `f64` values out (`SegmentStore::interpolate_range_f64`).
///
/// The same stored knots and reconstruction, without building a `BigDecimal` per output point.
/// The store layout is EVI-11's: 1024-row segments, the narrowest lossless value type,
/// nanosecond timestamps.
#[derive(Default)]
pub struct WeftStoredF64 {
	stored: Option<F64Store>,
}

impl std::fmt::Debug for WeftStoredF64 {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("WeftStoredF64").field("loaded", &self.stored.is_some()).finish()
	}
}

/// `t` as a nanosecond epoch.
fn nanos(t: chrono::DateTime<chrono::Utc>) -> anyhow::Result<i64> {
	t.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("instant {t} is outside the nanosecond range"))
}

impl Arm for WeftStoredF64 {
	fn info(&self) -> ArmInfo {
		ArmInfo { arm: "weftdb-stored-f64".into(), engine: "weftdb SegmentStore + splimes".into(), value_type: "f64".into(), method: "interpolation-on-read via SegmentStore::interpolate_range_f64 (Backend::Auto)".into() }
	}

	async fn load(&mut self, ds: &Dataset) -> anyhow::Result<()> {
		self.stored = None;
		let timestamps = ds.points.iter().map(|p| nanos(p.timestamp)).collect::<anyhow::Result<Vec<i64>>>()?;
		let values: Vec<BigDecimal> = ds.points.iter().map(|p| p.value.clone()).collect();
		let physical_type = weft_physical_type::recommend_encoding(&values, &BigDecimal::from(0)).physical_type;
		let dir = std::env::temp_dir().join(format!("weft-xbench-f64-{}-{}", std::process::id(), NEXT_F64_STORE.fetch_add(1, Ordering::Relaxed)));
		std::fs::create_dir_all(&dir)?;
		let (first, last) = (timestamps[0], timestamps[timestamps.len() - 1]);
		self.stored = Some(F64Store { store: seal(&dir, physical_type, &timestamps, &values).await?, dir, first, last });
		Ok(())
	}

	async fn interpolate(&mut self, ds: &Dataset, _knots: Vec<splimes::Point>) -> anyhow::Result<Output> {
		self.stored.as_ref().ok_or_else(|| anyhow::anyhow!("weftdb-stored-f64: nothing loaded"))?.interpolate(ds).await
	}
}

/// Open a store in `dir`, declare the arm's aspect and seal the rows in ingest-sized segments.
async fn seal(dir: &std::path::Path, physical_type: weft_physical_type::PhysicalType, timestamps: &[i64], values: &[BigDecimal]) -> anyhow::Result<SegmentStore> {
	let store = SegmentStore::open(dir).await?;
	store.declare(F64_ASPECT, &AspectSchema::new(physical_type, BigDecimal::from(0), TimeUnit::Nanos)).await?;
	for (ts, vs) in timestamps.chunks(weft_bench::DEFAULT_INGEST_BATCH_ROWS).zip(values.chunks(weft_bench::DEFAULT_INGEST_BATCH_ROWS)) {
		store.seal_declared(F64_ASPECT, ts, vs).await?;
	}
	Ok(store)
}

impl F64Store {
	/// Reconstruct `ds`'s grid from every stored row, as the other arms see every knot.
	async fn interpolate(&self, ds: &Dataset) -> anyhow::Result<Output> {
		let (lo, hi) = (nanos(ds.start)?, nanos(ds.end)?);
		let margin = lo.saturating_sub(self.first).max(self.last.saturating_sub(hi)).max(0);
		Ok(Output::Float(self.store.interpolate_range_f64(F64_ASPECT, lo, hi, margin, Interpolator::new(Spline::Linear, ds.resolution)).await?.into_parts().1))
	}
}
