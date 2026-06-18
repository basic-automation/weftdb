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
//! - [`PhysicalType`] — the schema-declared encoding (this slice: [`F64`],
//!   [`ScaledI64`], [`BigDecimalText`]; the remaining encodings named in Phase
//!   4.1 — `F32`, `ScaledI128`, `Decimal128` — land in following slices).
//! - [`PhysicalValue`] — one value held in its encoded physical form.
//! - [`PhysicalType::encode`] — `BigDecimal` -> [`Encoded`] (`PhysicalValue` +
//!   [`Exactness`]).
//! - [`PhysicalValue::to_logical`] — the always-available inverse, reconstructing
//!   the `BigDecimal` the encoding represents.
//!
//! [`F64`]: PhysicalType::F64
//! [`ScaledI64`]: PhysicalType::ScaledI64
//! [`BigDecimalText`]: PhysicalType::BigDecimalText
//!
//! ## Vendor-neutrality
//!
//! Per the workspace hard constraints this crate carries no vendor-specific
//! dependencies: it is a pure numeric encoding layer over `bigdecimal`, usable by
//! the store (`database`), the server (`dsp-server`), the harness (`dsp-bench`),
//! and the forthcoming Storage v2 columnar segments alike.

#![warn(clippy::pedantic, clippy::nursery, clippy::all)]

use bigdecimal::{
	num_bigint::{BigInt, Sign}, BigDecimal, FromPrimitive, ToPrimitive
};
use serde::{Deserialize, Serialize};

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
	/// Fixed-point: the value is stored as a signed 64-bit mantissa interpreted
	/// as `mantissa * 10^(-scale)`. Exact for any value whose magnitude fits an
	/// `i64` once shifted by `scale` decimal places and which has no more than
	/// `scale` fractional digits — the common case for sensor data with a known
	/// number of decimals. Integer-fast, GPU/SIMD-friendly.
	ScaledI64 {
		/// Number of fractional decimal digits the mantissa carries.
		scale: u8,
	},
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
	/// A [`PhysicalType::ScaledI64`]-encoded value: `mantissa * 10^(-scale)`.
	ScaledI64 {
		/// The fixed-point mantissa.
		mantissa: i64,
		/// The fractional-digit scale shared by the aspect.
		scale: u8,
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

impl PhysicalType {
	/// Encode a logical `BigDecimal` into this physical type, reporting the
	/// [`Exactness`] of the conversion.
	///
	/// # Errors
	///
	/// Returns [`EncodeError::Overflow`] if the value does not fit a
	/// [`ScaledI64`](Self::ScaledI64) mantissa, or [`EncodeError::NotFinite`] if
	/// it has no finite [`F64`](Self::F64) representation. Lossy-but-representable
	/// conversions are **not** errors — they succeed with
	/// [`Exactness::Lossy`] so the caller decides whether the residual is
	/// acceptable.
	pub fn encode(self, value: &BigDecimal) -> Result<Encoded, EncodeError> {
		match self {
			Self::F64 => {
				let f = value.to_f64().filter(|f| f.is_finite()).ok_or(EncodeError::NotFinite)?;
				let pv = PhysicalValue::F64(f);
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
			Self::ScaledI64 { .. } => "scaled_i64",
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
			Self::ScaledI64 { mantissa, scale } => BigDecimal::new(BigInt::from(*mantissa), i64::from(*scale)),
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
	fn names_are_stable() {
		assert_eq!(PhysicalType::F64.name(), "f64");
		assert_eq!(PhysicalType::ScaledI64 { scale: 3 }.name(), "scaled_i64");
		assert_eq!(PhysicalType::BigDecimalText.name(), "bigdecimal_text");
	}
}
