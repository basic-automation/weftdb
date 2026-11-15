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
	BigDecimal::from_str(s).expect("parses")
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
	let (ts, _vs) = store.read_time_range_of_with_expiry(ASPECT, crate::SeriesId::UNTAGGED, 0, 200, NOW_NS).await.expect("reads with expiry");
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
	store.ingest(ASPECT, &IngestBatch::dense("s1", vec![90, 89], vec![bd("a"), bd("b")])).await.expect("ingests");
	let (ts, vs) = store.read_time_range_of_with_expiry(ASPECT, crate::SeriesId::UNTAGGED, 0, 200, NOW_NS).await.expect("reads with expiry");
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
	store.ingest(ASPECT, &IngestBatch::dense("s1", vec![85, 95], vec![bd("x"), bd("y")])).await.expect("ingests");

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
	store.ingest(ASPECT, &IngestBatch::dense("s1", vec![50, 95], vec![bd("old"), bd("new")])).await.expect("ingests both rows in one segment");
	let (ts, vs) = store.read_time_range_of_with_expiry(ASPECT, crate::SeriesId::UNTAGGED, 0, 200, NOW_NS).await.expect("reads with expiry");
	// The expired row is filtered out, the visible row remains.
	assert_eq!(ts, vec![95i64], "only the visible row remains");
	assert_eq!(vs, vec![Some(bd("new"))], "only the visible value remains");
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
	store.ingest(ASPECT, &IngestBatch::dense("s1", vec![90, 89], vec![bd("a"), bd("b")])).await.expect("ingests");

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
	store.ingest(ASPECT, &IngestBatch::dense("s1", vec![80], vec![bd("a")])).await.expect("ingests");

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
	store.ingest(ASPECT, &IngestBatch::dense("s1", vec![50, 95, 100], vec![bd("a"), bd("b"), bd("c")])).await.expect("ingests");

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
	store.ingest(ASPECT, &IngestBatch::dense("s1", vec![100], vec![bd("now")])).await.expect("ingests");
	let sweep = store.expiry_sweep(NOW_NS).await.expect("sweep");
	assert_eq!(sweep.rows_expired, 0, "current rows are never expired");
	assert!(sweep.aspects_cleaned.is_empty(), "aspect not in cleaned list");

	// An expired row is reported separately.
	store.ingest(ASPECT, &IngestBatch::dense("s2", vec![95], vec![bd("old")])).await.expect("ingests an old row");
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
