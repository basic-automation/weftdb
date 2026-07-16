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
//! A separate **quality/null column** now lands too: [`Segment::build_nullable`]
//! takes a `&[Option<BigDecimal>]` value column, stores only the present values
//! densely, and records which rows are present in a [`NullMask`] so
//! [`Segment::decode_nullable`] reconstructs the gaps — making `null_count` real
//! (the dense [`Segment::build`] path is the all-present special case, costing no
//! mask bytes).
//!
//! Not yet here, and called out so the gap is honest: page-level subdivision and
//! per-page stats (the frame is single-block), tag/quality *pruning*, and the
//! catalog/metadata/index DBs. Those are the next Phase-4.3/4.4 slices.

use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};

use crate::{
	column::{recommend_encoding, ColumnEncoding}, nulls::NullMask, timestamp::{decode_delta_of_delta, encode_delta_of_delta, DeltaOfDeltaColumn, TimeUnit}
};

/// The on-disk/in-memory format version of a [`Segment`].
///
/// Bumped whenever the segment layout changes in a way that a reader must branch
/// on. A sealed `.dspseg` carries this so future Storage v2 code can refuse or
/// migrate an incompatible segment rather than misread it.
///
/// - **v1** — dense `(timestamp, value)` columns only (no quality column).
/// - **v2** — adds the [`NullMask`] quality column block, so a segment can carry
///   null/absent rows ([`Segment::build_nullable`]).
/// - **v3** — the timestamp-column block gains a self-describing codec selector
///   so a regular / small-jitter series' second differences store fixed-width
///   bit-packed instead of one-byte-per-value varint (realizing the bytes/point
///   saving, not just estimating it).
/// - **v4** — the value-column block gains a self-describing codec selector so a
///   `ScaledI64` column whose mantissas pack smaller stores fixed-width bit-packed
///   instead of per-value zig-zag varint (the value-column analogue of v3).
/// - **v5** — the timestamp-column block gains a fifth codec option, per-block adaptive
///   (dynamic) bit-packing, chosen for a mixed-magnitude second-difference stream.
pub const SEGMENT_FORMAT_VERSION: u16 = 5;

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
	/// A paged build was asked for a page height of zero rows — a page must hold at
	/// least one row, so the rows cannot be partitioned. See
	/// [`PagedSegment::build`](crate::page::PagedSegment::build).
	EmptyPageSize,
	/// An order-enforcing build ([`Segment::build_sorted`] /
	/// [`Segment::build_nullable_sorted`]) was given timestamps that step backwards.
	/// Reported at the first offending row (its timestamp is strictly less than its
	/// predecessor's); equal adjacent timestamps are accepted.
	OutOfOrder {
		/// The row whose timestamp broke monotonic non-decreasing order.
		index: usize,
		/// The preceding row's timestamp.
		previous: i64,
		/// This row's (smaller) timestamp.
		current: i64,
	},
}

impl std::fmt::Display for SegmentError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::LengthMismatch { timestamps, values } => write!(f, "segment column height mismatch: {timestamps} timestamps vs {values} values"),
			Self::EmptyPageSize => write!(f, "paged segment page height must be at least one row, got zero"),
			Self::OutOfOrder { index, previous, current } => write!(f, "out-of-order timestamp at row {index}: {current} < previous {previous}"),
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
	/// Number of rows, present and null alike (the timestamp column is dense — one
	/// timestamp per row). The authoritative height — the timestamp column alone
	/// cannot distinguish an empty column from a single-anchor one, so the segment
	/// records the count explicitly.
	pub row_count: usize,
	/// Number of null/absent values (rows with a timestamp but no value). Zero for
	/// a dense segment built with [`Segment::build`]; populated from the
	/// [`NullMask`] for one built with [`Segment::build_nullable`].
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

impl SegmentStats {
	/// Compute the per-segment statistics for a **dense** column (no nulls) in a
	/// single pass. Shared by [`Segment::build`] and the schema-declared
	/// [`AspectSchema::seal`](crate::schema::AspectSchema::seal) so both produce
	/// identical stats. Assumes the columns are equal height (the caller checks).
	pub(crate) fn from_columns(timestamps: &[i64], values: &[BigDecimal]) -> Self {
		Self::from_columns_nullable(timestamps, values, 0)
	}

	/// Compute the per-segment statistics for a column with `null_count` absent
	/// rows. `timestamps` is the dense per-row timestamp column (every row, present
	/// or null); `present_values` holds only the non-null values (its length is the
	/// row count minus `null_count`). Min/max value are over the present values
	/// only — a null contributes a timestamp but no value to the range.
	pub(crate) fn from_columns_nullable(timestamps: &[i64], present_values: &[BigDecimal], null_count: usize) -> Self {
		let time_sorted = timestamps.windows(2).all(|w| w[0] <= w[1]);
		Self { row_count: timestamps.len(), null_count, time_sorted, min_ts: timestamps.iter().copied().min(), max_ts: timestamps.iter().copied().max(), min_value: present_values.iter().min().cloned(), max_value: present_values.iter().max().cloned() }
	}
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
	/// The typed, physically-encoded value column — **present values only** (a null
	/// row contributes nothing here; see [`nulls`](Self::nulls)).
	pub values: ColumnEncoding,
	/// The integer-epoch timestamp column (delta-of-delta transform), dense over
	/// every row (present or null).
	pub timestamps: DeltaOfDeltaColumn,
	/// The quality column: which rows carry a value vs a null. Fully dense (no
	/// stored bytes) for a segment built with [`Segment::build`].
	pub nulls: NullMask,
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
		let stats = SegmentStats::from_columns(timestamps, values);
		Ok(Self { version: SEGMENT_FORMAT_VERSION, values: value_col, timestamps: ts_col, nulls: NullMask::all_present(values.len()), stats })
	}

	/// Build a segment from a dense timestamp column and a **nullable** value
	/// column — the Phase-4.3 quality-column path.
	///
	/// Every row has a timestamp; a `None` value marks a null/absent row. The
	/// present (non-null) values are encoded densely by [`recommend_encoding`]
	/// (the value column never stores a null), and a [`NullMask`] records which
	/// rows are present so [`decode_nullable`](Segment::decode_nullable) can
	/// interleave the values back with `None`s. A fully-present input produces a
	/// segment byte-for-byte equivalent to [`build`](Segment::build) (the mask is
	/// dense, costing nothing).
	///
	/// `min_value`/`max_value` in the stats are over the present values only.
	///
	/// # Errors
	///
	/// Returns [`SegmentError::LengthMismatch`] if `timestamps` and `values` differ
	/// in length. As with [`build`](Segment::build), the encoding itself never
	/// fails (the text encoding is the always-exact backstop).
	pub fn build_nullable(timestamps: &[i64], values: &[Option<BigDecimal>], unit: TimeUnit, value_tolerance: &BigDecimal) -> Result<Self, SegmentError> {
		if timestamps.len() != values.len() {
			return Err(SegmentError::LengthMismatch { timestamps: timestamps.len(), values: values.len() });
		}
		let present: Vec<bool> = values.iter().map(Option::is_some).collect();
		let nulls = NullMask::from_presence(&present);
		let present_values: Vec<BigDecimal> = values.iter().flatten().cloned().collect();
		let value_col = recommend_encoding(&present_values, value_tolerance);
		let ts_col = encode_delta_of_delta(timestamps, unit);
		let stats = SegmentStats::from_columns_nullable(timestamps, &present_values, nulls.null_count());
		Ok(Self { version: SEGMENT_FORMAT_VERSION, values: value_col, timestamps: ts_col, nulls, stats })
	}

	/// Build a segment, **enforcing monotonic non-decreasing timestamps** (roadmap
	/// Phase 4.2 order enforcement).
	///
	/// Identical to [`build`](Self::build) except it rejects a batch whose
	/// timestamps step backwards — returning [`SegmentError::OutOfOrder`] at the
	/// first offending row instead of storing an out-of-order segment
	/// (`time_sorted = false`). Use this on an ingest path that guarantees ordered
	/// appends; use [`build`](Self::build) when out-of-order data is expected and
	/// reconciled downstream (Phase 4.6). Equal adjacent timestamps are accepted (a
	/// repeated instant is in order).
	///
	/// # Errors
	///
	/// [`SegmentError::LengthMismatch`] if the columns differ in height (checked
	/// first), or [`SegmentError::OutOfOrder`] at the first row whose timestamp is
	/// strictly less than its predecessor's.
	pub fn build_sorted(timestamps: &[i64], values: &[BigDecimal], unit: TimeUnit, value_tolerance: &BigDecimal) -> Result<Self, SegmentError> {
		Self::check_order(timestamps, values.len())?;
		Self::build(timestamps, values, unit, value_tolerance)
	}

	/// The nullable-column counterpart to [`build_sorted`](Self::build_sorted):
	/// build a nullable segment ([`build_nullable`](Self::build_nullable)) while
	/// enforcing monotonic non-decreasing timestamps. The order check is over the
	/// dense timestamp column (every row, present or null).
	///
	/// # Errors
	///
	/// [`SegmentError::LengthMismatch`] if the columns differ in height (checked
	/// first), or [`SegmentError::OutOfOrder`] at the first backwards timestamp.
	pub fn build_nullable_sorted(timestamps: &[i64], values: &[Option<BigDecimal>], unit: TimeUnit, value_tolerance: &BigDecimal) -> Result<Self, SegmentError> {
		Self::check_order(timestamps, values.len())?;
		Self::build_nullable(timestamps, values, unit, value_tolerance)
	}

	/// Length-then-order gate shared by the `_sorted` builders: reject a height
	/// mismatch first (consistent error priority with the permissive builders),
	/// then the first backwards timestamp via
	/// [`first_order_violation`](crate::timestamp::first_order_violation).
	fn check_order(timestamps: &[i64], values_len: usize) -> Result<(), SegmentError> {
		if timestamps.len() != values_len {
			return Err(SegmentError::LengthMismatch { timestamps: timestamps.len(), values: values_len });
		}
		if let Some((index, previous, current)) = crate::timestamp::first_order_violation(timestamps) {
			return Err(SegmentError::OutOfOrder { index, previous, current });
		}
		Ok(())
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
	/// (`delta_of_delta`, `delta_of_delta_rle`, or `delta_of_delta_bitpack`); see
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

	/// **Realized** stored bytes of the value column — the exact on-disk payload the
	/// `.dspseg` frame writes under the codec it actually selects (see
	/// [`ColumnEncoding::best_serialized_bytes`]). The headline figure (owner
	/// sign-off, Phase 4/6): [`total_bytes`](Self::total_bytes) and
	/// [`bytes_per_point`](Self::bytes_per_point) build on it. The naive fixed-width
	/// figure this replaced remains available as
	/// [`logical_value_bytes`](Self::logical_value_bytes).
	#[must_use]
	pub fn value_bytes(&self) -> usize {
		self.values.best_serialized_bytes()
	}

	/// The naive **logical** (uncompressed fixed-width) size of the value column —
	/// `count * width` (see [`ColumnEncoding::estimated_bytes`]). The pre-flip headline
	/// figure, retained as the comparison baseline: `value_bytes / logical_value_bytes`
	/// reads as the value column's realized compression ratio.
	#[must_use]
	pub fn logical_value_bytes(&self) -> usize {
		self.values.estimated_bytes()
	}

	/// **Realized** stored bytes of the value column — an alias of
	/// [`value_bytes`](Self::value_bytes), kept for the schema-v7/v8 call sites that
	/// adopted the realized figure before the headline flipped to it.
	#[must_use]
	pub fn serialized_value_bytes(&self) -> usize {
		self.values.best_serialized_bytes()
	}

	/// Estimated stored bytes of the timestamp column, taking the cheapest of
	/// plain-varint, RLE, or bit-packed second differences (see
	/// [`DeltaOfDeltaColumn::best_estimated_bytes`]).
	#[must_use]
	pub fn timestamp_bytes(&self) -> usize {
		self.timestamps.best_estimated_bytes()
	}

	/// Estimated stored bytes of the quality column: zero for a dense segment,
	/// otherwise the packed presence bitmap (`ceil(row_count / 8)` bytes). See
	/// [`NullMask::estimated_bytes`].
	#[must_use]
	pub fn null_bytes(&self) -> usize {
		self.nulls.estimated_bytes()
	}

	/// Number of null/absent rows in the segment.
	#[must_use]
	pub const fn null_count(&self) -> usize {
		self.stats.null_count
	}

	/// `true` iff the segment carries at least one null/absent row.
	#[must_use]
	pub const fn has_nulls(&self) -> bool {
		self.stats.null_count > 0
	}

	/// Number of present (non-null) rows in the segment — the height of the value
	/// column. Equal to `row_count - null_count`.
	#[must_use]
	pub const fn present_count(&self) -> usize {
		self.stats.row_count - self.stats.null_count
	}

	/// `true` iff the segment holds rows but **every** value is null — a segment
	/// that carries timestamps and quality information but no measurements.
	///
	/// Such a segment can be skipped entirely by any query that needs an actual
	/// value (interpolation, value predicates, aggregation over values): it
	/// contributes only `None`s. Its `[min_value, max_value]` span is already
	/// [`None`] (so [`may_contain_value`](Self::may_contain_value) and
	/// [`prune_by_value`] skip it), but a *time*-range value query needs this
	/// explicit check — the time span overlaps even though there is nothing to read.
	/// An empty segment is **not** all-null (it has no rows at all).
	#[must_use]
	pub const fn is_all_null(&self) -> bool {
		self.stats.row_count > 0 && self.stats.null_count == self.stats.row_count
	}

	/// **Quality data skipping** (roadmap Phase 4.4): the number of **present**
	/// (non-null) rows whose timestamp falls in the inclusive range `[start, end]`.
	///
	/// This is the value-bearing population a range query over this segment will
	/// actually materialize — a window can overlap the segment's time span yet
	/// touch only null rows, in which case this returns `0` and the caller can skip
	/// the value column. Returns `0` immediately when the query is disjoint from the
	/// segment's time span (the [`overlaps_time`](Self::overlaps_time) fast path);
	/// otherwise it decodes the timestamp column once and walks the quality mask.
	#[must_use]
	pub fn present_count_in_range(&self, start: i64, end: i64) -> usize {
		if !self.overlaps_time(start, end) {
			return 0;
		}
		let timestamps = self.decode_timestamps();
		timestamps.iter().enumerate().filter(|&(row, &ts)| start <= ts && ts <= end && self.nulls.is_present(row)).count()
	}

	/// Total **realized** stored bytes: value column plus timestamp column plus the
	/// quality column (zero bytes for a dense segment, so this equals
	/// `value_bytes + timestamp_bytes` in the common case). Both columns report the
	/// codec actually written, so this is the honest on-disk payload figure.
	#[must_use]
	pub fn total_bytes(&self) -> usize {
		self.value_bytes() + self.timestamp_bytes() + self.null_bytes()
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

	/// Reconstruct the **present** logical values, densely.
	///
	/// For a dense segment this is the whole value column. For a nullable one it is
	/// only the non-null values (its length is [`row_count`](Segment::row_count)
	/// minus [`null_count`](Segment::null_count)); use
	/// [`decode_nullable`](Segment::decode_nullable) to recover them aligned to
	/// their timestamps with `None` in the gaps.
	#[must_use]
	pub fn decode_values(&self) -> Vec<BigDecimal> {
		self.values.decode()
	}

	/// Reconstruct both logical columns as parallel vectors, **assuming a dense
	/// segment** (one value per timestamp). For a segment that may carry nulls,
	/// prefer [`decode_nullable`](Segment::decode_nullable), whose value vector
	/// aligns to the timestamps. On a nullable segment the value vector this
	/// returns is shorter than the timestamps (the present values only).
	#[must_use]
	pub fn decode(&self) -> (Vec<i64>, Vec<BigDecimal>) {
		(self.decode_timestamps(), self.decode_values())
	}

	/// Reconstruct both logical columns, the value column aligned to the
	/// timestamps with `None` at every null/absent row — the exact inverse of
	/// [`build_nullable`](Segment::build_nullable).
	///
	/// The value vector always has the same length as the timestamp vector
	/// ([`row_count`](Segment::row_count)). For a dense segment every entry is
	/// `Some`.
	#[must_use]
	pub fn decode_nullable(&self) -> (Vec<i64>, Vec<Option<BigDecimal>>) {
		let timestamps = self.decode_timestamps();
		let present = self.values.decode();
		let mut values = Vec::with_capacity(self.stats.row_count);
		let mut next = 0;
		for row in 0..self.stats.row_count {
			if self.nulls.is_present(row) {
				values.push(present.get(next).cloned());
				next += 1;
			} else {
				values.push(None);
			}
		}
		(timestamps, values)
	}

	/// Seal this segment to its on-disk `.dspseg` byte frame (magic + header +
	/// column blocks + trailing CRC-32). See [`crate::dspseg::write_segment`].
	///
	/// Exact inverse of [`Segment::read_from`].
	#[must_use]
	pub fn write_to(&self) -> Vec<u8> {
		crate::dspseg::write_segment(self)
	}

	/// Seal to a `.dspseg` frame carrying a **persisted checkpoint index** over the
	/// timestamp column, so a point lookup resumes from the nearest checkpoint instead of
	/// decoding the whole column. See [`crate::dspseg::write_segment_checkpointed`].
	///
	/// Read by the ordinary [`read_from`](Self::read_from) — the codec tag is additive.
	/// Costs a little size for a large lookup win on the shape
	/// [`benefits_from_checkpoints`](Self::benefits_from_checkpoints) identifies.
	#[must_use]
	pub fn write_to_checkpointed(&self, stride: usize) -> Vec<u8> {
		crate::dspseg::write_segment_checkpointed(self, stride)
	}

	/// Whether a checkpoint index would actually help this segment's point lookups.
	///
	/// True only for a **sorted, irregular** timestamp column: an out-of-order column
	/// cannot be binary-searched (the read linear-scans), and a *regular* one already
	/// resolves in `O(1)` closed form — faster than any index — so checkpointing either
	/// would only add bytes. A factual predicate about the shape, not a policy: how many
	/// rows are worth the trade is the caller's call.
	#[must_use]
	pub fn benefits_from_checkpoints(&self) -> bool {
		self.stats.time_sorted && self.timestamps.arithmetic_stride().is_none()
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

	/// **Point lookup** (roadmap Phase 4.6 read-planner): the present value at
	/// exactly timestamp `t`, or [`None`] when no present row carries it.
	///
	/// The order signal picks the search. `t` outside `[min_ts, max_ts]` is
	/// rejected by the [`contains_timestamp`](Self::contains_timestamp) fast path
	/// without decoding. Otherwise the columns are decoded once and the matching
	/// row is found by **binary search** when the segment is
	/// [`time_sorted`](Self::is_time_sorted) (persisted per-segment; recovered by
	/// [`read_from`](Self::read_from)) and by a **linear scan** when it is
	/// out-of-order — a scan is the only correct search on unsorted timestamps.
	/// Both paths return the same answer; the flag only chooses the cheaper one.
	///
	/// When several rows share timestamp `t` (only possible in an out-of-order or
	/// duplicate-timestamp segment) the first **present** one, in row order, wins;
	/// a run of `t` whose values are all null yields [`None`].
	#[must_use]
	pub fn value_at(&self, t: i64) -> Option<BigDecimal> {
		if !self.contains_timestamp(t) {
			return None;
		}
		let (timestamps, values) = self.decode_nullable();
		point_lookup(&timestamps, &values, self.is_time_sorted(), t)
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

/// Point lookup over already-decoded, timestamp-aligned columns: the first
/// **present** value whose timestamp equals `t`, or [`None`] when no present row
/// carries `t`. Shared by [`Segment::value_at`] and
/// [`Page::value_at`](crate::page::Page::value_at) (roadmap Phase 4.6).
///
/// When `sorted` is `true` the timestamps are known monotonic non-decreasing, so
/// the run of rows equal to `t` is located by [`slice::partition_point`] (binary
/// search) and only that run is walked for the first present value; when it is
/// `false` every row is scanned, the only correct search on unsorted data. The
/// two branches are observationally identical — `sorted` must reflect the
/// segment's recorded [`SegmentStats::time_sorted`] so the cheaper search is only
/// taken when it is sound.
pub(crate) fn point_lookup(timestamps: &[i64], values: &[Option<BigDecimal>], sorted: bool, t: i64) -> Option<BigDecimal> {
	if sorted {
		let mut row = timestamps.partition_point(|&x| x < t);
		while row < timestamps.len() && timestamps[row] == t {
			if let Some(v) = &values[row] {
				return Some(v.clone());
			}
			row += 1;
		}
		None
	} else {
		timestamps.iter().zip(values).find_map(|(&ts, v)| if ts == t { v.clone() } else { None })
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

/// **Quality-aware time pruning over a set of segments** (roadmap Phase 4.4).
///
/// Like [`prune_by_time`], but for a query that needs actual **values** in the
/// inclusive range `[start, end]` (interpolation, value aggregation, gap fill).
/// Returns the indices of segments that both overlap the time range **and** carry
/// at least one present (non-null) value inside it — the ones that will yield a
/// measurement. A segment that overlaps the window but holds only null rows there
/// (in particular a fully [`all-null`](Segment::is_all_null) segment) is skipped:
/// scanning it would materialize nothing but `None`s.
///
/// This is strictly more selective than [`prune_by_time`] — every returned index
/// is also returned by `prune_by_time`, never the reverse. It decodes the
/// timestamp column of the time-overlapping segments to test the mask, so it is
/// heavier than the min/max-only [`prune_by_time`]; use it when the per-segment
/// decode is cheaper than reading value columns that turn out to be all null.
/// Order-independent.
#[must_use]
pub fn prune_present_by_time(segments: &[Segment], start: i64, end: i64) -> Vec<usize> {
	segments.iter().enumerate().filter(|(_, s)| s.present_count_in_range(start, end) > 0).map(|(i, _)| i).collect()
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
	fn value_at_binary_searches_a_sorted_segment() {
		let seg = Segment::build(&[10, 20, 30, 40, 50], &col(&["1.5", "2.5", "3.5", "4.5", "5.5"]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert!(seg.is_time_sorted());
		// An exact hit at each grid point.
		assert_eq!(seg.value_at(10), Some(BigDecimal::from_str("1.5").unwrap()));
		assert_eq!(seg.value_at(30), Some(BigDecimal::from_str("3.5").unwrap()));
		assert_eq!(seg.value_at(50), Some(BigDecimal::from_str("5.5").unwrap()));
		// Off-grid instants inside and outside the span are misses.
		assert_eq!(seg.value_at(25), None);
		assert_eq!(seg.value_at(5), None);
		assert_eq!(seg.value_at(60), None);
	}

	#[test]
	fn value_at_linear_scans_an_unsorted_segment() {
		// A dip makes the segment out-of-order, so the lookup must scan.
		let seg = Segment::build(&[10, 40, 20, 30], &col(&["1.0", "4.0", "2.0", "3.0"]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert!(!seg.is_time_sorted());
		assert_eq!(seg.value_at(40), Some(BigDecimal::from_str("4.0").unwrap()));
		assert_eq!(seg.value_at(20), Some(BigDecimal::from_str("2.0").unwrap()));
		assert_eq!(seg.value_at(25), None);
		// The same rows sorted give the same answers — the flag only picks the search.
		let sorted = Segment::build(&[10, 20, 30, 40], &col(&["1.0", "2.0", "3.0", "4.0"]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		for t in [10, 20, 30, 40, 25] {
			assert_eq!(seg.value_at(t), sorted.value_at(t), "sorted and unsorted disagree at {t}");
		}
	}

	#[test]
	fn value_at_skips_null_rows() {
		// A null at ts=20 yields None there; the surrounding present rows still hit.
		let ts = vec![10_i64, 20, 30];
		let vs = vec![Some(BigDecimal::from_str("1.5").unwrap()), None, Some(BigDecimal::from_str("3.5").unwrap())];
		let seg = Segment::build_nullable(&ts, &vs, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert_eq!(seg.value_at(10), Some(BigDecimal::from_str("1.5").unwrap()));
		assert_eq!(seg.value_at(20), None, "a null row has no present value");
		assert_eq!(seg.value_at(30), Some(BigDecimal::from_str("3.5").unwrap()));
	}

	#[test]
	fn value_at_returns_first_present_of_a_duplicate_run() {
		// Duplicate timestamps: the first present value in row order wins, even when
		// the first row of the run is null.
		let ts = vec![10_i64, 20, 20, 20, 30];
		let vs = vec![Some(BigDecimal::from_str("1.0").unwrap()), None, Some(BigDecimal::from_str("2.2").unwrap()), Some(BigDecimal::from_str("2.9").unwrap()), Some(BigDecimal::from_str("3.0").unwrap())];
		let seg = Segment::build_nullable(&ts, &vs, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert!(seg.is_time_sorted(), "equal timestamps are monotonic");
		assert_eq!(seg.value_at(20), Some(BigDecimal::from_str("2.2").unwrap()));
	}

	#[test]
	fn value_at_on_empty_segment_is_none() {
		let seg = Segment::build(&[], &[], TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert_eq!(seg.value_at(0), None);
	}

	#[test]
	fn length_mismatch_is_rejected() {
		let timestamps = vec![1_i64, 2, 3];
		let values = col(&["1.0", "2.0"]);
		let err = Segment::build(&timestamps, &values, TimeUnit::Millis, &BigDecimal::from(0)).expect_err("mismatched heights");
		assert_eq!(err, SegmentError::LengthMismatch { timestamps: 3, values: 2 });
	}

	#[test]
	fn build_sorted_accepts_ordered_and_rejects_backwards() {
		let zero = BigDecimal::from(0);
		// Ordered (with an equal-timestamp duplicate) seals like `build` does.
		let ok = Segment::build_sorted(&[10, 10, 20, 30], &col(&["1.0", "2.0", "3.0", "4.0"]), TimeUnit::Seconds, &zero).expect("ordered builds");
		assert!(ok.is_time_sorted());
		assert_eq!(ok, Segment::build(&[10, 10, 20, 30], &col(&["1.0", "2.0", "3.0", "4.0"]), TimeUnit::Seconds, &zero).expect("builds"));
		// A backwards step is rejected at the first offending row (row 2: 20 -> 15).
		let err = Segment::build_sorted(&[10, 20, 15, 30], &col(&["1.0", "2.0", "3.0", "4.0"]), TimeUnit::Seconds, &zero).expect_err("out of order");
		assert_eq!(err, SegmentError::OutOfOrder { index: 2, previous: 20, current: 15 });
		// Empty and single-row batches are vacuously ordered.
		assert!(Segment::build_sorted(&[], &[], TimeUnit::Seconds, &zero).is_ok());
		assert!(Segment::build_sorted(&[5], &col(&["1.0"]), TimeUnit::Seconds, &zero).is_ok());
	}

	#[test]
	fn build_sorted_reports_length_mismatch_before_order() {
		// A height mismatch takes priority over an order violation.
		let err = Segment::build_sorted(&[10, 5, 20], &col(&["1.0", "2.0"]), TimeUnit::Millis, &BigDecimal::from(0)).expect_err("mismatch first");
		assert_eq!(err, SegmentError::LengthMismatch { timestamps: 3, values: 2 });
	}

	#[test]
	fn build_nullable_sorted_enforces_order_over_the_dense_column() {
		let zero = BigDecimal::from(0);
		let vals = vec![Some(BigDecimal::from(1)), None, Some(BigDecimal::from(3))];
		// Dense timestamps ordered -> ok, and equal to the permissive nullable build.
		let ok = Segment::build_nullable_sorted(&[10, 20, 30], &vals, TimeUnit::Seconds, &zero).expect("ordered");
		assert_eq!(ok, Segment::build_nullable(&[10, 20, 30], &vals, TimeUnit::Seconds, &zero).expect("builds"));
		// The order check spans the null row too (row 1's timestamp regresses).
		let err = Segment::build_nullable_sorted(&[10, 5, 30], &vals, TimeUnit::Seconds, &zero).expect_err("out of order");
		assert_eq!(err, SegmentError::OutOfOrder { index: 1, previous: 10, current: 5 });
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

	fn ncol(lits: &[Option<&str>]) -> Vec<Option<BigDecimal>> {
		lits.iter().map(|o| o.map(|s| BigDecimal::from_str(s).expect("parses"))).collect()
	}

	#[test]
	fn nullable_segment_round_trips_with_gaps() {
		// Five rows, two of them null (rows 1 and 3).
		let timestamps = vec![10_i64, 20, 30, 40, 50];
		let values = ncol(&[Some("1.5"), None, Some("3.5"), None, Some("5.5")]);
		let seg = Segment::build_nullable(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert_eq!(seg.version, SEGMENT_FORMAT_VERSION);
		assert_eq!(seg.row_count(), 5);
		assert_eq!(seg.null_count(), 2);
		assert!(seg.has_nulls());
		// The value column holds only the three present values.
		assert_eq!(seg.values.len(), 3);
		assert_eq!(seg.decode_values(), col(&["1.5", "3.5", "5.5"]));
		// decode_nullable realigns them to the timestamps with None in the gaps.
		let (ts, vs) = seg.decode_nullable();
		assert_eq!(ts, timestamps);
		assert_eq!(vs, values);
	}

	#[test]
	fn nullable_stats_cover_present_values_only() {
		let timestamps = vec![1_i64, 2, 3, 4];
		// Present values 9.0 and 2.0; the nulls do not enter min/max.
		let values = ncol(&[Some("9.0"), None, Some("2.0"), None]);
		let seg = Segment::build_nullable(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert_eq!(seg.stats.null_count, 2);
		assert_eq!(seg.stats.min_value, Some(BigDecimal::from_str("2.0").unwrap()));
		assert_eq!(seg.stats.max_value, Some(BigDecimal::from_str("9.0").unwrap()));
		// Timestamps are dense — all four rows count toward the span.
		assert_eq!(seg.stats.min_ts, Some(1));
		assert_eq!(seg.stats.max_ts, Some(4));
	}

	#[test]
	fn fully_present_nullable_matches_dense_build() {
		let timestamps = vec![100_i64, 110, 120];
		let dense = col(&["1.0", "2.0", "3.0"]);
		let nullable = ncol(&[Some("1.0"), Some("2.0"), Some("3.0")]);
		let a = Segment::build(&timestamps, &dense, TimeUnit::Millis, &BigDecimal::from(0)).expect("builds");
		let b = Segment::build_nullable(&timestamps, &nullable, TimeUnit::Millis, &BigDecimal::from(0)).expect("builds");
		// A no-null nullable build is byte-for-byte the dense segment.
		assert_eq!(a, b);
		assert!(!b.has_nulls());
		assert_eq!(b.null_bytes(), 0, "a dense quality column costs nothing");
	}

	#[test]
	fn all_null_segment_has_an_empty_value_column() {
		let timestamps = vec![1_i64, 2, 3];
		let values: Vec<Option<BigDecimal>> = vec![None, None, None];
		let seg = Segment::build_nullable(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert_eq!(seg.row_count(), 3);
		assert_eq!(seg.null_count(), 3);
		assert!(seg.values.is_empty(), "no present values ⇒ empty value column");
		assert_eq!(seg.stats.min_value, None);
		let (ts, vs) = seg.decode_nullable();
		assert_eq!(ts, timestamps);
		assert_eq!(vs, values);
	}

	#[test]
	fn nullable_length_mismatch_is_rejected() {
		let timestamps = vec![1_i64, 2, 3];
		let values = ncol(&[Some("1.0"), None]);
		let err = Segment::build_nullable(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from(0)).expect_err("mismatch");
		assert_eq!(err, SegmentError::LengthMismatch { timestamps: 3, values: 2 });
	}

	#[test]
	fn nullable_total_bytes_includes_the_quality_column() {
		let timestamps: Vec<i64> = (0..16).collect();
		// One null ⇒ a sparse mask of ceil(16/8) = 2 bytes.
		let mut values: Vec<Option<BigDecimal>> = (0..16).map(|i| Some(BigDecimal::from(i))).collect();
		values[7] = None;
		let seg = Segment::build_nullable(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert_eq!(seg.null_bytes(), 2);
		assert_eq!(seg.total_bytes(), seg.value_bytes() + seg.timestamp_bytes() + seg.null_bytes());
	}

	#[test]
	fn present_count_and_all_null_classify_the_quality_column() {
		// A dense segment: every row present, none null.
		let dense = Segment::build(&[1_i64, 2, 3], &col(&["1.0", "2.0", "3.0"]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert_eq!(dense.present_count(), 3);
		assert!(!dense.is_all_null());
		// A partly-null segment: present_count excludes the nulls.
		let partial = Segment::build_nullable(&[1_i64, 2, 3, 4], &ncol(&[Some("1.0"), None, Some("3.0"), None]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert_eq!(partial.present_count(), 2);
		assert!(!partial.is_all_null());
		// An all-null segment: no present rows, classified all-null.
		let all_null = Segment::build_nullable(&[1_i64, 2, 3], &ncol(&[None, None, None]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert_eq!(all_null.present_count(), 0);
		assert!(all_null.is_all_null());
		// An empty segment is not all-null — it has no rows at all.
		let empty = Segment::build(&[], &[], TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert_eq!(empty.present_count(), 0);
		assert!(!empty.is_all_null());
	}

	#[test]
	fn present_count_in_range_counts_only_present_rows_in_window() {
		// Rows at ts 10..=50; rows 1 (ts 20) and 3 (ts 40) are null.
		let timestamps = vec![10_i64, 20, 30, 40, 50];
		let values = ncol(&[Some("1.0"), None, Some("3.0"), None, Some("5.0")]);
		let seg = Segment::build_nullable(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		// Whole span: three present values.
		assert_eq!(seg.present_count_in_range(0, 100), 3);
		// A window covering only the two null rows yields nothing.
		assert_eq!(seg.present_count_in_range(20, 40), 1, "ts 30 is the only present row in [20,40]");
		assert_eq!(seg.present_count_in_range(20, 20), 0, "ts 20 is null");
		assert_eq!(seg.present_count_in_range(40, 40), 0, "ts 40 is null");
		// A disjoint window short-circuits to zero.
		assert_eq!(seg.present_count_in_range(60, 90), 0);
		// Edge inclusivity.
		assert_eq!(seg.present_count_in_range(50, 50), 1, "ts 50 present, inclusive high edge");
		assert_eq!(seg.present_count_in_range(10, 10), 1, "ts 10 present, inclusive low edge");
	}

	#[test]
	fn prune_present_by_time_skips_all_null_and_disjoint_segments() {
		// Three segments over [0,40], [100,140], [200,240]. The middle is all-null.
		let dense = |base: i64| {
			let ts: Vec<i64> = (0..5).map(|i| base + i * 10).collect();
			let vs: Vec<BigDecimal> = (0..5).map(BigDecimal::from).collect();
			Segment::build(&ts, &vs, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds")
		};
		let all_null = {
			let ts: Vec<i64> = (0..5).map(|i| 100 + i * 10).collect();
			let vs: Vec<Option<BigDecimal>> = vec![None; 5];
			Segment::build_nullable(&ts, &vs, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds")
		};
		let segments = vec![dense(0), all_null, dense(200)];
		// A query spanning all three: plain time pruning keeps the all-null middle,
		// quality-aware pruning drops it.
		assert_eq!(prune_by_time(&segments, 0, 240), vec![0, 1, 2]);
		assert_eq!(prune_present_by_time(&segments, 0, 240), vec![0, 2]);
		// A query entirely inside the all-null segment yields nothing to read.
		assert_eq!(prune_present_by_time(&segments, 110, 130), Vec::<usize>::new());
		// Quality-aware pruning is a subset of plain time pruning here.
		assert_eq!(prune_present_by_time(&segments, 0, 40), vec![0]);
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
