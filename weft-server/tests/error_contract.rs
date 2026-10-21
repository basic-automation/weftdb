//! The error contract, end to end: every refusal this server produces answers with the
//! same JSON shape, a code from the §4 table, and the request id in the body as well as
//! in the header.
//!
//! The unit tests inside `weft_server::error` pin the type. This file pins what a client
//! actually receives, because the type can be right while the wiring is not: a handler that
//! hand-rolls `Json(json!({"error": …}))` still compiles, still says roughly the right
//! thing, and still breaks a caller that switches on `code`. What matters here is that the
//! keys are the promised ones, that `code` is a §4 name rather than a one-off string, and
//! that `request_id` in the body is the id in the header — the one number an operator needs
//! to find a request in a log.

use std::collections::BTreeSet;

use axum::{
	body::Body, http::{HeaderMap, Request as HttpRequest, StatusCode}, Router
};
use tower::ServiceExt as _;
use weft_server::{app_with_state, AppState, HostGuard};

/// One way a client can be refused, and what the answer must be.
struct Refusal {
	/// The request to send.
	request: HttpRequest<Body>,
	/// The status the caller branches on.
	status: StatusCode,
	/// The §4 code name.
	code: &'static str,
}

/// The router a refusal should be sent to.
///
/// The guard refusals need the loopback guard installed; every other refusal is the plain
/// server. Building the router per request rather than sharing one keeps a guard that had
/// been moved outside the routes from quietly making the other cases pass.
fn router_for(code: &str) -> Router {
	if matches!(code, "host_not_allowed" | "cross_origin_write") {
		app_with_state(AppState::new().with_host_guard(HostGuard::Loopback))
	} else {
		app_with_state(AppState::new())
	}
}

/// The requests that cover each way a client can be refused: no store, a body the server
/// will not read, a route that does not exist, and a guard that will not answer. All of
/// them work without a store, a network or a fault-injection build.
fn refusals() -> Vec<Refusal> {
	vec![
		Refusal { request: HttpRequest::builder().method("GET").uri("/api/v1/nonsense").body(Body::empty()).unwrap(), status: StatusCode::NOT_FOUND, code: "route_not_found" },
		Refusal { request: HttpRequest::builder().method("POST").uri("/api/v1/interpolate").header("content-type", "application/json").body(Body::from("{ not json")).unwrap(), status: StatusCode::BAD_REQUEST, code: "invalid_json" },
		Refusal { request: HttpRequest::builder().method("POST").uri("/api/v1/interpolate").header("content-type", "text/plain").body(Body::from("resolution=seconds")).unwrap(), status: StatusCode::UNSUPPORTED_MEDIA_TYPE, code: "unsupported_media_type" },
		// A read and a write against a server with no store: the same code, because the
		// reason the request cannot be served is the same.
		Refusal { request: HttpRequest::builder().method("GET").uri("/api/v1/storage/temperature/points?start=1&end=2").body(Body::empty()).unwrap(), status: StatusCode::SERVICE_UNAVAILABLE, code: "unconfigured" },
		// A parameter the endpoint cannot read. The read routes take an epoch integer in the
		// aspect's declared unit, and `soon` is not one: until the envelope this came back as
		// axum's plain-text 400, so a client parsing the body got nothing to branch on.
		Refusal { request: HttpRequest::builder().method("GET").uri("/api/v1/storage/temperature/points?start=soon&end=2").body(Body::empty()).unwrap(), status: StatusCode::BAD_REQUEST, code: "bad_request" },
		Refusal {
			// A batch the extractor can read: the refusal is then the missing store, not a
			// body that failed to deserialize.
			request: HttpRequest::builder().method("POST").uri("/api/v1/storage/temperature/points").header("content-type", "application/json").body(Body::from(r#"{"points":[]}"#)).unwrap(),
			status: StatusCode::SERVICE_UNAVAILABLE,
			code: "unconfigured",
		},
		// A Host the loopback guard will not answer. The route exists, so the guard is the
		// only reason to refuse, and the code is what tells the caller that.
		Refusal { request: HttpRequest::builder().method("GET").uri("/health").header("host", "example.com").body(Body::empty()).unwrap(), status: StatusCode::FORBIDDEN, code: "host_not_allowed" },
		// A cross-site Origin on a method that writes.
		Refusal { request: HttpRequest::builder().method("POST").uri("/api/v1/interpolate").header("host", "127.0.0.1:8080").header("origin", "https://other.example").header("content-type", "application/json").body(Body::from("{}")).unwrap(), status: StatusCode::FORBIDDEN, code: "cross_origin_write" },
	]
}

/// Send `request` and return its status, headers and parsed body.
///
/// Fails the test if a refusal did not answer with JSON at all — that is how a plain-text
/// or empty-body error path would otherwise slip through, and the shape below only means
/// something if the body is the envelope.
async fn refuse(router: Router, request: HttpRequest<Body>) -> (StatusCode, HeaderMap, serde_json::Value) {
	let path = request.uri().path().to_string();
	let mut response = router.oneshot(request).await.expect("the router answers every request");
	let status = response.status();
	let headers = std::mem::take(response.headers_mut());
	let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024).await.expect("a body fits");
	let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("{status} answer for {path} is not JSON ({error}); body was {:?}", String::from_utf8_lossy(&bytes)));
	(status, headers, body)
}

#[tokio::test]
async fn a_refusal_carries_the_code_the_caller_branches_on() {
	for refusal in refusals() {
		let code = refusal.code;
		let (status, _headers, body) = refuse(router_for(code), refusal.request).await;
		assert_eq!(status, refusal.status, "{code}: {body}");
		assert_eq!(body["code"], code, "{status} {code}: {body}");
		assert!(body["error"].as_str().is_some_and(|text| !text.is_empty()), "{code}: a refusal says something: {body}");
	}
}

#[tokio::test]
async fn every_refusal_carries_exactly_the_envelope_keys() {
	// The keys are exact, not a subset: a field that quietly appears in one handler's error
	// is a field every client now has to tolerate, and a field that quietly disappears is a
	// field a client reads as absent-and-therefore-fine.
	for refusal in refusals() {
		let code = refusal.code;
		let (_status, headers, body) = refuse(router_for(code), refusal.request).await;
		let object = body.as_object().unwrap_or_else(|| panic!("{code}: envelope object, got {body}"));
		let keys: BTreeSet<&str> = object.keys().map(String::as_str).collect();
		assert_eq!(keys, BTreeSet::from(["error", "code", "request_id"]), "{code}: envelope keys, and none of them null: {body}");
		assert_eq!(headers["content-type"], "application/json", "{code}: a refusal is JSON: {body}");
	}
}

#[tokio::test]
async fn the_body_names_the_same_request_id_as_the_header() {
	// The id is the join between a client's error and an operator's log line, so it has to
	// be in both places, and be the same string in both.
	for refusal in refusals() {
		let code = refusal.code;
		let inbound = "req-caller-supplied-7";
		let mut request = refusal.request;
		request.headers_mut().insert("x-request-id", inbound.parse().unwrap());
		let (_status, headers, body) = refuse(router_for(code), request).await;
		assert_eq!(headers["x-request-id"], inbound, "{code}: the header carries the id");
		assert_eq!(body["request_id"], inbound, "{code}: and so does the body, which is what gets pasted into a report: {body}");
	}
}

#[tokio::test]
async fn a_generated_request_id_reaches_the_body_too() {
	// With no id from the caller the server mints one. The body has to carry *that* id: a
	// client that quotes only its own error still has to be connectable to a log line.
	let refusal = Refusal { request: HttpRequest::builder().method("GET").uri("/api/v1/nonsense").body(Body::empty()).unwrap(), status: StatusCode::NOT_FOUND, code: "route_not_found" };
	let (_status, headers, body) = refuse(router_for(refusal.code), refusal.request).await;
	let header_id = headers["x-request-id"].to_str().expect("an id in the header");
	assert!(header_id.starts_with("req-"), "a minted id looks like a minted id: {header_id}");
	assert_eq!(body["request_id"], header_id, "the body repeats it: {body}");
}

#[tokio::test]
async fn a_server_side_refusal_says_the_fixed_sentence() {
	// `unconfigured` is a 5xx, and a 5xx body says what the server could not do rather than
	// what went wrong inside it. Here the two are the same sentence by design: there is no
	// detail to hide, and the operator reading the body still learns the cause.
	let refusal = refusals().into_iter().find(|refusal| refusal.code == "unconfigured").expect("the unconfigured read");
	let (_status, _headers, body) = refuse(router_for("unconfigured"), refusal.request).await;
	assert_eq!(body["error"], "no segment store is configured on this server", "{body}");
}

#[tokio::test]
async fn the_internal_envelope_marker_never_reaches_a_client() {
	// `x-weft-error-envelope` is how `trace::request_span` knows a body is its own to
	// stamp. If it survived into the response it would be a header a client has to ignore,
	// and a hint about internals.
	for refusal in refusals() {
		let code = refusal.code;
		let (_status, headers, _body) = refuse(router_for(code), refusal.request).await;
		assert!(!headers.contains_key("x-weft-error-envelope"), "{code}: the marker is stripped: {headers:?}");
	}
}

#[tokio::test]
async fn the_dropped_synonyms_do_not_appear_in_any_answer() {
	// The §4 table drops these names because each one meant two different failures. Checked
	// against every refusal this file can produce, so a handler that re-invents one fails
	// here even when its own unit test passes.
	// `bad_request` is deliberately absent: it is the table's generic 400, which is what an
	// unreadable query parameter maps to. `not_found` and `too_large` are inventions the
	// table never had — it says `aspect_not_found` / `route_not_found` and
	// `result_too_large` / `payload_too_large`, because "not found" and "too large" each
	// name two different failures until you say what was not found and what was too large.
	const DROPPED: &[&str] = &["invalid_request", "unauthorized", "store_unconfigured", "maintenance_busy", "limit_exceeded", "panic", "segment_invalid", "not_found", "too_large"];
	for refusal in refusals() {
		let code = refusal.code;
		let (_status, _headers, body) = refuse(router_for(code), refusal.request).await;
		let name = body["code"].as_str().unwrap_or_default();
		assert!(!DROPPED.contains(&name), "{code}: {name} is a synonym the §4 table dropped: {body}");
	}
}

#[tokio::test]
async fn a_successful_answer_is_left_alone() {
	// Stamping must not touch a body that is not an envelope. `/health` is JSON with a
	// `status`, and its keys are a contract of their own: a readiness reader that finds a
	// `code` in a success would have to be rewritten for no reason.
	let (_status, headers, body) = refuse(app_with_state(AppState::new()), HttpRequest::builder().method("GET").uri("/health").body(Body::empty()).unwrap()).await;
	assert_eq!(body["status"], "ok", "{body}");
	assert!(body.get("code").is_none(), "a success is not an envelope: {body}");
	assert!(body.get("request_id").is_none(), "nor is it rewritten: {body}");
	assert!(headers["x-request-id"].to_str().is_ok_and(|id| id.starts_with("req-")), "the header is still set on successes: {headers:?}");
}

#[tokio::test]
async fn an_empty_route_table_still_answers_the_root() {
	// The fallback must not swallow the routes that do exist: `/ready` is the one a
	// container runtime polls, and a 404 there would read as a crashed server.
	let (_status, _headers, body) = refuse(app_with_state(AppState::new()), HttpRequest::builder().method("GET").uri("/ready").body(Body::empty()).unwrap()).await;
	assert_eq!(body["segment_store"], serde_json::Value::Bool(false), "a server with no store says so honestly: {body}");
	assert!(body.get("code").is_none(), "/ready is a probe, not an envelope: {body}");
}
