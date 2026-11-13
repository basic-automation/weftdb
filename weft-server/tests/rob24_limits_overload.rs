#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]
//! ROB-24: limits and overload evidence — the combined battery.
//!
//! The per-limit ceilings are each pinned where they live: the read
//! ceilings in `read_limits.rs`, the ingest caps in `ingest.rs`'s own
//! tests, the gate's shedding in `routes.rs`'s admission tests, the
//! connection cap in `serve_path.rs`, the disk guard in
//! `ready_identity.rs` and the store's own tests. What none of those
//! shows is the whole battery against the production router at once:
//! several limits driven past their ceilings in one run, the refusals
//! typed (never a `500`, never a dropped connection), the probes
//! answering throughout, and the server still serving afterwards.
//!
//! That is what this file pins, against `app_with_state` over a real
//! store, with the production caps configured small so the ceilings
//! are crossed by the parameters and not by slow hardware:
//!
//! - the query class sheds on the production router (`503 overloaded`
//!   with `Retry-After`), and the probes still answer — the wiring
//!   the gate tests prove on a synthetic registry, proved here on the
//!   registry the binary serves;
//! - two ingests whose declared bodies sum past the in-flight budget
//!   shed exactly one, and the admitted one still seals — the
//!   concurrent half of the write budget, which the single-request
//!   test in `ingest.rs` cannot show;
//! - the read ceilings, the ingest row cap and the disk guard, each
//!   refused with its code and its limit, in one run that ends with
//!   the server serving a normal read and a normal ingest again.
//!
//! Nothing here is a soak: the overload soak itself is the SOA lane's
//! (`crash-soak.sh`, SOA-17; the 24 h run, SOA-18), and this battery
//! is the bounded, in-process evidence that the limits hold under
//! load. The full table — every 7.6 limit, its knob, its refusal and
//! the test that proves it — is `docs/limits-overload-evidence.md`.

use std::{sync::Arc, time::Duration};

use axum::{
	body::Body, http::{Request, StatusCode}, Router
};
use bigdecimal::BigDecimal;
use tokio::sync::Notify;
use tower::ServiceExt as _;
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weft_server::{app_with_state, AdmissionConfig, AppState, IngestConfig, ReadLimits};
use weftdb::{SegmentStore, SegmentStoreOptions};

/// A store holding `rows` sealed rows of `temperature` at seconds
/// `1..=rows`, so every ceiling below is crossed by a parameter.
async fn store_with_rows(dir: &std::path::Path, rows: usize) -> Arc<SegmentStore> {
	let store = SegmentStore::open(dir).await.expect("opens the store");
	let schema = AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds);
	store.declare("temperature", &schema).await.expect("declares");
	let timestamps: Vec<i64> = (1..=i64::try_from(rows).expect("rows fit an i64")).collect();
	let values: Vec<BigDecimal> = timestamps.iter().map(BigDecimal::from).collect();
	store.seal("temperature", &schema, &timestamps, &values).await.expect("seals");
	Arc::new(store)
}

/// A store holding `price` declared (F64, seconds) and nothing sealed.
async fn store_with_price(dir: &std::path::Path) -> Arc<SegmentStore> {
	let store = SegmentStore::open(dir).await.expect("opens the store");
	let schema = AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds);
	store.declare("price", &schema).await.expect("declares");
	Arc::new(store)
}

/// `GET uri`, returning the status, the parsed JSON body and the
/// `Retry-After` header when the answer carries one.
async fn get(router: &Router, uri: &str) -> (StatusCode, serde_json::Value, Option<String>) {
	let response = router.clone().oneshot(Request::builder().uri(uri).body(Body::empty()).expect("builds the request")).await.expect("answers");
	let status = response.status();
	let retry_after = response.headers().get(axum::http::header::RETRY_AFTER).and_then(|value| value.to_str().ok()).map(str::to_string);
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("reads the body");
	(status, serde_json::from_slice(&bytes).expect("a JSON body"), retry_after)
}

/// `POST uri` with a JSON body, returning the status, the parsed JSON
/// body and the `Retry-After` header when the answer carries one.
async fn post_json(router: &Router, uri: &str, body: &serde_json::Value) -> (StatusCode, serde_json::Value, Option<String>) {
	let bytes = serde_json::to_vec(body).expect("a JSON body");
	let response = router.clone().oneshot(Request::builder().method("POST").uri(uri).header("content-type", "application/json").header("content-length", bytes.len().to_string()).body(Body::from(bytes)).expect("builds the request")).await.expect("answers");
	let status = response.status();
	let retry_after = response.headers().get(axum::http::header::RETRY_AFTER).and_then(|value| value.to_str().ok()).map(str::to_string);
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("reads the body");
	(status, serde_json::from_slice(&bytes).expect("a JSON body"), retry_after)
}

/// The refusal shape every ceiling below must produce: the status the
/// error table gives the code, the code itself, and the limit the
/// caller hit.
fn assert_refused(status: StatusCode, body: &serde_json::Value, expected: StatusCode, code: &str, limit: u64) {
	assert_eq!(status, expected, "{body}");
	assert_eq!(body["code"], code, "{body}");
	assert_eq!(body["limit"], limit, "the refusal carries the ceiling it hit: {body}");
}

/// The query class sheds on the production router, and the probes
/// still answer.
///
/// The gate's shedding semantics are pinned in `routes.rs` on a
/// synthetic registry with a slow fixture; what that cannot show is
/// that the registry the binary serves carries the admission at all.
/// A zero query cap sheds every query — and only queries.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_class_sheds_on_the_production_router_and_probes_still_answer() {
	let dir = tempfile::TempDir::new().expect("tempdir");
	let store = store_with_rows(dir.path(), 12).await;
	let router = app_with_state(AppState::new().with_store(store).with_admission_config(AdmissionConfig::with_max_query_inflight(0)));

	let (status, body, retry_after) = get(&router, "/api/v1/storage/temperature/points?start=1&end=12").await;
	assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
	assert_eq!(body["code"], "overloaded", "{body}");
	assert_eq!(retry_after.as_deref(), Some("1"), "a shed query carries Retry-After: 1");

	let (status, body, _) = get(&router, "/health").await;
	assert_eq!(status, StatusCode::OK, "the health probe is not a query: {body}");
	let (status, body, _) = get(&router, "/ready").await;
	assert_eq!(status, StatusCode::OK, "the readiness probe is not a query: {body}");
}

/// Two ingests whose declared bodies sum past the in-flight budget
/// shed exactly one, and the admitted one still seals.
///
/// The single-request half is pinned in `ingest.rs` (one body over
/// the budget is refused before its handler runs); the concurrent
/// half needs a hold. The first request's body is a stream the test
/// releases: its first poll happens inside the handler, which is
/// after the gate admitted it, so the wait for that poll proves the
/// first request holds the whole budget before the second arrives.
/// The refusal then depends on the declared lengths, not on timing:
/// the held request is admitted first by construction, and the
/// second — the same body again, past the budget — is shed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_ingests_past_the_inflight_budget_shed_the_second_and_the_first_still_seals() {
	let dir = tempfile::TempDir::new().expect("tempdir");
	let store = store_with_price(dir.path()).await;
	let batch = serde_json::json!({ "points": [
		{ "timestamp": 100, "value": "1.5" },
		{ "timestamp": 110, "value": "2.5" },
		{ "timestamp": 120, "value": "3.5" },
	] });
	let payload = serde_json::to_vec(&batch).expect("a JSON body");
	let budget = u64::try_from(payload.len()).expect("a payload fits a u64");
	let router = app_with_state(AppState::new().with_store(store).with_ingest_config(IngestConfig::with_max_inflight_bytes(budget)));

	// The held request: one body, delivered only when the test says
	// so, with a signal on its first poll — the poll is the proof its
	// gate admission already happened.
	let entered = Arc::new(Notify::new());
	let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
	let held_stream = futures::stream::unfold(Some((Arc::clone(&entered), release_rx, payload.clone())), |state| async move {
		let (entered, release, payload) = state?;
		entered.notify_one();
		release.await.ok()?;
		Some((Ok::<axum::body::Bytes, std::io::Error>(axum::body::Bytes::from(payload)), None))
	});
	let held_request = Request::builder().method("POST").uri("/api/v1/storage/price/points").header("content-type", "application/json").header("content-length", payload.len().to_string()).body(Body::from_stream(held_stream)).expect("builds the held request");
	let held = tokio::spawn(router.clone().oneshot(held_request));

	// The held request is admitted and holding the whole budget
	// before the second request is sent.
	tokio::time::timeout(Duration::from_secs(10), entered.notified()).await.expect("the held ingest reaches its handler");

	let (status, body, retry_after) = post_json(&router, "/api/v1/storage/price/points", &batch).await;
	assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
	assert_eq!(body["code"], "overloaded", "{body}");
	assert_eq!(retry_after.as_deref(), Some("1"), "a shed ingest carries Retry-After: 1");

	// Release the held body: it seals, and the refused one sealed
	// nothing — the read back finds exactly its three rows.
	let _sent = release_tx.send(());
	let held_response = tokio::time::timeout(Duration::from_secs(10), held).await.expect("the held ingest answers once released").expect("the held task answers").expect("a response");
	assert_eq!(held_response.status(), StatusCode::CREATED, "the admitted ingest seals");
	let (status, body, _) = get(&router, "/api/v1/storage/price/points?start=100&end=120").await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["count"], 3, "only the admitted ingest sealed: {body}");
}

/// The read ceilings, the ingest row cap and the disk guard, refused
/// with their codes and limits in one run — and the server still
/// serving a normal read and a normal ingest afterwards.
///
/// Each ceiling's own semantics are pinned where it lives
/// (`read_limits.rs`, `ingest.rs`); this battery is the combined run
/// on one production-shaped router, and the survival check at the end
/// is the point: overload answers typed refusals, it never takes the
/// server down with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_ceiling_battery_refuses_each_shape_and_the_server_survives() {
	let dir = tempfile::TempDir::new().expect("tempdir");
	let store = store_with_rows(dir.path(), 12).await;
	let router = app_with_state(AppState::new().with_store(Arc::clone(&store)).with_read_limits(ReadLimits::new(5, 10, 2, 2, Duration::from_secs(30))).with_ingest_config(IngestConfig::with_max_rows(2)));

	// A page over the ceiling is refused with the ceiling in it. The
	// window stays inside the row ceiling (8 rows of 10), so the page
	// ceiling is the one that bites — the handler counts the window
	// first.
	let (status, body, _) = get(&router, "/api/v1/storage/temperature/points?start=1&end=8&limit=6").await;
	assert_refused(status, &body, StatusCode::UNPROCESSABLE_ENTITY, "result_too_large", 5);
	assert!(body["error"].as_str().unwrap_or_default().contains("WEFT_MAX_PAGE_ROWS"), "{body}");

	// A window over the row ceiling is refused however it is asked for.
	let (status, body, _) = get(&router, "/api/v1/storage/temperature/points?start=1&end=12").await;
	assert_refused(status, &body, StatusCode::UNPROCESSABLE_ENTITY, "result_too_large", 10);
	assert!(body["error"].as_str().unwrap_or_default().contains("WEFT_MAX_READ_ROWS"), "{body}");

	// A grid over the bucket ceiling is refused from the request's own bounds.
	// The window stays inside the row ceiling (3 rows of 10), so the
	// bucket ceiling is the one that bites.
	let (status, body, _) = get(&router, "/api/v1/storage/temperature/downsample?start=1&end=3&resolution=1s").await;
	assert_refused(status, &body, StatusCode::UNPROCESSABLE_ENTITY, "result_too_large", 2);
	assert!(body["error"].as_str().unwrap_or_default().contains("WEFT_MAX_DOWNSAMPLE_BUCKETS"), "{body}");

	// A multi-instant batch over the cap is refused before any segment opens.
	let (status, body, _) = get(&router, "/api/v1/storage/temperature/at-multi?t=1,2,3").await;
	assert_refused(status, &body, StatusCode::UNPROCESSABLE_ENTITY, "result_too_large", 2);
	assert!(body["error"].as_str().unwrap_or_default().contains("WEFT_MAX_AT_MULTI_INSTANTS"), "{body}");

	// A batch over the row cap is refused before any segment is written.
	let batch = serde_json::json!({ "points": [
		{ "timestamp": 100, "value": "1.5" },
		{ "timestamp": 110, "value": "2.5" },
		{ "timestamp": 120, "value": "3.5" },
	] });
	let (status, body, _) = post_json(&router, "/api/v1/storage/temperature/points", &batch).await;
	assert_refused(status, &body, StatusCode::PAYLOAD_TOO_LARGE, "too_many_rows", 2);

	// A decoded column set over the byte cap is refused the same way,
	// with the cap as a field — the same router shape, only the
	// decoded ceiling lowered (three rows are 96 bytes of columns plus
	// their mantissas, over a 64-byte ceiling).
	let decoded_router = app_with_state(AppState::new().with_store(Arc::clone(&store)).with_ingest_config(IngestConfig::with_max_decoded_bytes(64)));
	let (status, body, _) = post_json(&decoded_router, "/api/v1/storage/temperature/points", &batch).await;
	assert_refused(status, &body, StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large", 64);

	// Survival: a normal read and a normal ingest still serve. The
	// read's window stays inside the row ceiling.
	let (status, body, _) = get(&router, "/api/v1/storage/temperature/points?start=1&end=8&limit=5").await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["count"], 5, "{body}");
	let small = serde_json::json!({ "points": [
		{ "timestamp": 100, "value": "1.5" },
		{ "timestamp": 110, "value": "2.5" },
	] });
	let (status, body, _) = post_json(&router, "/api/v1/storage/temperature/points", &small).await;
	assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// The disk guard refuses writes with `507 insufficient_storage`
/// while reads keep working and `/ready` stays `200`.
///
/// The reserve is impossible (`u64::MAX / 2`), so the guard refuses
/// wherever the OS reports free space — the same probe the guard
/// itself calls decides the expectation, following
/// `ready_identity.rs`, rather than the test naming a platform.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disk_guard_refuses_writes_with_507_and_reads_keep_working() {
	let dir = tempfile::TempDir::new().expect("tempdir");
	let measurable = weftdb::disk_space_at(dir.path()).is_some();
	let options = SegmentStoreOptions::default().with_disk_reserve_bytes(u64::MAX / 2);
	let store = Arc::new(SegmentStore::open_with_options(dir.path(), options).await.expect("opens the store"));
	let schema = AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds);
	store.declare("temperature", &schema).await.expect("declares");
	let router = app_with_state(AppState::new().with_store(Arc::clone(&store)));
	drop(store);

	let batch = serde_json::json!({ "points": [{ "timestamp": 100, "value": "1.5" }] });
	let (status, body, _) = post_json(&router, "/api/v1/storage/temperature/points", &batch).await;
	if measurable {
		assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE, "{body}");
		assert_eq!(body["code"], "insufficient_storage", "{body}");
	}

	let (status, body, _) = get(&router, "/api/v1/storage/temperature/points?start=1&end=12").await;
	assert_eq!(status, StatusCode::OK, "reads keep working under a refusing guard: {body}");

	let (status, body, _) = get(&router, "/ready").await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["ready"], true, "{body}");
	assert_eq!(body["disk_refusing"], measurable, "the probe reports what the guard can measure: {body}");
}
