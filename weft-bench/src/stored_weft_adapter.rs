//! Stored WeftDB system adapter.
//!
//! Where [`WeftAdapter`](crate::WeftAdapter) hands the request's points straight to splimes,
//! this adapter measures **interpolation-on-read over stored data**: the input is sealed into a
//! real [`SegmentStore`] once, in [`SystemAdapter::prepare`], and every timed call is
//! [`SegmentStore::interpolate_range`] — index pruning, decoding the overlapping `.weftseg`
//! frames, lifting the rows to knots and running the same splimes [`Interpolator`]. The gap
//! between the two adapters' latencies is the cost of the storage read path, which is what
//! commercial claim #1 (interpolation-on-read for large stored ranges) is about.
//!
//! The store lives in a temporary directory owned by the adapter and removed when it drops.
//! The aspect is declared with the narrowest *lossless* physical type for the input values
//! (the one the run's storage estimate costs) and nanosecond timestamps, chrono's own precision, so the stored knots
//! equal the request's knots and the reconstruction is the in-memory adapter's, point for
//! point. The read window's margin is widened to cover every stored row, again so both
//! adapters interpolate from the same knots.

use std::{
	path::PathBuf, sync::{
		atomic::{AtomicU64, Ordering}, Arc, Mutex
	}
};

use async_trait::async_trait;
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use splimes::{Interpolator, Point, Resolution, Spline};
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weftdb::SegmentStore;

use crate::{
	adapter::{AdapterInfo, SystemAdapter}, ingest::DEFAULT_INGEST_BATCH_ROWS
};

/// The aspect the prepared input is stored under.
const ASPECT: &str = "bench_stored";

/// Counter distinguishing the store directories of adapters alive at once.
static NEXT_STORE_ID: AtomicU64 = AtomicU64::new(0);

/// A temporary directory removed on drop.
#[derive(Debug)]
struct TempStoreDir(PathBuf);

impl Drop for TempStoreDir {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

/// What identifies a prepared input without re-reading it: row count, both end instants and
/// both end values. Checking it is O(1), so the timed call can refuse a request whose points
/// are not the ones stored instead of silently answering for different data.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Signature {
	rows: usize,
	first: (i64, BigDecimal),
	last: (i64, BigDecimal),
}

impl Signature {
	fn of(points: &[Point]) -> Option<Self> {
		let (first, last) = (points.first()?, points.last()?);
		Some(Self { rows: points.len(), first: (first.timestamp.timestamp_nanos_opt()?, first.value.clone()), last: (last.timestamp.timestamp_nanos_opt()?, last.value.clone()) })
	}
}

/// A sealed store and what was sealed into it. Field order matters: the store drops before
/// its directory is removed.
struct Prepared {
	store: SegmentStore,
	signature: Signature,
	#[cfg_attr(not(test), allow(dead_code, reason = "held for its `Drop`, which removes the store directory"))]
	dir: TempStoreDir,
}

/// Adapter that benchmarks interpolation over WeftDB's stored segments.
pub struct StoredWeftAdapter {
	batch_rows: usize,
	prepared: Mutex<Option<Arc<Prepared>>>,
}

impl std::fmt::Debug for StoredWeftAdapter {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("StoredWeftAdapter").field("batch_rows", &self.batch_rows).finish_non_exhaustive()
	}
}

impl Default for StoredWeftAdapter {
	fn default() -> Self {
		Self::new()
	}
}

impl StoredWeftAdapter {
	/// Construct a stored adapter that seals its input in
	/// [`DEFAULT_INGEST_BATCH_ROWS`]-row segments, the ingest workload's batch.
	#[must_use]
	pub const fn new() -> Self {
		Self::with_batch_rows(DEFAULT_INGEST_BATCH_ROWS)
	}

	/// Construct a stored adapter that seals `batch_rows` rows per segment. More, smaller
	/// segments make the read path prune and decode more frames for the same window.
	#[must_use]
	pub const fn with_batch_rows(batch_rows: usize) -> Self {
		Self { batch_rows, prepared: Mutex::new(None) }
	}

	fn current(&self) -> Option<Arc<Prepared>> {
		self.prepared.lock().map_or_else(|poisoned| poisoned.into_inner().clone(), |guard| guard.clone())
	}
}

/// `instant` as a nanosecond epoch, or an error outside the `i64` nanosecond range
/// (roughly 1677–2262).
fn nanos(instant: DateTime<Utc>) -> anyhow::Result<i64> {
	instant.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("stored adapter: instant {instant} is outside the nanosecond range a stored aspect holds"))
}

/// `points` as nanosecond epochs, refusing what the store would not hold as given: an
/// instant outside the nanosecond range, or out-of-order rows.
fn epochs(points: &[Point]) -> anyhow::Result<Vec<i64>> {
	let mut out = Vec::with_capacity(points.len());
	for p in points {
		let t = nanos(p.timestamp)?;
		anyhow::ensure!(out.last().is_none_or(|prev| *prev <= t), "stored adapter: input is not sorted by time at {}", p.timestamp);
		out.push(t);
	}
	Ok(out)
}

#[async_trait]
impl SystemAdapter for StoredWeftAdapter {
	fn name(&self) -> &'static str {
		"weftdb-stored"
	}

	fn adapter_info(&self) -> AdapterInfo {
		// Read off the code below: the timed call is `SegmentStore::interpolate_range`, which
		// runs splimes' `Interpolator`, built from the requested `spline` unchanged.
		AdapterInfo { interface_version: 2, engine: "weftdb SegmentStore + splimes".to_string(), method: "interpolation-on-read via SegmentStore::interpolate_range (Backend::Auto)".to_string(), native_engine: true, honours_requested_spline: true }
	}

	async fn prepare(&self, points: &[Point]) -> anyhow::Result<()> {
		anyhow::ensure!(self.batch_rows > 0, "stored adapter: batch_rows must be > 0");
		let timestamps = epochs(points)?;
		let signature = Signature::of(points).ok_or_else(|| anyhow::anyhow!("stored adapter: nothing to store"))?;
		if self.current().is_some_and(|p| p.signature == signature) {
			return Ok(());
		}
		let values: Vec<BigDecimal> = points.iter().map(|p| p.value.clone()).collect();
		// The narrowest physical type that holds every value exactly; anything lossy would make
		// the stored reconstruction differ from the request's for a reason that is not the read path.
		let physical_type = weft_physical_type::recommend_encoding(&values, &BigDecimal::from(0)).physical_type;

		let root = std::env::temp_dir().join(format!("weft-bench-stored-{}-{}", std::process::id(), NEXT_STORE_ID.fetch_add(1, Ordering::Relaxed)));
		std::fs::create_dir_all(&root).map_err(|e| anyhow::anyhow!("cannot create stored-adapter dir {}: {e}", root.display()))?;
		let dir = TempStoreDir(root.clone());
		let store = seal(&root, physical_type, &timestamps, &values, self.batch_rows).await?;
		*self.prepared.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(Prepared { store, signature, dir }));
		Ok(())
	}

	async fn interpolate_range(&self, points: &mut [Point], start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline: Spline) -> anyhow::Result<Vec<Point>> {
		// Ingest is never timed here: a call for points that were not prepared is refused, not
		// quietly stored, so a missing `prepare` cannot turn a read benchmark into a write one.
		self.current().ok_or_else(|| anyhow::anyhow!("stored adapter: interpolate_range called before prepare"))?.interpolate(points, start, end, Interpolator::new(spline, resolution)).await
	}
}

/// Open a store in `root`, declare the bench aspect as `physical_type` with nanosecond
/// timestamps, and seal the rows in `batch_rows`-row segments.
async fn seal(root: &std::path::Path, physical_type: PhysicalType, timestamps: &[i64], values: &[BigDecimal], batch_rows: usize) -> anyhow::Result<SegmentStore> {
	let store = SegmentStore::open(root).await.map_err(|e| anyhow::anyhow!("cannot open stored-adapter store in {}: {e:#}", root.display()))?;
	store.declare(ASPECT, &AspectSchema::new(physical_type, BigDecimal::from(0), TimeUnit::Nanos)).await.map_err(|e| anyhow::anyhow!("cannot declare aspect {ASPECT}: {e:#}"))?;
	for (chunk_ts, chunk_values) in timestamps.chunks(batch_rows).zip(values.chunks(batch_rows)) {
		store.seal_declared(ASPECT, chunk_ts, chunk_values).await.map_err(|e| anyhow::anyhow!("cannot seal {} rows into aspect {ASPECT}: {e:#}", chunk_ts.len()))?;
	}
	Ok(store)
}

impl Prepared {
	/// Reconstruct `[start, end]` from the stored rows, refusing `points` that are not the ones
	/// stored.
	async fn interpolate(&self, points: &[Point], start: DateTime<Utc>, end: DateTime<Utc>, interpolator: Interpolator) -> anyhow::Result<Vec<Point>> {
		anyhow::ensure!(Signature::of(points).as_ref() == Some(&self.signature), "stored adapter: the request's points are not the ones prepared");
		let (lo, hi) = (nanos(start)?, nanos(end)?);
		// Read every stored row, as the in-memory adapter sees every request point.
		let margin = lo.saturating_sub(self.signature.first.0).max(self.signature.last.0.saturating_sub(hi)).max(0);
		Ok(self.store.interpolate_range(ASPECT, lo, hi, margin, interpolator).await?.into_points())
	}
}

#[cfg(test)]
mod tests {
	use chrono::TimeZone;

	use super::*;
	use crate::WeftAdapter;

	fn ts(secs: i64) -> DateTime<Utc> {
		Utc.timestamp_opt(secs, 0).single().expect("valid instant")
	}

	fn input() -> Vec<Point> {
		// Irregular spacing and decimal values, so neither the grid nor the encoding is trivial.
		[(0, "1.5"), (7, "2.25"), (9, "-0.75"), (20, "4.125"), (31, "3"), (40, "0.5")].iter().map(|&(t, v)| Point::new(ts(t), v.parse().expect("decimal"))).collect()
	}

	#[test]
	fn report_names_the_store_and_the_facts_the_code_shows() {
		let info = StoredWeftAdapter::new().adapter_info();
		assert_eq!(info.interface_version, 2);
		assert!(info.engine.contains("SegmentStore"));
		assert!(info.native_engine);
		assert!(info.honours_requested_spline, "the requested spline reaches Interpolator::new unchanged");
	}

	#[tokio::test]
	async fn the_stored_reconstruction_is_the_in_memory_one() {
		let stored = StoredWeftAdapter::with_batch_rows(2);
		stored.prepare(&input()).await.expect("prepares");
		for spline in [Spline::Linear, Spline::Cubic] {
			let expected = WeftAdapter::new().interpolate_range(&mut input(), ts(3), ts(38), Resolution::Seconds, spline).await.expect("in memory");
			let got = stored.interpolate_range(&mut input(), ts(3), ts(38), Resolution::Seconds, spline).await.expect("stored");
			assert_eq!(got.len(), expected.len(), "{spline:?}");
			for (g, e) in got.iter().zip(&expected) {
				assert_eq!((g.timestamp, &g.value), (e.timestamp, &e.value), "{spline:?}");
			}
		}
		drop(stored);
	}

	#[tokio::test]
	async fn a_call_before_prepare_or_for_other_points_is_refused() {
		let stored = StoredWeftAdapter::new();
		let err = stored.interpolate_range(&mut input(), ts(0), ts(40), Resolution::Seconds, Spline::Linear).await.expect_err("not prepared");
		assert!(err.to_string().contains("before prepare"), "{err}");
		stored.prepare(&input()).await.expect("prepares");
		let mut other = input();
		other.pop();
		let err = stored.interpolate_range(&mut other, ts(0), ts(31), Resolution::Seconds, Spline::Linear).await.expect_err("different points");
		drop(stored);
		assert!(err.to_string().contains("not the ones prepared"), "{err}");
	}

	#[tokio::test]
	async fn preparing_the_same_input_twice_reuses_the_store_and_dropping_removes_it() {
		let stored = StoredWeftAdapter::new();
		stored.prepare(&input()).await.expect("prepares");
		let first = stored.current().expect("prepared");
		stored.prepare(&input()).await.expect("prepares again");
		assert!(Arc::ptr_eq(&first, &stored.current().expect("prepared")), "the same input is not sealed twice");
		let dir = first.dir.0.clone();
		drop(first);
		assert!(dir.exists());
		drop(stored);
		assert!(!dir.exists(), "the store directory goes with the adapter");
	}

	#[tokio::test]
	async fn unsorted_or_out_of_range_input_is_refused() {
		let mut unsorted = input();
		unsorted.swap(1, 2);
		assert!(StoredWeftAdapter::new().prepare(&unsorted).await.is_err());
		let beyond = vec![Point::new(ts(0), BigDecimal::from(1)), Point::new(ts(10_000_000_000), BigDecimal::from(2))];
		assert!(StoredWeftAdapter::new().prepare(&beyond).await.is_err(), "year 2286 is past the i64 nanosecond range");
	}
}
