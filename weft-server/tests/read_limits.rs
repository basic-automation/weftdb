//! ROB-12: the read ceilings (decision 12) are wire behaviour, not a config
//! page — a request whose result shape is too big costs a refusal, not a scan.
//!
//! Before this slice a stored read asked for whatever its parameters named.
//! `?limit=100000000` was a legal page, a `resolution=1s` downsample over stored
//! history was a legal grid, and a client that named no page size at all was
//! served the entire window. Each of those is a request the server would lose an
//! afternoon to, and none of them is a request anyone meant to send.
//!
//! The five ceilings and what each one costs the client:
//!
//! - a page over `WEFT_MAX_PAGE_ROWS` — `422 result_too_large`, refused before the
//!   store is opened, since a page the server will not serve is work it can skip;
//! - a window over `WEFT_MAX_READ_ROWS` — `422` likewise, measured on the rows the
//!   read names rather than the rows the page shows, because the store materializes
//!   the whole window a read asks for;
//! - a grid over `WEFT_MAX_DOWNSAMPLE_BUCKETS` — `422` from the request's own bounds
//!   when it names both, and from the grid actually produced when it names neither
//!   (an unbounded window is as wide as the stored history, which only the read knows);
//! - an `at-multi` batch over `WEFT_MAX_AT_MULTI_INSTANTS` — `422` on the parsed list;
//! - a read still running when `WEFT_READ_TIMEOUT_SECS` expires — `504 query_timeout`.
//!
//! The default page is pinned here too, because it is now part of the API contract:
//! a read that names no page size is a paged read, not a whole-window read, and an
//! operator who lowers the ceiling below the default gets pages that small rather
//! than the default anyway.

use std::time::Duration;

use axum::{
	body::Body, http::{Request, StatusCode}, Router
};
use bigdecimal::BigDecimal;
use tower::ServiceExt as _;
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weft_server::{app_with_state, AppState, ReadLimits};
use weftdb::SegmentStore;

/// A server's worth of rows: twelve seconds of one aspect, sorted, so every
/// ceiling below is crossed by a parameter and not by the data being odd.
const ROWS: usize = 12;

fn schema() -> AspectSchema {
	AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds)
}

/// A store with `temperature` holding [`ROWS`] rows at seconds 1..=[`ROWS`].
async fn store(dir: &std::path::Path) -> std::sync::Arc<SegmentStore> {
	let store = SegmentStore::open(dir).await.expect("opens the store");
	store.declare("temperature", &schema()).await.expect("declares");
	let timestamps: Vec<i64> = (1..=i64::try_from(ROWS).expect("rows fit an i64")).collect();
	let values: Vec<BigDecimal> = timestamps.iter().map(|t| BigDecimal::from(*t)).collect();
	store.seal("temperature", &schema(), &timestamps, &values).await.expect("seals");
	std::sync::Arc::new(store)
}

/// A router serving that store under `limits`.
async fn router(limits: ReadLimits) -> (tempfile::TempDir, Router) {
	let dir = tempfile::TempDir::new().expect("tempdir");
	let store = store(dir.path()).await;
	(dir, app_with_state(AppState::new().with_store(store).with_read_limits(limits)))
}

/// `GET uri`, parsed as the JSON envelope every one of these answers is.
async fn get(router: &Router, uri: &str) -> (StatusCode, serde_json::Value) {
	let response = router.clone().oneshot(Request::builder().uri(uri).body(Body::empty()).expect("builds the request")).await.expect("answers");
	let status = response.status();
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("reads the body");
	(status, serde_json::from_slice(&bytes).expect("a JSON body"))
}

/// The refusal shape a ceiling is required to produce, checked in one place so
/// every ceiling below pins the same contract: the status the error table gives
/// `result_too_large`, a machine-readable code, and the limit the caller hit.
fn assert_refused(status: StatusCode, body: &serde_json::Value, limit: usize, variable: &str) {
	assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
	assert_eq!(body["code"], "result_too_large", "{body}");
	assert_eq!(body["limit"], limit, "the refusal carries the ceiling it hit: {body}");
	let error = body["error"].as_str().unwrap_or_default();
	assert!(error.contains(variable), "{error} must name {variable}");
}

/// A page over the ceiling is refused with the ceiling in it, and the same
/// request one row smaller is served — the cap is a bound, not a refusal to page.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_over_the_ceiling_is_refused_and_one_at_it_is_served() {
	let (_dir, router) = router(ReadLimits::new(5, 10_000, 10_000, 10_000, Duration::from_secs(30))).await;

	let (status, body) = get(&router, "/api/v1/storage/temperature/points?start=1&end=12&limit=6").await;
	assert_refused(status, &body, 5, "WEFT_MAX_PAGE_ROWS");

	let (status, body) = get(&router, "/api/v1/storage/temperature/points?start=1&end=12&limit=5").await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["count"], 5, "{body}");
	assert_eq!(body["total"], ROWS, "the window's width is still reported: {body}");
	assert!(body["next_cursor"].is_string(), "rows remain, so a cursor is offered: {body}");
}

/// A read that names no page size is a paged read. Twelve rows are under the
/// 10,000-row default, so this server's ceiling is what bounds it — and the
/// whole window is still reachable, one cursor at a time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_that_names_no_page_size_pages_from_the_default_and_walks_the_window() {
	let (_dir, router) = router(ReadLimits::new(5, 10_000, 10_000, 10_000, Duration::from_secs(30))).await;

	let mut seen = Vec::new();
	let mut cursor: Option<String> = None;
	for page in 0..10 {
		let uri = match &cursor {
			Some(token) => format!("/api/v1/storage/temperature/points?start=1&end=12&cursor={token}"),
			None => "/api/v1/storage/temperature/points?start=1&end=12".to_string(),
		};
		let (status, body) = get(&router, &uri).await;
		assert_eq!(status, StatusCode::OK, "page {page}: {body}");
		assert_eq!(body["limit"], 5, "the page served is the clamped default, and it is reported: {body}");
		for row in body["points"].as_array().unwrap_or(&Vec::new()) {
			seen.push(row["timestamp"].as_i64().expect("a timestamp"));
		}
		cursor = body["next_cursor"].as_str().map(str::to_string);
		if cursor.is_none() {
			break;
		}
	}
	let expected: Vec<i64> = (1..=i64::try_from(ROWS).expect("rows fit an i64")).collect();
	assert_eq!(seen, expected, "the default page plus `next_cursor` covers the window exactly once");
}

/// The window, not the page, is what `WEFT_MAX_READ_ROWS` bounds: the store
/// materializes the rows a read names, so a request for a window wider than the
/// ceiling is refused even though the page it would show fits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_window_over_the_row_ceiling_is_refused_however_it_is_asked_for() {
	let (_dir, router) = router(ReadLimits::new(100, 8, 10_000, 10_000, Duration::from_secs(30))).await;

	for uri in ["/api/v1/storage/temperature/points?start=1&end=12", "/api/v1/storage/temperature/points?start=1&end=12&limit=5", "/api/v1/storage/temperature/value-range?lo=0&hi=100"] {
		let (status, body) = get(&router, uri).await;
		assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{uri}: {body}");
		assert_eq!(body["code"], "result_too_large", "{uri}: {body}");
		assert_eq!(body["limit"], 8, "{uri}: {body}");
	}

	// The export shapes of the same window are refused the same way, and their
	// refusal says what a page-less read can do about it.
	let (status, _) = get(&router, "/api/v1/storage/temperature/range.csv?start=1&end=12").await;
	assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// A downsample grid is counted before the read when the request names its own
/// bounds, and after it when it does not — an unbounded window's grid is only
/// knowable once the buckets exist, and it is refused just as hard either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_downsample_grid_over_the_bucket_ceiling_is_refused() {
	let (_dir, router) = router(ReadLimits::new(10_000, 10_000, 2, 10_000, Duration::from_secs(30))).await;

	let (status, body) = get(&router, "/api/v1/storage/temperature/downsample?start=1&end=12&resolution=1s").await;
	assert_refused(status, &body, 2, "WEFT_MAX_DOWNSAMPLE_BUCKETS");

	// The same window on a grid that fits is served, so the cap is the grid and
	// not a blanket refusal to reduce anything.
	let (status, body) = get(&router, "/api/v1/storage/temperature/downsample?start=1&end=12&resolution=10s").await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert!(body["buckets"].as_u64().expect("a bucket count") <= 2, "{body}");

	// No bounds at all: the grid is as wide as the stored history, so the refusal
	// comes from the buckets that were produced.
	let (status, body) = get(&router, "/api/v1/storage/temperature/downsample?resolution=1s").await;
	assert_refused(status, &body, 2, "WEFT_MAX_DOWNSAMPLE_BUCKETS");
}

/// The multi-instant batch is capped on the instants it names, before any
/// segment opens — the batch is the read, so its size is known from the query.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_at_multi_batch_over_the_cap_is_refused_and_a_smaller_one_is_served() {
	let (_dir, router) = router(ReadLimits::new(10_000, 10_000, 10_000, 2, Duration::from_secs(30))).await;

	let (status, body) = get(&router, "/api/v1/storage/temperature/at-multi?t=1,2,3").await;
	assert_refused(status, &body, 2, "WEFT_MAX_AT_MULTI_INSTANTS");

	let (status, body) = get(&router, "/api/v1/storage/temperature/at-multi?t=1,2").await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["points"].as_array().expect("a list").len(), 2, "{body}");
}

/// What the read deadline does and does not bound, from the wire.
///
/// The deadline bounds how long the server *waits* for a read. What a read that is
/// still pending at its deadline answers — the `504 query_timeout` — is pinned in
/// `read_limits`' own tests, where a read that waits can be created deliberately;
/// asserting it here would mean constructing a stored read slow enough to lose a
/// race, which is a flake waiting to be written. Nor is a real read asserted served
/// under a zero deadline: a stored read touches the filesystem, and a read that
/// yields at all is already past a deadline of zero, so whether it wins that race is
/// a property of the machine's I/O rather than of the ceiling — on Windows CI this
/// test's read lost it and answered `504` while Linux and macOS won it, which is the
/// flake in its plainest form. `read_limits`' `a_read_that_never_waits_is_served_under_a_zero_deadline`
/// pins the never-waited case against a future that genuinely does not yield.
///
/// What is pinned on the wire is the part only the wire shows: the probes are
/// never bounded by a read deadline, so an operator whose reads have all been
/// stopped by the ceiling can still ask whether the server is up and ready.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_deadline_bounds_reads_and_never_the_probes() {
	let (_dir, router) = router(ReadLimits::defaults().with_read_timeout(Duration::ZERO)).await;

	let (status, body) = get(&router, "/health").await;
	assert_eq!(status, StatusCode::OK, "the health probe is not a read: {body}");
	let (status, body) = get(&router, "/ready").await;
	assert_eq!(status, StatusCode::OK, "a zero read deadline must not make a healthy server look unready: {body}");
}
