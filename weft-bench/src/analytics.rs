//! The **analytics pipeline** workload (#145): `weft-orchestration`'s `Pipeline` run end to end
//! over a stored real corpus, stage by stage.
//!
//! Each rep creates a fresh legacy [`Database`] under the data directory `WEFT_DATA_DIR` names
//! (never the user's default one), captures the corpus' first rows into a minute-resolution
//! aspect, runs the whole pipeline (batching, pattern extraction, event detection, correlation,
//! signal generation) with every stage timed, then captures the rest and runs it again: the
//! incremental run a live deployment makes. The database is removed after the rep.
//!
//! **Gate.** The pipeline carries a global-peak and a global-valley detector. Linear resampling on
//! the data's own minute grid fills a gap only with values strictly between its two ends, so the
//! resampled series' strict local maxima at the global maximum are exactly the corpus rows at the
//! maximum whose neighbouring rows are both lower, each as a one-minute-each-side window: a
//! reference computed from the raw rows, apart from the store and the spline. The detectors'
//! manifestations after the final run must equal it, peak for peak and valley for valley.

use std::{collections::BTreeSet, time::Instant};

use bigdecimal::{BigDecimal, FromPrimitive};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use splimes::Spline;
use weftdb::{
	database::traits::{AspectStructure, Inputs}, Database, DatabaseStructure, DatasetId, DictionaryConstraints, InputMeasurement, Resolution, Steps, Variability, VariablilityType
};
use weft_orchestration::Pipeline;

use crate::{
	ingest::{span_ns, IngestProfile}, schema::{BenchResult, CorrectnessReport, DatasetMeta, TimingBreakdown, SCHEMA_VERSION}, stats::LatencyStats
};

/// Workload class label of an analytics-pipeline run.
pub const WORKLOAD_ANALYTICS_PIPELINE: &str = "analytics_pipeline";

/// Name of the peak detector the workload registers.
const PEAKS: &str = "bench-peaks";

/// Name of the valley detector the workload registers.
const VALLEYS: &str = "bench-valleys";

/// Rows per capture call.
const CAPTURE_CHUNK: usize = 10_000;

/// The knobs of [`run_analytics_pipeline`].
#[derive(Debug, Clone, Copy)]
pub struct AnalyticsParams {
	/// Corpus rows the aspect ends with (`>= 4`; at most the corpus).
	pub rows: usize,
	/// Rows held back from the first run and captured before the incremental one (`< rows`).
	pub tail_rows: usize,
	/// The pipeline's batch size, in resampled points.
	pub batch_size: usize,
	/// Fresh databases to run the whole sequence in (`>= 1`).
	pub reps: usize,
}

/// One analytics-pipeline run's stage timings and outputs (schema v27), medians over the reps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyticsPoint {
	/// Rows the aspect held after the incremental capture.
	pub rows: usize,
	/// Rows captured before the incremental run.
	pub tail_rows: usize,
	/// The pipeline's batch size.
	pub batch_size: usize,
	/// Capturing the first `rows - tail_rows` rows, in nanoseconds.
	pub capture_ns: u64,
	/// `prepare_data` (batching and processing) of the first run.
	pub prepare_ns: u64,
	/// `extract_patterns` of the first run.
	pub patterns_ns: u64,
	/// `detect_events` of the first run.
	pub events_ns: u64,
	/// `correlate_events` of the first run.
	pub correlate_ns: u64,
	/// `generate_signals` and the state save of the first run.
	pub signals_ns: u64,
	/// The incremental `run()` after the tail was captured.
	pub incremental_ns: u64,
	/// Patterns the dictionary held after the final run.
	pub patterns: usize,
	/// Signals the pipeline held after the final run.
	pub signals: usize,
	/// Peak manifestations the reference expects and the detector found.
	pub peaks_expected: usize,
	pub peaks_found: usize,
	/// Valley manifestations the reference expects and the detector found.
	pub valleys_expected: usize,
	pub valleys_found: usize,
}

/// One rep's measurements.
struct Rep {
	capture_ns: u64,
	stages: [u64; 5],
	incremental_ns: u64,
	patterns: usize,
	signals: usize,
	peaks: BTreeSet<i64>,
	valleys: BTreeSet<i64>,
}

/// The midpoints (epoch seconds) of the manifestations a global-extreme detector must report
/// on `values`: rows at the extreme whose neighbouring rows are both strictly beyond it.
fn extreme_reference(timestamps: &[i64], values: &[BigDecimal], peaks: bool) -> BTreeSet<i64> {
	let Some(extreme) = (if peaks { values.iter().max() } else { values.iter().min() }) else {
		return BTreeSet::new();
	};
	let beyond = |neighbour: &BigDecimal| if peaks { neighbour < extreme } else { neighbour > extreme };
	(1..values.len().saturating_sub(1)).filter(|&i| &values[i] == extreme && beyond(&values[i - 1]) && beyond(&values[i + 1])).map(|i| timestamps[i]).collect()
}

/// Run the analytics-pipeline workload on the corpus `timestamps` (epoch seconds, strictly
/// rising, on whole minutes) and `values`.
///
/// # Errors
///
/// Returns an error if `WEFT_DATA_DIR` is unset, if a knob is out of range or the corpus is
/// shorter than `params.rows`, or if a database or pipeline operation fails. The detectors
/// disagreeing with the reference is not an error: it fails the gate.
pub async fn run_analytics_pipeline(profile: &IngestProfile, timestamps: &[i64], values: &[BigDecimal], missingness_fraction: f64, generation_ns: u64, params: AnalyticsParams) -> anyhow::Result<BenchResult> {
	anyhow::ensure!(std::env::var_os("WEFT_DATA_DIR").is_some_and(|dir| !dir.is_empty()), "the analytics pipeline creates legacy databases: set WEFT_DATA_DIR to a scratch directory");
	anyhow::ensure!(params.rows >= 4 && params.rows <= timestamps.len(), "the analytics pipeline needs 4..={} rows, not {}", timestamps.len(), params.rows);
	anyhow::ensure!(params.tail_rows < params.rows - 3, "the incremental tail ({}) must leave at least 4 rows for the first run", params.tail_rows);
	anyhow::ensure!(params.batch_size > 0 && params.reps > 0, "batch size and reps must be > 0");
	anyhow::ensure!(timestamps.len() == values.len(), "analytics corpus timestamps and values differ in length");
	let (timestamps, values) = (&timestamps[..params.rows], &values[..params.rows]);
	anyhow::ensure!(timestamps.windows(2).all(|w| w[0] < w[1]) && timestamps.iter().all(|t| t % 60 == 0), "the analytics corpus must be strictly rising whole minutes");
	let measurements = timestamps.iter().zip(values).map(|(&t, v)| Ok(InputMeasurement::new(DateTime::<Utc>::from_timestamp(t, 0).ok_or_else(|| anyhow::anyhow!("timestamp {t} is out of range"))?, v.clone()))).collect::<anyhow::Result<Vec<_>>>()?;
	let expected_peaks = extreme_reference(timestamps, values, true);
	let expected_valleys = extreme_reference(timestamps, values, false);

	let window_start = Instant::now();
	let mut reps = Vec::with_capacity(params.reps);
	for rep in 0..params.reps {
		let name = format!("weft-bench-analytics-{}-{rep}", uuid::Uuid::new_v4().simple());
		let outcome = run_rep(&name, &measurements, params).await;
		std::fs::remove_dir_all(std::path::Path::new(&weftdb::data_dir()).join(&name)).ok();
		reps.push(outcome?);
	}
	let measured_ns = span_ns(window_start);

	let median = |pick: &dyn Fn(&Rep) -> u64| {
		let mut v: Vec<u64> = reps.iter().map(pick).collect();
		v.sort_unstable();
		v[v.len() / 2]
	};
	let last = reps.last().ok_or_else(|| anyhow::anyhow!("no rep ran"))?;
	let exact = reps.iter().all(|r| r.peaks == expected_peaks && r.valleys == expected_valleys);
	let point = AnalyticsPoint {
		rows: params.rows,
		tail_rows: params.tail_rows,
		batch_size: params.batch_size,
		capture_ns: median(&|r| r.capture_ns),
		prepare_ns: median(&|r| r.stages[0]),
		patterns_ns: median(&|r| r.stages[1]),
		events_ns: median(&|r| r.stages[2]),
		correlate_ns: median(&|r| r.stages[3]),
		signals_ns: median(&|r| r.stages[4]),
		incremental_ns: median(&|r| r.incremental_ns),
		patterns: last.patterns,
		signals: last.signals,
		peaks_expected: expected_peaks.len(),
		peaks_found: last.peaks.len(),
		valleys_expected: expected_valleys.len(),
		valleys_found: last.valleys.len(),
	};
	let first_runs: Vec<u64> = reps.iter().map(|r| r.stages.iter().sum()).collect();
	let first_rows = params.rows - params.tail_rows;
	let p50_s = std::time::Duration::from_nanos(median(&|r| r.stages.iter().sum())).as_secs_f64();
	#[allow(clippy::cast_precision_loss, reason = "a row count far below 2^52")]
	let throughput = if p50_s > 0.0 { first_rows as f64 / p50_s } else { 0.0 };
	Ok(BenchResult {
		schema_version: SCHEMA_VERSION,
		profile: profile.name.clone(),
		adapter: "weftdb".into(),
		workload: WORKLOAD_ANALYTICS_PIPELINE.into(),
		dataset: DatasetMeta { input_points: params.rows, output_points: point.peaks_found + point.valleys_found, irregular: timestamps.windows(3).any(|w| w[2] - w[1] != w[1] - w[0]), missingness_fraction, seed: 0, signal_shape: None },
		reps: params.reps,
		latency: LatencyStats::from_samples(&first_runs),
		latency_ci: None,
		cold_warm: None,
		throughput_points_per_sec: throughput,
		timing: TimingBreakdown { dataset_generation_ns: generation_ns, measured_ns, end_to_end_ns: span_ns(window_start) },
		correctness: CorrectnessReport { output_count_ok: exact, expected_output_points: point.peaks_expected + point.valleys_expected, actual_output_points: point.peaks_found + point.valleys_found, values_finite: true, result_sha256: Some(crate::schema::ResultDigest::of(last.peaks.iter().map(|t| (t, "peak")).chain(last.valleys.iter().map(|t| (t, "valley"))))) },
		accuracy: None,
		storage: None,
		gpu_pool: None,
		adapter_info: None,
		growth: None,
		online: None,
		compressed_query: None,
		analytics: Some(point),
		samples_ns: first_runs,
	})
}

/// One rep in a fresh database `name`: capture, first run stage by stage, tail capture,
/// incremental run, then the detectors' manifestations.
async fn run_rep(name: &str, measurements: &[InputMeasurement], params: AnalyticsParams) -> anyhow::Result<Rep> {
	let database = Database::new(name).await?;
	let subject = database.observe_subject("bench").await?;
	let aspect_id = database.track_aspect(&subject.id(), "value", &Resolution::Minutes, None).await?.id();
	let (head, tail) = measurements.split_at(measurements.len() - params.tail_rows);

	let started = Instant::now();
	capture(&database, &aspect_id, head).await?;
	let capture_ns = span_ns(started);
	progress("capture", capture_ns);

	let constraints = DictionaryConstraints::new(Some(Steps::new(10, Spline::Linear)), Some(vec![VariablilityType::MaximumStatic(Variability::new(BigDecimal::from_f64(0.1).ok_or_else(|| anyhow::anyhow!("0.1 is not a decimal"))?))]));
	let mut pipeline = Pipeline::builder(database.clone(), aspect_id)
		.spline_method(Spline::Linear)
		.batch_size(params.batch_size)
		.add_dictionary("bench", "the analytics workload's dictionary", constraints)
		.with_peak_detector(PEAKS)
		.with_valley_detector(VALLEYS)
		.build()
		.await?;
	let mut stages = [0_u64; 5];
	let started = Instant::now();
	pipeline.prepare_data().await?;
	stages[0] = span_ns(started);
	progress("prepare_data", stages[0]);
	let started = Instant::now();
	pipeline.extract_patterns().await?;
	stages[1] = span_ns(started);
	progress("extract_patterns", stages[1]);
	let started = Instant::now();
	pipeline.detect_events().await?;
	stages[2] = span_ns(started);
	progress("detect_events", stages[2]);
	let started = Instant::now();
	pipeline.correlate_events().await?;
	stages[3] = span_ns(started);
	progress("correlate_events", stages[3]);
	let started = Instant::now();
	pipeline.generate_signals().await?;
	pipeline.save().await?;
	stages[4] = span_ns(started);
	progress("generate_signals", stages[4]);

	capture(&database, &aspect_id, tail).await?;
	let started = Instant::now();
	pipeline.run().await?;
	let incremental_ns = span_ns(started);
	progress("incremental run", incremental_ns);

	let midpoints = |events: Vec<weftdb::Event>| events.iter().flat_map(|e| e.manifestations().values().map(|m| m.midpoint().timestamp()).collect::<Vec<_>>()).collect::<BTreeSet<i64>>();
	let peaks = midpoints(weft_orchestration::detect_peaks(&database, &aspect_id, &Resolution::Minutes, &Spline::Linear, PEAKS).await?);
	let valleys = midpoints(weft_orchestration::detect_valleys(&database, &aspect_id, &Resolution::Minutes, &Spline::Linear, VALLEYS).await?);
	let patterns = pipeline.get_dictionary("bench").map_or(0, weftdb::Dictionary::len);
	let signals = pipeline.signals().len();
	drop(pipeline);
	drop(database);
	Ok(Rep { capture_ns, stages, incremental_ns, patterns, signals, peaks, valleys })
}

/// Report a finished step on stderr, so a long run shows where it is.
fn progress(step: &str, ns: u64) {
	eprintln!("analytics: {step} took {:.1} s", std::time::Duration::from_nanos(ns).as_secs_f64());
}

/// Capture `measurements` into `aspect` in chunks.
async fn capture(database: &Database, aspect: &weftdb::AspectId, measurements: &[InputMeasurement]) -> anyhow::Result<()> {
	for chunk in measurements.chunks(CAPTURE_CHUNK) {
		database.capture_measurement_chunk(aspect, DatasetId::new(), chunk).await?;
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn bd(s: &str) -> BigDecimal {
		s.parse().expect("parses")
	}

	#[test]
	fn the_reference_keeps_only_strict_extremes_away_from_the_ends() {
		let ts = [0, 60, 120, 180, 240, 300, 360];
		let vs = [bd("9"), bd("1"), bd("9"), bd("2"), bd("9"), bd("9"), bd("0")];
		// The first 9 is an end; the last two 9s are a plateau, not strict.
		assert_eq!(extreme_reference(&ts, &vs, true), BTreeSet::from([120]));
		// The 0 is an end, so no valley; 1 is not the minimum.
		assert_eq!(extreme_reference(&ts, &vs, false), BTreeSet::new());
		let dips = [bd("5"), bd("1"), bd("5"), bd("1"), bd("3")];
		assert_eq!(extreme_reference(&ts[..5], &dips, false), BTreeSet::from([60, 180]));
	}
}
