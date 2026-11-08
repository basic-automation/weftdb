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
//! Since #148 every engine is also asked the fair protocol's other two ways (roadmap 1.2): its
//! native fill where it has one (class A, [`ArmClass::Native`]), reported with how far its answer
//! lies from the line because most compute something else, and its knots fetched into this client
//! and interpolated here (class C, [`ArmClass::ClientSide`], [`client_linear`]), gated like the SQL.
//!
//! Both engines write to [`std::env::temp_dir`]; point `TMPDIR` at the reference volume (decision
//! 19: `/mnt/weftbench`) so they share a disk.

#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]

pub mod clickhouse;
pub mod combine;
pub mod duck;
pub mod influxdb3;
pub mod iotdb;
pub mod questdb;
pub mod render;
pub mod timescale;
pub mod tsbs;
pub mod weft;

/// A count or timestamp as `f64`, `NaN` in the case a type has no `f64` form (no integer type is in that case).
#[must_use]
pub fn as_f64<T: ToPrimitive + Copy>(v: T) -> f64 {
	v.to_f64().unwrap_or(f64::NAN)
}

/// Substitute each `{name}` in a SQL template with the value paired with `name`.
pub(crate) fn fill(template: &str, args: &[(&str, &dyn std::fmt::Display)]) -> String {
	args.iter().fold(template.to_owned(), |sql, (name, value)| sql.replace(&format!("{{{name}}}"), &value.to_string()))
}

use std::time::Instant;

use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use chrono::{DateTime, Utc};
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};
use splimes::{Point, Resolution, Spline};
use weft_bench::{
	accuracy::{synthetic_ground_truth, AccuracyMetrics}, report::RunMetadata, scale::ScalePreset, schema::{BetweenRun, ResultDigest}, stats::{BootstrapConfig, LatencyCis, LatencyStats}, uci::UciCorpus, InterpolationProfile, SignalShape, SyntheticParams
};

/// Version of the report this crate writes. Separate from `weft-bench`'s schema: the shapes differ.
///
/// v2 (EVI-15R) adds `invocation`, `scratch_dir` and `optimized`, which the renderer's
/// reproduction and caveat sections read; a v1 report deserializes with them empty.
///
/// v3 (EVI-14) adds the `downsample` phase, the `storage_bytes` of a load, and three arms
/// (`weftdb-stored-bulk`, `duckdb-decimal`, and a store-backed `weftdb-stored`).
///
/// v4 (#146) adds each result's `warmth` (warm, OS-cold or restarted) and the report's
/// `cold_hook`; a v3 report deserializes with every result warm.
///
/// v5 (#147) keeps each result's raw `samples_ns`; an older report deserializes with none.
///
/// v6 (#147) adds each gate's `answer_sha256`, a combined report's `between_run` and
/// `combined_from`, and the run manifest: the `inputs` files' hashes, the `binary_sha256` and the
/// engine `containers`.
///
/// v7 (#148) adds each arm's interpolation `class` (native, portable SQL, client-side) and, for a
/// native fill that computes another answer, its `own_answer`, and each gate's `unfilled_points`.
///
/// v8 (#516) adds each gate's `indeterminate`: a digest disagreement where every run stayed inside
/// its tolerance is non-determinism, not a wrong answer and not a failure. The combined gate's
/// `passed` stays false to keep the row out of verdicts.
///
/// v9 (#452) adds `ArmInfo::adapter_workload: Option<bool>` and `ArmInfo::is_self_measured()`.
/// A result marked `adapter_workload: false` (imported from a non-adapter weft-bench workload) must
/// not be presented as a cross-engine comparison.
pub const XBENCH_SCHEMA_VERSION: u32 = 9;

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
	/// One host's field from TSBS's `cpu-only` generator: a gap-free random walk every ten
	/// seconds, no ground truth.
	Tsbs,
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
	/// If `kind` is [`DatasetKind::Uci`] or [`DatasetKind::Tsbs`] (use [`Dataset::uci`] or
	/// [`Dataset::tsbs`]) or the truth cannot be computed.
	pub fn synthetic(kind: DatasetKind, scale: ScalePreset, seed: u64) -> anyhow::Result<Self> {
		let missingness = match kind {
			DatasetKind::Mcar => MISSINGNESS,
			DatasetKind::Block => 0.0,
			DatasetKind::Uci | DatasetKind::Tsbs => anyhow::bail!("a {} dataset is loaded from its file", kind.as_str()),
		};
		let params = SyntheticParams { seed, input_points: scale.point_count(), missingness_fraction: missingness, signal_shape: SignalShape::MultiSine, spline: Spline::Linear, resolution: Resolution::Seconds, value_decimals: Some(SYNTHETIC_DECIMALS), ..SyntheticParams::default() };
		let name = format!("{}-{}", kind.as_str(), scale.as_str());
		let profile = InterpolationProfile::synthetic(name.clone(), params);
		let generated = profile.generate();
		let (points, missingness_fraction) = if kind == DatasetKind::Block {
			let before = generated.len();
			let kept = remove_blocks(generated, MISSINGNESS, seed);
			let removed = as_f64(before - kept.len()) / as_f64(before);
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

	/// A TSBS series over the synthetic datasets' span, `scale.point_count()` seconds from its
	/// first reading, reconstructed per second: the same grid as the synthetic datasets, from a
	/// knot every TSBS `--log-interval` instead of every second.
	///
	/// # Errors
	///
	/// If fewer than two readings fall in the span.
	pub fn tsbs(series: &tsbs::TsbsSeries, scale: ScalePreset) -> anyhow::Result<Self> {
		let start = series.points.first().ok_or_else(|| anyhow::anyhow!("the TSBS series is empty"))?.timestamp;
		let span = chrono::Duration::seconds(i64::try_from(scale.point_count())?);
		let points: Vec<Point> = series.points.iter().take_while(|p| p.timestamp - start <= span).cloned().collect();
		anyhow::ensure!(points.len() >= 2, "{} readings of {} fall in the {} span; at least two are needed", points.len(), series.host, scale.as_str());
		Ok(Self { name: format!("tsbs-{}", scale.as_str()), kind: DatasetKind::Tsbs, scale, start, end: start + span, points, resolution: Resolution::Seconds, truth: None, missingness_fraction: 0.0, bucket: Resolution::Minutes })
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
			Self::Tsbs => "tsbs",
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
	let target = (as_f64(n) * fraction).to_usize().unwrap_or(0);
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
	/// Which of the three interpolation classes the arm is (v7); `None` in an older report.
	#[serde(default)]
	pub class: Option<ArmClass>,
	/// For a native fill that computes a different answer from the linear reconstruction, what it
	/// computes instead (v7). Its gate checks the grid's shape, not the values, and the report
	/// shows how far it lies from the reconstruction and the truth instead of timing it against
	/// arms that answer the question asked.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub own_answer: Option<String>,
	/// Whether this arm was produced through a proper adapter workload — `None` (the default)
	/// means it was (v9). `Some(false)` marks a result from a non-adapter workload (e.g. ingest,
	/// compression), which measures only WeftDB's own storage paths and must not appear in a
	/// cross-engine comparison (#452).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub adapter_workload: Option<bool>,
}

/// The fair protocol's three ways to interpolate with an engine (roadmap 1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArmClass {
	/// (A) The engine's own interpolation or fill: WeftDB's reconstruction, `TimescaleDB`'s
	/// `time_bucket_gapfill` + `interpolate`, `QuestDB`'s `SAMPLE BY … FILL(LINEAR)`, `InfluxDB` 3's
	/// `date_bin_gapfill` + `interpolate`, `IoTDB`'s `FILL METHOD LINEAR`, `ClickHouse`'s `WITH FILL …
	/// INTERPOLATE`.
	Native,
	/// (B) The linear reconstruction in portable SQL: a time grid, the knots either side of each
	/// instant (`ASOF`/`LATERAL` joins or windows), the line between them.
	PortableSql,
	/// (C) The raw knots fetched into this Rust client and interpolated there, end to end.
	ClientSide,
}

impl ArmClass {
	/// The class's letter and name, as the report prints it.
	#[must_use]
	pub const fn label(self) -> &'static str {
		match self {
			Self::Native => "A · native",
			Self::PortableSql => "B · portable SQL",
			Self::ClientSide => "C · client-side",
		}
	}
}

/// The linear reconstruction a client writes after fetching the knots.
///
/// On the grid `start + i·step` for `i < n`, the line between the knots either side of each
/// instant, the first and last segments extended past the data (splimes' `Linear`). `t` must not decrease; of knots at
/// one instant the last wins. `f64` throughout, as a client holding `f64` rows computes it.
#[must_use]
pub fn client_linear(t: &[i64], v: &[f64], start: i64, step: i64, n: usize) -> Vec<f64> {
	let (mut kt, mut kv): (Vec<i64>, Vec<f64>) = (Vec::with_capacity(t.len()), Vec::with_capacity(v.len()));
	for (&ti, &vi) in t.iter().zip(v) {
		if kt.last() == Some(&ti) {
			*kv.last_mut().unwrap_or(&mut 0.0) = vi;
		} else {
			kt.push(ti);
			kv.push(vi);
		}
	}
	if kt.len() < 2 {
		return vec![kv.first().copied().unwrap_or(f64::NAN); n];
	}
	let mut out = Vec::with_capacity(n);
	let mut seg = 0;
	for i in 0..n {
		let g = start + i64::try_from(i).unwrap_or(i64::MAX) * step;
		while seg + 2 < kt.len() && kt[seg + 1] <= g {
			seg += 1;
		}
		let (a_t, a_v, b_t, b_v) = (kt[seg], kv[seg], kt[seg + 1], kv[seg + 1]);
		// The SQL arms' arithmetic, not a fused multiply-add, so the classes compute alike.
		out.push(if g == a_t {
			a_v
		} else {
			let rise = (b_v - a_v) * (as_f64(g - a_t) / as_f64(b_t - a_t));
			a_v + rise
		});
	}
	out
}

/// A native fill's rows `(bucket start, value)` placed on the grid `start + i·step` for `i < n`;
/// an instant with no row, or a null one, is `NaN` (unfilled), and a row off the grid is dropped.
#[must_use]
pub fn place(rows: &[(i64, f64)], start: i64, step: i64, n: usize) -> Vec<f64> {
	let mut out = vec![f64::NAN; n];
	for &(b, v) in rows {
		if b >= start && (b - start) % step == 0 {
			if let Some(slot) = usize::try_from((b - start) / step).ok().and_then(|i| out.get_mut(i)) {
				*slot = v;
			}
		}
	}
	out
}

impl ArmInfo {
	/// The engine the arm runs on, as the cold hook names it: the arm name's first word
	/// (`clickhouse` for `clickhouse-double`).
	#[must_use]
	pub fn engine_key(&self) -> &str {
		self.arm.split('-').next().unwrap_or(&self.arm)
	}

	/// Whether this arm result came from a non-adapter workload — a result that measures
	/// WeftDB's own storage paths with no competitor adapter provenance.
	///
	/// True when `adapter_workload` is explicitly `Some(false)`. A self-measured result
	/// must not be presented as a cross-engine comparison (#452).
	#[must_use]
	pub fn is_self_measured(&self) -> bool {
		matches!(self.adapter_workload, Some(false))
	}
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
	/// What was warm when the reps ran (`warm` in a v3 report, which timed only warm reps).
	#[serde(default)]
	pub warmth: Warmth,
	/// Every timed rep behind [`Self::latency`], in nanoseconds and in the order taken (v5);
	/// empty in older reports.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub samples_ns: Vec<u64>,
	/// How the runs a combined report pooled differed (`weft-xbench combine`, v6); `None` for one run.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub between_run: Option<BetweenRun>,
}

/// What was warm when a result's reps ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Warmth {
	/// Back-to-back reps: the engine's caches and the OS page cache hold what the last rep read.
	#[default]
	Warm,
	/// The OS page cache dropped before each rep; the engine kept running, its own caches intact.
	OsCold,
	/// The engine restarted (an in-process arm closed and reopened its files) and the OS page
	/// cache dropped before each rep: the first query after a restart.
	Restarted,
}

impl Warmth {
	/// The name the report and the console use.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Warm => "warm",
			Self::OsCold => "os-cold",
			Self::Restarted => "restarted",
		}
	}
}

/// The command that makes a rep cold.
///
/// It runs as `sh -c 'HOOK "$@"' sh ACTION ENGINE` outside the clock: `drop-caches ENGINE` empties the OS page cache, `restart ENGINE` restarts that engine's
/// server (the in-process engines, `weftdb` and `duckdb`, have none to restart; the arm reopens
/// its files itself). `scripts/xbench/cold-hook.sh` is the reference host's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cold {
	/// The hook command line.
	pub hook: String,
	/// Cold reps per phase and warmth.
	pub reps: usize,
}

impl Cold {
	/// Run the hook for `action` on `engine`.
	///
	/// # Errors
	///
	/// If the hook cannot start or exits unsuccessfully.
	pub async fn run(&self, action: &str, engine: &str) -> anyhow::Result<()> {
		let status = tokio::process::Command::new("sh").arg("-c").arg(format!("{} \"$@\"", self.hook)).arg("sh").arg(action).arg(engine).status().await?;
		anyhow::ensure!(status.success(), "cold hook `{} {action} {engine}` exited with {status}", self.hook);
		Ok(())
	}
}

/// How many reps of each kind a run times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Protocol {
	/// Timed loads per arm and dataset.
	pub load_reps: usize,
	/// Warm reps per phase.
	pub reps: usize,
	/// Seed for the synthetic datasets and the bootstrap.
	pub seed: u64,
	/// OS-cold and post-restart reps, when the run has a hook to make them.
	pub cold: Option<Cold>,
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
	/// Grid instants the arm left without a value (v7): a native fill does not extend past the
	/// first and last knots. Always zero for an arm held to the reference.
	#[serde(default)]
	pub unfilled_points: usize,
	/// Set only by `combine` (#516): the runs' answer digests differed while every run stayed
	/// inside its tolerance, so the answer is not reproducible bit for bit but no run was wrong.
	/// Such a row is neither a pass nor a failure, and stays out of win/loss verdicts.
	#[serde(default)]
	pub indeterminate: bool,
	/// SHA-256 of this arm's answer ([`grid_digest`], [`bucket_digest`]), so separate runs of one arm
	/// can be shown to answer alike (v6). Engines differ in the last bits of a float, so arms are
	/// compared with each other by [`Self::max_abs_diff_vs_reference`], not by this.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub answer_sha256: Option<String>,
}

/// Significant digits an answer's digest keeps of each float: a run that differs past them (a
/// parallel sum's rounding order) still answers alike, one that differs within them does not.
const DIGEST_DIGITS: usize = 12;

/// SHA-256 of an interpolated grid, each value to [`DIGEST_DIGITS`] significant digits.
#[must_use]
pub fn grid_digest(values: &[f64]) -> String {
	ResultDigest::of(values.iter().enumerate().map(|(i, v)| (i, format!("{v:.*e}", DIGEST_DIGITS - 1))))
}

/// SHA-256 of downsample buckets: start, count, min and max exactly, the mean to
/// [`DIGEST_DIGITS`] significant digits.
#[must_use]
pub fn bucket_digest(rows: &[BucketRow]) -> String {
	ResultDigest::of(rows.iter().map(|r| (r.start_ns, format!("{} {:e} {:e} {:.*e}", r.count, r.min, r.max, DIGEST_DIGITS - 1, r.avg))))
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
		gate.answer_sha256 = Some(bucket_digest(rows));
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
		Self { expected_points: expected, actual_points: values.len(), values_finite, max_abs_diff_vs_reference, tolerance, passed, unfilled_points: 0, answer_sha256: Some(grid_digest(values)), indeterminate: false }
	}

	/// The gate of a native fill that answers a different question ([`ArmInfo::own_answer`]): the
	/// grid's shape, not its values. It passes when the grid has `expected` instants and every
	/// filled one is finite; the largest difference from the reference over the filled instants
	/// and the count of unfilled ones are recorded, not judged.
	#[must_use]
	pub fn check_shape(values: &[f64], reference: &[f64], expected: usize) -> Self {
		let filled: Vec<(f64, f64)> = values.iter().zip(reference).filter(|(v, _)| !v.is_nan()).map(|(a, b)| (*a, *b)).collect();
		let values_finite = filled.iter().all(|(v, _)| v.is_finite());
		let max_abs_diff_vs_reference = filled.iter().map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
		let magnitude = reference.iter().map(|v| v.abs()).fold(1.0, f64::max);
		let unfilled_points = values.len() - filled.len();
		Self { expected_points: expected, actual_points: values.len(), values_finite, max_abs_diff_vs_reference, tolerance: RELATIVE_TOLERANCE * magnitude, passed: values.len() == expected && values_finite && !filled.is_empty(), unfilled_points, answer_sha256: Some(grid_digest(values)), indeterminate: false }
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
	/// The hook that made the cold reps, when the run had any.
	#[serde(default)]
	pub cold_hook: Option<String>,
	/// Every arm × dataset × phase × warmth.
	pub results: Vec<ArmResult>,
	/// The input files the datasets were read from (`--uci`, `--tsbs`), with their hashes (v6).
	#[serde(default)]
	pub inputs: Vec<InputFile>,
	/// SHA-256 of the binary that ran (v6).
	#[serde(default)]
	pub binary_sha256: Option<String>,
	/// The engine servers' containers as `docker inspect` described them (`--container`, v6).
	#[serde(default)]
	pub containers: Vec<Container>,
	/// For a combined report, each pooled run's report file and its SHA-256 (v6); empty for one run.
	#[serde(default)]
	pub combined_from: Vec<InputFile>,
}

/// A file the run read, as the run found it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputFile {
	/// What the file was (`uci`, `tsbs`, `run`).
	pub role: String,
	/// The path given.
	pub path: String,
	/// Its size.
	pub bytes: u64,
	/// Its SHA-256.
	pub sha256: String,
}

impl InputFile {
	/// Hash the file at `path`.
	///
	/// # Errors
	///
	/// If the file cannot be read.
	pub fn hash(role: &str, path: &std::path::Path) -> anyhow::Result<Self> {
		use sha2::Digest as _;
		let mut file = std::fs::File::open(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
		let mut hasher = sha2::Sha256::new();
		let bytes = std::io::copy(&mut file, &mut hasher)?;
		Ok(Self { role: role.to_string(), path: path.display().to_string(), bytes, sha256: hex(&hasher.finalize()) })
	}
}

fn hex(bytes: &[u8]) -> String {
	use std::fmt::Write as _;
	bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
		let _ = write!(s, "{b:02x}");
		s
	})
}

/// An engine server's container: the image it ran and how it was started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Container {
	/// The engine (`clickhouse`, `timescaledb`, …).
	pub engine: String,
	/// The container's name.
	pub name: String,
	/// The image reference it was started from.
	pub image: String,
	/// The image's content digest (`sha256:…`), which pins it where a tag does not.
	pub image_id: String,
	/// Entrypoint and command, which carry the engine's command-line settings.
	pub command: Vec<String>,
	/// Environment, which carries the settings some images read (`IoTDB`, `QuestDB`); a variable
	/// whose name suggests a secret is kept with its value replaced.
	pub env: Vec<String>,
}

/// Names of environment variables whose values a report must not carry.
const SECRET_NAMES: [&str; 5] = ["PASS", "SECRET", "TOKEN", "KEY", "CREDENTIAL"];

impl Container {
	/// Describe the container `name` of `engine` from `docker inspect`.
	///
	/// # Errors
	///
	/// If `docker inspect` fails or prints something else than one container.
	pub fn inspect(engine: &str, name: &str) -> anyhow::Result<Self> {
		let out = std::process::Command::new("docker").args(["inspect", "--type", "container", name]).output()?;
		anyhow::ensure!(out.status.success(), "docker inspect {name}: {}", String::from_utf8_lossy(&out.stderr).trim());
		Self::from_inspect(engine, name, &serde_json::from_slice(&out.stdout)?)
	}

	/// Describe a container from `docker inspect`'s JSON.
	///
	/// # Errors
	///
	/// If the JSON is not one container's description.
	pub fn from_inspect(engine: &str, name: &str, json: &serde_json::Value) -> anyhow::Result<Self> {
		let c = json.as_array().and_then(|a| a.first()).ok_or_else(|| anyhow::anyhow!("docker inspect {name}: no container"))?;
		let strings = |v: &serde_json::Value| v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect::<Vec<_>>()).unwrap_or_default();
		let config = &c["Config"];
		let command = [strings(&config["Entrypoint"]), strings(&config["Cmd"])].concat();
		let env = strings(&config["Env"])
			.into_iter()
			.map(|kv| match kv.split_once('=') {
				Some((k, _)) if SECRET_NAMES.iter().any(|s| k.to_ascii_uppercase().contains(s)) => format!("{k}=<redacted>"),
				_ => kv,
			})
			.collect();
		Ok(Self { engine: engine.to_string(), name: name.to_string(), image: config["Image"].as_str().unwrap_or_default().to_string(), image_id: c["Image"].as_str().unwrap_or_default().to_string(), command, env })
	}
}

impl XbenchReport {
	/// True when every interpolation result passed its correctness gate.
	#[must_use]
	pub fn all_correct(&self) -> bool {
		self.results.iter().filter_map(|r| r.correctness.as_ref()).all(|c| c.passed)
	}
}

/// How long a restarted server gets to answer again.
pub const RESTART_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Call `probe` on the blocking pool until it succeeds, for up to [`RESTART_TIMEOUT`]: a
/// restarted server refuses connections, then errors, for a while before it answers.
///
/// # Errors
///
/// The probe's last error, once the timeout passes.
pub async fn await_ready<T: Send + 'static>(engine: &'static str, mut probe: impl FnMut() -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
	tokio::task::spawn_blocking(move || {
		let deadline = Instant::now() + RESTART_TIMEOUT;
		loop {
			match probe() {
				Ok(value) => return Ok(value),
				Err(e) if Instant::now() >= deadline => return Err(e.context(format!("{engine} did not answer within {RESTART_TIMEOUT:?} of a restart"))),
				Err(_) => std::thread::sleep(std::time::Duration::from_millis(250)),
			}
		}
	})
	.await?
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
	// A native fill's unfilled instants are left out, not scored as zeros.
	let (points, truth): (Vec<Point>, Vec<f64>) = grid.into_iter().zip(values).zip(truth).filter(|((_, v), _)| !v.is_nan()).map(|((t, v), truth)| (Point::new(t, BigDecimal::from_f64(*v).unwrap_or_default()), *truth)).unzip();
	AccuracyMetrics::from_aligned(&points, &truth).ok()
}

/// Measure one arm on one dataset.
///
/// `load_reps` timed loads (when the arm loads), then `reps` timed reconstructions from the last load, each checked against `reference` (the in-memory
/// WeftDB grid, in `f64`) and, for synthetic data, scored against the ground truth.
///
/// Knot copies for the arms that take points per call are made before the timer starts.
///
/// With [`Protocol::cold`], an arm that loads then times `cold.reps` more of each phase OS-cold and
/// as many after a restart, each gated like the warm reps; the hook and the reopen run outside the
/// clock.
///
/// # Errors
///
/// If `reps` or `load_reps` is zero, or the arm fails to load or reconstruct, or the cold hook
/// fails.
pub async fn measure<A: weft::Arm>(arm: &mut A, ds: &Dataset, reference: &Reference, protocol: &Protocol) -> anyhow::Result<Vec<ArmResult>> {
	let Protocol { load_reps, reps, seed, ref cold } = *protocol;
	anyhow::ensure!(reps > 0 && load_reps > 0, "reps and load reps must be > 0");
	anyhow::ensure!(cold.as_ref().is_none_or(|c| c.reps > 0), "cold reps must be > 0");
	let info = arm.info();
	let row = |phase, warmth, output_points, reps, samples: &[u64], correctness, accuracy, storage_bytes| {
		let (latency, latency_ci) = latency(samples, seed);
		ArmResult { dataset: ds.name.clone(), kind: ds.kind, scale: ds.scale.as_str().to_string(), input_points: ds.points.len(), output_points, missingness_fraction: ds.missingness_fraction, arm: info.clone(), phase, reps, latency, latency_ci, correctness, accuracy, storage_bytes, warmth, samples_ns: samples.to_vec(), between_run: None }
	};
	// An arm with nothing loaded keeps nothing to go cold.
	let warmths: &[Warmth] = match cold {
		Some(_) if arm.loads() => &[Warmth::Warm, Warmth::OsCold, Warmth::Restarted],
		_ => &[Warmth::Warm],
	};
	let mut results = Vec::with_capacity(3 * warmths.len());

	let mut load_samples = Vec::with_capacity(load_reps);
	// An arm whose load is another arm's figure loads once: it only needs the data there.
	for _ in 0..if arm.loads() && arm.reports_load() { load_reps } else { 1 } {
		// Outside the clock: the previous rep's teardown is not part of this load.
		arm.unload();
		let t0 = Instant::now();
		arm.load(ds).await?;
		load_samples.push(span_ns(t0));
	}
	if arm.loads() && arm.reports_load() {
		results.push(row(Phase::Load, Warmth::Warm, ds.points.len(), load_reps, &load_samples, None, None, arm.storage_bytes()));
	}

	let engine = info.engine_key();
	for &warmth in warmths {
		let reps = if warmth == Warmth::Warm { reps } else { cold.as_ref().map_or(reps, |c| c.reps) };
		let mut samples = Vec::with_capacity(reps);
		let mut last = None;
		for _ in 0..reps {
			chill(arm, cold.as_ref(), warmth, engine).await?;
			let knots = ds.points.clone();
			let t0 = Instant::now();
			let output = arm.interpolate(ds, knots).await?;
			samples.push(span_ns(t0));
			last = Some(output);
		}
		let values = last.map(|o| o.to_f64()).unwrap_or_default();
		let correctness = if info.own_answer.is_some() { Correctness::check_shape(&values, &reference.grid, ds.grid_len()) } else { Correctness::check(&values, &reference.grid, ds.grid_len()) };
		let accuracy = ds.truth.as_deref().and_then(|truth| accuracy(&values, ds, truth));
		results.push(row(Phase::Interpolate, warmth, values.len(), reps, &samples, Some(correctness), accuracy, None));

		if arm.downsamples() {
			let mut samples = Vec::with_capacity(reps);
			let mut last = Vec::new();
			for _ in 0..reps {
				chill(arm, cold.as_ref(), warmth, engine).await?;
				let knots = ds.points.clone();
				let t0 = Instant::now();
				let buckets = arm.downsample(ds, knots).await?;
				samples.push(span_ns(t0));
				last = buckets;
			}
			let correctness = Correctness::check_buckets(&last, &reference.buckets);
			results.push(row(Phase::Downsample, warmth, last.len(), reps, &samples, Some(correctness), None, None));
		}
	}
	Ok(results)
}

/// Make the next rep `warmth`: restart the engine and reopen the arm's handle on it, then drop the
/// OS page cache, so nothing the reopen read stays cached. A warm rep needs nothing.
async fn chill<A: weft::Arm>(arm: &mut A, cold: Option<&Cold>, warmth: Warmth, engine: &str) -> anyhow::Result<()> {
	let Some(cold) = cold else { return Ok(()) };
	match warmth {
		Warmth::Warm => return Ok(()),
		Warmth::OsCold => {}
		Warmth::Restarted => {
			cold.run("restart", engine).await?;
			arm.reopen().await?;
		}
	}
	cold.run("drop-caches", engine).await
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

/// The engine servers a run compares against, beside the in-process engines. An engine left
/// `None` is not run, and the report's engine list says which ran.
#[derive(Debug, Clone, Default)]
pub struct Servers {
	/// A `ClickHouse` HTTP endpoint (`--clickhouse URL`).
	pub clickhouse: Option<clickhouse::Server>,
	/// A `PostgreSQL` connection string for a server with `TimescaleDB` (`--timescale URL`).
	pub timescale: Option<String>,
	/// A `QuestDB` server's `host:port` (`--questdb ADDR`), QWP and HTTP on the one port.
	pub questdb: Option<questdb::Server>,
	/// An `InfluxDB` 3 HTTP endpoint (`--influxdb3 URL`).
	pub influxdb3: Option<influxdb3::Server>,
	/// An Apache `IoTDB` REST service (`--iotdb URL`).
	pub iotdb: Option<iotdb::Server>,
}

/// Run every arm over every dataset.
///
/// # Errors
///
/// If any arm fails outright; a wrong answer is not an error, it fails that result's gate.
pub async fn run(datasets: &[Dataset], servers: &Servers, protocol: &Protocol, metadata: RunMetadata, source_commit: Option<String>, invocation: Vec<String>) -> anyhow::Result<XbenchReport> {
	let mut results = Vec::new();
	for ds in datasets {
		let reference = reference(ds).await?;
		results.extend(measure(&mut weft::WeftMemory, ds, &reference, protocol).await?);
		results.extend(measure(&mut weft::WeftStore::stored(), ds, &reference, protocol).await?);
		results.extend(measure(&mut weft::WeftStore::stored_f64(), ds, &reference, protocol).await?);
		results.extend(measure(&mut weft::WeftStore::stored_bulk(), ds, &reference, protocol).await?);
		results.extend(measure(&mut duck::Duck::double(), ds, &reference, protocol).await?);
		results.extend(measure(&mut duck::Duck::decimal(), ds, &reference, protocol).await?);
		results.extend(measure(&mut duck::Duck::client(), ds, &reference, protocol).await?);
		if let Some(server) = &servers.clickhouse {
			results.extend(measure(&mut clickhouse::ClickHouse::double(server.clone()), ds, &reference, protocol).await?);
			results.extend(measure(&mut clickhouse::ClickHouse::decimal(server.clone()), ds, &reference, protocol).await?);
			results.extend(measure(&mut clickhouse::ClickHouse::double_merge(server.clone()), ds, &reference, protocol).await?);
			results.extend(measure(&mut clickhouse::ClickHouse::native(server.clone()), ds, &reference, protocol).await?);
			results.extend(measure(&mut clickhouse::ClickHouse::client(server.clone()), ds, &reference, protocol).await?);
		}
		if let Some(url) = &servers.timescale {
			results.extend(measure(&mut timescale::Timescale::double(url), ds, &reference, protocol).await?);
			results.extend(measure(&mut timescale::Timescale::numeric(url), ds, &reference, protocol).await?);
			results.extend(measure(&mut timescale::Timescale::native(url), ds, &reference, protocol).await?);
			results.extend(measure(&mut timescale::Timescale::client(url), ds, &reference, protocol).await?);
		}
		if let Some(server) = &servers.questdb {
			results.extend(measure(&mut questdb::QuestDb::double(server.clone()), ds, &reference, protocol).await?);
			results.extend(measure(&mut questdb::QuestDb::decimal(server.clone()), ds, &reference, protocol).await?);
			results.extend(measure(&mut questdb::QuestDb::double_linear(server.clone()), ds, &reference, protocol).await?);
			results.extend(measure(&mut questdb::QuestDb::native(server.clone()), ds, &reference, protocol).await?);
			results.extend(measure(&mut questdb::QuestDb::client(server.clone()), ds, &reference, protocol).await?);
		}
		if let Some(server) = &servers.influxdb3 {
			results.extend(measure(&mut influxdb3::Influx::double(server.clone()), ds, &reference, protocol).await?);
			results.extend(measure(&mut influxdb3::Influx::native(server.clone()), ds, &reference, protocol).await?);
			results.extend(measure(&mut influxdb3::Influx::client(server.clone()), ds, &reference, protocol).await?);
		}
		if let Some(server) = &servers.iotdb {
			results.extend(measure(&mut iotdb::IoTdb::double(server.clone()), ds, &reference, protocol).await?);
			results.extend(measure(&mut iotdb::IoTdb::native(server.clone()), ds, &reference, protocol).await?);
			results.extend(measure(&mut iotdb::IoTdb::client(server.clone()), ds, &reference, protocol).await?);
		}
	}
	let mut engines = vec![EngineVersion { engine: "weftdb".into(), version: metadata.weft_bench_version.clone() }, EngineVersion { engine: "duckdb".into(), version: duck::duckdb_version()? }];
	if let Some(server) = &servers.clickhouse {
		engines.push(EngineVersion { engine: "clickhouse".into(), version: server.version()? });
	}
	if let Some(url) = &servers.timescale {
		engines.push(EngineVersion { engine: "timescaledb".into(), version: timescale::version(url).await? });
	}
	if let Some(server) = &servers.questdb {
		engines.push(EngineVersion { engine: "questdb".into(), version: server.version()? });
	}
	if let Some(server) = &servers.influxdb3 {
		engines.push(EngineVersion { engine: "influxdb3".into(), version: server.version()? });
	}
	if let Some(server) = &servers.iotdb {
		engines.push(EngineVersion { engine: "iotdb".into(), version: server.version()? });
	}
	Ok(XbenchReport { schema_version: XBENCH_SCHEMA_VERSION, metadata, engines, seed: protocol.seed, source_commit, invocation, scratch_dir: Some(std::env::temp_dir().display().to_string()), optimized: Some(!cfg!(debug_assertions)), cold_hook: protocol.cold.as_ref().map(|c| c.hook.clone()), results, inputs: Vec::new(), binary_sha256: None, containers: Vec::new(), combined_from: Vec::new() })
}

#[cfg(test)]
mod tests {
	use chrono::TimeZone;
	use weft_bench::RunMetadata;

	use super::*;
	use crate::weft::Arm;

	#[test]
	fn a_container_is_described_with_its_secrets_redacted() {
		let json = serde_json::json!([{ "Image": "sha256:abc", "Config": { "Image": "timescale/timescaledb:2.17.2-pg16", "Entrypoint": ["docker-entrypoint.sh"], "Cmd": ["postgres", "-c", "timescaledb.telemetry_level=off"], "Env": ["POSTGRES_PASSWORD=hunter2", "POSTGRES_DB=xbench", "API_TOKEN=t"] } }]);
		let c = Container::from_inspect("timescaledb", "xbench-ts", &json).expect("describes");
		assert_eq!((c.image.as_str(), c.image_id.as_str()), ("timescale/timescaledb:2.17.2-pg16", "sha256:abc"));
		assert_eq!(c.command, ["docker-entrypoint.sh", "postgres", "-c", "timescaledb.telemetry_level=off"]);
		assert_eq!(c.env, ["POSTGRES_PASSWORD=<redacted>", "POSTGRES_DB=xbench", "API_TOKEN=<redacted>"]);
		assert!(Container::from_inspect("x", "y", &serde_json::json!([])).is_err());
	}

	#[test]
	fn the_client_line_is_the_reference_line() {
		// Knots at 10, 20 (twice, the last wins) and 40; the grid runs past both ends.
		let line = client_linear(&[10, 20, 20, 40], &[1.0, 9.0, 3.0, 7.0], 0, 10, 6);
		assert_eq!(line, [-1.0, 1.0, 3.0, 5.0, 7.0, 9.0]);
		assert_eq!(client_linear(&[5], &[2.0], 0, 1, 2), [2.0, 2.0]);
	}

	#[test]
	fn a_native_fill_is_placed_on_the_grid_and_judged_by_its_shape() {
		let grid = place(&[(10, 1.0), (20, f64::NAN), (25, 9.0), (30, 3.0), (90, 4.0)], 10, 10, 4);
		assert_eq!(grid.iter().map(|v| if v.is_nan() { None } else { Some(*v) }).collect::<Vec<_>>(), [Some(1.0), None, Some(3.0), None]);
		let gate = Correctness::check_shape(&grid, &[1.0, 2.0, 2.5, 4.0], 4);
		assert!(gate.passed);
		assert_eq!((gate.unfilled_points, gate.max_abs_diff_vs_reference), (2, 0.5));
		assert!(!Correctness::check_shape(&[f64::NAN; 4], &[1.0; 4], 4).passed, "a fill that filled nothing fails");
	}

	#[test]
	fn digests_hold_an_answer_not_its_last_bits() {
		let a = grid_digest(&[1.0, 2.5, 3.25]);
		assert_eq!(a, grid_digest(&[1.0, 2.5, 3.25 + 1e-15]), "below twelve digits is rounding");
		assert_ne!(a, grid_digest(&[1.0, 2.5, 3.2501]));
		let row = |avg: f64| BucketRow { start_ns: 0, count: 3, min: 1.0, max: 2.0, avg };
		assert_eq!(bucket_digest(&[row(1.5)]), bucket_digest(&[row(1.5 + 1e-15)]));
		assert_ne!(bucket_digest(&[row(1.5)]), bucket_digest(&[BucketRow { count: 4, ..row(1.5) }]));
		let gate = Correctness::check(&[1.0, 2.0], &[1.0, 2.0], 2);
		assert_eq!(gate.answer_sha256.as_deref(), Some(grid_digest(&[1.0, 2.0]).as_str()));
	}

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

	/// The `ClickHouse` arms against a live server, when `WEFT_XBENCH_CLICKHOUSE` names one
	/// (`http://127.0.0.1:8123`); without it there is nothing to test against and this passes.
	#[tokio::test]
	async fn clickhouse_matches_the_reference_on_edges_and_duplicates_when_a_server_is_given() {
		let Ok(url) = std::env::var("WEFT_XBENCH_CLICKHOUSE") else { return };
		let server = clickhouse::Server::new(&url);
		let mut ds = edge_dataset();
		ds.points.insert(3, Point::new(ts(14), "9".parse().expect("decimal")));
		let reference = reference(&ds).await.expect("reference");
		for mut arm in [clickhouse::ClickHouse::double(server.clone()), clickhouse::ClickHouse::decimal(server.clone())] {
			let name = arm.info().arm;
			let values = values_of(&mut arm, &ds).await;
			let gate = Correctness::check(&values, &reference.grid, ds.grid_len());
			assert!(gate.passed, "{name}: {gate:?}\n got {values:?}\nwant {:?}", reference.grid);
			assert!(arm.storage_bytes().is_some_and(|b| b > 0), "{name}: bytes on disk");
			let buckets = arm.downsample(&ds, ds.points.clone()).await.expect("downsamples");
			assert!(Correctness::check_buckets(&buckets, &reference.buckets).passed, "{name}: got {buckets:?}\nwant {:?}", reference.buckets);
		}
	}

	/// The `TimescaleDB` arms against a live server, when `WEFT_XBENCH_TIMESCALE` holds a connection
	/// string (`postgresql://postgres:…@127.0.0.1:5432/xbench`); without it this passes.
	#[tokio::test(flavor = "multi_thread")]
	async fn timescale_matches_the_reference_on_edges_and_duplicates_when_a_server_is_given() {
		let Ok(url) = std::env::var("WEFT_XBENCH_TIMESCALE") else { return };
		let mut ds = edge_dataset();
		ds.points.insert(3, Point::new(ts(14), "9".parse().expect("decimal")));
		let reference = reference(&ds).await.expect("reference");
		for mut arm in [timescale::Timescale::double(&url), timescale::Timescale::numeric(&url)] {
			let name = arm.info().arm;
			let values = values_of(&mut arm, &ds).await;
			let gate = Correctness::check(&values, &reference.grid, ds.grid_len());
			assert!(gate.passed, "{name}: {gate:?}\n got {values:?}\nwant {:?}", reference.grid);
			assert!(arm.storage_bytes().is_some_and(|b| b > 0), "{name}: bytes on disk");
			let buckets = arm.downsample(&ds, ds.points.clone()).await.expect("downsamples");
			assert!(Correctness::check_buckets(&buckets, &reference.buckets).passed, "{name}: got {buckets:?}\nwant {:?}", reference.buckets);
		}
	}

	/// The `QuestDB` arms against a live server, when `WEFT_XBENCH_QUESTDB` names one
	/// (`127.0.0.1:9000`); without it this passes.
	#[tokio::test(flavor = "multi_thread")]
	async fn questdb_matches_the_reference_on_edges_and_duplicates_when_a_server_is_given() {
		let Ok(addr) = std::env::var("WEFT_XBENCH_QUESTDB") else { return };
		let server = questdb::Server::new(&addr);
		let mut ds = edge_dataset();
		ds.points.insert(3, Point::new(ts(14), "9".parse().expect("decimal")));
		let reference = reference(&ds).await.expect("reference");
		for mut arm in [questdb::QuestDb::double(server.clone()), questdb::QuestDb::decimal(server.clone())] {
			let name = arm.info().arm;
			let values = values_of(&mut arm, &ds).await;
			let gate = Correctness::check(&values, &reference.grid, ds.grid_len());
			assert!(gate.passed, "{name}: {gate:?}\n got {values:?}\nwant {:?}", reference.grid);
			assert!(arm.storage_bytes().is_some_and(|b| b > 0), "{name}: bytes on disk");
			let buckets = arm.downsample(&ds, ds.points.clone()).await.expect("downsamples");
			assert!(Correctness::check_buckets(&buckets, &reference.buckets).passed, "{name}: got {buckets:?}\nwant {:?}", reference.buckets);
		}
	}

	/// The `InfluxDB` 3 arm against a live server, when `WEFT_XBENCH_INFLUXDB3` names one
	/// (`http://127.0.0.1:8181`); without it this passes. A series and an instant are `InfluxDB`'s
	/// key, so the duplicate instant is checked for interpolation (the last value wins, as in
	/// splimes) and the buckets on the dataset without it, where every row counts once anyway.
	#[tokio::test(flavor = "multi_thread")]
	async fn influxdb3_matches_the_reference_on_edges_and_duplicates_when_a_server_is_given() {
		let Ok(url) = std::env::var("WEFT_XBENCH_INFLUXDB3") else { return };
		let mut arm = influxdb3::Influx::double(influxdb3::Server::new(&url));
		let plain = edge_dataset();
		let mut dup = edge_dataset();
		dup.points.insert(3, Point::new(ts(14), "9".parse().expect("decimal")));
		let reference = reference(&dup).await.expect("reference");
		let values = values_of(&mut arm, &dup).await;
		let gate = Correctness::check(&values, &reference.grid, dup.grid_len());
		assert!(gate.passed, "{gate:?}\n got {values:?}\nwant {:?}", reference.grid);
		let reference = reference_of(&plain).await;
		arm.load(&plain).await.expect("loads");
		let buckets = arm.downsample(&plain, plain.points.clone()).await.expect("downsamples");
		assert!(Correctness::check_buckets(&buckets, &reference.buckets).passed, "got {buckets:?}\nwant {:?}", reference.buckets);
		assert!(arm.storage_bytes().is_none(), "the WAL's bytes are not reported as storage");
	}

	/// The `IoTDB` arm against a live server, when `WEFT_XBENCH_IOTDB` names its REST service
	/// (`http://127.0.0.1:18080`); without it this passes. A device and an instant are `IoTDB`'s key,
	/// so the duplicate instant is checked for interpolation (the last value wins, as in splimes) and
	/// the buckets on the dataset without it, where every row counts once anyway.
	#[tokio::test(flavor = "multi_thread")]
	async fn iotdb_matches_the_reference_on_edges_and_duplicates_when_a_server_is_given() {
		let Ok(url) = std::env::var("WEFT_XBENCH_IOTDB") else { return };
		let mut arm = iotdb::IoTdb::double(iotdb::Server::new(&url));
		let plain = edge_dataset();
		let mut dup = edge_dataset();
		dup.points.insert(3, Point::new(ts(14), "9".parse().expect("decimal")));
		let reference = reference(&dup).await.expect("reference");
		let values = values_of(&mut arm, &dup).await;
		let gate = Correctness::check(&values, &reference.grid, dup.grid_len());
		assert!(gate.passed, "{gate:?}\n got {values:?}\nwant {:?}", reference.grid);
		let reference = reference_of(&plain).await;
		arm.load(&plain).await.expect("loads");
		let buckets = arm.downsample(&plain, plain.points.clone()).await.expect("downsamples");
		assert!(Correctness::check_buckets(&buckets, &reference.buckets).passed, "got {buckets:?}\nwant {:?}", reference.buckets);
		assert!(arm.storage_bytes().is_some_and(|b| b > 0), "the flushed TsFiles are reported as storage");
	}

	/// An arm whose teardown is slow and whose load is instant: the in-memory reference with a
	/// load phase, so its answers pass the gates.
	struct SlowTeardown;

	impl Arm for SlowTeardown {
		fn info(&self) -> ArmInfo {
			ArmInfo { arm: "slow-teardown".into(), ..weft::WeftMemory.info() }
		}

		fn unload(&mut self) {
			std::thread::sleep(std::time::Duration::from_millis(200));
		}

		fn reopen(&mut self) -> impl std::future::Future<Output = anyhow::Result<()>> {
			std::future::ready(Ok(()))
		}

		fn load(&mut self, _ds: &Dataset) -> impl std::future::Future<Output = anyhow::Result<()>> {
			std::future::ready(Ok(()))
		}

		async fn interpolate(&mut self, ds: &Dataset, knots: Vec<splimes::Point>) -> anyhow::Result<Output> {
			weft::WeftMemory.interpolate(ds, knots).await
		}

		async fn downsample(&mut self, ds: &Dataset, knots: Vec<splimes::Point>) -> anyhow::Result<Vec<BucketRow>> {
			weft::WeftMemory.downsample(ds, knots).await
		}
	}

	#[tokio::test]
	async fn a_load_is_timed_without_the_previous_loads_teardown() {
		let ds = edge_dataset();
		let reference = reference_of(&ds).await;
		let results = measure(&mut SlowTeardown, &ds, &reference, &Protocol { load_reps: 3, reps: 1, seed: DEFAULT_SEED, cold: None }).await.expect("measures");
		let load = results.iter().find(|r| r.phase == Phase::Load).expect("a load row");
		assert!(load.latency.max_ns < 100_000_000, "a 200 ms teardown leaked into the load clock: {:?}", load.latency);
	}

	/// The reference with a load phase that counts its reopens.
	#[derive(Default)]
	struct Reopens(usize);

	impl Arm for Reopens {
		fn info(&self) -> ArmInfo {
			ArmInfo { arm: "probe-arm".into(), ..weft::WeftMemory.info() }
		}

		fn unload(&mut self) {}

		fn load(&mut self, _ds: &Dataset) -> impl std::future::Future<Output = anyhow::Result<()>> {
			std::future::ready(Ok(()))
		}

		fn reopen(&mut self) -> impl std::future::Future<Output = anyhow::Result<()>> {
			self.0 += 1;
			std::future::ready(Ok(()))
		}

		async fn interpolate(&mut self, ds: &Dataset, knots: Vec<splimes::Point>) -> anyhow::Result<Output> {
			weft::WeftMemory.interpolate(ds, knots).await
		}

		async fn downsample(&mut self, ds: &Dataset, knots: Vec<splimes::Point>) -> anyhow::Result<Vec<BucketRow>> {
			weft::WeftMemory.downsample(ds, knots).await
		}
	}

	#[tokio::test]
	async fn cold_reps_restart_and_drop_caches_before_each_rep_and_are_gated() {
		let ds = edge_dataset();
		let reference = reference_of(&ds).await;
		let log = std::env::temp_dir().join(format!("weft-xbench-cold-hook-{}.log", std::process::id()));
		let _ = std::fs::remove_file(&log);
		let protocol = Protocol { load_reps: 1, reps: 1, seed: DEFAULT_SEED, cold: Some(Cold { hook: format!("echo >>{} ", log.display()), reps: 2 }) };
		let mut arm = Reopens::default();
		let results = measure(&mut arm, &ds, &reference, &protocol).await.expect("measures");
		let rows: Vec<(Phase, Warmth, usize, bool)> = results.iter().map(|r| (r.phase, r.warmth, r.reps, r.correctness.as_ref().is_none_or(|c| c.passed))).collect();
		assert_eq!(rows, [(Phase::Load, Warmth::Warm, 1, true), (Phase::Interpolate, Warmth::Warm, 1, true), (Phase::Downsample, Warmth::Warm, 1, true), (Phase::Interpolate, Warmth::OsCold, 2, true), (Phase::Downsample, Warmth::OsCold, 2, true), (Phase::Interpolate, Warmth::Restarted, 2, true), (Phase::Downsample, Warmth::Restarted, 2, true)]);
		assert_eq!(arm.0, 4, "one reopen per post-restart rep");
		let calls = std::fs::read_to_string(&log).expect("the hook ran");
		let _ = std::fs::remove_file(&log);
		let os = "drop-caches probe\n".repeat(4);
		let restarted = "restart probe\ndrop-caches probe\n".repeat(4);
		assert_eq!(calls, format!("{os}{restarted}"), "the engine key is the arm name's first word");
	}

	#[tokio::test]
	async fn a_failing_cold_hook_stops_the_run_and_an_unloaded_arm_stays_warm() {
		let ds = edge_dataset();
		let reference = reference_of(&ds).await;
		let failing = Protocol { load_reps: 1, reps: 1, seed: DEFAULT_SEED, cold: Some(Cold { hook: "false".into(), reps: 1 }) };
		assert!(measure(&mut Reopens::default(), &ds, &reference, &failing).await.is_err());
		let results = measure(&mut weft::WeftMemory, &ds, &reference, &failing).await.expect("the in-memory arm has nothing to chill");
		assert!(results.iter().all(|r| r.warmth == Warmth::Warm));
	}

	async fn reference_of(ds: &Dataset) -> Reference {
		reference(ds).await.expect("reference")
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
	async fn the_in_process_arms_reopen_their_files_and_still_pass_cold() {
		let datasets = [Dataset::synthetic(DatasetKind::Mcar, ScalePreset::Tiny, DEFAULT_SEED).expect("builds")];
		// A hook that does nothing: what is tested is that a reopened store or database file
		// answers as the loaded one did.
		let protocol = Protocol { load_reps: 1, reps: 1, seed: DEFAULT_SEED, cold: Some(Cold { hook: "true".into(), reps: 2 }) };
		let report = run(&datasets, &Servers::default(), &protocol, RunMetadata::capture("test".into()), None, vec![]).await.expect("runs");
		// The 17 warm rows, and the 10 read rows of the six storing arms again OS-cold and restarted.
		assert_eq!(report.results.len(), 17 + 2 * 10);
		assert_eq!(report.results.iter().filter(|r| r.warmth == Warmth::Restarted && r.arm.arm.starts_with("weftdb-memory")).count(), 0);
		assert_eq!(report.cold_hook.as_deref(), Some("true"));
		assert!(report.all_correct(), "{:#?}", report.results.iter().filter(|r| !r.correctness.as_ref().is_none_or(|c| c.passed)).collect::<Vec<_>>());
	}

	#[tokio::test]
	async fn a_tiny_run_reports_both_phases_for_storing_arms_and_passes() {
		let datasets = [Dataset::synthetic(DatasetKind::Mcar, ScalePreset::Tiny, DEFAULT_SEED).expect("builds")];
		let report = run(&datasets, &Servers::default(), &Protocol { load_reps: 1, reps: 2, seed: DEFAULT_SEED, cold: None }, RunMetadata::capture("test".into()), None, vec![]).await.expect("runs");
		// memory: interpolate + downsample; stored, stored-bulk, duckdb-double, duckdb-decimal:
		// load + interpolate + downsample; stored-f64: load + interpolate; duckdb-client: interpolate
		// only (its load is duckdb-double's).
		assert_eq!(report.results.len(), 17);
		assert!(report.results.iter().any(|r| r.arm.arm == "duckdb-client" && r.arm.class == Some(ArmClass::ClientSide) && r.correctness.as_ref().is_some_and(|c| c.passed)));
		assert!(report.results.iter().filter(|r| r.phase == Phase::Load).all(|r| r.storage_bytes.is_some_and(|b| b > 0)), "every load reports its bytes on disk");
		assert!(report.all_correct(), "{:#?}", report.results.iter().filter_map(|r| r.correctness.as_ref()).collect::<Vec<_>>());
		assert!(report.results.iter().filter(|r| r.phase == Phase::Interpolate).all(|r| r.accuracy.is_some()));
		assert!(report.engines.iter().any(|e| e.engine == "duckdb" && !e.version.is_empty()));
	}

	#[test]
	fn a_tsbs_dataset_is_the_synthetic_grid_over_ten_second_knots() {
		use std::fmt::Write as _;
		let mut lp = String::new();
		for i in 0..800_i64 {
			for host in ["host_0", "host_1"] {
				writeln!(lp, "cpu,hostname={host},region=eu-central-1 usage_user={}i,usage_system=1i {}", 50 + i % 7, 1_451_606_400_000_000_000 + i * 10_000_000_000).expect("writes");
			}
		}
		let series = tsbs::load_from_reader(lp.as_bytes(), tsbs::DEFAULT_HOST, tsbs::DEFAULT_FIELD, 7_200).expect("reads");
		assert_eq!((series.points.len(), series.hosts), (721, 2));
		let ds = Dataset::tsbs(&series, ScalePreset::Tiny).expect("builds");
		assert_eq!((ds.name.as_str(), ds.kind, ds.points.len(), ds.grid_len()), ("tsbs-tiny", DatasetKind::Tsbs, 361, 3_601));
		assert_eq!(ds.points.last().expect("knots").timestamp, ds.end, "the last knot closes the grid");
		assert!(Dataset::synthetic(DatasetKind::Tsbs, ScalePreset::Tiny, DEFAULT_SEED).is_err());
	}
}
