#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]
//! FRE-10a: contract tests for the first half of the FRE-10 surface — probes,
//! the stateless compute endpoints, metrics, and the roles that gate them.
//! (FRE-10b, in W11, takes the storage and operator endpoints plus coverage;
//! the split is the pre-#99 ROADMAP text both lines carried before the
//! checklist slimming dropped the qualifiers.)
//!
//! The per-slice suites pin each handler's internals, and SEC-13's acceptance
//! suite pins the role matrix on one representative route per scope. What
//! neither pins is the composed client contract of this family, which is what
//! a caller actually programs against and what the 1.0 freeze promises:
//!
//! - the exact key set of every JSON answer in the family — a field that
//!   quietly appears or disappears is a contract change, not a refactor;
//! - both spellings of each probe (`/health` and `/healthz`, `/ready` and
//!   `/readyz`) answering identically;
//! - the media types of the compute output formats (JSON, CSV, Arrow IPC,
//!   Parquet) and the Prometheus media type of `/metrics`;
//! - the gate on *every* route in the family, enumerated from the sealed
//!   route registry itself, so a route added to one of these scopes without
//!   contract coverage fails here rather than shipping unexamined: probes
//!   answer anonymously, compute needs the compute scope (reader and up),
//!   and metrics needs the metrics scope (maintainer and up, decision 17).
//!
//! The registry enumeration is also the completeness check: the family sizes
//! are pinned, so extending a family means extending this file in the same
//! change.

use std::{collections::BTreeSet, sync::Arc};

use axum::{
	body::Body, http::{header, HeaderMap, Request, StatusCode}, Router
};
use tower::ServiceExt as _;
use weft_server::{
	app_with_metrics, app_with_state, auth::{format_key_hash, AuthConfig, KeyStore}, bench_runs::run_id, routes::{build_registry, RouteEntry, Scope}, AppState, BenchRuns, SharedMetrics, VERSION
};
use weftdb::SegmentStore;

// --- Principals ---------------------------------------------------------------

const READER_KEY: &str = "fre10a-reader-key";
const WRITER_KEY: &str = "fre10a-writer-key";
const DECLARER_KEY: &str = "fre10a-declarer-key";
const MAINTAINER_KEY: &str = "fre10a-maintainer-key";
const ADMIN_KEY: &str = "fre10a-admin-key";
const UNKNOWN_KEY: &str = "fre10a-no-such-key";

/// A keys document in the SEC-6 format holding one key per role that can
/// hold one (anonymous holds none — it is the absence of a key).
fn full_keys_document() -> String {
	let entries = [("reader-1", "reader", READER_KEY), ("writer-1", "writer", WRITER_KEY), ("declarer-1", "declarer", DECLARER_KEY), ("maintainer-1", "maintainer", MAINTAINER_KEY), ("operator", "admin", ADMIN_KEY)];
	let keys: Vec<String> = entries.iter().map(|(id, role, key)| format!("{{\"id\": \"{id}\", \"role\": \"{role}\", \"key_hash\": \"{}\"}}", format_key_hash(key))).collect();
	format!("{{\"version\": 1, \"keys\": [{}]}}", keys.join(", "))
}

/// A router with a real store and the full keys table in force.
async fn secured_router() -> (tempfile::TempDir, Router) {
	let dir = tempfile::tempdir().expect("tempdir");
	let store = Arc::new(SegmentStore::open(dir.path().join("store")).await.expect("store opens"));
	let keys = KeyStore::parse(&full_keys_document()).expect("keys parse");
	let router = app_with_state(AppState::new().with_store(store).with_auth(AuthConfig::with_keys(keys)));
	(dir, router)
}

// --- Harness ------------------------------------------------------------------

/// Send `request` and return its status, headers and raw body bytes.
async fn send(router: Router, request: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
	let response = router.oneshot(request).await.expect("the router answers every request");
	let status = response.status();
	let headers = response.headers().clone();
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("a body fits").to_vec();
	(status, headers, bytes)
}

/// Send `request` and parse the body as JSON, failing the test if it is not:
/// a contract answer that stops being JSON is a break, not a `Null`.
async fn send_json(router: Router, request: Request<Body>) -> (StatusCode, HeaderMap, serde_json::Value) {
	let (status, headers, bytes) = send(router, request).await;
	let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("{status} answer is not JSON ({error}); body was {:?}", String::from_utf8_lossy(&bytes)));
	(status, headers, body)
}

fn get(uri: &str, key: Option<&str>) -> Request<Body> {
	let mut builder = Request::builder().method("GET").uri(uri);
	if let Some(key) = key {
		builder = builder.header(header::AUTHORIZATION, format!("Bearer {key}"));
	}
	builder.body(Body::empty()).unwrap()
}

fn post_json(uri: &str, key: Option<&str>, body: &serde_json::Value) -> Request<Body> {
	let mut builder = Request::builder().method("POST").uri(uri).header(header::CONTENT_TYPE, "application/json");
	if let Some(key) = key {
		builder = builder.header(header::AUTHORIZATION, format!("Bearer {key}"));
	}
	builder.body(Body::from(serde_json::to_vec(body).unwrap())).unwrap()
}

fn post_text(uri: &str, key: Option<&str>, body: &str) -> Request<Body> {
	let mut builder = Request::builder().method("POST").uri(uri).header(header::CONTENT_TYPE, "text/plain");
	if let Some(key) = key {
		builder = builder.header(header::AUTHORIZATION, format!("Bearer {key}"));
	}
	builder.body(Body::from(body.to_string())).unwrap()
}

/// Send `request` where a successful answer may not be JSON at all (the
/// Prometheus text, the CSV/Arrow/Parquet formats): the body is parsed when
/// it is JSON — every refusal is an envelope, so the `code` assertions still
/// have something to read — and left `Null` when it is not.
async fn send_lenient(router: Router, request: Request<Body>) -> (StatusCode, HeaderMap, serde_json::Value) {
	let (status, headers, bytes) = send(router, request).await;
	(status, headers, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

/// The exact key set of a JSON object body.
fn key_set(body: &serde_json::Value) -> BTreeSet<&str> {
	body.as_object().unwrap_or_else(|| panic!("an object body, got {body}")).keys().map(String::as_str).collect()
}

fn expected<'a>(keys: &[&'a str]) -> BTreeSet<&'a str> {
	BTreeSet::from_iter(keys.iter().copied())
}

// --- Shared request bodies ------------------------------------------------------

/// Four samples a minute apart: enough for the default cubic spline, and on
/// the default minute grid every output point coincides with an input.
fn series_body() -> serde_json::Value {
	serde_json::json!({ "points": [
		{ "timestamp": "1970-01-01T00:00:00+00:00", "value": 1.0 },
		{ "timestamp": "1970-01-01T00:01:00+00:00", "value": 2.0 },
		{ "timestamp": "1970-01-01T00:02:00+00:00", "value": 1.5 },
		{ "timestamp": "1970-01-01T00:03:00+00:00", "value": 3.0 },
	] })
}

/// The point-query body: the instant is the second sample, a knot.
fn point_body() -> serde_json::Value {
	let mut body = series_body();
	body["instant"] = serde_json::json!("1970-01-01T00:01:00+00:00");
	body
}

/// Two line-protocol rows a minute apart (seconds precision).
const ILP_PAYLOAD: &str = "cpu,host=a load=0 1000000000\ncpu,host=a load=60 1000000060\n";

// --- The families, from the registry -------------------------------------------

fn family(scope: Scope) -> Vec<RouteEntry> {
	build_registry().table().entries().iter().filter(|entry| entry.spec.scope == scope).cloned().collect()
}

#[test]
fn the_fre10a_families_are_the_frozen_size() {
	// Pinned, not just non-empty: a route joining one of these scopes joins
	// the frozen contract surface, and this file is where its contract gets
	// pinned — the counts below are the reminder to extend it.
	assert_eq!(family(Scope::Probe).len(), 4, "probes: /health, /healthz, /ready, /readyz");
	assert_eq!(family(Scope::Metrics).len(), 4, "metrics: /metrics, the profile, the two bench-run routes");
	assert_eq!(family(Scope::Compute).len(), 17, "compute: nine interpolate routes, eight downsample routes");
}

/// The concrete path for a registry entry: the one templated path in this
/// family is the bench-run id, filled with a well-formed (unknown) id.
fn concrete_path(entry: &RouteEntry) -> String {
	entry.path.replace("{id}", &"a".repeat(64))
}

/// A request that passes the handler behind the gate, per route: the JSON
/// series for the JSON compute routes, line protocol (with its parameters)
/// for the ILP ones, nothing for the GETs.
fn family_request(entry: &RouteEntry, key: Option<&str>) -> Request<Body> {
	let path = concrete_path(entry);
	if entry.method == axum::http::Method::GET {
		return get(&path, key);
	}
	if path.contains("/ilp") {
		let query = if path.starts_with("/api/v1/interpolate") { "field=load&precision=s&spline=linear&resolution=seconds" } else { "field=load&precision=s&resolution=minutes" };
		return post_text(&format!("{path}?{query}"), key, ILP_PAYLOAD);
	}
	if path.ends_with("/point") {
		return post_json(&path, key, &point_body());
	}
	post_json(&path, key, &series_body())
}

// --- Probes ---------------------------------------------------------------------

#[tokio::test]
async fn health_answers_its_frozen_shape_on_both_spellings() {
	let router = app_with_state(AppState::new());
	let mut bodies = Vec::new();
	for uri in ["/health", "/healthz"] {
		let (status, _headers, body) = send_json(router.clone(), get(uri, None)).await;
		assert_eq!(status, StatusCode::OK, "{uri}");
		assert_eq!(key_set(&body), expected(&["status", "service", "version"]), "{uri}: exactly the probe keys: {body}");
		assert_eq!(body["status"], "ok", "{uri}: {body}");
		assert_eq!(body["service"], "weft-server", "{uri}: {body}");
		assert_eq!(body["version"], VERSION, "{uri}: {body}");
		bodies.push(body);
	}
	assert_eq!(bodies[0], bodies[1], "the alias is the same answer, byte for byte at the JSON level");
}

#[tokio::test]
async fn ready_answers_its_frozen_shape_on_both_spellings() {
	// No store: the stateless API is ready, and the identity fields are
	// present-but-null — a readiness reader must not have to guess which
	// fields exist based on whether a store is configured.
	let router = app_with_state(AppState::new());
	let readiness_keys = ["ready", "service", "version", "segment_store", "poisoned", "restart_required", "poison_reason", "store_uuid", "durability_class", "previous_clean_shutdown", "quarantined", "suspect", "incompatible_store"];
	let mut bodies = Vec::new();
	for uri in ["/ready", "/readyz"] {
		let (status, _headers, body) = send_json(router.clone(), get(uri, None)).await;
		assert_eq!(status, StatusCode::OK, "{uri}");
		assert_eq!(key_set(&body), expected(&readiness_keys), "{uri}: exactly the readiness keys: {body}");
		assert_eq!(body["ready"], true, "{uri}: {body}");
		assert_eq!(body["service"], "weft-server", "{uri}: {body}");
		assert_eq!(body["version"], VERSION, "{uri}: {body}");
		assert_eq!(body["segment_store"], false, "{uri}: {body}");
		assert_eq!(body["poisoned"], false, "{uri}: {body}");
		assert_eq!(body["restart_required"], false, "{uri}: {body}");
		assert_eq!(body["poison_reason"], serde_json::Value::Null, "{uri}: {body}");
		assert_eq!(body["store_uuid"], serde_json::Value::Null, "{uri}: {body}");
		assert_eq!(body["durability_class"], serde_json::Value::Null, "{uri}: {body}");
		assert_eq!(body["previous_clean_shutdown"], serde_json::Value::Null, "{uri}: {body}");
		assert_eq!(body["quarantined"], 0, "{uri}: {body}");
		assert_eq!(body["suspect"], 0, "{uri}: {body}");
		assert_eq!(body["incompatible_store"], false, "{uri}: {body}");
		bodies.push(body);
	}
	assert_eq!(bodies[0], bodies[1], "the alias is the same answer");
}

#[tokio::test]
async fn ready_with_a_store_reports_the_store_it_serves() {
	let dir = tempfile::tempdir().expect("tempdir");
	let store = Arc::new(SegmentStore::open(dir.path()).await.expect("store opens"));
	let store_uuid = store.store_uuid().to_string();
	let router = app_with_state(AppState::new().with_store(store.clone()));
	let (status, _headers, body) = send_json(router, get("/ready", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["ready"], true, "{body}");
	assert_eq!(body["segment_store"], true, "{body}");
	assert_eq!(body["store_uuid"], store_uuid, "the identity is the opened store's: {body}");
	assert_eq!(body["durability_class"], SegmentStore::durability_class(), "{body}");
	assert!(body["previous_clean_shutdown"].is_boolean(), "a configured store answers the question, one way or the other: {body}");
	assert_eq!(body["incompatible_store"], false, "{body}");
	drop(store);
}

// --- Compute: answer shapes ------------------------------------------------------

#[tokio::test]
async fn interpolate_answers_its_frozen_shape() {
	let router = app_with_state(AppState::new());
	let (status, headers, body) = send_json(router, post_json("/api/v1/interpolate", None, &series_body())).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(headers[header::CONTENT_TYPE], "application/json", "{body}");
	assert_eq!(key_set(&body), expected(&["spline", "resolution", "input_points", "output_points", "backend", "gpu_fallback", "points"]), "exactly the interpolation keys: {body}");
	assert_eq!(body["spline"], "Cubic", "the default method, named as the one that ran: {body}");
	assert!(body["resolution"].as_str().is_some_and(|text| !text.is_empty()), "{body}");
	assert_eq!(body["input_points"], 4, "{body}");
	assert_eq!(body["gpu_fallback"], false, "{body}");
	let backend = body["backend"].as_str().expect("the backend is named");
	assert!(["cpu", "parallel", "gpu"].contains(&backend), "the backend that ran, never the configured `auto`: {backend}");
	let points = body["points"].as_array().expect("points is an array");
	assert_eq!(body["output_points"], points.len(), "the count is the array's length: {body}");
	assert_eq!(points.len(), 4, "the default minute grid over a three-minute span: {body}");
	let mut previous: Option<&str> = None;
	for point in points {
		assert_eq!(key_set(point), expected(&["timestamp", "value", "kind"]), "exactly the point keys: {point}");
		assert_eq!(point["kind"], "raw", "a grid point on an input minute is the input: {point}");
		assert!(point["value"].as_f64().is_some(), "{point}");
		let timestamp = point["timestamp"].as_str().expect("timestamps travel as text");
		assert!(previous.is_none_or(|prev| prev < timestamp), "the grid ascends: {points:?}");
		previous = Some(timestamp);
	}
}

#[tokio::test]
async fn interpolate_point_answers_its_frozen_shape() {
	let router = app_with_state(AppState::new());
	let (status, _headers, body) = send_json(router, post_json("/api/v1/interpolate/point", None, &point_body())).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["spline", "instant", "input_points", "value", "kind", "backend", "gpu_fallback"]), "exactly the point-answer keys: {body}");
	assert_eq!(body["instant"], "1970-01-01T00:01:00Z", "the instant evaluated, echoed back in chrono's Zulu spelling: {body}");
	assert_eq!(body["input_points"], 4, "{body}");
	assert_eq!(body["kind"], "raw", "the instant is a knot, so the value is the sample's own: {body}");
	let value = body["value"].as_f64().expect("a numeric value");
	assert!((value - 2.0).abs() < 1e-9, "the knot's value, 2.0: {body}");
}

#[tokio::test]
async fn downsample_answers_its_frozen_shape() {
	let router = app_with_state(AppState::new());
	let (status, _headers, body) = send_json(router, post_json("/api/v1/downsample", None, &series_body())).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	// `fill` and `tags` are absent, not null: both are `skip_serializing_if`
	// fields, and a client distinguishes "not asked" from "asked, none".
	assert_eq!(key_set(&body), expected(&["resolution", "aggregations", "input_points", "buckets", "series"]), "exactly the downsample keys: {body}");
	assert_eq!(body["resolution"], "minutes", "{body}");
	assert_eq!(body["aggregations"], serde_json::json!(["min", "max", "avg"]), "the default reductions, in order: {body}");
	assert_eq!(body["input_points"], 4, "{body}");
	let series = body["series"].as_array().expect("series is an array");
	assert_eq!(body["buckets"], series.len(), "the count is the array's length: {body}");
	assert_eq!(series.len(), 4, "one bucket per minute: {body}");
	let counted: u64 = series.iter().map(|bucket| bucket["count"].as_u64().expect("a count")).sum();
	assert_eq!(counted, 4, "every input lands in exactly one bucket: {body}");
	for bucket in series {
		assert_eq!(key_set(bucket), expected(&["timestamp", "count", "aggregations"]), "exactly the bucket keys: {bucket}");
		assert_eq!(key_set(&bucket["aggregations"]), expected(&["min", "max", "avg"]), "the reductions asked for, keyed by name: {bucket}");
	}
}

#[tokio::test]
async fn compute_output_formats_carry_their_media_types() {
	// The same computation in four wrappings; the media type is how a client
	// knows which parser to reach for, and the file magics are what the
	// parsers themselves check first.
	let router = app_with_state(AppState::new());
	for base in ["/api/v1/interpolate", "/api/v1/downsample"] {
		let (status, headers, bytes) = send(router.clone(), post_json(&format!("{base}/csv"), None, &series_body())).await;
		assert_eq!(status, StatusCode::OK, "{base}/csv");
		assert_eq!(headers[header::CONTENT_TYPE], "text/csv; charset=utf-8", "{base}/csv");
		let text = String::from_utf8(bytes).expect("CSV is text");
		let header_line = text.lines().next().expect("a header line");
		assert!(header_line.starts_with("timestamp,"), "{base}/csv starts with its column header: {header_line}");

		let (status, headers, bytes) = send(router.clone(), post_json(&format!("{base}/arrow"), None, &series_body())).await;
		assert_eq!(status, StatusCode::OK, "{base}/arrow");
		assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.apache.arrow.stream", "{base}/arrow");
		assert_eq!(&bytes[..4], &[0xff, 0xff, 0xff, 0xff], "{base}/arrow opens with the IPC continuation marker");

		let (status, headers, bytes) = send(router.clone(), post_json(&format!("{base}/parquet"), None, &series_body())).await;
		assert_eq!(status, StatusCode::OK, "{base}/parquet");
		assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.apache.parquet", "{base}/parquet");
		assert_eq!(&bytes[..4], b"PAR1", "{base}/parquet opens with the file magic");
		assert_eq!(&bytes[bytes.len() - 4..], b"PAR1", "{base}/parquet closes with the file magic");
	}
}

#[tokio::test]
async fn compute_refusals_carry_the_contract_codes() {
	// The refusals a compute client branches on, across the JSON endpoints:
	// the envelope's `code` is the §4 name, not a one-off string.
	let router = app_with_state(AppState::new());
	let empty = serde_json::json!({ "points": [] });
	for uri in ["/api/v1/interpolate", "/api/v1/downsample"] {
		let (status, _headers, body) = send_json(router.clone(), post_json(uri, None, &empty)).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
		assert_eq!(body["code"], "bad_request", "{uri}: {body}");
	}
	let mut empty_point = point_body();
	empty_point["points"] = serde_json::json!([]);
	let (status, _headers, body) = send_json(router.clone(), post_json("/api/v1/interpolate/point", None, &empty_point)).await;
	assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
	assert_eq!(body["code"], "bad_request", "{body}");

	let mut inverted = series_body();
	inverted["start"] = serde_json::json!("1970-01-01T00:03:00+00:00");
	inverted["end"] = serde_json::json!("1970-01-01T00:00:00+00:00");
	let (status, _headers, body) = send_json(router, post_json("/api/v1/interpolate", None, &inverted)).await;
	assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
	assert_eq!(body["code"], "bad_request", "{body}");
}

// --- Metrics -----------------------------------------------------------------------

#[tokio::test]
async fn metrics_answers_prometheus_text_that_counts_the_compute_calls() {
	let metrics = SharedMetrics::default();
	let router = app_with_metrics(metrics);
	let (status, _headers, _body) = send_json(router.clone(), post_json("/api/v1/interpolate", None, &series_body())).await;
	assert_eq!(status, StatusCode::OK);
	let (status, _headers, _body) = send_json(router.clone(), post_json("/api/v1/downsample", None, &series_body())).await;
	assert_eq!(status, StatusCode::OK);

	let (status, headers, bytes) = send(router, get("/metrics", None)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "text/plain; version=0.0.4", "the Prometheus exposition media type, versioned");
	let text = String::from_utf8(bytes).expect("the exposition is text");
	for name in ["weft_interpolate_requests_total", "weft_downsample_requests_total", "weft_interpolate_output_points_total"] {
		assert!(text.contains(&format!("# HELP {name} ")), "{name} carries its HELP line:\n{text}");
		assert!(text.contains(&format!("# TYPE {name} counter")), "{name} is typed a counter:\n{text}");
	}
	assert!(text.lines().any(|line| line == "weft_interpolate_requests_total 1"), "the interpolation above is counted:\n{text}");
	assert!(text.lines().any(|line| line == "weft_downsample_requests_total 1"), "the downsample above is counted:\n{text}");
	assert!(text.lines().any(|line| line == "weft_interpolate_output_points_total 4"), "and so are its output points:\n{text}");
}

#[tokio::test]
async fn profile_current_answers_its_frozen_shape() {
	let metrics = SharedMetrics::default();
	let router = app_with_metrics(metrics);
	let (status, _headers, body) = send_json(router.clone(), get("/debug/profile/current", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["service", "version", "interpolate", "downsample", "ingest"]), "exactly the profile keys: {body}");
	assert_eq!(body["service"], "weft-server", "{body}");
	assert_eq!(body["version"], VERSION, "{body}");
	assert_eq!(key_set(&body["interpolate"]), expected(&["requests", "errors", "output_points", "output_raw_points", "output_interpolated_points", "output_extrapolated_points", "backend_cpu_requests", "backend_parallel_requests", "backend_gpu_requests", "gpu_fallbacks", "latency_seconds"]), "{body}");
	assert_eq!(key_set(&body["downsample"]), expected(&["requests", "errors", "output_buckets", "latency_seconds"]), "{body}");
	assert_eq!(key_set(&body["ingest"]), expected(&["requests", "errors", "rows_sealed", "segments_sealed", "latency_seconds"]), "{body}");
	for section in ["interpolate", "downsample", "ingest"] {
		let latency = &body[section]["latency_seconds"];
		assert_eq!(key_set(latency), expected(&["count", "sum_seconds", "p50_seconds", "p95_seconds", "p99_seconds"]), "{section}: exactly the latency keys: {body}");
		assert_eq!(body[section]["requests"], 0, "{section}: nothing has run yet: {body}");
		assert_eq!(latency["count"], 0, "{section}: {body}");
		assert_eq!(latency["p50_seconds"], serde_json::Value::Null, "{section}: a quantile with no observations is null, not zero: {body}");
		assert_eq!(latency["p95_seconds"], serde_json::Value::Null, "{section}: {body}");
		assert_eq!(latency["p99_seconds"], serde_json::Value::Null, "{section}: {body}");
	}

	// After a call, the same shape carries the observation.
	let (status, _headers, _body) = send_json(router.clone(), post_json("/api/v1/interpolate", None, &series_body())).await;
	assert_eq!(status, StatusCode::OK);
	let (status, _headers, body) = send_json(router, get("/debug/profile/current", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["interpolate"]["requests"], 1, "{body}");
	assert_eq!(body["interpolate"]["errors"], 0, "{body}");
	assert_eq!(body["interpolate"]["output_points"], 4, "{body}");
	assert_eq!(body["interpolate"]["latency_seconds"]["count"], 1, "{body}");
	assert!(body["interpolate"]["latency_seconds"]["p50_seconds"].as_f64().is_some(), "one observation is enough for a quantile: {body}");
	assert_eq!(body["downsample"]["requests"], 0, "the other sections are untouched: {body}");
}

// --- Metrics: the bench-run surface -------------------------------------------------

const MANIFEST: &[u8] = br#"{"schema_version":24,"generated_at":"2026-11-03T00:00:00Z","claim":"interp-latency"}"#;

#[tokio::test]
async fn bench_runs_serve_manifests_byte_for_byte() {
	let dir = tempfile::tempdir().expect("tempdir");
	std::fs::write(dir.path().join("interp__weftdb.manifest.json"), MANIFEST).expect("the manifest lands");
	let router = app_with_state(AppState::new().with_bench_runs(BenchRuns::new(dir.path())));

	let (status, _headers, body) = send_json(router.clone(), get("/bench/runs", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["runs"]), "exactly the list keys: {body}");
	let runs = body["runs"].as_array().expect("runs is an array");
	assert_eq!(runs.len(), 1, "{body}");
	let listed = &runs[0];
	assert_eq!(key_set(listed), expected(&["id", "path", "schema_version", "generated_at", "claim"]), "exactly the run-summary keys: {listed}");
	assert_eq!(listed["id"], run_id(MANIFEST), "the id is the manifest's SHA-256: {listed}");
	assert_eq!(listed["path"], "interp__weftdb.manifest.json", "{listed}");
	assert_eq!(listed["schema_version"], 24, "{listed}");
	assert_eq!(listed["generated_at"], "2026-11-03T00:00:00Z", "{listed}");
	assert_eq!(listed["claim"], "interp-latency", "{listed}");
	let id = listed["id"].as_str().expect("the id").to_string();

	// The manifest itself, byte for byte, so a client can re-hash the body
	// and get the id back — with the caching headers content addressing
	// earns it.
	let (status, headers, bytes) = send(router.clone(), get(&format!("/bench/runs/{id}"), None)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(bytes, MANIFEST, "the exact bytes");
	assert_eq!(headers[header::CONTENT_TYPE], "application/json");
	assert_eq!(headers[header::ETAG], format!("\"{id}\"").as_str());
	assert!(headers[header::CACHE_CONTROL].to_str().expect("cache control is text").contains("immutable"), "{headers:?}");

	// A well-formed id no manifest hashes to is a 404 with its own code; a
	// malformed one is a 400.
	let (status, _headers, body) = send_json(router.clone(), get(&format!("/bench/runs/{}", "0".repeat(64)), None)).await;
	assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
	assert_eq!(body["code"], "run_not_found", "{body}");
	let (status, _headers, body) = send_json(router, get("/bench/runs/abc", None)).await;
	assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
	assert_eq!(body["code"], "bad_request", "{body}");
}

#[tokio::test]
async fn bench_runs_without_a_directory_say_what_enables_them() {
	let router = app_with_state(AppState::new());
	for uri in ["/bench/runs".to_string(), format!("/bench/runs/{}", "a".repeat(64))] {
		let (status, _headers, body) = send_json(router.clone(), get(&uri, None)).await;
		assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}: {body}");
		assert_eq!(body["code"], "unconfigured", "{uri}: {body}");
		assert_eq!(body["details"]["enabled_by"], "WEFT_BENCH_RUNS_DIR", "{uri}: {body}");
	}
}

// --- Roles, over every route in the families ------------------------------------------

#[tokio::test]
async fn the_gate_holds_for_every_route_in_the_families() {
	let (_dir, router) = secured_router().await;
	let entries: Vec<RouteEntry> = [Scope::Probe, Scope::Metrics, Scope::Compute].iter().flat_map(|scope| family(*scope)).collect();
	assert_eq!(entries.len(), 25, "the whole FRE-10a surface, enumerated from the registry");

	for entry in &entries {
		let label = format!("{} {}", entry.method, entry.path);
		let scope = entry.spec.scope;

		// Anonymous: probes answer (they are the orchestrator's surface);
		// everything else refuses 401 with the §4 code and the Bearer
		// challenge the table pairs with it.
		let (status, headers, body) = send_lenient(router.clone(), family_request(entry, None)).await;
		if scope == Scope::Probe {
			assert_eq!(status, StatusCode::OK, "anonymous {label}: {body}");
		} else {
			assert_eq!(status, StatusCode::UNAUTHORIZED, "anonymous {label}: {body}");
			assert_eq!(body["code"], "unauthenticated", "anonymous {label}: {body}");
			assert!(headers.get(header::WWW_AUTHENTICATE).and_then(|value| value.to_str().ok()).is_some_and(|value| value.starts_with("Bearer")), "anonymous {label}: the 401 carries the challenge");
		}

		// A key the file does not hold authenticates as nothing — even on a
		// probe, where *absent* credentials pass but *refused* ones do not.
		let (status, _headers, body) = send_lenient(router.clone(), family_request(entry, Some(UNKNOWN_KEY))).await;
		assert_eq!(status, StatusCode::UNAUTHORIZED, "unknown key {label}: {body}");
		assert_eq!(body["code"], "unauthenticated", "unknown key {label}: {body}");

		// Reader: probes and compute pass; metrics refuses 403, because the
		// metrics scope enters the grants only at maintainer (decision 17).
		let (status, _headers, body) = send_lenient(router.clone(), family_request(entry, Some(READER_KEY))).await;
		if scope == Scope::Metrics {
			assert_eq!(status, StatusCode::FORBIDDEN, "reader {label}: {body}");
			assert_eq!(body["code"], "forbidden", "reader {label}: {body}");
		} else {
			assert_eq!(status, StatusCode::OK, "reader {label}: {body}");
		}

		// Writer and declarer hold no metrics scope either.
		for key in [WRITER_KEY, DECLARER_KEY] {
			if scope == Scope::Metrics {
				let (status, _headers, body) = send_lenient(router.clone(), family_request(entry, Some(key))).await;
				assert_eq!(status, StatusCode::FORBIDDEN, "{key} {label}: {body}");
			}
		}

		// Maintainer and admin pass every gate in the family. The bench-run
		// routes then refuse 503 `unconfigured` — no run directory is mounted
		// here — which is the handler answering, and proof the gate let the
		// request through.
		for key in [MAINTAINER_KEY, ADMIN_KEY] {
			let (status, _headers, body) = send_lenient(router.clone(), family_request(entry, Some(key))).await;
			if entry.path.starts_with("/bench/") {
				assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{key} {label}: past the gate, refused by the handler: {body}");
				assert_eq!(body["code"], "unconfigured", "{key} {label}: {body}");
			} else {
				assert_eq!(status, StatusCode::OK, "{key} {label}: {body}");
			}
		}
	}
}
