//! An **integer-native** reduction over `ScaledI64` columns: the same buckets and the same
//! values as [`reduce`](crate::reduce), computed on the scaled mantissas a `.weftseg` value
//! column already stores, without building a `BigDecimal` per sample.
//!
//! `weft-reduce/benches/decimal_tax.rs` measured the shipped `BigDecimal` reduction at ~43× an
//! `f64` loop on real BTC closes, while an exact integer loop over the stored mantissas was
//! *faster* than `f64`. The exactness is not what costs; the per-sample arbitrary-precision
//! arithmetic is. This module keeps `count`/`sum`/`min`/`max`/`first`/`last` in `i128`/`i64`
//! accumulators and materializes a [`BigDecimal`] only once per output bucket, so the
//! `BigDecimal` logical/API type and its exactness are unchanged. Only the hot loop moves to
//! integers.
//!
//! It covers the streaming reductions (`min`/`max`/`avg`/`sum`/`first`/`last`). Exact
//! percentiles, time-weighted averages and sketches need the per-sample values, so a request
//! naming any of them returns `Ok(None)` and the caller falls back to [`reduce`](crate::reduce).

use std::{collections::BTreeMap, sync::OnceLock};

use bigdecimal::{
	num_bigint::{BigInt, BigUint, Sign}, BigDecimal, ToPrimitive
};
use chrono::{DateTime, Utc};
use splimes::Resolution;

use crate::{bucket_start, resolution_step_secs, Aggregation, Bucket, BucketAcc, DdSketch, PartialReduction, ReduceError};

const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// The bucket index of an instant given in epoch nanoseconds, identical to
/// [`bucket_index`](crate::bucket_index) on the same instant: the sub-second grids floor the
/// nanoseconds to their step, and the second-and-coarser grids divide the floored seconds by
/// the step in seconds with truncating `/`, exactly as `bucket_index` does.
const fn base_of(resolution: Resolution, nanos: i64) -> i64 {
	match resolution_step_secs(resolution) {
		Some(step_secs) => nanos.div_euclid(NANOS_PER_SECOND) / step_secs,
		None => nanos.div_euclid(resolution.step_nanos()),
	}
}

/// The inclusive range of epoch nanoseconds [`base_of`] maps to bucket `base`, or `None` if
/// it is not representable in `i64` nanoseconds (the caller then indexes every sample).
///
/// The sub-second grids floor, so bucket `b` is `[b·step, b·step + step − 1]`. The
/// second-and-coarser grids divide the floored seconds with truncating `/`, so their bucket 0
/// spans `−(step − 1) ..= step − 1` seconds and a negative bucket `b` spans
/// `b·step − (step − 1) ..= b·step` seconds; each second then covers a whole second of
/// nanoseconds.
fn base_range(resolution: Resolution, base: i64) -> Option<(i64, i64)> {
	let Some(step) = resolution_step_secs(resolution) else {
		let step = resolution.step_nanos();
		let lo = base.checked_mul(step)?;
		return Some((lo, lo.checked_add(step - 1)?));
	};
	let anchor = base.checked_mul(step)?;
	let (lo_secs, hi_secs) = match base.signum() {
		1 => (anchor, anchor.checked_add(step - 1)?),
		0 => (-(step - 1), step - 1),
		_ => (anchor.checked_sub(step - 1)?, anchor),
	};
	Some((lo_secs.checked_mul(NANOS_PER_SECOND)?, hi_secs.checked_mul(NANOS_PER_SECOND)?.checked_add(NANOS_PER_SECOND - 1)?))
}

/// Running integer state for one bucket. `first`/`last` carry their instant and resolve ties
/// exactly as [`reduce`](crate::reduce) does (`first`: strictly earlier wins; `last`: equal
/// or later wins), so input order never changes the answer.
#[derive(Debug, Clone, Copy)]
struct ScaledAcc {
	count: u64,
	sum: i128,
	min: i64,
	max: i64,
	first: (i64, i64),
	last: (i64, i64),
}

impl ScaledAcc {
	const fn new(nanos: i64, mantissa: i64) -> Self {
		Self { count: 1, sum: mantissa as i128, min: mantissa, max: mantissa, first: (nanos, mantissa), last: (nanos, mantissa) }
	}

	const fn push(&mut self, nanos: i64, mantissa: i64) {
		self.count += 1;
		self.sum += mantissa as i128;
		if mantissa < self.min {
			self.min = mantissa;
		}
		if mantissa > self.max {
			self.max = mantissa;
		}
		if nanos < self.first.0 {
			self.first = (nanos, mantissa);
		}
		if nanos >= self.last.0 {
			self.last = (nanos, mantissa);
		}
	}

	const fn merge(&mut self, other: &Self) {
		self.count += other.count;
		self.sum += other.sum;
		if other.min < self.min {
			self.min = other.min;
		}
		if other.max > self.max {
			self.max = other.max;
		}
		if other.first.0 < self.first.0 {
			self.first = other.first;
		}
		if other.last.0 >= self.last.0 {
			self.last = other.last;
		}
	}
}

// Portions adapted from bigdecimal-rs (https://github.com/akubera/bigdecimal-rs), licensed under
// MIT OR Apache-2.0 and used here under Apache-2.0; see THIRD-PARTY-NOTICES. Modifications: its
// `Div` special cases and `impl_division` long division re-implemented over `u128`/`u64` integers
// instead of a `BigInt` division per digit, for an `i128` numerator and a `u64` divisor. The
// adapted code is `avg_like_bigdecimal`.

/// The significant digits `bigdecimal`'s `/` keeps (its build-time `DEFAULT_PRECISION`, `100`
/// unless `RUST_BIGDECIMAL_DEFAULT_PRECISION` overrides it at build time). The equality tests
/// below compare against `bigdecimal`'s own division, so a build with a different precision
/// fails them rather than silently diverging.
const BIGDECIMAL_DIV_PRECISION: u32 = 100;

/// `BigDecimal::new(sum, scale) / BigDecimal::from(count)`, computed the way `bigdecimal` 0.4
/// computes it and with an identical result, but with `u128` long division instead of a
/// `BigInt` `div_rem` per digit.
///
/// This is the per-bucket `avg`. It transcribes `bigdecimal`'s `impl_division`, including the
/// special cases its `Div` takes first: a zero numerator or a divisor of one returns the
/// numerator unchanged, and equal integers return `1` at the operand scale. Digits are
/// generated until the remainder is zero or the quotient holds [`BIGDECIMAL_DIV_PRECISION`]
/// digits, then a non-zero remainder rounds the last digit up when the next digit is `>= 5`
/// (`get_rounding_term` of a single digit). A divisor in `u64` keeps every remainder in `u128`.
fn avg_like_bigdecimal(sum: i128, scale: i64, count: u64) -> BigDecimal {
	if sum == 0 || count == 1 {
		return BigDecimal::new(BigInt::from(sum), scale);
	}
	if sum == i128::from(count) {
		return BigDecimal::new(BigInt::from(1), scale);
	}
	let negative = sum < 0;
	let den = u128::from(count);
	let mut num = sum.unsigned_abs();
	let mut scale = scale;
	while num < den {
		scale += 1;
		num *= 10;
	}
	let (lead, mut rem) = (num / den, num % den);
	if rem != 0 && !terminates(num, count) {
		return non_terminating_quotient(num, lead, scale, count, negative);
	}
	let mut quotient = BigInt::from(lead);
	if rem != 0 {
		let mut precision = lead.checked_ilog10().map_or(1, |d| d + 1);
		// Digits after the leading quotient, gathered in `u128` chunks of up to 38 digits so
		// the `BigInt` is touched once per chunk rather than once per digit.
		let mut digits = DigitSink { quotient, chunk: 0, chunk_digits: 0 };
		rem *= 10;
		// Every remainder stays below `10 × den`, so for any divisor up to `u64::MAX / 10` the
		// digit loop runs in `u64`, several times cheaper than `u128` division. A larger
		// divisor (a bucket of more than ~1.8e18 samples) keeps the `u128` loop.
		match u64::try_from(den) {
			Ok(d) if d <= u64::MAX / 10 => {
				let mut r = u64::try_from(rem).unwrap_or(u64::MAX);
				while r != 0 && precision < BIGDECIMAL_DIV_PRECISION {
					digits.push(u128::from(r / d));
					r = (r % d) * 10;
					precision += 1;
					scale += 1;
				}
				rem = u128::from(r);
			}
			_ => {
				while rem != 0 && precision < BIGDECIMAL_DIV_PRECISION {
					digits.push(rem / den);
					rem = (rem % den) * 10;
					precision += 1;
					scale += 1;
				}
			}
		}
		let DigitSink { quotient: q, chunk, chunk_digits } = digits;
		quotient = q;
		if chunk_digits > 0 {
			quotient = quotient * BigInt::from(10_u128.pow(chunk_digits)) + BigInt::from(chunk);
		}
		if rem != 0 && rem / den >= 5 {
			quotient += 1;
		}
	}
	let magnitude = BigDecimal::new(quotient, scale);
	if negative {
		-magnitude
	} else {
		magnitude
	}
}

/// Whether `num / count` has a finite decimal expansion: the divisor, reduced by the common
/// factor, has no prime factor but 2 and 5.
fn terminates(num: u128, count: u64) -> bool {
	let (mut a, mut b) = (count, u64::try_from(num % u128::from(count)).unwrap_or(0));
	while b != 0 {
		(a, b) = (b, a % b);
	}
	let mut d = count / a;
	d >>= d.trailing_zeros();
	while d.is_multiple_of(5) {
		d /= 5;
	}
	d == 1
}

/// `10^k` for `k` in `0..=BIGDECIMAL_DIV_PRECISION`, built once.
fn power_of_ten(k: u32) -> &'static BigUint {
	static POWERS: OnceLock<Vec<BigUint>> = OnceLock::new();
	let powers = POWERS.get_or_init(|| {
		let mut powers = vec![BigUint::from(1_u8)];
		for _ in 0..BIGDECIMAL_DIV_PRECISION {
			let next = powers.last().map_or_else(|| BigUint::from(1_u8), |p| p * 10_u8);
			powers.push(next);
		}
		powers
	});
	&powers[usize::try_from(k).unwrap_or(0).min(powers.len() - 1)]
}

/// The tail of [`avg_like_bigdecimal`] for a quotient that never terminates, where its digit
/// loop always runs to [`BIGDECIMAL_DIV_PRECISION`] digits: those digits are
/// `floor(num · 10^m / count)` for the `m` digits after `lead`'s, computed as one `BigUint`
/// multiplication and one division, then rounded up when the next digit is `>= 5`. The
/// remainder is never zero, so no digit loop stops early and the result is the loop's.
/// `BigUint`'s division by a `u64` is a single pass over its limbs.
fn non_terminating_quotient(num: u128, lead: u128, scale: i64, count: u64, negative: bool) -> BigDecimal {
	let m = BIGDECIMAL_DIV_PRECISION - lead.checked_ilog10().map_or(1, |d| d + 1);
	let shifted = BigUint::from(num) * power_of_ten(m);
	let remainder = u128::from((&shifted % count).to_u64().unwrap_or(0));
	let mut quotient = shifted / count;
	if remainder * 10 / u128::from(count) >= 5 {
		quotient += 1_u8;
	}
	let magnitude = BigDecimal::new(BigInt::from_biguint(Sign::Plus, quotient), scale + i64::from(m));
	if negative {
		-magnitude
	} else {
		magnitude
	}
}

/// Accumulates quotient digits into a `BigInt`, 38 at a time through a `u128` chunk, so the
/// `BigInt` is touched once per chunk rather than once per digit.
struct DigitSink {
	quotient: BigInt,
	chunk: u128,
	chunk_digits: u32,
}

impl DigitSink {
	fn push(&mut self, digit: u128) {
		self.chunk = self.chunk * 10 + digit;
		self.chunk_digits += 1;
		if self.chunk_digits == 38 {
			self.quotient = std::mem::take(&mut self.quotient) * BigInt::from(10_u128.pow(38)) + BigInt::from(self.chunk);
			(self.chunk, self.chunk_digits) = (0, 0);
		}
	}
}

/// `sum / count` for a [`BigDecimal`] sum, identical to `bigdecimal`'s own division: through
/// [`avg_like_bigdecimal`] when the sum's unscaled integer fits `i128` (every realistic bucket),
/// otherwise by the `BigDecimal` division itself.
pub fn avg_of_sum(sum: &BigDecimal, count: u64) -> BigDecimal {
	let (digits, scale) = sum.as_bigint_and_exponent();
	i128::try_from(&digits).map_or_else(|_| sum / &BigDecimal::from(count), |int| avg_like_bigdecimal(int, scale, count))
}

/// Whether [`reduce_scaled`] can compute `aggregation` from integer state alone.
const fn is_streaming(aggregation: Aggregation) -> bool {
	matches!(aggregation, Aggregation::Min | Aggregation::Max | Aggregation::Avg | Aggregation::Sum | Aggregation::First | Aggregation::Last)
}

/// Fold `(epoch_nanos, mantissa)` pairs into integer per-bucket state, applying the inclusive
/// `[start, end]` filter. `None` when a bound cannot be expressed in epoch nanoseconds or the
/// slices differ in length (the caller falls back to the `BigDecimal` path).
fn accumulate(epoch_nanos: &[i64], mantissas: &[i64], resolution: Resolution, start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>) -> Option<BTreeMap<i64, ScaledAcc>> {
	if epoch_nanos.len() != mantissas.len() {
		return None;
	}
	let bound = |b: Option<DateTime<Utc>>| b.map(|t| t.timestamp_nanos_opt()).map_or(Some(None), |n| n.map(Some));
	let (Some(lo), Some(hi)) = (bound(start), bound(end)) else {
		return None;
	};

	let mut buckets: BTreeMap<i64, ScaledAcc> = BTreeMap::new();
	// The bucket being filled. Time-ordered input stays in it until the key changes, so the
	// map is touched once per bucket rather than once per sample, and while a sample's
	// instant lies in the bucket's nanosecond range (`span`) it is not indexed at all.
	let mut current: Option<(i64, ScaledAcc)> = None;
	let mut span: Option<(i64, i64)> = None;
	for (&nanos, &mantissa) in epoch_nanos.iter().zip(mantissas) {
		if lo.is_some_and(|l| nanos < l) || hi.is_some_and(|h| nanos > h) {
			continue;
		}
		if let (Some((_, acc)), Some((from, to))) = (current.as_mut(), span) {
			if from <= nanos && nanos <= to {
				acc.push(nanos, mantissa);
				continue;
			}
		}
		let base = base_of(resolution, nanos);
		match current.as_mut() {
			Some((key, acc)) if *key == base => acc.push(nanos, mantissa),
			_ => {
				if let Some((key, acc)) = current.take() {
					buckets.entry(key).and_modify(|existing| existing.merge(&acc)).or_insert(acc);
				}
				current = Some((base, ScaledAcc::new(nanos, mantissa)));
				span = base_range(resolution, base);
			}
		}
	}
	if let Some((key, acc)) = current {
		buckets.entry(key).and_modify(|existing| existing.merge(&acc)).or_insert(acc);
	}

	Some(buckets)
}

/// Reduce a `ScaledI64` column (`value = mantissa × 10^-scale`) into grid-aligned buckets.
///
/// Returns the same buckets [`reduce`](crate::reduce) returns for the equivalent `BigDecimal`
/// points, with the same inclusive `[start, end]` filter, the same ascending order, the same
/// default reduction set for an empty `aggregations`, and equal values. It works from integer
/// accumulators and materializes a [`BigDecimal`] once per output bucket. `epoch_nanos[i]`
/// is the instant of `mantissas[i]` in nanoseconds since the Unix epoch; input order does not
/// matter, but time-ordered input takes a no-lookup fast path.
///
/// Returns `Ok(None)` when it cannot answer exactly and the caller should use
/// [`reduce`](crate::reduce): a requested reduction is not a streaming one (percentiles, TWA,
/// sketches), a bound cannot be expressed in epoch nanoseconds, or the slices differ in length.
///
/// # Errors
///
/// [`ReduceError::BucketStartOverflow`] if a bucket's grid start scales past the representable
/// time range.
pub fn reduce_scaled(epoch_nanos: &[i64], mantissas: &[i64], scale: u32, resolution: Resolution, start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>, aggregations: &[Aggregation]) -> Result<Option<Vec<Bucket>>, ReduceError> {
	let aggregations = if aggregations.is_empty() { &Aggregation::DEFAULT[..] } else { aggregations };
	if !aggregations.iter().all(|&a| is_streaming(a)) {
		return Ok(None);
	}
	let Some(buckets) = accumulate(epoch_nanos, mantissas, resolution, start, end) else {
		return Ok(None);
	};

	let scale = i64::from(scale);
	let decimal = |m: i128| BigDecimal::new(BigInt::from(m), scale);
	buckets.into_iter()
		.map(|(base, acc)| {
			let timestamp = bucket_start(resolution, base).ok_or(ReduceError::BucketStartOverflow)?;
			let sum = decimal(acc.sum);
			let mut values: BTreeMap<String, BigDecimal> = BTreeMap::new();
			for &agg in aggregations {
				let value = match agg {
					Aggregation::Min => decimal(i128::from(acc.min)),
					Aggregation::Max => decimal(i128::from(acc.max)),
					Aggregation::Sum => sum.clone(),
					// The same result `reduce`'s `sum / count` produces, from integer long division.
					Aggregation::Avg => avg_like_bigdecimal(acc.sum, scale, acc.count),
					Aggregation::First => decimal(i128::from(acc.first.1)),
					Aggregation::Last => decimal(i128::from(acc.last.1)),
					_ => unreachable!("non-streaming reductions return Ok(None) above"),
				};
				values.insert(agg.as_str().to_string(), value);
			}
			Ok(Bucket { timestamp, count: usize::try_from(acc.count).unwrap_or(usize::MAX), values })
		})
		.collect::<Result<Vec<_>, _>>()
		.map(Some)
}

/// The mergeable form of [`reduce_scaled`]: the same integer accumulation, returned as a
/// [`PartialReduction`] that merges exactly with partials built by
/// [`reduce_partial`](crate::reduce_partial), including persisted `.weftpart` sidecars.
///
/// Each bucket's integer state is converted once into the `BigDecimal` accumulator the
/// partial carries (sum, min, max, and first/last with their instants), so the per-sample
/// work stays in integers and only the per-bucket conversion touches `BigDecimal`. Like
/// [`reduce_scaled`], it returns `Ok(None)` for an exact percentile or TWA, a bound outside
/// epoch nanoseconds, or mismatched slices, and the caller falls back to `reduce_partial`.
/// Unlike [`reduce_scaled`] it also accepts the `sketch_p*` reductions (see below), so it covers
/// the whole `.weftpart` sidecar set.
///
/// # Errors
///
/// [`ReduceError::TimestampRange`] if a bucket's first/last instant cannot be represented as
/// a timestamp (unreachable for nanosecond epochs, which always are).
pub fn reduce_partial_scaled(epoch_nanos: &[i64], mantissas: &[i64], scale: u32, resolution: Resolution, start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>, aggregations: &[Aggregation]) -> Result<Option<PartialReduction>, ReduceError> {
	let aggregations = if aggregations.is_empty() { &Aggregation::DEFAULT[..] } else { aggregations };
	// The `sketch_p*` reductions are accepted too: they need each value, but only as the
	// sketch's `f64`, which `DdSketch::add_scaled` derives from the mantissa as the same `f64`
	// `add_decimal` gives the equal BigDecimal, so every sketch is identical. Percentiles and
	// TWA still need the samples: decline.
	if !aggregations.iter().all(|&a| is_streaming(a) || a.sketch_quantile().is_some()) {
		return Ok(None);
	}
	let sketching = aggregations.iter().any(|a| a.sketch_quantile().is_some());
	let Some(buckets) = accumulate(epoch_nanos, mantissas, resolution, start, end) else {
		return Ok(None);
	};
	let scale = i64::from(scale);
	let decimal = |m: i64| BigDecimal::new(BigInt::from(m), scale);
	let instant = |n: i64| DateTime::from_timestamp_nanos(n);
	let mut converted: BTreeMap<i64, BucketAcc> = buckets
		.into_iter()
		.map(|(base, acc)| {
			let state = BucketAcc { count: usize::try_from(acc.count).map_err(|_| ReduceError::TimestampRange)?, sum: BigDecimal::new(BigInt::from(acc.sum), scale), min: Some(decimal(acc.min)), max: Some(decimal(acc.max)), first: Some((instant(acc.first.0), decimal(acc.first.1))), last: Some((instant(acc.last.0), decimal(acc.last.1))), samples: Vec::new(), collect: false, sketch: sketching.then(DdSketch::with_default_accuracy) };
			Ok((base, state))
		})
		.collect::<Result<_, ReduceError>>()?;
	if sketching {
		let bound = |b: Option<DateTime<Utc>>| b.and_then(|t| t.timestamp_nanos_opt());
		let (lo, hi) = (bound(start), bound(end));
		// The buckets in key order, so a time-ordered pass finds each sample's bucket by its
		// nanosecond range and looks a bucket up only when the range is left.
		let mut ordered: Vec<(i64, &mut BucketAcc)> = converted.iter_mut().map(|(&base, acc)| (base, acc)).collect();
		let mut current: Option<(usize, Option<(i64, i64)>)> = None;
		for (&nanos, &mantissa) in epoch_nanos.iter().zip(mantissas) {
			if lo.is_some_and(|l| nanos < l) || hi.is_some_and(|h| nanos > h) {
				continue;
			}
			let position = match current {
				Some((position, Some((from, to)))) if from <= nanos && nanos <= to => position,
				_ => {
					let base = base_of(resolution, nanos);
					let Ok(position) = ordered.binary_search_by_key(&base, |(b, _)| *b) else { continue };
					current = Some((position, base_range(resolution, base)));
					position
				}
			};
			if let Some(sketch) = ordered[position].1.sketch.as_mut() {
				sketch.add_scaled(mantissa, scale).map_err(|_| ReduceError::SketchValue)?;
			}
		}
	}
	Ok(Some(PartialReduction { buckets: converted }))
}

#[cfg(test)]
mod tests {
	use splimes::Point;

	use super::*;
	use crate::reduce;

	/// The equivalent `BigDecimal` points for a scaled column.
	fn points(nanos: &[i64], mantissas: &[i64], scale: u32) -> Vec<Point> {
		nanos.iter().zip(mantissas).map(|(&n, &m)| Point { timestamp: DateTime::from_timestamp_nanos(n), value: BigDecimal::new(BigInt::from(m), i64::from(scale)) }).collect()
	}

	/// A deterministic pseudo-random stream.
	fn noise(seed: u64) -> impl FnMut() -> u64 {
		let mut state = seed;
		move || {
			state ^= state << 13;
			state ^= state >> 7;
			state ^= state << 17;
			state
		}
	}

	#[test]
	fn base_range_is_exactly_the_nanoseconds_base_of_maps_to_each_bucket() {
		let mut next = noise(0x51_7cc1_b727_220a);
		let resolutions = [Resolution::Nanoseconds, Resolution::Microseconds, Resolution::Milliseconds, Resolution::Seconds, Resolution::Minutes, Resolution::Hours, Resolution::Days, Resolution::Weeks, Resolution::Months, Resolution::Years];
		let fixed = [0_i64, 1, -1, 999_999_999, -999_999_999, -1_000_000_000, -1_000_000_001, 59 * NANOS_PER_SECOND, -61 * NANOS_PER_SECOND, 3_599_999_999_999, -3_600_000_000_000, 1_505_412_060 * NANOS_PER_SECOND];
		let random = (0..2_000).map(|_| (next() % 4_000_000_000_000_000_000).cast_signed() - 2_000_000_000_000_000_000);
		for nanos in fixed.into_iter().chain(random) {
			for r in resolutions {
				let base = base_of(r, nanos);
				let (from, to) = base_range(r, base).expect("representable");
				assert!(from <= nanos && nanos <= to, "{r:?}: {nanos} outside {from}..={to}");
				// The range is tight: its ends belong to the bucket and one past them do not.
				assert_eq!((base_of(r, from), base_of(r, to)), (base, base), "{r:?} at {nanos}");
				assert_ne!(from.checked_sub(1).map(|n| base_of(r, n)), Some(base), "{r:?}: {from} - 1");
				assert_ne!(to.checked_add(1).map(|n| base_of(r, n)), Some(base), "{r:?}: {to} + 1");
			}
		}
		assert_eq!(base_range(Resolution::Years, i64::MAX / 2), None, "unrepresentable ranges are declined");
	}

	#[test]
	fn base_of_matches_bucket_index_for_every_resolution() {
		let mut next = noise(0x9e37_79b9_7f4a_7c15);
		let resolutions = [Resolution::Nanoseconds, Resolution::Microseconds, Resolution::Milliseconds, Resolution::Seconds, Resolution::Minutes, Resolution::Hours, Resolution::Days, Resolution::Weeks, Resolution::Months, Resolution::Years];
		let fixed = [0_i64, 1, -1, 999_999_999, -999_999_999, -1_000_000_000, 59 * NANOS_PER_SECOND, -61 * NANOS_PER_SECOND, 1_505_412_060 * NANOS_PER_SECOND];
		let random = (0..2_000).map(|_| (next() % 4_000_000_000_000_000_000).cast_signed() - 2_000_000_000_000_000_000);
		for nanos in fixed.into_iter().chain(random) {
			let t = DateTime::from_timestamp_nanos(nanos);
			for r in resolutions {
				assert_eq!(base_of(r, nanos), crate::bucket_index(r, &t).expect("indexable"), "{r:?} at {nanos}");
			}
		}
	}

	#[test]
	fn reduce_scaled_equals_reduce_on_every_streaming_aggregation() {
		// Irregular, partly out-of-order instants (including negative epochs) and mixed-sign
		// mantissas at scale 8, over several resolutions and filters, with every streaming
		// reduction requested and with the default set.
		let mut next = noise(0x2545_f491_4f6c_dd1d);
		let mut t = -3_600 * NANOS_PER_SECOND;
		let mut nanos: Vec<i64> = (0..5_000)
			.map(|_| {
				t += (next() % 90_000_000_000).cast_signed();
				t
			})
			.collect();
		for i in (0..nanos.len()).step_by(37) {
			nanos.swap(i, (i + 11) % 5_000);
		}
		let mantissas: Vec<i64> = (0..5_000).map(|_| (next() % 2_000_000_000_000).cast_signed() - 1_000_000_000_000).collect();
		let pts = points(&nanos, &mantissas, 8);
		let all = [Aggregation::Min, Aggregation::Max, Aggregation::Avg, Aggregation::Sum, Aggregation::First, Aggregation::Last];
		let window = (Some(DateTime::from_timestamp_nanos(nanos[100])), Some(DateTime::from_timestamp_nanos(nanos[4_000])));
		for resolution in [Resolution::Seconds, Resolution::Minutes, Resolution::Hours, Resolution::Days] {
			for (start, end) in [(None, None), window] {
				for aggs in [&all[..], &[][..], &[Aggregation::Avg][..]] {
					let expected = reduce(&pts, resolution, start, end, aggs).expect("reduces");
					let actual = reduce_scaled(&nanos, &mantissas, 8, resolution, start, end, aggs).expect("reduces").expect("streaming reductions are supported");
					assert_eq!(actual, expected, "{resolution:?} {start:?}..{end:?} {aggs:?}");
				}
			}
		}
	}

	#[test]
	fn avg_like_bigdecimal_is_identical_to_bigdecimal_division() {
		// Same value AND same representation (int_val, scale) as `bigdecimal`'s own `/`, over
		// random sums and counts, the special cases, exact and repeating quotients, rounding
		// carries (…999 → …000) and extreme magnitudes.
		let mut next = noise(0xd1b5_4a32_d192_ed03);
		let mut cases: Vec<(i128, i64, u64)> = vec![(0, 8, 7), (5, 2, 1), (7, 0, 7), (-7, 3, 7), (1, 0, 3), (2, 0, 3), (-2, 5, 3), (10, 1, 4), (1, 0, 9_999_999_999), (i128::from(i64::MAX) * 1_000, 8, 3), (-i128::from(i64::MAX) * 1_000, 8, 7), (1, 0, u64::MAX), (u64::MAX.into(), 0, u64::MAX - 1), (2, 0, 30), (1, 0, 6)];
		for _ in 0..20_000 {
			let sum = i128::from((next() % 4_000_000_000_000).cast_signed() - 2_000_000_000_000) * i128::from(next() % 1_000 + 1);
			let count = next() % 100_000 + 1;
			let scale = (next() % 12).cast_signed();
			cases.push((sum, scale, count));
		}
		// Counts across the whole `u64` range (every remainder width the chunked division
		// sees), sums across `i128`, and divisors whose quotient terminates inside a chunk.
		for _ in 0..5_000 {
			let count = (next() >> (next() % 64)).max(1);
			let sum = (i128::from(next()) << (next() % 60)) * if next().is_multiple_of(2) { 1 } else { -1 };
			cases.push((sum, (next() % 30).cast_signed(), count));
		}
		for exp in 0..40_u32 {
			let (two, five) = (1_u64 << exp.min(63), 5_u64.saturating_pow(exp.min(27)));
			cases.extend([(1, 0, two), (3, 2, two), (7, 0, five), (-13, 4, five), (1, 0, two.saturating_mul(3))]);
		}
		for (sum, scale, count) in cases {
			let expected = &BigDecimal::new(BigInt::from(sum), scale) / &BigDecimal::from(count);
			let actual = avg_like_bigdecimal(sum, scale, count);
			assert_eq!(actual.as_bigint_and_exponent(), expected.as_bigint_and_exponent(), "{sum}e-{scale} / {count}");
			assert_eq!(avg_of_sum(&BigDecimal::new(BigInt::from(sum), scale), count).as_bigint_and_exponent(), expected.as_bigint_and_exponent());
		}
		// A sum past i128 takes the BigDecimal division, unchanged.
		let huge = BigDecimal::new(BigInt::from(i128::MAX) * BigInt::from(1_000), 3);
		assert_eq!(avg_of_sum(&huge, 7).as_bigint_and_exponent(), (&huge / &BigDecimal::from(7_u64)).as_bigint_and_exponent());
	}

	#[test]
	fn reduce_partial_scaled_merges_exactly_with_bigdecimal_partials() {
		// Split one series in two: reduce one half with the integer partial and the other with
		// the BigDecimal partial, merge in both orders, and finish. The result must equal a
		// single BigDecimal pass over the whole series, and the scaled partial alone must finish
		// to its half's single pass.
		let mut next = noise(0x6a09_e667_f3bc_c909);
		let nanos: Vec<i64> = (0..4_000_i64).map(|i| i * 17_000_000_000 + (next() % 9_000_000_000).cast_signed()).collect();
		let mantissas: Vec<i64> = (0..4_000).map(|_| (next() % 900_000_000).cast_signed() - 300_000_000).collect();
		let pts = points(&nanos, &mantissas, 6);
		let aggs = [Aggregation::Min, Aggregation::Max, Aggregation::Avg, Aggregation::Sum, Aggregation::First, Aggregation::Last];
		for resolution in [Resolution::Minutes, Resolution::Hours] {
			let whole = reduce(&pts, resolution, None, None, &aggs).expect("reduces");
			let scaled_half = reduce_partial_scaled(&nanos[..2_500], &mantissas[..2_500], 6, resolution, None, None, &aggs).expect("reduces").expect("streaming");
			assert_eq!(scaled_half.clone().finish(resolution, &aggs).expect("finishes"), reduce(&pts[..2_500], resolution, None, None, &aggs).expect("reduces"));
			let big_half = crate::reduce_partial(&pts[2_500..], resolution, None, None, &aggs).expect("reduces");
			let mut a = scaled_half.clone();
			a.merge(big_half.clone()).expect("merges");
			let mut b = big_half;
			b.merge(scaled_half).expect("merges");
			assert_eq!(a.finish(resolution, &aggs).expect("finishes"), whole);
			assert_eq!(b.finish(resolution, &aggs).expect("finishes"), whole);
		}
		assert!(reduce_partial_scaled(&nanos, &mantissas, 6, Resolution::Hours, None, None, &[Aggregation::P50]).expect("ok").is_none());
	}

	#[test]
	fn reduce_partial_scaled_builds_identical_sketches() {
		// With the sidecar's full set (streaming + sketch_p50..p99) the integer partial must
		// finish to exactly the BigDecimal partial's buckets, sketch quantiles included, and a
		// window must filter the sketch inputs too.
		let mut next = noise(0xbb67_ae85_84ca_a73b);
		let nanos: Vec<i64> = (0..3_000_i64).map(|i| i * 7_000_000_000 + (next() % 5_000_000_000).cast_signed()).collect();
		let mantissas: Vec<i64> = (0..3_000).map(|_| (next() % 9_000_000).cast_signed() - 1_000_000).collect();
		let pts = points(&nanos, &mantissas, 4);
		let aggs = [Aggregation::Min, Aggregation::Max, Aggregation::Avg, Aggregation::Sum, Aggregation::First, Aggregation::Last, Aggregation::SketchP50, Aggregation::SketchP90, Aggregation::SketchP95, Aggregation::SketchP99];
		for (start, end) in [(None, None), (Some(DateTime::from_timestamp_nanos(nanos[300])), Some(DateTime::from_timestamp_nanos(nanos[2_200])))] {
			let expected = crate::reduce_partial(&pts, Resolution::Hours, start, end, &aggs).expect("reduces").finish(Resolution::Hours, &aggs).expect("finishes");
			let actual = reduce_partial_scaled(&nanos, &mantissas, 4, Resolution::Hours, start, end, &aggs).expect("reduces").expect("covered").finish(Resolution::Hours, &aggs).expect("finishes");
			assert_eq!(actual, expected, "{start:?}..{end:?}");
		}
	}

	#[test]
	fn reduce_scaled_declines_what_it_cannot_answer_exactly() {
		let (nanos, mantissas) = ([0_i64, 1_000], [5_i64, 7]);
		assert_eq!(reduce_scaled(&nanos, &mantissas, 2, Resolution::Seconds, None, None, &[Aggregation::P99]), Ok(None));
		assert_eq!(reduce_scaled(&nanos, &mantissas, 2, Resolution::Seconds, None, None, &[Aggregation::Avg, Aggregation::Twa]), Ok(None));
		assert_eq!(reduce_scaled(&nanos, &mantissas[..1], 2, Resolution::Seconds, None, None, &[Aggregation::Avg]), Ok(None));
		assert_eq!(reduce_scaled(&[], &[], 2, Resolution::Seconds, None, None, &[Aggregation::Avg]), Ok(Some(Vec::new())));
	}
}
