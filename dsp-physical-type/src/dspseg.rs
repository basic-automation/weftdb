//! On-disk `.dspseg` framing (roadmap **Phase 4.3**, the paged binary layout slice).
//!
//! [`crate::segment`] gave Storage v2 the *in-memory* shape of a sealed segment: a
//! typed value column, a delta-of-delta timestamp column, and the per-segment stats
//! a reader prunes against. This module is the **byte layout** that segment seals
//! to — a hand-rolled, versioned, checksummed binary frame (`.dspseg`), built up in
//! slices:
//!
//! 1. **this slice** — the low-level byte primitives every later layer is written in
//!    (fixed-width little-endian integers, LEB128 unsigned varints, zig-zag signed
//!    varints, length-prefixed byte/UTF-8 blocks) plus an IEEE **CRC-32** for
//!    integrity,
//! 2. the value-column codec ([`PhysicalValue`](crate::PhysicalValue) streams),
//! 3. the timestamp-column codec ([`DeltaOfDeltaColumn`](crate::DeltaOfDeltaColumn)),
//! 4. the framed [`Segment`](crate::Segment) — magic + header (stats) + the two
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

/// Why a `.dspseg` byte stream could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DspSegError {
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
	/// The frame did not start with the expected `.dspseg` magic bytes.
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

impl std::fmt::Display for DspSegError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::UnexpectedEof { needed, remaining } => write!(f, "unexpected end of segment stream: needed {needed} bytes, {remaining} remaining"),
			Self::VarintTooLong => f.write_str("malformed varint: did not terminate within 10 bytes"),
			Self::InvalidUtf8 => f.write_str("length-prefixed block is not valid UTF-8"),
			Self::InvalidDecimal => f.write_str("stored decimal text did not parse"),
			Self::BadMagic => f.write_str("not a .dspseg frame: bad magic bytes"),
			Self::UnsupportedVersion { found } => write!(f, "unsupported .dspseg format version {found}"),
			Self::InvalidTag { kind, value } => write!(f, "invalid {kind} tag byte {value:#04x}"),
			Self::ChecksumMismatch { stored, computed } => write!(f, "segment checksum mismatch: stored {stored:#010x}, computed {computed:#010x}"),
			Self::TrailingBytes { remaining } => write!(f, "{remaining} trailing bytes after segment frame"),
			Self::InvalidNullMask(source) => write!(f, "invalid quality column: {source}"),
		}
	}
}

impl std::error::Error for DspSegError {
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
/// Used to checksum the body of a `.dspseg` frame so a single flipped or dropped
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

/// A growable little-endian byte sink for writing a `.dspseg` frame.
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

/// A cursor over a `.dspseg` byte slice with checked little-endian reads.
///
/// Every read advances the cursor and returns [`DspSegError::UnexpectedEof`] rather
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
	/// [`DspSegError::UnexpectedEof`] if fewer than `n` bytes remain.
	pub fn take(&mut self, n: usize) -> Result<&'a [u8], DspSegError> {
		if self.remaining() < n {
			return Err(DspSegError::UnexpectedEof { needed: n, remaining: self.remaining() });
		}
		let out = &self.buf[self.pos..self.pos + n];
		self.pos += n;
		Ok(out)
	}

	/// Read one raw byte.
	///
	/// # Errors
	///
	/// [`DspSegError::UnexpectedEof`] at end of stream.
	pub fn read_u8(&mut self) -> Result<u8, DspSegError> {
		Ok(self.take(1)?[0])
	}

	/// Read a little-endian `u16`.
	///
	/// # Errors
	///
	/// [`DspSegError::UnexpectedEof`] if fewer than 2 bytes remain.
	pub fn read_u16_le(&mut self) -> Result<u16, DspSegError> {
		let b = self.take(2)?;
		Ok(u16::from_le_bytes([b[0], b[1]]))
	}

	/// Read a little-endian `u32`.
	///
	/// # Errors
	///
	/// [`DspSegError::UnexpectedEof`] if fewer than 4 bytes remain.
	pub fn read_u32_le(&mut self) -> Result<u32, DspSegError> {
		let b = self.take(4)?;
		Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
	}

	/// Read a little-endian fixed-width `i64`.
	///
	/// # Errors
	///
	/// [`DspSegError::UnexpectedEof`] if fewer than 8 bytes remain.
	pub fn read_i64_le(&mut self) -> Result<i64, DspSegError> {
		let b = self.take(8)?;
		Ok(i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
	}

	/// Read a little-endian fixed-width `i128`.
	///
	/// # Errors
	///
	/// [`DspSegError::UnexpectedEof`] if fewer than 16 bytes remain.
	pub fn read_i128_le(&mut self) -> Result<i128, DspSegError> {
		let b = self.take(16)?;
		Ok(i128::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]]))
	}

	/// Read a little-endian `f64` (IEEE-754 byte pattern).
	///
	/// # Errors
	///
	/// [`DspSegError::UnexpectedEof`] if fewer than 8 bytes remain.
	pub fn read_f64_le(&mut self) -> Result<f64, DspSegError> {
		let b = self.take(8)?;
		Ok(f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
	}

	/// Read a little-endian `f32` (IEEE-754 byte pattern).
	///
	/// # Errors
	///
	/// [`DspSegError::UnexpectedEof`] if fewer than 4 bytes remain.
	pub fn read_f32_le(&mut self) -> Result<f32, DspSegError> {
		let b = self.take(4)?;
		Ok(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
	}

	/// Read an LEB128 unsigned varint.
	///
	/// # Errors
	///
	/// [`DspSegError::UnexpectedEof`] if the stream ends mid-varint, or
	/// [`DspSegError::VarintTooLong`] if it does not terminate within 10 bytes.
	pub fn read_uvarint(&mut self) -> Result<u64, DspSegError> {
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
				return Err(DspSegError::VarintTooLong);
			}
		}
	}

	/// Read a zig-zag + LEB128 signed varint (the inverse of
	/// [`ByteWriter::put_svarint`]).
	///
	/// # Errors
	///
	/// As [`read_uvarint`](Self::read_uvarint).
	pub fn read_svarint(&mut self) -> Result<i64, DspSegError> {
		let zz = self.read_uvarint()?;
		// Inverse zig-zag: (zz >> 1) ^ -(zz & 1).
		#[allow(clippy::cast_possible_wrap)]
		Ok(((zz >> 1) as i64) ^ -((zz & 1) as i64))
	}

	/// Read a length-prefixed byte block (varint length, then the bytes).
	///
	/// # Errors
	///
	/// [`DspSegError::UnexpectedEof`] if the stream is shorter than the declared
	/// length.
	pub fn read_bytes(&mut self) -> Result<&'a [u8], DspSegError> {
		let len = usize::try_from(self.read_uvarint()?).map_err(|_| DspSegError::VarintTooLong)?;
		self.take(len)
	}

	/// Read a length-prefixed UTF-8 string.
	///
	/// # Errors
	///
	/// [`DspSegError::UnexpectedEof`] if short, or [`DspSegError::InvalidUtf8`] if the
	/// bytes are not valid UTF-8.
	pub fn read_str(&mut self) -> Result<&'a str, DspSegError> {
		let bytes = self.read_bytes()?;
		std::str::from_utf8(bytes).map_err(|_| DspSegError::InvalidUtf8)
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
	timestamp::{DeltaOfDeltaColumn, TimeUnit}, ColumnEncoding, PhysicalType, PhysicalValue
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
fn read_physical_value(r: &mut ByteReader, physical_type: PhysicalType) -> Result<PhysicalValue, DspSegError> {
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
fn read_decimal(r: &mut ByteReader) -> Result<BigDecimal, DspSegError> {
	BigDecimal::from_str(r.read_str()?).map_err(|_| DspSegError::InvalidDecimal)
}

/// Value-column codec selector (self-describing byte in a v4+ value block).
///
/// [`VAL_CODEC_VARINT`] is the general per-value payload (the codec every physical
/// type can use); [`VAL_CODEC_BITPACK`] is the fixed-width bit-packed mantissa
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

/// Write a [`ColumnEncoding`] as a `.dspseg` value-column block.
///
/// After the header (physical-type tag, optional `ScaledI*` scale, count, lossy
/// count, `max_abs_error`) the block carries a self-describing codec byte, then the
/// coded payload. Three codecs are realized: the general per-value payload
/// ([`VAL_CODEC_VARINT`] — IEEE byte patterns for the floats, a zig-zag varint
/// mantissa for `ScaledI64`, full-width `i128` for the wide integers, length-prefixed
/// UTF-8 for `BigDecimalText`), fixed-width **bit-packing** of a `ScaledI64` column's
/// mantissas ([`VAL_CODEC_BITPACK`], a regular/small-jitter scaled series), and
/// **per-block adaptive bit-packing** of a `ScaledI64` column's mantissas
/// ([`VAL_CODEC_BLOCKED`], a mixed-magnitude scaled series where a global width
/// over-pays). The codec is chosen through [`ColumnEncoding::best_value_codec`], the
/// single source of truth, so each `ScaledI64` column realizes the smallest of the three
/// on disk while every other column keeps the per-value payload. All three codecs are
/// exact and lossless.
pub fn write_value_column(w: &mut ByteWriter, col: &ColumnEncoding) {
	w.put_u8(physical_type_tag(col.physical_type));
	match col.physical_type {
		PhysicalType::ScaledI64 { scale } | PhysicalType::ScaledI128 { scale } => w.put_u8(scale),
		_ => {}
	}
	w.put_uvarint(col.values.len() as u64);
	w.put_uvarint(col.lossy_count as u64);
	w.put_str(&col.max_abs_error.to_plain_string());
	match col.best_value_codec() {
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

/// Read a [`ColumnEncoding`] from a `.dspseg` value-column block — the exact
/// inverse of [`write_value_column`].
///
/// # Errors
///
/// [`DspSegError::InvalidTag`] for an unrecognised physical-type tag, an
/// unrecognised value-codec byte, or a bit-pack codec on a non-`ScaledI64` column;
/// [`DspSegError::InvalidDecimal`] if the stored `max_abs_error` does not parse; or
/// [`DspSegError::UnexpectedEof`] / [`DspSegError::VarintTooLong`] on a short or
/// malformed stream.
pub fn read_value_column(r: &mut ByteReader) -> Result<ColumnEncoding, DspSegError> {
	let tag = r.read_u8()?;
	let physical_type = match tag {
		TAG_F64 => PhysicalType::F64,
		TAG_F32 => PhysicalType::F32,
		TAG_SCALED_I64 => PhysicalType::ScaledI64 { scale: r.read_u8()? },
		TAG_SCALED_I128 => PhysicalType::ScaledI128 { scale: r.read_u8()? },
		TAG_DECIMAL128 => PhysicalType::Decimal128,
		TAG_BIGDECIMAL_TEXT => PhysicalType::BigDecimalText,
		other => return Err(DspSegError::InvalidTag { kind: "physical_type", value: other }),
	};
	let count = usize::try_from(r.read_uvarint()?).map_err(|_| DspSegError::VarintTooLong)?;
	let lossy_count = usize::try_from(r.read_uvarint()?).map_err(|_| DspSegError::VarintTooLong)?;
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
				return Err(DspSegError::InvalidTag { kind: "value_codec_bitpack_type", value: tag });
			};
			let width = u32::from(r.read_u8()?);
			let data_len = (count * width as usize).div_ceil(8);
			let bytes = r.take(data_len)?;
			crate::timestamp::bitpack_decode(width, bytes, count).into_iter().map(|mantissa| PhysicalValue::ScaledI64 { mantissa, scale }).collect()
		}
		VAL_CODEC_BLOCKED => {
			let PhysicalType::ScaledI64 { scale } = physical_type else {
				return Err(DspSegError::InvalidTag { kind: "value_codec_blocked_type", value: tag });
			};
			let block = usize::try_from(r.read_uvarint()?).map_err(|_| DspSegError::VarintTooLong)?;
			let bytes = r.read_bytes()?;
			crate::timestamp::blocked_bitpack_decode(bytes, block, count).into_iter().map(|mantissa| PhysicalValue::ScaledI64 { mantissa, scale }).collect()
		}
		other => return Err(DspSegError::InvalidTag { kind: "value_codec", value: other }),
	};
	Ok(ColumnEncoding { physical_type, values, lossy_count, max_abs_error })
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
const fn time_unit_from_tag(tag: u8) -> Result<TimeUnit, DspSegError> {
	match tag {
		TIME_UNIT_SECONDS => Ok(TimeUnit::Seconds),
		TIME_UNIT_MILLIS => Ok(TimeUnit::Millis),
		TIME_UNIT_MICROS => Ok(TimeUnit::Micros),
		TIME_UNIT_NANOS => Ok(TimeUnit::Nanos),
		other => Err(DspSegError::InvalidTag { kind: "time_unit", value: other }),
	}
}

/// Write a [`DeltaOfDeltaColumn`] as a `.dspseg` timestamp-column block.
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

/// Read a [`DeltaOfDeltaColumn`] from a `.dspseg` timestamp-column block — the exact
/// inverse of [`write_timestamp_column`].
///
/// # Errors
///
/// [`DspSegError::InvalidTag`] for an unrecognised time-unit or codec tag, or
/// [`DspSegError::UnexpectedEof`] / [`DspSegError::VarintTooLong`] on a short or
/// malformed stream.
pub fn read_timestamp_column(r: &mut ByteReader) -> Result<DeltaOfDeltaColumn, DspSegError> {
	let unit = time_unit_from_tag(r.read_u8()?)?;
	let first = r.read_i64_le()?;
	let first_delta = match r.read_u8()? {
		0 => None,
		_ => Some(r.read_svarint()?),
	};
	let count = usize::try_from(r.read_uvarint()?).map_err(|_| DspSegError::VarintTooLong)?;
	let dods = match r.read_u8()? {
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
			let block = usize::try_from(r.read_uvarint()?).map_err(|_| DspSegError::VarintTooLong)?;
			let bytes = r.read_bytes()?;
			crate::timestamp::blocked_bitpack_decode(bytes, block, count)
		}
		TS_CODEC_RLE => {
			let run_count = usize::try_from(r.read_uvarint()?).map_err(|_| DspSegError::VarintTooLong)?;
			let mut runs = Vec::with_capacity(run_count);
			for _ in 0..run_count {
				let value = r.read_svarint()?;
				let run_len = usize::try_from(r.read_uvarint()?).map_err(|_| DspSegError::VarintTooLong)?;
				runs.push((value, run_len));
			}
			crate::timestamp::rle_decode(&runs)
		}
		other => return Err(DspSegError::InvalidTag { kind: "timestamp_codec", value: other }),
	};
	Ok(DeltaOfDeltaColumn { first, first_delta, dods, unit })
}

// ---------------------------------------------------------------------------
// Segment frame (Phase 4.3, slice 4)
//
// The full `.dspseg` frame ties the pieces together: a magic prefix and format
// version, the per-segment header (the stats a reader prunes against — row/null
// counts, time-sorted flag, min/max ts and value), the value-column block, the
// timestamp-column block, and a trailing CRC-32 over everything before it. A
// single flipped byte anywhere in the body changes the checksum, so corruption is
// caught on read rather than silently misinterpreted.
// ---------------------------------------------------------------------------

use crate::{
	nulls::NullMask, page::{Page, PagedSegment, PAGED_SEGMENT_FORMAT_VERSION}, segment::SegmentStats, Segment, SEGMENT_FORMAT_VERSION
};

/// The magic prefix every `.dspseg` frame starts with.
const MAGIC: &[u8; 7] = b"DSPSEG\0";

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
fn read_null_column(r: &mut ByteReader, row_count: usize, null_count: usize) -> Result<NullMask, DspSegError> {
	let bits = match r.read_u8()? {
		0 => None,
		1 => Some(r.read_bytes()?.to_vec()),
		value => return Err(DspSegError::InvalidTag { kind: "null_column", value }),
	};
	NullMask::from_raw(row_count, null_count, bits).map_err(DspSegError::InvalidNullMask)
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
fn read_opt_i64(r: &mut ByteReader) -> Result<Option<i64>, DspSegError> {
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
fn read_opt_decimal(r: &mut ByteReader) -> Result<Option<BigDecimal>, DspSegError> {
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
fn read_segment_stats(r: &mut ByteReader) -> Result<SegmentStats, DspSegError> {
	let row_count = usize::try_from(r.read_uvarint()?).map_err(|_| DspSegError::VarintTooLong)?;
	let null_count = usize::try_from(r.read_uvarint()?).map_err(|_| DspSegError::VarintTooLong)?;
	let time_sorted = r.read_u8()? != 0;
	let min_ts = read_opt_i64(r)?;
	let max_ts = read_opt_i64(r)?;
	let min_value = read_opt_decimal(r)?;
	let max_value = read_opt_decimal(r)?;
	Ok(SegmentStats { row_count, null_count, time_sorted, min_ts, max_ts, min_value, max_value })
}

/// Encode a [`Segment`] into a complete, self-describing, checksummed `.dspseg`
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

/// Decode a [`Segment`] from a `.dspseg` byte frame — the exact inverse of
/// [`write_segment`].
///
/// The trailing CRC-32 is verified against the body **before** any field is
/// parsed, so a corrupt or truncated frame fails fast with
/// [`DspSegError::ChecksumMismatch`] rather than being misread.
///
/// # Errors
///
/// - [`DspSegError::UnexpectedEof`] if the frame is too short to hold a checksum.
/// - [`DspSegError::ChecksumMismatch`] if the stored CRC does not match the body.
/// - [`DspSegError::BadMagic`] / [`DspSegError::UnsupportedVersion`] for a frame
///   this reader does not recognise.
/// - [`DspSegError::TrailingBytes`] if bytes remain after a complete frame.
/// - the column/stat read errors ([`DspSegError::InvalidTag`],
///   [`DspSegError::InvalidDecimal`], …) on a malformed body.
pub fn read_segment(bytes: &[u8]) -> Result<Segment, DspSegError> {
	if bytes.len() < 4 {
		return Err(DspSegError::UnexpectedEof { needed: 4, remaining: bytes.len() });
	}
	let (body, crc_bytes) = bytes.split_at(bytes.len() - 4);
	let stored = u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
	let computed = crc32(body);
	if stored != computed {
		return Err(DspSegError::ChecksumMismatch { stored, computed });
	}
	let mut r = ByteReader::new(body);
	if r.take(MAGIC.len())? != MAGIC {
		return Err(DspSegError::BadMagic);
	}
	let version = r.read_u16_le()?;
	if version != SEGMENT_FORMAT_VERSION {
		return Err(DspSegError::UnsupportedVersion { found: version });
	}
	let stats = read_segment_stats(&mut r)?;
	let values = read_value_column(&mut r)?;
	let timestamps = read_timestamp_column(&mut r)?;
	let nulls = read_null_column(&mut r, stats.row_count, stats.null_count)?;
	if !r.is_empty() {
		return Err(DspSegError::TrailingBytes { remaining: r.remaining() });
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
/// `.dspseg` byte frame (format version 3).
///
/// The inverse is [`read_paged_segment`]; the round trip is exact. The frame's
/// per-page index lets a reader prune and seek to individual pages without
/// decoding the whole segment (see the module comment above).
#[must_use]
pub fn write_paged_segment(seg: &PagedSegment) -> Vec<u8> {
	// Encode each page's column block first so the index can carry its byte length.
	let page_blocks: Vec<Vec<u8>> = seg
		.pages
		.iter()
		.map(|page| {
			let mut pw = ByteWriter::with_capacity(page.total_bytes() + 16);
			write_value_column(&mut pw, &page.values);
			write_timestamp_column(&mut pw, &page.timestamps);
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

/// Decode a [`PagedSegment`] from a `.dspseg` byte frame — the exact inverse of
/// [`write_paged_segment`].
///
/// As with [`read_segment`], the trailing CRC-32 is verified against the body
/// **before** any field is parsed, so a corrupt or truncated frame fails fast with
/// [`DspSegError::ChecksumMismatch`]. Each page's column block is bounded to its
/// indexed byte length, so a page that does not exactly fill its block is rejected
/// ([`DspSegError::TrailingBytes`]).
///
/// # Errors
///
/// The same family as [`read_segment`]: [`DspSegError::UnexpectedEof`],
/// [`DspSegError::ChecksumMismatch`], [`DspSegError::BadMagic`],
/// [`DspSegError::UnsupportedVersion`] (when the version is not the paged version),
/// [`DspSegError::TrailingBytes`], and the per-column read errors.
pub fn read_paged_segment(bytes: &[u8]) -> Result<PagedSegment, DspSegError> {
	if bytes.len() < 4 {
		return Err(DspSegError::UnexpectedEof { needed: 4, remaining: bytes.len() });
	}
	let (body, crc_bytes) = bytes.split_at(bytes.len() - 4);
	let stored = u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
	let computed = crc32(body);
	if stored != computed {
		return Err(DspSegError::ChecksumMismatch { stored, computed });
	}
	let mut r = ByteReader::new(body);
	if r.take(MAGIC.len())? != MAGIC {
		return Err(DspSegError::BadMagic);
	}
	let version = r.read_u16_le()?;
	if version != PAGED_SEGMENT_FORMAT_VERSION {
		return Err(DspSegError::UnsupportedVersion { found: version });
	}
	let rows_per_page = usize::try_from(r.read_uvarint()?).map_err(|_| DspSegError::VarintTooLong)?;
	let stats = read_segment_stats(&mut r)?;
	let page_count = usize::try_from(r.read_uvarint()?).map_err(|_| DspSegError::VarintTooLong)?;
	// Read the index: each page's stats and the length of its column block.
	let mut index: Vec<(SegmentStats, usize)> = Vec::with_capacity(page_count);
	for _ in 0..page_count {
		let page_stats = read_segment_stats(&mut r)?;
		let block_len = usize::try_from(r.read_uvarint()?).map_err(|_| DspSegError::VarintTooLong)?;
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
			return Err(DspSegError::TrailingBytes { remaining: pr.remaining() });
		}
		pages.push(Page { values, timestamps, nulls, stats: page_stats });
	}
	if !r.is_empty() {
		return Err(DspSegError::TrailingBytes { remaining: r.remaining() });
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
		assert_eq!(r.read_u32_le(), Err(DspSegError::UnexpectedEof { needed: 4, remaining: 2 }));
		// A length prefix promising more than is present.
		let mut w = ByteWriter::new();
		w.put_uvarint(10);
		w.put_raw(b"abc");
		let framed = w.into_vec();
		let mut r2 = ByteReader::new(&framed);
		assert_eq!(r2.read_str(), Err(DspSegError::UnexpectedEof { needed: 10, remaining: 3 }));
	}

	#[test]
	fn non_terminating_varint_is_rejected() {
		// Eleven continuation bytes never terminate within the 64-bit budget.
		let bytes = [0x80_u8; 11];
		let mut r = ByteReader::new(&bytes);
		assert_eq!(r.read_uvarint(), Err(DspSegError::VarintTooLong));
	}

	#[test]
	fn invalid_utf8_block_is_rejected() {
		let mut w = ByteWriter::new();
		w.put_bytes(&[0xFF, 0xFE]); // not valid UTF-8
		let bytes = w.into_vec();
		let mut r = ByteReader::new(&bytes);
		assert_eq!(r.read_str(), Err(DspSegError::InvalidUtf8));
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
		// 0.00..0.63 scaled by 100 → mantissas 0..=63 (≤ 7 bits): fixed-width
		// bit-packing beats the one-byte-per-value varint floor, so the block selects
		// the bit-pack codec on disk.
		let lits: Vec<String> = (0..64).map(|i| format!("0.{i:02}")).collect();
		let refs: Vec<&str> = lits.iter().map(String::as_str).collect();
		let enc = encode_column(PhysicalType::ScaledI64 { scale: 2 }, &col(&refs)).unwrap();
		assert_eq!(enc.best_value_codec(), "scaled_bitpack");
		assert!(enc.bitpack_value_bytes().unwrap() < enc.serialized_bytes());
		// It round-trips exactly through the realized `.dspseg` value block…
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
		assert_eq!(read_value_column(&mut r), Err(DspSegError::InvalidTag { kind: "value_codec_bitpack_type", value: TAG_F64 }));
	}

	/// A scaled-int column whose mantissas mix a quiet region with a contiguous burst of
	/// large values — per-block adaptive bit-packing is the strict winner (global width
	/// over-pays), so the block selects the blocked codec on disk.
	fn mixed_magnitude_scaled_column() -> ColumnEncoding {
		use crate::encode_column;
		let lits: Vec<String> = (0..192).map(|i| if (64..128).contains(&i) { format!("{}", 1_000_000_000_i64 + i) } else { format!("{}", i % 5) }).collect();
		let refs: Vec<&str> = lits.iter().map(String::as_str).collect();
		encode_column(PhysicalType::ScaledI64 { scale: 0 }, &col(&refs)).unwrap()
	}

	#[test]
	fn value_column_realizes_blocked_on_a_mixed_magnitude_scaled_stream() {
		let enc = mixed_magnitude_scaled_column();
		assert_eq!(enc.best_value_codec(), "scaled_blocked", "mixed-magnitude scaled stream must pick the blocked codec");
		assert!(enc.blocked_value_bytes().unwrap() < enc.bitpack_value_bytes().unwrap(), "blocked must beat global bit-pack");
		// Round-trips exactly through the realized `.dspseg` value block…
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
		assert_eq!(read_value_column(&mut r), Err(DspSegError::InvalidTag { kind: "value_codec_blocked_type", value: TAG_F64 }));
	}

	#[test]
	fn segment_frame_round_trips_a_blocked_scaled_column() {
		// A mixed-magnitude scaled-int series that seals to the blocked value codec must
		// round-trip through the full framed segment (header, CRC, v4 layout). Two-decimal
		// values force the ScaledI64 encoding (F64 cannot represent 0.01 exactly), and the
		// burst (a full 64-wide block near 1e7) gives the mixed magnitude that lets per-block
		// bit-packing beat both the varint and the global bit-pack.
		let ts: Vec<i64> = (0..192).map(|i| 1_000 + i * 5).collect();
		let vs: Vec<BigDecimal> = (0..192).map(|i| BigDecimal::from_str(&if (64..128).contains(&i) { format!("{}.01", 10_000_000 + i) } else { format!("0.0{}", i % 5) }).unwrap()).collect();
		let seg = Segment::build(&ts, &vs, TimeUnit::Millis, &BigDecimal::from(0)).expect("builds");
		assert_eq!(seg.values.best_value_codec(), "scaled_blocked", "mixed-magnitude scaled stream must pick the blocked codec");
		let bytes = write_segment(&seg);
		let back = read_segment(&bytes).expect("reads");
		assert_eq!(back, seg, "blocked segment frame must round-trip exactly");
		assert_eq!(back.version, SEGMENT_FORMAT_VERSION);
	}

	#[test]
	fn segment_frame_round_trips_a_bitpacked_scaled_column() {
		// A regular scaled-int series that seals to the bit-pack value codec must
		// round-trip through the full framed segment (header, CRC, v4 layout).
		let ts: Vec<i64> = (0..64).map(|i| 1_000 + i * 5).collect();
		let vs: Vec<BigDecimal> = (0..64).map(|i| BigDecimal::from_str(&format!("0.{i:02}")).unwrap()).collect();
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
		let cases = [
			encode_column(PhysicalType::F64, &col(&["0.5", "2.25", "-128.0"])).unwrap(),
			encode_column(PhysicalType::F32, &col(&["0.5", "-0.25", "16.0"])).unwrap(),
			encode_column(PhysicalType::ScaledI64 { scale: 2 }, &col(&["1.25", "-3.75", "0.00", "5000.00"])).unwrap(),
			encode_column(PhysicalType::ScaledI128 { scale: 4 }, &col(&["1234567890.1234", "-9.0001"])).unwrap(),
			encode_column(PhysicalType::Decimal128, &col(&["123456789012345678901234.567890", "-1.5", "0"])).unwrap(),
			encode_column(PhysicalType::BigDecimalText, &col(&["1.5", "12345.6789", "-0.000001"])).unwrap(),
		];
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
		// currently over-reports (underselling DSP's bytes/point).
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
		assert_eq!(read_value_column(&mut r), Err(DspSegError::InvalidTag { kind: "physical_type", value: 99 }));
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
		assert_eq!(read_timestamp_column(&mut r), Err(DspSegError::InvalidTag { kind: "time_unit", value: 7 }));
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
		assert_eq!(read_timestamp_column(&mut r), Err(DspSegError::InvalidTag { kind: "timestamp_codec", value: 99 }));
	}

	/// Build a segment, seal it to a `.dspseg` frame, read it back, and assert exact
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
		assert_eq!(read_segment(&bad), Err(DspSegError::InvalidNullMask(crate::nulls::NullMaskError::NullCountMismatch { declared: 1, actual: 2 })));
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
				Err(DspSegError::ChecksumMismatch { .. }) => {}
				other => panic!("byte {i} flip must be a checksum mismatch, got {other:?}"),
			}
		}
		// Corrupting the trailing checksum itself is also caught.
		let mut bad_crc = good;
		let last = bad_crc.len() - 1;
		bad_crc[last] ^= 0x01;
		assert!(matches!(read_segment(&bad_crc), Err(DspSegError::ChecksumMismatch { .. })));
	}

	#[test]
	fn bad_magic_and_short_frames_are_rejected() {
		// Too short to even hold a checksum.
		assert_eq!(read_segment(&[0, 1, 2]), Err(DspSegError::UnexpectedEof { needed: 4, remaining: 3 }));
		// A correctly-checksummed frame whose body is 7 wrong-magic bytes: the CRC
		// gate passes, then the magic check rejects it.
		let mut bad = b"NOTSEG!".to_vec(); // 7 bytes, wrong magic
		bad.extend_from_slice(&crc32(&bad).to_le_bytes());
		assert_eq!(read_segment(&bad), Err(DspSegError::BadMagic));
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
		assert_eq!(read_segment(&bytes), Err(DspSegError::UnsupportedVersion { found: SEGMENT_FORMAT_VERSION + 1 }));
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

	#[test]
	fn paged_frame_rejects_wrong_version() {
		use crate::PagedSegment;
		// A paged frame fed to the single-block reader is an unsupported version, and
		// vice versa — the two layouts carry distinct format versions.
		let paged = PagedSegment::build(&[1, 2, 3], &col(&["1.0", "2.0", "3.0"]), TimeUnit::Seconds, &BigDecimal::from(0), 2).expect("builds");
		let paged_bytes = paged.write_to();
		assert_eq!(read_segment(&paged_bytes), Err(DspSegError::UnsupportedVersion { found: PAGED_SEGMENT_FORMAT_VERSION }));
		let single = Segment::build(&[1, 2, 3], &col(&["1.0", "2.0", "3.0"]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		let single_bytes = single.write_to();
		assert_eq!(read_paged_segment(&single_bytes), Err(DspSegError::UnsupportedVersion { found: SEGMENT_FORMAT_VERSION }));
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
				Err(DspSegError::ChecksumMismatch { .. }) => {}
				other => panic!("byte {i} flip must be a checksum mismatch, got {other:?}"),
			}
		}
	}
}
