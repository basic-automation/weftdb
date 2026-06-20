//! # dsp-physical-type
//!
//! Physical numeric encodings for DSP aspects (roadmap **Phase 4.1**, priority #5
//! in the updated order; Immediate Next Action #8).
//!
//! ## Why this crate exists
//!
//! `BigDecimal` is DSP's **logical/API value type** and stays that way (hard
//! constraint #4: *precision-aware, not precision-taxed*). But carrying a
//! heap-allocated arbitrary-precision decimal through the measurement hot path —
//! storage, SIMD, GPU upload, interpolation — is far too slow to win the
//! benchmarks the roadmap is built around. So each aspect *declares* a
//! [`PhysicalType`]: the fast, fixed-width encoding its values execute in.
//!
//! The non-negotiable rule the roadmap states twice is that downcasting is
//! **explicit, schema-declared, and never silent**. This crate enforces that at
//! the type level: [`PhysicalType::encode`] always returns the [`Exactness`] of
//! the conversion alongside the encoded value, so a caller can never lose
//! precision without being handed the residual error to gate on a schema-level
//! bound. A `BigDecimal -> f64` that drops digits is a *reported* `Lossy`, not a
//! quiet truncation.
//!
//! ## Shape
//!
//! - [`PhysicalType`] — the schema-declared encoding. All six named in Phase
//!   4.1 are present: [`F64`], [`F32`], [`ScaledI64`], [`ScaledI128`],
//!   [`Decimal128`], [`BigDecimalText`].
//! - [`PhysicalValue`] — one value held in its encoded physical form.
//! - [`PhysicalType::encode`] — `BigDecimal` -> [`Encoded`] (`PhysicalValue` +
//!   [`Exactness`]).
//! - [`PhysicalValue::to_logical`] — the always-available inverse, reconstructing
//!   the `BigDecimal` the encoding represents.
//! - [`PhysicalType::profile`] — declarative per-encoding metadata
//!   ([`PhysicalProfile`]: storage width, lossless/hot-path eligibility).
//! - [`column`] — batch [`encode_column`] of a whole column under one encoding,
//!   aggregating exactness and estimating storage bytes (Storage-v2 / bytes-per-
//!   point prep).
//! - [`timestamp`] — Phase 4.2 integer-epoch timestamp codecs: lossless
//!   delta / delta-of-delta transforms ([`encode_delta`], [`encode_delta_of_delta`])
//!   plus a zig-zag + varint byte estimate, the timestamp half of bytes/point.
//! - [`segment`] — Phase 4.3 in-memory typed columnar [`Segment`]: a value
//!   column, a timestamp column, and per-segment min/max/count stats, with an
//!   exact encode/decode round trip and a `bytes_per_point` matching the bench.
//!
//! [`F64`]: PhysicalType::F64
//! [`F32`]: PhysicalType::F32
//! [`ScaledI64`]: PhysicalType::ScaledI64
//! [`ScaledI128`]: PhysicalType::ScaledI128
//! [`Decimal128`]: PhysicalType::Decimal128
//! [`BigDecimalText`]: PhysicalType::BigDecimalText
//!
//! ## Vendor-neutrality
//!
//! Per the workspace hard constraints this crate carries no vendor-specific
//! dependencies: it is a pure numeric encoding layer over `bigdecimal`, usable by
//! the store (`database`), the server (`dsp-server`), the harness (`dsp-bench`),
//! and the forthcoming Storage v2 columnar segments alike.

#![warn(clippy::pedantic, clippy::nursery, clippy::all)]

pub mod column;
pub mod segment;
pub mod timestamp;

use bigdecimal::{
	num_bigint::{BigInt, Sign}, BigDecimal, FromPrimitive, ToPrimitive
};
pub use column::{encode_column, recommend_encoding, ColumnEncodeError, ColumnEncoding};
pub use segment::{prune_by_time, Segment, SegmentError, SegmentStats, SEGMENT_FORMAT_VERSION};
use serde::{Deserialize, Serialize};
pub use timestamp::{decode_delta, decode_delta_of_delta, encode_delta, encode_delta_of_delta, rle_decode, rle_encode, rle_varint_bytes, uvarint_len, zigzag_varint_bytes, zigzag_varint_len, DeltaColumn, DeltaOfDeltaColumn, TimeUnit};

/// A schema-declared physical encoding for an aspect's numeric values.
///
/// The variant fixes how every value in the aspect is laid out on the hot path.
/// `BigDecimal` remains the logical type the API speaks; this is the *physical*
/// representation it executes in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PhysicalType {
	/// IEEE-754 binary64. Eight bytes, SIMD- and GPU-friendly, the default fast
	/// path. Exact only for values that are exactly representable in binary
	/// floating point (e.g. `0.5`, `2.25`); `0.1` and most decimal fractions
	/// encode `Lossy`.
	F64,
	/// IEEE-754 binary32. Four bytes — half the width (and memory bandwidth) of
	/// [`F64`](Self::F64), the cheapest GPU-upload path — at the cost of ~7
	/// significant decimal digits. Exact only for binary-representable values
	/// within that precision; `NotFinite` above the binary32 range (~3.4e38).
	F32,
	/// Fixed-point: the value is stored as a signed 64-bit mantissa interpreted
	/// as `mantissa * 10^(-scale)`. Exact for any value whose magnitude fits an
	/// `i64` once shifted by `scale` decimal places and which has no more than
	/// `scale` fractional digits — the common case for sensor data with a known
	/// number of decimals. Integer-fast, GPU/SIMD-friendly.
	ScaledI64 {
		/// Number of fractional decimal digits the mantissa carries.
		scale: u8,
	},
	/// Fixed-point with a 128-bit mantissa: `mantissa * 10^(-scale)`. The wide
	/// sibling of [`ScaledI64`](Self::ScaledI64) for aspects whose shifted
	/// magnitude exceeds `i64` (~9.2e18) — high-precision financial or
	/// scientific values — while staying integer-exact within `scale`.
	ScaledI128 {
		/// Number of fractional decimal digits the mantissa carries.
		scale: u8,
	},
	/// Decimal floating point with a 128-bit mantissa and a **per-value** scale:
	/// `mantissa * 10^(-scale)`. Unlike the fixed-scale `ScaledI*` encodings the
	/// exponent rides with each value, so it represents any `BigDecimal` whose
	/// significant digits fit `i128` (up to 38 digits) exactly, across a wide
	/// dynamic range — values too large simply lose low-order digits (reported
	/// `Lossy`), never erroring.
	Decimal128,
	/// Lossless decimal text — the verbatim plain-string form of the
	/// `BigDecimal`. Always [`Exactness::Exact`] for every finite value, at the
	/// cost of variable width and parse-on-read. The escape hatch for aspects
	/// that must not lose a single digit.
	BigDecimalText,
}

/// A single value held in its encoded physical form.
///
/// Produced by [`PhysicalType::encode`] and turned back into a `BigDecimal` by
/// [`PhysicalValue::to_logical`]. The variant always matches the [`PhysicalType`]
/// it was encoded under.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PhysicalValue {
	/// An [`PhysicalType::F64`]-encoded value. Invariant: always finite (encode
	/// rejects non-finite results).
	F64(f64),
	/// An [`PhysicalType::F32`]-encoded value. Invariant: always finite.
	F32(f32),
	/// A [`PhysicalType::ScaledI64`]-encoded value: `mantissa * 10^(-scale)`.
	ScaledI64 {
		/// The fixed-point mantissa.
		mantissa: i64,
		/// The fractional-digit scale shared by the aspect.
		scale: u8,
	},
	/// A [`PhysicalType::ScaledI128`]-encoded value: `mantissa * 10^(-scale)`.
	ScaledI128 {
		/// The 128-bit fixed-point mantissa.
		mantissa: i128,
		/// The fractional-digit scale shared by the aspect.
		scale: u8,
	},
	/// A [`PhysicalType::Decimal128`]-encoded value: `mantissa * 10^(-scale)`
	/// with a per-value `scale`.
	Decimal128 {
		/// The 128-bit significand.
		mantissa: i128,
		/// This value's own base-10 scale (negative for magnitudes above 1).
		scale: i64,
	},
	/// A [`PhysicalType::BigDecimalText`]-encoded value: the plain-string decimal.
	BigDecimalText(String),
}

/// Whether an [`encode`](PhysicalType::encode) preserved the logical value
/// exactly, and if not, by how much it missed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exactness {
	/// The encoded value reconstructs to the original `BigDecimal` bit-for-bit.
	Exact,
	/// The encoding lost precision; `abs_error` is `|original - decoded|`
	/// (normalized), for a caller to compare against a schema-level error bound.
	Lossy {
		/// The absolute reconstruction error, `|original - decoded|`.
		abs_error: BigDecimal,
	},
}

impl Exactness {
	/// `true` iff the encoding was exact.
	#[must_use]
	pub const fn is_exact(&self) -> bool {
		matches!(self, Self::Exact)
	}

	/// The absolute reconstruction error: zero when [`Exact`](Self::Exact),
	/// otherwise the stored `abs_error`.
	#[must_use]
	pub fn abs_error(&self) -> BigDecimal {
		match self {
			Self::Exact => BigDecimal::from(0),
			Self::Lossy { abs_error } => abs_error.clone(),
		}
	}
}

/// The result of an [`encode`](PhysicalType::encode): the physical value plus the
/// [`Exactness`] of the conversion, so loss can never be silent.
#[derive(Debug, Clone, PartialEq)]
pub struct Encoded {
	/// The value in its physical encoding.
	pub value: PhysicalValue,
	/// Whether the encoding was lossless, and the residual error if not.
	pub exactness: Exactness,
}

/// Why a value could not be encoded under a given [`PhysicalType`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
	/// The value's magnitude does not fit the target integer encoding (the
	/// shifted mantissa overflows `i64` for [`PhysicalType::ScaledI64`]).
	Overflow,
	/// The value could not be represented as a finite `f64` (its magnitude
	/// exceeds the binary64 range) for [`PhysicalType::F64`].
	NotFinite,
}

impl std::fmt::Display for EncodeError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Overflow => f.write_str("value magnitude overflows the target integer encoding"),
			Self::NotFinite => f.write_str("value is not representable as a finite f64"),
		}
	}
}

impl std::error::Error for EncodeError {}

/// `10^n` as a `BigDecimal`, built without a `Pow` trait import.
fn pow10(n: u8) -> BigDecimal {
	let ten = BigInt::from(10);
	let mantissa = (0..n).fold(BigInt::from(1), |acc, _| acc * &ten);
	BigDecimal::from(mantissa)
}

/// Divide a `BigInt` by 10, rounding half away from zero — one step of the
/// digit-shedding loop that fits a mantissa into `i128` for
/// [`PhysicalType::Decimal128`].
fn round_div_10(n: &BigInt) -> BigInt {
	let bias = if n.sign() == Sign::Minus { BigInt::from(-5) } else { BigInt::from(5) };
	(n + bias) / BigInt::from(10)
}

/// Reduce `(mantissa, scale)` (value = `mantissa * 10^(-scale)`) until the
/// mantissa fits `i128`, shedding one low-order digit per step. Returns the
/// fitted `(i128, scale)`; exactness is decided by the caller via [`measure`].
fn fit_i128(mut mantissa: BigInt, mut scale: i64) -> (i128, i64) {
	loop {
		if let Some(m) = mantissa.to_i128() {
			return (m, scale);
		}
		mantissa = round_div_10(&mantissa);
		scale -= 1;
	}
}

/// Classify the loss between an original value and its reconstruction.
///
/// Zero-ness is decided on the difference's integer mantissa sign, which is
/// robust to `BigDecimal` scale differences (`0` is the only value with a
/// no-sign mantissa).
fn measure(original: &BigDecimal, decoded: &BigDecimal) -> Exactness {
	let diff = original - decoded;
	let (mantissa, _exp) = diff.as_bigint_and_exponent();
	if mantissa.sign() == Sign::NoSign {
		Exactness::Exact
	} else {
		Exactness::Lossy { abs_error: diff.abs().normalized() }
	}
}

/// Declarative metadata for a [`PhysicalType`].
///
/// This realizes the "each declaring storage encoding, eligibility, exactness
/// guarantees" sentence of roadmap Phase 4.1 as data a schema, planner, or
/// benchmark report can consume without re-deriving it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalProfile {
	/// The encoding's stable identifier (see [`PhysicalType::name`]).
	pub name: &'static str,
	/// Fixed per-value storage width in bytes for DSP's in-memory/columnar
	/// representation, or `None` for the variable-width
	/// [`BigDecimalText`](PhysicalType::BigDecimalText). (For
	/// [`Decimal128`](PhysicalType::Decimal128) this counts the 16-byte mantissa
	/// plus its 8-byte per-value scale, not IEEE-754 decimal128's packed 16.)
	pub fixed_width_bytes: Option<usize>,
	/// `true` only when the encoding reconstructs **every** finite value exactly
	/// (i.e. [`BigDecimalText`](PhysicalType::BigDecimalText)). The fixed-width
	/// encodings are exact only within their range/scale and otherwise report
	/// [`Exactness::Lossy`].
	pub always_lossless: bool,
	/// `true` when values can live on the SIMD/GPU/columnar hot path as a
	/// fixed-width column — every encoding except the variable-width
	/// [`BigDecimalText`](PhysicalType::BigDecimalText), which must go through
	/// `BigDecimal`.
	pub hot_path_eligible: bool,
}

impl PhysicalType {
	/// The declarative [`PhysicalProfile`] for this encoding.
	#[must_use]
	pub const fn profile(self) -> PhysicalProfile {
		let fixed_width_bytes = match self {
			Self::F32 => Some(4),
			Self::F64 | Self::ScaledI64 { .. } => Some(8),
			Self::ScaledI128 { .. } => Some(16),
			Self::Decimal128 => Some(24),
			Self::BigDecimalText => None,
		};
		PhysicalProfile { name: self.name(), fixed_width_bytes, always_lossless: matches!(self, Self::BigDecimalText), hot_path_eligible: fixed_width_bytes.is_some() }
	}

	/// Encode a logical `BigDecimal` into this physical type, reporting the
	/// [`Exactness`] of the conversion.
	///
	/// # Errors
	///
	/// Returns [`EncodeError::Overflow`] if the value does not fit a
	/// [`ScaledI64`](Self::ScaledI64) / [`ScaledI128`](Self::ScaledI128)
	/// mantissa, or [`EncodeError::NotFinite`] if it has no finite
	/// [`F64`](Self::F64) / [`F32`](Self::F32) representation. The
	/// [`Decimal128`](Self::Decimal128) and [`BigDecimalText`](Self::BigDecimalText)
	/// encodings never error. Lossy-but-representable conversions are **not**
	/// errors — they succeed with [`Exactness::Lossy`] so the caller decides
	/// whether the residual is acceptable.
	pub fn encode(self, value: &BigDecimal) -> Result<Encoded, EncodeError> {
		match self {
			Self::F64 => {
				let f = value.to_f64().filter(|f| f.is_finite()).ok_or(EncodeError::NotFinite)?;
				let pv = PhysicalValue::F64(f);
				let exactness = measure(value, &pv.to_logical());
				Ok(Encoded { value: pv, exactness })
			}
			Self::F32 => {
				let f = value.to_f32().filter(|f| f.is_finite()).ok_or(EncodeError::NotFinite)?;
				let pv = PhysicalValue::F32(f);
				let exactness = measure(value, &pv.to_logical());
				Ok(Encoded { value: pv, exactness })
			}
			Self::ScaledI64 { scale } => {
				let shifted = (value * pow10(scale)).round(0);
				let mantissa = shifted.to_i64().ok_or(EncodeError::Overflow)?;
				let pv = PhysicalValue::ScaledI64 { mantissa, scale };
				let exactness = measure(value, &pv.to_logical());
				Ok(Encoded { value: pv, exactness })
			}
			Self::ScaledI128 { scale } => {
				let shifted = (value * pow10(scale)).round(0);
				let mantissa = shifted.to_i128().ok_or(EncodeError::Overflow)?;
				let pv = PhysicalValue::ScaledI128 { mantissa, scale };
				let exactness = measure(value, &pv.to_logical());
				Ok(Encoded { value: pv, exactness })
			}
			Self::Decimal128 => {
				let (bigint, scale) = value.as_bigint_and_exponent();
				let (mantissa, scale) = fit_i128(bigint, scale);
				let pv = PhysicalValue::Decimal128 { mantissa, scale };
				let exactness = measure(value, &pv.to_logical());
				Ok(Encoded { value: pv, exactness })
			}
			Self::BigDecimalText => {
				let pv = PhysicalValue::BigDecimalText(value.to_plain_string());
				Ok(Encoded { value: pv, exactness: Exactness::Exact })
			}
		}
	}

	/// The encoding's stable wire/identifier name (e.g. for schema serialization
	/// and debug output).
	#[must_use]
	pub const fn name(self) -> &'static str {
		match self {
			Self::F64 => "f64",
			Self::F32 => "f32",
			Self::ScaledI64 { .. } => "scaled_i64",
			Self::ScaledI128 { .. } => "scaled_i128",
			Self::Decimal128 => "decimal128",
			Self::BigDecimalText => "bigdecimal_text",
		}
	}
}

impl PhysicalValue {
	/// Reconstruct the logical `BigDecimal` this physical value represents.
	///
	/// Always succeeds: every encoded value produced by
	/// [`PhysicalType::encode`] has a well-defined logical reconstruction. For
	/// the (encode-rejected) degenerate cases — a non-finite `f64` or an
	/// unparseable text — it yields zero rather than panicking.
	#[must_use]
	pub fn to_logical(&self) -> BigDecimal {
		match self {
			Self::F64(f) => BigDecimal::from_f64(*f).unwrap_or_default(),
			Self::F32(f) => BigDecimal::from_f32(*f).unwrap_or_default(),
			Self::ScaledI64 { mantissa, scale } => BigDecimal::new(BigInt::from(*mantissa), i64::from(*scale)),
			Self::ScaledI128 { mantissa, scale } => BigDecimal::new(BigInt::from(*mantissa), i64::from(*scale)),
			Self::Decimal128 { mantissa, scale } => BigDecimal::new(BigInt::from(*mantissa), *scale),
			Self::BigDecimalText(s) => s.parse().unwrap_or_default(),
		}
	}
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use super::*;

	fn bd(s: &str) -> BigDecimal {
		BigDecimal::from_str(s).expect("test literal parses")
	}

	#[test]
	fn f64_exact_for_binary_representable() {
		// 0.5, 2.25 and 128 are all exact in binary64.
		for lit in ["0.5", "2.25", "128", "-0.25"] {
			let v = bd(lit);
			let enc = PhysicalType::F64.encode(&v).expect("encodes");
			assert!(enc.exactness.is_exact(), "{lit} should encode exactly");
			assert_eq!(enc.value.to_logical(), v, "{lit} round-trips");
		}
	}

	#[test]
	fn f64_lossy_for_decimal_fraction() {
		let v = bd("0.1");
		let enc = PhysicalType::F64.encode(&v).expect("encodes");
		match enc.exactness {
			Exactness::Lossy { abs_error } => {
				assert!(abs_error > bd("0"), "error is positive");
				assert!(abs_error < bd("0.0000001"), "but tiny: {abs_error}");
			}
			Exactness::Exact => panic!("0.1 is not exact in binary64"),
		}
	}

	#[test]
	fn f64_not_finite_for_huge_magnitude() {
		// 10^400 has no finite binary64 representation.
		let mut s = String::from("1");
		s.push_str(&"0".repeat(400));
		let v = bd(&s);
		assert_eq!(PhysicalType::F64.encode(&v), Err(EncodeError::NotFinite));
	}

	#[test]
	fn scaled_i64_exact_within_scale() {
		let v = bd("12.34");
		let enc = PhysicalType::ScaledI64 { scale: 2 }.encode(&v).expect("encodes");
		assert_eq!(enc.value, PhysicalValue::ScaledI64 { mantissa: 1234, scale: 2 });
		assert!(enc.exactness.is_exact());
		assert_eq!(enc.value.to_logical(), v);
	}

	#[test]
	fn scaled_i64_lossy_beyond_scale() {
		// 12.345 at scale 2 rounds to 12.34 or 12.35; either way ~0.005 error.
		let v = bd("12.345");
		let enc = PhysicalType::ScaledI64 { scale: 2 }.encode(&v).expect("encodes");
		match enc.exactness {
			Exactness::Lossy { abs_error } => {
				assert!(abs_error <= bd("0.005"), "rounding error bounded: {abs_error}");
				assert!(abs_error > bd("0"));
			}
			Exactness::Exact => panic!("three fractional digits cannot be exact at scale 2"),
		}
	}

	#[test]
	fn scaled_i64_negative_round_trip() {
		let v = bd("-7.5");
		let enc = PhysicalType::ScaledI64 { scale: 1 }.encode(&v).expect("encodes");
		assert_eq!(enc.value, PhysicalValue::ScaledI64 { mantissa: -75, scale: 1 });
		assert!(enc.exactness.is_exact());
		assert_eq!(enc.value.to_logical(), v);
	}

	#[test]
	fn scaled_i64_overflow_reported() {
		// 10^30 shifted by scale 2 overflows i64 (~9.2 * 10^18).
		let mut s = String::from("1");
		s.push_str(&"0".repeat(30));
		let v = bd(&s);
		assert_eq!(PhysicalType::ScaledI64 { scale: 2 }.encode(&v), Err(EncodeError::Overflow));
	}

	#[test]
	fn bigdecimal_text_is_always_exact() {
		// A value with far more digits than f64 or i64 could ever hold.
		let v = bd("123456789012345678901234567890.123456789012345678901234567890");
		let enc = PhysicalType::BigDecimalText.encode(&v).expect("encodes");
		assert!(enc.exactness.is_exact());
		assert_eq!(enc.value.to_logical(), v);
	}

	#[test]
	fn f32_exact_and_lossy() {
		// 0.5 is exact in binary32; 0.1 is not.
		let exact = PhysicalType::F32.encode(&bd("0.5")).expect("encodes");
		assert!(exact.exactness.is_exact());
		assert_eq!(exact.value, PhysicalValue::F32(0.5));

		let lossy = PhysicalType::F32.encode(&bd("0.1")).expect("encodes");
		assert!(!lossy.exactness.is_exact(), "0.1 is not exact in binary32");
		// f32 loses more than f64, so the error is larger than the f64 case
		// but still small in absolute terms.
		assert!(lossy.exactness.abs_error() < bd("0.0001"));
	}

	#[test]
	fn f32_not_finite_above_range() {
		// ~1e40 exceeds the binary32 range (~3.4e38) but is fine for f64.
		let mut s = String::from("1");
		s.push_str(&"0".repeat(40));
		let v = bd(&s);
		assert_eq!(PhysicalType::F32.encode(&v), Err(EncodeError::NotFinite));
		assert!(PhysicalType::F64.encode(&v).is_ok(), "f64 still holds 1e40");
	}

	#[test]
	fn scaled_i128_holds_values_beyond_i64() {
		// 10^25 shifted by scale 2 = 10^27, far past i64 but inside i128.
		let mut s = String::from("1");
		s.push_str(&"0".repeat(25));
		let v = bd(&s);
		assert_eq!(PhysicalType::ScaledI64 { scale: 2 }.encode(&v), Err(EncodeError::Overflow));
		let enc = PhysicalType::ScaledI128 { scale: 2 }.encode(&v).expect("encodes");
		assert!(enc.exactness.is_exact());
		assert_eq!(enc.value.to_logical(), v);
	}

	#[test]
	fn scaled_i128_overflow_reported() {
		// 10^40 shifted by scale 2 overflows even i128 (~1.7 * 10^38).
		let mut s = String::from("1");
		s.push_str(&"0".repeat(40));
		let v = bd(&s);
		assert_eq!(PhysicalType::ScaledI128 { scale: 2 }.encode(&v), Err(EncodeError::Overflow));
	}

	#[test]
	fn decimal128_exact_within_38_digits() {
		// A 30-digit decimal with a fractional part — fits i128 (38 digits) exactly.
		let v = bd("123456789012345678901234.567890");
		let enc = PhysicalType::Decimal128.encode(&v).expect("encodes");
		assert!(enc.exactness.is_exact(), "30 significant digits fit i128");
		assert_eq!(enc.value.to_logical(), v);
	}

	#[test]
	fn decimal128_lossy_but_never_errors_beyond_38_digits() {
		// 50 significant digits cannot fit i128: low-order digits are shed,
		// reported Lossy, and the call still succeeds.
		let v = bd("12345678901234567890123456789012345678901234567890");
		let enc = PhysicalType::Decimal128.encode(&v).expect("never errors");
		match enc.exactness {
			Exactness::Lossy { abs_error } => assert!(abs_error > bd("0")),
			Exactness::Exact => panic!("50 digits cannot be exact in i128"),
		}
		// The reconstruction stays the same order of magnitude as the original.
		let decoded = enc.value.to_logical();
		assert!(decoded > bd("1e49") && decoded < bd("1.3e49"), "magnitude preserved: {decoded}");
	}

	#[test]
	fn decimal128_large_magnitude_round_trips() {
		// A whole number above i64 round-trips exactly via the per-value scale.
		let v = bd("9223372036854775808000"); // (i64::MAX + 1) * 1000
		let enc = PhysicalType::Decimal128.encode(&v).expect("encodes");
		assert!(enc.exactness.is_exact());
		assert_eq!(enc.value.to_logical(), v);
	}

	#[test]
	fn profiles_describe_each_encoding() {
		let f64 = PhysicalType::F64.profile();
		assert_eq!(f64.fixed_width_bytes, Some(8));
		assert!(f64.hot_path_eligible);
		assert!(!f64.always_lossless);

		assert_eq!(PhysicalType::F32.profile().fixed_width_bytes, Some(4));
		assert_eq!(PhysicalType::ScaledI64 { scale: 2 }.profile().fixed_width_bytes, Some(8));
		assert_eq!(PhysicalType::ScaledI128 { scale: 2 }.profile().fixed_width_bytes, Some(16));
		assert_eq!(PhysicalType::Decimal128.profile().fixed_width_bytes, Some(24));

		let text = PhysicalType::BigDecimalText.profile();
		assert_eq!(text.fixed_width_bytes, None);
		assert!(!text.hot_path_eligible, "text cannot live on the fixed-width hot path");
		assert!(text.always_lossless, "text is the only universally lossless encoding");
		assert_eq!(text.name, "bigdecimal_text");
	}

	#[test]
	fn names_are_stable() {
		assert_eq!(PhysicalType::F64.name(), "f64");
		assert_eq!(PhysicalType::F32.name(), "f32");
		assert_eq!(PhysicalType::ScaledI64 { scale: 3 }.name(), "scaled_i64");
		assert_eq!(PhysicalType::ScaledI128 { scale: 3 }.name(), "scaled_i128");
		assert_eq!(PhysicalType::Decimal128.name(), "decimal128");
		assert_eq!(PhysicalType::BigDecimalText.name(), "bigdecimal_text");
	}
}
