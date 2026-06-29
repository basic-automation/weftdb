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
	extract::{Path, State}, http::StatusCode, response::{IntoResponse, Response}, Json
};
use bigdecimal::BigDecimal;
use dsp_physical_type::{AspectSchema, PhysicalType, SegmentDescriptor, TimeUnit};
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

/// One point in an ingest batch: an integer epoch timestamp (in the aspect's
/// declared [`TimeUnit`]) and its value as lossless decimal text, or `null` for an
/// absent/null row.
#[derive(Debug, Clone, Deserialize)]
pub struct IngestPoint {
	/// Row timestamp (epoch integer in the aspect's declared unit).
	pub timestamp: i64,
	/// The value as decimal text (no float round-trip — hard constraint #4), or
	/// `null` for a null row (sealed into the segment's quality mask).
	pub value: Option<String>,
}

/// Request body for `POST /api/v1/storage/{aspect}/points` — seal a batch of points
/// into the aspect's declared schema.
///
/// When `rows_per_page` is set the batch seals into a **paged** segment (rows
/// partitioned into pages of that height for intra-segment page skipping);
/// otherwise it seals into a single-block segment.
#[derive(Debug, Clone, Deserialize)]
pub struct IngestRequest {
	/// The points to seal, in caller order (the seal records whether they are
	/// time-sorted).
	pub points: Vec<IngestPoint>,
	/// Optional page height — when present, seal a paged segment of this many rows
	/// per page (the last page may be shorter).
	#[serde(default)]
	pub rows_per_page: Option<usize>,
}

/// Response body for a successful ingest: the sealed segment's descriptor summary,
/// the inputs a later read prunes on.
#[derive(Debug, Clone, Serialize)]
pub struct IngestResponse {
	/// The aspect the batch was sealed into.
	pub aspect: String,
	/// The id assigned to the new segment (monotonic within the aspect).
	pub segment_id: u64,
	/// The `.dspseg` frame format version sealed (2 single-block, 3 paged).
	pub format_version: u16,
	/// Total rows sealed (present and null).
	pub row_count: usize,
	/// Null rows sealed.
	pub null_count: usize,
	/// The realized on-disk frame size in bytes (the bytes/point numerator).
	pub byte_len: u64,
	/// Smallest timestamp in the sealed segment, or `null` if it was empty.
	pub min_ts: Option<i64>,
	/// Largest timestamp in the sealed segment, or `null` if it was empty.
	pub max_ts: Option<i64>,
}

/// Classify a seal error: an encode/tolerance failure is a client-data problem
/// (the supplied values cannot be represented under the aspect's declared encoding
/// within its tolerance — hard constraint #4) → `400`; anything else (filesystem,
/// libSQL) is a `500`.
fn classify_seal_error(err: &anyhow::Error) -> StorageError {
	let message = err.to_string();
	if message.contains("seal failed") || message.contains("paged seal failed") {
		StorageError::BadRequest(message)
	} else {
		StorageError::Internal(message)
	}
}

/// Handle `POST /api/v1/storage/{aspect}/points`: seal a batch of points into
/// `aspect`'s declared schema.
///
/// Splits the batch into a dense timestamp column and an `Option`-valued value
/// column; a batch with any `null` value seals through the nullable (quality-mask)
/// path, an all-present batch through the dense path; `rows_per_page` selects a
/// paged frame. Returns `201 Created` with the sealed segment's descriptor.
///
/// # Errors
///
/// [`StorageError::Unconfigured`] (no store), [`StorageError::NotFound`] (aspect
/// undeclared), [`StorageError::BadRequest`] (empty batch, an unparseable value, or
/// values unrepresentable under the declared encoding/tolerance), and
/// [`StorageError::Internal`] on a filesystem/control-plane failure.
pub async fn ingest_points(State(state): State<AppState>, Path(aspect): Path<String>, Json(request): Json<IngestRequest>) -> Result<Response, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	if request.points.is_empty() {
		return Err(StorageError::BadRequest("no points to ingest (`points` is empty)".to_string()));
	}
	// An undeclared aspect is a clean 404 (its encoding is unknown) rather than the
	// seal's generic error; fetching the schema here also lets the nullable/paged
	// seal variants take it directly.
	let Some(schema) = store.schema_for(&aspect).await.map_err(|err| StorageError::Internal(err.to_string()))? else {
		return Err(StorageError::NotFound(format!("aspect `{aspect}` has no declared schema in the segment store")));
	};

	let mut timestamps = Vec::with_capacity(request.points.len());
	let mut values = Vec::with_capacity(request.points.len());
	let mut any_null = false;
	for point in &request.points {
		timestamps.push(point.timestamp);
		match &point.value {
			None => {
				any_null = true;
				values.push(None);
			}
			Some(text) => {
				let parsed: BigDecimal = text.parse().map_err(|_| StorageError::BadRequest(format!("value {text:?} (at timestamp {}) is not a decimal", point.timestamp)))?;
				values.push(Some(parsed));
			}
		}
	}

	let descriptor = seal_batch(&store, &aspect, &schema, &timestamps, &values, any_null, request.rows_per_page).await;
	drop(store);
	let descriptor = descriptor.map_err(|err| classify_seal_error(&err))?;
	let response = IngestResponse {
		aspect,
		segment_id: descriptor.id,
		format_version: descriptor.format_version,
		row_count: descriptor.row_count,
		null_count: descriptor.null_count,
		byte_len: descriptor.byte_len,
		min_ts: descriptor.min_ts,
		max_ts: descriptor.max_ts,
	};
	Ok((StatusCode::CREATED, Json(response)).into_response())
}

/// Dispatch the right seal path for the batch: nullable vs dense × paged vs
/// single-block. Kept as a free async fn taking `&SegmentStore` so the handler can
/// drop its store handle before building the response.
async fn seal_batch(store: &database::SegmentStore, aspect: &str, schema: &AspectSchema, timestamps: &[i64], values: &[Option<BigDecimal>], any_null: bool, rows_per_page: Option<usize>) -> anyhow::Result<SegmentDescriptor> {
	match (rows_per_page, any_null) {
		(Some(rows_per_page), true) => store.seal_paged_nullable(aspect, schema, timestamps, values, rows_per_page).await,
		(Some(rows_per_page), false) => store.seal_paged(aspect, schema, timestamps, &present_values(values), rows_per_page).await,
		(None, true) => store.seal_nullable(aspect, schema, timestamps, values).await,
		(None, false) => store.seal(aspect, schema, timestamps, &present_values(values)).await,
	}
}

/// Unwrap an all-present value column to the dense `BigDecimal` slice the dense seal
/// paths take. Only called when the caller has verified no value is `None`.
fn present_values(values: &[Option<BigDecimal>]) -> Vec<BigDecimal> {
	values.iter().map(|value| value.clone().unwrap_or_default()).collect()
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

	/// Declare `price` (F64, seconds) in a fresh store under `dir`, returning a router
	/// over that store.
	async fn router_with_declared_price(dir: &TempDir) -> axum::Router {
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens store"));
		store.declare("price", &dsp_physical_type::AspectSchema::new(dsp_physical_type::PhysicalType::F64, "0".parse().unwrap(), dsp_physical_type::TimeUnit::Seconds)).await.expect("declares");
		app_with_state(AppState::new().with_store(store))
	}

	#[tokio::test]
	async fn ingest_seals_a_batch_the_read_surface_returns() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let body = serde_json::json!({ "points": [
			{ "timestamp": 100, "value": "1.5" },
			{ "timestamp": 110, "value": "2.5" },
			{ "timestamp": 120, "value": "3.5" },
		] });
		let (status, body) = post_json(router, "/api/v1/storage/price/points", &body).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!(body["aspect"], "price");
		assert_eq!(body["row_count"], 3);
		assert_eq!(body["null_count"], 0);
		assert_eq!(body["min_ts"], 100);
		assert_eq!(body["max_ts"], 120);
		assert!(body["byte_len"].as_u64().unwrap() > 0);

		// The read surface returns exactly what was sealed.
		let router = router_with_declared_price_reopened(&dir).await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/price/points?start=100&end=120").body(Body::empty()).unwrap()).await.unwrap();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let read: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert_eq!(read["count"], 3);
		assert_eq!(read["points"][1]["value"], "2.5");
	}

	/// Reopen a router over an existing store dir (no re-declaration — the catalog
	/// persists), for reading back what a prior request sealed.
	async fn router_with_declared_price_reopened(dir: &TempDir) -> axum::Router {
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("reopens store"));
		app_with_state(AppState::new().with_store(store))
	}

	#[tokio::test]
	async fn ingest_nullable_batch_records_null_count() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let body = serde_json::json!({ "points": [
			{ "timestamp": 100, "value": "1.5" },
			{ "timestamp": 110, "value": null },
			{ "timestamp": 120, "value": "3.5" },
		] });
		let (status, body) = post_json(router, "/api/v1/storage/price/points", &body).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!(body["row_count"], 3);
		assert_eq!(body["null_count"], 1);
	}

	#[tokio::test]
	async fn ingest_paged_batch_seals_paged_frame() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let body = serde_json::json!({ "rows_per_page": 2, "points": [
			{ "timestamp": 100, "value": "1.5" },
			{ "timestamp": 110, "value": "2.5" },
			{ "timestamp": 120, "value": "3.5" },
			{ "timestamp": 130, "value": "4.5" },
		] });
		let (status, body) = post_json(router, "/api/v1/storage/price/points", &body).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		// Paged frames carry the paged format version (3), distinct from single-block (2).
		assert_eq!(body["format_version"], 3);
		assert_eq!(body["row_count"], 4);
	}

	#[tokio::test]
	async fn ingest_into_undeclared_aspect_is_not_found() {
		let dir = TempDir::new().unwrap();
		let router = router_with_empty_store(&dir).await;
		let body = serde_json::json!({ "points": [{ "timestamp": 1, "value": "1" }] });
		let (status, body) = post_json(router, "/api/v1/storage/ghost/points", &body).await;
		assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
	}

	#[tokio::test]
	async fn ingest_unparseable_value_is_bad_request() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let body = serde_json::json!({ "points": [{ "timestamp": 1, "value": "not-a-number" }] });
		let (status, body) = post_json(router, "/api/v1/storage/price/points", &body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("is not a decimal"));
	}

	#[tokio::test]
	async fn ingest_empty_batch_is_bad_request() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let body = serde_json::json!({ "points": [] });
		let (status, body) = post_json(router, "/api/v1/storage/price/points", &body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("no points"));
	}

	#[tokio::test]
	async fn ingest_value_exceeding_tolerance_is_rejected() {
		// Declare a scaled_i64 with scale 0 (integers only) and tolerance 0: a
		// fractional value cannot be represented exactly and must be rejected, not
		// silently downcast (hard constraint #4).
		let dir = TempDir::new().unwrap();
		let store = Arc::new(SegmentStore::open(dir.path()).await.unwrap());
		store.declare("counts", &dsp_physical_type::AspectSchema::new(dsp_physical_type::PhysicalType::ScaledI64 { scale: 0 }, "0".parse().unwrap(), dsp_physical_type::TimeUnit::Seconds)).await.unwrap();
		let router = app_with_state(AppState::new().with_store(store));
		let body = serde_json::json!({ "points": [{ "timestamp": 1, "value": "1.5" }] });
		let (status, body) = post_json(router, "/api/v1/storage/counts/points", &body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("seal failed"));
	}

	#[tokio::test]
	async fn ingest_without_store_is_unavailable() {
		let router = app_with_state(AppState::new());
		let body = serde_json::json!({ "points": [{ "timestamp": 1, "value": "1" }] });
		let (status, _body) = post_json(router, "/api/v1/storage/price/points", &body).await;
		assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
	}
}
