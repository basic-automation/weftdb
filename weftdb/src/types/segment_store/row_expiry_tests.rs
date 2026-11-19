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
	segment_store::row_expiry::{expired_cutoff, RowExpiryPolicy}, IngestBatch, TagSet
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
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS }).await;
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
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS }).await;
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
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS }).await;
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
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS }).await;
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
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS }).await;
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
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS }).await;
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
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS }).await;
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
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: 1 }).await; // 1 second TTL
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
	let cutoff = expired_cutoff(&RowExpiryPolicy::Ttl { ttl: 10 }, ns(100), &TimeUnit::Seconds);
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
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS }).await;
	store.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares");
	// The cutoff at NOW_NS is instant 90. Frame A ends at 79, so a read that respects the
	// TTL has nothing left to lose from dropping it whole.
	store.ingest(ASPECT, &dense(60, 79)).await.expect("ingests the wholly expired batch");
	// Frame B straddles it: 85..=89 are expired, 90..=120 are not.
	store.ingest(ASPECT, &dense(85, 120)).await.expect("ingests the straddling batch");
	assert_eq!(store.index.all(ASPECT).await.expect("index").len(), 2, "each ingest seals its own frame");
	assert_eq!(frames(dir.path()).len(), 2, "two frames on disk");

	let before_plain = store.read_time_range(ASPECT, 0, 200).await.expect("plain read before the purge");
	let before_expiry = store.read_time_range_of_with_expiry(ASPECT, crate::SeriesId::UNTAGGED, 0, 200, NOW_NS).await.expect("reads before the purge");
	let sweep = store.purge_expired(NOW_NS).await.expect("purges");
	assert!(sweep.busy.is_empty() && sweep.failed.is_empty(), "the aspect was neither busy nor failing: {:?}", sweep.failed.iter().map(|(aspect, err)| (aspect.clone(), err.to_string())).collect::<Vec<_>>());
	assert_eq!(sweep.segments_dropped, 1, "the wholly expired frame goes");
	assert_eq!(sweep.partial_segments, 1, "the straddling frame is counted, not rewritten");
	assert_eq!(sweep.rows_expired, 20 + 5, "the dropped frame's twenty rows plus the five expired rows inside the frame left behind");
	assert!(sweep.aspects_cleaned.contains(&ASPECT.to_string()), "the aspect is reported as cleaned");

	let after_plain = store.read_time_range(ASPECT, 0, 200).await.expect("plain read after the purge");
	let after_expiry = store.read_time_range_of_with_expiry(ASPECT, crate::SeriesId::UNTAGGED, 0, 200, NOW_NS).await.expect("reads after the purge");
	// A plain read does not apply the TTL, so it could use those rows: dropping the wholly
	// expired frame must remove exactly its instants from a plain read and touch nothing else.
	assert!(before_plain.0.contains(&60) && before_plain.0.contains(&79), "a plain read sees the wholly expired frame before the purge");
	assert!(!after_plain.0.iter().any(|&ts| ts <= 79), "the dropped frame's instants leave the plain read: {:?}", after_plain.0.iter().filter(|&&ts| ts <= 79).copied().collect::<Vec<_>>());
	assert_eq!(after_plain.0.len(), before_plain.0.len() - 20, "exactly the dropped frame's twenty rows go, no more");
	assert_eq!(after_plain.0.first().copied(), Some(85), "the straddling frame still starts the plain read");
	assert_eq!(after_plain.0.last().copied(), Some(120), "and still ends it");
	assert_eq!(before_expiry, after_expiry, "TTL-filtered read is unchanged after dropping a frame no read could use");
	assert_eq!(after_expiry.0.first().copied(), Some(90), "the first visible row is the first one at or past the cutoff");

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
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS }).await;
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
	let (dir2, store2) = make_store(RowExpiryPolicy::Ttl { ttl: 1_000_000 }).await;
	store2.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares");
	store2.ingest(ASPECT, &dense(1, 5)).await.expect("ingests");
	assert_eq!(expired_cutoff(&RowExpiryPolicy::Ttl { ttl: 1_000_000 }, NOW_NS, &TimeUnit::Seconds), i64::MIN, "the cutoff cannot be represented, so nothing is expired");
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
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS }).await;
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

// ── Criterion #797: shadow-segment guard ──────────────────────────────────
// A doomed segment whose extent overlaps a surviving segment of the same series
// must be kept on disk; dropping it would silently revert a plain read to the
// superseded values.

#[tokio::test]
async fn purge_does_not_drop_segment_that_shadows_a_survivor() {
	// #797 core: two overlapping segments of one series exactly as the issue describes.
	// Segment 1 (survivor) straddles the cutoff; segment 2 (correction) is wholly
	// expired but overlaps segment 1's extent. After the purge the correction must
	// still be visible to plain reads.
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS }).await;
	store.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares");

	// Survivor: ts 60..=120 — max_ts=120 > cutoff=90 → straddles, kept.
	// The batch keys differ because one idempotency key may name one batch only; both
	// batches still write the same untagged series, which is what makes the second a shadow.
	store.ingest(ASPECT, &IngestBatch::dense("k60", (60..=120).collect(), (60..=120).map(|_| bd("1")).collect())).await.expect("ingests survivor");

	// Correction: ts 70..=79 — max_ts=79 < cutoff=90 → would be doomed, but
	// overlaps the survivor (both are series "s1").  The correction carries
	// a different value so we can distinguish which wins.
	store.ingest(ASPECT, &IngestBatch::dense("k70", (70..=79).collect(), (70..=79).map(|_| bd("42")).collect())).await.expect("ingests correction");

	let before = store.read_time_range(ASPECT, 0, 200).await.expect("reads before purge");
	let sweep = store.purge_expired(NOW_NS).await.expect("purges");

	// The correction segment was kept because it overlaps the survivor.
	assert_eq!(sweep.shadowed_and_kept, 1, "the correction segment shadows the survivor and is kept");
	assert_eq!(sweep.segments_dropped, 0, "no segment was dropped");
	assert!(sweep.failed.is_empty(), "no failure");

	let after = store.read_time_range(ASPECT, 0, 200).await.expect("reads after purge");
	assert_eq!(before, after, "plain read is unchanged — the correction values survive");

	// The correction values are visible, not the superseded survivor values.
	let (ts, vs) = &after;
	let idx_75 = ts.iter().position(|t| *t == 75).expect("ts 75 present");
	assert_eq!(vs[idx_75], Some(bd("42")), "the correction (newer segment) wins at overlapped timestamps");

	drop(store);
	dir.close().expect("close temp dir");
}

#[tokio::test]
async fn purge_still_drops_doomed_segment_that_overlaps_nothing() {
	// #797 no-over-refusal: a doomed segment that overlaps no survivor of its
	// series is still dropped — the guard does not disable the purge.
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS }).await;
	store.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares");

	// Two series, each with one wholly expired segment in a range with no survivor.
	store.ingest(ASPECT, &IngestBatch::dense("s1", vec![60, 65, 69], vec![bd("1"), bd("2"), bd("3")])).await.expect("ingests s1 expired");
	store.ingest(ASPECT, &IngestBatch::dense("s2", vec![50, 55, 59], vec![bd("4"), bd("5"), bd("6")])).await.expect("ingests s2 expired");

	let sweep = store.purge_expired(NOW_NS).await.expect("purges");
	assert_eq!(sweep.shadowed_and_kept, 0, "no segment kept due to shadowing");
	assert_eq!(sweep.segments_dropped, 2, "both fully expired segments are dropped");
	assert!(sweep.failed.is_empty(), "no failure");

	drop(store);
	dir.close().expect("close temp dir");
}

// ── Criterion #798: chunking by MAX_PLANS_PER_TXN ────────────────────────
// An aspect with more plans than `MAX_PLANS_PER_TXN` (32) used to be refused by
// `swap_plans` outright: "a swap of aspect "temp" carries at most 32 plans, not 33".
// The sweep now chunks the plans, so such an aspect is purged in several
// transactions instead of never at all.
//
// The series, not the idempotency key, decides how many plans a sweep carries:
// plans are bucketed by `series_id`, and an untagged ingest lands on
// `SeriesId::UNTAGGED` no matter how many distinct keys it used.  Every test here
// therefore gives each series its own tag group — without that, 33 ingests collapse
// into one plan and the ceiling is never reached, which is the mistake the first
// version of these tests made.

#[tokio::test]
async fn purge_drops_33_series_in_one_pass() {
	// 33 series, each with one wholly expired segment: 33 plans, one more than
	// the per-transaction ceiling.  On main this aspect fails with the
	// `swap_plans` guard instead of purging (see the red run recorded on #798);
	// after the fix all 33 segments drop across two chunks (32 + 1).
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS }).await;
	store.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares");

	for i in 1..=33u64 {
		let groups = TagSet::from_pairs([("host", format!("h{i}"))]).expect("valid tags");
		let key = format!("k{i}");
		store.ingest(ASPECT, &IngestBatch::dense(&key, vec![80], vec![bd(&i.to_string())]).with_groups(groups)).await.expect("ingests an expired row");
	}

	let sweep = store.purge_expired(NOW_NS).await.expect("purges");
	assert!(sweep.failed.is_empty(), "#798: no aspect should fail: {:?}", sweep.failed.iter().map(|(a, e)| (a.clone(), e.to_string())).collect::<Vec<_>>());
	assert_eq!(sweep.segments_dropped, 33, "#798: all 33 wholly expired segments are dropped, across two swap transactions");
	assert_eq!(sweep.shadowed_and_kept, 0, "nothing here overlaps a survivor, so the #797 guard keeps nothing");
	assert_eq!(store.index.all(ASPECT).await.expect("index").len(), 0, "no frames remain");

	drop(store);
	dir.close().expect("close temp dir");
}

#[tokio::test]
async fn purge_drops_65_series_across_three_chunks() {
	// Two full chunks and a remainder (32 + 32 + 1): the chunk loop has to
	// advance twice, and the last, short chunk still has to commit.  A loop that
	// stranded the tail would drop 64 and leave one frame behind, which the index
	// assertion below catches.
	let (dir, store) = make_store(RowExpiryPolicy::Ttl { ttl: TTL_SECS }).await;
	store.declare(ASPECT, &schema(TimeUnit::Seconds)).await.expect("declares");

	for i in 1..=65u64 {
		let groups = TagSet::from_pairs([("host", format!("h{i}"))]).expect("valid tags");
		let key = format!("k{i}");
		store.ingest(ASPECT, &IngestBatch::dense(&key, vec![80], vec![bd(&i.to_string())]).with_groups(groups)).await.expect("ingests an expired row");
	}

	let sweep = store.purge_expired(NOW_NS).await.expect("purges");
	assert!(sweep.failed.is_empty(), "#798: a 65-plan aspect should not fail: {:?}", sweep.failed.iter().map(|(a, e)| (a.clone(), e.to_string())).collect::<Vec<_>>());
	assert_eq!(sweep.segments_dropped, 65, "32 + 32 + 1 segments drop");
	assert_eq!(store.index.all(ASPECT).await.expect("index").len(), 0, "the short final chunk committed too");

	drop(store);
	dir.close().expect("close temp dir");
}

// #798's partial-failure branch (one chunk commits, a later chunk fails, the
// sweep reports `completed == false` while keeping the committed counts) has no
// automated coverage.  There is no seam to make the second chunk's `swap_plans`
// fail while the first one succeeds without the `fault-injection` feature, which
// runs in a CI job that does not run these library tests, and a test written to
// accept either outcome proves nothing.  Recorded on #798 as the known gap rather
// than papered over.

/// Cut 3's environment switch: `WEFT_ROW_TTL` becomes a policy, and anything that is not a
/// positive count leaves expiry off. A typo in a TTL setting must never be able to empty a
/// store, which is why `0`, a negative and a malformed value all mean `NoExpiry` — the same
/// rule `maintenance_output_budget_from` applies to ROB-20's budgets.
#[test]
fn a_configured_row_ttl_becomes_a_policy_in_the_aspect_unit() {
	assert_eq!(super::row_expiry_policy_from(Some("3600")), RowExpiryPolicy::Ttl { ttl: 3600 }, "a positive count is the ttl, counted in the aspect's own time unit");
	assert_eq!(super::row_expiry_policy_from(Some(" 42 ")), RowExpiryPolicy::Ttl { ttl: 42 }, "the blanks around a value are the operator's, not part of the number");
	assert_eq!(super::row_expiry_policy_from(None), RowExpiryPolicy::NoExpiry, "unset means nothing ever expires");
	assert_eq!(super::row_expiry_policy_from(Some("0")), RowExpiryPolicy::NoExpiry, "zero means nothing ever expires: a mistyped zero must not empty the store");
	assert_eq!(super::row_expiry_policy_from(Some("")), RowExpiryPolicy::NoExpiry, "an empty value is a mistake, not a ttl of zero");
	assert_eq!(super::row_expiry_policy_from(Some("-1")), RowExpiryPolicy::NoExpiry, "a negative ttl is refused the same way");
	assert_eq!(super::row_expiry_policy_from(Some("1h")), RowExpiryPolicy::NoExpiry, "a value with a unit suffix is not a count: the store cannot know the aspect's unit at parse time");
}
