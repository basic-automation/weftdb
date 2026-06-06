//! Latency statistics for benchmark samples.
//!
//! DSP-Bench reports the distribution required by the roadmap's fair-protocol
//! section (Phase 1.1): median/mean/stddev and p50/p95/p99/max. Percentiles use
//! the nearest-rank method on a sorted copy of the samples, which is stable for
//! the small-to-moderate sample counts produced by short benchmark runs.

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
		let variance = sorted.iter()
			.map(|&v| {
				let d = v as f64 - mean_f;
				d * d
			})
			.sum::<f64>() / count as f64;
		#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
		let stddev_ns = variance.sqrt().round() as u64;

		Self { count, min_ns, max_ns, mean_ns, stddev_ns, p50_ns: percentile(&sorted, 0.50), p95_ns: percentile(&sorted, 0.95), p99_ns: percentile(&sorted, 0.99) }
	}
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
}
