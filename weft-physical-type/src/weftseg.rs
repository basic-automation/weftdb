//! On-disk `.weftseg` framing (roadmap **Phase 4.3**, the paged binary layout slice).
//!
//! [`crate::segment`] gave Storage v2 the *in-memory* shape of a sealed segment: a
//! typed value column, a delta-of-delta timestamp column, and the per-segment stats
//! a reader prunes against. This module is the **byte layout** that segment seals
//! to — a hand-rolled, versioned, checksummed binary frame (`.weftseg`), built up in
//! slices:
//!
//! 1. **this slice** — the low-level byte primitives every later layer is written in
//!    (fixed-width little-endian integers, LEB128 unsigned varints, zig-zag signed
//!    varints, length-prefixed byte/UTF-8 blocks) plus an IEEE **CRC-32** for
//!    integrity,
//! 2. the value-column codec ([`PhysicalValue`] streams),
//! 3. the timestamp-column codec ([`DeltaOfDeltaColumn`]),
//! 4. the framed [`Segment`] — magic + header (stats) + the two
//!    column blocks + a trailing checksum, with corruption detection.
//!
//! ## Why hand-rolled
//!
//! The 2026-06-20 run established empirically that a naive `bincode`-of-the-struct
//! frame is the wrong target: `bincode` cannot round-trip `BigDecimal` (it requires
//! `serde::Deserializer::deserialize_any`, which non-self-describing formats reject),
//! and the roadmap wants a *paged* columnar layout with per-page stats and checksums
//! anyway — not an opaque serde blob. So this module encodes the columns' primitive
//! byte streams directly, keeping `BigDecimalText` values as length-prefixed UTF-8,
//! which is exactly the layout a future paged reader can seek within.
//!
//! The primitives mirror the *estimators* the in-memory layer already uses for
//! bytes/point ([`crate::timestamp::zigzag_varint_len`], [`crate::timestamp::uvarint_len`]):
//! the realized on-disk size of a column written here matches the advisory estimate,
//! by construction, because both count the same varint widths.

/// Why a `.weftseg` byte stream could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WeftSegError {
	/// The reader ran out of bytes before a value was fully decoded.
	UnexpectedEof {
		/// Number of bytes the read needed.
		needed: usize,
		/// Number of bytes that remained.
		remaining: usize,
	},
	/// A LEB128 varint did not terminate within the maximum width (10 bytes for a
	/// 64-bit value) — a malformed or truncated stream.
	VarintTooLong,
	/// A length-prefixed UTF-8 block held bytes that are not valid UTF-8.
	InvalidUtf8,
	/// A stored `BigDecimal` text block (a value or a min/max stat) did not parse as
	/// a decimal — a corrupt or misframed stream.
	InvalidDecimal,
	/// The frame did not start with the expected `.weftseg` magic bytes.
	BadMagic,
	/// The frame's format version is not one this reader understands.
	UnsupportedVersion {
		/// The version found in the frame.
		found: u16,
	},
	/// A type/codec tag byte was not a value this reader recognises.
	InvalidTag {
		/// What kind of tag was being read (for diagnostics).
		kind: &'static str,
		/// The unrecognised tag byte.
		value: u8,
	},
	/// The trailing CRC-32 did not match the checksum recomputed over the body — the
	/// frame is corrupt or truncated.
	ChecksumMismatch {
		/// The checksum stored in the frame.
		stored: u32,
		/// The checksum recomputed over the body.
		computed: u32,
	},
	/// The frame decoded successfully but bytes remained after it — a sign the stream
	/// was misframed.
	TrailingBytes {
		/// How many bytes remained unread.
		remaining: usize,
	},
	/// The quality-column block was internally inconsistent (a bitmap whose length
	/// or clear-bit count disagrees with the header's row/null counts).
	InvalidNullMask(crate::nulls::NullMaskError),
}

impl std::fmt::Display for WeftSegError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::UnexpectedEof { needed, remaining } => write!(f, "unexpected end of segment stream: needed {needed} bytes, {remaining} remaining"),
			Self::VarintTooLong => f.write_str("malformed varint: did not terminate within 10 bytes"),
			Self::InvalidUtf8 => f.write_str("length-prefixed block is not valid UTF-8"),
			Self::InvalidDecimal => f.write_str("stored decimal text did not parse"),
			Self::BadMagic => f.write_str("not a .weftseg frame: bad magic bytes"),
			Self::UnsupportedVersion { found } => write!(f, "unsupported .weftseg format version {found}"),
			Self::InvalidTag { kind, value } => write!(f, "invalid {kind} tag byte {value:#04x}"),
			Self::ChecksumMismatch { stored, computed } => write!(f, "segment checksum mismatch: stored {stored:#010x}, computed {computed:#010x}"),
			Self::TrailingBytes { remaining } => write!(f, "{remaining} trailing bytes after segment frame"),
			Self::InvalidNullMask(source) => write!(f, "invalid quality column: {source}"),
		}
	}
}

impl std::error::Error for WeftSegError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::InvalidNullMask(source) => Some(source),
			_ => None,
		}
	}
}

/// Build the IEEE CRC-32 lookup table at compile time (reversed polynomial
/// `0xEDB8_8320`, the zlib/PNG variant).
const fn crc32_table() -> [u32; 256] {
	let mut table = [0_u32; 256];
	let mut i: u32 = 0;
	while i < 256 {
		let mut crc = i;
		let mut j = 0;
		while j < 8 {
			crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
			j += 1;
		}
		table[i as usize] = crc;
		i += 1;
	}
	table
}

/// The precomputed CRC-32 table.
const CRC32_TABLE: [u32; 256] = crc32_table();

/// IEEE CRC-32 (zlib/PNG variant) over a byte slice.
///
/// Used to checksum the body of a `.weftseg` frame so a single flipped or dropped
/// byte is detected on read rather than silently misinterpreted. Standard test
/// vector: `crc32(b"123456789") == 0xCBF4_3926`.
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
	let mut crc = 0xFFFF_FFFF_u32;
	for &byte in data {
		let idx = ((crc ^ u32::from(byte)) & 0xFF) as usize;
		crc = (crc >> 8) ^ CRC32_TABLE[idx];
	}
	crc ^ 0xFFFF_FFFF
}

/// A growable little-endian byte sink for writing a `.weftseg` frame.
///
/// The mirror image of [`ByteReader`]: every `put_*` here has a `read_*` there with
/// the exact inverse wire shape, so a value written and then read is recovered
/// bit-for-bit.
#[derive(Debug, Default, Clone)]
pub struct ByteWriter {
	buf: Vec<u8>,
}

impl ByteWriter {
	/// A new empty writer.
	#[must_use]
	pub const fn new() -> Self {
		Self { buf: Vec::new() }
	}

	/// A new writer with room for `capacity` bytes reserved up front.
	#[must_use]
	pub fn with_capacity(capacity: usize) -> Self {
		Self { buf: Vec::with_capacity(capacity) }
	}

	/// Number of bytes written so far.
	#[must_use]
	pub const fn len(&self) -> usize {
		self.buf.len()
	}

	/// Whether nothing has been written yet.
	#[must_use]
	pub const fn is_empty(&self) -> bool {
		self.buf.is_empty()
	}

	/// A read-only view of the bytes written so far (e.g. to checksum the body
	/// before appending the trailing CRC).
	#[must_use]
	pub fn as_slice(&self) -> &[u8] {
		&self.buf
	}

	/// Consume the writer, returning the underlying byte vector.
	#[must_use]
	pub fn into_vec(self) -> Vec<u8> {
		self.buf
	}

	/// Append one raw byte.
	pub fn put_u8(&mut self, value: u8) {
		self.buf.push(value);
	}

	/// Append raw bytes verbatim (no length prefix).
	pub fn put_raw(&mut self, bytes: &[u8]) {
		self.buf.extend_from_slice(bytes);
	}

	/// Append a `u16` little-endian.
	pub fn put_u16_le(&mut self, value: u16) {
		self.buf.extend_from_slice(&value.to_le_bytes());
	}

	/// Append a `u32` little-endian.
	pub fn put_u32_le(&mut self, value: u32) {
		self.buf.extend_from_slice(&value.to_le_bytes());
	}

	/// Append an `i64` little-endian (fixed 8 bytes) — used for the segment anchor,
	/// where a full-width value is expected.
	pub fn put_i64_le(&mut self, value: i64) {
		self.buf.extend_from_slice(&value.to_le_bytes());
	}

	/// Append an `i128` little-endian (fixed 16 bytes) — a `ScaledI128` /
	/// `Decimal128` mantissa, where the full width is needed.
	pub fn put_i128_le(&mut self, value: i128) {
		self.buf.extend_from_slice(&value.to_le_bytes());
	}

	/// Append an `f64` little-endian (its IEEE-754 byte pattern, 8 bytes).
	pub fn put_f64_le(&mut self, value: f64) {
		self.buf.extend_from_slice(&value.to_le_bytes());
	}

	/// Append an `f32` little-endian (its IEEE-754 byte pattern, 4 bytes).
	pub fn put_f32_le(&mut self, value: f32) {
		self.buf.extend_from_slice(&value.to_le_bytes());
	}

	/// Append a `u64` as an LEB128 unsigned varint (1..=10 bytes).
	pub fn put_uvarint(&mut self, mut value: u64) {
		loop {
			let mut byte = (value & 0x7F) as u8;
			value >>= 7;
			if value != 0 {
				byte |= 0x80;
			}
			self.buf.push(byte);
			if value == 0 {
				break;
			}
		}
	}

	/// Append an `i64` as a zig-zag + LEB128 signed varint — small-magnitude values
	/// (the common case for deltas) cost a single byte.
	pub fn put_svarint(&mut self, value: i64) {
		#[allow(clippy::cast_sign_loss)]
		let zz = ((value << 1) ^ (value >> 63)) as u64;
		self.put_uvarint(zz);
	}

	/// Append a length-prefixed byte block: an unsigned varint length, then the bytes.
	pub fn put_bytes(&mut self, bytes: &[u8]) {
		self.put_uvarint(bytes.len() as u64);
		self.buf.extend_from_slice(bytes);
	}

	/// Append a length-prefixed UTF-8 string (the on-disk form of a `BigDecimalText`
	/// value or a `BigDecimal` min/max stat).
	pub fn put_str(&mut self, value: &str) {
		self.put_bytes(value.as_bytes());
	}
}

/// A cursor over a `.weftseg` byte slice with checked little-endian reads.
///
/// Every read advances the cursor and returns [`WeftSegError::UnexpectedEof`] rather
/// than panicking when the stream is short, so a truncated or corrupt frame is a
/// recoverable error.
#[derive(Debug, Clone)]
pub struct ByteReader<'a> {
	buf: &'a [u8],
	pos: usize,
}

impl<'a> ByteReader<'a> {
	/// A reader positioned at the start of `buf`.
	#[must_use]
	pub const fn new(buf: &'a [u8]) -> Self {
		Self { buf, pos: 0 }
	}

	/// Number of bytes not yet consumed.
	#[must_use]
	pub const fn remaining(&self) -> usize {
		self.buf.len() - self.pos
	}

	/// Whether the cursor has consumed the whole slice.
	#[must_use]
	pub const fn is_empty(&self) -> bool {
		self.pos >= self.buf.len()
	}

	/// Take the next `n` bytes as a sub-slice, advancing the cursor.
	///
	/// # Errors
	///
	/// [`WeftSegError::UnexpectedEof`] if fewer than `n` bytes remain.
	pub fn take(&mut self, n: usize) -> Result<&'a [u8], WeftSegError> {
		if self.remaining() < n {
			return Err(WeftSegError::UnexpectedEof { needed: n, remaining: self.remaining() });
		}
		let out = &self.buf[self.pos..self.pos + n];
		self.pos += n;
		Ok(out)
	}

	/// Read one raw byte.
	///
	/// # Errors
	///
	/// [`WeftSegError::UnexpectedEof`] at end of stream.
	pub fn read_u8(&mut self) -> Result<u8, WeftSegError> {
		Ok(self.take(1)?[0])
	}

	/// Read a little-endian `u16`.
	///
	/// # Errors
	///
	/// [`WeftSegError::UnexpectedEof`] if fewer than 2 bytes remain.
	pub fn read_u16_le(&mut self) -> Result<u16, WeftSegError> {
		let b = self.take(2)?;
		Ok(u16::from_le_bytes([b[0], b[1]]))
	}

	/// Read a little-endian `u32`.
	///
	/// # Errors
	///
	/// [`WeftSegError::UnexpectedEof`] if fewer than 4 bytes remain.
	pub fn read_u32_le(&mut self) -> Result<u32, WeftSegError> {
		let b = self.take(4)?;
		Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
	}

	/// Read a little-endian fixed-width `i64`.
	///
	/// # Errors
	///
	/// [`WeftSegError::UnexpectedEof`] if fewer than 8 bytes remain.
	pub fn read_i64_le(&mut self) -> Result<i64, WeftSegError> {
		let b = self.take(8)?;
		Ok(i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
	}

	/// Read a little-endian fixed-width `i128`.
	///
	/// # Errors
	///
	/// [`WeftSegError::UnexpectedEof`] if fewer than 16 bytes remain.
	pub fn read_i128_le(&mut self) -> Result<i128, WeftSegError> {
		let b = self.take(16)?;
		Ok(i128::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]]))
	}

	/// Read a little-endian `f64` (IEEE-754 byte pattern).
	///
	/// # Errors
	///
	/// [`WeftSegError::UnexpectedEof`] if fewer than 8 bytes remain.
	pub fn read_f64_le(&mut self) -> Result<f64, WeftSegError> {
		let b = self.take(8)?;
		Ok(f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
	}

	/// Read a little-endian `f32` (IEEE-754 byte pattern).
	///
	/// # Errors
	///
	/// [`WeftSegError::UnexpectedEof`] if fewer than 4 bytes remain.
	pub fn read_f32_le(&mut self) -> Result<f32, WeftSegError> {
		let b = self.take(4)?;
		Ok(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
	}

	/// Read an LEB128 unsigned varint.
	///
	/// # Errors
	///
	/// [`WeftSegError::UnexpectedEof`] if the stream ends mid-varint, or
	/// [`WeftSegError::VarintTooLong`] if it does not terminate within 10 bytes.
	pub fn read_uvarint(&mut self) -> Result<u64, WeftSegError> {
		let mut result: u64 = 0;
		let mut shift = 0_u32;
		loop {
			let byte = self.read_u8()?;
			result |= u64::from(byte & 0x7F) << shift;
			if byte & 0x80 == 0 {
				return Ok(result);
			}
			shift += 7;
			if shift >= 64 {
				return Err(WeftSegError::VarintTooLong);
			}
		}
	}

	/// Read a zig-zag + LEB128 signed varint (the inverse of
	/// [`ByteWriter::put_svarint`]).
	///
	/// # Errors
	///
	/// As [`read_uvarint`](Self::read_uvarint).
	pub fn read_svarint(&mut self) -> Result<i64, WeftSegError> {
		let zz = self.read_uvarint()?;
		// Inverse zig-zag: (zz >> 1) ^ -(zz & 1).
		#[allow(clippy::cast_possible_wrap)]
		Ok(((zz >> 1) as i64) ^ -((zz & 1) as i64))
	}

	/// Read a length-prefixed byte block (varint length, then the bytes).
	///
	/// # Errors
	///
	/// [`WeftSegError::UnexpectedEof`] if the stream is shorter than the declared
	/// length.
	pub fn read_bytes(&mut self) -> Result<&'a [u8], WeftSegError> {
		let len = usize::try_from(self.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
		self.take(len)
	}

	/// Read a length-prefixed UTF-8 string.
	///
	/// # Errors
	///
	/// [`WeftSegError::UnexpectedEof`] if short, or [`WeftSegError::InvalidUtf8`] if the
	/// bytes are not valid UTF-8.
	pub fn read_str(&mut self) -> Result<&'a str, WeftSegError> {
		let bytes = self.read_bytes()?;
		std::str::from_utf8(bytes).map_err(|_| WeftSegError::InvalidUtf8)
	}
}

// ---------------------------------------------------------------------------
// Value-column codec (Phase 4.3, slice 2)
//
// The on-disk byte block for a `ColumnEncoding`: a one-byte physical-type tag
// (carrying the shared `scale` for the `ScaledI*` encodings), the value count,
// the lossy bookkeeping (`lossy_count` + `max_abs_error` as decimal text), then
// the packed per-value payloads. The reader dispatches the per-value shape on the
// header tag, so the column invariant (every value matches `physical_type`) is
// what makes write and read agree.
// ---------------------------------------------------------------------------

use std::str::FromStr;

use bigdecimal::BigDecimal;

use crate::{
	timestamp::{DeltaOfDeltaColumn, DodCheckpoint, DodCheckpoints, TimeUnit}, CascadeInner, ColumnEncoding, PhysicalType, PhysicalValue
};

const TAG_F64: u8 = 0;
const TAG_F32: u8 = 1;
const TAG_SCALED_I64: u8 = 2;
const TAG_SCALED_I128: u8 = 3;
const TAG_DECIMAL128: u8 = 4;
const TAG_BIGDECIMAL_TEXT: u8 = 5;

/// The stable one-byte on-disk tag for a [`PhysicalType`].
const fn physical_type_tag(pt: PhysicalType) -> u8 {
	match pt {
		PhysicalType::F64 => TAG_F64,
		PhysicalType::F32 => TAG_F32,
		PhysicalType::ScaledI64 { .. } => TAG_SCALED_I64,
		PhysicalType::ScaledI128 { .. } => TAG_SCALED_I128,
		PhysicalType::Decimal128 => TAG_DECIMAL128,
		PhysicalType::BigDecimalText => TAG_BIGDECIMAL_TEXT,
	}
}

/// Write one physical value's payload (the variant is recovered from the column's
/// header tag, so only the data is written here).
fn write_physical_value(w: &mut ByteWriter, value: &PhysicalValue) {
	match value {
		PhysicalValue::F64(f) => w.put_f64_le(*f),
		PhysicalValue::F32(f) => w.put_f32_le(*f),
		PhysicalValue::ScaledI64 { mantissa, .. } => w.put_svarint(*mantissa),
		PhysicalValue::ScaledI128 { mantissa, .. } => w.put_i128_le(*mantissa),
		PhysicalValue::Decimal128 { mantissa, scale } => {
			w.put_i128_le(*mantissa);
			w.put_svarint(*scale);
		}
		PhysicalValue::BigDecimalText(s) => w.put_str(s),
	}
}

/// Read one physical value's payload for a column of the given `physical_type`.
fn read_physical_value(r: &mut ByteReader, physical_type: PhysicalType) -> Result<PhysicalValue, WeftSegError> {
	Ok(match physical_type {
		PhysicalType::F64 => PhysicalValue::F64(r.read_f64_le()?),
		PhysicalType::F32 => PhysicalValue::F32(r.read_f32_le()?),
		PhysicalType::ScaledI64 { scale } => PhysicalValue::ScaledI64 { mantissa: r.read_svarint()?, scale },
		PhysicalType::ScaledI128 { scale } => PhysicalValue::ScaledI128 { mantissa: r.read_i128_le()?, scale },
		PhysicalType::Decimal128 => PhysicalValue::Decimal128 { mantissa: r.read_i128_le()?, scale: r.read_svarint()? },
		PhysicalType::BigDecimalText => PhysicalValue::BigDecimalText(r.read_str()?.to_owned()),
	})
}

/// Parse a stored decimal-text block back into a [`BigDecimal`].
fn read_decimal(r: &mut ByteReader) -> Result<BigDecimal, WeftSegError> {
	BigDecimal::from_str(r.read_str()?).map_err(|_| WeftSegError::InvalidDecimal)
}

/// Value-column codec selector (self-describing byte in a v4+ value block).
///
/// `VAL_CODEC_VARINT` is the general per-value payload (the codec every physical
/// type can use); `VAL_CODEC_BITPACK` is the fixed-width bit-packed mantissa
/// stream, defined only for a `ScaledI64` column and written only when it is
/// strictly smaller than the varint (a regular/small-jitter scaled series).
const VAL_CODEC_VARINT: u8 = 0;
/// Fixed-width bit-packing of a `ScaledI64` column's mantissas: a width byte then
/// `ceil(count * width / 8)` packed data bytes (LSB-first, zig-zag coded).
const VAL_CODEC_BITPACK: u8 = 1;
/// Per-block adaptive (blocked) bit-packing of a `ScaledI64` column's mantissas: a
/// block-size uvarint then a length-prefixed [`crate::timestamp::blocked_bitpack_encode`]
/// stream (each block carries its own one-byte width header). Written only when strictly
/// smallest — a mixed-magnitude mantissa column where a global width over-pays.
const VAL_CODEC_BLOCKED: u8 = 2;
/// Per-block Frame-of-Reference packing of a `ScaledI64` column's mantissas: a block-size
/// uvarint then a length-prefixed [`crate::timestamp::for_bitpack_encode`] stream (each
/// block carries a zig-zag-varint reference — its minimum — then the *unsigned* residuals
/// packed at the block's range width). Written only when strictly smallest — mantissas
/// clustered at a high base, or any all-non-negative stream where the unsigned residual
/// beats zig-zag's sign bit.
const VAL_CODEC_FOR: u8 = 3;
/// **Delta cascade** — the first two-level (composed) codec: a zig-zag-varint anchor (the
/// first mantissa), a one-byte inner-codec descriptor, then the first-difference stream packed
/// by that inner codec. Written only when strictly smallest — a trending `ScaledI64` column
/// (counter, monotone sensor) whose magnitude/range every single-level codec pays for, but
/// whose first differences collapse to a constant the inner packer crushes. The inner codec
/// descriptor names one of the five [`CascadeInner`] packers over the delta stream.
const VAL_CODEC_DELTA_CASCADE: u8 = 4;
/// **Transposed (`FastLanes`-layout) per-tile bit-packing** of a `ScaledI64` column's
/// mantissas: a tile-size uvarint then a length-prefixed
/// [`crate::timestamp::transpose_bitpack_encode`] stream (each tile carries a one-byte width
/// header followed by `width` bit-planes). The same *code* as `VAL_CODEC_BITPACK` with its
/// bits permuted, so it never wins the size race — it is written only when a caller asks for
/// it via [`FrameOptions::transposed_max_overhead`], to buy decode latency: the decoder reads
/// `u64` plane words and walks only the set bits, skipping a small-magnitude column's empty
/// high bit-planes wholesale. Random-access capable through
/// [`crate::timestamp::transpose_bitpack_decode_range`], so it does not regress the streaming
/// point read. *(src: `FastLanes` Compression Layout, VLDB'23 —
/// <https://www.vldb.org/pvldb/vol16/p2132-afroozeh.pdf>)*
const VAL_CODEC_TRANSPOSED: u8 = 5;

/// Cascade inner-codec descriptors — the second stage applied to the delta stream. They map
/// 1:1 to [`CascadeInner`] and mirror the top-level timestamp/value codec framings.
const CASCADE_INNER_VARINT: u8 = 0;
const CASCADE_INNER_BITPACK: u8 = 1;
const CASCADE_INNER_BLOCKED: u8 = 2;
const CASCADE_INNER_FOR: u8 = 3;
const CASCADE_INNER_RLE: u8 = 4;

/// Write a [`ColumnEncoding`] as a `.weftseg` value-column block.
///
/// After the header (physical-type tag, optional `ScaledI*` scale, count, lossy
/// count, `max_abs_error`) the block carries a self-describing codec byte, then the
/// coded payload. Four codecs are realized: the general per-value payload
/// (`VAL_CODEC_VARINT` — IEEE byte patterns for the floats, a zig-zag varint
/// mantissa for `ScaledI64`, full-width `i128` for the wide integers, length-prefixed
/// UTF-8 for `BigDecimalText`), fixed-width **bit-packing** of a `ScaledI64` column's
/// mantissas (`VAL_CODEC_BITPACK`, a regular/small-jitter scaled series),
/// **per-block adaptive bit-packing** (`VAL_CODEC_BLOCKED`, a mixed-magnitude scaled
/// series where a global width over-pays), and **per-block Frame-of-Reference** packing
/// (`VAL_CODEC_FOR`, mantissas clustered at a high base or all-non-negative). The
/// codec is chosen through [`ColumnEncoding::best_value_codec`], the single source of
/// truth, so each `ScaledI64` column realizes the smallest of the four on disk while
/// every other column keeps the per-value payload. All four codecs are exact and
/// lossless.
pub fn write_value_column(w: &mut ByteWriter, col: &ColumnEncoding) {
	write_value_column_selected(w, col, col.best_value_codec());
}

/// Write a [`ColumnEncoding`] as a `.weftseg` value block **with the two-level delta cascade
/// allowed** (the fifth codec, `VAL_CODEC_DELTA_CASCADE`).
///
/// Identical to [`write_value_column`] except the codec is chosen through
/// [`ColumnEncoding::best_value_codec_cascading`], so a trending `ScaledI64` column whose
/// first differences pack below every single-level codec is stored as a delta cascade. The
/// block is read back by the ordinary [`read_value_column`] (the cascade decode is
/// unconditional), so a cascade-sealed segment round-trips through the normal read path.
///
/// This is **opt-in**: the cascade is a broad realized-bytes change (it beats FOR on FOR's
/// own clustered fixtures), so folding it into the default seal is owner-gated. Callers that
/// want it request it explicitly.
pub fn write_value_column_cascading(w: &mut ByteWriter, col: &ColumnEncoding) {
	write_value_column_selected(w, col, col.best_value_codec_cascading());
}

/// Write a [`ColumnEncoding`] as a `.weftseg` value block, allowing the transposed codec.
///
/// The transposed (`FastLanes`-layout) codec (`VAL_CODEC_TRANSPOSED`) is permitted up to a
/// size overhead of `max_overhead` against the size-selected codec.
///
/// Identical to [`write_value_column`] except the codec is chosen through
/// [`ColumnEncoding::best_value_codec_transposed`], so a `ScaledI64` column whose transposed
/// footprint is within the ceiling stores its mantissas bit-plane-major. The block is read
/// back by the ordinary [`read_value_column`] and random-accessed by [`read_value_at`], so a
/// transposed segment serves the full-decode, point-read and windowed-range paths unchanged.
///
/// This is **opt-in**: the transposed layout trades bytes for decode latency, so folding it
/// into the default seal is a headline bytes/point change and is owner-gated. Callers request
/// it explicitly through [`FrameOptions`].
pub fn write_value_column_transposed(w: &mut ByteWriter, col: &ColumnEncoding, max_overhead: f64) {
	write_value_column_selected(w, col, col.best_value_codec_transposed(max_overhead));
}

/// Shared value-block writer: emit the header then the payload for the named `codec` (the
/// output of one of the [`ColumnEncoding`] selectors). Separating the selector from the
/// serialization lets the default and cascading writers share one exact encoder.
fn write_value_column_selected(w: &mut ByteWriter, col: &ColumnEncoding, codec: &str) {
	w.put_u8(physical_type_tag(col.physical_type));
	match col.physical_type {
		PhysicalType::ScaledI64 { scale } | PhysicalType::ScaledI128 { scale } => w.put_u8(scale),
		_ => {}
	}
	w.put_uvarint(col.values.len() as u64);
	w.put_uvarint(col.lossy_count as u64);
	w.put_str(&col.max_abs_error.to_plain_string());
	match codec {
		"scaled_delta_cascade" => {
			// A trending ScaledI64 column whose first differences pack (via an inner codec)
			// below every single-level codec (guaranteed by best_value_codec — so
			// delta_cascade_plan is Some over a scaled mantissa stream).
			let plan = col.delta_cascade_plan().unwrap_or(crate::DeltaCascadePlan { anchor: 0, deltas: Vec::new(), inner: CascadeInner::Varint, bytes: 0 });
			w.put_u8(VAL_CODEC_DELTA_CASCADE);
			w.put_svarint(plan.anchor);
			match plan.inner {
				CascadeInner::Bitpack => {
					let (width, packed) = crate::timestamp::bitpack_encode(&plan.deltas);
					w.put_u8(CASCADE_INNER_BITPACK);
					// Width is 0..=64 by construction, so the conversion never saturates.
					w.put_u8(u8::try_from(width).unwrap_or(64));
					// Raw (no length prefix): the reader derives the length from (count-1) * width.
					w.put_raw(&packed);
				}
				CascadeInner::Blocked => {
					let block = crate::timestamp::BLOCKED_BITPACK_BLOCK;
					w.put_u8(CASCADE_INNER_BLOCKED);
					w.put_uvarint(block as u64);
					// Length-prefixed: per-block widths vary (as the top-level blocked block).
					w.put_bytes(&crate::timestamp::blocked_bitpack_encode(&plan.deltas, block));
				}
				CascadeInner::For => {
					let block = crate::timestamp::BLOCKED_BITPACK_BLOCK;
					w.put_u8(CASCADE_INNER_FOR);
					w.put_uvarint(block as u64);
					// Length-prefixed: per-block references + widths vary (as the top-level FOR block).
					w.put_bytes(&crate::timestamp::for_bitpack_encode(&plan.deltas, block));
				}
				CascadeInner::Rle => {
					let runs = crate::timestamp::rle_encode(&plan.deltas);
					w.put_u8(CASCADE_INNER_RLE);
					w.put_uvarint(runs.len() as u64);
					for (value, run_len) in runs {
						w.put_svarint(value);
						w.put_uvarint(run_len as u64);
					}
				}
				CascadeInner::Varint => {
					w.put_u8(CASCADE_INNER_VARINT);
					// Raw (no length prefix): the reader decodes exactly (count-1) svarints.
					for &delta in &plan.deltas {
						w.put_svarint(delta);
					}
				}
			}
		}
		"scaled_transposed" => {
			// A ScaledI64 column whose transposed footprint is within the caller's overhead
			// ceiling (guaranteed by best_value_codec_transposed — so scaled_i64_mantissas is
			// Some). Same framing as the two per-block codecs: a tile-size uvarint then the
			// length-prefixed stream (per-tile widths vary, so the boundaries are not derivable
			// from count alone).
			let mantissas = col.scaled_i64_mantissas().unwrap_or_default();
			let tile = crate::timestamp::TRANSPOSE_TILE;
			w.put_u8(VAL_CODEC_TRANSPOSED);
			w.put_uvarint(tile as u64);
			w.put_bytes(&crate::timestamp::transpose_bitpack_encode(&mantissas, tile));
		}
		"scaled_for" => {
			// A ScaledI64 column whose mantissas FOR-pack below the varint, the global
			// bit-pack, and the blocked codec (guaranteed by best_value_codec — so
			// scaled_i64_mantissas is Some).
			let mantissas = col.scaled_i64_mantissas().unwrap_or_default();
			let block = crate::timestamp::BLOCKED_BITPACK_BLOCK;
			w.put_u8(VAL_CODEC_FOR);
			w.put_uvarint(block as u64);
			// Length-prefixed: per-block references and widths vary, so the boundaries are
			// not derivable from count alone (as the blocked block).
			w.put_bytes(&crate::timestamp::for_bitpack_encode(&mantissas, block));
		}
		"scaled_blocked" => {
			// A ScaledI64 column whose mantissas per-block adaptive bit-pack below both the
			// varint and the global bit-pack (guaranteed by best_value_codec — so
			// scaled_i64_mantissas is Some).
			let mantissas = col.scaled_i64_mantissas().unwrap_or_default();
			let block = crate::timestamp::BLOCKED_BITPACK_BLOCK;
			w.put_u8(VAL_CODEC_BLOCKED);
			w.put_uvarint(block as u64);
			// Length-prefixed: per-block widths vary, so the boundaries are not derivable
			// from count alone — the prefix bounds the payload in one step (as the timestamp
			// blocked block does).
			w.put_bytes(&crate::timestamp::blocked_bitpack_encode(&mantissas, block));
		}
		"scaled_bitpack" => {
			// A ScaledI64 column whose mantissas bit-pack below the varint (guaranteed by
			// best_value_codec — so scaled_i64_mantissas is Some).
			let mantissas = col.scaled_i64_mantissas().unwrap_or_default();
			let (width, packed) = crate::timestamp::bitpack_encode(&mantissas);
			w.put_u8(VAL_CODEC_BITPACK);
			// Width is 0..=64 by construction, so the conversion never saturates.
			w.put_u8(u8::try_from(width).unwrap_or(64));
			// Raw (no length prefix): the reader derives the length from count * width.
			w.put_raw(&packed);
		}
		// "varint" — the general per-value payload, and the only codec for non-scaled types.
		_ => {
			w.put_u8(VAL_CODEC_VARINT);
			for value in &col.values {
				write_physical_value(w, value);
			}
		}
	}
}

/// Read a [`ColumnEncoding`] from a `.weftseg` value-column block — the exact
/// inverse of [`write_value_column`].
///
/// # Errors
///
/// [`WeftSegError::InvalidTag`] for an unrecognised physical-type tag, an
/// unrecognised value-codec byte, or a bit-pack codec on a non-`ScaledI64` column;
/// [`WeftSegError::InvalidDecimal`] if the stored `max_abs_error` does not parse; or
/// [`WeftSegError::UnexpectedEof`] / [`WeftSegError::VarintTooLong`] on a short or
/// malformed stream.
pub fn read_value_column(r: &mut ByteReader) -> Result<ColumnEncoding, WeftSegError> {
	let tag = r.read_u8()?;
	let physical_type = match tag {
		TAG_F64 => PhysicalType::F64,
		TAG_F32 => PhysicalType::F32,
		TAG_SCALED_I64 => PhysicalType::ScaledI64 { scale: r.read_u8()? },
		TAG_SCALED_I128 => PhysicalType::ScaledI128 { scale: r.read_u8()? },
		TAG_DECIMAL128 => PhysicalType::Decimal128,
		TAG_BIGDECIMAL_TEXT => PhysicalType::BigDecimalText,
		other => return Err(WeftSegError::InvalidTag { kind: "physical_type", value: other }),
	};
	let count = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
	let lossy_count = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
	let max_abs_error = read_decimal(r)?;
	let values = match r.read_u8()? {
		VAL_CODEC_VARINT => {
			let mut values = Vec::with_capacity(count);
			for _ in 0..count {
				values.push(read_physical_value(r, physical_type)?);
			}
			values
		}
		VAL_CODEC_BITPACK => {
			let PhysicalType::ScaledI64 { scale } = physical_type else {
				return Err(WeftSegError::InvalidTag { kind: "value_codec_bitpack_type", value: tag });
			};
			let width = u32::from(r.read_u8()?);
			let data_len = (count * width as usize).div_ceil(8);
			let bytes = r.take(data_len)?;
			crate::timestamp::bitpack_decode(width, bytes, count).into_iter().map(|mantissa| PhysicalValue::ScaledI64 { mantissa, scale }).collect()
		}
		VAL_CODEC_BLOCKED => {
			let PhysicalType::ScaledI64 { scale } = physical_type else {
				return Err(WeftSegError::InvalidTag { kind: "value_codec_blocked_type", value: tag });
			};
			let block = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
			let bytes = r.read_bytes()?;
			crate::timestamp::blocked_bitpack_decode(bytes, block, count).into_iter().map(|mantissa| PhysicalValue::ScaledI64 { mantissa, scale }).collect()
		}
		VAL_CODEC_FOR => {
			let PhysicalType::ScaledI64 { scale } = physical_type else {
				return Err(WeftSegError::InvalidTag { kind: "value_codec_for_type", value: tag });
			};
			let block = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
			let bytes = r.read_bytes()?;
			crate::timestamp::for_bitpack_decode(bytes, block, count).into_iter().map(|mantissa| PhysicalValue::ScaledI64 { mantissa, scale }).collect()
		}
		VAL_CODEC_TRANSPOSED => {
			let PhysicalType::ScaledI64 { scale } = physical_type else {
				return Err(WeftSegError::InvalidTag { kind: "value_codec_transposed_type", value: tag });
			};
			let tile = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
			let bytes = r.read_bytes()?;
			crate::timestamp::transpose_bitpack_decode(bytes, tile, count).into_iter().map(|mantissa| PhysicalValue::ScaledI64 { mantissa, scale }).collect()
		}
		VAL_CODEC_DELTA_CASCADE => read_cascade_value_column(r, physical_type, tag, count)?,
		other => return Err(WeftSegError::InvalidTag { kind: "value_codec", value: other }),
	};
	Ok(ColumnEncoding { physical_type, values, lossy_count, max_abs_error })
}

/// Decode a `VAL_CODEC_DELTA_CASCADE` value block into its `ScaledI64` values — the
/// anchor (first mantissa) plus the inner-coded first-difference deltas, cumulatively summed.
///
/// Split out of [`read_value_column`] so each stays within one screen. The five inner
/// descriptors mirror the top-level `ScaledI64` codecs over the delta stream.
///
/// # Errors
///
/// [`WeftSegError::InvalidTag`] for a cascade block on a non-`ScaledI64` column or an
/// unrecognised inner descriptor; [`WeftSegError::UnexpectedEof`] / [`WeftSegError::VarintTooLong`]
/// on a short or malformed stream.
fn read_cascade_value_column(r: &mut ByteReader, physical_type: PhysicalType, tag: u8, count: usize) -> Result<Vec<PhysicalValue>, WeftSegError> {
	let PhysicalType::ScaledI64 { scale } = physical_type else {
		return Err(WeftSegError::InvalidTag { kind: "value_codec_delta_cascade_type", value: tag });
	};
	let anchor = r.read_svarint()?;
	let delta_count = count.saturating_sub(1);
	let deltas = match r.read_u8()? {
		CASCADE_INNER_VARINT => {
			let mut deltas = Vec::with_capacity(delta_count);
			for _ in 0..delta_count {
				deltas.push(r.read_svarint()?);
			}
			deltas
		}
		CASCADE_INNER_BITPACK => {
			let width = u32::from(r.read_u8()?);
			let data_len = (delta_count * width as usize).div_ceil(8);
			let bytes = r.take(data_len)?;
			crate::timestamp::bitpack_decode(width, bytes, delta_count)
		}
		CASCADE_INNER_BLOCKED => {
			let block = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
			let bytes = r.read_bytes()?;
			crate::timestamp::blocked_bitpack_decode(bytes, block, delta_count)
		}
		CASCADE_INNER_FOR => {
			let block = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
			let bytes = r.read_bytes()?;
			crate::timestamp::for_bitpack_decode(bytes, block, delta_count)
		}
		CASCADE_INNER_RLE => {
			let run_count = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
			let mut runs = Vec::with_capacity(run_count);
			for _ in 0..run_count {
				let value = r.read_svarint()?;
				let run_len = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
				runs.push((value, run_len));
			}
			crate::timestamp::rle_decode(&runs)
		}
		other => return Err(WeftSegError::InvalidTag { kind: "value_codec_cascade_inner", value: other }),
	};
	// Reconstruct: the anchor is the first mantissa, each delta the wrapping step to the next.
	let mut values = Vec::with_capacity(count);
	if count > 0 {
		let mut mantissa = anchor;
		values.push(PhysicalValue::ScaledI64 { mantissa, scale });
		for &delta in &deltas {
			mantissa = mantissa.wrapping_add(delta);
			values.push(PhysicalValue::ScaledI64 { mantissa, scale });
		}
	}
	Ok(values)
}

/// **Random-access single-value read** from a `.weftseg` value-column block: the
/// [`PhysicalValue`] at `index`, or `None` when `index` is past the column.
///
/// For the two per-block `ScaledI64` codecs (`VAL_CODEC_BLOCKED`, `VAL_CODEC_FOR`) this
/// reads only the block covering `index` — skipping the earlier blocks by their headers via
/// [`crate::timestamp::blocked_bitpack_decode_range`] /
/// [`crate::timestamp::for_bitpack_decode_range`] — instead of materializing the whole column;
/// the point-lookup lever the block-level random-access primitives exist for (roadmap Phase
/// 6.1). Every other codec falls back to a full [`read_value_column`] then an index, which is
/// always correct (and cheap for the fixed-width / per-value payloads). The result equals
/// `read_value_column(bytes).values.get(index).cloned()`.
///
/// # Errors
///
/// Propagates the same [`WeftSegError`]s as [`read_value_column`] (a malformed header, an
/// unrecognised tag/codec, or a short stream).
pub fn read_value_at(bytes: &[u8], index: usize) -> Result<Option<PhysicalValue>, WeftSegError> {
	let mut r = ByteReader::new(bytes);
	let tag = r.read_u8()?;
	let physical_type = match tag {
		TAG_F64 => PhysicalType::F64,
		TAG_F32 => PhysicalType::F32,
		TAG_SCALED_I64 => PhysicalType::ScaledI64 { scale: r.read_u8()? },
		TAG_SCALED_I128 => PhysicalType::ScaledI128 { scale: r.read_u8()? },
		TAG_DECIMAL128 => PhysicalType::Decimal128,
		TAG_BIGDECIMAL_TEXT => PhysicalType::BigDecimalText,
		other => return Err(WeftSegError::InvalidTag { kind: "physical_type", value: other }),
	};
	let count = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
	let _lossy_count = r.read_uvarint()?;
	let _max_abs_error = read_decimal(&mut r)?;
	if index >= count {
		return Ok(None);
	}
	match r.read_u8()? {
		VAL_CODEC_BLOCKED => {
			let PhysicalType::ScaledI64 { scale } = physical_type else {
				return Err(WeftSegError::InvalidTag { kind: "value_codec_blocked_type", value: tag });
			};
			let block = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
			let data = r.read_bytes()?;
			Ok(crate::timestamp::blocked_bitpack_decode_range(data, block, count, index, 1).first().map(|&mantissa| PhysicalValue::ScaledI64 { mantissa, scale }))
		}
		VAL_CODEC_FOR => {
			let PhysicalType::ScaledI64 { scale } = physical_type else {
				return Err(WeftSegError::InvalidTag { kind: "value_codec_for_type", value: tag });
			};
			let block = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
			let data = r.read_bytes()?;
			Ok(crate::timestamp::for_bitpack_decode_range(data, block, count, index, 1).first().map(|&mantissa| PhysicalValue::ScaledI64 { mantissa, scale }))
		}
		VAL_CODEC_BITPACK => {
			let PhysicalType::ScaledI64 { scale } = physical_type else {
				return Err(WeftSegError::InvalidTag { kind: "value_codec_bitpack_type", value: tag });
			};
			// Fixed global width → the value at `index` lives at bit `index * width`, an O(width)
			// read of the raw `count * width`-bit stream (no length prefix, as scaled_bitpack emits).
			let width = u32::from(r.read_u8()?);
			let data_len = (count * width as usize).div_ceil(8);
			let data = r.take(data_len)?;
			Ok(Some(PhysicalValue::ScaledI64 { mantissa: crate::timestamp::bitpack_decode_at(width, data, index), scale }))
		}
		VAL_CODEC_TRANSPOSED => {
			let PhysicalType::ScaledI64 { scale } = physical_type else {
				return Err(WeftSegError::InvalidTag { kind: "value_codec_transposed_type", value: tag });
			};
			// Skip whole tiles by their width headers, then decode only the covering tile. Note
			// the tile is 1024 lanes (vs the per-block codecs' 64), so a single-value read costs
			// a wider decode here — the honest cost side of the layout's decode-throughput win.
			let tile = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
			let data = r.read_bytes()?;
			Ok(crate::timestamp::transpose_bitpack_decode_range(data, tile, count, index, 1).first().map(|&mantissa| PhysicalValue::ScaledI64 { mantissa, scale }))
		}
		// Per-value / cascade payloads: a full decode then index (correct everywhere; the
		// random-access skip only helps the three fixed-layout codecs above).
		_ => Ok(read_value_column(&mut ByteReader::new(bytes))?.values.get(index).cloned()),
	}
}

/// **Random-access windowed read** from a `.weftseg` value-column block: the dense values at
/// `[start, start + len)`, clamped to the column (an empty vector when `start` is past its end).
///
/// This is the *range* sibling of [`read_value_at`], and the difference is not cosmetic. Each
/// fixed-layout codec locates a value by walking its block/tile headers **from the start of the
/// stream**, so resolving a window one value at a time with [`read_value_at`] re-walks that chain
/// per row — and for the 1024-lane `VAL_CODEC_TRANSPOSED` layout it additionally decodes a whole
/// tile to serve each single value, so an `N`-row window costs `O(N * 1024)` value decodes. Doing
/// the range decode **once** collapses that to one walk and one decode of the covering blocks:
///
/// - `VAL_CODEC_BLOCKED` / `VAL_CODEC_FOR` / `VAL_CODEC_TRANSPOSED` → the codec's own
///   `*_decode_range`, which skips the preceding blocks/tiles by their headers;
/// - `VAL_CODEC_BITPACK` → a per-index `O(width)` bit read (already cheap, no chain to walk);
/// - every other payload → a full [`read_value_column`] then a slice (always correct).
///
/// The result equals `read_value_column(bytes).values[start..start + len]` for every codec.
///
/// # Errors
///
/// Propagates the same [`WeftSegError`]s as [`read_value_column`] (a malformed header, an
/// unrecognised tag/codec, or a short stream).
pub fn read_value_range(bytes: &[u8], start: usize, len: usize) -> Result<Vec<PhysicalValue>, WeftSegError> {
	let mut r = ByteReader::new(bytes);
	let tag = r.read_u8()?;
	let physical_type = match tag {
		TAG_F64 => PhysicalType::F64,
		TAG_F32 => PhysicalType::F32,
		TAG_SCALED_I64 => PhysicalType::ScaledI64 { scale: r.read_u8()? },
		TAG_SCALED_I128 => PhysicalType::ScaledI128 { scale: r.read_u8()? },
		TAG_DECIMAL128 => PhysicalType::Decimal128,
		TAG_BIGDECIMAL_TEXT => PhysicalType::BigDecimalText,
		other => return Err(WeftSegError::InvalidTag { kind: "physical_type", value: other }),
	};
	let count = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
	let _lossy_count = r.read_uvarint()?;
	let _max_abs_error = read_decimal(&mut r)?;
	let end = start.saturating_add(len).min(count);
	if start >= end {
		return Ok(Vec::new());
	}
	let span = end - start;
	let scaled = |mantissas: Vec<i64>, scale: u8| mantissas.into_iter().map(|mantissa| PhysicalValue::ScaledI64 { mantissa, scale }).collect();
	match r.read_u8()? {
		VAL_CODEC_BLOCKED => {
			let PhysicalType::ScaledI64 { scale } = physical_type else {
				return Err(WeftSegError::InvalidTag { kind: "value_codec_blocked_type", value: tag });
			};
			let block = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
			let data = r.read_bytes()?;
			Ok(scaled(crate::timestamp::blocked_bitpack_decode_range(data, block, count, start, span), scale))
		}
		VAL_CODEC_FOR => {
			let PhysicalType::ScaledI64 { scale } = physical_type else {
				return Err(WeftSegError::InvalidTag { kind: "value_codec_for_type", value: tag });
			};
			let block = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
			let data = r.read_bytes()?;
			Ok(scaled(crate::timestamp::for_bitpack_decode_range(data, block, count, start, span), scale))
		}
		VAL_CODEC_TRANSPOSED => {
			let PhysicalType::ScaledI64 { scale } = physical_type else {
				return Err(WeftSegError::InvalidTag { kind: "value_codec_transposed_type", value: tag });
			};
			let tile = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
			let data = r.read_bytes()?;
			Ok(scaled(crate::timestamp::transpose_bitpack_decode_range(data, tile, count, start, span), scale))
		}
		VAL_CODEC_BITPACK => {
			let PhysicalType::ScaledI64 { scale } = physical_type else {
				return Err(WeftSegError::InvalidTag { kind: "value_codec_bitpack_type", value: tag });
			};
			let width = u32::from(r.read_u8()?);
			let data = r.take((count * width as usize).div_ceil(8))?;
			Ok(scaled((start..end).map(|i| crate::timestamp::bitpack_decode_at(width, data, i)).collect(), scale))
		}
		// Per-value / cascade payloads: a full decode then a slice (correct everywhere).
		//
		// The slice is taken with `get`, not by indexing: a malformed block can decode to FEWER
		// values than its header's `count` (a cascade block whose RLE inner run-lengths sum
		// short, say), and `count` is what bounded `end` above. Indexing would panic inside a
		// fallible public parser; a short decode is malformed input, so it yields whatever the
		// block actually held rather than aborting the process.
		_ => {
			let values = read_value_column(&mut ByteReader::new(bytes))?.values;
			Ok(values.get(start.min(values.len())..end.min(values.len())).unwrap_or_default().to_vec())
		}
	}
}

/// Whether a value block (a `section` positioned at its physical-type tag) uses one of the four
/// fixed-layout `ScaledI64` codecs (`VAL_CODEC_BLOCKED` / `VAL_CODEC_FOR` /
/// `VAL_CODEC_BITPACK` / `VAL_CODEC_TRANSPOSED`) that [`read_value_at`] can random-access.
/// Peeks the header + codec byte on a throwaway reader without consuming the caller's cursor.
/// `false` (including on a short/malformed header) routes the caller to the always-correct
/// full-decode fallback.
fn value_block_has_random_access_codec(section: &[u8]) -> bool {
	let mut r = ByteReader::new(section);
	let Ok(tag) = r.read_u8() else { return false };
	// Skip the optional scale byte the two scaled types carry.
	if matches!(tag, TAG_SCALED_I64 | TAG_SCALED_I128) && r.read_u8().is_err() {
		return false;
	}
	// count, lossy_count, max_abs_error, then the codec selector.
	if r.read_uvarint().is_err() || r.read_uvarint().is_err() || read_decimal(&mut r).is_err() {
		return false;
	}
	// These codecs are only defined over ScaledI64; the read paths assert it too.
	matches!((tag, r.read_u8()), (TAG_SCALED_I64, Ok(VAL_CODEC_BLOCKED | VAL_CODEC_FOR | VAL_CODEC_BITPACK | VAL_CODEC_TRANSPOSED)))
}

/// Advance `r` past a value block known to use a random-access codec (`VAL_CODEC_BLOCKED` /
/// `VAL_CODEC_FOR` / `VAL_CODEC_BITPACK` / `VAL_CODEC_TRANSPOSED`) — the header, the codec
/// byte, and the coded stream — leaving `r` at the following (timestamp) block. Only called after
/// [`value_block_has_random_access_codec`] confirmed the fast path, so the framing is exactly what
/// [`write_value_column_selected`] emits for
/// `scaled_blocked`/`scaled_for`/`scaled_bitpack`/`scaled_transposed` (the first, second and
/// fourth share the block/tile-size-uvarint + length-prefixed-stream shape).
fn skip_random_access_value_column(r: &mut ByteReader) -> Result<(), WeftSegError> {
	let tag = r.read_u8()?;
	if matches!(tag, TAG_SCALED_I64 | TAG_SCALED_I128) {
		r.read_u8()?; // scale
	}
	let count = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
	r.read_uvarint()?; // lossy_count
	read_decimal(r)?; // max_abs_error
	if r.read_u8()? == VAL_CODEC_BITPACK {
		// The fixed-width codec writes a width byte then a raw (unprefixed) count*width-bit stream.
		let width = usize::from(r.read_u8()?);
		r.take((count * width).div_ceil(8))?;
	} else {
		// The two per-block codecs write a block-size uvarint then a length-prefixed stream.
		r.read_uvarint()?; // block size
		r.read_bytes()?; // length-prefixed coded stream
	}
	Ok(())
}

/// The **dense value index** of logical `row` — the count of present rows before it. The value
/// column stores only present values, so this is the offset [`read_value_at`] / a decoded
/// present-value vector is indexed by. `O(1)` for a dense column (no nulls before it).
fn dense_rank(nulls: &NullMask, row: usize) -> usize {
	if nulls.null_count() == 0 {
		row
	} else {
		(0..row).filter(|&i| nulls.is_present(i)).count()
	}
}

/// Locate the **dense value index** of the first *present* row whose timestamp equals `t`, or
/// [`None`] when no present row carries `t` — the shared core of the streaming point read
/// (mirrors [`crate::segment::point_lookup`] + the dense-rank mapping [`Segment::decode_nullable`]
/// uses). A time-sorted column binary-searches the run of `t` for its first present row; an
/// out-of-order column linear-scans (the only sound search on unsorted timestamps).
fn locate_present_dense_index(timestamps: &[i64], nulls: &NullMask, sorted: bool, t: i64) -> Option<usize> {
	let row = if sorted {
		let mut row = timestamps.partition_point(|&x| x < t);
		loop {
			if row >= timestamps.len() || timestamps[row] != t {
				return None;
			}
			if nulls.is_present(row) {
				break row;
			}
			row += 1;
		}
	} else {
		timestamps.iter().enumerate().find_map(|(i, &ts)| (ts == t && nulls.is_present(i)).then_some(i))?
	};
	Some(dense_rank(nulls, row))
}

/// **Streaming batch point read over one column section** — a `section` positioned at its value
/// block (value + timestamp + quality columns, exactly a single-block frame's column region or one
/// paged-frame page block), resolving each `ts[k]` to the first present value at that instant
/// (matching [`Segment::value_at`] / [`crate::page::Page::value_at`]), aligned to `ts`.
///
/// The whole point of batching: the timestamp column + quality mask (and, on the fallback codec,
/// the value column) are decoded **once** and reused for every requested instant — a lookup of `N`
/// instants in one segment pays one timestamp decode, not `N`. On a per-block value codec the value
/// block is skipped by its framing and each covering block unpacked via [`read_value_at`]; every
/// other codec decodes the value column whole and indexes it. A section with no rows (or no `ts` in
/// its coarse span) is answered all-`None` without decoding a column byte.
fn read_points_from_section(section: &[u8], stats: &SegmentStats, ts: &[i64]) -> Result<Vec<Option<BigDecimal>>, WeftSegError> {
	let mut out = vec![None; ts.len()];
	// Coarse span bail — decode nothing unless some requested instant falls inside the segment.
	let (Some(min_ts), Some(max_ts)) = (stats.min_ts, stats.max_ts) else { return Ok(out) };
	if !ts.iter().any(|&t| min_ts <= t && t <= max_ts) {
		return Ok(out);
	}
	let fast = value_block_has_random_access_codec(section);
	let mut r = ByteReader::new(section);
	// Advance past the value block to the timestamp block: skip it on the fast path (keeping the
	// values on disk), fully decode it on the fallback (keeping the present values in hand).
	let present_values = if fast {
		skip_random_access_value_column(&mut r)?;
		None
	} else {
		Some(read_value_column(&mut r)?.values)
	};
	// **Lazy checkpointed path:** if the frame carries a persisted index over a
	// range-decodable dod stream *and* the column is sorted (a binary search is
	// meaningless otherwise), resolve each instant without decoding the timestamp column
	// at all — neither the codec stream nor the cumulative sum. Only a frame written by
	// the opt-in `write_segment_checkpointed` takes this branch.
	let lazy = if stats.time_sorted { read_lazy_checkpointed_ts(&mut r)? } else { None };
	// Otherwise decode the timestamp column as before.
	let ts_col = if lazy.is_none() { Some(read_timestamp_column(&mut r)?) } else { None };
	let nulls = read_null_column(&mut r, stats.row_count, stats.null_count)?;
	// **Regular-column fast path:** a time-sorted constant-stride column resolves each instant's
	// row in closed form (`(t - first) / step`), skipping the whole delta-of-delta reconstruction +
	// binary search. `time_sorted` guarantees `step > 0` and no wrap-around, so the index is unique
	// and exact. An irregular (or out-of-order) column decodes the timestamps and binary/linear
	// searches as before.
	let stride = if stats.time_sorted { ts_col.as_ref().and_then(DeltaOfDeltaColumn::arithmetic_stride).filter(|&(_, step)| step > 0) } else { None };
	let timestamps = if stride.is_none() { ts_col.as_ref().map(crate::timestamp::decode_delta_of_delta) } else { None };
	for (slot, &t) in out.iter_mut().zip(ts) {
		if t < min_ts || t > max_ts {
			continue;
		}
		let dense_index = if let Some((first, step)) = stride {
			// Closed form: the row whose timestamp is exactly `t`, if `t` lands on the grid.
			let diff = t.wrapping_sub(first);
			if diff % step != 0 {
				continue;
			}
			let quotient = diff / step;
			// `try_from` rejects a negative index; then bound it, guard against wrapping by
			// reconstructing, and require the row present.
			let Ok(row) = usize::try_from(quotient) else { continue };
			if row >= stats.row_count || first.wrapping_add(quotient.wrapping_mul(step)) != t || !nulls.is_present(row) {
				continue;
			}
			dense_rank(&nulls, row)
		} else if let Some(lz) = lazy.as_ref() {
			// Resume from the nearest checkpoint and range-decode only the dods walked,
			// skipping null rows across a run of duplicate timestamps exactly as
			// `locate_present_dense_index` does.
			let Some(row) = lz.lookup(t, |row| row < stats.row_count && nulls.is_present(row)) else {
				continue;
			};
			dense_rank(&nulls, row)
		} else {
			let Some(idx) = locate_present_dense_index(timestamps.as_ref().expect("decoded when irregular"), &nulls, stats.time_sorted, t) else {
				continue;
			};
			idx
		};
		let value = match &present_values {
			// Fast path: random-access the single covering block straight from the section bytes.
			None => read_value_at(section, dense_index)?,
			// Fallback: index the already-decoded present values.
			Some(values) => values.get(dense_index).cloned(),
		};
		*slot = value.map(|pv| pv.to_logical());
	}
	Ok(out)
}

/// **Streaming point read over one column section** — the single-instant case of
/// [`read_points_from_section`]. See it for the value-block-skip mechanics.
fn read_point_from_section(section: &[u8], stats: &SegmentStats, t: i64) -> Result<Option<BigDecimal>, WeftSegError> {
	Ok(read_points_from_section(section, stats, &[t])?.pop().flatten())
}

/// **Streaming single-value point read** from a single-block `.weftseg` frame — exactly
/// [`Segment::value_at`]'s answer, **without materializing the value column**.
///
/// The first *present* value whose timestamp equals `t`, read via the per-block random-access
/// codecs (`VAL_CODEC_BLOCKED` / `VAL_CODEC_FOR`) when the value block uses one; every other
/// codec falls back to a full value-column decode + index. Either way the result equals
/// `read_segment(bytes)?.value_at(t)` for every single-block frame. This is the point-lookup
/// lever the block-level random-access primitives (and [`read_value_at`]) exist for, wired one
/// level up to the framed segment (roadmap Phase 4/6). See `read_point_from_section`.
///
/// # Errors
///
/// Propagates the same [`WeftSegError`]s as [`read_segment`] (checksum mismatch, bad magic,
/// unsupported version — this reader is single-block-only, so a paged frame is a version error —
/// or a malformed column/stat body).
pub fn read_segment_point(bytes: &[u8], t: i64) -> Result<Option<BigDecimal>, WeftSegError> {
	// Verify the trailing CRC over the body before trusting any field (as read_segment does).
	if bytes.len() < 4 {
		return Err(WeftSegError::UnexpectedEof { needed: 4, remaining: bytes.len() });
	}
	let (body, crc_bytes) = bytes.split_at(bytes.len() - 4);
	let stored = u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
	let computed = crc32(body);
	if stored != computed {
		return Err(WeftSegError::ChecksumMismatch { stored, computed });
	}
	let mut r = ByteReader::new(body);
	if !is_known_magic(r.take(MAGIC.len())?) {
		return Err(WeftSegError::BadMagic);
	}
	let version = r.read_u16_le()?;
	if version != SEGMENT_FORMAT_VERSION {
		return Err(WeftSegError::UnsupportedVersion { found: version });
	}
	let stats = read_segment_stats(&mut r)?;
	// The value block begins at the current offset; the column region runs from here to the CRC.
	read_point_from_section(&body[r.pos..], &stats, t)
}

/// **Streaming single-value point read** from a **paged** `.weftseg` frame — exactly
/// [`PagedSegment::value_at`]'s answer, without materializing the pages a point lookup skips.
///
/// The per-page index (each page's stats + block byte length) is parsed first, so pages whose
/// `[min_ts, max_ts]` cannot contain `t` are pruned *without decoding a single column byte*
/// (on-disk intra-segment page skipping). Each surviving page — in page order, so the first
/// present value wins, matching [`PagedSegment::value_at`]'s `find_map` — is resolved by the
/// shared `read_point_from_section` over just that page's block (the block-skip value read
/// applies within the page too). For a time-sorted paged segment at most one page survives, so
/// a point lookup reads one page's timestamp column and one value block regardless of segment
/// size.
///
/// # Errors
///
/// Propagates the same [`WeftSegError`]s as [`read_paged_segment`] (checksum mismatch, bad magic,
/// unsupported version — this reader is paged-only — or a malformed index/column body).
pub fn read_paged_segment_point(bytes: &[u8], t: i64) -> Result<Option<BigDecimal>, WeftSegError> {
	if bytes.len() < 4 {
		return Err(WeftSegError::UnexpectedEof { needed: 4, remaining: bytes.len() });
	}
	let (body, crc_bytes) = bytes.split_at(bytes.len() - 4);
	let stored = u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
	let computed = crc32(body);
	if stored != computed {
		return Err(WeftSegError::ChecksumMismatch { stored, computed });
	}
	let mut r = ByteReader::new(body);
	if !is_known_magic(r.take(MAGIC.len())?) {
		return Err(WeftSegError::BadMagic);
	}
	let version = r.read_u16_le()?;
	if version != PAGED_SEGMENT_FORMAT_VERSION {
		return Err(WeftSegError::UnsupportedVersion { found: version });
	}
	let _rows_per_page = r.read_uvarint()?;
	let seg_stats = read_segment_stats(&mut r)?;
	// Coarse bail on the whole segment's span before touching the index.
	match (seg_stats.min_ts, seg_stats.max_ts) {
		(Some(lo), Some(hi)) if lo <= t && t <= hi => {}
		_ => return Ok(None),
	}
	let page_count = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
	// Parse the per-page index (stats + block length); the page data blocks follow it, so track
	// the running byte offset of each block within `body`.
	let mut index: Vec<(SegmentStats, usize)> = Vec::with_capacity(page_count);
	for _ in 0..page_count {
		let page_stats = read_segment_stats(&mut r)?;
		let block_len = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
		index.push((page_stats, block_len));
	}
	let mut block_start = r.pos;
	for (page_stats, block_len) in index {
		let contains = matches!((page_stats.min_ts, page_stats.max_ts), (Some(lo), Some(hi)) if lo <= t && t <= hi);
		if contains {
			// Bound the section to this page's block so read_value_at cannot read past it.
			let section = body.get(block_start..block_start + block_len).ok_or_else(|| WeftSegError::UnexpectedEof { needed: block_len, remaining: body.len().saturating_sub(block_start) })?;
			if let Some(value) = read_point_from_section(section, &page_stats, t)? {
				return Ok(Some(value));
			}
		}
		block_start += block_len;
	}
	Ok(None)
}

/// **Streaming batch point read** from a single-block `.weftseg` frame — [`read_segment_point`] for
/// many instants at once, returning one value per `ts[k]` (aligned to `ts`).
///
/// The frame is read once and its timestamp column decoded once for the whole batch (a value-block
/// skip per instant on a random-access codec), so `N` instants cost one timestamp decode rather
/// than `N`. Each element equals `read_segment_point(bytes, ts[k])`.
///
/// # Errors
///
/// Propagates the same [`WeftSegError`]s as [`read_segment`].
pub fn read_segment_points(bytes: &[u8], ts: &[i64]) -> Result<Vec<Option<BigDecimal>>, WeftSegError> {
	if bytes.len() < 4 {
		return Err(WeftSegError::UnexpectedEof { needed: 4, remaining: bytes.len() });
	}
	let (body, crc_bytes) = bytes.split_at(bytes.len() - 4);
	let stored = u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
	let computed = crc32(body);
	if stored != computed {
		return Err(WeftSegError::ChecksumMismatch { stored, computed });
	}
	let mut r = ByteReader::new(body);
	if !is_known_magic(r.take(MAGIC.len())?) {
		return Err(WeftSegError::BadMagic);
	}
	let version = r.read_u16_le()?;
	if version != SEGMENT_FORMAT_VERSION {
		return Err(WeftSegError::UnsupportedVersion { found: version });
	}
	let stats = read_segment_stats(&mut r)?;
	read_points_from_section(&body[r.pos..], &stats, ts)
}

/// **Streaming batch point read** from a **paged** `.weftseg` frame — [`read_paged_segment_point`]
/// for many instants at once, returning one value per `ts[k]` (aligned to `ts`).
///
/// The per-page index is parsed once; each page is decoded **at most once** for the whole batch,
/// and only when some still-unresolved instant falls in its span (on-disk page skipping preserved).
/// An instant takes the first page (in page order) that yields a present value, matching
/// [`PagedSegment::value_at`]. Each element equals `read_paged_segment_point(bytes, ts[k])`.
///
/// # Errors
///
/// Propagates the same [`WeftSegError`]s as [`read_paged_segment`].
pub fn read_paged_segment_points(bytes: &[u8], ts: &[i64]) -> Result<Vec<Option<BigDecimal>>, WeftSegError> {
	let mut out = vec![None; ts.len()];
	if bytes.len() < 4 {
		return Err(WeftSegError::UnexpectedEof { needed: 4, remaining: bytes.len() });
	}
	let (body, crc_bytes) = bytes.split_at(bytes.len() - 4);
	let stored = u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
	let computed = crc32(body);
	if stored != computed {
		return Err(WeftSegError::ChecksumMismatch { stored, computed });
	}
	let mut r = ByteReader::new(body);
	if !is_known_magic(r.take(MAGIC.len())?) {
		return Err(WeftSegError::BadMagic);
	}
	let version = r.read_u16_le()?;
	if version != PAGED_SEGMENT_FORMAT_VERSION {
		return Err(WeftSegError::UnsupportedVersion { found: version });
	}
	let _rows_per_page = r.read_uvarint()?;
	let seg_stats = read_segment_stats(&mut r)?;
	let (Some(seg_lo), Some(seg_hi)) = (seg_stats.min_ts, seg_stats.max_ts) else { return Ok(out) };
	if !ts.iter().any(|&t| seg_lo <= t && t <= seg_hi) {
		return Ok(out);
	}
	let page_count = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
	let mut index: Vec<(SegmentStats, usize)> = Vec::with_capacity(page_count);
	for _ in 0..page_count {
		let page_stats = read_segment_stats(&mut r)?;
		let block_len = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
		index.push((page_stats, block_len));
	}
	let mut resolved = vec![false; ts.len()];
	let mut block_start = r.pos;
	for (page_stats, block_len) in index {
		let (Some(page_lo), Some(page_hi)) = (page_stats.min_ts, page_stats.max_ts) else {
			block_start += block_len;
			continue;
		};
		// Decode the page only if some still-unresolved instant falls in its span (page skipping).
		if ts.iter().enumerate().any(|(k, &t)| !resolved[k] && page_lo <= t && t <= page_hi) {
			let section = body.get(block_start..block_start + block_len).ok_or_else(|| WeftSegError::UnexpectedEof { needed: block_len, remaining: body.len().saturating_sub(block_start) })?;
			let hits = read_points_from_section(section, &page_stats, ts)?;
			for ((slot, resolved), hit) in out.iter_mut().zip(resolved.iter_mut()).zip(hits) {
				if !*resolved && hit.is_some() {
					*slot = hit;
					*resolved = true;
				}
			}
		}
		block_start += block_len;
	}
	Ok(out)
}

/// **Windowed range read over one column section** — the `(timestamp, value)` rows of a `section`
/// (value + timestamp + quality columns) whose timestamp falls in the inclusive `[start, end]`
/// window, aligned and in row order. The shared core of the single-block and paged range reads.
///
/// For a **regular (constant-stride) sorted column with a random-access value codec** the window is
/// resolved without materializing the whole section: the row range `[lo, hi]` is computed in closed
/// form from the stride (`ts[i] = first + i*step`), the timestamps are generated directly, and only
/// the present values inside the window are unpacked via [`read_value_at`] (one block per present
/// row). Any other shape (irregular timestamps, a per-value/cascade value codec, an out-of-order
/// segment) fully decodes the section and filters, which is always correct.
fn read_range_from_section(section: &[u8], stats: &SegmentStats, start: i64, end: i64) -> Result<(Vec<i64>, Vec<Option<BigDecimal>>), WeftSegError> {
	// Coarse span bail — a window disjoint from the section yields nothing.
	let (Some(min_ts), Some(max_ts)) = (stats.min_ts, stats.max_ts) else { return Ok((Vec::new(), Vec::new())) };
	if end < min_ts || start > max_ts {
		return Ok((Vec::new(), Vec::new()));
	}
	// A full decode of the section then a filter — the always-correct fallback for every shape the
	// closed-form window below does not cover.
	let full_filtered = || -> Result<(Vec<i64>, Vec<Option<BigDecimal>>), WeftSegError> {
		let mut fr = ByteReader::new(section);
		let col = read_value_column(&mut fr)?;
		let ts_col = read_timestamp_column(&mut fr)?;
		let timestamps = crate::timestamp::decode_delta_of_delta(&ts_col);
		let nulls = read_null_column(&mut fr, stats.row_count, stats.null_count)?;
		let (mut out_times, mut out_values) = (Vec::new(), Vec::new());
		let mut dense = 0;
		for (row, &t) in timestamps.iter().enumerate() {
			let present = nulls.is_present(row);
			if start <= t && t <= end {
				out_times.push(t);
				out_values.push(if present { col.values.get(dense).cloned().map(|pv| pv.to_logical()) } else { None });
			}
			if present {
				dense += 1;
			}
		}
		Ok((out_times, out_values))
	};
	// The closed-form fast path needs a random-access value codec (per-present-row read) and a
	// sorted, constant-stride timestamp column.
	if !value_block_has_random_access_codec(section) {
		return full_filtered();
	}
	let mut sr = ByteReader::new(section);
	skip_random_access_value_column(&mut sr)?;
	let ts_col = read_timestamp_column(&mut sr)?;
	let nulls = read_null_column(&mut sr, stats.row_count, stats.null_count)?;
	let stride = if stats.time_sorted { ts_col.arithmetic_stride().filter(|&(_, step)| step > 0) } else { None };
	let Some((first, step)) = stride else {
		return full_filtered();
	};
	// Closed-form row window: lo = first row with ts >= start, hi = last row with ts <= end.
	let step128 = i128::from(step);
	let start_off = i128::from(start) - i128::from(first);
	let lo: usize = if start_off <= 0 {
		0
	} else {
		let (q, rem) = (start_off / step128, start_off % step128);
		usize::try_from(if rem == 0 { q } else { q + 1 }).unwrap_or(usize::MAX)
	};
	let end_off = i128::from(end) - i128::from(first);
	if end_off < 0 {
		return Ok((Vec::new(), Vec::new()));
	}
	let hi = usize::try_from(end_off / step128).unwrap_or(usize::MAX).min(stats.row_count - 1);
	if lo > hi {
		return Ok((Vec::new(), Vec::new()));
	}
	// Generate the window: timestamps in closed form, and the window's present values decoded in
	// ONE range read. Reading them one at a time (`read_value_at` per row) would re-walk the
	// codec's block/tile header chain from the start of the stream for every row — and for the
	// 1024-lane transposed layout would decode a whole tile per value, making a windowed read
	// dramatically slower than the full decode it is supposed to beat.
	let dense_lo = dense_rank(&nulls, lo);
	let dense_hi = dense_rank(&nulls, hi + 1);
	let window = read_value_range(section, dense_lo, dense_hi.saturating_sub(dense_lo))?;
	let mut timestamps = Vec::with_capacity(hi - lo + 1);
	let mut values = Vec::with_capacity(hi - lo + 1);
	let mut dense = dense_lo;
	for row in lo..=hi {
		let offset = i64::try_from(row).unwrap_or(i64::MAX);
		timestamps.push(first.wrapping_add(offset.wrapping_mul(step)));
		if nulls.is_present(row) {
			values.push(window.get(dense - dense_lo).cloned().map(|pv| pv.to_logical()));
			dense += 1;
		} else {
			values.push(None);
		}
	}
	Ok((timestamps, values))
}

/// **Windowed range read** from a single-block `.weftseg` frame — the rows in `[start, end]`.
///
/// Returns the `(timestamp, value)` rows whose timestamp falls in the inclusive `[start, end]`
/// window, aligned and in row order — exactly `read_segment(bytes)?.decode_nullable()` filtered to
/// `[start, end]`. A **regular block-coded segment** resolves the window in closed form and unpacks
/// only its present values (see `read_range_from_section`); any other shape full-decodes + filters.
/// Roadmap Phase 4/6 (the range-read analogue of the streaming point read).
///
/// # Errors
///
/// Propagates the same [`WeftSegError`]s as [`read_segment`].
pub fn read_segment_range(bytes: &[u8], start: i64, end: i64) -> Result<(Vec<i64>, Vec<Option<BigDecimal>>), WeftSegError> {
	if bytes.len() < 4 {
		return Err(WeftSegError::UnexpectedEof { needed: 4, remaining: bytes.len() });
	}
	let (body, crc_bytes) = bytes.split_at(bytes.len() - 4);
	let stored = u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
	let computed = crc32(body);
	if stored != computed {
		return Err(WeftSegError::ChecksumMismatch { stored, computed });
	}
	let mut r = ByteReader::new(body);
	if !is_known_magic(r.take(MAGIC.len())?) {
		return Err(WeftSegError::BadMagic);
	}
	let version = r.read_u16_le()?;
	if version != SEGMENT_FORMAT_VERSION {
		return Err(WeftSegError::UnsupportedVersion { found: version });
	}
	let stats = read_segment_stats(&mut r)?;
	read_range_from_section(&body[r.pos..], &stats, start, end)
}

/// **Windowed range read** from a **paged** `.weftseg` frame — the rows in `[start, end]`.
///
/// The per-page index is parsed once, so pages whose `[min_ts, max_ts]` is disjoint from the window
/// are skipped without decoding a column byte (on-disk page skipping, as [`read_paged_segment`]'s
/// range read); each surviving page is windowed through the shared `read_range_from_section` (a
/// regular page resolves its sub-window in closed form). Rows are concatenated in page order.
/// Equal to `read_paged_segment(bytes)?.read_time_range(start, end)`.
///
/// # Errors
///
/// Propagates the same [`WeftSegError`]s as [`read_paged_segment`].
pub fn read_paged_segment_range(bytes: &[u8], start: i64, end: i64) -> Result<(Vec<i64>, Vec<Option<BigDecimal>>), WeftSegError> {
	if bytes.len() < 4 {
		return Err(WeftSegError::UnexpectedEof { needed: 4, remaining: bytes.len() });
	}
	let (body, crc_bytes) = bytes.split_at(bytes.len() - 4);
	let stored = u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
	let computed = crc32(body);
	if stored != computed {
		return Err(WeftSegError::ChecksumMismatch { stored, computed });
	}
	let mut r = ByteReader::new(body);
	if !is_known_magic(r.take(MAGIC.len())?) {
		return Err(WeftSegError::BadMagic);
	}
	let version = r.read_u16_le()?;
	if version != PAGED_SEGMENT_FORMAT_VERSION {
		return Err(WeftSegError::UnsupportedVersion { found: version });
	}
	let _rows_per_page = r.read_uvarint()?;
	let seg_stats = read_segment_stats(&mut r)?;
	let (mut timestamps, mut values) = (Vec::new(), Vec::new());
	// Coarse bail on the whole segment span.
	match (seg_stats.min_ts, seg_stats.max_ts) {
		(Some(lo), Some(hi)) if end >= lo && start <= hi => {}
		_ => return Ok((timestamps, values)),
	}
	let page_count = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
	let mut index: Vec<(SegmentStats, usize)> = Vec::with_capacity(page_count);
	for _ in 0..page_count {
		let page_stats = read_segment_stats(&mut r)?;
		let block_len = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
		index.push((page_stats, block_len));
	}
	let mut block_start = r.pos;
	for (page_stats, block_len) in index {
		let overlaps = matches!((page_stats.min_ts, page_stats.max_ts), (Some(lo), Some(hi)) if end >= lo && start <= hi);
		if overlaps {
			let section = body.get(block_start..block_start + block_len).ok_or_else(|| WeftSegError::UnexpectedEof { needed: block_len, remaining: body.len().saturating_sub(block_start) })?;
			let (pt, pv) = read_range_from_section(section, &page_stats, start, end)?;
			timestamps.extend(pt);
			values.extend(pv);
		}
		block_start += block_len;
	}
	Ok((timestamps, values))
}

// ---------------------------------------------------------------------------
// Timestamp-column codec (Phase 4.3, slice 3)
//
// The on-disk byte block for a `DeltaOfDeltaColumn`: a one-byte time-unit tag,
// the full-width `i64` anchor, an optional first delta (a presence flag then a
// signed varint), the second-difference count (varint), then a codec selector
// byte and the coded stream. Four codecs are realized, one chosen per stream by
// `best_encoding_name` (the single source of truth): per-value zig-zag varints
// (`TS_CODEC_VARINT`), fixed-width bit-packing (`TS_CODEC_BITPACK`: a width byte +
// packed bits), Gorilla variable-length (`TS_CODEC_GORILLA`: a length-prefixed
// bucketed bitstream, for scattered jitter), and run-length coding
// (`TS_CODEC_RLE`: a run count + `(value, length)` pairs, for long constant runs).
// Every codec is exact and lossless, so the reported codec name always matches the
// bytes on disk.
// ---------------------------------------------------------------------------

const TIME_UNIT_SECONDS: u8 = 0;
const TIME_UNIT_MILLIS: u8 = 1;
const TIME_UNIT_MICROS: u8 = 2;
const TIME_UNIT_NANOS: u8 = 3;

/// Timestamp second-difference codec tags (the self-describing selector byte in a
/// v3+ timestamp block).
const TS_CODEC_VARINT: u8 = 0;
/// Fixed-width bit-packing: a width byte then `ceil(count * width / 8)` data bytes.
const TS_CODEC_BITPACK: u8 = 1;
/// Gorilla-style variable-length coding (roadmap Phase 6.1): a length-prefixed
/// [`crate::timestamp::encode_gorilla_dods`] bitstream. Chosen for scattered single
/// jitter, where every value is a bounded bucket and RLE cannot form runs.
const TS_CODEC_GORILLA: u8 = 2;
/// Run-length coding of the second differences (roadmap Phase 6.1): a run count then
/// `(svarint value, uvarint length)` pairs. Chosen for a long constant run (a
/// piecewise-regular series), where a handful of runs beat every per-value codec.
const TS_CODEC_RLE: u8 = 3;
/// Per-block adaptive (dynamic) bit-packing (roadmap Phase 6.1): a uvarint block size
/// then a length-prefixed [`crate::timestamp::blocked_bitpack_encode`] stream. Chosen
/// for a mixed-magnitude stream — a contiguous wide region among narrow runs — where a
/// single global width overpays and RLE/Gorilla do not fit.
const TS_CODEC_BLOCKED: u8 = 4;
/// **Not a codec — a wrapper** (roadmap Phase 4/6): a persisted sparse
/// [`DodCheckpoints`] index (stride, entry count, then delta-encoded
/// `(row, timestamp, delta)` entries) followed by the *inner* codec-tagged stream that
/// actually holds the dods. It lets a point lookup on an irregular sorted column resume
/// the reconstruction mid-stream instead of replaying it from the anchor.
///
/// Written only by the opt-in [`write_timestamp_column_checkpointed`]; **additive**, so
/// every frame written before it still reads unchanged (no format-version bump).
const TS_CODEC_CHECKPOINTED: u8 = 5;

/// The stable one-byte on-disk tag for a [`TimeUnit`].
const fn time_unit_tag(unit: TimeUnit) -> u8 {
	match unit {
		TimeUnit::Seconds => TIME_UNIT_SECONDS,
		TimeUnit::Millis => TIME_UNIT_MILLIS,
		TimeUnit::Micros => TIME_UNIT_MICROS,
		TimeUnit::Nanos => TIME_UNIT_NANOS,
	}
}

/// Recover a [`TimeUnit`] from its on-disk tag.
const fn time_unit_from_tag(tag: u8) -> Result<TimeUnit, WeftSegError> {
	match tag {
		TIME_UNIT_SECONDS => Ok(TimeUnit::Seconds),
		TIME_UNIT_MILLIS => Ok(TimeUnit::Millis),
		TIME_UNIT_MICROS => Ok(TimeUnit::Micros),
		TIME_UNIT_NANOS => Ok(TimeUnit::Nanos),
		other => Err(WeftSegError::InvalidTag { kind: "time_unit", value: other }),
	}
}

/// Write a [`DeltaOfDeltaColumn`] as a `.weftseg` timestamp-column block.
///
/// The second-difference stream is written under whichever codec is smallest for it —
/// **Gorilla** variable-length (scattered single jitter), **RLE** (long constant runs),
/// fixed-width **bit-packing** (a regular or small-jitter series), or per-value
/// **varint** — selected by a self-describing codec byte so the reader dispatches
/// without re-deriving the choice. The codec is chosen by routing through
/// [`DeltaOfDeltaColumn::best_encoding_name`], the single source of truth, so the
/// segment's reported codec name always matches the bytes actually written. Every
/// codec is exact and lossless, and each realizes the bytes/point saving the estimate
/// projects, not just reports it.
pub fn write_timestamp_column(w: &mut ByteWriter, col: &DeltaOfDeltaColumn) {
	w.put_u8(time_unit_tag(col.unit));
	w.put_i64_le(col.first);
	match col.first_delta {
		Some(delta) => {
			w.put_u8(1);
			w.put_svarint(delta);
		}
		None => w.put_u8(0),
	}
	w.put_uvarint(col.dods.len() as u64);
	write_dod_codec(w, col);
}

/// Write a `.weftseg` timestamp-column block **with a persisted sparse checkpoint index**.
///
/// Uses `TS_CODEC_CHECKPOINTED`, so a point lookup on an irregular sorted column can
/// skip the whole timestamp reconstruction (see [`DodCheckpoints`]).
///
/// **Opt-in**, and deliberately so: the index costs bytes the default writer does not
/// spend, so adopting it by default is a bytes/point (headline) change and is owner-gated
/// — exactly as the delta-cascade value codec is. Frames written by
/// [`write_timestamp_column`] are byte-for-byte unchanged, and this codec tag is
/// **additive**: every previously written frame still reads, so no format-version bump is
/// needed.
///
/// The index is only worth writing for a *sorted, irregular* column of a useful size; a
/// regular column already resolves in `O(1)` closed form and a tiny column decodes
/// trivially. When the index would be empty (fewer than two rows), this degrades to the
/// plain block.
pub fn write_timestamp_column_checkpointed(w: &mut ByteWriter, col: &DeltaOfDeltaColumn, stride: usize) {
	let checkpoints = col.checkpoints(stride);
	if checkpoints.points.is_empty() {
		write_timestamp_column(w, col);
		return;
	}
	w.put_u8(time_unit_tag(col.unit));
	w.put_i64_le(col.first);
	match col.first_delta {
		Some(delta) => {
			w.put_u8(1);
			w.put_svarint(delta);
		}
		None => w.put_u8(0),
	}
	w.put_uvarint(col.dods.len() as u64);
	w.put_u8(TS_CODEC_CHECKPOINTED);
	w.put_uvarint(checkpoints.stride as u64);
	// (the index, then a *random-access* inner stream — see below)
	w.put_uvarint(checkpoints.points.len() as u64);
	// Delta-encode the index against the previous entry: rows ascend and a checkpointed
	// column is sorted, so both stay small varints instead of full-width values.
	let (mut prev_row, mut prev_ts) = (0_usize, 0_i64);
	for cp in &checkpoints.points {
		w.put_uvarint((cp.row - prev_row) as u64);
		w.put_svarint(cp.timestamp.wrapping_sub(prev_ts));
		w.put_svarint(cp.delta);
		prev_row = cp.row;
		prev_ts = cp.timestamp;
	}
	// The inner stream is **forced to the per-block codec**, not `best_encoding_name`'s
	// pick. This is the whole point: the index alone only skips the cumulative-sum
	// reconstruction, which measurement showed is ~1% of a real point read — the *decode*
	// of the dod stream dominates. `TS_CODEC_BLOCKED` is the codec that can be
	// range-decoded (`blocked_bitpack_decode_range` skips whole blocks by their width
	// headers), so a probe touches only the blocks around its checkpoint and the decode
	// becomes sublinear too. A variable-length codec (varint/Gorilla/RLE) has no block
	// boundaries to skip to, so it could never deliver that.
	//
	// The trade: this may write more bytes than the best codec would (measured alongside
	// the lookup win in `benches/dodsearch.rs`) — another reason the path is opt-in.
	let block = crate::timestamp::BLOCKED_BITPACK_BLOCK;
	w.put_u8(TS_CODEC_BLOCKED);
	w.put_uvarint(block as u64);
	w.put_bytes(&crate::timestamp::blocked_bitpack_encode(&col.dods, block));
}

/// Write the `[codec tag][stream]` body of a second-difference column, choosing the
/// codec via [`DeltaOfDeltaColumn::best_encoding_name`].
fn write_dod_codec(w: &mut ByteWriter, col: &DeltaOfDeltaColumn) {
	match col.best_encoding_name() {
		"delta_of_delta_gorilla" => {
			w.put_u8(TS_CODEC_GORILLA);
			// Length-prefixed: a Gorilla stream's length is not derivable from the
			// count (variable per value), so the block carries it explicitly.
			w.put_bytes(&crate::timestamp::encode_gorilla_dods(&col.dods));
		}
		"delta_of_delta_rle" => {
			let runs = crate::timestamp::rle_encode(&col.dods);
			w.put_u8(TS_CODEC_RLE);
			w.put_uvarint(runs.len() as u64);
			for (value, count) in runs {
				w.put_svarint(value);
				w.put_uvarint(count as u64);
			}
		}
		"delta_of_delta_blocked" => {
			let block = crate::timestamp::BLOCKED_BITPACK_BLOCK;
			w.put_u8(TS_CODEC_BLOCKED);
			w.put_uvarint(block as u64);
			// Length-prefixed: per-block widths vary, so the block boundaries are not
			// derivable from the count alone without walking the stream — the prefix lets
			// the reader bound it in one step (as the Gorilla block does).
			w.put_bytes(&crate::timestamp::blocked_bitpack_encode(&col.dods, block));
		}
		"delta_of_delta_bitpack" => {
			let (width, packed) = crate::timestamp::bitpack_encode(&col.dods);
			w.put_u8(TS_CODEC_BITPACK);
			// Width is 0..=64 by construction, so the conversion never saturates.
			w.put_u8(u8::try_from(width).unwrap_or(64));
			// Raw (no length prefix): the reader derives the length from count * width.
			w.put_raw(&packed);
		}
		// "delta_of_delta" — per-value zig-zag varint, the general fallback.
		_ => {
			w.put_u8(TS_CODEC_VARINT);
			for &dod in &col.dods {
				w.put_svarint(dod);
			}
		}
	}
}

/// Read a [`DeltaOfDeltaColumn`] from a `.weftseg` timestamp-column block — the exact
/// inverse of [`write_timestamp_column`].
///
/// # Errors
///
/// [`WeftSegError::InvalidTag`] for an unrecognised time-unit or codec tag, or
/// [`WeftSegError::UnexpectedEof`] / [`WeftSegError::VarintTooLong`] on a short or
/// malformed stream.
pub fn read_timestamp_column(r: &mut ByteReader) -> Result<DeltaOfDeltaColumn, WeftSegError> {
	let unit = time_unit_from_tag(r.read_u8()?)?;
	let first = r.read_i64_le()?;
	let first_delta = match r.read_u8()? {
		0 => None,
		_ => Some(r.read_svarint()?),
	};
	let count = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
	let (dods, _) = read_dod_codec(r, count)?;
	Ok(DeltaOfDeltaColumn { first, first_delta, dods, unit })
}

/// Read a [`DeltaOfDeltaColumn`] together with its persisted [`DodCheckpoints`], if the
/// block carries them (i.e. it was written by [`write_timestamp_column_checkpointed`]).
///
/// The exact same bytes [`read_timestamp_column`] accepts — that one simply discards the
/// index. This is the read the streaming point path uses to skip the timestamp
/// reconstruction on an irregular sorted column.
///
/// # Errors
///
/// As [`read_timestamp_column`].
pub fn read_timestamp_column_with_checkpoints(r: &mut ByteReader) -> Result<(DeltaOfDeltaColumn, Option<DodCheckpoints>), WeftSegError> {
	let unit = time_unit_from_tag(r.read_u8()?)?;
	let first = r.read_i64_le()?;
	let first_delta = match r.read_u8()? {
		0 => None,
		_ => Some(r.read_svarint()?),
	};
	let count = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
	let (dods, checkpoints) = read_dod_codec(r, count)?;
	Ok((DeltaOfDeltaColumn { first, first_delta, dods, unit }, checkpoints))
}

/// A checkpointed timestamp block parsed **without decoding the dod stream**: the sparse
/// index plus a borrowed handle on the undecoded per-block stream, so a probe
/// range-decodes only the blocks it needs.
///
/// This is what makes the checkpoint index actually pay: skipping the cumulative-sum
/// reconstruction alone is ~1% of a real point read, because the dod *decode* dominates.
struct LazyCheckpointedTs<'a> {
	/// The anchor + first delta + unit — everything but the dods.
	first: i64,
	first_delta: i64,
	checkpoints: DodCheckpoints,
	/// The undecoded `blocked_bitpack_encode` dod stream.
	dod_bytes: &'a [u8],
	block: usize,
	count: usize,
}

impl LazyCheckpointedTs<'_> {
	/// The first row whose timestamp is `target` and which `accept`s, resuming from the
	/// nearest checkpoint and **range-decoding only the dods it walks over** — in chunks,
	/// so the common case touches one chunk and the whole column is never decoded.
	///
	/// Equal to the full-decode search for every frame (asserted by the frame tests).
	fn lookup(&self, target: i64, accept: impl Fn(usize) -> bool) -> Option<usize> {
		if self.first == target && accept(0) {
			return Some(0);
		}
		let idx = self.checkpoints.points.partition_point(|c| c.timestamp < target);
		let start = if idx == 0 { DodCheckpoint { row: 1, timestamp: self.first.wrapping_add(self.first_delta), delta: self.first_delta } } else { self.checkpoints.points[idx - 1] };

		let (mut row, mut ts, mut delta) = (start.row, start.timestamp, start.delta);
		// Walk forward, pulling the dods in stride-sized chunks. A sorted column means we
		// stop as soon as the timestamps pass `target`, so this normally decodes one chunk.
		let chunk = self.checkpoints.stride.max(1);
		loop {
			if ts == target && accept(row) {
				return Some(row);
			}
			if ts > target {
				return None;
			}
			// Advancing from row `r` consumes `dods[r - 1]`.
			let need = row.checked_sub(1)?;
			if need >= self.count {
				return None;
			}
			let window = crate::timestamp::blocked_bitpack_decode_range(self.dod_bytes, self.block, self.count, need, chunk);
			if window.is_empty() {
				return None;
			}
			for &dod in &window {
				delta = delta.wrapping_add(dod);
				ts = ts.wrapping_add(delta);
				row += 1;
				if ts == target && accept(row) {
					return Some(row);
				}
				if ts > target {
					return None;
				}
			}
		}
	}
}

/// Parse a checkpointed timestamp block lazily, or `None` if the block is not
/// checkpointed (or its inner codec is not range-decodable).
///
/// On `Some` the reader is advanced past the whole timestamp block (so the caller reads
/// the quality column next) **without the dod stream ever being decoded**; on `None` the
/// reader is left untouched so the caller falls back to the ordinary read.
fn read_lazy_checkpointed_ts<'a>(r: &mut ByteReader<'a>) -> Result<Option<LazyCheckpointedTs<'a>>, WeftSegError> {
	let mut probe = r.clone();
	let _unit = time_unit_from_tag(probe.read_u8()?)?;
	let first = probe.read_i64_le()?;
	let first_delta = match probe.read_u8()? {
		0 => return Ok(None),
		_ => probe.read_svarint()?,
	};
	let count = usize::try_from(probe.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
	if probe.read_u8()? != TS_CODEC_CHECKPOINTED {
		return Ok(None);
	}
	let stride = usize::try_from(probe.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
	let entries = usize::try_from(probe.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
	let mut points = Vec::with_capacity(entries);
	let (mut row, mut timestamp) = (0_usize, 0_i64);
	for _ in 0..entries {
		row += usize::try_from(probe.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
		timestamp = timestamp.wrapping_add(probe.read_svarint()?);
		let delta = probe.read_svarint()?;
		points.push(DodCheckpoint { row, timestamp, delta });
	}
	// Only the per-block codec can be range-decoded; anything else must decode wholesale,
	// which is the cost this path exists to avoid.
	if probe.read_u8()? != TS_CODEC_BLOCKED {
		return Ok(None);
	}
	let block = usize::try_from(probe.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
	let dod_bytes = probe.read_bytes()?;
	let rows = if count == 0 { 1 } else { count + 2 };
	// Commit: the block parsed cleanly, so hand the caller a reader positioned after it.
	*r = probe;
	Ok(Some(LazyCheckpointedTs { first, first_delta, checkpoints: DodCheckpoints { stride: stride.max(1), points, rows }, dod_bytes, block: block.max(1), count }))
}

/// Read a codec-tagged second-difference stream: the `[codec tag][stream]` body shared
/// by [`read_timestamp_column`] and the `TS_CODEC_CHECKPOINTED` wrapper.
///
/// Returns the dods and, for a checkpointed block, the persisted index.
fn read_dod_codec(r: &mut ByteReader, count: usize) -> Result<(Vec<i64>, Option<DodCheckpoints>), WeftSegError> {
	let dods = match r.read_u8()? {
		TS_CODEC_CHECKPOINTED => {
			// A wrapper, not a codec: the sparse index, then the inner codec-tagged stream
			// that actually holds the dods. One level only — the inner read rejects a
			// nested wrapper, so a malformed frame cannot recurse.
			let stride = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
			let entries = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
			let mut points = Vec::with_capacity(entries);
			let (mut row, mut timestamp) = (0_usize, 0_i64);
			for _ in 0..entries {
				// Rows ascend and timestamps are non-decreasing (a checkpointed column is
				// sorted), so both delta-encode against the previous entry.
				row += usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
				timestamp = timestamp.wrapping_add(r.read_svarint()?);
				let delta = r.read_svarint()?;
				points.push(DodCheckpoint { row, timestamp, delta });
			}
			let (dods, nested) = read_dod_codec(r, count)?;
			if nested.is_some() {
				return Err(WeftSegError::InvalidTag { kind: "timestamp_codec", value: TS_CODEC_CHECKPOINTED });
			}
			let rows = if count == 0 { usize::from(true) } else { count + 2 };
			return Ok((dods, Some(DodCheckpoints { stride: stride.max(1), points, rows })));
		}
		TS_CODEC_VARINT => {
			let mut dods = Vec::with_capacity(count);
			for _ in 0..count {
				dods.push(r.read_svarint()?);
			}
			dods
		}
		TS_CODEC_BITPACK => {
			let width = u32::from(r.read_u8()?);
			let data_len = (count * width as usize).div_ceil(8);
			let bytes = r.take(data_len)?;
			crate::timestamp::bitpack_decode(width, bytes, count)
		}
		TS_CODEC_GORILLA => {
			let bytes = r.read_bytes()?;
			crate::timestamp::decode_gorilla_dods(bytes, count)
		}
		TS_CODEC_BLOCKED => {
			let block = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
			let bytes = r.read_bytes()?;
			crate::timestamp::blocked_bitpack_decode(bytes, block, count)
		}
		TS_CODEC_RLE => {
			let run_count = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
			let mut runs = Vec::with_capacity(run_count);
			for _ in 0..run_count {
				let value = r.read_svarint()?;
				let run_len = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
				runs.push((value, run_len));
			}
			crate::timestamp::rle_decode(&runs)
		}
		other => return Err(WeftSegError::InvalidTag { kind: "timestamp_codec", value: other }),
	};
	Ok((dods, None))
}

// ---------------------------------------------------------------------------
// Segment frame (Phase 4.3, slice 4)
//
// The full `.weftseg` frame ties the pieces together: a magic prefix and format
// version, the per-segment header (the stats a reader prunes against — row/null
// counts, time-sorted flag, min/max ts and value), the value-column block, the
// timestamp-column block, and a trailing CRC-32 over everything before it. A
// single flipped byte anywhere in the body changes the checksum, so corruption is
// caught on read rather than silently misinterpreted.
// ---------------------------------------------------------------------------

use crate::{
	nulls::NullMask, page::{Page, PagedSegment, PAGED_SEGMENT_FORMAT_VERSION}, segment::SegmentStats, Segment, SEGMENT_FORMAT_VERSION
};

/// The magic prefix every `.weftseg` frame starts with.
const MAGIC: &[u8; 7] = b"WEFTSEG";

/// The pre-rename magic prefix, written when the project was called DSP and the segment
/// extension was `.dspseg`.
///
/// It is exactly the same length as [`MAGIC`], so a frame carrying it has an identical
/// layout and every offset after the prefix is unchanged — only the seven identifying bytes
/// differ. Readers therefore accept **either** prefix ([`is_known_magic`]) while writers only
/// ever emit [`MAGIC`], so segments sealed before the rename keep reading and are migrated
/// to the new prefix the next time they are rewritten (a reconcile, split, squash or
/// compaction).
const LEGACY_MAGIC: &[u8; 7] = b"DSPSEG\0";

/// Whether `prefix` is a segment magic this reader understands — the current [`MAGIC`] or
/// the pre-rename [`LEGACY_MAGIC`].
fn is_known_magic(prefix: &[u8]) -> bool {
	prefix == MAGIC || prefix == LEGACY_MAGIC
}

/// Write the quality-column block: a presence flag, then (for a sparse column)
/// the length-prefixed presence bitmap. A dense column writes a single `0` byte —
/// the row/null counts are already in the header, so a dense segment's quality
/// column costs exactly one byte on disk and zero in the bytes/point estimate.
fn write_null_column(w: &mut ByteWriter, nulls: &NullMask) {
	match nulls.bitmap_bytes() {
		None => w.put_u8(0),
		Some(bytes) => {
			w.put_u8(1);
			w.put_bytes(bytes);
		}
	}
}

/// Read the quality-column block back into a [`NullMask`], validating it against
/// the header's `row_count` / `null_count` (a corrupt bitmap length or clear-bit
/// count is rejected, not trusted).
fn read_null_column(r: &mut ByteReader, row_count: usize, null_count: usize) -> Result<NullMask, WeftSegError> {
	let bits = match r.read_u8()? {
		0 => None,
		1 => Some(r.read_bytes()?.to_vec()),
		value => return Err(WeftSegError::InvalidTag { kind: "null_column", value }),
	};
	NullMask::from_raw(row_count, null_count, bits).map_err(WeftSegError::InvalidNullMask)
}

/// Write an optional `i64` stat: a presence flag byte, then the fixed-width value.
fn put_opt_i64(w: &mut ByteWriter, value: Option<i64>) {
	match value {
		Some(v) => {
			w.put_u8(1);
			w.put_i64_le(v);
		}
		None => w.put_u8(0),
	}
}

/// Read an optional `i64` stat.
fn read_opt_i64(r: &mut ByteReader) -> Result<Option<i64>, WeftSegError> {
	match r.read_u8()? {
		0 => Ok(None),
		_ => Ok(Some(r.read_i64_le()?)),
	}
}

/// Write an optional `BigDecimal` stat as a presence flag + decimal text.
fn put_opt_decimal(w: &mut ByteWriter, value: Option<&BigDecimal>) {
	match value {
		Some(v) => {
			w.put_u8(1);
			w.put_str(&v.to_plain_string());
		}
		None => w.put_u8(0),
	}
}

/// Read an optional `BigDecimal` stat.
fn read_opt_decimal(r: &mut ByteReader) -> Result<Option<BigDecimal>, WeftSegError> {
	match r.read_u8()? {
		0 => Ok(None),
		_ => Ok(Some(read_decimal(r)?)),
	}
}

/// Write a [`SegmentStats`] header block: row/null counts, the time-sorted flag,
/// and the optional min/max ts/value stats — the data-skipping inputs a reader
/// prunes against. Shared by the single-block frame's header and each entry of the
/// paged frame's per-page index, so both layouts encode stats identically.
fn write_segment_stats(w: &mut ByteWriter, stats: &SegmentStats) {
	w.put_uvarint(stats.row_count as u64);
	w.put_uvarint(stats.null_count as u64);
	w.put_u8(u8::from(stats.time_sorted));
	put_opt_i64(w, stats.min_ts);
	put_opt_i64(w, stats.max_ts);
	put_opt_decimal(w, stats.min_value.as_ref());
	put_opt_decimal(w, stats.max_value.as_ref());
}

/// Read a [`SegmentStats`] header block — the exact inverse of
/// [`write_segment_stats`].
fn read_segment_stats(r: &mut ByteReader) -> Result<SegmentStats, WeftSegError> {
	let row_count = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
	let null_count = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
	let time_sorted = r.read_u8()? != 0;
	let min_ts = read_opt_i64(r)?;
	let max_ts = read_opt_i64(r)?;
	let min_value = read_opt_decimal(r)?;
	let max_value = read_opt_decimal(r)?;
	Ok(SegmentStats { row_count, null_count, time_sorted, min_ts, max_ts, min_value, max_value })
}

/// Encode a [`Segment`] into a complete, self-describing, checksummed `.weftseg`
/// byte frame.
///
/// The inverse is [`read_segment`]; the round trip is exact (every field —
/// including the header stats — is written, so the reconstructed segment compares
/// equal to the original).
#[must_use]
pub fn write_segment(seg: &Segment) -> Vec<u8> {
	let mut w = ByteWriter::with_capacity(64 + seg.total_bytes());
	w.put_raw(MAGIC);
	w.put_u16_le(seg.version);
	// Header (stats).
	write_segment_stats(&mut w, &seg.stats);
	// Column blocks.
	write_value_column(&mut w, &seg.values);
	write_timestamp_column(&mut w, &seg.timestamps);
	write_null_column(&mut w, &seg.nulls);
	// Trailing CRC-32 over the whole body so far.
	let checksum = crc32(w.as_slice());
	w.put_u32_le(checksum);
	w.into_vec()
}

/// The **opt-in** layout choices a `.weftseg` frame can be written with.
///
/// Both are storage-for-latency trades that are off by default, so
/// [`FrameOptions::DEFAULT`] writes byte-for-byte the frames WeftDB has always written and
/// [`write_segment_with`] is then exactly [`write_segment`]. Each is independently gated
/// because they accelerate different reads: the checkpoint index speeds up *locating a row*
/// on an irregular timestamp column, the transposed value codec speeds up *decoding values*.
///
/// Making either the default is a headline bytes/point change and is owner-gated (as the FOR
/// codec and the delta cascade were) — see ROADMAP.md.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameOptions {
	/// Rows between persisted timestamp checkpoints, or `None` for the ordinary timestamp
	/// block. See [`write_segment_checkpointed`] for the trade and the shapes it pays on.
	pub checkpoint_stride: Option<usize>,
	/// Ceiling on the size overhead ([`ColumnEncoding::transposed_overhead`]) the
	/// **transposed** value codec may pay against the size-selected codec, or `None` to never
	/// write it. `Some(1.0)` admits it only when free. See `VAL_CODEC_TRANSPOSED`.
	pub transposed_max_overhead: Option<f64>,
}

impl FrameOptions {
	/// Neither opt-in layout — byte-for-byte the historical frame.
	pub const DEFAULT: Self = Self { checkpoint_stride: None, transposed_max_overhead: None };
}

impl Default for FrameOptions {
	fn default() -> Self {
		Self::DEFAULT
	}
}

/// Encode a [`Segment`] into a `.weftseg` frame under explicit [`FrameOptions`].
///
/// The general form of [`write_segment`] (which is this with [`FrameOptions::DEFAULT`]) and
/// [`write_segment_checkpointed`] (this with a `checkpoint_stride`). Every combination is read
/// by the ordinary [`read_segment`] — both opt-ins are additive codec tags, so no frame
/// version changes and `read_segment(write_segment_with(s, opts)) == s` for any options.
#[must_use]
pub fn write_segment_with(seg: &Segment, opts: &FrameOptions) -> Vec<u8> {
	let mut w = ByteWriter::with_capacity(64 + seg.total_bytes());
	w.put_raw(MAGIC);
	w.put_u16_le(seg.version);
	write_segment_stats(&mut w, &seg.stats);
	match opts.transposed_max_overhead {
		Some(max) => write_value_column_transposed(&mut w, &seg.values, max),
		None => write_value_column(&mut w, &seg.values),
	}
	match opts.checkpoint_stride {
		Some(stride) => write_timestamp_column_checkpointed(&mut w, &seg.timestamps, stride),
		None => write_timestamp_column(&mut w, &seg.timestamps),
	}
	write_null_column(&mut w, &seg.nulls);
	let checksum = crc32(w.as_slice());
	w.put_u32_le(checksum);
	w.into_vec()
}

/// The value-codec name a single-block `.weftseg` frame's value block actually uses — one of
/// `varint` / `scaled_bitpack` / `scaled_blocked` / `scaled_for` / `scaled_delta_cascade` /
/// `scaled_transposed`.
///
/// Reads only the frame header and the value block's codec byte (no column is decoded), so it
/// is the cheap way for a test, a bench, or a storage-stats surface to assert *which* codec was
/// realized rather than inferring it from a size.
///
/// # Errors
///
/// [`WeftSegError::UnexpectedEof`] / [`WeftSegError::BadMagic`] /
/// [`WeftSegError::UnsupportedVersion`] for a frame this reader does not recognise, or
/// [`WeftSegError::InvalidTag`] for an unrecognised physical-type tag or codec byte.
pub fn frame_value_codec(bytes: &[u8]) -> Result<&'static str, WeftSegError> {
	if bytes.len() < 4 {
		return Err(WeftSegError::UnexpectedEof { needed: 4, remaining: bytes.len() });
	}
	let (body, _) = bytes.split_at(bytes.len() - 4);
	let mut r = ByteReader::new(body);
	if !is_known_magic(r.take(MAGIC.len())?) {
		return Err(WeftSegError::BadMagic);
	}
	let version = r.read_u16_le()?;
	if version != SEGMENT_FORMAT_VERSION {
		return Err(WeftSegError::UnsupportedVersion { found: version });
	}
	let _stats = read_segment_stats(&mut r)?;
	let tag = r.read_u8()?;
	if matches!(tag, TAG_SCALED_I64 | TAG_SCALED_I128) {
		r.read_u8()?; // scale
	}
	r.read_uvarint()?; // count
	r.read_uvarint()?; // lossy_count
	read_decimal(&mut r)?; // max_abs_error
	match r.read_u8()? {
		VAL_CODEC_VARINT => Ok("varint"),
		VAL_CODEC_BITPACK => Ok("scaled_bitpack"),
		VAL_CODEC_BLOCKED => Ok("scaled_blocked"),
		VAL_CODEC_FOR => Ok("scaled_for"),
		VAL_CODEC_DELTA_CASCADE => Ok("scaled_delta_cascade"),
		VAL_CODEC_TRANSPOSED => Ok("scaled_transposed"),
		other => Err(WeftSegError::InvalidTag { kind: "value_codec", value: other }),
	}
}

/// Encode a [`Segment`] into a `.weftseg` frame carrying a **persisted sparse checkpoint
/// index** over its timestamp column ([`write_timestamp_column_checkpointed`]).
///
/// Byte-identical to [`write_segment`] except for the timestamp block, and read by the
/// ordinary [`read_segment`] — the checkpoint tag is additive, so a frame written either
/// way reads with no version bump and `read_segment(write_segment_checkpointed(s)) == s`.
/// What it buys: [`read_segment_point`] resolves an instant on an **irregular sorted**
/// column by resuming from the nearest checkpoint rather than reconstructing the whole
/// timestamp column.
///
/// **Opt-in.** The index costs bytes, so making it the default is a bytes/point
/// (headline) change and is owner-gated — see ROADMAP.md. It pays only for a sorted,
/// irregular column large enough that the lookup dominates: a regular column already has
/// the `O(1)` closed form, and a small column decodes trivially.
#[must_use]
pub fn write_segment_checkpointed(seg: &Segment, stride: usize) -> Vec<u8> {
	let mut w = ByteWriter::with_capacity(64 + seg.total_bytes());
	w.put_raw(MAGIC);
	w.put_u16_le(seg.version);
	write_segment_stats(&mut w, &seg.stats);
	write_value_column(&mut w, &seg.values);
	write_timestamp_column_checkpointed(&mut w, &seg.timestamps, stride);
	write_null_column(&mut w, &seg.nulls);
	let checksum = crc32(w.as_slice());
	w.put_u32_le(checksum);
	w.into_vec()
}

/// Decode a [`Segment`] from a `.weftseg` byte frame — the exact inverse of
/// [`write_segment`].
///
/// The trailing CRC-32 is verified against the body **before** any field is
/// parsed, so a corrupt or truncated frame fails fast with
/// [`WeftSegError::ChecksumMismatch`] rather than being misread.
///
/// # Errors
///
/// - [`WeftSegError::UnexpectedEof`] if the frame is too short to hold a checksum.
/// - [`WeftSegError::ChecksumMismatch`] if the stored CRC does not match the body.
/// - [`WeftSegError::BadMagic`] / [`WeftSegError::UnsupportedVersion`] for a frame
///   this reader does not recognise.
/// - [`WeftSegError::TrailingBytes`] if bytes remain after a complete frame.
/// - the column/stat read errors ([`WeftSegError::InvalidTag`],
///   [`WeftSegError::InvalidDecimal`], …) on a malformed body.
pub fn read_segment(bytes: &[u8]) -> Result<Segment, WeftSegError> {
	if bytes.len() < 4 {
		return Err(WeftSegError::UnexpectedEof { needed: 4, remaining: bytes.len() });
	}
	let (body, crc_bytes) = bytes.split_at(bytes.len() - 4);
	let stored = u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
	let computed = crc32(body);
	if stored != computed {
		return Err(WeftSegError::ChecksumMismatch { stored, computed });
	}
	let mut r = ByteReader::new(body);
	if !is_known_magic(r.take(MAGIC.len())?) {
		return Err(WeftSegError::BadMagic);
	}
	let version = r.read_u16_le()?;
	if version != SEGMENT_FORMAT_VERSION {
		return Err(WeftSegError::UnsupportedVersion { found: version });
	}
	let stats = read_segment_stats(&mut r)?;
	let values = read_value_column(&mut r)?;
	let timestamps = read_timestamp_column(&mut r)?;
	let nulls = read_null_column(&mut r, stats.row_count, stats.null_count)?;
	if !r.is_empty() {
		return Err(WeftSegError::TrailingBytes { remaining: r.remaining() });
	}
	Ok(Segment { version, values, timestamps, nulls, stats })
}

// ---------------------------------------------------------------------------
// Paged segment frame (Phase 4.3/4.4, the intra-segment paging slice)
//
// A `PagedSegment` seals to its own framed layout (format version 3), distinct
// from the single-block segment frame above:
//
//   MAGIC | version=3 | rows_per_page | <segment-level stats>
//   | page_count | <per-page index: stats + block_len, page_count times>
//   | <page data blocks, concatenated> | CRC-32
//
// The per-page index carries each page's full stats (row/null counts, min/max
// ts/value) AND the byte length of its column block. Two properties fall out:
//   - a reader can PRUNE pages on their min/max ts/value without touching a
//     single column byte (Phase-4.4 intra-segment page skipping on disk), and
//   - the block lengths let it SEEK straight to a wanted page's bytes, skipping
//     the blocks before it — the on-disk realization of `read_time_range`.
// Each page block is pure columns (value + timestamp + quality); the page's stats
// live in the index, so they are written exactly once, mirroring how the
// single-block frame keeps stats in its header.
// ---------------------------------------------------------------------------

/// Encode a [`PagedSegment`] into a complete, self-describing, checksummed
/// `.weftseg` byte frame (format version 3).
///
/// The inverse is [`read_paged_segment`]; the round trip is exact. The frame's
/// per-page index lets a reader prune and seek to individual pages without
/// decoding the whole segment (see the module comment above).
#[must_use]
pub fn write_paged_segment(seg: &PagedSegment) -> Vec<u8> {
	write_paged_segment_inner(seg, &FrameOptions::DEFAULT)
}

/// Encode a [`PagedSegment`] whose every page carries a **persisted sparse checkpoint index**.
///
/// The paged sibling of [`write_segment_checkpointed`], with the same trade and the same
/// opt-in status.
///
/// Read by the ordinary [`read_paged_segment`] (the tag is additive), and
/// [`read_paged_segment_point`] resolves an instant within a surviving page without
/// decoding that page's timestamp column. Page pruning still happens first on the indexed
/// per-page min/max, so this only accelerates the page a probe actually lands in — the
/// win is therefore bounded by `rows_per_page`, and smaller than the single-block frame's.
#[must_use]
pub fn write_paged_segment_checkpointed(seg: &PagedSegment, stride: usize) -> Vec<u8> {
	write_paged_segment_inner(seg, &FrameOptions { checkpoint_stride: Some(stride), transposed_max_overhead: None })
}

/// Encode a [`PagedSegment`] into a `.weftseg` frame under explicit [`FrameOptions`] — the
/// paged sibling of [`write_segment_with`].
///
/// The options apply to **every page** (each page's value block is chosen independently, so a
/// page whose column exceeds the transposed overhead ceiling keeps its size-selected codec
/// while its siblings may not). Read by the ordinary [`read_paged_segment`].
#[must_use]
pub fn write_paged_segment_with(seg: &PagedSegment, opts: &FrameOptions) -> Vec<u8> {
	write_paged_segment_inner(seg, opts)
}

/// The shared paged-frame writer: [`FrameOptions`] select the plain or checkpointed timestamp
/// block and the plain or transposed value block for every page. Everything else about the
/// frame is identical.
fn write_paged_segment_inner(seg: &PagedSegment, opts: &FrameOptions) -> Vec<u8> {
	// Encode each page's column block first so the index can carry its byte length.
	let page_blocks: Vec<Vec<u8>> = seg
		.pages
		.iter()
		.map(|page| {
			let mut pw = ByteWriter::with_capacity(page.total_bytes() + 16);
			match opts.transposed_max_overhead {
				Some(max) => write_value_column_transposed(&mut pw, &page.values, max),
				None => write_value_column(&mut pw, &page.values),
			}
			match opts.checkpoint_stride {
				Some(stride) => write_timestamp_column_checkpointed(&mut pw, &page.timestamps, stride),
				None => write_timestamp_column(&mut pw, &page.timestamps),
			}
			write_null_column(&mut pw, &page.nulls);
			pw.into_vec()
		})
		.collect();
	let body_estimate: usize = page_blocks.iter().map(Vec::len).sum::<usize>() + 64 + seg.pages.len() * 32;
	let mut w = ByteWriter::with_capacity(body_estimate);
	w.put_raw(MAGIC);
	w.put_u16_le(seg.version);
	w.put_uvarint(seg.rows_per_page as u64);
	// Segment-level rollup stats.
	write_segment_stats(&mut w, &seg.stats);
	// Per-page index: each page's stats + its block length.
	w.put_uvarint(seg.pages.len() as u64);
	for (page, block) in seg.pages.iter().zip(&page_blocks) {
		write_segment_stats(&mut w, &page.stats);
		w.put_uvarint(block.len() as u64);
	}
	// Page data blocks, concatenated in page order.
	for block in &page_blocks {
		w.put_raw(block);
	}
	// Trailing CRC-32 over the whole body.
	let checksum = crc32(w.as_slice());
	w.put_u32_le(checksum);
	w.into_vec()
}

/// Decode a [`PagedSegment`] from a `.weftseg` byte frame — the exact inverse of
/// [`write_paged_segment`].
///
/// As with [`read_segment`], the trailing CRC-32 is verified against the body
/// **before** any field is parsed, so a corrupt or truncated frame fails fast with
/// [`WeftSegError::ChecksumMismatch`]. Each page's column block is bounded to its
/// indexed byte length, so a page that does not exactly fill its block is rejected
/// ([`WeftSegError::TrailingBytes`]).
///
/// # Errors
///
/// The same family as [`read_segment`]: [`WeftSegError::UnexpectedEof`],
/// [`WeftSegError::ChecksumMismatch`], [`WeftSegError::BadMagic`],
/// [`WeftSegError::UnsupportedVersion`] (when the version is not the paged version),
/// [`WeftSegError::TrailingBytes`], and the per-column read errors.
pub fn read_paged_segment(bytes: &[u8]) -> Result<PagedSegment, WeftSegError> {
	if bytes.len() < 4 {
		return Err(WeftSegError::UnexpectedEof { needed: 4, remaining: bytes.len() });
	}
	let (body, crc_bytes) = bytes.split_at(bytes.len() - 4);
	let stored = u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
	let computed = crc32(body);
	if stored != computed {
		return Err(WeftSegError::ChecksumMismatch { stored, computed });
	}
	let mut r = ByteReader::new(body);
	if !is_known_magic(r.take(MAGIC.len())?) {
		return Err(WeftSegError::BadMagic);
	}
	let version = r.read_u16_le()?;
	if version != PAGED_SEGMENT_FORMAT_VERSION {
		return Err(WeftSegError::UnsupportedVersion { found: version });
	}
	let rows_per_page = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
	let stats = read_segment_stats(&mut r)?;
	let page_count = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
	// Read the index: each page's stats and the length of its column block.
	let mut index: Vec<(SegmentStats, usize)> = Vec::with_capacity(page_count);
	for _ in 0..page_count {
		let page_stats = read_segment_stats(&mut r)?;
		let block_len = usize::try_from(r.read_uvarint()?).map_err(|_| WeftSegError::VarintTooLong)?;
		index.push((page_stats, block_len));
	}
	// Read each page's column block, bounded to its indexed length.
	let mut pages = Vec::with_capacity(page_count);
	for (page_stats, block_len) in index {
		let block = r.take(block_len)?;
		let mut pr = ByteReader::new(block);
		let values = read_value_column(&mut pr)?;
		let timestamps = read_timestamp_column(&mut pr)?;
		let nulls = read_null_column(&mut pr, page_stats.row_count, page_stats.null_count)?;
		if !pr.is_empty() {
			return Err(WeftSegError::TrailingBytes { remaining: pr.remaining() });
		}
		pages.push(Page { values, timestamps, nulls, stats: page_stats });
	}
	if !r.is_empty() {
		return Err(WeftSegError::TrailingBytes { remaining: r.remaining() });
	}
	Ok(PagedSegment { version, rows_per_page, pages, stats })
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn crc32_matches_the_standard_vector() {
		assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
		assert_eq!(crc32(b""), 0);
		// A single flipped byte changes the checksum.
		assert_ne!(crc32(b"123456789"), crc32(b"123456780"));
	}

	#[test]
	fn fixed_width_integers_round_trip() {
		let mut w = ByteWriter::new();
		w.put_u8(0xAB);
		w.put_u16_le(0xBEEF);
		w.put_u32_le(0xDEAD_BEEF);
		w.put_i64_le(-1_234_567_890_123);
		let bytes = w.into_vec();
		let mut r = ByteReader::new(&bytes);
		assert_eq!(r.read_u8().unwrap(), 0xAB);
		assert_eq!(r.read_u16_le().unwrap(), 0xBEEF);
		assert_eq!(r.read_u32_le().unwrap(), 0xDEAD_BEEF);
		assert_eq!(r.read_i64_le().unwrap(), -1_234_567_890_123);
		assert!(r.is_empty());
	}

	#[test]
	fn uvarint_round_trips_across_widths() {
		for value in [0_u64, 1, 127, 128, 16_383, 16_384, u64::from(u32::MAX), u64::MAX] {
			let mut w = ByteWriter::new();
			w.put_uvarint(value);
			// The written width matches the crate's estimator.
			assert_eq!(w.len(), crate::timestamp::uvarint_len(value), "width for {value}");
			let mut r = ByteReader::new(w.as_slice());
			assert_eq!(r.read_uvarint().unwrap(), value);
			assert!(r.is_empty());
		}
	}

	#[test]
	fn svarint_round_trips_and_matches_estimator() {
		for value in [0_i64, -1, 1, 63, 64, -64, -65, i64::MIN, i64::MAX] {
			let mut w = ByteWriter::new();
			w.put_svarint(value);
			assert_eq!(w.len(), crate::timestamp::zigzag_varint_len(value), "width for {value}");
			let mut r = ByteReader::new(w.as_slice());
			assert_eq!(r.read_svarint().unwrap(), value);
			assert!(r.is_empty());
		}
	}

	#[test]
	fn length_prefixed_blocks_round_trip() {
		let mut w = ByteWriter::new();
		w.put_str("");
		w.put_str("hello");
		w.put_str("12345.6789");
		w.put_bytes(&[0, 1, 2, 255]);
		let bytes = w.into_vec();
		let mut r = ByteReader::new(&bytes);
		assert_eq!(r.read_str().unwrap(), "");
		assert_eq!(r.read_str().unwrap(), "hello");
		assert_eq!(r.read_str().unwrap(), "12345.6789");
		assert_eq!(r.read_bytes().unwrap(), &[0, 1, 2, 255]);
		assert!(r.is_empty());
	}

	#[test]
	fn short_reads_error_rather_than_panic() {
		let bytes = [0x01_u8, 0x02];
		let mut r = ByteReader::new(&bytes);
		assert_eq!(r.read_u32_le(), Err(WeftSegError::UnexpectedEof { needed: 4, remaining: 2 }));
		// A length prefix promising more than is present.
		let mut w = ByteWriter::new();
		w.put_uvarint(10);
		w.put_raw(b"abc");
		let framed = w.into_vec();
		let mut r2 = ByteReader::new(&framed);
		assert_eq!(r2.read_str(), Err(WeftSegError::UnexpectedEof { needed: 10, remaining: 3 }));
	}

	#[test]
	fn non_terminating_varint_is_rejected() {
		// Eleven continuation bytes never terminate within the 64-bit budget.
		let bytes = [0x80_u8; 11];
		let mut r = ByteReader::new(&bytes);
		assert_eq!(r.read_uvarint(), Err(WeftSegError::VarintTooLong));
	}

	#[test]
	fn invalid_utf8_block_is_rejected() {
		let mut w = ByteWriter::new();
		w.put_bytes(&[0xFF, 0xFE]); // not valid UTF-8
		let bytes = w.into_vec();
		let mut r = ByteReader::new(&bytes);
		assert_eq!(r.read_str(), Err(WeftSegError::InvalidUtf8));
	}

	fn col(lits: &[&str]) -> Vec<BigDecimal> {
		lits.iter().map(|s| BigDecimal::from_str(s).expect("parses")).collect()
	}

	/// Round-trip a column through the value-column codec and assert exact recovery
	/// (both the encoding struct and its decoded logical values).
	fn assert_value_col_round_trips(enc: &ColumnEncoding) {
		let mut w = ByteWriter::new();
		write_value_column(&mut w, enc);
		let bytes = w.into_vec();
		let mut r = ByteReader::new(&bytes);
		let back = read_value_column(&mut r).expect("reads");
		assert!(r.is_empty(), "value-column codec must consume its whole block");
		assert_eq!(&back, enc, "{:?} column must round-trip", enc.physical_type);
		assert_eq!(back.decode(), enc.decode());
	}

	#[test]
	fn value_column_round_trips_every_encoding() {
		use crate::encode_column;
		// One column per physical type, each within that encoding's exact range.
		assert_value_col_round_trips(&encode_column(PhysicalType::F64, &col(&["0.5", "2.25", "-128.0"])).unwrap());
		assert_value_col_round_trips(&encode_column(PhysicalType::F32, &col(&["0.5", "-0.25", "16.0"])).unwrap());
		assert_value_col_round_trips(&encode_column(PhysicalType::ScaledI64 { scale: 2 }, &col(&["1.25", "-3.75", "0.00"])).unwrap());
		assert_value_col_round_trips(&encode_column(PhysicalType::ScaledI128 { scale: 4 }, &col(&["1234567890.1234", "-9.0001"])).unwrap());
		assert_value_col_round_trips(&encode_column(PhysicalType::Decimal128, &col(&["123456789012345678901234.567890", "-1.5", "0"])).unwrap());
		assert_value_col_round_trips(&encode_column(PhysicalType::BigDecimalText, &col(&["1.5", "12345.6789", "-0.000001"])).unwrap());
	}

	#[test]
	fn value_column_realizes_bitpack_on_a_regular_scaled_stream() {
		use crate::encode_column;
		// -0.32..0.31 scaled by 100 → mantissas -32..=31 (≤ 6 zig-zag bits, symmetric
		// around zero so a FOR reference buys nothing): fixed-width bit-packing beats
		// the one-byte-per-value varint floor, so the block selects the bit-pack codec
		// on disk.
		let values: Vec<BigDecimal> = (0..64).map(|i| BigDecimal::new((i - 32).into(), 2)).collect();
		let enc = encode_column(PhysicalType::ScaledI64 { scale: 2 }, &values).unwrap();
		assert_eq!(enc.best_value_codec(), "scaled_bitpack");
		assert!(enc.bitpack_value_bytes().unwrap() < enc.serialized_bytes());
		// It round-trips exactly through the realized `.weftseg` value block…
		assert_value_col_round_trips(&enc);
		// …and the realized block is strictly smaller than the same column written
		// under the varint codec (identical header, so the payloads decide).
		let mut w = ByteWriter::new();
		write_value_column(&mut w, &enc);
		let realized = w.into_vec().len();
		let mut vw = ByteWriter::new();
		vw.put_u8(physical_type_tag(enc.physical_type));
		if let PhysicalType::ScaledI64 { scale } = enc.physical_type {
			vw.put_u8(scale);
		}
		vw.put_uvarint(enc.values.len() as u64);
		vw.put_uvarint(enc.lossy_count as u64);
		vw.put_str(&enc.max_abs_error.to_plain_string());
		vw.put_u8(VAL_CODEC_VARINT);
		for value in &enc.values {
			write_physical_value(&mut vw, value);
		}
		assert!(realized < vw.into_vec().len(), "realized bit-pack block {realized} must beat the varint block");
	}

	#[test]
	fn bitpack_codec_on_a_non_scaled_column_is_rejected() {
		// A hand-crafted F64 value block that claims the bit-pack codec must be
		// refused, not misread — bit-packing is only defined for a ScaledI64 payload.
		let mut w = ByteWriter::new();
		w.put_u8(TAG_F64);
		w.put_uvarint(1); // count
		w.put_uvarint(0); // lossy_count
		w.put_str("0"); // max_abs_error
		w.put_u8(VAL_CODEC_BITPACK);
		w.put_u8(1); // width
		w.put_raw(&[0]); // one packed byte
		let bytes = w.into_vec();
		let mut r = ByteReader::new(&bytes);
		assert_eq!(read_value_column(&mut r), Err(WeftSegError::InvalidTag { kind: "value_codec_bitpack_type", value: TAG_F64 }));
	}

	/// A scaled-int column whose mantissas mix a quiet zero-straddling region with a
	/// contiguous sign-alternating burst of large values — per-block adaptive bit-packing
	/// is the strict winner (global width over-pays, a FOR reference is wasted on data
	/// whose per-block residual range equals its zig-zag magnitude), so the block selects
	/// the blocked codec on disk.
	fn mixed_magnitude_scaled_column() -> ColumnEncoding {
		use crate::encode_column;
		let lits: Vec<String> = (0..192).map(|i| if (64..128).contains(&i) { format!("{}", (1_000_000_000_i64 + i) * if i % 2 == 0 { 1 } else { -1 }) } else { format!("{}", (i % 5) - 2) }).collect();
		let refs: Vec<&str> = lits.iter().map(String::as_str).collect();
		encode_column(PhysicalType::ScaledI64 { scale: 0 }, &col(&refs)).unwrap()
	}

	#[test]
	fn value_column_realizes_blocked_on_a_mixed_magnitude_scaled_stream() {
		let enc = mixed_magnitude_scaled_column();
		assert_eq!(enc.best_value_codec(), "scaled_blocked", "mixed-magnitude scaled stream must pick the blocked codec");
		assert!(enc.blocked_value_bytes().unwrap() < enc.bitpack_value_bytes().unwrap(), "blocked must beat global bit-pack");
		// Round-trips exactly through the realized `.weftseg` value block…
		assert_value_col_round_trips(&enc);
		// …and the realized blocked block is strictly smaller than the same column written
		// under the global bit-pack codec (identical header, so the payloads decide).
		let mut w = ByteWriter::new();
		write_value_column(&mut w, &enc);
		let realized = w.into_vec().len();
		let mantissas = enc.scaled_i64_mantissas().unwrap();
		let (width, packed) = crate::timestamp::bitpack_encode(&mantissas);
		let mut bw = ByteWriter::new();
		bw.put_u8(physical_type_tag(enc.physical_type));
		if let PhysicalType::ScaledI64 { scale } = enc.physical_type {
			bw.put_u8(scale);
		}
		bw.put_uvarint(enc.values.len() as u64);
		bw.put_uvarint(enc.lossy_count as u64);
		bw.put_str(&enc.max_abs_error.to_plain_string());
		bw.put_u8(VAL_CODEC_BITPACK);
		bw.put_u8(u8::try_from(width).unwrap_or(64));
		bw.put_raw(&packed);
		assert!(realized < bw.into_vec().len(), "realized blocked block {realized} must beat the global bit-pack block");
	}

	#[test]
	fn value_column_realizes_for_on_a_clustered_high_base_stream() {
		use crate::encode_column;
		// Mantissas clustered near 1e9 with a small jitter — the FOR codec subtracts each
		// block's minimum and packs only the jitter, so it is the strict winner and the
		// block selects the FOR codec on disk.
		let lits: Vec<String> = (0..192).map(|i| format!("{}", 1_000_000_000_i64 + (i % 7))).collect();
		let refs: Vec<&str> = lits.iter().map(String::as_str).collect();
		let enc = encode_column(PhysicalType::ScaledI64 { scale: 0 }, &col(&refs)).unwrap();
		assert_eq!(enc.best_value_codec(), "scaled_for", "clustered high-base scaled stream must pick the FOR codec");
		assert!(enc.for_value_bytes().unwrap() < enc.blocked_value_bytes().unwrap(), "FOR must beat blocked");
		// Round-trips exactly through the realized `.weftseg` value block…
		assert_value_col_round_trips(&enc);
		// …and the realized FOR block is strictly smaller than the same column written
		// under the blocked codec (identical header, so the payloads decide).
		let mut w = ByteWriter::new();
		write_value_column(&mut w, &enc);
		let realized = w.into_vec().len();
		let mantissas = enc.scaled_i64_mantissas().unwrap();
		let block = crate::timestamp::BLOCKED_BITPACK_BLOCK;
		let mut bw = ByteWriter::new();
		bw.put_u8(physical_type_tag(enc.physical_type));
		if let PhysicalType::ScaledI64 { scale } = enc.physical_type {
			bw.put_u8(scale);
		}
		bw.put_uvarint(enc.values.len() as u64);
		bw.put_uvarint(enc.lossy_count as u64);
		bw.put_str(&enc.max_abs_error.to_plain_string());
		bw.put_u8(VAL_CODEC_BLOCKED);
		bw.put_uvarint(block as u64);
		bw.put_bytes(&crate::timestamp::blocked_bitpack_encode(&mantissas, block));
		assert!(realized < bw.into_vec().len(), "realized FOR block {realized} must beat the blocked block");
	}

	#[test]
	fn for_codec_on_a_non_scaled_column_is_rejected() {
		// A hand-crafted F64 value block that claims the FOR codec must be refused —
		// Frame-of-Reference packing is only defined for a ScaledI64 payload.
		let mut w = ByteWriter::new();
		w.put_u8(TAG_F64);
		w.put_uvarint(1); // count
		w.put_uvarint(0); // lossy_count
		w.put_str("0"); // max_abs_error
		w.put_u8(VAL_CODEC_FOR);
		w.put_uvarint(64); // block size
		w.put_bytes(&[0, 0]); // a length-prefixed (bogus) payload
		let bytes = w.into_vec();
		let mut r = ByteReader::new(&bytes);
		assert_eq!(read_value_column(&mut r), Err(WeftSegError::InvalidTag { kind: "value_codec_for_type", value: TAG_F64 }));
	}

	#[test]
	fn segment_frame_round_trips_a_for_scaled_column() {
		// A clustered-high-base scaled-int series that seals to the FOR value codec must
		// round-trip through the full framed segment (header, CRC, v4 layout). Two-decimal
		// values force the ScaledI64 encoding; a fixed 1e7 integer part with a small
		// fractional jitter gives mantissas clustered near 1e9 where FOR is the strict
		// winner.
		let ts: Vec<i64> = (0..192).map(|i| 1_000 + i * 5).collect();
		let vs: Vec<BigDecimal> = (0..192).map(|i| BigDecimal::from_str(&format!("10000000.0{}", i % 7)).unwrap()).collect();
		let seg = Segment::build(&ts, &vs, TimeUnit::Millis, &BigDecimal::from(0)).expect("builds");
		assert_eq!(seg.values.best_value_codec(), "scaled_for", "clustered high-base scaled stream must pick the FOR codec");
		let bytes = write_segment(&seg);
		let back = read_segment(&bytes).expect("reads");
		assert_eq!(back, seg, "FOR segment frame must round-trip exactly");
		assert_eq!(back.version, SEGMENT_FORMAT_VERSION);
	}

	/// Round-trip a column through the **opt-in cascading** value-column writer and the
	/// ordinary reader, asserting exact recovery — the cascade decode is unconditional, so a
	/// cascade-sealed block reads back through the normal path.
	fn assert_cascading_value_col_round_trips(enc: &ColumnEncoding) {
		let mut w = ByteWriter::new();
		write_value_column_cascading(&mut w, enc);
		let bytes = w.into_vec();
		let mut r = ByteReader::new(&bytes);
		let back = read_value_column(&mut r).expect("reads");
		assert!(r.is_empty(), "cascade value-column block must be fully consumed");
		assert_eq!(&back, enc, "cascade column must round-trip exactly");
		assert_eq!(back.decode(), enc.decode());
	}

	#[test]
	fn cascade_value_block_round_trips_a_constant_trend_via_rle_inner() {
		// A pure constant trend (+7/step): the delta stream is 255 sevens, which the RLE inner
		// codec collapses to one run — the cascading writer selects the cascade and the block
		// round-trips through the ordinary reader.
		let lits: Vec<String> = (0..256).map(|i| format!("{}", 5_000_000_000_i64 + i64::from(i) * 7)).collect();
		let refs: Vec<&str> = lits.iter().map(String::as_str).collect();
		let enc = crate::encode_column(PhysicalType::ScaledI64 { scale: 0 }, &col(&refs)).expect("encodes");
		assert_eq!(enc.best_value_codec_cascading(), "scaled_delta_cascade");
		assert_eq!(enc.delta_cascade_plan().unwrap().inner, CascadeInner::Rle, "a constant delta stream selects the RLE inner codec");
		assert_cascading_value_col_round_trips(&enc);
	}

	#[test]
	fn cascade_value_block_round_trips_a_jittery_trend_via_for_inner() {
		// A near-constant trend (+7 with a small +0..2 jitter): RLE can no longer form one run,
		// but each block's first differences share a tight range, so the FOR inner codec (block
		// minimum + narrow residual) wins — exercising the cascade's FOR inner branch on disk.
		let lits: Vec<String> = {
			let mut acc = 9_000_000_000_i64;
			(0..256).map(|i| {
				let s = format!("{acc}");
				acc += 7 + i64::from(i % 3);
				s
			})
			.collect()
		};
		let refs: Vec<&str> = lits.iter().map(String::as_str).collect();
		let enc = crate::encode_column(PhysicalType::ScaledI64 { scale: 0 }, &col(&refs)).expect("encodes");
		assert_eq!(enc.best_value_codec_cascading(), "scaled_delta_cascade");
		assert_eq!(enc.delta_cascade_plan().unwrap().inner, CascadeInner::For, "a jittery trend selects the FOR inner codec");
		assert_cascading_value_col_round_trips(&enc);
	}

	#[test]
	fn cascade_inner_bitpack_and_varint_read_arms_round_trip_hand_crafted_blocks() {
		// Directly exercise the bitpack and varint inner-codec read arms (which a natural
		// corpus rarely selects over FOR/RLE) by hand-writing a cascade block for each and
		// asserting the mantissas reconstruct via the anchor + cumulative-sum.
		let scale = 2_u8;
		let anchor = 100_i64;
		let deltas = [3_i64, -1, 4, -1, 5, -9, 2, 6];
		let count = deltas.len() + 1;
		// Expected mantissas: cumulative sum from the anchor.
		let mut expected = vec![PhysicalValue::ScaledI64 { mantissa: anchor, scale }];
		let mut m = anchor;
		for &d in &deltas {
			m += d;
			expected.push(PhysicalValue::ScaledI64 { mantissa: m, scale });
		}
		for inner in [CASCADE_INNER_BITPACK, CASCADE_INNER_VARINT] {
			let mut w = ByteWriter::new();
			w.put_u8(TAG_SCALED_I64);
			w.put_u8(scale);
			w.put_uvarint(count as u64);
			w.put_uvarint(0); // lossy_count
			w.put_str("0"); // max_abs_error
			w.put_u8(VAL_CODEC_DELTA_CASCADE);
			w.put_svarint(anchor);
			w.put_u8(inner);
			if inner == CASCADE_INNER_BITPACK {
				let (width, packed) = crate::timestamp::bitpack_encode(&deltas);
				w.put_u8(u8::try_from(width).unwrap());
				w.put_raw(&packed);
			} else {
				for &d in &deltas {
					w.put_svarint(d);
				}
			}
			let bytes = w.into_vec();
			let mut r = ByteReader::new(&bytes);
			let back = read_value_column(&mut r).expect("reads");
			assert!(r.is_empty(), "cascade block must be fully consumed (inner={inner})");
			assert_eq!(back.values, expected, "inner={inner} must reconstruct the mantissas");
		}
	}

	/// Round-trip a column through the **opt-in transposed** value-column writer and the
	/// ordinary reader at the given overhead ceiling, asserting exact recovery.
	fn assert_transposed_value_col_round_trips(enc: &ColumnEncoding, max_overhead: f64) {
		let mut w = ByteWriter::new();
		write_value_column_transposed(&mut w, enc, max_overhead);
		let bytes = w.into_vec();
		let mut r = ByteReader::new(&bytes);
		let back = read_value_column(&mut r).expect("reads");
		assert!(r.is_empty(), "transposed value-column block must be fully consumed");
		assert_eq!(&back, enc, "transposed column must round-trip exactly");
		assert_eq!(back.decode(), enc.decode());
	}

	/// A zero-straddling small-magnitude `ScaledI64` column that spans several 1024-lane tiles
	/// plus a short trailing one — the regime the plain bit-pack family wins on size, so the
	/// transposed layout is admitted at (near) parity.
	fn transposed_corpus() -> ColumnEncoding {
		let lits: Vec<String> = (0..2_500).map(|i| format!("{}", ((i * 37) % 1_001) - 500)).collect();
		crate::encode_column(PhysicalType::ScaledI64 { scale: 0 }, &col(&lits.iter().map(String::as_str).collect::<Vec<_>>())).expect("encodes")
	}

	#[test]
	fn transposed_value_block_round_trips_and_reports_its_codec() {
		let enc = transposed_corpus();
		assert_eq!(enc.best_value_codec_transposed(1.05), "scaled_transposed", "a small-magnitude column is admitted at a 5% ceiling");
		assert_transposed_value_col_round_trips(&enc, 1.05);

		// The realized block really is the transposed codec, and its size matches the column's
		// own size function (the selector and the writer cannot disagree).
		let mut w = ByteWriter::new();
		write_value_column_transposed(&mut w, &enc, 1.05);
		let bytes = w.into_vec();
		let mut probe = ByteReader::new(&bytes);
		probe.read_u8().expect("tag");
		probe.read_u8().expect("scale");
		probe.read_uvarint().expect("count");
		probe.read_uvarint().expect("lossy");
		read_decimal(&mut probe).expect("max_abs_error");
		assert_eq!(probe.read_u8().expect("codec"), VAL_CODEC_TRANSPOSED);
		let tile = usize::try_from(probe.read_uvarint().expect("tile")).expect("fits");
		assert_eq!(tile, crate::timestamp::TRANSPOSE_TILE);
		assert_eq!(probe.read_bytes().expect("stream").len(), enc.transposed_value_bytes().expect("scaled column"), "the writer emits exactly transposed_value_bytes");
	}

	#[test]
	fn transposed_codec_is_refused_when_it_would_cost_too_much() {
		// A clustered high-base column: FOR wins the size race by a wide margin, so the
		// transposed layout (which permutes the *bit-pack* code, not FOR's) would bloat the
		// column. The ceiling must refuse it and fall back to the size-selected codec — and the
		// written block must then be byte-identical to the default writer's.
		let lits: Vec<String> = (0..2_048).map(|i| format!("{}", 9_000_000_000_i64 + i64::from(i % 7))).collect();
		let enc = crate::encode_column(PhysicalType::ScaledI64 { scale: 0 }, &col(&lits.iter().map(String::as_str).collect::<Vec<_>>())).expect("encodes");
		assert_eq!(enc.best_value_codec(), "scaled_for", "the corpus must be a FOR-shaped column");
		let overhead = enc.transposed_overhead().expect("scaled column");
		assert!(overhead > 1.05, "the transposed layout really is much larger here (ratio {overhead})");
		assert_eq!(enc.best_value_codec_transposed(1.05), "scaled_for", "the ceiling refuses it");

		let mut opt_in = ByteWriter::new();
		write_value_column_transposed(&mut opt_in, &enc, 1.05);
		let mut default = ByteWriter::new();
		write_value_column(&mut default, &enc);
		assert_eq!(opt_in.into_vec(), default.into_vec(), "a refused transposed seal is byte-identical to the default");

		// A ceiling generous enough admits the same column — the gate is the only thing
		// standing between them, and the frame still round-trips.
		assert_eq!(enc.best_value_codec_transposed(overhead + 0.01), "scaled_transposed");
		assert_transposed_value_col_round_trips(&enc, overhead + 0.01);

		// A tighter ceiling refuses it just the same, and a nonsense one never admits it.
		assert_eq!(enc.best_value_codec_transposed(0.5), "scaled_for");
		assert_eq!(enc.best_value_codec_transposed(f64::NAN), "scaled_for");
		assert_eq!(enc.best_value_codec_transposed(0.0), "scaled_for");
		assert_eq!(enc.best_value_codec_transposed(f64::INFINITY), "scaled_for", "a non-finite ceiling admits nothing");
	}

	#[test]
	fn the_transposed_layout_can_be_a_strict_size_win() {
		// Pins the corrected claim: the transposed codec is NOT merely "the same bits permuted,
		// so never smaller". It adapts its width per 1024-lane tile while paying one width
		// header per tile, where the blocked codec pays one per 64-value block and the global
		// bit-pack pays the column's widest value for every value. A column whose magnitude
		// changes across wide spans therefore comes in strictly under every size-selected
		// codec — which is what makes a sub-1.0 ceiling meaningful rather than dead config.
		// Values STRADDLE ZERO within every block, so a Frame-of-Reference block minimum buys
		// nothing (the residual range equals the magnitude range) and FOR cannot undercut the
		// plain bit-pack family. The magnitude then changes once, halfway through, across a span
		// far wider than a 64-value block: a quiet +/-1 half and a loud +/-2^30 half. Both the
		// blocked codec and the transposed codec pack the same bits at the same per-span widths
		// — the only difference left is that blocked writes one width header per 64 values and
		// transposed writes one per 1024.
		let lits: Vec<String> = (0..8_192_i64)
			.map(|i| {
				let magnitude = if i < 4_096 { 1 } else { 1_i64 << 30 };
				format!("{}", if i % 2 == 0 { magnitude } else { -magnitude })
			})
			.collect();
		let enc = crate::encode_column(PhysicalType::ScaledI64 { scale: 0 }, &col(&lits.iter().map(String::as_str).collect::<Vec<_>>())).expect("encodes");

		let transposed = enc.transposed_value_bytes().expect("scaled column");
		let best = enc.best_serialized_bytes();
		let overhead = enc.transposed_overhead().expect("scaled column");
		assert!(transposed < best, "the transposed layout is strictly smaller here ({transposed} vs {best})");
		assert!(overhead < 1.0, "so the overhead ratio is below 1.0 ({overhead})");

		// A sub-1.0 ceiling therefore ADMITS this column (and it still round-trips exactly),
		// while a ceiling below the achieved ratio still refuses it.
		assert_eq!(enc.best_value_codec_transposed(1.0), "scaled_transposed", "free-or-better admits a strict win");
		assert_eq!(enc.best_value_codec_transposed(overhead + 0.001), "scaled_transposed");
		assert_eq!(enc.best_value_codec_transposed(overhead / 2.0), enc.best_value_codec(), "a ceiling under the achieved ratio still refuses");
		assert_transposed_value_col_round_trips(&enc, 1.0);
	}

	#[test]
	fn transposed_frame_round_trips_and_serves_point_and_range_reads() {
		// The whole-frame story: a transposed-sealed segment must decode, point-read and
		// range-read identically to the default-sealed one, and `frame_value_codec` must name
		// what each actually wrote.
		let n = 2_500_i64;
		let timestamps: Vec<i64> = (0..n).map(|i| 1_000 + i * 10).collect();
		// Scale 2, so most values (0.37, 1.23, …) are NOT exactly representable in binary
		// floating point and the zero-tolerance encoder picks `ScaledI64` rather than `F64` —
		// the only payload the transposed codec is defined over.
		let values: Vec<BigDecimal> = (0..n).map(|i| BigDecimal::new((((i * 37) % 1_001) - 500).into(), 2)).collect();
		let seg = Segment::build_sorted(&timestamps, &values, crate::timestamp::TimeUnit::Millis, &BigDecimal::from(0)).expect("builds");

		let linear = seg.write_to();
		let transposed = seg.write_to_with(&FrameOptions { checkpoint_stride: None, transposed_max_overhead: Some(1.05) });
		assert_eq!(frame_value_codec(&transposed).expect("codec"), "scaled_transposed");
		assert_ne!(frame_value_codec(&linear).expect("codec"), "scaled_transposed", "the default seal is unchanged");
		// FrameOptions::DEFAULT must reproduce the historical frame exactly.
		assert_eq!(seg.write_to_with(&FrameOptions::DEFAULT), linear, "DEFAULT options write the historical frame byte-for-byte");

		assert_eq!(read_segment(&transposed).expect("reads"), seg, "the transposed frame round-trips to the same segment");

		// Point reads agree at a tile boundary, inside a tile, in the short trailing tile, and
		// off-grid; so do windowed range reads spanning a tile boundary.
		for row in [0_i64, 1, 1_023, 1_024, 2_047, 2_048, n - 1] {
			let t = 1_000 + row * 10;
			assert_eq!(read_segment_point(&transposed, t).expect("reads"), read_segment_point(&linear, t).expect("reads"), "point read at row {row}");
		}
		assert_eq!(read_segment_point(&transposed, 1_005).expect("reads"), None, "an off-grid instant is absent in both");
		for (start, end) in [(1_000_i64, 1_090_i64), (1_000 + 1_020 * 10, 1_000 + 1_030 * 10), (1_000 + (n - 3) * 10, 1_000 + (n + 10) * 10)] {
			assert_eq!(read_segment_range(&transposed, start, end).expect("reads"), read_segment_range(&linear, start, end).expect("reads"), "range read [{start}, {end}]");
		}

		// And it composes with the other opt-in (the timestamp checkpoint index).
		let both = seg.write_to_with(&FrameOptions { checkpoint_stride: Some(1_024), transposed_max_overhead: Some(1.05) });
		assert_eq!(frame_value_codec(&both).expect("codec"), "scaled_transposed");
		assert_eq!(read_segment(&both).expect("reads"), seg, "both opt-ins together still round-trip");
	}

	#[test]
	fn read_value_range_does_not_panic_on_a_short_decoding_block() {
		// A malformed cascade block: the header claims 10 values, but the RLE inner stream's
		// run lengths sum to 2, so the decode yields 3. `end` is bounded by the header's count,
		// so a bare slice would panic — a fallible public parser must not. `read_value_at` was
		// already safe here (it uses `.get`), and the two must agree.
		let scale = 0_u8;
		let mut w = ByteWriter::new();
		w.put_u8(TAG_SCALED_I64);
		w.put_u8(scale);
		w.put_uvarint(10); // header count — a lie
		w.put_uvarint(0);
		w.put_str("0");
		w.put_u8(VAL_CODEC_DELTA_CASCADE);
		w.put_svarint(100); // anchor
		w.put_u8(CASCADE_INNER_RLE);
		w.put_uvarint(1); // one run...
		w.put_svarint(5);
		w.put_uvarint(2); // ...of length 2, so only 3 values reconstruct
		let bytes = w.into_vec();

		let decoded = read_value_column(&mut ByteReader::new(&bytes)).expect("reads").values;
		assert_eq!(decoded.len(), 3, "the block really does decode short of its header count");
		// The whole over-long window, a window starting inside the real values, and one
		// starting past them — none may panic, and all must agree with the decoded prefix.
		assert_eq!(read_value_range(&bytes, 0, 10).expect("reads"), decoded);
		assert_eq!(read_value_range(&bytes, 2, 8).expect("reads"), decoded[2..].to_vec());
		assert_eq!(read_value_range(&bytes, 5, 5).expect("reads"), Vec::new());
		assert_eq!(read_value_at(&bytes, 9).expect("reads"), None, "read_value_at agrees: nothing at row 9");
	}

	#[test]
	fn transposed_value_block_rejects_a_non_scaled_payload() {
		// The codec is only defined over ScaledI64. Hand-craft a transposed block tagged F64
		// and assert the reader refuses it rather than misreading the stream.
		let mut w = ByteWriter::new();
		w.put_u8(TAG_F64);
		w.put_uvarint(4);
		w.put_uvarint(0);
		w.put_str("0");
		w.put_u8(VAL_CODEC_TRANSPOSED);
		w.put_uvarint(crate::timestamp::TRANSPOSE_TILE as u64);
		w.put_bytes(&crate::timestamp::transpose_bitpack_encode(&[1, 2, 3, 4], crate::timestamp::TRANSPOSE_TILE));
		let bytes = w.into_vec();
		let err = read_value_column(&mut ByteReader::new(&bytes)).expect_err("must refuse");
		assert!(matches!(err, WeftSegError::InvalidTag { kind: "value_codec_transposed_type", .. }), "got {err:?}");
		assert!(read_value_at(&bytes, 0).is_err(), "the random-access read refuses it too");
	}

	#[test]
	fn read_value_at_matches_the_full_decode_across_every_codec() {
		// The random-access single-value read must equal the full-decode value at each index,
		// for every value codec (the two per-block codecs take the fast block-skip path; the
		// rest fall back to a full decode + index). Out-of-range indices return None.
		let scale2 = PhysicalType::ScaledI64 { scale: 2 };
		// A regular small ramp (bitpack), a mixed-magnitude column (blocked), a clustered high
		// base (FOR), a wide-sparse column (varint), and an F64 column (fixed-width fallback).
		let bitpack_col = crate::encode_column(scale2, &(0..80).map(|i| BigDecimal::new((i - 40).into(), 2)).collect::<Vec<_>>()).unwrap();
		let blocked_lits: Vec<String> = (0..192).map(|i| if (64..128).contains(&i) { format!("{}", ((10_000_000 + i) * 100 + 1) * if i % 2 == 0 { 1 } else { -1 }) } else { format!("{}", (i % 5) - 2) }).collect();
		let blocked_col = crate::encode_column(PhysicalType::ScaledI64 { scale: 0 }, &col(&blocked_lits.iter().map(String::as_str).collect::<Vec<_>>())).unwrap();
		let for_lits: Vec<String> = (0..192).map(|i| format!("10000000.0{}", i % 7)).collect();
		let for_col = crate::encode_column(scale2, &col(&for_lits.iter().map(String::as_str).collect::<Vec<_>>())).unwrap();
		let varint_col = crate::encode_column(PhysicalType::ScaledI64 { scale: 0 }, &col(&["1", "1000000000", "2", "3"])).unwrap();
		let f64_col = crate::encode_column(PhysicalType::F64, &col(&["0.5", "1.5", "2.5", "3.5", "4.5"])).unwrap();

		for enc in [&bitpack_col, &blocked_col, &for_col, &varint_col, &f64_col] {
			let mut w = ByteWriter::new();
			write_value_column(&mut w, enc);
			let bytes = w.into_vec();
			let full = read_value_column(&mut ByteReader::new(&bytes)).expect("reads");
			for i in [0_usize, 1, enc.len() / 2, enc.len().saturating_sub(1)] {
				assert_eq!(read_value_at(&bytes, i).expect("reads"), full.values.get(i).cloned(), "codec {} index {i}", enc.best_value_codec());
			}
			assert_eq!(read_value_at(&bytes, enc.len()).expect("reads"), None, "out-of-range index must be None ({})", enc.best_value_codec());
		}

		// The opt-in cascade path too: a trending column sealed with the cascading writer must
		// random-access-read the same values the full cascade decode yields.
		let trend_lits: Vec<String> = (0..256).map(|i| format!("{}", 5_000_000_000_i64 + i64::from(i) * 7)).collect();
		let trend = crate::encode_column(PhysicalType::ScaledI64 { scale: 0 }, &col(&trend_lits.iter().map(String::as_str).collect::<Vec<_>>())).unwrap();
		let mut w = ByteWriter::new();
		write_value_column_cascading(&mut w, &trend);
		let bytes = w.into_vec();
		let full = read_value_column(&mut ByteReader::new(&bytes)).expect("reads");
		assert_eq!(full.best_value_codec_cascading(), "scaled_delta_cascade");
		for i in [0_usize, 1, 128, 255] {
			assert_eq!(read_value_at(&bytes, i).expect("reads"), full.values.get(i).cloned(), "cascade index {i}");
		}
		assert_eq!(read_value_at(&bytes, 256).expect("reads"), None);

		// The opt-in transposed path: tile-level random access must agree with the full decode
		// at tile boundaries, inside a tile, and in the short trailing tile.
		let transposed_enc = transposed_corpus();
		let mut w = ByteWriter::new();
		write_value_column_transposed(&mut w, &transposed_enc, 1.05);
		let bytes = w.into_vec();
		let full = read_value_column(&mut ByteReader::new(&bytes)).expect("reads");
		assert_eq!(full.values.len(), transposed_enc.len());
		for i in [0_usize, 1, 1_023, 1_024, 1_025, 2_047, 2_048, 2_499] {
			assert_eq!(read_value_at(&bytes, i).expect("reads"), full.values.get(i).cloned(), "transposed index {i}");
		}
		assert_eq!(read_value_at(&bytes, transposed_enc.len()).expect("reads"), None, "out-of-range index must be None (transposed)");
	}

	#[test]
	fn read_value_range_matches_the_full_decode_across_every_codec() {
		// The windowed value read must equal the full decode sliced to the window, for every
		// codec and every window shape — it is what the range read path relies on to avoid
		// re-walking each codec's block/tile chain per row.
		let scale2 = PhysicalType::ScaledI64 { scale: 2 };
		let bitpack_col = crate::encode_column(scale2, &(0..80).map(|i| BigDecimal::new((i - 40).into(), 2)).collect::<Vec<_>>()).unwrap();
		let blocked_lits: Vec<String> = (0..192).map(|i| if (64..128).contains(&i) { format!("{}", ((10_000_000 + i) * 100 + 1) * if i % 2 == 0 { 1 } else { -1 }) } else { format!("{}", (i % 5) - 2) }).collect();
		let blocked_col = crate::encode_column(PhysicalType::ScaledI64 { scale: 0 }, &col(&blocked_lits.iter().map(String::as_str).collect::<Vec<_>>())).unwrap();
		let for_lits: Vec<String> = (0..192).map(|i| format!("10000000.0{}", i % 7)).collect();
		let for_col = crate::encode_column(scale2, &col(&for_lits.iter().map(String::as_str).collect::<Vec<_>>())).unwrap();
		let varint_col = crate::encode_column(PhysicalType::ScaledI64 { scale: 0 }, &col(&["1", "1000000000", "2", "3"])).unwrap();
		let f64_col = crate::encode_column(PhysicalType::F64, &col(&["0.5", "1.5", "2.5", "3.5", "4.5"])).unwrap();

		// Each column paired with the writer that realizes the codec under test.
		let mut cases: Vec<(String, Vec<u8>, usize)> = Vec::new();
		for enc in [&bitpack_col, &blocked_col, &for_col, &varint_col, &f64_col] {
			let mut w = ByteWriter::new();
			write_value_column(&mut w, enc);
			cases.push((enc.best_value_codec().to_string(), w.into_vec(), enc.len()));
		}
		let transposed_enc = transposed_corpus();
		let mut w = ByteWriter::new();
		write_value_column_transposed(&mut w, &transposed_enc, 1.05);
		cases.push(("scaled_transposed".to_string(), w.into_vec(), transposed_enc.len()));
		let trend_lits: Vec<String> = (0..256).map(|i| format!("{}", 5_000_000_000_i64 + i64::from(i) * 7)).collect();
		let trend = crate::encode_column(PhysicalType::ScaledI64 { scale: 0 }, &col(&trend_lits.iter().map(String::as_str).collect::<Vec<_>>())).unwrap();
		let mut w = ByteWriter::new();
		write_value_column_cascading(&mut w, &trend);
		cases.push(("scaled_delta_cascade".to_string(), w.into_vec(), trend.len()));

		for (codec, bytes, n) in cases {
			let full = read_value_column(&mut ByteReader::new(&bytes)).expect("reads").values;
			assert_eq!(full.len(), n, "{codec}: full decode length");
			// Full span, a prefix, an interior window, a suffix, a single value, a window
			// straddling a block/tile boundary, a zero-length window, and one past the end.
			let windows = [(0_usize, n), (0, 1), (n / 3, n / 3), (n.saturating_sub(3), 3), (n / 2, 1), (1_023.min(n.saturating_sub(1)), 4.min(n)), (n / 2, 0), (n, 5), (n.saturating_sub(2), 100)];
			for (start, len) in windows {
				let got = read_value_range(&bytes, start, len).expect("reads");
				let want = &full[start.min(n)..start.saturating_add(len).min(n)];
				assert_eq!(got.as_slice(), want, "{codec}: window ({start}, {len})");
			}
		}
	}

	#[test]
	fn read_segment_point_matches_value_at_across_codecs() {
		// The streaming point read must equal Segment::value_at for every single-block frame: the
		// two per-block value codecs take the block-skip fast path, the rest fall back to the full
		// decode. Cover present / absent-in-range / out-of-range / duplicate-run / null-row
		// lookups, sorted and out-of-order.

		// A mixed-magnitude scaled column → the blocked value codec (fast path). Reinterpreting the
		// winning-blocked mantissas at scale 2 keeps the exact mantissa distribution (so the codec
		// choice is unchanged) while making the values f64-inexact, so recommend_encoding rejects
		// f64 and selects ScaledI64 (a plain-integer column would encode f64-exact and never reach
		// the block codecs): a wide zero-straddling burst among near-zero values picks blocked.
		let scaled2 = |m: i64| -> String {
			let (sign, a) = (if m < 0 { "-" } else { "" }, m.abs());
			format!("{sign}{}.{:02}", a / 100, a % 100)
		};
		let blocked_lits: Vec<String> = (0..192).map(|i| scaled2(if (64..128).contains(&i) { ((10_000_000 + i) * 100 + 1) * if i % 2 == 0 { 1 } else { -1 } } else { (i % 5) - 2 })).collect();
		let blocked_vals = col(&blocked_lits.iter().map(String::as_str).collect::<Vec<_>>());
		let blocked_ts: Vec<i64> = (0..192).map(|i| i64::from(i) * 10).collect();
		let blocked = Segment::build_sorted(&blocked_ts, &blocked_vals, TimeUnit::Millis, &BigDecimal::from(0)).expect("builds");
		assert_eq!(blocked.values.best_value_codec(), "scaled_blocked");

		// A clustered-high-base scaled column → the FOR value codec (fast path).
		let for_lits: Vec<String> = (0..192).map(|i| format!("10000000.0{}", i % 7)).collect();
		let for_vals = col(&for_lits.iter().map(String::as_str).collect::<Vec<_>>());
		let for_ts: Vec<i64> = (0..192).map(|i| 1_000 + i64::from(i) * 3).collect();
		let for_seg = Segment::build_sorted(&for_ts, &for_vals, TimeUnit::Micros, &BigDecimal::from(0)).expect("builds");
		assert_eq!(for_seg.values.best_value_codec(), "scaled_for");

		// A small-magnitude scaled column that cycles the *full* range within every block →
		// fixed-width bit-pack (fast path): a global width already fits every mantissa and no block
		// has a tighter range, so per-block/FOR headers only cost more. (37 is coprime with 81, so
		// each 64-wide block spans the whole ±40 spread; scale 2 so build selects ScaledI64.)
		let bitpack_lits: Vec<String> = (0..192).map(|i| scaled2((i * 37) % 81 - 40)).collect();
		let bitpack_vals = col(&bitpack_lits.iter().map(String::as_str).collect::<Vec<_>>());
		let bitpack_ts: Vec<i64> = (0..192).map(|i| 5_000 + i64::from(i) * 2).collect();
		let bitpack = Segment::build_sorted(&bitpack_ts, &bitpack_vals, TimeUnit::Millis, &BigDecimal::from(0)).expect("builds");
		assert_eq!(bitpack.values.best_value_codec(), "scaled_bitpack");

		// A wide-sparse column → varint (fallback path); and an out-of-order variant (linear scan).
		let varint = Segment::build(&[10, 20, 30, 40], &col(&["1", "1000000000", "2", "3"]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		let unsorted = Segment::build(&[40, 10, 30, 20], &col(&["4", "1", "3", "2"]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert!(!unsorted.is_time_sorted());

		// An F64 column → fixed-width fallback.
		let f64_seg = Segment::build(&[5, 15, 25], &col(&["0.5", "1.5", "2.5"]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");

		for seg in [&blocked, &for_seg, &bitpack, &varint, &unsorted, &f64_seg] {
			let bytes = seg.write_to();
			let (timestamps, _) = seg.decode_nullable();
			// Query every stored timestamp plus around/outside the span — the streaming read must
			// agree with value_at at each.
			let mut queries: Vec<i64> = timestamps.clone();
			if let (Some(&lo), Some(&hi)) = (timestamps.iter().min(), timestamps.iter().max()) {
				queries.extend([lo - 1, hi + 1, lo + 1]); // out-of-range low/high and an interior miss
			}
			queries.extend([i64::MIN, i64::MAX]);
			for &t in &queries {
				assert_eq!(read_segment_point(&bytes, t).expect("reads"), seg.value_at(t), "codec {} at t={t}", seg.values.best_value_codec());
			}
		}

		// A nullable sorted blocked segment with a duplicate timestamp whose first row is null: the
		// dense-rank mapping (the value column stores only present values) + first-present-of-a-run
		// must still match value_at.
		let mut null_vals: Vec<Option<BigDecimal>> = blocked_vals.iter().cloned().map(Some).collect();
		for &row in &[150_usize, 160, 170] {
			null_vals[row] = None; // null only tail rows (after the burst's dense range) so its
			                       // 64-wide block stays aligned and keeps picking a block codec
		}
		let mut null_ts = blocked_ts.clone();
		null_ts[151] = null_ts[150]; // rows 150 (null) & 151 (present) now share a timestamp
		let nullable = Segment::build_nullable_sorted(&null_ts, &null_vals, TimeUnit::Millis, &BigDecimal::from(0)).expect("builds");
		// A block codec keeps the fast path exercised through the null/dense-rank mapping.
		assert!(matches!(nullable.values.best_value_codec(), "scaled_blocked" | "scaled_for"), "present values still pick a per-block codec, got {}", nullable.values.best_value_codec());
		let bytes = nullable.write_to();
		let (timestamps, _) = nullable.decode_nullable();
		for &t in timestamps.iter().chain(&[null_ts[0] - 5, null_ts[191] + 5]) {
			assert_eq!(read_segment_point(&bytes, t).expect("reads"), nullable.value_at(t), "nullable blocked at t={t}");
		}
	}

	#[test]
	fn read_paged_segment_point_matches_value_at() {
		// The paged streaming point read must equal PagedSegment::value_at for every frame: pages
		// are pruned on their indexed min/max ts, and the surviving page's value block takes the
		// same block-skip fast path. Cover a multi-page FOR segment (dense and nullable, including
		// a duplicate timestamp straddling a page boundary), across present / miss / out-of-range.
		let for_lits: Vec<String> = (0..300).map(|i| format!("10000000.{:02}", i % 97)).collect();
		let for_vals = col(&for_lits.iter().map(String::as_str).collect::<Vec<_>>());
		let for_ts: Vec<i64> = (0..300).map(|i| 1_000 + i64::from(i) * 10).collect();

		let dense = PagedSegment::build(&for_ts, &for_vals, TimeUnit::Millis, &BigDecimal::from(0), 64).expect("builds");
		assert!(dense.page_count() >= 4, "the corpus must span several pages");
		assert!(matches!(dense.pages[0].values.best_value_codec(), "scaled_for" | "scaled_blocked"), "a page must pick a per-block codec so the fast path is exercised");

		// A nullable variant: null a couple of rows, and give rows 128/129 (a page boundary at
		// rows_per_page=64: page 2 starts at row 128) a shared timestamp with the first present.
		let mut null_vals: Vec<Option<BigDecimal>> = for_vals.iter().cloned().map(Some).collect();
		null_vals[128] = None;
		null_vals[200] = None;
		let mut null_ts = for_ts.clone();
		null_ts[129] = null_ts[128]; // rows 128 (null) & 129 (present) share a timestamp at a page edge
		let nullable = PagedSegment::build_nullable(&null_ts, &null_vals, TimeUnit::Millis, &BigDecimal::from(0), 64).expect("builds");

		for seg in [&dense, &nullable] {
			let bytes = seg.write_to();
			let (timestamps, _) = seg.decode_nullable();
			let mut queries: Vec<i64> = timestamps.clone();
			if let (Some(&lo), Some(&hi)) = (timestamps.iter().min(), timestamps.iter().max()) {
				queries.extend([lo - 1, hi + 1, lo + 5]); // out-of-range low/high and an interior miss
			}
			queries.extend([i64::MIN, i64::MAX]);
			for &t in &queries {
				assert_eq!(read_paged_segment_point(&bytes, t).expect("reads"), seg.value_at(t), "paged at t={t}");
			}
		}
	}

	#[test]
	fn batch_point_reads_match_the_per_instant_reads() {
		// The batch readers must return, per instant, exactly what the single-instant readers do —
		// including duplicate instants, misses, and out-of-range, in a scrambled order (the result
		// is aligned to the query slice, not sorted).
		let for_lits: Vec<String> = (0..300).map(|i| format!("10000000.{:02}", i % 97)).collect();
		let for_vals = col(&for_lits.iter().map(String::as_str).collect::<Vec<_>>());
		let for_ts: Vec<i64> = (0..300).map(|i| 1_000 + i64::from(i) * 10).collect();
		let single = Segment::build_sorted(&for_ts, &for_vals, TimeUnit::Millis, &BigDecimal::from(0)).expect("builds");
		let paged = PagedSegment::build(&for_ts, &for_vals, TimeUnit::Millis, &BigDecimal::from(0), 64).expect("builds");

		// A scrambled batch: present instants, a repeat, an interior miss, and both out-of-range ends.
		let batch: Vec<i64> = vec![2500, 1000, 3990, 2500, 2505, 0, 9_999_999, 1_500, 1_505];

		let single_bytes = single.write_to();
		let seg_batch = read_segment_points(&single_bytes, &batch).expect("reads");
		assert_eq!(seg_batch.len(), batch.len());
		for (k, &t) in batch.iter().enumerate() {
			assert_eq!(seg_batch[k], read_segment_point(&single_bytes, t).expect("reads"), "single-block batch slot {k} (t={t})");
		}

		let paged_bytes = paged.write_to();
		let paged_batch = read_paged_segment_points(&paged_bytes, &batch).expect("reads");
		assert_eq!(paged_batch.len(), batch.len());
		for (k, &t) in batch.iter().enumerate() {
			assert_eq!(paged_batch[k], read_paged_segment_point(&paged_bytes, t).expect("reads"), "paged batch slot {k} (t={t})");
		}
		// An empty query slice is answered with an empty vector, touching nothing.
		assert!(read_segment_points(&single_bytes, &[]).expect("reads").is_empty());
		assert!(read_paged_segment_points(&paged_bytes, &[]).expect("reads").is_empty());
	}

	#[test]
	fn read_segment_range_matches_the_full_decode_filter() {
		// The windowed range read must equal `decode_nullable()` filtered to [start, end] for every
		// window — the regular/random-access-codec segments take the closed-form fast path, the
		// irregular / non-block-codec / out-of-order ones the full-decode fallback.
		let scaled2 = |m: i64| -> String {
			let (sign, a) = (if m < 0 { "-" } else { "" }, m.abs());
			format!("{sign}{}.{:02}", a / 100, a % 100)
		};
		// Regular + FOR value codec (fast path).
		let for_lits: Vec<String> = (0..300).map(|i| format!("10000000.{:02}", i % 97)).collect();
		let for_vals = col(&for_lits.iter().map(String::as_str).collect::<Vec<_>>());
		let regular_ts: Vec<i64> = (0..300).map(|i| 1_000 + i64::from(i) * 10).collect();
		let regular = Segment::build_sorted(&regular_ts, &for_vals, TimeUnit::Millis, &BigDecimal::from(0)).expect("builds");
		assert_eq!(regular.values.best_value_codec(), "scaled_for");
		// Regular + nulls (sparse fast path exercises the dense-rank window).
		let mut null_vals: Vec<Option<BigDecimal>> = for_vals.iter().cloned().map(Some).collect();
		for &row in &[3_usize, 4, 128, 250] {
			null_vals[row] = None;
		}
		let regular_null = Segment::build_nullable_sorted(&regular_ts, &null_vals, TimeUnit::Millis, &BigDecimal::from(0)).expect("builds");
		// Irregular monotonic (fallback: block codec but non-constant stride).
		let irr_lits: Vec<String> = (0..64).map(|i| scaled2((i * 37) % 81 - 40)).collect();
		let irregular = Segment::build_sorted(&[0_i64, 5, 6, 20, 21, 40, 100, 101].iter().chain((200..256).collect::<Vec<_>>().iter()).copied().collect::<Vec<_>>(), &col(&irr_lits.iter().map(String::as_str).collect::<Vec<_>>()), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		// F64 (non-random-access value codec → fallback), regular timestamps.
		let f64_seg = Segment::build(&[10_i64, 20, 30, 40, 50], &col(&["0.5", "1.5", "2.5", "3.5", "4.5"]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		// Out-of-order (fallback).
		let unsorted = Segment::build(&[40_i64, 10, 30, 20], &col(&["4", "1", "3", "2"]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");

		for seg in [&regular, &regular_null, &irregular, &f64_seg, &unsorted] {
			let bytes = seg.write_to();
			let (all_ts, all_vs) = seg.decode_nullable();
			let (lo, hi) = (all_ts.iter().min().copied().unwrap_or(0), all_ts.iter().max().copied().unwrap_or(0));
			// Windows: full span, an interior sub-window (on- and off-grid ends), single-point,
			// empty (gap), and fully out of range on both sides.
			for (start, end) in [(lo, hi), (lo + 15, hi - 15), (lo - 100, lo - 1), (hi + 1, hi + 100), (lo + 105, lo + 105), (lo + 106, lo + 107)] {
				let (rt, rv) = read_segment_range(&bytes, start, end).expect("reads");
				let expected: (Vec<i64>, Vec<Option<BigDecimal>>) = all_ts.iter().zip(&all_vs).filter(|(t, _)| start <= **t && **t <= end).map(|(&t, v)| (t, v.clone())).unzip();
				assert_eq!((rt, rv), expected, "codec {} window [{start},{end}]", seg.values.best_value_codec());
			}
		}

		// Paged variant: a multi-page regular FOR frame — read_paged_segment_range must equal
		// PagedSegment::read_time_range for every window (page skipping + per-page closed-form).
		let paged = PagedSegment::build(&regular_ts, &for_vals, TimeUnit::Millis, &BigDecimal::from(0), 64).expect("builds");
		let paged_bytes = paged.write_to();
		let (lo, hi) = (regular_ts[0], regular_ts[299]);
		for (start, end) in [(lo, hi), (lo + 615, hi - 615), (lo - 100, lo - 1), (hi + 1, hi + 100), (lo + 655, lo + 655), (lo + 656, lo + 657)] {
			let (rt, rv) = read_paged_segment_range(&paged_bytes, start, end).expect("reads");
			assert_eq!((rt, rv), paged.read_time_range(start, end), "paged window [{start},{end}]");
		}
	}

	#[test]
	fn cascade_codec_on_a_non_scaled_column_is_rejected() {
		// A hand-crafted F64 value block claiming the delta cascade must be refused — the
		// cascade is only defined over a ScaledI64 mantissa stream.
		let mut w = ByteWriter::new();
		w.put_u8(TAG_F64);
		w.put_uvarint(2); // count
		w.put_uvarint(0); // lossy_count
		w.put_str("0"); // max_abs_error
		w.put_u8(VAL_CODEC_DELTA_CASCADE);
		w.put_svarint(0); // anchor
		w.put_u8(CASCADE_INNER_VARINT);
		w.put_svarint(1); // one delta
		let bytes = w.into_vec();
		let mut r = ByteReader::new(&bytes);
		assert_eq!(read_value_column(&mut r), Err(WeftSegError::InvalidTag { kind: "value_codec_delta_cascade_type", value: TAG_F64 }));
	}

	#[test]
	fn cascade_with_an_unknown_inner_descriptor_is_rejected() {
		// A ScaledI64 cascade block whose inner descriptor byte is not one of the five known
		// packers must error rather than mis-decode.
		let mut w = ByteWriter::new();
		w.put_u8(TAG_SCALED_I64);
		w.put_u8(0); // scale
		w.put_uvarint(2); // count
		w.put_uvarint(0); // lossy_count
		w.put_str("0"); // max_abs_error
		w.put_u8(VAL_CODEC_DELTA_CASCADE);
		w.put_svarint(0); // anchor
		w.put_u8(99); // unknown inner descriptor
		let bytes = w.into_vec();
		let mut r = ByteReader::new(&bytes);
		assert_eq!(read_value_column(&mut r), Err(WeftSegError::InvalidTag { kind: "value_codec_cascade_inner", value: 99 }));
	}

	#[test]
	fn blocked_codec_on_a_non_scaled_column_is_rejected() {
		// A hand-crafted F64 value block that claims the blocked codec must be refused —
		// per-block bit-packing is only defined for a ScaledI64 payload.
		let mut w = ByteWriter::new();
		w.put_u8(TAG_F64);
		w.put_uvarint(1); // count
		w.put_uvarint(0); // lossy_count
		w.put_str("0"); // max_abs_error
		w.put_u8(VAL_CODEC_BLOCKED);
		w.put_uvarint(64); // block size
		w.put_bytes(&[0, 0]); // a length-prefixed (bogus) payload
		let bytes = w.into_vec();
		let mut r = ByteReader::new(&bytes);
		assert_eq!(read_value_column(&mut r), Err(WeftSegError::InvalidTag { kind: "value_codec_blocked_type", value: TAG_F64 }));
	}

	#[test]
	fn segment_frame_round_trips_a_blocked_scaled_column() {
		// A mixed-magnitude scaled-int series that seals to the blocked value codec must
		// round-trip through the full framed segment (header, CRC, v4 layout). Two-decimal
		// values force the ScaledI64 encoding (F64 cannot represent 0.01 exactly); the
		// quiet region straddles zero symmetrically and the burst (a full 64-wide block
		// near ±1e7) alternates sign, so each block's FOR residual range equals its
		// zig-zag magnitude — per-block bit-packing beats the varint, the global
		// bit-pack, and FOR (whose per-block reference is wasted here).
		let ts: Vec<i64> = (0..192).map(|i| 1_000 + i * 5).collect();
		let vs: Vec<BigDecimal> = (0..192)
			.map(|i| {
				let mantissa: i64 = if (64..128).contains(&i) { ((10_000_000 + i) * 100 + 1) * if i % 2 == 0 { 1 } else { -1 } } else { (i % 5) - 2 };
				BigDecimal::new(mantissa.into(), 2)
			})
			.collect();
		let seg = Segment::build(&ts, &vs, TimeUnit::Millis, &BigDecimal::from(0)).expect("builds");
		assert_eq!(seg.values.best_value_codec(), "scaled_blocked", "mixed-magnitude scaled stream must pick the blocked codec");
		let bytes = write_segment(&seg);
		let back = read_segment(&bytes).expect("reads");
		assert_eq!(back, seg, "blocked segment frame must round-trip exactly");
		assert_eq!(back.version, SEGMENT_FORMAT_VERSION);
	}

	#[test]
	fn segment_frame_round_trips_a_bitpacked_scaled_column() {
		// A regular zero-symmetric scaled-int series that seals to the bit-pack value
		// codec must round-trip through the full framed segment (header, CRC, v4
		// layout). Symmetric mantissas keep FOR from strictly winning (its residual
		// width equals zig-zag's, so the reference varint loses the tie).
		let ts: Vec<i64> = (0..64).map(|i| 1_000 + i * 5).collect();
		let vs: Vec<BigDecimal> = (0..64).map(|i| BigDecimal::new((i - 32).into(), 2)).collect();
		let seg = Segment::build(&ts, &vs, TimeUnit::Millis, &BigDecimal::from(0)).expect("builds");
		assert_eq!(seg.values.best_value_codec(), "scaled_bitpack", "regular scaled stream must pick bit-pack");
		let bytes = write_segment(&seg);
		let back = read_segment(&bytes).expect("reads");
		assert_eq!(back, seg, "bit-packed segment frame must round-trip exactly");
		assert_eq!(back.version, SEGMENT_FORMAT_VERSION);
	}

	#[test]
	fn serialized_bytes_matches_the_realized_value_payload() {
		use crate::encode_column;
		// serialized_bytes() must equal the exact bytes write_physical_value emits for
		// the values (the payload, excluding the column header) — for every physical
		// type. This is the realize-accurate figure estimated_bytes() does not give.
		let cases = [encode_column(PhysicalType::F64, &col(&["0.5", "2.25", "-128.0"])).unwrap(), encode_column(PhysicalType::F32, &col(&["0.5", "-0.25", "16.0"])).unwrap(), encode_column(PhysicalType::ScaledI64 { scale: 2 }, &col(&["1.25", "-3.75", "0.00", "5000.00"])).unwrap(), encode_column(PhysicalType::ScaledI128 { scale: 4 }, &col(&["1234567890.1234", "-9.0001"])).unwrap(), encode_column(PhysicalType::Decimal128, &col(&["123456789012345678901234.567890", "-1.5", "0"])).unwrap(), encode_column(PhysicalType::BigDecimalText, &col(&["1.5", "12345.6789", "-0.000001"])).unwrap()];
		for enc in &cases {
			let mut w = ByteWriter::new();
			for value in &enc.values {
				write_physical_value(&mut w, value);
			}
			assert_eq!(w.into_vec().len(), enc.serialized_bytes(), "{:?} payload must match serialized_bytes()", enc.physical_type);
		}
	}

	#[test]
	fn scaled_i64_serialized_bytes_beats_the_naive_fixed_width_estimate() {
		use crate::encode_column;
		// Small ScaledI64 mantissas are varint-coded to ~1 byte each, so the realized
		// payload is far below the naive len*8 estimate — the divergence estimated_bytes()
		// currently over-reports (underselling WeftDB's bytes/point).
		let enc = encode_column(PhysicalType::ScaledI64 { scale: 2 }, &col(&["0.01", "0.02", "0.03", "0.05", "0.08"])).unwrap();
		assert!(enc.serialized_bytes() < enc.estimated_bytes(), "realized {} must beat naive {}", enc.serialized_bytes(), enc.estimated_bytes());
		assert_eq!(enc.estimated_bytes(), enc.values.len() * 8, "the naive estimate is len*8 for ScaledI64");
	}

	#[test]
	fn value_column_preserves_lossy_bookkeeping() {
		use crate::encode_column;
		// 0.1/0.3 are not binary-exact: the column is lossy and carries a positive
		// max error that must survive the round trip.
		let enc = encode_column(PhysicalType::F64, &col(&["0.5", "0.1", "0.3"])).unwrap();
		assert!(!enc.is_exact());
		let zero = BigDecimal::from(0);
		assert!(enc.max_abs_error > zero);
		assert_value_col_round_trips(&enc);
	}

	#[test]
	fn value_column_recommend_encoding_round_trips() {
		// The realistic path: let recommend_encoding pick, then seal/read it back.
		let values: Vec<BigDecimal> = (0..50).map(|i| BigDecimal::from_str(&format!("{i}.{:02}", i % 100)).unwrap()).collect();
		let enc = crate::recommend_encoding(&values, &BigDecimal::from(0));
		assert_value_col_round_trips(&enc);
	}

	#[test]
	fn empty_value_column_round_trips() {
		use crate::encode_column;
		assert_value_col_round_trips(&encode_column(PhysicalType::F64, &[]).unwrap());
		assert_value_col_round_trips(&encode_column(PhysicalType::BigDecimalText, &[]).unwrap());
	}

	#[test]
	fn unknown_physical_type_tag_is_rejected() {
		let bytes = [99_u8, 0, 0]; // tag 99 is not a physical type
		let mut r = ByteReader::new(&bytes);
		assert_eq!(read_value_column(&mut r), Err(WeftSegError::InvalidTag { kind: "physical_type", value: 99 }));
	}

	/// Round-trip a timestamp column through the codec, asserting the column and its
	/// decoded epochs both recover exactly.
	fn assert_ts_col_round_trips(values: &[i64], unit: TimeUnit) {
		use crate::{decode_delta_of_delta, encode_delta_of_delta};
		let enc = encode_delta_of_delta(values, unit);
		let mut w = ByteWriter::new();
		write_timestamp_column(&mut w, &enc);
		let bytes = w.into_vec();
		let mut r = ByteReader::new(&bytes);
		let back = read_timestamp_column(&mut r).expect("reads");
		assert!(r.is_empty(), "timestamp codec must consume its whole block");
		assert_eq!(back, enc);
		assert_eq!(decode_delta_of_delta(&back), if values.is_empty() { vec![0] } else { values.to_vec() });
	}

	#[test]
	fn timestamp_column_round_trips_regular_and_irregular() {
		// Perfectly regular (all-zero second differences).
		assert_ts_col_round_trips(&(0..1_000).map(|i| 1_000 + i * 10).collect::<Vec<_>>(), TimeUnit::Millis);
		// Irregular gaps.
		assert_ts_col_round_trips(&[5, 9, 12, 100, 101, 102, 50], TimeUnit::Micros);
		// Each unit tag round-trips.
		assert_ts_col_round_trips(&[1, 2, 3], TimeUnit::Seconds);
		assert_ts_col_round_trips(&[10, 20], TimeUnit::Nanos);
	}

	#[test]
	fn timestamp_column_round_trips_edge_sizes() {
		// Single point (first_delta None) and empty.
		assert_ts_col_round_trips(&[42], TimeUnit::Seconds);
		assert_ts_col_round_trips(&[], TimeUnit::Seconds);
		// Extreme values exercise the wrapping arithmetic and full-width varints.
		assert_ts_col_round_trips(&[i64::MIN, i64::MAX, 0, i64::MIN], TimeUnit::Nanos);
	}

	#[test]
	fn unknown_time_unit_tag_is_rejected() {
		let bytes = [7_u8]; // tag 7 is not a time unit
		let mut r = ByteReader::new(&bytes);
		assert_eq!(read_timestamp_column(&mut r), Err(WeftSegError::InvalidTag { kind: "time_unit", value: 7 }));
	}

	#[test]
	fn regular_series_timestamp_block_bit_packs_on_disk() {
		use crate::encode_delta_of_delta;
		// A regular 1000-point series: second differences are all zero, so the writer
		// picks bit-packing (width 0). The on-disk block is a handful of bytes — far
		// below the ~1000 a per-value varint stream would spend — and still round-trips.
		let values: Vec<i64> = (0..1_000).map(|i| 1_000 + i * 10).collect();
		let enc = encode_delta_of_delta(&values, TimeUnit::Millis);
		let mut w = ByteWriter::new();
		write_timestamp_column(&mut w, &enc);
		let bytes = w.into_vec();
		// unit(1) + anchor(8) + first_delta flag(1)+svarint(1) + count varint(2) +
		// codec byte(1) + width byte(1) + 0 data bytes = 15.
		assert!(bytes.len() < 20, "regular series must bit-pack tiny: {} bytes", bytes.len());
		let mut r = ByteReader::new(&bytes);
		assert_eq!(read_timestamp_column(&mut r).expect("reads"), enc);
	}

	#[test]
	fn scattered_single_jitter_timestamp_block_uses_gorilla_on_disk() {
		use crate::{encode_delta_of_delta, timestamp::zigzag_varint_bytes};
		// The roadmap-6.1 regime: a regular 1000ms base where every 16th interval carries
		// an isolated moderate jitter within Gorilla's +/-2048 bucket. best_encoding_name
		// picks Gorilla, so the writer stores it, and the block round-trips losslessly and
		// is far below a per-value varint stream.
		let mut ts = Vec::with_capacity(1000);
		let mut t = 0_i64;
		for i in 0..1000 {
			t += if i % 16 == 15 { 1_000 + 1_500 } else { 1_000 };
			ts.push(t);
		}
		let enc = encode_delta_of_delta(&ts, TimeUnit::Millis);
		assert_eq!(enc.best_encoding_name(), "delta_of_delta_gorilla", "scattered jitter must route to the Gorilla codec");
		let mut w = ByteWriter::new();
		write_timestamp_column(&mut w, &enc);
		let bytes = w.into_vec();
		// The Gorilla codec byte sits right after the count varint. unit(1)+anchor(8)+
		// first_delta flag(1)+svarint + count varint; the flag byte is 1, so the codec
		// tag is the byte after the first_delta svarint + count varint — assert the block
		// beats varint decisively and round-trips instead of hunting the exact offset.
		assert!(bytes.len() < zigzag_varint_bytes(&enc.dods), "Gorilla block {} must beat the {}-byte varint stream", bytes.len(), zigzag_varint_bytes(&enc.dods));
		let mut r = ByteReader::new(&bytes);
		let back = read_timestamp_column(&mut r).expect("reads");
		assert!(r.is_empty(), "the Gorilla block must be fully consumed");
		assert_eq!(back, enc);
		assert_eq!(crate::decode_delta_of_delta(&back), ts, "epochs recover exactly through the Gorilla codec");
	}

	#[test]
	fn mixed_magnitude_timestamp_block_uses_blocked_on_disk() {
		use crate::{timestamp::bitpack_bytes, DeltaOfDeltaColumn};
		// The roadmap-6.1 dynamic-bit-packing regime: a second-difference stream that is
		// mostly narrow ±1 jitter with one contiguous wide window (inside a single 64-wide
		// block). best_encoding_name picks the per-block adaptive codec, so the writer
		// stores it; the block round-trips losslessly and beats the single global-width
		// bit-pack decisively. Build the column directly so the dods carry the intended
		// shape (a level shift in epochs would only spike the dods at its edges).
		let dods: Vec<i64> = (0..256).map(|i| if (96..128).contains(&i) { 500_000_000 + i } else { (i % 3) - 1 }).collect();
		let enc = DeltaOfDeltaColumn { first: 1_000, first_delta: Some(1_000), dods, unit: TimeUnit::Millis };
		assert_eq!(enc.best_encoding_name(), "delta_of_delta_blocked", "a mixed-magnitude window must route to the blocked codec");
		let mut w = ByteWriter::new();
		write_timestamp_column(&mut w, &enc);
		let bytes = w.into_vec();
		assert!(bytes.len() < 8 + bitpack_bytes(&enc.dods), "blocked block {} must beat the global bit-pack {}", bytes.len(), 8 + bitpack_bytes(&enc.dods));
		let mut r = ByteReader::new(&bytes);
		let back = read_timestamp_column(&mut r).expect("reads");
		assert!(r.is_empty(), "the blocked block must be fully consumed");
		assert_eq!(back, enc, "the blocked timestamp block must round-trip exactly");
		assert_eq!(crate::decode_delta_of_delta(&back), crate::decode_delta_of_delta(&enc), "epochs recover exactly through the blocked codec");
	}

	#[test]
	fn gorilla_timestamp_block_round_trips_via_the_shared_helper() {
		// Route scattered jitter through the whole-block-consumption helper too.
		let mut ts = Vec::with_capacity(320);
		let mut t = 0_i64;
		for i in 0..320 {
			t += if i % 10 == 9 { 1_000 + 800 } else { 1_000 };
			ts.push(t);
		}
		assert_ts_col_round_trips(&ts, TimeUnit::Millis);
	}

	#[test]
	fn constant_run_timestamp_block_uses_rle_on_disk() {
		use crate::encode_delta_of_delta;
		// A constant nonzero acceleration: deltas grow by a fixed step, so the second
		// differences are one long run. best_encoding_name picks RLE, so the writer now
		// *realizes* it on disk (previously it fell back to bit-pack/varint while still
		// reporting "delta_of_delta_rle" — the divergence this slice closes). The block
		// round-trips exactly and the on-disk codec matches the reported name.
		let mut values = vec![0_i64, 1];
		let mut delta = 1_i64;
		for _ in 0..300 {
			delta += 5;
			let next = values.last().unwrap() + delta;
			values.push(next);
		}
		let enc = encode_delta_of_delta(&values, TimeUnit::Seconds);
		assert_eq!(enc.best_encoding_name(), "delta_of_delta_rle", "a long constant run must route to RLE");
		let mut w = ByteWriter::new();
		write_timestamp_column(&mut w, &enc);
		let bytes = w.into_vec();
		// A single run collapses to a few bytes — far below a per-value stream.
		assert!(bytes.len() < 40, "RLE block must be tiny for one run: {} bytes", bytes.len());
		let mut r = ByteReader::new(&bytes);
		let back = read_timestamp_column(&mut r).expect("reads");
		assert!(r.is_empty(), "the RLE block must be fully consumed");
		assert_eq!(back, enc);
		assert_eq!(crate::decode_delta_of_delta(&back), values, "epochs recover exactly through the RLE codec");
	}

	#[test]
	fn unknown_timestamp_codec_tag_is_rejected() {
		// unit=seconds(0), anchor i64=0 (8 bytes), first_delta flag=0, count=0, codec=99.
		let bytes = [0_u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 99];
		let mut r = ByteReader::new(&bytes);
		assert_eq!(read_timestamp_column(&mut r), Err(WeftSegError::InvalidTag { kind: "timestamp_codec", value: 99 }));
	}

	/// Build a segment, seal it to a `.weftseg` frame, read it back, and assert exact
	/// recovery of the segment and its decoded columns.
	fn assert_segment_frame_round_trips(timestamps: &[i64], values: &[BigDecimal], unit: TimeUnit, tolerance: &BigDecimal) {
		let seg = Segment::build(timestamps, values, unit, tolerance).expect("builds");
		let bytes = write_segment(&seg);
		assert!(bytes.starts_with(MAGIC), "frame must carry the magic prefix");
		let back = read_segment(&bytes).expect("reads");
		assert_eq!(back, seg, "segment frame must round-trip exactly");
		assert_eq!(back.decode(), seg.decode());
		// And via the ergonomic Segment methods.
		assert_eq!(Segment::read_from(&seg.write_to()).expect("reads"), seg);
	}

	#[test]
	fn segment_frame_round_trips_across_shapes() {
		// Regular series, lossless.
		let ts: Vec<i64> = (0..200).map(|i| 1_000 + i * 10).collect();
		let vs: Vec<BigDecimal> = (0..200).map(BigDecimal::from).collect();
		assert_segment_frame_round_trips(&ts, &vs, TimeUnit::Millis, &BigDecimal::from(0));
		// Irregular timestamps + scaled-decimal values.
		assert_segment_frame_round_trips(&[30, 10, 50, 20], &col(&["3.50", "1.25", "9.75", "2.00"]), TimeUnit::Seconds, &BigDecimal::from(0));
		// Lossy F32 within tolerance (non-exact value column).
		assert_segment_frame_round_trips(&[0, 1, 2], &col(&["0.1", "0.2", "0.3"]), TimeUnit::Micros, &BigDecimal::from_str("0.01").unwrap());
		// High-precision text-backed values.
		assert_segment_frame_round_trips(&[100, 200], &col(&["123456789012345678901234567890.5", "-0.000000000001"]), TimeUnit::Nanos, &BigDecimal::from(0));
		// Single point and empty.
		assert_segment_frame_round_trips(&[42], &col(&["7.5"]), TimeUnit::Seconds, &BigDecimal::from(0));
		assert_segment_frame_round_trips(&[], &[], TimeUnit::Seconds, &BigDecimal::from(0));
	}

	#[test]
	fn nullable_segment_frame_round_trips() {
		let ncol = |lits: &[Option<&str>]| -> Vec<Option<BigDecimal>> { lits.iter().map(|o| o.map(|s| BigDecimal::from_str(s).expect("parses"))).collect() };
		let assert_nullable_round_trips = |timestamps: &[i64], values: &[Option<BigDecimal>]| {
			let seg = Segment::build_nullable(timestamps, values, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
			let bytes = write_segment(&seg);
			let back = read_segment(&bytes).expect("reads");
			assert_eq!(back, seg, "nullable segment frame must round-trip exactly (mask included)");
			assert_eq!(back.decode_nullable(), seg.decode_nullable());
			assert_eq!(back.null_count(), seg.null_count());
		};
		// Interior gaps.
		assert_nullable_round_trips(&[10, 20, 30, 40, 50], &ncol(&[Some("1.5"), None, Some("3.5"), None, Some("5.5")]));
		// Leading and trailing nulls, multi-byte bitmap (9 rows ⇒ 2 bytes).
		assert_nullable_round_trips(&(0..9).collect::<Vec<_>>(), &ncol(&[None, Some("1"), Some("2"), Some("3"), None, Some("5"), Some("6"), Some("7"), None]));
		// Every row null — an empty value column with a full-clear mask.
		assert_nullable_round_trips(&[1, 2, 3], &ncol(&[None, None, None]));
		// A fully-present nullable build seals to the dense frame and still round-trips.
		assert_nullable_round_trips(&[100, 110, 120], &ncol(&[Some("1.0"), Some("2.0"), Some("3.0")]));
	}

	#[test]
	fn corrupt_null_mask_block_is_rejected() {
		// Build a sparse segment, then corrupt the declared null count in the header
		// so it disagrees with the bitmap the frame carries. The CRC is recomputed so
		// the integrity gate passes and the null-mask validation fires instead.
		let ncol = |lits: &[Option<&str>]| -> Vec<Option<BigDecimal>> { lits.iter().map(|o| o.map(|s| BigDecimal::from_str(s).expect("parses"))).collect() };
		let seg = Segment::build_nullable(&[1, 2, 3, 4], &ncol(&[Some("1.0"), None, Some("3.0"), None]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		let bytes = write_segment(&seg);
		// Header layout: MAGIC (7) + version (2) + row_count uvarint + null_count uvarint.
		// row_count = 4 and null_count = 2 are both single-byte varints; null_count is
		// the 10th byte (index 9 + 1 for row_count = index 10).
		let null_count_idx = MAGIC.len() + 2 + 1;
		let body_len = bytes.len() - 4;
		let mut bad = bytes;
		bad[null_count_idx] = 1; // claim 1 null where the bitmap encodes 2
		let new_crc = crc32(&bad[..body_len]);
		bad[body_len..].copy_from_slice(&new_crc.to_le_bytes());
		assert_eq!(read_segment(&bad), Err(WeftSegError::InvalidNullMask(crate::nulls::NullMaskError::NullCountMismatch { declared: 1, actual: 2 })));
	}

	#[test]
	fn corrupt_frame_is_detected_by_checksum() {
		let seg = Segment::build(&[10, 20, 30, 40], &col(&["1.0", "2.0", "3.0", "4.0"]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		let good = write_segment(&seg);
		// Flipping any single body byte trips the CRC.
		for i in [MAGIC.len() + 2, good.len() / 2, good.len() - 5] {
			let mut bad = good.clone();
			bad[i] ^= 0xFF;
			match read_segment(&bad) {
				Err(WeftSegError::ChecksumMismatch { .. }) => {}
				other => panic!("byte {i} flip must be a checksum mismatch, got {other:?}"),
			}
		}
		// Corrupting the trailing checksum itself is also caught.
		let mut bad_crc = good;
		let last = bad_crc.len() - 1;
		bad_crc[last] ^= 0x01;
		assert!(matches!(read_segment(&bad_crc), Err(WeftSegError::ChecksumMismatch { .. })));
	}

	#[test]
	fn bad_magic_and_short_frames_are_rejected() {
		// Too short to even hold a checksum.
		assert_eq!(read_segment(&[0, 1, 2]), Err(WeftSegError::UnexpectedEof { needed: 4, remaining: 3 }));
		// A correctly-checksummed frame whose body is 7 wrong-magic bytes: the CRC
		// gate passes, then the magic check rejects it.
		let mut bad = b"NOTSEG!".to_vec(); // 7 bytes, wrong magic
		bad.extend_from_slice(&crc32(&bad).to_le_bytes());
		assert_eq!(read_segment(&bad), Err(WeftSegError::BadMagic));
	}

	#[test]
	fn unsupported_version_is_rejected() {
		let seg = Segment::build(&[1, 2], &col(&["1.0", "2.0"]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		let mut bytes = write_segment(&seg);
		// Bump the version field (just after the 7-byte magic) and re-checksum so the
		// frame is valid except for the version.
		let vpos = MAGIC.len();
		bytes[vpos] = bytes[vpos].wrapping_add(1);
		let body_len = bytes.len() - 4;
		let new_crc = crc32(&bytes[..body_len]).to_le_bytes();
		bytes[body_len..].copy_from_slice(&new_crc);
		assert_eq!(read_segment(&bytes), Err(WeftSegError::UnsupportedVersion { found: SEGMENT_FORMAT_VERSION + 1 }));
	}

	#[test]
	fn realized_frame_decodes_to_the_original_data() {
		// End-to-end: raw data -> segment -> bytes -> segment -> raw data.
		let ts: Vec<i64> = (0..500).map(|i| 1_600_000_000 + i * 60).collect();
		let vs: Vec<BigDecimal> = (0..500).map(|i| BigDecimal::from_str(&format!("{}.{:03}", i, (i * 7) % 1000)).unwrap()).collect();
		let seg = Segment::build(&ts, &vs, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		let back = Segment::read_from(&seg.write_to()).expect("reads");
		let (rts, rvs) = back.decode();
		assert_eq!(rts, ts);
		assert_eq!(rvs, vs);
		assert!(back.is_exact());
	}

	#[test]
	fn paged_segment_frame_round_trips_across_shapes() {
		use crate::PagedSegment;
		let ncol = |lits: &[Option<&str>]| -> Vec<Option<BigDecimal>> { lits.iter().map(|o| o.map(|s| BigDecimal::from_str(s).expect("parses"))).collect() };
		// Multi-page regular series, lossless.
		let ts: Vec<i64> = (0..500).map(|i| 1_600_000_000 + i * 60).collect();
		let vs: Vec<BigDecimal> = (0..500).map(BigDecimal::from).collect();
		let seg = PagedSegment::build(&ts, &vs, TimeUnit::Seconds, &BigDecimal::from(0), 64).expect("builds");
		assert!(seg.page_count() > 1);
		let bytes = write_paged_segment(&seg);
		assert!(bytes.starts_with(MAGIC), "paged frame carries the magic prefix");
		let back = read_paged_segment(&bytes).expect("reads");
		assert_eq!(back, seg, "paged frame must round-trip exactly");
		assert_eq!(back.decode_nullable(), seg.decode_nullable());
		// Via the ergonomic methods.
		assert_eq!(PagedSegment::read_from(&seg.write_to()).expect("reads"), seg);
		// Nullable, multi-page, with gaps spanning page boundaries.
		let nts: Vec<i64> = (0..20).collect();
		let nvs = ncol(&[Some("1"), None, Some("3"), Some("4"), None, Some("6"), Some("7"), Some("8"), None, Some("10"), Some("11"), None, Some("13"), Some("14"), Some("15"), None, Some("17"), Some("18"), Some("19"), None]);
		let nseg = PagedSegment::build_nullable(&nts, &nvs, TimeUnit::Millis, &BigDecimal::from(0), 7).expect("builds");
		let nback = PagedSegment::read_from(&nseg.write_to()).expect("reads");
		assert_eq!(nback, nseg);
		assert_eq!(nback.null_count(), nseg.null_count());
		// Single page and empty.
		let one = PagedSegment::build(&[42], &col(&["7.5"]), TimeUnit::Seconds, &BigDecimal::from(0), 1_000).expect("builds");
		assert_eq!(PagedSegment::read_from(&one.write_to()).expect("reads"), one);
		let empty = PagedSegment::build(&[], &[], TimeUnit::Seconds, &BigDecimal::from(0), 64).expect("builds");
		assert_eq!(PagedSegment::read_from(&empty.write_to()).expect("reads"), empty);
	}

	#[test]
	fn paged_frame_preserves_per_page_pruning_after_read() {
		use crate::PagedSegment;
		// 12 rows, 4 per page ⇒ pages spanning [0,30], [40,70], [80,110].
		let ts: Vec<i64> = (0..12).map(|i| i * 10).collect();
		let vs: Vec<BigDecimal> = (0..12).map(BigDecimal::from).collect();
		let seg = PagedSegment::build(&ts, &vs, TimeUnit::Seconds, &BigDecimal::from(0), 4).expect("builds");
		let back = PagedSegment::read_from(&seg.write_to()).expect("reads");
		// The per-page index survives the round trip, so pruning works on the reread.
		assert_eq!(back.prune_pages_by_time(45, 65), vec![1]);
		assert_eq!(back.read_time_range(35, 75).0, vec![40, 50, 60, 70]);
	}

	/// The checkpointed frame must be *indistinguishable* from the plain one on every read
	/// path — the whole safety claim of an additive codec tag.
	#[test]
	fn checkpointed_frame_reads_identically_to_the_plain_frame() {
		// A sorted IRREGULAR column (varying gaps) — the shape with no closed form, which
		// is what the checkpoint index exists for.
		let mut t = 1_000_i64;
		let ts: Vec<i64> = (0..600)
			.map(|i: i64| {
				t += 1 + (i * 7) % 23;
				t
			})
			.collect();
		let vs: Vec<BigDecimal> = (0..600).map(|i| BigDecimal::from_str(&format!("{}.5", 100 + i % 50)).unwrap()).collect();
		let seg = Segment::build(&ts, &vs, TimeUnit::Millis, &BigDecimal::from(0)).expect("builds");
		assert!(seg.stats.time_sorted, "fixture must be sorted");
		assert!(seg.timestamps.arithmetic_stride().is_none(), "fixture must be irregular (no closed form)");

		for stride in [1_usize, 8, 64, 1024] {
			let plain = write_segment(&seg);
			let checkpointed = write_segment_checkpointed(&seg, stride);
			// 1. The frame round-trips through the ordinary reader (additive tag).
			assert_eq!(read_segment(&checkpointed).expect("reads"), seg, "stride={stride}: checkpointed frame must round-trip");
			// 2. Every point lookup agrees with the plain frame — present and absent.
			for &probe in &ts {
				assert_eq!(read_segment_point(&checkpointed, probe).expect("reads"), read_segment_point(&plain, probe).expect("reads"), "stride={stride}: point read at {probe} must match the plain frame");
			}
			for probe in [i64::MIN, 0, 999, ts[0] - 1, ts[5] + 1, *ts.last().unwrap() + 1, i64::MAX] {
				assert_eq!(read_segment_point(&checkpointed, probe).expect("reads"), read_segment_point(&plain, probe).expect("reads"), "stride={stride}: absent probe {probe} must match");
			}
			// 3. A batch read agrees too.
			let batch: Vec<i64> = vec![ts[0], ts[299], 12_345_678, ts[599], ts[42]];
			assert_eq!(read_segment_points(&checkpointed, &batch).expect("reads"), read_segment_points(&plain, &batch).expect("reads"), "stride={stride}: batch read must match");
		}
	}

	/// Null rows across duplicate timestamps are the subtle case: the read must skip to the
	/// first *present* row of the run, exactly as the full-decode path does.
	#[test]
	fn checkpointed_frame_skips_nulls_across_duplicate_timestamps() {
		// Duplicated, irregular timestamps with nulls sprinkled onto the leading rows of
		// each duplicate run.
		let ts: Vec<i64> = vec![10, 10, 10, 25, 25, 40, 61, 61, 90];
		let vs: Vec<Option<BigDecimal>> = vec![None, None, Some(BigDecimal::from(3)), None, Some(BigDecimal::from(5)), Some(BigDecimal::from(6)), None, None, Some(BigDecimal::from(9))];
		let seg = Segment::build_nullable(&ts, &vs, TimeUnit::Millis, &BigDecimal::from(0)).expect("builds");
		for stride in [1_usize, 2, 4, 64] {
			let plain = write_segment(&seg);
			let checkpointed = write_segment_checkpointed(&seg, stride);
			for probe in [10_i64, 25, 40, 61, 90, 11, 100] {
				assert_eq!(read_segment_point(&checkpointed, probe).expect("reads"), read_segment_point(&plain, probe).expect("reads"), "stride={stride}: probe {probe} must resolve to the first present row of its run");
			}
			// An all-null run must report absent, not the next run's value.
			assert_eq!(read_segment_point(&checkpointed, 61).expect("reads"), None, "an all-null duplicate run is absent");
		}
	}

	/// The paged sibling: a checkpointed paged frame must read identically to the plain
	/// paged frame, with page pruning still applying on top.
	#[test]
	fn checkpointed_paged_frame_reads_identically_to_the_plain_paged_frame() {
		let mut t = 5_000_i64;
		let ts: Vec<i64> = (0..1_000)
			.map(|i: i64| {
				t += 1 + (i * 11) % 37;
				t
			})
			.collect();
		let vs: Vec<BigDecimal> = (0..1_000).map(|i| BigDecimal::from_str(&format!("{}.75", 200 + i % 90)).unwrap()).collect();
		let seg = PagedSegment::build(&ts, &vs, TimeUnit::Millis, &BigDecimal::from(0), 128).expect("builds");
		assert!(seg.stats.time_sorted, "fixture must be sorted");
		assert!(seg.pages.len() > 1, "fixture must actually be paged");

		for stride in [8_usize, 64, 512] {
			let plain = write_paged_segment(&seg);
			let checkpointed = write_paged_segment_checkpointed(&seg, stride);
			assert_eq!(read_paged_segment(&checkpointed).expect("reads"), seg, "stride={stride}: checkpointed paged frame must round-trip");
			for &probe in &ts {
				assert_eq!(read_paged_segment_point(&checkpointed, probe).expect("reads"), read_paged_segment_point(&plain, probe).expect("reads"), "stride={stride}: paged point read at {probe} must match");
			}
			for probe in [i64::MIN, 0, ts[0] - 1, ts[10] + 1, *ts.last().unwrap() + 1, i64::MAX] {
				assert_eq!(read_paged_segment_point(&checkpointed, probe).expect("reads"), read_paged_segment_point(&plain, probe).expect("reads"), "stride={stride}: absent paged probe {probe} must match");
			}
			let batch: Vec<i64> = vec![ts[0], ts[500], 7, ts[999], ts[137]];
			assert_eq!(read_paged_segment_points(&checkpointed, &batch).expect("reads"), read_paged_segment_points(&plain, &batch).expect("reads"), "stride={stride}: paged batch read must match");
		}
	}

	#[test]
	fn checkpointed_frame_falls_back_for_a_degenerate_column() {
		// Fewer than two rows: no index is possible, so the writer degrades to the plain
		// block and the frame is byte-identical.
		let seg = Segment::build(&[42], &col(&["7"]), TimeUnit::Millis, &BigDecimal::from(0)).expect("builds");
		assert_eq!(write_segment_checkpointed(&seg, 64), write_segment(&seg), "a single-row column has no index to write");
		assert_eq!(read_segment_point(&write_segment_checkpointed(&seg, 64), 42).expect("reads"), Some(BigDecimal::from(7)));
	}

	#[test]
	fn paged_frame_rejects_wrong_version() {
		use crate::PagedSegment;
		// A paged frame fed to the single-block reader is an unsupported version, and
		// vice versa — the two layouts carry distinct format versions.
		let paged = PagedSegment::build(&[1, 2, 3], &col(&["1.0", "2.0", "3.0"]), TimeUnit::Seconds, &BigDecimal::from(0), 2).expect("builds");
		let paged_bytes = paged.write_to();
		assert_eq!(read_segment(&paged_bytes), Err(WeftSegError::UnsupportedVersion { found: PAGED_SEGMENT_FORMAT_VERSION }));
		let single = Segment::build(&[1, 2, 3], &col(&["1.0", "2.0", "3.0"]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		let single_bytes = single.write_to();
		assert_eq!(read_paged_segment(&single_bytes), Err(WeftSegError::UnsupportedVersion { found: SEGMENT_FORMAT_VERSION }));
	}

	#[test]
	fn corrupt_paged_frame_is_detected_by_checksum() {
		use crate::PagedSegment;
		let seg = PagedSegment::build(&(0..20).collect::<Vec<_>>(), &(0..20).map(BigDecimal::from).collect::<Vec<_>>(), TimeUnit::Seconds, &BigDecimal::from(0), 8).expect("builds");
		let good = write_paged_segment(&seg);
		for i in [MAGIC.len() + 2, good.len() / 2, good.len() - 5] {
			let mut bad = good.clone();
			bad[i] ^= 0xFF;
			match read_paged_segment(&bad) {
				Err(WeftSegError::ChecksumMismatch { .. }) => {}
				other => panic!("byte {i} flip must be a checksum mismatch, got {other:?}"),
			}
		}
	}
}
