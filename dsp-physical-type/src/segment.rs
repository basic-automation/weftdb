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
	/// Whether the timestamps are monotonic non-decreasing (`ts[i] <= ts[i+1]`).
	///
	/// `true` for the in-order common case (and vacuously for an empty or
	/// single-row segment). A `false` value flags **out-of-order ingest** (Phase
	/// 4.6) and tells a reader it cannot binary-search within the segment for a
	/// point lookup — it must scan linearly. Computed once at
	/// [`build`](Segment::build).
	pub time_sorted: bool,
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
		let time_sorted = timestamps.windows(2).all(|w| w[0] <= w[1]);
		let stats = SegmentStats { row_count: values.len(), null_count: 0, time_sorted, min_ts: timestamps.iter().copied().min(), max_ts: timestamps.iter().copied().max(), min_value: values.iter().min().cloned(), max_value: values.iter().max().cloned() };
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

	/// The name of the timestamp codec chosen for this segment
	/// (`delta_of_delta` or `delta_of_delta_rle`); see
	/// [`DeltaOfDeltaColumn::best_encoding_name`].
	#[must_use]
	pub fn timestamp_encoding_name(&self) -> &'static str {
		self.timestamps.best_encoding_name()
	}

	/// Whether this segment's timestamps are monotonic non-decreasing.
	///
	/// `true` admits intra-segment binary search for a point lookup; `false`
	/// signals out-of-order ingest (Phase 4.6) and forces a linear scan. See
	/// [`SegmentStats::time_sorted`].
	#[must_use]
	pub const fn is_time_sorted(&self) -> bool {
		self.stats.time_sorted
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

	/// Seal this segment to its on-disk `.dspseg` byte frame (magic + header +
	/// column blocks + trailing CRC-32). See [`crate::dspseg::write_segment`].
	///
	/// Exact inverse of [`Segment::read_from`].
	#[must_use]
	pub fn write_to(&self) -> Vec<u8> {
		crate::dspseg::write_segment(self)
	}

	/// Read a segment back from a `.dspseg` byte frame, verifying its checksum.
	///
	/// Exact inverse of [`Segment::write_to`]. See [`crate::dspseg::read_segment`].
	///
	/// # Errors
	///
	/// Propagates [`crate::dspseg::DspSegError`] for a corrupt, truncated, or
	/// unrecognised frame (checksum mismatch, bad magic, unsupported version, …).
	pub fn read_from(bytes: &[u8]) -> Result<Self, crate::dspseg::DspSegError> {
		crate::dspseg::read_segment(bytes)
	}

	/// The inclusive `(min, max)` timestamp span this segment covers, or [`None`]
	/// when empty. The coarse index a time-range query prunes against.
	#[must_use]
	pub const fn time_range(&self) -> Option<(i64, i64)> {
		match (self.stats.min_ts, self.stats.max_ts) {
			(Some(lo), Some(hi)) => Some((lo, hi)),
			_ => None,
		}
	}

	/// The inclusive `(min, max)` value span this segment covers, or [`None`] when
	/// empty.
	#[must_use]
	pub fn value_range(&self) -> Option<(BigDecimal, BigDecimal)> {
		match (&self.stats.min_value, &self.stats.max_value) {
			(Some(lo), Some(hi)) => Some((lo.clone(), hi.clone())),
			_ => None,
		}
	}

	/// **Data skipping** (roadmap Phase 4.4): whether this segment *may* hold any
	/// row whose timestamp falls in the inclusive query range `[start, end]`.
	///
	/// Returns `false` only when the segment can be **safely skipped** — its
	/// `[min_ts, max_ts]` span is disjoint from `[start, end]`, so no row inside it
	/// can match (an empty segment is always skippable). A `true` result is
	/// *necessary, not sufficient*: the span overlaps, so the segment must be read,
	/// but the rows within it still need the exact predicate applied.
	#[must_use]
	pub const fn overlaps_time(&self, start: i64, end: i64) -> bool {
		match self.time_range() {
			Some((lo, hi)) => lo <= end && start <= hi,
			None => false,
		}
	}

	/// Data-skipping shorthand for a single-instant lookup: whether `ts` lies
	/// within `[min_ts, max_ts]`. A `false` result means the segment cannot hold
	/// that timestamp; `true` means it might (subject to the exact column scan).
	#[must_use]
	pub const fn contains_timestamp(&self, ts: i64) -> bool {
		self.overlaps_time(ts, ts)
	}

	/// **Data skipping** on the value column: whether this segment *may* hold any
	/// value in the inclusive range `[lo, hi]`.
	///
	/// Returns `false` only when `[min_value, max_value]` is disjoint from
	/// `[lo, hi]` (or the segment is empty) — the safe-to-skip case. As with
	/// [`overlaps_time`](Self::overlaps_time), `true` is necessary but not
	/// sufficient.
	#[must_use]
	pub fn may_contain_value(&self, lo: &BigDecimal, hi: &BigDecimal) -> bool {
		self.value_range().is_some_and(|(min, max)| &min <= hi && lo <= &max)
	}
}

/// **Data skipping over a set of segments** (roadmap Phase 4.4).
///
/// Returns the indices of the segments in `segments` that *may* hold a row in the
/// inclusive time range `[start, end]` — i.e. the ones a query must actually scan.
///
/// Every index **not** returned is a segment safely skipped without touching its
/// columns (its span is disjoint from the query, or it is empty). This is the
/// realistic query-time win the per-segment min/max stats exist for: a long-lived
/// aspect is a sequence of sealed segments, and a bounded range query reads only
/// the few that overlap. Order-independent — segments need not be sorted.
#[must_use]
pub fn prune_by_time(segments: &[Segment], start: i64, end: i64) -> Vec<usize> {
	segments.iter().enumerate().filter(|(_, s)| s.overlaps_time(start, end)).map(|(i, _)| i).collect()
}

/// **Value data skipping over a set of segments** (roadmap Phase 4.4).
///
/// The value-column mirror of [`prune_by_time`]: returns the indices of the
/// segments whose `[min_value, max_value]` span may intersect the inclusive range
/// `[lo, hi]` — the ones a value-predicate query must scan. Every other segment
/// is safely skipped on its min/max stats alone. Order-independent.
#[must_use]
pub fn prune_by_value(segments: &[Segment], lo: &BigDecimal, hi: &BigDecimal) -> Vec<usize> {
	segments.iter().enumerate().filter(|(_, s)| s.may_contain_value(lo, hi)).map(|(i, _)| i).collect()
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
	fn time_sorted_flag_reflects_ordering() {
		// In-order (and equal-timestamp) series are monotonic.
		let sorted = Segment::build(&[10, 10, 20, 30], &col(&["1.0", "2.0", "3.0", "4.0"]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert!(sorted.is_time_sorted());
		assert!(sorted.stats.time_sorted);
		// A single dip breaks monotonicity → out-of-order.
		let unsorted = Segment::build(&[10, 30, 20, 40], &col(&["1.0", "2.0", "3.0", "4.0"]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert!(!unsorted.is_time_sorted());
		// Empty and single-row segments are vacuously sorted.
		assert!(Segment::build(&[], &[], TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds").is_time_sorted());
		assert!(Segment::build(&[5], &col(&["1.0"]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds").is_time_sorted());
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

	#[test]
	fn time_range_pruning_skips_disjoint_segments() {
		// Segment spans timestamps [100, 140].
		let timestamps: Vec<i64> = (0..5).map(|i| 100 + i * 10).collect();
		let values = col(&["1.0", "2.0", "3.0", "4.0", "5.0"]);
		let seg = Segment::build(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert_eq!(seg.time_range(), Some((100, 140)));
		// Overlapping queries must be read.
		assert!(seg.overlaps_time(120, 130), "fully inside");
		assert!(seg.overlaps_time(0, 100), "touches the low edge");
		assert!(seg.overlaps_time(140, 999), "touches the high edge");
		assert!(seg.overlaps_time(0, 999), "superset");
		// Disjoint queries are safely skipped.
		assert!(!seg.overlaps_time(0, 99), "entirely before");
		assert!(!seg.overlaps_time(141, 200), "entirely after");
		// Single-instant lookups.
		assert!(seg.contains_timestamp(100));
		assert!(seg.contains_timestamp(135));
		assert!(!seg.contains_timestamp(99));
		assert!(!seg.contains_timestamp(141));
	}

	#[test]
	fn value_range_pruning_skips_disjoint_segments() {
		let timestamps: Vec<i64> = (0..4).collect();
		let values = col(&["10.0", "25.0", "5.0", "18.0"]); // span [5, 25]
		let seg = Segment::build(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert_eq!(seg.value_range(), Some((BigDecimal::from_str("5.0").unwrap(), BigDecimal::from_str("25.0").unwrap())));
		assert!(seg.may_contain_value(&BigDecimal::from(0), &BigDecimal::from(7)), "overlaps low end");
		assert!(seg.may_contain_value(&BigDecimal::from(20), &BigDecimal::from(100)), "overlaps high end");
		assert!(seg.may_contain_value(&BigDecimal::from(12), &BigDecimal::from(13)), "inside");
		assert!(!seg.may_contain_value(&BigDecimal::from(26), &BigDecimal::from(100)), "above the span");
		assert!(!seg.may_contain_value(&BigDecimal::from(0), &BigDecimal::from(4)), "below the span");
	}

	#[test]
	fn prune_by_time_selects_only_overlapping_segments() {
		// Three sealed segments covering [0,90], [100,190], [200,290].
		let seg = |base: i64| {
			let ts: Vec<i64> = (0..10).map(|i| base + i * 10).collect();
			let vs: Vec<BigDecimal> = (0..10).map(BigDecimal::from).collect();
			Segment::build(&ts, &vs, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds")
		};
		let segments = vec![seg(0), seg(100), seg(200)];
		// A query inside the middle segment scans only it.
		assert_eq!(prune_by_time(&segments, 120, 150), vec![1]);
		// A query straddling the first two scans both, skips the third.
		assert_eq!(prune_by_time(&segments, 50, 150), vec![0, 1]);
		// A query covering everything scans all three.
		assert_eq!(prune_by_time(&segments, 0, 290), vec![0, 1, 2]);
		// A query in a gap between segments scans none.
		assert_eq!(prune_by_time(&segments, 91, 99), Vec::<usize>::new());
		// A query past the end scans none.
		assert_eq!(prune_by_time(&segments, 1_000, 2_000), Vec::<usize>::new());
	}

	#[test]
	fn prune_by_value_selects_only_overlapping_segments() {
		// Segments with disjoint value spans: [0,9], [100,109], [200,209].
		let seg = |base: i64| {
			let ts: Vec<i64> = (0..10).collect();
			let vs: Vec<BigDecimal> = (0..10).map(|i| BigDecimal::from(base + i)).collect();
			Segment::build(&ts, &vs, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds")
		};
		let segments = vec![seg(0), seg(100), seg(200)];
		assert_eq!(prune_by_value(&segments, &BigDecimal::from(102), &BigDecimal::from(108)), vec![1]);
		assert_eq!(prune_by_value(&segments, &BigDecimal::from(5), &BigDecimal::from(105)), vec![0, 1]);
		assert_eq!(prune_by_value(&segments, &BigDecimal::from(0), &BigDecimal::from(209)), vec![0, 1, 2]);
		// A range in a value gap scans none.
		assert_eq!(prune_by_value(&segments, &BigDecimal::from(50), &BigDecimal::from(60)), Vec::<usize>::new());
	}

	#[test]
	fn prune_by_time_handles_empty_inputs() {
		assert!(prune_by_time(&[], 0, 100).is_empty());
		let empty_seg = Segment::build(&[], &[], TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		// An empty segment is always skipped.
		assert!(prune_by_time(std::slice::from_ref(&empty_seg), i64::MIN, i64::MAX).is_empty());
	}

	#[test]
	fn empty_segment_prunes_to_nothing() {
		let seg = Segment::build(&[], &[], TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert_eq!(seg.time_range(), None);
		assert_eq!(seg.value_range(), None);
		// An empty segment can be skipped for every query.
		assert!(!seg.overlaps_time(i64::MIN, i64::MAX));
		assert!(!seg.contains_timestamp(0));
		assert!(!seg.may_contain_value(&BigDecimal::from(0), &BigDecimal::from(0)));
	}
}
