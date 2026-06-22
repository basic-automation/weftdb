//! Per-row presence / null mask (roadmap **Phase 4.3** quality column, first slice).
//!
//! Phases 4.1–4.3 built a segment from a *dense* `(timestamp, value)` column —
//! every row carried a value, so [`SegmentStats::null_count`](crate::SegmentStats)
//! was always zero. Real measurement streams have gaps: a sensor drops a sample,
//! an ingest batch is sparse, a row is recorded with a timestamp but no reading.
//! Phase 4.3 names a dedicated **quality/null column** beside the value column for
//! exactly this, and this module is its in-memory shape.
//!
//! ## What a mask records
//!
//! A nullable aspect stores its **timestamps densely** — every row has a time,
//! null-valued or not — but only the **present** (non-null) rows contribute a
//! value to the value column. The mask is the bridge between the two: for each
//! row it records whether the next value in the (shorter) value column belongs to
//! it, so a reader can interleave present values with `None`s and reconstruct the
//! full logical column.
//!
//! ## Dense rows cost nothing
//!
//! The overwhelmingly common case is a fully dense column (no nulls). That case
//! stores **zero** mask bytes — [`bits`](NullMask) is `None`, every row is
//! present by construction — so a non-nullable segment's bytes/point is exactly
//! what it was before this column existed. Only a column that actually contains a
//! null pays for the bitmap.
//!
//! The representation is a presence **bitmap**: one bit per row, set ⇒ present.
//! An RLE null-run encoding (cheaper for long contiguous gaps) is a later
//! compression slice — the bitmap is the lossless, random-access baseline the
//! same way plain delta-of-delta is the timestamp baseline.

use serde::{Deserialize, Serialize};

/// A per-row presence mask: which of a segment's rows carry a value vs a null.
///
/// Build one from per-row presence with [`from_presence`](NullMask::from_presence),
/// or a fully dense one with [`all_present`](NullMask::all_present). Query it with
/// [`is_present`](NullMask::is_present); a dense mask answers `true` for every row
/// in range without storing any bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NullMask {
	/// Total number of rows the mask covers (the logical column height).
	row_count: usize,
	/// Number of null/absent rows (rows with a timestamp but no value).
	null_count: usize,
	/// Presence bitmap, one bit per row, **LSB-first** within each byte (row `i`
	/// is bit `i % 8` of byte `i / 8`); a set bit means the row is present. `None`
	/// when the column is fully dense — every row present, no bytes stored.
	bits: Option<Vec<u8>>,
}

impl NullMask {
	/// A fully dense mask over `row_count` rows: every row present, no nulls, no
	/// stored bytes.
	#[must_use]
	pub const fn all_present(row_count: usize) -> Self {
		Self { row_count, null_count: 0, bits: None }
	}

	/// Build a mask from a per-row presence slice (`true` = the row has a value).
	///
	/// A fully-present input produces a dense mask (no stored bytes); any `false`
	/// entry triggers a packed bitmap of `ceil(row_count / 8)` bytes.
	#[must_use]
	pub fn from_presence(present: &[bool]) -> Self {
		let row_count = present.len();
		let null_count = present.iter().filter(|p| !**p).count();
		if null_count == 0 {
			return Self::all_present(row_count);
		}
		let mut bits = vec![0_u8; row_count.div_ceil(8)];
		for (i, &p) in present.iter().enumerate() {
			if p {
				bits[i / 8] |= 1 << (i % 8);
			}
		}
		Self { row_count, null_count, bits: Some(bits) }
	}

	/// Reconstruct a mask from its raw parts — used by the `.dspseg` frame reader
	/// after it has read back the row count, null count, and (for a sparse column)
	/// the packed bitmap bytes. A dense column passes `bits = None`.
	///
	/// # Errors
	///
	/// Returns [`NullMaskError`] if the parts are inconsistent: a `bits` buffer
	/// whose length is not `ceil(row_count / 8)`, or a `null_count` that does not
	/// match the number of clear bits (or is non-zero for a dense column). This is
	/// the integrity check a reader applies to an untrusted frame.
	pub fn from_raw(row_count: usize, null_count: usize, bits: Option<Vec<u8>>) -> Result<Self, NullMaskError> {
		match bits {
			None => {
				if null_count != 0 {
					return Err(NullMaskError::NullCountMismatch { declared: null_count, actual: 0 });
				}
				Ok(Self::all_present(row_count))
			}
			Some(bytes) => {
				let expected = row_count.div_ceil(8);
				if bytes.len() != expected {
					return Err(NullMaskError::BitmapLength { expected, found: bytes.len() });
				}
				let clear = (0..row_count).filter(|&i| (bytes[i / 8] >> (i % 8)) & 1 == 0).count();
				if clear != null_count {
					return Err(NullMaskError::NullCountMismatch { declared: null_count, actual: clear });
				}
				Ok(Self { row_count, null_count, bits: Some(bytes) })
			}
		}
	}

	/// Number of rows the mask covers.
	#[must_use]
	pub const fn row_count(&self) -> usize {
		self.row_count
	}

	/// Number of null/absent rows.
	#[must_use]
	pub const fn null_count(&self) -> usize {
		self.null_count
	}

	/// Number of present (non-null) rows — the height of the value column the mask
	/// pairs with.
	#[must_use]
	pub const fn present_count(&self) -> usize {
		self.row_count - self.null_count
	}

	/// `true` iff the column is fully dense — no nulls, no stored bitmap.
	#[must_use]
	pub const fn is_dense(&self) -> bool {
		self.bits.is_none()
	}

	/// Whether row `index` carries a value. Out-of-range indices answer `false`.
	#[must_use]
	pub fn is_present(&self, index: usize) -> bool {
		if index >= self.row_count {
			return false;
		}
		self.bits.as_ref().is_none_or(|bits| (bits[index / 8] >> (index % 8)) & 1 == 1)
	}

	/// The packed presence bitmap, or [`None`] for a dense column. The bytes the
	/// `.dspseg` frame writer stores verbatim (LSB-first, `ceil(row_count / 8)`
	/// bytes).
	#[must_use]
	pub fn bitmap_bytes(&self) -> Option<&[u8]> {
		self.bits.as_deref()
	}

	/// Estimated stored bytes of the quality column: zero for a dense column,
	/// otherwise the packed bitmap length (`ceil(row_count / 8)`).
	#[must_use]
	pub fn estimated_bytes(&self) -> usize {
		self.bits.as_ref().map_or(0, Vec::len)
	}
}

/// Why a [`NullMask`] could not be reconstructed from raw frame parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NullMaskError {
	/// The packed bitmap length did not match `ceil(row_count / 8)`.
	BitmapLength {
		/// The length the row count implies.
		expected: usize,
		/// The length actually supplied.
		found: usize,
	},
	/// The declared null count did not match the number of clear bits in the
	/// bitmap (or was non-zero for a dense column).
	NullCountMismatch {
		/// The null count the frame declared.
		declared: usize,
		/// The null count the bitmap actually encodes.
		actual: usize,
	},
}

impl std::fmt::Display for NullMaskError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::BitmapLength { expected, found } => write!(f, "null bitmap length {found} does not match the {expected} bytes the row count implies"),
			Self::NullCountMismatch { declared, actual } => write!(f, "declared null count {declared} does not match the {actual} absent rows in the bitmap"),
		}
	}
}

impl std::error::Error for NullMaskError {}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn dense_mask_stores_no_bytes() {
		let mask = NullMask::all_present(1_000);
		assert!(mask.is_dense());
		assert_eq!(mask.row_count(), 1_000);
		assert_eq!(mask.null_count(), 0);
		assert_eq!(mask.present_count(), 1_000);
		assert_eq!(mask.estimated_bytes(), 0);
		assert!(mask.bitmap_bytes().is_none());
		assert!(mask.is_present(0));
		assert!(mask.is_present(999));
		assert!(!mask.is_present(1_000), "out of range is absent");
	}

	#[test]
	fn fully_present_presence_collapses_to_dense() {
		let mask = NullMask::from_presence(&[true, true, true]);
		assert!(mask.is_dense(), "no nulls ⇒ no bitmap");
		assert_eq!(mask.null_count(), 0);
	}

	#[test]
	fn sparse_mask_tracks_each_row() {
		// present, null, present, present, null  → 2 nulls, 3 present.
		let present = [true, false, true, true, false];
		let mask = NullMask::from_presence(&present);
		assert!(!mask.is_dense());
		assert_eq!(mask.row_count(), 5);
		assert_eq!(mask.null_count(), 2);
		assert_eq!(mask.present_count(), 3);
		// 5 rows ⇒ 1 byte.
		assert_eq!(mask.estimated_bytes(), 1);
		for (i, &p) in present.iter().enumerate() {
			assert_eq!(mask.is_present(i), p, "row {i}");
		}
	}

	#[test]
	fn bitmap_is_lsb_first() {
		// Only row 0 present ⇒ low bit set, value 0b0000_0001.
		let mask = NullMask::from_presence(&[true, false, false, false]);
		assert_eq!(mask.bitmap_bytes(), Some(&[0b0000_0001_u8][..]));
		// Only row 3 present ⇒ bit 3 set.
		let mask = NullMask::from_presence(&[false, false, false, true]);
		assert_eq!(mask.bitmap_bytes(), Some(&[0b0000_1000_u8][..]));
	}

	#[test]
	fn bitmap_spans_multiple_bytes() {
		// 9 rows, only the last present ⇒ 2 bytes, bit 0 of byte 1 set.
		let mut present = vec![false; 9];
		present[8] = true;
		let mask = NullMask::from_presence(&present);
		assert_eq!(mask.estimated_bytes(), 2);
		assert_eq!(mask.bitmap_bytes(), Some(&[0b0000_0000_u8, 0b0000_0001_u8][..]));
		assert!(mask.is_present(8));
		assert!(!mask.is_present(7));
	}

	#[test]
	fn from_raw_round_trips_a_sparse_mask() {
		let mask = NullMask::from_presence(&[true, false, true, true, false]);
		let bits = mask.bitmap_bytes().map(<[u8]>::to_vec);
		let back = NullMask::from_raw(mask.row_count(), mask.null_count(), bits).expect("valid parts");
		assert_eq!(mask, back);
	}

	#[test]
	fn from_raw_accepts_a_dense_column() {
		let back = NullMask::from_raw(10, 0, None).expect("dense is valid");
		assert!(back.is_dense());
		assert_eq!(back, NullMask::all_present(10));
	}

	#[test]
	fn from_raw_rejects_a_wrong_length_bitmap() {
		// 5 rows imply 1 byte; supplying 2 is a corrupt frame.
		let err = NullMask::from_raw(5, 1, Some(vec![0b0000_0001, 0])).expect_err("bad length");
		assert_eq!(err, NullMaskError::BitmapLength { expected: 1, found: 2 });
	}

	#[test]
	fn from_raw_rejects_a_mismatched_null_count() {
		// Bitmap encodes 2 nulls (rows 1 and 2 clear) but the frame declares 1.
		let err = NullMask::from_raw(4, 1, Some(vec![0b0000_1001])).expect_err("bad count");
		assert_eq!(err, NullMaskError::NullCountMismatch { declared: 1, actual: 2 });
	}

	#[test]
	fn from_raw_rejects_nonzero_nulls_on_a_dense_column() {
		let err = NullMask::from_raw(4, 1, None).expect_err("dense cannot have nulls");
		assert_eq!(err, NullMaskError::NullCountMismatch { declared: 1, actual: 0 });
	}

	#[test]
	fn all_null_column_is_all_clear() {
		let mask = NullMask::from_presence(&[false, false, false]);
		assert_eq!(mask.null_count(), 3);
		assert_eq!(mask.present_count(), 0);
		assert_eq!(mask.bitmap_bytes(), Some(&[0b0000_0000_u8][..]));
		assert!(!mask.is_present(0));
	}

	#[test]
	fn empty_mask_is_dense_and_empty() {
		let mask = NullMask::from_presence(&[]);
		assert!(mask.is_dense());
		assert_eq!(mask.row_count(), 0);
		assert_eq!(mask.estimated_bytes(), 0);
		assert!(!mask.is_present(0));
	}
}
