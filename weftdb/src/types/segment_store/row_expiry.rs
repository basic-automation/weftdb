//! Row-level expiry policy for [`SegmentStore`](super::SegmentStore).
//!
//! Cut 1: library policy + read-path visibility + sweep, no env var, no weft-server,
//! no OpenAPI/config-schema change.
//!
//! A [`RowExpiryPolicy`] says how old a row must be before reads hide it and the expiry
//! sweep schedules it for deletion. The default is [`RowExpiryPolicy::NoExpiry`] — every row is always
//! visible. When a TTL is set, a row whose age in the aspect's time unit is strictly
//! **greater** than the policy's TTL is treated as expired: read paths filter it out, and
//! the expiry sweep may drop it physically (cut 1 keeps this report-only for sweep).
//!
//! ## Clock injection
//!
//! The clock is **never** `SystemTime::now()` inside the policy. The store passes an
//! explicit `now_ns` (an ns-since-epoch instant) from the caller — the sweep's epoch
//! instant or the read's `now` parameter.
//!
//! ## Boundary semantics
//!
//! The boundary is **strict**: a row whose age equals the TTL is visible.
//!
//! - Age = `now - row_timestamp` in the aspect's time unit.
//! - Expired iff `age > ttl`.
//!
//! This means `TTL = 0` expires every row whose timestamp is strictly less than `now`
//! (rows at exactly `now` are preserved). The default is `NoExpiry`, which preserves all
//! rows regardless of age.
//!
//! ## Two passes: counting, and dropping what no read can reach
//!
//! [`SegmentStore::expiry_sweep`](super::SegmentStore::expiry_sweep) counts expired rows and
//! removes nothing. It is the safe default, and a store that only ever runs it keeps every
//! byte it has ever written.
//!
//! [`SegmentStore::purge_expired`](super::SegmentStore::purge_expired) goes further and
//! **removes segments from the store**. It drops a segment only when its recorded extent ends
//! strictly before the cutoff, which is the condition under which no read that respects the
//! TTL can return any of its rows: the index row goes, the frame is journaled `'retired'`, and
//! the reaper unlinks it. A segment holding both expired and live rows is **left alone** and
//! counted in [`partial_segments`](ExpirySweep::partial_segments) - rewriting a frame that
//! also holds live rows is a separate change - so a TTL shorter than a segment's span leaves
//! residue on disk. That residue is invisible to reads and visible to `du`. The second pass
//! exists so the caller decides whether residue it cannot read is acceptable, rather than a
//! policy enum implying that it is.
//!
//! Like every other maintenance pass, a purge needs the aspect's lock: an aspect it could not
//! take is reported in [`busy`](ExpirySweep::busy) rather than waited for.

use weft_physical_type::timestamp::TimeUnit;

/// Row-expiry policy attached per-aspect.
///
/// `NoExpiry` lets every row live; `Ttl { ttl }` hides rows whose age in the aspect's
/// time unit is strictly greater than `ttl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowExpiryPolicy {
	/// No expiry: every row is always visible (the default).
	NoExpiry,
	/// Rows whose age in the aspect's time unit is strictly greater than `ttl` are
	/// expired and hidden from read paths. A row whose age exactly equals `ttl` is
	/// **not** expired.
	Ttl { ttl: u64 },
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Whether `row_ts` (an epoch in `unit`) is expired at `now_ns` (ns since epoch) under `policy`.
///
/// `NoExpiry` → always `false`. `Ttl` → `(now - row_ts) > ttl` in the aspect's unit.
///
/// # Design
///
/// - `now_ns` is caller-supplied; the function never reads the wall clock.
/// - A future row (`row_ts > now_unit`) is never expired.
/// - The boundary is strict (age == ttl → not expired).
/// - Arithmetic is infallible: `now_ns` as an i64 is in the representaable nanosecond
///   range (years ~1677 to ~2262). The unit conversion is lossless in this range.
#[must_use]
pub fn is_row_expired(policy: &RowExpiryPolicy, now_ns: i64, row_ts: i64, unit: &TimeUnit) -> bool {
	match policy {
		RowExpiryPolicy::NoExpiry => false,
		RowExpiryPolicy::Ttl { ttl } => {
			let now_unit = unit.from_nanos(now_ns);
			if row_ts >= now_unit {
				return false;
			}
			// A TTL wider than any age an i64 timestamp can express is
			// unreachable, so saturating it keeps the comparison total.
			let ttl = i64::try_from(*ttl).unwrap_or(i64::MAX);
			// `now_unit > row_ts` here, so the age is positive.
			now_unit.saturating_sub(row_ts) > ttl
		}
	}
}

/// Compute the cutoff timestamp (in the aspect's unit) for `policy` at `now_ns`.
///
/// A row with `timestamp < cutoff` is expired (strictly less than). For `NoExpiry` (or
/// when `now - ttl` would underflow), returns `i64::MIN` so nothing is expired.
///
/// The cutoff is `floor(now_ns_as_unit - ttl)`, a **strictly less than** boundary.
#[must_use]
pub fn expired_cutoff(policy: &RowExpiryPolicy, now_ns: i64, unit: &TimeUnit) -> i64 {
	match policy {
		RowExpiryPolicy::NoExpiry => i64::MIN,
		RowExpiryPolicy::Ttl { ttl } => {
			let now_unit = unit.from_nanos(now_ns);
			// A cutoff at or below the whole representable range means "nothing
			// is expired", and Cut 1 reports that as `i64::MIN`: when the TTL
			// is wider than `now_unit` itself (including a saturating huge TTL
			// or a pre-epoch clock) the cutoff is refused rather than negative.
			let ttl = i64::try_from(*ttl).unwrap_or(i64::MAX);
			if now_unit < ttl {
				i64::MIN
			} else {
				now_unit - ttl
			}
		}
	}
}

// ---------------------------------------------------------------------------
// Sweep report types
// ---------------------------------------------------------------------------

/// Outcome of one aspect's expiry sweep pass.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AspectExpirySwept {
	/// Rows the pass found expired. A purge counts the rows of the segments it dropped *and*
	/// the rows inside the segments it left alone, so the figure means "rows the TTL hides",
	/// not "rows this pass deleted".
	pub rows_expired: u64,
	/// Segments removed from the store. Always 0 for the report-only sweep.
	pub segments_dropped: u64,
	/// Segments holding both expired and live rows, which a purge leaves on disk.
	pub partial_segments: u64,
	/// Oldest (minimum) timestamp of an expired row, if any were found.
	pub oldest_expired_ts: Option<i64>,
	/// Newest (maximum) timestamp of an expired row, if any were found.
	pub newest_expired_ts: Option<i64>,
	/// Segments that are wholly expired but overlap a surviving segment of the same
	/// series and were therefore kept on disk to avoid shadow-reverting plain reads
	/// (#797). A shadowed-and-kept segment is neither dropped (it would silently
	/// reveal the superseded rows of the survivor) nor straddling (every row is
	/// expired).
	pub shadowed_and_kept: u64,
	/// Whether the sweep ran to completion on this aspect (cut 1: always true on
	/// success — no physical deletion yet).
	pub completed: bool,
}

impl Default for AspectExpirySwept {
	/// A successful pass with nothing to expire.
	fn default() -> Self {
		Self { rows_expired: 0, segments_dropped: 0, partial_segments: 0, shadowed_and_kept: 0, oldest_expired_ts: None, newest_expired_ts: None, completed: true }
	}
}

/// Store-wide expiry sweep: how many aspects were cleaned, and how many rows
/// the sweep reported as expired.
#[derive(Debug)]
#[non_exhaustive]
pub struct ExpirySweep {
	/// Aspects whose expired rows were counted (and would be deleted in a later cut).
	pub aspects_cleaned: Vec<String>,
	/// Aspects that were busy (another maintenance operation held the lock).
	pub busy: Vec<String>,
	/// Aspects whose sweep produced an error (the sweep was interrupted by an index
	/// read/commit failure, not by the expiry check).
	pub failed: Vec<(String, anyhow::Error)>,
	/// Total rows counted as expired across all aspects.
	pub rows_expired: u64,
	/// Total segments removed across all aspects. 0 for the report-only sweep.
	pub segments_dropped: u64,
	/// Total segments holding both expired and live rows across all aspects.
	pub partial_segments: u64,
	/// Segments that were wholly expired but kept on disk because they overlapped a
	/// surviving segment of the same series (#797).
	pub shadowed_and_kept: u64,
}

impl Default for ExpirySweep {
	/// Empty sweep (no aspects scanned, nothing expired).
	fn default() -> Self {
		Self { aspects_cleaned: Vec::new(), busy: Vec::new(), failed: Vec::new(), rows_expired: 0u64, segments_dropped: 0u64, partial_segments: 0u64, shadowed_and_kept: 0u64 }
	}
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
	use weft_physical_type::timestamp::TimeUnit;

	use super::*;

	fn ns(secs: i64) -> i64 {
		secs * 1_000_000_000
	}

	// ── Criterion 1: NoExpiry (default) ──────────────────────────────────
	// NoExpiry must never expire any row, regardless of age.

	#[test]
	fn no_expiry_never_expires() {
		let policy = RowExpiryPolicy::NoExpiry;
		let unit = TimeUnit::Seconds;
		// Any now_ns and any row_ts: always false.
		assert!(!is_row_expired(&policy, ns(0), 0, &unit), "epoch 0 at epoch 0 is not expired");
		assert!(!is_row_expired(&policy, ns(1_000_000), 0, &unit), "one second later at one-second-old row is not expired");
		assert!(!is_row_expired(&policy, ns(999_999_999), 0, &unit), "row younger than ttl is not expired");
		// Raw nanosecond values: `ns(i64::MAX)` would overflow the multiply.
		assert!(!is_row_expired(&policy, i64::MAX, i64::MAX, &unit), "max values cause no issues");
	}

	#[test]
	fn no_expiry_cutoff_is_min() {
		let policy = RowExpiryPolicy::NoExpiry;
		let unit = TimeUnit::Seconds;
		assert_eq!(expired_cutoff(&policy, ns(100), &unit), i64::MIN);
		assert_eq!(expired_cutoff(&policy, ns(0), &unit), i64::MIN);
	}

	// ── Criterion 2: TTL hides old rows ──────────────────────────────────

	#[test]
	fn ttl_hides_rows_older_than_now_minus_ttl() {
		let policy = RowExpiryPolicy::Ttl { ttl: 10 };
		let unit = TimeUnit::Seconds;

		let now_ns = ns(1000);

		// row_ts = 995: age = 5, which is > 10? No. Visible.
		assert!(!is_row_expired(&policy, now_ns, 995, &unit), "age 5 < ttl 10 → visible");

		// row_ts = 985: age = 15, which is > 10. Expired.
		assert!(is_row_expired(&policy, now_ns, 985, &unit), "age 15 > ttl 10 → expired");

		// row_ts = 989: age = 11, > 10. Expired.
		assert!(is_row_expired(&policy, now_ns, 989, &unit), "age 11 > ttl 10 → expired");

		// row_ts = 1000 (now): age = 0, not expired.
		assert!(!is_row_expired(&policy, now_ns, 1000, &unit), "age 0 ≤ ttl 10 → visible");
	}

	// ── Criterion 3: Boundary — age == TTL is NOT expired ─────────────────

	#[test]
	fn boundary_age_equals_ttl_is_not_expired() {
		let policy = RowExpiryPolicy::Ttl { ttl: 10 };
		let unit = TimeUnit::Seconds;

		let now_ns = ns(1000);

		// row_ts = 990: age = 10, exactly equal to ttl. NOT expired.
		assert!(!is_row_expired(&policy, now_ns, 990, &unit), "age == ttl → NOT expired");

		// row_ts = 989: age = 11, > 10. Expired.
		assert!(is_row_expired(&policy, now_ns, 989, &unit), "age 11 > ttl 10 → expired");
	}

	// ── Criterion 3 (extra): TTL = 0 expires anything strictly before now ─

	#[test]
	fn zero_ttl_expires_rows_strictly_before_now() {
		let policy = RowExpiryPolicy::Ttl { ttl: 0 };
		let unit = TimeUnit::Seconds;

		let now_ns = ns(1000);

		assert!(!is_row_expired(&policy, now_ns, 1000, &unit), "exactly now: age 0 > 0? no → visible");
		assert!(is_row_expired(&policy, now_ns, 999, &unit), "one unit before now: age 1 > 0 → expired");
		assert!(!is_row_expired(&policy, now_ns, 1001, &unit), "future row: not expired");
	}

	// ── Clock injection ──────────────────────────────────────────────────
	// The function never calls SystemTime::now(). now_ns is always from the caller.

	#[test]
	fn clock_is_injected() {
		// The function signature takes `now_ns: i64` — no SystemTime inside.
		// This test asserts the contract: if we control now_ns, we control expiry.
		let policy = RowExpiryPolicy::Ttl { ttl: 60 };
		let unit = TimeUnit::Seconds;

		// Row at 0, now at 70 (age 70 > 60 → expired) vs now at 50 (age 50 ≤ 60 → visible).
		assert!(is_row_expired(&policy, ns(70), 0, &unit), "now=70, row=0, age 70 > 60 → expired");
		assert!(!is_row_expired(&policy, ns(50), 0, &unit), "now=50, row=0, age 50 ≤ 60 → visible");
	}

	// ── Different time units ─────────────────────────────────────────────

	#[test]
	fn ttl_works_in_different_time_units() {
		// TTL in seconds, but aspect uses Millis. Internally, conversion
		// is done via `unit.from_nanos(now_ns)` — so TTL is interpreted in
		// the aspect's time unit.

		// Seconds aspect: ttl = 1 → rows older than 1 second are expired.
		let sec_policy = RowExpiryPolicy::Ttl { ttl: 1 };
		let sec_unit = TimeUnit::Seconds;

		// now = 10s, row at 8s → age = 2 > 1 → expired.
		assert!(is_row_expired(&sec_policy, ns(10), 8, &sec_unit), "seconds unit: age 2 > 1 → expired");
		assert!(!is_row_expired(&sec_policy, ns(10), 9, &sec_unit), "seconds unit: age 1 == 1 → NOT expired");

		// Millis aspect: ttl = 1000 → rows older than 1000 ms are expired.
		let ms_policy = RowExpiryPolicy::Ttl { ttl: 1000 };
		let ms_unit = TimeUnit::Millis;

		// now = 5_000_000_000 ns = 5000 ms
		let now_ns = ns(5); // 5_000_000_000 ns = 5000 ms in from_nanos
				    // row_ts = 3999 ms → age = 1001 > 1000 → expired
		assert!(is_row_expired(&ms_policy, now_ns, 3999, &ms_unit), "millis unit: age 1001 > 1000 → expired");
		// row_ts = 4000 ms → age = 1000 == 1000 → NOT expired
		assert!(!is_row_expired(&ms_policy, now_ns, 4000, &ms_unit), "millis unit: age 1000 == 1000 → NOT expired");
	}

	// ── Future rows ──────────────────────────────────────────────────────

	#[test]
	fn future_rows_are_not_expired() {
		let policy = RowExpiryPolicy::Ttl { ttl: 10 };
		let unit = TimeUnit::Seconds;

		let now_ns = ns(100);
		// row_ts = 200 is 100 units in the future.
		assert!(!is_row_expired(&policy, now_ns, 200, &unit), "future row is never expired");
	}

	// ── Cutoff computation ───────────────────────────────────────────────

	#[test]
	fn expired_cutoff_returns_correct_boundary() {
		// TTL 10 at now=1000s → cutoff = 990 = now - ttl.
		// A row at ts=990 has age == TTL → visible (strict boundary).
		// The cutoff is a boundary value: rows with ts < cutoff are expired;
		// rows with ts == cutoff have age == TTL and are NOT expired.
		let policy = RowExpiryPolicy::Ttl { ttl: 10 };
		let unit = TimeUnit::Seconds;
		assert_eq!(expired_cutoff(&policy, ns(1000), &unit), 990i64, "cutoff = now - ttl = 1000 - 10 = 990");
		// The cutoff value 990, when used with strict '>' (not '>='), correctly means
		// only rows with ts < 990 are expired. The cutoff itself is NOT a threshold
		// that includes the boundary row.
	}

	#[test]
	fn expired_cutoff_underflow_returns_min() {
		// now_unit < ttl: most negative case.
		let policy = RowExpiryPolicy::Ttl { ttl: 100 };
		let unit = TimeUnit::Seconds;
		assert_eq!(expired_cutoff(&policy, ns(50), &unit), i64::MIN, "underflow: 50 < 100 → no expiry possible");

		// Negative now (before epoch) with large TTL.
		let policy_big = RowExpiryPolicy::Ttl { ttl: 10_000_000_000_000_000 };
		assert_eq!(expired_cutoff(&policy_big, ns(-500), &unit), i64::MIN, "very large TTL underflows");
	}

	// ── Sweep report types ───────────────────────────────────────────────

	#[test]
	fn default_sweep_is_empty() {
		let sweep = ExpirySweep::default();
		assert_eq!(sweep.aspects_cleaned, [] as [String; 0]);
		assert_eq!(sweep.busy, [] as [String; 0]);
		assert!(sweep.failed.is_empty());
		assert_eq!(sweep.rows_expired, 0);
	}
}
