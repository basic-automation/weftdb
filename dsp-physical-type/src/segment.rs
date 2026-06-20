//! In-memory typed columnar segment (roadmap **Phase 4.3**, first slice).
//!
//! Phase 4.1 gave each aspect a [`PhysicalType`](crate::PhysicalType) value
//! column ([`crate::column`]); Phase 4.2 gave it an integer-epoch timestamp column
//! with delta / delta-of-delta / RLE codecs ([`crate::timestamp`]). Phase 4.3
//! binds the two into a **segment** — the unit Storage v2 seals to disk as a
//! `.dspseg` file. This module is the in-memory shape of that segment: the typed
//! value column, the timestamp column, and the per-segment statistics a reader
//! needs to prune and account for it, with an exact encode/decode round trip.
//!
//! ## What this slice covers (and what it does not)
//!
//! Phase 4.3 ultimately names a long list of per-segment metadata — quality
//! columns, page offsets, per-page stats, checksums, codec tags. This first slice
//! is deliberately bounded to the pieces every later one builds on:
//!
//! - the **typed value column** ([`ColumnEncoding`]) chosen by
//!   [`recommend_encoding`] within a declared tolerance,
//! - the **timestamp column** ([`DeltaOfDeltaColumn`]) packed with the cheaper of
//!   plain-varint or RLE second differences,
//! - per-segment **min/max timestamp**, **min/max value**, and **row count**
//!   (the data-skipping inputs of Phase 4.4),
//! - a format **version** so a sealed segment can evolve,
//! - and a [`bytes_per_point`](Segment::bytes_per_point) that **matches the
//!   DSP-Bench `StorageEstimate`** — the advisory estimate the bench already
//!   reports and the realized cost of an actual stored segment are the same
//!   number, by construction (both call the same column byte estimators).
//!
//! Not yet here, and called out so the gap is honest: a separate quality/null
//! column (so `null_count` is currently always zero — a segment is built from a
//! dense `(timestamp, value)` column), page-level subdivision and per-page stats,
//! checksums, and the on-disk framing/serialization of the `.dspseg` file itself.
//! Those are the next Phase-4.3/4.4 slices.

use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};

use crate::{
	column::{recommend_encoding, ColumnEncoding}, timestamp::{decode_delta_of_delta, encode_delta_of_delta, DeltaOfDeltaColumn, TimeUnit}
};

/// The on-disk/in-memory format version of a [`Segment`].
///
/// Bumped whenever the segment layout changes in a way that a reader must branch
/// on. A sealed `.dspseg` carries this so future Storage v2 code can refuse or
/// migrate an incompatible segment rather than misread it.
pub const SEGMENT_FORMAT_VERSION: u16 = 1;

/// Why a [`Segment`] could not be built from its input columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentError {
	/// The timestamp and value slices had different lengths — a segment's columns
	/// must be the same height (one value per timestamp).
	LengthMismatch {
		/// Number of timestamps supplied.
		timestamps: usize,
		/// Number of values supplied.
		values: usize,
	},
}

impl std::fmt::Display for SegmentError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::LengthMismatch { timestamps, values } => write!(f, "segment column height mismatch: {timestamps} timestamps vs {values} values"),
		}
	}
}

impl std::error::Error for SegmentError {}

/// Per-segment summary statistics — the metadata a reader consults before
/// touching the columns (data skipping, accounting, integrity).
///
/// `min`/`max` are [`None`] only for an empty segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentStats {
	/// Number of rows (one value per timestamp). The authoritative height — the
	/// timestamp column alone cannot distinguish an empty column from a
	/// single-anchor one, so the segment records the count explicitly.
	pub row_count: usize,
	/// Number of null/absent values. Always `0` in this slice (segments are built
	/// from a dense column); a dedicated quality column lands in a later slice.
	pub null_count: usize,
	/// Smallest timestamp in the segment, or [`None`] when empty.
	pub min_ts: Option<i64>,
	/// Largest timestamp in the segment, or [`None`] when empty.
	pub max_ts: Option<i64>,
	/// Smallest logical value in the segment, or [`None`] when empty.
	pub min_value: Option<BigDecimal>,
	/// Largest logical value in the segment, or [`None`] when empty.
	pub max_value: Option<BigDecimal>,
}

/// An in-memory typed columnar segment: a timestamp column, a value column, and
/// the statistics binding them.
///
/// Build one with [`Segment::build`]; reconstruct the logical columns with
/// [`Segment::decode`]. The round trip is exact for the timestamps (the codecs
/// are lossless) and exact for the values up to the tolerance the value encoding
/// was chosen under (`0` tolerance ⇒ fully lossless, reported by
/// [`is_exact`](Segment::is_exact)).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
	/// The segment layout version ([`SEGMENT_FORMAT_VERSION`] at build time).
	pub version: u16,
	/// The typed, physically-encoded value column.
	pub values: ColumnEncoding,
	/// The integer-epoch timestamp column (delta-of-delta transform).
	pub timestamps: DeltaOfDeltaColumn,
	/// Per-segment summary statistics.
	pub stats: SegmentStats,
}

impl Segment {
	/// Build a segment from parallel timestamp and value columns.
	///
	/// The value column is encoded by [`recommend_encoding`] — the narrowest
	/// hot-path [`PhysicalType`](crate::PhysicalType) whose worst per-value error
	/// is within `value_tolerance` (use `0` for a fully lossless segment). The
	/// timestamp column is delta-of-delta transformed under `unit`. Min/max
	/// statistics are computed from the inputs in one pass.
	///
	/// # Errors
	///
	/// Returns [`SegmentError::LengthMismatch`] if `timestamps` and `values`
	/// differ in length. The encoding itself never fails — `recommend_encoding`
	/// backstops to the always-exact text encoding.
	pub fn build(timestamps: &[i64], values: &[BigDecimal], unit: TimeUnit, value_tolerance: &BigDecimal) -> Result<Self, SegmentError> {
		if timestamps.len() != values.len() {
			return Err(SegmentError::LengthMismatch { timestamps: timestamps.len(), values: values.len() });
		}
		let value_col = recommend_encoding(values, value_tolerance);
		let ts_col = encode_delta_of_delta(timestamps, unit);
		let stats = SegmentStats { row_count: values.len(), null_count: 0, min_ts: timestamps.iter().copied().min(), max_ts: timestamps.iter().copied().max(), min_value: values.iter().min().cloned(), max_value: values.iter().max().cloned() };
		Ok(Self { version: SEGMENT_FORMAT_VERSION, values: value_col, timestamps: ts_col, stats })
	}

	/// Number of rows in the segment.
	#[must_use]
	pub const fn row_count(&self) -> usize {
		self.stats.row_count
	}

	/// `true` iff the segment holds no rows.
	#[must_use]
	pub const fn is_empty(&self) -> bool {
		self.stats.row_count == 0
	}

	/// `true` iff every value reconstructs exactly (the value encoding was lossless
	/// for the whole column). Timestamps are always lossless.
	#[must_use]
	pub const fn is_exact(&self) -> bool {
		self.values.is_exact()
	}

	/// The physical encoding the value column was stored under.
	#[must_use]
	pub const fn physical_type(&self) -> crate::PhysicalType {
		self.values.physical_type
	}

	/// The epoch resolution the timestamp column is stored in.
	#[must_use]
	pub const fn time_unit(&self) -> TimeUnit {
		self.timestamps.unit
	}

	/// Estimated stored bytes of the value column (see
	/// [`ColumnEncoding::estimated_bytes`]).
	#[must_use]
	pub fn value_bytes(&self) -> usize {
		self.values.estimated_bytes()
	}

	/// Estimated stored bytes of the timestamp column, taking the cheaper of
	/// plain-varint or RLE second differences (see
	/// [`DeltaOfDeltaColumn::best_estimated_bytes`]).
	#[must_use]
	pub fn timestamp_bytes(&self) -> usize {
		self.timestamps.best_estimated_bytes()
	}

	/// Total estimated stored bytes: value column plus timestamp column.
	#[must_use]
	pub fn total_bytes(&self) -> usize {
		self.value_bytes() + self.timestamp_bytes()
	}

	/// Storage cost in **bytes per point** — the north-star term. Equal to the
	/// DSP-Bench `StorageEstimate::total_bytes_per_point` for the same data, by
	/// construction (both sum the same column byte estimators over the row count).
	/// Zero for an empty segment.
	#[must_use]
	pub fn bytes_per_point(&self) -> f64 {
		if self.stats.row_count == 0 {
			return 0.0;
		}
		#[allow(clippy::cast_precision_loss)]
		let n = self.stats.row_count as f64;
		#[allow(clippy::cast_precision_loss)]
		let total = self.total_bytes() as f64;
		total / n
	}

	/// Reconstruct the logical timestamp column.
	///
	/// Respects the recorded [`row_count`](Segment::row_count): an empty segment
	/// decodes to an empty vector, even though the underlying delta-of-delta column
	/// would reconstruct a lone anchor (the ambiguity the timestamp codec documents
	/// and the segment resolves by carrying the count).
	#[must_use]
	pub fn decode_timestamps(&self) -> Vec<i64> {
		if self.stats.row_count == 0 {
			return Vec::new();
		}
		decode_delta_of_delta(&self.timestamps)
	}

	/// Reconstruct the logical value column.
	#[must_use]
	pub fn decode_values(&self) -> Vec<BigDecimal> {
		self.values.decode()
	}

	/// Reconstruct both logical columns as parallel vectors.
	#[must_use]
	pub fn decode(&self) -> (Vec<i64>, Vec<BigDecimal>) {
		(self.decode_timestamps(), self.decode_values())
	}
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use super::*;
	use crate::PhysicalType;

	fn col(lits: &[&str]) -> Vec<BigDecimal> {
		lits.iter().map(|s| BigDecimal::from_str(s).expect("parses")).collect()
	}

	#[test]
	fn round_trips_a_regular_segment() {
		let timestamps: Vec<i64> = (0..5).map(|i| 1_000 + i * 10).collect();
		let values = col(&["1.25", "2.50", "3.75", "5.00", "6.25"]);
		let seg = Segment::build(&timestamps, &values, TimeUnit::Micros, &BigDecimal::from(0)).expect("builds");
		assert_eq!(seg.version, SEGMENT_FORMAT_VERSION);
		assert_eq!(seg.row_count(), 5);
		assert!(!seg.is_empty());
		assert!(seg.is_exact());
		let (ts, vs) = seg.decode();
		assert_eq!(ts, timestamps);
		assert_eq!(vs, values);
	}

	#[test]
	fn stats_capture_min_max_and_count() {
		let timestamps = vec![30_i64, 10, 50, 20];
		let values = col(&["3.0", "1.0", "9.0", "2.0"]);
		let seg = Segment::build(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert_eq!(seg.stats.row_count, 4);
		assert_eq!(seg.stats.null_count, 0);
		assert_eq!(seg.stats.min_ts, Some(10));
		assert_eq!(seg.stats.max_ts, Some(50));
		assert_eq!(seg.stats.min_value, Some(BigDecimal::from_str("1.0").unwrap()));
		assert_eq!(seg.stats.max_value, Some(BigDecimal::from_str("9.0").unwrap()));
	}

	#[test]
	fn length_mismatch_is_rejected() {
		let timestamps = vec![1_i64, 2, 3];
		let values = col(&["1.0", "2.0"]);
		let err = Segment::build(&timestamps, &values, TimeUnit::Millis, &BigDecimal::from(0)).expect_err("mismatched heights");
		assert_eq!(err, SegmentError::LengthMismatch { timestamps: 3, values: 2 });
	}

	#[test]
	fn empty_segment_decodes_to_empty() {
		let seg = Segment::build(&[], &[], TimeUnit::Nanos, &BigDecimal::from(0)).expect("builds");
		assert!(seg.is_empty());
		assert_eq!(seg.row_count(), 0);
		assert_eq!(seg.stats.min_ts, None);
		assert_eq!(seg.stats.max_value, None);
		let (ts, vs) = seg.decode();
		assert!(ts.is_empty(), "empty segment must decode to no timestamps, not a lone anchor");
		assert!(vs.is_empty());
		assert!((seg.bytes_per_point() - 0.0).abs() < f64::EPSILON);
	}

	#[test]
	fn single_point_round_trips() {
		let seg = Segment::build(&[42], &col(&["7.5"]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert_eq!(seg.row_count(), 1);
		let (ts, vs) = seg.decode();
		assert_eq!(ts, vec![42]);
		assert_eq!(vs, col(&["7.5"]));
	}

	#[test]
	fn bytes_per_point_sums_the_two_columns() {
		let timestamps: Vec<i64> = (0..1_000).map(|i| 1_000 + i * 10).collect();
		let values: Vec<BigDecimal> = (0..1_000).map(BigDecimal::from).collect();
		let seg = Segment::build(&timestamps, &values, TimeUnit::Micros, &BigDecimal::from(0)).expect("builds");
		// Value + timestamp bytes, divided by row count, equals the public metric.
		let expected = f64::from(u32::try_from(seg.value_bytes() + seg.timestamp_bytes()).expect("fits u32")) / 1_000.0;
		assert!((seg.bytes_per_point() - expected).abs() < f64::EPSILON);
		// The regular timestamp series collapses to a tiny RLE'd column, so the
		// timestamp half is far below the raw 8 bytes/point.
		assert!(seg.timestamp_bytes() < 100, "regular series packs tiny: {}", seg.timestamp_bytes());
	}

	#[test]
	fn lossy_tolerance_picks_a_narrower_encoding() {
		// 0.1-style decimals are not binary-exact; a generous tolerance lets the
		// segment take the cheapest (lossy F32) encoding, reported as not-exact.
		let timestamps: Vec<i64> = (0..3).collect();
		let values = col(&["0.1", "0.2", "0.3"]);
		let seg = Segment::build(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from_str("0.01").unwrap()).expect("builds");
		assert_eq!(seg.physical_type(), PhysicalType::F32);
		assert!(!seg.is_exact(), "a lossy encoding within tolerance is honestly reported");
	}

	#[test]
	fn segment_serde_round_trips() {
		let timestamps: Vec<i64> = (0..4).map(|i| i * 100).collect();
		let values = col(&["1.0", "2.0", "3.0", "4.0"]);
		let seg = Segment::build(&timestamps, &values, TimeUnit::Millis, &BigDecimal::from(0)).expect("builds");
		let json = serde_json::to_string(&seg).expect("serializes");
		let back: Segment = serde_json::from_str(&json).expect("deserializes");
		assert_eq!(seg, back);
		assert_eq!(back.decode(), (timestamps, values));
	}
}
