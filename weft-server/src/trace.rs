//! Per-request tracing **root span** + `x-request-id` correlation (roadmap Phase 3).
//!
//! The compute paths already emit per-stage spans (`interpolate.parse` /
//! `interpolate.compute` / `interpolate.serialize`, `downsample.parse` /
//! `downsample.reduce`), but until now each was a root — there was no per-request
//! span to hang them under, so two concurrent requests' stages interleaved in the
//! logs with nothing tying a stage back to the request that drove it.
//!
//! This middleware adds that root: every request runs inside a `request` span
//! carrying its method, path, and a correlation **request id**, so a `RUST_LOG` run
//! shows `request{…}:interpolate.engine:interpolate.compute` — the full call tree
//! under one id. The id is echoed back in the `x-request-id` response header (and an
//! inbound `x-request-id` is honoured, so a caller's own id threads through). This is
//! the request-span groundwork the roadmap's OTLP trace export builds on — the OTLP
//! exporter is the next slice; this slice ships the span + header with no new
//! dependency (`axum::middleware::from_fn`, not a `tower-http` `TraceLayer`).

use std::sync::atomic::{AtomicU64, Ordering};

use axum::{extract::Request, http::HeaderValue, middleware::Next, response::Response};
use tracing::Instrument as _;

/// The request-correlation header, read on the way in and written on the way out.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Process-local monotonic source of request ids when the client supplies none.
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// axum middleware: wrap every request in a `request` root span and stamp the
/// response with its `x-request-id`.
///
/// The span records the HTTP method, the request path, and a correlation id — an
/// inbound `x-request-id` when the caller supplied one (so an upstream id threads
/// through unchanged), otherwise a freshly minted process-local `req-<hex>`. The
/// downstream handler runs *inside* the span, so every stage span it opens
/// (`interpolate.engine`, `downsample.reduce`, …) nests under it. The same id is set
/// on the response so a client can correlate its call with the server-side trace.
pub async fn request_span(request: Request, next: Next) -> Response {
	let method = request.method().clone();
	let path = request.uri().path().to_owned();
	let request_id = request.headers().get(REQUEST_ID_HEADER).and_then(|value| value.to_str().ok()).map_or_else(|| format!("req-{:016x}", REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed)), ToOwned::to_owned);
	let span = tracing::info_span!("request", method = %method, path = %path, request_id = %request_id);
	let mut response = next.run(request).instrument(span).await;
	if let Ok(value) = HeaderValue::from_str(&request_id) {
		response.headers_mut().insert(REQUEST_ID_HEADER, value);
	}
	response
}

#[cfg(test)]
mod tests {
	use axum::{
		body::Body, http::{Request as HttpRequest, StatusCode}
	};
	use tower::ServiceExt as _;

	use crate::{app_with_state, state::AppState};

	#[tokio::test]
	async fn health_response_carries_a_minted_request_id() {
		let router = app_with_state(AppState::new());
		let response = router.oneshot(HttpRequest::builder().uri("/health").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		let id = response.headers().get(super::REQUEST_ID_HEADER).expect("x-request-id set on the response");
		assert!(id.to_str().unwrap().starts_with("req-"), "a minted id is `req-<hex>`: {id:?}");
	}

	#[tokio::test]
	async fn inbound_request_id_is_echoed_back() {
		let router = app_with_state(AppState::new());
		let response = router.oneshot(HttpRequest::builder().uri("/health").header(super::REQUEST_ID_HEADER, "trace-abc-123").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		assert_eq!(response.headers().get(super::REQUEST_ID_HEADER).unwrap(), "trace-abc-123", "an inbound id threads through unchanged");
	}
}
