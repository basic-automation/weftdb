#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]
//! FRE-10b: contract tests for the second half of the FRE-10 surface — the
//! storage endpoints and the operator endpoints, plus coverage.
//! (FRE-10a, in W9, takes probes, the stateless compute endpoints, metrics
//! and roles; the split is the pre-#99 ROADMAP text both lines carried before
//! the checklist slimming dropped the qualifiers — see the FRE-10a DECISIONS
//! entry, 2026-11-08 — and FRE-9b's surviving line, "`OpenAPI`: storage and
//! operator endpoints", mirrors it.)
//!
//! The per-slice suites pin each handler's internals, and SEC-13's acceptance
//! suite pins the role matrix on one representative route per scope. What
//! neither pins is the composed client contract of the storage and operator
//! families, which is what a caller actually programs against and what the
//! 1.0 freeze promises:
//!
//! - the exact key set of every JSON answer in the families — a field that
//!   quietly appears or disappears is a contract change, not a refactor;
//! - the media types and file magics of the stored-range exports
//!   (JSON points, CSV, Arrow IPC, Parquet) and of the stored downsample and
//!   interpolate formats;
//! - the write surface's answers: the `201` a declaration and an ingest seal
//!   with, the idempotent replay, the four ingest formats, and the operator
//!   passes (reconcile, squash, compact, fsck, quarantine, backup, drill);
//! - the gate on *every* route in the families, enumerated from the sealed
//!   route registry itself, so a route joining one of these scopes without
//!   contract coverage fails here rather than shipping unexamined: reads pass
//!   at reader, ingest at writer, declare at declarer, maintenance at
//!   maintainer, backup at admin — the cumulative grants, role by role.
//!
//! The coverage test is the cross-check the two halves need: it pins the
//! whole registry (73 operations over 8 scopes) and the partition of the
//! scopes between the two files, so the halves sum to the registry instead
//! of each file only knowing its own side. One staleness it already records:
//! the headroom route joined the metrics scope after FRE-10a was written, so
//! FRE-10a's pinned metrics size (4) is one short of the registry today
//! (5) — the FRE-10a salvage bumps that count when it merges; this file
//! pins 5 as the current truth.

use std::{collections::BTreeSet, sync::Arc};

use axum::{
	body::Body, http::{header, HeaderMap, Method, Request, StatusCode}, Router
};
use bigdecimal::BigDecimal;
use tower::ServiceExt as _;
use weft_arrow::{columns_to_record_batch_typed, write_parquet};
use weft_physical_type::{PhysicalType, TimeUnit};
use weft_server::{
	app_with_state, auth::{format_key_hash, AuthConfig, KeyStore}, routes::{build_registry, RouteEntry, Scope}, store_open::{ScopePin, ServingScope}, AppState, VERSION
};
use weftdb::SegmentStore;

// --- Principals ---------------------------------------------------------------

const READER_KEY: &str = "fre10b-reader-key";
const WRITER_KEY: &str = "fre10b-writer-key";
const DECLARER_KEY: &str = "fre10b-declarer-key";
const MAINTAINER_KEY: &str = "fre10b-maintainer-key";
const ADMIN_KEY: &str = "fre10b-admin-key";
const UNKNOWN_KEY: &str = "fre10b-no-such-key";

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

fn delete(uri: &str, key: Option<&str>) -> Request<Body> {
	let mut builder = Request::builder().method("DELETE").uri(uri);
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

fn post_bytes(uri: &str, key: Option<&str>, content_type: &str, body: &[u8]) -> Request<Body> {
	let mut builder = Request::builder().method("POST").uri(uri).header(header::CONTENT_TYPE, content_type);
	if let Some(key) = key {
		builder = builder.header(header::AUTHORIZATION, format!("Bearer {key}"));
	}
	builder.body(Body::from(body.to_vec())).unwrap()
}

/// A POST that carries no body: the query-only operator passes and the
/// query-only stored-downsample POSTs take their parameters from the URI.
fn post_empty(uri: &str, key: Option<&str>) -> Request<Body> {
	let mut builder = Request::builder().method("POST").uri(uri);
	if let Some(key) = key {
		builder = builder.header(header::AUTHORIZATION, format!("Bearer {key}"));
	}
	builder.body(Body::empty()).unwrap()
}

// --- Seed data ------------------------------------------------------------------

/// The aspect every seeded router declares: `f64` values on a seconds grid.
const ASPECT: &str = "price";

/// Four samples a minute apart, on the seconds grid the reads query.
fn declare_body() -> serde_json::Value {
	serde_json::json!({ "name": ASPECT, "physical_type": "f64", "timestamp_unit": "seconds" })
}

fn ingest_body() -> serde_json::Value {
	serde_json::json!({ "points": [
		{ "timestamp": 0, "value": "1" },
		{ "timestamp": 60, "value": "2" },
		{ "timestamp": 120, "value": "1.5" },
		{ "timestamp": 180, "value": "3" },
	] })
}

/// Two line-protocol rows in the tagged series `{host: a}` (TAG-11): the
/// read-back separates them from the untagged series by the `tag.*` selector.
const ILP_BODY: &str = "cpu,host=a load=5 240\ncpu,host=a load=6 300\n";

/// Two CSV rows, the same shape the CSV export writes back.
const CSV_BODY: &str = "timestamp,value\n240,4\n300,5\n";

/// A Parquet file holding the seed rows, built the way the range export
/// builds its typed batch — the ingest the Parquet route must accept.
fn parquet_bytes() -> Vec<u8> {
	let values: Vec<Option<BigDecimal>> = ["1", "2", "1.5", "3"].iter().map(|text| Some(text.parse().expect("a decimal"))).collect();
	let batch = columns_to_record_batch_typed(TimeUnit::Seconds, PhysicalType::F64, &[0, 60, 120, 180], &values).expect("a typed batch builds");
	write_parquet(&[batch]).expect("parquet writes")
}

/// A router with a real store holding the declared aspect and the four seed
/// rows, no auth: the shape tests' starting point.
async fn seeded_router() -> (tempfile::TempDir, Router) {
	let dir = tempfile::tempdir().expect("tempdir");
	let router = app_with_state(AppState::new().with_store(Arc::new(SegmentStore::open(dir.path().join("store")).await.expect("store opens"))));
	let (status, _headers, body) = send_json(router.clone(), post_json("/api/v1/storage/aspects", None, &declare_body())).await;
	assert_eq!(status, StatusCode::CREATED, "the seed declaration: {body}");
	let (status, _headers, body) = send_json(router.clone(), post_json("/api/v1/storage/price/points", None, &ingest_body())).await;
	assert_eq!(status, StatusCode::CREATED, "the seed ingest: {body}");
	(dir, router)
}

/// A router with the same seeded store and the full keys table in force.
/// The seed goes through the secured router itself, with the admin key —
/// the declare and ingest routes are gated, so no unauthenticated seeding
/// path exists to lean on.
async fn secured_seeded_router() -> (tempfile::TempDir, Router) {
	let dir = tempfile::tempdir().expect("tempdir");
	let store = Arc::new(SegmentStore::open(dir.path().join("store")).await.expect("store opens"));
	let keys = KeyStore::parse(&full_keys_document()).expect("keys parse");
	let router = app_with_state(AppState::new().with_store(store).with_auth(AuthConfig::with_keys(keys)));
	let (status, _headers, body) = send_json(router.clone(), post_json("/api/v1/storage/aspects", Some(ADMIN_KEY), &declare_body())).await;
	assert_eq!(status, StatusCode::CREATED, "the seed declaration: {body}");
	let (status, _headers, body) = send_json(router.clone(), post_json("/api/v1/storage/price/points", Some(ADMIN_KEY), &ingest_body())).await;
	assert_eq!(status, StatusCode::CREATED, "the seed ingest: {body}");
	(dir, router)
}

// --- The families, from the registry -------------------------------------------

fn family(scope: Scope) -> Vec<RouteEntry> {
	build_registry().table().entries().iter().filter(|entry| entry.spec.scope == scope).cloned().collect()
}

#[test]
fn the_fre10b_families_are_the_frozen_size() {
	// Pinned, not just non-empty: a route joining one of these scopes joins
	// the frozen contract surface, and this file is where its contract gets
	// pinned — the counts below are the reminder to extend it.
	assert_eq!(family(Scope::Read).len(), 31, "read: version, 3 grafana, 27 storage reads");
	assert_eq!(family(Scope::Ingest).len(), 4, "ingest: json, ilp, csv, parquet");
	assert_eq!(family(Scope::Declare).len(), 1, "declare: POST /api/v1/storage/aspects");
	assert_eq!(family(Scope::Maintain).len(), 9, "maintain: reconcile x2, squash, compact, fsck, quarantine x4");
	assert_eq!(family(Scope::Backup).len(), 2, "backup: snapshot and restore drill");
}

#[test]
fn the_contract_coverage_spans_the_whole_registry() {
	// "Plus coverage": the two FRE-10 halves partition the registry — every
	// scope belongs to exactly one half, and the halves sum to the whole
	// surface. A route added anywhere moves one of these counts, and the
	// file that owns its scope is where its contract gets pinned.
	let table = build_registry().table();
	assert_eq!(table.entries().len(), 73, "the registry's whole surface");
	let halves: [(Scope, &str); 8] = [(Scope::Probe, "fre10a"), (Scope::Metrics, "fre10a"), (Scope::Compute, "fre10a"), (Scope::Read, "fre10b"), (Scope::Ingest, "fre10b"), (Scope::Declare, "fre10b"), (Scope::Maintain, "fre10b"), (Scope::Backup, "fre10b")];
	let mut seen = BTreeSet::new();
	for (scope, _) in &halves {
		assert!(seen.insert(scope), "each scope is claimed by exactly one half");
	}
	assert_eq!(seen.len(), Scope::ALL.len(), "every scope in the frozen table is claimed");
	let sizes: [(Scope, usize); 8] = [(Scope::Probe, 4), (Scope::Metrics, 5), (Scope::Compute, 17), (Scope::Read, 31), (Scope::Ingest, 4), (Scope::Declare, 1), (Scope::Maintain, 9), (Scope::Backup, 2)];
	let mut total = 0;
	for (scope, size) in sizes {
		assert_eq!(family(scope).len(), size, "scope {} holds {size} routes", scope.as_str());
		total += size;
	}
	assert_eq!(total, table.entries().len(), "the halves sum to the registry");
	let first_half: usize = halves.iter().filter(|(_, half)| *half == "fre10a").map(|(scope, _)| family(*scope).len()).sum();
	let second_half: usize = halves.iter().filter(|(_, half)| *half == "fre10b").map(|(scope, _)| family(*scope).len()).sum();
	assert_eq!((first_half, second_half), (26, 47), "fre10a 4+5+17, fre10b 31+4+1+9+2");
}

/// The concrete path for a registry entry: the templates carry `{aspect}`,
/// `{id}` and `{gen}`, filled with the seeded aspect and a well-formed
/// (unknown) quarantine identity.
fn concrete_path(entry: &RouteEntry) -> String {
	entry.path.replace("{aspect}", ASPECT).replace("{id}", "0").replace("{gen}", "0")
}

/// A request that passes the gate behind the handler, per route: the seeded
/// window for the reads, the seed bodies for the writes, the operator's
/// parameters for the passes. The Parquet ingest needs real Parquet bytes,
/// built by the caller the way the range export builds them.
fn family_request(entry: &RouteEntry, key: Option<&str>, parquet: &[u8]) -> Request<Body> {
	let path = concrete_path(entry);
	let method = entry.method.clone();
	if method == Method::GET {
		return match path.as_str() {
			"/api/v1/storage/price/points" => get("/api/v1/storage/price/points?start=0&end=180", key),
			"/api/v1/storage/price/range" | "/api/v1/storage/price/range.parquet" | "/api/v1/storage/price/range.csv" => get(&format!("{path}?start=0&end=180"), key),
			"/api/v1/storage/price/at" => get("/api/v1/storage/price/at?t=60", key),
			"/api/v1/storage/price/at-multi" => get("/api/v1/storage/price/at-multi?t=0,60,180", key),
			_ if path.contains("/value-range") => get(&format!("{path}?lo=1&hi=3"), key),
			_ if path.contains("/value-points") => get("/api/v1/storage/price/value-points?lo=1&hi=3", key),
			_ if path.contains("/downsample") => get(&format!("{path}?start=0&end=180"), key),
			_ if path.contains("/interpolate") => get(&format!("{path}?start=0&end=180"), key),
			_ => get(&path, key),
		};
	}
	// POST.
	if path == "/api/v1/storage/aspects" {
		return post_json(&path, key, &declare_body());
	}
	if path == "/api/v1/storage/price/points" {
		return post_json(&path, key, &ingest_body());
	}
	if path == "/api/v1/storage/price/ilp" {
		return post_text("/api/v1/storage/price/ilp?field=load&measurement=cpu&precision=s", key, ILP_BODY);
	}
	if path == "/api/v1/storage/price/csv" {
		return post_text("/api/v1/storage/price/csv", key, CSV_BODY);
	}
	if path == "/api/v1/storage/price/parquet" {
		return post_bytes("/api/v1/storage/price/parquet", key, "application/vnd.apache.parquet", parquet);
	}
	if path.contains("/downsample") {
		return post_empty(&format!("{path}?start=0&end=180"), key);
	}
	if path == "/api/v1/storage/price/compact" {
		return post_empty("/api/v1/storage/price/compact?target_rows=10", key);
	}
	if path == "/api/v1/storage/backup" {
		return post_empty("/api/v1/storage/backup?label=fre10b-gate", key);
	}
	if path == "/api/v1/storage/restore/drill" {
		return post_empty("/api/v1/storage/restore/drill?label=fre10b-no-such-backup", key);
	}
	if path == "/api/v1/grafana/metrics" {
		return post_json(&path, key, &serde_json::json!({}));
	}
	if path == "/api/v1/grafana/query" {
		return post_json(&path, key, &grafana_query_body());
	}
	post_empty(&path, key)
}

/// The Grafana panel query over the seed window: one target, the seeded
/// aspect, a minute interval over the three-minute span.
fn grafana_query_body() -> serde_json::Value {
	serde_json::json!({
		"range": { "from": "1970-01-01T00:00:00Z", "to": "1970-01-01T00:03:00Z" },
		"intervalMs": 60000,
		"maxDataPoints": 100,
		"targets": [{ "target": ASPECT, "refId": "A" }],
	})
}

/// What a key that holds the route's scope gets back: `201` for the writes
/// that seal something, the handler's own `404` for the operator routes
/// that name a resource that does not exist, `200` everywhere else. The
/// `404`s are still the gate passing — the handler answered, with the §4
/// code for the missing resource.
fn expected_pass_status(entry: &RouteEntry) -> StatusCode {
	match entry.spec.operation_id {
		"storage_aspect_declare" | "storage_ingest_points" | "storage_ingest_ilp" | "storage_ingest_csv" | "storage_ingest_parquet" => StatusCode::CREATED,
		"storage_quarantine_release" | "storage_quarantine_purge" | "storage_restore_drill" => StatusCode::NOT_FOUND,
		_ => StatusCode::OK,
	}
}

/// The gate for one route, over all seven principals: anonymous and unknown
/// keys are refused `401` everywhere in these families (no probes live
/// here); each role passes exactly the scopes its grant holds, and a key
/// that passes reaches the handler — never a silent `401`/`403` past it.
async fn check_route_gate(router: Router, entry: &RouteEntry, parquet: &[u8]) {
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
		let (status, headers, body) = send_lenient(router.clone(), family_request(entry, key, parquet)).await;
		match role {
			None => {
				assert_eq!(status, StatusCode::UNAUTHORIZED, "{name} {label}: {body}");
				assert_eq!(body["code"], "unauthenticated", "{name} {label}: {body}");
				if key.is_none() {
					assert!(headers.get(header::WWW_AUTHENTICATE).and_then(|value| value.to_str().ok()).is_some_and(|value| value.starts_with("Bearer")), "{name} {label}: the 401 carries the challenge");
				}
			}
			Some(role) if !role.allows(scope) => {
				assert_eq!(status, StatusCode::FORBIDDEN, "{name} {label}: {body}");
				assert_eq!(body["code"], "forbidden", "{name} {label}: {body}");
			}
			Some(_) => {
				assert_eq!(status, expected_pass_status(entry), "{name} {label}: past the gate, the handler answers: {body}");
				if status == StatusCode::NOT_FOUND {
					assert_eq!(body["code"], "aspect_not_found", "{name} {label}: the missing resource has its §4 code: {body}");
				}
			}
		}
	}
}

#[tokio::test]
async fn the_gate_holds_for_every_route_in_the_families() {
	let (_dir, router) = secured_seeded_router().await;
	let parquet = parquet_bytes();
	let entries: Vec<RouteEntry> = [Scope::Read, Scope::Ingest, Scope::Declare, Scope::Maintain, Scope::Backup].iter().flat_map(|scope| family(*scope)).collect();
	assert_eq!(entries.len(), 47, "the whole FRE-10b surface, enumerated from the registry");
	for entry in &entries {
		check_route_gate(router.clone(), entry, &parquet).await;
	}
}

// --- Storage reads: answer shapes -------------------------------------------------

#[tokio::test]
async fn points_answer_their_frozen_shape() {
	let (_dir, router) = seeded_router().await;
	let (status, _headers, body) = send_json(router.clone(), get("/api/v1/storage/price/points?start=0&end=180", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["aspect", "tags", "time_unit", "total", "count", "offset", "limit", "points"]), "exactly the points keys: {body}");
	assert_eq!(body["aspect"], "price", "{body}");
	assert_eq!(body["tags"], serde_json::json!({}), "the untagged series: {body}");
	assert_eq!(body["time_unit"], "seconds", "{body}");
	assert_eq!(body["total"], 4, "{body}");
	assert_eq!(body["count"], 4, "{body}");
	assert_eq!(body["offset"], 0, "{body}");
	assert_eq!(body["limit"], 10_000, "the contract default page, named rather than silent: {body}");
	let points = body["points"].as_array().expect("points is an array");
	assert_eq!(points.len(), 4, "{body}");
	for (point, (timestamp, value)) in points.iter().zip([(0, "1"), (60, "2"), (120, "1.5"), (180, "3")]) {
		assert_eq!(key_set(point), expected(&["timestamp", "value"]), "exactly the row keys: {point}");
		assert_eq!(point["timestamp"], timestamp, "{point}");
		assert_eq!(point["value"], value, "lossless decimal text, no float round-trip: {point}");
	}
}

#[tokio::test]
async fn point_lookups_answer_their_frozen_shapes() {
	let (_dir, router) = seeded_router().await;
	// A hit: the value is present, `found` says so.
	let (status, _headers, body) = send_json(router.clone(), get("/api/v1/storage/price/at?t=60", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["aspect", "tags", "time_unit", "timestamp", "value", "found"]), "exactly the point keys: {body}");
	assert_eq!(body["aspect"], "price", "{body}");
	assert_eq!(body["tags"], serde_json::json!({}), "{body}");
	assert_eq!(body["time_unit"], "seconds", "{body}");
	assert_eq!(body["timestamp"], 60, "{body}");
	assert_eq!(body["value"], "2", "{body}");
	assert_eq!(body["found"], true, "{body}");

	// A miss: `value` is absent, not null — and `found: false` is what
	// distinguishes the miss from a present-but-omitted value.
	let (status, _headers, body) = send_json(router.clone(), get("/api/v1/storage/price/at?t=61", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["aspect", "tags", "time_unit", "timestamp", "found"]), "a miss omits `value`: {body}");
	assert_eq!(body["timestamp"], 61, "{body}");
	assert_eq!(body["found"], false, "{body}");

	// The batch lookup: one entry per queried instant, in query order, a
	// miss carried as an entry with `found: false` and no `value`.
	let (status, _headers, body) = send_json(router, get("/api/v1/storage/price/at-multi?t=180,61,0", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["aspect", "tags", "time_unit", "points"]), "exactly the batch keys: {body}");
	let points = body["points"].as_array().expect("points is an array");
	assert_eq!(points.len(), 3, "{body}");
	assert_eq!(points[0]["timestamp"], 180, "{body}");
	assert_eq!(points[0]["value"], "3", "{body}");
	assert_eq!(points[0]["found"], true, "{body}");
	assert_eq!(points[1]["timestamp"], 61, "{body}");
	assert_eq!(key_set(&points[1]), expected(&["timestamp", "found"]), "the miss entry: {body}");
	assert_eq!(points[1]["found"], false, "{body}");
	assert_eq!(points[2]["timestamp"], 0, "{body}");
	assert_eq!(points[2]["value"], "1", "{body}");
}

#[tokio::test]
async fn value_range_answers_its_frozen_shape() {
	let (_dir, router) = seeded_router().await;
	let (status, _headers, body) = send_json(router, get("/api/v1/storage/price/value-points?lo=1&hi=2", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["aspect", "tags", "time_unit", "total", "count", "offset", "limit", "points"]), "the value read is the same shape as the time read: {body}");
	assert_eq!(body["total"], 3, "the rows in [1, 2]: 1, 1.5, 2: {body}");
	assert_eq!(body["count"], 3, "{body}");
	let values: Vec<&str> = body["points"].as_array().expect("points is an array").iter().map(|point| point["value"].as_str().expect("a decimal value")).collect();
	assert_eq!(values, ["1", "2", "1.5"], "the band filters by value; the rows come back in time order: {body}");
}

#[tokio::test]
async fn catalog_aspects_schema_and_stats_answer_their_frozen_shapes() {
	let (_dir, router) = seeded_router().await;
	let (status, _headers, body) = send_json(router.clone(), get("/api/v1/storage/catalog", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["database", "subject", "databases"]), "exactly the catalog keys: {body}");
	assert_eq!(body["database"], "default", "the scope this store opened under: {body}");
	assert_eq!(body["subject"], "default", "{body}");
	assert_eq!(body["databases"], serde_json::json!([{ "name": "default", "subjects": ["default"] }]), "{body}");

	let (status, _headers, body) = send_json(router.clone(), get("/api/v1/storage/aspects", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["aspects"]), "exactly the list keys: {body}");
	let aspects = body["aspects"].as_array().expect("aspects is an array");
	assert_eq!(aspects.len(), 1, "{body}");
	assert_eq!(key_set(&aspects[0]), expected(&["name", "physical_type", "value_tolerance", "timestamp_unit"]), "no scale on an f64 aspect, no upsert keys on the list: {body}");
	assert_eq!(aspects[0]["name"], "price", "{body}");
	assert_eq!(aspects[0]["physical_type"], "f64", "{body}");
	assert_eq!(aspects[0]["value_tolerance"], "0", "{body}");
	assert_eq!(aspects[0]["timestamp_unit"], "seconds", "{body}");

	let (status, _headers, body) = send_json(router.clone(), get("/api/v1/storage/price/schema", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["aspect"]), "exactly the schema keys: {body}");
	assert_eq!(body["aspect"], aspects[0], "the schema read is the list read: {body}");

	let (status, _headers, body) = send_json(router.clone(), get("/api/v1/storage/price/stats", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["aspect", "segment_count", "total_rows", "total_nulls", "total_bytes", "bytes_per_point", "unsorted_segments", "overlapping_segments", "time_range", "value_range"]), "exactly the aspect-stats keys: {body}");
	assert_eq!(body["aspect"], "price", "{body}");
	assert_eq!(body["segment_count"], 1, "{body}");
	assert_eq!(body["total_rows"], 4, "{body}");
	assert_eq!(body["total_nulls"], 0, "{body}");
	assert!(body["total_bytes"].as_u64().is_some_and(|bytes| bytes > 0), "a sealed segment occupies bytes: {body}");
	assert!(body["bytes_per_point"].as_f64().is_some_and(|cost| cost > 0.0), "the north-star cost term: {body}");
	assert_eq!(body["unsorted_segments"], 0, "{body}");
	assert_eq!(body["overlapping_segments"], 0, "{body}");
	assert_eq!(body["time_range"], serde_json::json!([0, 180]), "{body}");
	assert_eq!(body["value_range"], serde_json::json!(["1", "3"]), "{body}");

	let (status, _headers, body) = send_json(router, get("/api/v1/storage/stats", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["aspect_count", "segment_count", "total_rows", "total_nulls", "total_bytes", "bytes_per_point", "unsorted_segments", "overlapping_segments", "time_range"]), "exactly the store-stats keys: {body}");
	assert_eq!(body["aspect_count"], 1, "{body}");
	assert_eq!(body["segment_count"], 1, "{body}");
	assert_eq!(body["total_rows"], 4, "{body}");
	assert_eq!(body["time_range"], serde_json::json!([0, 180]), "{body}");
}

// --- Storage reads: export formats ---------------------------------------------

#[tokio::test]
async fn range_exports_carry_their_media_types_and_magics() {
	let (_dir, router) = seeded_router().await;
	// The bare range is the Arrow stream, as the handler's name says.
	let (status, headers, bytes) = send(router.clone(), get("/api/v1/storage/price/range?start=0&end=180", None)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.apache.arrow.stream");
	assert_eq!(&bytes[..4], &[0xff, 0xff, 0xff, 0xff], "the IPC continuation marker");

	let (status, headers, bytes) = send(router.clone(), get("/api/v1/storage/price/range.parquet?start=0&end=180", None)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.apache.parquet");
	assert_eq!(&bytes[..4], b"PAR1", "the Parquet file magic");
	assert_eq!(&bytes[bytes.len() - 4..], b"PAR1", "and its closing magic");

	let (status, headers, bytes) = send(router.clone(), get("/api/v1/storage/price/range.csv?start=0&end=180", None)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "text/csv; charset=utf-8");
	let text = String::from_utf8(bytes).expect("CSV is text");
	let mut lines = text.lines();
	assert_eq!(lines.next().expect("a header line"), "timestamp,value", "the export's header: {text}");
	assert_eq!(lines.clone().count(), 4, "one line per seed row: {text}");
	assert!(text.contains("0,1"), "the first row, lossless: {text}");
	assert!(text.contains("120,1.5"), "the decimal row: {text}");

	// The value band exports the same three wrappings.
	let (status, headers, bytes) = send(router.clone(), get("/api/v1/storage/price/value-range?lo=1&hi=3", None)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.apache.arrow.stream");
	assert_eq!(&bytes[..4], &[0xff, 0xff, 0xff, 0xff]);

	let (status, headers, bytes) = send(router.clone(), get("/api/v1/storage/price/value-range.parquet?lo=1&hi=3", None)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.apache.parquet");
	assert_eq!(&bytes[..4], b"PAR1");

	let (status, headers, bytes) = send(router, get("/api/v1/storage/price/value-range.csv?lo=1&hi=3", None)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(headers[header::CONTENT_TYPE], "text/csv; charset=utf-8");
	let text = String::from_utf8(bytes).expect("CSV is text");
	assert_eq!(text.lines().count(), 5, "header plus the four rows in the band: {text}");
}

// --- Stored downsample and interpolate ------------------------------------------

#[tokio::test]
async fn stored_downsample_answers_its_frozen_shape() {
	let (_dir, router) = seeded_router().await;
	for (method, uri) in [("GET", "/api/v1/storage/price/downsample?start=0&end=180"), ("POST", "/api/v1/storage/price/downsample?start=0&end=180")] {
		let request = if method == "GET" { get(uri, None) } else { post_empty(uri, None) };
		let (status, _headers, body) = send_json(router.clone(), request).await;
		assert_eq!(status, StatusCode::OK, "{method}: {body}");
		// The stored answer is the compute answer's shape plus `tags`: the
		// series reduced, named so a tagged read is distinguishable.
		assert_eq!(key_set(&body), expected(&["resolution", "aggregations", "input_points", "buckets", "series", "tags"]), "{method}: exactly the downsample keys: {body}");
		assert_eq!(body["resolution"], "minutes", "{method}: {body}");
		assert_eq!(body["aggregations"], serde_json::json!(["min", "max", "avg"]), "{method}: the default reductions, in order: {body}");
		assert_eq!(body["input_points"], 4, "{method}: {body}");
		assert_eq!(body["buckets"], 4, "{method}: one bucket per minute: {body}");
		assert_eq!(body["tags"], serde_json::json!({}), "{method}: the untagged series: {body}");
		let series = body["series"].as_array().expect("series is an array");
		assert_eq!(series.len(), 4, "{method}: {body}");
		for (bucket, value) in series.iter().zip([1.0, 2.0, 1.5, 3.0]) {
			assert_eq!(key_set(bucket), expected(&["timestamp", "count", "aggregations"]), "{method}: exactly the bucket keys: {bucket}");
			assert_eq!(bucket["count"], 1, "{method}: {bucket}");
			assert_eq!(bucket["aggregations"]["min"], value, "{method}: {bucket}");
			assert_eq!(bucket["aggregations"]["max"], value, "{method}: {bucket}");
			assert_eq!(bucket["aggregations"]["avg"], value, "{method}: {bucket}");
		}
	}
}

#[tokio::test]
async fn stored_downsample_formats_carry_their_media_types() {
	let (_dir, router) = seeded_router().await;
	for (method, base) in [("GET", "/api/v1/storage/price/downsample"), ("POST", "/api/v1/storage/price/downsample")] {
		for suffix in ["csv", "arrow", "parquet"] {
			let uri = format!("{base}.{suffix}?start=0&end=180");
			let request = if method == "GET" { get(&uri, None) } else { post_empty(&uri, None) };
			let (status, headers, bytes) = send(router.clone(), request).await;
			let label = format!("{method} {uri}");
			assert_eq!(status, StatusCode::OK, "{label}");
			match suffix {
				"csv" => {
					assert_eq!(headers[header::CONTENT_TYPE], "text/csv; charset=utf-8", "{label}");
					let text = String::from_utf8(bytes).expect("CSV is text");
					assert!(text.lines().next().is_some_and(|line| line.starts_with("timestamp,")), "{label} starts with its column header: {text}");
				}
				"arrow" => {
					assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.apache.arrow.stream", "{label}");
					assert_eq!(&bytes[..4], &[0xff, 0xff, 0xff, 0xff], "{label} opens with the IPC continuation marker");
				}
				_ => {
					assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.apache.parquet", "{label}");
					assert_eq!(&bytes[..4], b"PAR1", "{label} opens with the file magic");
					assert_eq!(&bytes[bytes.len() - 4..], b"PAR1", "{label} closes with the file magic");
				}
			}
		}
	}
}

#[tokio::test]
async fn stored_interpolate_answers_its_frozen_shape() {
	let (_dir, router) = seeded_router().await;
	let (status, _headers, body) = send_json(router.clone(), get("/api/v1/storage/price/interpolate?start=0&end=180", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["aspect", "time_unit", "start", "end", "margin", "spline", "resolution", "backend", "output_points", "points"]), "exactly the stored-interpolation keys: {body}");
	assert_eq!(body["aspect"], "price", "{body}");
	assert_eq!(body["time_unit"], "seconds", "{body}");
	assert_eq!(body["start"], 0, "{body}");
	assert_eq!(body["end"], 180, "{body}");
	assert_eq!(body["margin"], 180, "the default margin is the window itself: {body}");
	assert_eq!(body["spline"], "Cubic", "the default method, named as the one that ran: {body}");
	// The stored answer renders the resolution with the enum's `Debug` name
	// (`stored_interpolate.rs` formats `{resolution:?}`), unlike the
	// downsample and stateless-compute answers, which echo the lowercase
	// token — pinned as the wire actually answers it.
	assert_eq!(body["resolution"], "Minutes", "{body}");
	let backend = body["backend"].as_str().expect("the backend is named");
	assert!(["Cpu", "Parallel", "Gpu"].contains(&backend), "the backend that ran: {backend}");
	assert_eq!(body["output_points"], 4, "{body}");
	let points = body["points"].as_array().expect("points is an array");
	assert_eq!(points.len(), 4, "{body}");
	for (point, value) in points.iter().zip([1.0, 2.0, 1.5, 3.0]) {
		assert_eq!(key_set(point), expected(&["timestamp", "value", "kind"]), "exactly the point keys: {point}");
		assert_eq!(point["kind"], "raw", "a grid point on an input minute is the input: {point}");
		assert_eq!(point["value"], value, "{point}");
	}
}

#[tokio::test]
async fn stored_interpolate_formats_carry_their_media_types() {
	let (_dir, router) = seeded_router().await;
	for suffix in ["csv", "arrow", "parquet"] {
		let uri = format!("/api/v1/storage/price/interpolate.{suffix}?start=0&end=180");
		let (status, headers, bytes) = send(router.clone(), get(&uri, None)).await;
		assert_eq!(status, StatusCode::OK, "{uri}");
		match suffix {
			"csv" => {
				assert_eq!(headers[header::CONTENT_TYPE], "text/csv; charset=utf-8", "{uri}");
				let text = String::from_utf8(bytes).expect("CSV is text");
				assert!(text.lines().next().is_some_and(|line| line.starts_with("timestamp,")), "{uri} starts with its column header: {text}");
			}
			"arrow" => {
				assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.apache.arrow.stream", "{uri}");
				assert_eq!(&bytes[..4], &[0xff, 0xff, 0xff, 0xff], "{uri} opens with the IPC continuation marker");
			}
			_ => {
				assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.apache.parquet", "{uri}");
				assert_eq!(&bytes[..4], b"PAR1", "{uri} opens with the file magic");
				assert_eq!(&bytes[bytes.len() - 4..], b"PAR1", "{uri} closes with the file magic");
			}
		}
	}
}

// --- Declare and ingest: answer shapes ------------------------------------------

#[tokio::test]
async fn declare_answers_its_frozen_shape() {
	let dir = tempfile::tempdir().expect("tempdir");
	let router = app_with_state(AppState::new().with_store(Arc::new(SegmentStore::open(dir.path().join("store")).await.expect("store opens"))));
	// A fresh declaration answers `201` with the schema read back from the
	// store — the declaration as held, not an echo of the request.
	let (status, _headers, body) = send_json(router.clone(), post_json("/api/v1/storage/aspects", None, &declare_body())).await;
	assert_eq!(status, StatusCode::CREATED, "{body}");
	assert_eq!(key_set(&body), expected(&["aspect"]), "exactly the declaration keys: {body}");
	assert_eq!(key_set(&body["aspect"]), expected(&["name", "physical_type", "value_tolerance", "timestamp_unit"]), "the held schema: {body}");
	assert_eq!(body["aspect"]["name"], "price", "{body}");
	assert_eq!(body["aspect"]["physical_type"], "f64", "{body}");
	assert_eq!(body["aspect"]["value_tolerance"], "0", "{body}");
	assert_eq!(body["aspect"]["timestamp_unit"], "seconds", "{body}");

	// Re-declaring the identical schema is a no-op that still answers `201`.
	let (status, _headers, body) = send_json(router.clone(), post_json("/api/v1/storage/aspects", None, &declare_body())).await;
	assert_eq!(status, StatusCode::CREATED, "an identical re-declaration: {body}");
	assert_eq!(body["aspect"]["physical_type"], "f64", "{body}");

	// Any other re-declaration is a `409 schema_conflict` and changes nothing.
	let mut conflict = declare_body();
	conflict["physical_type"] = serde_json::json!("f32");
	let (status, _headers, body) = send_json(router.clone(), post_json("/api/v1/storage/aspects", None, &conflict)).await;
	assert_eq!(status, StatusCode::CONFLICT, "{body}");
	assert_eq!(body["code"], "schema_conflict", "{body}");
	let (status, _headers, body) = send_json(router, get("/api/v1/storage/price/schema", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["aspect"]["physical_type"], "f64", "the conflict changed nothing: {body}");
}

#[tokio::test]
async fn ingest_answers_its_frozen_shape() {
	let dir = tempfile::tempdir().expect("tempdir");
	let router = app_with_state(AppState::new().with_store(Arc::new(SegmentStore::open(dir.path().join("store")).await.expect("store opens"))));
	let (status, _headers, _) = send_json(router.clone(), post_json("/api/v1/storage/aspects", None, &declare_body())).await;
	assert_eq!(status, StatusCode::CREATED);
	let (status, headers, body) = send_json(router.clone(), post_json("/api/v1/storage/price/points", None, &ingest_body())).await;
	assert_eq!(status, StatusCode::CREATED, "{body}");
	assert_eq!(key_set(&body), expected(&["aspect", "segment_id", "format_version", "row_count", "null_count", "byte_len", "min_ts", "max_ts", "time_sorted", "segment_ids", "series_id", "tags", "replayed"]), "exactly the ingest keys: {body}");
	assert_eq!(body["aspect"], "price", "{body}");
	assert_eq!(body["format_version"], 5, "a single-block seal: {body}");
	assert_eq!(body["row_count"], 4, "{body}");
	assert_eq!(body["null_count"], 0, "{body}");
	assert!(body["byte_len"].as_u64().is_some_and(|bytes| bytes > 0), "a sealed segment occupies bytes: {body}");
	assert_eq!(body["min_ts"], 0, "{body}");
	assert_eq!(body["max_ts"], 180, "{body}");
	assert_eq!(body["time_sorted"], true, "{body}");
	assert_eq!(body["segment_ids"], serde_json::json!([body["segment_id"]]), "one segment, named twice: {body}");
	assert_eq!(body["series_id"], 0, "the untagged series: {body}");
	assert_eq!(body["tags"], serde_json::json!({}), "{body}");
	assert_eq!(body["replayed"], false, "{body}");
	assert!(headers.get("Idempotency-Replayed").is_none(), "a fresh seal is not a replay");

	// The same batch under the same idempotency key is a replay: `200`,
	// nothing sealed twice, the recorded receipt answered back.
	let key = "fre10b-replay-key";
	let request = Request::builder().method("POST").uri("/api/v1/storage/price/points").header(header::CONTENT_TYPE, "application/json").header("Idempotency-Key", key).body(Body::from(serde_json::to_vec(&ingest_body()).unwrap())).unwrap();
	let (status, _headers, body) = send_json(router.clone(), request).await;
	assert_eq!(status, StatusCode::CREATED, "{body}");
	let request = Request::builder().method("POST").uri("/api/v1/storage/price/points").header(header::CONTENT_TYPE, "application/json").header("Idempotency-Key", key).body(Body::from(serde_json::to_vec(&ingest_body()).unwrap())).unwrap();
	let (status, headers, replay) = send_json(router.clone(), request).await;
	assert_eq!(status, StatusCode::OK, "the replay: {replay}");
	assert_eq!(headers["Idempotency-Replayed"], "true", "the replay is marked in the header");
	assert_eq!(replay["replayed"], true, "{replay}");
	assert_eq!(replay["segment_id"], body["segment_id"], "the recorded receipt, answered back: {replay}");
	assert_eq!(replay["row_count"], 4, "{replay}");

	// Two seals happened, not three: the plain seal plus the keyed seal —
	// the replay added no rows of its own. And the two seals hold the same
	// instants, so the read shows four rows, not eight: the store is
	// last-writer-wins per instant (`segment_store/read.rs`), so sealing
	// the same batch twice rewrites the same points rather than doubling
	// them.
	let (status, _headers, body) = send_json(router, get("/api/v1/storage/price/points?start=0&end=180", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["total"], 4, "the replay sealed nothing twice, and the second seal rewrote the same instants: {body}");
}

#[tokio::test]
async fn the_other_ingest_formats_answer_their_frozen_shapes() {
	let (_dir, router) = seeded_router().await;
	// ILP: the tagged series the rows name, sealed like the JSON rows.
	let (status, _headers, body) = send_json(router.clone(), post_text("/api/v1/storage/price/ilp?field=load&measurement=cpu&precision=s", None, ILP_BODY)).await;
	assert_eq!(status, StatusCode::CREATED, "{body}");
	assert_eq!(body["row_count"], 2, "{body}");
	assert_eq!(body["series_id"], 1, "the first tagged series: {body}");
	assert_eq!(body["tags"], serde_json::json!({ "host": "a" }), "{body}");
	assert_eq!(body["min_ts"], 240, "{body}");
	assert_eq!(body["max_ts"], 300, "{body}");

	// CSV: the export's own shape, read back in.
	let (status, _headers, body) = send_json(router.clone(), post_text("/api/v1/storage/price/csv", None, CSV_BODY)).await;
	assert_eq!(status, StatusCode::CREATED, "{body}");
	assert_eq!(body["row_count"], 2, "{body}");
	assert_eq!(body["min_ts"], 240, "{body}");
	assert_eq!(body["max_ts"], 300, "{body}");

	// Parquet: the typed file this suite builds, sealed like the rest.
	let (status, _headers, body) = send_json(router.clone(), post_bytes("/api/v1/storage/price/parquet", None, "application/vnd.apache.parquet", &parquet_bytes())).await;
	assert_eq!(status, StatusCode::CREATED, "{body}");
	assert_eq!(body["row_count"], 4, "{body}");
	assert_eq!(body["min_ts"], 0, "{body}");
	assert_eq!(body["max_ts"], 180, "{body}");

	// The tagged rows live apart from the untagged ones: the selector names
	// the series, and the answer carries the tags it read.
	let (status, _headers, body) = send_json(router.clone(), get("/api/v1/storage/price/points?start=0&end=300&tag.host=a", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["tags"], serde_json::json!({ "host": "a" }), "{body}");
	assert_eq!(body["total"], 2, "only the tagged rows: {body}");
	let values: Vec<&str> = body["points"].as_array().expect("points is an array").iter().map(|point| point["value"].as_str().expect("a decimal value")).collect();
	assert_eq!(values, ["5", "6"], "{body}");

	// And the untagged read is untouched by the tagged writes. Over the
	// whole window it holds the JSON seal's four rows plus the CSV seal's
	// two — the Parquet seal rewrote the JSON seal's instants rather than
	// doubling them (last-writer-wins per instant).
	let (status, _headers, body) = send_json(router, get("/api/v1/storage/price/points?start=0&end=300", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["tags"], serde_json::json!({}), "{body}");
	assert_eq!(body["total"], 6, "the JSON/Parquet instants plus the CSV rows: {body}");
	let values: Vec<&str> = body["points"].as_array().expect("points is an array").iter().map(|point| point["value"].as_str().expect("a decimal value")).collect();
	assert_eq!(values, ["1", "2", "1.5", "3", "4", "5"], "{body}");
}

// --- Version and Grafana ---------------------------------------------------------

#[tokio::test]
async fn version_answers_its_frozen_shape() {
	let dir = tempfile::tempdir().expect("tempdir");
	let store = Arc::new(SegmentStore::open(dir.path().join("store")).await.expect("store opens"));
	let router = app_with_state(AppState::new().with_store(store).with_serving_scope(ServingScope::new("default", "default", ScopePin::Enforced)));
	let (status, _headers, body) = send_json(router, get("/api/v1/version", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["service", "version", "build", "store", "scope_pin", "exit_codes"]), "exactly the version keys: {body}");
	assert_eq!(body["service"], "weft-server", "{body}");
	assert_eq!(body["version"], VERSION, "{body}");
	assert_eq!(key_set(&body["build"]), expected(&["os", "arch", "family", "env", "target", "rustc", "git_sha", "git_dirty", "pointer_width", "debug_assertions", "profile"]), "exactly the build keys: {body}");
	assert_eq!(body["build"]["os"], std::env::consts::OS, "{body}");
	assert_eq!(body["build"]["arch"], std::env::consts::ARCH, "{body}");
	assert_eq!(body["build"]["family"], std::env::consts::FAMILY, "{body}");
	assert_eq!(body["build"]["pointer_width"], 64, "{body}");
	assert_eq!(body["build"]["profile"], if cfg!(debug_assertions) { "debug" } else { "release" }, "{body}");
	assert_eq!(key_set(&body["store"]), expected(&["configured", "store_uuid", "durability_class", "recorded_scope", "layout_version", "min_read_layout", "min_write_layout", "last_written_layout", "supported_layout", "migrating_to", "applied_through", "writable_by_this_build", "report_only", "incompatible_store", "previous_clean_shutdown"]), "exactly the store-identity keys: {body}");
	assert_eq!(body["store"]["configured"], true, "{body}");
	assert!(body["store"]["store_uuid"].as_str().is_some_and(|uuid| uuid.len() == 36), "the identity /ready reports: {body}");
	assert_eq!(body["store"]["layout_version"], weftdb::SUPPORTED_LAYOUT, "a store this build created is in this build's layout: {body}");
	assert_eq!(body["store"]["writable_by_this_build"], true, "{body}");
	assert_eq!(body["store"]["report_only"], false, "{body}");
	assert_eq!(body["store"]["incompatible_store"], false, "{body}");
	assert_eq!(body["store"]["previous_clean_shutdown"], false, "a store this process opened and never shut down: {body}");
	assert_eq!(key_set(&body["scope_pin"]), expected(&["mode", "serving", "recorded", "matches_store"]), "exactly the scope-pin keys: {body}");
	assert_eq!(body["scope_pin"]["mode"], "enforced", "{body}");
	assert_eq!(body["scope_pin"]["serving"], serde_json::json!({ "database": "default", "subject": "default" }), "{body}");
	assert_eq!(body["scope_pin"]["recorded"], body["scope_pin"]["serving"], "a store created by this scope records it: {body}");
	assert_eq!(body["scope_pin"]["matches_store"], true, "{body}");
	let codes: Vec<i64> = body["exit_codes"]
		.as_array()
		.expect("a table")
		.iter()
		.map(|row| {
			assert_eq!(key_set(row), expected(&["code", "meaning"]), "exactly the exit-code keys: {row}");
			row["code"].as_i64().expect("a code")
		})
		.collect();
	assert_eq!(codes, [0, 1, 2, 65, 70, 75, 78], "plan C7's table: {body}");
}

#[tokio::test]
async fn grafana_answers_its_frozen_shapes() {
	let (_dir, router) = seeded_router().await;
	let (status, _headers, body) = send_json(router.clone(), get("/api/v1/grafana", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body, serde_json::json!({ "status": "ok", "datasource": "weftdb" }), "the connection test: {body}");

	let (status, _headers, body) = send_json(router.clone(), post_json("/api/v1/grafana/metrics", None, &serde_json::json!({}))).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	let options = body.as_array().expect("an array of options");
	assert_eq!(options.len(), 1, "{body}");
	assert_eq!(key_set(&options[0]), expected(&["label", "value", "payloads"]), "exactly the picker keys: {body}");
	assert_eq!(options[0]["label"], "price", "{body}");
	assert_eq!(options[0]["value"], "price", "{body}");
	let payloads = options[0]["payloads"].as_array().expect("payloads is an array");
	assert_eq!(payloads.len(), 1, "{body}");
	assert_eq!(payloads[0]["label"], "Reduction", "{body}");
	assert_eq!(payloads[0]["name"], "agg", "{body}");
	assert_eq!(payloads[0]["type"], "select", "{body}");
	assert_eq!(payloads[0]["placeholder"], "avg", "{body}");
	let offered: Vec<&str> = payloads[0]["options"].as_array().expect("options is an array").iter().map(|option| option["value"].as_str().expect("a value")).collect();
	assert_eq!(offered, ["avg", "min", "max", "sum", "count", "first", "last", "p50", "p95", "p99", "twa"], "the reductions the picker offers: {body}");

	let (status, _headers, body) = send_json(router, post_json("/api/v1/grafana/query", None, &grafana_query_body())).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	let series = body.as_array().expect("an array of series");
	assert_eq!(series.len(), 1, "{body}");
	assert_eq!(key_set(&series[0]), expected(&["target", "refId", "datapoints"]), "exactly the series keys: {body}");
	assert_eq!(series[0]["target"], "price", "{body}");
	assert_eq!(series[0]["refId"], "A", "{body}");
	assert_eq!(series[0]["datapoints"], serde_json::json!([[1.0, 0], [2.0, 60000], [1.5, 120_000], [3.0, 180_000]]), "one minute-bucket per seed row, as [value, epoch_ms]: {body}");
}

// --- Maintenance ------------------------------------------------------------------

#[tokio::test]
async fn maintenance_answers_its_frozen_shapes() {
	let (_dir, router) = seeded_router().await;
	// The per-aspect reconcile on ordered data: the pass runs, rewrites
	// nothing, and reports the order health it found.
	let (status, _headers, body) = send_json(router.clone(), post_empty("/api/v1/storage/price/reconcile", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["aspect", "mode", "triggered", "reconciled", "cold_reconciled", "hot_reconciled", "unsorted_segments", "overlapping_segments"]), "exactly the reconcile keys: {body}");
	assert_eq!(body["aspect"], "price", "{body}");
	assert_eq!(body["mode"], "threshold", "{body}");
	assert_eq!(body["triggered"], true, "{body}");
	assert_eq!(body["reconciled"], 0, "ordered data has nothing to rewrite: {body}");
	assert_eq!(body["cold_reconciled"], 0, "{body}");
	assert_eq!(body["hot_reconciled"], 0, "{body}");
	assert_eq!(body["unsorted_segments"], 0, "{body}");
	assert_eq!(body["overlapping_segments"], 0, "{body}");

	// The store-wide sweep: the same report, one row per aspect, with the
	// sweep's own threshold and the failure lists it must carry.
	let (status, _headers, body) = send_json(router.clone(), post_empty("/api/v1/storage/reconcile", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["threshold", "mode", "aspects_scanned", "aspects_reconciled", "segments_reconciled", "cold_reconciled", "hot_reconciled", "unsorted_segments", "overlapping_segments", "failed", "tripped"]), "exactly the sweep keys: {body}");
	assert_eq!(body["threshold"], 1, "the default threshold: {body}");
	assert_eq!(body["mode"], "threshold", "{body}");
	assert_eq!(body["aspects_scanned"], 1, "{body}");
	assert_eq!(body["aspects_reconciled"], 0, "{body}");
	assert_eq!(body["segments_reconciled"], 0, "{body}");
	assert_eq!(body["unsorted_segments"], 0, "{body}");
	assert_eq!(body["overlapping_segments"], 0, "{body}");
	assert_eq!(body["failed"], serde_json::json!([]), "{body}");
	assert_eq!(body["tripped"], serde_json::json!([]), "{body}");

	// Squash and compact on one segment: the pass runs, removes nothing,
	// and reports the count it leaves behind.
	let (status, _headers, body) = send_json(router.clone(), post_empty("/api/v1/storage/price/squash", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["aspect", "triggered", "removed", "segment_count"]), "exactly the squash keys: {body}");
	assert_eq!(body["triggered"], true, "{body}");
	assert_eq!(body["removed"], 0, "one segment folds into one: {body}");
	assert_eq!(body["segment_count"], 1, "{body}");

	let (status, _headers, body) = send_json(router.clone(), post_empty("/api/v1/storage/price/compact?target_rows=10", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["aspect", "removed", "segment_count"]), "exactly the compact keys: {body}");
	assert_eq!(body["removed"], 0, "{body}");
	assert_eq!(body["segment_count"], 1, "{body}");

	// fsck on a clean store: the recovery report's counts, all quiet.
	let (status, _headers, body) = send_json(router.clone(), post_empty("/api/v1/storage/fsck", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["checked", "quarantined", "suspected", "rebound", "seeded", "purged", "orphans", "adopted", "held", "series_rebound", "held_cleared", "skipped_outside_segments", "incompatible_store", "report_only"]), "exactly the recovery-report keys: {body}");
	assert_eq!(body["quarantined"], 0, "{body}");
	assert_eq!(body["suspected"], 0, "{body}");
	assert_eq!(body["incompatible_store"], false, "{body}");
	assert_eq!(body["report_only"], false, "{body}");

	// Quarantine: nothing quarantined, nothing to purge.
	let (status, _headers, body) = send_json(router.clone(), get("/api/v1/storage/quarantine", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body, serde_json::json!([]), "the listing is the records, not an envelope: {body}");

	let (status, _headers, body) = send_json(router, post_empty("/api/v1/storage/quarantine/purge-expired", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["purged"]), "exactly the purge keys: {body}");
	assert_eq!(body["purged"], 0, "{body}");
}

// --- Backup and the restore drill ---------------------------------------------------

#[tokio::test]
async fn backup_and_restore_drill_answer_their_frozen_shapes() {
	let (_dir, router) = seeded_router().await;
	let (status, _headers, body) = send_json(router.clone(), post_empty("/api/v1/storage/backup?label=fre10b-shape", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["dir", "databases", "total_rows", "total_bytes", "verify", "duration_ms"]), "exactly the backup keys: {body}");
	assert!(body["dir"].as_str().is_some_and(|dir| dir.ends_with("fre10b-shape")), "the snapshot lands under its label: {body}");
	assert_eq!(body["verify"], "source", "the quiescent-store cross-check, by default: {body}");
	assert!(body["total_bytes"].as_u64().is_some_and(|bytes| bytes > 0), "four snapshot files occupy bytes: {body}");
	assert!(body["duration_ms"].as_u64().is_some(), "a duration, not a missing field: {body}");
	let databases = body["databases"].as_array().expect("databases is an array");
	assert_eq!(databases.len(), 4, "{body}");
	let mut names: Vec<&str> = databases.iter().map(|db| db["name"].as_str().expect("a name")).collect();
	names.sort_unstable();
	assert_eq!(names, ["aspect_catalog.db", "catalog.db", "metadata.db", "segment_index.db"], "the four control-plane databases, named as the files they are: {body}");
	let mut summed_rows = 0;
	for db in databases {
		assert_eq!(key_set(db), expected(&["name", "tables", "rows", "bytes"]), "exactly the per-database keys: {db}");
		summed_rows += db["rows"].as_i64().expect("a row count");
	}
	assert_eq!(body["total_rows"], summed_rows, "the total is the databases' sum: {body}");
	assert!(body["total_rows"].as_i64().is_some_and(|rows| rows > 0), "the control plane has rows to verify: {body}");
	let backup_rows = body["total_rows"].as_i64().expect("a row count");

	// The drill rehearses that backup: same rows verified readable, the
	// same label echoed, and `restorable` true — the answer stands.
	let (status, _headers, body) = send_json(router, post_empty("/api/v1/storage/restore/drill?label=fre10b-shape", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(key_set(&body), expected(&["label", "databases", "total_rows", "total_bytes", "duration_ms", "restorable", "cleanup_error"]), "exactly the drill keys: {body}");
	assert_eq!(body["label"], "fre10b-shape", "{body}");
	assert_eq!(body["total_rows"], backup_rows, "the drill verifies the backup's rows: {body}");
	assert_eq!(body["restorable"], true, "{body}");
	assert_eq!(body["cleanup_error"], serde_json::Value::Null, "the rehearsal copy was removed: {body}");
	assert_eq!(body["databases"].as_array().expect("databases is an array").len(), 4, "{body}");
}

// --- No store, and the refusals ------------------------------------------------------

#[tokio::test]
async fn without_a_store_every_storage_and_operator_route_is_unconfigured() {
	// The whole FRE-10b surface, minus the two routes that answer without
	// a store by design: the version route names the missing store, and the
	// Grafana connection test answers whether the process is up.
	let router = app_with_state(AppState::new());
	let parquet = parquet_bytes();
	let entries: Vec<RouteEntry> = [Scope::Read, Scope::Ingest, Scope::Declare, Scope::Maintain, Scope::Backup].iter().flat_map(|scope| family(*scope)).collect();
	assert_eq!(entries.len(), 47, "the whole FRE-10b surface, enumerated from the registry");
	for entry in &entries {
		if entry.spec.operation_id == "get_version" || entry.spec.operation_id == "grafana_test" {
			let (status, _headers, _body) = send(router.clone(), family_request(entry, None, &parquet)).await;
			assert_eq!(status, StatusCode::OK, "{} {} answers without a store", entry.method, entry.path);
			continue;
		}
		let (status, _headers, body) = send_json(router.clone(), family_request(entry, None, &parquet)).await;
		assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{} {}: {body}", entry.method, entry.path);
		assert_eq!(body["code"], "unconfigured", "{} {}: {body}", entry.method, entry.path);
	}
}

#[tokio::test]
async fn refusals_carry_the_contract_codes() {
	let (_dir, router) = seeded_router().await;
	// An aspect the store never declared: every read, write and operator
	// pass names it the same way.
	for uri in ["/api/v1/storage/nope/points?start=0&end=180", "/api/v1/storage/nope/range?start=0&end=180", "/api/v1/storage/nope/at?t=60", "/api/v1/storage/nope/schema", "/api/v1/storage/nope/downsample?start=0&end=180", "/api/v1/storage/nope/interpolate?start=0&end=180"] {
		let (status, _headers, body) = send_json(router.clone(), get(uri, None)).await;
		assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
		assert_eq!(body["code"], "aspect_not_found", "{uri}: {body}");
	}
	// The one exception, by design: the stats route answers the empty
	// rollup for an aspect with no sealed segments (`storage.rs`:
	// "reports the empty rollup (all zeros), not an error"), not a 404.
	let (status, _headers, body) = send_json(router.clone(), get("/api/v1/storage/nope/stats", None)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["total_rows"], 0, "{body}");
	assert_eq!(body["segment_count"], 0, "{body}");
	assert_eq!(body["time_range"], serde_json::Value::Null, "{body}");
	assert_eq!(body["value_range"], serde_json::Value::Null, "{body}");
	for uri in ["/api/v1/storage/nope/reconcile", "/api/v1/storage/nope/squash", "/api/v1/storage/nope/compact?target_rows=10"] {
		let (status, _headers, body) = send_json(router.clone(), post_empty(uri, None)).await;
		assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
		assert_eq!(body["code"], "aspect_not_found", "{uri}: {body}");
	}
	let (status, _headers, body) = send_json(router.clone(), post_json("/api/v1/storage/nope/points", None, &ingest_body())).await;
	assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
	assert_eq!(body["code"], "aspect_not_found", "{body}");

	// A parameter the endpoint cannot read, and a pass missing its required
	// parameter: both are the caller's `400`, with the §4 code.
	let (status, _headers, body) = send_json(router.clone(), get("/api/v1/storage/price/points?start=soon&end=180", None)).await;
	assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
	assert_eq!(body["code"], "bad_request", "{body}");
	let (status, _headers, body) = send_json(router.clone(), get("/api/v1/storage/price/points?start=0&end=180&bogus=1", None)).await;
	assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
	assert_eq!(body["code"], "bad_request", "{body}");
	let (status, _headers, body) = send_json(router.clone(), post_empty("/api/v1/storage/price/compact", None)).await;
	assert_eq!(status, StatusCode::BAD_REQUEST, "compact without target_rows: {body}");
	assert_eq!(body["code"], "bad_request", "{body}");
	let (status, _headers, body) = send_json(router.clone(), get("/api/v1/storage/price/at-multi?t=1x", None)).await;
	assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
	assert_eq!(body["code"], "bad_request", "{body}");
	let (status, _headers, body) = send_json(router.clone(), get("/api/v1/storage/price/value-range?lo=soon&hi=3", None)).await;
	assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
	assert_eq!(body["code"], "bad_request", "{body}");

	// A declaration the schema grammar refuses, and a backup label the
	// label grammar refuses (plus the daemon's reserved form).
	let mut bad_declare = declare_body();
	bad_declare["physical_type"] = serde_json::json!("no-such-encoding");
	let (status, _headers, body) = send_json(router.clone(), post_json("/api/v1/storage/aspects", None, &bad_declare)).await;
	assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
	assert_eq!(body["code"], "bad_request", "{body}");
	let (status, _headers, body) = send_json(router.clone(), post_empty("/api/v1/storage/backup?label=../escape", None)).await;
	assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
	assert_eq!(body["code"], "bad_request", "{body}");
	let (status, _headers, body) = send_json(router.clone(), post_empty("/api/v1/storage/backup?label=backup-123", None)).await;
	assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
	assert_eq!(body["code"], "bad_request", "{body}");

	// Operator routes naming resources that do not exist.
	let (status, _headers, body) = send_json(router.clone(), post_empty("/api/v1/storage/restore/drill?label=fre10b-no-such-backup", None)).await;
	assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
	assert_eq!(body["code"], "aspect_not_found", "{body}");
	let (status, _headers, body) = send_json(router.clone(), post_empty("/api/v1/storage/quarantine/price/0/0/release", None)).await;
	assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
	assert_eq!(body["code"], "aspect_not_found", "{body}");
	let (status, _headers, body) = send_json(router.clone(), post_empty("/api/v1/storage/quarantine/price/0/0/purge", None)).await;
	assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
	assert_eq!(body["code"], "aspect_not_found", "{body}");

	// A method the route does not serve: the gate's `405`, with `Allow`
	// naming what the path does serve.
	let (status, headers, body) = send_json(router, delete("/api/v1/storage/price/points", None)).await;
	assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");
	assert_eq!(body["code"], "method_not_allowed", "{body}");
	assert_eq!(headers[header::ALLOW], "GET, POST", "both verbs the path serves: {body}");
}
