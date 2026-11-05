//! Checked numeric conversions.
//!
//! Every conversion here is either lossless or saturating and never silently wraps,
//! which is what lets the crate avoid bare `as` casts between integer and float types.

/// `v` as an `f64`, rounded to nearest exactly as an `as` cast would.
///
/// Splitting into two `u32` halves keeps every step a lossless `From` conversion; the
/// single fused multiply-add is then the only rounding.
#[must_use]
pub fn u64_to_f64(v: u64) -> f64 {
	let high = u32::try_from(v >> 32).unwrap_or(u32::MAX);
	let low = u32::try_from(v & u64::from(u32::MAX)).unwrap_or(u32::MAX);
	f64::from(high).mul_add(4_294_967_296.0, f64::from(low))
}

/// `v` as an `f64`; a `usize` wider than 64 bits saturates (no such target exists).
#[must_use]
pub fn usize_to_f64(v: usize) -> f64 {
	u64_to_f64(u64::try_from(v).unwrap_or(u64::MAX))
}

/// `v` as an `f64`, rounded to nearest.
#[must_use]
pub fn i64_to_f64(v: i64) -> f64 {
	let magnitude = u64_to_f64(v.unsigned_abs());
	if v < 0 {
		-magnitude
	} else {
		magnitude
	}
}

/// `v` as an `i64`, saturating at [`i64::MAX`] (a `usize` never exceeds it on a 64-bit target).
#[must_use]
pub fn usize_to_i64(v: usize) -> i64 {
	i64::try_from(v).unwrap_or(i64::MAX)
}
