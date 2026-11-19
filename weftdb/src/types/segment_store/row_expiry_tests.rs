//! P12-722 cut 1: store-level row expiry integration tests.
//!
//! Tests the expiry policy through `SegmentStore` — reading visible rows,
//! the sweep report, and the interaction with the existing row-level
//! newer-wins merge.  The policy is per-store (cut 1); per-aspect policies
//! are deferred to cut 2.

use std::str::FromStr;

use bigdecimal::BigDecimal;
use tempfile::TempDir;
use weft_physical_type::{timestamp::TimeUnit, AspectSchema, PhysicalType};

use super::{SegmentStore, SegmentStoreOptions};
use crate::{
	segment_store::row_expiry::{expired_cutoff, RowExpiryPolicy}, IngestBatch
};

fn bd(s: &str) -> BigDecimal {
	BigDecimal::from_str(s).unwrap_or_else(|e| panic!("bd({s:?}) is not a decimal literal: {e}"))
}

const ASPECT: &str = "temp";
const TTL_SECS: u64 = 10;
const NOW_NS: i64 = 100_000_000_000; // T = 100s

fn schema(unit: TimeUnit) -> AspectSchema {
	AspectSchema::new(PhysicalType::F64, bd("0"), unit)
}

fn ns(t: i64) -> i64 {
	t * 1_000_000_000
}

async fn make_store(expiry: RowExpiryPolicy) -> (TempDir, SegmentStore) {
	let dir = TempDir::new().expect("tempdir");
	let opts = SegmentStoreOptions::default().with_expiry(expiry);
	let store = SegmentStore::open_with_options(dir.path(), opts).await.expect("opens with options");
	(dir, store)
}

// ── Criterion 1: NoExpiry (default) leaves every read byte-identical ─────
// A read on a NoExpiry store returns the same rows as a store with no
// options at all.

#[tokio::test]
async fn no_expiry_makes_no_difference_to_reads() {
	let (dir, store) = make_store(RowExpiryPolicy::NoExpiry).await;
	store.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares");
	store.ingest(ASPECT, &IngestBatch::dense("s1", vec![90, 95, 100], vec![bd("1"), bd("2"), bd("3")])).await.expect("ingests rows");
	// With NoExpiry, all rows are visible regardless of now_ns.
	let (ts, vs) = store.read_time_range_of_with_expiry(ASPECT, crate::SeriesId::UNTAGGED, 0, 200, NOW_NS).await.expect("reads with expiry");
	let (ts2, vs2) = store.read_time_range(ASPECT, 0, 200).await.expect("reads without expiry");
	assert_eq!(ts, ts2, "timestamps match");
	assert_eq!(vs, vs2, "values match");
	assert_eq!(ts, vec![90i64, 95, 100], "all three rows visible");
	drop(store);
	dir.close().expect("close temp dir");
}

// ── Criterion 2: TTL hides old rows ───────────────────────────────────────
// Rows older than now - TTL are hidden from read paths.

#[tokio::test]
async fn ttl_hides_rows_older_than_now_minus_ttl() {
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS, unit: TimeUnit::Seconds }).await;
	store.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares");
	// Rows at t=80 (age 20 > 10 → expired) and t=95 (age 5 ≤ 10 → visible).
	store.ingest(ASPECT, &IngestBatch::dense("s1", vec![80, 95, 100], vec![bd("1"), bd("2"), bd("3")])).await.expect("ingests");
	let (ts, vs) = store.read_time_range_of_with_expiry(ASPECT, crate::SeriesId::UNTAGGED, 0, 200, NOW_NS).await.expect("reads with expiry");
	assert_eq!(ts, vec![95i64, 100], "old rows are hidden by TTL");
	assert_eq!(vs, vec![Some(bd("2")), Some(bd("3"))], "values match");
	drop(store);
	dir.close().expect("close temp dir");
}

// ── Criterion 3: Boundary — age == TTL is NOT expired ─────────────────────

#[tokio::test]
async fn boundary_age_equals_ttl_is_not_expired() {
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS, unit: TimeUnit::Seconds }).await;
	store.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares");
	// Row at t=90: age = 100 - 90 = 10, which equals TTL → NOT expired.
	// Ascending: the batch API takes timestamps in order.
	store.ingest(ASPECT, &IngestBatch::dense("s1", vec![89, 90], vec![bd("2"), bd("1")])).await.expect("ingests");
	let (ts, _vs) = store.read_time_range_of_with_expiry(ASPECT, crate::SeriesId::UNTAGGED, 0, 200, NOW_NS).await.expect("reads with expiry");
	assert!(ts.contains(&90), "row at t=90 (age == TTL) must be visible");
	assert!(!ts.contains(&89), "row at t=89 (age > TTL) must be hidden");
	drop(store);
	dir.close().expect("close temp dir");
}

// ── Criterion 4: Clock is injected, never SystemTime::now() ───────────────

#[tokio::test]
async fn expiry_clock_is_injected() {
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS, unit: TimeUnit::Seconds }).await;
	store.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares");
	store.ingest(ASPECT, &IngestBatch::dense("s1", vec![85, 95], vec![bd("1"), bd("2")])).await.expect("ingests");

	// With now_ns = T=100s: t=85 expires (age 15 > 10), t=95 visible.
	let (ts_a, _) = store.read_time_range_of_with_expiry(ASPECT, crate::SeriesId::UNTAGGED, 0, 200, ns(100)).await.expect("at now=100");
	assert!(!ts_a.contains(&85), "at now=100, t=85 is expired");
	assert!(ts_a.contains(&95), "at now=100, t=95 is visible");

	// With now_ns = T=90s: t=85 visible (age 5 ≤ 10), t=95 visible.
	let (ts_b, _) = store.read_time_range_of_with_expiry(ASPECT, crate::SeriesId::UNTAGGED, 0, 200, ns(90)).await.expect("at now=90");
	assert!(ts_b.contains(&85), "at now=90, t=85 is visible (age 5 ≤ TTL)");
	assert!(ts_b.contains(&95), "at now=90, t=95 is visible");

	drop(store);
	dir.close().expect("close temp dir");
}

// ── Criterion 5: Expiry applies to rows, not segments ────────────────────
// A segment whose sidecar max_ts is expired but which contains a row within TTL
// must still be read — expiry is applied per row, not per segment.

#[tokio::test]
async fn expiry_applies_per_row_not_segment() {
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS, unit: TimeUnit::Seconds }).await;
	store.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares");
	// One segment with rows at t=50 (expired at now=100) and t=95 (visible).
	store.ingest(ASPECT, &IngestBatch::dense("s1", vec![50, 95], vec![bd("1"), bd("2")])).await.expect("ingests both rows in one segment");
	let (ts, vs) = store.read_time_range_of_with_expiry(ASPECT, crate::SeriesId::UNTAGGED, 0, 200, NOW_NS).await.expect("reads with expiry");
	// The expired row is filtered out, the visible row remains.
	assert_eq!(ts, vec![95i64], "only the visible row remains");
	assert_eq!(vs, vec![Some(bd("2"))], "only the visible value remains");
	drop(store);
	dir.close().expect("close temp dir");
}

// ── Criterion 6: Sweep correctly handles the age==TTL boundary ────────

#[tokio::test]
async fn sweep_boundary_age_equals_ttl_not_expired() {
	// Sweep must NOT count a row at the age==TTL boundary as expired.
	// Fully-expired shortcut must use < cutoff, not <= cutoff.
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS, unit: TimeUnit::Seconds }).await;
	store.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares");

	// Rows at t=90 (age 10 == TTL → NOT expired) and t=89 (age 11 > TTL → expired).
	// Same segment: min_ts=89, max_ts=90.
	// Ascending: the batch API takes timestamps in order.
	store.ingest(ASPECT, &IngestBatch::dense("s1", vec![89, 90], vec![bd("2"), bd("1")])).await.expect("ingests");

	let sweep = store.expiry_sweep(NOW_NS).await.expect("sweep");
	assert_eq!(sweep.rows_expired, 1, "only the row at t=89 (age > TTL) is expired");

	// Read path agrees: row at t=90 is visible, row at t=89 is hidden.
	let (ts, _) = store.read_time_range_of_with_expiry(ASPECT, crate::SeriesId::UNTAGGED, 0, 200, NOW_NS).await.expect("reads with expiry");
	assert!(ts.contains(&90), "boundary row (age == TTL) visible in read path");
	assert!(!ts.contains(&89), "row with age > TTL hidden in read path");

	drop(store);
	dir.close().expect("close temp dir");
}

#[tokio::test]
async fn sweep_segment_fully_below_cutoff_counts_all_as_expired() {
	// A segment where max_ts < cutoff is fully expired; all rows count.
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS, unit: TimeUnit::Seconds }).await;
	store.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares");
	// Row at t=80 (age 20 > 10) — cutoff = 90, max_ts 80 < 90 → fully expired.
	store.ingest(ASPECT, &IngestBatch::dense("s1", vec![80], vec![bd("1")])).await.expect("ingests");

	let sweep = store.expiry_sweep(NOW_NS).await.expect("sweep");
	assert_eq!(sweep.rows_expired, 1, "fully expired segment: all rows counted");

	drop(store);
	dir.close().expect("close temp dir");
}

// ── Criterion 7: Sweep is idempotent and reports correctly ────────────────

#[tokio::test]
async fn expiry_sweep_is_idempotent_and_report_only() {
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS, unit: TimeUnit::Seconds }).await;
	store.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares");
	store.ingest(ASPECT, &IngestBatch::dense("s1", vec![50, 95, 100], vec![bd("1"), bd("2"), bd("3")])).await.expect("ingests");

	// First sweep: rows at t=50 should be expired.
	let sweep1 = store.expiry_sweep(NOW_NS).await.expect("first sweep");
	assert!(sweep1.rows_expired >= 1, "first sweep reports expired rows");
	assert!(sweep1.aspects_cleaned.contains(&ASPECT.to_string()), "the cleaned aspect is reported");

	// Second sweep: same count (report-only, nothing was deleted).
	let sweep2 = store.expiry_sweep(NOW_NS).await.expect("second sweep");
	assert_eq!(sweep2.rows_expired, sweep1.rows_expired, "second sweep reports the same count as first (idempotent)");

	drop(store);
	dir.close().expect("close temp dir");
}

// ── Criterion 8: TTL < segment close window — sweep is safe (report-only) ─
// The sweep does not delete data, so it cannot invalidate any recent-window
// query. The report accurately distinguishes "nothing expired" from
// "K rows expired".

#[tokio::test]
async fn ttl_smaller_than_close_window_is_safe() {
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: 1, unit: TimeUnit::Seconds }).await; // 1 second TTL
	store.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares");
	// All rows at t=100 are at "now", so none should be expired.
	store.ingest(ASPECT, &IngestBatch::dense("s1", vec![100], vec![bd("1")])).await.expect("ingests");
	let sweep = store.expiry_sweep(NOW_NS).await.expect("sweep");
	assert_eq!(sweep.rows_expired, 0, "current rows are never expired");
	assert!(sweep.aspects_cleaned.is_empty(), "aspect not in cleaned list");

	// An expired row is reported separately.
	store.ingest(ASPECT, &IngestBatch::dense("s2", vec![95], vec![bd("2")])).await.expect("ingests an old row");
	let sweep2 = store.expiry_sweep(NOW_NS).await.expect("second sweep");
	assert!(sweep2.rows_expired > 0, "now an old row is found expired");

	// The sweep does not delete anything, so the old row is still readable
	// if we read without expiry filtering.
	let (ts, _) = store.read_time_range(ASPECT, 0, 200).await.expect("reads without expiry");
	assert_eq!(ts.len(), 2, "both rows still exist on disk");
	drop(store);
	dir.close().expect("close temp dir");
}

// ── RowExpiryPolicy helpers unit check (integration-level validation) ─────

#[tokio::test]
async fn expired_cutoff_works() {
	let cutoff = expired_cutoff(&RowExpiryPolicy::Ttl { ttl: 10, unit: TimeUnit::Seconds }, ns(100), &TimeUnit::Seconds);
	assert_eq!(cutoff, 90, "cutoff = 100 - 10");

	let no_cutoff = expired_cutoff(&RowExpiryPolicy::NoExpiry, ns(100), &TimeUnit::Seconds);
	assert_eq!(no_cutoff, i64::MIN, "NoExpiry cutoff is MIN");
}

// ── Cut 2 (#775): a purge removes the frames no read can use any more ────────
//
// The read-path filter of cut 1 made expired rows invisible; a purge makes the
// frames that hold *only* such rows leave the store. A frame that straddles the
// cutoff is counted and left alone — rewriting it is cut 3 — so a report says
// both what went and what could not go.

/// Everything under `segments/`, sorted: what a purge must physically remove and what it
/// must leave alone. Sorted because `read_dir` gives no order of its own.
fn frames(root: &std::path::Path) -> Vec<String> {
	let mut names = std::fs::read_dir(root.join("segments")).map(|dir| dir.filter_map(Result::ok).filter_map(|entry| entry.file_name().to_str().map(str::to_string)).collect::<Vec<_>>()).unwrap_or_default();
	names.sort();
	names
}

/// Dense batch of the instants `from..=to`, each valued by its own instant.
fn dense(from: i64, to: i64) -> IngestBatch {
	let timestamps = (from..=to).collect::<Vec<i64>>();
	let values = (from..=to).map(|t| bd(&t.to_string())).collect::<Vec<BigDecimal>>();
	IngestBatch::dense(format!("k{from}"), timestamps, values)
}

#[tokio::test]
async fn purge_drops_a_fully_expired_segment_and_keeps_a_straddling_one() {
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS, unit: TimeUnit::Seconds }).await;
	store.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares");
	// The cutoff at NOW_NS is instant 90. Frame A ends at 79, so a read that respects the
	// TTL has nothing left to lose from dropping it whole.
	store.ingest(ASPECT, &dense(60, 79)).await.expect("ingests the wholly expired batch");
	// Frame B straddles it: 85..=89 are expired, 90..=120 are not.
	store.ingest(ASPECT, &dense(85, 120)).await.expect("ingests the straddling batch");
	assert_eq!(store.index.all(ASPECT).await.expect("index").len(), 2, "each ingest seals its own frame");
	assert_eq!(frames(dir.path()).len(), 2, "two frames on disk");

	let before = store.read_time_range_of_with_expiry(ASPECT, crate::SeriesId::UNTAGGED, 0, 200, NOW_NS).await.expect("reads before the purge");
	let sweep = store.purge_expired(NOW_NS).await.expect("purges");
	assert!(sweep.busy.is_empty() && sweep.failed.is_empty(), "the aspect was neither busy nor failing: {:?}", sweep.failed.iter().map(|(aspect, err)| (aspect.clone(), err.to_string())).collect::<Vec<_>>());
	assert_eq!(sweep.segments_dropped, 1, "the wholly expired frame goes");
	assert_eq!(sweep.partial_segments, 1, "the straddling frame is counted, not rewritten");
	assert_eq!(sweep.rows_expired, 20 + 5, "the dropped frame's twenty rows plus the five expired rows inside the frame left behind");
	assert!(sweep.aspects_cleaned.contains(&ASPECT.to_string()), "the aspect is reported as cleaned");

	let after = store.read_time_range_of_with_expiry(ASPECT, crate::SeriesId::UNTAGGED, 0, 200, NOW_NS).await.expect("reads after the purge");
	assert_eq!(before, after, "dropping a frame no read could use changes no answer");
	assert_eq!(after.0.first().copied(), Some(90), "the first visible row is the first one at or past the cutoff");

	let rows = store.index.all(ASPECT).await.expect("index after the purge");
	assert_eq!(rows.len(), 1, "only the straddling frame keeps an index row");
	assert_eq!(rows[0].max_ts, Some(120), "the survivor is the frame that was kept");
	store.reap(super::MaintenanceWait::Skip).await.expect("reaps the retired frame");
	let left = frames(dir.path());
	assert_eq!(left.len(), 1, "the retired frame left the directory: {left:?}");

	// Idempotence in what it removes; the residue of a straddling frame is still counted,
	// because counting is not what a purge does to the store.
	let again = store.purge_expired(NOW_NS).await.expect("purges a second time");
	assert_eq!(again.segments_dropped, 0, "a purge that already ran drops nothing twice");
	assert_eq!(again.partial_segments, 1, "the straddling frame is still there to be counted");
	assert_eq!(again.rows_expired, 5, "and its expired rows are still expired");
	assert_eq!(again.aspects_cleaned, Vec::<String>::new(), "nothing cleaned, so nothing reported cleaned");
	assert_eq!(store.index.all(ASPECT).await.expect("index after the second purge").len(), 1, "the second pass wrote nothing");

	drop(store);
	dir.close().expect("close temp dir");
}

#[tokio::test]
async fn purge_keeps_a_frame_whose_newest_row_sits_exactly_at_the_cutoff() {
	// The shortcut is `max_ts < cutoff`, not `<=`: the row at the cutoff has age equal to
	// the TTL, which is not expired, so the frame that ends there must survive.
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS, unit: TimeUnit::Seconds }).await;
	store.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares");
	store.ingest(ASPECT, &dense(80, 90)).await.expect("ingests a batch ending at the cutoff");
	let sweep = store.purge_expired(NOW_NS).await.expect("purges");
	assert_eq!(sweep.segments_dropped, 0, "the newest row is at the cutoff, so nothing may be dropped");
	assert_eq!(sweep.partial_segments, 1, "the frame still straddles, and is counted as residue");
	assert_eq!(sweep.rows_expired, 10, "80..=89 are expired; 90 is not");
	assert_eq!(store.index.all(ASPECT).await.expect("index").len(), 1, "the frame keeps its index row");
	assert_eq!(frames(dir.path()).len(), 1, "the frame stays on disk");
	drop(store);
	dir.close().expect("close temp dir");
}

#[tokio::test]
async fn purge_writes_nothing_when_nothing_can_be_expired() {
	// No policy at all: the purge answers the empty report and opens no transaction.
	let (dir, store) = make_store(RowExpiryPolicy::NoExpiry).await;
	store.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares");
	store.ingest(ASPECT, &dense(1, 5)).await.expect("ingests");
	let sweep = store.purge_expired(NOW_NS).await.expect("purges a NoExpiry store");
	assert_eq!(sweep.segments_dropped, 0, "a store with no TTL drops nothing");
	assert_eq!(sweep.rows_expired, 0, "and counts nothing expired");
	assert_eq!(sweep.aspects_cleaned, Vec::<String>::new(), "no aspect was touched");

	// A TTL longer than the store's whole history: the cutoff is not representable, which
	// is the same answer as no expiry.
	let (dir2, store2) = make_store(RowExpiryPolicy::Ttl { ttl: 1_000_000, unit: TimeUnit::Seconds }).await;
	store2.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares");
	store2.ingest(ASPECT, &dense(1, 5)).await.expect("ingests");
	assert_eq!(expired_cutoff(&RowExpiryPolicy::Ttl { ttl: 1_000_000, unit: TimeUnit::Seconds }, NOW_NS, &TimeUnit::Seconds), i64::MIN, "the cutoff cannot be represented, so nothing is expired");
	let sweep = store2.purge_expired(NOW_NS).await.expect("purges a store whose TTL exceeds its history");
	assert_eq!(sweep.segments_dropped, 0, "an unrepresentable cutoff drops nothing");
	assert_eq!(store2.index().all(ASPECT).await.expect("index").len(), 1, "the frame keeps its index row");
	assert_eq!(frames(dir.path()).len(), 1, "the first store's directory is untouched");
	assert_eq!(frames(dir2.path()).len(), 1, "and so is the second's");

	drop(store);
	dir.close().expect("close temp dir");
	drop(store2);
	dir2.close().expect("close second temp dir");
}

#[tokio::test]
async fn purge_clears_every_aspect_it_is_asked_to() {
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS, unit: TimeUnit::Seconds }).await;
	store.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares");
	store.declare("other", &schema(TimeUnit::Seconds)).await.expect("declares the second aspect");
	store.ingest(ASPECT, &dense(50, 69)).await.expect("ingests into the first aspect");
	store.ingest("other", &dense(55, 69)).await.expect("ingests into the second aspect");
	let sweep = store.purge_expired(NOW_NS).await.expect("purges both aspects");
	assert_eq!(sweep.segments_dropped, 2, "one wholly expired frame per aspect");
	assert_eq!(sweep.rows_expired, 20 + 15, "both frames' rows count as expired");
	assert_eq!(sweep.aspects_cleaned.len(), 2, "both aspects are reported");
	assert_eq!(store.index.all(ASPECT).await.expect("index").len(), 0, "the first aspect keeps no frame");
	assert_eq!(store.index.all("other").await.expect("index").len(), 0, "and so does the second");
	drop(store);
	dir.close().expect("close temp dir");
}

/// Cut 3's environment switch: `WEFT_ROW_TTL` becomes a policy, and anything that is not a
/// positive count leaves expiry off. A typo in a TTL setting must never be able to empty a
/// store, which is why `0`, a negative and a malformed value all mean `NoExpiry` — the same
/// rule `maintenance_output_budget_from` applies to ROB-20's budgets.
#[test]
fn a_configured_row_ttl_becomes_a_policy_in_the_aspect_unit() {
	assert_eq!(super::row_expiry_policy_from(Some("3600")), RowExpiryPolicy::Ttl { ttl: 3600, unit: TimeUnit::Seconds }, "a positive count is the ttl, counted in seconds (the default unit for bare numbers)");
	assert_eq!(super::row_expiry_policy_from(Some(" 42 ")), RowExpiryPolicy::Ttl { ttl: 42, unit: TimeUnit::Seconds }, "the blanks around a value are the operator's, not part of the number");
	assert_eq!(super::row_expiry_policy_from(None), RowExpiryPolicy::NoExpiry, "unset means nothing ever expires");
	assert_eq!(super::row_expiry_policy_from(Some("0")), RowExpiryPolicy::NoExpiry, "zero means nothing ever expires: a mistyped zero must not empty the store");
	assert_eq!(super::row_expiry_policy_from(Some("")), RowExpiryPolicy::NoExpiry, "an empty value is a mistake, not a ttl of zero");
	assert_eq!(super::row_expiry_policy_from(Some("-1")), RowExpiryPolicy::NoExpiry, "a negative ttl is refused the same way");
	assert_eq!(super::row_expiry_policy_from(Some("1h")), RowExpiryPolicy::NoExpiry, "a value with a unit suffix is not a count: the store cannot know the aspect's unit at parse time");
}
// ── Red-evidence tests (#799): written against `origin/main`, ported to the fix ──

/// One policy, two aspects with different units, one wall clock.
///
/// On `origin/main` the TTL is a bare number interpreted in each aspect's own unit, so
/// `Ttl { ttl: 1 }` means one **second** for the Seconds aspect and one **nanosecond**
/// for the Nanos aspect. A Nanos row aged half a wall-clock second therefore expires
/// under a one-second TTL, and the assertion on that row **fails on `main`**.
#[tokio::test]
async fn two_aspects_one_policy_same_wall_clock_age() {
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: 1, unit: TimeUnit::Seconds }).await;
	store.declare("secs", &schema(TimeUnit::Seconds)).await.expect("declares the seconds aspect");
	store.declare("nanos", &schema(TimeUnit::Nanos)).await.expect("declares the nanos aspect");

	// Wall clock NOW_NS is 100 s. Aged two seconds in each unit.
	store.ingest("secs", &IngestBatch::dense("k1", vec![98], vec![bd("1")])).await.expect("ingests the seconds row");
	store.ingest("nanos", &IngestBatch::dense("k2", vec![98_000_000_000i64], vec![bd("2")])).await.expect("ingests the nanos row aged 2 s");
	// Half a wall-clock second old in the Nanos aspect: inside a one second TTL.
	store.ingest("nanos", &IngestBatch::dense("k3", vec![99_500_000_000i64], vec![bd("3")])).await.expect("ingests the nanos row aged 0.5 s");

	let (secs, _) = store.read_time_range_of_with_expiry("secs", crate::SeriesId::UNTAGGED, 0, 200, NOW_NS).await.expect("reads the seconds aspect");
	assert!(!secs.contains(&98), "Seconds aspect: a row aged 2 s is past a 1 s TTL");

	let (nanos, _) = store.read_time_range_of_with_expiry("nanos", crate::SeriesId::UNTAGGED, 0, 200_000_000_000, NOW_NS).await.expect("reads the nanos aspect");
	assert!(!nanos.contains(&98_000_000_000i64), "Nanos aspect: a row aged 2 s is past a 1 s TTL");
	assert!(nanos.contains(&99_500_000_000i64), "Nanos aspect: a row aged 0.5 s sits inside a 1 s TTL, so it must stay visible");

	let sweep = store.expiry_sweep(NOW_NS).await.expect("sweeps");
	assert_eq!(sweep.rows_expired, 2, "the sweep expires the same two rows the read hid");

	drop(store);
	dir.close().expect("close temp dir");
}

/// The read filter and the purge sweep must agree on a wall clock before 1970.
///
/// `origin/main` computed expiry two different ways: `expired_cutoff` compared the TTL
/// against a **nanosecond** now (so with `now_ns = -1` it saturated to `i64::MIN` and the
/// sweep dropped nothing) while `is_row_expired` compared an **aspect-unit** age (so the
/// read hid the row). One row, two answers, and the assertion on the read **fails on
/// `main`**. The fix routes both through one cutoff rule, so they must agree.
#[tokio::test]
async fn read_filter_and_purge_agree_for_pre_epoch_rows() {
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: 10, unit: TimeUnit::Seconds }).await;
	store.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares the aspect");
	store.ingest(ASPECT, &IngestBatch::dense("k1", vec![-100i64], vec![bd("1")])).await.expect("ingests a pre-epoch row");

	// One nanosecond before the epoch: in a Seconds aspect that truncates to 0, so the
	// row at -100 is 100 aspect ticks old and the 10-tick TTL has passed.
	let now_ns = -1i64;
	let (ts, _) = store.read_time_range_of_with_expiry(ASPECT, crate::SeriesId::UNTAGGED, -200, 200, now_ns).await.expect("reads with a pre-epoch now");
	assert!(ts.contains(&-100i64), "a now one nanosecond before the epoch truncates to 0 in a Seconds aspect, so the cutoff is unrepresentable and nothing expires -- the read path must keep the row the purge keeps");

	let cutoff = expired_cutoff(&RowExpiryPolicy::Ttl { ttl: 10, unit: TimeUnit::Seconds }, now_ns, &TimeUnit::Seconds);
	assert_eq!(is_row_expired(&RowExpiryPolicy::Ttl { ttl: 10, unit: TimeUnit::Seconds }, now_ns, -100, &TimeUnit::Seconds), -100 < cutoff, "the read filter and the purge sweep must use one rule");
	assert_eq!(cutoff, i64::MIN, "an unrepresentable cutoff means nothing is expired, on the read path as well as in the sweep");

	drop(store);
	dir.close().expect("close temp dir");
}

/// The strict boundary (a row exactly as old as the TTL is kept) in a non-Seconds aspect.
#[tokio::test]
async fn boundary_age_equals_ttl_not_expired_in_millis_aspect() {
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: 1, unit: TimeUnit::Seconds }).await;
	store.declare(ASPECT, &schema(TimeUnit::Millis)).await.expect("declares the millis aspect");

	// NOW_NS is 100 s = 100_000 ms; the TTL is 1 s = 1000 ms in this aspect.
	// t = 99_000 is exactly the TTL old; t = 98_999 is one millisecond older.
	store.ingest(ASPECT, &IngestBatch::dense("k1", vec![98_999i64, 99_000i64], vec![bd("2"), bd("1")])).await.expect("ingests both rows");

	let (ts, _) = store.read_time_range_of_with_expiry(ASPECT, crate::SeriesId::UNTAGGED, 0, 200_000, NOW_NS).await.expect("reads with expiry");
	assert!(ts.contains(&99_000i64), "a row at t=99000 ms is exactly the TTL old and must stay visible");
	assert!(!ts.contains(&98_999i64), "a row at t=98999 ms is past the TTL and must be hidden");

	let sweep = store.expiry_sweep(NOW_NS).await.expect("sweeps");
	assert_eq!(sweep.rows_expired, 1, "the sweep drops only the row past the boundary");

	drop(store);
	dir.close().expect("close temp dir");
}

/// A bare number in `WEFT_ROW_TTL` means seconds, and a suffix carries the operator's unit.
#[test]
fn ttl_suffix_grammar_is_seconds_by_default() {
	assert_eq!(super::row_expiry_policy_from(Some("3600")), RowExpiryPolicy::Ttl { ttl: 3600, unit: TimeUnit::Seconds }, "a bare number is seconds");
	assert_eq!(super::row_expiry_policy_from(Some("250ms")), RowExpiryPolicy::Ttl { ttl: 250, unit: TimeUnit::Millis }, "ms is milliseconds");
	assert_eq!(super::row_expiry_policy_from(Some("1500us")), RowExpiryPolicy::Ttl { ttl: 1500, unit: TimeUnit::Micros }, "us is microseconds");
	assert_eq!(super::row_expiry_policy_from(Some("7ns")), RowExpiryPolicy::Ttl { ttl: 7, unit: TimeUnit::Nanos }, "ns is nanoseconds");
	assert_eq!(super::row_expiry_policy_from(Some("4s")), RowExpiryPolicy::Ttl { ttl: 4, unit: TimeUnit::Seconds }, "an explicit s matches the default");
	assert_eq!(super::row_expiry_policy_from(Some("0")), RowExpiryPolicy::NoExpiry, "a zero TTL switches expiry off");
	assert_eq!(super::row_expiry_policy_from(Some("-1")), RowExpiryPolicy::NoExpiry, "a negative TTL switches expiry off");
	assert_eq!(super::row_expiry_policy_from(Some("1h")), RowExpiryPolicy::NoExpiry, "an unsupported unit suffix is refused, not guessed");
	assert_eq!(super::row_expiry_policy_from(Some("ms")), RowExpiryPolicy::NoExpiry, "a suffix with no number is refused");
}

/// A TTL finer than the aspect's tick saturates to zero: age > 0 expires, age 0 does not.
#[test]
fn ttl_finer_than_the_aspect_unit_saturates() {
	let fine = RowExpiryPolicy::Ttl { ttl: 1, unit: TimeUnit::Nanos };
	assert_eq!(fine.ttl_in_aspect_unit(&TimeUnit::Seconds), Some(0), "one nanosecond in a Seconds aspect is less than one tick, so it saturates to 0");
	assert_eq!(fine.ttl_in_aspect_unit(&TimeUnit::Nanos), Some(1), "in its own unit it stays one");
	let coarse = RowExpiryPolicy::Ttl { ttl: u64::MAX, unit: TimeUnit::Nanos };
	assert_eq!(coarse.ttl_in_aspect_unit(&TimeUnit::Nanos), Some(i64::MAX), "a TTL too wide for i64 saturates instead of wrapping");
}
