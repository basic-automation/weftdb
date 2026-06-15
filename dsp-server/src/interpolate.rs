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
	http::StatusCode, response::{IntoResponse, Response}, Json
};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use splimes::{Point, Resolution, Spline};

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
/// # Errors
///
/// Returns [`ApiError::BadRequest`] for an empty point set, a non-finite value,
/// or an inverted range, and [`ApiError::Internal`] if the engine fails.
pub async fn interpolate(Json(request): Json<InterpolateRequest>) -> Result<Json<InterpolateResponse>, ApiError> {
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
	if end < start {
		return Err(ApiError::bad_request("`end` must not be before `start`"));
	}

	let spline: Spline = request.spline.into();
	let resolution: Resolution = request.resolution.into();
	let spline_label = spline.to_string();
	let resolution_label = format!("{resolution:?}");

	let output = splimes::auto_interpolate(&mut points, start, end, resolution, spline).await.map_err(|err| ApiError::internal(err.to_string()))?;

	let points: Vec<OutputPoint> = output.iter().map(|p| OutputPoint { timestamp: p.timestamp, value: p.value.to_f64().unwrap_or_default() }).collect();

	Ok(Json(InterpolateResponse { spline: spline_label, resolution: resolution_label, output_points: points.len(), input_points, points }))
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
}
