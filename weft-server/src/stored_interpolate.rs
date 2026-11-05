//! **Interpolation on read over stored data**:
//! `GET /api/v1/storage/{aspect}/interpolate?start=&end=&resolution=&spline=&margin=`
//! and its `.csv` / `.arrow` / `.parquet` siblings.
//!
//! The compute endpoints (`POST /api/v1/interpolate…`) interpolate a series the caller sends
//! in the body. This route interpolates an aspect's **persisted** samples in place, so a client
//! reconstructs a large stored range onto a regular grid without first fetching the raw rows:
//! the API surface for interpolation-on-read over stored data, which before this route existed
//! only as the library call [`weftdb::SegmentStore::interpolate_range_f64`] (EVI-10A). The 1.0
//! plan's decision 18 deferred the route past 1.0; the owner brought it forward on 2026-11-04
//! (see `DECISIONS.md`).
//!
//! ## What it reads and what it returns
//!
//! `start` and `end` are inclusive integer epochs **in the aspect's declared time unit**, as on
//! every other stored read. The output grid runs from `start` to `end` at `resolution` (the
//! compute endpoints' tokens, default `minutes`), anchored at `start`, with `spline`
//! (`linear`, `quadratic`, `cubic`; default `cubic`). The knots are the stored samples in
//! `[start − margin, end + margin]`, so a spline near either edge sees the samples just
//! outside the window rather than extrapolating; `margin` (aspect units, at least 0) defaults
//! to the window's own width, `end − start`. Every output point carries its provenance
//! (`raw`, `interpolated`, `extrapolated`), so an edge the margin did not cover is labelled,
//! never passed off as measured.
//!
//! Values come back as `f64`, the interpolation family's wire type: the route runs
//! `interpolate_range_f64`, which skips the per-point `BigDecimal` that the module docs of
//! [`weftdb::interpolate_range`] measure at ~80% of the call. Interpolated and extrapolated
//! values are exactly what splimes computed; a `raw` point is the stored value rounded to the
//! nearest `f64`, as the compute endpoints' JSON already rounds it. Timestamps are RFC 3339 in
//! JSON and CSV and nanosecond epochs in Arrow and Parquet, the reconstructed-series layout the
//! compute endpoints serve, because a grid finer than the aspect's unit has instants that unit
//! cannot hold.
//!
//! ## Bounds
//!
//! Two caps bound a request, and both are refused with `422 result_too_large` and a `limit`:
//!
//! - the **output grid**: at most [`InterpolateConfig::max_output_points`] (the compute
//!   endpoints' cap, `WEFT_MAX_INTERPOLATE_POINTS`), checked before anything is read;
//! - the **knots**: at most [`weftdb::interpolate_range::MAX_KNOTS`] stored rows in the read
//!   window, checked by the library on the rows it read.
//!
//! ## Streaming
//!
//! The JSON and CSV routes stream (P5-5.3): the knots go to splimes as epoch ticks, the grid
//! comes back in chunks of [`STREAM_CHUNK_POINTS`], and each chunk is serialised and sent as
//! soon as it is computed, with at most [`STREAM_DEPTH`] chunks computed ahead, so a large grid
//! is never held whole. The response is committed after the first chunk, so request refusals and
//! engine errors before it keep their status codes; a failure after it cuts the body short. The
//! JSON object carries the same fields as before, with `spline`, `backend` and `output_points`
//! after `points`. Arrow and Parquet are still built whole.
//!
//! The scope is `read` (the route reads stored data, as the range reads do) and the admission
//! class is `query`, as on every stored read.

use axum::{
	extract::{Path, State}, response::{IntoResponse, Response}
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use splimes::Interpolation;
use tracing::Instrument as _;
use weft_physical_type::TimeUnit;
use weftdb::interpolate_range::StoredWindowError;

use crate::{
	error::{self, ApiError, Code, StrictQuery}, interpolate::{self, InterpolateConfig, OutputPoint}, state::AppState
};

/// Query parameters for the stored-range interpolation.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredInterpolateParams {
	/// Inclusive grid start (epoch integer in the aspect's declared unit). Required.
	pub start: i64,
	/// Inclusive grid end (epoch integer in the aspect's declared unit). Required.
	pub end: i64,
	/// Grid resolution token (`nanoseconds`..`years`); defaults to `minutes`.
	#[serde(default)]
	pub resolution: Option<String>,
	/// Spline token (`linear`/`quadratic`/`cubic`); defaults to `cubic`.
	#[serde(default)]
	pub spline: Option<String>,
	/// How far outside `[start, end]` stored samples are read as knots, in the aspect's unit;
	/// at least 0. Defaults to `end − start`.
	#[serde(default)]
	pub margin: Option<i64>,
}

/// The JSON response of `GET /api/v1/storage/{aspect}/interpolate`.
#[derive(Debug, Clone, Serialize)]
pub struct StoredInterpolateResponse {
	/// The aspect read.
	pub aspect: String,
	/// The aspect's declared time unit, the unit of `start`, `end` and `margin`.
	pub time_unit: &'static str,
	/// The grid's first instant, as requested (aspect unit).
	pub start: i64,
	/// The grid's last instant, as requested (aspect unit).
	pub end: i64,
	/// The margin applied (aspect unit): the request's, or the default `end − start`.
	pub margin: i64,
	/// The spline splimes ran (`Display` form): the requested one, or the simpler one it
	/// stepped down to when the window held too few distinct knots.
	pub spline: String,
	/// The grid resolution.
	pub resolution: String,
	/// The backend splimes ran on (`Cpu`, `Parallel`, `Gpu`): `Auto` picks it by grid size
	/// once the startup calibration has measured this machine.
	pub backend: String,
	/// Number of grid points.
	pub output_points: usize,
	/// The reconstructed series on the grid, with provenance.
	pub points: Vec<OutputPoint>,
}

/// The instant of `epoch` in `unit`, or `None` outside chrono's range — the mapping
/// `SegmentStore::interpolate_range` itself uses, so the pre-flight below and the library agree
/// on which bounds are representable.
const fn instant(epoch: i64, unit: TimeUnit) -> Option<DateTime<Utc>> {
	match unit {
		TimeUnit::Seconds => DateTime::<Utc>::from_timestamp(epoch, 0),
		TimeUnit::Millis => DateTime::<Utc>::from_timestamp_millis(epoch),
		TimeUnit::Micros => DateTime::<Utc>::from_timestamp_micros(epoch),
		TimeUnit::Nanos => Some(DateTime::<Utc>::from_timestamp_nanos(epoch)),
	}
}

/// Map a failure of the library call to the error table: the two typed window refusals, a
/// splimes error by variant (as the compute endpoints map it), and anything else as a read.
fn classify(err: &anyhow::Error, config: InterpolateConfig) -> ApiError {
	if let Some(window) = err.downcast_ref::<StoredWindowError>() {
		return match window {
			StoredWindowError::TooManyRows { limit, .. } => ApiError::new(Code::ResultTooLarge, window.to_string()).with_limit(*limit as u64),
			StoredWindowError::NoSample { .. } => ApiError::bad_request(format!("{window}; widen `start`..`end` or `margin`")),
			// `StoredWindowError` is `#[non_exhaustive]`: a refusal added later is still the
			// caller's window, not a server failure.
			_ => ApiError::bad_request(window.to_string()),
		};
	}
	if let Some(engine) = err.downcast_ref::<splimes::Error>() {
		return interpolate::engine_error(engine, config);
	}
	error::classify_read(err)
}

/// Validate the request, read the window's knots and interpolate them: shared by the four
/// output formats, which differ only in how they serialize the result.
///
/// Traced as `interpolate.range` (the stored counterpart of the compute endpoints'
/// `interpolate.engine`), recording the grid size and the backend splimes picked.
/// A validated request: everything [`stored_interpolate_inner`] and [`stored_interpolate_stream`]
/// need before they read.
struct Validated {
	store: std::sync::Arc<weftdb::SegmentStore>,
	unit: TimeUnit,
	start: i64,
	end: i64,
	margin: i64,
	resolution: splimes::Resolution,
	interpolator: splimes::Interpolator,
}

/// Validate the request: tokens, window, margin, the aspect's schema, and the grid cap.
async fn validate(state: AppState, config: InterpolateConfig, aspect: &str, params: &StoredInterpolateParams) -> Result<Validated, ApiError> {
	let store = state.store().cloned().ok_or(ApiError::unconfigured())?;
	drop(state);
	let spline = interpolate::parse_spline_token(params.spline.as_deref())?;
	let resolution = interpolate::parse_resolution_token(params.resolution.as_deref())?;
	let (start, end) = (params.start, params.end);
	if end < start {
		return Err(ApiError::bad_request(format!("`end` ({end}) must not be before `start` ({start})")));
	}
	let margin = params.margin.unwrap_or_else(|| end.saturating_sub(start));
	if margin < 0 {
		return Err(ApiError::bad_request(format!("`margin` ({margin}) must not be negative")));
	}
	// An undeclared aspect is a 404 before any read work, as on the sibling reads.
	let schema = store.schema_for(aspect).await.map_err(|err| error::classify_read(&err))?.ok_or_else(|| ApiError::aspect_not_found(format!("aspect `{aspect}` has no declared schema in the segment store")))?;
	let unit = schema.timestamp_unit;
	let place = |name: &str, epoch: i64| instant(epoch, unit).ok_or_else(|| ApiError::new(Code::ValueNotRepresentable, format!("`{name}` ({epoch}) cannot be placed in time at the aspect's timestamp unit ({})", unit.name())));
	let (from, to) = (place("start", start)?, place("end", end)?);

	let interpolator = config.interpolator(spline, resolution);
	// The grid cap, before the read: splimes sizes the grid and checks it against
	// `max_points` before it looks at the knots, so a run over no knots answers
	// `OutputTooLarge` for an oversized grid and `NoPoints` for any other, allocating nothing.
	match interpolator.run_f64(&[], &[], from, to) {
		Err(splimes::Error::NoPoints) | Ok(_) => {}
		Err(err) => return Err(interpolate::engine_error(&err, config)),
	}
	Ok(Validated { store, unit, start, end, margin, resolution, interpolator })
}

async fn stored_interpolate_inner(state: AppState, config: InterpolateConfig, aspect: &str, params: &StoredInterpolateParams) -> Result<StoredInterpolateResponse, ApiError> {
	let Validated { store, unit, start, end, margin, resolution, interpolator } = validate(state, config, aspect, params).await?;
	let span = tracing::info_span!("interpolate.range", %aspect, start, end, margin, resolution = ?resolution, output_points = tracing::field::Empty, backend = tracing::field::Empty);
	let result = store.interpolate_range_f64(aspect, start, end, margin, interpolator).instrument(span.clone()).await;
	drop(store);
	let series: Interpolation<f64> = result.map_err(|err| classify(&err, config))?;

	let points = tracing::info_span!(parent: &span, "interpolate.serialize", output_points = series.len()).in_scope(|| series.iter().map(|(timestamp, value, kind)| if value.is_finite() { Ok(OutputPoint { timestamp, value: *value, kind }) } else { Err(ApiError::new(Code::NotFinite, format!("the interpolated value at {timestamp} has no finite f64 representation; refusing to return a silently narrowed substitute"))) }).collect::<Result<Vec<_>, ApiError>>())?;
	let backend = format!("{:?}", series.backend());
	span.record("output_points", points.len());
	span.record("backend", backend.as_str());
	Ok(StoredInterpolateResponse { aspect: aspect.to_string(), time_unit: unit.name(), start, end, margin, spline: series.spline().to_string(), resolution: format!("{resolution:?}"), backend, output_points: points.len(), points })
}

/// Grid points per streamed chunk on the JSON and CSV routes: small enough that one chunk's
/// text stays a few MB, large enough that a GPU run is a handful of dispatches per million
/// points. Not tuned by measurement (P5-5.3's measurement stage compares it).
pub const STREAM_CHUNK_POINTS: usize = 64 * 1024;

/// Chunks computed ahead of the one being serialised (the channel depth of
/// [`weftdb::interpolate_range::StoredStream`]).
pub const STREAM_DEPTH: usize = 4;

/// The two text formats the route streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
	Json,
	Csv,
}

/// The instant of grid tick `tick` of `unit` (the stream's tick unit, which is a stored unit or
/// nanoseconds).
fn tick_instant(tick: i64, unit: splimes::TickUnit) -> Option<DateTime<Utc>> {
	match unit.nanos_per_tick() {
		1 => Some(DateTime::<Utc>::from_timestamp_nanos(tick)),
		1_000 => DateTime::<Utc>::from_timestamp_micros(tick),
		1_000_000 => DateTime::<Utc>::from_timestamp_millis(tick),
		1_000_000_000 => DateTime::<Utc>::from_timestamp(tick, 0),
		_ => None,
	}
}

/// Serialise one chunk in `format` (JSON: comma-separated `OutputPoint` objects, with a
/// leading comma unless it is the grid's first chunk; CSV: one `timestamp,value,kind` row per
/// point, the buffered writer's layout). Refuses a non-finite value, as the buffered route did.
fn encode_chunk(chunk: &weftdb::interpolate_range::StoredChunk, unit: splimes::TickUnit, format: Format) -> Result<String, ApiError> {
	use std::fmt::Write as _;
	let mut out = String::with_capacity(chunk.len() * 64);
	for (i, ((&tick, &value), &kind)) in chunk.ticks.iter().zip(&chunk.values).zip(&chunk.kinds).enumerate() {
		let timestamp = tick_instant(tick, unit).ok_or_else(|| ApiError::internal(format!("grid tick {tick} ({unit}) has no instant")))?;
		if !value.is_finite() {
			return Err(ApiError::new(Code::NotFinite, format!("the interpolated value at {timestamp} has no finite f64 representation; refusing to return a silently narrowed substitute")));
		}
		match format {
			Format::Json => {
				if chunk.first + i > 0 {
					out.push(',');
				}
				let point = serde_json::to_string(&OutputPoint { timestamp, value, kind }).map_err(|err| ApiError::internal(format!("serialising a point: {err}")))?;
				out.push_str(&point);
			}
			Format::Csv => {
				let _ = writeln!(out, "{},{},{}", timestamp.to_rfc3339(), value, kind.as_str());
			}
		}
	}
	Ok(out)
}

/// The state of a streamed body.
struct StreamBody {
	/// Text to send before the next chunk (the header and the first chunk, already encoded).
	pending: Option<String>,
	/// The run; `None` once it has finished.
	stream: Option<weftdb::interpolate_range::StoredStream>,
	unit: splimes::TickUnit,
	format: Format,
	span: tracing::Span,
}

/// An I/O error for the body: hyper ends the response without its terminator, so a client sees
/// a truncated body rather than a complete one.
fn abort(message: impl std::fmt::Display) -> std::io::Error {
	std::io::Error::other(message.to_string())
}

impl StreamBody {
	/// The next piece of the body, or `None` at its end.
	async fn step(mut self) -> Option<(Result<axum::body::Bytes, std::io::Error>, Self)> {
		if let Some(text) = self.pending.take() {
			return Some((Ok(text.into()), self));
		}
		let stream = self.stream.as_mut()?;
		if let Some(chunk) = stream.next().await {
			return match encode_chunk(&chunk, self.unit, self.format) {
				Ok(text) => Some((Ok(text.into()), self)),
				Err(err) => {
					// Stop the run: the response cannot change status any more.
					drop(self.stream.take());
					Some((Err(abort(err.message())), self))
				}
			};
		}
		let summary = match self.stream.take()?.finish().await {
			Ok(summary) => summary,
			Err(err) => return Some((Err(abort(format!("{err:#}"))), self)),
		};
		if !summary.is_complete() {
			return Some((Err(abort(format!("the run stopped after {} of {} points", summary.delivered(), summary.len()))), self));
		}
		let backend = format!("{:?}", summary.backend());
		self.span.record("output_points", summary.len());
		self.span.record("backend", backend.as_str());
		match self.format {
			Format::Csv => None,
			Format::Json => {
				let tail = format!("],\"spline\":{},\"backend\":{},\"output_points\":{}}}", serde_json::Value::from(summary.spline().to_string()), serde_json::Value::from(backend), summary.len());
				Some((Ok(tail.into()), self))
			}
		}
	}
}

/// The JSON and CSV routes, streamed (P5-5.3): the knots are read and validated, the first
/// chunk is computed, and only then is the response committed, so every request refusal and
/// every engine error splimes raises before its first point keep their status codes. The rest
/// of the grid is serialised chunk by chunk as splimes computes it, with at most
/// [`STREAM_DEPTH`] chunks in flight, so the server never holds the whole grid. An error after
/// the first chunk (a non-finite value further in) can no longer change the status: the body
/// is cut short instead, which a client sees as an incomplete response.
///
/// The JSON body is the buffered route's object with the same fields; `spline`, `backend` and
/// `output_points`, known only once the run ends, come after `points`.
async fn stored_interpolate_stream(state: AppState, config: InterpolateConfig, aspect: String, params: &StoredInterpolateParams, format: Format) -> Result<Response, ApiError> {
	let Validated { store, unit, start, end, margin, resolution, interpolator } = validate(state, config, &aspect, params).await?;
	let span = tracing::info_span!("interpolate.range", %aspect, start, end, margin, resolution = ?resolution, output_points = tracing::field::Empty, backend = tracing::field::Empty);
	let result = store.interpolate_range_stream(&aspect, start, end, margin, interpolator.stream_chunk_points(STREAM_CHUNK_POINTS), STREAM_DEPTH).instrument(span.clone()).await;
	drop(store);
	let mut stream = result.map_err(|err| classify(&err, config))?;
	let ticks = stream.tick_unit();
	let Some(first) = stream.next().await else {
		// No chunk: the run failed before its first point (a grid is never empty).
		return Err(match stream.finish().await {
			Err(err) => classify(&err, config),
			Ok(summary) => ApiError::internal(format!("the interpolation delivered nothing of {} points", summary.len())),
		});
	};
	let first = encode_chunk(&first, ticks, format)?;
	let (content_type, head) = match format {
		Format::Csv => ("text/csv; charset=utf-8", String::from("timestamp,value,kind\n")),
		Format::Json => {
			let head = serde_json::json!({ "aspect": aspect, "time_unit": unit.name(), "start": start, "end": end, "margin": margin, "resolution": format!("{resolution:?}") }).to_string();
			// `{…}` without its closing brace, then the points array.
			("application/json", format!("{},\"points\":[", &head[..head.len() - 1]))
		}
	};
	let body = StreamBody { pending: Some(head + &first), stream: Some(stream), unit: ticks, format, span };
	let body = axum::body::Body::from_stream(futures::stream::unfold(body, StreamBody::step));
	Ok(([(axum::http::header::CONTENT_TYPE, content_type)], body).into_response())
}

/// Handle `GET /api/v1/storage/{aspect}/interpolate?start&end&resolution&spline&margin`:
/// reconstruct the aspect's stored samples onto a regular grid (see the [module docs](self)).
///
/// # Errors
///
/// `503 unconfigured` without a store; `404 aspect_not_found` for an undeclared aspect;
/// `400 bad_request` for an unknown token, an inverted window, a negative margin, or a window
/// with no stored sample; `422 value_not_representable` for a bound outside the representable
/// range; `422 result_too_large` (with `limit`) for a grid over the output cap or a read window
/// over [`MAX_KNOTS`](weftdb::interpolate_range::MAX_KNOTS) stored rows; `400 not_finite` for a computed value with no finite `f64`;
/// and `500 internal` on a read or engine failure.
pub async fn storage_interpolate(State(state): State<AppState>, State(config): State<InterpolateConfig>, Path(aspect): Path<String>, StrictQuery(params): StrictQuery<StoredInterpolateParams>) -> Result<Response, ApiError> {
	stored_interpolate_stream(state, config, aspect, &params, Format::Json).await
}

/// Handle `GET /api/v1/storage/{aspect}/interpolate.csv`: [`storage_interpolate`] as a
/// `timestamp,value,kind` CSV document, the compute endpoints' CSV layout.
///
/// # Errors
///
/// As [`storage_interpolate`].
pub async fn storage_interpolate_csv(State(state): State<AppState>, State(config): State<InterpolateConfig>, Path(aspect): Path<String>, StrictQuery(params): StrictQuery<StoredInterpolateParams>) -> Result<Response, ApiError> {
	stored_interpolate_stream(state, config, aspect, &params, Format::Csv).await
}

/// Handle `GET /api/v1/storage/{aspect}/interpolate.arrow`: [`storage_interpolate`] as an
/// Arrow IPC stream (`timestamp` nanoseconds, `value` `Float64`, `kind`).
///
/// # Errors
///
/// As [`storage_interpolate`], plus `500 internal` on an Arrow-encoding failure.
pub async fn storage_interpolate_arrow(State(state): State<AppState>, State(config): State<InterpolateConfig>, Path(aspect): Path<String>, StrictQuery(params): StrictQuery<StoredInterpolateParams>) -> Result<Response, ApiError> {
	let response = stored_interpolate_inner(state, config, &aspect, &params).await?;
	interpolate::points_to_arrow(&response.points)
}

/// Handle `GET /api/v1/storage/{aspect}/interpolate.parquet`: [`storage_interpolate`] as a
/// Parquet file, the same three columns as the Arrow sibling.
///
/// # Errors
///
/// As [`storage_interpolate`], plus `500 internal` on a Parquet-encoding failure.
pub async fn storage_interpolate_parquet(State(state): State<AppState>, State(config): State<InterpolateConfig>, Path(aspect): Path<String>, StrictQuery(params): StrictQuery<StoredInterpolateParams>) -> Result<Response, ApiError> {
	let response = stored_interpolate_inner(state, config, &aspect, &params).await?;
	interpolate::points_to_parquet(&response.points)
}

#[cfg(test)]
mod tests {
	use std::{num::NonZeroUsize, sync::Arc};

	use axum::{
		body::Body, http::{header, Request, StatusCode}
	};
	use bigdecimal::BigDecimal;
	use tempfile::TempDir;
	use tower::ServiceExt;
	use weft_physical_type::{AspectSchema, PhysicalType};
	use weftdb::SegmentStore;

	use super::*;
	use crate::{
		app_with_state, auth::{format_key_hash, AuthConfig, KeyStore}, test_support::get_json
	};

	/// A store with aspect `line` (`ScaledI64` scale 2, seconds) holding v = 2·t at every tenth
	/// second from 0 to 190, sealed as two segments that meet at t = 100 — the library tests'
	/// corpus, so the HTTP answers can be checked against the same arithmetic.
	async fn line_store(dir: &TempDir) -> Arc<SegmentStore> {
		let store = SegmentStore::open(dir.path()).await.expect("opens store");
		store.declare("line", &AspectSchema::new(PhysicalType::ScaledI64 { scale: 2 }, BigDecimal::from(0), TimeUnit::Seconds)).await.expect("declares");
		store.declare("empty", &AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds)).await.expect("declares");
		for half in [0_i64, 1] {
			let ts: Vec<i64> = (0..10).map(|i| (half * 10 + i) * 10).collect();
			let vs: Vec<BigDecimal> = ts.iter().map(|t| BigDecimal::from(2 * t)).collect();
			store.seal_declared("line", &ts, &vs).await.expect("seals");
		}
		Arc::new(store)
	}

	async fn router(dir: &TempDir) -> axum::Router {
		app_with_state(AppState::new().with_store(line_store(dir).await))
	}

	async fn get_bytes(router: axum::Router, uri: &str) -> (StatusCode, String, Vec<u8>) {
		let response = router.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap()).await.unwrap();
		let status = response.status();
		let content_type = response.headers().get(header::CONTENT_TYPE).map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
		(status, content_type, axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec())
	}

	#[tokio::test]
	async fn json_reconstructs_across_segments_with_provenance() {
		let dir = TempDir::new().unwrap();
		let (status, body) = get_json(router(&dir).await, "/api/v1/storage/line/interpolate?start=95&end=105&resolution=seconds&spline=linear&margin=20").await;
		assert_eq!(status, StatusCode::OK, "{body}");
		assert_eq!(body["aspect"], "line");
		assert_eq!(body["time_unit"], "seconds");
		assert_eq!((body["start"].as_i64(), body["end"].as_i64(), body["margin"].as_i64()), (Some(95), Some(105), Some(20)));
		assert_eq!(body["spline"], "Linear");
		assert_eq!(body["resolution"], "Seconds");
		assert!(body["backend"].is_string(), "{body}");
		assert_eq!(body["output_points"], 11);
		let points = body["points"].as_array().unwrap();
		assert_eq!(points.len(), 11);
		for (i, point) in points.iter().enumerate() {
			let t = 95 + i64::try_from(i).unwrap();
			#[allow(clippy::cast_precision_loss)]
			let expected = (2 * t) as f64;
			assert_eq!(point["value"].as_f64(), Some(expected), "v = 2t at {t}");
			assert_eq!(point["kind"], if t == 100 { "raw" } else { "interpolated" }, "at {t}");
			assert_eq!(point["timestamp"].as_str().unwrap().parse::<DateTime<Utc>>().unwrap(), DateTime::from_timestamp(t, 0).unwrap());
		}
	}

	#[tokio::test]
	async fn the_default_margin_is_the_window_width_and_covers_the_edges() {
		let dir = TempDir::new().unwrap();
		// [95, 105] alone holds only the knot at 100; the default margin of 10 reads 90 and 110.
		let (status, body) = get_json(router(&dir).await, "/api/v1/storage/line/interpolate?start=95&end=105&resolution=seconds&spline=linear").await;
		assert_eq!(status, StatusCode::OK, "{body}");
		assert_eq!(body["margin"], 10);
		assert_eq!(body["points"][0]["kind"], "interpolated");
		assert_eq!(body["points"][0]["value"].as_f64(), Some(190.0));
		// An explicit zero margin leaves the edges to extrapolation, and says so.
		let (status, body) = get_json(router(&dir).await, "/api/v1/storage/line/interpolate?start=95&end=105&resolution=seconds&spline=linear&margin=0").await;
		assert_eq!(status, StatusCode::OK, "{body}");
		assert_eq!(body["points"][0]["kind"], "extrapolated");
	}

	#[tokio::test]
	async fn the_defaults_are_cubic_on_a_minute_grid() {
		let dir = TempDir::new().unwrap();
		let (status, body) = get_json(router(&dir).await, "/api/v1/storage/line/interpolate?start=0&end=180").await;
		assert_eq!(status, StatusCode::OK, "{body}");
		assert_eq!(body["spline"], "Cubic");
		assert_eq!(body["resolution"], "Minutes");
		assert_eq!(body["output_points"], 4, "0, 60, 120, 180");
		// Every grid point is a stored sample here, so every one is raw, at its stored value.
		let values: Vec<f64> = body["points"].as_array().unwrap().iter().map(|p| p["value"].as_f64().unwrap()).collect();
		assert_eq!(values, vec![0.0, 120.0, 240.0, 360.0]);
	}

	/// The four formats serve the same series: CSV text, and the Arrow and Parquet
	/// reconstructed-series batch (nanosecond timestamps, `f64` values, provenance).
	#[tokio::test]
	async fn every_format_serves_the_same_series() {
		let dir = TempDir::new().unwrap();
		let router = router(&dir).await;
		let query = "?start=95&end=105&resolution=seconds&spline=linear&margin=20";
		let (status, json) = get_json(router.clone(), &format!("/api/v1/storage/line/interpolate{query}")).await;
		assert_eq!(status, StatusCode::OK);
		let expected_values: Vec<f64> = json["points"].as_array().unwrap().iter().map(|p| p["value"].as_f64().unwrap()).collect();
		let expected_kinds: Vec<String> = json["points"].as_array().unwrap().iter().map(|p| p["kind"].as_str().unwrap().to_string()).collect();
		let expected_nanos: Vec<i64> = (95..=105).map(|t: i64| t * 1_000_000_000).collect();

		let (status, content_type, bytes) = get_bytes(router.clone(), &format!("/api/v1/storage/line/interpolate.csv{query}")).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(content_type, "text/csv; charset=utf-8");
		let text = String::from_utf8(bytes).unwrap();
		let mut lines = text.lines();
		assert_eq!(lines.next(), Some("timestamp,value,kind"));
		let rows: Vec<Vec<String>> = lines.map(|l| l.split(',').map(str::to_string).collect()).collect();
		assert_eq!(rows.len(), 11);
		for ((row, value), kind) in rows.iter().zip(&expected_values).zip(&expected_kinds) {
			assert_eq!(row[1].parse::<f64>().unwrap(), *value);
			assert_eq!(&row[2], kind);
		}
		assert_eq!(rows[0][0].parse::<DateTime<Utc>>().unwrap(), DateTime::from_timestamp(95, 0).unwrap());

		for (suffix, content) in [("arrow", "application/vnd.apache.arrow.stream"), ("parquet", "application/vnd.apache.parquet")] {
			let (status, content_type, bytes) = get_bytes(router.clone(), &format!("/api/v1/storage/line/interpolate.{suffix}{query}")).await;
			assert_eq!(status, StatusCode::OK, "{suffix}");
			assert_eq!(content_type, content, "{suffix}");
			let batches = if suffix == "arrow" { weft_arrow::read_ipc_stream(&bytes).expect("arrow ipc") } else { weft_arrow::read_parquet(&bytes).expect("parquet") };
			assert_eq!(batches.len(), 1, "{suffix}");
			let (unit, timestamps, values, kinds) = weft_arrow::reconstructed_series_from_record_batch(&batches[0]).expect("reconstructed series");
			assert_eq!(unit, TimeUnit::Nanos, "{suffix}");
			assert_eq!(timestamps, expected_nanos, "{suffix}");
			assert_eq!(values, expected_values, "{suffix}");
			assert_eq!(kinds, expected_kinds, "{suffix}");
		}
	}

	/// Every refusal names its code, on every format (the error comes before serialization).
	#[tokio::test]
	async fn bad_requests_are_refused_with_the_table_codes() {
		let dir = TempDir::new().unwrap();
		let router = router(&dir).await;
		for suffix in ["", ".csv", ".arrow", ".parquet"] {
			for (query, status, code) in [
				("?start=10&end=0", StatusCode::BAD_REQUEST, "bad_request"),
				("?start=0&end=10&margin=-1", StatusCode::BAD_REQUEST, "bad_request"),
				("?start=0&end=10&spline=akima", StatusCode::BAD_REQUEST, "bad_request"),
				("?start=0&end=10&resolution=fortnights", StatusCode::BAD_REQUEST, "bad_request"),
				("?start=0", StatusCode::BAD_REQUEST, "bad_request"),
				("?start=0&end=10&agg=avg", StatusCode::BAD_REQUEST, "bad_request"),
				("?start=0&end=ten", StatusCode::BAD_REQUEST, "bad_request"),
				// No stored sample in [1000 − 0, 2000 + 0].
				("?start=1000&end=2000&margin=0", StatusCode::BAD_REQUEST, "bad_request"),
				// A day of nanoseconds is 86.4 trillion grid points.
				("?start=0&end=86400&resolution=nanoseconds", StatusCode::UNPROCESSABLE_ENTITY, "result_too_large"),
				// chrono's range ends in the year 262143; seconds beyond it have no instant.
				("?start=0&end=9223372036854775807&margin=0", StatusCode::UNPROCESSABLE_ENTITY, "value_not_representable"),
			] {
				let uri = format!("/api/v1/storage/line/interpolate{suffix}{query}");
				let (got, body) = get_json(router.clone(), &uri).await;
				assert_eq!((got, body["code"].as_str()), (status, Some(code)), "{uri}: {body}");
			}
			let (got, body) = get_json(router.clone(), &format!("/api/v1/storage/nope/interpolate{suffix}?start=0&end=10")).await;
			assert_eq!((got, body["code"].as_str()), (StatusCode::NOT_FOUND, Some("aspect_not_found")), "{body}");
			let (got, body) = get_json(router.clone(), &format!("/api/v1/storage/empty/interpolate{suffix}?start=0&end=10")).await;
			assert_eq!((got, body["code"].as_str()), (StatusCode::BAD_REQUEST, Some("bad_request")), "{body}");
			assert!(body["error"].as_str().unwrap().contains("no stored sample"), "{body}");
		}
	}

	#[tokio::test]
	async fn the_grid_cap_is_the_operators_and_names_its_limit() {
		let dir = TempDir::new().unwrap();
		let store = line_store(&dir).await;
		let config = InterpolateConfig::new(NonZeroUsize::new(10).unwrap());
		let router = app_with_state(AppState::new().with_store(store).with_interpolate_config(config));
		let (status, body) = get_json(router.clone(), "/api/v1/storage/line/interpolate?start=0&end=9&resolution=seconds").await;
		assert_eq!(status, StatusCode::OK, "ten points is at the cap: {body}");
		let (status, body) = get_json(router, "/api/v1/storage/line/interpolate?start=0&end=10&resolution=seconds").await;
		assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
		assert_eq!(body["code"], "result_too_large");
		assert_eq!(body["limit"], 10);
	}

	#[test]
	fn the_knot_cap_is_result_too_large_with_its_limit() {
		let err = anyhow::Error::from(StoredWindowError::TooManyRows { aspect: "line".to_string(), lo: 0, hi: 10, rows: weftdb::interpolate_range::MAX_KNOTS + 1, limit: weftdb::interpolate_range::MAX_KNOTS });
		let api = classify(&err.context("interpolating"), InterpolateConfig::DEFAULT);
		assert_eq!(api.code(), Code::ResultTooLarge);
		assert!(api.message().contains("narrow the window"), "{}", api.message());
		let response = api.into_response();
		assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
	}

	#[tokio::test]
	async fn without_a_store_the_route_is_unconfigured() {
		let (status, body) = get_json(app_with_state(AppState::new()), "/api/v1/storage/line/interpolate?start=0&end=10").await;
		assert_eq!((status, body["code"].as_str()), (StatusCode::SERVICE_UNAVAILABLE, Some("unconfigured")));
	}

	#[tokio::test]
	async fn only_get_is_served() {
		let dir = TempDir::new().unwrap();
		let response = router(&dir).await.oneshot(Request::builder().method("POST").uri("/api/v1/storage/line/interpolate?start=0&end=10").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
		assert_eq!(response.headers().get(header::ALLOW).unwrap(), "GET");
	}

	/// The route is a stored read: the `read` scope, so every key role from `reader` up
	/// reaches it, and no key is refused with `401`.
	#[tokio::test]
	async fn auth_gates_it_on_the_read_scope() {
		const READER: &str = "weft_reader_token_for_stored_interpolate";
		const WRITER: &str = "weft_writer_token_for_stored_interpolate";
		let keys = format!("{{\"version\": 1, \"keys\": [{{\"id\": \"r\", \"role\": \"reader\", \"key_hash\": \"{}\"}}, {{\"id\": \"w\", \"role\": \"writer\", \"key_hash\": \"{}\"}}]}}", format_key_hash(READER), format_key_hash(WRITER));
		let dir = TempDir::new().unwrap();
		let router = app_with_state(AppState::new().with_store(line_store(&dir).await).with_auth(AuthConfig::with_keys(KeyStore::parse(&keys).unwrap())));
		for suffix in ["", ".csv", ".arrow", ".parquet"] {
			let uri = format!("/api/v1/storage/line/interpolate{suffix}?start=0&end=180");
			let anonymous = router.clone().oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap()).await.unwrap();
			assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED, "{uri}");
			for token in [READER, WRITER] {
				let response = router.clone().oneshot(Request::builder().uri(&uri).header(header::AUTHORIZATION, format!("Bearer {token}")).body(Body::empty()).unwrap()).await.unwrap();
				assert_eq!(response.status(), StatusCode::OK, "{uri} as {token}");
			}
		}
	}

	#[test]
	fn the_routes_are_registered_as_stored_reads() {
		let table = crate::routes::build_registry().table();
		for (path, operation) in [("/api/v1/storage/{aspect}/interpolate", "storage_interpolate_get"), ("/api/v1/storage/{aspect}/interpolate.csv", "storage_interpolate_csv_get"), ("/api/v1/storage/{aspect}/interpolate.arrow", "storage_interpolate_arrow_get"), ("/api/v1/storage/{aspect}/interpolate.parquet", "storage_interpolate_parquet_get")] {
			let entry = table.by_operation(operation).unwrap_or_else(|| panic!("{operation} is registered"));
			assert_eq!((entry.method.as_str(), entry.path), ("GET", path));
			let range = table.by_operation("storage_downsample_get").unwrap();
			assert_eq!(entry.spec.scope, range.spec.scope, "{operation}: the stored reads' scope");
			assert_eq!(entry.spec.admission_class, range.spec.admission_class, "{operation}: the stored reads' admission class");
			assert_eq!(entry.spec.audit, range.spec.audit, "{operation}: audited as a stored read");
			assert_eq!(entry.spec.tier, range.spec.tier, "{operation}: the stored reads' tier");
		}
	}

	/// A grid of several streamed chunks (190,001 millisecond points over a seconds aspect, so
	/// nanosecond ticks) serves, on JSON and CSV, exactly the library's `f64` result.
	#[tokio::test]
	async fn a_multi_chunk_stream_is_the_library_result() {
		let dir = TempDir::new().unwrap();
		let store = line_store(&dir).await;
		let expected = store.interpolate_range_f64("line", 0, 190, 0, InterpolateConfig::DEFAULT.interpolator(splimes::Spline::Cubic, splimes::Resolution::Milliseconds)).await.unwrap();
		assert!(expected.len() > 2 * STREAM_CHUNK_POINTS, "several chunks");
		let router = app_with_state(AppState::new().with_store(store));
		let query = "?start=0&end=190&resolution=milliseconds&spline=cubic&margin=0";
		let (status, body) = get_json(router.clone(), &format!("/api/v1/storage/line/interpolate{query}")).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body["output_points"].as_u64(), Some(expected.len() as u64));
		assert_eq!(body["spline"], expected.spline().to_string());
		let points = body["points"].as_array().unwrap();
		assert_eq!(points.len(), expected.len());
		for (point, (timestamp, value, kind)) in points.iter().zip(expected.iter()) {
			assert_eq!(point["timestamp"].as_str().unwrap().parse::<DateTime<Utc>>().unwrap(), timestamp);
			assert_eq!(point["value"].as_f64().unwrap().to_bits(), value.to_bits());
			assert_eq!(point["kind"], kind.as_str());
		}
		let (status, content_type, bytes) = get_bytes(router, &format!("/api/v1/storage/line/interpolate.csv{query}")).await;
		assert_eq!((status, content_type.as_str()), (StatusCode::OK, "text/csv; charset=utf-8"));
		let text = String::from_utf8(bytes).unwrap();
		let mut lines = text.lines();
		assert_eq!(lines.next(), Some("timestamp,value,kind"));
		let mut rows = 0;
		for (line, (timestamp, value, kind)) in lines.zip(expected.iter()) {
			assert_eq!(line, format!("{},{},{}", timestamp.to_rfc3339(), value, kind.as_str()));
			rows += 1;
		}
		assert_eq!(rows, expected.len());
	}
}
