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
		}
	}
}

impl std::error::Error for DspSegError {}

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

/// Write a [`ColumnEncoding`] as a `.dspseg` value-column block.
///
/// The mantissa/payload encoding is chosen per physical type: IEEE byte patterns
/// for the floats, a zig-zag varint mantissa for `ScaledI64` (small mantissas cost
/// one byte), full-width `i128` for the wide integer encodings, and length-prefixed
/// UTF-8 for `BigDecimalText`. The shared `scale` of a `ScaledI*` column rides in
/// the header tag, not per value.
pub fn write_value_column(w: &mut ByteWriter, col: &ColumnEncoding) {
	w.put_u8(physical_type_tag(col.physical_type));
	match col.physical_type {
		PhysicalType::ScaledI64 { scale } | PhysicalType::ScaledI128 { scale } => w.put_u8(scale),
		_ => {}
	}
	w.put_uvarint(col.values.len() as u64);
	w.put_uvarint(col.lossy_count as u64);
	w.put_str(&col.max_abs_error.to_plain_string());
	for value in &col.values {
		write_physical_value(w, value);
	}
}

/// Read a [`ColumnEncoding`] from a `.dspseg` value-column block — the exact
/// inverse of [`write_value_column`].
///
/// # Errors
///
/// [`DspSegError::InvalidTag`] for an unrecognised physical-type tag,
/// [`DspSegError::InvalidDecimal`] if the stored `max_abs_error` does not parse,
/// or [`DspSegError::UnexpectedEof`] / [`DspSegError::VarintTooLong`] on a short or
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
	let mut values = Vec::with_capacity(count);
	for _ in 0..count {
		values.push(read_physical_value(r, physical_type)?);
	}
	Ok(ColumnEncoding { physical_type, values, lossy_count, max_abs_error })
}

// ---------------------------------------------------------------------------
// Timestamp-column codec (Phase 4.3, slice 3)
//
// The on-disk byte block for a `DeltaOfDeltaColumn`: a one-byte time-unit tag,
// the full-width `i64` anchor, an optional first delta (a presence flag then a
// signed varint), and the second-difference stream as a varint count + zig-zag
// varints. Plain delta-of-delta is written here; run-length coding of the second
// differences (the cheaper codec for a regular series, already chosen by
// `best_encoding_name` for the bytes/point estimate) is a later compression slice
// — this layer is exact and lossless either way.
// ---------------------------------------------------------------------------

const TIME_UNIT_SECONDS: u8 = 0;
const TIME_UNIT_MILLIS: u8 = 1;
const TIME_UNIT_MICROS: u8 = 2;
const TIME_UNIT_NANOS: u8 = 3;

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
	for &dod in &col.dods {
		w.put_svarint(dod);
	}
}

/// Read a [`DeltaOfDeltaColumn`] from a `.dspseg` timestamp-column block — the exact
/// inverse of [`write_timestamp_column`].
///
/// # Errors
///
/// [`DspSegError::InvalidTag`] for an unrecognised time-unit tag, or
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
	let mut dods = Vec::with_capacity(count);
	for _ in 0..count {
		dods.push(r.read_svarint()?);
	}
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

use crate::{segment::SegmentStats, Segment, SEGMENT_FORMAT_VERSION};

/// The magic prefix every `.dspseg` frame starts with.
const MAGIC: &[u8; 7] = b"DSPSEG\0";

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
	w.put_uvarint(seg.stats.row_count as u64);
	w.put_uvarint(seg.stats.null_count as u64);
	w.put_u8(u8::from(seg.stats.time_sorted));
	put_opt_i64(&mut w, seg.stats.min_ts);
	put_opt_i64(&mut w, seg.stats.max_ts);
	put_opt_decimal(&mut w, seg.stats.min_value.as_ref());
	put_opt_decimal(&mut w, seg.stats.max_value.as_ref());
	// Column blocks.
	write_value_column(&mut w, &seg.values);
	write_timestamp_column(&mut w, &seg.timestamps);
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
	let row_count = usize::try_from(r.read_uvarint()?).map_err(|_| DspSegError::VarintTooLong)?;
	let null_count = usize::try_from(r.read_uvarint()?).map_err(|_| DspSegError::VarintTooLong)?;
	let time_sorted = r.read_u8()? != 0;
	let min_ts = read_opt_i64(&mut r)?;
	let max_ts = read_opt_i64(&mut r)?;
	let min_value = read_opt_decimal(&mut r)?;
	let max_value = read_opt_decimal(&mut r)?;
	let values = read_value_column(&mut r)?;
	let timestamps = read_timestamp_column(&mut r)?;
	if !r.is_empty() {
		return Err(DspSegError::TrailingBytes { remaining: r.remaining() });
	}
	let stats = SegmentStats { row_count, null_count, time_sorted, min_ts, max_ts, min_value, max_value };
	Ok(Segment { version, values, timestamps, stats })
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
}
