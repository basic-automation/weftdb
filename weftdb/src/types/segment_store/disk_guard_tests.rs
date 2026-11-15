//! ROB-17: a seal that would leave the store's filesystem below the disk
//! guard's reserve is refused before any frame is written. The reserve rides
//! [`SegmentStoreOptions`], so a test holds an impossible reserve (more than
//! any disk) and watches the refusal on a real filesystem, with no fake.

use std::{path::Path, str::FromStr};

use bigdecimal::BigDecimal;
use tempfile::TempDir;
use weft_physical_type::{timestamp::TimeUnit, AspectSchema, PhysicalType};

use super::{SegmentStore, SegmentStoreOptions};
use crate::{IngestBatch, StoreError};

/// The aspect every test here writes.
const ASPECT: &str = "price";

/// A reserve no filesystem can satisfy.
const IMPOSSIBLE: u64 = u64::MAX / 2;

fn values(count: i64) -> (Vec<i64>, Vec<BigDecimal>) {
	(0..count).map(|t| (t, BigDecimal::from_str(&t.to_string()).expect("parses"))).unzip()
}

async fn store_at(root: &Path, reserve: u64) -> SegmentStore {
	let store = SegmentStore::open_with_options(root, SegmentStoreOptions::default().with_disk_reserve_bytes(reserve)).await.expect("opens");
	store.declare(ASPECT, &AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds)).await.expect("declares");
	store
}

/// The `.weftseg` frames under the store's `segments/` directory.
fn frames(root: &Path) -> usize {
	std::fs::read_dir(root.join("segments")).map_or(0, |dir| dir.filter_map(Result::ok).filter(|entry| entry.path().extension().is_some_and(|ext| ext == "weftseg")).count())
}

#[test]
fn the_library_default_leaves_the_guard_off() {
	assert_eq!(SegmentStoreOptions::default().disk_reserve_bytes, 0, "a library open refuses nothing it did not refuse before ROB-17");
	assert_eq!(SegmentStoreOptions::default().with_disk_reserve_bytes(7).disk_reserve_bytes, 7);
}

#[tokio::test]
async fn an_ingest_below_the_reserve_writes_nothing() {
	let dir = TempDir::new().unwrap();
	let store = store_at(dir.path(), IMPOSSIBLE).await;
	let (ts, vs) = values(1_000);
	let err = store.ingest(ASPECT, &IngestBatch::dense("k1", ts, vs)).await.expect_err("the guard refuses");
	let refusal = err.downcast_ref::<StoreError>().expect("a typed refusal through the ingest's context");
	let StoreError::InsufficientStorage { reserve_bytes, need_bytes, available_bytes } = refusal else {
		panic!("expected InsufficientStorage, got {refusal:?}");
	};
	assert_eq!(*reserve_bytes, IMPOSSIBLE);
	assert!(*need_bytes > 0 && *available_bytes < IMPOSSIBLE);
	assert_eq!(frames(dir.path()), 0, "no frame was written");
	assert_eq!(store.segment_count(ASPECT).await.unwrap(), 0, "nothing was published");
	assert!(store.headroom().is_some_and(|headroom| headroom.refusing()), "the headroom report says the guard is refusing");
	// The refusal is not recorded against the key: the same batch lands once there is room.
	drop(store);
	let store = store_at(dir.path(), 1).await;
	let (ts, vs) = values(1_000);
	let receipt = store.ingest(ASPECT, &IngestBatch::dense("k1", ts, vs)).await.expect("admitted under a satisfiable reserve");
	assert!(!receipt.duplicate);
	assert_eq!(store.segment_count(ASPECT).await.unwrap(), 1);
	let headroom = store.headroom().expect("described");
	drop(store);
	assert!(!headroom.refusing());
	assert_eq!(headroom.writable_bytes, headroom.available_bytes - 1);
}

#[tokio::test]
async fn a_direct_seal_below_the_reserve_writes_nothing() {
	let dir = TempDir::new().unwrap();
	let store = store_at(dir.path(), IMPOSSIBLE).await;
	let (ts, vs) = values(100);
	let err = store.seal_declared(ASPECT, &ts, &vs).await.expect_err("the guard refuses");
	assert!(matches!(err.downcast_ref::<StoreError>(), Some(StoreError::InsufficientStorage { .. })), "{err:#}");
	let err = store.seal_declared_paged(ASPECT, &ts, &vs, 10).await.expect_err("the paged seal too");
	drop(store);
	assert!(matches!(err.downcast_ref::<StoreError>(), Some(StoreError::InsufficientStorage { .. })), "{err:#}");
	assert_eq!(frames(dir.path()), 0);
}

#[tokio::test]
async fn a_disabled_guard_still_reports_headroom() {
	let dir = TempDir::new().unwrap();
	let store = store_at(dir.path(), 0).await;
	let (ts, vs) = values(10);
	store.seal_declared(ASPECT, &ts, &vs).await.expect("no guard, no refusal");
	let headroom = store.headroom().expect("described");
	let guard = store.disk_guard();
	drop(store);
	assert_eq!((headroom.reserve_bytes, headroom.writable_bytes), (0, headroom.available_bytes));
	assert!(!headroom.refusing());
	assert!(!guard.is_enabled());
}

/// ROB-20 G2: a maintenance swap (the overlap merge) whose output the disk guard
/// would push below the reserve is refused before any byte is written, and the store
/// is unchanged afterwards. The two segments are sealed under a satisfiable reserve
/// and the store is reopened under an impossible one: the seal path has its own
/// admission, so sealing under `IMPOSSIBLE` would refuse the seal, not the merge.
#[tokio::test]
async fn a_maintenance_swap_refuses_below_the_reserve() {
	let dir = TempDir::new().unwrap();
	let store = store_at(dir.path(), 1).await;
	// Overlap in time but differ in content: the merge cannot take the skip-identical
	// path, so it has to write an output and the admission is reached.
	let (ts_a, vs_a) = values(100);
	store.seal_declared(ASPECT, &ts_a, &vs_a).await.expect("first seal");
	let ts_b: Vec<i64> = (50..150).collect();
	let vs_b: Vec<BigDecimal> = ts_b.iter().map(|t| BigDecimal::from(*t)).collect();
	store.seal_declared(ASPECT, &ts_b, &vs_b).await.expect("second seal");
	drop(store);

	let store = store_at(dir.path(), IMPOSSIBLE).await;
	let start_frames = frames(dir.path());
	let start_segments = store.segment_count(ASPECT).await.unwrap();
	assert_eq!(start_segments, 2, "two overlapping segments to merge");

	let err = store.reconcile_overlaps(ASPECT).await.expect_err("the guard refuses the merge before any write");
	assert!(matches!(err.downcast_ref::<StoreError>(), Some(StoreError::InsufficientStorage { .. })), "{err:#}");

	assert_eq!(frames(dir.path()), start_frames, "no output frame was left behind");
	assert_eq!(store.segment_count(ASPECT).await.unwrap(), start_segments, "the segment set is unchanged after the refusal");
	assert!(store.headroom().is_some_and(|headroom| headroom.refusing()), "the headroom report says the guard is refusing");
	drop(store);
}

/// ROB-20 G2: the same merge with ample headroom still runs, so the admission is not
/// an unconditional refusal. Partial overlap again, so this is the real write path
/// rather than a skip-identical keep.
#[tokio::test]
async fn a_maintenance_swap_succeeds_with_ample_headroom() {
	let dir = TempDir::new().unwrap();
	let store = store_at(dir.path(), 1).await;
	let (ts_a, vs_a) = values(100);
	store.seal_declared(ASPECT, &ts_a, &vs_a).await.expect("first seal");
	let ts_b: Vec<i64> = (50..150).collect();
	let vs_b: Vec<BigDecimal> = ts_b.iter().map(|t| BigDecimal::from(*t)).collect();
	store.seal_declared(ASPECT, &ts_b, &vs_b).await.expect("second seal");
	assert_eq!(frames(dir.path()), 2, "two segments before the merge");

	let removed = store.reconcile_overlaps(ASPECT).await.expect("the merge succeeds with headroom");
	assert_eq!(removed, 1, "two overlapping segments leave one behind");
	assert_eq!(store.segment_count(ASPECT).await.unwrap(), 1, "only one segment remains after the merge");
	drop(store);
}
