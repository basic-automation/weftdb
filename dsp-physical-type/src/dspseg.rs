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
}
