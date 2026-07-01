//! The interpolation endpoint (`POST /api/v1/interpolate`).
//!
//! This is the first benchmark-grade *capability* endpoint on the Phase-2 API:
//! it drives DSP's flagship interpolation-on-read path (`splimes::auto_interpolate`,
//! which selects CPU / SIMD / parallel / GPU strategies internally) through plain
//! HTTP + JSON, exactly the surface DSP-Bench and external clients need so they no
//! longer have to embed the Rust API to exercise the engine.
//!
//! ## Numeric boundary (deliberate, documented)
//!
//! The wire DTOs carry values as JSON `f64`. DSP's logical/API numeric type is
//! `BigDecimal` and stays that way internally (the request value is widened to
//! `BigDecimal` before the engine sees it); the `f64` on the wire is a transport
//! convenience for this slice, not a precision decision. Schema-declared physical
//! encodings (Phase 4) replace this with explicit, lossless-by-default types — at
//! which point this endpoint gains a precision-preserving value representation.

use axum::{
	extract::{Query, State}, http::{header, StatusCode}, response::{IntoResponse, Response}, Json
};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use chrono::{DateTime, Utc};
use dsp_line_protocol::TimestampPrecision;
use serde::{Deserialize, Serialize};
use splimes::{Point, Resolution, Spline};

use crate::metrics::SharedMetrics;

/// One input sample on the wire.
#[derive(Debug, Clone, Deserialize)]
pub struct InputPoint {
	/// Sample timestamp (RFC 3339).
	pub timestamp: DateTime<Utc>,
	/// Sample value. Widened to `BigDecimal` before the engine sees it.
	pub value: f64,
}

/// One interpolated sample in the response.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutputPoint {
	/// Output-grid timestamp.
	pub timestamp: DateTime<Utc>,
	/// Interpolated value (narrowed from the engine's `BigDecimal`).
	pub value: f64,
	/// Provenance of this value: `raw` (the grid point coincides with an input
	/// observation), `interpolated` (within the observed span), or `extrapolated`
	/// (outside it).
	pub kind: PointKind,
}

/// Provenance of a reconstructed value.
///
/// Lets a caller never silently treat a synthetic point as an observed one (the
/// Phase-2 raw/interpolated/extrapolated distinction — backlog item B-tags,
/// synthetic-point marking).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PointKind {
	/// The output timestamp coincides with an input observation.
	Raw,
	/// The timestamp lies within `[min_ts, max_ts]` of the inputs (and is not raw).
	Interpolated,
	/// The timestamp lies outside the observed span.
	Extrapolated,
}

/// Classify an output timestamp against the input observations: `Raw` if it
/// coincides with an input, else `Extrapolated` when outside the observed span,
/// else `Interpolated`.
fn classify(timestamp: DateTime<Utc>, input_timestamps: &std::collections::HashSet<DateTime<Utc>>, min_ts: Option<DateTime<Utc>>, max_ts: Option<DateTime<Utc>>) -> PointKind {
	if input_timestamps.contains(&timestamp) {
		return PointKind::Raw;
	}
	match (min_ts, max_ts) {
		(Some(lo), Some(hi)) if timestamp < lo || timestamp > hi => PointKind::Extrapolated,
		_ => PointKind::Interpolated,
	}
}

/// Spline method selector. Mirrors [`splimes::Spline`] but with stable,
/// vendor-neutral JSON tokens (unit variants serialize as a string, e.g.
/// `"cubic"`; `polynomial` carries its parameters).
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SplineSpec {
	/// Piecewise-linear.
	Linear,
	/// Quadratic spline.
	Quadratic,
	/// Cubic spline (default; DSP's strongest smooth reconstruction).
	#[default]
	Cubic,
	/// Polynomial of the given degree, with an optional extrapolation
	/// bounds factor.
	Polynomial {
		/// Polynomial degree.
		degree: usize,
		/// Optional extrapolation bounds factor.
		#[serde(default)]
		bounds_factor: Option<f64>,
	},
}

impl From<SplineSpec> for Spline {
	fn from(spec: SplineSpec) -> Self {
		match spec {
			SplineSpec::Linear => Self::Linear,
			SplineSpec::Quadratic => Self::Quadratic,
			SplineSpec::Cubic => Self::Cubic,
			SplineSpec::Polynomial { degree, bounds_factor } => Self::Polynomial(degree, bounds_factor),
		}
	}
}

/// Output-grid resolution selector. Mirrors [`splimes::Resolution`] with stable
/// lowercase JSON tokens.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResolutionSpec {
	/// 1 ns grid.
	Nanoseconds,
	/// 1 µs grid.
	Microseconds,
	/// 1 ms grid.
	Milliseconds,
	/// 1 s grid.
	Seconds,
	/// 1 min grid (default).
	#[default]
	Minutes,
	/// 1 h grid.
	Hours,
	/// 1 day grid.
	Days,
	/// 1 week grid.
	Weeks,
	/// 1 month grid.
	Months,
	/// 1 year grid.
	Years,
}

impl From<ResolutionSpec> for Resolution {
	fn from(spec: ResolutionSpec) -> Self {
		match spec {
			ResolutionSpec::Nanoseconds => Self::Nanoseconds,
			ResolutionSpec::Microseconds => Self::Microseconds,
			ResolutionSpec::Milliseconds => Self::Milliseconds,
			ResolutionSpec::Seconds => Self::Seconds,
			ResolutionSpec::Minutes => Self::Minutes,
			ResolutionSpec::Hours => Self::Hours,
			ResolutionSpec::Days => Self::Days,
			ResolutionSpec::Weeks => Self::Weeks,
			ResolutionSpec::Months => Self::Months,
			ResolutionSpec::Years => Self::Years,
		}
	}
}

/// Request body for `POST /api/v1/interpolate`.
#[derive(Debug, Clone, Deserialize)]
pub struct InterpolateRequest {
	/// Spline method (default cubic).
	#[serde(default)]
	pub spline: SplineSpec,
	/// Output-grid resolution (default minutes).
	#[serde(default)]
	pub resolution: ResolutionSpec,
	/// Optional inclusive range start; defaults to the earliest input timestamp.
	#[serde(default)]
	pub start: Option<DateTime<Utc>>,
	/// Optional inclusive range end; defaults to the latest input timestamp.
	#[serde(default)]
	pub end: Option<DateTime<Utc>>,
	/// Input samples (must be non-empty).
	pub points: Vec<InputPoint>,
}

/// Response body for `POST /api/v1/interpolate`.
#[derive(Debug, Clone, Serialize)]
pub struct InterpolateResponse {
	/// Spline method actually used (`Display` form).
	pub spline: String,
	/// Output-grid resolution actually used.
	pub resolution: String,
	/// Number of input samples accepted.
	pub input_points: usize,
	/// Number of interpolated samples produced.
	pub output_points: usize,
	/// The interpolated series on the output grid.
	pub points: Vec<OutputPoint>,
}

/// An API error rendered as `{"error": "..."}` with an appropriate status.
#[derive(Debug)]
pub enum ApiError {
	/// Caller-side problem (malformed or out-of-domain request) → 400.
	BadRequest(String),
	/// Engine-side failure → 500.
	Internal(String),
}

impl ApiError {
	fn bad_request(message: impl Into<String>) -> Self {
		Self::BadRequest(message.into())
	}

	fn internal(message: impl Into<String>) -> Self {
		Self::Internal(message.into())
	}
}

/// JSON error envelope.
#[derive(Debug, Serialize)]
struct ErrorBody {
	error: String,
}

impl IntoResponse for ApiError {
	fn into_response(self) -> Response {
		let (status, error) = match self {
			Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
			Self::Internal(message) => (StatusCode::INTERNAL_SERVER_ERROR, message),
		};
		(status, Json(ErrorBody { error })).into_response()
	}
}

/// Handle `POST /api/v1/interpolate`: reconstruct an irregular series onto a
/// regular output grid using DSP's interpolation engine.
///
/// Records request / error / output-point counters on [`SharedMetrics`] around
/// the core work, so `/metrics` reflects the engine load this endpoint drives.
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] for an empty point set, a non-finite value,
/// or an inverted range, and [`ApiError::Internal`] if the engine fails.
pub async fn interpolate(State(metrics): State<SharedMetrics>, Json(request): Json<InterpolateRequest>) -> Result<Json<InterpolateResponse>, ApiError> {
	let start = std::time::Instant::now();
	metrics.record_interpolate_request();
	let result = interpolate_inner(request).await;
	record_outcome(&metrics, &result);
	// Latency of every request — success and error alike — feeds the p95 target.
	metrics.observe_interpolate_latency(start.elapsed());
	result
}

/// Update the interpolation counters from a finished request's outcome, so both
/// the JSON and ILP handlers account identically.
fn record_outcome(metrics: &SharedMetrics, result: &Result<Json<InterpolateResponse>, ApiError>) {
	match result {
		Ok(response) => metrics.add_output_points(response.0.output_points as u64),
		Err(_) => metrics.record_interpolate_error(),
	}
}

/// The core JSON-request handling: validate, lift to `BigDecimal` points, derive
/// the range, then delegate to [`run_interpolation`].
async fn interpolate_inner(request: InterpolateRequest) -> Result<Json<InterpolateResponse>, ApiError> {
	if request.points.is_empty() {
		return Err(ApiError::bad_request("`points` must not be empty"));
	}

	let input_points = request.points.len();
	let mut points: Vec<Point> = Vec::with_capacity(input_points);
	for sample in &request.points {
		let value = BigDecimal::from_f64(sample.value).ok_or_else(|| ApiError::bad_request("a point value is not a finite number"))?;
		points.push(Point::new(sample.timestamp, value));
	}

	let start = request.start.unwrap_or_else(|| points.iter().map(|p| p.timestamp).min().unwrap_or_else(Utc::now));
	let end = request.end.unwrap_or_else(|| points.iter().map(|p| p.timestamp).max().unwrap_or_else(Utc::now));

	run_interpolation(points, start, end, request.spline.into(), request.resolution.into(), input_points).await
}

/// Run the engine over a prepared point set and shape the response. Shared by
/// the JSON and ILP entry points so both produce identical result envelopes.
async fn run_interpolation(mut points: Vec<Point>, start: DateTime<Utc>, end: DateTime<Utc>, spline: Spline, resolution: Resolution, input_points: usize) -> Result<Json<InterpolateResponse>, ApiError> {
	if end < start {
		return Err(ApiError::bad_request("`end` must not be before `start`"));
	}

	let spline_label = spline.to_string();
	let resolution_label = format!("{resolution:?}");

	// Capture input provenance before the engine consumes the points, so each
	// output grid point can be marked raw / interpolated / extrapolated.
	let input_timestamps: std::collections::HashSet<DateTime<Utc>> = points.iter().map(|p| p.timestamp).collect();
	let min_ts = points.iter().map(|p| p.timestamp).min();
	let max_ts = points.iter().map(|p| p.timestamp).max();

	let output = splimes::auto_interpolate(&mut points, start, end, resolution, spline).await.map_err(|err| ApiError::internal(err.to_string()))?;

	let points: Vec<OutputPoint> = output.iter().map(|p| OutputPoint { timestamp: p.timestamp, value: p.value.to_f64().unwrap_or_default(), kind: classify(p.timestamp, &input_timestamps, min_ts, max_ts) }).collect();

	Ok(Json(InterpolateResponse { spline: spline_label, resolution: resolution_label, output_points: points.len(), input_points, points }))
}

/// The stable lowercase CSV token for a point's provenance (matches the JSON
/// `kind` serialization).
const fn kind_token(kind: PointKind) -> &'static str {
	match kind {
		PointKind::Raw => "raw",
		PointKind::Interpolated => "interpolated",
		PointKind::Extrapolated => "extrapolated",
	}
}

/// Render an [`InterpolateResponse`] as a `timestamp,value,kind` CSV document.
///
/// The CSV counterpart of the JSON interpolate response, for a client that wants
/// the reconstructed series straight into a spreadsheet / `DuckDB` / pandas without
/// parsing JSON. The timestamp is RFC 3339, the value is the same `f64` the JSON
/// body carries (the documented wire-numeric boundary of this endpoint — Storage v2
/// physical types replace it with a lossless representation later), and `kind` is
/// the raw/interpolated/extrapolated provenance token. None of the three columns
/// can contain a comma, so no field escaping is required.
fn interpolate_response_to_csv(response: &InterpolateResponse) -> Response {
	use std::fmt::Write as _;
	let mut out = String::from("timestamp,value,kind\n");
	for point in &response.points {
		let _ = writeln!(out, "{},{},{}", point.timestamp.to_rfc3339(), point.value, kind_token(point.kind));
	}
	([(header::CONTENT_TYPE, "text/csv; charset=utf-8")], out).into_response()
}

/// The Arrow IPC stream content type, per the Apache Arrow conventions.
const ARROW_STREAM_CONTENT_TYPE: &str = "application/vnd.apache.arrow.stream";

/// The Apache Parquet file content type.
const PARQUET_CONTENT_TYPE: &str = "application/vnd.apache.parquet";

/// Decompose an [`InterpolateResponse`] into the primitive columns the
/// reconstructed-series Arrow/Parquet interchange consumes: nanosecond epochs, the
/// `f64` values (the endpoint's documented wire-numeric boundary), and the
/// provenance tokens.
///
/// Kept private and primitive-typed so no `arrow-*` type appears in this module's
/// signatures — all Arrow knowledge stays inside `dsp-arrow`.
fn response_columns(response: &InterpolateResponse) -> (Vec<i64>, Vec<f64>, Vec<&'static str>) {
	let mut timestamps = Vec::with_capacity(response.points.len());
	let mut values = Vec::with_capacity(response.points.len());
	let mut kinds = Vec::with_capacity(response.points.len());
	for point in &response.points {
		timestamps.push(point.timestamp.timestamp_nanos_opt().unwrap_or_default());
		values.push(point.value);
		kinds.push(kind_token(point.kind));
	}
	(timestamps, values, kinds)
}

/// Render an [`InterpolateResponse`] as an **Arrow IPC stream**
/// (`application/vnd.apache.arrow.stream`).
///
/// Shared by the JSON and ILP Arrow handlers so both emit the identical
/// self-describing `timestamp`/`value`/`kind` batch. All `arrow-*` knowledge stays
/// inside `dsp-arrow`; this hands it only primitive columns.
///
/// # Errors
///
/// Returns [`ApiError::Internal`] if the Arrow encoding fails.
fn interpolate_response_to_arrow(response: &InterpolateResponse) -> Result<Response, ApiError> {
	let (timestamps, values, kinds) = response_columns(response);
	let bytes = dsp_arrow::reconstructed_series_to_ipc_bytes(dsp_physical_type::TimeUnit::Nanos, &timestamps, &values, &kinds).map_err(|err| ApiError::internal(err.to_string()))?;
	Ok(([(header::CONTENT_TYPE, ARROW_STREAM_CONTENT_TYPE)], bytes).into_response())
}

/// Render an [`InterpolateResponse`] as an **Apache Parquet** file
/// (`application/vnd.apache.parquet`). The Parquet counterpart of
/// [`interpolate_response_to_arrow`], shared by the JSON and ILP handlers.
///
/// # Errors
///
/// Returns [`ApiError::Internal`] if the Parquet encoding fails.
fn interpolate_response_to_parquet(response: &InterpolateResponse) -> Result<Response, ApiError> {
	let (timestamps, values, kinds) = response_columns(response);
	let bytes = dsp_arrow::reconstructed_series_to_parquet_bytes(dsp_physical_type::TimeUnit::Nanos, &timestamps, &values, &kinds).map_err(|err| ApiError::internal(err.to_string()))?;
	Ok(([(header::CONTENT_TYPE, PARQUET_CONTENT_TYPE)], bytes).into_response())
}

/// Handle `POST /api/v1/interpolate/arrow`: the Arrow-IPC-output sibling of
/// [`interpolate`].
///
/// Takes the identical JSON request body and runs the identical engine path, then
/// serves the reconstructed series as an **Arrow IPC stream**
/// (`application/vnd.apache.arrow.stream`) — a self-describing, columnar
/// `timestamp`/`value`/`kind` batch ready for `DataFusion`, pandas/`PyArrow`, Arrow
/// Flight, or a `.arrow` file. The columnar counterpart of the JSON/CSV interpolate
/// outputs, and the compute-side mirror of the storage Arrow range exports; the
/// heavy `arrow-*` conversion lives entirely in `dsp-arrow`.
///
/// # Errors
///
/// As [`interpolate`]: [`ApiError::BadRequest`] for an empty point set, a
/// non-finite value, or an inverted range, and [`ApiError::Internal`] on an engine
/// or Arrow-encoding failure.
pub async fn interpolate_arrow(State(metrics): State<SharedMetrics>, Json(request): Json<InterpolateRequest>) -> Result<Response, ApiError> {
	metrics.record_interpolate_request();
	let result = interpolate_inner(request).await;
	record_outcome(&metrics, &result);
	interpolate_response_to_arrow(&result?.0)
}

/// Handle `POST /api/v1/interpolate/parquet`: the Parquet-output sibling of
/// [`interpolate`].
///
/// Identical request body and engine path as [`interpolate`], but serves the
/// reconstructed series as an **Apache Parquet** file
/// (`application/vnd.apache.parquet`) — the on-disk columnar interchange `DuckDB`,
/// Spark, pandas/`Polars`, and the `InfluxDB`-3 FDAP stack read natively. The
/// Parquet counterpart of [`interpolate_arrow`]; the self-describing Arrow schema
/// (time unit, value encoding) is embedded in the file.
///
/// # Errors
///
/// As [`interpolate`]: [`ApiError::BadRequest`] for an empty point set, a
/// non-finite value, or an inverted range, and [`ApiError::Internal`] on an engine
/// or Parquet-encoding failure.
pub async fn interpolate_parquet(State(metrics): State<SharedMetrics>, Json(request): Json<InterpolateRequest>) -> Result<Response, ApiError> {
	metrics.record_interpolate_request();
	let result = interpolate_inner(request).await;
	record_outcome(&metrics, &result);
	interpolate_response_to_parquet(&result?.0)
}

/// Handle `POST /api/v1/interpolate/csv`: the CSV-output sibling of
/// [`interpolate`].
///
/// Takes the identical JSON request body and runs the identical engine path, then
/// serves the reconstructed series as a `timestamp,value,kind` CSV document
/// (`text/csv; charset=utf-8`) instead of JSON — the output-format counterpart of
/// the storage CSV range exports, so a benchmark or client can pull an interpolated
/// series in the same universal format.
///
/// # Errors
///
/// As [`interpolate`]: [`ApiError::BadRequest`] for an empty point set, a
/// non-finite value, or an inverted range, and [`ApiError::Internal`] on an engine
/// failure.
pub async fn interpolate_csv(State(metrics): State<SharedMetrics>, Json(request): Json<InterpolateRequest>) -> Result<Response, ApiError> {
	metrics.record_interpolate_request();
	let result = interpolate_inner(request).await;
	record_outcome(&metrics, &result);
	Ok(interpolate_response_to_csv(&result?.0))
}

/// Request body for `POST /api/v1/interpolate/point`.
#[derive(Debug, Clone, Deserialize)]
pub struct PointRequest {
	/// Spline method (default cubic).
	#[serde(default)]
	pub spline: SplineSpec,
	/// The single instant to evaluate the reconstructed signal at.
	pub instant: DateTime<Utc>,
	/// Input samples (must be non-empty).
	pub points: Vec<InputPoint>,
}

/// Response body for `POST /api/v1/interpolate/point`.
#[derive(Debug, Clone, Serialize)]
pub struct PointResponse {
	/// Spline method actually used (`Display` form).
	pub spline: String,
	/// The instant evaluated, echoed back.
	pub instant: DateTime<Utc>,
	/// Number of input samples accepted.
	pub input_points: usize,
	/// The reconstructed value at the instant (narrowed from `BigDecimal`).
	pub value: f64,
	/// Whether the value is raw (coincides with an input), interpolated
	/// (in-range), or extrapolated (out-of-range).
	pub kind: PointKind,
}

/// Handle `POST /api/v1/interpolate/point`: evaluate the reconstructed signal at
/// a single instant (point query / single-instant lookup), labelling the result
/// interpolated vs extrapolated.
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] for an empty point set or a non-finite value,
/// and [`ApiError::Internal`] if the engine fails.
pub async fn interpolate_point(State(metrics): State<SharedMetrics>, Json(request): Json<PointRequest>) -> Result<Json<PointResponse>, ApiError> {
	metrics.record_interpolate_request();
	let result = interpolate_point_inner(request).await;
	match &result {
		Ok(_) => metrics.add_output_points(1),
		Err(_) => metrics.record_interpolate_error(),
	}
	result
}

async fn interpolate_point_inner(request: PointRequest) -> Result<Json<PointResponse>, ApiError> {
	let PointRequest { spline, instant, points: input } = request;
	if input.is_empty() {
		return Err(ApiError::bad_request("`points` must not be empty"));
	}

	let mut points: Vec<Point> = Vec::with_capacity(input.len());
	for sample in &input {
		let value = BigDecimal::from_f64(sample.value).ok_or_else(|| ApiError::bad_request("a point value is not a finite number"))?;
		points.push(Point::new(sample.timestamp, value));
	}

	let input_timestamps: std::collections::HashSet<DateTime<Utc>> = points.iter().map(|p| p.timestamp).collect();
	let min_ts = points.iter().map(|p| p.timestamp).min();
	let max_ts = points.iter().map(|p| p.timestamp).max();
	let kind = classify(instant, &input_timestamps, min_ts, max_ts);

	let spline: Spline = spline.into();
	let spline_label = spline.to_string();

	// Evaluate the fitted spline at the single instant: the spline's value at
	// `instant` is independent of the rest of the output grid, so we interpolate a
	// minimal nanosecond-wide grid that begins at the instant and read the value
	// back at the instant itself (the first grid point). `auto_interpolate`
	// rejects a zero-span range, hence the `+1ns` end.
	let end = instant + Resolution::Nanoseconds.to_step();
	let output = splimes::auto_interpolate(&mut points, instant, end, Resolution::Nanoseconds, spline).await.map_err(|err| ApiError::internal(err.to_string()))?;
	let value = output.first().ok_or_else(|| ApiError::internal("interpolation produced no value at the instant"))?.value.to_f64().unwrap_or_default();

	Ok(Json(PointResponse { spline: spline_label, instant, input_points: input.len(), value, kind }))
}

/// Query parameters for the ILP interpolation endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct IlpParams {
	/// Numeric field to project each line-protocol record onto (required).
	pub field: String,
	/// Timestamp precision token (`ns`/`us`/`ms`/`s`); defaults to nanoseconds
	/// (ILP's own default).
	#[serde(default)]
	pub precision: Option<String>,
	/// Spline token (`linear`/`quadratic`/`cubic`); defaults to cubic. Polynomial
	/// is only reachable via the JSON endpoint (it needs structured parameters).
	#[serde(default)]
	pub spline: Option<String>,
	/// Resolution token (`seconds`..`years`); defaults to minutes.
	#[serde(default)]
	pub resolution: Option<String>,
}

/// Interpolate an `InfluxDB` Line Protocol payload onto a regular grid.
///
/// Handles `POST /api/v1/interpolate/ilp`: parses the body as ILP (the wire
/// format TSBS / `InfluxDB` / `QuestDB` speak), projects the chosen numeric
/// field into a series, and interpolates it.
///
/// The payload is the request body (`text/plain`); the field, precision, spline,
/// and resolution are query parameters. The series range is the data's own
/// timestamp span. Parsing uses the shared, vendor-neutral `dsp-line-protocol`
/// crate, so the server and the benchmark harness accept the exact same dialect.
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] for an unknown precision/spline/resolution
/// token, a malformed payload, fewer than two usable points, or a zero-span
/// series, and [`ApiError::Internal`] if the engine fails.
pub async fn interpolate_ilp(State(metrics): State<SharedMetrics>, Query(params): Query<IlpParams>, body: String) -> Result<Json<InterpolateResponse>, ApiError> {
	let start = std::time::Instant::now();
	metrics.record_interpolate_request();
	let result = interpolate_ilp_inner(&params, &body).await;
	record_outcome(&metrics, &result);
	// The ILP path is what a TSBS-style harness drives — its latency feeds p95 too.
	metrics.observe_interpolate_latency(start.elapsed());
	result
}

/// Handle `POST /api/v1/interpolate/ilp/csv`: the CSV-output sibling of
/// [`interpolate_ilp`].
///
/// Identical ILP parsing, projection, and engine path as [`interpolate_ilp`], but
/// serves the reconstructed series as a `timestamp,value,kind` CSV document. Rounds
/// the ILP compute path out to the same output-format set the JSON path offers
/// (JSON / CSV / Arrow / Parquet), so a TSBS-style harness feeding line protocol can
/// pull results in any of them.
///
/// # Errors
///
/// As [`interpolate_ilp`].
pub async fn interpolate_ilp_csv(State(metrics): State<SharedMetrics>, Query(params): Query<IlpParams>, body: String) -> Result<Response, ApiError> {
	metrics.record_interpolate_request();
	let result = interpolate_ilp_inner(&params, &body).await;
	record_outcome(&metrics, &result);
	Ok(interpolate_response_to_csv(&result?.0))
}

/// Handle `POST /api/v1/interpolate/ilp/arrow`: the Arrow-IPC-output sibling of
/// [`interpolate_ilp`].
///
/// Identical ILP path as [`interpolate_ilp`], serving the reconstructed series as an
/// **Arrow IPC stream** (`application/vnd.apache.arrow.stream`). The line-protocol
/// on-ramp to columnar interpolation output.
///
/// # Errors
///
/// As [`interpolate_ilp`], plus [`ApiError::Internal`] on an Arrow-encoding failure.
pub async fn interpolate_ilp_arrow(State(metrics): State<SharedMetrics>, Query(params): Query<IlpParams>, body: String) -> Result<Response, ApiError> {
	metrics.record_interpolate_request();
	let result = interpolate_ilp_inner(&params, &body).await;
	record_outcome(&metrics, &result);
	interpolate_response_to_arrow(&result?.0)
}

/// Handle `POST /api/v1/interpolate/ilp/parquet`: the Parquet-output sibling of
/// [`interpolate_ilp`].
///
/// Identical ILP path as [`interpolate_ilp`], serving the reconstructed series as an
/// **Apache Parquet** file (`application/vnd.apache.parquet`).
///
/// # Errors
///
/// As [`interpolate_ilp`], plus [`ApiError::Internal`] on a Parquet-encoding failure.
pub async fn interpolate_ilp_parquet(State(metrics): State<SharedMetrics>, Query(params): Query<IlpParams>, body: String) -> Result<Response, ApiError> {
	metrics.record_interpolate_request();
	let result = interpolate_ilp_inner(&params, &body).await;
	record_outcome(&metrics, &result);
	interpolate_response_to_parquet(&result?.0)
}

async fn interpolate_ilp_inner(params: &IlpParams, body: &str) -> Result<Json<InterpolateResponse>, ApiError> {
	let precision = parse_precision_token(params.precision.as_deref())?;
	let spline = parse_spline_token(params.spline.as_deref())?;
	let resolution = parse_resolution_token(params.resolution.as_deref())?;

	let points = dsp_line_protocol::parse_points(body, &params.field, precision).map_err(|err| ApiError::bad_request(err.to_string()))?;

	// `parse_points` returns points sorted ascending by timestamp; the engine
	// needs at least two distinct instants to interpolate between.
	if points.len() < 2 {
		return Err(ApiError::bad_request(format!("need at least two points carrying field `{}` with a timestamp, found {}", params.field, points.len())));
	}
	let start = points.first().map_or_else(Utc::now, |p| p.timestamp);
	let end = points.last().map_or_else(Utc::now, |p| p.timestamp);
	if end <= start {
		return Err(ApiError::bad_request("the line-protocol series spans zero time"));
	}

	let input_points = points.len();
	run_interpolation(points, start, end, spline, resolution, input_points).await
}

/// Map an optional precision token to [`TimestampPrecision`] (default ns).
pub(crate) fn parse_precision_token(token: Option<&str>) -> Result<TimestampPrecision, ApiError> {
	match token.map(str::to_ascii_lowercase).as_deref() {
		None | Some("ns" | "nanoseconds" | "nanos") => Ok(TimestampPrecision::Nanoseconds),
		Some("us" | "µs" | "microseconds" | "micros") => Ok(TimestampPrecision::Microseconds),
		Some("ms" | "milliseconds" | "millis") => Ok(TimestampPrecision::Milliseconds),
		Some("s" | "sec" | "secs" | "seconds") => Ok(TimestampPrecision::Seconds),
		Some(other) => Err(ApiError::bad_request(format!("unknown precision `{other}` (use ns/us/ms/s)"))),
	}
}

/// Map an optional spline token to [`Spline`] (default cubic).
fn parse_spline_token(token: Option<&str>) -> Result<Spline, ApiError> {
	match token.map(str::to_ascii_lowercase).as_deref() {
		Some("linear") => Ok(Spline::Linear),
		Some("quadratic") => Ok(Spline::Quadratic),
		None | Some("cubic") => Ok(Spline::Cubic),
		Some(other) => Err(ApiError::bad_request(format!("unknown spline `{other}` (use linear/quadratic/cubic; polynomial via the JSON endpoint)"))),
	}
}

/// Map an optional resolution token to [`Resolution`] (default minutes).
pub(crate) fn parse_resolution_token(token: Option<&str>) -> Result<Resolution, ApiError> {
	match token.map(str::to_ascii_lowercase).as_deref() {
		Some("nanoseconds") => Ok(Resolution::Nanoseconds),
		Some("microseconds") => Ok(Resolution::Microseconds),
		Some("milliseconds") => Ok(Resolution::Milliseconds),
		Some("seconds") => Ok(Resolution::Seconds),
		None | Some("minutes") => Ok(Resolution::Minutes),
		Some("hours") => Ok(Resolution::Hours),
		Some("days") => Ok(Resolution::Days),
		Some("weeks") => Ok(Resolution::Weeks),
		Some("months") => Ok(Resolution::Months),
		Some("years") => Ok(Resolution::Years),
		Some(other) => Err(ApiError::bad_request(format!("unknown resolution `{other}`"))),
	}
}

#[cfg(test)]
mod tests {
	use axum::{
		body::Body, http::{Request, StatusCode}
	};
	use chrono::TimeZone;
	use tower::ServiceExt;

	use super::*;
	use crate::app;

	async fn post_json(uri: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
		let response = app().oneshot(Request::builder().method("POST").uri(uri).header("content-type", "application/json").body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let value = serde_json::from_slice(&bytes).unwrap();
		(status, value)
	}

	fn ts(secs: i64) -> String {
		Utc.timestamp_opt(secs, 0).unwrap().to_rfc3339()
	}

	#[tokio::test]
	async fn linear_interpolation_fills_the_grid() {
		// Two points one minute apart, value 0 → 60; a linear reconstruction on a
		// 10-second grid must pass through the midpoint at value 30.
		let body = serde_json::json!({
			"spline": "linear",
			"resolution": "seconds",
			"points": [
				{ "timestamp": ts(0), "value": 0.0 },
				{ "timestamp": ts(60), "value": 60.0 },
			],
		});
		let (status, body) = post_json("/api/v1/interpolate", body).await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["spline"], "Linear");
		assert_eq!(body["input_points"], 2);
		assert!(body["output_points"].as_u64().unwrap() >= 2);
		let pts = body["points"].as_array().unwrap();
		assert!(!pts.is_empty());
		// Monotonic, in-range values for a linear fill between 0 and 60.
		for p in pts {
			let v = p["value"].as_f64().unwrap();
			assert!((0.0..=60.0).contains(&v), "value {v} out of [0,60]");
		}
	}

	#[tokio::test]
	async fn range_points_carry_provenance() {
		// On a 10s grid the endpoints t=0 and t=60 coincide with inputs (raw); the
		// interior grid points are synthetic (interpolated). Nothing is out of range
		// because the grid spans exactly the input range.
		let body = serde_json::json!({
			"spline": "linear",
			"resolution": "seconds",
			"points": [
				{ "timestamp": ts(0), "value": 0.0 },
				{ "timestamp": ts(60), "value": 60.0 },
			],
		});
		let (status, body) = post_json("/api/v1/interpolate", body).await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		let pts = body["points"].as_array().unwrap();
		let first = &pts[0];
		assert_eq!(first["timestamp"], "1970-01-01T00:00:00Z");
		assert_eq!(first["kind"], "raw");
		let last = pts.last().unwrap();
		assert_eq!(last["timestamp"], "1970-01-01T00:01:00Z");
		assert_eq!(last["kind"], "raw");
		// An interior synthetic point is interpolated.
		assert!(pts.iter().any(|p| p["kind"] == "interpolated"));
		// No point is extrapolated (the grid stays within the input span).
		assert!(!pts.iter().any(|p| p["kind"] == "extrapolated"));
	}

	#[tokio::test]
	async fn default_spline_is_cubic() {
		let body = serde_json::json!({
			"points": [
				{ "timestamp": ts(0), "value": 1.0 },
				{ "timestamp": ts(120), "value": 2.0 },
				{ "timestamp": ts(240), "value": 1.5 },
			],
		});
		let (status, body) = post_json("/api/v1/interpolate", body).await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["spline"], "Cubic");
	}

	#[tokio::test]
	async fn empty_points_is_bad_request() {
		let (status, body) = post_json("/api/v1/interpolate", serde_json::json!({ "points": [] })).await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
		assert!(body["error"].as_str().unwrap().contains("points"));
	}

	#[tokio::test]
	async fn inverted_range_is_bad_request() {
		let body = serde_json::json!({
			"start": ts(100),
			"end": ts(0),
			"points": [ { "timestamp": ts(50), "value": 1.0 } ],
		});
		let (status, body) = post_json("/api/v1/interpolate", body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
		assert!(body["error"].as_str().unwrap().contains("end"));
	}

	#[test]
	fn spline_spec_maps_to_engine() {
		assert!(matches!(Spline::from(SplineSpec::Linear), Spline::Linear));
		assert!(matches!(Spline::from(SplineSpec::default()), Spline::Cubic));
		assert!(matches!(Spline::from(SplineSpec::Polynomial { degree: 3, bounds_factor: Some(1.5) }), Spline::Polynomial(3, Some(1.5))));
	}

	#[test]
	fn resolution_spec_maps_to_engine() {
		assert!(matches!(Resolution::from(ResolutionSpec::default()), Resolution::Minutes));
		assert!(matches!(Resolution::from(ResolutionSpec::Hours), Resolution::Hours));
	}

	async fn post_text(uri: &str, body: &str) -> (StatusCode, serde_json::Value) {
		let response = app().oneshot(Request::builder().method("POST").uri(uri).header("content-type", "text/plain").body(Body::from(body.to_string())).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let value = serde_json::from_slice(&bytes).unwrap();
		(status, value)
	}

	#[tokio::test]
	async fn ilp_endpoint_interpolates_a_line_protocol_payload() {
		// Two cpu rows a minute apart (seconds precision); linear fill on a 10s grid.
		let payload = "cpu,host=a load=0 1000000000\ncpu,host=a load=60 1000000060\n";
		let (status, body) = post_text("/api/v1/interpolate/ilp?field=load&precision=s&spline=linear&resolution=seconds", payload).await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["spline"], "Linear");
		assert_eq!(body["input_points"], 2);
		assert!(body["output_points"].as_u64().unwrap() >= 2);
		for p in body["points"].as_array().unwrap() {
			let v = p["value"].as_f64().unwrap();
			assert!((0.0..=60.0).contains(&v), "value {v} out of [0,60]");
		}
	}

	#[tokio::test]
	async fn ilp_endpoint_observes_latency() {
		use axum::body::Body;
		use tower::ServiceExt;

		let metrics = crate::SharedMetrics::default();
		let router = crate::app_with_metrics(metrics.clone());
		let payload = "cpu,host=a load=0 1000000000\ncpu,host=a load=60 1000000060\n";
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/interpolate/ilp?field=load&precision=s&spline=linear&resolution=seconds").header("content-type", "text/plain").body(Body::from(payload)).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		// The ILP path feeds the same interpolate latency histogram as the JSON path.
		let snap = metrics.interpolate_latency.snapshot();
		assert_eq!(snap.count, 1);
	}

	#[tokio::test]
	async fn ilp_endpoint_rejects_too_few_points() {
		let payload = "cpu,host=a load=1 1000000000\n";
		let (status, body) = post_text("/api/v1/interpolate/ilp?field=load&precision=s", payload).await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
		assert!(body["error"].as_str().unwrap().contains("at least two"));
	}

	#[tokio::test]
	async fn ilp_endpoint_rejects_unknown_spline_token() {
		let payload = "cpu load=1 1\ncpu load=2 2\n";
		let (status, body) = post_text("/api/v1/interpolate/ilp?field=load&spline=bogus", payload).await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
		assert!(body["error"].as_str().unwrap().contains("unknown spline"));
	}

	#[tokio::test]
	async fn ilp_endpoint_rejects_malformed_payload() {
		let (status, body) = post_text("/api/v1/interpolate/ilp?field=load", "this is not line protocol\n").await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
		// A parse error carries a line number.
		assert!(body["error"].as_str().unwrap().contains("line 1"));
	}

	#[test]
	fn precision_token_parsing() {
		assert!(matches!(parse_precision_token(None), Ok(TimestampPrecision::Nanoseconds)));
		assert!(matches!(parse_precision_token(Some("MS")), Ok(TimestampPrecision::Milliseconds)));
		assert!(parse_precision_token(Some("fortnights")).is_err());
	}

	#[tokio::test]
	async fn point_query_interpolates_at_an_instant() {
		// Linear 0→60 over a minute; the value at the 30s midpoint is 30, in-range.
		let body = serde_json::json!({
			"spline": "linear",
			"instant": ts(30),
			"points": [
				{ "timestamp": ts(0), "value": 0.0 },
				{ "timestamp": ts(60), "value": 60.0 },
			],
		});
		let (status, body) = post_json("/api/v1/interpolate/point", body).await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["spline"], "Linear");
		assert_eq!(body["input_points"], 2);
		assert_eq!(body["kind"], "interpolated");
		let v = body["value"].as_f64().unwrap();
		assert!((v - 30.0).abs() < 1e-6, "value {v} != 30");
	}

	#[tokio::test]
	async fn point_query_passes_through_a_knot() {
		// Evaluating exactly at an input timestamp returns that input's value and
		// is marked `raw` (it coincides with an observation, not a synthetic point).
		let body = serde_json::json!({
			"spline": "linear",
			"instant": ts(60),
			"points": [
				{ "timestamp": ts(0), "value": 0.0 },
				{ "timestamp": ts(60), "value": 60.0 },
			],
		});
		let (status, body) = post_json("/api/v1/interpolate/point", body).await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["kind"], "raw");
		let v = body["value"].as_f64().unwrap();
		assert!((v - 60.0).abs() < 1e-6, "value {v} != 60");
	}

	#[tokio::test]
	async fn point_query_flags_extrapolation() {
		// An instant past the last sample is labelled extrapolated.
		let body = serde_json::json!({
			"spline": "linear",
			"instant": ts(120),
			"points": [
				{ "timestamp": ts(0), "value": 0.0 },
				{ "timestamp": ts(60), "value": 60.0 },
			],
		});
		let (status, body) = post_json("/api/v1/interpolate/point", body).await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["kind"], "extrapolated");
		assert!(body["value"].as_f64().is_some());
	}

	#[tokio::test]
	async fn point_query_empty_points_is_bad_request() {
		let body = serde_json::json!({ "instant": ts(0), "points": [] });
		let (status, body) = post_json("/api/v1/interpolate/point", body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
		assert!(body["error"].as_str().unwrap().contains("points"));
	}

	/// POST a JSON body and return `(status, content-type, text body)`.
	async fn post_json_for_text(uri: &str, body: serde_json::Value) -> (StatusCode, String, String) {
		let response = app().oneshot(Request::builder().method("POST").uri(uri).header("content-type", "application/json").body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap()).await.unwrap();
		let status = response.status();
		let content_type = response.headers().get("content-type").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		(status, content_type, String::from_utf8(bytes.to_vec()).unwrap())
	}

	#[tokio::test]
	async fn interpolate_csv_serves_a_timestamped_series() {
		// Linear 0->60 over a minute on a 10s grid; the CSV carries the raw endpoints
		// and the interpolated interior, with a header row.
		let body = serde_json::json!({
			"spline": "linear",
			"resolution": "seconds",
			"points": [
				{ "timestamp": ts(0), "value": 0.0 },
				{ "timestamp": ts(60), "value": 60.0 },
			],
		});
		let (status, content_type, text) = post_json_for_text("/api/v1/interpolate/csv", body).await;
		assert_eq!(status, StatusCode::OK, "body: {text}");
		assert_eq!(content_type, "text/csv; charset=utf-8");
		let lines: Vec<&str> = text.lines().collect();
		assert_eq!(lines[0], "timestamp,value,kind");
		// The first row is the raw observation at t=0.
		assert!(lines[1].starts_with("1970-01-01T00:00:00+00:00,0,raw"), "row: {}", lines[1]);
		// Some interior row is interpolated.
		assert!(lines.iter().any(|l| l.contains(",interpolated")));
		// The closing observation at t=60 is raw.
		assert!(lines.last().unwrap().contains(",raw"));
	}

	#[tokio::test]
	async fn interpolate_csv_empty_points_is_bad_request() {
		let (status, _content_type, text) = post_json_for_text("/api/v1/interpolate/csv", serde_json::json!({ "points": [] })).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {text}");
	}

	#[test]
	fn kind_token_maps_each_provenance() {
		assert_eq!(kind_token(PointKind::Raw), "raw");
		assert_eq!(kind_token(PointKind::Interpolated), "interpolated");
		assert_eq!(kind_token(PointKind::Extrapolated), "extrapolated");
	}

	/// POST a JSON body and return `(status, content-type, raw bytes)`.
	async fn post_json_for_bytes(uri: &str, body: serde_json::Value) -> (StatusCode, String, Vec<u8>) {
		let response = app().oneshot(Request::builder().method("POST").uri(uri).header("content-type", "application/json").body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap()).await.unwrap();
		let status = response.status();
		let content_type = response.headers().get("content-type").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		(status, content_type, bytes.to_vec())
	}

	fn linear_ramp_body() -> serde_json::Value {
		serde_json::json!({
			"spline": "linear",
			"resolution": "seconds",
			"points": [
				{ "timestamp": ts(0), "value": 0.0 },
				{ "timestamp": ts(60), "value": 60.0 },
			],
		})
	}

	#[tokio::test]
	async fn interpolate_arrow_serves_a_reconstructed_series_batch() {
		let (status, content_type, bytes) = post_json_for_bytes("/api/v1/interpolate/arrow", linear_ramp_body()).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(content_type, "application/vnd.apache.arrow.stream");

		// The bytes are a real Arrow IPC stream carrying the three-column
		// reconstructed-series batch (timestamp/value/kind), with the raw endpoints.
		let batches = dsp_arrow::read_ipc_stream(&bytes).expect("valid arrow ipc");
		assert_eq!(batches.len(), 1);
		let (unit, timestamps, values, kinds) = dsp_arrow::reconstructed_series_from_record_batch(&batches[0]).expect("reads back");
		assert_eq!(unit, dsp_physical_type::TimeUnit::Nanos);
		assert!(!timestamps.is_empty());
		assert_eq!(timestamps.len(), values.len());
		assert_eq!(timestamps.len(), kinds.len());
		// First and last grid points coincide with input observations.
		assert_eq!(kinds.first().map(String::as_str), Some("raw"));
		assert_eq!(kinds.last().map(String::as_str), Some("raw"));
		assert!(kinds.iter().any(|k| k == "interpolated"));
		for v in &values {
			assert!((0.0..=60.0).contains(v), "value {v} out of [0,60]");
		}
	}

	#[tokio::test]
	async fn interpolate_parquet_serves_a_parquet_file() {
		let (status, content_type, bytes) = post_json_for_bytes("/api/v1/interpolate/parquet", linear_ramp_body()).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(content_type, "application/vnd.apache.parquet");
		// Parquet magic.
		assert_eq!(&bytes[..4], b"PAR1");

		let batches = dsp_arrow::read_parquet(&bytes).expect("valid parquet");
		let (unit, timestamps, values, kinds) = dsp_arrow::reconstructed_series_from_record_batch(&batches[0]).expect("reads back");
		assert_eq!(unit, dsp_physical_type::TimeUnit::Nanos);
		assert_eq!(timestamps.len(), values.len());
		assert_eq!(timestamps.len(), kinds.len());
		assert_eq!(kinds.first().map(String::as_str), Some("raw"));
	}

	#[tokio::test]
	async fn interpolate_arrow_empty_points_is_bad_request() {
		let (status, _content_type, _bytes) = post_json_for_bytes("/api/v1/interpolate/arrow", serde_json::json!({ "points": [] })).await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
	}

	/// POST a text (ILP) body and return `(status, content-type, raw bytes)`.
	async fn post_text_for_bytes(uri: &str, body: &str) -> (StatusCode, String, Vec<u8>) {
		let response = app().oneshot(Request::builder().method("POST").uri(uri).header("content-type", "text/plain").body(Body::from(body.to_string())).unwrap()).await.unwrap();
		let status = response.status();
		let content_type = response.headers().get("content-type").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		(status, content_type, bytes.to_vec())
	}

	const ILP_RAMP: &str = "cpu,host=a load=0 1000000000\ncpu,host=a load=60 1000000060\n";

	#[tokio::test]
	async fn interpolate_ilp_csv_serves_a_series() {
		let (status, content_type, bytes) = post_text_for_bytes("/api/v1/interpolate/ilp/csv?field=load&precision=s&spline=linear&resolution=seconds", ILP_RAMP).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(content_type, "text/csv; charset=utf-8");
		let text = String::from_utf8(bytes).unwrap();
		assert_eq!(text.lines().next(), Some("timestamp,value,kind"));
		assert!(text.contains(",raw"));
	}

	#[tokio::test]
	async fn interpolate_ilp_arrow_serves_a_batch() {
		let (status, content_type, bytes) = post_text_for_bytes("/api/v1/interpolate/ilp/arrow?field=load&precision=s&spline=linear&resolution=seconds", ILP_RAMP).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(content_type, "application/vnd.apache.arrow.stream");
		let batches = dsp_arrow::read_ipc_stream(&bytes).expect("valid arrow ipc");
		let (unit, timestamps, values, kinds) = dsp_arrow::reconstructed_series_from_record_batch(&batches[0]).expect("reads back");
		assert_eq!(unit, dsp_physical_type::TimeUnit::Nanos);
		assert_eq!(timestamps.len(), values.len());
		assert_eq!(kinds.first().map(String::as_str), Some("raw"));
		for v in &values {
			assert!((0.0..=60.0).contains(v), "value {v} out of [0,60]");
		}
	}

	#[tokio::test]
	async fn interpolate_ilp_parquet_serves_a_file() {
		let (status, content_type, bytes) = post_text_for_bytes("/api/v1/interpolate/ilp/parquet?field=load&precision=s&spline=linear&resolution=seconds", ILP_RAMP).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(content_type, "application/vnd.apache.parquet");
		assert_eq!(&bytes[..4], b"PAR1");
		let batches = dsp_arrow::read_parquet(&bytes).expect("valid parquet");
		let (_unit, timestamps, _values, kinds) = dsp_arrow::reconstructed_series_from_record_batch(&batches[0]).expect("reads back");
		assert!(!timestamps.is_empty());
		assert_eq!(kinds.first().map(String::as_str), Some("raw"));
	}

	#[tokio::test]
	async fn interpolate_ilp_arrow_rejects_too_few_points() {
		let (status, _content_type, _bytes) = post_text_for_bytes("/api/v1/interpolate/ilp/arrow?field=load&precision=s", "cpu load=1 1000000000\n").await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
	}
}
