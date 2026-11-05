//! Numeric conversions the server needs and the standard library does not
//! offer losslessly: counts reported as `f64` for JSON and Prometheus.

/// `n` as the nearest `f64`, rounded once (to nearest, ties to even) — the
/// value an `as` cast yields, spelled without one.
///
/// Every integer below 2^53 converts exactly; a larger one is rounded to the
/// nearest representable `f64`, which is what a count reported as a JSON or
/// Prometheus number means anyway.
#[must_use]
pub fn u64_to_f64(n: u64) -> f64 {
	const TWO_POW_32: f64 = 4_294_967_296.0;
	let high = u32::try_from(n >> 32).unwrap_or(u32::MAX);
	let low = u32::try_from(n & u64::from(u32::MAX)).unwrap_or(u32::MAX);
	// `high * 2^32` is exact; the one addition is the one rounding.
	f64::from(high).mul_add(TWO_POW_32, f64::from(low))
}

/// [`u64_to_f64`] for a `usize` count.
#[must_use]
pub fn usize_to_f64(n: usize) -> f64 {
	u64_to_f64(u64::try_from(n).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn conversions_round_once_to_nearest() {
		let cases: [(u64, f64); 7] = [(0, 0.0), (1, 1.0), (1 << 32, 4_294_967_296.0), ((1 << 53) - 1, 9_007_199_254_740_991.0), ((1 << 53) + 1, 9_007_199_254_740_992.0), ((1 << 53) + 3, 9_007_199_254_740_996.0), (u64::MAX, 18_446_744_073_709_551_616.0)];
		for (n, expected) in cases {
			assert_eq!(u64_to_f64(n).to_bits(), expected.to_bits(), "{n}");
		}
		assert_eq!(usize_to_f64(3).to_bits(), 3.0_f64.to_bits());
	}
}
