//! The Arrow Flight listener over real gRPC: what a Flight client sees.
//!
//! Each test stores rows through the HTTP router (the write path a client would use), then
//! reads them back through `weft_server::flight::serve` on a loopback port with the Arrow
//! project's own `FlightClient`.

use std::sync::Arc;

use arrow_flight::{FlightClient, FlightDescriptor, Ticket};
use axum::{body::Body, http::Request};
use futures::TryStreamExt as _;
use tokio::sync::watch;
use tower::ServiceExt as _;
use weft_server::{
	app_with_state, auth::format_key_hash, flight::{self, FlightTicket}, AppState, AuthConfig, KeyStore
};
use weftdb::SegmentStore;

const READER: &str = "weft_flight_reader_key_0001";

struct Running {
	client: FlightClient,
	stop: watch::Sender<bool>,
	served: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
	_dir: tempfile::TempDir,
}

async fn post(router: &axum::Router, uri: &str, body: serde_json::Value) {
	let response = router.clone().oneshot(Request::builder().method("POST").uri(uri).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap()).await.unwrap();
	assert!(response.status().is_success(), "{uri}: {}", response.status());
}

/// A store holding `price` (seconds; 1.25 at 10, 2.5 at 20, 3.75 at 30), served over Flight.
async fn running(auth: AuthConfig) -> Running {
	let dir = tempfile::tempdir().unwrap();
	let store = Arc::new(SegmentStore::open(dir.path()).await.unwrap());
	let router = app_with_state(AppState::new().with_store(store.clone()));
	post(&router, "/api/v1/storage/aspects", serde_json::json!({"name": "price", "physical_type": "scaled_i64", "scale": 2, "timestamp_unit": "seconds"})).await;
	post(&router, "/api/v1/storage/price/points", serde_json::json!({"points": [{"timestamp": 10, "value": "1.25"}, {"timestamp": 20, "value": "2.5"}, {"timestamp": 30, "value": "3.75"}]})).await;
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	let (stop, shutdown) = watch::channel(false);
	let served = tokio::spawn(flight::serve(listener, AppState::new().with_store(store).with_auth(auth), shutdown));
	let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}")).unwrap().connect().await.unwrap();
	Running { client: FlightClient::new(channel), stop, served, _dir: dir }
}

fn keys() -> AuthConfig {
	let text = format!(r#"{{"version":1,"keys":[{{"id":"r","role":"reader","key_hash":"{}"}}]}}"#, format_key_hash(READER));
	AuthConfig::with_keys(KeyStore::parse(&text).unwrap())
}

fn rows(batches: &[arrow_array::RecordBatch]) -> usize {
	batches.iter().map(arrow_array::RecordBatch::num_rows).sum()
}

fn code(status: &tonic::Status) -> &str {
	status.metadata().get(flight::ERROR_CODE_METADATA).and_then(|value| value.to_str().ok()).unwrap_or_default()
}

fn status_of(error: arrow_flight::error::FlightError) -> tonic::Status {
	match error {
		arrow_flight::error::FlightError::Tonic(status) => *status,
		other => panic!("expected a gRPC status, got {other}"),
	}
}

#[tokio::test]
async fn do_get_streams_a_window_and_a_band() {
	let mut server = running(AuthConfig::off()).await;
	let window = FlightTicket { aspect: "price".into(), start: Some(15), end: Some(30), lo: None, hi: None };
	let batches: Vec<_> = server.client.do_get(Ticket::new(window.to_bytes())).await.unwrap().try_collect().await.unwrap();
	assert_eq!(rows(&batches), 2, "rows at 20 and 30");
	let band = FlightTicket { aspect: "price".into(), start: None, end: None, lo: Some("1".into()), hi: Some("2.5".into()) };
	let batches: Vec<_> = server.client.do_get(Ticket::new(band.to_bytes())).await.unwrap().try_collect().await.unwrap();
	assert_eq!(rows(&batches), 2, "1.25 and 2.5");
	let whole: Vec<_> = server.client.do_get(Ticket::new(FlightTicket::whole("price").to_bytes())).await.unwrap().try_collect().await.unwrap();
	assert_eq!(rows(&whole), 3);
	let _ = server.stop.send(true);
	server.served.await.unwrap().expect("a clean stop");
}

#[tokio::test]
async fn the_catalog_and_schema_are_served() {
	let mut server = running(AuthConfig::off()).await;
	let infos: Vec<_> = server.client.list_flights(Vec::<u8>::new()).await.unwrap().try_collect().await.unwrap();
	assert_eq!(infos.len(), 1);
	assert_eq!(infos[0].flight_descriptor.as_ref().unwrap().path, ["price"]);
	let ticket = infos[0].endpoint[0].ticket.clone().unwrap();
	let batches: Vec<_> = server.client.do_get(ticket).await.unwrap().try_collect().await.unwrap();
	assert_eq!(rows(&batches), 3, "a listed flight's ticket reads the aspect");
	let schema = server.client.get_schema(FlightDescriptor::new_path(vec!["price".into()])).await.unwrap();
	assert_eq!(schema, *batches[0].schema(), "the schema answered without rows is the one the rows come in");
	let info = server.client.get_flight_info(FlightDescriptor::new_cmd(FlightTicket::whole("price").to_bytes())).await.unwrap();
	assert_eq!(info.endpoint.len(), 1);
	// A value band answers in the schema advertised for it, the aspect's typed one.
	let band = FlightTicket { aspect: "price".into(), start: None, end: None, lo: Some("1".into()), hi: Some("3".into()) };
	let advertised = server.client.get_schema(FlightDescriptor::new_cmd(band.to_bytes())).await.unwrap();
	let rows: Vec<_> = server.client.do_get(Ticket::new(band.to_bytes())).await.unwrap().try_collect().await.unwrap();
	assert_eq!(advertised, *rows[0].schema(), "GetSchema for a band is the schema DoGet streams it in");
	assert_eq!(advertised, schema, "and it is the window's schema");
}

#[tokio::test]
async fn refusals_keep_their_envelope_codes() {
	let mut server = running(AuthConfig::off()).await;
	let missing = status_of(server.client.do_get(Ticket::new(FlightTicket::whole("nope").to_bytes())).await.err().unwrap());
	assert_eq!((missing.code(), code(&missing)), (tonic::Code::NotFound, "aspect_not_found"), "{missing:?}");
	let bad = status_of(server.client.do_get(Ticket::new(b"{\"aspect\":\"price\"}".to_vec())).await.err().unwrap());
	assert_eq!((bad.code(), code(&bad)), (tonic::Code::InvalidArgument, "bad_request"));
	let put = server.client.do_put(futures::stream::empty()).await;
	let put = match put {
		Err(error) => status_of(error),
		Ok(mut results) => status_of(results.try_next().await.err().expect("a refused put")),
	};
	assert_eq!(put.code(), tonic::Code::Unimplemented, "read-only");
}

#[tokio::test]
async fn with_keys_a_read_needs_a_key() {
	let mut server = running(keys()).await;
	let ticket = || Ticket::new(FlightTicket::whole("price").to_bytes());
	let absent = status_of(server.client.do_get(ticket()).await.err().unwrap());
	assert_eq!((absent.code(), code(&absent)), (tonic::Code::Unauthenticated, "unauthenticated"));
	server.client.add_header("authorization", "Bearer weft_not_a_key_this_server_holds").unwrap();
	assert_eq!(status_of(server.client.do_get(ticket()).await.err().unwrap()).code(), tonic::Code::Unauthenticated);
	server.client.add_header("authorization", &format!("Bearer {READER}")).unwrap();
	let batches: Vec<_> = server.client.do_get(ticket()).await.unwrap().try_collect().await.unwrap();
	assert_eq!(rows(&batches), 3);
}
