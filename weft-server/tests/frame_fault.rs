//! What a client gets when a frame on disk cannot be decoded.
//!
//! X-ERR gave `frame_undecodable` a sentence and X-FRAME gave it a producer, and this file is
//! where the two meet. It cannot be a unit test: `weftdb::FrameFaultError` has no public
//! constructor and `WeftSegError` is `#[non_exhaustive]`, precisely so that a status cannot be
//! invented here — the only way to reach these branches is a real file the store cannot read.
//!
//! The two cases are the two halves of plan `C5`. Damage is the store's problem and not the
//! caller's, so it is `500 frame_undecodable` with the `integrity` class: the request was fine,
//! the file is not, and quarantining it is a storage-layer decision that belongs to the slice
//! which verifies frames on read. A frame written by a newer build is the *binary's* problem, so
//! it is `503 incompatible_store` with the `newer_format` class — the same store answers as soon
//! as the server is updated, which is why the code is the one `/ready` already uses to say "this
//! store needs a newer server". Neither answer is derived from the decoder's wording: the class
//! comes from the fault object, and the sentence stays the fixed one X-ERR chose.

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

/// `GET` the range that covers the sealed points, and return the status plus the JSON body.
async fn read_range(dir: &Path) -> (StatusCode, serde_json::Value) {
	let store = SegmentStore::open(dir).await.expect("re-opens the store");
	let router = app_with_state(AppState::new().with_store(Arc::new(store)));
	let response = router.oneshot(Request::builder().method("GET").uri("/api/v1/storage/price/points?start=0&end=100").body(Body::empty()).expect("builds the request")).await.expect("answers");
	let status = response.status();
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("reads the body");
	(status, serde_json::from_slice(&bytes).expect("a JSON body"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frame_that_fails_its_checksum_answers_500_frame_undecodable() {
	let dir = tempfile::TempDir::new().expect("tempdir");
	seal_one_frame(dir.path()).await;
	let frame = only_frame(dir.path());

	// Flip a byte in the middle of the body and leave the trailer alone: the checksum does not
	// match, which is the plain `integrity` case.
	let mut bytes = fs::read(&frame).expect("reads the frame");
	let middle = bytes.len() / 2;
	bytes[middle] ^= 0xff;
	write_frame(&frame, &bytes);

	let (status, body) = read_range(dir.path()).await;
	assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "damaged data on disk is a server fault, not a bad request: {body}");
	assert_eq!(body["code"], "frame_undecodable", "{body}");
	assert_eq!(body["details"]["fault"], "integrity", "the fault class is what the caller branches on: {body}");
	assert_eq!(body["error"], "the server could not decode data it has on disk", "the sentence is X-ERR's fixed one, not the decoder's: {body}");
	// The decoder's own words are kept for the log, and the envelope's `error` field is not the
	// place they leak: this asserts the sanitisation rule holds on this path too.
	assert!(!body["error"].as_str().unwrap_or_default().contains("weftseg"), "the path does not belong in the body: {body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frame_from_a_newer_build_answers_503_incompatible_store() {
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

	let (status, body) = read_range(dir.path()).await;
	assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "a store this binary cannot read is a store/version condition: {body}");
	assert_eq!(body["code"], "incompatible_store", "{body}");
	assert_eq!(body["details"]["fault"], "newer_format", "the class says the binary is what is behind: {body}");
}
