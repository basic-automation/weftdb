//! Lossless-where-possible numeric conversions for measurement arithmetic.
//!
//! Benchmark statistics mix integer counts and nanosecond durations with `f64`
//! arithmetic. These helpers spell out each conversion once, with the same
//! rounding and saturation the built-in `as` casts give, so call sites stay free
//! of bare casts.

/// `2^32` as an `f64`.
const TWO_POW_32: f64 = 4_294_967_296.0;
/// `2^64` as an `f64`.
const TWO_POW_64: f64 = 18_446_744_073_709_551_616.0;
/// `2^63` as an `f64`.
const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;

/// Converts a `u64` to the nearest `f64` (round to nearest, ties to even).
#[must_use]
pub fn u64_to_f64(value: u64) -> f64 {
	let high = u32::try_from(value >> 32).unwrap_or(u32::MAX);
	let low = u32::try_from(value & u64::from(u32::MAX)).unwrap_or(u32::MAX);
	// `high * 2^32` is exact, so the single addition rounds once, as `as` does.
	f64::from(high).mul_add(TWO_POW_32, f64::from(low))
}

/// Converts a `usize` to the nearest `f64`.
#[must_use]
pub fn usize_to_f64(value: usize) -> f64 {
	u64_to_f64(u64::try_from(value).unwrap_or(u64::MAX))
}

/// Converts a `u128` to an `f64` (nearest, up to a final rounding step).
#[must_use]
pub fn u128_to_f64(value: u128) -> f64 {
	let high = u64::try_from(value >> 64).unwrap_or(u64::MAX);
	let low = u64::try_from(value & u128::from(u64::MAX)).unwrap_or(u64::MAX);
	u64_to_f64(high).mul_add(TWO_POW_64, u64_to_f64(low))
}

/// Converts an `i64` to the nearest `f64`.
#[must_use]
pub fn i64_to_f64(value: i64) -> f64 {
	let magnitude = u64_to_f64(value.unsigned_abs());
	if value < 0 {
		-magnitude
	} else {
		magnitude
	}
}

/// Converts an `f64` to a `u64` with `as` semantics: truncates toward zero,
/// saturates at the bounds, and maps `NaN` to zero.
#[must_use]
pub fn f64_to_u64(value: f64) -> u64 {
	if value.is_nan() || value < 1.0 {
		return 0;
	}
	if value >= TWO_POW_64 {
		return u64::MAX;
	}
	let bits = value.to_bits();
	// The biased exponent of a finite value in [1, 2^64) lies in [1023, 1086].
	let exponent = i32::try_from((bits >> 52) & 0x7ff).unwrap_or(0) - 1075;
	let mantissa = (bits & ((1_u64 << 52) - 1)) | (1_u64 << 52);
	let shift = exponent.unsigned_abs();
	if exponent >= 0 {
		mantissa << shift
	} else {
		mantissa >> shift
	}
}

/// Converts an `f64` to an `i64` with `as` semantics: truncates toward zero,
/// saturates at the bounds, and maps `NaN` to zero.
#[must_use]
pub fn f64_to_i64(value: f64) -> i64 {
	if value.is_nan() {
		return 0;
	}
	if value >= TWO_POW_63 {
		return i64::MAX;
	}
	if value <= -TWO_POW_63 {
		return i64::MIN;
	}
	let magnitude = i64::try_from(f64_to_u64(value.abs())).unwrap_or(i64::MAX);
	if value < 0.0 {
		-magnitude
	} else {
		magnitude
	}
}

/// Converts an `f64` to a `usize` with `as` semantics (truncating, saturating,
/// `NaN` to zero).
#[must_use]
pub fn f64_to_usize(value: f64) -> usize {
	usize::try_from(f64_to_u64(value)).unwrap_or(usize::MAX)
}

/// Converts a `u128` to a `u64`, saturating at `u64::MAX`.
#[must_use]
pub fn u128_to_u64(value: u128) -> u64 {
	u64::try_from(value).unwrap_or(u64::MAX)
}

/// Converts a `u64` to a `usize`, saturating at `usize::MAX`.
#[must_use]
pub fn u64_to_usize(value: u64) -> usize {
	usize::try_from(value).unwrap_or(usize::MAX)
}

/// Converts a `usize` to an `i64`, saturating at `i64::MAX`.
#[must_use]
pub fn usize_to_i64(value: usize) -> i64 {
	i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn integer_to_float_rounds_to_nearest() {
		assert_eq!(u64_to_f64(0).to_bits(), 0.0_f64.to_bits());
		assert_eq!(u64_to_f64(123_456_789).to_bits(), 123_456_789.0_f64.to_bits());
		// 2^53 + 1 is not representable; ties-to-even rounds it down to 2^53.
		assert_eq!(u64_to_f64((1 << 53) + 1).to_bits(), 9_007_199_254_740_992.0_f64.to_bits());
		assert_eq!(u64_to_f64(u64::MAX).to_bits(), TWO_POW_64.to_bits());
		assert_eq!(i64_to_f64(-5).to_bits(), (-5.0_f64).to_bits());
		assert_eq!(u128_to_f64(1 << 64).to_bits(), TWO_POW_64.to_bits());
	}

	#[test]
	fn float_to_integer_truncates_and_saturates() {
		let cases: [(f64, u64, i64); 9] = [(0.0, 0, 0), (0.9, 0, 0), (1.0, 1, 1), (3.99, 3, 3), (4_503_599_627_370_497.0, 4_503_599_627_370_497, 4_503_599_627_370_497), (1e19, 10_000_000_000_000_000_000, i64::MAX), (2e19, u64::MAX, i64::MAX), (f64::NAN, 0, 0), (f64::INFINITY, u64::MAX, i64::MAX)];
		for (value, unsigned, signed) in cases {
			assert_eq!(f64_to_u64(value), unsigned, "{value}");
			assert_eq!(f64_to_i64(value), signed, "{value}");
		}
		assert_eq!(f64_to_u64(-1.0), 0);
		assert_eq!(f64_to_i64(-3.99), -3);
		assert_eq!(f64_to_i64(f64::NEG_INFINITY), i64::MIN);
	}
}
