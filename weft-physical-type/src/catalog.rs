//! Segment-index catalog model (roadmap **Phase 4.3**, control-plane slice).
//!
//! Phases 4.1–4.3 produced sealed `.weftseg` segments — typed columns, per-segment
//! min/max stats, an on-disk frame. A long-lived aspect is a *sequence* of those
//! segments, and a bounded range query should open only the few that overlap it.
//! The in-segment pruning helpers ([`Segment::prune_by_time`](crate::prune_by_time))
//! already answer "which of *these loaded* segments overlap?" — but loading every
//! segment just to read its stats defeats the purpose. The control plane needs a
//! **lightweight index row** per sealed segment that it can keep resident (and, the
//! next slice, persist in libSQL) so it prunes *before* touching any `.weftseg` byte.
//!
//! This module is that row and its in-memory collection, kept vendor-neutral:
//!
//! - [`SegmentDescriptor`] — the metadata the roadmap names for `segment_index`:
//!   `(min_ts, max_ts, min_value, max_value, row_count, null_count, byte length,
//!   path)` plus the segment's identity, physical encoding, timestamp unit, and
//!   format version. Derived directly from a sealed [`Segment`] or [`PagedSegment`]
//!   ([`SegmentDescriptor::of_segment`] / [`SegmentDescriptor::of_paged_segment`]),
//!   so the index can never disagree with the segment it describes.
//! - [`SegmentIndex`] — an ordered set of descriptors answering the same
//!   time/value pruning the per-segment helpers do, but over the *descriptors*
//!   alone: [`SegmentIndex::prune_by_time`] returns the descriptors a query must
//!   open, every other one skipped on its min/max stats without a single column
//!   byte read.
//!
//! The libSQL persistence that turns this resident index into a durable
//! `segment_index.db` table is the next Phase-4.3 slice; it stores exactly these
//! fields (the `BigDecimal` value bounds as their plain-text form, per hard
//! constraint #4 — no silent float downcast even in the catalog). This pure model
//! is what it round-trips through.

use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};

use crate::{page::PagedSegment, segment::Segment, timestamp::TimeUnit, PhysicalType};

/// One `segment_index` row: the resident metadata describing a single sealed
/// segment, enough to prune a query against it without opening its `.weftseg`.
///
/// Build one from a sealed segment with [`SegmentDescriptor::of_segment`] or
/// [`SegmentDescriptor::of_paged_segment`]; the caller supplies the identity
/// (`id`, `path`) and the realized on-disk `byte_len` (the segment knows its
/// estimated column bytes, not the exact framed size — that is what the seal
/// returns), and the descriptor copies the pruning stats verbatim from the
/// segment's own [`SegmentStats`](crate::SegmentStats).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentDescriptor {
	/// Stable identifier of the segment within its aspect (a monotonically
	/// assigned sequence number, in practice). Identity for the control plane.
	pub id: u64,
	/// Where the sealed `.weftseg` frame lives — the locator a pruned query opens.
	pub path: String,
	/// The segment-frame format version the segment was sealed under
	/// ([`SEGMENT_FORMAT_VERSION`](crate::SEGMENT_FORMAT_VERSION) for a single-block
	/// [`Segment`], [`PAGED_SEGMENT_FORMAT_VERSION`](crate::PAGED_SEGMENT_FORMAT_VERSION)
	/// for a [`PagedSegment`]).
	pub format_version: u16,
	/// The physical value encoding the segment stores its values under, or [`None`]
	/// for an empty paged segment that holds no page to read it from. (A single
	/// [`Segment`] always knows its encoding, even when empty.)
	pub physical_type: Option<PhysicalType>,
	/// The epoch resolution the timestamp column is stored in, or [`None`] for an
	/// empty paged segment.
	pub time_unit: Option<TimeUnit>,
	/// Total rows in the segment, present and null alike.
	pub row_count: usize,
	/// Null/absent rows in the segment (rows with a timestamp but no value).
	pub null_count: usize,
	/// Whether the segment's timestamps are monotonic non-decreasing — `true`
	/// admits a binary search within the segment for a point lookup, `false` flags
	/// out-of-order ingest.
	pub time_sorted: bool,
	/// Smallest timestamp in the segment, or [`None`] when empty — a time-pruning input.
	pub min_ts: Option<i64>,
	/// Largest timestamp in the segment, or [`None`] when empty — a time-pruning input.
	pub max_ts: Option<i64>,
	/// Smallest logical value (over present rows only), or [`None`] when the segment
	/// is empty or all-null — a value-pruning input.
	pub min_value: Option<BigDecimal>,
	/// Largest logical value (over present rows only), or [`None`] when the segment
	/// is empty or all-null — a value-pruning input.
	pub max_value: Option<BigDecimal>,
	/// The realized on-disk size of the sealed `.weftseg` frame, in bytes — the
	/// storage-accounting input (the north-star bytes/point numerator).
	pub byte_len: u64,
}

impl SegmentDescriptor {
	/// Build a descriptor for a sealed single-block [`Segment`].
	///
	/// `byte_len` is the realized framed size — typically `segment.write_to().len()`
	/// — which the descriptor records exactly rather than re-estimating, so the
	/// catalog's storage accounting reflects what is actually on disk.
	#[must_use]
	pub fn of_segment(id: u64, path: impl Into<String>, byte_len: u64, segment: &Segment) -> Self {
		let stats = &segment.stats;
		Self { id, path: path.into(), format_version: segment.version, physical_type: Some(segment.physical_type()), time_unit: Some(segment.time_unit()), row_count: stats.row_count, null_count: stats.null_count, time_sorted: stats.time_sorted, min_ts: stats.min_ts, max_ts: stats.max_ts, min_value: stats.min_value.clone(), max_value: stats.max_value.clone(), byte_len }
	}

	/// Build a descriptor for a sealed [`PagedSegment`].
	///
	/// The physical type / timestamp unit are read from the first page (every page
	/// of a schema-declared seal carries the same declared encoding); an empty
	/// paged segment has no page, so both are [`None`]. The pruning stats come from
	/// the segment-level rollup.
	#[must_use]
	pub fn of_paged_segment(id: u64, path: impl Into<String>, byte_len: u64, segment: &PagedSegment) -> Self {
		let stats = &segment.stats;
		let first = segment.pages.first();
		Self { id, path: path.into(), format_version: segment.version, physical_type: first.map(|p| p.values.physical_type), time_unit: first.map(|p| p.timestamps.unit), row_count: stats.row_count, null_count: stats.null_count, time_sorted: stats.time_sorted, min_ts: stats.min_ts, max_ts: stats.max_ts, min_value: stats.min_value.clone(), max_value: stats.max_value.clone(), byte_len }
	}

	/// The inclusive `(min, max)` timestamp span this segment covers, or [`None`]
	/// when empty.
	#[must_use]
	pub const fn time_range(&self) -> Option<(i64, i64)> {
		match (self.min_ts, self.max_ts) {
			(Some(lo), Some(hi)) => Some((lo, hi)),
			_ => None,
		}
	}

	/// The inclusive `(min, max)` value span this segment covers, or [`None`] when
	/// empty or all-null.
	#[must_use]
	pub fn value_range(&self) -> Option<(BigDecimal, BigDecimal)> {
		match (&self.min_value, &self.max_value) {
			(Some(lo), Some(hi)) => Some((lo.clone(), hi.clone())),
			_ => None,
		}
	}

	/// Number of present (non-null) rows — `row_count - null_count`.
	#[must_use]
	pub const fn present_count(&self) -> usize {
		self.row_count - self.null_count
	}

	/// `true` iff the segment holds rows but every value is null — a segment a
	/// value-bearing query can skip entirely (mirrors
	/// [`Segment::is_all_null`](crate::Segment::is_all_null)).
	#[must_use]
	pub const fn is_all_null(&self) -> bool {
		self.row_count > 0 && self.null_count == self.row_count
	}

	/// Storage cost in **bytes per point** for this sealed segment — the realized
	/// frame size over the row count. Unlike
	/// [`Segment::bytes_per_point`](crate::Segment::bytes_per_point) (which divides
	/// the *estimated column* bytes), this divides the *whole framed* size, so it
	/// includes the segment's header/index/checksum overhead. Zero for an empty
	/// segment.
	#[must_use]
	pub fn bytes_per_point(&self) -> f64 {
		if self.row_count == 0 {
			return 0.0;
		}
		#[allow(clippy::cast_precision_loss)]
		let n = self.row_count as f64;
		#[allow(clippy::cast_precision_loss)]
		let total = self.byte_len as f64;
		total / n
	}

	/// **Data skipping** (roadmap Phase 4.4): whether this segment *may* hold a row
	/// in the inclusive time range `[start, end]`. `false` ⇒ safely skipped without
	/// opening the `.weftseg`. Mirrors [`Segment::overlaps_time`](crate::Segment::overlaps_time).
	#[must_use]
	pub const fn overlaps_time(&self, start: i64, end: i64) -> bool {
		match self.time_range() {
			Some((lo, hi)) => lo <= end && start <= hi,
			None => false,
		}
	}

	/// Single-instant shorthand of [`overlaps_time`](Self::overlaps_time): whether
	/// `ts` lies within `[min_ts, max_ts]`.
	#[must_use]
	pub const fn contains_timestamp(&self, ts: i64) -> bool {
		self.overlaps_time(ts, ts)
	}

	/// **Value data skipping**: whether this segment *may* hold a value in the
	/// inclusive range `[lo, hi]`. `false` ⇒ safely skipped. Mirrors
	/// [`Segment::may_contain_value`](crate::Segment::may_contain_value).
	#[must_use]
	pub fn may_contain_value(&self, lo: &BigDecimal, hi: &BigDecimal) -> bool {
		self.value_range().is_some_and(|(min, max)| &min <= hi && lo <= &max)
	}
}

/// The resident `segment_index` for one aspect: an ordered set of
/// [`SegmentDescriptor`]s that prunes a query down to the segments it must open.
///
/// This is the in-memory shape of what the control plane keeps (and the next
/// slice persists in libSQL). Pushing a descriptor when a segment is sealed and
/// pruning on read is the whole query-planning win the per-segment min/max stats
/// exist for: a bounded range query consults the index — never the `.weftseg`
/// files — to decide which few segments overlap.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentIndex {
	/// The descriptors, in insertion (typically seal) order.
	descriptors: Vec<SegmentDescriptor>,
}

impl SegmentIndex {
	/// An empty index.
	#[must_use]
	pub const fn new() -> Self {
		Self { descriptors: Vec::new() }
	}

	/// Record a freshly sealed segment's descriptor.
	pub fn push(&mut self, descriptor: SegmentDescriptor) {
		self.descriptors.push(descriptor);
	}

	/// Number of indexed segments.
	#[must_use]
	pub const fn len(&self) -> usize {
		self.descriptors.len()
	}

	/// `true` iff the index holds no segments.
	#[must_use]
	pub const fn is_empty(&self) -> bool {
		self.descriptors.is_empty()
	}

	/// All descriptors, in insertion order.
	#[must_use]
	pub fn descriptors(&self) -> &[SegmentDescriptor] {
		&self.descriptors
	}

	/// Iterate the descriptors in insertion order.
	pub fn iter(&self) -> std::slice::Iter<'_, SegmentDescriptor> {
		self.descriptors.iter()
	}
}

impl<'a> IntoIterator for &'a SegmentIndex {
	type IntoIter = std::slice::Iter<'a, SegmentDescriptor>;
	type Item = &'a SegmentDescriptor;

	fn into_iter(self) -> Self::IntoIter {
		self.descriptors.iter()
	}
}

impl SegmentIndex {
	/// Total rows across every indexed segment.
	#[must_use]
	pub fn total_rows(&self) -> u64 {
		self.descriptors.iter().map(|d| d.row_count as u64).sum()
	}

	/// Total on-disk bytes across every indexed segment.
	#[must_use]
	pub fn total_bytes(&self) -> u64 {
		self.descriptors.iter().map(|d| d.byte_len).sum()
	}

	/// The number of indexed segments whose timestamps are **not** monotonic
	/// non-decreasing ([`SegmentDescriptor::time_sorted`] is `false`).
	///
	/// An order-health signal for read planning and operators: an out-of-order
	/// segment cannot be binary-searched for a point lookup and forces a linear scan
	/// (roadmap Phase 4.6), so a growing count predicts rising read-scan cost. Zero
	/// means every segment admits ordered access. A caller enforcing order at ingest
	/// (`require_sorted`) keeps this at zero by construction.
	#[must_use]
	pub fn unsorted_count(&self) -> usize {
		self.descriptors.iter().filter(|d| !d.time_sorted).count()
	}

	/// The indexed segments whose time span **overlaps at least one other segment's**
	/// span — the **cross-segment** out-of-order signal (roadmap Phase 4.6),
	/// complementing the *intra*-segment [`unsorted_count`](SegmentIndex::unsorted_count).
	///
	/// A segment can be perfectly sorted internally (so it never appears in
	/// `unsorted_count`) yet still cover a time range that a later-sealed segment
	/// re-enters — late data landing inside an already-covered window. Those
	/// overlapping segments are exactly the cross-segment reconciliation candidates:
	/// the split-not-rewrite merge (see [`SplitPolicy`](crate::SplitPolicy)) operates
	/// on a segment whose window a late batch overlaps. This returns the involved
	/// descriptors in **index (seal) order**.
	///
	/// Two segments overlap when their inclusive `[min_ts, max_ts]` spans intersect.
	/// Empty segments (no time span) never overlap. Runs in `O(n log n)`: sort the
	/// spans by `min_ts`, then a segment is involved iff it starts at or before an
	/// earlier segment's end (`overlaps a predecessor`) or the next-starting segment
	/// begins at or before its end (`is overlapped by a successor`).
	#[must_use]
	pub fn overlapping_segments(&self) -> Vec<&SegmentDescriptor> {
		// (original index, min_ts, max_ts) for every non-empty segment.
		let mut spans: Vec<(usize, i64, i64)> = self.descriptors.iter().enumerate().filter_map(|(i, d)| d.time_range().map(|(lo, hi)| (i, lo, hi))).collect();
		if spans.len() < 2 {
			return Vec::new();
		}
		spans.sort_by_key(|s| (s.1, s.2));
		let mut flagged = vec![false; spans.len()];
		// Overlaps a predecessor: this span starts at or before the running max end.
		let mut running_max_hi = i64::MIN;
		for (k, &(_, lo, hi)) in spans.iter().enumerate() {
			if k > 0 && lo <= running_max_hi {
				flagged[k] = true;
			}
			if hi > running_max_hi {
				running_max_hi = hi;
			}
		}
		// Overlapped by a successor: the next-starting span (smallest later min_ts)
		// begins at or before this span's end.
		for (k, w) in spans.windows(2).enumerate() {
			if w[1].1 <= w[0].2 {
				flagged[k] = true;
			}
		}
		// Emit the involved descriptors in original index order.
		let mut positions: Vec<usize> = spans.iter().zip(&flagged).filter_map(|(&(orig, _, _), &f)| f.then_some(orig)).collect();
		positions.sort_unstable();
		positions.into_iter().map(|i| &self.descriptors[i]).collect()
	}

	/// The number of indexed segments that overlap at least one other in time — the
	/// count form of [`overlapping_segments`](SegmentIndex::overlapping_segments), the
	/// cross-segment order-health signal. Zero when every segment covers a disjoint
	/// time window.
	#[must_use]
	pub fn overlapping_count(&self) -> usize {
		self.overlapping_segments().len()
	}

	/// Aspect-wide storage cost in **bytes per point**: the total framed bytes over
	/// the total rows. Zero when the index holds no rows.
	#[must_use]
	pub fn bytes_per_point(&self) -> f64 {
		let rows = self.total_rows();
		if rows == 0 {
			return 0.0;
		}
		#[allow(clippy::cast_precision_loss)]
		let n = rows as f64;
		#[allow(clippy::cast_precision_loss)]
		let total = self.total_bytes() as f64;
		total / n
	}

	/// The inclusive `(min, max)` timestamp span covered by the whole index, or
	/// [`None`] when it holds no non-empty segment — the coarsest data-skipping
	/// bound (a query disjoint from this can skip the aspect entirely).
	#[must_use]
	pub fn time_range(&self) -> Option<(i64, i64)> {
		let mut span: Option<(i64, i64)> = None;
		for (lo, hi) in self.descriptors.iter().filter_map(SegmentDescriptor::time_range) {
			span = Some(match span {
				Some((slo, shi)) => (slo.min(lo), shi.max(hi)),
				None => (lo, hi),
			});
		}
		span
	}

	/// **Data skipping over the index** (roadmap Phase 4.4): the descriptors whose
	/// segments may hold a row in the inclusive time range `[start, end]` — the ones
	/// a query must open. Every descriptor not returned is a segment safely skipped
	/// on its min/max stats alone, without opening its `.weftseg`. Insertion order is
	/// preserved.
	#[must_use]
	pub fn prune_by_time(&self, start: i64, end: i64) -> Vec<&SegmentDescriptor> {
		self.descriptors.iter().filter(|d| d.overlaps_time(start, end)).collect()
	}

	/// **Value data skipping over the index**: the descriptors whose segments may
	/// hold a value in the inclusive range `[lo, hi]`. The value-column mirror of
	/// [`prune_by_time`](Self::prune_by_time).
	#[must_use]
	pub fn prune_by_value(&self, lo: &BigDecimal, hi: &BigDecimal) -> Vec<&SegmentDescriptor> {
		self.descriptors.iter().filter(|d| d.may_contain_value(lo, hi)).collect()
	}

	/// **Quality-aware time pruning over the index**: like
	/// [`prune_by_time`](Self::prune_by_time), but for a value-bearing query — it
	/// additionally drops any segment that overlaps the window but is fully
	/// [`all-null`](SegmentDescriptor::is_all_null) (it would yield only `None`s).
	///
	/// This prunes only on the resident stats (`row_count`/`null_count` and the time
	/// span); it cannot tell whether the *present* rows fall inside the window
	/// (that needs the segment's timestamp column, the heavier
	/// [`prune_present_by_time`](crate::prune_present_by_time)). So it skips
	/// guaranteed-empty (all-null) segments cheaply and leaves the finer check to
	/// the opened segment.
	#[must_use]
	pub fn prune_present_by_time(&self, start: i64, end: i64) -> Vec<&SegmentDescriptor> {
		self.descriptors.iter().filter(|d| d.overlaps_time(start, end) && !d.is_all_null()).collect()
	}
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use super::*;

	fn ncol(lits: &[Option<&str>]) -> Vec<Option<BigDecimal>> {
		lits.iter().map(|o| o.map(|s| BigDecimal::from_str(s).expect("parses"))).collect()
	}

	/// A sealed single-block segment over timestamps `[base, base+90]`, values 0..10.
	fn sealed(base: i64) -> (Segment, u64) {
		let ts: Vec<i64> = (0..10).map(|i| base + i * 10).collect();
		let vs: Vec<BigDecimal> = (0..10).map(BigDecimal::from).collect();
		let seg = Segment::build(&ts, &vs, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		let len = seg.write_to().len() as u64;
		(seg, len)
	}

	#[test]
	fn descriptor_copies_segment_stats_verbatim() {
		let (seg, len) = sealed(100);
		let d = SegmentDescriptor::of_segment(7, "segments/7.weftseg", len, &seg);
		assert_eq!(d.id, 7);
		assert_eq!(d.path, "segments/7.weftseg");
		assert_eq!(d.format_version, seg.version);
		assert_eq!(d.physical_type, Some(seg.physical_type()));
		assert_eq!(d.time_unit, Some(seg.time_unit()));
		assert_eq!(d.row_count, 10);
		assert_eq!(d.null_count, 0);
		assert_eq!(d.min_ts, Some(100));
		assert_eq!(d.max_ts, Some(190));
		assert_eq!(d.min_value, Some(BigDecimal::from(0)));
		assert_eq!(d.max_value, Some(BigDecimal::from(9)));
		assert_eq!(d.byte_len, len);
		assert!(d.byte_len > 0);
	}

	#[test]
	fn descriptor_pruning_mirrors_the_segment() {
		let (seg, len) = sealed(100);
		let d = SegmentDescriptor::of_segment(0, "s.weftseg", len, &seg);
		// Time pruning matches Segment::overlaps_time.
		assert_eq!(d.time_range(), seg.time_range());
		assert!(d.overlaps_time(120, 130));
		assert!(!d.overlaps_time(0, 99));
		assert!(d.contains_timestamp(100));
		assert!(!d.contains_timestamp(200));
		// Value pruning matches Segment::may_contain_value.
		assert_eq!(d.value_range(), seg.value_range());
		assert!(d.may_contain_value(&BigDecimal::from(5), &BigDecimal::from(6)));
		assert!(!d.may_contain_value(&BigDecimal::from(20), &BigDecimal::from(30)));
	}

	#[test]
	fn descriptor_from_paged_segment() {
		let ts: Vec<i64> = (0..12).map(|i| i * 10).collect();
		let vs: Vec<BigDecimal> = (0..12).map(BigDecimal::from).collect();
		let seg = PagedSegment::build(&ts, &vs, TimeUnit::Millis, &BigDecimal::from(0), 4).expect("builds");
		let len = seg.write_to().len() as u64;
		let d = SegmentDescriptor::of_paged_segment(3, "p.weftseg", len, &seg);
		assert_eq!(d.format_version, seg.version);
		assert_eq!(d.physical_type, Some(seg.pages[0].values.physical_type));
		assert_eq!(d.time_unit, Some(TimeUnit::Millis));
		assert_eq!(d.row_count, 12);
		assert_eq!(d.min_ts, Some(0));
		assert_eq!(d.max_ts, Some(110));
		assert_eq!(d.time_range(), seg.time_range());
	}

	#[test]
	fn empty_paged_descriptor_has_no_encoding() {
		let seg = PagedSegment::build(&[], &[], TimeUnit::Seconds, &BigDecimal::from(0), 4).expect("builds");
		let d = SegmentDescriptor::of_paged_segment(0, "empty.weftseg", seg.write_to().len() as u64, &seg);
		assert_eq!(d.physical_type, None, "no page ⇒ no encoding to report");
		assert_eq!(d.time_unit, None);
		assert_eq!(d.row_count, 0);
		assert_eq!(d.time_range(), None);
		assert!((d.bytes_per_point() - 0.0).abs() < f64::EPSILON);
	}

	#[test]
	fn nullable_descriptor_tracks_null_count() {
		let ts = vec![10_i64, 20, 30, 40];
		let vs = ncol(&[Some("1.0"), None, Some("3.0"), None]);
		let seg = Segment::build_nullable(&ts, &vs, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		let d = SegmentDescriptor::of_segment(1, "n.weftseg", seg.write_to().len() as u64, &seg);
		assert_eq!(d.row_count, 4);
		assert_eq!(d.null_count, 2);
		assert_eq!(d.present_count(), 2);
		assert!(!d.is_all_null());
		// An all-null segment is classified as such.
		let all_null = Segment::build_nullable(&ts, &ncol(&[None, None, None, None]), TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		let ad = SegmentDescriptor::of_segment(2, "an.weftseg", all_null.write_to().len() as u64, &all_null);
		assert!(ad.is_all_null());
		assert_eq!(ad.present_count(), 0);
		assert_eq!(ad.value_range(), None);
	}

	#[test]
	fn index_prunes_to_overlapping_segments() {
		let mut index = SegmentIndex::new();
		for (i, base) in [0_i64, 100, 200].into_iter().enumerate() {
			let (seg, len) = sealed(base);
			index.push(SegmentDescriptor::of_segment(i as u64, format!("s{i}.weftseg"), len, &seg));
		}
		assert_eq!(index.len(), 3);
		assert!(!index.is_empty());
		// Each sealed() spans [base, base+90].
		let ids = |ds: Vec<&SegmentDescriptor>| ds.iter().map(|d| d.id).collect::<Vec<_>>();
		// A query inside the middle segment opens only it.
		assert_eq!(ids(index.prune_by_time(120, 150)), vec![1]);
		// A query straddling the first two opens both.
		assert_eq!(ids(index.prune_by_time(50, 150)), vec![0, 1]);
		// A query covering everything opens all three.
		assert_eq!(ids(index.prune_by_time(0, 290)), vec![0, 1, 2]);
		// A query in a gap opens none.
		assert!(index.prune_by_time(91, 99).is_empty());
	}

	#[test]
	fn index_value_pruning_selects_disjoint_spans() {
		let mut index = SegmentIndex::new();
		for (i, base) in [0_i64, 100, 200].into_iter().enumerate() {
			let ts: Vec<i64> = (0..10).collect();
			let vs: Vec<BigDecimal> = (0..10).map(|v| BigDecimal::from(base + v)).collect();
			let seg = Segment::build(&ts, &vs, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
			index.push(SegmentDescriptor::of_segment(i as u64, format!("v{i}.weftseg"), seg.write_to().len() as u64, &seg));
		}
		let ids = |ds: Vec<&SegmentDescriptor>| ds.iter().map(|d| d.id).collect::<Vec<_>>();
		// Spans [0,9], [100,109], [200,209].
		assert_eq!(ids(index.prune_by_value(&BigDecimal::from(102), &BigDecimal::from(108))), vec![1]);
		assert_eq!(ids(index.prune_by_value(&BigDecimal::from(5), &BigDecimal::from(105))), vec![0, 1]);
		assert!(index.prune_by_value(&BigDecimal::from(50), &BigDecimal::from(60)).is_empty());
	}

	#[test]
	fn index_quality_pruning_drops_all_null_segments() {
		let mut index = SegmentIndex::new();
		// Segment 0: dense over [0,40].
		let (dense, dlen) = {
			let ts: Vec<i64> = (0..5).map(|i| i * 10).collect();
			let vs: Vec<BigDecimal> = (0..5).map(BigDecimal::from).collect();
			let s = Segment::build(&ts, &vs, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
			let l = s.write_to().len() as u64;
			(s, l)
		};
		index.push(SegmentDescriptor::of_segment(0, "d.weftseg", dlen, &dense));
		// Segment 1: all-null over [100,140].
		let all_null = {
			let ts: Vec<i64> = (0..5).map(|i| 100 + i * 10).collect();
			Segment::build_nullable(&ts, &vec![None; 5], TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds")
		};
		index.push(SegmentDescriptor::of_segment(1, "a.weftseg", all_null.write_to().len() as u64, &all_null));
		let ids = |ds: Vec<&SegmentDescriptor>| ds.iter().map(|d| d.id).collect::<Vec<_>>();
		// Plain time pruning keeps the all-null segment; quality-aware pruning drops it.
		assert_eq!(ids(index.prune_by_time(0, 140)), vec![0, 1]);
		assert_eq!(ids(index.prune_present_by_time(0, 140)), vec![0]);
	}

	#[test]
	fn index_accounts_total_rows_bytes_and_span() {
		let mut index = SegmentIndex::new();
		assert_eq!(index.total_rows(), 0);
		assert_eq!(index.total_bytes(), 0);
		assert_eq!(index.time_range(), None);
		assert!((index.bytes_per_point() - 0.0).abs() < f64::EPSILON);
		let mut expected_bytes = 0_u64;
		for (i, base) in [0_i64, 100, 200].into_iter().enumerate() {
			let (seg, len) = sealed(base);
			expected_bytes += len;
			index.push(SegmentDescriptor::of_segment(i as u64, format!("s{i}.weftseg"), len, &seg));
		}
		assert_eq!(index.total_rows(), 30);
		assert_eq!(index.total_bytes(), expected_bytes);
		// Whole-index span covers the first segment's low edge to the last's high edge.
		assert_eq!(index.time_range(), Some((0, 290)));
		#[allow(clippy::cast_precision_loss)]
		let expected_bpp = expected_bytes as f64 / 30.0;
		assert!((index.bytes_per_point() - expected_bpp).abs() < f64::EPSILON);
	}

	/// A sealed single-block segment whose timestamps step backwards (so
	/// `time_sorted` is `false`), over `base..` then a dip.
	fn sealed_unsorted(base: i64) -> (Segment, u64) {
		let ts = vec![base, base + 30, base + 10, base + 40];
		let vs: Vec<BigDecimal> = (0..4).map(BigDecimal::from).collect();
		let seg = Segment::build(&ts, &vs, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		assert!(!seg.is_time_sorted(), "fixture must be out-of-order");
		let len = seg.write_to().len() as u64;
		(seg, len)
	}

	#[test]
	fn index_counts_unsorted_segments() {
		let mut index = SegmentIndex::new();
		assert_eq!(index.unsorted_count(), 0);
		// Two sorted, one unsorted.
		let (a, la) = sealed(0);
		index.push(SegmentDescriptor::of_segment(0, "a.weftseg", la, &a));
		let (b, lb) = sealed_unsorted(100);
		index.push(SegmentDescriptor::of_segment(1, "b.weftseg", lb, &b));
		let (c, lc) = sealed(200);
		index.push(SegmentDescriptor::of_segment(2, "c.weftseg", lc, &c));
		assert_eq!(index.unsorted_count(), 1);
	}

	#[test]
	fn index_detects_cross_segment_overlap() {
		let mut index = SegmentIndex::new();
		// An empty index and a single segment have nothing to overlap.
		assert_eq!(index.overlapping_count(), 0);
		let (only, len) = sealed(0);
		index.push(SegmentDescriptor::of_segment(0, "only.weftseg", len, &only));
		assert_eq!(index.overlapping_count(), 0, "a lone segment overlaps nothing");
		// Grow to three disjoint windows [0,90] [100,190] [200,290] — no cross overlap.
		for (i, base) in [(1_u64, 100_i64), (2, 200)] {
			let (seg, len) = sealed(base);
			index.push(SegmentDescriptor::of_segment(i, format!("s{i}.weftseg"), len, &seg));
		}
		assert_eq!(index.overlapping_count(), 0, "disjoint windows: no cross-segment overlap");
		// A late segment [50,140] re-enters the windows of segment 0 [0,90] and 1 [100,190].
		let (late, len) = sealed(50);
		index.push(SegmentDescriptor::of_segment(3, "late.weftseg", len, &late));
		let ids: Vec<u64> = index.overlapping_segments().iter().map(|d| d.id).collect();
		assert_eq!(index.overlapping_count(), 3);
		assert_eq!(ids, vec![0, 1, 3], "segments 0, 1 and the late 3 overlap, returned in index order");
	}

	#[test]
	fn index_overlap_counts_boundary_touch_and_containment() {
		let mut index = SegmentIndex::new();
		// [0,90] and [90,180] share the instant 90 — an inclusive-range overlap.
		let (a, la) = sealed(0);
		index.push(SegmentDescriptor::of_segment(0, "a.weftseg", la, &a));
		let (b, lb) = sealed(90);
		index.push(SegmentDescriptor::of_segment(1, "b.weftseg", lb, &b));
		assert_eq!(index.overlapping_count(), 2, "touching at a shared timestamp counts as overlap");
		// A wide segment [0,290] added last contains both disjoint neighbours downstream.
		let mut wide = SegmentIndex::new();
		let ts: Vec<i64> = (0..30).map(|i| i * 10).collect();
		let vs: Vec<BigDecimal> = (0..30).map(BigDecimal::from).collect();
		let w = Segment::build(&ts, &vs, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		let (n1, l1) = sealed(500);
		wide.push(SegmentDescriptor::of_segment(0, "n1.weftseg", l1, &n1));
		wide.push(SegmentDescriptor::of_segment(1, "wide.weftseg", w.write_to().len() as u64, &w));
		// [500,590] and [0,290] are disjoint → no overlap.
		assert_eq!(wide.overlapping_count(), 0);
	}

	#[test]
	fn index_serde_round_trips() {
		let mut index = SegmentIndex::new();
		let (seg, len) = sealed(0);
		index.push(SegmentDescriptor::of_segment(0, "s0.weftseg", len, &seg));
		let json = serde_json::to_string(&index).expect("serializes");
		let back: SegmentIndex = serde_json::from_str(&json).expect("deserializes");
		assert_eq!(index, back);
	}
}
