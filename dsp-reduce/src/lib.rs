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

pub mod sketch;

use std::collections::BTreeMap;

use bigdecimal::BigDecimal;
pub use sketch::{DdSketch, SketchError};
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
	/// Time-weighted average by the **linear (trapezoidal)** method: each interval
	/// contributes the mean of its two endpoints, `Σ½(vᵢ+vᵢ₊₁)Δtᵢ / ΣΔtᵢ`, i.e. the
	/// signal is taken to move along the line between measurements rather than holding
	/// constant. Best for irregularly-sampled *continuous* signals (temperature, flow);
	/// [`Twa`](Self::Twa)'s LOCF weighting is best for change-only/step sensors.
	TwaLinear,
	/// Time-weighted average by LOCF weighting that additionally carries the bucket's
	/// **last sample forward to the bucket's end**.
	///
	/// [`Twa`](Self::Twa) gives the final sample no weight (it has no successor to
	/// measure against), which biases a bucket toward its earlier values — a sensor that
	/// reports `0` at the start and `100` just after it averages near `0`, though it held
	/// `100` for nearly the whole bucket. Because a bucket's grid end is known, that dwell
	/// *is* measurable, and this reduction counts it. Only meaningful for LOCF: the linear
	/// method has no successor value to interpolate toward.
	TwaBucketEnd,
	/// Approximate 50th percentile via a mergeable [`DdSketch`] — bounded memory, with
	/// the relative error bounded by [`SKETCH_ALPHA`].
	SketchP50,
	/// Approximate 90th percentile via a mergeable [`DdSketch`].
	SketchP90,
	/// Approximate 95th percentile via a mergeable [`DdSketch`].
	SketchP95,
	/// Approximate 99th percentile via a mergeable [`DdSketch`].
	SketchP99,
}

/// The relative-error bound of the `sketch_p*` reductions: 1%.
///
/// The common default for latency monitoring. Declared and fixed rather than silent —
/// the exact nearest-rank percentiles remain available whenever the approximation is
/// not acceptable.
///
/// **Rank convention.** The `sketch_p*` reductions share the exact `p*` reductions'
/// nearest-rank convention (1-based ordinal `⌈q·n⌉`), so the two name the **same sample
/// at every bucket size** and `sketch_p*` is always within [`SKETCH_ALPHA`] of `p*` —
/// they are substitutable. (`DdSketch`'s reference rank is `⌊q·(n-1)⌋`, which on a small
/// bucket selects a *different* sample — `p99` of three would be the middle one; DSP
/// deliberately does not inherit that.) The exact percentiles still cost nothing on a
/// small bucket; the sketch earns its keep when a bucket is too large to materialize or
/// the result must merge.
pub const SKETCH_ALPHA: f64 = 0.01;

/// The bucket budget of the `sketch_p*` reductions, per sign store.
///
/// Makes the reductions' memory bound *absolute* rather than merely logarithmic in the
/// value range. At [`SKETCH_ALPHA`] this covers a dynamic range of roughly `1.0202^2048`
/// (~10¹⁷ — sub-nanosecond to astronomical in one bucket), so a realistic column never
/// reaches it and never collapses; it exists to cap the pathological case rather than to
/// bite in practice. See [`DdSketch::with_max_bins`] for what collapsing costs when it
/// does trigger.
pub const SKETCH_MAX_BINS: usize = 2048;

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
			Self::TwaLinear => "twa_linear",
			Self::TwaBucketEnd => "twa_bucket_end",
			Self::SketchP50 => "sketch_p50",
			Self::SketchP90 => "sketch_p90",
			Self::SketchP95 => "sketch_p95",
			Self::SketchP99 => "sketch_p99",
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
			"twa" | "time_weighted_avg" | "twa_locf" => Some(Self::Twa),
			"twa_linear" | "time_weighted_avg_linear" => Some(Self::TwaLinear),
			"twa_bucket_end" | "twa_locf_end" => Some(Self::TwaBucketEnd),
			"sketch_p50" | "sketch_median" => Some(Self::SketchP50),
			"sketch_p90" => Some(Self::SketchP90),
			"sketch_p95" => Some(Self::SketchP95),
			"sketch_p99" => Some(Self::SketchP99),
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

	/// The quantile this reduction reads from the bucket's [`DdSketch`], or `None` for
	/// the non-sketch reductions.
	#[must_use]
	pub const fn sketch_quantile(self) -> Option<f64> {
		match self {
			Self::SketchP50 => Some(0.50),
			Self::SketchP90 => Some(0.90),
			Self::SketchP95 => Some(0.95),
			Self::SketchP99 => Some(0.99),
			_ => None,
		}
	}

	/// Whether this reduction needs the whole bucket materialized (the exact
	/// nearest-rank percentiles need the sorted values; the time-weighted averages need
	/// the time-ordered samples). The streaming reductions do not, so [`reduce`] only
	/// collects samples when one of these is asked.
	///
	/// The `sketch_p*` reductions are deliberately **not** here: they fold each value
	/// into a [`DdSketch`] as it arrives, which is the whole point — bounded memory
	/// regardless of bucket size. Collecting for them would forfeit the saving.
	#[must_use]
	pub const fn needs_full_bucket(self) -> bool {
		self.percentile_rank().is_some() || matches!(self, Self::Twa | Self::TwaLinear | Self::TwaBucketEnd)
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
	/// A `sketch_p*` reduction was requested but a value has no finite `f64` image, so
	/// it cannot be mapped into a sketch bucket. Surfaced rather than dropped: silently
	/// skipping a sample would corrupt the quantile without any signal.
	#[error("a value cannot be represented in the quantile sketch")]
	SketchValue,
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
	/// Present only when a `sketch_p*` reduction is requested; values fold in as they
	/// arrive, so the bucket's memory stays bounded no matter how many samples land.
	sketch: Option<DdSketch>,
}

impl BucketAcc {
	/// A fresh accumulator; `collect` materializes the full bucket for the exact
	/// percentiles / TWA, `sketch` streams values into a [`DdSketch`] instead.
	fn new(collect: bool, sketch: Option<DdSketch>) -> Self {
		Self { count: 0, sum: BigDecimal::from(0), min: None, max: None, first: None, last: None, samples: Vec::new(), collect, sketch }
	}

	/// Fold one `(timestamp, value)` into the bucket.
	///
	/// # Errors
	///
	/// [`ReduceError::SketchValue`] if a sketch reduction was requested and the value
	/// has no finite `f64` image (so it cannot be placed in a sketch bucket).
	fn push(&mut self, timestamp: DateTime<Utc>, value: BigDecimal) -> Result<(), ReduceError> {
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
		if let Some(sketch) = self.sketch.as_mut() {
			sketch.add_decimal(&value).map_err(|_| ReduceError::SketchValue)?;
		}
		if self.last.as_ref().is_none_or(|(t, _)| timestamp >= *t) {
			self.last = Some((timestamp, value));
		}
		Ok(())
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
			} else if let Some(q) = agg.sketch_quantile() {
				self.sketch.as_ref().and_then(|s| s.quantile_decimal(q))
			} else {
				match agg {
					Aggregation::Min => self.min.clone(),
					Aggregation::Max => self.max.clone(),
					Aggregation::Sum => Some(self.sum.clone()),
					Aggregation::Avg => (self.count > 0).then(|| &self.sum / &count),
					Aggregation::First => self.first.as_ref().map(|(_, v)| v.clone()),
					Aggregation::Last => self.last.as_ref().map(|(_, v)| v.clone()),
					Aggregation::Twa => time_weighted_average(&self.samples, TwaMethod::Locf, None),
					Aggregation::TwaLinear => time_weighted_average(&self.samples, TwaMethod::Linear, None),
					// The grid end of *this* bucket: the start of the next one.
					Aggregation::TwaBucketEnd => bucket_start(resolution, base.wrapping_add(1)).and_then(|end| time_weighted_average(&self.samples, TwaMethod::Locf, Some(end))),
					Aggregation::P50 | Aggregation::P90 | Aggregation::P95 | Aggregation::P99 => unreachable!("handled by percentile_rank above"),
					Aggregation::SketchP50 | Aggregation::SketchP90 | Aggregation::SketchP95 | Aggregation::SketchP99 => unreachable!("handled by sketch_quantile above"),
				}
			};
			if let Some(value) = value {
				values.insert(agg.as_str().to_string(), value);
			}
		}
		Ok(Bucket { timestamp, count: self.count, values })
	}
}

/// How a time-weighted average values the span *between* two samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TwaMethod {
	/// Last-observation-carried-forward: the interval is worth its left endpoint, so
	/// the signal is a step function that holds until the next sample.
	Locf,
	/// Linear/trapezoidal: the interval is worth the mean of its two endpoints, so the
	/// signal moves along the line between measurements.
	Linear,
}

/// The **time-weighted average** of `samples` under `method`: every interval between
/// consecutive samples (ascending by timestamp) is weighted by its duration, and valued
/// by `method` — [`TwaMethod::Locf`] takes the left endpoint `vᵢ`, [`TwaMethod::Linear`]
/// the trapezoidal mean `½(vᵢ+vᵢ₊₁)`. Both reduce to `Σ wᵢ·Δtᵢ / ΣΔtᵢ`.
///
/// `None` for an empty slice; a single sample is its own value. When every sample
/// shares one instant (total weight zero), falls back to the unweighted arithmetic
/// mean. Weights are measured in milliseconds — the unit cancels in the ratio, so it
/// only bounds sub-millisecond resolution, which a downsample bucket never needs.
fn time_weighted_average(samples: &[(DateTime<Utc>, BigDecimal)], method: TwaMethod, bucket_end: Option<DateTime<Utc>>) -> Option<BigDecimal> {
	if samples.is_empty() {
		return None;
	}
	if samples.len() == 1 && bucket_end.is_none() {
		return Some(samples[0].1.clone());
	}
	let mut ordered: Vec<&(DateTime<Utc>, BigDecimal)> = samples.iter().collect();
	ordered.sort_by_key(|(t, _)| *t);
	let mut weighted = BigDecimal::from(0);
	let mut total_ms: i64 = 0;
	// The final sample has no successor, so `Twa`/`TwaLinear` give it no weight. When a
	// `bucket_end` is supplied (`TwaBucketEnd`) its dwell to the grid boundary is known
	// and counted — LOCF only, since there is no successor value to interpolate toward.
	if let (Some(end), Some(&&(last_t, ref last_v))) = (bucket_end, ordered.last()) {
		let dt = (end - last_t).num_milliseconds().max(0);
		if dt > 0 {
			weighted += last_v * BigDecimal::from(dt);
			total_ms += dt;
		}
	}
	for pair in ordered.windows(2) {
		let dt = (pair[1].0 - pair[0].0).num_milliseconds().max(0);
		if dt > 0 {
			// The interval's representative value: its left endpoint (LOCF) or the mean
			// of its endpoints (linear/trapezoidal). The ½ stays exact in BigDecimal.
			let value = match method {
				TwaMethod::Locf => pair[0].1.clone(),
				TwaMethod::Linear => (&pair[0].1 + &pair[1].1) / BigDecimal::from(2),
			};
			weighted += value * BigDecimal::from(dt);
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
	// The exact percentiles and TWA require the full bucket materialized; the streaming
	// reductions do not, so only collect when one of those is actually requested.
	let collect = aggregations.iter().any(|a| a.needs_full_bucket());
	// A sketch reduction streams into a per-bucket DdSketch instead of collecting.
	let sketching = aggregations.iter().any(|a| a.sketch_quantile().is_some());

	// A BTreeMap keyed by the bucket index yields buckets in ascending index order,
	// which is ascending time order for a fixed resolution.
	let mut buckets: BTreeMap<i64, BucketAcc> = BTreeMap::new();
	for p in points {
		if start.is_some_and(|s| p.timestamp < s) || end.is_some_and(|e| p.timestamp > e) {
			continue;
		}
		let base = resolution.to_base(&p.timestamp).map_err(|_| ReduceError::TimestampRange)?;
		// The sketch is built inside the insert closure so a bucket that already exists
		// does not construct (and immediately drop) one per point.
		buckets.entry(base).or_insert_with(|| BucketAcc::new(collect, sketching.then(DdSketch::with_default_accuracy))).push(p.timestamp, p.value.clone())?;
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
		assert_eq!(buckets[0].values.get("p50").unwrap(), &BigDecimal::from_str("2.2").unwrap(), "p50 of 1.1/2.2/3.3 is exactly 2.2");
	}

	#[test]
	fn from_token_round_trips_as_str_and_rejects_unknown() {
		for agg in [Aggregation::Min, Aggregation::Max, Aggregation::Avg, Aggregation::Sum, Aggregation::First, Aggregation::Last, Aggregation::P50, Aggregation::P90, Aggregation::P95, Aggregation::P99, Aggregation::Twa, Aggregation::TwaLinear] {
			assert_eq!(Aggregation::from_token(agg.as_str()), Some(agg), "{} must round-trip", agg.as_str());
		}
		assert_eq!(Aggregation::from_token("MEDIAN"), Some(Aggregation::P50), "median is a case-insensitive p50 alias");
		assert_eq!(Aggregation::from_token(" avg "), Some(Aggregation::Avg), "surrounding whitespace is trimmed");
		assert_eq!(Aggregation::from_token("twa_locf"), Some(Aggregation::Twa), "twa_locf names the LOCF method explicitly");
		assert_eq!(Aggregation::from_token("time_weighted_avg_linear"), Some(Aggregation::TwaLinear), "the long-form linear alias parses");
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
	fn linear_twa_uses_the_trapezoidal_endpoint_mean() {
		// Same fixture as the LOCF test: 10@t=0, 20@t=30, 5@t=60.
		// Linear: interval [0,30] is worth ½(10+20)=15 over 30s, [30,60] is ½(20+5)=12.5
		// over 30s -> (15*30 + 12.5*30)/60 = 13.75. LOCF gives 15 on the same input, so
		// the two methods are genuinely distinct.
		let points = vec![pt(0, "10"), pt(30, "20"), pt(60, "5")];
		let buckets = reduce(&points, Resolution::Hours, None, None, &[Aggregation::TwaLinear, Aggregation::Twa]).expect("reduces");
		assert_eq!(buckets.len(), 1);
		assert!((get(&buckets[0], Aggregation::TwaLinear) - 13.75).abs() < 1e-9, "linear TWA trapezoidal mean is 13.75");
		assert!((get(&buckets[0], Aggregation::Twa) - 15.0).abs() < 1e-9, "LOCF TWA is unchanged at 15");
	}

	#[test]
	fn linear_twa_of_a_straight_ramp_is_the_midpoint_and_is_exact() {
		// On a linear ramp the trapezoidal average is exactly the midpoint value,
		// regardless of how irregularly the ramp is sampled — the property that makes
		// the linear method right for continuous signals. Ramp v = t over [0, 100],
		// sampled irregularly; the exact answer is 50.
		let points = vec![pt(0, "0"), pt(7, "7"), pt(63, "63"), pt(100, "100")];
		let buckets = reduce(&points, Resolution::Hours, None, None, &[Aggregation::TwaLinear]).expect("reduces");
		assert_eq!(buckets[0].values.get("twa_linear").unwrap(), &BigDecimal::from_str("50").unwrap(), "trapezoidal average of a straight ramp is exactly its midpoint");
	}

	#[test]
	fn linear_twa_is_order_independent() {
		let scrambled = vec![pt(60, "5"), pt(0, "10"), pt(30, "20")];
		let b = reduce(&scrambled, Resolution::Hours, None, None, &[Aggregation::TwaLinear]).expect("reduces");
		assert!((get(&b[0], Aggregation::TwaLinear) - 13.75).abs() < 1e-9, "linear TWA sorts by time before weighting");
	}

	#[test]
	fn twa_bucket_end_counts_the_last_sample_dwell_to_the_grid_boundary() {
		// The motivating bias: a change-only sensor reports 0 at 00:00 and 100 at 00:01,
		// inside a 1-hour bucket. Plain TWA weights 0 over the single minute between the
		// samples and gives 100 NO weight at all -> 0.0, though the sensor held 100 for 59
		// of the bucket's 60 minutes. Weighting the last sample to the bucket end gives
		// (0*60s + 100*3540s)/3600s = 98.333...
		let points = vec![pt(0, "0"), pt(60, "100")];
		let buckets = reduce(&points, Resolution::Hours, None, None, &[Aggregation::Twa, Aggregation::TwaBucketEnd, Aggregation::Avg]).expect("reduces");
		assert_eq!(buckets.len(), 1);
		let b = &buckets[0];
		assert!(get(b, Aggregation::Twa).abs() < 1e-9, "plain TWA gives the final sample no weight -> 0");
		assert!((get(b, Aggregation::TwaBucketEnd) - 98.333_333_333).abs() < 1e-6, "the bucket-end variant counts 100's 59-minute dwell -> ~98.33, got {}", get(b, Aggregation::TwaBucketEnd));
		assert!((get(b, Aggregation::Avg) - 50.0).abs() < 1e-9, "the unweighted mean ignores dwell entirely -> 50");
	}

	#[test]
	fn twa_bucket_end_of_a_single_sample_is_the_value_and_is_order_independent() {
		// One sample: it holds for the whole bucket, so the answer is its own value —
		// the same as plain TWA, reached by a different route (a real dwell, not the
		// single-sample shortcut).
		let one = reduce(&[pt(5, "42")], Resolution::Minutes, None, None, &[Aggregation::TwaBucketEnd]).expect("reduces");
		assert!((get(&one[0], Aggregation::TwaBucketEnd) - 42.0).abs() < 1e-9, "a lone sample holds the whole bucket");
		// A sample exactly on the bucket end boundary of its own bucket cannot happen
		// (it would land in the next bucket), so the last dwell is always > 0 here.
		let scrambled = reduce(&[pt(60, "100"), pt(0, "0")], Resolution::Hours, None, None, &[Aggregation::TwaBucketEnd]).expect("reduces");
		assert!((get(&scrambled[0], Aggregation::TwaBucketEnd) - 98.333_333_333).abs() < 1e-6, "bucket-end TWA sorts by time before weighting");
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
	fn sketch_percentiles_track_the_exact_percentiles_within_the_bound() {
		// The claim that makes the sketch reductions usable: on a real bucket their
		// answer is within SKETCH_ALPHA of the exact nearest-rank percentile.
		let points: Vec<Point> = (1..=1000).map(|v| pt(i64::from(v - 1), &v.to_string())).collect();
		let buckets = reduce(&points, Resolution::Hours, None, None, &[Aggregation::P50, Aggregation::SketchP50, Aggregation::P90, Aggregation::SketchP90, Aggregation::P99, Aggregation::SketchP99]).expect("reduces");
		assert_eq!(buckets.len(), 1, "all 1000 points fall in one hour-bucket");
		let b = &buckets[0];
		for (exact, approx) in [(Aggregation::P50, Aggregation::SketchP50), (Aggregation::P90, Aggregation::SketchP90), (Aggregation::P99, Aggregation::SketchP99)] {
			let (e, a) = (get(b, exact), get(b, approx));
			let rel = (a - e).abs() / e.abs();
			// One bucket of slack: the exact and sketch reductions use different rank
			// conventions, so on adjacent-integer data they may pick neighbouring samples.
			assert!(rel <= SKETCH_ALPHA * 2.0, "{} ({a}) must track {} ({e}) within the sketch bound, relative error {rel}", approx.as_str(), exact.as_str());
		}
	}

	/// The substitutability guarantee: because the sketch shares the exact percentiles'
	/// nearest-rank convention, the two name the same sample at **every** bucket size —
	/// including the tiny buckets where the reference `DDSketch` rank would diverge.
	#[test]
	fn sketch_and_exact_percentiles_agree_on_small_buckets() {
		for n in 1..=12_u32 {
			let points: Vec<Point> = (1..=n).map(|v| pt(i64::from(v - 1), &v.to_string())).collect();
			let buckets = reduce(&points, Resolution::Hours, None, None, &[Aggregation::P50, Aggregation::SketchP50, Aggregation::P90, Aggregation::SketchP90, Aggregation::P99, Aggregation::SketchP99]).expect("reduces");
			let b = &buckets[0];
			for (exact, approx) in [(Aggregation::P50, Aggregation::SketchP50), (Aggregation::P90, Aggregation::SketchP90), (Aggregation::P99, Aggregation::SketchP99)] {
				let (e, a) = (get(b, exact), get(b, approx));
				let rel = (a - e).abs() / e.abs();
				assert!(rel <= SKETCH_ALPHA, "n={n}: {} ({a}) must name the same sample as {} ({e}) — relative error {rel}", approx.as_str(), exact.as_str());
			}
		}
	}

	#[test]
	fn sketch_reductions_do_not_materialize_the_bucket() {
		// The bounded-memory property: asking only for a sketch percentile must not set
		// the collect flag (which is what materializes every sample in the bucket).
		assert!(!Aggregation::SketchP99.needs_full_bucket(), "a sketch reduction streams — it must not request the full bucket");
		assert!(Aggregation::P99.needs_full_bucket(), "the exact percentile still needs the bucket");
		assert!(Aggregation::Twa.needs_full_bucket(), "TWA still needs the time-ordered samples");
		// And it still produces an answer without collection.
		let points: Vec<Point> = (1..=100).map(|v| pt(i64::from(v - 1), &v.to_string())).collect();
		let buckets = reduce(&points, Resolution::Hours, None, None, &[Aggregation::SketchP50]).expect("reduces");
		assert!(buckets[0].values.contains_key("sketch_p50"), "the streamed sketch still yields a value");
	}

	#[test]
	fn sketch_tokens_and_aliases_parse() {
		for agg in [Aggregation::SketchP50, Aggregation::SketchP90, Aggregation::SketchP95, Aggregation::SketchP99] {
			assert_eq!(Aggregation::from_token(agg.as_str()), Some(agg), "{} round-trips", agg.as_str());
		}
		assert_eq!(Aggregation::from_token("sketch_median"), Some(Aggregation::SketchP50), "sketch_median aliases sketch_p50");
		assert_eq!(Aggregation::from_token("SKETCH_P99"), Some(Aggregation::SketchP99), "sketch tokens are case-insensitive");
	}

	#[test]
	fn sketch_handles_negative_and_zero_buckets() {
		// A bucket straddling zero must still reduce (the sketch mirrors negatives and
		// counts zero exactly), rather than erroring or dropping samples.
		let points = vec![pt(0, "-50"), pt(1, "0"), pt(2, "50")];
		let buckets = reduce(&points, Resolution::Hours, None, None, &[Aggregation::SketchP50, Aggregation::SketchP99]).expect("reduces a zero-straddling bucket");
		assert_eq!(buckets[0].count, 3);
		assert!(get(&buckets[0], Aggregation::SketchP50).abs() < 1e-9, "median of -50/0/50 is exactly 0 (zero is counted, not approximated)");
		// The sketch uses DSP's nearest-rank convention, so even on a 3-sample bucket it
		// names the same sample the exact p99 does — the top one. (Under DDSketch's own
		// floor(q*(n-1)) rank it would have returned the middle sample; DSP deliberately
		// does not inherit that divergence.)
		assert!((get(&buckets[0], Aggregation::SketchP99) - 50.0).abs() / 50.0 <= SKETCH_ALPHA, "sketch p99 of -50/0/50 is the top sample, matching the exact p99");
	}

	#[test]
	fn sketch_negative_only_bucket_reports_negative_quantiles() {
		// The mirrored negative store: an all-negative bucket must report negative
		// quantiles within the bound, not fall back to zero or error.
		let points: Vec<Point> = (1..=100).map(|v| pt(i64::from(v - 1), &format!("-{v}"))).collect();
		let buckets = reduce(&points, Resolution::Hours, None, None, &[Aggregation::SketchP50]).expect("reduces");
		let got = get(&buckets[0], Aggregation::SketchP50);
		// Sorted ascending: -100 … -1; rank floor(0.5*99) = 49 -> -51.
		assert!((got - -51.0).abs() / 51.0 <= SKETCH_ALPHA, "median of -100..-1 is about -51, got {got}");
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
