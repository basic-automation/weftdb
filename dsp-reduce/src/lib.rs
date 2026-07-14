//! # dsp-reduce
//!
//! Vendor-neutral time-series **downsampling reductions** — the canonical home for
//! DSP's aggregation semantics, shared across the surfaces that reduce a series into
//! grid-aligned buckets (the HTTP `downsample` endpoint, the `dsp-bench` downsample
//! workload, and any future SDK).
//!
//! A reduction groups points into buckets aligned to the epoch grid at a chosen
//! [`splimes::Resolution`] (via [`splimes::Resolution::to_base`], the same index the
//! engine uses everywhere else), then computes one or more [`Aggregation`]s per
//! non-empty bucket. Per DSP's precision principle every reduction runs in
//! [`BigDecimal`] — the logical/API numeric type — so sums and averages carry no
//! float drift; a caller that needs `f64` (a JSON body, a plot) converts at its own
//! boundary, never inside the reduction.
//!
//! The core is intentionally small and dependency-light (`splimes` for `Point` /
//! `Resolution`, `bigdecimal`, `chrono`): it is a leaf crate every reducing surface
//! can depend on without pulling in an HTTP stack or a storage backend.

#![warn(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::module_name_repetitions)]

use std::collections::BTreeMap;

use bigdecimal::BigDecimal;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use splimes::{Point, Resolution, SECONDS_IN_DAY, SECONDS_IN_HOUR, SECONDS_IN_MINUTE, SECONDS_IN_MONTH, SECONDS_IN_WEEK, SECONDS_IN_YEAR};

/// A per-bucket reduction over the values that fell in the bucket.
///
/// The wire tokens are vendor-neutral lowercase (`"min"`, `"max"`, `"avg"`, `"sum"`,
/// `"first"`, `"last"`); the bucket count is always reported separately (see
/// [`Bucket::count`]) and so is not a member here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Aggregation {
	/// Smallest value in the bucket.
	Min,
	/// Largest value in the bucket.
	Max,
	/// Arithmetic mean of the bucket (sum ÷ count, computed in [`BigDecimal`]).
	Avg,
	/// Sum of the bucket's values.
	Sum,
	/// First value in the bucket by ascending timestamp.
	First,
	/// Last value in the bucket by ascending timestamp.
	Last,
	/// 50th percentile (median) of the bucket, by the nearest-rank method.
	P50,
	/// 90th percentile of the bucket, by the nearest-rank method.
	P90,
	/// 95th percentile of the bucket, by the nearest-rank method.
	P95,
	/// 99th percentile of the bucket, by the nearest-rank method.
	P99,
	/// Time-weighted average: each sample weighted by the time until the next sample
	/// in the bucket (a left-endpoint / LOCF weighting), so irregularly-spaced samples
	/// contribute in proportion to how long they were in effect.
	Twa,
}

impl Aggregation {
	/// The stable wire key this reduction is reported under.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Min => "min",
			Self::Max => "max",
			Self::Avg => "avg",
			Self::Sum => "sum",
			Self::First => "first",
			Self::Last => "last",
			Self::P50 => "p50",
			Self::P90 => "p90",
			Self::P95 => "p95",
			Self::P99 => "p99",
			Self::Twa => "twa",
		}
	}

	/// Parse a reduction from its wire token (case-insensitive), the inverse of
	/// [`as_str`](Self::as_str). Accepts `median` as an alias for `p50`. Returns `None`
	/// for an unknown token. Shared so every surface (HTTP query params, the bench CLI)
	/// parses the same set the reduction supports.
	#[must_use]
	pub fn from_token(token: &str) -> Option<Self> {
		match token.trim().to_ascii_lowercase().as_str() {
			"min" => Some(Self::Min),
			"max" => Some(Self::Max),
			"avg" => Some(Self::Avg),
			"sum" => Some(Self::Sum),
			"first" => Some(Self::First),
			"last" => Some(Self::Last),
			"p50" | "median" => Some(Self::P50),
			"p90" => Some(Self::P90),
			"p95" => Some(Self::P95),
			"p99" => Some(Self::P99),
			"twa" | "time_weighted_avg" => Some(Self::Twa),
			_ => None,
		}
	}

	/// The percentile rank in `1..=100` this reduction selects, or `None` for the
	/// non-percentile reductions.
	#[must_use]
	pub const fn percentile_rank(self) -> Option<u8> {
		match self {
			Self::P50 => Some(50),
			Self::P90 => Some(90),
			Self::P95 => Some(95),
			Self::P99 => Some(99),
			_ => None,
		}
	}

	/// Whether this reduction needs the whole bucket materialized (percentiles need
	/// the sorted values; [`Self::Twa`] needs the time-ordered samples). The streaming
	/// reductions do not, so [`reduce`] only collects samples when one of these is asked.
	#[must_use]
	pub const fn needs_full_bucket(self) -> bool {
		self.percentile_rank().is_some() || matches!(self, Self::Twa)
	}

	/// Every reduction, in a stable order — the natural set for a full downsample. Kept
	/// to the six *streaming* reductions (percentiles need the full bucket materialized,
	/// so they are opt-in rather than part of the default full set).
	pub const ALL: [Self; 6] = [Self::Min, Self::Max, Self::Avg, Self::Sum, Self::First, Self::Last];

	/// The default reduction set (`min`/`max`/`avg`) applied when a caller requests
	/// none explicitly — the classic downsample triple.
	pub const DEFAULT: [Self; 3] = [Self::Min, Self::Max, Self::Avg];
}

/// One emitted bucket of a reduction.
///
/// Carries its grid-aligned start, the number of input samples that fell in it, and
/// the requested reductions keyed by their wire name ([`Aggregation::as_str`]).
/// Values stay in [`BigDecimal`] — no lossy `f64` cast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bucket {
	/// Grid-aligned bucket start timestamp (the inverse of [`Resolution::to_base`]).
	pub timestamp: DateTime<Utc>,
	/// Number of input samples that fell in this bucket.
	pub count: usize,
	/// The requested reductions, keyed by their wire name.
	pub values: BTreeMap<String, BigDecimal>,
}

/// Why a reduction could not be produced.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReduceError {
	/// A point's timestamp could not be mapped to a bucket index at the resolution
	/// (only nanosecond resolution can overflow, for a timestamp far outside the
	/// representable nanosecond-epoch range).
	#[error("a timestamp cannot be indexed at the requested resolution")]
	TimestampRange,
	/// A bucket's grid-aligned start timestamp scaled past the representable
	/// `i64`-second range (a coarse resolution far from the epoch).
	#[error("a bucket start overflows the representable time range")]
	BucketStartOverflow,
}

/// Running aggregate state for one bucket, accumulated in [`BigDecimal`] so sums and
/// averages carry no float drift. `first`/`last` track their value **with** the
/// timestamp, so the reduction is correct regardless of input order (no pre-sort
/// required). The full bucket is materialized in `samples` only when a percentile
/// reduction is requested (they need the sorted set); otherwise it stays empty so the
/// streaming reductions pay no collection cost.
#[derive(Debug)]
struct BucketAcc {
	count: usize,
	sum: BigDecimal,
	min: Option<BigDecimal>,
	max: Option<BigDecimal>,
	first: Option<(DateTime<Utc>, BigDecimal)>,
	last: Option<(DateTime<Utc>, BigDecimal)>,
	/// `(timestamp, value)` pairs, populated only when `collect` is set (a percentile
	/// or time-weighted average is requested).
	samples: Vec<(DateTime<Utc>, BigDecimal)>,
	collect: bool,
}

impl BucketAcc {
	/// A fresh accumulator; `collect` materializes the full bucket for percentiles / TWA.
	fn new(collect: bool) -> Self {
		Self { count: 0, sum: BigDecimal::from(0), min: None, max: None, first: None, last: None, samples: Vec::new(), collect }
	}

	/// Fold one `(timestamp, value)` into the bucket.
	fn push(&mut self, timestamp: DateTime<Utc>, value: BigDecimal) {
		self.count += 1;
		self.sum += &value;
		if self.min.as_ref().is_none_or(|m| &value < m) {
			self.min = Some(value.clone());
		}
		if self.max.as_ref().is_none_or(|m| &value > m) {
			self.max = Some(value.clone());
		}
		if self.first.as_ref().is_none_or(|(t, _)| timestamp < *t) {
			self.first = Some((timestamp, value.clone()));
		}
		if self.collect {
			self.samples.push((timestamp, value.clone()));
		}
		if self.last.as_ref().is_none_or(|(t, _)| timestamp >= *t) {
			self.last = Some((timestamp, value));
		}
	}

	/// Materialize the requested reductions and the grid-aligned bucket start.
	fn finish(self, resolution: Resolution, base: i64, aggregations: &[Aggregation]) -> Result<Bucket, ReduceError> {
		let timestamp = bucket_start(resolution, base).ok_or(ReduceError::BucketStartOverflow)?;
		let count = BigDecimal::from(self.count as u64);
		// Percentiles read the values in ascending value order; sort a value-only copy
		// once, lazily, only if a percentile is requested.
		let sorted_values: Option<Vec<BigDecimal>> = aggregations.iter().any(|a| a.percentile_rank().is_some()).then(|| {
			let mut v: Vec<BigDecimal> = self.samples.iter().map(|(_, val)| val.clone()).collect();
			v.sort();
			v
		});
		let mut values: BTreeMap<String, BigDecimal> = BTreeMap::new();
		for &agg in aggregations {
			let value = if let Some(rank) = agg.percentile_rank() {
				sorted_values.as_deref().and_then(|s| percentile(s, rank))
			} else {
				match agg {
					Aggregation::Min => self.min.clone(),
					Aggregation::Max => self.max.clone(),
					Aggregation::Sum => Some(self.sum.clone()),
					Aggregation::Avg => (self.count > 0).then(|| &self.sum / &count),
					Aggregation::First => self.first.as_ref().map(|(_, v)| v.clone()),
					Aggregation::Last => self.last.as_ref().map(|(_, v)| v.clone()),
					Aggregation::Twa => time_weighted_average(&self.samples),
					Aggregation::P50 | Aggregation::P90 | Aggregation::P95 | Aggregation::P99 => unreachable!("handled by percentile_rank above"),
				}
			};
			if let Some(value) = value {
				values.insert(agg.as_str().to_string(), value);
			}
		}
		Ok(Bucket { timestamp, count: self.count, values })
	}
}

/// The **time-weighted average** of `samples`: each sample weighted by the time until
/// the next sample in ascending-timestamp order (a left-endpoint / LOCF weighting).
///
/// `None` for an empty slice; a single sample is its own value. When every sample
/// shares one instant (total weight zero), falls back to the unweighted arithmetic
/// mean. Weights are measured in milliseconds — the unit cancels in the ratio, so it
/// only bounds sub-millisecond resolution, which a downsample bucket never needs.
fn time_weighted_average(samples: &[(DateTime<Utc>, BigDecimal)]) -> Option<BigDecimal> {
	if samples.is_empty() {
		return None;
	}
	if samples.len() == 1 {
		return Some(samples[0].1.clone());
	}
	let mut ordered: Vec<&(DateTime<Utc>, BigDecimal)> = samples.iter().collect();
	ordered.sort_by_key(|(t, _)| *t);
	let mut weighted = BigDecimal::from(0);
	let mut total_ms: i64 = 0;
	for pair in ordered.windows(2) {
		let dt = (pair[1].0 - pair[0].0).num_milliseconds().max(0);
		if dt > 0 {
			weighted += &pair[0].1 * BigDecimal::from(dt);
			total_ms += dt;
		}
	}
	if total_ms == 0 {
		// Degenerate: all samples within one millisecond — no time spread to weight by,
		// so report the plain arithmetic mean.
		let sum: BigDecimal = samples.iter().map(|(_, v)| v.clone()).sum();
		return Some(sum / BigDecimal::from(samples.len() as u64));
	}
	Some(weighted / BigDecimal::from(total_ms))
}

/// The nearest-rank percentile of a **sorted** `samples` slice: the value at rank
/// `ceil(rank/100 * n)` (1-based, clamped into range). `None` for an empty slice.
/// Nearest-rank returns an actual observed value (no interpolation), which keeps the
/// result exact in [`BigDecimal`].
fn percentile(samples: &[BigDecimal], rank: u8) -> Option<BigDecimal> {
	if samples.is_empty() {
		return None;
	}
	// idx = ceil(rank/100 * n) - 1, clamped to [0, n-1]. Integer arithmetic:
	// ceil(rank * n / 100) = (rank * n + 99) / 100.
	let n = samples.len();
	let ordinal = (usize::from(rank) * n).div_ceil(100); // 1-based, >= 1 for rank >= 1
	let idx = ordinal.saturating_sub(1).min(n - 1);
	Some(samples[idx].clone())
}

/// Reduce `points` into grid-aligned buckets at `resolution`.
///
/// Computes each of `aggregations` per non-empty bucket, returned ascending by time.
/// Points whose timestamp falls outside the inclusive `[start, end]` range (when a
/// bound is given) are excluded. Only non-empty buckets are emitted. All arithmetic
/// is in [`BigDecimal`]. When `aggregations` is empty, [`Aggregation::DEFAULT`]
/// (`min`/`max`/`avg`) is applied.
///
/// # Errors
///
/// Returns [`ReduceError::TimestampRange`] if a point cannot be indexed at the
/// resolution, or [`ReduceError::BucketStartOverflow`] if a bucket's grid start
/// scales past the representable time range.
pub fn reduce(points: &[Point], resolution: Resolution, start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>, aggregations: &[Aggregation]) -> Result<Vec<Bucket>, ReduceError> {
	let aggregations = if aggregations.is_empty() { &Aggregation::DEFAULT[..] } else { aggregations };
	// Percentiles and TWA require the full bucket materialized; the streaming
	// reductions do not, so only collect when one of those is actually requested.
	let collect = aggregations.iter().any(|a| a.needs_full_bucket());

	// A BTreeMap keyed by the bucket index yields buckets in ascending index order,
	// which is ascending time order for a fixed resolution.
	let mut buckets: BTreeMap<i64, BucketAcc> = BTreeMap::new();
	for p in points {
		if start.is_some_and(|s| p.timestamp < s) || end.is_some_and(|e| p.timestamp > e) {
			continue;
		}
		let base = resolution.to_base(&p.timestamp).map_err(|_| ReduceError::TimestampRange)?;
		buckets.entry(base).or_insert_with(|| BucketAcc::new(collect)).push(p.timestamp, p.value.clone());
	}

	buckets.into_iter().map(|(base, acc)| acc.finish(resolution, base, aggregations)).collect()
}

/// Reconstruct a bucket's grid-aligned start timestamp from its resolution index.
///
/// The inverse of [`Resolution::to_base`]. Returns `None` only if the index scales
/// past the representable `i64`-second range.
#[must_use]
pub fn bucket_start(resolution: Resolution, base: i64) -> Option<DateTime<Utc>> {
	let duration = match resolution {
		Resolution::Nanoseconds => Duration::nanoseconds(base),
		Resolution::Microseconds => Duration::microseconds(base),
		Resolution::Milliseconds => Duration::milliseconds(base),
		Resolution::Seconds => Duration::seconds(base),
		Resolution::Minutes => Duration::seconds(base.checked_mul(SECONDS_IN_MINUTE)?),
		Resolution::Hours => Duration::seconds(base.checked_mul(SECONDS_IN_HOUR)?),
		Resolution::Days => Duration::seconds(base.checked_mul(SECONDS_IN_DAY)?),
		Resolution::Weeks => Duration::seconds(base.checked_mul(SECONDS_IN_WEEK)?),
		Resolution::Months => Duration::seconds(base.checked_mul(SECONDS_IN_MONTH)?),
		Resolution::Years => Duration::seconds(base.checked_mul(SECONDS_IN_YEAR)?),
	};
	DateTime::<Utc>::UNIX_EPOCH.checked_add_signed(duration)
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use bigdecimal::ToPrimitive;
	use chrono::TimeZone;

	use super::*;

	fn pt(secs: i64, value: &str) -> Point {
		Point { timestamp: Utc.timestamp_opt(secs, 0).single().expect("valid instant"), value: BigDecimal::from_str(value).expect("valid decimal") }
	}

	fn get(b: &Bucket, agg: Aggregation) -> f64 {
		b.values.get(agg.as_str()).expect("aggregation present").to_f64().expect("finite")
	}

	#[test]
	fn reduces_into_minute_buckets_with_every_aggregation() {
		// Two minute-buckets: 00:00 holds t=0,30 (10, 20); 00:01 holds t=60,90 (30, 5).
		let points = vec![pt(0, "10"), pt(30, "20"), pt(60, "30"), pt(90, "5")];
		let buckets = reduce(&points, Resolution::Minutes, None, None, &Aggregation::ALL).expect("reduces");
		assert_eq!(buckets.len(), 2, "two non-empty minute buckets");

		let b0 = &buckets[0];
		assert_eq!(b0.timestamp, Utc.timestamp_opt(0, 0).single().unwrap());
		assert_eq!(b0.count, 2);
		assert!((get(b0, Aggregation::Min) - 10.0).abs() < 1e-9);
		assert!((get(b0, Aggregation::Max) - 20.0).abs() < 1e-9);
		assert!((get(b0, Aggregation::Avg) - 15.0).abs() < 1e-9);
		assert!((get(b0, Aggregation::Sum) - 30.0).abs() < 1e-9);
		assert!((get(b0, Aggregation::First) - 10.0).abs() < 1e-9);
		assert!((get(b0, Aggregation::Last) - 20.0).abs() < 1e-9);

		let b1 = &buckets[1];
		assert_eq!(b1.timestamp, Utc.timestamp_opt(60, 0).single().unwrap());
		assert!((get(b1, Aggregation::Min) - 5.0).abs() < 1e-9);
		assert!((get(b1, Aggregation::Max) - 30.0).abs() < 1e-9);
		assert!((get(b1, Aggregation::First) - 30.0).abs() < 1e-9, "first by ascending time");
		assert!((get(b1, Aggregation::Last) - 5.0).abs() < 1e-9, "last by ascending time");
	}

	#[test]
	fn first_last_are_by_time_regardless_of_input_order() {
		// Scrambled input order: first/last must still track earliest/latest timestamp.
		let points = vec![pt(90, "5"), pt(0, "10"), pt(30, "20"), pt(60, "30")];
		let buckets = reduce(&points, Resolution::Minutes, None, None, &[Aggregation::First, Aggregation::Last]).expect("reduces");
		assert_eq!(buckets.len(), 2);
		assert!((get(&buckets[0], Aggregation::First) - 10.0).abs() < 1e-9, "earliest in bucket 0 is t=0 -> 10");
		assert!((get(&buckets[0], Aggregation::Last) - 20.0).abs() < 1e-9, "latest in bucket 0 is t=30 -> 20");
		assert!((get(&buckets[1], Aggregation::Last) - 5.0).abs() < 1e-9, "latest in bucket 1 is t=90 -> 5");
	}

	#[test]
	fn average_is_exact_in_bigdecimal() {
		// 1/3 + values whose f64 average would drift: BigDecimal keeps it exact.
		let points = vec![pt(0, "0.1"), pt(1, "0.2"), pt(2, "0.3")];
		let buckets = reduce(&points, Resolution::Minutes, None, None, &[Aggregation::Sum, Aggregation::Avg]).expect("reduces");
		assert_eq!(buckets.len(), 1);
		let sum = buckets[0].values.get("sum").unwrap();
		assert_eq!(sum, &BigDecimal::from_str("0.6").unwrap(), "sum is exact in BigDecimal");
		let avg = buckets[0].values.get("avg").unwrap();
		assert_eq!(avg, &BigDecimal::from_str("0.2").unwrap(), "avg is exact in BigDecimal");
	}

	#[test]
	fn range_bounds_exclude_out_of_range_points() {
		let points = vec![pt(0, "10"), pt(60, "20"), pt(120, "30"), pt(180, "40")];
		// Keep only [60, 120]: two buckets (00:01, 00:02).
		let start = Utc.timestamp_opt(60, 0).single().unwrap();
		let end = Utc.timestamp_opt(120, 0).single().unwrap();
		let buckets = reduce(&points, Resolution::Minutes, Some(start), Some(end), &[Aggregation::Sum]).expect("reduces");
		assert_eq!(buckets.len(), 2, "the two in-range points land in two buckets");
		assert_eq!(buckets[0].timestamp, start);
		assert_eq!(buckets[1].timestamp, end);
	}

	#[test]
	fn empty_aggregations_applies_the_default_triple() {
		let points = vec![pt(0, "10"), pt(30, "20")];
		let buckets = reduce(&points, Resolution::Minutes, None, None, &[]).expect("reduces");
		assert_eq!(buckets.len(), 1);
		let keys: Vec<&str> = buckets[0].values.keys().map(String::as_str).collect();
		assert_eq!(keys, vec!["avg", "max", "min"], "default is min/max/avg (BTreeMap orders the keys)");
	}

	#[test]
	fn nearest_rank_percentiles_select_observed_values() {
		// A single minute-bucket of 1..=10. Nearest-rank: p50 -> idx ceil(.5*10)-1 = 4 -> 5,
		// p90 -> ceil(.9*10)-1 = 8 -> 9, p95 -> ceil(.95*10)-1 = 9 -> 10, p99 -> 10.
		let points: Vec<Point> = (1..=10).map(|v| pt(i64::from(v - 1), &v.to_string())).collect();
		let buckets = reduce(&points, Resolution::Minutes, None, None, &[Aggregation::P50, Aggregation::P90, Aggregation::P95, Aggregation::P99]).expect("reduces");
		assert_eq!(buckets.len(), 1);
		let b = &buckets[0];
		assert!((get(b, Aggregation::P50) - 5.0).abs() < 1e-9, "p50 nearest-rank of 1..10 is 5");
		assert!((get(b, Aggregation::P90) - 9.0).abs() < 1e-9, "p90 is 9");
		assert!((get(b, Aggregation::P95) - 10.0).abs() < 1e-9, "p95 is 10");
		assert!((get(b, Aggregation::P99) - 10.0).abs() < 1e-9, "p99 is 10");
	}

	#[test]
	fn percentiles_are_order_independent_and_exact() {
		// Scrambled input, and a percentile must return an exact observed BigDecimal.
		let points = vec![pt(2, "3.3"), pt(0, "1.1"), pt(1, "2.2")];
		let buckets = reduce(&points, Resolution::Minutes, None, None, &[Aggregation::P50]).expect("reduces");
		use std::str::FromStr;
		assert_eq!(buckets[0].values.get("p50").unwrap(), &BigDecimal::from_str("2.2").unwrap(), "p50 of 1.1/2.2/3.3 is exactly 2.2");
	}

	#[test]
	fn from_token_round_trips_as_str_and_rejects_unknown() {
		for agg in [Aggregation::Min, Aggregation::Max, Aggregation::Avg, Aggregation::Sum, Aggregation::First, Aggregation::Last, Aggregation::P50, Aggregation::P90, Aggregation::P95, Aggregation::P99, Aggregation::Twa] {
			assert_eq!(Aggregation::from_token(agg.as_str()), Some(agg), "{} must round-trip", agg.as_str());
		}
		assert_eq!(Aggregation::from_token("MEDIAN"), Some(Aggregation::P50), "median is a case-insensitive p50 alias");
		assert_eq!(Aggregation::from_token(" avg "), Some(Aggregation::Avg), "surrounding whitespace is trimmed");
		assert_eq!(Aggregation::from_token("bogus"), None, "an unknown token is rejected");
	}

	#[test]
	fn time_weighted_average_weights_by_dwell_time() {
		// Value 10 held from t=0 to t=30 (30s), value 20 from t=30 to t=60 (30s), value 5
		// at t=60 (the last sample carries no forward weight). TWA = (10*30 + 20*30)/60 = 15.
		// The unweighted mean would be (10+20+5)/3 = 11.67, so the weighting is visible.
		let points = vec![pt(0, "10"), pt(30, "20"), pt(60, "5")];
		let buckets = reduce(&points, Resolution::Hours, None, None, &[Aggregation::Twa, Aggregation::Avg]).expect("reduces");
		assert_eq!(buckets.len(), 1, "all three fall in one hour-bucket");
		assert!((get(&buckets[0], Aggregation::Twa) - 15.0).abs() < 1e-9, "TWA weights 10 and 20 over equal 30s spans -> 15");
		// Order-independence: scrambling the input yields the same TWA.
		let scrambled = vec![pt(60, "5"), pt(0, "10"), pt(30, "20")];
		let b2 = reduce(&scrambled, Resolution::Hours, None, None, &[Aggregation::Twa]).expect("reduces");
		assert!((get(&b2[0], Aggregation::Twa) - 15.0).abs() < 1e-9, "TWA is order-independent");
	}

	#[test]
	fn twa_single_sample_is_the_value_and_same_instant_is_the_mean() {
		// One sample -> its own value.
		let one = reduce(&[pt(5, "42")], Resolution::Minutes, None, None, &[Aggregation::Twa]).expect("reduces");
		assert!((get(&one[0], Aggregation::Twa) - 42.0).abs() < 1e-9);
		// Two samples at the same instant (zero time spread) -> arithmetic mean.
		let same = reduce(&[pt(5, "10"), pt(5, "20")], Resolution::Minutes, None, None, &[Aggregation::Twa]).expect("reduces");
		assert!((get(&same[0], Aggregation::Twa) - 15.0).abs() < 1e-9, "zero spread falls back to the mean");
	}

	#[test]
	fn no_points_yields_no_buckets() {
		let buckets = reduce(&[], Resolution::Minutes, None, None, &Aggregation::ALL).expect("reduces");
		assert!(buckets.is_empty(), "an empty input yields no buckets");
	}

	#[test]
	fn seconds_resolution_gives_one_bucket_per_second() {
		let points = vec![pt(0, "1"), pt(0, "2"), pt(1, "3")];
		let buckets = reduce(&points, Resolution::Seconds, None, None, &[Aggregation::Sum]).expect("reduces");
		assert_eq!(buckets.len(), 2, "t=0 (two samples) and t=1 (one) are separate second-buckets");
		assert_eq!(buckets[0].count, 2);
		assert_eq!(buckets[1].count, 1);
	}

	#[test]
	fn bucket_start_inverts_to_base() {
		// For a representative resolution, bucket_start(to_base(t)) aligns t down to the grid.
		let t = Utc.timestamp_opt(3661, 0).single().unwrap(); // 01:01:01
		let base = Resolution::Minutes.to_base(&t).unwrap();
		let start = bucket_start(Resolution::Minutes, base).unwrap();
		assert_eq!(start, Utc.timestamp_opt(3660, 0).single().unwrap(), "aligns down to 01:01:00");
	}
}
