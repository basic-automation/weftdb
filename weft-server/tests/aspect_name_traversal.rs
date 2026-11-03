//! SEC-11: aspect-name traversal, end to end over HTTP.
//!
//! An aspect name becomes part of every frame's file name (`segments/<aspect>-<id>`
//! in the legacy scheme), so a name carrying a path — `../x`, an absolute path, a
//! Windows separator — would put a frame outside the store root. The declare
//! validation (1.0 plan decision 9, X-NAME) refuses such names at
//! `POST /api/v1/storage/aspects`, and D-S10's containment refuses them again at
//! the store layer. The unit tests inside `weftdb::aspect_name` pin the rules and
//! the handler tests in `weft_server::declare` pin the first spellings; this file
//! pins what a client can actually do with every spelling, on every route that
//! takes an aspect name:
//!
//! - declaring any traversal spelling is a `400 invalid_aspect_name` naming the
//!   name and the rule it broke, and declares and writes nothing;
//! - a traversal spelling in the URL path (percent-encoded, backslash, absolute,
//!   or a literal `..` segment) is decoded exactly once and then treated as the
//!   aspect name it decodes to: undeclared, it is a plain `404 aspect_not_found`
//!   on every data route, and no route builds a path from it;
//! - an aspect a pre-validation build wrote straight into the catalog under a
//!   traversal name is refused with `400 invalid_aspect_name` on every route
//!   that would seal, read or maintain its frames;
//! - text that merely *looks* encoded (`%2e%2e%2fx` in a JSON body, or a
//!   double-encoded URL segment) is a literal name, never a path: it declares,
//!   and its frames land inside `segments/` like any other aspect's.

use std::{
	path::{Path, PathBuf}, sync::Arc
};

use axum::{
	body::Body, http::{Request, StatusCode}, Router
};
use tempfile::TempDir;
use tower::ServiceExt as _;
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weft_server::{app_with_state, AppState};
use weftdb::SegmentStore;

/// Send one request to `router`, returning the status and the parsed JSON body.
async fn call(router: Router, method: &str, uri: &str, body: Option<(&str, String)>) -> (StatusCode, serde_json::Value) {
	let mut builder = Request::builder().method(method).uri(uri);
	let payload = match body {
		Some((content_type, text)) => {
			builder = builder.header("content-type", content_type);
			Body::from(text)
		}
		None => Body::empty(),
	};
	let response = router.oneshot(builder.body(payload).expect("builds the request")).await.expect("the router answers");
	let status = response.status();
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("reads the body");
	(status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

fn json(text: String) -> Option<(&'static str, String)> {
	Some(("application/json", text))
}

fn declare_body(name: &str) -> Option<(&'static str, String)> {
	json(serde_json::json!({ "name": name, "physical_type": "f64", "timestamp_unit": "seconds" }).to_string())
}

const POINTS: &str = r#"{"points":[{"timestamp":0,"value":"1"}]}"#;

/// Every file anywhere under `dir`, recursively.
fn files_under(dir: &Path) -> Vec<PathBuf> {
	let mut found = Vec::new();
	for entry in std::fs::read_dir(dir).expect("reads the directory").map(Result::unwrap) {
		let path = entry.path();
		if path.is_dir() {
			found.extend(files_under(&path));
		} else {
			found.push(path);
		}
	}
	found
}

/// The frame files (`.weftseg` / `.weftpart`) anywhere under `dir`.
fn frames_under(dir: &Path) -> Vec<PathBuf> {
	files_under(dir).into_iter().filter(|path| path.extension().is_some_and(|ext| ext == "weftseg" || ext == "weftpart")).collect()
}

/// A store rooted at `<dir>/store` behind a router, plus the store itself.
async fn store_and_state(dir: &TempDir) -> (Arc<SegmentStore>, AppState) {
	let store = Arc::new(SegmentStore::open(dir.path().join("store")).await.expect("opens the store"));
	let state = AppState::new().with_store(store.clone());
	(store, state)
}

/// One aspect-addressed route: method, URI, and an optional `(content type, body)`.
type AspectRoute = (&'static str, String, Option<(&'static str, String)>);

/// Every route that takes an aspect name in its path, addressed at the encoded
/// aspect segment `enc`: the seals, the reads and the per-aspect maintenance.
fn aspect_routes(enc: &str) -> Vec<AspectRoute> {
	vec![("POST", format!("/api/v1/storage/{enc}/points"), json(POINTS.to_string())), ("POST", format!("/api/v1/storage/{enc}/ilp?field=v"), Some(("text/plain", "m v=1 0".to_string()))), ("POST", format!("/api/v1/storage/{enc}/csv"), Some(("text/plain", "0,1".to_string()))), ("GET", format!("/api/v1/storage/{enc}/points?start=0&end=10"), None), ("GET", format!("/api/v1/storage/{enc}/range?start=0&end=10"), None), ("GET", format!("/api/v1/storage/{enc}/range.csv?start=0&end=10"), None), ("GET", format!("/api/v1/storage/{enc}/at?t=0"), None), ("GET", format!("/api/v1/storage/{enc}/downsample"), None), ("GET", format!("/api/v1/storage/{enc}/value-points?lo=0&hi=1"), None), ("GET", format!("/api/v1/storage/{enc}/schema"), None), ("POST", format!("/api/v1/storage/{enc}/reconcile"), None), ("POST", format!("/api/v1/storage/{enc}/squash"), None), ("POST", format!("/api/v1/storage/{enc}/compact?target_rows=10"), None)]
}

/// Declare refuses every spelling of a name that could move a path, alias a
/// device, or hide what it is — decision 9's list (no `/` or `\`, not `.` or
/// `..`, no control characters, at most 160 encoded bytes) plus the rules the
/// module documents around it. Each refusal is the §4 `invalid_aspect_name`
/// envelope naming the aspect and the rule, and none of them declares or
/// writes anything.
#[tokio::test]
async fn declare_refuses_every_traversal_spelling() {
	let dir = TempDir::new().unwrap();
	let (store, state) = store_and_state(&dir).await;
	let overlong = "x".repeat(weftdb::aspect_name::MAX_ASPECT_NAME_BYTES + 1);
	let names: Vec<&str> = vec!["../x", "../../x", "..\\x", "..\\..\\x", "a/b", "a\\b", "a/../../x", "/tmp/x", "C:\\x", "C:/x", ".", "..", "...", ".hidden", "x/", "", "a\u{0}b", "a\nb", "a\u{7f}b", "trail.", "trail ", "CON", "nul:x", overlong.as_str()];
	for name in &names {
		let (status, body) = call(app_with_state(state.clone()), "POST", "/api/v1/storage/aspects", declare_body(name)).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "{name:?} must be refused; body: {body}");
		assert_eq!(body["code"], "invalid_aspect_name", "{name:?}: body: {body}");
		assert_eq!(body["details"]["aspect"], serde_json::json!(name), "{name:?}: the refusal names the aspect; body: {body}");
		assert!(body["details"]["reason"].as_str().is_some_and(|reason| !reason.is_empty()), "{name:?}: the refusal names the rule; body: {body}");
	}
	// The reason says which rule, so a client is not left parsing prose: a
	// separator is a separator, and the length cap reports the limit.
	let (status, body) = call(app_with_state(state.clone()), "POST", "/api/v1/storage/aspects", declare_body("a/b")).await;
	assert_eq!(status, StatusCode::BAD_REQUEST);
	assert!(body["details"]["reason"].as_str().unwrap().contains("`/`"), "body: {body}");
	let (_, body) = call(app_with_state(state.clone()), "POST", "/api/v1/storage/aspects", declare_body(&overlong)).await;
	assert!(body["details"]["reason"].as_str().unwrap().contains("160"), "body: {body}");

	let declared = store.list_declared_aspects().await.unwrap();
	assert!(declared.is_empty(), "a refused declare writes nothing: {declared:?}");
	assert!(frames_under(dir.path()).is_empty(), "no frame was written anywhere");
	// And nothing escaped the store root either: every file the server made is
	// the store's own, under `<dir>/store`.
	let store_root = dir.path().join("store");
	assert!(files_under(dir.path()).iter().all(|path| path.starts_with(&store_root)), "every file stays inside the store root");
}

/// A traversal name in the URL is a name, not a route escape: the router hands
/// the handler the segment decoded exactly once, the handler looks that name
/// up, and an undeclared one — however it was spelled — is the same
/// `404 aspect_not_found` as any other unknown aspect, on every data route.
/// Nothing is declared, sealed or read for it.
#[tokio::test]
async fn traversal_spellings_in_the_url_are_unknown_aspects() {
	let dir = TempDir::new().unwrap();
	let (store, state) = store_and_state(&dir).await;
	// Unix `../`, fully encoded dots, a Windows separator, an absolute path and
	// a climb to a real file: five spellings of names no store will create.
	for enc in ["..%2Fx", "%2e%2e%2fx", "..%5Cx", "%2Ftmp%2Fx", "..%2F..%2Fetc%2Fpasswd"] {
		for (method, uri, body) in aspect_routes(enc) {
			let (status, body) = call(app_with_state(state.clone()), method, &uri, body).await;
			assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}: body: {body}");
			assert_eq!(body["code"], "aspect_not_found", "{method} {uri}: body: {body}");
		}
		// Stats is the one read that answers for an undeclared aspect (it sums
		// the frames the store holds for the name); for a traversal name it
		// sums none, and reads nothing outside `segments/`.
		let (status, body) = call(app_with_state(state.clone()), "GET", &format!("/api/v1/storage/{enc}/stats"), None).await;
		assert_eq!(status, StatusCode::OK, "{enc} stats: {body}");
		assert_eq!(body["segment_count"], 0, "{enc} stats: {body}");
		assert_eq!(body["total_rows"], 0, "{enc} stats: {body}");
	}
	// A literal `..` segment is not normalised away by the router either: it
	// reaches the handler as the name `..` and is unknown like any other.
	let (status, body) = call(app_with_state(state.clone()), "GET", "/api/v1/storage/../points?start=0&end=10", None).await;
	assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
	assert_eq!(body["code"], "aspect_not_found", "body: {body}");
	// Decoding happens exactly once: a double-encoded segment names the aspect
	// literally called `%2e%2e%2fx`, which is also simply not declared.
	let (status, body) = call(app_with_state(state.clone()), "GET", "/api/v1/storage/%252e%252e%252fx/points?start=0&end=10", None).await;
	assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
	assert!(body["error"].as_str().unwrap().contains("`%2e%2e%2fx`"), "the name is decoded once, not twice: {body}");

	let declared = store.list_declared_aspects().await.unwrap();
	assert!(declared.is_empty(), "nothing was declared: {declared:?}");
	assert!(frames_under(dir.path()).is_empty(), "no frame was written anywhere");
}

/// The fixed route words (`aspects`, `catalog`, …) are not traversal either:
/// addressed through the `{aspect}` slot they are ordinary names, undeclared
/// here, so they answer `404 aspect_not_found` rather than reaching the fixed
/// route's handler or a path.
#[tokio::test]
async fn route_words_in_the_aspect_slot_are_unknown_aspects() {
	let dir = TempDir::new().unwrap();
	let (_store, state) = store_and_state(&dir).await;
	for word in ["aspects", "catalog", "stats", "reconcile", "backup"] {
		for suffix in ["schema", "points?start=0&end=10"] {
			let uri = format!("/api/v1/storage/{word}/{suffix}");
			let (status, body) = call(app_with_state(state.clone()), "GET", &uri, None).await;
			assert_eq!(status, StatusCode::NOT_FOUND, "GET {uri}: body: {body}");
			assert_eq!(body["code"], "aspect_not_found", "GET {uri}: body: {body}");
		}
	}
}

/// The dangerous case is a name that is already in the catalog: a build from
/// before the validation wrote `../x` straight in. Every route that would seal
/// into it, read its frames or maintain them refuses with
/// `400 invalid_aspect_name` — the name is never joined into a path — and no
/// frame appears anywhere.
#[tokio::test]
async fn a_previously_declared_traversal_name_is_refused_on_every_data_route() {
	let dir = TempDir::new().unwrap();
	let (store, state) = store_and_state(&dir).await;
	let schema = AspectSchema::new(PhysicalType::F64, "0".parse().unwrap(), TimeUnit::Seconds);
	store.catalog().declare(store.database(), store.subject(), "../x", &schema).await.expect("raw declare, as a pre-validation build wrote it");

	for (method, uri, body) in aspect_routes("..%2Fx") {
		// The schema route is the exception asserted below: it answers from the
		// catalog alone and never builds a frame path.
		if uri.ends_with("/schema") {
			continue;
		}
		let (status, body) = call(app_with_state(state.clone()), method, &uri, body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {uri}: body: {body}");
		assert_eq!(body["code"], "invalid_aspect_name", "{method} {uri}: body: {body}");
		assert_eq!(body["details"]["aspect"], "../x", "{method} {uri}: body: {body}");
	}
	// The two routes that never build a frame path answer from the catalog and
	// the segment listing alone: the schema echoes what the catalog holds, and
	// stats sums zero frames, because sealing was refused above.
	let (status, body) = call(app_with_state(state.clone()), "GET", "/api/v1/storage/..%2Fx/schema", None).await;
	assert_eq!(status, StatusCode::OK, "body: {body}");
	assert_eq!(body["aspect"]["name"], "../x", "body: {body}");
	let (status, body) = call(app_with_state(state.clone()), "GET", "/api/v1/storage/..%2Fx/stats", None).await;
	assert_eq!(status, StatusCode::OK, "body: {body}");
	assert_eq!(body["segment_count"], 0, "body: {body}");

	assert!(frames_under(dir.path()).is_empty(), "no frame was written anywhere");
}

/// The mirror image: percent-encoded text in a JSON body is not decoded at
/// all, and a double-encoded URL segment decodes once — so `%2e%2e%2fx` is a
/// lawful (if odd) aspect name whose frames live in `segments/` under their
/// encoded file names, exactly like an ordinary aspect's.
#[tokio::test]
async fn encoded_text_is_a_literal_name_not_a_path() {
	let dir = TempDir::new().unwrap();
	let (store, state) = store_and_state(&dir).await;
	for name in ["%2e%2e%2fx", "price"] {
		let (status, body) = call(app_with_state(state.clone()), "POST", "/api/v1/storage/aspects", declare_body(name)).await;
		assert_eq!(status, StatusCode::CREATED, "{name:?}: body: {body}");
		assert_eq!(body["aspect"]["name"], name, "body: {body}");
	}
	let (status, body) = call(app_with_state(state.clone()), "POST", "/api/v1/storage/price/points", json(POINTS.to_string())).await;
	assert_eq!(status, StatusCode::CREATED, "body: {body}");
	// The double-encoded segment addresses the aspect whose name is the
	// single-decoded text, and it reads as an ordinary empty aspect.
	let (status, body) = call(app_with_state(state.clone()), "GET", "/api/v1/storage/%252e%252e%252fx/points?start=0&end=10", None).await;
	assert_eq!(status, StatusCode::OK, "body: {body}");
	assert_eq!(body["aspect"], "%2e%2e%2fx", "body: {body}");

	// The one frame the seal wrote sits directly inside the store's
	// `segments/` directory — the containment every name in this file is about.
	let segments = dir.path().join("store").join("segments");
	let frames = frames_under(dir.path());
	assert_eq!(frames.len(), 1, "exactly the sealed frame exists: {frames:?}");
	assert_eq!(frames[0].parent(), Some(segments.as_path()), "the frame is a direct child of segments/: {frames:?}");
	let declared = store.list_declared_aspects().await.unwrap();
	assert_eq!(declared.len(), 2, "both literal names were declared: {declared:?}");
}
