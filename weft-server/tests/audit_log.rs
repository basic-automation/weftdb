#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]
//! SEC-9's audit log, end to end over HTTP: the attempt/outcome pair per
//! audited operation, the `weft_audit` events (committed ingest, auth
//! denial), reads leaving no trace, and the fail-closed refusal when the
//! log cannot be written.

use std::{path::PathBuf, sync::Arc};

use axum::{
	body::Body, http::{header, Request, StatusCode}
};
use tower::ServiceExt as _;
use weft_server::{app_with_state, AppState, AuditLog, AuthConfig, KeyStore};
use weftdb::SegmentStore;

fn read_records(path: &std::path::Path) -> Vec<serde_json::Value> {
	let text = std::fs::read_to_string(path).expect("the audit file exists");
	text.lines().map(|line| serde_json::from_str(line).expect("each line is one JSON record")).collect()
}

async fn send(router: axum::Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
	let response = router.oneshot(request).await.unwrap();
	let status = response.status();
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
	(status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

fn declare_request() -> Request<Body> {
	let body = serde_json::json!({ "name": "price", "physical_type": "f64", "timestamp_unit": "seconds" });
	Request::builder().method("POST").uri("/api/v1/storage/aspects").header(header::CONTENT_TYPE, "application/json").body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap()
}

async fn router_with_store(audit_path: PathBuf) -> (tempfile::TempDir, axum::Router, PathBuf) {
	let dir = tempfile::tempdir().expect("tempdir");
	let store = Arc::new(SegmentStore::open(dir.path().join("store")).await.expect("store opens"));
	let audit = Arc::new(AuditLog::open(&audit_path).expect("audit opens"));
	let router = app_with_state(AppState::new().with_store(store).with_audit_log(audit));
	(dir, router, audit_path)
}

/// A mutating call writes exactly two records — attempt then outcome,
/// joined by the audit id — and a read writes none.
#[tokio::test]
async fn a_declare_writes_an_attempt_and_an_outcome_and_a_read_writes_nothing() {
	let dir = tempfile::tempdir().expect("tempdir");
	let audit_path = dir.path().join("audit.jsonl");
	let (_store_dir, router, audit_path) = router_with_store(audit_path).await;

	let (status, _) = send(router.clone(), declare_request()).await;
	assert_eq!(status, StatusCode::CREATED);
	// A read of the catalog the declare just wrote: not audited.
	let (status, _) = send(router.clone(), Request::builder().method("GET").uri("/api/v1/storage/aspects").body(Body::empty()).unwrap()).await;
	assert_eq!(status, StatusCode::OK);

	let records = read_records(&audit_path);
	assert_eq!(records.len(), 2, "one attempt + one outcome, and nothing for the read: {records:?}");
	assert_eq!(records[0]["kind"], "attempt");
	assert_eq!(records[0]["operation"], "storage_aspect_declare");
	assert_eq!(records[0]["method"], "POST");
	assert_eq!(records[0]["principal"], serde_json::Value::Null, "no keys configured: the anonymous principal");
	assert_eq!(records[1]["kind"], "outcome");
	assert_eq!(records[1]["audit_id"], records[0]["seq"]);
	assert_eq!(records[1]["status"], 201);
	assert_eq!(records[1]["result"], "ok");
}

/// A committed ingest adds its `weft_audit` event between the ingest
/// route's own attempt and outcome records; the event carries the
/// fields D-S14 reserved for it.
#[tokio::test]
async fn a_committed_ingest_emits_its_event_into_the_file() {
	let dir = tempfile::tempdir().expect("tempdir");
	let audit_path = dir.path().join("audit.jsonl");
	let (_store_dir, router, audit_path) = router_with_store(audit_path).await;
	let (status, _) = send(router.clone(), declare_request()).await;
	assert_eq!(status, StatusCode::CREATED);

	let body = serde_json::json!({ "points": [{ "timestamp": 0, "value": "1.5" }, { "timestamp": 10, "value": "2.5" }] });
	let request = Request::builder().method("POST").uri("/api/v1/storage/price/points").header(header::CONTENT_TYPE, "application/json").body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap();
	let (status, _) = send(router.clone(), request).await;
	assert_eq!(status, StatusCode::CREATED);

	let records = read_records(&audit_path);
	let kinds: Vec<&str> = records.iter().map(|record| record["kind"].as_str().unwrap()).collect();
	assert_eq!(kinds, ["attempt", "outcome", "attempt", "event", "outcome"], "{records:?}");
	let event = &records[3];
	assert_eq!(event["event"], "ingest");
	assert_eq!(event["aspect"], "price");
	assert_eq!(event["format"], "json");
	assert_eq!(event["rows"], 2);
	assert_eq!(records[2]["operation"], "storage_ingest_points");
	assert_eq!(records[4]["audit_id"], records[2]["seq"]);
}

/// An auth denial is an event, not an attempt: the operation was never
/// attempted against the store.
#[tokio::test]
async fn an_auth_denial_is_an_event_not_an_attempt() {
	let dir = tempfile::tempdir().expect("tempdir");
	let audit_path = dir.path().join("audit.jsonl");
	let token = "weft_admin_token_for_audit_tests";
	let keys = format!("{{\"version\": 1, \"keys\": [{{\"id\": \"operator\", \"role\": \"admin\", \"key_hash\": \"{}\"}}]}}", weft_server::auth::format_key_hash(token));
	let store = KeyStore::parse(&keys).expect("keys parse");
	let audit = Arc::new(AuditLog::open(&audit_path).expect("audit opens"));
	let router = app_with_state(AppState::new().with_auth(AuthConfig::with_keys(store)).with_audit_log(audit));

	// No key on a read route: refused 401, recorded as a denial event.
	let (status, body) = send(router.clone(), Request::builder().method("GET").uri("/api/v1/storage/stats").body(Body::empty()).unwrap()).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED);
	assert_eq!(body["code"], "unauthenticated");
	let records = read_records(&audit_path);
	assert_eq!(records.len(), 1, "{records:?}");
	assert_eq!(records[0]["kind"], "event");
	assert_eq!(records[0]["event"], "auth_denied");
	assert_eq!(records[0]["code"], "unauthenticated");
	assert_eq!(records[0]["operation"], "storage_stats");

	// The admin key declaring an aspect: the attempt names the principal.
	let request = Request::builder().method("POST").uri("/api/v1/storage/aspects").header(header::AUTHORIZATION, format!("Bearer {token}")).header(header::CONTENT_TYPE, "application/json").body(Body::from(serde_json::to_vec(&serde_json::json!({"name": "price", "physical_type": "f64", "timestamp_unit": "seconds"})).unwrap())).unwrap();
	let (status, _) = send(router.clone(), request).await;
	assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "no store configured — but the audited attempt still lands");
	let records = read_records(&audit_path);
	let attempt = records.iter().find(|record| record["kind"] == "attempt").expect("the declare attempt");
	assert_eq!(attempt["principal"], "operator");
	assert_eq!(attempt["role"], "admin");
}

/// Fail-closed: a log whose writes fail refuses the audited operation
/// with `503 audit_unavailable`, and the handler never runs — the
/// aspect is not declared. The write failure is a file-size rlimit of
/// zero in a child process (the stand-in for a disk that fills under a
/// running server; an rlimit is process-wide, so the scenario runs in
/// a child — the `WEFT_TEST_*` pattern the crash matrices use).
#[cfg(unix)]
#[tokio::test]
async fn a_log_that_cannot_be_written_refuses_the_operation() {
	const CHILD_ENV: &str = "WEFT_TEST_AUDIT_REFUSAL_CHILD";
	if std::env::var_os(CHILD_ENV).is_none() {
		let exe = std::env::current_exe().expect("the test binary's path");
		let status = std::process::Command::new(exe).args(["--exact", "a_log_that_cannot_be_written_refuses_the_operation", "--nocapture"]).env(CHILD_ENV, "1").status().expect("the child runs");
		assert!(status.success(), "the child verdict: {status}");
		return;
	}
	// The child: the store opens first (its writes are uncapped), then
	// the rlimit lands and only the audit writes fail.
	let dir = tempfile::tempdir().expect("tempdir");
	let store = Arc::new(SegmentStore::open(dir.path().join("store")).await.expect("store opens"));
	let mut saved = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
	unsafe {
		libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
		assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &raw mut saved), 0);
		let limit = libc::rlimit { rlim_cur: 0, rlim_max: saved.rlim_max };
		assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &raw const limit), 0, "the rlimit is set");
	}
	let audit = Arc::new(AuditLog::open(&dir.path().join("audit.jsonl")).expect("the audit file opens; only its writes are capped"));
	let router = app_with_state(AppState::new().with_store(store).with_audit_log(audit));

	let (status, body) = send(router.clone(), declare_request()).await;
	assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
	assert_eq!(body["code"], "audit_unavailable", "{body}");

	// The handler never ran: the catalog is still empty. (The read is
	// not audited, so it answers normally off the same broken log.)
	let (status, body) = send(router.clone(), Request::builder().method("GET").uri("/api/v1/storage/aspects").body(Body::empty()).unwrap()).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(body["aspects"].as_array().map(Vec::len), Some(0), "{body}");

	// Restore the cap before the store's drop writes its shutdown mark.
	unsafe {
		assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &raw const saved), 0, "the rlimit is restored");
	}
}
