//! Checked numeric conversions, used in place of `as` casts.

use bigdecimal::ToPrimitive;

/// The nearest `f64` to a count.
///
/// Counts past 2^53 round to the nearest representable value, as an `as` cast does.
#[must_use]
pub fn usize_to_f64(value: usize) -> f64 {
	u64::try_from(value).ok().and_then(|value| value.to_f64()).unwrap_or(f64::MAX)
}
