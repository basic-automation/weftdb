//! The stored-range query endpoints (roadmap Phase 2 — raw range / stored
//! queries), serving DSP's on-disk Storage v2 segments as **Apache Arrow IPC
//! stream bytes**.
//!
//! These are the first endpoints that read persisted measurement data rather than
//! operating on a request-supplied series: a client asks for the rows of an aspect
//! in a time (or value) window, and the server prunes the segment index, opens only
//! the overlapping `.dspseg` files, and streams the result back in the portable,
//! self-describing Arrow IPC wire form (`application/vnd.apache.arrow.stream`) —
//! ready for `DataFusion`, pandas/`PyArrow`, Arrow Flight, or a `.arrow` file.
//!
//! ## Where the bytes come from (and the dependency boundary)
//!
//! The handler is a thin shell over
//! [`dsp_arrow_store::read_time_range_to_ipc_bytes`] /
//! [`read_value_range_to_ipc_bytes`](dsp_arrow_store::read_value_range_to_ipc_bytes).
//! That bridge crate is the only place allowed to depend on **both** `database`
//! (the segment store) and `dsp-arrow` (the Arrow tree), so the heavy `arrow-*`
//! dependency never reaches the lean hot-path core — `dsp-server` pulls it in only
//! transitively, here at the API surface, which is its proper home.
//!
//! ## The store is optional
//!
//! When the operator has not configured a store root the [`AppState`] carries no
//! [`SegmentStore`](database::SegmentStore), and these endpoints answer
//! `503 Service Unavailable`. An aspect that was never declared in the store is a
//! `404 Not Found` (its timestamp unit / encoding is unknown, so there is nothing
//! to read), and any other read failure is a `500`.

use axum::{
	body::Body, extract::{Path, Query, State}, http::{header, StatusCode}, response::{IntoResponse, Response}, Json
};
use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};

use crate::state::AppState;

/// The Arrow IPC stream content type, per the Apache Arrow conventions.
const ARROW_STREAM_CONTENT_TYPE: &str = "application/vnd.apache.arrow.stream";

/// Query parameters for the time-range read.
///
/// An inclusive `[start, end]` window of integer epoch timestamps **in the
/// aspect's declared `TimeUnit`** (the store keeps timestamps as integers; the
/// unit travels in the response schema metadata).
#[derive(Debug, Clone, Deserialize)]
pub struct TimeRangeParams {
	/// Inclusive window start (epoch integer in the aspect's declared unit).
	pub start: i64,
	/// Inclusive window end (epoch integer in the aspect's declared unit).
	pub end: i64,
}

/// Query parameters for the value-range read: the inclusive `[lo, hi]` value band,
/// each parsed losslessly as a `BigDecimal` (DSP's logical numeric type — no
/// float round-trip on the wire).
#[derive(Debug, Clone, Deserialize)]
pub struct ValueRangeParams {
	/// Inclusive lower value bound (decimal text).
	pub lo: String,
	/// Inclusive upper value bound (decimal text).
	pub hi: String,
}

/// An error from a storage endpoint, rendered as `{"error": "..."}` with the
/// status code that fits the cause.
#[derive(Debug)]
pub enum StorageError {
	/// No segment store is configured (no store root) → 503.
	Unconfigured,
	/// The aspect was never declared in the store → 404.
	NotFound(String),
	/// A malformed request parameter (e.g. an unparseable value bound) → 400.
	BadRequest(String),
	/// An underlying read or serialization failure → 500.
	Internal(String),
}

/// JSON error envelope (mirrors the interpolation endpoint's shape).
#[derive(Debug, Serialize)]
struct ErrorBody {
	error: String,
}

impl IntoResponse for StorageError {
	fn into_response(self) -> Response {
		let (status, error) = match self {
			Self::Unconfigured => (StatusCode::SERVICE_UNAVAILABLE, "no segment store is configured on this server".to_string()),
			Self::NotFound(message) => (StatusCode::NOT_FOUND, message),
			Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
			Self::Internal(message) => (StatusCode::INTERNAL_SERVER_ERROR, message),
		};
		(status, Json(ErrorBody { error })).into_response()
	}
}

/// Classify a bridge read error: an undeclared aspect (its schema is unknown) is a
/// `404`, everything else (libSQL prune, filesystem read, corrupt frame, IPC
/// serialization) is a `500`.
fn classify_read_error(err: &anyhow::Error) -> StorageError {
	let message = err.to_string();
	if message.contains("no declared schema") {
		StorageError::NotFound(message)
	} else {
		StorageError::Internal(message)
	}
}

/// Build the `200 OK` Arrow-IPC-stream response from the serialized bytes.
fn arrow_stream_response(bytes: Vec<u8>) -> Response {
	([(header::CONTENT_TYPE, ARROW_STREAM_CONTENT_TYPE)], Body::from(bytes)).into_response()
}

/// Handle `GET /api/v1/storage/{aspect}/range?start&end`.
///
/// Reads the rows of `aspect` in the inclusive `[start, end]` timestamp window and
/// streams them as Arrow IPC bytes (typed value column for the aspect's declared
/// encoding).
///
/// # Errors
///
/// [`StorageError::Unconfigured`] when no store is attached,
/// [`StorageError::NotFound`] when the aspect is undeclared, and
/// [`StorageError::Internal`] on a read/serialization failure.
pub async fn storage_time_range(State(state): State<AppState>, Path(aspect): Path<String>, Query(params): Query<TimeRangeParams>) -> Result<Response, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	let result = dsp_arrow_store::read_time_range_to_ipc_bytes(&store, &aspect, params.start, params.end).await;
	drop(store);
	Ok(arrow_stream_response(result.map_err(|err| classify_read_error(&err))?))
}

/// Handle `GET /api/v1/storage/{aspect}/value-range?lo&hi`.
///
/// Reads the present rows of `aspect` whose value falls in the inclusive `[lo, hi]`
/// band and streams them as Arrow IPC bytes (lossless decimal-text value column).
///
/// # Errors
///
/// As [`storage_time_range`], plus [`StorageError::BadRequest`] when a value bound
/// does not parse as a decimal.
pub async fn storage_value_range(State(state): State<AppState>, Path(aspect): Path<String>, Query(params): Query<ValueRangeParams>) -> Result<Response, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	let lo: BigDecimal = params.lo.parse().map_err(|_| StorageError::BadRequest(format!("`lo` is not a decimal: {:?}", params.lo)))?;
	let hi: BigDecimal = params.hi.parse().map_err(|_| StorageError::BadRequest(format!("`hi` is not a decimal: {:?}", params.hi)))?;
	let result = dsp_arrow_store::read_value_range_to_ipc_bytes(&store, &aspect, &lo, &hi).await;
	drop(store);
	Ok(arrow_stream_response(result.map_err(|err| classify_read_error(&err))?))
}

#[cfg(test)]
mod tests {
	use std::{str::FromStr, sync::Arc};

	use axum::{
		body::Body, http::{Request, StatusCode}
	};
	use bigdecimal::BigDecimal;
	use database::SegmentStore;
	use dsp_arrow::{read_ipc_stream, record_batches_to_columns};
	use dsp_physical_type::{AspectSchema, PhysicalType, TimeUnit};
	use tempfile::TempDir;
	use tower::ServiceExt;

	use crate::{app_with_state, AppState};

	fn bd(s: &str) -> BigDecimal {
		BigDecimal::from_str(s).expect("test literal parses")
	}

	/// Open a store under `dir`, declare `price` (F64, seconds), seal one batch, and
	/// hand back the ready store. The significant-`Drop` `SegmentStore` lives only
	/// until the tail `Arc::new` here, so it never trips the drop-tightening lint in
	/// the caller.
	async fn sealed_price_store(dir: &TempDir) -> Arc<SegmentStore> {
		let store = SegmentStore::open(dir.path()).await.expect("opens store");
		store.declare("price", &AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)).await.expect("declares aspect");
		store.seal_declared("price", &[100_i64, 110, 120, 130, 140], &[bd("1.5"), bd("2.5"), bd("3.5"), bd("4.5"), bd("5.5")]).await.expect("seals");
		Arc::new(store)
	}

	/// Build a router whose state carries a fresh segment store with one declared,
	/// sealed aspect (`price`, F64, seconds), returning the temp dir so it outlives
	/// the test.
	async fn router_with_sealed_price() -> (TempDir, axum::Router) {
		let dir = TempDir::new().expect("temp dir");
		let store = sealed_price_store(&dir).await;
		(dir, app_with_state(AppState::new().with_store(store)))
	}

	#[tokio::test]
	async fn time_range_streams_arrow_ipc_bytes() {
		let (_dir, router) = router_with_sealed_price().await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/price/range?start=110&end=130").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		assert_eq!(response.headers().get("content-type").unwrap(), "application/vnd.apache.arrow.stream");

		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let batches = read_ipc_stream(&bytes).expect("reads stream");
		let (ts, vs) = record_batches_to_columns(&batches).expect("reads back columns");
		assert_eq!(ts, vec![110, 120, 130]);
		assert_eq!(vs, vec![Some(bd("2.5")), Some(bd("3.5")), Some(bd("4.5"))]);
	}

	#[tokio::test]
	async fn value_range_streams_only_in_band_rows() {
		let (_dir, router) = router_with_sealed_price().await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/price/value-range?lo=2.5&hi=4.5").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let batches = read_ipc_stream(&bytes).expect("reads stream");
		let (_ts, vs) = record_batches_to_columns(&batches).expect("reads back columns");
		assert_eq!(vs, vec![Some(bd("2.5")), Some(bd("3.5")), Some(bd("4.5"))]);
	}

	#[tokio::test]
	async fn empty_window_is_a_valid_zero_row_stream() {
		let (_dir, router) = router_with_sealed_price().await;
		// A window past every stored point still streams a valid one-batch stream.
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/price/range?start=1000&end=2000").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let batches = read_ipc_stream(&bytes).expect("reads stream");
		assert_eq!(batches.len(), 1);
		assert_eq!(batches[0].num_rows(), 0);
	}

	#[tokio::test]
	async fn undeclared_aspect_is_not_found() {
		let (_dir, router) = router_with_sealed_price().await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/never_declared/range?start=0&end=100").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::NOT_FOUND);
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert!(body["error"].as_str().unwrap().contains("no declared schema"));
	}

	#[tokio::test]
	async fn unparseable_value_bound_is_bad_request() {
		let (_dir, router) = router_with_sealed_price().await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/price/value-range?lo=abc&hi=10").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::BAD_REQUEST);
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert!(body["error"].as_str().unwrap().contains("not a decimal"));
	}

	#[tokio::test]
	async fn storage_endpoint_without_store_is_unavailable() {
		// A router with no configured store (the default) answers 503.
		let router = app_with_state(AppState::new());
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/price/range?start=0&end=100").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
	}
}
