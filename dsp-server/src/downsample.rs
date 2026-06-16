//! The downsample / aggregation endpoint (`POST /api/v1/downsample`).
//!
//! This is the second benchmark-grade *query* capability on the Phase-2 API (the
//! roadmap's "downsample query" surface, priority #2 / Phase 2).
//!
//! Where the interpolation endpoint *reconstructs* a denser series, this one
//! *reduces* a series: it groups the input samples into fixed time buckets
//! aligned to the resolution grid and reports a per-bucket aggregate (min / max /
//! avg / sum / first / last) alongside the bucket count. It is the portable
//! building block DSP-Bench needs for the downsampling workload (min/max/avg/
//! count, OHLC-style reductions) without embedding the Rust API.
//!
//! ## Bucketing semantics
//!
//! Buckets are aligned to the epoch grid for the chosen resolution (the same
//! [`splimes::Resolution::to_base`] index the engine uses), so a bucket's start
//! is reproducible from the resolution alone — independent of where the series
//! happens to begin. Only buckets that actually contain samples are emitted;
//! gap-filling is the interpolation endpoint's job, not this one's.
//!
//! ## Numeric boundary (deliberate, documented)
//!
//! Reductions run in `BigDecimal` — DSP's logical/API numeric type — so sums and
//! averages do not accumulate float drift on the hot path; only the final wire
//! value is narrowed to JSON `f64`, mirroring the interpolation endpoint's
//! documented transport boundary (Phase 4 physical encodings replace this with a
//! precision-preserving representation).

use std::collections::BTreeMap;

use axum::{extract::State, Json};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use splimes::{Point, Resolution, SECONDS_IN_DAY, SECONDS_IN_HOUR, SECONDS_IN_MINUTE, SECONDS_IN_MONTH, SECONDS_IN_WEEK, SECONDS_IN_YEAR};

use crate::{
	interpolate::{ApiError, InputPoint, ResolutionSpec}, metrics::SharedMetrics
};

/// A per-bucket reduction selector.
///
/// Vendor-neutral lowercase JSON tokens (`"min"`, `"max"`, `"avg"`, `"sum"`,
/// `"first"`, `"last"`); the bucket count is always reported separately and so is
/// not a member here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Aggregation {
	/// Smallest value in the bucket.
	Min,
	/// Largest value in the bucket.
	Max,
	/// Arithmetic mean of the bucket (sum ÷ count, computed in `BigDecimal`).
	Avg,
	/// Sum of the bucket's values.
	Sum,
	/// First value in the bucket by ascending timestamp.
	First,
	/// Last value in the bucket by ascending timestamp.
	Last,
}

impl Aggregation {
	/// The stable wire key this reduction is reported under.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Min => "min",
			Self::Max => "max",
			Self::Avg => "avg",
			Self::Sum => "sum",
			Self::First => "first",
			Self::Last => "last",
		}
	}
}

/// Request body for `POST /api/v1/downsample`.
#[derive(Debug, Clone, Deserialize)]
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
	/// Bucket resolution actually used.
	pub resolution: String,
	/// Reductions reported, in request order.
	pub aggregations: Vec<String>,
	/// Number of input samples accepted (within the range).
	pub input_points: usize,
	/// Number of non-empty buckets produced.
	pub buckets: usize,
	/// The reduced series, one entry per non-empty bucket, ascending by time.
	pub series: Vec<DownsampleBucket>,
}

/// Handle `POST /api/v1/downsample`: reduce a series into grid-aligned buckets.
///
/// Records request / error / bucket counters on [`SharedMetrics`], mirroring the
/// interpolation endpoint's accounting so `/metrics` reflects this endpoint too.
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] for an empty point set, a non-finite value,
/// or an inverted range, and [`ApiError::Internal`] if a timestamp cannot be
/// mapped onto the resolution grid.
pub async fn downsample(State(metrics): State<SharedMetrics>, Json(request): Json<DownsampleRequest>) -> Result<Json<DownsampleResponse>, ApiError> {
	metrics.record_downsample_request();
	let result = downsample_inner(request);
	match &result {
		Ok(response) => metrics.add_downsample_buckets(response.0.buckets as u64),
		Err(_) => metrics.record_downsample_error(),
	}
	result
}

/// Default reductions when the caller does not name any.
const DEFAULT_AGGREGATIONS: [Aggregation; 3] = [Aggregation::Min, Aggregation::Max, Aggregation::Avg];

/// The JSON path: validate, lift to `BigDecimal` points, derive the window, then
/// delegate to [`run_downsample`]. Synchronous — no engine call is needed.
fn downsample_inner(request: DownsampleRequest) -> Result<Json<DownsampleResponse>, ApiError> {
	let DownsampleRequest { resolution, aggregations, start, end, points: input } = request;
	if input.is_empty() {
		return Err(ApiError::BadRequest("`points` must not be empty".to_string()));
	}

	let resolution: Resolution = resolution.into();
	let aggregations = if aggregations.is_empty() { DEFAULT_AGGREGATIONS.to_vec() } else { aggregations };

	let mut points: Vec<Point> = Vec::with_capacity(input.len());
	for sample in &input {
		let value = BigDecimal::from_f64(sample.value).ok_or_else(|| ApiError::BadRequest("a point value is not a finite number".to_string()))?;
		points.push(Point::new(sample.timestamp, value));
	}
	points.sort_by_key(|p| p.timestamp);
	let start = start.unwrap_or_else(|| points.first().map_or_else(Utc::now, |p| p.timestamp));
	let end = end.unwrap_or_else(|| points.last().map_or_else(Utc::now, |p| p.timestamp));

	run_downsample(&points, start, end, resolution, &aggregations)
}

/// Run the bucketed reduction over a prepared, ascending-sorted point set and
/// shape the response. Shared by the JSON and ILP entry points so both produce
/// identical result envelopes.
///
/// Sorted timestamps make same-bucket samples contiguous, so a single pass over
/// the windowed series folds each bucket without a hash map.
fn run_downsample(points: &[Point], start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, aggregations: &[Aggregation]) -> Result<Json<DownsampleResponse>, ApiError> {
	if end < start {
		return Err(ApiError::BadRequest("`end` must not be before `start`".to_string()));
	}

	let mut series: Vec<DownsampleBucket> = Vec::new();
	let mut input_points = 0usize;
	let mut current: Option<(i64, BucketAcc)> = None;
	for point in points {
		if point.timestamp < start || point.timestamp > end {
			continue;
		}
		let base = resolution.to_base(&point.timestamp).map_err(|err| ApiError::Internal(err.to_string()))?;
		input_points += 1;
		match &mut current {
			Some((cur_base, acc)) if *cur_base == base => acc.push(point.value.clone()),
			_ => {
				if let Some((cur_base, acc)) = current.take() {
					series.push(acc.finish(resolution, cur_base, aggregations)?);
				}
				let mut acc = BucketAcc::default();
				acc.push(point.value.clone());
				current = Some((base, acc));
			}
		}
	}
	if let Some((cur_base, acc)) = current.take() {
		series.push(acc.finish(resolution, cur_base, aggregations)?);
	}

	Ok(Json(DownsampleResponse { resolution: resolution.to_string(), aggregations: aggregations.iter().map(|a| a.as_str().to_string()).collect(), input_points, buckets: series.len(), series }))
}

/// Query parameters for the ILP downsample endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct DownsampleIlpParams {
	/// Numeric field to project each line-protocol record onto (required).
	pub field: String,
	/// Timestamp precision token (`ns`/`us`/`ms`/`s`); defaults to nanoseconds.
	#[serde(default)]
	pub precision: Option<String>,
	/// Resolution token (`seconds`..`years`); defaults to minutes.
	#[serde(default)]
	pub resolution: Option<String>,
	/// Comma-separated reductions (e.g. `min,max,avg`); defaults to `min,max,avg`.
	#[serde(default)]
	pub agg: Option<String>,
}

/// Downsample an `InfluxDB` Line Protocol payload into grid-aligned buckets.
///
/// Handles `POST /api/v1/downsample/ilp`: parses the body as ILP (the wire format
/// TSBS / `InfluxDB` / `QuestDB` speak), projects the chosen numeric field into a
/// series, and reduces it. The payload is the request body (`text/plain`); the
/// field, precision, resolution, and aggregation list are query parameters. The
/// window is the data's own timestamp span. Parsing uses the shared, vendor-
/// neutral `dsp-line-protocol` crate so the server and the benchmark harness
/// accept the exact same dialect.
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] for an unknown precision/resolution/aggregation
/// token, a malformed payload, or fewer than one usable point, and
/// [`ApiError::Internal`] if a timestamp cannot be mapped onto the grid.
pub async fn downsample_ilp(State(metrics): State<SharedMetrics>, axum::extract::Query(params): axum::extract::Query<DownsampleIlpParams>, body: String) -> Result<Json<DownsampleResponse>, ApiError> {
	metrics.record_downsample_request();
	let result = downsample_ilp_inner(&params, &body);
	match &result {
		Ok(response) => metrics.add_downsample_buckets(response.0.buckets as u64),
		Err(_) => metrics.record_downsample_error(),
	}
	result
}

fn downsample_ilp_inner(params: &DownsampleIlpParams, body: &str) -> Result<Json<DownsampleResponse>, ApiError> {
	let precision = crate::interpolate::parse_precision_token(params.precision.as_deref())?;
	let resolution = crate::interpolate::parse_resolution_token(params.resolution.as_deref())?;
	let aggregations = parse_aggregations(params.agg.as_deref())?;

	// `parse_points` returns points sorted ascending by timestamp.
	let points = dsp_line_protocol::parse_points(body, &params.field, precision).map_err(|err| ApiError::BadRequest(err.to_string()))?;
	if points.is_empty() {
		return Err(ApiError::BadRequest(format!("no points carry field `{}` with a timestamp", params.field)));
	}
	let start = points.first().map_or_else(Utc::now, |p| p.timestamp);
	let end = points.last().map_or_else(Utc::now, |p| p.timestamp);

	run_downsample(&points, start, end, resolution, &aggregations)
}

/// Parse a comma-separated aggregation list (default `[min, max, avg]`).
fn parse_aggregations(token: Option<&str>) -> Result<Vec<Aggregation>, ApiError> {
	let Some(token) = token.map(str::trim).filter(|t| !t.is_empty()) else {
		return Ok(DEFAULT_AGGREGATIONS.to_vec());
	};
	token.split(',').map(str::trim).filter(|t| !t.is_empty()).map(parse_aggregation_token).collect()
}

/// Map a single aggregation token to [`Aggregation`].
fn parse_aggregation_token(token: &str) -> Result<Aggregation, ApiError> {
	match token.to_ascii_lowercase().as_str() {
		"min" => Ok(Aggregation::Min),
		"max" => Ok(Aggregation::Max),
		"avg" => Ok(Aggregation::Avg),
		"sum" => Ok(Aggregation::Sum),
		"first" => Ok(Aggregation::First),
		"last" => Ok(Aggregation::Last),
		other => Err(ApiError::BadRequest(format!("unknown aggregation `{other}` (use min/max/avg/sum/first/last)"))),
	}
}

/// Running aggregate state for one bucket, accumulated in `BigDecimal` so sums
/// and averages carry no float drift.
#[derive(Debug, Default)]
struct BucketAcc {
	count: usize,
	sum: BigDecimal,
	min: Option<BigDecimal>,
	max: Option<BigDecimal>,
	first: Option<BigDecimal>,
	last: Option<BigDecimal>,
}

impl BucketAcc {
	/// Fold one value into the bucket. Callers push in ascending-timestamp order,
	/// so `first`/`last` track first/last by time.
	fn push(&mut self, value: BigDecimal) {
		self.count += 1;
		self.sum += &value;
		if self.min.as_ref().is_none_or(|m| &value < m) {
			self.min = Some(value.clone());
		}
		if self.max.as_ref().is_none_or(|m| &value > m) {
			self.max = Some(value.clone());
		}
		if self.first.is_none() {
			self.first = Some(value.clone());
		}
		self.last = Some(value);
	}

	/// Materialize the requested reductions and the grid-aligned bucket start.
	fn finish(self, resolution: Resolution, base: i64, aggregations: &[Aggregation]) -> Result<DownsampleBucket, ApiError> {
		let timestamp = bucket_start(resolution, base).ok_or_else(|| ApiError::Internal("bucket start overflows the representable time range".to_string()))?;
		let count = BigDecimal::from(self.count as u64);
		let mut values: BTreeMap<String, f64> = BTreeMap::new();
		for &agg in aggregations {
			let value = match agg {
				Aggregation::Min => self.min.clone(),
				Aggregation::Max => self.max.clone(),
				Aggregation::Sum => Some(self.sum.clone()),
				Aggregation::Avg => (self.count > 0).then(|| &self.sum / &count),
				Aggregation::First => self.first.clone(),
				Aggregation::Last => self.last.clone(),
			};
			if let Some(value) = value {
				values.insert(agg.as_str().to_string(), value.to_f64().unwrap_or_default());
			}
		}
		Ok(DownsampleBucket { timestamp, count: self.count, aggregations: values })
	}
}

/// Reconstruct a bucket's grid-aligned start timestamp from its resolution index
/// (the inverse of [`Resolution::to_base`]). Returns `None` only if the index
/// scales past the representable `i64`-second range.
fn bucket_start(resolution: Resolution, base: i64) -> Option<DateTime<Utc>> {
	let duration = match resolution {
		Resolution::Nanoseconds => Duration::nanoseconds(base),
		Resolution::Microseconds => Duration::microseconds(base),
		Resolution::Milliseconds => Duration::milliseconds(base),
		Resolution::Seconds => Duration::seconds(base),
		Resolution::Minutes => Duration::seconds(base.checked_mul(SECONDS_IN_MINUTE)?),
		Resolution::Hours => Duration::seconds(base.checked_mul(SECONDS_IN_HOUR)?),
		Resolution::Days => Duration::seconds(base.checked_mul(SECONDS_IN_DAY)?),
		Resolution::Weeks => Duration::seconds(base.checked_mul(SECONDS_IN_WEEK)?),
		Resolution::Months => Duration::seconds(base.checked_mul(SECONDS_IN_MONTH)?),
		Resolution::Years => Duration::seconds(base.checked_mul(SECONDS_IN_YEAR)?),
	};
	DateTime::<Utc>::UNIX_EPOCH.checked_add_signed(duration)
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
		let base = Resolution::Minutes.to_base(&t).unwrap();
		assert_eq!(base, 2);
		let start = bucket_start(Resolution::Minutes, base).unwrap();
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
		assert_eq!(parse_aggregations(Some("min,SUM, last")).unwrap(), vec![Aggregation::Min, Aggregation::Sum, Aggregation::Last]);
		assert!(parse_aggregations(Some("min,bogus")).is_err());
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
		let (status, body) = post_text("/api/v1/downsample/ilp?field=load&agg=median", payload).await;
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
}
