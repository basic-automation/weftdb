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

	/// The realized footprint in bytes of the **per-block adaptive (blocked) bit-packed**
	/// value codec for a `ScaledI64` column — the per-block width-header + packed stream
	/// of [`crate::timestamp::blocked_bitpack_bytes`] at
	/// [`crate::timestamp::BLOCKED_BITPACK_BLOCK`]. `None` for any other physical type.
	///
	/// The value-column analogue of the timestamp per-block adaptive codec: where the
	/// single-width [`bitpack_value_bytes`](Self::bitpack_value_bytes) pays the column's
	/// widest mantissa for *every* value, this pays each block's own width, so a column
	/// mixing a quiet low-magnitude region with a burst of large mantissas packs the wide
	/// values into only the few blocks they span instead of widening the whole column.
	/// Compare all three (varint / global bit-pack / blocked) to pick the smallest.
	#[must_use]
	pub fn blocked_value_bytes(&self) -> Option<usize> {
		self.scaled_i64_mantissas().map(|m| crate::timestamp::blocked_bitpack_bytes(&m, crate::timestamp::BLOCKED_BITPACK_BLOCK))
	}

	/// The realized footprint in bytes of the **Frame-of-Reference (FOR) per-block** value
	/// codec for a `ScaledI64` column — the per-block reference-subtracted stream of
	/// [`crate::timestamp::for_bitpack_bytes`] at [`crate::timestamp::BLOCKED_BITPACK_BLOCK`].
	/// `None` for any other physical type.
	///
	/// Where [`blocked_value_bytes`](Self::blocked_value_bytes) zig-zags each mantissa and
	/// pays the block's *magnitude* width, FOR subtracts each block's minimum and packs the
	/// *unsigned* residual, so it pays only the block's *range* — the dominant regime for
	/// real value columns (mantissas clustered at a high base: a sensor reading near a fixed
	/// offset). Because the residual is unsigned, FOR also shaves ~1 bit/value off zig-zag
	/// packing on any all-non-negative column, so it wins broadly, not just in the clustered
	/// regime.
	///
	/// **Adopted (owner sign-off, Phase 6.1):** folded into
	/// [`best_value_codec`](Self::best_value_codec) and realized on disk as
	/// `VAL_CODEC_FOR`, together with the headline bytes/point flip to the realized figure.
	/// The timestamp-column FOR estimate stays advisory (dods are near-zero, FOR rarely
	/// wins there).
	#[must_use]
	pub fn for_value_bytes(&self) -> Option<usize> {
		self.scaled_i64_mantissas().map(|m| crate::timestamp::for_bitpack_bytes(&m, crate::timestamp::BLOCKED_BITPACK_BLOCK))
	}

	/// The advisory footprint of a **cascade** codec on a `ScaledI64` column: delta-transform
	/// the mantissas (first differences), *then* pack the differences with the smallest of the
	/// varint / bit-pack / per-block bit-pack / FOR / run-length codecs. `None` for any other
	/// physical type.
	///
	/// The shipped value codecs ([`best_value_codec`](Self::best_value_codec)) are all
	/// *single-level* — they pack the raw mantissas. A monotonically **trending** column (a
	/// counter, a monotone sensor) defeats them: FOR pays the whole run's range, bit-packing
	/// pays the widest mantissa. The delta transform turns that trend into a near-constant
	/// difference stream, which the same packers (or RLE, on a constant delta) then crush. This
	/// is the first slice of the roadmap's cascading-codec item (FOR→delta→bit-pack chains):
	/// an **advisory** estimate over the same first-difference transform as [`crate::encode_delta`]
	/// and the shipped packers — not a `.dspseg` codec (a composed on-disk pipeline is the
	/// residue). The
	/// first mantissa is the varint anchor; a single-value column has only that anchor.
	/// *(src: Vortex / `FastLanes` cascading compression — <https://vortex.dev/>)*
	#[must_use]
	pub fn delta_cascade_bytes(&self) -> Option<usize> {
		use crate::timestamp::{bitpack_bytes, blocked_bitpack_bytes, for_bitpack_bytes, rle_encode, rle_varint_bytes, zigzag_varint_bytes, zigzag_varint_len, BLOCKED_BITPACK_BLOCK};
		let mantissas = self.scaled_i64_mantissas()?;
		let Some((&first, rest)) = mantissas.split_first() else {
			return Some(0);
		};
		let anchor = zigzag_varint_len(first);
		if rest.is_empty() {
			return Some(anchor);
		}
		// First differences (the delta transform), wrapping so an extreme swing never panics.
		let mut deltas = Vec::with_capacity(rest.len());
		let mut prev = first;
		for &m in rest {
			deltas.push(m.wrapping_sub(prev));
			prev = m;
		}
		let packed = zigzag_varint_bytes(&deltas).min(bitpack_bytes(&deltas)).min(blocked_bitpack_bytes(&deltas, BLOCKED_BITPACK_BLOCK)).min(for_bitpack_bytes(&deltas, BLOCKED_BITPACK_BLOCK)).min(rle_varint_bytes(&rle_encode(&deltas)));
		Some(anchor + packed)
	}

	/// The raw `f64` values of this column, in input order — `None` for any other
	/// physical type.
	///
	/// The Gorilla XOR codec is defined only for the IEEE-754 `F64` payload (the scaled
	/// codecs target the integer mantissa stream, `BigDecimalText` is UTF-8). An `F64`
	/// column is guaranteed to hold only `F64` values, so the map is total.
	#[must_use]
	pub fn f64_values(&self) -> Option<Vec<f64>> {
		if !matches!(self.physical_type, PhysicalType::F64) {
			return None;
		}
		Some(
			self.values
				.iter()
				.map(|v| match v {
					PhysicalValue::F64(f) => *f,
					// Unreachable: an F64 column holds only F64 values.
					_ => 0.0,
				})
				.collect(),
		)
	}

	/// The realized footprint in bytes of the **Gorilla XOR** value codec for an `F64`
	/// column — [`crate::floatcodec::xor_f64_bytes`]. `None` for any other physical type
	/// (XOR compression is defined only for the raw IEEE-754 payload).
	///
	/// The `F64` analogue of the scaled-integer bit-pack codecs, and the one compression
	/// path for a *lossy* value column: where [`serialized_bytes`](Self::serialized_bytes)
	/// pays a flat 8 B for every float, the XOR codec stores only the meaningful bits
	/// between a value and its predecessor — a repeated value costs one bit, a
	/// slowly-varying series a handful. **Advisory only** (roadmap Phase 6.1): surfaced
	/// here and benchmarked against the raw `8 * len`, but not yet wired into the
	/// `.dspseg` writer or a codec selector (that is the adopt-or-drop slice, mirroring
	/// how the Gorilla-timestamp and FOR codecs were introduced advisory-first).
	#[must_use]
	pub fn gorilla_f64_bytes(&self) -> Option<usize> {
		self.f64_values().map(|f| crate::floatcodec::xor_f64_bytes(&f))
	}

	/// The footprint in bytes of the **smallest available** `f64` value codec for an `F64`
	/// column — [`crate::floatcodec::best_f64_bytes`], the min of the Gorilla XOR, Chimp
	/// XOR, and uncompressed `raw` baseline. `None` for any other physical type.
	///
	/// Where [`gorilla_f64_bytes`](Self::gorilla_f64_bytes) reports one codec, this reports
	/// what a selector would actually pick — never worse than the raw `8 * value_count` an
	/// `F64` block writes today. This is the figure a future on-disk f64 codec would realize;
	/// the paired [`best_f64_codec`](Self::best_f64_codec) names which codec wins. **Advisory**
	/// (roadmap Phase 6.1) — not yet wired into the `.dspseg` writer.
	#[must_use]
	pub fn best_f64_bytes(&self) -> Option<usize> {
		self.f64_values().map(|f| crate::floatcodec::best_f64_bytes(&f))
	}

	/// The name of the codec [`best_f64_bytes`](Self::best_f64_bytes) selects for an `F64`
	/// column — `"chimp128"`, `"chimp"`, `"gorilla"`, or `"raw"` (see
	/// [`crate::floatcodec::best_f64_codec`]). `None` for any other physical type.
	#[must_use]
	pub fn best_f64_codec(&self) -> Option<&'static str> {
		self.f64_values().map(|f| crate::floatcodec::best_f64_codec(&f).0)
	}

	/// The smallest realized value-codec footprint for this column — the figure the
	/// per-column codec selector writes on disk. For a `ScaledI64` column this is
	/// `min(varint, global bit-pack, per-block adaptive bit-pack, per-block FOR)`; for
	/// every other physical type it is exactly
	/// [`serialized_bytes`](Self::serialized_bytes) (the only codec defined for that
	/// payload).
	///
	/// All four `ScaledI64` codecs are realized on disk; the blocked and FOR codecs are
	/// chosen only when strictly smallest, so a regular/uniform column is byte-for-byte
	/// unchanged by their existence.
	#[must_use]
	pub fn best_serialized_bytes(&self) -> usize {
		let varint = self.serialized_bytes();
		let bitpack = self.bitpack_value_bytes().unwrap_or(varint);
		let blocked = self.blocked_value_bytes().unwrap_or(varint);
		let for_ = self.for_value_bytes().unwrap_or(varint);
		varint.min(bitpack).min(blocked).min(for_)
	}

	/// The name of the value codec [`best_serialized_bytes`](Self::best_serialized_bytes)
	/// selects — `"scaled_for"` when per-block Frame-of-Reference packing is strictly
	/// smallest for a `ScaledI64` column (clustered or all-non-negative mantissas),
	/// `"scaled_blocked"` when per-block adaptive bit-packing is strictly smallest (a
	/// mixed-magnitude mantissa stream), `"scaled_bitpack"` when global bit-packing is
	/// strictly smaller than the per-value varint, otherwise `"varint"` (the general
	/// per-value payload, and the only codec for the non-scaled types). Ties keep the
	/// simpler codec (`varint` > `scaled_bitpack` > `scaled_blocked` > `scaled_for`), so a
	/// uniform/single-block scaled series keeps the `scaled_bitpack` label. The single
	/// source of truth the `.dspseg` writer routes through.
	#[must_use]
	pub fn best_value_codec(&self) -> &'static str {
		let varint = self.serialized_bytes();
		let Some(bitpack) = self.bitpack_value_bytes() else {
			return "varint";
		};
		let blocked = self.blocked_value_bytes().unwrap_or(varint);
		let for_ = self.for_value_bytes().unwrap_or(varint);
		let best_zigzag = varint.min(bitpack).min(blocked);
		if for_ < best_zigzag {
			"scaled_for"
		} else if blocked < varint && blocked < bitpack {
			"scaled_blocked"
		} else if bitpack < varint {
			"scaled_bitpack"
		} else {
			"varint"
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
		// varint floor. Values -0.32 … 0.31 → mantissas -32..=31 (≤ 6 zig-zag bits,
		// symmetric around zero so a FOR reference buys nothing).
		let values: Vec<BigDecimal> = (0..64).map(|i| BigDecimal::new((i - 32).into(), 2)).collect();
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

	#[test]
	fn blocked_value_codec_beats_bitpack_on_a_mixed_magnitude_scaled_stream() {
		// A scaled-int column mixing a quiet zero-straddling region with one contiguous
		// sign-alternating burst of large mantissas: global bit-packing must pay the
		// burst's ~31-bit width for every row, per-block adaptive bit-packing confines it
		// to the blocks the burst spans, and a FOR reference is wasted (each block's
		// residual range equals its zig-zag magnitude, so FOR only adds the reference
		// varint). The blocked codec must be the strict winner.
		let lits: Vec<String> = (0..192)
			.map(|i| if (64..128).contains(&i) { format!("{}", (1_000_000_000_i64 + i) * if i % 2 == 0 { 1 } else { -1 }) } else { format!("{}", (i % 5) - 2) })
			.collect();
		let refs: Vec<&str> = lits.iter().map(String::as_str).collect();
		let values = col(&refs);
		let enc = encode_column(PhysicalType::ScaledI64 { scale: 0 }, &values).expect("encodes");
		let varint = enc.serialized_bytes();
		let bitpack = enc.bitpack_value_bytes().expect("scaled column bit-packs");
		let blocked = enc.blocked_value_bytes().expect("scaled column blocks");
		let for_bytes = enc.for_value_bytes().expect("scaled column FORs");
		assert!(blocked < bitpack, "blocked {blocked} must beat global bit-pack {bitpack}");
		assert!(blocked < varint, "blocked {blocked} must beat varint {varint}");
		assert!(blocked < for_bytes, "blocked {blocked} must beat FOR {for_bytes} on zero-straddling data");
		assert_eq!(enc.best_serialized_bytes(), blocked);
		assert_eq!(enc.best_value_codec(), "scaled_blocked");
	}

	#[test]
	fn blocked_value_codec_ties_bitpack_on_a_uniform_stream_keeping_the_simpler_label() {
		// A uniform small-mantissa zero-symmetric ramp: every block packs to the same
		// width, so blocked cannot beat global bit-pack — the selector keeps the simpler
		// `scaled_bitpack` label (blocked wins only on genuine mixed magnitude).
		let values: Vec<BigDecimal> = (0..64).map(|i| BigDecimal::new((i - 32).into(), 2)).collect();
		let enc = encode_column(PhysicalType::ScaledI64 { scale: 2 }, &values).expect("encodes");
		let bitpack = enc.bitpack_value_bytes().expect("scaled column bit-packs");
		let blocked = enc.blocked_value_bytes().expect("scaled column blocks");
		assert!(blocked >= bitpack, "blocked {blocked} must not beat global bit-pack {bitpack} on a uniform stream");
		assert_eq!(enc.best_value_codec(), "scaled_bitpack");
	}

	#[test]
	fn for_value_codec_wins_and_is_selected_on_a_clustered_high_base() {
		// A scaled-int column whose mantissas are all clustered at a high base (near 1e9 with
		// a small ±jitter) — the common real-sensor regime. Global and per-block bit-packing
		// both zig-zag the ~1e9 magnitude for every value; FOR subtracts each block's ~1e9
		// minimum and packs only the jitter's few bits + one reference varint per block.
		let lits: Vec<String> = (0..192).map(|i| format!("{}", 1_000_000_000_i64 + (i % 7))).collect();
		let refs: Vec<&str> = lits.iter().map(String::as_str).collect();
		let enc = encode_column(PhysicalType::ScaledI64 { scale: 0 }, &col(&refs)).expect("encodes");
		let blocked = enc.blocked_value_bytes().expect("scaled column blocks");
		let for_bytes = enc.for_value_bytes().expect("scaled column FORs");
		// Measured: for=90 B vs blocked=747 B / bitpack=745 B / varint=960 B — an 88%
		// reduction, the FOR-before-bit-packing win on clustered-high-base mantissas.
		assert!(for_bytes < blocked, "FOR {for_bytes} must beat blocked {blocked} on a clustered high base");
		assert!(for_bytes * 3 < blocked, "FOR {for_bytes} should be well under a third of blocked {blocked}");
		// FOR is ADOPTED (owner sign-off): the selector picks it when strictly smallest,
		// and the realized footprint is the FOR figure.
		assert_eq!(enc.best_value_codec(), "scaled_for");
		assert_eq!(enc.best_serialized_bytes(), for_bytes);
	}

	#[test]
	fn for_value_codec_ties_keep_the_simpler_codec() {
		// A single-block zero-symmetric ramp: FOR's residual width equals zig-zag's, so
		// its per-block reference varint means it cannot strictly win — the selector
		// keeps the simpler `scaled_bitpack` label. (On an all-non-negative ramp FOR
		// genuinely wins — zig-zag pays a sign bit FOR does not — so that regime
		// correctly selects `scaled_for` instead.)
		let values: Vec<BigDecimal> = (0..64).map(|i| BigDecimal::new((i - 32).into(), 0)).collect();
		let enc = encode_column(PhysicalType::ScaledI64 { scale: 0 }, &values).expect("encodes");
		let bitpack = enc.bitpack_value_bytes().expect("scaled column bit-packs");
		let for_bytes = enc.for_value_bytes().expect("scaled column FORs");
		assert!(for_bytes >= bitpack, "FOR {for_bytes} must not strictly beat global bit-pack {bitpack} on a single-block ramp");
		assert_eq!(enc.best_value_codec(), "scaled_bitpack");
	}

	#[test]
	fn for_value_estimate_is_none_for_non_scaled_columns() {
		// FOR, like the other scaled codecs, is defined only for a ScaledI64 mantissa stream.
		let floats = encode_column(PhysicalType::F64, &col(&["0.5", "1.5"])).expect("encodes");
		assert_eq!(floats.for_value_bytes(), None);
	}

	#[test]
	fn delta_cascade_beats_the_single_level_codecs_on_a_trending_column() {
		// A monotonically trending mantissa column (a counter climbing by ~7 per step from a
		// high base) defeats every single-level codec: FOR pays the whole run's range, bit-pack
		// pays the widest mantissa. The delta cascade turns the trend into a near-constant
		// difference stream that RLE/bit-pack then crush — the cascading-codec win.
		let lits: Vec<String> = (0..256).map(|i| format!("{}", 5_000_000_000_i64 + i64::from(i) * 7)).collect();
		let refs: Vec<&str> = lits.iter().map(String::as_str).collect();
		let enc = encode_column(PhysicalType::ScaledI64 { scale: 0 }, &col(&refs)).expect("encodes");
		let cascade = enc.delta_cascade_bytes().expect("scaled column cascades");
		let single_level = enc.best_serialized_bytes();
		assert!(cascade < single_level, "delta cascade {cascade} must beat the best single-level codec {single_level} on a trend");
		// Round-trip: the first-difference transform the estimate measures is losslessly
		// invertible (cumulative sum from the anchor recovers every mantissa).
		let mantissas = enc.scaled_i64_mantissas().unwrap();
		let mut deltas = Vec::new();
		let mut prev = mantissas[0];
		for &m in &mantissas[1..] {
			deltas.push(m.wrapping_sub(prev));
			prev = m;
		}
		let mut recon = vec![mantissas[0]];
		for &d in &deltas {
			let next = recon.last().unwrap().wrapping_add(d);
			recon.push(next);
		}
		assert_eq!(recon, mantissas, "the delta transform must round-trip the mantissas");
	}

	#[test]
	fn delta_cascade_is_none_for_non_scaled_and_trivial_for_short_columns() {
		// Defined only for a ScaledI64 mantissa stream; a single value costs just its varint anchor.
		let floats = encode_column(PhysicalType::F64, &col(&["0.5", "1.5"])).expect("encodes");
		assert_eq!(floats.delta_cascade_bytes(), None);
		let one = encode_column(PhysicalType::ScaledI64 { scale: 0 }, &col(&["42"])).expect("encodes");
		assert_eq!(one.delta_cascade_bytes(), Some(crate::timestamp::zigzag_varint_len(42)));
	}

	#[test]
	fn gorilla_f64_estimate_beats_raw_on_a_stable_exponent_float_column() {
		// A stable-exponent f64 series the recommender lands on F64 (a sensor drifting around
		// a fixed base near 1000): consecutive IEEE patterns share sign/exponent/high mantissa
		// bits, so the XOR codec undercuts the flat 8 B/value the varint payload writes. (A
		// zero-crossing signal would sweep exponents and Gorilla could not help — an honest
		// regime boundary, so the fixture stays in the regime where the win is real.)
		let lits: Vec<String> = (0..256).map(|i| format!("{:.9}", 1000.0 + f64::from(i) * 0.001)).collect();
		let refs: Vec<&str> = lits.iter().map(String::as_str).collect();
		let enc = encode_column(PhysicalType::F64, &col(&refs)).expect("encodes");
		let raw = enc.serialized_bytes();
		let gorilla = enc.gorilla_f64_bytes().expect("f64 column XOR-codes");
		assert_eq!(raw, 256 * 8, "raw F64 payload is 8 B/value");
		assert!(gorilla < raw, "gorilla {gorilla} must beat raw {raw} on a smooth f64 column");
		// The advisory method decodes back to the exact column via the floatcodec.
		let decoded = crate::floatcodec::xor_f64_decode(&crate::floatcodec::xor_f64_encode(&enc.f64_values().unwrap()), enc.len());
		assert_eq!(decoded, enc.f64_values().unwrap());
	}

	#[test]
	fn best_f64_advisory_selects_a_codec_no_worse_than_raw() {
		// The best-of advisory over an F64 column never exceeds the raw payload and names the
		// winning codec.
		let lits: Vec<String> = (0..256).map(|i| format!("{:.9}", 1000.0 + f64::from(i) * 0.001)).collect();
		let refs: Vec<&str> = lits.iter().map(String::as_str).collect();
		let enc = encode_column(PhysicalType::F64, &col(&refs)).expect("encodes");
		let best = enc.best_f64_bytes().expect("f64 column has a best codec");
		assert!(best <= enc.serialized_bytes(), "best {best} must not exceed raw {}", enc.serialized_bytes());
		assert!(best <= enc.gorilla_f64_bytes().unwrap(), "best must not exceed the gorilla-only figure");
		let name = enc.best_f64_codec().expect("f64 column names a codec");
		assert!(matches!(name, "gorilla" | "chimp" | "chimp128" | "raw"), "codec name {name} must be a known f64 codec");
		// A scaled column has no f64 codec at all.
		let scaled = encode_column(PhysicalType::ScaledI64 { scale: 2 }, &col(&["1.25", "2.50"])).expect("encodes");
		assert_eq!(scaled.best_f64_bytes(), None);
		assert_eq!(scaled.best_f64_codec(), None);
	}

	#[test]
	fn gorilla_f64_estimate_is_none_for_non_f64_columns() {
		// The XOR codec is defined only for the raw IEEE-754 payload — a scaled-integer
		// column has no f64 stream to XOR.
		let scaled = encode_column(PhysicalType::ScaledI64 { scale: 2 }, &col(&["1.25", "2.50"])).expect("encodes");
		assert_eq!(scaled.f64_values(), None);
		assert_eq!(scaled.gorilla_f64_bytes(), None);
	}
}
