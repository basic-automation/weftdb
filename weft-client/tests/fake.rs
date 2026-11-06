//! The client's own behaviour against a fake server: what it sends, how it follows cursors,
//! when it retries, and what it refuses to believe. The wire contract against the real
//! server is `tests/live.rs`.
#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]

use std::sync::{
	atomic::{AtomicUsize, Ordering}, Arc, Mutex
};

use axum::{
	extract::{Query, State}, http::{HeaderMap, StatusCode}, response::IntoResponse, routing::{get, post}, Json, Router
};
use futures::TryStreamExt as _;
use serde_json::{json, Value};
use weftdb_client::{Client, ErrorKind, Page, WriteOptions};

#[derive(Clone, Default)]
struct Seen {
	hits: Arc<AtomicUsize>,
	headers: Arc<Mutex<Vec<HeaderMap>>>,
	bodies: Arc<Mutex<Vec<Value>>>,
}

async fn serve(router: Router) -> String {
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
	format!("http://{addr}")
}

fn overloaded() -> axum::response::Response {
	(StatusCode::SERVICE_UNAVAILABLE, [("retry-after", "0"), ("x-request-id", "req-busy")], Json(json!({"error": "busy", "code": "overloaded", "request_id": "req-busy"}))).into_response()
}

#[tokio::test]
async fn an_overloaded_read_is_retried_and_a_write_is_not() {
	let seen = Seen::default();
	let router = Router::new()
		.route(
			"/health",
			get(|State(seen): State<Seen>| async move {
				if seen.hits.fetch_add(1, Ordering::SeqCst) < 2 {
					overloaded()
				} else {
					Json(json!({"status": "ok"})).into_response()
				}
			}),
		)
		.route(
			"/api/v1/storage/fsck",
			post(|State(seen): State<Seen>| async move {
				seen.hits.fetch_add(1, Ordering::SeqCst);
				overloaded()
			}),
		)
		.with_state(seen.clone());
	let base = serve(router).await;
	let client = Client::new(&base).unwrap();
	assert_eq!(client.health().await.unwrap()["status"], "ok");
	assert_eq!(seen.hits.load(Ordering::SeqCst), 3, "two refusals, then the answer");

	seen.hits.store(0, Ordering::SeqCst);
	let error = client.fsck().await.unwrap_err();
	assert_eq!(error.kind(), ErrorKind::Overloaded);
	assert_eq!(error.retry_after(), Some(std::time::Duration::ZERO), "a Retry-After of 0 is a value, not an absence");
	assert_eq!(error.request_id(), Some("req-busy"));
	assert_eq!(seen.hits.load(Ordering::SeqCst), 1, "a write is never retried by the client");

	// A Retry-After too large for a Duration is no wait at all rather than a panic.
	let router = Router::new().route("/health", get(|| async { (StatusCode::SERVICE_UNAVAILABLE, [("retry-after", "1e300")], Json(json!({"error": "busy", "code": "overloaded"}))).into_response() }));
	let huge = Client::builder(serve(router).await).max_retries(0).build().unwrap().health().await.unwrap_err();
	assert_eq!((huge.kind(), huge.retry_after()), (ErrorKind::Overloaded, None));

	let no_retries = Client::builder(&base).max_retries(0).build().unwrap();
	seen.hits.store(0, Ordering::SeqCst);
	assert_eq!(no_retries.health().await.unwrap_err().kind(), ErrorKind::Overloaded);
}

#[tokio::test]
async fn the_key_and_the_idempotency_key_travel_as_headers() {
	let seen = Seen::default();
	let router = Router::new()
		.route(
			"/api/v1/storage/{aspect}/points",
			post(|State(seen): State<Seen>, headers: HeaderMap, Json(body): Json<Value>| async move {
				seen.headers.lock().unwrap().push(headers);
				seen.bodies.lock().unwrap().push(body);
				Json(json!({"aspect": "p", "segment_id": 7, "format_version": 3, "row_count": 2, "null_count": 1, "byte_len": 10, "min_ts": 1, "max_ts": 2, "time_sorted": true, "segment_ids": [7], "replayed": false}))
			}),
		)
		.with_state(seen.clone());
	let client = Client::builder(serve(router).await).api_key("weft_k1").build().unwrap();
	let summary = client.write_points("p", [(1, Some("1.10")), (2, None)], &WriteOptions::idempotent("batch-9").require_sorted()).await.unwrap();
	assert_eq!((summary.row_count, summary.null_count, summary.replayed), (2, 1, false));
	let headers = seen.headers.lock().unwrap()[0].clone();
	assert_eq!(headers["authorization"], "Bearer weft_k1");
	assert_eq!(headers["idempotency-key"], "batch-9");
	assert!(headers["user-agent"].to_str().unwrap().starts_with("weft-client/"));
	assert_eq!(seen.bodies.lock().unwrap()[0], json!({"points": [{"timestamp": 1, "value": "1.10"}, {"timestamp": 2, "value": null}], "require_sorted": true}), "values travel as the decimal text that was typed");
}

#[tokio::test]
async fn a_stream_follows_every_cursor_to_the_end() {
	let seen = Seen::default();
	let router = Router::new()
		.route(
			"/api/v1/storage/{aspect}/points",
			get(|State(seen): State<Seen>, Query(query): Query<std::collections::HashMap<String, String>>| async move {
				seen.hits.fetch_add(1, Ordering::SeqCst);
				assert_eq!(query["limit"], "2");
				let (rows, next): (Vec<i64>, Option<&str>) = match query.get("cursor").map(String::as_str) {
					None => (vec![1, 2], Some("c2")),
					Some("c2") => (vec![3, 4], Some("c3")),
					Some("c3") => (vec![5], None),
					Some(other) => panic!("unknown cursor {other}"),
				};
				let points: Vec<Value> = rows.iter().map(|t| json!({"timestamp": t, "value": format!("{t}.5")})).collect();
				let mut page = json!({"aspect": "p", "time_unit": "seconds", "total": 5, "count": points.len(), "offset": 0, "limit": 2, "points": points});
				if let Some(next) = next {
					page["next_cursor"] = json!(next);
				}
				Json(page)
			}),
		)
		.with_state(seen.clone());
	let client = Client::new(serve(router).await).unwrap();
	let rows: Vec<_> = client.points_stream("p", 0, 10, 2).try_collect().await.unwrap();
	assert_eq!(rows.iter().map(|p| p.timestamp).collect::<Vec<_>>(), [1, 2, 3, 4, 5]);
	assert_eq!(rows[4].value.as_ref().unwrap().to_string(), "5.5");
	assert_eq!(seen.hits.load(Ordering::SeqCst), 3, "one request per page, none after the last");
	let page = client.points("p", 0, 10, &Page::after("c3", 2)).await.unwrap();
	assert_eq!((page.count, page.next_cursor), (1, None));
}

#[tokio::test]
async fn a_bench_run_whose_bytes_do_not_hash_to_its_id_is_refused() {
	let manifest = br#"{"schema_version":23}"#;
	let good = sha256_hex(manifest);
	let router = Router::new().route("/bench/runs/{id}", get(|| async { (StatusCode::OK, [("content-type", "application/json")], r#"{"schema_version":23}"#) }));
	let client = Client::new(serve(router).await).unwrap();
	assert_eq!(client.bench_run(&good).await.unwrap(), manifest);
	let error = client.bench_run(&"0".repeat(64)).await.unwrap_err();
	assert_eq!(error.kind(), ErrorKind::Integrity, "{error}");
}

#[tokio::test]
async fn a_refusal_that_is_not_json_still_names_its_status() {
	let router = Router::new().route("/health", get(|| async { (StatusCode::BAD_GATEWAY, "proxy says no") }));
	let error = Client::new(serve(router).await).unwrap().health().await.unwrap_err();
	assert_eq!((error.kind(), error.status(), error.message()), (ErrorKind::Server, Some(502), "proxy says no"));
}

#[tokio::test]
async fn an_unreachable_server_is_a_transport_error() {
	let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
	let addr = listener.local_addr().unwrap();
	drop(listener);
	let error = Client::new(format!("http://{addr}")).unwrap().health().await.unwrap_err();
	assert_eq!(error.kind(), ErrorKind::Transport, "{error}");
	assert!(std::error::Error::source(&error).is_some(), "the cause is kept");
}

fn sha256_hex(bytes: &[u8]) -> String {
	use sha2::Digest as _;
	format!("{:x}", sha2::Sha256::digest(bytes))
}
