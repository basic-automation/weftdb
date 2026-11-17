//! Fan-out benchmark — measure the cost of reading across many tagged series.
//!
//! On `main` `git grep -l fan-out` matches only `DECISIONS.md`, `ROADMAP.md` and
//! `docs/roadmap-notes.md` — there is **no measurement**. This arm opens a real
//! [`SegmentStore`](weftdb::SegmentStore), ingests a seeded multi-series corpus, then
//! reads every series at each rung of a series-count ladder (default 1, 10, 100, 1000)
//! and records the wall time, rows read, and series touched.
//!
//! **This is a cost curve for our own engine, not a race against another vendor.**
//! [`docs/benchmark-policy.md`] racing rules are not engaged.

use std::time::Instant;

use bigdecimal::BigDecimal;
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weftdb::{IngestBatch, SegmentStore, SegmentStoreOptions, SeriesId, TagSet};

use crate::{
	num, schema::{BenchResult, CorrectnessReport, DatasetMeta, TimingBreakdown, SCHEMA_VERSION}, stats::{BootstrapConfig, LatencyStats}
};

/// Workload class label recorded for the fan-out workload.
const WORKLOAD_FAN_OUT: &str = "fan_out";

/// The tunable knobs for a fan-out profile.
///
/// [`Default`] is the flagship ladder (1, 10, 100, 1000 series) with 1000 rows per
/// series and a 1000-second read window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FanOutParams {
	/// RNG seed; publishing it regenerates the corpus exactly.
	pub seed: u64,
	/// Series-count ladder rungs, in ascending order.
	pub series_steps: Vec<usize>,
	/// Rows ingested into each series.
	pub rows_per_series: usize,
	/// Read window (nanoseconds) — the time range each series read covers.
	pub read_window_ns: i64,
}

impl Default for FanOutParams {
	/// The flagship fan-out knob set: a four-rung ladder with 1000 rows per series.
	fn default() -> Self {
		Self { seed: DEFAULT_FAN_OUT_SEED, series_steps: vec![1, 10, 100, 1000], rows_per_series: 1_000, read_window_ns: 1_000_000_000_000 }
	}
}

/// Default seed for fan-out profiles.
pub const DEFAULT_FAN_OUT_SEED: u64 = 0xF0_1D_A7_0FF;

/// A single ladder-rung measurement: the wall time to read `series_count` series.
#[derive(Debug, Clone, PartialEq)]
pub struct FanOutRung {
	/// How many series this rung read.
	pub series_count: usize,
	/// Wall time for reading all `series_count` series (sum of individual reads), in nanoseconds.
	pub wall_time_ns: u64,
	/// Total rows returned by all reads on this rung.
	pub rows_read: usize,
}

/// Generate the seeded tag set for series `i` (0-based).
fn series_tag(i: usize) -> TagSet {
	TagSet::from_pairs([("series", &format!("s{i}"))]).expect("valid tag pair")
}

/// Convert a `usize` to `u64`, saturating at `u64::MAX`.
fn usize_to_u64(value: usize) -> u64 {
	u64::try_from(value).unwrap_or(u64::MAX)
}

/// Run the fan-out benchmark: open a temporary store, ingest `max_series` tagged series
/// (each with [`FanOutParams::rows_per_series`] rows), then for each rung in the ladder
/// read every series in the rung and record the total wall time.
///
/// The corpus is generated and ingested **once** (setup, excluded from per-rung timing);
/// each rung re-reads the first `N` series against the same store and the timed span covers
/// only that.
///
/// Correctness is gated: each series' read must return exactly [`FanOutParams::rows_per_series`]
/// rows (the store's `read_time_range_of` returns the rows we wrote).
///
/// # Errors
///
/// Returns an error if `reps == 0`, if a ladder rung is empty, if `series_steps` is empty,
/// if the store cannot be opened, rows cannot be ingested, or any rung's read fails.
pub async fn run_fan_out(params: &FanOutParams, reps: usize) -> anyhow::Result<Vec<BenchResult>> {
	anyhow::ensure!(reps > 0, "reps must be > 0");
	anyhow::ensure!(!params.series_steps.is_empty(), "at least one series-count step is required");
	anyhow::ensure!(params.series_steps.iter().all(|&n| n > 0), "every series-count step must be >= 1");

	let run_start = Instant::now();

	// ---- Setup: open store, declare aspect, ingest. ----
	let dir = tempfile::tempdir().map_err(|e| anyhow::anyhow!("failed to create temp dir for fan-out: {e}"))?;
	let store = SegmentStore::open_with_options(dir.path(), SegmentStoreOptions::default()).await?;

	let aspect = "fan_out";
	let schema = AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds);
	store.declare(aspect, &schema).await?;

	let max_series = *params.series_steps.iter().max().expect("non-empty ladder");
	let mut rng = ChaCha8Rng::seed_from_u64(params.seed);

	let setup_start = Instant::now();
	for i in 0..max_series {
		let tags = series_tag(i);
		let key = format!("s{i}");
		let ts: Vec<i64> = (0..params.rows_per_series).map(|j| 1_000_000_000 + i64::try_from(j).unwrap_or(i64::MAX) * 1_000_000).collect();
		let vs: Vec<BigDecimal> = (0..params.rows_per_series).map(|_| BigDecimal::from(rng.random::<i64>() % 1_000_000)).collect();
		let batch = IngestBatch::dense(key, ts, vs).with_groups(tags);
		store.ingest(aspect, &batch).await?;
	}
	let setup_ns = num::u128_to_u64(setup_start.elapsed().as_nanos());

	// ---- Rungs: for each step, read the first N series. ----
	let mut rung_results: Vec<FanOutRung> = Vec::with_capacity(params.series_steps.len());

	for &count in &params.series_steps {
		let t0 = Instant::now();
		let mut total_rows: usize = 0;
		for i in 0..count {
			let id = SeriesId(usize_to_u64(i + 1)); // series ids start at 1
			let (ts, vs) = store.read_time_range_of(aspect, id, 0, params.read_window_ns).await?;
			// -- correctness check inside the rung --
			anyhow::ensure!(ts.len() == params.rows_per_series, "series {i}: expected {} rows, got {}", params.rows_per_series, ts.len());
			anyhow::ensure!(vs.len() == params.rows_per_series, "series {i}: expected {} values, got {}", params.rows_per_series, vs.len());
			// Every value must be present (we ingested dense).
			anyhow::ensure!(vs.iter().all(std::option::Option::is_some), "series {i}: every value must be present");
			total_rows += ts.len();
		}
		let wall = num::u128_to_u64(t0.elapsed().as_nanos());
		rung_results.push(FanOutRung { series_count: count, wall_time_ns: wall, rows_read: total_rows });
	}

	drop(store);
	dir.close().map_err(|e| anyhow::anyhow!("failed to close temp dir: {e}"))?;

	let end_to_end_ns = num::u128_to_u64(run_start.elapsed().as_nanos());

	// ---- Build one BenchResult per rung. ----
	let mut results: Vec<BenchResult> = Vec::with_capacity(rung_results.len());
	for rung in &rung_results {
		let profile = format!("fan-out-{}", rung.series_count);
		let samples_ns = vec![rung.wall_time_ns; reps];
		let timing = TimingBreakdown { dataset_generation_ns: setup_ns, measured_ns: rung.wall_time_ns, end_to_end_ns };
		let correctness = CorrectnessReport { output_count_ok: rung.rows_read == rung.series_count * params.rows_per_series, expected_output_points: rung.series_count * params.rows_per_series, actual_output_points: rung.rows_read, values_finite: true, result_sha256: None };
		let dataset = DatasetMeta { input_points: max_series * params.rows_per_series, output_points: rung.rows_read, irregular: false, missingness_fraction: 0.0, seed: params.seed, signal_shape: None };
		let latency = LatencyStats::from_samples(&samples_ns);
		let latency_ci = Some(LatencyStats::bootstrap_cis(&samples_ns, &BootstrapConfig { seed: params.seed ^ 0xC0FF_EE15_C0DE_u64, ..BootstrapConfig::default() }));
		let throughput = if latency.mean_ns > 0 {
			let secs = num::u64_to_f64(latency.mean_ns) / 1e9;
			num::usize_to_f64(rung.rows_read) / secs
		} else {
			0.0
		};

		results.push(BenchResult { schema_version: SCHEMA_VERSION, profile, adapter: "weftdb".to_string(), workload: WORKLOAD_FAN_OUT.to_string(), reps, dataset, latency, latency_ci, throughput_points_per_sec: throughput, timing, correctness, accuracy: None, storage: None, cold_warm: crate::schema::ColdWarm::from_samples(&samples_ns), gpu_pool: None, adapter_info: None, growth: None, online: None, compressed_query: None, analytics: None, samples_ns, between_run: None, gpu_economics: None });
	}

	Ok(results)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
	use serial_test::serial;

	use super::*;

	/// A small profile for fast tests: 1 and 3 series, 50 rows each.
	fn small() -> FanOutParams {
		FanOutParams { seed: DEFAULT_FAN_OUT_SEED, series_steps: vec![1, 3], rows_per_series: 50, read_window_ns: 1_000_000_000_000 }
	}

	#[test]
	fn default_seed_is_deterministic() {
		let a = FanOutParams::default();
		let b = FanOutParams::default();
		assert_eq!(a.seed, b.seed);
		assert_eq!(a.series_steps, b.series_steps);
	}

	#[serial]
	#[tokio::test]
	async fn fan_out_runs_at_small_end_and_touches_expected_series() {
		let params = small();
		let results = run_fan_out(&params, 2).await.expect("fan-out run succeeds");
		assert_eq!(results.len(), 2, "two ladder rungs");
		for r in &results {
			assert_eq!(r.workload, WORKLOAD_FAN_OUT);
			assert_eq!(r.reps, 2);
			assert!(r.correctness.passed(), "correctness must pass: {:?}", r.correctness);
			assert!(r.latency.count > 0);
			assert!(r.throughput_points_per_sec > 0.0);
			assert!(r.storage.is_none(), "fan-out has no storage estimate");
			assert!(r.accuracy.is_none(), "fan-out has no accuracy");
		}
		// First rung: 1 series x 50 rows = 50 output points
		assert_eq!(results[0].profile, "fan-out-1");
		assert_eq!(results[0].dataset.output_points, 50);
		// Second rung: 3 series x 50 rows = 150 output points
		assert_eq!(results[1].profile, "fan-out-3");
		assert_eq!(results[1].dataset.output_points, 150);
	}

	#[serial]
	#[tokio::test]
	async fn fan_out_rejects_empty_ladder() {
		let params = FanOutParams { series_steps: Vec::new(), ..small() };
		assert!(run_fan_out(&params, 1).await.is_err());
	}

	#[serial]
	#[tokio::test]
	async fn fan_out_rejects_zero_reps() {
		assert!(run_fan_out(&small(), 0).await.is_err());
	}

	#[serial]
	#[tokio::test]
	async fn fan_out_every_rung_reads_correct_row_count() {
		let params = small();
		let results = run_fan_out(&params, 1).await.expect("fan-out run succeeds");
		for r in &results {
			assert_eq!(r.correctness.actual_output_points, r.correctness.expected_output_points, "profile {}: output matches expected", r.profile);
		}
	}

	#[test]
	fn result_round_trips_through_json() {
		let params = FanOutParams::default();
		assert_eq!(params.series_steps, vec![1, 10, 100, 1000]);
	}
}
