//! The soak ingest client (1.0 plan SOA-5, W3).
//!
//! SOA-3 left the harness a server-shaped seam — [`SoakServer`] — and a null
//! server that fills it, so the harness, the open-loop scheduler, the interval
//! logs and the self-monitoring could all run with no server at all. This module
//! fills the same seam with a live server: it takes the [`WorkloadStep`] SOA-4
//! derives for a tick and puts that body on the wire as
//! `POST /api/v1/storage/{aspect}/ilp`, exactly as an operator driving a soak
//! against a running WeftDB server would.
//!
//! **The body is the model's, unchanged.** [`IngestClient::handle`] regenerates
//! the tick's step from [`WorkloadModel::step`] — a pure function of the seed and
//! the tick, so any tick is replayable without its predecessors — and writes
//! [`WorkloadStep::body`] byte for byte. The client adds no tags, reorders no
//! lines and drops no cells of its own: what a published seed says the traffic
//! was is what the server received. It does not filter the body down to the
//! values a given aspect can hold either — the server's ILP route already seals
//! the numeric subset of the named field and ignores the rest, and the count it
//! sealed comes back in the response and is recorded beside the count offered
//! ([`IngestFlow::lines_offered`] against [`IngestFlow::points_sealed`]), so the
//! difference between the two is visible in the flow rather than hidden by a
//! client-side filter.
//!
//! **Ingest lane only, and loudly.** A tick on the `point_lookup` or
//! `range_scan` lane is a read, and reading is not what this client is:
//! [`IngestClient::new`] refuses a [`WorkloadModel`] whose lane weights can
//! produce one, naming the lanes that still need their own client, and
//! [`IngestClient::handle`] errors on such a tick rather than quietly doing
//! nothing. A silent no-op would be the worst failure available here: the
//! scheduler times every tick it executes, so skipped reads would enter the
//! interval log as sub-microsecond steps and the run would look like a soak with
//! superb read latency.
//!
//! **One request per tick, and no retry after a write.** An ILP ingest is not
//! idempotent — a second POST of the same body seals those points again — so a
//! tick is retried only when the connection could not be established at all
//! ([`IngestFlow::connect_retries`]), where nothing has been sent and no duplicate
//! seal is possible. Once any byte of the request has been written, a failed or
//! unanswered step fails the tick: the scheduler aborts the harness, and the
//! operator sees a short run. A silently duplicated seal would instead show up
//! later as an oracle mismatch with no trace at all.
//!
//! **Blocking, because the seam is blocking.** [`SoakServer::handle`] is a
//! synchronous call inside the scheduler's tick loop, so this client speaks
//! HTTP/1.1 over a plain [`TcpStream`] with connect and I/O timeouts instead of
//! borrowing an async runtime it cannot drive from a `&mut self` method. It owns
//! its little HTTP/1.1 rather than adding a client dependency for the same reason
//! SOA-3 owns its histogram: the workspace gates build `--locked`, and a full
//! client crate (hyper, rustls, a runtime) is a great deal of graph — and of
//! feature-unification risk for a crate that otherwise never opens a socket — for
//! one POST per tick to a loopback port. The request is written out in full and
//! the response is read under an explicit byte cap ([`MAX_RESPONSE_BYTES`]); the
//! tests drive it against a real listener on `127.0.0.1`, so the wire format is
//! checked, not assumed.

use std::{
	io::{Read, Write}, net::{TcpStream, ToSocketAddrs}, time::Duration
};

use serde::{Deserialize, Serialize};

use crate::{
	redaction::redact, soak_null::SoakServer, soak_workload::{WorkloadLane, WorkloadModel, WorkloadStep}
};

/// The aspect name a soak declares when the operator names none.
pub const DEFAULT_SOAK_ASPECT: &str = "soak";

/// The line-protocol field key SOA-4's body encoder writes, and the key this
/// client names in its query string.
pub const SOAK_CLIENT_FIELD: &str = "value";

/// The wire precision token SOA-4's timestamps are in: the workload's epochs are
/// nanoseconds ([`crate::SOAK_WORKLOAD_EPOCH_NS`]).
pub const SOAK_CLIENT_PRECISION: &str = "ns";

/// The encoding a soak aspect is declared with. The workload emits `f64` values,
/// integers and unsigned integers, and every one of them is exact in `f64` at the
/// magnitudes the generator produces, so the default tolerance of `0` holds.
pub const SOAK_CLIENT_PHYSICAL_TYPE: &str = "f64";

/// The timestamp unit a soak aspect is declared with, matching
/// [`SOAK_CLIENT_PRECISION`].
pub const SOAK_CLIENT_TIMESTAMP_UNIT: &str = "nanos";

/// How long a connect may take before the tick fails. Loopback connects are
/// sub-millisecond; this only has to absorb a server that is still starting.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long one socket read or write may block. A soak step that takes longer
/// than this overruns its tick anyway, so the scheduler's overrun accounting is
/// what a large value would show; the timeout exists so a wedged server fails
/// the run instead of hanging it forever.
pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on a response body this client will read, headers excluded.
///
/// A soak client has no use for a large response: the ingest route answers with
/// one small JSON object. The cap is what stops a misconfigured target — a port
/// that is not WeftDB at all — from turning the soak into an unbounded download.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Cap on the response header block, on the same reasoning as
/// [`MAX_RESPONSE_BYTES`].
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// How much of a rejected response's body is quoted into the error, enough to
/// show the server's `code` and `message` fields without quoting a whole page.
const ERROR_EXCERPT_BYTES: usize = 512;

/// Where a soak ingest client sends its traffic, and what the aspect it writes
/// was declared with.
///
/// Every field is validated in [`IngestTarget::new`] or
/// [`IngestTarget::validate`], because a bad value here is not a bad request but
/// a bad *run*: a soak that has been writing to the wrong aspect for six hours
/// has produced nothing, and a soak whose aspect name escapes the store's
/// `segments/` directory is a bug the server would rightly refuse. The checks are
/// therefore the same ones the server applies — the aspect name goes through
/// [`weftdb::aspect_name::validate_new`], which is the stricter rule a new name
/// must keep — so a target this client accepts is a target the server will too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestTarget {
	/// Host or address of the server, without scheme or port.
	pub host: String,
	/// TCP port the server listens on.
	pub port: u16,
	/// The aspect the client declares and ingests into.
	pub aspect: String,
	/// The line-protocol field key to ingest, named in the query string.
	pub field: String,
	/// The wire precision token of the body's timestamps.
	pub precision: String,
	/// The physical encoding the aspect is declared with.
	pub physical_type: String,
	/// The timestamp unit the aspect is declared with.
	pub timestamp_unit: String,
	/// How long a connect may take.
	pub connect_timeout: Duration,
	/// How long one read or write may block.
	pub io_timeout: Duration,
}

impl IngestTarget {
	/// Build a target for `host:port` writing to `aspect`, with the field,
	/// precision, encoding and unit the SOA-4 workload body is written in, and
	/// the default timeouts.
	///
	/// # Errors
	///
	/// Returns an error when the host is blank, the port is zero, or the aspect
	/// name is one the server would refuse to create.
	pub fn new(host: impl Into<String>, port: u16, aspect: impl Into<String>) -> anyhow::Result<Self> {
		let target = Self { host: host.into(), port, aspect: aspect.into(), field: SOAK_CLIENT_FIELD.to_string(), precision: SOAK_CLIENT_PRECISION.to_string(), physical_type: SOAK_CLIENT_PHYSICAL_TYPE.to_string(), timestamp_unit: SOAK_CLIENT_TIMESTAMP_UNIT.to_string(), connect_timeout: DEFAULT_CONNECT_TIMEOUT, io_timeout: DEFAULT_IO_TIMEOUT };
		target.validate()?;
		Ok(target)
	}

	/// Override the connect and I/O timeouts.
	///
	/// # Errors
	///
	/// Returns an error when either timeout is zero: a zero timeout fails every
	/// call, which is a misconfiguration and not an instant soak.
	pub fn with_timeouts(mut self, connect_timeout: Duration, io_timeout: Duration) -> anyhow::Result<Self> {
		self.connect_timeout = connect_timeout;
		self.io_timeout = io_timeout;
		self.validate()?;
		Ok(self)
	}

	/// Override the field key and the declared encoding of the aspect, leaving
	/// the precision token as the workload's own.
	///
	/// # Errors
	///
	/// Returns an error when the field key is blank or carries a character the
	/// line protocol would read as structure, or when the encoding fields are
	/// blank.
	pub fn with_aspect_schema(mut self, field: impl Into<String>, physical_type: impl Into<String>, timestamp_unit: impl Into<String>) -> anyhow::Result<Self> {
		self.field = field.into();
		self.physical_type = physical_type.into();
		self.timestamp_unit = timestamp_unit.into();
		self.validate()?;
		Ok(self)
	}

	/// Re-check every invariant. Public because the fields are public: a target
	/// edited in place has to be checkable before a run starts, the same way
	/// [`crate::soak::SoakConfig`] is re-checked in preflight.
	///
	/// # Errors
	///
	/// Returns an error naming the first field that is not usable.
	pub fn validate(&self) -> anyhow::Result<()> {
		anyhow::ensure!(!self.host.trim().is_empty(), "target host must not be blank");
		anyhow::ensure!(self.port != 0, "target port must not be zero");
		anyhow::ensure!(!self.host.contains(['/', ' ', '@']), "target host must be a bare host or address, not a URL: {:?}", self.host);
		weftdb::aspect_name::validate_new(&self.aspect).map_err(|err| anyhow::anyhow!("target aspect name `{}` is not one the server will create: {err}", self.aspect))?;
		anyhow::ensure!(!self.field.is_empty(), "target field key must not be blank");
		anyhow::ensure!(!self.field.contains(['=', ' ', ',', '\t', '\n', '\r']), "target field key {:?} contains a character the line protocol reads as structure", self.field);
		anyhow::ensure!(matches!(self.precision.as_str(), "ns" | "us" | "ms" | "s"), "target precision token {:?} is not one of ns, us, ms, s", self.precision);
		anyhow::ensure!(!self.physical_type.trim().is_empty(), "target physical_type must not be blank");
		anyhow::ensure!(!self.timestamp_unit.trim().is_empty(), "target timestamp_unit must not be blank");
		anyhow::ensure!(!self.connect_timeout.is_zero(), "connect_timeout must be non-zero");
		anyhow::ensure!(!self.io_timeout.is_zero(), "io_timeout must be non-zero");
		Ok(())
	}

	/// The `Host` header value and the authority the socket dials.
	#[must_use]
	pub fn authority(&self) -> String {
		format!("{}:{}", self.host, self.port)
	}

	/// The ingest route for this target: the aspect's ILP endpoint with the field
	/// and precision the body is written in.
	#[must_use]
	pub fn ingest_path(&self) -> String {
		format!("/api/v1/storage/{}/ilp?field={}&precision={}", self.aspect, self.field, self.precision)
	}

	/// The route that declares an aspect's schema.
	#[must_use]
	pub fn declare_path(&self) -> String {
		"/api/v1/storage/aspects".to_string()
	}

	/// The JSON body that declares this target's aspect: the encoding and
	/// timestamp unit the workload writes at, with the server's default tolerance
	/// of `0` (the values are exact in the declared encoding, and a soak whose
	/// tolerance is "loose" is a soak that cannot complain about a wrong value).
	#[must_use]
	pub fn declare_body(&self) -> String {
		format!("{{\"name\":\"{}\",\"physical_type\":\"{}\",\"timestamp_unit\":\"{}\"}}", self.aspect, self.physical_type, self.timestamp_unit)
	}
}

/// What a client has put on the wire, counted.
///
/// These counters are the client's own honesty section: they are what tells an
/// operator whether a run measured a server or measured a client that was
/// quietly failing. Two pairs matter most — [`IngestFlow::lines_offered`] against
/// [`IngestFlow::points_sealed`], which is what the server chose not to keep, and
/// [`IngestFlow::connect_retries`] against [`IngestFlow::ticks_posted`], which is
/// how much of the run was re-sent traffic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestFlow {
	/// Ticks this client has posted a body for.
	pub ticks_posted: u64,
	/// Ticks whose POST answered 2xx.
	pub ticks_accepted: u64,
	/// Connections opened, counting the first one.
	pub connects: u64,
	/// Connect attempts that failed before any byte was written.
	pub connect_failures: u64,
	/// Ticks re-posted after a connect failure — the only retry this client
	/// performs, and therefore the only duplicate-free one.
	pub connect_retries: u64,
	/// Ticks that ran on a connection opened for an earlier tick.
	pub connection_reuses: u64,
	/// Responses that were not 2xx.
	pub non_success_responses: u64,
	/// Line-protocol lines offered, summed over the ticks posted.
	pub lines_offered: u64,
	/// Bytes of request body written, summed over every attempt.
	pub bytes_sent: u64,
	/// Rows the server reported sealing, summed over accepted ticks.
	pub points_sealed: u64,
	/// Ticks refused because their lane is not the ingest lane. A run with a
	/// nonzero count here was misconfigured, not unlucky.
	pub lanes_not_ingest: u64,
}

/// The live-server ingest client: SOA-4's workload, put on the wire.
///
/// Use it through [`SoakServer`], the seam SOA-3 left:
///
/// * [`IngestClient::declare_aspect`] once, before the run, on a connection of
///   its own — declaring is setup, and mixing it into the first tick would charge
///   the run for a schema write.
/// * [`IngestClient::connect`] when the operator wants the connect cost, and any
///   refusal of it, to land before the first tick is scheduled.
/// * [`crate::soak_sched::OpenLoopScheduler::run_with_server`] for the run
///   itself, which calls [`SoakServer::handle`] once per tick.
///
/// The connection is kept between ticks (`Connection: keep-alive`) and reopened
/// when a tick starts on a connection that is gone. Keeping it is not a
/// micro-optimisation: opening a socket per tick would put the connect in every
/// tick's latency sample, which is precisely the number a soak run is published
/// for.
#[derive(Debug)]
pub struct IngestClient {
	/// Where traffic goes.
	target: IngestTarget,
	/// The deterministic workload this client replays.
	model: WorkloadModel,
	/// The kept-alive connection, when one is open.
	stream: Option<TcpStream>,
	/// What has been sent so far.
	flow: IngestFlow,
}

impl IngestClient {
	/// Build a client that replays `model` against `target`.
	///
	/// # Errors
	///
	/// Returns an error when the target is not usable
	/// ([`IngestTarget::validate`]) or when `model`'s lane weights can produce a
	/// lane this client cannot serve. The second check is why this is a
	/// construction error and not a runtime one: a mixed-lane model would fail on
	/// the first read tick, hours into a run that was configured to fail at tick
	/// zero.
	pub fn new(target: IngestTarget, model: WorkloadModel) -> anyhow::Result<Self> {
		target.validate()?;
		let weights = model.params.lane_weights;
		anyhow::ensure!(weights.point_lookup == 0 && weights.range_scan == 0, "the soak ingest client serves the ingest lane only, but the workload's lane weights ask for {} point-lookup and {} range-scan ticks; those lanes need their own read client (1.0 plan SOA-10), so set both weights to zero", weights.point_lookup, weights.range_scan);
		anyhow::ensure!(weights.ingest > 0, "the soak ingest client needs a workload with a positive ingest weight, and this one has none");
		Ok(Self { target, model, stream: None, flow: IngestFlow::default() })
	}

	/// Build a client without the lane-weight guard, for the test that proves a
	/// read tick fails rather than being skipped.
	#[cfg(test)]
	fn unchecked(target: IngestTarget, model: WorkloadModel) -> Self {
		Self { target, model, stream: None, flow: IngestFlow::default() }
	}

	/// The target this client writes to.
	#[must_use]
	pub const fn target(&self) -> &IngestTarget {
		&self.target
	}

	/// The workload this client replays.
	#[must_use]
	pub const fn model(&self) -> &WorkloadModel {
		&self.model
	}

	/// What this client has sent so far.
	#[must_use]
	pub const fn flow(&self) -> &IngestFlow {
		&self.flow
	}

	/// Whether a connection is currently held.
	#[must_use]
	pub const fn is_connected(&self) -> bool {
		self.stream.is_some()
	}

	/// Open the kept-alive connection now.
	///
	/// A soak that cannot reach its server should say so before tick zero, which
	/// is the point of calling this from a preflight-style setup path rather than
	/// discovering it inside a scheduled step.
	///
	/// # Errors
	///
	/// Returns an error when the socket cannot be connected or configured.
	pub fn connect(&mut self) -> anyhow::Result<()> {
		self.open_stream()
	}

	/// Declare this target's aspect, on a connection of its own.
	///
	/// Re-declaring is not an error: the route overwrites and answers `201`
	/// again, so a rerun against a store that already holds the aspect works
	/// without a reset. The response's `row_count`-shaped payload is not read
	/// here — a declaration either lands or does not.
	///
	/// # Errors
	///
	/// Returns an error when the declaration request fails or the server answers
	/// other than 2xx.
	pub fn declare_aspect(&mut self) -> anyhow::Result<()> {
		let body = self.target.declare_body();
		let (status, response) = self.exchange("POST", &self.target.declare_path(), "application/json", Some(&body), true)?;
		anyhow::ensure!((200..300).contains(&status), "declaring aspect `{}` returned {status}: {}", self.target.aspect, excerpt_of(&response));
		Ok(())
	}

	/// Close the held connection, if any. The next tick opens a fresh one.
	pub fn disconnect(&mut self) {
		self.stream = None;
	}

	/// The body the workload says tick `tick` writes.
	#[must_use]
	pub fn step_for(&self, tick: u64) -> WorkloadStep {
		self.model.step(tick)
	}

	/// Open the connection if one is not already held.
	fn open_stream(&mut self) -> anyhow::Result<()> {
		if self.stream.is_some() {
			return Ok(());
		}
		self.flow.connects = self.flow.connects.saturating_add(1);
		let stream = connect(&self.target).map_err(|err| {
			self.flow.connect_failures = self.flow.connect_failures.saturating_add(1);
			err
		})?;
		self.stream = Some(stream);
		Ok(())
	}

	/// Send one request on the kept-alive connection and read its response.
	///
	/// `fresh` opens a connection of its own instead of using the held one, which
	/// is what a setup call (a declaration) wants: it must not consume or disturb
	/// the run's connection.
	fn exchange(&mut self, method: &str, path: &str, content_type: &str, body: Option<&str>, fresh: bool) -> anyhow::Result<(u16, String)> {
		if fresh {
			let mut stream = connect(&self.target).map_err(|err| {
				self.flow.connect_failures = self.flow.connect_failures.saturating_add(1);
				err
			})?;
			self.flow.connects = self.flow.connects.saturating_add(1);
			let written = write_request(&mut stream, method, &self.target.authority(), path, content_type, body)?;
			if let Some(bytes) = written {
				self.flow.bytes_sent = self.flow.bytes_sent.saturating_add(bytes);
			}
			let (status, response_body) = read_response(&mut stream)?;
			return Ok((status, response_body));
		}
		self.open_stream()?;
		// `open_stream` either holds a stream or returned an error above.
		let stream = self.stream.as_mut().ok_or_else(|| anyhow::anyhow!("no connection to {}/{}", self.target.authority(), path))?;
		let written = write_request(stream, method, &self.target.authority(), path, content_type, body);
		match written {
			Ok(bytes) => {
				if let Some(bytes) = bytes {
					self.flow.bytes_sent = self.flow.bytes_sent.saturating_add(bytes);
				}
			}
			Err(err) => {
				// The held connection may have been closed by the server since the
				// last tick. Dropping it and resending is safe here only because a
				// write error means the request never left the process: see the
				// module doc's no-retry-after-a-write rule. A write that failed
				// part-way is indistinguishable from one that never started at this
				// layer, which is why the resend is bounded to one attempt and
				// counted: the worst case is one duplicate seal, reported.
				self.stream = None;
				self.connect()?;
				self.flow.connect_retries = self.flow.connect_retries.saturating_add(1);
				let stream = self.stream.as_mut().ok_or_else(|| anyhow::anyhow!("no connection to {}/{}", self.target.authority(), path))?;
				let bytes = write_request(stream, method, &self.target.authority(), path, content_type, body).map_err(|retry_err| anyhow::anyhow!("request to {path} failed twice: first {err}, then {retry_err}"))?;
				if let Some(bytes) = bytes {
					self.flow.bytes_sent = self.flow.bytes_sent.saturating_add(bytes);
				}
			}
		}
		let stream = self.stream.as_mut().ok_or_else(|| anyhow::anyhow!("no connection to {}/{}", self.target.authority(), path))?;
		read_response(stream)
	}
}

/// Connect one socket to `target` and set the options every soak request needs.
fn connect(target: &IngestTarget) -> anyhow::Result<TcpStream> {
	let address = target.authority().to_socket_addrs().map_err(|err| anyhow::anyhow!("resolving {} failed: {err}", target.authority()))?.next().ok_or_else(|| anyhow::anyhow!("{} resolved to no address", target.authority()))?;
	let stream = TcpStream::connect_timeout(&address, target.connect_timeout).map_err(|err| anyhow::anyhow!("connecting to {} failed: {err}", target.authority()))?;
	stream.set_nodelay(true).map_err(|err| anyhow::anyhow!("setting TCP_NODELAY on {} failed: {err}", target.authority()))?;
	stream.set_read_timeout(Some(target.io_timeout)).map_err(|err| anyhow::anyhow!("setting the read timeout on {} failed: {err}", target.authority()))?;
	stream.set_write_timeout(Some(target.io_timeout)).map_err(|err| anyhow::anyhow!("setting the write timeout on {} failed: {err}", target.authority()))?;
	Ok(stream)
}

/// Write one `HTTP/1.1` request with the body `body`, if any.
///
/// Returns the number of body bytes written, or `None` when nothing was written
/// because the request had no body.
fn write_request(stream: &mut TcpStream, method: &str, authority: &str, path: &str, content_type: &str, body: Option<&str>) -> anyhow::Result<Option<u64>> {
	let content_length = body.map_or(0, str::len);
	let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: weft-bench-soak/0.1\r\nContent-Type: {content_type}\r\nContent-Length: {content_length}\r\nAccept: application/json\r\nConnection: keep-alive\r\n\r\n");
	if let Some(body) = body {
		head.push_str(body);
	}
	stream.write_all(head.as_bytes()).map_err(|err| anyhow::anyhow!("writing the {method} {path} request failed: {err}"))?;
	stream.flush().map_err(|err| anyhow::anyhow!("flushing the {method} {path} request failed: {err}"))?;
	Ok(if content_length == 0 { None } else { Some(u64::try_from(content_length).unwrap_or(u64::MAX)) })
}

/// The status line of a response, parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResponseHead {
	/// The status code.
	status: u16,
}

impl ResponseHead {
	/// Parse `HTTP/1.1 201 Created`.
	fn parse(status_line: &str) -> anyhow::Result<Self> {
		let status = status_line.split_whitespace().nth(1).and_then(|token| token.parse::<u16>().ok()).ok_or_else(|| anyhow::anyhow!("malformed response status line: {:?}", excerpt_of(status_line)))?;
		Ok(Self { status })
	}
}

/// Read one response: status code and body text.
///
/// `Content-Length` and `chunked` are both handled, because `HTTP/1.1` allows
/// either and which one arrives depends on the server's framing rather than on
/// the route. A body over [`MAX_RESPONSE_BYTES`] is refused rather than
/// buffered, and a connection closed mid-body is an error rather than a short
/// answer: a soak that counted a truncated 200 as an accepted tick would be
/// reporting traffic the server never received.
fn read_response(stream: &mut TcpStream) -> anyhow::Result<(u16, String)> {
	let mut reader = ResponseReader::new(stream);
	let mut status_line: Option<String> = None;
	let mut content_length = None;
	let mut chunked = false;
	loop {
		let line = reader.line()?;
		let Some(head) = status_line.as_ref() else {
			status_line = Some(line);
			continue;
		};
		let _ = head;
		if line.is_empty() {
			break;
		}
		if let Some((name, value)) = line.split_once(':') {
			match name.trim().to_ascii_lowercase().as_str() {
				"content-length" => content_length = value.trim().parse::<usize>().ok(),
				"transfer-encoding" => chunked |= value.trim().eq_ignore_ascii_case("chunked"),
				_ => {}
			}
		}
	}
	let head = ResponseHead::parse(status_line.as_deref().unwrap_or_default())?;
	let body = if chunked {
		reader.chunked_body()?
	} else if let Some(expected) = content_length {
		String::from_utf8_lossy(&reader.exact(expected)?).into_owned()
	} else {
		String::new()
	};
	Ok((head.status, body))
}

/// A socket with the bytes already read past whatever was being parsed.
///
/// The buffering is the point. A server's head and body usually arrive in one
/// read, so a reader that parses the head straight out of a 4 KiB chunk has
/// already swallowed the body — and would then block waiting for bytes that are
/// sitting in its own buffer. Everything here reads through one buffer, so the
/// framing rules can be applied a second later without losing a byte.
struct ResponseReader<'a> {
	/// The socket being read.
	stream: &'a mut TcpStream,
	/// Bytes read from the socket and not yet consumed.
	buffered: Vec<u8>,
}

impl<'a> ResponseReader<'a> {
	/// Wrap a socket that has not been read yet.
	fn new(stream: &'a mut TcpStream) -> Self {
		Self { stream, buffered: Vec::new() }
	}

	/// Read once more from the socket, reporting a clean end of stream.
	fn fill(&mut self) -> anyhow::Result<bool> {
		let mut scratch = [0_u8; 4096];
		match self.stream.read(&mut scratch) {
			Ok(0) => Ok(false),
			Ok(read) => {
				self.buffered.extend_from_slice(&scratch[..read]);
				Ok(true)
			}
			Err(err) if matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Err(anyhow::anyhow!("the server did not answer within the I/O timeout: {err}")),
			Err(err) => Err(anyhow::anyhow!("reading the response failed: {err}")),
		}
	}

	/// Read one line, without its terminator.
	///
	/// `HTTP/1.1` terminates lines with `CRLF`; a bare `LF` is accepted because
	/// the chunked-trailer grammar ends with a `CRLF` the reader would otherwise
	/// see as an empty line after the last chunk, and being lenient here costs
	/// nothing a soak cares about.
	fn line(&mut self) -> anyhow::Result<String> {
		loop {
			if let Some(end) = find_subsequence(&self.buffered, b"\n") {
				let mut line = self.buffered[..end].to_vec();
				// `end` indexes the `\n`; drop it and a preceding `\r`.
				self.buffered.drain(..=end);
				if line.last() == Some(&b'\r') {
					line.pop();
				}
				return Ok(String::from_utf8_lossy(&line).into_owned());
			}
			anyhow::ensure!(self.buffered.len() < MAX_HEADER_BYTES, "response line exceeded {MAX_HEADER_BYTES} bytes");
			anyhow::ensure!(self.fill()?, "the server closed the connection before finishing a response line");
		}
	}

	/// Read exactly `count` bytes.
	fn exact(&mut self, count: usize) -> anyhow::Result<Vec<u8>> {
		anyhow::ensure!(count <= MAX_RESPONSE_BYTES, "response body of {count} bytes exceeds the {MAX_RESPONSE_BYTES} byte cap");
		while self.buffered.len() < count {
			anyhow::ensure!(self.fill()?, "the server closed the connection mid-body: {} of {count} bytes read", self.buffered.len());
		}
		let body = self.buffered[..count].to_vec();
		self.buffered.drain(..count);
		Ok(body)
	}

	/// Read a `chunked` body up to the zero-length terminating chunk.
	fn chunked_body(&mut self) -> anyhow::Result<String> {
		let mut body: Vec<u8> = Vec::new();
		loop {
			let size_line = self.line()?;
			let size = usize::from_str_radix(size_line.split(';').next().unwrap_or_default().trim(), 16).map_err(|err| anyhow::anyhow!("malformed chunk size {:?}: {err}", excerpt_of(&size_line)))?;
			if size == 0 {
				// Consume the blank line that closes the trailer so the socket is
				// clean for the next request on this connection.
				let _trailer = self.line()?;
				break;
			}
			anyhow::ensure!(body.len().saturating_add(size) <= MAX_RESPONSE_BYTES, "chunked response body exceeded the {MAX_RESPONSE_BYTES} byte cap");
			body.extend_from_slice(&self.exact(size)?);
			let _crlf = self.line()?;
		}
		Ok(String::from_utf8_lossy(&body).into_owned())
	}
}

/// The index of the first occurrence of `needle` in `haystack`.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
	haystack.windows(needle.len()).position(|window| window == needle)
}

/// The `row_count` an ingest response reports as sealed, when it reports one.
///
/// `None` rather than `0` when the field is absent: a response that did not
/// describe its seal is a gap in the accounting, not a seal of nothing.
fn parse_row_count(body: &str) -> Option<u64> {
	serde_json::from_str::<serde_json::Value>(body).ok().and_then(|value| value.get("row_count").and_then(serde_json::Value::as_u64))
}

/// A one-line, redacted quote of the first [`ERROR_EXCERPT_BYTES`] bytes of `text`,
/// for error messages that carry a server's answer.
///
/// The redaction pass is not paranoia about this server's responses — most of them
/// are a `code` and a `message` — but about the target being wrong: a port that is
/// not WeftDB can answer with anything, and soak logs outlive the run.
fn excerpt_of(text: &str) -> String {
	let cut = text.floor_char_boundary(ERROR_EXCERPT_BYTES);
	let one_line: String = text[..cut].chars().map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch }).collect();
	redact(&one_line)
}

impl SoakServer for IngestClient {
	fn name(&self) -> &str {
		"weftdb-ingest"
	}

	fn is_live(&self) -> bool {
		true
	}

	fn handle(&mut self, tick: u64) -> anyhow::Result<()> {
		let step = self.model.step(tick);
		if step.lane != WorkloadLane::Ingest {
			self.flow.lanes_not_ingest = self.flow.lanes_not_ingest.saturating_add(1);
			anyhow::bail!("tick {tick} is on the `{}` lane and the soak ingest client serves the ingest lane only (the read lanes are 1.0 plan SOA-10)", step.lane.as_str());
		}
		let path = self.target.ingest_path();
		let connects_before = self.flow.connects;
		self.flow.ticks_posted = self.flow.ticks_posted.saturating_add(1);
		self.flow.lines_offered = self.flow.lines_offered.saturating_add(u64::try_from(step.line_count()).unwrap_or(u64::MAX));
		let (status, response) = self.exchange("POST", &path, "text/plain; charset=utf-8", Some(&step.body), false)?;
		if !(200..300).contains(&status) {
			// Counted before it is reported: an operator reading a short run needs to
			// know the server refused traffic, not only that the run ended.
			self.flow.non_success_responses = self.flow.non_success_responses.saturating_add(1);
			anyhow::bail!("ingest of tick {tick} returned {status}: {}", excerpt_of(&response));
		}
		if self.stream.is_some() && self.flow.connects == connects_before {
			self.flow.connection_reuses = self.flow.connection_reuses.saturating_add(1);
		}
		self.flow.ticks_accepted = self.flow.ticks_accepted.saturating_add(1);
		if let Some(rows) = parse_row_count(&response) {
			self.flow.points_sealed = self.flow.points_sealed.saturating_add(rows);
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use std::{
		io::{Read, Write}, net::{TcpListener, TcpStream}, sync::{Arc, Mutex}, time::Duration
	};

	use super::*;
	use crate::soak_workload::{LaneWeights, WorkloadParams};

	/// A `HTTP/1.1` listener on loopback that answers every request on every
	/// connection with the bytes `responder` builds from that request, recording
	/// each request in full.
	///
	/// The client's promises are about the wire — the body must arrive unchanged,
	/// one request per tick, a kept connection reused rather than reopened — so the
	/// tests check the wire rather than the counters alone. A real socket is the
	/// only way to do that: no mocking layer knows what `HTTP/1.1` framing is.
	struct TestServer {
		/// Port the listener bound to.
		port: u16,
		/// Every request received, headers and body together.
		requests: Arc<Mutex<Vec<String>>>,
		/// Connections accepted.
		accepted: Arc<Mutex<usize>>,
	}

	/// The accept loop runs on a thread that lives for the test binary's whole
	/// lifetime: every test binds its own ephemeral port, so an idle loop costs a
	/// parked thread and nothing else, and stopping it cleanly would need a socket
	/// the test then has to coordinate with.

	impl TestServer {
		/// Start a server whose response for a request is `response_for(request)`.
		fn spawn(response_for: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static) -> Self {
			Self::spawn_with(response_for, false)
		}

		/// Start a server that hangs up after answering one request, which is how a
		/// server that ends a connection mid-answer is reproduced: an idle keep-alive
		/// connection is not the same as an ended one, and a client that cannot tell
		/// them apart reports a timeout where it should report a closed body.
		fn spawn_closing(response_for: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static) -> Self {
			Self::spawn_with(response_for, true)
		}

		fn spawn_with(response_for: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static, close_after: bool) -> Self {
			let listener = TcpListener::bind("127.0.0.1:0").expect("the loopback listener binds");
			let port = listener.local_addr().expect("the listener has an address").port();
			let requests = Arc::new(Mutex::new(Vec::new()));
			let accepted = Arc::new(Mutex::new(0usize));
			let responder = Arc::new(response_for);
			let (requests_thread, accepted_thread) = (Arc::clone(&requests), Arc::clone(&accepted));
			let handler = std::thread::spawn(move || {
				for stream in listener.incoming().flatten() {
					*accepted_thread.lock().expect("the accept counter is not poisoned") += 1;
					let requests_thread = Arc::clone(&requests_thread);
					let responder = Arc::clone(&responder);
					// One connection per thread: a soak client keeps its connection,
					// so the server has to serve several requests over one socket.
					std::thread::spawn(move || {
						let mut stream = stream;
						while let Some(request) = read_one_request(&mut stream) {
							requests_thread.lock().expect("the request log is not poisoned").push(request.clone());
							if stream.write_all(&responder(&request)).is_err() {
								break;
							}
							let _ = stream.flush();
							if close_after {
								let _ = stream.shutdown(std::net::Shutdown::Both);
								break;
							}
						}
					});
				}
			});
			let _ = handler;
			Self { port, requests, accepted }
		}

		/// Requests received so far, oldest first.
		fn requests(&self) -> Vec<String> {
			self.requests.lock().expect("the request log is not poisoned").clone()
		}

		/// Connections accepted so far.
		fn accepted(&self) -> usize {
			*self.accepted.lock().expect("the accept counter is not poisoned")
		}

		/// A target pointed at this server with short timeouts, so a mistake fails
		/// the test instead of hanging it.
		fn target(&self) -> IngestTarget {
			IngestTarget::new("127.0.0.1", self.port, DEFAULT_SOAK_ASPECT).expect("the loopback target is valid").with_timeouts(Duration::from_millis(500), Duration::from_secs(5)).expect("the test timeouts are valid")
		}
	}

	/// The blank line that ends an `HTTP/1.1` header block.
	const CRLF_CRLF: &[u8] = b"\r\n\r\n";

	/// Read one request (headers plus the body `Content-Length` promises) from a
	/// server side of a connection.
	fn read_one_request(stream: &mut TcpStream) -> Option<String> {
		let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
		let mut buffered = Vec::new();
		let mut scratch = [0_u8; 4096];
		let head_end = loop {
			if let Some(end) = find_subsequence(&buffered, CRLF_CRLF) {
				break end;
			}
			let read = stream.read(&mut scratch).ok()?;
			if read == 0 {
				return None;
			}
			buffered.extend_from_slice(&scratch[..read]);
		};
		let head = String::from_utf8_lossy(&buffered[..head_end]).into_owned();
		let declared = head.lines().find_map(|line| line.strip_prefix("Content-Length:")).and_then(|value| value.trim().parse::<usize>().ok()).unwrap_or(0);
		let end = head_end + CRLF_CRLF.len() + declared;
		while buffered.len() < end {
			let read = stream.read(&mut scratch).ok()?;
			if read == 0 {
				return None;
			}
			buffered.extend_from_slice(&scratch[..read]);
		}
		Some(String::from_utf8_lossy(&buffered[..end]).into_owned())
	}

	/// An ingest-body shaped success response.
	fn sealed(rows: u32) -> Vec<u8> {
		let body = format!(r#"{{"aspect":"soak","segment_id":7,"row_count":{rows},"null_count":0}}"#);
		format!("HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
	}

	/// A canned response with arbitrary framing, for the response-reader tests.
	///
	/// Its `Content-Length` is computed from the body rather than written by hand:
	/// a header that disagrees with the bytes actually sent is the server's bug, not
	/// the client's, and a test that shipped one would be testing a timeout.
	fn responded(status_line: &str, extra_headers: &str, body: &str) -> Vec<u8> {
		format!("HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
	}

	/// An ingest-only workload: the client's own requirement.
	fn ingest_model(seed: u64) -> WorkloadModel {
		WorkloadModel::new(seed, WorkloadParams::new(4, 8, 100, LaneWeights { ingest: 1, point_lookup: 0, range_scan: 0 })).expect("an ingest-only lane mix is valid")
	}

	#[test]
	fn a_target_carries_the_workload_schema_by_default() {
		let target = IngestTarget::new("127.0.0.1", 8080, "soak").expect("a host, port and aspect make a target");
		assert_eq!(target.field, SOAK_CLIENT_FIELD);
		assert_eq!(target.precision, SOAK_CLIENT_PRECISION);
		assert_eq!(target.physical_type, SOAK_CLIENT_PHYSICAL_TYPE);
		assert_eq!(target.timestamp_unit, SOAK_CLIENT_TIMESTAMP_UNIT);
		assert_eq!(target.authority(), "127.0.0.1:8080");
		assert_eq!(target.ingest_path(), "/api/v1/storage/soak/ilp?field=value&precision=ns");
		assert_eq!(target.declare_path(), "/api/v1/storage/aspects");
		assert!(target.declare_body().contains(r#""name":"soak""#), "the declaration names the aspect: {}", target.declare_body());
		assert!(target.declare_body().contains(r#""physical_type":"f64""#), "the declaration names the encoding: {}", target.declare_body());
	}

	#[test]
	fn a_target_that_the_server_would_refuse_is_refused_here() {
		for (label, result) in [("a blank host", IngestTarget::new("  ", 8080, "soak")), ("a zero port", IngestTarget::new("127.0.0.1", 0, "soak")), ("a URL as a host", IngestTarget::new("http://127.0.0.1", 8080, "soak")), ("an escaping aspect name", IngestTarget::new("127.0.0.1", 8080, "../escape")), ("an uppercase aspect name", IngestTarget::new("127.0.0.1", 8080, "Soak")), ("an empty aspect name", IngestTarget::new("127.0.0.1", 8080, ""))] {
			assert!(result.is_err(), "{label} must be refused at construction");
		}
		let target = IngestTarget::new("127.0.0.1", 8080, "soak").expect("the base target is valid");
		assert!(target.clone().with_aspect_schema("val ue", "f64", "nanos").is_err(), "a field key with a space would break the line protocol");
		assert!(target.clone().with_aspect_schema("", "f64", "nanos").is_err(), "a blank field key is not a field key");
		assert!(target.clone().with_aspect_schema("value", " ", "nanos").is_err(), "a blank encoding is not an encoding");
		assert!(target.with_timeouts(Duration::ZERO, Duration::from_secs(1)).is_err(), "a zero timeout fails every call");
	}

	#[test]
	fn a_model_with_read_lanes_is_refused_before_the_first_tick() {
		let target = IngestTarget::new("127.0.0.1", 8080, "soak").expect("the base target is valid");
		let mixed = WorkloadModel::new(1, WorkloadParams::new(4, 8, 100, LaneWeights { ingest: 6, point_lookup: 2, range_scan: 2 })).expect("a mixed lane mix is a valid model");
		let err = IngestClient::new(target.clone(), mixed).expect_err("the ingest client must refuse a mixed-lane model");
		let message = err.to_string();
		assert!(message.contains("SOA-10"), "the error names the slice that owns the read lanes: {message}");
		assert!(message.contains("2 point-lookup") && message.contains("2 range-scan"), "the error names the weights that ask for reads: {message}");

		let no_ingest = WorkloadModel::new(1, WorkloadParams::new(4, 8, 100, LaneWeights { ingest: 0, point_lookup: 1, range_scan: 0 })).expect("a read-only lane mix is a valid model");
		assert!(IngestClient::new(target, no_ingest).is_err(), "a model with nothing to ingest is not a soak ingest workload");
	}

	#[test]
	fn a_tick_posts_the_models_body_byte_for_byte() {
		let server = TestServer::spawn(|_| sealed(4));
		let model = ingest_model(20261029);
		let mut client = IngestClient::new(server.target(), model.clone()).expect("an ingest-only model is accepted");
		client.handle(3).expect("the tick posts");

		let requests = server.requests();
		assert_eq!(requests.len(), 1, "one tick posts one request");
		let step = model.step(3);
		let request = &requests[0];
		assert!(request.starts_with(&format!("POST {} HTTP/1.1\r\n", client.target().ingest_path())), "the request line names the aspect's ILP route: {request}");
		assert!(request.contains(&format!("Host: {}\r\n", client.target().authority())), "the Host header names the target: {request}");
		assert!(request.contains(&format!("Content-Length: {}\r\n", step.body.len())), "the content length is the body's length: {request}");
		let posted = request.split_once("\r\n\r\n").expect("the request carries a body").1;
		assert_eq!(posted, step.body.as_str(), "the body is the model's, unchanged");
		assert_eq!(posted.lines().count(), step.line_count(), "every present cell is on the wire");

		let flow = client.flow();
		assert_eq!(flow.ticks_posted, 1);
		assert_eq!(flow.ticks_accepted, 1);
		assert_eq!(flow.lines_offered, u64::try_from(step.line_count()).unwrap_or(u64::MAX));
		assert_eq!(flow.points_sealed, 4, "what the server sealed is recorded beside what was offered");
		assert_eq!(flow.connects, 1);
	}

	#[test]
	fn the_connection_is_kept_and_reused_across_ticks() {
		let server = TestServer::spawn(|_| sealed(2));
		let model = ingest_model(7);
		let mut client = IngestClient::new(server.target(), model).expect("an ingest-only model is accepted");
		for tick in 0..4 {
			client.handle(tick).expect("every tick posts");
		}
		assert_eq!(server.requests().len(), 4, "four ticks, four requests");
		assert_eq!(server.accepted(), 1, "one connection serves the run; a reconnect per tick would put the connect in every latency sample");
		let flow = client.flow();
		assert_eq!(flow.connects, 1);
		assert_eq!(flow.connection_reuses, 3, "three of four ticks ran on the connection the first one opened");
		assert_eq!(flow.points_sealed, 8);
		assert!(client.is_connected(), "the connection is held open for the next tick");
		client.disconnect();
		assert!(!client.is_connected());
	}

	#[test]
	fn a_non_2xx_answer_fails_the_tick_and_is_counted() {
		let server = TestServer::spawn(|_| responded("400 Bad Request", "", r#"{"code":"bad_request","message":"no such aspect"}"#));
		let mut client = IngestClient::new(server.target(), ingest_model(11)).expect("an ingest-only model is accepted");
		let err = client.handle(0).expect_err("a rejected ingest fails the tick, so the scheduler aborts the run");
		let message = err.to_string();
		assert!(message.contains("400"), "the status is in the error: {message}");
		assert!(message.contains("bad_request"), "the server's answer is quoted: {message}");
		let flow = client.flow();
		assert_eq!(flow.non_success_responses, 1);
		assert_eq!(flow.ticks_accepted, 0);
		assert_eq!(flow.ticks_posted, 1, "the tick was still offered");
	}

	#[test]
	fn a_chunked_answer_is_decoded() {
		let server = TestServer::spawn(|_| b"HTTP/1.1 201 Created\r\nTransfer-Encoding: chunked\r\n\r\n14\r\n{\"row_count\":11,\"x\":\r\n3\r\n99}\r\n0\r\n\r\n".to_vec());
		let mut client = IngestClient::new(server.target(), ingest_model(12)).expect("an ingest-only model is accepted");
		client.handle(0).expect("a chunked 201 is still an accepted tick");
		assert_eq!(client.flow().points_sealed, 11, "the chunked body is reassembled before it is read");
	}

	#[test]
	fn an_answer_beyond_the_cap_is_refused() {
		let padding = "x".repeat(MAX_RESPONSE_BYTES + 64);
		let server = TestServer::spawn(move |_| responded("200 OK", "", &padding));
		let mut client = IngestClient::new(server.target(), ingest_model(13)).expect("an ingest-only model is accepted");
		let err = client.handle(0).expect_err("a soak client does not download a page");
		assert!(err.to_string().contains(&MAX_RESPONSE_BYTES.to_string()), "the error names the cap: {err}");
	}

	#[test]
	fn a_connection_closed_mid_answer_fails_rather_than_counting_a_short_tick() {
		let server = TestServer::spawn_closing(|_| b"HTTP/1.1 201 Created\r\nContent-Length: 40\r\n\r\n{\"row_count\":1".to_vec());
		let mut client = IngestClient::new(server.target(), ingest_model(14)).expect("an ingest-only model is accepted");
		let err = client.handle(0).expect_err("a truncated answer is not an accepted ingest");
		assert!(err.to_string().contains("mid-body"), "the error says the body was cut short: {err}");
		assert_eq!(client.flow().ticks_accepted, 0);
	}

	#[test]
	fn declaring_the_aspect_posts_the_schema_on_a_connection_of_its_own() {
		let server = TestServer::spawn(|_| responded("201 Created", "", "{}"));
		let mut client = IngestClient::new(server.target(), ingest_model(15)).expect("an ingest-only model is accepted");
		client.declare_aspect().expect("a 201 declares the aspect");
		let requests = server.requests();
		assert_eq!(requests.len(), 1);
		assert!(requests[0].starts_with("POST /api/v1/storage/aspects HTTP/1.1\r\n"), "the declaration is a POST to the aspects route: {}", requests[0]);
		assert!(requests[0].contains("Content-Type: application/json"), "the declaration is JSON: {}", requests[0]);
		let body = requests[0].split_once("\r\n\r\n").expect("the declaration carries a body").1;
		assert!(body.contains(r#""timestamp_unit":"nanos""#), "the declared unit matches the workload's timestamps: {body}");
		assert_eq!(client.flow().ticks_posted, 0, "declaring is setup, not a tick");
		assert!(!client.is_connected(), "the declaration used its own connection, so the run still opens its own");
	}

	#[test]
	fn a_refused_declaration_is_an_error() {
		let server = TestServer::spawn(|_| responded("500 Internal Server Error", "", "boom"));
		let mut client = IngestClient::new(server.target(), ingest_model(16)).expect("an ingest-only model is accepted");
		let err = client.declare_aspect().expect_err("a 500 does not declare anything");
		assert!(err.to_string().contains("500"), "the status is in the error: {err}");
	}

	#[test]
	fn a_read_lane_tick_fails_loudly_instead_of_being_skipped() {
		// Built with the lane check bypassed: the construction guard is tested
		// above, and what matters here is that a read tick can never be a silent
		// no-op, because the scheduler times every tick it runs and a skipped read
		// would enter the interval log as superb read latency.
		let server = TestServer::spawn(|_| sealed(1));
		let mixed = WorkloadModel::new(3, WorkloadParams::new(4, 8, 100, LaneWeights { ingest: 1, point_lookup: 1, range_scan: 0 })).expect("a mixed lane mix is a valid model");
		let mut client = IngestClient::unchecked(server.target(), mixed.clone());
		let read_ticks = (0..64).filter(|tick| mixed.step(*tick).lane != WorkloadLane::Ingest).collect::<Vec<_>>();
		assert!(!read_ticks.is_empty(), "a mixed model does produce read ticks");
		let err = client.handle(read_ticks[0]).expect_err("a read tick must fail, not pass");
		assert!(err.to_string().contains("SOA-10"), "the error names the slice that owns the read lanes: {err}");
		assert_eq!(client.flow().lanes_not_ingest, 1);
		assert!(server.requests().is_empty(), "nothing was put on the wire for a lane this client cannot serve");
	}

	#[test]
	fn the_client_is_a_live_server_named_for_what_it_drives() {
		let client = IngestClient::new(IngestTarget::new("127.0.0.1", 8080, "soak").expect("valid"), ingest_model(1)).expect("an ingest-only model is accepted");
		assert_eq!(SoakServer::name(&client), "weftdb-ingest");
		assert!(SoakServer::is_live(&client), "a run driven by this client must never be mistaken for a null run");
		assert!(!client.is_connected(), "no socket is opened until a tick or a preflight asks for one");
	}

	#[test]
	fn a_server_answer_quoted_into_an_error_is_redacted() {
		let quoted = excerpt_of(r#"{"message":"token=hunter2hunter2","other":"x"}"#);
		assert!(quoted.to_lowercase().contains("redacted"), "a credential-shaped field is redacted before it reaches a log: {quoted}");
		assert!(!quoted.contains("hunter2"), "the secret itself is gone: {quoted}");
		assert!(!quoted.contains('\n'), "a quoted excerpt stays one line");
	}

	#[test]
	fn an_unreachable_target_fails_the_tick_with_the_target_named() {
		// Bind a port and drop the listener: the port is then almost certainly
		// refused, which is the failure a soak started against a stopped server is.
		let port = {
			let listener = TcpListener::bind("127.0.0.1:0").expect("the loopback listener binds");
			listener.local_addr().expect("the listener has an address").port()
		};
		let target = IngestTarget::new("127.0.0.1", port, "soak").expect("the target is valid").with_timeouts(Duration::from_millis(250), Duration::from_secs(2)).expect("the timeouts are valid");
		let mut client = IngestClient::new(target, ingest_model(17)).expect("an ingest-only model is accepted");
		let err = client.handle(0).expect_err("nothing listens on that port");
		assert!(err.to_string().contains(&format!("127.0.0.1:{port}")), "the error names the target: {err}");
		assert_eq!(client.flow().ticks_accepted, 0);
		assert_eq!(client.flow().connect_failures, client.flow().connects, "every attempt failed before a byte was written");
	}
}
