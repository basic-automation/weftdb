//! The stored-range query endpoints (roadmap Phase 2 — raw range / stored
//! queries), serving WeftDB's on-disk Storage v2 segments as **Apache Arrow IPC
//! stream bytes**.
//!
//! These are the first endpoints that read persisted measurement data rather than
//! operating on a request-supplied series: a client asks for the rows of an aspect
//! in a time (or value) window, and the server prunes the segment index, opens only
//! the overlapping `.weftseg` files, and streams the result back in the portable,
//! self-describing Arrow IPC wire form (`application/vnd.apache.arrow.stream`) —
//! ready for `DataFusion`, pandas/`PyArrow`, Arrow Flight, or a `.arrow` file.
//!
//! ## Where the bytes come from (and the dependency boundary)
//!
//! The handler is a thin shell over
//! [`weft_arrow_store::read_time_range_to_ipc_bytes`] /
//! [`read_value_range_to_ipc_bytes`](weft_arrow_store::read_value_range_to_ipc_bytes).
//! That bridge crate is the only place allowed to depend on **both** `database`
//! (the segment store) and `weft-arrow` (the Arrow tree), so the heavy `arrow-*`
//! dependency never reaches the lean hot-path core — `weft-server` pulls it in only
//! transitively, here at the API surface, which is its proper home.
//!
//! ## The store is optional
//!
//! When the operator has not configured a store root the [`AppState`] carries no
//! [`SegmentStore`](weftdb::SegmentStore), and these endpoints answer
//! `503 Service Unavailable`. An aspect that was never declared in the store is a
//! `404 Not Found` (its timestamp unit / encoding is unknown, so there is nothing
//! to read), and any other read failure is a `500`.

use axum::{
	body::{Body, Bytes}, extract::{Path, Query, State}, http::{header, StatusCode}, response::{IntoResponse, Response}, Json
};
use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};
use tracing::Instrument as _;

use crate::{manage::IngestResponse, state::AppState};

/// The Arrow IPC stream content type, per the Apache Arrow conventions.
const ARROW_STREAM_CONTENT_TYPE: &str = "application/vnd.apache.arrow.stream";

/// The Apache Parquet file content type.
const PARQUET_CONTENT_TYPE: &str = "application/vnd.apache.parquet";

/// The CSV content type (RFC 4180), UTF-8.
const CSV_CONTENT_TYPE: &str = "text/csv; charset=utf-8";

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
/// each parsed losslessly as a `BigDecimal` (WeftDB's logical numeric type — no
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

/// Carry an [`ApiError`](crate::interpolate::ApiError) from the shared compute-side
/// parsers/serializers into this module's error type **without collapsing its status
/// class** — a bad token stays a `400`, an encoding failure stays a `500`. Lets the
/// storage downsample endpoint reuse the compute endpoint's token parsing and
/// Arrow/Parquet rendering verbatim.
impl From<crate::interpolate::ApiError> for StorageError {
	fn from(err: crate::interpolate::ApiError) -> Self {
		match err {
			crate::interpolate::ApiError::BadRequest(message) => Self::BadRequest(message),
			crate::interpolate::ApiError::Internal(message) => Self::Internal(message),
		}
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

/// Build the `200 OK` Parquet-file response from the serialized bytes.
fn parquet_response(bytes: Vec<u8>) -> Response {
	([(header::CONTENT_TYPE, PARQUET_CONTENT_TYPE)], Body::from(bytes)).into_response()
}

/// Build the `200 OK` CSV response from the rendered document.
fn csv_response(body: String) -> Response {
	([(header::CONTENT_TYPE, CSV_CONTENT_TYPE)], body).into_response()
}

/// Render `(timestamp, value)` rows as a two-column CSV document with a
/// `timestamp,value` header.
///
/// This is the universal, dependency-free interchange beside the typed
/// Arrow/Parquet exports: any spreadsheet, `DuckDB` `read_csv`, or pandas
/// `read_csv` consumes it. A null value is an empty field; present values are
/// lossless decimal text (the same no-float-round-trip guarantee the JSON read
/// gives — hard constraint #4). The two columns are a plain integer and a
/// `BigDecimal` `Display` form, neither of which can contain a comma, quote, or
/// newline, so no RFC-4180 field escaping is ever required (which is exactly why
/// hand-rolling the writer is safe here).
fn render_csv<I>(rows: I) -> String
where
	I: IntoIterator<Item = (i64, Option<BigDecimal>)>,
{
	use std::fmt::Write as _;
	let mut out = String::from("timestamp,value\n");
	for (timestamp, value) in rows {
		match value {
			Some(value) => {
				let _ = writeln!(out, "{timestamp},{value}");
			}
			None => {
				let _ = writeln!(out, "{timestamp},");
			}
		}
	}
	out
}

/// Resolve an aspect's declared schema or map its absence to a clean `404` (the
/// CSV reads go straight to the store rather than through the bridge, so they
/// reproduce the bridge's not-found semantics explicitly).
async fn require_schema(store: &weftdb::SegmentStore, aspect: &str) -> Result<weft_physical_type::AspectSchema, StorageError> {
	let schema = store.schema_for(aspect).await.map_err(|err| StorageError::Internal(err.to_string()))?;
	schema.ok_or_else(|| StorageError::NotFound(format!("aspect `{aspect}` has no declared schema in the segment store")))
}

/// Handle `GET /api/v1/storage/{aspect}/range.csv?start&end`.
///
/// The CSV counterpart of [`storage_time_range`] / [`storage_time_range_json`]:
/// reads the rows of `aspect` in the inclusive `[start, end]` timestamp window and
/// serves them as a `timestamp,value` CSV document (lossless decimal-text values,
/// empty field for a null row) — the lowest-common-denominator interchange a design
/// partner can load into a spreadsheet, `DuckDB`, or pandas without speaking Arrow.
///
/// # Errors
///
/// [`StorageError::Unconfigured`] when no store is attached,
/// [`StorageError::NotFound`] when the aspect is undeclared, and
/// [`StorageError::Internal`] on a read failure.
pub async fn storage_time_range_csv(State(state): State<AppState>, Path(aspect): Path<String>, Query(params): Query<TimeRangeParams>) -> Result<Response, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	require_schema(&store, &aspect).await?;
	let result = store.read_time_range(&aspect, params.start, params.end).instrument(tracing::info_span!("storage.range.read", %aspect, start = params.start, end = params.end, format = "csv")).await;
	drop(store);
	let (timestamps, values) = result.map_err(|err| classify_read_error(&err))?;
	Ok(csv_response(render_csv(timestamps.into_iter().zip(values))))
}

/// Handle `GET /api/v1/storage/{aspect}/value-range.csv?lo&hi`.
///
/// The CSV counterpart of [`storage_value_range`] / [`storage_value_range_json`]:
/// reads the present rows of `aspect` whose value falls in the inclusive `[lo, hi]`
/// band and serves them as a `timestamp,value` CSV document. Value-range reads
/// return only present rows, so every value field is populated.
///
/// # Errors
///
/// As [`storage_time_range_csv`], plus [`StorageError::BadRequest`] when a value
/// bound does not parse as a decimal.
pub async fn storage_value_range_csv(State(state): State<AppState>, Path(aspect): Path<String>, Query(params): Query<ValueRangeParams>) -> Result<Response, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	let lo: BigDecimal = params.lo.parse().map_err(|_| StorageError::BadRequest(format!("`lo` is not a decimal: {:?}", params.lo)))?;
	let hi: BigDecimal = params.hi.parse().map_err(|_| StorageError::BadRequest(format!("`hi` is not a decimal: {:?}", params.hi)))?;
	require_schema(&store, &aspect).await?;
	let result = store.read_value_range(&aspect, &lo, &hi).instrument(tracing::info_span!("storage.value_range.read", %aspect, %lo, %hi, format = "csv")).await;
	drop(store);
	let (timestamps, values) = result.map_err(|err| classify_read_error(&err))?;
	Ok(csv_response(render_csv(timestamps.into_iter().zip(values.into_iter().map(Some)))))
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
	// The columnar read + Arrow encode are fused in the arrow-store call, so one span
	// covers both stages of this format's hot path.
	let result = weft_arrow_store::read_time_range_to_ipc_bytes(&store, &aspect, params.start, params.end).instrument(tracing::info_span!("storage.range.read", %aspect, start = params.start, end = params.end, format = "arrow")).await;
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
	let result = weft_arrow_store::read_value_range_to_ipc_bytes(&store, &aspect, &lo, &hi).instrument(tracing::info_span!("storage.value_range.read", %aspect, %lo, %hi, format = "arrow")).await;
	drop(store);
	Ok(arrow_stream_response(result.map_err(|err| classify_read_error(&err))?))
}

/// Handle `GET /api/v1/storage/{aspect}/range.parquet?start&end`.
///
/// The Parquet counterpart of [`storage_time_range`]: reads the rows of `aspect`
/// in the inclusive `[start, end]` timestamp window and serves them as an Apache
/// Parquet file (typed value column for the aspect's declared encoding), ready to
/// load straight into `DuckDB`/pandas/`Polars`.
///
/// # Errors
///
/// As [`storage_time_range`] (unconfigured store, undeclared aspect, or a
/// read/serialization failure).
pub async fn storage_time_range_parquet(State(state): State<AppState>, Path(aspect): Path<String>, Query(params): Query<TimeRangeParams>) -> Result<Response, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	let result = weft_arrow_store::read_time_range_to_parquet_bytes(&store, &aspect, params.start, params.end).instrument(tracing::info_span!("storage.range.read", %aspect, start = params.start, end = params.end, format = "parquet")).await;
	drop(store);
	Ok(parquet_response(result.map_err(|err| classify_read_error(&err))?))
}

/// Handle `GET /api/v1/storage/{aspect}/value-range.parquet?lo&hi`.
///
/// The Parquet counterpart of [`storage_value_range`]: reads the present rows of
/// `aspect` whose value falls in the inclusive `[lo, hi]` band and serves them as
/// an Apache Parquet file (lossless decimal-text value column).
///
/// # Errors
///
/// As [`storage_value_range`], including [`StorageError::BadRequest`] when a value
/// bound does not parse as a decimal.
pub async fn storage_value_range_parquet(State(state): State<AppState>, Path(aspect): Path<String>, Query(params): Query<ValueRangeParams>) -> Result<Response, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	let lo: BigDecimal = params.lo.parse().map_err(|_| StorageError::BadRequest(format!("`lo` is not a decimal: {:?}", params.lo)))?;
	let hi: BigDecimal = params.hi.parse().map_err(|_| StorageError::BadRequest(format!("`hi` is not a decimal: {:?}", params.hi)))?;
	let result = weft_arrow_store::read_value_range_to_parquet_bytes(&store, &aspect, &lo, &hi).instrument(tracing::info_span!("storage.value_range.read", %aspect, %lo, %hi, format = "parquet")).await;
	drop(store);
	Ok(parquet_response(result.map_err(|err| classify_read_error(&err))?))
}

/// Query parameters for the Parquet ingest endpoint: an optional page height.
#[derive(Debug, Clone, Deserialize)]
pub struct ParquetIngestParams {
	/// Optional page height — seal a paged segment of this many rows per page.
	#[serde(default)]
	pub rows_per_page: Option<usize>,
	/// When `true`, reject an out-of-order batch (`400`) instead of sealing it — the
	/// Parquet counterpart of the JSON/CSV/ILP `require_sorted` flag.
	#[serde(default)]
	pub require_sorted: bool,
}

/// Classify a Parquet-ingest error: an undeclared aspect is a `404`; a malformed
/// Parquet body, a batch missing WeftDB's columns, or a value unrepresentable under the
/// declared encoding/tolerance (hard constraint #4) are client-data problems →
/// `400`; anything else (filesystem, libSQL) is a `500`.
fn classify_ingest_error(err: &anyhow::Error) -> StorageError {
	let message = err.to_string();
	if message.contains("no declared schema") {
		StorageError::NotFound(message)
	} else if message.contains("seal failed") || message.contains("paged seal failed") || message.contains("parquet error") || message.contains("is missing the") || message.contains("expected Arrow") || message.contains("required metadata") || message.contains("unrecognized time unit") || message.contains("does not parse") || message.contains("out-of-order timestamp") {
		StorageError::BadRequest(message)
	} else {
		StorageError::Internal(message)
	}
}

/// Handle `POST /api/v1/storage/{aspect}/parquet?rows_per_page`.
///
/// The write-side counterpart of the Parquet range export and the Parquet sibling
/// of the JSON / ILP ingest endpoints: ingests an Apache Parquet file (the request
/// body, `application/vnd.apache.parquet`) into `aspect`'s declared schema, sealing
/// one new segment. The decode + seal lives in `weft-arrow-store` (so the heavy
/// `arrow-*` tree stays off the lean core); the no-silent-downcast guarantee holds
/// — a value unrepresentable under the declared encoding/tolerance is rejected
/// `400`, never downcast. Returns `201 Created` with the sealed segment's descriptor.
///
/// # Errors
///
/// [`StorageError::Unconfigured`] (no store), [`StorageError::NotFound`] (aspect
/// undeclared), [`StorageError::BadRequest`] (empty/malformed Parquet body or values
/// unrepresentable under the declared encoding/tolerance), and
/// [`StorageError::Internal`] on a filesystem/control-plane failure.
pub async fn storage_ingest_parquet(State(state): State<AppState>, Path(aspect): Path<String>, Query(params): Query<ParquetIngestParams>, body: Bytes) -> Result<Response, StorageError> {
	let metrics = state.metrics().clone();
	metrics.record_ingest_request();
	let store = state.store().cloned().ok_or_else(|| {
		metrics.record_ingest_error();
		StorageError::Unconfigured
	})?;
	drop(state);
	let result = storage_ingest_parquet_inner(&store, &metrics, &aspect, params.rows_per_page, params.require_sorted, &body).await;
	drop(store);
	if result.is_err() {
		metrics.record_ingest_error();
	}
	result
}

/// The body of [`storage_ingest_parquet`], split out so the handler records an error
/// metric for any failure path uniformly.
async fn storage_ingest_parquet_inner(store: &weftdb::SegmentStore, metrics: &crate::metrics::SharedMetrics, aspect: &str, rows_per_page: Option<usize>, require_sorted: bool, body: &Bytes) -> Result<Response, StorageError> {
	if body.is_empty() {
		return Err(StorageError::BadRequest("empty Parquet body".to_string()));
	}
	// `storage.ingest.parquet` stage span (roadmap Phase 3 — the Arrow-decode-on-ingest
	// analogue of the JSON/CSV/ILP `storage.ingest.parse`+`seal` spans). The arrow-store
	// ingest is atomic (Parquet decode → typed-column seal in one call) and the leaf
	// `weft-arrow-store` stays tracing-free by design, so decode+seal ride one span here,
	// carrying the request's byte length and the sort-guard flag.
	let descriptor = weft_arrow_store::ingest_parquet_into_aspect(store, aspect, body, rows_per_page, require_sorted).instrument(tracing::info_span!("storage.ingest.parquet", %aspect, byte_len = body.len(), require_sorted, format = "parquet")).await.map_err(|err| classify_ingest_error(&err))?;
	metrics.record_ingest_seal(u64::try_from(descriptor.row_count).unwrap_or(u64::MAX));
	let response = IngestResponse { aspect: aspect.to_string(), segment_id: descriptor.id, format_version: descriptor.format_version, row_count: descriptor.row_count, null_count: descriptor.null_count, byte_len: descriptor.byte_len, min_ts: descriptor.min_ts, max_ts: descriptor.max_ts, time_sorted: descriptor.time_sorted };
	Ok((StatusCode::CREATED, Json(response)).into_response())
}

/// One stored row in the JSON range response: an integer timestamp and its value
/// as lossless decimal text, or `null` for a null row.
#[derive(Debug, Clone, Serialize)]
pub struct StoredPoint {
	/// Row timestamp (epoch integer in the aspect's declared unit).
	pub timestamp: i64,
	/// The stored value as decimal text, or `null` for a null row. Decimal text
	/// keeps every digit (no float round-trip — hard constraint #4).
	pub value: Option<String>,
}

/// Query parameters for the JSON points read: the inclusive `[start, end]` window
/// plus optional **pagination** (backlog B-rest — declarative `take`/`page` query
/// params over the REST facade).
///
/// `offset` skips that many rows of the window (in segment-seal then in-segment
/// order); `limit` caps how many are returned. Both are applied after the
/// (page-pruned) read, so a paginated request still only opens the segments the
/// window overlaps.
///
/// Two declarative aliases mirror the legacy `DSM-Database` REST surface:
/// `take` is an alias for `limit` (the page size — `limit` wins if both are
/// given), and `page` is a **1-based** page number that derives the offset from
/// the page size (`offset = (page - 1) * page_size`). When `page` is present it
/// supersedes any explicit `offset`; a `page` without a page size falls back to
/// page 1 (offset 0).
#[derive(Debug, Clone, Deserialize)]
pub struct PointsRangeParams {
	/// Inclusive window start (epoch integer in the aspect's declared unit).
	pub start: i64,
	/// Inclusive window end (epoch integer in the aspect's declared unit).
	pub end: i64,
	/// Skip this many leading rows of the window before returning (default 0).
	#[serde(default)]
	pub offset: Option<usize>,
	/// Return at most this many rows (default: all remaining after `offset`).
	#[serde(default)]
	pub limit: Option<usize>,
	/// Declarative alias for `limit` (the page size). `limit` takes precedence if
	/// both are supplied.
	#[serde(default)]
	pub take: Option<usize>,
	/// 1-based page number. With a page size (`limit`/`take`) it derives the
	/// offset and supersedes `offset`.
	#[serde(default)]
	pub page: Option<usize>,
	/// Opaque forward-iteration cursor (from a prior response's `next_cursor`). When
	/// present it supersedes `offset`/`page` as the start position; a malformed token
	/// is a `400`.
	#[serde(default)]
	pub cursor: Option<String>,
}

/// Response body for `GET /api/v1/storage/{aspect}/points` — the JSON (non-Arrow)
/// stored-range read.
#[derive(Debug, Clone, Serialize)]
pub struct StoredRangeResponse {
	/// The aspect read.
	pub aspect: String,
	/// The aspect's declared timestamp unit token (e.g. `"seconds"`), so a consumer
	/// can interpret the integer timestamps.
	pub time_unit: &'static str,
	/// Total rows in the window **before** pagination — the count a client pages
	/// through (so it knows whether more pages remain).
	pub total: usize,
	/// Number of rows returned in this page (after `offset`/`limit`).
	pub count: usize,
	/// The number of leading rows skipped (the applied `offset`, 0 if none).
	pub offset: usize,
	/// The effective page size applied (the resolved `limit`/`take`); absent when
	/// the read was unbounded.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub limit: Option<usize>,
	/// The 1-based page number served, present only for a page-based request
	/// (`page` supplied).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub page: Option<usize>,
	/// Opaque forward-iteration cursor for the **next** page (roadmap B-rest): present
	/// when more rows remain past this page, absent at the end of the window. A client
	/// pages by re-issuing the request with `?cursor=<next_cursor>` until it is absent.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub next_cursor: Option<String>,
	/// The rows in this page, in segment-seal then in-segment order.
	pub points: Vec<StoredPoint>,
}

/// Handle `GET /api/v1/storage/{aspect}/points?start&end&offset&limit&take&page`.
///
/// The JSON counterpart of the Arrow [`storage_time_range`] read, for clients that
/// do not speak Arrow IPC: returns the rows of `aspect` in the inclusive
/// `[start, end]` window as a lossless decimal-text point list, tagged with the
/// aspect's declared timestamp unit. Optional `offset`/`limit` paginate the window
/// (backlog B-rest), with the declarative `take` (alias for `limit`) and `page`
/// (1-based, derives the offset) aliases also accepted; `total` reports the
/// pre-pagination row count. For stable forward iteration a client follows the
/// opaque `next_cursor` (re-issuing with `?cursor=…` until it is absent) instead of
/// managing offsets; a cursor supersedes `offset`/`page`.
///
/// # Errors
///
/// [`StorageError::Unconfigured`] when no store is attached,
/// [`StorageError::NotFound`] when the aspect is undeclared (its timestamp unit is
/// unknown), and [`StorageError::Internal`] on a read failure.
pub async fn storage_time_range_json(State(state): State<AppState>, Path(aspect): Path<String>, Query(params): Query<PointsRangeParams>) -> Result<Json<StoredRangeResponse>, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	let schema = store.schema_for(&aspect).await.map_err(|err| StorageError::Internal(err.to_string()))?;
	let Some(schema) = schema else {
		return Err(StorageError::NotFound(format!("aspect `{aspect}` has no declared schema in the segment store")));
	};
	let result = store.read_time_range(&aspect, params.start, params.end).instrument(tracing::info_span!("storage.range.read", %aspect, start = params.start, end = params.end, format = "json")).await;
	drop(store);
	let (timestamps, values) = result.map_err(|err| classify_read_error(&err))?;
	let total = timestamps.len();
	let (offset, page_size, page) = resolve_pagination_with_cursor(params.offset, params.limit, params.take, params.page, params.cursor.as_deref())?;
	// Paginate + stringify under a serialize span: skip `offset` rows of the window,
	// take at most `page_size`.
	let points: Vec<StoredPoint> = tracing::info_span!("storage.range.serialize", total, offset).in_scope(|| {
		let paginated = timestamps.into_iter().zip(values).skip(offset);
		match page_size {
			Some(size) => paginated.take(size).map(|(timestamp, value)| StoredPoint { timestamp, value: value.map(|v| v.to_string()) }).collect(),
			None => paginated.map(|(timestamp, value)| StoredPoint { timestamp, value: value.map(|v| v.to_string()) }).collect(),
		}
	});
	let next_cursor = next_cursor(offset, points.len(), total);
	Ok(Json(StoredRangeResponse { aspect, time_unit: schema.timestamp_unit.name(), total, count: points.len(), offset, limit: page_size, page, next_cursor, points }))
}

/// Resolve the declarative B-rest pagination params shared by the JSON range reads
/// into `(offset, page_size, page_echo)`.
///
/// The page size is `limit` if present else the `take` alias. `page` (1-based)
/// derives the offset (`(page - 1) * page_size`) and supersedes an explicit
/// `offset`; a `page` without a page size falls back to page 1 (offset 0). The
/// returned `page_echo` is the served 1-based page (only for a page-based request).
fn resolve_pagination(offset: Option<usize>, limit: Option<usize>, take: Option<usize>, page: Option<usize>) -> (usize, Option<usize>, Option<usize>) {
	let page_size = limit.or(take);
	let offset = page.map_or_else(|| offset.unwrap_or(0), |page| page_size.map_or(0, |size| page.saturating_sub(1).saturating_mul(size)));
	(offset, page_size, page.map(|page| page.max(1)))
}

/// Encode a forward-iteration position — a row offset in the deterministic read order
/// — as an opaque **cursor** token (roadmap B-rest): the fixed-width hex of the
/// offset. Paired with [`decode_cursor`]. A client following `next_cursor` never
/// computes offsets itself; the token is stable while the window's rows are unchanged.
fn encode_cursor(offset: usize) -> String {
	format!("{offset:016x}")
}

/// Decode a [`encode_cursor`] token back to a row offset, or `None` when it is
/// malformed (a handler surfaces that as a `400`).
fn decode_cursor(token: &str) -> Option<usize> {
	usize::from_str_radix(token.trim(), 16).ok()
}

/// Resolve the effective start offset and the `page`/`next_cursor` echoes from the
/// declarative pagination params plus an optional forward-iteration `cursor`.
///
/// A present `cursor` supersedes `offset`/`page` (it *is* the resolved start position,
/// so the `page` echo is cleared); a malformed `cursor` is a `400`. Returns the
/// `(offset, page_size, page_echo)` triple that drives the slice.
fn resolve_pagination_with_cursor(offset: Option<usize>, limit: Option<usize>, take: Option<usize>, page: Option<usize>, cursor: Option<&str>) -> Result<(usize, Option<usize>, Option<usize>), StorageError> {
	let (offset, page_size, page) = resolve_pagination(offset, limit, take, page);
	match cursor {
		Some(token) => {
			let cursor_offset = decode_cursor(token).ok_or_else(|| StorageError::BadRequest(format!("`cursor` is not a valid token: {token:?}")))?;
			Ok((cursor_offset, page_size, None))
		}
		None => Ok((offset, page_size, page)),
	}
}

/// The forward-iteration cursor for the *next* page: `Some(token)` when rows remain
/// past the one just served (`start + count < total`), `None` at the end of the
/// window (so a client stops when `next_cursor` is absent).
fn next_cursor(start: usize, count: usize, total: usize) -> Option<String> {
	(start + count < total).then(|| encode_cursor(start + count))
}

/// Query parameters for the JSON value-range read: the inclusive `[lo, hi]` value
/// band (as decimal text) plus the same declarative B-rest pagination as the time
/// read.
#[derive(Debug, Clone, Deserialize)]
pub struct ValuePointsParams {
	/// Inclusive lower value bound (decimal text).
	pub lo: String,
	/// Inclusive upper value bound (decimal text).
	pub hi: String,
	/// Skip this many leading rows before returning (default 0).
	#[serde(default)]
	pub offset: Option<usize>,
	/// Return at most this many rows (default: all remaining after `offset`).
	#[serde(default)]
	pub limit: Option<usize>,
	/// Declarative alias for `limit` (the page size). `limit` wins if both are given.
	#[serde(default)]
	pub take: Option<usize>,
	/// 1-based page number; with a page size it derives the offset.
	#[serde(default)]
	pub page: Option<usize>,
	/// Opaque forward-iteration cursor (from a prior response's `next_cursor`);
	/// supersedes `offset`/`page`, malformed → `400`.
	#[serde(default)]
	pub cursor: Option<String>,
}

/// Handle `GET /api/v1/storage/{aspect}/value-points?lo&hi&offset&limit&take&page`.
///
/// The JSON counterpart of the Arrow [`storage_value_range`] read, for clients that
/// do not speak Arrow IPC: returns the **present** rows of `aspect` whose value
/// falls in the inclusive `[lo, hi]` band as a lossless decimal-text point list,
/// tagged with the aspect's declared timestamp unit. Value-range reads return only
/// present rows, so every row carries a value. Pagination matches the time read
/// (B-rest `offset`/`limit`/`take`/`page`).
///
/// # Errors
///
/// [`StorageError::Unconfigured`] (no store), [`StorageError::NotFound`] (aspect
/// undeclared), [`StorageError::BadRequest`] when a value bound does not parse as a
/// decimal, and [`StorageError::Internal`] on a read failure.
pub async fn storage_value_range_json(State(state): State<AppState>, Path(aspect): Path<String>, Query(params): Query<ValuePointsParams>) -> Result<Json<StoredRangeResponse>, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	let lo: BigDecimal = params.lo.parse().map_err(|_| StorageError::BadRequest(format!("`lo` is not a decimal: {:?}", params.lo)))?;
	let hi: BigDecimal = params.hi.parse().map_err(|_| StorageError::BadRequest(format!("`hi` is not a decimal: {:?}", params.hi)))?;
	let schema = store.schema_for(&aspect).await.map_err(|err| StorageError::Internal(err.to_string()))?;
	let Some(schema) = schema else {
		return Err(StorageError::NotFound(format!("aspect `{aspect}` has no declared schema in the segment store")));
	};
	let result = store.read_value_range(&aspect, &lo, &hi).instrument(tracing::info_span!("storage.value_range.read", %aspect, %lo, %hi, format = "json")).await;
	drop(store);
	let (timestamps, values) = result.map_err(|err| classify_read_error(&err))?;
	let total = timestamps.len();
	let (offset, page_size, page) = resolve_pagination_with_cursor(params.offset, params.limit, params.take, params.page, params.cursor.as_deref())?;
	let points: Vec<StoredPoint> = tracing::info_span!("storage.value_range.serialize", total, offset).in_scope(|| {
		let paginated = timestamps.into_iter().zip(values).skip(offset);
		match page_size {
			Some(size) => paginated.take(size).map(|(timestamp, value)| StoredPoint { timestamp, value: Some(value.to_string()) }).collect(),
			None => paginated.map(|(timestamp, value)| StoredPoint { timestamp, value: Some(value.to_string()) }).collect(),
		}
	});
	let next_cursor = next_cursor(offset, points.len(), total);
	Ok(Json(StoredRangeResponse { aspect, time_unit: schema.timestamp_unit.name(), total, count: points.len(), offset, limit: page_size, page, next_cursor, points }))
}

/// Query parameters for the single-instant point lookup: the instant `t` to read
/// (an epoch integer in the aspect's declared `TimeUnit`).
#[derive(Debug, Clone, Deserialize)]
pub struct PointParams {
	/// The instant to look up (epoch integer in the aspect's declared unit).
	pub t: i64,
}

/// Response body for `GET /api/v1/storage/{aspect}/at` — a single-instant point
/// lookup against the stored segments.
#[derive(Debug, Clone, Serialize)]
pub struct StoredPointResponse {
	/// The aspect read.
	pub aspect: String,
	/// The aspect's declared timestamp unit token (e.g. `"seconds"`), so a consumer
	/// can interpret the integer timestamp.
	pub time_unit: &'static str,
	/// The instant queried (echoed back, epoch integer in the declared unit).
	pub timestamp: i64,
	/// The present value at `timestamp` as lossless decimal text, or `null` when no
	/// present row carries it (no row at the instant, or the row(s) there are null).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub value: Option<String>,
	/// Whether a present value was found at the instant — `false` distinguishes a
	/// genuine miss from a present-but-serialization-omitted `value`.
	pub found: bool,
}

/// Handle `GET /api/v1/storage/{aspect}/at?t=<epoch>`.
///
/// The single-instant counterpart of the range reads (roadmap Phase 4.6
/// read-planner): returns the present value of `aspect` at exactly timestamp `t`,
/// or a `found: false` miss. The store prunes the segment index to the files whose
/// span covers `t`, opens only those, and resolves the instant with each segment's
/// persisted order signal — a `time_sorted` segment binary-searches, an
/// out-of-order one linear-scans (see
/// [`SegmentStore::read_point`](weftdb::SegmentStore::read_point)). Values are
/// lossless decimal text (no float round-trip — hard constraint #4).
///
/// # Errors
///
/// [`StorageError::Unconfigured`] when no store is attached,
/// [`StorageError::NotFound`] when the aspect is undeclared (its timestamp unit is
/// unknown), and [`StorageError::Internal`] on a read failure.
pub async fn storage_point(State(state): State<AppState>, Path(aspect): Path<String>, Query(params): Query<PointParams>) -> Result<Json<StoredPointResponse>, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	let schema = require_schema(&store, &aspect).await?;
	let result = store.read_point(&aspect, params.t).instrument(tracing::info_span!("storage.point.read", %aspect, t = params.t)).await;
	drop(store);
	let value = result.map_err(|err| classify_read_error(&err))?;
	let found = value.is_some();
	Ok(Json(StoredPointResponse { aspect, time_unit: schema.timestamp_unit.name(), timestamp: params.t, value: value.map(|v| v.to_string()), found }))
}

/// Query parameters for the batch point lookup: a comma-separated list of instants.
#[derive(Debug, Clone, Deserialize)]
pub struct MultiPointParams {
	/// Comma-separated epoch integers in the aspect's declared unit (e.g. `1000,2000,3000`).
	pub t: String,
}

/// One instant's result in a [`StoredPointsResponse`], echoed in the query order.
#[derive(Debug, Clone, Serialize)]
pub struct StoredPointEntry {
	/// The instant queried (epoch integer in the declared unit).
	pub timestamp: i64,
	/// The present value as lossless decimal text, or omitted on a miss.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub value: Option<String>,
	/// Whether a present value was found at the instant.
	pub found: bool,
}

/// Response body for `GET /api/v1/storage/{aspect}/at-multi` — a batch point lookup, one
/// entry per queried instant in query order.
#[derive(Debug, Clone, Serialize)]
pub struct StoredPointsResponse {
	/// The aspect read.
	pub aspect: String,
	/// The aspect's declared timestamp unit token (e.g. `"seconds"`).
	pub time_unit: &'static str,
	/// One result per queried instant, aligned to the `t` list.
	pub points: Vec<StoredPointEntry>,
}

/// Handle `GET /api/v1/storage/{aspect}/at-multi?t=<epoch>,<epoch>,…`.
///
/// The batch counterpart of [`storage_point`] (roadmap Phase 4/6): resolves many instants in
/// one pass through [`SegmentStore::read_points`](weftdb::SegmentStore::read_points), which
/// prunes the index once by the batch's whole span and decodes each surviving segment's
/// timestamp column once for the whole batch. Results are returned in the query order (an
/// instant may repeat). Values are lossless decimal text.
///
/// # Errors
///
/// [`StorageError::Unconfigured`] when no store is attached, [`StorageError::NotFound`] when
/// the aspect is undeclared, [`StorageError::BadRequest`] when the `t` list has a non-integer
/// entry, and [`StorageError::Internal`] on a read failure.
pub async fn storage_points(State(state): State<AppState>, Path(aspect): Path<String>, Query(params): Query<MultiPointParams>) -> Result<Json<StoredPointsResponse>, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	let schema = require_schema(&store, &aspect).await?;
	// Parse the comma-separated instants; a non-integer (or otherwise malformed) entry is a 400.
	let ts: Vec<i64> = params.t.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::parse::<i64>).collect::<Result<_, _>>().map_err(|e| StorageError::BadRequest(format!("invalid `t` list {:?}: {e}", params.t)))?;
	let result = store.read_points(&aspect, &ts).instrument(tracing::info_span!("storage.points.read", %aspect, count = ts.len())).await;
	drop(store);
	let values = result.map_err(|err| classify_read_error(&err))?;
	let points = ts.iter().zip(values).map(|(&timestamp, v)| StoredPointEntry { timestamp, found: v.is_some(), value: v.map(|v| v.to_string()) }).collect();
	Ok(Json(StoredPointsResponse { aspect, time_unit: schema.timestamp_unit.name(), points }))
}

/// One declared aspect's identity in the [`AspectListResponse`].
#[derive(Debug, Clone, Serialize)]
pub struct AspectInfo {
	/// The aspect name (unique within the store's `(database, subject)` scope).
	pub name: String,
	/// The declared physical encoding token (e.g. `"f64"`, `"scaled_i64"`).
	pub physical_type: &'static str,
	/// The declared per-value error tolerance (decimal text — `"0"` for an exact
	/// encoding).
	pub value_tolerance: String,
	/// The declared timestamp unit token (e.g. `"seconds"`, `"millis"`).
	pub timestamp_unit: &'static str,
}

/// Response body for `GET /api/v1/storage/aspects`.
#[derive(Debug, Clone, Serialize)]
pub struct AspectListResponse {
	/// The declared aspects, in store order.
	pub aspects: Vec<AspectInfo>,
}

/// Response body for `GET /api/v1/storage/{aspect}/stats` — the materialized
/// segment-set rollup, including the north-star storage cost in bytes per point.
#[derive(Debug, Clone, Serialize)]
pub struct AspectStatsResponse {
	/// The aspect this rollup describes.
	pub aspect: String,
	/// Number of sealed segments.
	pub segment_count: usize,
	/// Total rows (present and null) across every segment.
	pub total_rows: u64,
	/// Total null rows across every segment.
	pub total_nulls: u64,
	/// Total realized on-disk bytes across every `.weftseg` frame.
	pub total_bytes: u64,
	/// The north-star cost term: total framed bytes over total rows (0 when empty).
	pub bytes_per_point: f64,
	/// The number of sealed segments whose timestamps are not monotonic
	/// non-decreasing — an order-health signal (a growing count predicts rising
	/// read-scan cost, since an out-of-order segment cannot be binary-searched). Zero
	/// when every segment admits ordered access.
	pub unsorted_segments: usize,
	/// The number of sealed segments whose time window overlaps at least one other
	/// segment's — the *cross-segment* order-health signal (roadmap Phase 4.6),
	/// distinct from `unsorted_segments` (intra-segment disorder). Non-zero means late
	/// data re-entered an already-covered window; these are the cross-segment
	/// reconciliation candidates. Computed by an index scan (not the O(1) rollup).
	pub overlapping_segments: usize,
	/// Inclusive `[min, max]` timestamp span, or `null` when the aspect holds no
	/// non-empty segment.
	pub time_range: Option<[i64; 2]>,
	/// Inclusive `[min, max]` value span as decimal text, or `null` when the aspect
	/// holds no value-bearing segment.
	pub value_range: Option<[String; 2]>,
}

/// Response body for `GET /api/v1/storage/stats` — the store-wide aggregate.
#[derive(Debug, Clone, Serialize)]
pub struct StoreStatsResponse {
	/// Number of aspects with a materialized rollup.
	pub aspect_count: usize,
	/// Total sealed segments across every aspect.
	pub segment_count: usize,
	/// Total rows (present and null) across every aspect.
	pub total_rows: u64,
	/// Total null rows across every aspect.
	pub total_nulls: u64,
	/// Total realized on-disk bytes across every aspect's `.weftseg` frames.
	pub total_bytes: u64,
	/// The store-wide north-star cost term: total framed bytes over total rows.
	pub bytes_per_point: f64,
	/// Total out-of-order segments across every aspect — the store-wide order-health
	/// signal (0 when every sealed segment admits ordered access).
	pub unsorted_segments: usize,
	/// Total segments across every aspect whose time window overlaps another segment
	/// in the same aspect — the store-wide *cross-segment* order-health signal (roadmap
	/// Phase 4.6). Computed by an index scan per aspect (not the O(1) rollup).
	pub overlapping_segments: usize,
	/// Inclusive `[min, max]` timestamp span across the union of aspects, or `null`.
	pub time_range: Option<[i64; 2]>,
}

/// One registered database in the [`CatalogResponse`]: its name and the subjects
/// registered under it.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogDatabase {
	/// The database name.
	pub name: String,
	/// The subjects registered under this database, in name order.
	pub subjects: Vec<String>,
}

/// Response body for `GET /api/v1/storage/catalog` — the control-plane hierarchy
/// the configured store sits in.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogResponse {
	/// The database namespace this server's store is **scoped** to (its declares /
	/// seals / reads all happen here).
	pub database: String,
	/// The subject namespace this server's store is scoped to.
	pub subject: String,
	/// Every database registered in the store's `catalog.db`, each with its
	/// subjects — the full hierarchy a root holds, not just the active scope.
	pub databases: Vec<CatalogDatabase>,
}

/// Collect the registered `(database, subject)` hierarchy from a store's registry.
/// Kept as a free async fn taking `&SegmentStore` so the handler can drop the store
/// handle before building its response.
async fn collect_catalog(store: &weftdb::SegmentStore) -> anyhow::Result<Vec<CatalogDatabase>> {
	let database_names = store.registry().list_databases().await?;
	let mut databases = Vec::with_capacity(database_names.len());
	for name in database_names {
		let subjects = store.registry().list_subjects(&name).await?;
		databases.push(CatalogDatabase { name, subjects });
	}
	Ok(databases)
}

/// Handle `GET /api/v1/storage/catalog`: report the database/subject scope the
/// configured store is bound to, plus the full registered DB/subject hierarchy.
///
/// # Errors
///
/// [`StorageError::Unconfigured`] when no store is attached, or
/// [`StorageError::Internal`] on a control-plane read failure.
pub async fn storage_catalog(State(state): State<AppState>) -> Result<Json<CatalogResponse>, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	let scope_database = store.database().to_string();
	let scope_subject = store.subject().to_string();
	let result = collect_catalog(&store).await;
	drop(store);
	let databases = result.map_err(|err| StorageError::Internal(err.to_string()))?;
	Ok(Json(CatalogResponse { database: scope_database, subject: scope_subject, databases }))
}

/// Collect the declared aspects and their schemas into the response DTO. Kept as a
/// free async fn taking `&SegmentStore` so the handler can drop the store handle
/// before building its response.
async fn collect_aspects(store: &weftdb::SegmentStore) -> anyhow::Result<Vec<AspectInfo>> {
	let names = store.list_declared_aspects().await?;
	let mut aspects = Vec::with_capacity(names.len());
	for name in names {
		if let Some(schema) = store.schema_for(&name).await? {
			aspects.push(AspectInfo { name, physical_type: schema.value.name(), value_tolerance: schema.value_tolerance.to_string(), timestamp_unit: schema.timestamp_unit.name() });
		}
	}
	Ok(aspects)
}

/// Handle `GET /api/v1/storage/aspects`: list the store's declared aspects and the
/// physical encoding / timestamp unit each was declared under.
///
/// # Errors
///
/// [`StorageError::Unconfigured`] when no store is attached, or
/// [`StorageError::Internal`] on a control-plane read failure.
pub async fn storage_aspects(State(state): State<AppState>) -> Result<Json<AspectListResponse>, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	let result = collect_aspects(&store).await;
	drop(store);
	let aspects = result.map_err(|err| StorageError::Internal(err.to_string()))?;
	Ok(Json(AspectListResponse { aspects }))
}

/// Response body for `GET /api/v1/storage/{aspect}/schema` — a single aspect's
/// declared schema (the read counterpart of the `POST …/aspects` declaration).
#[derive(Debug, Clone, Serialize)]
pub struct AspectSchemaResponse {
	/// The declared aspect and its physical encoding / tolerance / timestamp unit.
	pub aspect: AspectInfo,
}

/// Handle `GET /api/v1/storage/{aspect}/schema`: return one aspect's declared
/// schema, the single-aspect read counterpart of `GET …/aspects` (which lists all)
/// and of the `POST …/aspects` declaration.
///
/// # Errors
///
/// [`StorageError::Unconfigured`] when no store is attached,
/// [`StorageError::NotFound`] when the aspect is undeclared, and
/// [`StorageError::Internal`] on a control-plane read failure.
pub async fn storage_aspect_schema(State(state): State<AppState>, Path(aspect): Path<String>) -> Result<Json<AspectSchemaResponse>, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	let result = store.schema_for(&aspect).await;
	drop(store);
	let schema = result.map_err(|err| StorageError::Internal(err.to_string()))?;
	let Some(schema) = schema else {
		return Err(StorageError::NotFound(format!("aspect `{aspect}` has no declared schema in the segment store")));
	};
	let aspect = AspectInfo { name: aspect, physical_type: schema.value.name(), value_tolerance: schema.value_tolerance.to_string(), timestamp_unit: schema.timestamp_unit.name() };
	Ok(Json(AspectSchemaResponse { aspect }))
}

/// Handle `GET /api/v1/storage/{aspect}/stats`.
///
/// Returns the aspect's materialized segment-set rollup, including the north-star
/// bytes/point. An aspect with no sealed segments reports the empty rollup (all
/// zeros), not an error.
///
/// # Errors
///
/// [`StorageError::Unconfigured`] when no store is attached, or
/// [`StorageError::Internal`] on a control-plane read failure.
pub async fn storage_aspect_stats(State(state): State<AppState>, Path(aspect): Path<String>) -> Result<Json<AspectStatsResponse>, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	// The O(1) rollup carries every field except the cross-segment overlap count,
	// which has no incremental fold — a second, index-scanning `aspect_stats` supplies
	// it (this is an operator stats endpoint, not the measurement hot path).
	let meta_result = store.aspect_metadata(&aspect).await;
	let overlap_result = store.aspect_stats(&aspect).await;
	drop(store);
	let meta = meta_result.map_err(|err| StorageError::Internal(err.to_string()))?;
	let overlapping_segments = overlap_result.map_err(|err| StorageError::Internal(err.to_string()))?.overlapping_segments;
	// Read the borrowing accessor and the Copy fields before moving `value_range` out.
	let bytes_per_point = meta.bytes_per_point();
	let time_range = meta.time_range.map(Into::into);
	let value_range = meta.value_range.map(|(lo, hi)| [lo.to_string(), hi.to_string()]);
	Ok(Json(AspectStatsResponse { aspect, segment_count: meta.segment_count, total_rows: meta.total_rows, total_nulls: meta.total_nulls, total_bytes: meta.total_bytes, bytes_per_point, unsorted_segments: meta.unsorted_segments, overlapping_segments, time_range, value_range }))
}

/// Handle `GET /api/v1/storage/stats`: the store-wide aggregate over every aspect's
/// rollup — the subject-wide north-star bytes/point.
///
/// # Errors
///
/// [`StorageError::Unconfigured`] when no store is attached, or
/// [`StorageError::Internal`] on a control-plane read failure.
pub async fn storage_stats(State(state): State<AppState>) -> Result<Json<StoreStatsResponse>, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	// The O(1) rollup carries every field except the cross-segment overlap total,
	// which has no incremental fold — a separate index-scanning aggregate supplies it.
	let result = store.store_stats().await;
	let overlap_result = store.store_overlapping_segments().await;
	drop(store);
	let summary = result.map_err(|err| StorageError::Internal(err.to_string()))?;
	let overlapping_segments = overlap_result.map_err(|err| StorageError::Internal(err.to_string()))?;
	let bytes_per_point = summary.bytes_per_point();
	let time_range = summary.time_range.map(Into::into);
	Ok(Json(StoreStatsResponse { aspect_count: summary.aspect_count, segment_count: summary.segment_count, total_rows: summary.total_rows, total_nulls: summary.total_nulls, total_bytes: summary.total_bytes, bytes_per_point, unsorted_segments: summary.unsorted_segments, overlapping_segments, time_range }))
}

/// Query parameters for the stored-range downsample.
///
/// The `[start, end]` window is inclusive and expressed as integer epochs **in the
/// aspect's declared `TimeUnit`** (as every other stored-range read here), while the
/// resolution and aggregation list use the same tokens the compute downsample
/// endpoint accepts.
#[derive(Debug, Clone, Deserialize)]
pub struct StorageDownsampleParams {
	/// Inclusive window start (epoch integer in the aspect's declared unit).
	/// Defaults to the whole stored history when omitted.
	#[serde(default)]
	pub start: Option<i64>,
	/// Inclusive window end (epoch integer in the aspect's declared unit).
	/// Defaults to the whole stored history when omitted.
	#[serde(default)]
	pub end: Option<i64>,
	/// Resolution token (`seconds`..`years`); defaults to minutes.
	#[serde(default)]
	pub resolution: Option<String>,
	/// Comma-separated reductions (e.g. `min,max,sketch_p99`); defaults to `min,max,avg`.
	#[serde(default)]
	pub agg: Option<String>,
}

/// Reduce an aspect's **stored** segments over `[start, end]` into grid-aligned
/// buckets, shared by this endpoint's four output formats.
///
/// This is the bounded-memory path: [`weftdb::SegmentStore::downsample_range`]
/// prunes the segment index by time and folds each surviving segment into its own
/// mergeable `PartialReduction`, so only one segment's rows are ever resident and a
/// range far larger than RAM still reduces — and with a `sketch_p*` reduction the
/// per-bucket state is bounded too. An omitted bound spans the whole stored history.
///
/// Traced as `downsample.range` so a `RUST_LOG`/OTLP run attributes the stored
/// reduction apart from the compute endpoints' `downsample.reduce`.
async fn storage_downsample_inner(state: AppState, aspect: &str, params: &StorageDownsampleParams) -> Result<crate::downsample::DownsampleResponse, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	let resolution = crate::interpolate::parse_resolution_token(params.resolution.as_deref())?;
	let aggregations = crate::downsample::parse_aggregations(params.agg.as_deref())?;
	// An undeclared aspect is a 404 before any read work, matching the sibling reads.
	require_schema(&store, aspect).await?;
	// An omitted bound means "all of stored history" — the index prune is inclusive,
	// so the saturating bounds simply select every segment.
	let start = params.start.unwrap_or(i64::MIN);
	let end = params.end.unwrap_or(i64::MAX);
	if end < start {
		return Err(StorageError::BadRequest(format!("`end` ({end}) must not be before `start` ({start})")));
	}

	let span = tracing::info_span!("downsample.range", %aspect, start, end, resolution = ?resolution, buckets = tracing::field::Empty);
	let result = store.downsample_range(aspect, start, end, resolution, &aggregations).instrument(span.clone()).await;
	drop(store);
	let buckets = result.map_err(|err| classify_read_error(&err))?;
	let response = crate::downsample::buckets_to_response(buckets, resolution, &aggregations);
	span.record("buckets", response.buckets);
	Ok(response)
}

/// Handle `GET|POST /api/v1/storage/{aspect}/downsample?start&end&resolution&agg`.
///
/// The stored-data counterpart of `POST /api/v1/downsample`: where that endpoint
/// reduces a series the caller supplies in the request body, this one reduces the
/// aspect's **persisted** `.weftseg` segments in place — the surface that puts the
/// bounded-memory cross-segment reduction (including the mergeable `sketch_p*`
/// percentiles) in front of a client without the client first fetching the range.
/// Registered on both verbs: it is a read (hence `GET`, as every sibling stored
/// read) that the roadmap named as a `POST`; it takes no body either way.
///
/// # Errors
///
/// [`StorageError::Unconfigured`] when no store is attached, [`StorageError::NotFound`]
/// for an undeclared aspect, [`StorageError::BadRequest`] for an unknown
/// resolution/aggregation token or an inverted window, and [`StorageError::Internal`]
/// on a read or reduction failure.
pub async fn storage_downsample(State(state): State<AppState>, Path(aspect): Path<String>, Query(params): Query<StorageDownsampleParams>) -> Result<Json<crate::downsample::DownsampleResponse>, StorageError> {
	Ok(Json(storage_downsample_inner(state, &aspect, &params).await?))
}

/// Handle `GET|POST /api/v1/storage/{aspect}/downsample.csv`: the CSV-output
/// sibling of [`storage_downsample`], serving the identical reduction as a
/// `timestamp,count,<agg>…` document.
///
/// # Errors
///
/// As [`storage_downsample`].
pub async fn storage_downsample_csv(State(state): State<AppState>, Path(aspect): Path<String>, Query(params): Query<StorageDownsampleParams>) -> Result<Response, StorageError> {
	let response = storage_downsample_inner(state, &aspect, &params).await?;
	Ok(crate::downsample::downsample_response_to_csv(&response))
}

/// Handle `GET|POST /api/v1/storage/{aspect}/downsample.arrow`: the Arrow-IPC-output
/// sibling of [`storage_downsample`], serving the identical reduction as a
/// self-describing reduction-table batch.
///
/// # Errors
///
/// As [`storage_downsample`], plus [`StorageError::Internal`] on an Arrow-encoding
/// failure.
pub async fn storage_downsample_arrow(State(state): State<AppState>, Path(aspect): Path<String>, Query(params): Query<StorageDownsampleParams>) -> Result<Response, StorageError> {
	let response = storage_downsample_inner(state, &aspect, &params).await?;
	Ok(crate::downsample::downsample_response_to_arrow(&response)?)
}

/// Handle `GET|POST /api/v1/storage/{aspect}/downsample.parquet`: the Parquet-output
/// sibling of [`storage_downsample`].
///
/// # Errors
///
/// As [`storage_downsample`], plus [`StorageError::Internal`] on a Parquet-encoding
/// failure.
pub async fn storage_downsample_parquet(State(state): State<AppState>, Path(aspect): Path<String>, Query(params): Query<StorageDownsampleParams>) -> Result<Response, StorageError> {
	let response = storage_downsample_inner(state, &aspect, &params).await?;
	Ok(crate::downsample::downsample_response_to_parquet(&response)?)
}

#[cfg(test)]
mod tests {
	use std::{str::FromStr, sync::Arc};

	use axum::{
		body::Body, http::{Request, StatusCode}
	};
	use bigdecimal::BigDecimal;
	use tempfile::TempDir;
	use tower::ServiceExt;
	use weft_arrow::{read_ipc_stream, record_batches_to_columns};
	use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
	use weftdb::SegmentStore;

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
	async fn point_lookup_returns_the_value_at_an_exact_instant() {
		let (_dir, router) = router_with_sealed_price().await;
		// A hit at a stored grid point.
		let (status, body) = get_json(router, "/api/v1/storage/price/at?t=120").await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body["aspect"], "price");
		assert_eq!(body["time_unit"], "seconds");
		assert_eq!(body["timestamp"], 120);
		assert_eq!(body["value"], "3.5");
		assert_eq!(body["found"], true);
	}

	#[tokio::test]
	async fn point_lookup_off_grid_instant_is_a_miss() {
		let (_dir, router) = router_with_sealed_price().await;
		// An instant between stored points: found=false, and value is omitted.
		let (status, body) = get_json(router, "/api/v1/storage/price/at?t=125").await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body["found"], false);
		assert_eq!(body["timestamp"], 125);
		assert!(body.get("value").is_none() || body["value"].is_null(), "a miss omits the value: {body}");
	}

	#[tokio::test]
	async fn point_lookup_undeclared_aspect_is_not_found() {
		let (_dir, router) = router_with_sealed_price().await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/never_declared/at?t=100").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::NOT_FOUND);
	}

	#[tokio::test]
	async fn point_lookup_without_store_is_unavailable() {
		let router = app_with_state(AppState::new());
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/price/at?t=100").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
	}

	#[tokio::test]
	async fn multi_point_lookup_returns_values_in_query_order() {
		let (_dir, router) = router_with_sealed_price().await;
		// A scrambled batch: hits, an off-grid miss (125), a repeat (120), and out-of-range (500).
		let (status, body) = get_json(router, "/api/v1/storage/price/at-multi?t=120,100,125,120,500,140").await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body["aspect"], "price");
		assert_eq!(body["time_unit"], "seconds");
		let points = body["points"].as_array().expect("points array");
		assert_eq!(points.len(), 6);
		assert_eq!(points[0]["timestamp"], 120);
		assert_eq!(points[0]["value"], "3.5");
		assert_eq!(points[0]["found"], true);
		assert_eq!(points[1]["value"], "1.5"); // t=100
		assert_eq!(points[2]["found"], false); // t=125 off-grid
		assert!(points[2].get("value").is_none() || points[2]["value"].is_null());
		assert_eq!(points[3]["value"], "3.5"); // t=120 repeat
		assert_eq!(points[4]["found"], false); // t=500 out of range
		assert_eq!(points[5]["value"], "5.5"); // t=140
	}

	#[tokio::test]
	async fn multi_point_lookup_rejects_a_non_integer_instant() {
		let (_dir, router) = router_with_sealed_price().await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/price/at-multi?t=100,abc").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::BAD_REQUEST);
	}

	#[tokio::test]
	async fn time_range_serves_a_parquet_file() {
		use weft_arrow::read_parquet;
		let (_dir, router) = router_with_sealed_price().await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/price/range.parquet?start=110&end=130").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		assert_eq!(response.headers().get("content-type").unwrap(), "application/vnd.apache.parquet");

		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		assert_eq!(&bytes[..4], b"PAR1");
		let batches = read_parquet(&bytes).expect("reads parquet");
		let (ts, vs) = record_batches_to_columns(&batches).expect("reads back columns");
		assert_eq!(ts, vec![110, 120, 130]);
		assert_eq!(vs, vec![Some(bd("2.5")), Some(bd("3.5")), Some(bd("4.5"))]);
	}

	#[tokio::test]
	async fn value_range_serves_a_parquet_file() {
		use weft_arrow::read_parquet;
		let (_dir, router) = router_with_sealed_price().await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/price/value-range.parquet?lo=2.5&hi=4.5").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		assert_eq!(response.headers().get("content-type").unwrap(), "application/vnd.apache.parquet");
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let batches = read_parquet(&bytes).expect("reads parquet");
		let (_ts, vs) = record_batches_to_columns(&batches).expect("reads back columns");
		assert_eq!(vs, vec![Some(bd("2.5")), Some(bd("3.5")), Some(bd("4.5"))]);
	}

	#[tokio::test]
	async fn parquet_export_without_store_is_unavailable() {
		let router = app_with_state(AppState::new());
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/price/range.parquet?start=0&end=100").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
	}

	/// Build a router over a store with `price` declared (F64, seconds) but no rows
	/// sealed — the starting point for an ingest test.
	async fn router_with_declared_empty_price(dir: &TempDir) -> axum::Router {
		let store = SegmentStore::open(dir.path()).await.expect("opens store");
		store.declare("price", &AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)).await.expect("declares aspect");
		app_with_state(AppState::new().with_store(Arc::new(store)))
	}

	/// Build a Parquet file body carrying the given columns under an F64/seconds
	/// schema, the way an external producer would.
	fn price_parquet_body(timestamps: &[i64], values: &[Option<BigDecimal>]) -> Vec<u8> {
		use weft_arrow::{columns_to_record_batch_typed, write_parquet};
		let batch = columns_to_record_batch_typed(TimeUnit::Seconds, PhysicalType::F64, timestamps, values);
		write_parquet(std::slice::from_ref(&batch)).expect("writes parquet")
	}

	async fn post_parquet(router: axum::Router, uri: &str, body: Vec<u8>) -> (StatusCode, serde_json::Value) {
		let response = router.oneshot(Request::builder().method("POST").uri(uri).body(Body::from(body)).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
		(status, json)
	}

	#[tokio::test]
	async fn parquet_ingest_seals_a_batch_the_read_surface_returns() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_empty_price(&dir).await;
		let body = price_parquet_body(&[100, 110, 120], &[Some(bd("1.5")), Some(bd("2.5")), Some(bd("3.5"))]);
		let (status, json) = post_parquet(router, "/api/v1/storage/price/parquet", body).await;
		assert_eq!(status, StatusCode::CREATED, "body: {json}");
		assert_eq!(json["aspect"], "price");
		assert_eq!(json["row_count"], 3);
		assert_eq!(json["null_count"], 0);
		assert_eq!(json["min_ts"], 100);
		assert_eq!(json["max_ts"], 120);

		// Reopen and read back through the JSON points surface.
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("reopens"));
		let router = app_with_state(AppState::new().with_store(store));
		let (status, read) = get_json(router, "/api/v1/storage/price/points?start=100&end=120").await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(read["count"], 3);
		assert_eq!(read["points"][1]["value"], "2.5");
	}

	#[tokio::test]
	async fn parquet_ingest_into_undeclared_aspect_is_not_found() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_empty_price(&dir).await;
		let body = price_parquet_body(&[1], &[Some(bd("1"))]);
		let (status, _json) = post_parquet(router, "/api/v1/storage/never_declared/parquet", body).await;
		assert_eq!(status, StatusCode::NOT_FOUND);
	}

	#[tokio::test]
	async fn parquet_ingest_rejects_a_garbage_body() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_empty_price(&dir).await;
		let (status, _json) = post_parquet(router, "/api/v1/storage/price/parquet", b"not a parquet file".to_vec()).await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
	}

	#[tokio::test]
	async fn parquet_ingest_empty_body_is_bad_request() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_empty_price(&dir).await;
		let (status, _json) = post_parquet(router, "/api/v1/storage/price/parquet", Vec::new()).await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
	}

	#[tokio::test]
	async fn parquet_ingest_without_store_is_unavailable() {
		let router = app_with_state(AppState::new());
		let (status, _json) = post_parquet(router, "/api/v1/storage/price/parquet", price_parquet_body(&[1], &[Some(bd("1"))])).await;
		assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
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

	/// Fetch and parse a JSON GET response from a router.
	async fn get_json(router: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
		let response = router.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		(status, serde_json::from_slice(&bytes).unwrap())
	}

	#[tokio::test]
	async fn aspects_lists_the_declared_aspect_with_its_schema() {
		let (_dir, router) = router_with_sealed_price().await;
		let (status, body) = get_json(router, "/api/v1/storage/aspects").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		let aspects = body["aspects"].as_array().unwrap();
		assert_eq!(aspects.len(), 1);
		assert_eq!(aspects[0]["name"], "price");
		assert_eq!(aspects[0]["physical_type"], "f64");
		assert_eq!(aspects[0]["timestamp_unit"], "seconds");
	}

	#[tokio::test]
	async fn aspect_stats_reports_rows_span_and_bytes_per_point() {
		let (_dir, router) = router_with_sealed_price().await;
		let (status, body) = get_json(router, "/api/v1/storage/price/stats").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["aspect"], "price");
		assert!(body["segment_count"].as_u64().unwrap() >= 1);
		assert_eq!(body["total_rows"], 5);
		// Five points were sealed across [100, 140].
		assert_eq!(body["time_range"][0], 100);
		assert_eq!(body["time_range"][1], 140);
		// The north-star cost term is a positive, finite bytes/point.
		let bpp = body["bytes_per_point"].as_f64().unwrap();
		assert!(bpp > 0.0 && bpp.is_finite(), "bytes_per_point = {bpp}");
		// The sole sealed segment is in order, so the order-health signal is clean.
		assert_eq!(body["unsorted_segments"], 0);
	}

	#[tokio::test]
	async fn parquet_ingest_require_sorted_rejects_out_of_order() {
		use weft_arrow::{columns_to_record_batch_typed, write_parquet};

		let dir = TempDir::new().expect("temp dir");
		let store = SegmentStore::open(dir.path()).await.expect("opens store");
		store.declare("price", &AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)).await.expect("declares");
		let router = app_with_state(AppState::new().with_store(Arc::new(store)));
		// An out-of-order Parquet batch (row 1: 90 < 100).
		let batch = columns_to_record_batch_typed(TimeUnit::Seconds, PhysicalType::F64, &[100_i64, 90, 120], &[Some(bd("1")), Some(bd("2")), Some(bd("3"))]);
		let bytes = write_parquet(std::slice::from_ref(&batch)).expect("writes parquet");

		let post = |uri: &'static str| {
			let router = router.clone();
			let body = bytes.clone();
			async move { router.oneshot(Request::builder().method("POST").uri(uri).header("content-type", "application/vnd.apache.parquet").body(Body::from(body)).unwrap()).await.unwrap() }
		};
		// require_sorted -> 400 with the offending row.
		let rejected = post("/api/v1/storage/price/parquet?require_sorted=true").await;
		assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
		let msg = axum::body::to_bytes(rejected.into_body(), usize::MAX).await.unwrap();
		let msg: serde_json::Value = serde_json::from_slice(&msg).unwrap();
		assert!(msg["error"].as_str().unwrap().contains("out-of-order timestamp"), "body: {msg}");
		// Default (no flag) -> 201.
		let accepted = post("/api/v1/storage/price/parquet").await;
		assert_eq!(accepted.status(), StatusCode::CREATED);
	}

	#[tokio::test]
	async fn aspect_stats_reports_unsorted_segments_after_an_out_of_order_seal() {
		let dir = TempDir::new().expect("temp dir");
		let store = SegmentStore::open(dir.path()).await.expect("opens store");
		store.declare("price", &AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)).await.expect("declares");
		// One ordered segment, then one whose timestamps step backwards.
		store.seal_declared("price", &[0_i64, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.expect("ordered");
		store.seal_declared("price", &[100_i64, 130, 110], &[bd("4"), bd("5"), bd("6")]).await.expect("out of order");
		let router = app_with_state(AppState::new().with_store(Arc::new(store)));
		let (status, body) = get_json(router, "/api/v1/storage/price/stats").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["segment_count"], 2);
		// The rollup fold path (record_seal) surfaced the out-of-order segment at the endpoint.
		assert_eq!(body["unsorted_segments"], 1, "body: {body}");
		// The two segments cover disjoint windows, so there is no cross-segment overlap.
		assert_eq!(body["overlapping_segments"], 0, "body: {body}");
	}

	#[tokio::test]
	async fn aspect_stats_reports_cross_segment_overlap() {
		let dir = TempDir::new().expect("temp dir");
		let store = SegmentStore::open(dir.path()).await.expect("opens store");
		store.declare("price", &AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)).await.expect("declares");
		// Two internally-sorted segments whose time windows overlap ([0,20] and [10,30]).
		store.seal_declared("price", &[0_i64, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.expect("first");
		store.seal_declared("price", &[10_i64, 20, 30], &[bd("4"), bd("5"), bd("6")]).await.expect("overlapping");
		let router = app_with_state(AppState::new().with_store(Arc::new(store)));
		let (status, body) = get_json(router, "/api/v1/storage/price/stats").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		// Both segments are internally sorted, but their windows overlap.
		assert_eq!(body["unsorted_segments"], 0, "body: {body}");
		assert_eq!(body["overlapping_segments"], 2, "body: {body}");
	}

	#[tokio::test]
	async fn store_stats_aggregates_over_aspects() {
		let (_dir, router) = router_with_sealed_price().await;
		let (status, body) = get_json(router, "/api/v1/storage/stats").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["aspect_count"], 1);
		assert_eq!(body["total_rows"], 5);
		assert!(body["bytes_per_point"].as_f64().unwrap() > 0.0);
		// The store's sole segment is in order — store-wide order health is clean.
		assert_eq!(body["unsorted_segments"], 0);
		// A single segment overlaps nothing.
		assert_eq!(body["overlapping_segments"], 0);
	}

	#[tokio::test]
	async fn stats_without_store_is_unavailable() {
		let router = app_with_state(AppState::new());
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/stats").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
	}

	#[tokio::test]
	async fn points_endpoint_returns_lossless_json_rows() {
		let (_dir, router) = router_with_sealed_price().await;
		let (status, body) = get_json(router, "/api/v1/storage/price/points?start=110&end=130").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["aspect"], "price");
		assert_eq!(body["time_unit"], "seconds");
		assert_eq!(body["count"], 3);
		let points = body["points"].as_array().unwrap();
		assert_eq!(points.len(), 3);
		assert_eq!(points[0]["timestamp"], 110);
		// Values are lossless decimal text, not floats.
		assert_eq!(points[0]["value"], "2.5");
		assert_eq!(points[1]["value"], "3.5");
		assert_eq!(points[2]["value"], "4.5");
	}

	#[tokio::test]
	async fn points_endpoint_undeclared_aspect_is_not_found() {
		let (_dir, router) = router_with_sealed_price().await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/never_declared/points?start=0&end=100").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::NOT_FOUND);
	}

	#[tokio::test]
	async fn points_endpoint_empty_window_returns_zero_rows() {
		let (_dir, router) = router_with_sealed_price().await;
		let (status, body) = get_json(router, "/api/v1/storage/price/points?start=1000&end=2000").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["count"], 0);
		assert!(body["points"].as_array().unwrap().is_empty());
	}

	#[tokio::test]
	async fn catalog_reports_scope_and_registered_hierarchy() {
		let (_dir, router) = router_with_sealed_price().await;
		let (status, body) = get_json(router, "/api/v1/storage/catalog").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		// A store opened with `SegmentStore::open` is scoped to default/default.
		assert_eq!(body["database"], "default");
		assert_eq!(body["subject"], "default");
		let databases = body["databases"].as_array().unwrap();
		assert!(databases.iter().any(|d| d["name"] == "default" && d["subjects"].as_array().unwrap().iter().any(|s| s == "default")));
	}

	#[tokio::test]
	async fn catalog_without_store_is_unavailable() {
		let router = app_with_state(AppState::new());
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/catalog").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
	}

	#[tokio::test]
	async fn aspect_schema_returns_one_declared_aspect() {
		let (_dir, router) = router_with_sealed_price().await;
		let (status, body) = get_json(router, "/api/v1/storage/price/schema").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["aspect"]["name"], "price");
		assert_eq!(body["aspect"]["physical_type"], "f64");
		assert_eq!(body["aspect"]["timestamp_unit"], "seconds");
		assert_eq!(body["aspect"]["value_tolerance"], "0");
	}

	#[tokio::test]
	async fn aspect_schema_undeclared_is_not_found() {
		let (_dir, router) = router_with_sealed_price().await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/never_declared/schema").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::NOT_FOUND);
	}

	#[tokio::test]
	async fn points_pagination_offset_and_limit_page_the_window() {
		// The sealed `price` aspect holds five rows at ts 100..=140.
		let (_dir, router) = router_with_sealed_price().await;
		// Page: skip 1, take 2 over the full window.
		let (status, body) = get_json(router, "/api/v1/storage/price/points?start=100&end=140&offset=1&limit=2").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		// `total` is the whole window (5); this page returns 2 starting at offset 1.
		assert_eq!(body["total"], 5);
		assert_eq!(body["count"], 2);
		assert_eq!(body["offset"], 1);
		let points = body["points"].as_array().unwrap();
		assert_eq!(points.len(), 2);
		// Offset 1 skips ts 100; the page is ts 110, 120.
		assert_eq!(points[0]["timestamp"], 110);
		assert_eq!(points[1]["timestamp"], 120);
	}

	#[tokio::test]
	async fn points_cursor_iterates_the_window_forward() {
		// The sealed `price` aspect holds five rows at ts 100..=140.
		let (_dir, router) = router_with_sealed_price().await;
		let mut seen: Vec<i64> = Vec::new();
		let mut uri = "/api/v1/storage/price/points?start=100&end=140&limit=2".to_string();
		let mut pages = 0;
		loop {
			pages += 1;
			assert!(pages <= 6, "cursor iteration did not terminate");
			let (status, body) = get_json(router.clone(), &uri).await;
			assert_eq!(status, StatusCode::OK, "body: {body}");
			for p in body["points"].as_array().unwrap() {
				seen.push(p["timestamp"].as_i64().unwrap());
			}
			match body.get("next_cursor").and_then(serde_json::Value::as_str) {
				Some(cursor) => uri = format!("/api/v1/storage/price/points?start=100&end=140&limit=2&cursor={cursor}"),
				None => break,
			}
		}
		// Three pages of 2/2/1 walk every row exactly once, in read order; no next_cursor
		// on the final page.
		assert_eq!(seen, vec![100, 110, 120, 130, 140]);
		assert_eq!(pages, 3);
	}

	#[tokio::test]
	async fn points_cursor_supersedes_offset_and_clears_page_echo() {
		let (_dir, router) = router_with_sealed_price().await;
		// cursor=0000000000000003 (offset 3) with an explicit offset=1 that it overrides.
		let (status, body) = get_json(router, "/api/v1/storage/price/points?start=100&end=140&limit=2&offset=1&cursor=0000000000000003").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["offset"], 3, "the cursor position supersedes the explicit offset");
		assert!(body.get("page").is_none(), "a cursor read clears the page echo");
		let points = body["points"].as_array().unwrap();
		assert_eq!(points.len(), 2, "rows 130 and 140 remain from offset 3");
		assert_eq!(points[0]["timestamp"], 130);
		assert_eq!(points[1]["timestamp"], 140);
		assert!(body.get("next_cursor").is_none(), "offset 3 + 2 rows = 5 = total → end of window");
	}

	#[tokio::test]
	async fn points_malformed_cursor_is_a_bad_request() {
		let (_dir, router) = router_with_sealed_price().await;
		let (status, _body) = get_json(router, "/api/v1/storage/price/points?start=100&end=140&cursor=zzzz").await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
	}

	#[tokio::test]
	async fn points_pagination_offset_past_end_returns_empty_page() {
		let (_dir, router) = router_with_sealed_price().await;
		let (status, body) = get_json(router, "/api/v1/storage/price/points?start=100&end=140&offset=100").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["total"], 5);
		assert_eq!(body["count"], 0);
		assert!(body["points"].as_array().unwrap().is_empty());
	}

	#[tokio::test]
	async fn points_without_pagination_returns_whole_window() {
		let (_dir, router) = router_with_sealed_price().await;
		let (status, body) = get_json(router, "/api/v1/storage/price/points?start=100&end=140").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		// No offset/limit -> total == count, offset 0.
		assert_eq!(body["total"], 5);
		assert_eq!(body["count"], 5);
		assert_eq!(body["offset"], 0);
		// Unbounded read omits the `limit`/`page` echoes.
		assert!(body.get("limit").is_none());
		assert!(body.get("page").is_none());
	}

	#[tokio::test]
	async fn points_take_alias_is_an_alias_for_limit() {
		let (_dir, router) = router_with_sealed_price().await;
		// `take=2` with no `limit` behaves like `limit=2`.
		let (status, body) = get_json(router, "/api/v1/storage/price/points?start=100&end=140&take=2").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["total"], 5);
		assert_eq!(body["count"], 2);
		assert_eq!(body["offset"], 0);
		assert_eq!(body["limit"], 2);
		// `take` alone is not page-based, so no `page` echo.
		assert!(body.get("page").is_none());
		let points = body["points"].as_array().unwrap();
		assert_eq!(points[0]["timestamp"], 100);
		assert_eq!(points[1]["timestamp"], 110);
	}

	#[tokio::test]
	async fn points_limit_wins_over_take_alias() {
		let (_dir, router) = router_with_sealed_price().await;
		// When both are given, `limit` takes precedence over `take`.
		let (status, body) = get_json(router, "/api/v1/storage/price/points?start=100&end=140&limit=1&take=4").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["count"], 1);
		assert_eq!(body["limit"], 1);
	}

	#[tokio::test]
	async fn points_page_derives_offset_from_page_size() {
		let (_dir, router) = router_with_sealed_price().await;
		// page 2 with size 2 -> offset (2-1)*2 = 2: rows ts 120, 130.
		let (status, body) = get_json(router, "/api/v1/storage/price/points?start=100&end=140&page=2&take=2").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["total"], 5);
		assert_eq!(body["count"], 2);
		assert_eq!(body["offset"], 2);
		assert_eq!(body["limit"], 2);
		assert_eq!(body["page"], 2);
		let points = body["points"].as_array().unwrap();
		assert_eq!(points[0]["timestamp"], 120);
		assert_eq!(points[1]["timestamp"], 130);
	}

	#[tokio::test]
	async fn points_page_supersedes_explicit_offset() {
		let (_dir, router) = router_with_sealed_price().await;
		// `page=1` forces offset 0 even though `offset=3` is also supplied.
		let (status, body) = get_json(router, "/api/v1/storage/price/points?start=100&end=140&page=1&limit=2&offset=3").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["offset"], 0);
		assert_eq!(body["page"], 1);
		let points = body["points"].as_array().unwrap();
		assert_eq!(points[0]["timestamp"], 100);
	}

	#[tokio::test]
	async fn points_page_without_page_size_falls_back_to_page_one() {
		let (_dir, router) = router_with_sealed_price().await;
		// `page` with no `limit`/`take` -> offset 0, whole window returned.
		let (status, body) = get_json(router, "/api/v1/storage/price/points?start=100&end=140&page=5").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["offset"], 0);
		assert_eq!(body["count"], 5);
		assert_eq!(body["page"], 5);
		assert!(body.get("limit").is_none());
	}

	#[tokio::test]
	async fn value_points_returns_only_in_band_rows_as_json() {
		let (_dir, router) = router_with_sealed_price().await;
		// price values are 1.5..5.5 at ts 100..140; band [2.5, 4.5] keeps three rows.
		let (status, body) = get_json(router, "/api/v1/storage/price/value-points?lo=2.5&hi=4.5").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["aspect"], "price");
		assert_eq!(body["time_unit"], "seconds");
		assert_eq!(body["total"], 3);
		assert_eq!(body["count"], 3);
		let points = body["points"].as_array().unwrap();
		assert_eq!(points[0]["timestamp"], 110);
		assert_eq!(points[0]["value"], "2.5");
		assert_eq!(points[2]["value"], "4.5");
	}

	#[tokio::test]
	async fn value_points_paginates_with_take_and_page() {
		let (_dir, router) = router_with_sealed_price().await;
		// Whole band [1.5, 5.5] = 5 rows; page 2 of size 2 -> ts 120, 130.
		let (status, body) = get_json(router, "/api/v1/storage/price/value-points?lo=1.5&hi=5.5&take=2&page=2").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["total"], 5);
		assert_eq!(body["count"], 2);
		assert_eq!(body["offset"], 2);
		assert_eq!(body["limit"], 2);
		assert_eq!(body["page"], 2);
		let points = body["points"].as_array().unwrap();
		assert_eq!(points[0]["timestamp"], 120);
		assert_eq!(points[1]["timestamp"], 130);
	}

	#[tokio::test]
	async fn value_points_unparseable_bound_is_bad_request() {
		let (_dir, router) = router_with_sealed_price().await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/price/value-points?lo=abc&hi=5").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::BAD_REQUEST);
	}

	#[tokio::test]
	async fn value_points_undeclared_aspect_is_not_found() {
		let (_dir, router) = router_with_sealed_price().await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/never_declared/value-points?lo=0&hi=100").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::NOT_FOUND);
	}

	/// Fetch a GET response and return `(status, content-type, body text)`.
	async fn get_text(router: axum::Router, uri: &str) -> (StatusCode, String, String) {
		let response = router.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap()).await.unwrap();
		let status = response.status();
		let content_type = response.headers().get("content-type").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		(status, content_type, String::from_utf8(bytes.to_vec()).unwrap())
	}

	#[tokio::test]
	async fn time_range_serves_a_csv_document() {
		let (_dir, router) = router_with_sealed_price().await;
		let (status, content_type, body) = get_text(router, "/api/v1/storage/price/range.csv?start=110&end=130").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(content_type, "text/csv; charset=utf-8");
		// Header row plus the three in-window rows, lossless decimal text.
		assert_eq!(body, "timestamp,value\n110,2.5\n120,3.5\n130,4.5\n");
	}

	#[tokio::test]
	async fn value_range_serves_a_csv_document() {
		let (_dir, router) = router_with_sealed_price().await;
		let (status, _content_type, body) = get_text(router, "/api/v1/storage/price/value-range.csv?lo=2.5&hi=4.5").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body, "timestamp,value\n110,2.5\n120,3.5\n130,4.5\n");
	}

	#[tokio::test]
	async fn time_range_csv_empty_window_is_header_only() {
		let (_dir, router) = router_with_sealed_price().await;
		let (status, _content_type, body) = get_text(router, "/api/v1/storage/price/range.csv?start=1000&end=2000").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		// No rows in the window -> just the header.
		assert_eq!(body, "timestamp,value\n");
	}

	/// Open a store under `dir`, declare `price` (F64, seconds), seal one *nullable*
	/// batch (middle row null), and hand back the ready store. Mirrors
	/// [`sealed_price_store`]'s tail-`Arc::new` shape so the significant-`Drop`
	/// `SegmentStore` never trips the drop-tightening lint.
	async fn sealed_nullable_price_store(dir: &TempDir) -> Arc<SegmentStore> {
		let schema = AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds);
		let store = SegmentStore::open(dir.path()).await.expect("opens store");
		store.declare("price", &schema).await.expect("declares aspect");
		store.seal_nullable("price", &schema, &[100_i64, 110, 120], &[Some(bd("1.5")), None, Some(bd("3.5"))]).await.expect("seals nullable");
		Arc::new(store)
	}

	#[tokio::test]
	async fn time_range_csv_renders_null_rows_as_empty_fields() {
		// A nullable seal lets us confirm a null value is an empty CSV field. Inline the
		// store into `with_store` (no intermediate binding) so the significant-`Drop`
		// handle never trips the drop-tightening lint.
		let dir = TempDir::new().unwrap();
		let router = app_with_state(AppState::new().with_store(sealed_nullable_price_store(&dir).await));
		let (status, _content_type, body) = get_text(router, "/api/v1/storage/price/range.csv?start=100&end=120").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body, "timestamp,value\n100,1.5\n110,\n120,3.5\n");
	}

	#[tokio::test]
	async fn csv_export_undeclared_aspect_is_not_found() {
		let (_dir, router) = router_with_sealed_price().await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/never_declared/range.csv?start=0&end=100").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::NOT_FOUND);
	}

	#[tokio::test]
	async fn csv_export_without_store_is_unavailable() {
		let router = app_with_state(AppState::new());
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/price/range.csv?start=0&end=100").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
	}

	#[tokio::test]
	async fn value_range_csv_unparseable_bound_is_bad_request() {
		let (_dir, router) = router_with_sealed_price().await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/price/value-range.csv?lo=abc&hi=10").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::BAD_REQUEST);
	}

	/// Open a store and seal `load` (F64, seconds) as **three separate segments**, one
	/// per minute bucket, each carrying values 1..=3 of that minute. Three seals mean
	/// three `.weftseg` files, so a reduction over the whole span can only be right if it
	/// actually folds across segments — which is what this endpoint exists to do.
	async fn sealed_three_segment_load_store(dir: &TempDir) -> Arc<SegmentStore> {
		let store = SegmentStore::open(dir.path()).await.expect("opens store");
		store.declare("load", &AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)).await.expect("declares aspect");
		for minute in 0..3_i64 {
			// Minute `m` holds samples at m*60+{0,20,40} with values m*10+{1,2,3}.
			let ts: Vec<i64> = (0..3).map(|i| minute * 60 + i * 20).collect();
			let vs: Vec<BigDecimal> = (0..3).map(|i| bd(&format!("{}", minute * 10 + i + 1))).collect();
			store.seal_declared("load", &ts, &vs).await.expect("seals");
		}
		Arc::new(store)
	}

	async fn router_with_three_segment_load() -> (TempDir, axum::Router) {
		let dir = TempDir::new().expect("temp dir");
		let store = sealed_three_segment_load_store(&dir).await;
		(dir, app_with_state(AppState::new().with_store(store)))
	}

	#[tokio::test]
	async fn stored_downsample_reduces_across_segments() {
		let (_dir, router) = router_with_three_segment_load().await;
		// Each minute is its own segment AND its own bucket, so a correct answer needs
		// every segment opened and folded — a single-segment read would lose two buckets.
		let (status, body) = get_json(router, "/api/v1/storage/load/downsample?resolution=minutes&agg=min,max,sum").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["resolution"], "minutes");
		assert_eq!(body["buckets"], 3, "one bucket per sealed segment: {body}");
		assert_eq!(body["input_points"], 9);
		let series = body["series"].as_array().unwrap();
		// Minute 0: values 1,2,3 · minute 1: 11,12,13 · minute 2: 21,22,23.
		assert_eq!(series[0]["timestamp"], "1970-01-01T00:00:00Z");
		assert_eq!(series[0]["aggregations"]["min"], 1.0);
		assert_eq!(series[0]["aggregations"]["max"], 3.0);
		assert_eq!(series[0]["aggregations"]["sum"], 6.0);
		assert_eq!(series[1]["aggregations"]["min"], 11.0);
		assert_eq!(series[1]["aggregations"]["sum"], 36.0);
		assert_eq!(series[2]["aggregations"]["max"], 23.0);
		assert_eq!(series[2]["aggregations"]["sum"], 66.0);
	}

	#[tokio::test]
	async fn stored_downsample_window_prunes_segments() {
		let (_dir, router) = router_with_three_segment_load().await;
		// The window covers only minute 1's segment; the other two must be pruned out.
		let (status, body) = get_json(router, "/api/v1/storage/load/downsample?start=60&end=119&resolution=minutes&agg=sum").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["buckets"], 1);
		assert_eq!(body["input_points"], 3);
		assert_eq!(body["series"][0]["aggregations"]["sum"], 36.0);
	}

	#[tokio::test]
	async fn stored_downsample_omitted_bounds_span_all_history() {
		let (_dir, router) = router_with_three_segment_load().await;
		// No start/end at all — the whole stored history reduces into one hour bucket.
		let (status, body) = get_json(router, "/api/v1/storage/load/downsample?resolution=hours&agg=sum,first,last").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["buckets"], 1);
		assert_eq!(body["input_points"], 9);
		assert_eq!(body["series"][0]["aggregations"]["sum"], 108.0);
		assert_eq!(body["series"][0]["aggregations"]["first"], 1.0);
		assert_eq!(body["series"][0]["aggregations"]["last"], 23.0);
	}

	#[tokio::test]
	async fn stored_downsample_defaults_to_min_max_avg_minutes() {
		let (_dir, router) = router_with_three_segment_load().await;
		let (status, body) = get_json(router, "/api/v1/storage/load/downsample").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["resolution"], "minutes");
		assert_eq!(body["aggregations"], serde_json::json!(["min", "max", "avg"]));
		assert_eq!(body["series"][0]["aggregations"]["avg"], 2.0);
	}

	#[tokio::test]
	async fn stored_downsample_sketch_percentile_is_reachable() {
		let (_dir, router) = router_with_three_segment_load().await;
		// The bounded-memory mergeable reduction is the reason this surface exists: it
		// must survive the per-segment partial-reduce + merge and stay inside its bound.
		let (status, body) = get_json(router, "/api/v1/storage/load/downsample?resolution=hours&agg=sketch_p99,p99").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		let aggs = &body["series"][0]["aggregations"];
		let exact = aggs["p99"].as_f64().unwrap();
		let sketch = aggs["sketch_p99"].as_f64().unwrap();
		assert!((exact - 23.0).abs() < 1e-9, "exact p99 of 1..=23 is 23, got {exact}");
		// SKETCH_ALPHA is 1%, so the sketch must name the same sample within its bound.
		assert!((sketch - exact).abs() / exact < 0.01, "sketch_p99 {sketch} must be within 1% of exact {exact}");
	}

	#[tokio::test]
	async fn stored_downsample_post_matches_get() {
		// The roadmap named this endpoint a POST; the stored-read family is GET. Both
		// are registered and must answer identically (it takes no body either way).
		let (_dir, router) = router_with_three_segment_load().await;
		let (get_status, get_body) = get_json(router, "/api/v1/storage/load/downsample?resolution=minutes&agg=sum").await;
		let (_dir2, router2) = router_with_three_segment_load().await;
		let response = router2.oneshot(Request::builder().method("POST").uri("/api/v1/storage/load/downsample?resolution=minutes&agg=sum").body(Body::empty()).unwrap()).await.unwrap();
		let post_status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let post_body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert_eq!(get_status, StatusCode::OK);
		assert_eq!(post_status, StatusCode::OK);
		assert_eq!(get_body, post_body, "GET and POST must serve the identical reduction");
	}

	#[tokio::test]
	async fn stored_downsample_csv_serves_bucketed_rows() {
		let (_dir, router) = router_with_three_segment_load().await;
		let (status, content_type, text) = get_text(router, "/api/v1/storage/load/downsample.csv?resolution=minutes&agg=min,max").await;
		assert_eq!(status, StatusCode::OK, "body: {text}");
		assert_eq!(content_type, "text/csv; charset=utf-8");
		let lines: Vec<&str> = text.lines().collect();
		assert_eq!(lines[0], "timestamp,count,min,max");
		assert_eq!(lines[1], "1970-01-01T00:00:00+00:00,3,1,3");
		assert_eq!(lines[3], "1970-01-01T00:02:00+00:00,3,21,23");
		assert_eq!(lines.len(), 4);
	}

	#[tokio::test]
	async fn stored_downsample_arrow_serves_a_reduction_table() {
		let (_dir, router) = router_with_three_segment_load().await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/load/downsample.arrow?resolution=minutes&agg=min,max").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		assert_eq!(response.headers().get("content-type").unwrap(), "application/vnd.apache.arrow.stream");
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let batches = read_ipc_stream(&bytes).expect("valid arrow ipc");
		let (unit, _ts, counts, aggs) = weft_arrow::reduction_table_from_record_batch(&batches[0]).expect("reads back");
		assert_eq!(unit, TimeUnit::Nanos);
		assert_eq!(counts, vec![3, 3, 3]);
		assert_eq!(aggs[0], ("min".to_string(), vec![1.0, 11.0, 21.0]));
		assert_eq!(aggs[1], ("max".to_string(), vec![3.0, 13.0, 23.0]));
	}

	#[tokio::test]
	async fn stored_downsample_parquet_serves_a_file() {
		let (_dir, router) = router_with_three_segment_load().await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/load/downsample.parquet?resolution=minutes&agg=sum").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		assert_eq!(response.headers().get("content-type").unwrap(), "application/vnd.apache.parquet");
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		assert_eq!(&bytes[..4], b"PAR1");
		let batches = weft_arrow::read_parquet(&bytes).expect("valid parquet");
		let (_unit, _ts, counts, aggs) = weft_arrow::reduction_table_from_record_batch(&batches[0]).expect("reads back");
		assert_eq!(counts, vec![3, 3, 3]);
		assert_eq!(aggs[0], ("sum".to_string(), vec![6.0, 36.0, 66.0]));
	}

	#[tokio::test]
	async fn stored_downsample_undeclared_aspect_is_not_found() {
		let (_dir, router) = router_with_three_segment_load().await;
		let (status, _body) = get_json(router, "/api/v1/storage/never_declared/downsample").await;
		assert_eq!(status, StatusCode::NOT_FOUND);
	}

	#[tokio::test]
	async fn stored_downsample_without_store_is_unavailable() {
		let router = app_with_state(AppState::new());
		let (status, _body) = get_json(router, "/api/v1/storage/load/downsample").await;
		assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
	}

	#[tokio::test]
	async fn stored_downsample_rejects_bad_tokens_and_inverted_window() {
		let (_dir, router) = router_with_three_segment_load().await;
		let (status, body) = get_json(router, "/api/v1/storage/load/downsample?resolution=fortnights").await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");

		let (_dir, router) = router_with_three_segment_load().await;
		let (status, body) = get_json(router, "/api/v1/storage/load/downsample?agg=bogus").await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
		assert!(body["error"].as_str().unwrap().contains("unknown aggregation"));

		let (_dir, router) = router_with_three_segment_load().await;
		let (status, body) = get_json(router, "/api/v1/storage/load/downsample?start=200&end=100").await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
		assert!(body["error"].as_str().unwrap().contains("end"));
	}

	#[tokio::test]
	async fn stored_downsample_empty_window_is_an_empty_series() {
		let (_dir, router) = router_with_three_segment_load().await;
		// A window past every sealed segment reduces to nothing — not an error.
		let (status, body) = get_json(router, "/api/v1/storage/load/downsample?start=100000&end=200000").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["buckets"], 0);
		assert_eq!(body["input_points"], 0);
		assert_eq!(body["series"], serde_json::json!([]));
	}
}
