//! The Arrow Flight listener: stored reads as Arrow record batches over gRPC (Phase 2).
//!
//! Opt-in and experimental. With [`FLIGHT_ADDR_ENV`] set, the server also listens there for
//! Arrow Flight, beside — not on — the one HTTP/1.1 serve path: Flight is gRPC, which
//! needs HTTP/2. What it serves is the same typed record batch the HTTP Arrow range
//! routes encode as an IPC stream, so a Flight client (`pyarrow.flight`, the Arrow Flight
//! clients for Java, Go, C++, R) gets a stored window without the stream being buffered
//! into one HTTP body.
//!
//! **Read-only.** `DoGet`, `GetFlightInfo`, `GetSchema` and `ListFlights` are served;
//! `DoPut`, `DoExchange`, `DoAction` and `Handshake` answer `UNIMPLEMENTED`. Writes keep
//! the HTTP ingest routes, with their idempotency keys, admission budget and audit records.
//!
//! **Tickets** are JSON, the same in a ticket and in a descriptor's `cmd`:
//!
//! - `{"aspect": "price", "start": 0, "end": 9999}` — the stored window `[start, end]`
//!   (epoch integers in the aspect's declared unit), as `GET …/range` reads it;
//! - `{"aspect": "price", "lo": "1.5", "hi": "2"}` — the rows whose value is in `[lo, hi]`
//!   (decimal text), as `GET …/value-range` reads it.
//!
//! A descriptor `path` of one element, `["price"]`, is the aspect's whole time range.
//!
//! **Access.** The listener is plaintext and binds loopback only: a non-loopback
//! [`FLIGHT_ADDR_ENV`] refuses the start (see [`flight_addr`]), because a bearer key over
//! plaintext gRPC on a network is the exposure SEC-5's plaintext gate exists to refuse.
//! With a keys file mounted, every call needs `authorization: Bearer <key>` whose role
//! grants the `read` scope — the scope the HTTP range routes ask for — and a refusal is
//! recorded as an `auth_denied` audit event, as on the HTTP path. Each call takes a permit
//! from a query gate sized like the HTTP one (`WEFT_QUERY_MAX_INFLIGHT`), and a full gate is
//! `UNAVAILABLE`, the gRPC shape of `503 overloaded`.
//!
//! **Errors** carry the HTTP envelope's code: the status message reads `<code>: <message>`
//! and the `x-weft-error-code` metadata holds the code alone, so a client branches on the
//! same names (`aspect_not_found`, `bad_request`, …) on either transport.

use std::{net::SocketAddr, pin::Pin, sync::Arc, time::Duration};

use arrow_flight::{
	encode::FlightDataEncoderBuilder, flight_service_server::{FlightService, FlightServiceServer}, Action, ActionType, Criteria, Empty, FlightData, FlightDescriptor, FlightEndpoint, FlightInfo, HandshakeRequest, HandshakeResponse, PollInfo, PutResult, SchemaAsIpc, SchemaResult, Ticket
};
use arrow_ipc::writer::IpcWriteOptions;
use bigdecimal::BigDecimal;
use futures::{stream, Stream, StreamExt as _, TryStreamExt as _};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{watch, Semaphore};
use tonic::{metadata::MetadataMap, Request, Response, Status, Streaming};
use tracing::Instrument as _;

use crate::{
	auth::Presented, error::{self, ApiError, Code}, routes::Scope, state::AppState
};

/// The environment variable that turns the Flight listener on, naming its address.
pub const FLIGHT_ADDR_ENV: &str = "WEFT_FLIGHT_ADDR";

/// How long a stopping listener waits for the streams already open before it lets go.
pub const FLIGHT_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// The metadata key a refused call carries its envelope code in.
pub const ERROR_CODE_METADATA: &str = "x-weft-error-code";

/// The address the Flight listener should bind, from [`FLIGHT_ADDR_ENV`]'s value.
///
/// # Errors
///
/// A value that is not a socket address, or one that is not loopback: the listener is
/// plaintext, and SEC-5's rule is that plaintext does not leave the host.
pub fn flight_addr(value: Option<&str>) -> Result<Option<SocketAddr>, String> {
	let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else { return Ok(None) };
	let addr: SocketAddr = value.parse().map_err(|error| format!("{FLIGHT_ADDR_ENV}={value:?} is not a socket address ({error}); e.g. 127.0.0.1:8815"))?;
	if !addr.ip().to_canonical().is_loopback() {
		return Err(format!("{FLIGHT_ADDR_ENV}={addr} is not loopback: the Flight listener is plaintext gRPC, so it binds loopback only (reach it through an authenticated tunnel or proxy that terminates TLS)"));
	}
	Ok(Some(addr))
}

/// One stored read, as a ticket or a descriptor command names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlightTicket {
	/// The aspect to read.
	pub aspect: String,
	/// The window's first instant (time window).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub start: Option<i64>,
	/// The window's last instant (time window).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub end: Option<i64>,
	/// The band's low value, as decimal text (value band).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub lo: Option<String>,
	/// The band's high value, as decimal text (value band).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub hi: Option<String>,
}

/// What a [`FlightTicket`] reads.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Read {
	Time { aspect: String, start: i64, end: i64 },
	Value { aspect: String, lo: BigDecimal, hi: BigDecimal },
}

impl FlightTicket {
	/// The whole time range of `aspect`.
	#[must_use]
	pub fn whole(aspect: impl Into<String>) -> Self {
		Self { aspect: aspect.into(), start: Some(i64::MIN), end: Some(i64::MAX), lo: None, hi: None }
	}

	/// The ticket's bytes.
	///
	/// # Panics
	///
	/// Never: a ticket is plain JSON.
	#[must_use]
	pub fn to_bytes(&self) -> Vec<u8> {
		serde_json::to_vec(self).expect("a ticket serializes")
	}

	fn parse(bytes: &[u8]) -> Result<Read, ApiError> {
		let ticket: Self = serde_json::from_slice(bytes).map_err(|error| ApiError::bad_request(format!("a Flight ticket is JSON naming an aspect and either `start`/`end` or `lo`/`hi`: {error}")))?;
		ticket.read()
	}

	fn read(self) -> Result<Read, ApiError> {
		if self.aspect.is_empty() {
			return Err(ApiError::bad_request("a Flight ticket names its `aspect`"));
		}
		match (self.start, self.end, self.lo, self.hi) {
			(Some(start), Some(end), None, None) => Ok(Read::Time { aspect: self.aspect, start, end }),
			(None, None, Some(lo), Some(hi)) => {
				let bound = |name: &str, text: &str| weft_physical_type::parse_decimal(text).map_err(|e| error::decimal_error(e, ApiError::bad_request(format!("`{name}` is not a decimal: {text:?}"))));
				Ok(Read::Value { lo: bound("lo", &lo)?, hi: bound("hi", &hi)?, aspect: self.aspect })
			}
			_ => Err(ApiError::bad_request("a Flight ticket carries `start` and `end` (a time window) or `lo` and `hi` (a value band), and not both")),
		}
	}
}

impl Read {
	fn aspect(&self) -> &str {
		match self {
			Self::Time { aspect, .. } | Self::Value { aspect, .. } => aspect,
		}
	}
}

/// The gRPC status for an envelope refusal, keeping its code.
fn status(error: &ApiError) -> Status {
	let code = error.code();
	let grpc = match code.status().as_u16() {
		400 | 413 | 415 | 422 => tonic::Code::InvalidArgument,
		401 => tonic::Code::Unauthenticated,
		403 => tonic::Code::PermissionDenied,
		404 => tonic::Code::NotFound,
		409 | 410 => tonic::Code::FailedPrecondition,
		503 => tonic::Code::Unavailable,
		_ => tonic::Code::Internal,
	};
	let message = if code.status().is_server_error() { code.redacted_message().to_owned() } else { error.message().to_owned() };
	let mut status = Status::new(grpc, format!("{}: {message}", code.as_str()));
	if let Ok(value) = code.as_str().parse() {
		status.metadata_mut().insert(ERROR_CODE_METADATA, value);
	}
	status
}

type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

/// The Flight service over one server's state.
#[derive(Clone)]
pub struct WeftFlight {
	state: AppState,
	gate: Arc<Semaphore>,
}

impl WeftFlight {
	/// A service over `state`, its query gate sized from `state`'s admission config.
	#[must_use]
	pub fn new(state: AppState) -> Self {
		let permits = state.admission_config().max_query_inflight().max(1);
		Self { state, gate: Arc::new(Semaphore::new(permits)) }
	}

	/// Check the caller's key against the `read` scope, recording a refusal.
	fn authorize(&self, metadata: &MetadataMap, operation: &'static str) -> Result<(), Status> {
		let Some(keys) = self.state.auth().key_store() else {
			// Authentication off or disabled. Off is safe here because the listener binds
			// loopback only; disabled means a gateway in front owns authentication.
			return Ok(());
		};
		// The HTTP path's own parsing, so a key reads the same on both transports (scheme in
		// any case, one header, no stray whitespace).
		let (code, principal, message) = match crate::auth::presented(&metadata.clone().into_headers(), &keys) {
			Presented::Absent => (Code::Unauthenticated, None, "this call needs the `read` scope, and it carries no key: send `authorization: Bearer <key>` metadata".to_owned()),
			Presented::Refused => (Code::Unauthenticated, None, "the call's credentials are not a key this server holds".to_owned()),
			Presented::Principal(principal) if principal.role.allows(Scope::Read) => return Ok(()),
			Presented::Principal(principal) => {
				let message = format!("key `{}` holds the {} role, which does not grant the `read` scope", principal.id, principal.role.as_str());
				(Code::Forbidden, Some(principal), message)
			}
		};
		if let Some(log) = self.state.audit_log() {
			log.emit_event("auth_denied", json!({"code": code.as_str(), "method": "flight", "path": operation, "operation": operation, "principal": principal.as_ref().map(|p| p.id.clone()), "role": principal.as_ref().map(|p| p.role.as_str())}));
		}
		Err(status(&ApiError::new(code, message)))
	}

	fn admit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, Status> {
		self.gate.clone().try_acquire_owned().map_err(|_| status(&ApiError::new(Code::Overloaded, "the Flight listener's query gate is full; retry shortly")))
	}

	fn store(&self) -> Result<Arc<weftdb::SegmentStore>, Status> {
		self.state.store().cloned().ok_or_else(|| status(&ApiError::unconfigured()))
	}

	async fn batch(&self, read: &Read) -> Result<arrow_array::RecordBatch, Status> {
		let store = self.store()?;
		let batch = match read {
			Read::Time { aspect, start, end } => weft_arrow_store::read_time_range_to_record_batch_typed(&store, aspect, *start, *end).instrument(tracing::info_span!("flight.read", %aspect, start, end)).await,
			Read::Value { aspect, lo, hi } => weft_arrow_store::read_value_range_to_record_batch_typed(&store, aspect, lo, hi).instrument(tracing::info_span!("flight.read", %aspect, %lo, %hi)).await,
		};
		batch.map_err(|error| status(&error::classify_read(&error)))
	}

	/// The schema a read answers with: a time read over an empty window, so no rows are
	/// decoded to learn it. Both reads build the aspect's typed batch, so a value band's rows
	/// arrive in this schema too.
	async fn schema(&self, read: &Read) -> Result<arrow_schema::SchemaRef, Status> {
		let empty = match read {
			Read::Time { aspect, .. } | Read::Value { aspect, .. } => Read::Time { aspect: aspect.clone(), start: 1, end: 0 },
		};
		Ok(self.batch(&empty).await?.schema())
	}

	fn read_from_descriptor(descriptor: &FlightDescriptor) -> Result<(Read, Vec<u8>), Status> {
		if !descriptor.cmd.is_empty() {
			let read = FlightTicket::parse(&descriptor.cmd).map_err(|error| status(&error))?;
			return Ok((read, descriptor.cmd.to_vec()));
		}
		match descriptor.path.as_slice() {
			[aspect] => {
				let ticket = FlightTicket::whole(aspect.clone());
				Ok((ticket.clone().read().map_err(|error| status(&error))?, ticket.to_bytes()))
			}
			_ => Err(status(&ApiError::bad_request("a Flight descriptor is a JSON ticket `cmd`, or a `path` of exactly one aspect name"))),
		}
	}

	async fn info(&self, read: &Read, ticket: Vec<u8>, descriptor: FlightDescriptor) -> Result<FlightInfo, Status> {
		let schema = self.schema(read).await?;
		let info = FlightInfo::new().try_with_schema(&schema).map_err(|error| status(&ApiError::internal(error.to_string())))?;
		Ok(info.with_descriptor(descriptor).with_endpoint(FlightEndpoint::new().with_ticket(Ticket::new(ticket))).with_total_records(-1).with_total_bytes(-1))
	}
}

fn unimplemented(what: &str) -> Status {
	let mut status = Status::unimplemented(format!("bad_request: {what} is not served: the Flight listener is read-only; write through the HTTP ingest routes"));
	if let Ok(value) = "bad_request".parse() {
		status.metadata_mut().insert(ERROR_CODE_METADATA, value);
	}
	status
}

#[tonic::async_trait]
impl FlightService for WeftFlight {
	type DoActionStream = BoxStream<arrow_flight::Result>;
	type DoExchangeStream = BoxStream<FlightData>;
	type DoGetStream = BoxStream<FlightData>;
	type DoPutStream = BoxStream<PutResult>;
	type HandshakeStream = BoxStream<HandshakeResponse>;
	type ListActionsStream = BoxStream<ActionType>;
	type ListFlightsStream = BoxStream<FlightInfo>;

	async fn handshake(&self, _request: Request<Streaming<HandshakeRequest>>) -> Result<Response<Self::HandshakeStream>, Status> {
		Err(Status::unimplemented("no handshake: send `authorization: Bearer <key>` metadata on each call"))
	}

	async fn list_flights(&self, request: Request<Criteria>) -> Result<Response<Self::ListFlightsStream>, Status> {
		self.authorize(request.metadata(), "ListFlights")?;
		let _permit = self.admit()?;
		let store = self.store()?;
		let names = store.list_declared_aspects().await.map_err(|error| status(&ApiError::internal(error.to_string())))?;
		let mut infos = Vec::with_capacity(names.len());
		for name in names {
			let ticket = FlightTicket::whole(name.clone());
			let read = ticket.clone().read().map_err(|error| status(&error))?;
			infos.push(self.info(&read, ticket.to_bytes(), FlightDescriptor::new_path(vec![name])).await);
		}
		Ok(Response::new(Box::pin(stream::iter(infos))))
	}

	async fn get_flight_info(&self, request: Request<FlightDescriptor>) -> Result<Response<FlightInfo>, Status> {
		self.authorize(request.metadata(), "GetFlightInfo")?;
		let _permit = self.admit()?;
		let descriptor = request.into_inner();
		let (read, ticket) = Self::read_from_descriptor(&descriptor)?;
		Ok(Response::new(self.info(&read, ticket, descriptor).await?))
	}

	async fn poll_flight_info(&self, _request: Request<FlightDescriptor>) -> Result<Response<PollInfo>, Status> {
		Err(unimplemented("PollFlightInfo"))
	}

	async fn get_schema(&self, request: Request<FlightDescriptor>) -> Result<Response<SchemaResult>, Status> {
		self.authorize(request.metadata(), "GetSchema")?;
		let _permit = self.admit()?;
		let (read, _) = Self::read_from_descriptor(request.get_ref())?;
		let schema = self.schema(&read).await?;
		let result = SchemaAsIpc::new(&schema, &IpcWriteOptions::default()).try_into().map_err(|error: arrow_schema::ArrowError| status(&ApiError::internal(error.to_string())))?;
		Ok(Response::new(result))
	}

	async fn do_get(&self, request: Request<Ticket>) -> Result<Response<Self::DoGetStream>, Status> {
		self.authorize(request.metadata(), "DoGet")?;
		let permit = self.admit()?;
		let read = FlightTicket::parse(&request.get_ref().ticket).map_err(|error| status(&error))?;
		let batch = self.batch(&read).await?;
		tracing::debug!(aspect = read.aspect(), rows = batch.num_rows(), "flight.do_get");
		let schema = batch.schema();
		let encoded = FlightDataEncoderBuilder::new().with_schema(schema).build(stream::iter([Ok(batch)])).map_err(|error| status(&ApiError::internal(error.to_string())));
		// The permit is held until the stream is drained or dropped, like an HTTP read's.
		let held = encoded.map(move |item| {
			let _ = &permit;
			item
		});
		Ok(Response::new(Box::pin(held)))
	}

	async fn do_put(&self, _request: Request<Streaming<FlightData>>) -> Result<Response<Self::DoPutStream>, Status> {
		Err(unimplemented("DoPut"))
	}

	async fn do_exchange(&self, _request: Request<Streaming<FlightData>>) -> Result<Response<Self::DoExchangeStream>, Status> {
		Err(unimplemented("DoExchange"))
	}

	async fn do_action(&self, _request: Request<Action>) -> Result<Response<Self::DoActionStream>, Status> {
		Err(unimplemented("DoAction"))
	}

	async fn list_actions(&self, _request: Request<Empty>) -> Result<Response<Self::ListActionsStream>, Status> {
		Ok(Response::new(Box::pin(stream::empty())))
	}
}

/// Serve Flight on `listener` until `shutdown` turns `true`, then give the open streams
/// [`FLIGHT_SHUTDOWN_GRACE`] to finish. A stream still open after that is abandoned: tonic
/// runs each connection on its own task, so it may keep reading until the store it holds
/// is shut down under it and its read fails.
///
/// # Errors
///
/// The gRPC server's own failure (the listener is already bound).
pub async fn serve(listener: tokio::net::TcpListener, state: AppState, mut shutdown: watch::Receiver<bool>) -> Result<(), tonic::transport::Error> {
	let incoming = tonic::transport::server::TcpIncoming::from(listener);
	let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
	let signal = async move {
		while !*shutdown.borrow() {
			if shutdown.changed().await.is_err() {
				break;
			}
		}
		let _ = stop_tx.send(());
	};
	let server = tonic::transport::Server::builder().add_service(FlightServiceServer::new(WeftFlight::new(state))).serve_with_incoming_shutdown(incoming, async move {
		let _ = stop_rx.await;
	});
	tokio::pin!(server);
	tokio::select! {
		result = &mut server => return result,
		() = signal => {}
	}
	match tokio::time::timeout(FLIGHT_SHUTDOWN_GRACE, server).await {
		Ok(result) => result,
		Err(_) => {
			tracing::warn!(grace_secs = FLIGHT_SHUTDOWN_GRACE.as_secs(), "the Flight listener's open streams outlived the shutdown grace; dropping them");
			Ok(())
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_address_is_loopback_or_nothing() {
		assert_eq!(flight_addr(None).unwrap(), None);
		assert_eq!(flight_addr(Some("  ")).unwrap(), None);
		assert_eq!(flight_addr(Some("127.0.0.1:8815")).unwrap(), Some("127.0.0.1:8815".parse().unwrap()));
		assert!(flight_addr(Some("[::1]:8815")).unwrap().is_some());
		assert!(flight_addr(Some("0.0.0.0:8815")).unwrap_err().contains("loopback"));
		assert!(flight_addr(Some("10.0.0.5:8815")).unwrap_err().contains("loopback"));
		assert!(flight_addr(Some("localhost")).unwrap_err().contains("socket address"));
	}

	#[test]
	fn a_ticket_names_a_window_or_a_band_and_not_both() {
		assert_eq!(FlightTicket::parse(br#"{"aspect":"p","start":1,"end":2}"#).unwrap(), Read::Time { aspect: "p".into(), start: 1, end: 2 });
		assert!(matches!(FlightTicket::parse(br#"{"aspect":"p","lo":"1.5","hi":"2"}"#).unwrap(), Read::Value { .. }));
		for bad in [&br#"{"aspect":"p","start":1}"#[..], br#"{"aspect":"p","start":1,"end":2,"lo":"1","hi":"2"}"#, br#"{"aspect":"","start":1,"end":2}"#, br#"{"aspect":"p","lo":"x","hi":"2"}"#, br#"{"aspect":"p","start":1,"end":2,"extra":1}"#, b"not json"] {
			let error = FlightTicket::parse(bad).unwrap_err();
			assert_eq!(error.code(), Code::BadRequest, "{}", String::from_utf8_lossy(bad));
		}
		assert_eq!(FlightTicket::parse(&FlightTicket::whole("p").to_bytes()).unwrap(), Read::Time { aspect: "p".into(), start: i64::MIN, end: i64::MAX });
	}

	#[test]
	fn a_refusal_keeps_its_envelope_code() {
		let refused = status(&ApiError::new(Code::AspectNotFound, "no aspect `q`"));
		assert_eq!(refused.code(), tonic::Code::NotFound);
		assert_eq!(refused.message(), "aspect_not_found: no aspect `q`");
		assert_eq!(refused.metadata().get(ERROR_CODE_METADATA).unwrap(), "aspect_not_found");
		let server = status(&ApiError::internal("disk on fire at /var/secret"));
		assert_eq!(server.code(), tonic::Code::Internal);
		assert!(!server.message().contains("secret"), "a 5xx sends the code's sentence, not its cause: {}", server.message());
		assert_eq!(status(&ApiError::new(Code::Overloaded, "full")).code(), tonic::Code::Unavailable);
	}
}
