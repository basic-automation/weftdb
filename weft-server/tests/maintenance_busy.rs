//! Maintenance operations on one aspect take turns (docs/design/crash-consistency.md
//! section 5.3, M1; slice S7). While another operation holds an aspect, the HTTP
//! maintenance endpoints wait for it, up to the store's maintenance wait, and then answer
//! `409 Conflict`, while a daemon tick skips it at once and maintains the other aspects.
//! The lock of a real store is held through weftdb's `fault-injection` hook, as a daemon
//! pass in progress would hold it.
//!
//! Run with `cargo test -p weft-server --features fault-injection --test maintenance_busy`.

use std::{
	sync::Arc, time::{Duration, Instant}
};

use axum::{
	body::Body, http::{Request, StatusCode}, Router
};
use bigdecimal::BigDecimal;
use tower::ServiceExt;
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weft_server::{app_with_state, reconcile_tick, AppState, Metrics, SharedMetrics};
use weftdb::SegmentStore;

/// How long the store under test waits for a busy aspect.
const WAIT: Duration = Duration::from_millis(200);

fn schema() -> AspectSchema {
	AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds)
}

/// `POST uri` on `router`, returning the status and the JSON body.
async fn post(router: Router, uri: &str) -> (StatusCode, serde_json::Value) {
	let response = router.oneshot(Request::builder().method("POST").uri(uri).body(Body::empty()).expect("builds the request")).await.expect("answers");
	let status = response.status();
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("reads the body");
	(status, serde_json::from_slice(&bytes).expect("a JSON body"))
}

/// A store whose aspects `price` and `temp` each hold two out-of-order segments that
/// overlap, so that every maintenance endpoint has work to do on either.
async fn store_with_work(dir: &std::path::Path) -> Arc<SegmentStore> {
	let store = SegmentStore::open(dir).await.expect("opens the store").with_maintenance_wait(WAIT);
	for aspect in ["price", "temp"] {
		store.declare(aspect, &schema()).await.expect("declares");
		store.seal(aspect, &schema(), &[30, 10, 20], &[BigDecimal::from(3), BigDecimal::from(1), BigDecimal::from(2)]).await.expect("seals");
		store.seal(aspect, &schema(), &[25, 15, 35], &[BigDecimal::from(5), BigDecimal::from(4), BigDecimal::from(6)]).await.expect("seals");
	}
	Arc::new(store)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_maintenance_endpoints_answer_409_while_another_operation_holds_the_aspect() {
	let dir = tempfile::TempDir::new().expect("tempdir");
	let store = store_with_work(dir.path()).await;
	let router = app_with_state(AppState::new().with_store(store.clone()));
	let held = store.hold_maintenance("price").await.expect("takes the aspect");

	let mut answers = Vec::new();
	for uri in ["/api/v1/storage/price/reconcile", "/api/v1/storage/price/reconcile?threshold=1", "/api/v1/storage/price/reconcile?hot_cold=true", "/api/v1/storage/price/reconcile?overlaps=true", "/api/v1/storage/price/squash", "/api/v1/storage/price/squash?max_segments=1", "/api/v1/storage/price/compact?target_rows=10", "/api/v1/storage/reconcile"] {
		let started = Instant::now();
		let (status, body) = post(router.clone(), uri).await;
		answers.push((uri, status, started.elapsed(), body));
	}
	let price_after = store.aspect_stats("price").await.expect("reads the stats");
	let temp_after = store.aspect_stats("temp").await.expect("reads the stats");
	drop(held);
	let (status, body) = post(router, "/api/v1/storage/price/reconcile").await;
	let price_released = store.aspect_stats("price").await.expect("reads the stats");
	drop(store);

	for (uri, status, elapsed, body) in answers {
		assert_eq!(status, StatusCode::CONFLICT, "{uri}: {body}");
		assert!(elapsed >= WAIT, "{uri} waited for the aspect before answering: {elapsed:?}");
		let error = body["error"].as_str().unwrap_or_default();
		assert!(error.contains("price") && error.contains("maintenance"), "{uri} names the busy aspect: {body}");
	}
	assert_eq!((price_after.segment_count, price_after.unsorted_segments), (2, 2), "nothing touched the busy aspect");
	assert_eq!(temp_after.unsorted_segments, 0, "the store-wide sweep still maintained the free aspect");
	assert_eq!(status, StatusCode::OK, "once the aspect is let go the endpoint works: {body}");
	assert_eq!(price_released.unsorted_segments, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_tick_skips_a_busy_aspect_without_waiting() {
	let dir = tempfile::TempDir::new().expect("tempdir");
	let store = store_with_work(dir.path()).await;
	let metrics: SharedMetrics = Arc::new(Metrics::default());
	let held = store.hold_maintenance("price").await.expect("takes the aspect");
	// The aspect stays held until the tick returns, so a tick that waited for it would
	// never finish.
	let sweep = tokio::time::timeout(Duration::from_secs(30), reconcile_tick(&store, &metrics, 1)).await.expect("the tick does not wait for the busy aspect").expect("the tick runs");
	let price = store.aspect_stats("price").await.expect("reads the stats").unsorted_segments;
	drop(held);
	let next = reconcile_tick(&store, &metrics, 1).await.expect("the next tick runs");
	drop(store);

	assert_eq!(sweep.busy, vec!["price".to_string()], "the tick skipped the busy aspect");
	assert_eq!(sweep.aspects_reconciled, 1, "and maintained the free one");
	assert!(sweep.failed.is_empty(), "a busy aspect is not a failure");
	assert_eq!(price, 2, "the busy aspect was left alone");
	assert_eq!((next.busy.len(), next.aspects_reconciled), (0, 1), "the next tick maintains it");
	assert_eq!(metrics.snapshot().reconcile.failed_passes, 0, "and nothing was counted as a failed pass");
}
