//! The catalog-**management** endpoints (roadmap Phase 2 — *create DB / subject /
//! aspect; define schema/physical type; batch ingest*).
//!
//! These are the write-path counterpart of the read-only
//! [`storage`](crate::storage) surface. Until now the segment store could only be
//! populated **in-process** (a test or an
//! embedding host called [`SegmentStore::declare`](database::SegmentStore::declare)
//! / `seal_declared` directly); the read endpoints served whatever was already on
//! disk. These handlers let an HTTP client declare an aspect's schema and ingest a
//! batch of points, so a client can populate the store the
//! [`storage`](crate::storage) read endpoints then serve — closing the Phase-2
//! "DB/subject/aspect management" + "schema & physical-type definition" + "batch
//! ingest" gaps.
//!
//! ## Scope
//!
//! A configured [`SegmentStore`](database::SegmentStore) is opened against one
//! `(database, subject)` namespace (the binary's `DSP_SEGMENT_STORE_ROOT` wiring),
//! so these endpoints manage **aspects** within that scope — the unit a client
//! actually declares a schema for and seals batches into. The DB/subject hierarchy
//! is fixed at server start; aspect declaration + ingest is the live write path.
//!
//! ## Vendor-neutrality / storage boundary
//!
//! As with [`storage`](crate::storage) the heavy `arrow-*` tree never appears here:
//! these are plain JSON request/response handlers over the `database` control plane
//! and the typed `.dspseg` seal path. The declared `value_tolerance` is honoured by
//! the seal (hard constraint #4 — an ingest whose values cannot be represented
//! under the declared encoding within tolerance is **rejected**, never silently
//! downcast).

use axum::{
	extract::State, http::StatusCode, response::{IntoResponse, Response}, Json
};
use bigdecimal::BigDecimal;
use dsp_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use serde::{Deserialize, Serialize};

use crate::{state::AppState, storage::{AspectInfo, StorageError}};

/// Parse a [`PhysicalType`] from its stable wire token (the inverse of
/// [`PhysicalType::name`]) plus an optional `scale`.
///
/// The fixed-point encodings (`scaled_i64`, `scaled_i128`) require a `scale` (the
/// number of fractional decimal digits the mantissa carries); the others ignore it.
///
/// # Errors
///
/// Returns a human-readable message when the token is unknown, or when a
/// scale-bearing encoding was named without a `scale`.
fn parse_physical_type(token: &str, scale: Option<u8>) -> Result<PhysicalType, String> {
	match token {
		"f64" => Ok(PhysicalType::F64),
		"f32" => Ok(PhysicalType::F32),
		"scaled_i64" => scale.map(|scale| PhysicalType::ScaledI64 { scale }).ok_or_else(|| "`scaled_i64` requires a `scale` (fractional decimal digits)".to_string()),
		"scaled_i128" => scale.map(|scale| PhysicalType::ScaledI128 { scale }).ok_or_else(|| "`scaled_i128` requires a `scale` (fractional decimal digits)".to_string()),
		"decimal128" => Ok(PhysicalType::Decimal128),
		"bigdecimal_text" => Ok(PhysicalType::BigDecimalText),
		other => Err(format!("unknown physical_type {other:?} (expected one of f64, f32, scaled_i64, scaled_i128, decimal128, bigdecimal_text)")),
	}
}

/// Parse a [`TimeUnit`] from its stable wire token (the inverse of
/// [`TimeUnit::name`]).
///
/// # Errors
///
/// Returns a human-readable message when the token is unknown.
fn parse_time_unit(token: &str) -> Result<TimeUnit, String> {
	match token {
		"seconds" => Ok(TimeUnit::Seconds),
		"millis" => Ok(TimeUnit::Millis),
		"micros" => Ok(TimeUnit::Micros),
		"nanos" => Ok(TimeUnit::Nanos),
		other => Err(format!("unknown timestamp_unit {other:?} (expected one of seconds, millis, micros, nanos)")),
	}
}

/// Request body for `POST /api/v1/storage/aspects` — declare an aspect's schema.
///
/// The `physical_type`/`timestamp_unit` tokens mirror the read surface
/// ([`AspectInfo`]); `scale` is required only for the fixed-point encodings, and
/// `value_tolerance` defaults to `"0"` (exact) when omitted.
#[derive(Debug, Clone, Deserialize)]
pub struct DeclareAspectRequest {
	/// The aspect name to declare (unique within the store's `(database, subject)`
	/// scope; a re-declaration overwrites).
	pub name: String,
	/// The physical encoding token (e.g. `"f64"`, `"scaled_i64"`).
	pub physical_type: String,
	/// Fractional-decimal-digit scale, required for `scaled_i64` / `scaled_i128`.
	#[serde(default)]
	pub scale: Option<u8>,
	/// The permitted per-value reconstruction error as decimal text. Defaults to
	/// `"0"` (the encoding must be exact for every value).
	#[serde(default)]
	pub value_tolerance: Option<String>,
	/// The timestamp resolution token (e.g. `"seconds"`, `"millis"`).
	pub timestamp_unit: String,
}

/// Response body for a successful aspect declaration: the schema the store now
/// holds, echoed in the same shape the read surface lists it ([`AspectInfo`]).
#[derive(Debug, Clone, Serialize)]
pub struct DeclareAspectResponse {
	/// The declared aspect.
	pub aspect: AspectInfo,
}

/// Build the [`AspectSchema`] a [`DeclareAspectRequest`] describes, mapping every
/// parse failure to a `400`.
fn schema_from_request(request: &DeclareAspectRequest) -> Result<AspectSchema, StorageError> {
	let physical_type = parse_physical_type(&request.physical_type, request.scale).map_err(StorageError::BadRequest)?;
	let timestamp_unit = parse_time_unit(&request.timestamp_unit).map_err(StorageError::BadRequest)?;
	let tolerance_text = request.value_tolerance.as_deref().unwrap_or("0");
	let value_tolerance: BigDecimal = tolerance_text.parse().map_err(|_| StorageError::BadRequest(format!("`value_tolerance` is not a decimal: {tolerance_text:?}")))?;
	Ok(AspectSchema::new(physical_type, value_tolerance, timestamp_unit))
}

/// Handle `POST /api/v1/storage/aspects`: declare an aspect's schema (physical
/// encoding + value tolerance + timestamp unit) in the configured store.
///
/// Returns `201 Created` with the stored schema. A re-declaration of the same
/// aspect overwrites and still returns `201`.
///
/// # Errors
///
/// [`StorageError::Unconfigured`] when no store is attached,
/// [`StorageError::BadRequest`] when a token/tolerance does not parse, and
/// [`StorageError::Internal`] on a control-plane write failure.
pub async fn declare_aspect(State(state): State<AppState>, Json(request): Json<DeclareAspectRequest>) -> Result<Response, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	let schema = schema_from_request(&request)?;
	let result = store.declare(&request.name, &schema).await;
	drop(store);
	result.map_err(|err| StorageError::Internal(err.to_string()))?;
	let aspect = AspectInfo { name: request.name, physical_type: schema.value.name(), value_tolerance: schema.value_tolerance.to_string(), timestamp_unit: schema.timestamp_unit.name() };
	Ok((StatusCode::CREATED, Json(DeclareAspectResponse { aspect })).into_response())
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use axum::{
		body::Body, http::{Request, StatusCode}
	};
	use database::SegmentStore;
	use tempfile::TempDir;
	use tower::ServiceExt;

	use crate::{app_with_state, AppState};

	/// Build a router over a fresh, empty store under `dir` (no aspects declared).
	async fn router_with_empty_store(dir: &TempDir) -> axum::Router {
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens store"));
		app_with_state(AppState::new().with_store(store))
	}

	/// POST `body` (a JSON value) to `uri`, returning the status and parsed JSON body.
	async fn post_json(router: axum::Router, uri: &str, body: &serde_json::Value) -> (StatusCode, serde_json::Value) {
		let response = router.oneshot(Request::builder().method("POST").uri(uri).header("content-type", "application/json").body(Body::from(serde_json::to_vec(body).unwrap())).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		(status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
	}

	#[tokio::test]
	async fn declare_creates_an_aspect_the_read_surface_lists() {
		let dir = TempDir::new().unwrap();
		let router = router_with_empty_store(&dir).await;
		let body = serde_json::json!({ "name": "price", "physical_type": "f64", "timestamp_unit": "seconds" });
		let (status, body) = post_json(router, "/api/v1/storage/aspects", &body).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!(body["aspect"]["name"], "price");
		assert_eq!(body["aspect"]["physical_type"], "f64");
		assert_eq!(body["aspect"]["timestamp_unit"], "seconds");
		// value_tolerance defaulted to exact.
		assert_eq!(body["aspect"]["value_tolerance"], "0");

		// And the read surface now lists it.
		let router = router_with_empty_store(&dir).await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/aspects").body(Body::empty()).unwrap()).await.unwrap();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let listed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert_eq!(listed["aspects"][0]["name"], "price");
	}

	#[tokio::test]
	async fn declare_scaled_i64_requires_scale() {
		let dir = TempDir::new().unwrap();
		let router = router_with_empty_store(&dir).await;
		let body = serde_json::json!({ "name": "temp", "physical_type": "scaled_i64", "timestamp_unit": "millis" });
		let (status, body) = post_json(router, "/api/v1/storage/aspects", &body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("requires a `scale`"));
	}

	#[tokio::test]
	async fn declare_scaled_i64_with_scale_succeeds() {
		let dir = TempDir::new().unwrap();
		let router = router_with_empty_store(&dir).await;
		let body = serde_json::json!({ "name": "temp", "physical_type": "scaled_i64", "scale": 2, "timestamp_unit": "millis" });
		let (status, body) = post_json(router, "/api/v1/storage/aspects", &body).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!(body["aspect"]["physical_type"], "scaled_i64");
		assert_eq!(body["aspect"]["timestamp_unit"], "millis");
	}

	#[tokio::test]
	async fn declare_unknown_physical_type_is_bad_request() {
		let dir = TempDir::new().unwrap();
		let router = router_with_empty_store(&dir).await;
		let body = serde_json::json!({ "name": "x", "physical_type": "float", "timestamp_unit": "seconds" });
		let (status, body) = post_json(router, "/api/v1/storage/aspects", &body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("unknown physical_type"));
	}

	#[tokio::test]
	async fn declare_unknown_timestamp_unit_is_bad_request() {
		let dir = TempDir::new().unwrap();
		let router = router_with_empty_store(&dir).await;
		let body = serde_json::json!({ "name": "x", "physical_type": "f64", "timestamp_unit": "fortnights" });
		let (status, body) = post_json(router, "/api/v1/storage/aspects", &body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("unknown timestamp_unit"));
	}

	#[tokio::test]
	async fn declare_without_store_is_unavailable() {
		let router = app_with_state(AppState::new());
		let body = serde_json::json!({ "name": "x", "physical_type": "f64", "timestamp_unit": "seconds" });
		let (status, _body) = post_json(router, "/api/v1/storage/aspects", &body).await;
		assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
	}
}
