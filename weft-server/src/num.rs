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

#[cfg(test)]
mod json_float_parse_tests {
	/// Every JSON float the server reads is the nearest `f64` to its text, whichever way the
	/// binary was built. `serde_json`'s default float parser is not correctly rounded: before
	/// the workspace turned on its `float_roundtrip` feature, a `weft-server` built on its
	/// own (`cargo build -p weft-server`, which is how the release workflow and the Docker
	/// image build it) read both values below one ULP low, and only a whole-workspace build —
	/// where another member switches on `arbitrary_precision` — read them right.
	#[test]
	fn json_floats_parse_to_the_nearest_f64() {
		for text in ["900719925474100.5", "0.009999999999990905", "9007199254740993", "1e-19", "2.2250738585072014e-308", "0.30000000000000004"] {
			let from_json: f64 = serde_json::from_str(text).unwrap();
			let exact: f64 = text.parse().unwrap();
			assert_eq!(from_json.to_bits(), exact.to_bits(), "{text}: serde_json read {from_json:e}, the nearest f64 is {exact:e}");
			let in_a_body: serde_json::Value = serde_json::from_str(&format!("{{\"value\": {text}}}")).unwrap();
			assert_eq!(in_a_body["value"].as_f64().map(f64::to_bits), Some(exact.to_bits()), "{text} inside a request body");
		}
	}
}
