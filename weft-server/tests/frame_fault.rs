//! What a client gets when a frame on disk cannot be decoded.
//!
//! It cannot be a unit test: `weftdb::FrameFaultError` has no public
//! constructor and `WeftSegError` is `#[non_exhaustive]`, precisely so that a status cannot be
//! invented here — the only way to reach these branches is a real file the store cannot read.
//!
//! Before D-S12 these two cases answered at read time (`500 frame_undecodable` for damage,
//! `503 incompatible_store` for a newer build). D-S12 supersedes that contract: startup
//! recovery (plan `C5`, §5 D-S12) quarantines the frame when the store opens, so the read
//! itself no longer meets the fault. Damage quarantines as corrupt and the read answers
//! `200` with the segment gone; a newer build quarantines as suspect, the read answers
//! `200` carrying `Weft-Incomplete`, and `/ready` answers `503 incompatible_store` — the
//! store answers as soon as the server is updated, which is why that code is the one
//! `/ready` uses to say "this store needs a newer server". Neither classification is
//! derived from the decoder's wording: the class comes from the fault object.

use std::{
	fs, path::{Path, PathBuf}, sync::Arc
};

use axum::{
	body::Body, http::{Request, StatusCode}
};
use bigdecimal::BigDecimal;
use tower::ServiceExt;
use weft_physical_type::{crc32, AspectSchema, PhysicalType, TimeUnit, FRAME_VERSION_MAX_KNOWN};
use weft_server::{app_with_state, AppState};
use weftdb::SegmentStore;

fn schema() -> AspectSchema {
	AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds)
}

/// A store holding one sealed frame of three points, closed at the end of the call.
///
/// The store is dropped on purpose: a frame read while it is still open can be served from a
/// cache, and the whole point is to make the decoder look at damaged bytes.
async fn seal_one_frame(dir: &Path) {
	let store = SegmentStore::open(dir).await.expect("opens the store");
	store.declare("price", &schema()).await.expect("declares the aspect");
	store.seal("price", &schema(), &[10, 20, 30], &[BigDecimal::from(1), BigDecimal::from(2), BigDecimal::from(3)]).await.expect("seals the frame");
	drop(store);
}

/// The single `.weftseg` the store wrote.
fn only_frame(dir: &Path) -> PathBuf {
	let segments = dir.join("segments");
	let mut names = fs::read_dir(&segments).expect("the segment directory exists").map(|entry| entry.expect("a directory entry").path()).filter(|path| path.extension().is_some_and(|ext| ext == "weftseg")).collect::<Vec<_>>();
	names.sort();
	assert_eq!(names.len(), 1, "one seal writes one frame, found {names:?}");
	names.remove(0)
}

/// Re-write `path` with `bytes`, re-stamping nothing: callers that change the body recompute the
/// trailer themselves so that each case says which checksum it expects to pass.
fn write_frame(path: &Path, bytes: &[u8]) {
	fs::write(path, bytes).expect("writes the frame back");
}

/// Re-open the store (running startup recovery) and serve it: the router the
/// tests below share, so `/ready` and the read see the same recovery pass.
async fn serve(dir: &Path) -> axum::Router {
	let store = SegmentStore::open(dir).await.expect("re-opens the store");
	app_with_state(AppState::new().with_store(Arc::new(store)))
}

/// `GET` a path on `router`, returning status, headers and the JSON body.
async fn get(router: &axum::Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
	let response = router.clone().oneshot(Request::builder().method("GET").uri(uri).body(Body::empty()).expect("builds the request")).await.expect("answers");
	let status = response.status();
	let headers = response.headers().clone();
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("reads the body");
	(status, headers, serde_json::from_slice(&bytes).expect("a JSON body"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frame_that_fails_its_checksum_is_quarantined_at_startup_and_reads_empty() {
	let dir = tempfile::TempDir::new().expect("tempdir");
	seal_one_frame(dir.path()).await;
	let frame = only_frame(dir.path());

	// Flip a byte in the middle of the body and leave the trailer alone: the checksum does not
	// match, which is the plain `integrity` case.
	let mut bytes = fs::read(&frame).expect("reads the frame");
	let middle = bytes.len() / 2;
	bytes[middle] ^= 0xff;
	write_frame(&frame, &bytes);

	// Startup recovery quarantines the corrupt segment, so the read no longer
	// meets the fault: it answers 200 with the segment gone.
	let router = serve(dir.path()).await;
	let (status, headers, body) = get(&router, "/api/v1/storage/price/points?start=0&end=100").await;
	assert_eq!(status, StatusCode::OK, "a quarantined segment is gone from reads: {body}");
	assert_eq!(body["points"], serde_json::json!([]), "{body}");
	assert_eq!(body["total"], 0, "{body}");
	assert!(headers.get("Weft-Incomplete").is_none(), "corrupt-only quarantine carries no incomplete header");
	// `/ready` counts the quarantine and stays ready: corruption is not a
	// version condition.
	let (status, _, body) = get(&router, "/ready").await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["quarantined"], 1, "{body}");
	assert_eq!(body["suspect"], 0, "{body}");
	assert_eq!(body["incompatible_store"], false, "{body}");
	assert_eq!(body["ready"], true, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frame_from_a_newer_build_is_suspect_at_startup_and_ready_reports_incompatible() {
	let dir = tempfile::TempDir::new().expect("tempdir");
	seal_one_frame(dir.path()).await;
	let frame = only_frame(dir.path());

	// Claim a version this build has never heard of, and re-stamp the trailer so the checksum
	// passes: the only thing wrong with the file is that it is newer than the server. The frame
	// header is `MAGIC | u16 version`, and the trailer is `crc32` over everything before it, so
	// both have to move together or the test would only be proving the case above.
	let mut bytes = fs::read(&frame).expect("reads the frame");
	let newer = FRAME_VERSION_MAX_KNOWN + 1;
	bytes[7..9].copy_from_slice(&newer.to_le_bytes());
	let end = bytes.len() - 4;
	let stamped = crc32(&bytes[..end]).to_le_bytes();
	bytes[end..].copy_from_slice(&stamped);
	write_frame(&frame, &bytes);

	// Startup recovery quarantines the newer-format frame as suspect: the read
	// answers 200 with the segment gone and carries `Weft-Incomplete`, and
	// `/ready` answers 503 with the store report.
	let router = serve(dir.path()).await;
	let (status, headers, body) = get(&router, "/api/v1/storage/price/points?start=0&end=100").await;
	assert_eq!(status, StatusCode::OK, "a quarantined segment is gone from reads: {body}");
	assert_eq!(body["points"], serde_json::json!([]), "{body}");
	assert_eq!(headers.get("Weft-Incomplete").and_then(|value| value.to_str().ok()), Some("true"), "a suspect record makes reads incomplete");
	let (status, _, body) = get(&router, "/ready").await;
	assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "a store this binary cannot read is a store/version condition: {body}");
	assert_eq!(body["ready"], false, "{body}");
	assert_eq!(body["incompatible_store"], true, "{body}");
	assert_eq!(body["suspect"], 1, "{body}");
}
