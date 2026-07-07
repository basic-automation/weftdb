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

	/// The **realized** on-disk payload size in bytes — the exact number of bytes the
	/// `.dspseg` value block writes for these values (the codec each variant uses in
	/// `dspseg::write_physical_value`), excluding the column header.
	///
	/// This differs from [`estimated_bytes`](Self::estimated_bytes), which reports the
	/// naive fixed-width figure (`len * width`). For a `ScaledI64` column the mantissas
	/// are written as zig-zag varints, so small-magnitude values cost one byte rather
	/// than eight — [`estimated_bytes`](Self::estimated_bytes) over-reports the stored
	/// size (it *undersells* DSP's realized bytes/point). This method is the accurate
	/// figure, matching what a sealed segment actually occupies:
	///
	/// - `F64` → 8, `F32` → 4, `ScaledI128` → 16 (fixed width, so it agrees with the
	///   naive estimate);
    /// - `ScaledI64` → the zig-zag-varint width of each mantissa (1..=10 bytes);
	/// - `Decimal128` → 16 for the significand + the zig-zag-varint width of the
	///   per-value scale;
	/// - `BigDecimalText` → the UTF-8 length plus its unsigned-varint length prefix.
	#[must_use]
	pub fn serialized_bytes(&self) -> usize {
		use crate::timestamp::{uvarint_len, zigzag_varint_len};
		self.values
			.iter()
			.map(|v| match v {
				PhysicalValue::F64(_) => 8,
				PhysicalValue::F32(_) => 4,
				PhysicalValue::ScaledI64 { mantissa, .. } => zigzag_varint_len(*mantissa),
				PhysicalValue::ScaledI128 { .. } => 16,
				PhysicalValue::Decimal128 { scale, .. } => 16 + zigzag_varint_len(*scale),
				PhysicalValue::BigDecimalText(s) => uvarint_len(s.len() as u64) + s.len(),
			})
			.sum()
	}

	/// The `ScaledI64` mantissas of this column, in input order — `None` for any
	/// other physical type.
	///
	/// Bit-packing the value column is only defined for the scaled-integer encoding,
	/// whose payload is a homogeneous signed-`i64` stream (the floats are IEEE byte
	/// patterns, the wide integers are full-width, `BigDecimalText` is UTF-8). A
	/// `ScaledI64` column is guaranteed to hold only `ScaledI64` values, so the map
	/// is total.
	#[must_use]
	pub fn scaled_i64_mantissas(&self) -> Option<Vec<i64>> {
		if !matches!(self.physical_type, PhysicalType::ScaledI64 { .. }) {
			return None;
		}
		Some(
			self.values
				.iter()
				.map(|v| match v {
					PhysicalValue::ScaledI64 { mantissa, .. } => *mantissa,
					// Unreachable: a ScaledI64 column holds only ScaledI64 values.
					_ => 0,
				})
				.collect(),
		)
	}

	/// The realized footprint in bytes of the **fixed-width bit-packed** value codec
	/// for a `ScaledI64` column — a one-byte width selector plus the packed mantissa
	/// stream ([`crate::timestamp::bitpack_bytes`], which already counts the width
	/// byte). `None` for any other physical type (bit-packing is only defined for the
	/// scaled-integer payload).
	///
	/// This is the value-column analogue of the timestamp bit-pack codec: a regular
	/// or small-jitter scaled-integer series (mantissas differing by only a few bits)
	/// packs to `~len * width / 8` bytes, well under the per-value zig-zag varint's
	/// one-byte-per-value floor that [`serialized_bytes`](Self::serialized_bytes)
	/// reports. Compare the two to pick the smaller codec per column.
	#[must_use]
	pub fn bitpack_value_bytes(&self) -> Option<usize> {
		self.scaled_i64_mantissas().map(|m| crate::timestamp::bitpack_bytes(&m))
	}

	/// The smallest realized value-codec footprint for this column — the figure a
	/// per-column codec selector *would* write once the bit-pack codec is realized on
	/// disk. For a `ScaledI64` column this is `min(varint, bit-pack)`; for every
	/// other physical type it is exactly [`serialized_bytes`](Self::serialized_bytes)
	/// (the only codec defined for that payload).
	///
	/// Advisory today (the `.dspseg` value block still writes the per-value varint):
	/// this is the estimate half of the codec, mirroring how the Gorilla timestamp
	/// codec first landed its `gorilla_bytes` estimate before the on-disk selector.
	#[must_use]
	pub fn best_serialized_bytes(&self) -> usize {
		let varint = self.serialized_bytes();
		self.bitpack_value_bytes().map_or(varint, |bitpack| bitpack.min(varint))
	}

	/// The name of the value codec [`best_serialized_bytes`](Self::best_serialized_bytes)
	/// would select — `"scaled_bitpack"` when bit-packing is strictly smaller than the
	/// per-value varint for a `ScaledI64` column, otherwise `"varint"` (the general
	/// per-value payload, and the only codec for the non-scaled types). Ties keep the
	/// varint codec (no random-access penalty for equal bytes).
	#[must_use]
	pub fn best_value_codec(&self) -> &'static str {
		match self.bitpack_value_bytes() {
			Some(bitpack) if bitpack < self.serialized_bytes() => "scaled_bitpack",
			_ => "varint",
		}
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

	#[test]
	fn scaled_i64_mantissas_only_for_scaled_columns() {
		let values = col(&["1.25", "2.50", "-3.75"]);
		let scaled = encode_column(PhysicalType::ScaledI64 { scale: 2 }, &values).expect("encodes");
		// scale 2 ⇒ mantissas are the values * 100.
		assert_eq!(scaled.scaled_i64_mantissas(), Some(vec![125, 250, -375]));
		// Any other physical type has no bit-packable mantissa stream.
		let floats = encode_column(PhysicalType::F64, &values).expect("encodes");
		assert_eq!(floats.scaled_i64_mantissas(), None);
		assert_eq!(floats.bitpack_value_bytes(), None);
		// …so its best codec is the varint payload unchanged.
		assert_eq!(floats.best_serialized_bytes(), floats.serialized_bytes());
		assert_eq!(floats.best_value_codec(), "varint");
	}

	#[test]
	fn bitpack_value_codec_beats_varint_on_a_regular_scaled_stream() {
		// A long, slowly-rising scaled-int series: every mantissa is small-magnitude
		// (a few bits), so fixed-width bit-packing crushes the one-byte-per-value
		// varint floor. Values 0.00, 0.01, … 0.63 → mantissas 0..=63 (≤ 7 bits).
		let lits: Vec<String> = (0..64).map(|i| format!("{}.{:02}", i / 100, i % 100)).collect();
		let refs: Vec<&str> = lits.iter().map(String::as_str).collect();
		let values = col(&refs);
		let enc = encode_column(PhysicalType::ScaledI64 { scale: 2 }, &values).expect("encodes");
		let varint = enc.serialized_bytes();
		let bitpack = enc.bitpack_value_bytes().expect("scaled column bit-packs");
		assert!(bitpack < varint, "bit-pack {bitpack} must beat varint {varint} on a small-mantissa stream");
		assert_eq!(enc.best_serialized_bytes(), bitpack);
		assert_eq!(enc.best_value_codec(), "scaled_bitpack");
	}

	#[test]
	fn bitpack_value_codec_loses_to_varint_on_wide_sparse_mantissas() {
		// A column of large-magnitude mantissas: fixed-width bit-packing must pay the
		// widest value's bit-width for every row, so the per-value varint (which sizes
		// each value independently) wins. The selector must fall back to varint.
		let values = col(&["1", "1000000000", "2"]);
		let enc = encode_column(PhysicalType::ScaledI64 { scale: 0 }, &values).expect("encodes");
		let varint = enc.serialized_bytes();
		let bitpack = enc.bitpack_value_bytes().expect("scaled column bit-packs");
		assert!(bitpack > varint, "wide-sparse bit-pack {bitpack} must lose to varint {varint}");
		assert_eq!(enc.best_serialized_bytes(), varint);
		assert_eq!(enc.best_value_codec(), "varint");
	}

	#[test]
	fn bitpack_value_codec_round_trips_the_mantissas() {
		// The realized codec reuses the timestamp bit-pack primitives over the value
		// column's mantissa stream — prove that round-trip is exact before the on-disk
		// wiring depends on it.
		use crate::timestamp::{bitpack_decode, bitpack_encode};
		let values = col(&["1.25", "2.50", "-3.75", "0.00", "12.34"]);
		let enc = encode_column(PhysicalType::ScaledI64 { scale: 2 }, &values).expect("encodes");
		let mantissas = enc.scaled_i64_mantissas().expect("scaled column");
		let (width, packed) = bitpack_encode(&mantissas);
		assert_eq!(bitpack_decode(width, &packed, mantissas.len()), mantissas);
	}
}
