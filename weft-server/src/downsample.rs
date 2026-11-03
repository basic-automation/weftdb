//! The downsample / aggregation endpoint (`POST /api/v1/downsample`).
//!
//! This is the second benchmark-grade *query* capability on the Phase-2 API (the
//! roadmap's "downsample query" surface, priority #2 / Phase 2).
//!
//! Where the interpolation endpoint *reconstructs* a denser series, this one
//! *reduces* a series: it groups the input samples into fixed time buckets
//! aligned to the resolution grid and reports a per-bucket aggregate (min / max /
//! avg / sum / first / last) alongside the bucket count. It is the portable
//! building block Weft-Bench needs for the downsampling workload (min/max/avg/
//! count, OHLC-style reductions) without embedding the Rust API.
//!
//! ## Bucketing semantics
//!
//! Buckets are aligned to the epoch grid for the chosen resolution (the
//! [`weft_reduce::bucket_index`] every reducing surface uses), so a bucket's start
//! is reproducible from the resolution alone — independent of where the series
//! happens to begin. By default only buckets that actually contain samples are
//! emitted.
//!
//! ## Gap filling (`fill`)
//!
//! With `fill` (a body field here, `?fill=` on the ILP and stored-range endpoints)
//! every grid step between the bounds is emitted, the empty ones synthesized by
//! [`weft_reduce::fill()`]: `null`, `prev` (or `previous`, `locf`), `linear`,
//! `quadratic`, `cubic`, or a constant. A synthesized bucket has `count == 0` in
//! every output format, so it is never passed off as measured, and a reduction the
//! method leaves without a value (`null`, or a line past the last measured bucket)
//! is missing from the JSON map, an empty CSV cell and an Arrow/Parquet null. The
//! grid is bounded by [`MAX_FILL_BUCKETS`]; a longer one is refused with `400`.
//!
//! ## Bucket widths (`width`, `?resolution=15m`)
//!
//! A bucket may be a whole number of resolution steps — `5s`, `15m`, `6h` — as a `width`
//! body field (which overrides `resolution`) or as the `resolution` token of the ILP and
//! stored-range endpoints. Buckets stay epoch-aligned (a 15-minute bucket starts on the
//! quarter hour), and every reduction is exact: the series is reduced by the unit and each
//! run of unit buckets merged ([`weft_reduce::reduce_width`]).
//!
//! ## Numeric boundary (deliberate, documented)
//!
//! Reductions run in `BigDecimal` — WeftDB's logical/API numeric type — so sums and
//! averages do not accumulate float drift on the hot path; only the final wire
//! value is narrowed to JSON `f64`, mirroring the interpolation endpoint's
//! documented transport boundary (Phase 4 physical encodings replace this with a
//! precision-preserving representation). The narrowing is never silent: a
//! computed value with no finite `f64` representation fails the request with a
//! `not_finite` error rather than returning a substitute such as `0.0` (FRE-5).

use std::collections::BTreeMap;

use axum::{
	extract::State, http::header, response::{IntoResponse, Response}, Json
};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use splimes::{Point, Resolution};
// The reduction core is the vendor-neutral `weft-reduce` crate — shared with the
// `weft-bench` downsample workload so both drive one implementation. `Aggregation` is
// re-exported so the server's public API surface (see `lib.rs`) is unchanged.
pub use weft_reduce::Aggregation;
use weft_reduce::{reduce_width, BucketWidth, Fill};

use crate::{
	error::{self, ApiError, Code, StrictJson, StrictQuery}, interpolate::{InputPoint, ResolutionSpec}, metrics::SharedMetrics
};

/// Request body for `POST /api/v1/downsample`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownsampleRequest {
	/// Bucket width (default minutes), aligned to the epoch grid.
	#[serde(default)]
	pub resolution: ResolutionSpec,
	/// Reductions to report per bucket; defaults to `[min, max, avg]` when omitted
	/// or empty. The bucket count is always included regardless.
	#[serde(default)]
	pub aggregations: Vec<Aggregation>,
	/// Optional inclusive range start; defaults to the earliest input timestamp.
	#[serde(default)]
	pub start: Option<DateTime<Utc>>,
	/// Optional inclusive range end; defaults to the latest input timestamp.
	#[serde(default)]
	pub end: Option<DateTime<Utc>>,
	/// Input samples (must be non-empty).
	pub points: Vec<InputPoint>,
	/// A bucket width that is a whole number of steps (`5s`, `15m`, `6h`); overrides
	/// `resolution` when given.
	#[serde(default)]
	pub width: Option<String>,
	/// Gap fill: emit every grid step between the bounds, the empty ones synthesized by
	/// this method (`null`, `prev`, `linear`, `quadratic`, `cubic` or a constant); omitted,
	/// only non-empty buckets are emitted.
	#[serde(default)]
	pub fill: Option<String>,
}

/// The most buckets a gap fill may emit: the interpolation endpoint's default output
/// cap, since a filled grid is an output grid of the same kind.
pub const MAX_FILL_BUCKETS: usize = crate::interpolate::MAX_INTERPOLATE_OUTPUT_POINTS;

/// Parse a bucket-width token: a resolution name (`minutes`, the default), or a count and a
/// unit (`5s`, `15m`, `15 minutes`).
///
/// # Errors
///
/// `bad_request` for a token that is neither.
pub(crate) fn parse_width_token(token: Option<&str>) -> Result<BucketWidth, ApiError> {
	match crate::interpolate::parse_resolution_token(token) {
		Ok(resolution) => Ok(BucketWidth::from(resolution)),
		Err(err) => token.and_then(BucketWidth::parse).ok_or(err),
	}
}

/// Parse a `fill` token; `None` or empty is no fill.
///
/// # Errors
///
/// `bad_request` for a token that names no method and is not a number.
pub(crate) fn parse_fill(token: Option<&str>) -> Result<Option<Fill>, ApiError> {
	let Some(token) = token.map(str::trim).filter(|t| !t.is_empty()) else { return Ok(None) };
	Fill::from_token(token).map(Some).ok_or_else(|| ApiError::bad_request(format!("unknown fill `{token}`: one of null, prev, linear, quadratic, cubic, or a number")))
}

/// Gap-fill `buckets` over `[start, end]` (a missing bound is the first or last bucket's).
///
/// # Errors
///
/// `bad_request` when the grid would exceed [`MAX_FILL_BUCKETS`] or a spline fill fails,
/// and `value_not_representable` when a bound has no bucket at the resolution.
pub(crate) fn apply_fill(buckets: &[weft_reduce::Bucket], width: BucketWidth, start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>, method: &Fill) -> Result<Vec<weft_reduce::Bucket>, ApiError> {
	weft_reduce::fill_width(buckets, width, start, end, method, MAX_FILL_BUCKETS).map_err(|err| match err {
		weft_reduce::FillError::TooManyBuckets { grid, max } => ApiError::bad_request(format!("a fill over this window would emit {grid} buckets, more than the {max} allowed; narrow the window or coarsen the resolution")),
		weft_reduce::FillError::TimestampRange | weft_reduce::FillError::BucketStartOverflow => ApiError::new(error::Code::ValueNotRepresentable, "a bound of the fill window cannot be placed in a bucket at the resolution you asked for"),
		weft_reduce::FillError::Spline(reason) => ApiError::bad_request(format!("the spline fill failed: {reason}")),
		other => ApiError::internal(other.to_string()),
	})
}

/// One emitted bucket in the response.
#[derive(Debug, Clone, Serialize)]
pub struct DownsampleBucket {
	/// Grid-aligned bucket start timestamp.
	pub timestamp: DateTime<Utc>,
	/// Number of input samples that fell in this bucket.
	pub count: usize,
	/// The requested reductions, keyed by their wire name (see [`Aggregation::as_str`]).
	pub aggregations: BTreeMap<String, f64>,
}

/// Response body for `POST /api/v1/downsample`.
#[derive(Debug, Clone, Serialize)]
pub struct DownsampleResponse {
	/// Bucket width actually used: a resolution name (`minutes`), or a count and one
	/// (`15 minutes`).
	pub resolution: String,
	/// Reductions reported, in request order.
	pub aggregations: Vec<String>,
	/// Number of input samples accepted (within the range).
	pub input_points: usize,
	/// Number of buckets in `series` (non-empty ones, plus the synthesized ones under `fill`).
	pub buckets: usize,
	/// The reduced series, ascending by time: one entry per non-empty bucket, and under
	/// `fill` one per grid step, a synthesized one with `count == 0`.
	pub series: Vec<DownsampleBucket>,
	/// The fill method, when the request named one.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub fill: Option<String>,
}

/// Handle `POST /api/v1/downsample`: reduce a series into grid-aligned buckets.
///
/// Records request / error / bucket counters on [`SharedMetrics`], mirroring the
/// interpolation endpoint's accounting so `/metrics` reflects this endpoint too.
///
/// # Errors
///
/// Returns `bad_request` (`400`) for an empty point set, a non-finite value,
/// or an inverted range, and `internal` (`500`) if a timestamp cannot be
/// mapped onto the resolution grid.
pub async fn downsample(State(metrics): State<SharedMetrics>, StrictJson(request): StrictJson<DownsampleRequest>) -> Result<Json<DownsampleResponse>, ApiError> {
	let start = std::time::Instant::now();
	metrics.record_downsample_request();
	let result = downsample_inner(request);
	match &result {
		Ok(response) => metrics.add_downsample_buckets(response.0.buckets as u64),
		Err(_) => metrics.record_downsample_error(),
	}
	// Latency of every request — success and error alike — feeds the p95 target.
	metrics.observe_downsample_latency(start.elapsed());
	result
}

/// Default reductions when the caller does not name any.
const DEFAULT_AGGREGATIONS: [Aggregation; 3] = Aggregation::DEFAULT;

/// The JSON path: validate, lift to `BigDecimal` points, derive the window, then
/// delegate to [`run_downsample`]. Synchronous — no engine call is needed.
fn downsample_inner(request: DownsampleRequest) -> Result<Json<DownsampleResponse>, ApiError> {
	let DownsampleRequest { resolution, aggregations, start, end, points: input, width, fill } = request;
	let method = parse_fill(fill.as_deref())?;
	let width = match width.as_deref() {
		Some(token) => BucketWidth::parse(token).ok_or_else(|| ApiError::bad_request(format!("unknown bucket width `{token}`: a count and a unit, such as 5s or 15m")))?,
		None => BucketWidth::from(Resolution::from(resolution)),
	};
	if input.is_empty() {
		return Err(ApiError::bad_request("`points` must not be empty".to_string()));
	}

	let aggregations = if aggregations.is_empty() { DEFAULT_AGGREGATIONS.to_vec() } else { aggregations };

	// `downsample.parse` child span (roadmap Phase 3): the f64 → `BigDecimal` value lift
	// and timestamp sort of the input, timed apart from the `downsample.reduce` fold so a
	// `RUST_LOG` run attributes value-conversion cost separately from the reduction.
	let mut points: Vec<Point> = Vec::with_capacity(input.len());
	{
		let _parse = tracing::info_span!("downsample.parse", candidate_points = input.len()).entered();
		for sample in &input {
			let value = BigDecimal::from_f64(sample.value).ok_or_else(|| ApiError::bad_request("a point value is not a finite number".to_string()))?;
			points.push(Point::new(sample.timestamp, value));
		}
		points.sort_by_key(|p| p.timestamp);
	}
	let start = start.unwrap_or_else(|| points.first().map_or_else(Utc::now, |p| p.timestamp));
	let end = end.unwrap_or_else(|| points.last().map_or_else(Utc::now, |p| p.timestamp));

	run_downsample(&points, start, end, width, &aggregations, method.as_ref().map(|m| (m, fill.as_deref().unwrap_or_default())))
}

/// Run the bucketed reduction over a prepared point set and shape the response.
/// Shared by the JSON and ILP entry points so both produce identical result
/// envelopes.
///
/// The reduction itself is the vendor-neutral [`weft_reduce::reduce`] — the same
/// implementation the `weft-bench` downsample workload benchmarks — which groups the
/// windowed points into grid-aligned buckets in `BigDecimal`; this layer converts
/// each bucket's reductions to the wire `f64` at the API boundary.
///
/// Traced as the `downsample.reduce` span (roadmap Phase 3): records the candidate
/// point count and resolution at entry and the in-window point count and produced
/// bucket count on completion, so a `RUST_LOG`-enabled run sees the reduction pass's
/// fan-in and fan-out.
#[tracing::instrument(name = "downsample.reduce", skip_all, fields(candidate_points = points.len(), resolution = %width, input_points = tracing::field::Empty, buckets = tracing::field::Empty))]
fn run_downsample(points: &[Point], start: DateTime<Utc>, end: DateTime<Utc>, width: BucketWidth, aggregations: &[Aggregation], fill: Option<(&Fill, &str)>) -> Result<Json<DownsampleResponse>, ApiError> {
	if end < start {
		return Err(ApiError::bad_request("`end` must not be before `start`".to_string()));
	}

	let mut buckets = reduce_width(points, width, Some(start), Some(end), aggregations).map_err(|err| error::from_reduce_error(&err))?;
	if let Some((method, _)) = fill {
		buckets = apply_fill(&buckets, width, Some(start), Some(end), method)?;
	}
	let mut response = buckets_to_response(buckets, width, aggregations)?;
	response.fill = fill.map(|(_, token)| token.trim().to_string());

	let span = tracing::Span::current();
	span.record("input_points", response.input_points);
	span.record("buckets", response.buckets);
	Ok(Json(response))
}

/// Convert a computed [`BigDecimal`] to the wire `f64`, refusing a value that
/// has no finite `f64` representation.
///
/// The compute endpoints' documented transport boundary narrows the final
/// value to `f64`; a value outside the `f64` range used to fall back to `0.0`,
/// a plausible-looking answer the caller never asked for. FRE-5's
/// no-silent-narrowing clause makes that a typed `not_finite` refusal instead,
/// matching the taxonomy's treatment of produced non-finite values elsewhere
/// (see [`error::from_reduce_error`]). In-range values convert exactly as
/// before; the boundary itself is unchanged.
pub(crate) fn wire_value(value: &BigDecimal, what: &str) -> Result<f64, ApiError> {
	value.to_f64().filter(|v| v.is_finite()).ok_or_else(|| ApiError::new(Code::NotFinite, format!("{what} has no finite f64 representation; refusing to return a silently narrowed substitute")))
}

/// Shape a reduced bucket set into the wire [`DownsampleResponse`].
///
/// The single place a `weft-reduce` [`Bucket`](weft_reduce::Bucket) becomes an API
/// response: it narrows each `BigDecimal` reduction to the documented wire `f64`
/// at the API boundary and derives the reported counts. A reduction with no
/// finite `f64` representation fails the whole response via [`wire_value`]
/// rather than shipping a substitute value. Shared by the compute
/// endpoints (which reduce a request-supplied series) and the stored-range
/// endpoint (which reduces persisted segments via
/// [`weftdb::SegmentStore::downsample_range`]), so both envelopes are identical
/// by construction and cannot drift.
pub(crate) fn buckets_to_response(buckets: Vec<weft_reduce::Bucket>, width: BucketWidth, aggregations: &[Aggregation]) -> Result<DownsampleResponse, ApiError> {
	// Every in-window point lands in exactly one bucket, so the bucket counts sum to
	// the in-window point total the response reports.
	let input_points: usize = buckets.iter().map(|b| b.count).sum();
	let mut series: Vec<DownsampleBucket> = Vec::with_capacity(buckets.len());
	for b in buckets {
		let mut values: BTreeMap<String, f64> = BTreeMap::new();
		for (name, value) in b.values {
			let wire = wire_value(&value, &format!("the `{name}` reduction for the bucket at {}", b.timestamp))?;
			values.insert(name, wire);
		}
		series.push(DownsampleBucket { timestamp: b.timestamp, count: b.count, aggregations: values });
	}
	Ok(DownsampleResponse { resolution: width.to_string(), aggregations: aggregations.iter().map(|a| a.as_str().to_string()).collect(), input_points, buckets: series.len(), series, fill: None })
}

/// Render a [`DownsampleResponse`] as a `timestamp,count,<agg>…` CSV document.
///
/// The CSV counterpart of the JSON downsample response, completing the CSV
/// output surface beside the interpolate and storage range exports. The columns
/// are `timestamp` (RFC 3339, grid-aligned bucket start), `count`, then one
/// column per requested reduction **in request order**; each value is the same
/// wire `f64` the JSON body carries. The rows are the JSON's buckets: a measured
/// bucket carries every requested reduction, and under `fill` a synthesized bucket
/// (`count` 0) may leave a cell blank where its method gives no value. None of the
/// columns can contain a comma, so no field escaping is required.
pub(crate) fn downsample_response_to_csv(response: &DownsampleResponse) -> Response {
	use std::fmt::Write as _;
	let mut out = String::from("timestamp,count");
	for agg in &response.aggregations {
		out.push(',');
		out.push_str(agg);
	}
	out.push('\n');
	for bucket in &response.series {
		let _ = write!(out, "{},{}", bucket.timestamp.to_rfc3339(), bucket.count);
		for agg in &response.aggregations {
			match bucket.aggregations.get(agg) {
				Some(value) => {
					let _ = write!(out, ",{value}");
				}
				None => out.push(','),
			}
		}
		out.push('\n');
	}
	([(header::CONTENT_TYPE, "text/csv; charset=utf-8")], out).into_response()
}

/// The Arrow IPC stream content type, per the Apache Arrow conventions.
const ARROW_STREAM_CONTENT_TYPE: &str = "application/vnd.apache.arrow.stream";

/// The Apache Parquet file content type.
const PARQUET_CONTENT_TYPE: &str = "application/vnd.apache.parquet";

/// Decompose a [`DownsampleResponse`] into the primitive columns the reduction-table
/// Arrow/Parquet interchange consumes: nanosecond bucket epochs, per-bucket counts,
/// and one `f64` column per requested reduction (in request order).
///
/// Kept private and primitive-typed so no `arrow-*` type appears in this module's
/// signatures — all Arrow knowledge stays inside `weft-arrow`.
fn response_columns(response: &DownsampleResponse) -> (Vec<i64>, Vec<i64>, Vec<Vec<Option<f64>>>) {
	let mut timestamps = Vec::with_capacity(response.series.len());
	let mut counts = Vec::with_capacity(response.series.len());
	for bucket in &response.series {
		timestamps.push(bucket.timestamp.timestamp_nanos_opt().unwrap_or_default());
		counts.push(i64::try_from(bucket.count).unwrap_or(i64::MAX));
	}
	// A measured bucket carries every requested reduction; a filled one may lack some, which
	// becomes a null (a column with none stays non-null, the dense schema).
	let agg_columns: Vec<Vec<Option<f64>>> = response.aggregations.iter().map(|name| response.series.iter().map(|bucket| bucket.aggregations.get(name).copied()).collect()).collect();
	(timestamps, counts, agg_columns)
}

/// Render a [`DownsampleResponse`] as an **Arrow IPC stream**
/// (`application/vnd.apache.arrow.stream`).
///
/// Shared by the JSON and ILP Arrow handlers so both emit the identical
/// self-describing reduction-table batch. All `arrow-*` knowledge stays inside
/// `weft-arrow`; this hands it only primitive columns.
///
/// # Errors
///
/// Returns `internal` (`500`) if the Arrow encoding fails.
pub(crate) fn downsample_response_to_arrow(response: &DownsampleResponse) -> Result<Response, ApiError> {
	let (timestamps, counts, agg_columns) = response_columns(response);
	let names: Vec<&str> = response.aggregations.iter().map(String::as_str).collect();
	let bytes = weft_arrow::reduction_table_with_gaps_to_ipc_bytes(weft_physical_type::TimeUnit::Nanos, &timestamps, &counts, &names, &agg_columns).map_err(|err| ApiError::internal(err.to_string()))?;
	Ok(([(header::CONTENT_TYPE, ARROW_STREAM_CONTENT_TYPE)], bytes).into_response())
}

/// Render a [`DownsampleResponse`] as an **Apache Parquet** file
/// (`application/vnd.apache.parquet`). The Parquet counterpart of
/// [`downsample_response_to_arrow`], shared by the JSON and ILP handlers.
///
/// # Errors
///
/// Returns `internal` (`500`) if the Parquet encoding fails.
pub(crate) fn downsample_response_to_parquet(response: &DownsampleResponse) -> Result<Response, ApiError> {
	let (timestamps, counts, agg_columns) = response_columns(response);
	let names: Vec<&str> = response.aggregations.iter().map(String::as_str).collect();
	let bytes = weft_arrow::reduction_table_with_gaps_to_parquet_bytes(weft_physical_type::TimeUnit::Nanos, &timestamps, &counts, &names, &agg_columns).map_err(|err| ApiError::internal(err.to_string()))?;
	Ok(([(header::CONTENT_TYPE, PARQUET_CONTENT_TYPE)], bytes).into_response())
}

/// Handle `POST /api/v1/downsample/arrow`: the Arrow-IPC-output sibling of
/// [`downsample`].
///
/// Takes the identical JSON request body and runs the identical bucketed reduction,
/// then serves the reduced series as an **Arrow IPC stream**
/// (`application/vnd.apache.arrow.stream`) — a self-describing columnar batch with a
/// `timestamp` column, a `count` column, and one `Float64` column per requested
/// reduction. The columnar counterpart of `POST /api/v1/downsample/csv` and the
/// mirror of the interpolate Arrow output; the heavy `arrow-*` conversion lives
/// entirely in `weft-arrow`.
///
/// # Errors
///
/// As [`downsample`]: `bad_request` (`400`) for an empty point set, a non-finite
/// value, or an inverted range, and `internal` (`500`) on a grid-mapping or
/// Arrow-encoding failure.
pub async fn downsample_arrow(State(metrics): State<SharedMetrics>, StrictJson(request): StrictJson<DownsampleRequest>) -> Result<Response, ApiError> {
	metrics.record_downsample_request();
	let result = downsample_inner(request);
	match &result {
		Ok(response) => metrics.add_downsample_buckets(response.0.buckets as u64),
		Err(_) => metrics.record_downsample_error(),
	}
	downsample_response_to_arrow(&result?.0)
}

/// Handle `POST /api/v1/downsample/parquet`: the Parquet-output sibling of
/// [`downsample`].
///
/// Identical request body and reduction as [`downsample`], but serves the reduced
/// series as an **Apache Parquet** file (`application/vnd.apache.parquet`) — the
/// on-disk columnar interchange `DuckDB`, Spark, pandas/`Polars`, and the
/// `InfluxDB`-3 FDAP stack read natively. The Parquet counterpart of
/// [`downsample_arrow`].
///
/// # Errors
///
/// As [`downsample`]: `bad_request` (`400`) for an empty point set, a non-finite
/// value, or an inverted range, and `internal` (`500`) on a grid-mapping or
/// Parquet-encoding failure.
pub async fn downsample_parquet(State(metrics): State<SharedMetrics>, StrictJson(request): StrictJson<DownsampleRequest>) -> Result<Response, ApiError> {
	metrics.record_downsample_request();
	let result = downsample_inner(request);
	match &result {
		Ok(response) => metrics.add_downsample_buckets(response.0.buckets as u64),
		Err(_) => metrics.record_downsample_error(),
	}
	downsample_response_to_parquet(&result?.0)
}

/// Handle `POST /api/v1/downsample/csv`: the CSV-output sibling of [`downsample`].
///
/// Takes the identical JSON request body and runs the identical bucketed reduction,
/// then serves the reduced series as a `timestamp,count,<agg>…` CSV document
/// (`text/csv; charset=utf-8`) instead of JSON — the downsample counterpart of
/// `POST /api/v1/interpolate/csv` and the storage CSV range exports.
///
/// # Errors
///
/// As [`downsample`]: `bad_request` (`400`) for an empty point set, a non-finite
/// value, or an inverted range, and `internal` (`500`) if a timestamp cannot be
/// mapped onto the resolution grid.
pub async fn downsample_csv(State(metrics): State<SharedMetrics>, StrictJson(request): StrictJson<DownsampleRequest>) -> Result<Response, ApiError> {
	metrics.record_downsample_request();
	let result = downsample_inner(request);
	match &result {
		Ok(response) => metrics.add_downsample_buckets(response.0.buckets as u64),
		Err(_) => metrics.record_downsample_error(),
	}
	Ok(downsample_response_to_csv(&result?.0))
}

/// Query parameters for the ILP downsample endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownsampleIlpParams {
	/// Numeric field to project each line-protocol record onto (required).
	pub field: String,
	/// Timestamp precision token (`ns`/`us`/`ms`/`s`); defaults to nanoseconds.
	#[serde(default)]
	pub precision: Option<String>,
	/// Resolution token (`seconds`..`years`) or bucket width (`5s`, `15m`); defaults to minutes.
	#[serde(default)]
	pub resolution: Option<String>,
	/// Comma-separated reductions (e.g. `min,max,avg`); defaults to `min,max,avg`.
	#[serde(default)]
	pub agg: Option<String>,
	/// Gap fill method (`null`, `prev`, `linear`, `quadratic`, `cubic` or a constant).
	#[serde(default)]
	pub fill: Option<String>,
}

/// Downsample an `InfluxDB` Line Protocol payload into grid-aligned buckets.
///
/// Handles `POST /api/v1/downsample/ilp`: parses the body as ILP (the wire format
/// TSBS / `InfluxDB` / `QuestDB` speak), projects the chosen numeric field into a
/// series, and reduces it. The payload is the request body (`text/plain`); the
/// field, precision, resolution, and aggregation list are query parameters. The
/// window is the data's own timestamp span. Parsing uses the shared, vendor-
/// neutral `weft-line-protocol` crate so the server and the benchmark harness
/// accept the exact same dialect.
///
/// # Errors
///
/// Returns `bad_request` (`400`) for an unknown precision/resolution/aggregation
/// token, a malformed payload, or fewer than one usable point, and
/// `internal` (`500`) if a timestamp cannot be mapped onto the grid.
pub async fn downsample_ilp(State(metrics): State<SharedMetrics>, StrictQuery(params): StrictQuery<DownsampleIlpParams>, body: String) -> Result<Json<DownsampleResponse>, ApiError> {
	let start = std::time::Instant::now();
	metrics.record_downsample_request();
	let result = downsample_ilp_inner(&params, &body);
	match &result {
		Ok(response) => metrics.add_downsample_buckets(response.0.buckets as u64),
		Err(_) => metrics.record_downsample_error(),
	}
	// The ILP path is what a TSBS-style harness drives — its latency feeds p95 too.
	metrics.observe_downsample_latency(start.elapsed());
	result
}

/// Account a finished ILP-downsample outcome on [`SharedMetrics`] identically to the
/// JSON path, so every output-format sibling reports the same counters.
fn record_downsample_outcome(metrics: &SharedMetrics, result: &Result<Json<DownsampleResponse>, ApiError>) {
	match result {
		Ok(response) => metrics.add_downsample_buckets(response.0.buckets as u64),
		Err(_) => metrics.record_downsample_error(),
	}
}

/// Handle `POST /api/v1/downsample/ilp/csv`: the CSV-output sibling of
/// [`downsample_ilp`].
///
/// Identical ILP parsing, projection, and reduction as [`downsample_ilp`], but
/// serves the reduced series as a `timestamp,count,<agg>…` CSV document. Rounds the
/// ILP downsample path out to the same output-format set the JSON path offers
/// (JSON / CSV / Arrow / Parquet).
///
/// # Errors
///
/// As [`downsample_ilp`].
pub async fn downsample_ilp_csv(State(metrics): State<SharedMetrics>, StrictQuery(params): StrictQuery<DownsampleIlpParams>, body: String) -> Result<Response, ApiError> {
	metrics.record_downsample_request();
	let result = downsample_ilp_inner(&params, &body);
	record_downsample_outcome(&metrics, &result);
	Ok(downsample_response_to_csv(&result?.0))
}

/// Handle `POST /api/v1/downsample/ilp/arrow`: the Arrow-IPC-output sibling of
/// [`downsample_ilp`].
///
/// Identical ILP path as [`downsample_ilp`], serving the reduced series as an
/// **Arrow IPC stream** (`application/vnd.apache.arrow.stream`).
///
/// # Errors
///
/// As [`downsample_ilp`], plus `internal` (`500`) on an Arrow-encoding failure.
pub async fn downsample_ilp_arrow(State(metrics): State<SharedMetrics>, StrictQuery(params): StrictQuery<DownsampleIlpParams>, body: String) -> Result<Response, ApiError> {
	metrics.record_downsample_request();
	let result = downsample_ilp_inner(&params, &body);
	record_downsample_outcome(&metrics, &result);
	downsample_response_to_arrow(&result?.0)
}

/// Handle `POST /api/v1/downsample/ilp/parquet`: the Parquet-output sibling of
/// [`downsample_ilp`].
///
/// Identical ILP path as [`downsample_ilp`], serving the reduced series as an
/// **Apache Parquet** file (`application/vnd.apache.parquet`).
///
/// # Errors
///
/// As [`downsample_ilp`], plus `internal` (`500`) on a Parquet-encoding failure.
pub async fn downsample_ilp_parquet(State(metrics): State<SharedMetrics>, StrictQuery(params): StrictQuery<DownsampleIlpParams>, body: String) -> Result<Response, ApiError> {
	metrics.record_downsample_request();
	let result = downsample_ilp_inner(&params, &body);
	record_downsample_outcome(&metrics, &result);
	downsample_response_to_parquet(&result?.0)
}

fn downsample_ilp_inner(params: &DownsampleIlpParams, body: &str) -> Result<Json<DownsampleResponse>, ApiError> {
	let precision = crate::interpolate::parse_precision_token(params.precision.as_deref())?;
	let width = parse_width_token(params.resolution.as_deref())?;
	let aggregations = parse_aggregations(params.agg.as_deref())?;
	let method = parse_fill(params.fill.as_deref())?;

	// `parse_points` returns points sorted ascending by timestamp.
	let points = weft_line_protocol::parse_points(body, &params.field, precision).map_err(|err| crate::error::from_line_protocol_error(&err))?;
	if points.is_empty() {
		return Err(ApiError::bad_request(format!("no points carry field `{}` with a timestamp", params.field)));
	}
	let start = points.first().map_or_else(Utc::now, |p| p.timestamp);
	let end = points.last().map_or_else(Utc::now, |p| p.timestamp);

	run_downsample(&points, start, end, width, &aggregations, method.as_ref().map(|m| (m, params.fill.as_deref().unwrap_or_default())))
}

/// Parse a comma-separated aggregation list (default `[min, max, avg]`).
pub(crate) fn parse_aggregations(token: Option<&str>) -> Result<Vec<Aggregation>, ApiError> {
	let Some(token) = token.map(str::trim).filter(|t| !t.is_empty()) else {
		return Ok(DEFAULT_AGGREGATIONS.to_vec());
	};
	token.split(',').map(str::trim).filter(|t| !t.is_empty()).map(parse_aggregation_token).collect()
}

/// Map a single aggregation token to [`Aggregation`] via the shared
/// [`Aggregation::from_token`] parser, so the HTTP surface accepts exactly the token
/// set the reduction supports (including the `median`/`time_weighted_avg` aliases) and
/// cannot drift from `weft-reduce` as reductions are added.
fn parse_aggregation_token(token: &str) -> Result<Aggregation, ApiError> {
	Aggregation::from_token(token).ok_or_else(|| ApiError::bad_request(format!("unknown aggregation `{token}` (use min/max/avg/sum/first/last/p50/p90/p95/p99/twa/twa_linear/twa_bucket_end/sketch_p50/sketch_p90/sketch_p95/sketch_p99)")))
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
	async fn buckets_by_minute_with_min_max_avg() {
		// Three samples in minute 0 (values 0,10,20) and two in minute 1 (30,50).
		let body = serde_json::json!({
			"resolution": "minutes",
			"aggregations": ["min", "max", "avg", "sum", "first", "last"],
			"points": [
				{ "timestamp": ts(0), "value": 0.0 },
				{ "timestamp": ts(20), "value": 10.0 },
				{ "timestamp": ts(40), "value": 20.0 },
				{ "timestamp": ts(60), "value": 30.0 },
				{ "timestamp": ts(90), "value": 50.0 },
			],
		});
		let (status, body) = post_json("/api/v1/downsample", body).await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["resolution"], "minutes");
		assert_eq!(body["input_points"], 5);
		assert_eq!(body["buckets"], 2);
		let series = body["series"].as_array().unwrap();
		assert_eq!(series.len(), 2);

		let first = &series[0];
		assert_eq!(first["count"], 3);
		assert_eq!(first["timestamp"], "1970-01-01T00:00:00Z");
		assert_eq!(first["aggregations"]["min"], 0.0);
		assert_eq!(first["aggregations"]["max"], 20.0);
		assert_eq!(first["aggregations"]["avg"], 10.0);
		assert_eq!(first["aggregations"]["sum"], 30.0);
		assert_eq!(first["aggregations"]["first"], 0.0);
		assert_eq!(first["aggregations"]["last"], 20.0);

		let second = &series[1];
		assert_eq!(second["count"], 2);
		assert_eq!(second["timestamp"], "1970-01-01T00:01:00Z");
		assert_eq!(second["aggregations"]["min"], 30.0);
		assert_eq!(second["aggregations"]["max"], 50.0);
		assert_eq!(second["aggregations"]["avg"], 40.0);
	}

	#[tokio::test]
	async fn default_aggregations_are_min_max_avg() {
		let body = serde_json::json!({
			"resolution": "seconds",
			"points": [
				{ "timestamp": ts(0), "value": 2.0 },
				{ "timestamp": ts(0), "value": 4.0 },
			],
		});
		let (status, body) = post_json("/api/v1/downsample", body).await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["aggregations"], serde_json::json!(["min", "max", "avg"]));
		let bucket = &body["series"][0];
		assert_eq!(bucket["count"], 2);
		assert_eq!(bucket["aggregations"]["avg"], 3.0);
		// No `sum` key was requested.
		assert!(bucket["aggregations"].get("sum").is_none());
	}

	#[tokio::test]
	async fn json_body_accepts_the_documented_reduction_spellings() {
		// The README's underscore spellings and aliases, and a bucket-end name, all in one
		// body. The response names each reduction in the spelling the README documents, so a
		// client can send those names back unchanged.
		let body = serde_json::json!({
			"resolution": "seconds",
			"aggregations": ["min", "median", "twa_linear", "sketch_p99", "twa_bucket_end"],
			"points": [
				{ "timestamp": ts(0), "value": 2.0 },
				{ "timestamp": ts(1), "value": 4.0 },
			],
		});
		let (status, resp) = post_json("/api/v1/downsample", body).await;
		assert_eq!(status, StatusCode::OK, "body: {resp}");
		assert_eq!(resp["aggregations"], serde_json::json!(["min", "p50", "twa_linear", "sketch_p99", "twa_bucket_end"]));
		assert_eq!(resp["series"][0]["aggregations"]["min"], 2.0);
	}

	#[tokio::test]
	async fn json_body_refuses_an_upper_case_reduction_by_name() {
		// Tokens are exact-case, so `Min` is a 400 that names it, the same as `twa_lineer`.
		let body = serde_json::json!({
			"resolution": "seconds",
			"aggregations": ["Min"],
			"points": [{ "timestamp": ts(0), "value": 2.0 }],
		});
		let response = app().oneshot(Request::builder().method("POST").uri("/api/v1/downsample").header("content-type", "application/json").body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::BAD_REQUEST);
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		assert!(String::from_utf8_lossy(&bytes).contains("unknown aggregation `Min`"));
	}

	#[tokio::test]
	async fn json_body_rejects_an_unknown_reduction_by_name() {
		let body = serde_json::json!({
			"resolution": "seconds",
			"aggregations": ["twa_lineer"],
			"points": [{ "timestamp": ts(0), "value": 2.0 }],
		});
		let response = app().oneshot(Request::builder().method("POST").uri("/api/v1/downsample").header("content-type", "application/json").body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::BAD_REQUEST);
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		assert!(String::from_utf8_lossy(&bytes).contains("unknown aggregation `twa_lineer`"));
	}

	#[tokio::test]
	async fn out_of_range_samples_are_excluded() {
		// Only the sample at t=15 falls inside the inclusive [10, 20] window.
		let body = serde_json::json!({
			"resolution": "seconds",
			"aggregations": ["sum"],
			"start": ts(10),
			"end": ts(20),
			"points": [
				{ "timestamp": ts(5), "value": 1.0 },
				{ "timestamp": ts(15), "value": 2.0 },
				{ "timestamp": ts(25), "value": 3.0 },
			],
		});
		let (status, resp) = post_json("/api/v1/downsample", body).await;
		assert_eq!(status, StatusCode::OK, "body: {resp}");
		assert_eq!(resp["input_points"], 1);
		assert_eq!(resp["buckets"], 1);
		assert_eq!(resp["series"][0]["aggregations"]["sum"], 2.0);
	}

	#[tokio::test]
	async fn empty_points_is_bad_request() {
		let (status, body) = post_json("/api/v1/downsample", serde_json::json!({ "points": [] })).await;
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
		let (status, body) = post_json("/api/v1/downsample", body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
		assert!(body["error"].as_str().unwrap().contains("end"));
	}

	#[test]
	fn bucket_start_is_grid_aligned() {
		// 1970-01-01T00:02:00Z falls in minute-bucket index 2.
		let t = Utc.timestamp_opt(125, 0).unwrap();
		let base = weft_reduce::bucket_index(Resolution::Minutes, &t).unwrap();
		assert_eq!(base, 2);
		let start = weft_reduce::bucket_start(Resolution::Minutes, base).unwrap();
		assert_eq!(start, Utc.timestamp_opt(120, 0).unwrap());
	}

	#[test]
	fn aggregation_tokens_are_stable() {
		assert_eq!(Aggregation::Min.as_str(), "min");
		assert_eq!(Aggregation::Avg.as_str(), "avg");
		assert_eq!(Aggregation::Last.as_str(), "last");
	}

	#[test]
	fn aggregation_list_parsing() {
		assert_eq!(parse_aggregations(None).unwrap(), DEFAULT_AGGREGATIONS.to_vec());
		assert_eq!(parse_aggregations(Some("  ")).unwrap(), DEFAULT_AGGREGATIONS.to_vec());
		assert_eq!(parse_aggregations(Some("min,sum, last")).unwrap(), vec![Aggregation::Min, Aggregation::Sum, Aggregation::Last]);
		assert!(parse_aggregations(Some("min,bogus")).is_err());
		assert!(parse_aggregations(Some("min,SUM")).is_err(), "an upper-case token is refused, not folded");
	}

	#[test]
	fn aggregation_tokens_track_the_shared_reduce_parser() {
		// The HTTP surface parses via `weft_reduce::Aggregation::from_token`, so every
		// token the reduction supports is reachable here — including the aliases the
		// server's own former token match did not carry.
		for agg in [Aggregation::Min, Aggregation::Max, Aggregation::Avg, Aggregation::Sum, Aggregation::First, Aggregation::Last, Aggregation::P50, Aggregation::P90, Aggregation::P95, Aggregation::P99, Aggregation::Twa] {
			assert_eq!(parse_aggregations(Some(agg.as_str())).unwrap(), vec![agg], "{} must parse at the HTTP surface", agg.as_str());
		}
		assert_eq!(parse_aggregations(Some("median")).unwrap(), vec![Aggregation::P50], "median aliases p50");
		assert_eq!(parse_aggregations(Some("time_weighted_avg")).unwrap(), vec![Aggregation::Twa], "time_weighted_avg aliases twa");
	}

	async fn post_text(uri: &str, body: &str) -> (StatusCode, serde_json::Value) {
		let response = app().oneshot(Request::builder().method("POST").uri(uri).header("content-type", "text/plain").body(Body::from(body.to_string())).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let value = serde_json::from_slice(&bytes).unwrap();
		(status, value)
	}

	#[tokio::test]
	async fn ilp_endpoint_downsamples_a_line_protocol_payload() {
		// Three cpu rows in one minute and one in the next (seconds precision).
		// 1000000020 is minute-aligned (divisible by 60), so the first three rows
		// share a minute bucket and the +60s row opens the next.
		let payload = "cpu,host=a load=0 1000000020\ncpu,host=a load=10 1000000040\ncpu,host=a load=20 1000000060\ncpu,host=a load=50 1000000080\n";
		let (status, body) = post_text("/api/v1/downsample/ilp?field=load&precision=s&resolution=minutes&agg=min,max,avg", payload).await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["resolution"], "minutes");
		assert_eq!(body["input_points"], 4);
		assert_eq!(body["buckets"], 2);
		let first = &body["series"][0];
		assert_eq!(first["count"], 3);
		assert_eq!(first["aggregations"]["min"], 0.0);
		assert_eq!(first["aggregations"]["max"], 20.0);
		assert_eq!(first["aggregations"]["avg"], 10.0);
		let second = &body["series"][1];
		assert_eq!(second["count"], 1);
		assert_eq!(second["aggregations"]["avg"], 50.0);
	}

	#[tokio::test]
	async fn ilp_endpoint_defaults_aggregations() {
		let payload = "cpu load=1 1\ncpu load=3 2\n";
		let (status, body) = post_text("/api/v1/downsample/ilp?field=load&precision=s", payload).await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["aggregations"], serde_json::json!(["min", "max", "avg"]));
	}

	#[tokio::test]
	async fn ilp_endpoint_rejects_unknown_aggregation() {
		let payload = "cpu load=1 1\ncpu load=2 2\n";
		let (status, body) = post_text("/api/v1/downsample/ilp?field=load&agg=bogus", payload).await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
		assert!(body["error"].as_str().unwrap().contains("unknown aggregation"));
	}

	#[tokio::test]
	async fn ilp_endpoint_rejects_missing_field() {
		// No record carries field `temp`, so the projected series is empty.
		let payload = "cpu load=1 1\ncpu load=2 2\n";
		let (status, body) = post_text("/api/v1/downsample/ilp?field=temp&precision=s", payload).await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
		assert!(body["error"].as_str().unwrap().contains("no points"));
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
	async fn downsample_csv_serves_bucketed_rows() {
		// Three samples in minute 0 (0,10,20) and two in minute 1 (30,50).
		let body = serde_json::json!({
			"resolution": "minutes",
			"aggregations": ["min", "max", "avg"],
			"points": [
				{ "timestamp": ts(0), "value": 0.0 },
				{ "timestamp": ts(20), "value": 10.0 },
				{ "timestamp": ts(40), "value": 20.0 },
				{ "timestamp": ts(60), "value": 30.0 },
				{ "timestamp": ts(90), "value": 50.0 },
			],
		});
		let (status, content_type, text) = post_json_for_text("/api/v1/downsample/csv", body).await;
		assert_eq!(status, StatusCode::OK, "body: {text}");
		assert_eq!(content_type, "text/csv; charset=utf-8");
		let lines: Vec<&str> = text.lines().collect();
		// Header carries the requested reductions in request order.
		assert_eq!(lines[0], "timestamp,count,min,max,avg");
		// Minute-0 bucket: count 3, min 0, max 20, avg 10.
		assert_eq!(lines[1], "1970-01-01T00:00:00+00:00,3,0,20,10");
		// Minute-1 bucket: count 2, min 30, max 50, avg 40.
		assert_eq!(lines[2], "1970-01-01T00:01:00+00:00,2,30,50,40");
		assert_eq!(lines.len(), 3);
	}

	/// Minute 0 holds 0 and 10, minute 3 holds 30: two empty minutes between.
	fn gapped_body(fill: &str) -> serde_json::Value {
		serde_json::json!({
			"resolution": "minutes",
			"aggregations": ["min", "max"],
			"fill": fill,
			"points": [
				{ "timestamp": ts(0), "value": 0.0 },
				{ "timestamp": ts(30), "value": 10.0 },
				{ "timestamp": ts(180), "value": 30.0 },
			],
		})
	}

	#[tokio::test]
	async fn a_linear_fill_emits_every_minute_and_marks_the_synthesized_ones() {
		let (status, body) = post_json("/api/v1/downsample", gapped_body("linear")).await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!((body["buckets"].as_u64(), body["fill"].as_str(), body["input_points"].as_u64()), (Some(4), Some("linear"), Some(3)));
		let series = body["series"].as_array().unwrap();
		assert_eq!(series.iter().map(|b| b["count"].as_u64().unwrap()).collect::<Vec<_>>(), [2, 0, 0, 1], "a synthesized bucket has count 0");
		assert_eq!(series[1]["aggregations"]["min"], 10.0, "min 0 → 30 over three steps");
		assert_eq!(series[2]["aggregations"]["max"], 23.333_333_333_333_332);
		let (status, plain) = post_json("/api/v1/downsample", gapped_body("")).await;
		assert_eq!((status, plain["buckets"].as_u64(), plain.get("fill")), (StatusCode::OK, Some(2), None), "an empty fill is no fill");
	}

	#[tokio::test]
	async fn a_null_fill_leaves_the_cells_empty_in_every_format() {
		let (status, body) = post_json("/api/v1/downsample", gapped_body("null")).await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert!(body["series"][1]["aggregations"].as_object().unwrap().is_empty());
		let (_, _, text) = post_json_for_text("/api/v1/downsample/csv", gapped_body("null")).await;
		assert_eq!(text.lines().nth(2), Some("1970-01-01T00:01:00+00:00,0,,"));
		let (status, _, bytes) = post_json_for_bytes("/api/v1/downsample/parquet", gapped_body("null")).await;
		assert_eq!(status, StatusCode::OK);
		let batch = &weft_arrow::read_parquet(&bytes).expect("valid parquet")[0];
		assert_eq!(batch.column_by_name("min").map(|c| c.null_count()), Some(2));
		let (status, _, bytes) = post_json_for_bytes("/api/v1/downsample/arrow", gapped_body("prev")).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(weft_arrow::read_ipc_stream(&bytes).expect("valid arrow")[0].column_by_name("min").map(|c| c.null_count()), Some(0), "a carried-forward fill has every value");
	}

	#[tokio::test]
	async fn a_fill_is_bounded_and_its_name_checked() {
		let (status, body) = post_json("/api/v1/downsample", gapped_body("sideways")).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		let mut wide = gapped_body("null");
		wide["resolution"] = "nanoseconds".into();
		wide["end"] = ts(86_400).into();
		let (status, body) = post_json("/api/v1/downsample", wide).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body.to_string().contains("more than the"), "{body}");
	}

	#[tokio::test]
	async fn a_bucket_width_is_a_whole_number_of_steps_and_epoch_aligned() {
		let body = serde_json::json!({
			"width": "5s",
			"aggregations": ["min", "max"],
			"points": (0..12).map(|s| serde_json::json!({ "timestamp": ts(s), "value": s as f64 })).collect::<Vec<_>>(),
		});
		let (status, body) = post_json("/api/v1/downsample", body).await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["resolution"], "5 seconds");
		let series = body["series"].as_array().unwrap();
		assert_eq!(series.iter().map(|b| (b["timestamp"].as_str().unwrap().to_string(), b["count"].as_u64().unwrap(), b["aggregations"]["max"].as_f64().unwrap())).collect::<Vec<_>>(), [("1970-01-01T00:00:00Z".to_string(), 5, 4.0), ("1970-01-01T00:00:05Z".to_string(), 5, 9.0), ("1970-01-01T00:00:10Z".to_string(), 2, 11.0)]);
		let mut bad = gapped_body("");
		bad["width"] = "5 parsecs".into();
		assert_eq!(post_json("/api/v1/downsample", bad).await.0, StatusCode::BAD_REQUEST);
	}

	#[tokio::test]
	async fn the_ilp_endpoint_takes_a_width_as_its_resolution() {
		let response = app().oneshot(Request::builder().method("POST").uri("/api/v1/downsample/ilp?field=v&precision=s&resolution=15m&agg=max&fill=null").body(Body::from("m v=1 60\nm v=4 1000\nm v=9 2800\n")).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		let body: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
		assert_eq!(body["resolution"], "15 minutes");
		assert_eq!(body["series"].as_array().map(|s| s.iter().map(|b| b["count"].as_u64().unwrap()).collect::<Vec<_>>()), Some(vec![1, 1, 0, 1]), "{body}");
	}

	#[tokio::test]
	async fn the_ilp_endpoint_takes_fill_as_a_query_parameter() {
		let response = app().oneshot(Request::builder().method("POST").uri("/api/v1/downsample/ilp?field=v&precision=s&resolution=minutes&agg=min&fill=prev").body(Body::from("m v=1 0\nm v=4 180\n")).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		let body: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
		assert_eq!(body["series"].as_array().map(|s| s.iter().map(|b| b["aggregations"]["min"].as_f64().unwrap()).collect::<Vec<_>>()), Some(vec![1.0, 1.0, 1.0, 4.0]));
	}

	#[tokio::test]
	async fn downsample_csv_empty_points_is_bad_request() {
		let (status, _content_type, text) = post_json_for_text("/api/v1/downsample/csv", serde_json::json!({ "points": [] })).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {text}");
	}

	/// POST a JSON body and return `(status, content-type, raw bytes)`.
	async fn post_json_for_bytes(uri: &str, body: serde_json::Value) -> (StatusCode, String, Vec<u8>) {
		let response = app().oneshot(Request::builder().method("POST").uri(uri).header("content-type", "application/json").body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap()).await.unwrap();
		let status = response.status();
		let content_type = response.headers().get("content-type").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		(status, content_type, bytes.to_vec())
	}

	fn two_bucket_body() -> serde_json::Value {
		// Three samples in minute 0 (0,10,20) and two in minute 1 (30,50).
		serde_json::json!({
			"resolution": "minutes",
			"aggregations": ["min", "max", "avg"],
			"points": [
				{ "timestamp": ts(0), "value": 0.0 },
				{ "timestamp": ts(20), "value": 10.0 },
				{ "timestamp": ts(40), "value": 20.0 },
				{ "timestamp": ts(60), "value": 30.0 },
				{ "timestamp": ts(90), "value": 50.0 },
			],
		})
	}

	#[tokio::test]
	async fn downsample_arrow_serves_a_reduction_table_batch() {
		let (status, content_type, bytes) = post_json_for_bytes("/api/v1/downsample/arrow", two_bucket_body()).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(content_type, "application/vnd.apache.arrow.stream");

		let batches = weft_arrow::read_ipc_stream(&bytes).expect("valid arrow ipc");
		let (unit, timestamps, counts, aggs) = weft_arrow::reduction_table_from_record_batch(&batches[0]).expect("reads back");
		assert_eq!(unit, weft_physical_type::TimeUnit::Nanos);
		assert_eq!(timestamps.len(), 2);
		assert_eq!(counts, vec![3, 2]);
		// Reductions preserve request order and per-bucket values.
		assert_eq!(aggs[0], ("min".to_string(), vec![0.0, 30.0]));
		assert_eq!(aggs[1], ("max".to_string(), vec![20.0, 50.0]));
		assert_eq!(aggs[2], ("avg".to_string(), vec![10.0, 40.0]));
	}

	#[tokio::test]
	async fn downsample_parquet_serves_a_parquet_file() {
		let (status, content_type, bytes) = post_json_for_bytes("/api/v1/downsample/parquet", two_bucket_body()).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(content_type, "application/vnd.apache.parquet");
		assert_eq!(&bytes[..4], b"PAR1");

		let batches = weft_arrow::read_parquet(&bytes).expect("valid parquet");
		let (_unit, _timestamps, counts, aggs) = weft_arrow::reduction_table_from_record_batch(&batches[0]).expect("reads back");
		assert_eq!(counts, vec![3, 2]);
		assert_eq!(aggs[2], ("avg".to_string(), vec![10.0, 40.0]));
	}

	#[tokio::test]
	async fn downsample_arrow_empty_points_is_bad_request() {
		let (status, _content_type, _bytes) = post_json_for_bytes("/api/v1/downsample/arrow", serde_json::json!({ "points": [] })).await;
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

	// Three cpu rows sharing minute bucket 1000000020 and one opening the next.
	const ILP_BUCKETS: &str = "cpu,host=a load=0 1000000020\ncpu,host=a load=10 1000000040\ncpu,host=a load=20 1000000060\ncpu,host=a load=50 1000000080\n";

	#[tokio::test]
	async fn downsample_ilp_csv_serves_bucketed_rows() {
		let (status, content_type, bytes) = post_text_for_bytes("/api/v1/downsample/ilp/csv?field=load&precision=s&resolution=minutes&agg=min,max,avg", ILP_BUCKETS).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(content_type, "text/csv; charset=utf-8");
		let text = String::from_utf8(bytes).unwrap();
		assert_eq!(text.lines().next(), Some("timestamp,count,min,max,avg"));
	}

	#[tokio::test]
	async fn downsample_ilp_arrow_serves_a_reduction_table() {
		let (status, content_type, bytes) = post_text_for_bytes("/api/v1/downsample/ilp/arrow?field=load&precision=s&resolution=minutes&agg=min,max,avg", ILP_BUCKETS).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(content_type, "application/vnd.apache.arrow.stream");
		let batches = weft_arrow::read_ipc_stream(&bytes).expect("valid arrow ipc");
		let (unit, _ts, counts, aggs) = weft_arrow::reduction_table_from_record_batch(&batches[0]).expect("reads back");
		assert_eq!(unit, weft_physical_type::TimeUnit::Nanos);
		assert_eq!(counts, vec![3, 1]);
		assert_eq!(aggs[0], ("min".to_string(), vec![0.0, 50.0]));
	}

	#[tokio::test]
	async fn downsample_ilp_parquet_serves_a_file() {
		let (status, content_type, bytes) = post_text_for_bytes("/api/v1/downsample/ilp/parquet?field=load&precision=s&resolution=minutes", ILP_BUCKETS).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(content_type, "application/vnd.apache.parquet");
		assert_eq!(&bytes[..4], b"PAR1");
		let batches = weft_arrow::read_parquet(&bytes).expect("valid parquet");
		let (_unit, _ts, counts, _aggs) = weft_arrow::reduction_table_from_record_batch(&batches[0]).expect("reads back");
		assert_eq!(counts, vec![3, 1]);
	}

	#[tokio::test]
	async fn downsample_ilp_arrow_rejects_missing_field() {
		let (status, _content_type, _bytes) = post_text_for_bytes("/api/v1/downsample/ilp/arrow?field=temp&precision=s", ILP_BUCKETS).await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
	}

	#[test]
	fn wire_value_refuses_values_outside_f64() {
		// 1e308 * 1e10 = 1e318: a finite BigDecimal with no finite f64 form.
		let huge = BigDecimal::from_f64(1e308).unwrap() * BigDecimal::from_f64(1e10).unwrap();
		let err = wire_value(&huge, "the test value").unwrap_err();
		assert_eq!(err.code(), Code::NotFinite);
		// In-range values convert exactly as before.
		assert_eq!(wire_value(&BigDecimal::from_f64(1.5).unwrap(), "the test value").unwrap(), 1.5);
		assert_eq!(wire_value(&BigDecimal::from_f64(-1e308).unwrap(), "the test value").unwrap(), -1e308);
	}

	#[tokio::test]
	async fn sum_beyond_f64_range_is_refused_not_zeroed() {
		// Two 1e308 samples in one bucket: the BigDecimal sum (2e308) has no
		// finite f64 representation. FRE-5 (no silent narrowing): the endpoint
		// refuses with not_finite instead of returning a substitute 0.0.
		let body = serde_json::json!({
			"resolution": "minutes",
			"aggregations": ["sum"],
			"points": [
				{ "timestamp": ts(0), "value": 1e308 },
				{ "timestamp": ts(20), "value": 1e308 },
			],
		});
		let (status, body) = post_json("/api/v1/downsample", body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert_eq!(body["code"], "not_finite", "body: {body}");
	}
}
