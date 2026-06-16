//! # dsp-server
//!
//! The benchmark-grade HTTP API surface for DSP (roadmap Phase 2). A commercial
//! time-series engine cannot lead with an embedded Rust API plus a TUI: DSP-Bench
//! must be able to drive DSP entirely through public endpoints, and external
//! tooling (TSBS, migration harnesses, Grafana, SDKs) needs a stable HTTP surface.
//! This crate is that surface.
//!
//! ## Shape (this is the skeleton)
//!
//! - [`app`] builds the [`axum::Router`] for the whole service. It is the single
//!   place routes are registered, so tests exercise the exact router the binary
//!   serves (no divergence between test and production wiring).
//! - Liveness vs readiness are kept distinct, matching standard orchestration
//!   probes: [`health`] reports the process is up (`GET /health`), [`ready`]
//!   reports the service is ready to accept traffic (`GET /ready`). Today
//!   readiness is unconditional; as real dependencies (control-plane DB, segment
//!   store) are wired in, `ready` gains the checks while `health` stays cheap.
//!
//! The ingest/query/interpolation endpoints (REST, then `InfluxDB` Line Protocol)
//! land on top of this skeleton in subsequent slices.
//!
//! ## Vendor-neutrality
//!
//! Per the workspace hard constraints this crate carries no vendor-specific
//! dependencies; it is a thin HTTP shell over the DSP core and never a place for
//! a concrete connector to leak in.

#![warn(clippy::pedantic, clippy::nursery, clippy::all)]

pub mod downsample;
pub mod interpolate;
pub mod metrics;

use axum::{
	routing::{get, post}, Json, Router
};
pub use downsample::{downsample, downsample_ilp, Aggregation, DownsampleRequest, DownsampleResponse};
pub use interpolate::{interpolate, interpolate_ilp, interpolate_point, InterpolateRequest, InterpolateResponse, PointRequest, PointResponse, ValueKind};
pub use metrics::{Metrics, MetricsSnapshot, SharedMetrics};
use serde::Serialize;

/// The server's package version, surfaced in probe responses so a deployed
/// instance is identifiable from a plain `curl`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Logical service name, echoed in every probe payload.
pub const SERVICE: &str = "dsp-server";

/// Response body for the liveness probe (`GET /health`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthResponse {
	/// Always `"ok"` while the process can serve a request.
	pub status: &'static str,
	/// Logical service name ([`SERVICE`]).
	pub service: &'static str,
	/// Running build version ([`VERSION`]).
	pub version: &'static str,
}

impl Default for HealthResponse {
	fn default() -> Self {
		Self { status: "ok", service: SERVICE, version: VERSION }
	}
}

/// Response body for the readiness probe (`GET /ready`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadyResponse {
	/// `true` once the service can accept traffic. Unconditional today; gains
	/// real dependency checks as the control plane and segment store are wired in.
	pub ready: bool,
	/// Logical service name ([`SERVICE`]).
	pub service: &'static str,
	/// Running build version ([`VERSION`]).
	pub version: &'static str,
}

impl Default for ReadyResponse {
	fn default() -> Self {
		Self { ready: true, service: SERVICE, version: VERSION }
	}
}

/// Build the application router with a fresh metrics registry. This is the
/// single source of truth for the service's route table; the binary and the
/// tests both go through it.
pub fn app() -> Router {
	app_with_metrics(SharedMetrics::default())
}

/// Build the application router over a caller-supplied [`SharedMetrics`], so a
/// test (or an embedding host) can observe the counters the handlers update.
pub fn app_with_metrics(metrics: SharedMetrics) -> Router {
	Router::new().route("/health", get(health)).route("/ready", get(ready)).route("/metrics", get(metrics::metrics)).route("/api/v1/interpolate", post(interpolate)).route("/api/v1/interpolate/ilp", post(interpolate_ilp)).route("/api/v1/interpolate/point", post(interpolate_point)).route("/api/v1/downsample", post(downsample)).route("/api/v1/downsample/ilp", post(downsample_ilp)).with_state(metrics)
}

/// Liveness probe: the process is up and can serve a request.
async fn health() -> Json<HealthResponse> {
	Json(HealthResponse::default())
}

/// Readiness probe: the service is ready to accept traffic.
async fn ready() -> Json<ReadyResponse> {
	Json(ReadyResponse::default())
}

#[cfg(test)]
mod tests {
	use axum::{
		body::Body, http::{Request, StatusCode}
	};
	use tower::ServiceExt;

	use super::*;

	async fn get_json(uri: &str) -> (StatusCode, serde_json::Value) {
		let response = app().oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let value = serde_json::from_slice(&bytes).unwrap();
		(status, value)
	}

	#[tokio::test]
	async fn health_returns_ok() {
		let (status, body) = get_json("/health").await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body["status"], "ok");
		assert_eq!(body["service"], SERVICE);
		assert_eq!(body["version"], VERSION);
	}

	#[tokio::test]
	async fn ready_returns_ready() {
		let (status, body) = get_json("/ready").await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body["ready"], true);
		assert_eq!(body["service"], SERVICE);
		assert_eq!(body["version"], VERSION);
	}

	#[tokio::test]
	async fn unknown_route_is_404() {
		let response = app().oneshot(Request::builder().uri("/nope").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::NOT_FOUND);
	}

	#[test]
	fn probe_defaults_are_consistent() {
		assert_eq!(HealthResponse::default().status, "ok");
		assert!(ReadyResponse::default().ready);
		assert_eq!(HealthResponse::default().service, ReadyResponse::default().service);
	}

	#[tokio::test]
	async fn metrics_endpoint_renders_prometheus() {
		let response = app().oneshot(Request::builder().uri("/metrics").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		let content_type = response.headers().get("content-type").unwrap().to_str().unwrap().to_string();
		assert!(content_type.starts_with("text/plain"));
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let text = String::from_utf8(bytes.to_vec()).unwrap();
		assert!(text.contains("dsp_interpolate_requests_total"));
	}

	#[tokio::test]
	async fn interpolate_increments_shared_metrics() {
		let metrics = SharedMetrics::default();
		let router = app_with_metrics(metrics.clone());
		let body = serde_json::json!({
			"spline": "linear",
			"resolution": "seconds",
			"points": [
				{ "timestamp": "1970-01-01T00:00:00Z", "value": 0.0 },
				{ "timestamp": "1970-01-01T00:00:10Z", "value": 10.0 },
			],
		});
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/interpolate").header("content-type", "application/json").body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		let snap = metrics.snapshot();
		assert_eq!(snap.interpolate.requests, 1);
		assert_eq!(snap.interpolate.errors, 0);
		assert!(snap.interpolate.output_points >= 2);
	}
}
