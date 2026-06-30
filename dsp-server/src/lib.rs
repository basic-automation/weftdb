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
pub mod manage;
pub mod metrics;
pub mod state;
pub mod storage;

use axum::{
	routing::{get, post}, Json, Router
};
pub use downsample::{downsample, downsample_csv, downsample_ilp, Aggregation, DownsampleRequest, DownsampleResponse};
pub use interpolate::{interpolate, interpolate_csv, interpolate_ilp, interpolate_point, InterpolateRequest, InterpolateResponse, PointKind, PointRequest, PointResponse};
pub use manage::{declare_aspect, ingest_csv, ingest_ilp, ingest_points, CsvIngestParams, DeclareAspectRequest, DeclareAspectResponse, IlpIngestParams, IngestPoint, IngestRequest, IngestResponse};
pub use metrics::{Metrics, MetricsSnapshot, SharedMetrics};
use serde::Serialize;
pub use state::AppState;
pub use storage::{storage_aspect_schema, storage_aspect_stats, storage_aspects, storage_catalog, storage_ingest_parquet, storage_stats, storage_time_range, storage_time_range_csv, storage_time_range_json, storage_time_range_parquet, storage_value_range, storage_value_range_csv, storage_value_range_json, storage_value_range_parquet, AspectInfo, AspectListResponse, AspectSchemaResponse, AspectStatsResponse, CatalogDatabase, CatalogResponse, ParquetIngestParams, StorageError, StoredPoint, StoredRangeResponse, StoreStatsResponse, TimeRangeParams, ValuePointsParams, ValueRangeParams};

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
	/// `true` once the service can accept traffic. The stateless API is always
	/// ready; this gains further dependency checks as more of the control plane is
	/// wired in.
	pub ready: bool,
	/// Logical service name ([`SERVICE`]).
	pub service: &'static str,
	/// Running build version ([`VERSION`]).
	pub version: &'static str,
	/// Whether a segment store is configured — i.e. the storage-query endpoints
	/// (`/api/v1/storage/...`) are live rather than answering `503`.
	pub segment_store: bool,
}

impl Default for ReadyResponse {
	fn default() -> Self {
		Self { ready: true, service: SERVICE, version: VERSION, segment_store: false }
	}
}

/// Build the application router with a fresh metrics registry and no segment
/// store. This is the single source of truth for the service's route table; the
/// binary and the tests both go through it.
pub fn app() -> Router {
	app_with_state(AppState::new())
}

/// Build the application router over a caller-supplied [`SharedMetrics`].
///
/// Lets a test (or an embedding host) observe the counters the handlers update.
/// No segment store is attached (the storage endpoints answer `503`).
pub fn app_with_metrics(metrics: SharedMetrics) -> Router {
	app_with_state(AppState::with_metrics(metrics))
}

/// Build the application router over a fully-formed [`AppState`].
///
/// This is the single place routes are registered. The state carries the metrics
/// handle (projected to the capability handlers via [`axum::extract::FromRef`])
/// and an optional segment store backing the storage-query endpoints.
pub fn app_with_state(state: AppState) -> Router {
	Router::new().route("/health", get(health)).route("/ready", get(ready)).route("/metrics", get(metrics::metrics)).route("/api/v1/interpolate", post(interpolate)).route("/api/v1/interpolate/csv", post(interpolate_csv)).route("/api/v1/interpolate/ilp", post(interpolate_ilp)).route("/api/v1/interpolate/point", post(interpolate_point)).route("/api/v1/downsample", post(downsample)).route("/api/v1/downsample/csv", post(downsample_csv)).route("/api/v1/downsample/ilp", post(downsample_ilp)).route("/api/v1/storage/aspects", get(storage_aspects).post(manage::declare_aspect)).route("/api/v1/storage/catalog", get(storage_catalog)).route("/api/v1/storage/stats", get(storage_stats)).route("/api/v1/storage/{aspect}/range", get(storage_time_range)).route("/api/v1/storage/{aspect}/range.parquet", get(storage_time_range_parquet)).route("/api/v1/storage/{aspect}/range.csv", get(storage_time_range_csv)).route("/api/v1/storage/{aspect}/points", get(storage_time_range_json).post(manage::ingest_points)).route("/api/v1/storage/{aspect}/ilp", post(manage::ingest_ilp)).route("/api/v1/storage/{aspect}/csv", post(manage::ingest_csv)).route("/api/v1/storage/{aspect}/value-range", get(storage_value_range)).route("/api/v1/storage/{aspect}/value-range.parquet", get(storage_value_range_parquet)).route("/api/v1/storage/{aspect}/value-range.csv", get(storage_value_range_csv)).route("/api/v1/storage/{aspect}/value-points", get(storage_value_range_json)).route("/api/v1/storage/{aspect}/parquet", post(storage_ingest_parquet)).route("/api/v1/storage/{aspect}/stats", get(storage_aspect_stats)).route("/api/v1/storage/{aspect}/schema", get(storage_aspect_schema)).with_state(state)
}

/// Liveness probe: the process is up and can serve a request.
async fn health() -> Json<HealthResponse> {
	Json(HealthResponse::default())
}

/// Readiness probe: the service is ready to accept traffic. Reports whether a
/// segment store is configured so an operator can confirm the storage endpoints
/// are live.
async fn ready(axum::extract::State(state): axum::extract::State<AppState>) -> Json<ReadyResponse> {
	Json(ReadyResponse { segment_store: state.store().is_some(), ..ReadyResponse::default() })
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
		// The default router has no segment store, so the storage endpoints are off.
		assert_eq!(body["segment_store"], false);
	}

	#[tokio::test]
	async fn ready_reports_a_configured_segment_store() {
		use std::sync::Arc;

		use database::SegmentStore;
		use tempfile::TempDir;

		let dir = TempDir::new().unwrap();
		// Construct the store inline so the significant-`Drop` `SegmentStore` is never
		// bound on its own (avoids the drop-tightening lint).
		let router = app_with_state(AppState::new().with_store(Arc::new(SegmentStore::open(dir.path()).await.unwrap())));
		let response = router.oneshot(Request::builder().uri("/ready").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		// With a store attached, readiness advertises the storage endpoints as live.
		assert_eq!(body["segment_store"], true);
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
