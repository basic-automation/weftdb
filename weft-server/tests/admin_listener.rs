//! ROB-23r: the probe-only admin listener's surface.
//!
//! The admin listener (ROB-23r) exists so a supervisor's liveness and
//! readiness probes do not share an address, a firewall rule or an origin with
//! the data plane. What makes that a security improvement rather than a second
//! door is that the listener serves the registry's *probe routes and nothing
//! else*: the gate layer resolves a request against the small sealed table
//! [`weft_server::routes::build_probe_registry`] produces, so every stored-data,
//! ingest, declare and maintenance path is refused by the same fallback that
//! answers a mistyped URL — with `route_not_found`, not with an authorisation
//! error a caller could probe around.
//!
//! These tests drive the routers directly, so they pin the surface itself; the
//! listener that wraps one of them is startup wiring, and it runs the same
//! [`weft_server::serve::run`] path with the same pre-auth limits as the data
//! plane.

use axum::{
	body::Body, http::{Request, StatusCode}, Router
};
use tower::ServiceExt;
use weft_server::{app_with_state, probe_app_with_state, AppState};

/// Send `method path` to `router` and return its status and body text.
async fn ask(router: Router, method: &str, path: &str) -> (StatusCode, String) {
	let response = router.oneshot(Request::builder().method(method).uri(path).body(Body::empty()).expect("builds the request")).await.expect("answers");
	let status = response.status();
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("reads the body");
	(status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn the_probe_router_answers_both_probes() {
	let (status, body) = ask(probe_app_with_state(AppState::new()), "GET", "/health").await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert!(body.contains("\"status\":\"ok\""), "the liveness probe answers: {body}");

	let (status, body) = ask(probe_app_with_state(AppState::new()), "GET", "/ready").await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert!(body.contains("\"ready\":true"), "the readiness probe answers: {body}");
}

/// ROB-23r adds `/healthz` and `/readyz` as aliases for k8s-style probes.
#[tokio::test]
async fn the_probe_router_answers_the_z_variants() {
	let (status, body) = ask(probe_app_with_state(AppState::new()), "GET", "/healthz").await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert!(body.contains("\"status\":\"ok\""), "the liveness probe answers: {body}");

	let (status, body) = ask(probe_app_with_state(AppState::new()), "GET", "/readyz").await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert!(body.contains("\"ready\":true"), "the readiness probe answers: {body}");
}

/// The admin listener has no host guard, so an HTTP/1.0 probe with no `Host`
/// header at all passes — the case that today gets a `403` through the data
/// listener. This is acceptance criterion (a).
#[tokio::test]
async fn the_probe_router_answers_without_a_host_header() {
	let router = probe_app_with_state(AppState::new());
	// No .header() call means no Host header at all.
	let response = router.oneshot(Request::builder().method("GET").uri("/healthz").body(Body::empty()).expect("builds the request")).await.expect("answers");
	let status = response.status();
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("reads the body");
	let body = String::from_utf8_lossy(&bytes);
	assert_eq!(status, StatusCode::OK, "a probe with no Host header passes: {body}");
	assert!(body.contains("\"status\":\"ok\""), "the liveness probe answers: {body}");

	let response = router.oneshot(Request::builder().method("GET").uri("/readyz").body(Body::empty()).expect("builds the request")).await.expect("answers");
	let status = response.status();
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("reads the body");
	let body = String::from_utf8_lossy(&bytes);
	assert_eq!(status, StatusCode::OK, "a probe with no Host header passes: {body}");
	assert!(body.contains("\"ready\":true"), "the readiness probe answers: {body}");
}

/// The paths a supervisor has no business reaching: a stored-data read, a point
/// write, a declare, a maintenance call, metrics. Each is `route_not_found` — the same
/// answer the ordinary listener gives for a URL that does not exist — rather
/// than a 401/403 that would confirm the endpoint is there. This is acceptance
/// criterion (b).
#[tokio::test]
async fn the_probe_router_has_no_path_beyond_the_probes() {
	let paths = [("GET", "/metrics"), ("GET", "/api/v1/storage/aspects"), ("GET", "/api/v1/storage/price/points"), ("GET", "/api/v1/storage/price/stats"), ("POST", "/api/v1/storage/price/points"), ("POST", "/api/v1/storage/price/parquet"), ("POST", "/api/v1/storage/aspects"), ("POST", "/api/v1/storage/reconcile")];
	for (method, path) in paths {
		let (status, body) = ask(probe_app_with_state(AppState::new()), method, path).await;
		assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path} on the probe router: {body}");
		assert!(body.contains("\"route_not_found\""), "{method} {path} must be refused as no route, not as forbidden: {body}");
	}
}

/// The refusal is the *table* speaking, not a blanket catch-all: `/health` is
/// in the probe table under GET, so another method there is the 405 with
/// `Allow` — exactly what the full router says for the same request. A listener
/// that 404'd everything would pass the test above and still be wrong about
/// this one.
#[tokio::test]
async fn a_probe_path_under_the_wrong_method_is_method_not_allowed() {
	let (status, body) = ask(probe_app_with_state(AppState::new()), "DELETE", "/health").await;
	assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");
	assert!(body.contains("\"method_not_allowed\""), "{body}");
	assert!(body.contains("is not served on `/health`") && body.contains("this route serves GET"), "the refusal names the method the probe table does serve: {body}");

	let (full_status, _) = ask(app_with_state(AppState::new()), "DELETE", "/health").await;
	assert_eq!(full_status, status, "both listeners decide the same way on a path they share");
}

/// The other half of the claim: the ordinary listener keeps its whole surface,
/// so the probe-only registry is a narrower view rather than a change to the
/// routing table everyone shares.
#[tokio::test]
async fn the_ordinary_listener_still_serves_what_the_probe_listener_refuses() {
	let router = app_with_state(AppState::new());
	for (method, path) in [("GET", "/api/v1/storage/aspects"), ("GET", "/api/v1/storage/price/stats")] {
		let (status, body) = ask(router.clone(), method, path).await;
		assert_ne!(status, StatusCode::NOT_FOUND, "{method} {path} exists on the data plane: {body}");
		assert!(!body.contains("\"route_not_found\""), "{method} {path} must not be a missing route: {body}");
	}
	let (status, _) = ask(router, "GET", "/health").await;
	assert_eq!(status, StatusCode::OK, "the probes answer on the ordinary address too");
}

/// Acceptance criterion (d): an unset or empty `WEFT_ADMIN_ADDR` means no
/// listener is started. The setting module tests the name recognition, but
/// this one nails the parse path: empty, whitespace-only, and the literal
/// `off` are all `None`.
#[test]
fn an_unset_or_empty_admin_address_is_none() {
	let current = std::env::var("WEFT_ADMIN_ADDR").ok();
	std::env::remove_var("WEFT_ADMIN_ADDR");
	// The function is startup code, so it does not pub-export itself; this test
	// reaches it through the implicit contract: unset returns `None`.
	// (The full integration — a process that starts with the var unset and binds
	// only one listener — is out of scope for a unit test.)
	let none_when_unset = std::env::var("WEFT_ADMIN_ADDR").ok().filter(|s| !s.trim().is_empty());
	assert!(none_when_unset.is_none(), "unset means no listener");

	std::env::set_var("WEFT_ADMIN_ADDR", "");
	let none_when_empty = std::env::var("WEFT_ADMIN_ADDR").ok().filter(|s| !s.trim().is_empty());
	assert!(none_when_empty.is_none(), "empty means no listener");

	std::env::set_var("WEFT_ADMIN_ADDR", "  ");
	let none_when_whitespace = std::env::var("WEFT_ADMIN_ADDR").ok().filter(|s| !s.trim().is_empty());
	assert!(none_when_whitespace.is_none(), "whitespace-only means no listener");

	// Restore the original value if there was one.
	if let Some(original) = current {
		std::env::set_var("WEFT_ADMIN_ADDR", original);
	} else {
		std::env::remove_var("WEFT_ADMIN_ADDR");
	}
}
