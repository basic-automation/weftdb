//! Intra-segment **page** subdivision (roadmap **Phase 4.3 / 4.4**).
//!
//! Phase 4.3 sealed a whole aspect's rows into a single-block [`Segment`]: one
//! value column, one timestamp column, one set of min/max stats. That makes
//! *inter*-segment skipping work ([`prune_by_time`](crate::prune_by_time)), but a
//! bounded range query inside one large segment still has to decode the entire
//! value/timestamp column even when it wants a few hundred rows out of a million.
//!
//! This module adds the *intra*-segment layer the single-block frame deferred: a
//! [`PagedSegment`] splits its rows into fixed-height [`Page`]s, each independently
//! encoded and carrying its **own** min/max ts/value stats. A range query then
//! prunes to the handful of pages that overlap the window
//! ([`PagedSegment::prune_pages_by_time`]) and decodes only those
//! ([`PagedSegment::read_time_range`]) — the Phase-4.4 intra-segment page skipping.
//!
//! ## Relationship to [`Segment`]
//!
//! A [`Page`] is the same in-memory shape as a [`Segment`]'s columns (a typed value
//! column, a delta-of-delta timestamp column, a [`NullMask`], and [`SegmentStats`])
//! — but it is *not* a frame unit: it has no format version, magic, or per-page
//! checksum. Those live once at the [`PagedSegment`]/frame level. Reusing the same
//! column codecs means a page's `bytes_per_point` is computed by the very same
//! estimators as a [`Segment`]'s, so a paged segment and an equivalent single-block
//! one report storage cost on one ruler.
//!
//! This slice is the **in-memory model only**. The on-disk `.dspseg` frame with a
//! per-page offset table (format version 3) is the next slice; a [`PagedSegment`]
//! still round-trips through `serde` today.

use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};

use crate::{
	column::{recommend_encoding, ColumnEncoding}, nulls::NullMask, segment::{SegmentError, SegmentStats}, timestamp::{decode_delta_of_delta, encode_delta_of_delta, DeltaOfDeltaColumn, TimeUnit}
};

/// The format version of a [`PagedSegment`] frame.
///
/// A paged segment is a distinct on-disk layout from the single-block
/// [`SEGMENT_FORMAT_VERSION`](crate::SEGMENT_FORMAT_VERSION): it carries a per-page
/// offset/stats table the single-block frame has no concept of. Versioned
/// separately so a reader can branch on the layout it is handed.
///
/// - **v3** — first paged layout: fixed-height pages, each with its own min/max
///   ts/value stats and quality column, plus a segment-level rollup.
/// - **v4** — each page's timestamp block gains the self-describing codec selector
///   (fixed-width bit-packing vs varint second differences); see
///   [`SEGMENT_FORMAT_VERSION`](crate::SEGMENT_FORMAT_VERSION) v3.
pub const PAGED_SEGMENT_FORMAT_VERSION: u16 = 4;

/// The default page height when a caller does not specify one.
///
/// A balance between pruning granularity (smaller pages skip more precisely) and
/// per-page stat overhead (smaller pages cost more index entries).
pub const DEFAULT_ROWS_PER_PAGE: usize = 1_024;

/// One fixed-height block of rows within a [`PagedSegment`].
///
/// Holds its own typed value column (present values only), delta-of-delta timestamp
/// column, quality mask, and [`SegmentStats`] — the same shape a [`Segment`] binds,
/// minus the frame-level version/magic/checksum. A query prunes against a page's
/// [`stats`](Page::stats) exactly as it does a segment's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page {
	/// The typed, physically-encoded value column — present values only.
	pub values: ColumnEncoding,
	/// The integer-epoch timestamp column, dense over every row in the page.
	pub timestamps: DeltaOfDeltaColumn,
	/// The quality column: which rows in the page carry a value vs a null.
	pub nulls: NullMask,
	/// Per-page summary statistics (the intra-segment data-skipping inputs).
	pub stats: SegmentStats,
}

impl Page {
	/// Build a dense page from parallel timestamp and value columns (one value per
	/// timestamp). The value column is encoded by [`recommend_encoding`] within
	/// `value_tolerance`; the timestamps are delta-of-delta transformed under `unit`.
	fn build(timestamps: &[i64], values: &[BigDecimal], unit: TimeUnit, value_tolerance: &BigDecimal) -> Self {
		let value_col = recommend_encoding(values, value_tolerance);
		let ts_col = encode_delta_of_delta(timestamps, unit);
		let stats = SegmentStats::from_columns(timestamps, values);
		Self { values: value_col, timestamps: ts_col, nulls: NullMask::all_present(values.len()), stats }
	}

	/// Build a page from a dense timestamp column and a nullable value column — the
	/// present values are stored densely and a [`NullMask`] records which rows are
	/// present.
	fn build_nullable(timestamps: &[i64], values: &[Option<BigDecimal>], unit: TimeUnit, value_tolerance: &BigDecimal) -> Self {
		let present: Vec<bool> = values.iter().map(Option::is_some).collect();
		let nulls = NullMask::from_presence(&present);
		let present_values: Vec<BigDecimal> = values.iter().flatten().cloned().collect();
		let value_col = recommend_encoding(&present_values, value_tolerance);
		let ts_col = encode_delta_of_delta(timestamps, unit);
		let stats = SegmentStats::from_columns_nullable(timestamps, &present_values, nulls.null_count());
		Self { values: value_col, timestamps: ts_col, nulls, stats }
	}

	/// Number of rows in the page (present and null alike).
	#[must_use]
	pub const fn row_count(&self) -> usize {
		self.stats.row_count
	}

	/// `true` iff the page holds no rows.
	#[must_use]
	pub const fn is_empty(&self) -> bool {
		self.stats.row_count == 0
	}

	/// The inclusive `(min, max)` timestamp span this page covers, or [`None`] when
	/// empty — the coarse index a range query prunes against.
	#[must_use]
	pub const fn time_range(&self) -> Option<(i64, i64)> {
		match (self.stats.min_ts, self.stats.max_ts) {
			(Some(lo), Some(hi)) => Some((lo, hi)),
			_ => None,
		}
	}

	/// Whether this page *may* hold a row whose timestamp falls in the inclusive
	/// range `[start, end]`. `false` ⇒ safe to skip the page entirely.
	#[must_use]
	pub const fn overlaps_time(&self, start: i64, end: i64) -> bool {
		match self.time_range() {
			Some((lo, hi)) => lo <= end && start <= hi,
			None => false,
		}
	}

	/// Number of present (non-null) rows in the page — equal to
	/// `row_count - null_count`.
	#[must_use]
	pub const fn present_count(&self) -> usize {
		self.stats.row_count - self.stats.null_count
	}

	/// `true` iff the page holds rows but **every** value is null — a page a
	/// value-bearing query can skip entirely (it yields only `None`s).
	#[must_use]
	pub const fn is_all_null(&self) -> bool {
		self.stats.row_count > 0 && self.stats.null_count == self.stats.row_count
	}

	/// The number of **present** (non-null) rows whose timestamp falls in the
	/// inclusive range `[start, end]` — the value-bearing population a window
	/// actually materializes from this page (Phase-4.4 quality data skipping).
	/// Short-circuits to `0` on a disjoint window before decoding.
	#[must_use]
	pub fn present_count_in_range(&self, start: i64, end: i64) -> usize {
		if !self.overlaps_time(start, end) {
			return 0;
		}
		let timestamps = self.decode_timestamps();
		timestamps.iter().enumerate().filter(|&(row, &ts)| start <= ts && ts <= end && self.nulls.is_present(row)).count()
	}

	/// Estimated stored bytes of the page's three columns (value + timestamp +
	/// quality), on the same estimators a [`Segment`] uses.
	#[must_use]
	pub fn total_bytes(&self) -> usize {
		self.values.estimated_bytes() + self.timestamps.best_estimated_bytes() + self.nulls.estimated_bytes()
	}

	/// Reconstruct the page's logical timestamp column (empty when the page is
	/// empty, resolving the lone-anchor ambiguity the timestamp codec documents).
	#[must_use]
	pub fn decode_timestamps(&self) -> Vec<i64> {
		if self.stats.row_count == 0 {
			return Vec::new();
		}
		decode_delta_of_delta(&self.timestamps)
	}

	/// Reconstruct both columns, the value column aligned to the timestamps with
	/// `None` at every null row — the page-level mirror of
	/// [`Segment::decode_nullable`](crate::Segment::decode_nullable).
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
}

/// A segment subdivided into fixed-height [`Page`]s for intra-segment data
/// skipping (roadmap Phase 4.3/4.4).
///
/// Build with [`PagedSegment::build`] / [`build_nullable`](Self::build_nullable);
/// the rows are partitioned into pages of [`rows_per_page`](Self::rows_per_page)
/// (the final page may be shorter). A range query prunes to the overlapping pages
/// with [`prune_pages_by_time`](Self::prune_pages_by_time) and decodes only those
/// with [`read_time_range`](Self::read_time_range), skipping the rest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PagedSegment {
	/// The paged layout version ([`PAGED_SEGMENT_FORMAT_VERSION`] at build time).
	pub version: u16,
	/// The target page height — every page but the last has exactly this many rows.
	pub rows_per_page: usize,
	/// The pages, in row order.
	pub pages: Vec<Page>,
	/// Segment-level rollup statistics over all pages (the inter-segment
	/// data-skipping inputs — the same role [`Segment::stats`](crate::Segment) plays).
	pub stats: SegmentStats,
}

impl PagedSegment {
	/// Partition `timestamps`/`values` into pages of `rows_per_page` rows each and
	/// build a paged segment. Each page is independently encoded within
	/// `value_tolerance` under `unit`; the segment-level stats roll up the pages.
	///
	/// # Errors
	///
	/// Returns [`SegmentError::LengthMismatch`] if the columns differ in length, or
	/// [`SegmentError::EmptyPageSize`] if `rows_per_page` is zero.
	pub fn build(timestamps: &[i64], values: &[BigDecimal], unit: TimeUnit, value_tolerance: &BigDecimal, rows_per_page: usize) -> Result<Self, SegmentError> {
		if timestamps.len() != values.len() {
			return Err(SegmentError::LengthMismatch { timestamps: timestamps.len(), values: values.len() });
		}
		if rows_per_page == 0 {
			return Err(SegmentError::EmptyPageSize);
		}
		let pages: Vec<Page> = timestamps.chunks(rows_per_page).zip(values.chunks(rows_per_page)).map(|(ts, vs)| Page::build(ts, vs, unit, value_tolerance)).collect();
		let stats = SegmentStats::from_columns(timestamps, values);
		Ok(Self { version: PAGED_SEGMENT_FORMAT_VERSION, rows_per_page, pages, stats })
	}

	/// Partition a dense timestamp column and a **nullable** value column into pages
	/// — the quality-column paged path. Each page stores its present values densely
	/// with its own [`NullMask`]; the segment-level `null_count` is the sum.
	///
	/// # Errors
	///
	/// Returns [`SegmentError::LengthMismatch`] if the columns differ in length, or
	/// [`SegmentError::EmptyPageSize`] if `rows_per_page` is zero.
	pub fn build_nullable(timestamps: &[i64], values: &[Option<BigDecimal>], unit: TimeUnit, value_tolerance: &BigDecimal, rows_per_page: usize) -> Result<Self, SegmentError> {
		if timestamps.len() != values.len() {
			return Err(SegmentError::LengthMismatch { timestamps: timestamps.len(), values: values.len() });
		}
		if rows_per_page == 0 {
			return Err(SegmentError::EmptyPageSize);
		}
		let pages: Vec<Page> = timestamps.chunks(rows_per_page).zip(values.chunks(rows_per_page)).map(|(ts, vs)| Page::build_nullable(ts, vs, unit, value_tolerance)).collect();
		let present_values: Vec<BigDecimal> = values.iter().flatten().cloned().collect();
		let null_count = values.iter().filter(|v| v.is_none()).count();
		let stats = SegmentStats::from_columns_nullable(timestamps, &present_values, null_count);
		Ok(Self { version: PAGED_SEGMENT_FORMAT_VERSION, rows_per_page, pages, stats })
	}

	/// Number of pages.
	#[must_use]
	pub const fn page_count(&self) -> usize {
		self.pages.len()
	}

	/// Total number of rows across all pages.
	#[must_use]
	pub const fn row_count(&self) -> usize {
		self.stats.row_count
	}

	/// `true` iff the paged segment holds no rows.
	#[must_use]
	pub const fn is_empty(&self) -> bool {
		self.stats.row_count == 0
	}

	/// Number of null/absent rows across all pages.
	#[must_use]
	pub const fn null_count(&self) -> usize {
		self.stats.null_count
	}

	/// The inclusive `(min, max)` timestamp span of the whole segment, or [`None`]
	/// when empty.
	#[must_use]
	pub const fn time_range(&self) -> Option<(i64, i64)> {
		match (self.stats.min_ts, self.stats.max_ts) {
			(Some(lo), Some(hi)) => Some((lo, hi)),
			_ => None,
		}
	}

	/// Total estimated stored bytes — the sum over the pages' columns. Segment- and
	/// page-level stat *tables* are frame overhead accounted by the `.dspseg` layout,
	/// not here.
	#[must_use]
	pub fn total_bytes(&self) -> usize {
		self.pages.iter().map(Page::total_bytes).sum()
	}

	/// Storage cost in **bytes per point** — the north-star term, on the same ruler
	/// as [`Segment::bytes_per_point`](crate::Segment::bytes_per_point). Zero for an
	/// empty segment.
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

	/// **Intra-segment page skipping** (roadmap Phase 4.4): the indices of the pages
	/// whose `[min_ts, max_ts]` span overlaps the inclusive query range
	/// `[start, end]` — the only pages a range query must decode.
	///
	/// Every page index **not** returned is skipped without touching its columns.
	/// This is the win the per-page stats exist for: a bounded query inside a large
	/// segment reads a few pages, not the whole column. Order-independent over pages.
	#[must_use]
	pub fn prune_pages_by_time(&self, start: i64, end: i64) -> Vec<usize> {
		self.pages.iter().enumerate().filter(|(_, p)| p.overlaps_time(start, end)).map(|(i, _)| i).collect()
	}

	/// **Quality-aware intra-segment page skipping** (roadmap Phase 4.4): the
	/// indices of the pages that both overlap `[start, end]` **and** carry at least
	/// one present (non-null) value inside it — the only pages a value-bearing query
	/// (interpolation, value aggregation, gap fill) will get a measurement from.
	///
	/// The page-level analogue of
	/// [`prune_present_by_time`](crate::prune_present_by_time): strictly more
	/// selective than [`prune_pages_by_time`](Self::prune_pages_by_time) — it also
	/// drops a page that overlaps the window but holds only nulls there (in
	/// particular a fully [`all-null`](Page::is_all_null) page). Heavier than the
	/// min/max-only [`prune_pages_by_time`] (it decodes the overlapping pages'
	/// timestamp columns to test the mask), so reach for it when skipping an all-null
	/// page's value column is worth that decode.
	#[must_use]
	pub fn prune_present_pages_by_time(&self, start: i64, end: i64) -> Vec<usize> {
		self.pages.iter().enumerate().filter(|(_, p)| p.present_count_in_range(start, end) > 0).map(|(i, _)| i).collect()
	}

	/// The number of **present** (non-null) rows across the whole segment whose
	/// timestamp falls in the inclusive range `[start, end]` — summed over the pages
	/// that overlap the window (non-overlapping pages contribute `0` without
	/// decoding). The value-bearing population a windowed query materializes.
	#[must_use]
	pub fn present_count_in_range(&self, start: i64, end: i64) -> usize {
		self.pages.iter().map(|p| p.present_count_in_range(start, end)).sum()
	}

	/// Reconstruct **all** rows as parallel `(timestamps, values)` vectors with a
	/// `None` at every null row, concatenating the pages in order — the exact
	/// inverse of [`build`](Self::build) / [`build_nullable`](Self::build_nullable).
	#[must_use]
	pub fn decode_nullable(&self) -> (Vec<i64>, Vec<Option<BigDecimal>>) {
		let mut timestamps = Vec::with_capacity(self.stats.row_count);
		let mut values = Vec::with_capacity(self.stats.row_count);
		for page in &self.pages {
			let (ts, vs) = page.decode_nullable();
			timestamps.extend(ts);
			values.extend(vs);
		}
		(timestamps, values)
	}

	/// Seal this paged segment to its on-disk `.dspseg` byte frame (format version
	/// 3): magic + header + per-page index + page blocks + trailing CRC-32. See
	/// [`crate::dspseg::write_paged_segment`].
	///
	/// Exact inverse of [`PagedSegment::read_from`].
	#[must_use]
	pub fn write_to(&self) -> Vec<u8> {
		crate::dspseg::write_paged_segment(self)
	}

	/// Read a paged segment back from a `.dspseg` byte frame, verifying its checksum.
	///
	/// Exact inverse of [`PagedSegment::write_to`]. See
	/// [`crate::dspseg::read_paged_segment`].
	///
	/// # Errors
	///
	/// Propagates [`crate::dspseg::DspSegError`] for a corrupt, truncated, or
	/// unrecognised frame (checksum mismatch, bad magic, wrong/unsupported version).
	pub fn read_from(bytes: &[u8]) -> Result<Self, crate::dspseg::DspSegError> {
		crate::dspseg::read_paged_segment(bytes)
	}

	/// Decode only the rows whose timestamp falls in the inclusive range
	/// `[start, end]`, **skipping pages** that do not overlap it (the realized
	/// page-skipping read). Returns parallel `(timestamps, values)` vectors with a
	/// `None` at every null row, in row order.
	///
	/// Equivalent to filtering [`decode_nullable`](Self::decode_nullable) by the
	/// window, but it only decodes the overlapping pages — the columns of skipped
	/// pages are never reconstructed.
	#[must_use]
	pub fn read_time_range(&self, start: i64, end: i64) -> (Vec<i64>, Vec<Option<BigDecimal>>) {
		let mut timestamps = Vec::new();
		let mut values = Vec::new();
		for page_idx in self.prune_pages_by_time(start, end) {
			let (ts, vs) = self.pages[page_idx].decode_nullable();
			for (t, v) in ts.into_iter().zip(vs) {
				if start <= t && t <= end {
					timestamps.push(t);
					values.push(v);
				}
			}
		}
		(timestamps, values)
	}
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use super::*;

	fn col(lits: &[&str]) -> Vec<BigDecimal> {
		lits.iter().map(|s| BigDecimal::from_str(s).expect("parses")).collect()
	}

	fn ncol(lits: &[Option<&str>]) -> Vec<Option<BigDecimal>> {
		lits.iter().map(|o| o.map(|s| BigDecimal::from_str(s).expect("parses"))).collect()
	}

	#[test]
	fn partitions_into_fixed_height_pages() {
		// 10 rows, 4 per page ⇒ pages of 4, 4, 2.
		let timestamps: Vec<i64> = (0..10).map(|i| 100 + i * 10).collect();
		let values: Vec<BigDecimal> = (0..10).map(BigDecimal::from).collect();
		let seg = PagedSegment::build(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from(0), 4).expect("builds");
		assert_eq!(seg.version, PAGED_SEGMENT_FORMAT_VERSION);
		assert_eq!(seg.page_count(), 3);
		assert_eq!(seg.pages[0].row_count(), 4);
		assert_eq!(seg.pages[1].row_count(), 4);
		assert_eq!(seg.pages[2].row_count(), 2);
		assert_eq!(seg.row_count(), 10);
		// Each page carries its own time span.
		assert_eq!(seg.pages[0].time_range(), Some((100, 130)));
		assert_eq!(seg.pages[1].time_range(), Some((140, 170)));
		assert_eq!(seg.pages[2].time_range(), Some((180, 190)));
		// Segment-level rollup spans the whole thing.
		assert_eq!(seg.time_range(), Some((100, 190)));
	}

	#[test]
	fn round_trips_all_rows_through_pages() {
		let timestamps: Vec<i64> = (0..1_000).map(|i| 1_000 + i * 5).collect();
		let values: Vec<BigDecimal> = (0..1_000).map(BigDecimal::from).collect();
		let seg = PagedSegment::build(&timestamps, &values, TimeUnit::Micros, &BigDecimal::from(0), 128).expect("builds");
		assert!(seg.page_count() > 1, "1000 rows / 128 per page is several pages");
		let (ts, vs) = seg.decode_nullable();
		assert_eq!(ts, timestamps);
		let expected: Vec<Option<BigDecimal>> = values.iter().cloned().map(Some).collect();
		assert_eq!(vs, expected);
	}

	#[test]
	fn page_pruning_selects_only_overlapping_pages() {
		// 12 rows at ts 0,10,..,110; 4 per page ⇒ pages [0,30], [40,70], [80,110].
		let timestamps: Vec<i64> = (0..12).map(|i| i * 10).collect();
		let values: Vec<BigDecimal> = (0..12).map(BigDecimal::from).collect();
		let seg = PagedSegment::build(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from(0), 4).expect("builds");
		assert_eq!(seg.page_count(), 3);
		// A query inside the middle page touches only it.
		assert_eq!(seg.prune_pages_by_time(45, 65), vec![1]);
		// A query straddling pages 0 and 1.
		assert_eq!(seg.prune_pages_by_time(25, 45), vec![0, 1]);
		// A query covering everything reads all pages.
		assert_eq!(seg.prune_pages_by_time(0, 110), vec![0, 1, 2]);
		// A query past the end reads nothing.
		assert_eq!(seg.prune_pages_by_time(200, 300), Vec::<usize>::new());
	}

	#[test]
	fn read_time_range_skips_pages_and_filters_rows() {
		let timestamps: Vec<i64> = (0..12).map(|i| i * 10).collect();
		let values: Vec<BigDecimal> = (0..12).map(BigDecimal::from).collect();
		let seg = PagedSegment::build(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from(0), 4).expect("builds");
		// Window [35, 75]: spans pages 0 (ts 30 excluded), 1 (40,50,60,70), 2 (80 excluded).
		let (ts, vs) = seg.read_time_range(35, 75);
		assert_eq!(ts, vec![40, 50, 60, 70]);
		let expected = vec![Some(BigDecimal::from(4)), Some(BigDecimal::from(5)), Some(BigDecimal::from(6)), Some(BigDecimal::from(7))];
		assert_eq!(vs, expected);
		// read_time_range over the whole span equals decode_nullable.
		let whole = seg.read_time_range(i64::MIN, i64::MAX);
		assert_eq!(whole, seg.decode_nullable());
	}

	#[test]
	fn nullable_paged_segment_round_trips_with_gaps() {
		// 6 rows, 3 per page; nulls at rows 1 and 4 (one per page).
		let timestamps = vec![10_i64, 20, 30, 40, 50, 60];
		let values = ncol(&[Some("1.0"), None, Some("3.0"), Some("4.0"), None, Some("6.0")]);
		let seg = PagedSegment::build_nullable(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from(0), 3).expect("builds");
		assert_eq!(seg.page_count(), 2);
		assert_eq!(seg.null_count(), 2);
		assert_eq!(seg.pages[0].stats.null_count, 1);
		assert_eq!(seg.pages[1].stats.null_count, 1);
		let (ts, vs) = seg.decode_nullable();
		assert_eq!(ts, timestamps);
		assert_eq!(vs, values);
	}

	#[test]
	fn nullable_rollup_stats_cover_present_values_only() {
		let timestamps = vec![1_i64, 2, 3, 4, 5];
		let values = ncol(&[Some("9.0"), None, Some("2.0"), None, Some("7.0")]);
		let seg = PagedSegment::build_nullable(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from(0), 2).expect("builds");
		assert_eq!(seg.null_count(), 2);
		assert_eq!(seg.stats.min_value, Some(BigDecimal::from_str("2.0").unwrap()));
		assert_eq!(seg.stats.max_value, Some(BigDecimal::from_str("9.0").unwrap()));
		assert_eq!(seg.stats.min_ts, Some(1));
		assert_eq!(seg.stats.max_ts, Some(5));
	}

	#[test]
	fn single_page_when_rows_fit() {
		let timestamps: Vec<i64> = (0..5).collect();
		let values = col(&["1.0", "2.0", "3.0", "4.0", "5.0"]);
		let seg = PagedSegment::build(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from(0), 1_000).expect("builds");
		assert_eq!(seg.page_count(), 1);
		assert_eq!(seg.pages[0].row_count(), 5);
	}

	#[test]
	fn empty_paged_segment_has_no_pages() {
		let seg = PagedSegment::build(&[], &[], TimeUnit::Seconds, &BigDecimal::from(0), 64).expect("builds");
		assert_eq!(seg.page_count(), 0);
		assert!(seg.is_empty());
		assert_eq!(seg.time_range(), None);
		assert!((seg.bytes_per_point() - 0.0).abs() < f64::EPSILON);
		assert_eq!(seg.prune_pages_by_time(i64::MIN, i64::MAX), Vec::<usize>::new());
		let (ts, vs) = seg.decode_nullable();
		assert!(ts.is_empty());
		assert!(vs.is_empty());
	}

	#[test]
	fn zero_page_size_is_rejected() {
		let err = PagedSegment::build(&[1_i64], &col(&["1.0"]), TimeUnit::Seconds, &BigDecimal::from(0), 0).expect_err("zero page size");
		assert_eq!(err, SegmentError::EmptyPageSize);
	}

	#[test]
	fn length_mismatch_is_rejected() {
		let err = PagedSegment::build(&[1_i64, 2, 3], &col(&["1.0", "2.0"]), TimeUnit::Seconds, &BigDecimal::from(0), 4).expect_err("mismatch");
		assert_eq!(err, SegmentError::LengthMismatch { timestamps: 3, values: 2 });
	}

	#[test]
	fn bytes_per_point_matches_a_single_block_segment() {
		// A paged segment and an equivalent single-block one estimate storage on the
		// same per-column ruler; per-point cost differs only by page granularity, not
		// by a different estimator. With one page they coincide exactly.
		let timestamps: Vec<i64> = (0..500).map(|i| i * 7).collect();
		let values: Vec<BigDecimal> = (0..500).map(BigDecimal::from).collect();
		let paged = PagedSegment::build(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from(0), 10_000).expect("builds");
		let single = crate::Segment::build(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert_eq!(paged.page_count(), 1);
		assert_eq!(paged.total_bytes(), single.total_bytes());
		assert!((paged.bytes_per_point() - single.bytes_per_point()).abs() < f64::EPSILON);
	}

	#[test]
	fn page_present_count_and_all_null_classify_the_quality_column() {
		// A paged segment, 4 rows per page; page 1 is entirely null.
		let timestamps: Vec<i64> = (0..12).map(|i| i * 10).collect();
		let values = ncol(&[Some("0"), Some("1"), Some("2"), Some("3"), None, None, None, None, Some("8"), Some("9"), None, Some("11")]);
		let seg = PagedSegment::build_nullable(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from(0), 4).expect("builds");
		assert_eq!(seg.page_count(), 3);
		assert_eq!(seg.pages[0].present_count(), 4);
		assert!(!seg.pages[0].is_all_null());
		assert!(seg.pages[1].is_all_null(), "page 1 is entirely null");
		assert_eq!(seg.pages[1].present_count(), 0);
		assert_eq!(seg.pages[2].present_count(), 3);
		assert!(!seg.pages[2].is_all_null());
	}

	#[test]
	fn prune_present_pages_skips_all_null_pages() {
		// Pages [0,30], [40,70] (all-null), [80,110].
		let timestamps: Vec<i64> = (0..12).map(|i| i * 10).collect();
		let values = ncol(&[Some("0"), Some("1"), Some("2"), Some("3"), None, None, None, None, Some("8"), Some("9"), Some("10"), Some("11")]);
		let seg = PagedSegment::build_nullable(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from(0), 4).expect("builds");
		// Plain page pruning keeps the all-null middle page; quality-aware drops it.
		assert_eq!(seg.prune_pages_by_time(0, 110), vec![0, 1, 2]);
		assert_eq!(seg.prune_present_pages_by_time(0, 110), vec![0, 2]);
		// A window entirely inside the all-null page yields nothing to read.
		assert_eq!(seg.prune_present_pages_by_time(45, 65), Vec::<usize>::new());
		// Quality-aware pruning is a subset of plain pruning.
		assert_eq!(seg.prune_present_pages_by_time(0, 30), vec![0]);
	}

	#[test]
	fn present_count_in_range_sums_across_pages() {
		let timestamps: Vec<i64> = (0..12).map(|i| i * 10).collect();
		let values = ncol(&[Some("0"), None, Some("2"), Some("3"), None, None, None, None, Some("8"), Some("9"), None, Some("11")]);
		let seg = PagedSegment::build_nullable(&timestamps, &values, TimeUnit::Seconds, &BigDecimal::from(0), 4).expect("builds");
		// Whole span: present values at ts 0,20,30,80,90,110 ⇒ 6.
		assert_eq!(seg.present_count_in_range(i64::MIN, i64::MAX), 6);
		// Window [20, 90] spans pages 0 (20,30), 1 (none), 2 (80,90) ⇒ 4.
		assert_eq!(seg.present_count_in_range(20, 90), 4);
		// A window inside the all-null page is zero.
		assert_eq!(seg.present_count_in_range(40, 70), 0);
	}

	#[test]
	fn paged_segment_serde_round_trips() {
		let timestamps: Vec<i64> = (0..20).map(|i| i * 100).collect();
		let values: Vec<BigDecimal> = (0..20).map(BigDecimal::from).collect();
		let seg = PagedSegment::build(&timestamps, &values, TimeUnit::Millis, &BigDecimal::from(0), 8).expect("builds");
		let json = serde_json::to_string(&seg).expect("serializes");
		let back: PagedSegment = serde_json::from_str(&json).expect("deserializes");
		assert_eq!(seg, back);
	}
}
