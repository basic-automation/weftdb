//! Columnar batch encoding.
//!
//! Storage v2 (roadmap Phase 4.3) stores each aspect as typed columnar segments,
//! and DSP-Bench's headline cost metric is **storage bytes per point**. Both want
//! to encode a *whole column* of logical `BigDecimal` values under one declared
//! [`PhysicalType`] in a single pass, learn whether the encoding was lossless for
//! every value (and if not, the worst per-value error), and estimate the byte
//! footprint. This module is that batch entry point on top of the per-value
//! [`PhysicalType::encode`].
//!
//! A column is all-or-nothing on *representability*: if any value cannot be
//! encoded at all (an [`EncodeError`] — `Overflow`/`NotFinite`), the whole column
//! fails with the offending index, because a segment cannot mix encodings.
//! *Lossy-but-representable* values do not fail; they are counted and their worst
//! error is surfaced for the caller to gate against a schema-level bound.

use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};

use crate::{EncodeError, Exactness, PhysicalType, PhysicalValue};

/// A column of logical values encoded under a single [`PhysicalType`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnEncoding {
	/// The encoding every value in [`values`](Self::values) was produced under.
	pub physical_type: PhysicalType,
	/// The encoded values, in input order.
	pub values: Vec<PhysicalValue>,
	/// How many values encoded [`Exactness::Lossy`] (zero ⇒ the column is exact).
	pub lossy_count: usize,
	/// The largest per-value absolute reconstruction error across the column
	/// (zero when [`is_exact`](Self::is_exact)).
	pub max_abs_error: BigDecimal,
}

impl ColumnEncoding {
	/// `true` iff every value reconstructs exactly.
	#[must_use]
	pub const fn is_exact(&self) -> bool {
		self.lossy_count == 0
	}

	/// Number of values in the column.
	#[must_use]
	pub const fn len(&self) -> usize {
		self.values.len()
	}

	/// `true` iff the column holds no values.
	#[must_use]
	pub const fn is_empty(&self) -> bool {
		self.values.is_empty()
	}

	/// Estimated storage footprint in bytes for the value column.
	///
	/// For fixed-width encodings this is `len * width`; for the variable-width
	/// [`PhysicalType::BigDecimalText`] it sums the UTF-8 byte length of each
	/// stored decimal string. (Timestamp/quality columns and segment framing are
	/// out of scope — this is the value column alone, the dominant term in
	/// bytes/point.)
	#[must_use]
	pub fn estimated_bytes(&self) -> usize {
		self.physical_type.profile().fixed_width_bytes.map_or_else(
			|| {
				self.values
					.iter()
					.map(|v| match v {
						PhysicalValue::BigDecimalText(s) => s.len(),
						_ => 0,
					})
					.sum()
			},
			|width| self.values.len() * width,
		)
	}

	/// Reconstruct the logical `BigDecimal` column.
	#[must_use]
	pub fn decode(&self) -> Vec<BigDecimal> {
		self.values.iter().map(PhysicalValue::to_logical).collect()
	}
}

/// A value at a known position in a column could not be encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnEncodeError {
	/// Index of the offending value in the input slice.
	pub index: usize,
	/// Why that value could not be encoded.
	pub source: EncodeError,
}

impl std::fmt::Display for ColumnEncodeError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "value at index {} could not be encoded: {}", self.index, self.source)
	}
}

impl std::error::Error for ColumnEncodeError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(&self.source)
	}
}

/// Encode a whole column of logical values under one [`PhysicalType`] in a single
/// pass, aggregating exactness across the column.
///
/// # Errors
///
/// Returns [`ColumnEncodeError`] for the first value that cannot be represented
/// at all ([`EncodeError::Overflow`] / [`EncodeError::NotFinite`]) — a column
/// cannot mix encodings, so one unrepresentable value disqualifies the whole
/// column for this `physical_type`. Lossy-but-representable values never fail;
/// they are tallied into [`ColumnEncoding::lossy_count`] and
/// [`ColumnEncoding::max_abs_error`].
pub fn encode_column(physical_type: PhysicalType, values: &[BigDecimal]) -> Result<ColumnEncoding, ColumnEncodeError> {
	let mut out = Vec::with_capacity(values.len());
	let mut lossy_count = 0;
	let mut max_abs_error = BigDecimal::from(0);
	for (index, value) in values.iter().enumerate() {
		let encoded = physical_type.encode(value).map_err(|source| ColumnEncodeError { index, source })?;
		if let Exactness::Lossy { abs_error } = encoded.exactness {
			lossy_count += 1;
			if abs_error > max_abs_error {
				max_abs_error = abs_error;
			}
		}
		out.push(encoded.value);
	}
	Ok(ColumnEncoding { physical_type, values: out, lossy_count, max_abs_error })
}

/// The number of fractional decimal digits needed to hold every value exactly —
/// the minimal shared `scale` for a `ScaledI*` encoding of the column.
fn max_fractional_digits(values: &[BigDecimal]) -> u8 {
	values.iter()
		.map(|v| {
			let (_mantissa, exponent) = v.normalized().as_bigint_and_exponent();
			u8::try_from(exponent.max(0)).unwrap_or(u8::MAX)
		})
		.max()
		.unwrap_or(0)
}

/// **Advisory** encoding selection: the narrowest hot-path [`PhysicalType`] that
/// encodes this whole column within `max_abs_error`.
///
/// This implements the roadmap's "execute each series in the fastest *safe*
/// physical encoding" intent (hard constraint #4) as a recommendation: schema
/// still *declares* the encoding, but tooling, ingest heuristics, and bench
/// reports want a principled suggestion plus the realized bytes/point. Candidates
/// are tried cheapest-first — `F32` (4 B), `F64` (8 B), `ScaledI64` (8 B) at the
/// column's minimal exact scale, `ScaledI128` (16 B), `Decimal128` (24 B) — and
/// the first whose [`ColumnEncoding::max_abs_error`] is within the tolerance
/// wins. [`PhysicalType::BigDecimalText`] is the always-exact backstop, so a
/// result is guaranteed (an empty column trivially picks the cheapest, `F32`).
///
/// Returns the chosen [`ColumnEncoding`] directly — both the decision and the
/// encoded column — so the caller need not re-encode.
#[must_use]
pub fn recommend_encoding(values: &[BigDecimal], max_abs_error: &BigDecimal) -> ColumnEncoding {
	let scale = max_fractional_digits(values);
	let candidates = [PhysicalType::F32, PhysicalType::F64, PhysicalType::ScaledI64 { scale }, PhysicalType::ScaledI128 { scale }, PhysicalType::Decimal128, PhysicalType::BigDecimalText];
	for physical_type in candidates {
		if let Ok(encoding) = encode_column(physical_type, values) {
			if &encoding.max_abs_error <= max_abs_error {
				return encoding;
			}
		}
	}
	// Unreachable: BigDecimalText always encodes exactly (error 0 <= tolerance).
	encode_column(PhysicalType::BigDecimalText, values).unwrap_or_else(|_| ColumnEncoding { physical_type: PhysicalType::BigDecimalText, values: Vec::new(), lossy_count: 0, max_abs_error: BigDecimal::from(0) })
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use super::*;

	fn col(lits: &[&str]) -> Vec<BigDecimal> {
		lits.iter().map(|s| BigDecimal::from_str(s).expect("parses")).collect()
	}

	#[test]
	fn scaled_i64_column_exact_and_sized() {
		let values = col(&["1.25", "2.50", "-3.75", "0.00"]);
		let enc = encode_column(PhysicalType::ScaledI64 { scale: 2 }, &values).expect("encodes");
		assert!(enc.is_exact());
		assert_eq!(enc.len(), 4);
		assert!(!enc.is_empty());
		assert_eq!(enc.estimated_bytes(), 4 * 8);
		assert_eq!(enc.decode(), values);
	}

	#[test]
	fn f64_column_tracks_worst_error() {
		// 0.5 is exact; 0.1 and 0.3 are not — the column is lossy and reports
		// the largest per-value error.
		let values = col(&["0.5", "0.1", "0.3"]);
		let enc = encode_column(PhysicalType::F64, &values).expect("encodes");
		assert!(!enc.is_exact());
		assert_eq!(enc.lossy_count, 2);
		let zero = BigDecimal::from(0);
		assert!(enc.max_abs_error > zero);
		assert!(enc.max_abs_error < BigDecimal::from_str("0.0000001").unwrap());
	}

	#[test]
	fn overflow_reports_offending_index() {
		let mut huge = String::from("1");
		huge.push_str(&"0".repeat(30));
		let values = col(&["1.0", "2.0", &huge, "4.0"]);
		let err = encode_column(PhysicalType::ScaledI64 { scale: 2 }, &values).expect_err("overflows");
		assert_eq!(err.index, 2);
		assert_eq!(err.source, EncodeError::Overflow);
	}

	#[test]
	fn text_column_sizes_by_string_bytes() {
		let values = col(&["1.5", "12345.678"]);
		let enc = encode_column(PhysicalType::BigDecimalText, &values).expect("encodes");
		assert!(enc.is_exact());
		// "1.5" (3) + "12345.678" (9) = 12 bytes.
		assert_eq!(enc.estimated_bytes(), 12);
		assert_eq!(enc.decode(), values);
	}

	#[test]
	fn empty_column_is_exact_and_zero_bytes() {
		let enc = encode_column(PhysicalType::F64, &[]).expect("encodes");
		assert!(enc.is_exact());
		assert!(enc.is_empty());
		assert_eq!(enc.estimated_bytes(), 0);
	}

	#[test]
	fn recommend_prefers_cheapest_exact_for_small_integers() {
		// 1, 2, 3 are all exact in binary32 — the cheapest hot-path encoding wins.
		let values = col(&["1", "2", "3"]);
		let rec = recommend_encoding(&values, &BigDecimal::from(0));
		assert_eq!(rec.physical_type, PhysicalType::F32);
		assert!(rec.is_exact());
	}

	#[test]
	fn recommend_falls_to_scaled_int_when_floats_are_lossy() {
		// 0.1/0.2/0.3 are not binary-exact, so an exact requirement skips F32/F64
		// and lands on ScaledI64 at the column's minimal scale (1).
		let values = col(&["0.1", "0.2", "0.3"]);
		let rec = recommend_encoding(&values, &BigDecimal::from(0));
		assert_eq!(rec.physical_type, PhysicalType::ScaledI64 { scale: 1 });
		assert!(rec.is_exact());
		assert_eq!(rec.decode(), values);
	}

	#[test]
	fn recommend_uses_a_lossy_float_within_tolerance() {
		// With a generous tolerance the same column takes the cheapest encoding
		// that fits — lossy F32.
		let values = col(&["0.1", "0.2", "0.3"]);
		let rec = recommend_encoding(&values, &BigDecimal::from_str("0.01").unwrap());
		assert_eq!(rec.physical_type, PhysicalType::F32);
		assert!(!rec.is_exact());
	}

	#[test]
	fn recommend_backstops_to_text_for_exact_high_precision() {
		// A 40-digit value with a fractional part: ScaledI128 overflows,
		// Decimal128 is lossy, so an exact requirement falls to BigDecimalText.
		let mut whole = String::from("1");
		whole.push_str(&"0".repeat(39));
		let big = format!("{whole}.5");
		let values = col(&[&big]);
		let rec = recommend_encoding(&values, &BigDecimal::from(0));
		assert_eq!(rec.physical_type, PhysicalType::BigDecimalText);
		assert!(rec.is_exact());
		assert_eq!(rec.decode(), values);
	}

	#[test]
	fn recommend_on_empty_column_picks_cheapest() {
		let rec = recommend_encoding(&[], &BigDecimal::from(0));
		assert_eq!(rec.physical_type, PhysicalType::F32);
		assert!(rec.is_empty());
	}
}
