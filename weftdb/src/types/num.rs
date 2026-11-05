//! Lossless-where-possible numeric conversions, written once so the rest of
//! the crate never reaches for a bare `as` cast.
//!
//! Each conversion keeps the semantics of the primitive cast it replaces: an
//! integer becomes the nearest `f64`, and a float becomes an integer by
//! truncating toward zero and saturating at the target's bounds, with NaN
//! mapping to zero.

use bigdecimal::ToPrimitive;

/// The nearest `f64` to an integer, as an `as f64` cast would give it.
pub fn to_f64<T: ToPrimitive + Copy>(n: T) -> f64 {
	ToPrimitive::to_f64(&n).unwrap_or(f64::NAN)
}

/// A float truncated toward zero into a `u32`, saturating at its bounds.
pub fn f64_to_u32(x: f64) -> u32 {
	x.to_u32().unwrap_or(if x > 0.0 { u32::MAX } else { 0 })
}

/// A float truncated toward zero into an `i32`, saturating at its bounds.
pub fn f64_to_i32(x: f64) -> i32 {
	x.to_i32().unwrap_or_else(|| {
		if x.is_nan() {
			0
		} else if x > 0.0 {
			i32::MAX
		} else {
			i32::MIN
		}
	})
}

/// A float truncated toward zero into a `usize`, saturating at its bounds.
pub fn f64_to_usize(x: f64) -> usize {
	x.to_usize().unwrap_or(if x > 0.0 { usize::MAX } else { 0 })
}

/// An `i64` clamped into a `usize`: negatives become zero.
pub fn i64_to_usize(n: i64) -> usize {
	usize::try_from(n).unwrap_or(if n < 0 { 0 } else { usize::MAX })
}

/// A `usize` clamped into an `i64`.
pub fn usize_to_i64(n: usize) -> i64 {
	i64::try_from(n).unwrap_or(i64::MAX)
}

/// A `u128` clamped into a `u64`.
pub fn u128_to_u64(n: u128) -> u64 {
	u64::try_from(n).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn float_conversions_saturate_like_as() {
		assert_eq!(f64_to_u32(-1.5), 0);
		assert_eq!(f64_to_u32(f64::NAN), 0);
		assert_eq!(f64_to_u32(1e20), u32::MAX);
		assert_eq!(f64_to_u32(3.9), 3);
		assert_eq!(f64_to_i32(-1e20), i32::MIN);
		assert_eq!(f64_to_i32(-2.7), -2);
		assert_eq!(f64_to_usize(7.99), 7);
		assert!((to_f64(u64::MAX) - 18_446_744_073_709_551_615.0).abs() < 1.0);
	}
}
