#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]
//! `/ready` carries the store identity and build info the plan assigns to
//! SOA-14/C-5 (§3, D-S12 row): `store_uuid`, durability class,
//! `previous_clean_shutdown` and the build version, beside the counts the
//! fault tests already pin.
//!
//! D-S12 landed those fields, and `frame_fault.rs` / `ready_poisoned.rs` pin
//! the counts, the poison report and the `503 incompatible_store` answer —
//! but nothing pinned the identity fields themselves, so a refactor could
//! drop or miswire one and no test would notice. The soak harness reads
//! `/ready` to know which store and which build it is soaking; this file is
//! the pin it relies on (SOA-14). The expected values are read from the
//! served store itself, never restated here, so the test fails exactly when
//! the response stops describing the store it serves.

use std::{path::Path, sync::Arc};

use axum::{
	body::Body, http::{Request, StatusCode}
};
use bigdecimal::BigDecimal;
use tower::ServiceExt as _;
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weft_server::{app, app_with_state, AppState, VERSION};
use weftdb::SegmentStore;

fn schema() -> AspectSchema {
	AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds)
}

/// A store holding one sealed frame, shut down cleanly at the end of the
/// call, so the re-open in the test observes a clean previous shutdown.
async fn seal_one_frame(dir: &Path) {
	let store = SegmentStore::open(dir).await.expect("opens the store");
	store.declare("price", &schema()).await.expect("declares the aspect");
	store.seal("price", &schema(), &[10], &[BigDecimal::from(1)]).await.expect("seals the frame");
	store.shutdown().await.expect("shuts down cleanly");
	drop(store);
}

/// `GET /ready` on `router`, returning status and the JSON body.
async fn get_ready(router: &axum::Router) -> (StatusCode, serde_json::Value) {
	let response = router.clone().oneshot(Request::builder().method("GET").uri("/ready").body(Body::empty()).expect("builds the request")).await.expect("answers");
	let status = response.status();
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("reads the body");
	(status, serde_json::from_slice(&bytes).expect("a JSON body"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_carries_the_store_identity_and_build_info() {
	let dir = tempfile::TempDir::new().expect("tempdir");
	seal_one_frame(dir.path()).await;

	// Re-open the store and read the identity from the store itself: the
	// response has to describe this store, not just carry plausible values.
	let store = Arc::new(SegmentStore::open(dir.path()).await.expect("re-opens the store"));
	let store_uuid = store.store_uuid().to_string();
	let previous_clean_shutdown = store.previous_clean_shutdown();
	assert!(previous_clean_shutdown, "the seal step shut down cleanly, so the re-open must observe it");
	let router = app_with_state(AppState::new().with_store(Arc::clone(&store)));
	drop(store);

	let (status, body) = get_ready(&router).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["ready"], true, "{body}");
	assert_eq!(body["segment_store"], true, "{body}");
	assert_eq!(body["service"], "weft-server", "{body}");
	// Build info (SOA-14/C-5): the running build's version.
	assert_eq!(body["version"], VERSION, "{body}");
	// Store identity (SOA-14/C-5): the served store's own values.
	assert_eq!(body["store_uuid"], store_uuid, "{body}");
	assert_eq!(body["durability_class"], SegmentStore::durability_class(), "{body}");
	assert_eq!(body["previous_clean_shutdown"], previous_clean_shutdown, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_without_a_store_carries_build_info_and_no_identity() {
	let (status, body) = get_ready(&app()).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["ready"], true, "{body}");
	assert_eq!(body["segment_store"], false, "{body}");
	// The build info is carried with or without a store; the identity fields
	// belong to the store and stay null while none is configured.
	assert_eq!(body["version"], VERSION, "{body}");
	assert_eq!(body["store_uuid"], serde_json::Value::Null, "{body}");
	assert_eq!(body["durability_class"], serde_json::Value::Null, "{body}");
	assert_eq!(body["previous_clean_shutdown"], serde_json::Value::Null, "{body}");
}

/// ROB-17: `/ready` says whether the disk guard refuses writes, as a boolean, and stays
/// `200 ready` while it does — reads keep working. An impossible reserve refuses wherever the
/// OS reports free space (`statvfs` on Unix, `GetDiskFreeSpaceExW` on Windows); the guard is
/// inert only where the probe answers `None`, and a reserve of 0 turns it off. Rather than
/// naming platforms, the test asks the same probe the guard uses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_reports_a_refusing_disk_guard_and_stays_ready() {
	for reserve in [u64::MAX / 2, 0] {
		let dir = tempfile::tempdir().expect("temp dir");
		// What the guard can refuse is what the guard can measure: the assertion follows
		// `disk_space_at`, the probe the guard itself calls, so a platform that gains or
		// loses free-space measurement moves the expectation instead of the test.
		let measurable = weftdb::disk_space_at(dir.path()).is_some();
		let expected = reserve > 0 && measurable;
		let store = SegmentStore::open_with_options(dir.path(), weftdb::SegmentStoreOptions::default().with_disk_reserve_bytes(reserve)).await.expect("opens");
		let router = app_with_state(AppState::new().with_store(Arc::new(store)));
		let (status, body) = get_ready(&router).await;
		assert_eq!(status, StatusCode::OK, "{body}");
		assert_eq!(body["ready"], true, "{body}");
		assert_eq!(body["disk_refusing"], expected, "reserve {reserve}, probe measurable: {measurable} — `disk_refusing` must match what the guard can measure. {body}");
	}
	let (_, body) = get_ready(&app()).await;
	assert_eq!(body["disk_refusing"], false, "no store, nothing to refuse: {body}");
}
