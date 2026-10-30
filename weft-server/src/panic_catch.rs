//! The one panic-catch layer (robustness track **ROB-3**, release plan C12).
//!
//! Plan C12 settles panic handling in three parts, and this module is the
//! second of them:
//!
//! 1. ROB-2's hook (see [`crate::panic_hook`]) aborts the process on a
//!    panic inside a control-plane write. That abort happens *during* the
//!    unwind this layer would otherwise catch, so a turso-origin panic is
//!    never converted into an HTTP response here — the fail-stop is
//!    preserved, mechanically, by hook ordering.
//! 2. **This layer** is the only layer that catches panics. FRE-6a's
//!    proposed CatchPanicLayer was dropped in X-ERR precisely so there is
//!    exactly one: no `tower_http` catch layer is installed anywhere, and
//!    no handler catches its own panics. Every request runs through
//!    [`weftdb::exec::catch`], the `weftdb::exec` entry point, and a caught
//!    panic becomes the §4 envelope instead of a dropped connection.
//! 3. SEC-9's audited routes will catch `JoinError::is_panic` inside their
//!    own spawned tasks (a later slice); that is a task boundary, not a
//!    second catch layer on this path.
//!
//! ## How a caught panic is mapped
//!
//! - **Ingest routes** (the four `POST /api/v1/storage/{aspect}/…` write
//!   routes, the registry's `Scope::Ingest` surface) map by
//!   [`IngestPhase`](weftdb::IngestPhase), plan D21 — the same mapping
//!   D-S14 applies to a `JoinError` on this path. The request runs inside
//!   [`weftdb::exec::ingest_phase_scope`], and
//!   [`SegmentStore::ingest`](weftdb::SegmentStore::ingest) advances the
//!   task-local phase as it goes:
//!   - before [`Committing`](weftdb::IngestPhase::Committing) — validating,
//!     encoding, writing frames — the batch's one index COMMIT has not
//!     run, so the write **did not commit**: `500 commit_failed` with
//!     `committed: false`, the envelope that makes a retry obviously safe.
//!     (Frames already written are crash litter for recovery to classify,
//!     exactly as after a crash at the same point.)
//!   - at [`Committing`](weftdb::IngestPhase::Committing) the COMMIT may
//!     or may not have landed — that ambiguity is the definition of
//!     `503 outcome_unknown`, whose retry is the same idempotency key, not
//!     a blind resend.
//! - **Every other route** maps to `500 internal`.
//!
//! In all cases the client gets the code's redacted sentence (the §4
//! rule for 5xx); the panic's message and the phase go to the server log
//! only. A panic payload is never formatted into the response.

use axum::{
	body::Body, extract::Request, http::Method, middleware::Next, response::{IntoResponse, Response}
};
use weftdb::IngestPhase;

use crate::error::{ApiError, Code};

/// Whether `request` is one of the registry's ingest routes: a `POST`
/// under `/api/v1/storage/{aspect}/` to one of the four ingest suffixes
/// `routes::register_ingest_routes` registers. Decided from the request
/// line because this layer sits outside the registry's gate — the gate
/// has not matched the route yet when the catch must be installed around
/// it — and the four suffixes are the frozen ingest surface.
fn is_ingest_route(request: &Request<Body>) -> bool {
	if request.method() != Method::POST {
		return false;
	}
	let path = request.uri().path();
	let Some(rest) = path.strip_prefix("/api/v1/storage/") else { return false };
	let Some((_, suffix)) = rest.rsplit_once('/') else { return false };
	matches!(suffix, "points" | "ilp" | "csv" | "parquet")
}

/// The panic-catch middleware: install it once, around the whole router
/// ([`crate::app_with_state`] does), and every handler panic becomes an
/// envelope. See the module docs for the mapping.
pub async fn catch_panic(request: Request<Body>, next: Next) -> Response {
	if is_ingest_route(&request) {
		// The phase scope wraps the catch, never the other way round: the
		// catch reads the phase back while the scope is still open.
		match weftdb::exec::ingest_phase_scope(weftdb::exec::catch(next.run(request))).await {
			Ok(response) => response,
			Err(caught) => {
				tracing::error!(panic = %caught.message, phase = ?caught.ingest_phase, "ingest handler panicked; mapped by IngestPhase (ROB-3, D21)");
				match caught.ingest_phase {
					Some(IngestPhase::Committing) => ApiError::server(Code::OutcomeUnknown, format!("panic while committing an ingest: {}", caught.message)).into_response(),
					_ => ApiError::commit_failed(format!("panic before the ingest committed (phase {:?}): {}", caught.ingest_phase, caught.message)).into_response(),
				}
			}
		}
	} else {
		match weftdb::exec::catch(next.run(request)).await {
			Ok(response) => response,
			Err(caught) => {
				tracing::error!(panic = %caught.message, "handler panicked; answered as internal (ROB-3)");
				ApiError::internal(format!("handler panicked: {}", caught.message)).into_response()
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use axum::{
		body::Body, http::{Request, StatusCode}, routing::post, Router
	};
	use tower::ServiceExt as _;

	use super::*;

	async fn seal_explodes() -> &'static str {
		panic!("seal exploded")
	}

	async fn commit_explodes() -> &'static str {
		weftdb::exec::set_ingest_phase(IngestPhase::Committing);
		panic!("commit exploded")
	}

	async fn compute_explodes() -> &'static str {
		panic!("compute exploded")
	}

	fn panicking_router() -> Router {
		Router::new().route("/api/v1/storage/{aspect}/points", post(seal_explodes)).route("/api/v1/storage/{aspect}/ilp", post(commit_explodes)).route("/calm", post(|| async { "fine" })).layer(axum::middleware::from_fn(catch_panic))
	}

	async fn body_json(response: Response) -> serde_json::Value {
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		serde_json::from_slice(&bytes).unwrap()
	}

	#[tokio::test]
	async fn a_non_ingest_panic_is_500_internal_with_the_detail_redacted() {
		// A compute-style route registered on the test router: reuse the
		// ingest path shape off the POST/storage prefix by hitting /calm's
		// sibling — simplest is a second router for the plain case.
		let router = Router::new().route("/boom", post(compute_explodes)).layer(axum::middleware::from_fn(catch_panic));
		let response = router.oneshot(Request::builder().method("POST").uri("/boom").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
		let body = body_json(response).await;
		assert_eq!(body["code"], "internal");
		assert!(!body.to_string().contains("compute exploded"), "the panic message never reaches the client: {body}");
	}

	#[tokio::test]
	async fn an_ingest_panic_before_committing_is_commit_failed_and_uncommitted() {
		let response = panicking_router().oneshot(Request::builder().method("POST").uri("/api/v1/storage/price/points").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
		let body = body_json(response).await;
		assert_eq!(body["code"], "commit_failed");
		assert_eq!(body["committed"], false, "pre-commit means a retry is safe, and the envelope says so");
	}

	#[tokio::test]
	async fn an_ingest_panic_while_committing_is_outcome_unknown() {
		let response = panicking_router().oneshot(Request::builder().method("POST").uri("/api/v1/storage/price/ilp").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
		let body = body_json(response).await;
		assert_eq!(body["code"], "outcome_unknown");
		assert!(body.get("committed").is_none(), "unknown means the envelope does not claim either way");
	}

	#[tokio::test]
	async fn a_request_that_does_not_panic_passes_through_untouched() {
		let response = panicking_router().oneshot(Request::builder().method("POST").uri("/calm").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		assert_eq!(&bytes[..], b"fine");
	}

	#[test]
	fn ingest_route_detection_matches_the_four_ingest_suffixes_only() {
		let request = |method: &str, uri: &str| Request::builder().method(method).uri(uri).body(Body::empty()).unwrap();
		assert!(is_ingest_route(&request("POST", "/api/v1/storage/price/points")));
		assert!(is_ingest_route(&request("POST", "/api/v1/storage/price/parquet")));
		assert!(!is_ingest_route(&request("GET", "/api/v1/storage/price/points")), "reads are not ingest");
		assert!(!is_ingest_route(&request("POST", "/api/v1/storage/price/reconcile")), "maintenance is not ingest");
		assert!(!is_ingest_route(&request("POST", "/api/v1/interpolate")));
	}
}
