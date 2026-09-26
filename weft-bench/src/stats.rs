//! Latency statistics for benchmark samples.
//!
//! Weft-Bench reports the distribution required by the roadmap's fair-protocol
//! section (Phase 1.1): median/mean/stddev and p50/p95/p99/max **with bootstrap
//! confidence intervals**. Percentiles use the nearest-rank method on a sorted
//! copy of the samples, which is stable for the small-to-moderate sample counts
//! produced by short benchmark runs.
//!
//! Confidence intervals are produced by seeded nonparametric bootstrap
//! resampling ([`LatencyStats::bootstrap_cis`]): the sample is resampled with
//! replacement `resamples` times, the statistic of interest is recomputed on
//! each resample, and the interval is read off the resulting bootstrap
//! distribution by the percentile method. The RNG seed is part of the published
//! config so an interval is exactly reproducible — the same fair-protocol
//! discipline applied to dataset generation.

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

/// Summary statistics for a set of latency samples, all in nanoseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyStats {
	/// Number of samples summarized.
	pub count: usize,
	/// Smallest observed latency (ns).
	pub min_ns: u64,
	/// Largest observed latency (ns).
	pub max_ns: u64,
	/// Arithmetic mean latency (ns), rounded to the nearest integer.
	pub mean_ns: u64,
	/// Population standard deviation (ns), rounded to the nearest integer.
	pub stddev_ns: u64,
	/// 50th percentile / median (ns).
	pub p50_ns: u64,
	/// 95th percentile (ns).
	pub p95_ns: u64,
	/// 99th percentile (ns).
	pub p99_ns: u64,
}

impl LatencyStats {
	/// Summarize a slice of latency samples (nanoseconds).
	///
	/// Returns an all-zero summary with `count == 0` when `samples` is empty so
	/// callers never have to special-case the empty run.
	#[must_use]
	pub fn from_samples(samples: &[u64]) -> Self {
		if samples.is_empty() {
			return Self { count: 0, min_ns: 0, max_ns: 0, mean_ns: 0, stddev_ns: 0, p50_ns: 0, p95_ns: 0, p99_ns: 0 };
		}

		let mut sorted = samples.to_vec();
		sorted.sort_unstable();

		let count = sorted.len();
		let min_ns = sorted[0];
		let max_ns = sorted[count - 1];

		let sum: u128 = sorted.iter().map(|&v| u128::from(v)).sum();
		#[allow(clippy::cast_possible_truncation)]
		let mean_ns = (sum / count as u128) as u64;

		// Population variance computed in f64 to avoid overflow; rounded to ns.
		// The integer→f64 casts are intentional for statistical aggregation and
		// lose at most sub-nanosecond significance at realistic sample counts.
		#[allow(clippy::cast_precision_loss)]
		let mean_f = sum as f64 / count as f64;
		#[allow(clippy::cast_precision_loss)]
		let variance = sorted
			.iter()
			.map(|&v| {
				let d = v as f64 - mean_f;
				d * d
			})
			.sum::<f64>()
			/ count as f64;
		#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
		let stddev_ns = variance.sqrt().round() as u64;

		Self { count, min_ns, max_ns, mean_ns, stddev_ns, p50_ns: percentile(&sorted, 0.50), p95_ns: percentile(&sorted, 0.95), p99_ns: percentile(&sorted, 0.99) }
	}

	/// Compute seeded bootstrap confidence intervals for the mean and the p50,
	/// p95 and p99 latency percentiles.
	///
	/// The sample is resampled with replacement `config.resamples` times; each
	/// statistic is recomputed on every resample and the interval is the
	/// `config.confidence`-level percentile interval of the resulting bootstrap
	/// distribution. The point estimate is the statistic computed on the original
	/// sample (identical to the matching [`LatencyStats`] field). Resampling is
	/// driven by a `ChaCha8` RNG seeded from `config.seed`, so a given
	/// `(samples, config)` pair always yields the same intervals.
	///
	/// An empty sample yields all-zero intervals; a single-element sample yields
	/// degenerate intervals where `lower == point == upper`.
	#[must_use]
	pub fn bootstrap_cis(samples: &[u64], config: &BootstrapConfig) -> LatencyCis {
		let confidence = config.confidence.clamp(0.0, 1.0);
		if samples.is_empty() || config.resamples == 0 {
			let zero = ConfidenceInterval { point_ns: 0, lower_ns: 0, upper_ns: 0 };
			return LatencyCis { resamples: config.resamples, confidence, seed: config.seed, mean: zero.clone(), p50: zero.clone(), p95: zero.clone(), p99: zero };
		}

		let mut original = samples.to_vec();
		original.sort_unstable();
		let n = original.len();

		// Bootstrap distributions, one per reported statistic.
		let mut mean_dist: Vec<u64> = Vec::with_capacity(config.resamples);
		let mut p50_dist: Vec<u64> = Vec::with_capacity(config.resamples);
		let mut p95_dist: Vec<u64> = Vec::with_capacity(config.resamples);
		let mut p99_dist: Vec<u64> = Vec::with_capacity(config.resamples);

		let mut rng = ChaCha8Rng::seed_from_u64(config.seed);
		let mut resample = vec![0u64; n];
		for _ in 0..config.resamples {
			let mut sum: u128 = 0;
			for slot in &mut resample {
				let pick = original[rng.random_range(0..n)];
				*slot = pick;
				sum += u128::from(pick);
			}
			resample.sort_unstable();
			#[allow(clippy::cast_possible_truncation)]
			mean_dist.push((sum / n as u128) as u64);
			p50_dist.push(percentile(&resample, 0.50));
			p95_dist.push(percentile(&resample, 0.95));
			p99_dist.push(percentile(&resample, 0.99));
		}

		let sum: u128 = original.iter().map(|&v| u128::from(v)).sum();
		#[allow(clippy::cast_possible_truncation)]
		let mean_point = (sum / n as u128) as u64;

		LatencyCis { resamples: config.resamples, confidence, seed: config.seed, mean: interval_from(&mut mean_dist, mean_point, confidence), p50: interval_from(&mut p50_dist, percentile(&original, 0.50), confidence), p95: interval_from(&mut p95_dist, percentile(&original, 0.95), confidence), p99: interval_from(&mut p99_dist, percentile(&original, 0.99), confidence) }
	}
}

/// Configuration for [`LatencyStats::bootstrap_cis`]. All three knobs are
/// published alongside the interval so it is exactly reproducible.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BootstrapConfig {
	/// Number of bootstrap resamples to draw.
	pub resamples: usize,
	/// Confidence level in `(0.0, 1.0)` (e.g. `0.95` for a 95% interval).
	pub confidence: f64,
	/// Seed for the resampling RNG (publishable for reproducibility).
	pub seed: u64,
}

impl Default for BootstrapConfig {
	/// A 95% interval from 1000 resamples with a fixed seed — a reasonable
	/// default for the short runs Weft-Bench produces.
	fn default() -> Self {
		Self { resamples: 1000, confidence: 0.95, seed: 0xB007_5EED }
	}
}

/// A single confidence interval, in nanoseconds: the point estimate plus its
/// lower and upper bounds. `lower_ns <= point_ns <= upper_ns` always holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfidenceInterval {
	/// Statistic computed on the original sample.
	pub point_ns: u64,
	/// Lower bound of the bootstrap interval.
	pub lower_ns: u64,
	/// Upper bound of the bootstrap interval.
	pub upper_ns: u64,
}

/// Bootstrap confidence intervals for the headline latency statistics, plus the
/// config that produced them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatencyCis {
	/// Number of bootstrap resamples drawn.
	pub resamples: usize,
	/// Confidence level used (clamped to `[0.0, 1.0]`).
	pub confidence: f64,
	/// RNG seed used for resampling.
	pub seed: u64,
	/// Interval for the mean latency.
	pub mean: ConfidenceInterval,
	/// Interval for the p50 latency.
	pub p50: ConfidenceInterval,
	/// Interval for the p95 latency.
	pub p95: ConfidenceInterval,
	/// Interval for the p99 latency.
	pub p99: ConfidenceInterval,
}

/// Read a percentile-method confidence interval off a bootstrap distribution.
///
/// `dist` is sorted in place. The bounds are the `(1 - confidence) / 2` and
/// `(1 + confidence) / 2` quantiles. The point estimate is clamped into
/// `[lower, upper]` so the documented `lower <= point <= upper` invariant holds
/// even when the original statistic lands just outside the resampled spread.
fn interval_from(dist: &mut [u64], point_ns: u64, confidence: f64) -> ConfidenceInterval {
	dist.sort_unstable();
	let alpha = 1.0 - confidence;
	let lower_ns = percentile(dist, alpha / 2.0);
	let upper_ns = percentile(dist, 1.0 - alpha / 2.0);
	let point_ns = point_ns.clamp(lower_ns, upper_ns);
	ConfidenceInterval { point_ns, lower_ns, upper_ns }
}

/// Nearest-rank percentile of an already-sorted, non-empty slice.
///
/// `q` is a quantile in `[0.0, 1.0]`. The rank is `ceil(q * n)` clamped to a
/// valid index, matching the common nearest-rank definition.
#[must_use]
fn percentile(sorted: &[u64], q: f64) -> u64 {
	debug_assert!(!sorted.is_empty(), "percentile of empty slice");
	let n = sorted.len();
	if q <= 0.0 {
		return sorted[0];
	}
	if q >= 1.0 {
		return sorted[n - 1];
	}
	#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
	let rank = (q * n as f64).ceil() as usize;
	let idx = rank.saturating_sub(1).min(n - 1);
	sorted[idx]
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn empty_samples_yield_zeroed_summary() {
		let stats = LatencyStats::from_samples(&[]);
		assert_eq!(stats.count, 0);
		assert_eq!(stats.max_ns, 0);
		assert_eq!(stats.p99_ns, 0);
	}

	#[test]
	fn single_sample_is_every_percentile() {
		let stats = LatencyStats::from_samples(&[42]);
		assert_eq!(stats.count, 1);
		assert_eq!(stats.min_ns, 42);
		assert_eq!(stats.max_ns, 42);
		assert_eq!(stats.mean_ns, 42);
		assert_eq!(stats.stddev_ns, 0);
		assert_eq!(stats.p50_ns, 42);
		assert_eq!(stats.p95_ns, 42);
		assert_eq!(stats.p99_ns, 42);
	}

	#[test]
	fn percentiles_use_nearest_rank() {
		// 1..=100; nearest-rank: p50 -> idx 49 (50), p95 -> idx 94 (95), p99 -> idx 98 (99).
		let samples: Vec<u64> = (1..=100).collect();
		let stats = LatencyStats::from_samples(&samples);
		assert_eq!(stats.count, 100);
		assert_eq!(stats.min_ns, 1);
		assert_eq!(stats.max_ns, 100);
		assert_eq!(stats.p50_ns, 50);
		assert_eq!(stats.p95_ns, 95);
		assert_eq!(stats.p99_ns, 99);
	}

	#[test]
	fn unsorted_input_is_summarized_correctly() {
		let stats = LatencyStats::from_samples(&[5, 1, 3, 2, 4]);
		assert_eq!(stats.min_ns, 1);
		assert_eq!(stats.max_ns, 5);
		assert_eq!(stats.mean_ns, 3);
		assert_eq!(stats.p50_ns, 3);
	}

	#[test]
	fn bootstrap_is_deterministic_for_a_given_seed() {
		let samples: Vec<u64> = (1..=200).collect();
		let cfg = BootstrapConfig { resamples: 500, confidence: 0.95, seed: 12345 };
		let a = LatencyStats::bootstrap_cis(&samples, &cfg);
		let b = LatencyStats::bootstrap_cis(&samples, &cfg);
		assert_eq!(a, b, "same (samples, config) must yield identical intervals");
	}

	#[test]
	fn bootstrap_seed_changes_the_interval() {
		let samples: Vec<u64> = (1..=200).collect();
		let a = LatencyStats::bootstrap_cis(&samples, &BootstrapConfig { resamples: 500, confidence: 0.95, seed: 1 });
		let b = LatencyStats::bootstrap_cis(&samples, &BootstrapConfig { resamples: 500, confidence: 0.95, seed: 2 });
		// Different seeds explore different resamples; the mean interval should
		// differ for a spread-out sample (vanishingly unlikely to coincide).
		assert_ne!((a.mean.lower_ns, a.mean.upper_ns), (b.mean.lower_ns, b.mean.upper_ns));
	}

	#[test]
	fn bootstrap_interval_brackets_the_point_and_is_ordered() {
		let samples: Vec<u64> = (1..=200).collect();
		let cis = LatencyStats::bootstrap_cis(&samples, &BootstrapConfig::default());
		for ci in [&cis.mean, &cis.p50, &cis.p95, &cis.p99] {
			assert!(ci.lower_ns <= ci.point_ns, "lower {} > point {}", ci.lower_ns, ci.point_ns);
			assert!(ci.point_ns <= ci.upper_ns, "point {} > upper {}", ci.point_ns, ci.upper_ns);
		}
		// The mean point estimate must match the deterministic summary.
		assert_eq!(cis.mean.point_ns, LatencyStats::from_samples(&samples).mean_ns);
		// Config is echoed back for reproducibility.
		assert_eq!(cis.resamples, 1000);
		assert!((cis.confidence - 0.95).abs() < f64::EPSILON);
	}

	#[test]
	fn bootstrap_of_constant_sample_is_degenerate() {
		let cis = LatencyStats::bootstrap_cis(&[42, 42, 42, 42], &BootstrapConfig::default());
		// Every resample of a constant is the same constant.
		assert_eq!(cis.mean, ConfidenceInterval { point_ns: 42, lower_ns: 42, upper_ns: 42 });
		assert_eq!(cis.p99, ConfidenceInterval { point_ns: 42, lower_ns: 42, upper_ns: 42 });
	}

	#[test]
	fn bootstrap_of_empty_or_zero_resamples_is_zeroed() {
		let zeroed = ConfidenceInterval { point_ns: 0, lower_ns: 0, upper_ns: 0 };

		let empty = LatencyStats::bootstrap_cis(&[], &BootstrapConfig::default());
		assert_eq!(empty.mean, zeroed);
		assert_eq!(empty.p95, zeroed);

		let no_resamples = LatencyStats::bootstrap_cis(&[1, 2, 3], &BootstrapConfig { resamples: 0, confidence: 0.95, seed: 1 });
		assert_eq!(no_resamples.p50, zeroed);
	}
}
