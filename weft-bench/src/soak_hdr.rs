//! HDR-style histogram for soak interval logs (1.0 plan SOA-3, W2).
//!
//! The soak's per-interval latency summaries need a histogram whose error
//! stays bounded in *relative* terms across the whole range a step can take —
//! from a sub-microsecond null-server tick to a multi-second stalled step —
//! which is what an HDR histogram gives: bucket widths double every 1024
//! buckets, so any recorded value lands in a bucket at most ~0.1% wide
//! relative to the value itself (three significant figures).
//!
//! This is an in-crate implementation, not the `hdrhistogram` crate: that
//! crate is not in the workspace lockfile, and the workspace gates build
//! `--locked`, so a new dependency would be a lockfile change for a
//! structure small enough to own. The bucketing below is the HDR scheme
//! (1024 sub-buckets per power-of-two range); only the storage differs —
//! counts are kept in a plain growable vector indexed by the bucket
//! function, rather than in HDR's packed counts array.
//!
//! Bucket function, for a value `v` in nanoseconds:
//!
//! * `v < 1024`: bucket `v` itself — every nanosecond exact.
//! * `v >= 1024`: let `s = floor(log2(v)) - 10`, so `v` lies in
//!   `[2^(s+10), 2^(s+11))`; the bucket is `1024 * (s + 1) +
//!   (v - 2^(s+10)) / 2^s`. Each power-of-two range is split into 1024
//!   equal buckets, so the bucket holding `v` is at most `v / 1024` wide.
//!
//! Percentiles return the bucket's *upper* bound (HDR's "highest
//! equivalent value" convention): a percentile of an exact small value is
//! that value, and a percentile of a large value is within 0.1% above it.
//! Minimum, maximum and mean are tracked exactly (the sum in `u128`, so
//! no overflow for any realistic run), not read back from buckets.

/// An HDR-style histogram over nanosecond values.
///
/// Values are recorded in nanoseconds; [`HdrHistogram::record`] takes the
/// raw `u64` so callers convert from [`std::time::Duration`] with
/// saturation at the call site.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HdrHistogram {
	/// Count per bucket, indexed by [`bucket_index`].
	counts: Vec<u64>,
	/// Total number of recorded values.
	total: u64,
	/// Sum of recorded values, for an exact mean.
	sum_ns: u128,
	/// Smallest recorded value, if any.
	min_ns: Option<u64>,
	/// Largest recorded value, if any.
	max_ns: Option<u64>,
}

/// Number of sub-buckets per power-of-two range (2^10: three significant
/// figures).
const SUB_BUCKETS: u64 = 1024;

impl HdrHistogram {
	/// An empty histogram.
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// Bucket index for `value` (see the module docs).
	fn bucket_index(value: u64) -> usize {
		if value < SUB_BUCKETS {
			return value as usize;
		}
		// floor(log2(value)) >= 10 here, so `s >= 0` and the shifts below
		// cannot overflow: `s + 10 <= 63` for any `u64` value.
		let floor_log2 = 63 - value.leading_zeros() as usize;
		let s = floor_log2 - 10;
		let start = 1_u64 << (s + 10);
		let width = 1_u64 << s;
		(1024 * (s + 1) + ((value - start) / width) as usize) as usize
	}

	/// Upper bound (inclusive) of the bucket at `index`.
	fn bucket_upper(index: usize) -> u64 {
		if (index as u64) < SUB_BUCKETS {
			return index as u64;
		}
		let s = index / 1024 - 1;
		let sub = index % 1024;
		let start = 1_u128 << (s + 10);
		let width = 1_u128 << s;
		(start + (sub as u128 + 1) * width - 1) as u64
	}

	/// Record one value.
	pub fn record(&mut self, value_ns: u64) {
		let index = Self::bucket_index(value_ns);
		if self.counts.len() <= index {
			self.counts.resize(index + 1, 0);
		}
		self.counts[index] = self.counts[index].saturating_add(1);
		self.total = self.total.saturating_add(1);
		self.sum_ns += u128::from(value_ns);
		self.min_ns = Some(self.min_ns.map_or(value_ns, |min| min.min(value_ns)));
		self.max_ns = Some(self.max_ns.map_or(value_ns, |max| max.max(value_ns)));
	}

	/// Merge another histogram into this one (for a whole-run summary over
	/// per-interval histograms).
	pub fn merge(&mut self, other: &Self) {
		if self.counts.len() < other.counts.len() {
			self.counts.resize(other.counts.len(), 0);
		}
		for (index, count) in other.counts.iter().enumerate() {
			self.counts[index] = self.counts[index].saturating_add(*count);
		}
		self.total = self.total.saturating_add(other.total);
		self.sum_ns += other.sum_ns;
		self.min_ns = match (self.min_ns, other.min_ns) {
			(Some(a), Some(b)) => Some(a.min(b)),
			(a, b) => a.or(b),
		};
		self.max_ns = match (self.max_ns, other.max_ns) {
			(Some(a), Some(b)) => Some(a.max(b)),
			(a, b) => a.or(b),
		};
	}

	/// Number of recorded values.
	#[must_use]
	pub const fn total_count(&self) -> u64 {
		self.total
	}

	/// Smallest recorded value, if any.
	#[must_use]
	pub const fn min_ns(&self) -> Option<u64> {
		self.min_ns
	}

	/// Largest recorded value, if any.
	#[must_use]
	pub const fn max_ns(&self) -> Option<u64> {
		self.max_ns
	}

	/// Exact mean of recorded values, if any.
	#[must_use]
	pub fn mean_ns(&self) -> Option<f64> {
		if self.total == 0 {
			None
		} else {
			Some(self.sum_ns as f64 / self.total as f64)
		}
	}

	/// Value at the given percentile (0–100, clamped): the smallest bucket
	/// upper bound whose cumulative count reaches the percentile rank.
	/// Returns `None` for an empty histogram.
	///
	/// # Panics
	///
	/// Never panics on a finite percentile; non-finite input is clamped
	/// to the ends of the range.
	#[must_use]
	pub fn value_at_percentile(&self, percentile: f64) -> Option<u64> {
		if self.total == 0 {
			return None;
		}
		let clamped = if percentile.is_nan() { 0.0 } else { percentile.clamp(0.0, 100.0) };
		let rank = ((clamped / 100.0) * self.total as f64).ceil() as u64;
		let rank = rank.max(1);
		let mut cumulative: u64 = 0;
		for (index, count) in self.counts.iter().enumerate() {
			cumulative = cumulative.saturating_add(*count);
			if cumulative >= rank {
				return Some(Self::bucket_upper(index));
			}
		}
		// Unreachable when `total > 0` (the counts sum to `total`), but
		// fall back to the exact maximum rather than failing.
		self.max_ns
	}

	/// Median (p50).
	#[must_use]
	pub fn p50_ns(&self) -> Option<u64> {
		self.value_at_percentile(50.0)
	}

	/// 90th percentile.
	#[must_use]
	pub fn p90_ns(&self) -> Option<u64> {
		self.value_at_percentile(90.0)
	}

	/// 99th percentile.
	#[must_use]
	pub fn p99_ns(&self) -> Option<u64> {
		self.value_at_percentile(99.0)
	}

	/// 99.9th percentile.
	#[must_use]
	pub fn p999_ns(&self) -> Option<u64> {
		self.value_at_percentile(99.9)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn empty_histogram_has_no_stats() {
		let hist = HdrHistogram::new();
		assert_eq!(hist.total_count(), 0);
		assert_eq!(hist.min_ns(), None);
		assert_eq!(hist.max_ns(), None);
		assert_eq!(hist.mean_ns(), None);
		assert_eq!(hist.value_at_percentile(50.0), None);
	}

	#[test]
	fn small_values_are_exact() {
		let mut hist = HdrHistogram::new();
		for value in 1..=1000_u64 {
			hist.record(value);
		}
		assert_eq!(hist.total_count(), 1000);
		assert_eq!(hist.min_ns(), Some(1));
		assert_eq!(hist.max_ns(), Some(1000));
		assert!((hist.mean_ns().expect("mean") - 500.5).abs() < f64::EPSILON);
		assert_eq!(hist.p50_ns(), Some(500));
		assert_eq!(hist.p90_ns(), Some(900));
		assert_eq!(hist.p99_ns(), Some(990));
	}

	#[test]
	fn large_values_stay_within_hdr_error() {
		// Values around one second in nanoseconds: bucket width there is
		// 2^20 ns (~1 ms), i.e. ~0.1% of the value.
		let mut hist = HdrHistogram::new();
		for _ in 0..100 {
			hist.record(1_000_000_000);
		}
		let p50 = hist.p50_ns().expect("p50");
		let value = 1_000_000_000_u64;
		assert!(p50 >= value, "percentile must not under-report: {p50}");
		assert!(p50 - value <= value / 500, "percentile must stay within ~0.2% above the value: {p50}");
		assert_eq!(hist.min_ns(), Some(value));
		assert_eq!(hist.max_ns(), Some(value));
		assert_eq!(hist.mean_ns(), Some(value as f64));
	}

	#[test]
	fn repeated_single_value_reports_that_value_at_every_percentile() {
		let mut hist = HdrHistogram::new();
		for _ in 0..50 {
			hist.record(42);
		}
		assert_eq!(hist.p50_ns(), Some(42));
		assert_eq!(hist.p99_ns(), Some(42));
		assert_eq!(hist.value_at_percentile(100.0), Some(42));
		assert_eq!(hist.value_at_percentile(0.0), Some(42));
	}

	#[test]
	fn merge_combines_counts_and_exact_stats() {
		let mut left = HdrHistogram::new();
		left.record(10);
		left.record(20);
		let mut right = HdrHistogram::new();
		right.record(30);
		left.merge(&right);
		assert_eq!(left.total_count(), 3);
		assert_eq!(left.min_ns(), Some(10));
		assert_eq!(left.max_ns(), Some(30));
		assert!((left.mean_ns().expect("mean") - 20.0).abs() < f64::EPSILON);
		assert_eq!(left.p50_ns(), Some(20));
	}
}
