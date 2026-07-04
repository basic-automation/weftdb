//! Size-based out-of-order **split geometry** (roadmap **Phase 4.6**).
//!
//! When late/out-of-order data lands inside a sealed segment's already-covered time
//! span, the shipped reconciliation path
//! ([`SegmentStore::reconcile_segment`](../../database/index.html)) rewrites the
//! *whole* segment — it stable-sorts every row and re-seals. That is correct but pays
//! write amplification proportional to the whole segment, even when the late data
//! only disturbs a small suffix of it.
//!
//! QuestDB avoids that with a **split-not-rewrite** model: rather than rewriting a
//! partition when late data arrives, it *splits* the partition at the boundary where
//! the late data begins, keeps the untouched **prefix** as-is, and merges only the
//! small **suffix** with the new rows — deferring a full **squash** of the
//! accumulated splits until their count crosses a threshold. Crucially the split is
//! **size-gated**: it fires only when the untouched prefix is both larger than the
//! bytes a split would still rewrite (suffix + new data) *and* above a minimum size
//! floor; below that floor the bookkeeping of a split costs more than just rewriting
//! the whole (small) segment. *(src: split fires when "the existing partition prefix
//! is larger than the new data plus suffix" past `cairo.o3.partition.split.min.size`
//! = 50 MiB — <https://questdb.com/docs/concepts/partitions/>)*
//!
//! This module is the **pure geometry + policy** first slice of that path: it decides
//! *whether* a split is worthwhile ([`SplitPolicy::decide`]) and *where* the boundary
//! falls in a sorted segment ([`split_index`]), with no I/O and no segment rewriting.
//! The reconciliation machinery that acts on the decision is a later slice.

/// Whether to reconcile late/out-of-order data by rewriting a whole segment or by
/// splitting it and merging only the affected suffix.
///
/// Returned by [`SplitPolicy::decide`]. The choice is purely about write
/// amplification: a full rewrite touches every byte of the segment; a split leaves
/// the prefix bytes untouched and rewrites only `suffix + new_data`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDecision {
	/// Rewrite the entire segment (stable-sort all rows and re-seal) — the shipped
	/// in-place path. Cheaper than a split when the untouched prefix is small or below
	/// the min-split floor, so the split bookkeeping would not pay for itself.
	FullRewrite,
	/// Split the segment at the late-data boundary: keep the prefix untouched and
	/// merge only the suffix with the new rows. Worth it when the prefix is large and
	/// above the floor, since those prefix bytes are never rewritten.
	Split,
}

impl SplitDecision {
	/// Whether this decision is [`Split`](SplitDecision::Split).
	#[must_use]
	pub const fn is_split(self) -> bool {
		matches!(self, Self::Split)
	}
}

/// The size-based split-vs-rewrite policy (roadmap Phase 4.6), modeled on QuestDB's
/// partition split.
///
/// A segment about to absorb late data is conceptually cut at the boundary where the
/// late data begins into a **prefix** (existing rows strictly before the late window)
/// and a **suffix** (existing rows at or after it, which must merge with the new
/// rows). [`decide`](SplitPolicy::decide) chooses between rewriting the whole segment
/// and splitting off just the prefix, using the same two-part test QuestDB applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitPolicy {
	/// The minimum prefix size, in bytes, below which a split never fires — a full
	/// in-place rewrite of a small segment is cheaper than carrying a split. QuestDB's
	/// analogue is `cairo.o3.partition.split.min.size` (default 50 MiB).
	pub min_split_bytes: u64,
}

impl SplitPolicy {
	/// QuestDB's default `cairo.o3.partition.split.min.size` — 50 MiB.
	pub const QUESTDB_DEFAULT_MIN_SPLIT_BYTES: u64 = 50 * 1024 * 1024;

	/// A policy with an explicit minimum split size in bytes.
	#[must_use]
	pub const fn new(min_split_bytes: u64) -> Self {
		Self { min_split_bytes }
	}

	/// A policy using QuestDB's default 50 MiB minimum split size.
	#[must_use]
	pub const fn questdb_default() -> Self {
		Self { min_split_bytes: Self::QUESTDB_DEFAULT_MIN_SPLIT_BYTES }
	}

	/// Decide whether to [`Split`](SplitDecision::Split) or [`FullRewrite`](SplitDecision::FullRewrite)
	/// given the byte sizes of the three pieces a late-data merge sees:
	///
	/// - `prefix_bytes` — the existing rows strictly before the late window; a split
	///   leaves these untouched.
	/// - `suffix_bytes` — the existing rows at or after the late window; a split still
	///   rewrites these (merged with the new rows).
	/// - `new_data_bytes` — the incoming late rows.
	///
	/// A split fires **only** when the untouched prefix is both:
	/// 1. at least [`min_split_bytes`](SplitPolicy::min_split_bytes) (above the floor —
	///    below it the split bookkeeping costs more than a whole-segment rewrite), and
	/// 2. strictly larger than `suffix_bytes + new_data_bytes` (the bytes a split would
	///    still rewrite) — i.e. splitting actually avoids rewriting the majority of the
	///    segment.
	///
	/// Otherwise a full rewrite is chosen. `suffix + new` is computed with a saturating
	/// add so an implausibly huge input degrades to `FullRewrite` rather than wrapping.
	#[must_use]
	pub const fn decide(&self, prefix_bytes: u64, suffix_bytes: u64, new_data_bytes: u64) -> SplitDecision {
		let merge_bytes = suffix_bytes.saturating_add(new_data_bytes);
		if prefix_bytes >= self.min_split_bytes && prefix_bytes > merge_bytes {
			SplitDecision::Split
		} else {
			SplitDecision::FullRewrite
		}
	}
}

/// The split boundary in a **sorted** existing segment: the number of rows whose
/// timestamp is strictly less than `late_min` — i.e. the length of the untouched
/// prefix when late data whose earliest timestamp is `late_min` is merged in.
///
/// Equivalently the index of the first existing row at or after the late window, so
/// `existing_ts[..split_index]` is the prefix a split keeps and
/// `existing_ts[split_index..]` is the suffix it merges. Assumes `existing_ts` is
/// non-decreasing (as a sealed sorted segment's timestamps are); it runs an
/// `O(log n)` partition point over that order. When every row is before `late_min`
/// the result is `existing_ts.len()` (a pure append — nothing to merge); when every
/// row is at or after it the result is `0` (the whole segment is the suffix).
#[must_use]
pub fn split_index(existing_ts: &[i64], late_min: i64) -> usize {
	existing_ts.partition_point(|&t| t < late_min)
}

/// Merge two timestamp-ordered row sequences into one, **newer-wins on a shared
/// timestamp** (roadmap Phase 4.6 — the merge step of the split-not-rewrite path).
///
/// `older` and `newer` are each **non-decreasing by timestamp** (as sealed sorted
/// segments are); `older` is the earlier-sealed data and `newer` the later batch that
/// re-entered its window. The result is a single non-decreasing sequence in which, at
/// any timestamp carried by **both** sides, every `newer` row supersedes every `older`
/// row (upsert / last-writer-wins) — matching the answer
/// [`SegmentStore::read_point`](../../database/index.html) gives across overlapping
/// segments, where the most-recently-sealed segment wins. Rows at a timestamp unique
/// to one side are kept as-is, including a within-side run of equal timestamps.
///
/// Runs in `O(len(older) + len(newer))` via a two-pointer merge. When the inputs do
/// not overlap in time this is a plain concatenation in timestamp order.
#[must_use]
pub fn merge_newer_wins<V: Clone>(older: &[(i64, V)], newer: &[(i64, V)]) -> Vec<(i64, V)> {
	let mut out: Vec<(i64, V)> = Vec::with_capacity(older.len() + newer.len());
	let (mut i, mut j) = (0_usize, 0_usize);
	while i < older.len() && j < newer.len() {
		match older[i].0.cmp(&newer[j].0) {
			std::cmp::Ordering::Less => {
				out.push(older[i].clone());
				i += 1;
			},
			std::cmp::Ordering::Greater => {
				out.push(newer[j].clone());
				j += 1;
			},
			std::cmp::Ordering::Equal => {
				// Shared instant: newer supersedes older. Drop the whole older run at
				// this timestamp and emit the whole newer run.
				let t = older[i].0;
				while i < older.len() && older[i].0 == t {
					i += 1;
				}
				while j < newer.len() && newer[j].0 == t {
					out.push(newer[j].clone());
					j += 1;
				}
			},
		}
	}
	out.extend_from_slice(&older[i..]);
	out.extend_from_slice(&newer[j..]);
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn split_fires_when_prefix_dominates_and_clears_the_floor() {
		let policy = SplitPolicy::new(1000);
		// Prefix 8000 >= floor 1000 and > suffix(500) + new(200) = 700 → split.
		assert_eq!(policy.decide(8000, 500, 200), SplitDecision::Split);
		assert!(policy.decide(8000, 500, 200).is_split());
	}

	#[test]
	fn full_rewrite_when_prefix_below_the_floor() {
		let policy = SplitPolicy::new(1000);
		// Prefix 900 < floor 1000 → rewrite the whole (small) segment even though the
		// prefix outweighs suffix + new.
		assert_eq!(policy.decide(900, 10, 10), SplitDecision::FullRewrite);
	}

	#[test]
	fn full_rewrite_when_prefix_does_not_outweigh_the_merge() {
		let policy = SplitPolicy::new(100);
		// Prefix 600 >= floor 100 but not > suffix(400) + new(300) = 700 → rewrite.
		assert_eq!(policy.decide(600, 400, 300), SplitDecision::FullRewrite);
		// Exactly equal is not "strictly larger" → rewrite.
		assert_eq!(policy.decide(700, 400, 300), SplitDecision::FullRewrite);
		// One byte over the merge size, and above the floor → split.
		assert_eq!(policy.decide(701, 400, 300), SplitDecision::Split);
	}

	#[test]
	fn saturating_merge_degrades_to_full_rewrite() {
		let policy = SplitPolicy::new(0);
		// suffix + new saturates to u64::MAX, which no prefix can strictly exceed.
		assert_eq!(policy.decide(u64::MAX, u64::MAX, 1), SplitDecision::FullRewrite);
	}

	#[test]
	fn questdb_default_floor_is_50_mib() {
		let policy = SplitPolicy::questdb_default();
		assert_eq!(policy.min_split_bytes, 50 * 1024 * 1024);
		// A 10 MiB prefix is below the 50 MiB floor → rewrite, even dominating tiny merges.
		assert_eq!(policy.decide(10 * 1024 * 1024, 1, 1), SplitDecision::FullRewrite);
		// A 60 MiB prefix clears the floor and dominates → split.
		assert_eq!(policy.decide(60 * 1024 * 1024, 1024, 1024), SplitDecision::Split);
	}

	#[test]
	fn merge_newer_wins_supersedes_shared_timestamps() {
		// Disjoint tails concatenate in order.
		let older = [(10_i64, "a"), (20, "b")];
		let newer = [(30_i64, "c"), (40, "d")];
		assert_eq!(merge_newer_wins(&older, &newer), vec![(10, "a"), (20, "b"), (30, "c"), (40, "d")]);
		// Overlap at 20 and 30: newer wins those instants, older's 10 and newer's 40 survive.
		let older = [(10_i64, "a"), (20, "old20"), (30, "old30")];
		let newer = [(20_i64, "new20"), (30, "new30"), (40, "d")];
		assert_eq!(merge_newer_wins(&older, &newer), vec![(10, "a"), (20, "new20"), (30, "new30"), (40, "d")]);
	}

	#[test]
	fn merge_newer_wins_handles_equal_timestamp_runs() {
		// Older has a run at 20; newer replaces the whole run with its own row at 20.
		let older = [(20_i64, "o1"), (20, "o2"), (30, "o3")];
		let newer = [(20_i64, "n1"), (40, "n2")];
		assert_eq!(merge_newer_wins(&older, &newer), vec![(20, "n1"), (30, "o3"), (40, "n2")]);
		// A run unique to one side is preserved intact.
		let older = [(10_i64, "a")];
		let newer = [(20_i64, "b1"), (20, "b2")];
		assert_eq!(merge_newer_wins(&older, &newer), vec![(10, "a"), (20, "b1"), (20, "b2")]);
	}

	#[test]
	fn merge_newer_wins_with_empty_sides() {
		let rows = [(1_i64, "x"), (2, "y")];
		assert_eq!(merge_newer_wins::<&str>(&[], &rows), rows.to_vec());
		assert_eq!(merge_newer_wins::<&str>(&rows, &[]), rows.to_vec());
		assert_eq!(merge_newer_wins::<&str>(&[], &[]), Vec::<(i64, &str)>::new());
	}

	#[test]
	fn split_index_partitions_a_sorted_segment() {
		let ts = [10_i64, 20, 30, 40, 50];
		// Late data starting at 35 → prefix is [10,20,30] (3 rows), suffix [40,50].
		assert_eq!(split_index(&ts, 35), 3);
		// Late min equal to an existing timestamp is at-or-after → boundary before it.
		assert_eq!(split_index(&ts, 30), 2);
		// Before everything → whole segment is the suffix.
		assert_eq!(split_index(&ts, 5), 0);
		// After everything → pure append, nothing to merge.
		assert_eq!(split_index(&ts, 100), 5);
		// Empty segment.
		assert_eq!(split_index(&[], 42), 0);
	}
}
