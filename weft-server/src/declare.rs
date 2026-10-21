//! Declaring an aspect: the catalog-side half of the write path.
//!
//! One endpoint, `POST /api/v1/storage/aspects`, and the request types that
//! describe a schema. The wire token for a physical type is parsed here and
//! nowhere else, so a name typo and an unsupported encoding fail the same way.
//!
//! The heavy `arrow-*` tree never appears here: this is a plain JSON handler
//! over the `database` control plane.

use axum::{
	extract::State, http::StatusCode, response::{IntoResponse, Response}, Json
};
use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};

use crate::{
	error::{self, ApiError, StrictJson}, state::AppState, storage::AspectInfo
};

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
	/// scope; a re-declaration overwrites). It also names the aspect's files on disk, so
	/// it must pass [`weftdb::aspect_name::validate`]: at most 160 bytes, no `/`, `\`,
	/// control characters, leading `.`, trailing `.` or space, not a Windows device name
	/// such as `CON` or `nul:x`, and, on Windows, none of `<>:"|?*`.
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
fn schema_from_request(request: &DeclareAspectRequest) -> Result<AspectSchema, ApiError> {
	let physical_type = parse_physical_type(&request.physical_type, request.scale).map_err(ApiError::bad_request)?;
	let timestamp_unit = parse_time_unit(&request.timestamp_unit).map_err(ApiError::bad_request)?;
	let tolerance_text = request.value_tolerance.as_deref().unwrap_or("0");
	let value_tolerance: BigDecimal = tolerance_text.parse().map_err(|_| ApiError::bad_request(format!("`value_tolerance` is not a decimal: {tolerance_text:?}")))?;
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
/// [`ApiError::unconfigured`] when no store is attached,
/// [`Code::InvalidAspectName`](crate::error::Code::InvalidAspectName) when the name is not one the store can
/// address as a frame file name, [`ApiError::bad_request`] when a physical type, time unit or
/// tolerance does not parse, and a `500` on a control-plane write failure.
pub async fn declare_aspect(State(state): State<AppState>, StrictJson(request): StrictJson<DeclareAspectRequest>) -> Result<Response, ApiError> {
	let store = state.store().cloned().ok_or(ApiError::unconfigured())?;
	drop(state);
	// The name becomes part of every frame's file name: refuse one that could leave the
	// store's `segments/` directory before anything is written. This has its own code in the §4
	// table, so a client branches on `invalid_aspect_name` and on `details.reason` rather than on
	// a sentence shared with every other 400 this route can send.
	weftdb::aspect_name::validate(&request.name).map_err(|err| error::aspect_name_refusal(&err))?;
	let schema = schema_from_request(&request)?;
	let result = store.declare(&request.name, &schema).await;
	drop(store);
	// A refusal the store raises itself is classified the way a read's is — it rejects the same
	// unsafe names, and a root this binary will not open is `503 incompatible_store`. A write
	// that failed for a reason the table has no name for stays a `500` with the cause logged.
	result.map_err(|err| error::classify_typed(&err).unwrap_or_else(|| ApiError::internal(&err)))?;
	let aspect = AspectInfo { name: request.name, physical_type: schema.value.name(), value_tolerance: schema.value_tolerance.to_string(), timestamp_unit: schema.timestamp_unit.name() };
	Ok((StatusCode::CREATED, Json(DeclareAspectResponse { aspect })).into_response())
}

#[cfg(test)]

mod tests {

	use std::sync::Arc;

	use axum::{
		body::Body, http::{Request, StatusCode}
	};
	use tempfile::TempDir;
	use tower::ServiceExt;
	use weftdb::SegmentStore;

	use crate::{app_with_state, test_support::*, AppState};

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

	/// Regression: the declared name becomes part of every frame's file name, so a name
	/// that would leave the store's `segments/` directory — a `..` traversal or an
	/// absolute path — is a `400` in the JSON error envelope, and nothing is declared or
	/// written.
	#[tokio::test]
	async fn declare_rejects_path_escaping_names() {
		let dir = TempDir::new().unwrap();
		let store = Arc::new(SegmentStore::open(dir.path().join("store")).await.expect("opens store"));
		let state = AppState::new().with_store(store.clone());
		let absolute = dir.path().join("abs").to_string_lossy().into_owned();
		for name in ["../x", "../../x", absolute.as_str(), "a/b", "..", ".hidden", "CON", "a\u{0}b"] {
			let body = serde_json::json!({ "name": name, "physical_type": "f64", "timestamp_unit": "seconds" });
			let (status, body) = post_json(app_with_state(state.clone()), "/api/v1/storage/aspects", &body).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "{name:?} must be refused; body: {body}");
			assert!(body["error"].as_str().unwrap().contains("invalid aspect name"), "body: {body}");
		}
		// An ingest addressed through percent-encoded separators finds nothing declared.
		let points = serde_json::json!({ "points": [{ "timestamp": 0, "value": "1" }] });
		let (status, body) = post_json(app_with_state(state.clone()), "/api/v1/storage/..%2F..%2Fx/points", &points).await;
		assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
		let declared = store.list_declared_aspects().await.unwrap();
		drop(state);
		drop(store);
		assert!(declared.is_empty(), "nothing was declared: {declared:?}");
		assert!(frames_under(dir.path()).is_empty(), "no frame was written anywhere");
	}

	/// An aspect declared under an unsafe name by an older version (straight into the
	/// catalog) is refused on use with a `400` (ingest, read and per-aspect maintenance),
	/// the store-wide sweep lists it in `failed`, and no frame is written for it.
	#[tokio::test]
	async fn a_previously_declared_unsafe_name_is_a_bad_request_on_use() {
		let dir = TempDir::new().unwrap();
		let store = Arc::new(SegmentStore::open(dir.path().join("store")).await.expect("opens store"));
		let schema = weft_physical_type::AspectSchema::new(weft_physical_type::PhysicalType::F64, "0".parse().unwrap(), weft_physical_type::TimeUnit::Seconds);
		store.catalog().declare(store.database(), store.subject(), "../x", &schema).await.expect("raw declare");
		let state = AppState::new().with_store(store.clone());
		let points = serde_json::json!({ "points": [{ "timestamp": 0, "value": "1" }] });
		let (status, body) = post_json(app_with_state(state.clone()), "/api/v1/storage/..%2Fx/points", &points).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert_eq!(body["code"], "invalid_aspect_name", "the refusal is named by code, not by a sentence: {body}");
		assert!(body["error"].as_str().unwrap().contains("invalid aspect name"), "body: {body}");
		assert_eq!(body["details"]["aspect"], "../x", "the name travels beside the refusal: {body}");
		let response = app_with_state(state.clone()).oneshot(Request::builder().uri("/api/v1/storage/..%2Fx/range.csv?start=0&end=10").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::BAD_REQUEST);
		for uri in ["/api/v1/storage/..%2Fx/reconcile", "/api/v1/storage/..%2Fx/reconcile?threshold=1", "/api/v1/storage/..%2Fx/reconcile?hot_cold=true", "/api/v1/storage/..%2Fx/reconcile?overlaps=true", "/api/v1/storage/..%2Fx/squash", "/api/v1/storage/..%2Fx/squash?max_segments=1", "/api/v1/storage/..%2Fx/compact?target_rows=10"] {
			let (status, body) = post_empty(app_with_state(state.clone()), uri).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "POST {uri}: {body}");
			assert!(body["error"].as_str().unwrap().contains("invalid aspect name"), "POST {uri}: {body}");
		}
		let (status, body) = post_empty(app_with_state(state.clone()), "/api/v1/storage/reconcile").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["failed"][0]["aspect"], "../x", "the sweep reports the unsafe aspect: {body}");
		assert!(body["failed"][0]["error"].as_str().unwrap().contains("invalid aspect name"), "body: {body}");
		drop(state);
		drop(store);
		assert!(frames_under(dir.path()).is_empty(), "no frame was written anywhere");
	}
}
