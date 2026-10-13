//! **Gap filling** over a reduction: a dense bucket grid in which the buckets that held no
//! samples are synthesized by a declared [`Fill`] method.
//!
//! [`reduce`](crate::reduce) emits only the buckets that contain samples. Many queries want
//! every grid step instead — a chart axis, a join against another series, or the TSM-Bench
//! interpolation query `SAMPLE BY 5s FILL(LINEAR)` (Q5). [`fill`] takes the reduced buckets
//! and returns the dense grid between the bounds, keeping every reduced bucket unchanged and
//! adding a bucket with `count == 0` at every missing step. A count of zero is how a filled
//! bucket is told apart from a measured one: synthesized values are always marked, never
//! passed off as observed.
//!
//! The methods follow the SQL `FILL` vocabulary the time-series engines share (QuestDB's
//! `SAMPLE BY … FILL(NULL | PREV | LINEAR | <constant>)`):
//!
//! - [`Fill::Null`]: the bucket is emitted with no values.
//! - [`Fill::Previous`]: each reduction carries the nearest earlier bucket's value forward.
//! - [`Fill::Linear`]: each reduction is interpolated linearly, by grid step, between the
//!   nearest earlier and later buckets that have it. Steps before the first or after the last
//!   such bucket have no neighbour on one side and are left without that value.
//! - [`Fill::Value`]: every reduction takes the given constant.
//! - [`Fill::Spline`]: each reduction is interpolated by a splimes spline through the
//!   buckets that have it (the bucket values at their grid starts), which is WeftDB's
//!   interpolation engine applied to the reduced series. Like the linear fill it gives no
//!   value outside the first and last such bucket.
//!
//! Values stay in [`BigDecimal`]. A linear fill computes `prev + (next − prev)·k/n` exactly
//! when the quotient terminates and to `BigDecimal`'s default division precision otherwise.
//! A spline fill carries splimes' numerical contract instead: inputs are rounded to `f64`
//! and each filled value is the shortest decimal that round-trips to the computed `f64`.
//! Either way a filled value is synthesized and marked so by its `count == 0`.

use std::collections::BTreeMap;

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use splimes::{Point, PointKind, Resolution, Spline};

use crate::{bucket_index, bucket_start, Bucket};

/// How [`fill`] synthesizes the values of a bucket that held no samples.
#[derive(Debug, Clone, PartialEq)]
pub enum Fill {
	/// Emit the bucket with no values (SQL `FILL(NULL)`).
	Null,
	/// Carry each reduction's previous value forward (SQL `FILL(PREV)`).
	Previous,
	/// Interpolate each reduction linearly between its neighbours (SQL `FILL(LINEAR)`).
	Linear,
	/// Give every reduction this constant (SQL `FILL(<value>)`).
	Value(BigDecimal),
	/// Interpolate each reduction with this splimes spline through its buckets.
	Spline(Spline),
}

impl Fill {
	/// Parse a wire token: `null`, `prev`/`previous`/`locf`, `linear`, `quadratic`, `cubic`,
	/// or a decimal constant (case-insensitive, surrounding whitespace ignored). `None` for
	/// anything else. `quadratic` and `cubic` are [`Fill::Spline`]s; `linear` is the exact
	/// [`Fill::Linear`], not a linear spline.
	#[must_use]
	pub fn from_token(token: &str) -> Option<Self> {
		let token = token.trim();
		match token.to_ascii_lowercase().as_str() {
			"null" => Some(Self::Null),
			"prev" | "previous" | "locf" => Some(Self::Previous),
			"linear" => Some(Self::Linear),
			"quadratic" => Some(Self::Spline(Spline::Quadratic)),
			"cubic" => Some(Self::Spline(Spline::Cubic)),
			_ => token.parse::<BigDecimal>().ok().map(Self::Value),
		}
	}
}

/// Why a fill could not be produced.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FillError {
	/// The buckets were not in strictly ascending grid order, or two fell on one step.
	#[error("buckets must be in strictly ascending grid order")]
	Unordered,
	/// The dense grid would hold more buckets than the caller allows.
	#[error("the filled grid would hold {grid} buckets, more than the limit of {max}")]
	TooManyBuckets {
		/// Buckets the grid between the bounds would hold.
		grid: u128,
		/// The caller's limit.
		max: usize,
	},
	/// A bound or bucket could not be indexed at the resolution.
	#[error("a timestamp cannot be indexed at the requested resolution")]
	TimestampRange,
	/// A grid step's start lies outside the representable time range.
	#[error("a bucket start overflows the representable time range")]
	BucketStartOverflow,
	/// The spline interpolation failed (an invalid spline, or a value with no finite `f64`).
	#[error("spline fill failed: {0}")]
	Spline(String),
}

/// The dense bucket grid at `resolution` from `start`'s bucket to `end`'s, with the steps
/// that `buckets` lacks synthesized by `method`.
///
/// `buckets` is a reduction's output at the same `resolution`: ascending, one bucket per
/// grid step (as [`reduce`](crate::reduce) returns them). Each is kept unchanged; every
/// missing step gets a bucket with `count == 0` whose values `method` decides (see
/// [`Fill`]). Without `start` the grid begins at the first bucket, without `end` it ends at
/// the last, and an empty input without both bounds yields an empty grid. A `start` after
/// `end` also yields an empty grid. Buckets outside the bounds are dropped, but still serve
/// as neighbours for [`Fill::Previous`] and [`Fill::Linear`]. Constant and linear fills give
/// a value to each reduction any input bucket carries.
///
/// # Errors
///
/// [`FillError::Unordered`] if `buckets` is not strictly ascending by grid step;
/// [`FillError::TooManyBuckets`] if the grid would exceed `max_buckets`;
/// [`FillError::TimestampRange`] / [`FillError::BucketStartOverflow`] if a bound or step is
/// outside the representable range at `resolution`.
pub fn fill(buckets: &[Bucket], resolution: Resolution, start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>, method: &Fill, max_buckets: usize) -> Result<Vec<Bucket>, FillError> {
	let index = |t: &DateTime<Utc>| bucket_index(resolution, t).map_err(|_| FillError::TimestampRange);
	let steps = buckets.iter().map(|b| index(&b.timestamp)).collect::<Result<Vec<i64>, _>>()?;
	if steps.windows(2).any(|w| w[0] >= w[1]) {
		return Err(FillError::Unordered);
	}
	let (Some(first), Some(last)) = (start.as_ref().map(index).transpose()?.or_else(|| steps.first().copied()), end.as_ref().map(index).transpose()?.or_else(|| steps.last().copied())) else {
		return Ok(Vec::new());
	};
	if first > last {
		return Ok(Vec::new());
	}
	let grid = u128::try_from(i128::from(last) - i128::from(first) + 1).unwrap_or(u128::MAX);
	if grid > max_buckets as u128 {
		return Err(FillError::TooManyBuckets { grid, max: max_buckets });
	}

	// Every reduction any bucket carries, for the constant fill.
	let keys: Vec<&String> = if matches!(method, Fill::Value(_)) {
		let mut keys: Vec<&String> = buckets.iter().flat_map(|b| b.values.keys()).collect();
		keys.sort_unstable();
		keys.dedup();
		keys
	} else {
		Vec::new()
	};
	// `next` is the position of the first input bucket at or after the current step, and
	// `latest` holds each reduction's most recent value (with its step) before it.
	let mut next = steps.partition_point(|&s| s < first);
	let mut latest: BTreeMap<&String, (i64, &BigDecimal)> = BTreeMap::new();
	for (bucket, &s) in buckets[..next].iter().zip(&steps) {
		latest.extend(bucket.values.iter().map(|(k, v)| (k, (s, v))));
	}
	let splined = match method {
		Fill::Spline(spline) => spline_values(buckets, resolution, first, last, *spline)?,
		_ => BTreeMap::new(),
	};
	let mut out = Vec::with_capacity(usize::try_from(grid).unwrap_or(0));
	for step in first..=last {
		if steps.get(next) == Some(&step) {
			latest.extend(buckets[next].values.iter().map(|(k, v)| (k, (step, v))));
			out.push(buckets[next].clone());
			next += 1;
			continue;
		}
		let timestamp = bucket_start(resolution, step).ok_or(FillError::BucketStartOverflow)?;
		let values = match method {
			Fill::Null => BTreeMap::new(),
			Fill::Value(constant) => keys.iter().map(|&k| (k.clone(), constant.clone())).collect(),
			Fill::Previous => latest.iter().map(|(&k, &(_, v))| (k.clone(), v.clone())).collect(),
			Fill::Linear => linear_values(&latest, &buckets[next..], &steps[next..], step),
			Fill::Spline(_) => splined.iter().filter_map(|(key, by_step)| Some((key.clone(), by_step.get(&step)?.clone()))).collect(),
		};
		out.push(Bucket { timestamp, count: 0, values });
	}
	Ok(out)
}

/// Each reduction's linear interpolation at grid `step`, between its most recent value
/// before the step (`latest`) and its first value in `later` (the input buckets after the
/// step, ascending, with their grid `steps`).
fn linear_values(latest: &BTreeMap<&String, (i64, &BigDecimal)>, later: &[Bucket], steps: &[i64], step: i64) -> BTreeMap<String, BigDecimal> {
	latest.iter()
		.filter_map(|(&key, &(s0, v0))| {
			// Usually the very next bucket carries the reduction and the search ends at once.
			let (s1, v1) = later.iter().zip(steps).find_map(|(b, &s)| b.values.get(key).map(|v| (s, v)))?;
			let value = v0 + (v1 - v0) * BigDecimal::from(step - s0) / BigDecimal::from(s1 - s0);
			Some((key.clone(), value))
		})
		.collect()
}

/// For each reduction the buckets carry, its spline interpolation at every grid step of
/// `first..=last` that lies strictly inside that reduction's buckets and is not one of
/// them, keyed by step. splimes' grid at `resolution` anchored at bucket `first`'s start
/// is exactly the bucket grid (both step by `Resolution::step_nanos`), so grid point `k`
/// is step `first + k`.
fn spline_values(buckets: &[Bucket], resolution: Resolution, first: i64, last: i64, spline: Spline) -> Result<BTreeMap<String, BTreeMap<i64, BigDecimal>>, FillError> {
	let (start, end) = (bucket_start(resolution, first).ok_or(FillError::BucketStartOverflow)?, bucket_start(resolution, last).ok_or(FillError::BucketStartOverflow)?);
	let mut keys: Vec<&String> = buckets.iter().flat_map(|b| b.values.keys()).collect();
	keys.sort_unstable();
	keys.dedup();
	let mut out = BTreeMap::new();
	for key in keys {
		let knots: Vec<Point> = buckets.iter().filter_map(|b| b.values.get(key).map(|v| Point::new(b.timestamp, v.clone()))).collect();
		let series = splimes::interpolate(&knots, start, end, resolution, spline).map_err(|e| FillError::Spline(e.to_string()))?;
		let by_step: BTreeMap<i64, BigDecimal> = series.kinds().iter().zip(series.values()).zip(first..).filter(|((kind, _), _)| **kind == PointKind::Interpolated).map(|((_, value), step)| (step, value.clone())).collect();
		out.insert(key.clone(), by_step);
	}
	Ok(out)
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use splimes::Point;

	use super::*;
	use crate::{reduce, Aggregation};

	fn dec(s: &str) -> BigDecimal {
		BigDecimal::from_str(s).expect("decimal")
	}

	fn at(secs: i64) -> DateTime<Utc> {
		DateTime::from_timestamp(secs, 0).expect("in range")
	}

	/// Samples at the given seconds with the given values.
	fn points(samples: &[(i64, &str)]) -> Vec<Point> {
		samples.iter().map(|&(t, v)| Point { timestamp: at(t), value: dec(v) }).collect()
	}

	/// Minute buckets at minutes 0, 1 and 4 (avg 10, 20, 50), so minutes 2 and 3 are gaps.
	fn gappy() -> Vec<Bucket> {
		reduce(&points(&[(0, "10"), (30, "10"), (60, "20"), (240, "50")]), Resolution::Minutes, None, None, &[Aggregation::Avg, Aggregation::Max]).expect("reduces")
	}

	fn avg(bucket: &Bucket) -> Option<&BigDecimal> {
		bucket.values.get("avg")
	}

	#[test]
	fn the_grid_is_dense_measured_buckets_are_kept_and_filled_ones_have_count_zero() {
		let buckets = gappy();
		for method in [Fill::Null, Fill::Previous, Fill::Linear, Fill::Value(dec("0"))] {
			let filled = fill(&buckets, Resolution::Minutes, None, None, &method, 100).expect("fills");
			assert_eq!(filled.iter().map(|b| b.timestamp).collect::<Vec<_>>(), (0..5).map(|m| at(m * 60)).collect::<Vec<_>>(), "{method:?}");
			assert_eq!(filled.iter().map(|b| b.count).collect::<Vec<_>>(), vec![2, 1, 0, 0, 1]);
			for kept in [0, 1, 4] {
				assert!(buckets.contains(&filled[kept]), "a measured bucket is unchanged");
			}
		}
	}

	#[test]
	fn each_method_fills_as_declared() {
		let buckets = gappy();
		let run = |method: Fill| fill(&buckets, Resolution::Minutes, None, None, &method, 100).expect("fills");
		let null = run(Fill::Null);
		assert!(null[2].values.is_empty() && null[3].values.is_empty());
		let prev = run(Fill::Previous);
		assert_eq!((avg(&prev[2]), avg(&prev[3])), (Some(&dec("20")), Some(&dec("20"))));
		// 20 → 50 over three steps: 30 and 40.
		let linear = run(Fill::Linear);
		assert_eq!((avg(&linear[2]), avg(&linear[3])), (Some(&dec("30")), Some(&dec("40"))));
		assert_eq!(linear[2].values.get("max"), Some(&dec("30")), "every reduction is interpolated");
		let constant = run(Fill::Value(dec("-1.5")));
		assert_eq!(constant[3].values.keys().collect::<Vec<_>>(), vec!["avg", "max"]);
		assert_eq!(avg(&constant[3]), Some(&dec("-1.5")));
	}

	#[test]
	fn a_non_terminating_linear_step_is_a_bigdecimal_quotient() {
		// 0 → 1 over three steps: a third and two thirds, to BigDecimal's division precision.
		let buckets = reduce(&points(&[(0, "0"), (180, "1")]), Resolution::Minutes, None, None, &[Aggregation::Avg]).expect("reduces");
		let filled = fill(&buckets, Resolution::Minutes, None, None, &Fill::Linear, 10).expect("fills");
		let third = avg(&filled[1]).expect("filled");
		assert_eq!(third, &(BigDecimal::from(1) / BigDecimal::from(3)));
		assert_eq!(avg(&filled[2]).expect("filled"), &(BigDecimal::from(2) / BigDecimal::from(3)));
	}

	#[test]
	fn bounds_extend_or_trim_the_grid_and_edges_have_no_linear_neighbour() {
		let buckets = gappy();
		// Minute -2 to minute 6: two leading and two trailing steps beyond the data.
		let filled = fill(&buckets, Resolution::Minutes, Some(at(-120)), Some(at(410)), &Fill::Linear, 100).expect("fills");
		assert_eq!(filled.len(), 9);
		assert_eq!(filled.first().map(|b| b.timestamp), Some(at(-120)));
		assert!(filled[0].values.is_empty() && filled[8].values.is_empty(), "no neighbour on one side, no linear value");
		let prev = fill(&buckets, Resolution::Minutes, Some(at(-120)), Some(at(410)), &Fill::Previous, 100).expect("fills");
		assert!(prev[0].values.is_empty(), "nothing earlier to carry");
		assert_eq!(avg(&prev[8]), Some(&dec("50")), "the last value carries to the end bound");
		// A window inside the data still interpolates from buckets outside it.
		let inner = fill(&buckets, Resolution::Minutes, Some(at(150)), Some(at(170)), &Fill::Linear, 100).expect("fills");
		assert_eq!(inner.len(), 1);
		assert_eq!((inner[0].timestamp, inner[0].count, avg(&inner[0])), (at(120), 0, Some(&dec("30"))));
		assert_eq!(fill(&buckets, Resolution::Minutes, Some(at(300)), Some(at(0)), &Fill::Null, 100), Ok(Vec::new()), "start after end");
	}

	#[test]
	fn empty_input_needs_both_bounds() {
		assert_eq!(fill(&[], Resolution::Minutes, None, Some(at(600)), &Fill::Null, 100), Ok(Vec::new()));
		let filled = fill(&[], Resolution::Minutes, Some(at(0)), Some(at(179)), &Fill::Value(dec("7")), 100).expect("fills");
		assert_eq!(filled.iter().map(|b| (b.count, b.values.len())).collect::<Vec<_>>(), vec![(0, 0); 3], "no reductions to give the constant to");
	}

	#[test]
	fn reductions_missing_from_a_bucket_take_the_nearest_bucket_that_has_them() {
		// `p50` exists only in the first bucket: linear has no later neighbour for it, and the
		// forward fill reaches back past the bucket that lacks it.
		let mut buckets = gappy();
		buckets[0].values.insert("p50".into(), dec("9"));
		let prev = fill(&buckets, Resolution::Minutes, None, None, &Fill::Previous, 100).expect("fills");
		assert_eq!(prev[2].values.get("p50"), Some(&dec("9")));
		let linear = fill(&buckets, Resolution::Minutes, None, None, &Fill::Linear, 100).expect("fills");
		assert_eq!(linear[2].values.get("p50"), None);
		assert_eq!(avg(&linear[2]), Some(&dec("30")));
	}

	#[test]
	fn unordered_input_and_oversized_grids_are_refused() {
		let mut buckets = gappy();
		buckets.swap(0, 1);
		assert_eq!(fill(&buckets, Resolution::Minutes, None, None, &Fill::Null, 100), Err(FillError::Unordered));
		let buckets = gappy();
		assert_eq!(fill(&buckets, Resolution::Minutes, None, None, &Fill::Null, 4), Err(FillError::TooManyBuckets { grid: 5, max: 4 }));
		assert_eq!(fill(&buckets, Resolution::Nanoseconds, None, Some(DateTime::<Utc>::MAX_UTC), &Fill::Null, 100), Err(FillError::TimestampRange), "a bound outside the nanosecond epoch does not index");
	}

	#[test]
	fn a_spline_fill_follows_the_curve_and_leaves_the_edges() {
		// Hourly buckets of a parabola v = h² with hours 3..=5 missing, plus one leading
		// and one trailing step beyond the data.
		let samples: Vec<(i64, String)> = (0..10_i64).filter(|h| !(3..=5).contains(h)).map(|h| (h * 3_600, (h * h).to_string())).collect();
		let samples: Vec<(i64, &str)> = samples.iter().map(|(t, v)| (*t, v.as_str())).collect();
		let buckets = reduce(&points(&samples), Resolution::Hours, None, None, &[Aggregation::Avg]).expect("reduces");
		let filled = fill(&buckets, Resolution::Hours, Some(at(-3_600)), Some(at(10 * 3_600)), &Fill::Spline(Spline::Quadratic), 100).expect("fills");
		assert_eq!(filled.len(), 12);
		for h in 3..=5_i64 {
			let bucket = &filled[usize::try_from(h + 1).unwrap()];
			assert_eq!(bucket.count, 0);
			let v = avg(bucket).expect("interpolated").to_string().parse::<f64>().unwrap();
			// A quadratic through a parabola's knots reproduces it, to f64 precision.
			assert!((v - f64::from(i32::try_from(h * h).unwrap())).abs() < 1e-9, "hour {h}: {v}");
		}
		assert!(filled[0].values.is_empty() && filled[11].values.is_empty(), "no spline value outside the data");
		for kept in [1, 2, 3, 7, 8, 9, 10] {
			assert!(buckets.contains(&filled[kept]), "measured buckets are unchanged");
		}
		// Linear, by contrast, cuts the chord: 4 → 36 at hour 3 gives 12, not 9.
		let linear = fill(&buckets, Resolution::Hours, None, None, &Fill::Linear, 100).expect("fills");
		assert_eq!(avg(&linear[3]), Some(&dec("12")));
	}

	#[test]
	fn an_invalid_spline_is_an_error() {
		let buckets = gappy();
		let err = fill(&buckets, Resolution::Minutes, None, None, &Fill::Spline(Spline::Polynomial(0, None)), 100).unwrap_err();
		assert!(matches!(err, FillError::Spline(_)), "{err:?}");
	}

	#[test]
	fn tokens_parse() {
		assert_eq!(Fill::from_token(" NULL "), Some(Fill::Null));
		assert_eq!(Fill::from_token("prev"), Some(Fill::Previous));
		assert_eq!(Fill::from_token("locf"), Some(Fill::Previous));
		assert_eq!(Fill::from_token("Linear"), Some(Fill::Linear));
		assert_eq!(Fill::from_token("-2.50"), Some(Fill::Value(dec("-2.50"))));
		assert_eq!(Fill::from_token("CUBIC"), Some(Fill::Spline(Spline::Cubic)));
		assert_eq!(Fill::from_token("quadratic"), Some(Fill::Spline(Spline::Quadratic)));
		assert_eq!(Fill::from_token("nearest"), None);
	}
}
