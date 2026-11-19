#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]
//! SEAL-IDEM-HTTP-RED: repro (or refute) issue #823.
//!
//! #823 claims that on a seal-backed aspect an HTTP POST retried with the
//! same `Idempotency-Key` stores the batch twice, because the key resolved at
//! `weft-server/src/ingest.rs:717` is never threaded into the seal gate
//! (`weftdb/src/types/legacy_seal.rs:290` derives a payload-based key from the
//! dataset and row-CRC, ignoring the caller-supplied one).
//!
//! ## What this test establishes
//!
//! **The claim does NOT reproduce.**  The HTTP path
//! (`POST /api/v1/storage/{aspect}/points`) calls
//! [`SegmentStore::ingest`] with `IngestBatch::new(key, …)`, where `key` **is**
//! the resolved [`Idempotency-Key`] header.  The store's ledger deduplicates
//! on this key: a retry with the same key and the same payload returns the
//! first receipt with `duplicate = true`, and no second write reaches the
//! segment store.  This mechanism is independent of
//! `WEFT_LEGACY_DEFAULT_STORAGE` and of
//! [`legacy_seal::ingest_batch_with_gate`], which is only reached through the
//! Database-level entry points (`batch_capture_measurements`,
//! `capture_new_measurement` etc.) — never from the HTTP handlers.
//!
//! #823's premise conflates two separate code paths:
//!
//! 1. **HTTP handler** → `SegmentStore::ingest(batch)` — threads the
//!    caller-supplied key.
//! 2. **Legacy Database path** → `legacy_seal::ingest_batch_with_gate` —
//!    derives a payload-based key and never receives an HTTP header.
//!
//! There is no convergence point on `origin/main` where the HTTP key could be
//! lost.  A future refactoring that merges the two paths must ensure the HTTP
//! idempotency key is forwarded, but that is not today's problem.

use std::sync::Arc;

use axum::{
	body::Body, http::{header, HeaderMap, Request, StatusCode}, Router
};
use serial_test::serial;
use tower::ServiceExt as _;
use weft_server::{app_with_state, AppState};
use weftdb::SegmentStore;

// ── Helpers ────────────────────────────────────────────────────────────────

async fn send(router: Router, request: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
	let response = router.oneshot(request).await.expect("the router answers every request");
	let status = response.status();
	let headers = response.headers().clone();
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("a body fits").to_vec();
	(status, headers, bytes)
}

async fn send_json(router: Router, request: Request<Body>) -> (StatusCode, HeaderMap, serde_json::Value) {
	let (status, headers, bytes) = send(router, request).await;
	let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("{status} answer is not JSON ({error}); body was {:?}", String::from_utf8_lossy(&bytes)));
	(status, headers, body)
}

/// The JSON points body both POSTs carry (three points at 10 s, 20 s, 30 s).
fn ingest_body() -> serde_json::Value {
	serde_json::json!({ "points": [
		{ "timestamp": 10, "value": "1.5" },
		{ "timestamp": 20, "value": "2.5" },
		{ "timestamp": 30, "value": "3.5" },
	] })
}

/// Build a POST request to `/api/v1/storage/price/points` with the given
/// body bytes and the fixed `Idempotency-Key`.
fn idem_post(key: &str, body_bytes: Vec<u8>) -> Request<Body> {
	Request::builder().method("POST").uri("/api/v1/storage/price/points").header(header::CONTENT_TYPE, "application/json").header("Idempotency-Key", key).body(Body::from(body_bytes)).unwrap()
}

// ── Test ───────────────────────────────────────────────────────────────────

#[serial]
#[tokio::test]
async fn http_idem_key_prevents_second_store_on_seal_backed_aspect() {
	// Enforce the seal-default world (#768), even though it does not affect
	// HTTP-declared aspects (they are always in the SegmentStore).
	std::env::set_var("WEFT_LEGACY_DEFAULT_STORAGE", "segments");

	let dir = tempfile::tempdir().expect("tempdir");
	let store = Arc::new(SegmentStore::open(dir.path().join("store")).await.expect("store opens"));
	let router = app_with_state(AppState::new().with_store(store));

	// 1. Declare the aspect via the HTTP API (→ SegmentStore::declare).
	let (status, _headers, body) = send_json(
		router.clone(),
		Request::builder()
			.method("POST")
			.uri("/api/v1/storage/aspects")
			.header(header::CONTENT_TYPE, "application/json")
			.body(Body::from(
				serde_json::to_vec(&serde_json::json!({
					"name": "price",
					"physical_type": "f64",
					"timestamp_unit": "seconds",
				}))
				.unwrap(),
			))
			.unwrap(),
	)
	.await;
	assert_eq!(status, StatusCode::CREATED, "aspect declaration: {body}");
	assert_eq!(body["aspect"]["name"], "price", "{body}");

	// 2. First POST with an explicit Idempotency-Key → 201 Created.
	let key = "seal-idem-http-repro-001";
	let body_bytes = serde_json::to_vec(&ingest_body()).unwrap();
	let (status1, _headers1, body1) = send_json(router.clone(), idem_post(key, body_bytes.clone())).await;
	assert_eq!(status1, StatusCode::CREATED, "first POST: {body1}");
	assert_eq!(body1["replayed"], false, "a fresh seal is not a replay: {body1}");
	assert_eq!(body1["row_count"], 3, "{body1}");
	let first_segment_id = body1["segment_id"].clone();

	// 3. Retry with the same Idempotency-Key and the same body → 200 OK,
	//    replayed=true, same segment_id.
	let (status2, headers2, body2) = send_json(router.clone(), idem_post(key, body_bytes)).await;
	assert_eq!(status2, StatusCode::OK, "the replay answered 200, not 201: {body2}");
	assert_eq!(headers2["Idempotency-Replayed"], "true", "the replay is marked in the header");
	assert_eq!(body2["replayed"], true, "the replay is signalled: {body2}");
	assert_eq!(body2["segment_id"], first_segment_id, "the replayed receipt answers the original segment id: {body2}");
	assert_eq!(body2["row_count"], 3, "{body2}");

	// 4. Read back the range: the batch must appear once, not twice.
	let (status3, _headers3, body3) = send_json(router, Request::builder().method("GET").uri("/api/v1/storage/price/points?start=0&end=100").body(Body::empty()).unwrap()).await;
	assert_eq!(status3, StatusCode::OK, "range read: {body3}");
	assert_eq!(body3["total"], 3, "a retried POST with one Idempotency-Key stored the batch twice on a seal-backed aspect (#823)");
	assert_eq!(body3["count"], 3, "the points array length matches: {body3}");

	std::env::remove_var("WEFT_LEGACY_DEFAULT_STORAGE");
}
