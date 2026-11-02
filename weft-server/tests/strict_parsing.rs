//! Strict request parsing (FRE-6b): a request that names a field or a parameter the
//! endpoint does not have is refused, not silently narrowed to the request the server
//! wishes it had been.
//!
//! Before this slice every request type in `weft-server` deserialized the way serde
//! does by default: an unknown JSON field or query parameter was ignored. A caller
//! with a typo — `require_sorte` on an ingest, `limti` on a read — got a successful
//! answer to a different request than the one it meant to send, and nothing anywhere
//! said so. That is the request-side twin of the silent narrowing FRE-5 removed from
//! the compute responses. The refusal is the extractor's existing one: `invalid_json`
//! for a body, `bad_request` for a query, both in the §4 envelope (X-ERR), so a caller
//! branches on the same codes it already handles for a malformed request.
//!
//! Two pins live here. The HTTP tests prove the wiring: the refusal reaches the wire
//! on routes from each lane FRE-6b sits on (L-INGEST, L-COMPUTE, the storage reads,
//! the operator surface). The type tests prove the coverage: every request type the
//! server extracts is enumerated, so a future endpoint cannot quietly reintroduce a
//! lenient type — adding one means adding it here and giving it the attribute.

use axum::{
	body::Body, http::{Request as HttpRequest, StatusCode}, Router
};
use tower::ServiceExt as _;
use weft_server::{
	app_with_state, backup_routes::{BackupParams, RestoreDrillParams}, declare::DeclareAspectRequest, downsample::{DownsampleIlpParams, DownsampleRequest}, ingest::{CsvIngestParams, IlpIngestParams, IngestPoint, IngestRequest, ParquetIngestParams}, interpolate::{IlpParams, InputPoint, InterpolateRequest, PointRequest}, maintain::{CompactParams, QuarantineParams, ReconcileParams, SquashParams}, storage::{MultiPointParams, PointParams, PointsRangeParams, StorageDownsampleParams, TimeRangeParams, ValuePointsParams, ValueRangeParams}, AppState
};

/// Send one request to a storeless server and return the status and parsed body.
async fn answer(request: HttpRequest<Body>) -> (StatusCode, serde_json::Value) {
	let router: Router = app_with_state(AppState::new());
	let response = router.oneshot(request).await.expect("the router answers every request");
	let status = response.status();
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("the body is small");
	(status, serde_json::from_slice(&bytes).expect("a refusal is the JSON envelope"))
}

fn json_post(uri: &str, body: &str) -> HttpRequest<Body> {
	HttpRequest::builder().method("POST").uri(uri).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap()
}

/// A JSON body naming a field the endpoint does not have is `invalid_json`, on a
/// compute route and on an ingest route alike — and the message names the field, so
/// the caller learns which of its assumptions is wrong.
#[tokio::test]
async fn an_unknown_json_field_is_refused() {
	for (uri, body) in [("/api/v1/interpolate", r#"{"points":[],"limti":10}"#), ("/api/v1/downsample", r#"{"points":[],"aggregaton":["avg"]}"#), ("/api/v1/storage/temperature/points", r#"{"points":[],"require_sorte":true}"#), ("/api/v1/storage/aspects", r#"{"name":"a","physical_type":"f64","timestamp_unit":"seconds","tolerence":"0"}"#)] {
		let (status, body) = answer(json_post(uri, body)).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
		assert_eq!(body["code"], "invalid_json", "{uri}");
		assert!(body["error"].as_str().unwrap().contains("unknown field"), "{uri}: {}", body["error"]);
	}
}

/// Strictness reaches inside the body: a field invented on one point of a batch, or
/// inside the spline's structured variant, is refused the same way — a top-level-only
/// check would leave the nested shapes as the lenient back door.
#[tokio::test]
async fn an_unknown_nested_field_is_refused() {
	for (uri, body) in [("/api/v1/storage/temperature/points", r#"{"points":[{"timestamp":1,"value":"2","unit":"c"}]}"#), ("/api/v1/interpolate", r#"{"points":[],"spline":{"polynomial":{"degree":2,"bound":1.5}}}"#)] {
		let (status, body) = answer(json_post(uri, body)).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
		assert_eq!(body["code"], "invalid_json", "{uri}");
		assert!(body["error"].as_str().unwrap().contains("unknown field"), "{uri}: {}", body["error"]);
	}
}

/// A query parameter the endpoint does not have is `bad_request`, on a read route
/// and on an ingest route alike. Before this slice both were silently dropped, so
/// `?start=1&end=2&limit=10` against a route with no `limit` read the whole window
/// and the caller never knew its page size had been ignored.
#[tokio::test]
async fn an_unknown_query_parameter_is_refused() {
	for uri in ["/api/v1/storage/temperature/points?start=1&end=2&limti=10", "/api/v1/storage/temperature/downsample?resoluton=seconds"] {
		let request = HttpRequest::builder().method("GET").uri(uri).body(Body::empty()).unwrap();
		let (status, body) = answer(request).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
		assert_eq!(body["code"], "bad_request", "{uri}");
		assert!(body["error"].as_str().unwrap().contains("unknown field"), "{uri}: {}", body["error"]);
	}
	// The CSV ingest takes its parameters in the query and its payload in the body.
	let request = HttpRequest::builder().method("POST").uri("/api/v1/storage/temperature/csv?rows_per_pag=10").header("content-type", "text/csv").body(Body::from("timestamp,value\n1,2\n")).unwrap();
	let (status, body) = answer(request).await;
	assert_eq!(status, StatusCode::BAD_REQUEST);
	assert_eq!(body["code"], "bad_request");
	assert!(body["error"].as_str().unwrap().contains("unknown field"), "{}", body["error"]);
	// The ILP ingest takes its parameters in the query and its payload in the body.
	let request = HttpRequest::builder().method("POST").uri("/api/v1/storage/temperature/ilp?field=v&precison=ms").header("content-type", "text/plain").body(Body::from("m v=1 1")).unwrap();
	let (status, body) = answer(request).await;
	assert_eq!(status, StatusCode::BAD_REQUEST);
	assert_eq!(body["code"], "bad_request");
}

/// The strictness refuses only what it should: a well-formed request still parses,
/// and the refusal it then meets is the missing store — proof the extractor accepted
/// the body rather than the test passing because everything is refused.
#[tokio::test]
async fn a_well_formed_request_still_parses() {
	let (status, body) = answer(json_post("/api/v1/storage/temperature/points", r#"{"points":[]}"#)).await;
	assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
	assert_eq!(body["code"], "unconfigured");
	let request = HttpRequest::builder().method("GET").uri("/api/v1/storage/temperature/points?start=1&end=2").body(Body::empty()).unwrap();
	let (status, body) = answer(request).await;
	assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
	assert_eq!(body["code"], "unconfigured");
}

/// Every JSON request type refuses a field it does not have. The payloads are the
/// smallest body each type accepts; the extra key is the refusal's whole subject.
#[test]
fn every_json_request_type_refuses_an_unknown_field() {
	assert_unknown_field::<DeclareAspectRequest>(r#"{"name":"a","physical_type":"f64","timestamp_unit":"seconds","zzz_unknown":1}"#);
	assert_unknown_field::<DownsampleRequest>(r#"{"points":[],"zzz_unknown":1}"#);
	assert_unknown_field::<IngestRequest>(r#"{"points":[],"zzz_unknown":1}"#);
	assert_unknown_field::<IngestPoint>(r#"{"timestamp":1,"value":"2","zzz_unknown":1}"#);
	assert_unknown_field::<InputPoint>(r#"{"timestamp":"2026-01-01T00:00:00Z","value":1.5,"zzz_unknown":1}"#);
	assert_unknown_field::<InterpolateRequest>(r#"{"points":[],"zzz_unknown":1}"#);
	assert_unknown_field::<PointRequest>(r#"{"instant":"2026-01-01T00:00:00Z","points":[],"zzz_unknown":1}"#);
}

fn assert_unknown_field<T: serde::de::DeserializeOwned + std::fmt::Debug>(body: &str) {
	let err = serde_json::from_str::<T>(body).expect_err("an unknown field must be refused");
	assert!(err.to_string().contains("unknown field"), "{err}");
}

/// Every query-parameter type refuses a parameter it does not have. The payloads are
/// JSON spellings of the same flat shapes the query string carries: the refusal is a
/// property of the type's derive, not of the decoder, and the wire-level tests above
/// pin the `serde_urlencoded` path itself on representative routes.
#[test]
fn every_query_request_type_refuses_an_unknown_parameter() {
	assert_unknown_field::<RestoreDrillParams>(r#"{"label":"x","zzz_unknown":1}"#);
	assert_unknown_field::<BackupParams>(r#"{"zzz_unknown":1}"#);
	assert_unknown_field::<DownsampleIlpParams>(r#"{"field":"f","zzz_unknown":1}"#);
	assert_unknown_field::<CsvIngestParams>(r#"{"zzz_unknown":1}"#);
	assert_unknown_field::<IlpIngestParams>(r#"{"field":"f","zzz_unknown":1}"#);
	assert_unknown_field::<ParquetIngestParams>(r#"{"zzz_unknown":1}"#);
	assert_unknown_field::<IlpParams>(r#"{"field":"f","zzz_unknown":1}"#);
	assert_unknown_field::<ReconcileParams>(r#"{"zzz_unknown":1}"#);
	assert_unknown_field::<SquashParams>(r#"{"zzz_unknown":1}"#);
	assert_unknown_field::<CompactParams>(r#"{"zzz_unknown":1}"#);
	assert_unknown_field::<QuarantineParams>(r#"{"zzz_unknown":1}"#);
	assert_unknown_field::<TimeRangeParams>(r#"{"start":1,"end":2,"zzz_unknown":1}"#);
	assert_unknown_field::<ValueRangeParams>(r#"{"lo":"1","hi":"2","zzz_unknown":1}"#);
	assert_unknown_field::<PointsRangeParams>(r#"{"start":1,"end":2,"zzz_unknown":1}"#);
	assert_unknown_field::<ValuePointsParams>(r#"{"lo":"1","hi":"2","zzz_unknown":1}"#);
	assert_unknown_field::<PointParams>(r#"{"t":1,"zzz_unknown":1}"#);
	assert_unknown_field::<MultiPointParams>(r#"{"t":"1","zzz_unknown":1}"#);
	assert_unknown_field::<StorageDownsampleParams>(r#"{"zzz_unknown":1}"#);
}
