//! Grafana: the JSON-datasource protocol over stored aspects (Phase 2, experimental).
//!
//! Grafana's community JSON datasource (`simpod-json-datasource`) speaks three calls
//! against a base URL, and these routes answer them with the base URL set to
//! `http://<server>/api/v1/grafana`:
//!
//! - `GET  /api/v1/grafana` — "Save & test": `200` when the server answers.
//! - `POST /api/v1/grafana/metrics` — the metric picker: one entry per declared aspect, with
//!   the reduction (`agg`) offered as a payload option.
//! - `POST /api/v1/grafana/query` — one time series per target: the aspect's stored
//!   downsample over the panel's time range, bucketed at the panel's interval (widened so a
//!   panel never gets more than `maxDataPoints` points), as `[[value, epoch_ms], …]`.
//!
//! The reduction is the stored downsample's, computed over the stored segments (no rows
//! are shipped to Grafana to be reduced there). Grafana plots `f64`, so a bucket's value is
//! the reduction's `f64`, exactly as the JSON downsample route answers it; the lossless
//! reads stay on the storage routes. A bucket with no value for the reduction (an empty one)
//! is left out, which Grafana draws as a gap.
//!
//! The routes need the `read` scope; give Grafana a reader key as a custom
//! `Authorization: Bearer <key>` header on the datasource.

use axum::{body::Bytes, extract::State, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
	error::{self, ApiError}, state::AppState, storage::StorageDownsampleParams
};

/// The reduction a target gets when its payload names none.
pub const DEFAULT_AGG: &str = "avg";

/// The reductions the metric picker offers. Any single token the stored downsample takes
/// works (aliases such as `median` included), plus `count`: the rows in each bucket.
const OFFERED_AGGS: &[&str] = &["avg", "min", "max", "sum", "count", "first", "last", "p50", "p95", "p99", "twa"];

/// The most targets one query may carry: each is a schema lookup and a stored downsample,
/// and a panel has a handful.
pub const MAX_TARGETS_PER_QUERY: usize = 64;

/// What a target's `agg` asks for: one stored-downsample reduction (by its canonical name),
/// or the bucket's row count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reduction {
	Aggregation(weft_reduce::Aggregation),
	Count,
}

impl Reduction {
	fn parse(token: Option<&str>) -> Result<Self, ApiError> {
		let token = token.map(str::trim).filter(|token| !token.is_empty()).unwrap_or(DEFAULT_AGG);
		if token == "count" {
			return Ok(Self::Count);
		}
		weft_reduce::Aggregation::from_token(token).map(Self::Aggregation).ok_or_else(|| ApiError::bad_request(format!("unknown reduction `{token}` in a target's `agg`: one of {}", OFFERED_AGGS.join(", "))))
	}
}

/// A bucket width as a token the stored downsample reads: the coarsest unit that divides
/// it exactly (so `600000` ms is `10m`, which keeps the reduction on its per-segment fast
/// path), and otherwise a count small enough for the width's 32-bit count, rounded up so
/// the panel's point budget still holds.
#[must_use]
pub fn width_token(ms: u64) -> String {
	const UNITS: [(u64, &str); 6] = [(604_800_000, "w"), (86_400_000, "d"), (3_600_000, "h"), (60_000, "m"), (1_000, "s"), (1, "ms")];
	let ms = ms.max(1);
	if let Some((per, unit)) = UNITS.iter().find(|(per, _)| ms % per == 0 && ms / per <= u64::from(u32::MAX)) {
		return format!("{}{unit}", ms / per);
	}
	UNITS.iter()
		.rev()
		.find_map(|(per, unit)| {
			let count = ms.div_ceil(*per);
			(count <= u64::from(u32::MAX)).then(|| format!("{count}{unit}"))
		})
		.unwrap_or_else(|| format!("{}w", u32::MAX))
}

/// A query's widest grid: one panel never asks for more buckets than this, whatever its
/// `maxDataPoints` says.
pub const MAX_POINTS_PER_TARGET: u64 = 11_000;

fn parse<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, ApiError> {
	if body.is_empty() {
		return serde_json::from_slice(b"{}").map_err(|error| ApiError::bad_request(format!("the request body is not the Grafana JSON shape: {error}")));
	}
	serde_json::from_slice(body).map_err(|error| ApiError::bad_request(format!("the request body is not the Grafana JSON shape: {error}")))
}

/// `GET /api/v1/grafana`: the datasource's connection test.
pub async fn test() -> Json<Value> {
	Json(json!({"status": "ok", "datasource": "weftdb"}))
}

/// One entry of the metric picker.
#[derive(Debug, Serialize)]
pub struct MetricOption {
	/// What the picker shows.
	pub label: String,
	/// What a target carries.
	pub value: String,
	/// The per-target options.
	pub payloads: Vec<Value>,
}

/// `POST /api/v1/grafana/metrics`: every declared aspect, filtered by the picker's typed
/// prefix (`metric`) when it sends one.
///
/// # Errors
///
/// `503 unconfigured` without a store; a body that is not JSON is `400`.
pub async fn metrics(State(state): State<AppState>, body: Bytes) -> Result<Json<Vec<MetricOption>>, ApiError> {
	#[derive(Deserialize, Default)]
	struct Find {
		#[serde(default)]
		metric: Option<String>,
	}
	let find: Find = parse(&body)?;
	let store = state.store().cloned().ok_or_else(ApiError::unconfigured)?;
	drop(state);
	let names = store.list_declared_aspects().await.map_err(|error| error::classify_read(&error))?;
	let needle = find.metric.unwrap_or_default().to_lowercase();
	let payload = json!({"label": "Reduction", "name": "agg", "type": "select", "placeholder": DEFAULT_AGG, "options": OFFERED_AGGS.iter().map(|agg| json!({"label": agg, "value": agg})).collect::<Vec<_>>()});
	Ok(Json(names.into_iter().filter(|name| name.to_lowercase().contains(&needle)).map(|name| MetricOption { label: name.clone(), value: name, payloads: vec![payload.clone()] }).collect()))
}

/// The time range of a query.
#[derive(Debug, Deserialize)]
pub struct Range {
	/// The first instant.
	pub from: DateTime<Utc>,
	/// The last instant.
	pub to: DateTime<Utc>,
}

/// One target of a query.
#[derive(Debug, Deserialize)]
pub struct Target {
	/// The aspect.
	#[serde(default)]
	pub target: Option<String>,
	/// The panel's id for this target.
	#[serde(default, rename = "refId")]
	pub ref_id: Option<String>,
	/// A hidden target is not queried.
	#[serde(default)]
	pub hide: bool,
	/// The per-target options (`agg`).
	#[serde(default)]
	pub payload: Option<Value>,
}

/// `POST /api/v1/grafana/query`'s body: the fields this server reads; Grafana sends more.
#[derive(Debug, Deserialize)]
pub struct Query {
	/// The panel's time range.
	pub range: Range,
	/// The panel's interval in milliseconds.
	#[serde(default, rename = "intervalMs")]
	pub interval_ms: Option<u64>,
	/// The most points the panel can draw.
	#[serde(default, rename = "maxDataPoints")]
	pub max_data_points: Option<u64>,
	/// The series to draw.
	#[serde(default)]
	pub targets: Vec<Target>,
}

/// One answered series.
#[derive(Debug, Serialize)]
pub struct Series {
	/// The aspect it reads.
	pub target: String,
	/// The panel's id for it.
	#[serde(rename = "refId", skip_serializing_if = "Option::is_none")]
	pub ref_id: Option<String>,
	/// `[value, epoch_ms]` pairs, oldest first.
	pub datapoints: Vec<(f64, i64)>,
}

/// The bucket width for a panel: its interval, widened until the range fits in its point
/// budget, and never under a millisecond.
#[must_use]
pub fn bucket_ms(from: DateTime<Utc>, to: DateTime<Utc>, interval_ms: Option<u64>, max_data_points: Option<u64>) -> u64 {
	let span = u64::try_from((to - from).num_milliseconds()).unwrap_or(0);
	let budget = max_data_points.unwrap_or(MAX_POINTS_PER_TARGET).clamp(1, MAX_POINTS_PER_TARGET);
	let floor = span.div_ceil(budget);
	interval_ms.unwrap_or(0).max(floor).max(1)
}

/// An instant as an epoch integer in a unit `nanos_per_unit` nanoseconds long, rounded
/// toward the past (so a window's bounds include the instants they name).
fn to_unit(instant: DateTime<Utc>, nanos_per_unit: i64) -> i64 {
	let nanos = i128::from(instant.timestamp()) * 1_000_000_000 + i128::from(instant.timestamp_subsec_nanos());
	i64::try_from(nanos.div_euclid(i128::from(nanos_per_unit.max(1)))).unwrap_or(if nanos < 0 { i64::MIN } else { i64::MAX })
}

/// `POST /api/v1/grafana/query`: each visible target's stored downsample.
///
/// # Errors
///
/// `400` for a body that is not the query shape, a range that ends before it starts, or a
/// reduction the stored downsample does not know; `404 aspect_not_found` for a target that
/// names no declared aspect; `503 unconfigured` without a store.
pub async fn query(State(state): State<AppState>, body: Bytes) -> Result<Json<Vec<Series>>, ApiError> {
	let query: Query = parse(&body)?;
	if query.range.to < query.range.from {
		return Err(ApiError::bad_request("the query's `range.to` is before its `range.from`"));
	}
	let visible: Vec<Target> = query.targets.into_iter().filter(|target| !target.hide && target.target.as_deref().is_some_and(|name| !name.is_empty())).collect();
	if visible.len() > MAX_TARGETS_PER_QUERY {
		return Err(ApiError::bad_request(format!("a query may carry at most {MAX_TARGETS_PER_QUERY} visible targets; this one has {}", visible.len())));
	}
	let store = state.store().cloned().ok_or_else(ApiError::unconfigured)?;
	let width = width_token(bucket_ms(query.range.from, query.range.to, query.interval_ms, query.max_data_points));
	let mut answered = Vec::new();
	for target in visible {
		let Some(aspect) = target.target else { continue };
		let reduction = Reduction::parse(target.payload.as_ref().and_then(|payload| payload.get("agg")).and_then(Value::as_str))?;
		// `count` rides on any reduction's buckets; `avg` is the cheapest to ask for.
		let agg = match reduction {
			Reduction::Aggregation(aggregation) => aggregation.as_str(),
			Reduction::Count => DEFAULT_AGG,
		};
		let schema = store.schema_for(&aspect).await.map_err(|error| error::classify_read(&error))?.ok_or_else(|| ApiError::aspect_not_found(format!("aspect `{aspect}` has no declared schema in the segment store")))?;
		let per = schema.timestamp_unit.nanos_per_unit();
		let params = StorageDownsampleParams { start: Some(to_unit(query.range.from, per)), end: Some(to_unit(query.range.to, per)), resolution: Some(width.clone()), agg: Some(agg.to_owned()), fill: None };
		let response = crate::storage::storage_downsample_inner(state.clone(), &aspect, &params, &weftdb::SeriesSelector::untagged()).await?;
		let datapoints = response
			.series
			.iter()
			.filter_map(|bucket| {
				let value = match reduction {
					Reduction::Count => Some(crate::num::usize_to_f64(bucket.count)),
					Reduction::Aggregation(_) => bucket.aggregations.get(agg).copied(),
				};
				value.map(|value| (value, bucket.timestamp.timestamp_millis()))
			})
			.collect();
		answered.push(Series { target: aspect, ref_id: target.ref_id, datapoints });
	}
	Ok(Json(answered))
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use axum::{
		body::Body, http::{Request, StatusCode}
	};
	use tower::ServiceExt as _;

	use super::*;

	fn at(text: &str) -> DateTime<Utc> {
		DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
	}

	#[test]
	fn a_panel_gets_its_interval_but_never_more_points_than_it_draws() {
		let (from, to) = (at("2026-01-01T00:00:00Z"), at("2026-01-01T01:00:00Z"));
		assert_eq!(bucket_ms(from, to, Some(60_000), Some(1_000)), 60_000, "the interval already fits");
		assert_eq!(bucket_ms(from, to, Some(1_000), Some(100)), 36_000, "widened to 100 points over the hour");
		assert_eq!(bucket_ms(from, to, None, None), 328, "no hints: the server's own cap");
		assert_eq!(bucket_ms(from, from, Some(0), Some(0)), 1, "never under a millisecond");
	}

	#[test]
	fn a_width_is_spelled_in_the_coarsest_exact_unit_that_fits() {
		assert_eq!(width_token(600_000), "10m");
		assert_eq!(width_token(86_400_000), "1d");
		assert_eq!(width_token(1_500), "1500ms");
		assert_eq!(width_token(0), "1ms");
		assert_eq!(width_token(5_000_000_000), "5000000s", "past u32 milliseconds, still a whole count");
		assert_eq!(width_token(5_000_000_001), "5000001s", "not whole: rounded up, so the point budget holds");
		assert!(weft_reduce::BucketWidth::parse(&width_token(u64::MAX)).is_some(), "{}", width_token(u64::MAX));
	}

	#[test]
	fn a_reduction_is_one_token_or_count() {
		assert_eq!(Reduction::parse(None).unwrap(), Reduction::Aggregation(weft_reduce::Aggregation::Avg));
		assert_eq!(Reduction::parse(Some(" median ")).unwrap(), Reduction::Aggregation(weft_reduce::Aggregation::P50), "an alias resolves to its canonical name");
		assert_eq!(Reduction::parse(Some("count")).unwrap(), Reduction::Count);
		assert_eq!(Reduction::parse(Some("avg,max")).unwrap_err().code(), crate::error::Code::BadRequest, "one reduction per target");
		for offered in OFFERED_AGGS {
			assert!(Reduction::parse(Some(offered)).is_ok(), "the picker offers {offered}");
		}
	}

	#[test]
	fn an_instant_lands_in_the_aspects_unit_rounded_down() {
		let instant = at("1970-01-01T00:00:01.999Z");
		assert_eq!(to_unit(instant, 1_000_000_000), 1);
		assert_eq!(to_unit(instant, 1_000_000), 1_999);
		assert_eq!(to_unit(at("1969-12-31T23:59:59.5Z"), 1_000_000_000), -1, "toward the past, not toward zero");
	}

	async fn call(router: &axum::Router, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
		let request = Request::builder().method(method).uri(uri).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap();
		let response = router.clone().oneshot(request).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		(status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
	}

	#[tokio::test]
	async fn grafana_charts_a_stored_aspect() {
		let dir = tempfile::tempdir().unwrap();
		let store = Arc::new(weftdb::SegmentStore::open(dir.path()).await.unwrap());
		let router = crate::app_with_state(AppState::new().with_store(store));
		let (status, _) = call(&router, "POST", "/api/v1/storage/aspects", json!({"name": "temp", "physical_type": "f64", "timestamp_unit": "seconds"})).await;
		assert!(status.is_success(), "{status}");
		// Two readings a minute for ten minutes: 0, 1, 2, … at 0 s, 30 s, 60 s, ….
		let points: Vec<Value> = (0..20).map(|i| json!({"timestamp": i * 30, "value": i.to_string()})).collect();
		let (status, _) = call(&router, "POST", "/api/v1/storage/temp/points", json!({"points": points})).await;
		assert!(status.is_success(), "{status}");

		let (status, test) = call(&router, "GET", "/api/v1/grafana", Value::Null).await;
		assert_eq!((status, &test["status"]), (StatusCode::OK, &json!("ok")));
		let (status, metrics) = call(&router, "POST", "/api/v1/grafana/metrics", json!({"metric": "TE"})).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(metrics[0]["value"], "temp");
		assert_eq!(metrics[0]["payloads"][0]["name"], "agg");
		let (_, none) = call(&router, "POST", "/api/v1/grafana/metrics", json!({"metric": "humidity"})).await;
		assert_eq!(none, json!([]));

		let body = json!({"range": {"from": "1970-01-01T00:00:00Z", "to": "1970-01-01T00:09:59Z"}, "intervalMs": 60_000, "maxDataPoints": 1_000, "panelId": 2, "scopedVars": {}, "targets": [{"target": "temp", "refId": "A", "payload": {"agg": "max"}}, {"target": "temp", "refId": "B"}, {"target": "temp", "refId": "C", "hide": true}]});
		let (status, series) = call(&router, "POST", "/api/v1/grafana/query", body).await;
		assert_eq!(status, StatusCode::OK, "{series}");
		assert_eq!(series.as_array().unwrap().len(), 2, "the hidden target is not queried");
		assert_eq!(series[0]["refId"], "A");
		let maxima = series[0]["datapoints"].as_array().unwrap();
		assert_eq!(maxima.len(), 10, "one bucket a minute");
		assert_eq!(maxima[0], json!([1.0, 0]), "max of 0 and 1, at the minute's start in epoch ms");
		assert_eq!(maxima[9], json!([19.0, 540_000]));
		assert_eq!(series[1]["datapoints"][0], json!([0.5, 0]), "the default reduction is avg");
		let counted = json!({"range": {"from": "1970-01-01T00:00:00Z", "to": "1970-01-01T00:09:59Z"}, "intervalMs": 60_000, "targets": [{"target": "temp", "payload": {"agg": "count"}}, {"target": "temp", "payload": {"agg": "median"}}]});
		let (status, counted) = call(&router, "POST", "/api/v1/grafana/query", counted).await;
		assert_eq!(status, StatusCode::OK, "{counted}");
		assert_eq!(counted[0]["datapoints"][0], json!([2.0, 0]), "two readings a minute");
		assert_eq!(counted[1]["datapoints"].as_array().unwrap().len(), 10, "an alias still finds its values");

		let (status, missing) = call(&router, "POST", "/api/v1/grafana/query", json!({"range": {"from": "1970-01-01T00:00:00Z", "to": "1970-01-01T00:01:00Z"}, "targets": [{"target": "nope"}]})).await;
		assert_eq!((status, &missing["code"]), (StatusCode::NOT_FOUND, &json!("aspect_not_found")));
		let (status, bad) = call(&router, "POST", "/api/v1/grafana/query", json!({"range": {"from": "1970-01-01T00:01:00Z", "to": "1970-01-01T00:00:00Z"}, "targets": []})).await;
		assert_eq!((status, &bad["code"]), (StatusCode::BAD_REQUEST, &json!("bad_request")));
	}
}
