#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]
//! FRE-10a: contract tests for the first half of the FRE-10 surface — the
//! probe endpoints, the stateless compute endpoints (interpolate, downsample),
//! the metrics and profile endpoints, plus the role matrix on every route in
//! the three families and the coverage cross-check that partitions the 74-route
//! registry between the two FRE-10 files.
//!
//! The per-family suites pin each handler's response shape and media type.
//! The gate suite enumerates every route in the families against all seven
//! principals (anonymous, unknown, reader, writer, declarer, maintainer,
//! admin) and proves that the cumulative grant table holds at the scope level:
//! probes pass at anonymous, compute passes at reader, metrics passes at
//! maintainer — and a key that is refused the scope is never allowed to reach
//! the handler. The coverage test pins the partition: 26 routes in FRE-10a
//! (4+5+17), 48 in FRE-10b, 74 total.

use std::{collections::BTreeSet, sync::Arc};

use axum::{
	body::Body, http::{header, HeaderMap, Method, Request, StatusCode}, Router
};
use tower::ServiceExt as _;
use weft_server::{
	app_with_state, auth::{format_key_hash, AuthConfig, KeyStore}, routes::{build_registry, RouteEntry, Scope}, AppState, VERSION
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

/// Send `request` where a successful answer may not be JSON at all (the
/// CSV/Arrow/Parquet formats): the body is parsed when it is JSON — every
/// refusal is an envelope, so the `code` assertions still have something to
/// read — and left `Null` when it is not.
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

/// A POST that carries no body.
fn post_empty(uri: &str, key: Option<&str>) -> Request<Body> {
	let mut builder = Request::builder().method("POST").uri(uri);
	if let Some(key) = key {
		builder = builder.header(header::AUTHORIZATION, format!("Bearer {key}"));
	}
	builder.body(Body::empty()).unwrap()
}

// --- Request bodies -----------------------------------------------------------

/// Three points a minute apart: a valid interpolate / downsample body.
fn interpolate_body() -> serde_json::Value {
	serde_json::json!({ "points": [
		{ "timestamp": "1970-01-01T00:00:00+00:00", "value": 1.0 },
		{ "timestamp": "1970-01-01T00:01:00+00:00", "value": 2.0 },
		{ "timestamp": "1970-01-01T00:02:00+00:00", "value": 1.5 },
	] })
}

/// For the downsample JSON endpoint — the same shape as `interpolate_body`
/// but kept symmetric so the two endpoints can diverge their seeds if needed.
fn downsample_body() -> serde_json::Value {
	serde_json::json!({ "points": [
		{ "timestamp": "1970-01-01T00:00:00+00:00", "value": 1.0 },
		{ "timestamp": "1970-01-01T00:01:00+00:00", "value": 2.0 },
		{ "timestamp": "1970-01-01T00:02:00+00:00", "value": 1.5 },
	] })
}

/// For the interpolate-point endpoint: three points and a grid instant.
fn interpolate_point_body() -> serde_json::Value {
	serde_json::json!({ "points": [
		{ "timestamp": "1970-01-01T00:00:00+00:00", "value": 1.0 },
		{ "timestamp": "1970-01-01T00:02:00+00:00", "value": 1.5 },
	], "instant": "1970-01-01T00:01:00+00:00" })
}

/// Two line-protocol rows for the ILP compute endpoints.
const ILP_BODY: &str = "cpu,host=a load=5 0\ncpu,host=a load=6 60\n";

/// A router with a real store and the full keys table in force.
/// The store is empty (no declared aspect, no rows) — the metrics
/// routes that need a store (headroom) answer with the real disk
/// numbers; the bench-run routes answer `503 unconfigured` because
/// no `WEFT_BENCH_RUNS_DIR` is set.
async fn secured_router() -> (tempfile::TempDir, Router) {
	let dir = tempfile::tempdir().expect("tempdir");
	let store = Arc::new(SegmentStore::open(dir.path().join("store")).await.expect("store opens"));
	let keys = KeyStore::parse(&full_keys_document()).expect("keys parse");
	let router = app_with_state(AppState::new().with_store(store).with_auth(AuthConfig::with_keys(keys)));
	(dir, router)
}

// --- The families, from the registry -------------------------------------------

fn family(scope: Scope) -> Vec<RouteEntry> {
	build_registry().table().entries().iter().filter(|entry| entry.spec.scope == scope).cloned().collect()
}

#[test]
fn the_fre10a_families_are_the_frozen_size() {
	// Pinned, not just non-empty: a route joining one of these scopes joins
	// the frozen contract surface, and this file is where its contract gets
	// pinned — the counts below are the reminder to extend it.
	assert_eq!(family(Scope::Probe).len(), 4, "probe: /health, /healthz, /ready, /readyz");
	assert_eq!(family(Scope::Metrics).len(), 5, "metrics: /metrics, /debug/profile/current, /api/v1/storage/headroom, /bench/runs, /bench/runs/{{id}}");
	assert_eq!(family(Scope::Compute).len(), 17, "compute: 9 interpolate + 8 downsample");
}

#[test]
fn the_contract_coverage_spans_the_whole_registry() {
	// "Plus coverage": the two FRE-10 halves partition the registry — every
	// scope belongs to exactly one half, and the halves sum to the whole
	// surface. A route added anywhere moves one of these counts, and the
	// file that owns its scope is where its contract gets pinned.
	let table = build_registry().table();
	assert_eq!(table.entries().len(), 74, "the registry's whole surface — was 73 before storage_restore_into (#739)");
	let halves: [(Scope, &str); 8] = [(Scope::Probe, "fre10a"), (Scope::Metrics, "fre10a"), (Scope::Compute, "fre10a"), (Scope::Read, "fre10b"), (Scope::Ingest, "fre10b"), (Scope::Declare, "fre10b"), (Scope::Maintain, "fre10b"), (Scope::Backup, "fre10b")];
	let mut seen = BTreeSet::new();
	for (scope, _) in &halves {
		assert!(seen.insert(scope), "each scope is claimed by exactly one half");
	}
	assert_eq!(seen.len(), Scope::ALL.len(), "every scope in the frozen table is claimed");
	let sizes: [(Scope, usize); 8] = [(Scope::Probe, 4), (Scope::Metrics, 5), (Scope::Compute, 17), (Scope::Read, 31), (Scope::Ingest, 4), (Scope::Declare, 1), (Scope::Maintain, 9), (Scope::Backup, 3)]; // storage_restore_into (#739) joins Backup, was 2
	let mut total = 0;
	for (scope, size) in sizes {
		assert_eq!(family(scope).len(), size, "scope {} holds {size} routes", scope.as_str());
		total += size;
	}
	assert_eq!(total, table.entries().len(), "the halves sum to the registry");
	let first_half: usize = halves.iter().filter(|(_, half)| *half == "fre10a").map(|(scope, _)| family(*scope).len()).sum();
	let second_half: usize = halves.iter().filter(|(_, half)| *half == "fre10b").map(|(scope, _)| family(*scope).len()).sum();
	assert_eq!((first_half, second_half), (26, 48), "fre10a 4+5+17, fre10b 31+4+1+9+3");
}

// --- Gate: every route in the families ----------------------------------------

/// What a key that holds the route's scope gets back: the handler's answer
/// when the gate passes. The bench-run routes answer `503 unconfigured`
/// because no `WEFT_BENCH_RUNS_DIR` is set — that is the gate passing, not a
/// refusal. Headroom is not in that list: these routers carry a real store, so
/// it answers `200` with the host's free-space numbers.
fn expected_pass_status(entry: &RouteEntry) -> StatusCode {
	match entry.spec.operation_id {
		"list_bench_runs" | "get_bench_run" => StatusCode::SERVICE_UNAVAILABLE,
		_ => StatusCode::OK,
	}
}

/// A request that passes the gate behind the handler, per route: the
/// stateless compute endpoints carry a valid body; probes and metrics
/// routes need no body beyond the GET/POST method itself.
/// The templated paths in these families, with a parameter value that reaches
/// the handler. A bench run id is 64 lower-case hex digits; its value is
/// irrelevant here, because the bench routes answer `503` before it is looked
/// up.
fn concrete_path(path: &'static str) -> &'static str {
	match path {
		"/bench/runs/{id}" => "/bench/runs/0000000000000000000000000000000000000000000000000000000000000000",
		other => other,
	}
}

/// True for the eight line-protocol compute routes: their body is `text/plain`
/// line protocol and their URI carries a `field` and a `precision`.
fn is_ilp_route(path: &str) -> bool {
	path.split('/').any(|segment| segment == "ilp")
}

fn family_request(entry: &RouteEntry, key: Option<&str>) -> Request<Body> {
	let path = concrete_path(entry.path);
	if entry.method == Method::GET {
		return get(path, key);
	}
	if is_ilp_route(path) {
		return post_text(&format!("{path}?field=load&precision=s"), key, ILP_BODY);
	}
	// The remaining POSTs: the stateless compute routes carry a JSON body,
	// every other route in these families is a bodyless POST.
	match path {
		"/api/v1/interpolate" | "/api/v1/interpolate/csv" | "/api/v1/interpolate/arrow" | "/api/v1/interpolate/parquet" => post_json(path, key, &interpolate_body()),
		"/api/v1/interpolate/point" => post_json(path, key, &interpolate_point_body()),
		"/api/v1/downsample" | "/api/v1/downsample/csv" | "/api/v1/downsample/arrow" | "/api/v1/downsample/parquet" => post_json(path, key, &downsample_body()),
		_ => post_empty(path, key),
	}
}

/// The gate for one route, over all seven principals: anonymous and unknown
/// keys are refused `401` everywhere except the probe scope, which anonymous
/// holds (SEC-3); each role passes exactly the scopes its grant holds, and a
/// key that passes reaches the handler — never a silent `401`/`403` past it.
async fn check_route_gate(router: Router, entry: &RouteEntry) {
	let label = format!("{} {}", entry.method, entry.path);
	let scope = entry.spec.scope;
	let roles = [(None, "anonymous"), (Some(UNKNOWN_KEY), "unknown"), (Some(READER_KEY), "reader"), (Some(WRITER_KEY), "writer"), (Some(DECLARER_KEY), "declarer"), (Some(MAINTAINER_KEY), "maintainer"), (Some(ADMIN_KEY), "admin")];
	for (key, name) in roles {
		let role = match name {
			"reader" => Some(weft_server::Role::Reader),
			"writer" => Some(weft_server::Role::Writer),
			"declarer" => Some(weft_server::Role::Declarer),
			"maintainer" => Some(weft_server::Role::Maintainer),
			"admin" => Some(weft_server::Role::Admin),
			_ => None,
		};
		let (status, headers, body) = send_lenient(router.clone(), family_request(entry, key)).await;
		match role {
			None => {
				// Anonymous holds the probe scope; unknown never does.
				if key.is_none() && scope == Scope::Probe {
					assert_eq!(status, expected_pass_status(entry), "anonymous {label}: {body}");
				} else {
					assert_eq!(status, StatusCode::UNAUTHORIZED, "{name} {label}: {body}");
					assert_eq!(body["code"], "unauthenticated", "{name} {label}: {body}");
					if key.is_none() {
						assert!(headers.get(header::WWW_AUTHENTICATE).and_then(|value| value.to_str().ok()).is_some_and(|value| value.starts_with("Bearer")), "{name} {label}: the 401 carries the challenge");
					}
				}
			}
			Some(role) if !role.allows(scope) => {
				assert_eq!(status, StatusCode::FORBIDDEN, "{name} {label}: {body}");
				assert_eq!(body["code"], "forbidden", "{name} {label}: {body}");
			}
			Some(_) => {
				assert_eq!(status, expected_pass_status(entry), "{name} {label}: past the gate, the handler answers: {body}");
			}
		}
	}
}

#[tokio::test]
async fn the_gate_holds_for_every_route_in_the_families() {
	let (_dir, router) = secured_router().await;
	let entries: Vec<RouteEntry> = [Scope::Probe, Scope::Metrics, Scope::Compute].iter().flat_map(|scope| family(*scope)).collect();
	assert_eq!(entries.len(), 26, "the whole FRE-10a surface, enumerated from the registry");
	for entry in &entries {
		check_route_gate(router.clone(), entry).await;
	}
}

// --- Probes: answer shapes ----------------------------------------------------

#[tokio::test]
async fn health_answers_its_frozen_shape() {
	let router = secured_router().await.1;
	// /health is the canonical liveness probe; /healthz is its alias.
	for uri in ["/health", "/healthz"] {
		let (status, _headers, body) = send_json(router.clone(), get(uri, None)).await;
		assert_eq!(status, StatusCode::OK, "{uri}: {body}");
		assert_eq!(key_set(&body), expected(&["status", "service", "version"]), "{uri}: exactly the health keys: {body}");
		assert_eq!(body["status"], "ok", "{uri}: {body}");
		assert_eq!(body["service"], "weft-server", "{uri}: {body}");
		assert_eq!(body["version"], VERSION, "{uri}: {body}");
	}
}

#[tokio::test]
async fn ready_answers_its_frozen_shape() {
	let (_dir, router) = secured_router().await;
	// /ready is the canonical readiness probe; /readyz is its alias.
	for uri in ["/ready", "/readyz"] {
		let (status, _headers, body) = send_json(router.clone(), get(uri, None)).await;
		assert_eq!(status, StatusCode::OK, "{uri}: {body}");
		// With a store: segment_store, store_uuid, durability_class,
		// previous_clean_shutdown, quarantined, suspect, incompatible_store,
		// and the flattened disk-guard and write-poison fields.
		assert_eq!(key_set(&body), expected(&["ready", "service", "version", "segment_store", "poisoned", "restart_required", "poison_reason", "store_uuid", "durability_class", "previous_clean_shutdown", "quarantined", "suspect", "incompatible_store", "disk_refusing",]), "{uri}: exactly the ready keys: {body}");
		assert_eq!(body["service"], "weft-server", "{uri}: {body}");
		assert_eq!(body["version"], VERSION, "{uri}: {body}");
		assert_eq!(body["ready"], true, "{uri}: {body}");
		assert_eq!(body["segment_store"], true, "{uri}: {body}");
		assert_eq!(body["poisoned"], false, "{uri}: {body}");
		assert_eq!(body["restart_required"], false, "{uri}: {body}");
		assert_eq!(body["quarantined"], 0, "{uri}: {body}");
		assert_eq!(body["suspect"], 0, "{uri}: {body}");
		assert_eq!(body["incompatible_store"], false, "{uri}: {body}");
		assert_eq!(body["disk_refusing"], false, "{uri}: {body}");
	}
}

// --- Metrics: answer shapes ---------------------------------------------------

#[tokio::test]
async fn metrics_answers_its_frozen_shape() {
	let router = secured_router().await.1;
	let (status, headers, bytes) = send(router.clone(), get("/metrics", Some(MAINTAINER_KEY))).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "text/plain; version=0.0.4");
	let text = String::from_utf8(bytes).expect("metrics is text");
	// The known counter names (v0.0.4 of the exposition format).
	assert!(text.contains("# HELP weft_interpolate_requests_total"), "the metrics text names the interpolate counter: first 200 bytes:\n{}", &text[..text.len().min(200)]);
	assert!(text.contains("# HELP weft_downsample_requests_total"), "the metrics text names the downsample counter");
	assert!(text.contains("# TYPE weft_interpolate_requests_total counter"), "the type line is present");
	assert!(text.contains("weft_interpolate_requests_total 0"), "the initial counter value is 0: {}...", &text[..text.len().min(200)]);
}

#[tokio::test]
async fn profile_current_answers_its_frozen_shape() {
	let router = secured_router().await.1;
	let (status, _headers, body) = send_json(router.clone(), get("/debug/profile/current", Some(MAINTAINER_KEY))).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	// The profile report: service identity, then a section per profiled family.
	// Three families carry a profile today; the maintenance and backup lanes
	// count their work in ledger rows, not in this report.
	assert_eq!(body["service"], "weft-server", "{body}");
	assert_eq!(body["version"], VERSION, "{body}");
	let sections = ["interpolate", "downsample", "ingest"];
	for section in &sections {
		assert!(body[section].is_object(), "the {section} section is present: {body}");
	}
}

// --- Compute (stateless): answer shapes ---------------------------------------

#[tokio::test]
async fn interpolate_json_answers_its_frozen_shape() {
	let router = secured_router().await.1;
	let (status, _headers, body) = send_json(router.clone(), post_json("/api/v1/interpolate", Some(READER_KEY), &interpolate_body())).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	// InterpolateResponse: spline, resolution, input_points, output_points,
	// backend, gpu_fallback, points[].
	assert_eq!(key_set(&body), expected(&["spline", "resolution", "input_points", "output_points", "backend", "gpu_fallback", "points"]), "exactly the interpolate keys: {body}");
	// The tokens are the request tokens echoed back: `spline` is the
	// `SplineSpec` serde token (lower-case) and `resolution` the
	// `ResolutionSpec` token, so the response round-trips into a request.
	assert_eq!(body["spline"], "cubic", "the default method: {body}");
	assert_eq!(body["resolution"], "minutes", "the default resolution: {body}");
	assert_eq!(body["input_points"], 3, "{body}");
	assert_eq!(body["output_points"], 3, "three aligned input minutes → three grid points: {body}");
	assert_eq!(body["gpu_fallback"], false, "no GPU configured: {body}");
	assert!(body["backend"].as_str().is_some_and(|b| matches!(b, "cpu" | "gpu")), "the backend that ran: {body}");
	let points = body["points"].as_array().expect("points is an array");
	assert_eq!(points.len(), 3, "{body}");
	for point in points {
		assert_eq!(key_set(point), expected(&["timestamp", "value", "kind"]), "exactly the point keys: {point}");
		assert!(point["kind"].as_str().is_some_and(|k| matches!(k, "raw" | "interpolated" | "extrapolated")), "every point has a known kind: {point}");
	}
}

#[tokio::test]
async fn downsample_json_answers_its_frozen_shape() {
	let router = secured_router().await.1;
	let (status, _headers, body) = send_json(router.clone(), post_json("/api/v1/downsample", Some(READER_KEY), &downsample_body())).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	// DownsampleResponse: resolution, aggregations, input_points, buckets,
	// series[], optional fill, optional tags (absent on the compute endpoint).
	assert_eq!(key_set(&body), expected(&["resolution", "aggregations", "input_points", "buckets", "series"]), "exactly the downsample keys: {body}");
	// The bucket timestamps sit on minute boundaries, so one bucket per input.
	assert_eq!(body["resolution"], "minutes", "the default resolution: {body}");
	assert_eq!(body["aggregations"], serde_json::json!(["min", "max", "avg"]), "the default reductions, in order: {body}");
	assert_eq!(body["input_points"], 3, "{body}");
	assert_eq!(body["buckets"], 3, "one bucket per minute: {body}");
	let series = body["series"].as_array().expect("series is an array");
	assert_eq!(series.len(), 3, "{body}");
	for bucket in series {
		assert_eq!(key_set(bucket), expected(&["timestamp", "count", "aggregations"]), "exactly the bucket keys: {bucket}");
		assert_eq!(bucket["count"], 1, "{bucket}");
	}
}

#[tokio::test]
async fn interpolate_point_answers_its_frozen_shape() {
	let router = secured_router().await.1;
	let (status, _headers, body) = send_json(router.clone(), post_json("/api/v1/interpolate/point", Some(READER_KEY), &interpolate_point_body())).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	// PointResponse: spline, instant, input_points, value, kind, backend, gpu_fallback.
	assert_eq!(key_set(&body), expected(&["spline", "instant", "input_points", "value", "kind", "backend", "gpu_fallback"]), "exactly the point-response keys: {body}");
	assert_eq!(body["input_points"], 2, "{body}");
	assert_eq!(body["gpu_fallback"], false, "{body}");
	assert!(body["value"].as_f64().is_some_and(f64::is_finite), "the interpolated value is finite: {body}");
	assert!(body["kind"].as_str().is_some_and(|k| matches!(k, "raw" | "interpolated" | "extrapolated")), "every point has a known kind: {body}");
}

// --- Compute format routes: media types ---------------------------------------

#[tokio::test]
async fn compute_formats_carry_their_media_types() {
	let router = secured_router().await.1;
	let json_body = interpolate_body();

	// Interpolate JSON: application/json
	let (status, headers, _) = send(router.clone(), post_json("/api/v1/interpolate", Some(READER_KEY), &json_body)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "application/json");

	// Interpolate CSV: text/csv
	let (status, headers, bytes) = send(router.clone(), post_json("/api/v1/interpolate/csv", Some(READER_KEY), &json_body)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "text/csv; charset=utf-8");
	let text = String::from_utf8(bytes).expect("CSV is text");
	assert!(text.lines().next().is_some_and(|line| line.starts_with("timestamp,")), "the CSV starts with its column header: {text}");

	// Interpolate Arrow: application/vnd.apache.arrow.stream
	let (status, headers, bytes) = send(router.clone(), post_json("/api/v1/interpolate/arrow", Some(READER_KEY), &json_body)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.apache.arrow.stream");
	assert_eq!(&bytes[..4], &[0xff, 0xff, 0xff, 0xff], "the IPC continuation marker");

	// Interpolate Parquet: application/vnd.apache.parquet
	let (status, headers, bytes) = send(router.clone(), post_json("/api/v1/interpolate/parquet", Some(READER_KEY), &json_body)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.apache.parquet");
	assert_eq!(&bytes[..4], b"PAR1", "the Parquet file magic");
	assert_eq!(&bytes[bytes.len() - 4..], b"PAR1", "and its closing magic");

	let downsample_body = downsample_body();

	// Downsample JSON: application/json
	let (status, headers, _) = send(router.clone(), post_json("/api/v1/downsample", Some(READER_KEY), &downsample_body)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "application/json");

	// Downsample CSV
	let (status, headers, bytes) = send(router.clone(), post_json("/api/v1/downsample/csv", Some(READER_KEY), &downsample_body)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "text/csv; charset=utf-8");
	let text = String::from_utf8(bytes).expect("CSV is text");
	assert!(text.lines().next().is_some_and(|line| line.starts_with("timestamp,")), "the CSV starts with its column header: {text}");

	// Downsample Arrow
	let (status, headers, bytes) = send(router.clone(), post_json("/api/v1/downsample/arrow", Some(READER_KEY), &downsample_body)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.apache.arrow.stream");
	assert_eq!(&bytes[..4], &[0xff, 0xff, 0xff, 0xff]);

	// Downsample Parquet
	let (status, headers, bytes) = send(router, post_json("/api/v1/downsample/parquet", Some(READER_KEY), &downsample_body)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.apache.parquet");
	assert_eq!(&bytes[..4], b"PAR1", "the Parquet file magic");
	assert_eq!(&bytes[bytes.len() - 4..], b"PAR1", "and its closing magic");
}

#[tokio::test]
async fn ilp_compute_formats_carry_their_media_types() {
	let router = secured_router().await.1;

	// ILP interpolate JSON: application/json
	let (status, headers, _) = send(router.clone(), post_text("/api/v1/interpolate/ilp?field=load&precision=s", Some(READER_KEY), ILP_BODY)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "application/json");

	// ILP interpolate CSV
	let (status, headers, _) = send(router.clone(), post_text("/api/v1/interpolate/ilp/csv?field=load&precision=s", Some(READER_KEY), ILP_BODY)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "text/csv; charset=utf-8");

	// ILP interpolate Arrow
	let (status, headers, bytes) = send(router.clone(), post_text("/api/v1/interpolate/ilp/arrow?field=load&precision=s", Some(READER_KEY), ILP_BODY)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.apache.arrow.stream");
	assert_eq!(&bytes[..4], &[0xff, 0xff, 0xff, 0xff]);

	// ILP interpolate Parquet
	let (status, headers, bytes) = send(router.clone(), post_text("/api/v1/interpolate/ilp/parquet?field=load&precision=s", Some(READER_KEY), ILP_BODY)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.apache.parquet");
	assert_eq!(&bytes[..4], b"PAR1");

	// ILP downsample JSON
	let (status, headers, _) = send(router.clone(), post_text("/api/v1/downsample/ilp?field=load&precision=s", Some(READER_KEY), ILP_BODY)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "application/json");

	// ILP downsample CSV
	let (status, headers, _) = send(router.clone(), post_text("/api/v1/downsample/ilp/csv?field=load&precision=s", Some(READER_KEY), ILP_BODY)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "text/csv; charset=utf-8");

	// ILP downsample Arrow
	let (status, headers, bytes) = send(router.clone(), post_text("/api/v1/downsample/ilp/arrow?field=load&precision=s", Some(READER_KEY), ILP_BODY)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.apache.arrow.stream");
	assert_eq!(&bytes[..4], &[0xff, 0xff, 0xff, 0xff]);

	// ILP downsample Parquet
	let (status, headers, bytes) = send(router, post_text("/api/v1/downsample/ilp/parquet?field=load&precision=s", Some(READER_KEY), ILP_BODY)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.apache.parquet");
	assert_eq!(&bytes[..4], b"PAR1");
}
