//! Gap-fill workload — a bucketed query that must return **every** grid step, filling the
//! steps an outage left empty.
//!
//! This is the shape of TSM-Bench's interpolation query Q5 (`SELECT … SAMPLE BY 5s
//! FILL(LINEAR)`, PVLDB 16(11)): reduce a series into grid-aligned buckets, then synthesize
//! the empty ones. The workload generates a dense, seeded series (a smooth sinusoid plus a
//! little noise, at two decimal places like the downsample generator), deletes whole
//! **outages** (runs of consecutive buckets), and times [`weft_reduce::reduce`] followed by
//! [`weft_reduce::fill`] over what remains.
//!
//! Unlike the plain downsample it has a ground truth to score: the generator knows the
//! noise-free signal under every deleted bucket, so the report's accuracy block compares
//! each **filled** bucket's value with the mean of the clean signal over that bucket's
//! instants. Measured buckets are not scored (they are exact reductions). Correctness is
//! that the output grid is dense, every outage bucket was synthesized with `count == 0`, the
//! measured buckets hold every surviving sample, and every value is finite.
//!
//! WeftDB's grid only has unit widths (`s`/`m`/`h`/…), so Q5's 5-second buckets are not
//! expressible; the default here is one-second samples into one-minute buckets.

use std::{collections::BTreeSet, time::Instant};

use bigdecimal::{BigDecimal, ToPrimitive};
use chrono::{DateTime, Duration, TimeZone, Utc};
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use splimes::{Point, Resolution};
use weft_reduce::{bucket_index, fill, reduce, Aggregation, Bucket, Fill};

use crate::{
	accuracy::AccuracyMetrics, schema::{BenchResult, CorrectnessReport, DatasetMeta, TimingBreakdown, SCHEMA_VERSION}, stats::{BootstrapConfig, LatencyStats}
};

/// Workload class label recorded for the gap-fill workload.
const WORKLOAD_GAP_FILL: &str = "gap_fill";

/// A fixed epoch anchor (2020-01-01T00:00:00Z), as the downsample workload uses.
const EPOCH_ANCHOR_SECS: i64 = 1_577_836_800;

/// Default seed for gap-fill profiles.
pub const DEFAULT_GAP_FILL_SEED: u64 = 0x6A9F_111E_D5EE_D001;

/// The most buckets a gap-fill query may emit (the dense grid is bounded, as a server
/// would bound it).
const MAX_GRID_BUCKETS: usize = 10_000_000;

/// The tunable knobs for a gap-fill profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapFillParams {
	/// RNG seed; publishing it regenerates the corpus and its outages exactly.
	pub seed: u64,
	/// Samples generated before the outages are cut (one every [`Self::input_stride_secs`]).
	pub point_count: usize,
	/// Spacing between samples, in seconds.
	pub input_stride_secs: i64,
	/// Bucket resolution of the query.
	pub bucket_resolution: Resolution,
	/// Share of the interior buckets removed by outages, in percent (`0..=90`).
	pub outage_percent: u32,
	/// Mean outage length in buckets (each outage is 1..=2×mean−1 buckets, uniformly).
	pub mean_outage_buckets: u32,
	/// How the empty buckets are filled.
	pub method: Fill,
	/// The reduction computed per bucket and scored.
	pub aggregation: Aggregation,
}

impl Default for GapFillParams {
	/// One day of one-second samples into one-minute buckets, 20% of the buckets lost to
	/// outages averaging five minutes, filled linearly, `avg` per bucket.
	fn default() -> Self {
		Self { seed: DEFAULT_GAP_FILL_SEED, point_count: 86_400, input_stride_secs: 1, bucket_resolution: Resolution::Minutes, outage_percent: 20, mean_outage_buckets: 5, method: Fill::Linear, aggregation: Aggregation::Avg }
	}
}

/// A reproducible gap-fill workload profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapFillProfile {
	/// Human-readable profile name, recorded in results.
	pub name: String,
	/// The knobs, clamped to their valid ranges.
	pub params: GapFillParams,
}

/// A generated gap-fill corpus: the surviving samples and, for every deleted bucket, the
/// mean of the noise-free signal over the instants it covered.
#[derive(Debug, Clone, PartialEq)]
pub struct GapFillCorpus {
	/// The samples outside the outages, ascending.
	pub points: Vec<Point>,
	/// `(bucket start, clean mean)` for each deleted bucket, ascending.
	pub truth: Vec<(DateTime<Utc>, f64)>,
	/// Samples generated before the outages were cut.
	pub generated: usize,
	/// The first and last bucket of the grid (both kept, so every outage is interior).
	pub span: (DateTime<Utc>, DateTime<Utc>),
}

impl GapFillProfile {
	/// Build a profile, clamping the knobs (at least three buckets' worth of samples is the
	/// caller's job; the outage share is capped at 90% and the mean length at ≥ 1).
	#[must_use]
	pub fn new(name: impl Into<String>, params: GapFillParams) -> Self {
		let params = GapFillParams { point_count: params.point_count.max(2), input_stride_secs: params.input_stride_secs.max(1), outage_percent: params.outage_percent.min(90), mean_outage_buckets: params.mean_outage_buckets.max(1), ..params };
		Self { name: name.into(), params }
	}

	/// The noise-free signal at sample `i`: a one-hour-period sinusoid around 50.
	fn clean(&self, i: usize) -> f64 {
		use std::f64::consts::TAU;
		#[allow(clippy::cast_precision_loss)]
		let seconds = (i as f64) * (self.params.input_stride_secs as f64);
		20.0_f64.mul_add((seconds / 3_600.0 * TAU).sin(), 50.0)
	}

	/// Generate the series, cut the outages, and record the truth under them.
	///
	/// # Panics
	///
	/// Never for a profile built by [`Self::new`]: the fixed anchor plus the generated span
	/// stays inside chrono's range and indexes at every resolution.
	#[must_use]
	pub fn generate(&self) -> GapFillCorpus {
		let p = &self.params;
		let mut rng = ChaCha8Rng::seed_from_u64(p.seed);
		let anchor = Utc.timestamp_opt(EPOCH_ANCHOR_SECS, 0).single().expect("valid fixed epoch anchor");
		let n = p.point_count;
		#[allow(clippy::cast_possible_wrap)]
		let instant = |i: usize| anchor + Duration::seconds(i as i64 * p.input_stride_secs);
		let step = |i: usize| bucket_index(p.bucket_resolution, &instant(i)).expect("indexable");
		let (first, last) = (step(0), step(n - 1));

		// Outages: walking the interior buckets, start one with the probability that makes
		// the expected removed share `outage_percent` (an outage of mean length m costs m
		// buckets, then at least one kept bucket separates it from the next).
		let share = f64::from(p.outage_percent) / 100.0;
		let mean = f64::from(p.mean_outage_buckets);
		let start_probability = if share > 0.0 { (share / mean.mul_add(1.0 - share, share)).min(1.0) } else { 0.0 };
		let mut removed = BTreeSet::new();
		let mut b = first + 1;
		while b < last {
			if rng.random_range(0.0..1.0) < start_probability {
				let length = i64::from(rng.random_range(1..=2 * p.mean_outage_buckets - 1));
				removed.extend(b..(b + length).min(last));
				b += length + 1;
			} else {
				b += 1;
			}
		}

		let mut points = Vec::with_capacity(n);
		let mut sums: std::collections::BTreeMap<i64, (f64, u32)> = std::collections::BTreeMap::new();
		for i in 0..n {
			let clean = self.clean(i);
			let noise = rng.random_range(-1.0..=1.0);
			let s = step(i);
			if removed.contains(&s) {
				let entry = sums.entry(s).or_insert((0.0, 0));
				entry.0 += clean;
				entry.1 += 1;
				continue;
			}
			#[allow(clippy::cast_possible_truncation)]
			let cents = ((clean + noise) * 100.0).round() as i64;
			points.push(Point { timestamp: instant(i), value: BigDecimal::new(cents.into(), 2) });
		}
		let start_of = |s: i64| weft_reduce::bucket_start(p.bucket_resolution, s).expect("in range");
		let truth = sums.into_iter().map(|(s, (sum, count))| (start_of(s), sum / f64::from(count))).collect();
		GapFillCorpus { points, truth, generated: n, span: (start_of(first), start_of(last)) }
	}
}

/// Elapsed nanoseconds since `since`, saturated into a `u64`.
#[allow(clippy::cast_possible_truncation)]
fn span_ns(since: Instant) -> u64 {
	since.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

/// The gap-fill query: reduce, then fill the grid between the corpus bounds.
fn query(profile: &GapFillProfile, corpus: &GapFillCorpus) -> anyhow::Result<Vec<Bucket>> {
	let p = &profile.params;
	let buckets = reduce(&corpus.points, p.bucket_resolution, None, None, &[p.aggregation]).map_err(|e| anyhow::anyhow!("reduction failed: {e}"))?;
	fill(&buckets, p.bucket_resolution, Some(corpus.span.0), Some(corpus.span.1), &p.method, MAX_GRID_BUCKETS).map_err(|e| anyhow::anyhow!("fill failed: {e}"))
}

/// Run a gap-fill profile for `reps` timed repetitions and return a fully-populated
/// [`BenchResult`] (workload `gap_fill`).
///
/// The corpus is generated once (setup, excluded from latency); each rep runs the whole
/// query (reduce + fill). Throughput is input samples queried per second. The accuracy
/// block scores the filled buckets against the clean signal; a fill that synthesizes no
/// values ([`Fill::Null`]) has none.
///
/// # Errors
///
/// Returns an error if `reps == 0`, if the corpus has no outage to fill or no samples, or
/// if the query fails.
pub fn run_gap_fill(profile: &GapFillProfile, reps: usize) -> anyhow::Result<BenchResult> {
	anyhow::ensure!(reps > 0, "reps must be > 0");
	let run_start = Instant::now();
	let corpus = profile.generate();
	let generation_ns = span_ns(run_start);
	anyhow::ensure!(!corpus.points.is_empty(), "the corpus has no samples");
	anyhow::ensure!(!corpus.truth.is_empty(), "the corpus has no outage to fill (raise --gf-outage or --gf-points)");

	let mut samples_ns = Vec::with_capacity(reps);
	let mut grid = Vec::new();
	for _ in 0..reps {
		let t0 = Instant::now();
		grid = query(profile, &corpus)?;
		samples_ns.push(span_ns(t0));
	}
	let measured_ns = samples_ns.iter().copied().fold(0_u64, u64::saturating_add);
	let timing = TimingBreakdown { dataset_generation_ns: generation_ns, measured_ns, end_to_end_ns: span_ns(run_start) };

	// Correctness: one bucket per grid step, the outage buckets exactly the synthesized
	// ones, the measured ones holding every surviving sample, every value finite.
	let p = &profile.params;
	let steps: Vec<i64> = grid.iter().map(|b| bucket_index(p.bucket_resolution, &b.timestamp)).collect::<Result<_, _>>().map_err(|e| anyhow::anyhow!("{e}"))?;
	let dense = steps.windows(2).all(|w| w[1] == w[0] + 1) && grid.first().map(|b| b.timestamp) == Some(corpus.span.0) && grid.last().map(|b| b.timestamp) == Some(corpus.span.1);
	let filled: Vec<&Bucket> = grid.iter().filter(|b| b.count == 0).collect();
	let filled_match = filled.len() == corpus.truth.len() && filled.iter().zip(&corpus.truth).all(|(b, (t, _))| b.timestamp == *t);
	let counted: usize = grid.iter().map(|b| b.count).sum();
	let values_finite = grid.iter().flat_map(|b| b.values.values()).all(|v| v.to_f64().is_some_and(f64::is_finite));
	let correctness = CorrectnessReport { output_count_ok: dense && filled_match && counted == corpus.points.len(), expected_output_points: corpus.truth.len(), actual_output_points: filled.len(), values_finite };

	// Accuracy: each filled bucket's value against the clean mean under it.
	let key = p.aggregation.as_str();
	let scored: Vec<(Point, f64)> = filled.iter().zip(&corpus.truth).filter_map(|(b, &(timestamp, truth))| b.values.get(key).map(|v| (Point { timestamp, value: v.clone() }, truth))).collect();
	let accuracy = if scored.is_empty() {
		None
	} else {
		let (predicted, truth): (Vec<Point>, Vec<f64>) = scored.into_iter().unzip();
		Some(AccuracyMetrics::from_aligned(&predicted, &truth).map_err(|e| anyhow::anyhow!("scoring the filled buckets failed: {e}"))?)
	};

	let latency = LatencyStats::from_samples(&samples_ns);
	let latency_ci = Some(LatencyStats::bootstrap_cis(&samples_ns, &BootstrapConfig { seed: p.seed ^ 0x_6A9F_C0DE_u64, ..BootstrapConfig::default() }));
	#[allow(clippy::cast_precision_loss)]
	let throughput_points_per_sec = if latency.mean_ns > 0 { corpus.points.len() as f64 / (latency.mean_ns as f64 / 1e9) } else { 0.0 };
	#[allow(clippy::cast_precision_loss)]
	let missingness_fraction = 1.0 - corpus.points.len() as f64 / corpus.generated as f64;
	Ok(BenchResult { schema_version: SCHEMA_VERSION, profile: profile.name.clone(), adapter: "weftdb".to_string(), workload: WORKLOAD_GAP_FILL.to_string(), reps, dataset: DatasetMeta { input_points: corpus.points.len(), output_points: grid.len(), irregular: false, missingness_fraction, seed: p.seed, signal_shape: None }, latency, latency_ci, throughput_points_per_sec, timing, correctness, accuracy, storage: None })
}

#[cfg(test)]
mod tests {
	use super::*;

	fn small(method: Fill) -> GapFillProfile {
		GapFillProfile::new("gf-small", GapFillParams { point_count: 6 * 3_600, method, ..GapFillParams::default() })
	}

	#[test]
	fn outages_are_interior_reproducible_and_near_the_requested_share() {
		let profile = small(Fill::Linear);
		let corpus = profile.generate();
		assert_eq!(profile.generate(), corpus, "seeded");
		let grid_buckets = 6 * 60;
		#[allow(clippy::cast_precision_loss)]
		let share = corpus.truth.len() as f64 / f64::from(grid_buckets - 2);
		assert!((0.1..0.3).contains(&share), "removed share {share} should be near 20%");
		assert!(corpus.truth.iter().all(|(t, _)| *t > corpus.span.0 && *t < corpus.span.1), "outages never touch the grid edges");
		assert!(corpus.points.iter().all(|p| p.value.fractional_digit_count() == 2));
		assert_eq!(corpus.points.len() + corpus.truth.len() * 60, corpus.generated, "a removed minute loses its 60 samples");
	}

	#[test]
	fn every_method_passes_and_only_value_fills_are_scored() {
		for method in [Fill::Linear, Fill::Previous, Fill::Value(BigDecimal::from(50))] {
			let result = run_gap_fill(&small(method.clone()), 2).expect("runs");
			assert!(result.correctness.passed(), "{method:?}: {:?}", result.correctness);
			assert_eq!(result.workload, "gap_fill");
			let accuracy = result.accuracy.expect("value fills are scored");
			assert_eq!(accuracy.count, result.correctness.expected_output_points);
		}
		let null = run_gap_fill(&small(Fill::Null), 1).expect("runs");
		assert!(null.correctness.passed());
		assert!(null.accuracy.is_none(), "a null fill synthesizes nothing to score");
	}

	#[test]
	fn linear_fill_beats_carrying_the_last_value_on_a_smooth_signal() {
		let linear = run_gap_fill(&small(Fill::Linear), 1).expect("runs").accuracy.expect("scored");
		let previous = run_gap_fill(&small(Fill::Previous), 1).expect("runs").accuracy.expect("scored");
		assert!(linear.rmse < previous.rmse, "linear {} vs previous {}", linear.rmse, previous.rmse);
	}

	#[test]
	fn no_outage_is_an_error_not_a_vacuous_pass() {
		let profile = GapFillProfile::new("gf-none", GapFillParams { point_count: 3_600, outage_percent: 0, ..GapFillParams::default() });
		assert!(run_gap_fill(&profile, 1).is_err());
		assert!(run_gap_fill(&small(Fill::Linear), 0).is_err());
	}
}
