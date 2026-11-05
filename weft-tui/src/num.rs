//! Numeric conversion helpers that make lossy and saturating conversions explicit.

use num_traits::{Bounded, NumCast, ToPrimitive, Zero};

/// Converts an integer to `f64`, accepting the precision loss above 2^53.
///
/// Every primitive integer has an `f64` approximation, so the fallback is never taken.
pub fn lossy_f64<T: ToPrimitive + Copy>(value: T) -> f64 {
	value.to_f64().unwrap_or(f64::NAN)
}

/// Converts an `f64` to an integer, truncating toward zero and saturating at the target's
/// bounds (`NaN` becomes zero), matching the semantics of a float-to-int `as` cast.
pub fn saturating_from_f64<T: NumCast + Bounded + Zero>(value: f64) -> T {
	NumCast::from(value).unwrap_or_else(|| {
		if value.is_nan() {
			T::zero()
		} else if value > 0.0 {
			T::max_value()
		} else {
			T::min_value()
		}
	})
}

/// Integer percentage of `part` over `whole`, or zero when `whole` is zero.
pub fn percent<T: NumCast + Bounded + Zero, N: ToPrimitive + Copy + Zero + PartialOrd>(part: N, whole: N) -> T {
	if whole > N::zero() {
		saturating_from_f64(lossy_f64(part) / lossy_f64(whole) * 100.0)
	} else {
		T::zero()
	}
}
