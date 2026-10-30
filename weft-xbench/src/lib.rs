//! Cross-engine interpolation benchmark (evidence track EVI-13).
//!
//! The first report's scope is the 1.0 plan's decision 18: WeftDB — **stored, in-memory and
//! `f64`** — against `DuckDB` `DOUBLE` (and, in EVI-14, `DECIMAL`), on missing-completely-at-random
//! gaps, block missingness and the UCI household-power corpus, at three scales. This crate holds
//! the two arms that slice names, **load** and **interpolation**:
//!
//! - **load** times getting a dataset into durable storage: WeftDB's `SegmentStore` seal against
//!   `DuckDB`'s appender followed by `CHECKPOINT`. The in-memory arm has nothing to load and reports
//!   no load figure rather than a zero.
//! - **interpolation** times reconstructing a regular grid from the loaded data with
//!   piecewise-linear interpolation, the one method every arm implements. `DuckDB` has no native
//!   interpolation, so its arm is the SQL a `DuckDB` user would write: two `ASOF JOIN`s for the knots
//!   either side of each grid instant, extended past the data along the first and last segments'
//!   lines exactly as splimes' `Linear` does.
//!
//! Every arm is checked against the in-memory WeftDB reconstruction on the same grid (the
//! reference), and synthetic datasets are also scored against their analytic ground truth. A
//! result whose grid or values disagree with the reference fails its correctness gate, and the
//! report says so beside its timing instead of dropping it.
//!
//! Both engines write to [`std::env::temp_dir`]; point `TMPDIR` at the reference volume (decision
//! 19: `/mnt/weftbench`) so they share a disk.

#![warn(clippy::pedantic, clippy::nursery, clippy::all)]

pub mod duck;
pub mod render;
pub mod weft;

use std::time::Instant;

use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use chrono::{DateTime, Utc};
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};
use splimes::{Point, Resolution, Spline};
use weft_bench::{
	accuracy::{synthetic_ground_truth, AccuracyMetrics}, report::RunMetadata, scale::ScalePreset, stats::{BootstrapConfig, LatencyCis, LatencyStats}, uci::UciCorpus, InterpolationProfile, SignalShape, SyntheticParams
};

/// Version of the report this crate writes. Separate from `weft-bench`'s schema: the shapes differ.
///
/// v2 (EVI-15R) adds `invocation`, `scratch_dir` and `optimized`, which the renderer's
/// reproduction and caveat sections read; a v1 report deserializes with them empty.
///
/// v3 (EVI-14) adds the `downsample` phase, the `storage_bytes` of a load, and three arms
/// (`weftdb-stored-bulk`, `duckdb-decimal`, and a store-backed `weftdb-stored`).
pub const XBENCH_SCHEMA_VERSION: u32 = 3;

/// The reductions the downsample phase computes per bucket.
pub const DOWNSAMPLE_AGGREGATIONS: [weft_reduce::Aggregation; 3] = [weft_reduce::Aggregation::Min, weft_reduce::Aggregation::Max, weft_reduce::Aggregation::Avg];

/// Seed for every generated dataset unless the caller gives another.
pub const DEFAULT_SEED: u64 = 0x5EED_E713;

/// Decimal places synthetic values are rounded to, so every engine stores the same exact numbers
/// and EVI-14's `DECIMAL` arm can hold them.
pub const SYNTHETIC_DECIMALS: u32 = 6;

/// Share of samples removed from the synthetic datasets, by either missingness pattern.
pub const MISSINGNESS: f64 = 0.20;

/// How missing samples are distributed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatasetKind {
	/// Synthetic signal, each sample dropped independently (missing completely at random).
	Mcar,
	/// Synthetic signal with contiguous runs of samples removed (an outage).
	Block,
	/// The UCI household-power corpus: real data, its own gaps, no ground truth.
	Uci,
}

/// One benchmark input: the knots every arm loads and the grid every arm fills.
#[derive(Debug, Clone)]
pub struct Dataset {
	/// Name recorded in the report (`<kind>-<scale>`).
	pub name: String,
	/// Missingness pattern.
	pub kind: DatasetKind,
	/// Size preset.
	pub scale: ScalePreset,
	/// Time-sorted knots.
	pub points: Vec<Point>,
	/// First grid instant.
	pub start: DateTime<Utc>,
	/// Last grid instant (inclusive).
	pub end: DateTime<Utc>,
	/// Grid step.
	pub resolution: Resolution,
	/// Analytic ground truth on the grid, for synthetic datasets.
	pub truth: Option<Vec<f64>>,
	/// Share of the source series that is missing.
	pub missingness_fraction: f64,
	/// Bucket width for the downsample phase.
	pub bucket: Resolution,
}

/// One downsample bucket, as `f64` for comparison across engines.
#[derive(Debug, Clone, PartialEq)]
pub struct BucketRow {
	/// The bucket's epoch-aligned start, in nanoseconds.
	pub start_ns: i64,
	/// Rows in the bucket.
	pub count: u64,
	/// Smallest value.
	pub min: f64,
	/// Largest value.
	pub max: f64,
	/// Mean.
	pub avg: f64,
}

impl BucketRow {
	/// `weft_reduce`'s buckets as rows.
	///
	/// # Errors
	///
	/// If a bucket lacks one of [`DOWNSAMPLE_AGGREGATIONS`] or its start is out of range.
	pub fn from_weft(buckets: &[weft_reduce::Bucket]) -> anyhow::Result<Vec<Self>> {
		buckets.iter()
			.map(|b| {
				let get = |k: &str| b.values.get(k).and_then(ToPrimitive::to_f64).ok_or_else(|| anyhow::anyhow!("bucket at {} has no {k}", b.timestamp));
				Ok(Self { start_ns: b.timestamp.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("bucket start {} out of range", b.timestamp))?, count: b.count as u64, min: get("min")?, max: get("max")?, avg: get("avg")? })
			})
			.collect()
	}
}

impl Dataset {
	/// Number of instants on the grid.
	#[must_use]
	pub fn grid_len(&self) -> usize {
		weft_bench::adapter::grid_timestamps(self.start, self.end, self.resolution).len()
	}

	/// A synthetic dataset over one day at `scale.point_count()` samples, reconstructed per second.
	///
	/// Both patterns start from the same seeded profile; `Block` generates it gap-free and then
	/// removes runs, so the two differ only in where the gaps fall. The ground truth is the
	/// profile's analytic signal, which removal does not change.
	///
	/// # Errors
	///
	/// If `kind` is [`DatasetKind::Uci`] (use [`Dataset::uci`]) or the truth cannot be computed.
	pub fn synthetic(kind: DatasetKind, scale: ScalePreset, seed: u64) -> anyhow::Result<Self> {
		let missingness = match kind {
			DatasetKind::Mcar => MISSINGNESS,
			DatasetKind::Block => 0.0,
			DatasetKind::Uci => anyhow::bail!("a UCI dataset is loaded with Dataset::uci"),
		};
		let params = SyntheticParams { seed, input_points: scale.point_count(), missingness_fraction: missingness, signal_shape: SignalShape::MultiSine, spline: Spline::Linear, resolution: Resolution::Seconds, value_decimals: Some(SYNTHETIC_DECIMALS), ..SyntheticParams::default() };
		let name = format!("{}-{}", kind.as_str(), scale.as_str());
		let profile = InterpolationProfile::synthetic(name.clone(), params);
		let generated = profile.generate();
		let (points, missingness_fraction) = if kind == DatasetKind::Block {
			let before = generated.len();
			let kept = remove_blocks(generated, MISSINGNESS, seed);
			#[allow(clippy::cast_precision_loss)]
			let removed = (before - kept.len()) as f64 / before as f64;
			(kept, removed)
		} else {
			(generated, MISSINGNESS)
		};
		let truth = synthetic_ground_truth(&profile)?;
		Ok(Self { name, kind, scale, points, start: profile.start(), end: profile.end(), resolution: profile.resolution, truth: Some(truth), missingness_fraction, bucket: Resolution::Minutes })
	}

	/// The first `scale.point_count()` rows of a UCI corpus, reconstructed per minute (the corpus'
	/// own cadence) from its first to its last instant.
	///
	/// # Errors
	///
	/// If the corpus has fewer than two rows or an instant outside chrono's range.
	pub fn uci(corpus: &UciCorpus, scale: ScalePreset) -> anyhow::Result<Self> {
		let rows = scale.point_count().min(corpus.timestamps.len());
		anyhow::ensure!(rows >= 2, "the UCI corpus needs at least two rows, found {rows}");
		let points = corpus.timestamps[..rows].iter().zip(&corpus.values[..rows]).map(|(&t, v)| DateTime::<Utc>::from_timestamp(t, 0).map(|at| Point::new(at, v.clone())).ok_or_else(|| anyhow::anyhow!("UCI instant {t} is out of range"))).collect::<anyhow::Result<Vec<_>>>()?;
		let (start, end) = (points[0].timestamp, points[rows - 1].timestamp);
		Ok(Self { name: format!("uci-{}", scale.as_str()), kind: DatasetKind::Uci, scale, points, start, end, resolution: Resolution::Minutes, truth: None, missingness_fraction: corpus.missingness_fraction(), bucket: Resolution::Hours })
	}
}

impl DatasetKind {
	/// Stable label used in dataset names.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Mcar => "mcar",
			Self::Block => "block",
			Self::Uci => "uci",
		}
	}
}

/// Remove about `fraction` of `points` as contiguous runs, seeded. A run is 1/200 of the series
/// (at least ten samples), so a small series still has outages several samples long; the first and
/// last samples are kept so the grid's ends stay inside the data.
fn remove_blocks(points: Vec<Point>, fraction: f64, seed: u64) -> Vec<Point> {
	let n = points.len();
	if n < 4 {
		return points;
	}
	let run = (n / 200).max(10).min(n / 4).max(1);
	#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
	let target = (n as f64 * fraction) as usize;
	let mut drop = vec![false; n];
	let mut dropped = 0;
	let mut rng = ChaCha8Rng::seed_from_u64(seed ^ 0xB10C);
	let mut attempts = 0;
	while dropped < target && attempts < 100_000 {
		attempts += 1;
		let at = rng.random_range(1..n - run);
		for flag in &mut drop[at..at + run] {
			if !*flag {
				*flag = true;
				dropped += 1;
			}
		}
	}
	points.into_iter().zip(drop).filter_map(|(p, d)| (!d).then_some(p)).collect()
}

/// What an arm reports about itself, recorded with each of its results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArmInfo {
	/// Stable arm name (`weftdb-memory`, `duckdb-double`, …).
	pub arm: String,
	/// Engine and version that produced the numbers.
	pub engine: String,
	/// The value type the engine computes and returns in.
	pub value_type: String,
	/// How the arm stores and reconstructs.
	pub method: String,
}

/// An arm's reconstruction in the type its engine returns; converted to `f64` only after timing.
#[derive(Debug)]
pub enum Output {
	/// Exact-decimal points (the `BigDecimal` WeftDB arms).
	Decimal(Vec<Point>),
	/// Binary floats on the grid, in grid order.
	Float(Vec<f64>),
}

impl Output {
	/// The values as `f64`, in grid order (`NaN` where a decimal has no `f64` image).
	#[must_use]
	pub fn to_f64(&self) -> Vec<f64> {
		match self {
			Self::Decimal(points) => points.iter().map(|p| p.value.to_f64().unwrap_or(f64::NAN)).collect(),
			Self::Float(values) => values.clone(),
		}
	}
}

/// One measured phase of one arm on one dataset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArmResult {
	/// Dataset name.
	pub dataset: String,
	/// Missingness pattern.
	pub kind: DatasetKind,
	/// Size preset name.
	pub scale: String,
	/// Knots loaded.
	pub input_points: usize,
	/// Grid instants reconstructed (interpolation phase) or rows loaded (load phase).
	pub output_points: usize,
	/// Share of the source series missing.
	pub missingness_fraction: f64,
	/// The arm's own report.
	pub arm: ArmInfo,
	/// `load` or `interpolate`.
	pub phase: Phase,
	/// Timed repetitions.
	pub reps: usize,
	/// Latency distribution over the reps.
	pub latency: LatencyStats,
	/// Bootstrap confidence intervals for the latency.
	pub latency_ci: LatencyCis,
	/// The correctness gate (interpolation phase only).
	pub correctness: Option<Correctness>,
	/// Error against the analytic ground truth (synthetic datasets, interpolation phase).
	pub accuracy: Option<AccuracyMetrics>,
	/// Bytes on disk after the load (load phase of a storing arm).
	#[serde(default)]
	pub storage_bytes: Option<u64>,
}

/// Which phase a result times.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
	/// Getting the dataset into durable storage.
	Load,
	/// Reconstructing the grid from loaded data.
	Interpolate,
	/// Reducing loaded data into buckets (count, min, max, avg).
	Downsample,
}

/// Agreement of an arm's grid with the in-memory WeftDB reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Correctness {
	/// Grid instants expected.
	pub expected_points: usize,
	/// Grid instants returned.
	pub actual_points: usize,
	/// Every returned value is finite.
	pub values_finite: bool,
	/// Largest absolute difference from the reference, over the grid.
	pub max_abs_diff_vs_reference: f64,
	/// The tolerance the difference was held to.
	pub tolerance: f64,
	/// Grid size matches, values are finite and within tolerance.
	pub passed: bool,
}

/// Relative tolerance against the reference.
///
/// Binary floating point and exact decimals agree to about 1e-12 of the series' magnitude on a
/// linear interpolation; anything past 1e-9 is a different answer, not rounding.
pub const RELATIVE_TOLERANCE: f64 = 1e-9;

impl Correctness {
	/// Compare buckets with the reference's: the same buckets, with the same counts, and min, max
	/// and avg within tolerance. A missing, extra or shifted bucket fails outright.
	#[must_use]
	pub fn check_buckets(rows: &[BucketRow], reference: &[BucketRow]) -> Self {
		let same_shape = rows.len() == reference.len() && rows.iter().zip(reference).all(|(a, b)| a.start_ns == b.start_ns && a.count == b.count);
		let flat = |rs: &[BucketRow]| rs.iter().flat_map(|r| [r.min, r.max, r.avg]).collect::<Vec<f64>>();
		let mut gate = Self::check(&flat(rows), &flat(reference), reference.len() * 3);
		gate.expected_points = reference.len();
		gate.actual_points = rows.len();
		if !same_shape {
			gate.max_abs_diff_vs_reference = f64::INFINITY;
			gate.passed = false;
		}
		gate
	}

	/// Compare `values` with `reference` on a grid of `expected` instants.
	#[must_use]
	pub fn check(values: &[f64], reference: &[f64], expected: usize) -> Self {
		let values_finite = values.iter().all(|v| v.is_finite());
		let max_abs_diff_vs_reference = if values.len() == reference.len() { values.iter().zip(reference).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max) } else { f64::INFINITY };
		let magnitude = reference.iter().map(|v| v.abs()).fold(1.0, f64::max);
		let tolerance = RELATIVE_TOLERANCE * magnitude;
		let passed = values.len() == expected && values_finite && max_abs_diff_vs_reference <= tolerance;
		Self { expected_points: expected, actual_points: values.len(), values_finite, max_abs_diff_vs_reference, tolerance, passed }
	}
}

/// Engine versions the run used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineVersion {
	/// Engine name.
	pub engine: String,
	/// Version string as the engine reports it.
	pub version: String,
}

/// The cross-engine report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XbenchReport {
	/// [`XBENCH_SCHEMA_VERSION`].
	pub schema_version: u32,
	/// Hardware, disk, toolchain: the same run metadata `weft-bench` records.
	pub metadata: RunMetadata,
	/// Engines and versions.
	pub engines: Vec<EngineVersion>,
	/// Seed for the synthetic datasets.
	pub seed: u64,
	/// The repository commit the WeftDB arms were built from, when the run could read it.
	pub source_commit: Option<String>,
	/// The command line that produced the report, for the reproduction section.
	#[serde(default)]
	pub invocation: Vec<String>,
	/// Where both engines wrote their files (`TMPDIR`).
	#[serde(default)]
	pub scratch_dir: Option<String>,
	/// Whether the binary was an optimised build (`None` in a v1 report, which did not record it).
	#[serde(default)]
	pub optimized: Option<bool>,
	/// Every arm × dataset × phase.
	pub results: Vec<ArmResult>,
}

impl XbenchReport {
	/// True when every interpolation result passed its correctness gate.
	#[must_use]
	pub fn all_correct(&self) -> bool {
		self.results.iter().filter_map(|r| r.correctness.as_ref()).all(|c| c.passed)
	}
}

/// Elapsed nanoseconds since `since`, saturated into a `u64`.
#[must_use]
pub fn span_ns(since: Instant) -> u64 {
	u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// Latency statistics and their bootstrap intervals, seeded from the dataset seed.
#[must_use]
pub fn latency(samples: &[u64], seed: u64) -> (LatencyStats, LatencyCis) {
	(LatencyStats::from_samples(samples), LatencyStats::bootstrap_cis(samples, &BootstrapConfig { seed: seed ^ 0x_C0FF_EE15_C0DE_u64, ..BootstrapConfig::default() }))
}

/// Accuracy of `values` (grid order) against `truth`, or `None` when the lengths differ.
#[must_use]
pub fn accuracy(values: &[f64], ds: &Dataset, truth: &[f64]) -> Option<AccuracyMetrics> {
	let grid = weft_bench::adapter::grid_timestamps(ds.start, ds.end, ds.resolution);
	if grid.len() != values.len() {
		return None;
	}
	let points: Vec<Point> = grid.into_iter().zip(values).map(|(t, v)| Point::new(t, BigDecimal::from_f64(*v).unwrap_or_default())).collect();
	AccuracyMetrics::from_aligned(&points, truth).ok()
}

/// Measure one arm on one dataset.
///
/// `load_reps` timed loads (when the arm loads), then `reps` timed reconstructions from the last load, each checked against `reference` (the in-memory
/// WeftDB grid, in `f64`) and, for synthetic data, scored against the ground truth.
///
/// Knot copies for the arms that take points per call are made before the timer starts.
///
/// # Errors
///
/// If `reps` or `load_reps` is zero, or the arm fails to load or reconstruct.
pub async fn measure<A: weft::Arm>(arm: &mut A, ds: &Dataset, reference: &Reference, load_reps: usize, reps: usize, seed: u64) -> anyhow::Result<Vec<ArmResult>> {
	anyhow::ensure!(reps > 0 && load_reps > 0, "reps and load reps must be > 0");
	let info = arm.info();
	let row = |phase, output_points, reps, samples: &[u64], correctness, accuracy, storage_bytes| {
		let (latency, latency_ci) = latency(samples, seed);
		ArmResult { dataset: ds.name.clone(), kind: ds.kind, scale: ds.scale.as_str().to_string(), input_points: ds.points.len(), output_points, missingness_fraction: ds.missingness_fraction, arm: info.clone(), phase, reps, latency, latency_ci, correctness, accuracy, storage_bytes }
	};
	let mut results = Vec::with_capacity(3);

	let mut load_samples = Vec::with_capacity(load_reps);
	for _ in 0..if arm.loads() { load_reps } else { 1 } {
		let t0 = Instant::now();
		arm.load(ds).await?;
		load_samples.push(span_ns(t0));
	}
	if arm.loads() {
		results.push(row(Phase::Load, ds.points.len(), load_reps, &load_samples, None, None, arm.storage_bytes()));
	}

	let mut samples = Vec::with_capacity(reps);
	let mut last = None;
	for _ in 0..reps {
		let knots = ds.points.clone();
		let t0 = Instant::now();
		let output = arm.interpolate(ds, knots).await?;
		samples.push(span_ns(t0));
		last = Some(output);
	}
	let values = last.map(|o| o.to_f64()).unwrap_or_default();
	let correctness = Correctness::check(&values, &reference.grid, ds.grid_len());
	let accuracy = ds.truth.as_deref().and_then(|truth| accuracy(&values, ds, truth));
	results.push(row(Phase::Interpolate, values.len(), reps, &samples, Some(correctness), accuracy, None));

	if arm.downsamples() {
		let mut samples = Vec::with_capacity(reps);
		let mut last = Vec::new();
		for _ in 0..reps {
			let knots = ds.points.clone();
			let t0 = Instant::now();
			let buckets = arm.downsample(ds, knots).await?;
			samples.push(span_ns(t0));
			last = buckets;
		}
		let correctness = Correctness::check_buckets(&last, &reference.buckets);
		results.push(row(Phase::Downsample, last.len(), reps, &samples, Some(correctness), None, None));
	}
	Ok(results)
}

/// What every arm is checked against: WeftDB's in-memory grid (`f64`) and `weft_reduce`'s buckets.
#[derive(Debug, Clone)]
pub struct Reference {
	/// The in-memory reconstruction, in grid order.
	pub grid: Vec<f64>,
	/// The in-memory reduction, in bucket order.
	pub buckets: Vec<BucketRow>,
}

/// The in-memory WeftDB reconstruction of `ds`, in `f64`: the reference every arm is checked against.
///
/// # Errors
///
/// If splimes refuses the dataset.
pub async fn reference(ds: &Dataset) -> anyhow::Result<Reference> {
	use weft::Arm;
	let grid = weft::WeftMemory.interpolate(ds, ds.points.clone()).await?.to_f64();
	let buckets = weft::WeftMemory.downsample(ds, ds.points.clone()).await?;
	Ok(Reference { grid, buckets })
}

/// Run every arm over every dataset.
///
/// # Errors
///
/// If any arm fails outright; a wrong answer is not an error, it fails that result's gate.
pub async fn run(datasets: &[Dataset], load_reps: usize, reps: usize, seed: u64, metadata: RunMetadata, source_commit: Option<String>, invocation: Vec<String>) -> anyhow::Result<XbenchReport> {
	let mut results = Vec::new();
	for ds in datasets {
		let reference = reference(ds).await?;
		results.extend(measure(&mut weft::WeftMemory, ds, &reference, load_reps, reps, seed).await?);
		results.extend(measure(&mut weft::WeftStore::stored(), ds, &reference, load_reps, reps, seed).await?);
		results.extend(measure(&mut weft::WeftStore::stored_f64(), ds, &reference, load_reps, reps, seed).await?);
		results.extend(measure(&mut weft::WeftStore::stored_bulk(), ds, &reference, load_reps, reps, seed).await?);
		results.extend(measure(&mut duck::Duck::double(), ds, &reference, load_reps, reps, seed).await?);
		results.extend(measure(&mut duck::Duck::decimal(), ds, &reference, load_reps, reps, seed).await?);
	}
	let engines = vec![EngineVersion { engine: "weftdb".into(), version: metadata.weft_bench_version.clone() }, EngineVersion { engine: "duckdb".into(), version: duck::duckdb_version()? }];
	Ok(XbenchReport { schema_version: XBENCH_SCHEMA_VERSION, metadata, engines, seed, source_commit, invocation, scratch_dir: Some(std::env::temp_dir().display().to_string()), optimized: Some(!cfg!(debug_assertions)), results })
}

#[cfg(test)]
mod tests {
	use chrono::TimeZone;
	use weft_bench::RunMetadata;

	use super::*;
	use crate::weft::Arm;

	fn ts(secs: i64) -> DateTime<Utc> {
		Utc.timestamp_opt(secs, 0).single().expect("valid instant")
	}

	/// A hand-made dataset whose grid starts before the first knot and ends after the last, so
	/// both extrapolation edges and exact knot hits are exercised.
	fn edge_dataset() -> Dataset {
		let points = [(10, "1.5"), (13, "2.25"), (14, "-0.75"), (20, "4.125"), (27, "3"), (31, "0.5")].iter().map(|&(t, v)| Point::new(ts(t), v.parse().expect("decimal"))).collect();
		Dataset { name: "edge".into(), kind: DatasetKind::Mcar, scale: ScalePreset::Tiny, points, start: ts(5), end: ts(36), resolution: Resolution::Seconds, truth: None, missingness_fraction: 0.0, bucket: Resolution::Minutes }
	}

	async fn values_of<A: Arm>(arm: &mut A, ds: &Dataset) -> Vec<f64> {
		arm.load(ds).await.expect("loads");
		arm.interpolate(ds, ds.points.clone()).await.expect("interpolates").to_f64()
	}

	#[tokio::test]
	async fn every_arm_matches_the_reference_including_both_extrapolation_edges() {
		let ds = edge_dataset();
		let reference = reference(&ds).await.expect("reference").grid;
		assert_eq!(reference.len(), ds.grid_len());
		for (name, values) in [("stored", values_of(&mut weft::WeftStore::stored(), &ds).await), ("stored-f64", values_of(&mut weft::WeftStore::stored_f64(), &ds).await), ("stored-bulk", values_of(&mut weft::WeftStore::stored_bulk(), &ds).await), ("duckdb", values_of(&mut duck::Duck::double(), &ds).await), ("duckdb-decimal", values_of(&mut duck::Duck::decimal(), &ds).await)] {
			let gate = Correctness::check(&values, &reference, ds.grid_len());
			assert!(gate.passed, "{name}: {gate:?}\n got {values:?}\nwant {reference:?}");
		}
		// The first grid instant (5) lies before the first knot (10): the first segment's line
		// extended, 1.5 + (2.25 - 1.5) * (5 - 10) / 3 = 0.25.
		assert!((reference[0] - 0.25).abs() < 1e-12, "{}", reference[0]);
	}

	#[tokio::test]
	async fn duckdb_keeps_the_last_of_duplicate_instants_as_splimes_does() {
		let mut ds = edge_dataset();
		ds.points.insert(3, Point::new(ts(14), "9".parse().expect("decimal")));
		let reference = reference(&ds).await.expect("reference").grid;
		let values = values_of(&mut duck::Duck::double(), &ds).await;
		assert!(Correctness::check(&values, &reference, ds.grid_len()).passed, "got {values:?}\nwant {reference:?}");
	}

	#[test]
	fn block_removal_takes_contiguous_runs_near_the_target_share() {
		let ds = Dataset::synthetic(DatasetKind::Block, ScalePreset::Tiny, DEFAULT_SEED).expect("builds");
		assert!((0.18..=0.25).contains(&ds.missingness_fraction), "{}", ds.missingness_fraction);
		let mcar = Dataset::synthetic(DatasetKind::Mcar, ScalePreset::Tiny, DEFAULT_SEED).expect("builds");
		// Block gaps are long; MCAR gaps are mostly single samples, so the largest block gap is
		// several times the largest MCAR gap.
		let widest = |ds: &Dataset| ds.points.windows(2).map(|w| (w[1].timestamp - w[0].timestamp).num_milliseconds()).max().unwrap_or(0);
		assert!(widest(&ds) > 3 * widest(&mcar), "block {} vs mcar {}", widest(&ds), widest(&mcar));
		assert_eq!(ds.truth.as_ref().map(Vec::len), Some(ds.grid_len()));
	}

	#[test]
	fn a_shifted_missing_or_wrong_bucket_fails_the_gate() {
		let row = |start_ns, count, avg| BucketRow { start_ns, count, min: 0.0, max: 2.0, avg };
		let reference = [row(0, 3, 1.0), row(60, 2, 1.5)];
		assert!(Correctness::check_buckets(&reference, &reference).passed);
		assert!(!Correctness::check_buckets(&[row(0, 3, 1.0), row(120, 2, 1.5)], &reference).passed, "shifted");
		assert!(!Correctness::check_buckets(&[row(0, 3, 1.0), row(60, 1, 1.5)], &reference).passed, "count differs");
		assert!(!Correctness::check_buckets(&[row(0, 3, 1.0)], &reference).passed, "missing");
		assert!(!Correctness::check_buckets(&[row(0, 3, 1.0), row(60, 2, 1.6)], &reference).passed, "wrong avg");
	}

	#[test]
	fn a_wrong_or_short_grid_fails_the_gate() {
		let reference = [1.0, 2.0, 3.0];
		assert!(Correctness::check(&[1.0, 2.0, 3.0], &reference, 3).passed);
		assert!(!Correctness::check(&[1.0, 2.0, 3.1], &reference, 3).passed);
		assert!(!Correctness::check(&[1.0, 2.0], &reference, 3).passed);
		assert!(!Correctness::check(&[1.0, f64::NAN, 3.0], &reference, 3).passed);
	}

	#[tokio::test]
	async fn a_tiny_run_reports_both_phases_for_storing_arms_and_passes() {
		let datasets = [Dataset::synthetic(DatasetKind::Mcar, ScalePreset::Tiny, DEFAULT_SEED).expect("builds")];
		let report = run(&datasets, 1, 2, DEFAULT_SEED, RunMetadata::capture("test".into()), None, vec![]).await.expect("runs");
		// memory: interpolate + downsample; stored, stored-bulk, duckdb-double, duckdb-decimal:
		// load + interpolate + downsample; stored-f64: load + interpolate.
		assert_eq!(report.results.len(), 16);
		assert!(report.results.iter().filter(|r| r.phase == Phase::Load).all(|r| r.storage_bytes.is_some_and(|b| b > 0)), "every load reports its bytes on disk");
		assert!(report.all_correct(), "{:#?}", report.results.iter().filter_map(|r| r.correctness.as_ref()).collect::<Vec<_>>());
		assert!(report.results.iter().filter(|r| r.phase == Phase::Interpolate).all(|r| r.accuracy.is_some()));
		assert!(report.engines.iter().any(|e| e.engine == "duckdb" && !e.version.is_empty()));
	}
}
