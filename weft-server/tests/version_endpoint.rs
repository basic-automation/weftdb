#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]
//! `GET /api/v1/version` (FRE-12b): which build this is, which store it serves, and what
//! its exit statuses mean.
//!
//! The body is assembled from process state, so these tests run it over the two states a
//! deployment is in — with a store and without — and over both sides of the scope pin. The
//! auth assertion is the one that matters for the exposure rule: with keys mounted this
//! route must answer `401`, because `/health` and `/ready` are the documented anonymous
//! surface and this route names layout numbers and the served scope.

use std::sync::Arc;

use axum::{
	body::Body, http::{header, Request, StatusCode}
};
use tower::ServiceExt;
use weft_server::{
	app_with_state, store_open::{ScopePin, ServingScope}, AppState, AuthConfig, KeyStore
};
use weftdb::SegmentStore;

/// `GET <uri>` on `router`, optionally carrying `token`, as `(status, body)`.
async fn get(router: axum::Router, uri: &str, token: Option<&str>) -> (StatusCode, serde_json::Value) {
	let mut builder = Request::builder().method("GET").uri(uri);
	if let Some(token) = token {
		builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
	}
	let response = router.oneshot(builder.body(Body::empty()).expect("builds the request")).await.expect("answers");
	let status = response.status();
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("reads the body");
	let body = if bytes.is_empty() { serde_json::Value::Null } else { serde_json::from_slice(&bytes).expect("a JSON body") };
	(status, body)
}

/// A store at a fresh root, created under `database`/`subject`.
async fn store_at(root: &std::path::Path, database: &str, subject: &str) -> Arc<SegmentStore> {
	Arc::new(SegmentStore::open_scoped(root, database, subject).await.expect("opens a store"))
}

#[tokio::test]
async fn a_store_reports_its_layout_and_the_scope_the_pin_reads() {
	let dir = tempfile::tempdir().expect("tempdir");
	let store = store_at(&dir.path().join("store"), "market", "BTCUSD").await;
	let router = app_with_state(AppState::new().with_store(store).with_serving_scope(ServingScope::new("market", "BTCUSD", ScopePin::Enforced)));

	let (status, body) = get(router, "/api/v1/version", None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["service"], "weft-server");
	assert!(body["version"].as_str().is_some_and(|v| !v.is_empty()), "the package version: {body}");
	assert_eq!(body["build"]["profile"], if cfg!(debug_assertions) { "debug" } else { "release" }, "the profile that changes every latency number");
	assert_eq!(body["build"]["os"], std::env::consts::OS);
	assert_eq!(body["store"]["configured"], true);
	assert_eq!(body["store"]["supported_layout"], weftdb::SUPPORTED_LAYOUT);
	assert_eq!(body["store"]["layout_version"], weftdb::SUPPORTED_LAYOUT, "a store this build created is in this build's layout");
	assert_eq!(body["store"]["writable_by_this_build"], true);
	assert_eq!(body["store"]["report_only"], false);
	assert_eq!(body["store"]["incompatible_store"], false);
	assert_eq!(body["store"]["previous_clean_shutdown"], false, "a store this process opened and never shut down has no clean shutdown behind it");
	assert!(body["store"]["store_uuid"].as_str().is_some_and(|uuid| uuid.len() == 36), "the identity /ready reports: {body}");
	assert_eq!(body["scope_pin"]["mode"], "enforced");
	assert_eq!(body["scope_pin"]["serving"], serde_json::json!({"database": "market", "subject": "BTCUSD"}));
	assert_eq!(body["scope_pin"]["recorded"], body["scope_pin"]["serving"], "a store created by this scope records it");
	assert_eq!(body["scope_pin"]["matches_store"], true);
}

#[tokio::test]
async fn a_stateless_server_says_so_and_still_answers() {
	let router = app_with_state(AppState::new());
	let (status, body) = get(router, "/api/v1/version", None).await;
	assert_eq!(status, StatusCode::OK, "the route is answered with no store, not 503: {body}");
	assert_eq!(body["store"]["configured"], false);
	assert!(body["store"]["store_uuid"].is_null(), "no store, no identity: {body}");
	assert_eq!(body["store"]["supported_layout"], weftdb::SUPPORTED_LAYOUT, "what this build knows is known either way");
	assert!(body["scope_pin"]["serving"].is_null() && body["scope_pin"]["recorded"].is_null() && body["scope_pin"]["matches_store"].is_null());
}

#[tokio::test]
async fn the_body_carries_the_exit_statuses_the_process_exits_with() {
	let (_, body) = get(app_with_state(AppState::new()), "/api/v1/version", None).await;
	let codes: Vec<i32> = body["exit_codes"].as_array().expect("a table").iter().map(|row| i32::try_from(row["code"].as_i64().expect("a code")).expect("an exit code fits an i32")).collect();
	assert_eq!(codes, [0, 1, 2, 65, 70, 75, 78], "plan C7's table, so a supervisor can read the number it just saw: {body}");
	let incompatible = body["exit_codes"].as_array().expect("a table").iter().find(|row| row["code"] == 65).expect("65 is in the table");
	assert!(incompatible["meaning"].as_str().is_some_and(|m| m.contains("scope mismatch")), "and 65 says a scope mismatch is one of its causes: {incompatible}");
}

#[tokio::test]
async fn a_pin_turned_off_leaves_the_mismatch_on_the_record() {
	// The store records the scope it was created under. Serving it under another is
	// what WEFT_STORE_SCOPE_PIN=off allows, so the body has to say so rather than
	// let the deployment look healthy.
	let dir = tempfile::tempdir().expect("tempdir");
	let root = dir.path().join("store");
	let store = store_at(&root, "market", "BTCUSD").await;
	store.shutdown().await.expect("shuts down");
	drop(store);
	let store = store_at(&root, "default", "default").await;
	assert!(store.previous_clean_shutdown(), "the open behind this one called shutdown, so the flag is set");

	let router = app_with_state(AppState::new().with_store(Arc::clone(&store)).with_serving_scope(ServingScope::new("default", "default", ScopePin::Off)));
	drop(store);
	let (status, body) = get(router, "/api/v1/version", None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["scope_pin"]["mode"], "off");
	assert_eq!(body["scope_pin"]["serving"], serde_json::json!({"database": "default", "subject": "default"}));
	assert_eq!(body["scope_pin"]["recorded"], serde_json::json!({"database": "market", "subject": "BTCUSD"}), "the store's own marker is what it disagrees with");
	assert_eq!(body["scope_pin"]["matches_store"], false);
}

#[tokio::test]
async fn the_route_takes_a_key_where_one_is_mounted() {
	let token = "weft_reader_token_for_version_tests";
	let keys = format!("{{\"version\": 1, \"keys\": [{{\"id\": \"reader-1\", \"role\": \"reader\", \"key_hash\": \"{}\"}}]}}", weft_server::auth::format_key_hash(token));
	let router = app_with_state(AppState::new().with_auth(AuthConfig::with_keys(KeyStore::parse(&keys).expect("keys parse"))));

	let (status, body) = get(router.clone(), "/api/v1/version", None).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED, "layout numbers and the served scope are not an anonymous answer: {body}");
	assert_eq!(body["code"], "unauthenticated");

	let (status, body) = get(router.clone(), "/api/v1/version", Some(token)).await;
	assert_eq!(status, StatusCode::OK, "a reader key sees the build and the store: {body}");
	assert_eq!(body["service"], "weft-server");

	// The documented exposure rule, unchanged by this route: the probes stay open.
	for probe in ["/health", "/ready"] {
		let (status, _) = get(router.clone(), probe, None).await;
		assert_eq!(status, StatusCode::OK, "{probe} stays anonymous");
	}
}
