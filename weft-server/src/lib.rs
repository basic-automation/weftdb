//! # weft-server
//!
//! The benchmark-grade HTTP API surface for WeftDB (roadmap Phase 2). A commercial
//! time-series engine cannot lead with an embedded Rust API plus a TUI: Weft-Bench
//! must be able to drive WeftDB entirely through public endpoints, and external
//! tooling (TSBS, migration harnesses, Grafana, SDKs) needs a stable HTTP surface.
//! This crate is that surface.
//!
//! ## Shape (this is the skeleton)
//!
//! - [`app`] builds the [`axum::Router`] for the whole service from the sealed
//!   route registry in [`routes`] (SEC-3): the registry is the single place
//!   routes are registered, so tests exercise the exact router the binary
//!   serves (no divergence between test and production wiring), and the same
//!   registry produces the policy table, the class table and the OpenAPI
//!   document.
//! - Liveness vs readiness are kept distinct, matching standard orchestration
//!   probes: `health` reports the process is up (`GET /health`), `ready`
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
//! dependencies; it is a thin HTTP shell over the WeftDB core and never a place for
//! a concrete connector to leak in.

// Proving the axum handler bounds for the full router exceeds the default limit of 128;
// rustc warns that overflowing it will become a hard error.
#![recursion_limit = "256"]
#![warn(clippy::pedantic, clippy::nursery, clippy::all)]

pub mod backup_daemon;
pub mod backup_routes;
pub mod declare;
pub mod downsample;
pub mod error;
pub mod gpu;
pub mod host_guard;
/// FRE-14c's operator route: `weft-server import-rows-mode`, which reads a 0.1.x data
/// directory into a Storage v2 store. It is a library module rather than code in `main` so
/// its argument parsing and its report are testable, like every other server behaviour.
pub mod import_rows_mode;
pub mod ingest;
pub mod interpolate;
pub mod maintain;
pub mod metrics;
pub mod panic_hook;
pub mod reconcile_daemon;
pub mod routes;
pub mod serve;
pub mod settings;
pub mod state;
pub mod storage;
pub mod store_open;
#[cfg(test)]
mod test_fs;
#[cfg(test)]
mod test_support;
pub mod trace;

use axum::{middleware, Router};
pub use backup_daemon::{backup_tick, list_generated_backups, prune_generated_backups, spawn_backup_daemon, BackupDaemonConfig};
pub use downsample::{downsample, downsample_arrow, downsample_csv, downsample_ilp, downsample_ilp_arrow, downsample_ilp_csv, downsample_ilp_parquet, downsample_parquet, Aggregation, DownsampleRequest, DownsampleResponse};
pub use error::{ApiError, Code};
pub use host_guard::{HostGuard, ALLOW_ANY_HOST_ENV};
pub use interpolate::{interpolate, interpolate_arrow, interpolate_csv, interpolate_ilp, interpolate_ilp_arrow, interpolate_ilp_csv, interpolate_ilp_parquet, interpolate_parquet, interpolate_point, InterpolateConfig, InterpolateRequest, InterpolateResponse, PointKind, PointRequest, PointResponse, MAX_INTERPOLATE_OUTPUT_POINTS, MAX_INTERPOLATE_POINTS_ENV};
pub use metrics::{DownsampleProfile, IngestProfile, InterpolateProfile, LatencyHistogram, LatencyHistogramSnapshot, LatencyProfile, Metrics, MetricsSnapshot, ProfileReport, SharedMetrics};
pub use reconcile_daemon::{reconcile_tick, reconcile_tick_hot_cold, reconcile_tick_overlaps, spawn_reconcile_daemon, ReconcileDaemonConfig};
pub use routes::{AdmissionClass, Audit, ClassEntry, PolicyEntry, Role, RouteEntry, RouteSpec, RouteTable, Routes, Scope, SealedRoutes};
use serde::Serialize;
pub use serve::{ServeLimits, ShutdownSignals, HEADER_READ_TIMEOUT_ENV, KEEP_ALIVE_TIMEOUT_ENV, MAX_CONNECTIONS_ENV, WRITE_STALL_TIMEOUT_ENV};
pub use state::AppState;
pub use storage::{storage_aspect_schema, storage_aspect_stats, storage_aspects, storage_catalog, storage_downsample, storage_downsample_arrow, storage_downsample_csv, storage_downsample_parquet, storage_point, storage_points, storage_stats, storage_time_range, storage_time_range_csv, storage_time_range_json, storage_time_range_parquet, storage_value_range, storage_value_range_csv, storage_value_range_json, storage_value_range_parquet, AspectInfo, AspectListResponse, AspectSchemaResponse, AspectStatsResponse, CatalogDatabase, CatalogResponse, MultiPointParams, PointParams, StorageDownsampleParams, StoreStatsResponse, StoredPoint, StoredPointEntry, StoredPointResponse, StoredPointsResponse, StoredRangeResponse, TimeRangeParams, ValuePointsParams, ValueRangeParams};

pub use crate::{
	declare::{declare_aspect, DeclareAspectRequest, DeclareAspectResponse}, ingest::{ingest_csv, ingest_ilp, ingest_parquet, ingest_points, CsvIngestParams, IlpIngestParams, IngestPoint, IngestRequest, IngestResponse, ParquetIngestParams}, maintain::{fsck_store, purge_expired_quarantine, quarantine_list, quarantine_purge, quarantine_release, reconcile_aspect, reconcile_store, PurgeExpiredResponse, QuarantineParams, ReconcileResponse, ReconcileStoreResponse, SweepFailure}
};

/// The server's package version, surfaced in probe responses so a deployed
/// instance is identifiable from a plain `curl`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Logical service name, echoed in every probe payload.
pub const SERVICE: &str = "weft-server";

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
#[allow(clippy::struct_excessive_bools, reason = "a probe body is a set of independent flags that orchestrators and operators read by name")]
pub struct ReadyResponse {
	/// `true` once the service can accept traffic. The stateless API is always
	/// ready; this gains further dependency checks as more of the control plane is
	/// wired in. `false` — answered with `503` — exactly when the store reports
	/// `incompatible_store` (D-S12): a newer-format frame means this build must
	/// not serve this store.
	pub ready: bool,
	/// Logical service name ([`SERVICE`]).
	pub service: &'static str,
	/// Running build version ([`VERSION`]).
	pub version: &'static str,
	/// Whether a segment store is configured — i.e. the storage-query endpoints
	/// (`/api/v1/storage/...`) are live rather than answering `503`.
	pub segment_store: bool,
	/// Whether the segment store is write-poisoned: a control-plane COMMIT failed in a
	/// way that may still have committed (not a conflict found while validating it), and
	/// the store refuses every write until the server restarts. Reads keep working, so
	/// `ready` stays `true`.
	pub poisoned: bool,
	/// Whether the server must be restarted to accept writes again. The restart's open
	/// replays the control plane's log, which settles the ambiguous commit.
	pub restart_required: bool,
	/// Why the store is poisoned, while it is.
	pub poison_reason: Option<String>,
	/// The store's identity (`store_uuid` of its `STORE_FORMAT` marker), when a
	/// store is configured. D-S12.
	pub store_uuid: Option<String>,
	/// The store's durability class (`full`, or `process-crash-only` on Windows),
	/// when a store is configured. D-S12.
	pub durability_class: Option<&'static str>,
	/// Whether the previous process that held the store shut down cleanly, when a
	/// store is configured. D-S12.
	pub previous_clean_shutdown: Option<bool>,
	/// Corrupt quarantine records the last recovery pass left, counted only (SEC:
	/// `/ready` carries counts, never segment contents). D-S12.
	pub quarantined: usize,
	/// Suspect quarantine records the last recovery pass left, counted only. D-S12.
	pub suspect: usize,
	/// Whether any suspect record's cause is a frame version this build predates:
	/// the store holds data written by a newer server, and `ready` is `false`. D-S12.
	pub incompatible_store: bool,
}

impl Default for ReadyResponse {
	fn default() -> Self {
		Self { ready: true, service: SERVICE, version: VERSION, segment_store: false, poisoned: false, restart_required: false, poison_reason: None, store_uuid: None, durability_class: None, previous_clean_shutdown: None, quarantined: 0, suspect: 0, incompatible_store: false }
	}
}

impl ReadyResponse {
	/// This response with the store's write poison, if it has one, reported.
	#[must_use]
	pub fn with_poison(self, poison: Option<weftdb::Poisoned>) -> Self {
		let poisoned = poison.is_some();
		Self { poisoned, restart_required: poisoned, poison_reason: poison.map(|p| p.reason), ..self }
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
/// The router is built by the sealed route registry ([`routes::build_registry`]):
/// every route it serves is a registry entry with a [`RouteSpec`], and the
/// registry's fail-closed gate is part of that router. The state carries the
/// metrics handle (projected to the capability handlers via
/// [`axum::extract::FromRef`]), an optional segment store backing the
/// storage-query endpoints, and the request guard ([`host_guard`]) applied to
/// every route, inside the per-request trace span.
///
/// The `Weft-Incomplete` layer runs after the handlers: when the store holds a
/// suspect quarantine record, stored-data reads are incomplete (the suspect
/// segments are removed from reads), and their responses carry the header (D-S12).
pub fn app_with_state(state: AppState) -> Router {
	routes::build_registry().router().layer(middleware::from_fn_with_state(state.clone(), incomplete_header)).layer(middleware::from_fn_with_state(state.host_guard(), host_guard::enforce)).layer(middleware::from_fn(trace::request_span)).with_state(state)
}

/// Mark stored-data reads incomplete when the store holds suspect segments
/// (D-S12, C5): a suspect segment is removed from reads, so a range, point or
/// downsample answer over a store holding one is missing data the store still
/// holds on disk. The header rides on stored-data reads only — `GET`
/// `/api/v1/storage/...` and the POST downsample reads — never on ingest,
/// declare, maintenance or the probes, whose answers are not segment reads.
async fn incomplete_header(axum::extract::State(state): axum::extract::State<AppState>, request: axum::http::Request<axum::body::Body>, next: axum::middleware::Next) -> axum::response::Response {
	let is_storage_read = {
		let path = request.uri().path();
		let method = request.method();
		path.starts_with("/api/v1/storage/") && (method == axum::http::Method::GET || (method == axum::http::Method::POST && path.contains("/downsample")))
	};
	let mut response = next.run(request).await;
	if is_storage_read && state.store().is_some_and(|store| store.has_suspect_segments()) {
		response.headers_mut().insert("Weft-Incomplete", axum::http::HeaderValue::from_static("true"));
	}
	response
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
		// No store, so nothing to poison.
		assert_eq!(body["poisoned"], false);
		assert_eq!(body["restart_required"], false);
		assert_eq!(body["poison_reason"], serde_json::Value::Null);
	}

	/// A poisoned store is reported as poisoned and needing a restart, with the reason,
	/// and the service stays ready: reads keep working.
	#[test]
	fn ready_reports_a_poisoned_store() {
		let reason = "segment_index transaction failed with an ambiguous COMMIT: COMMIT: I/O error (Other): sync".to_string();
		let ready = ReadyResponse { segment_store: true, ..ReadyResponse::default() }.with_poison(Some(weftdb::Poisoned { reason: reason.clone() }));
		let body = serde_json::to_value(&ready).unwrap();
		assert_eq!(body["ready"], true);
		assert_eq!(body["poisoned"], true);
		assert_eq!(body["restart_required"], true);
		assert_eq!(body["poison_reason"], reason);
		assert_eq!(ReadyResponse::default().with_poison(None), ReadyResponse::default(), "no poison changes nothing");
	}

	#[tokio::test]
	async fn ready_reports_a_configured_segment_store() {
		use std::sync::Arc;

		use tempfile::TempDir;
		use weftdb::SegmentStore;

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
		// A freshly opened store is not poisoned.
		assert_eq!(body["poisoned"], false);
		assert_eq!(body["restart_required"], false);
		assert_eq!(body["poison_reason"], serde_json::Value::Null);
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
		assert!(text.contains("weft_interpolate_requests_total"));
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
