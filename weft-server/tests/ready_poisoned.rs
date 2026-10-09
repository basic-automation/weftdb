//! `GET /ready` reports the write poison of the segment store the server serves
//! (docs/design/crash-consistency.md section 5.1): `poisoned`, `restart_required` and the
//! reason, while `ready` stays `true`, because reads keep working. The handler reads the
//! poison from that store, so this poisons a real one (through weftdb's `fault-injection`
//! hook) rather than building a response by hand.
//!
//! Run with `cargo test -p weft-server --features fault-injection --test ready_poisoned`.

use std::sync::Arc;

use axum::{
	body::Body, http::{Request, StatusCode}, Router
};
use tower::ServiceExt;
use weft_server::{app_with_state, AppState};
use weftdb::SegmentStore;

/// `GET /ready` on `router`, as JSON.
async fn ready(router: Router) -> serde_json::Value {
	let response = router.oneshot(Request::builder().uri("/ready").body(Body::empty()).expect("builds the request")).await.expect("answers");
	assert_eq!(response.status(), StatusCode::OK);
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("reads the body");
	serde_json::from_slice(&bytes).expect("a JSON body")
}

#[tokio::test]
async fn ready_reports_the_poison_of_the_store_it_serves() {
	let reason = "segment_index transaction failed with an ambiguous COMMIT: COMMIT: I/O error (Other): sync";
	let dir = tempfile::TempDir::new().expect("tempdir");
	let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens the store"));
	let router = app_with_state(AppState::new().with_store(store.clone()));
	let healthy = ready(router.clone()).await;
	store.inject_poison(reason);
	let poisoned = ready(router).await;
	drop(store);

	assert_eq!((&healthy["segment_store"], &healthy["poisoned"], &healthy["restart_required"], &healthy["poison_reason"]), (&true.into(), &false.into(), &false.into(), &serde_json::Value::Null), "a fresh store is not poisoned: {healthy}");
	assert_eq!(poisoned["ready"], true, "reads keep working, so the server stays ready: {poisoned}");
	assert_eq!(poisoned["segment_store"], true);
	assert_eq!(poisoned["poisoned"], true, "{poisoned}");
	assert_eq!(poisoned["restart_required"], true, "{poisoned}");
	assert_eq!(poisoned["poison_reason"], reason, "{poisoned}");
}
