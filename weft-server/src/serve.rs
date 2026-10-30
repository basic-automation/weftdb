//! The one HTTP/1.1 serve path (release plan §3 SEC-4, §4 C9).
//!
//! [`run`] is the only place this crate serves HTTP. It replaces the bare
//! `axum::serve` call the binary used before SEC-4, which accepted without
//! limit, waited on headers and on keep-alive peers forever, and had no
//! shutdown signal at all. Every listener goes through here — the plaintext
//! listener via [`run`], SEC-5's TLS listener via [`run_tls`], which is the
//! same loop with a rustls handshake in front of each connection — so the
//! pre-auth limits below apply before any route, gate or auth code runs:
//! a connection is counted at accept time, before a single byte is read.
//!
//! ## Limits (all pre-auth, all per connection)
//!
//! - **Connection cap.** At most [`ServeLimits::max_connections`] connections
//!   are served at once. The cap is held as a semaphore permit taken *before*
//!   `accept`, so excess connections wait in the kernel backlog unread rather
//!   than being accepted and parked. The default is derived from the
//!   process's `RLIMIT_NOFILE` soft limit: half of it, so connection sockets
//!   can never consume the descriptors the store needs for its own files,
//!   floored at [`MIN_MAX_CONNECTIONS`] and capped at
//!   [`MAX_MAX_CONNECTIONS`]. `WEFT_MAX_CONNECTIONS` overrides it.
//! - **Header deadline.** A peer has [`ServeLimits::header_read_timeout`] to
//!   deliver a complete request header block (hyper's
//!   `header_read_timeout`, driven by a tokio timer). This is the slow-loris
//!   bound ROB D8 asked for.
//! - **Keep-alive deadline.** A connection that makes no read progress for
//!   [`ServeLimits::keep_alive_timeout`] is closed. Between requests that is
//!   exactly the idle keep-alive bound; the deadline only ever fires when no
//!   bytes are arriving, because available input is always taken first.
//! - **Write-stall timeout.** A connection whose writes make no progress for
//!   [`ServeLimits::write_stall_timeout`] is closed, so a peer that stops
//!   reading cannot pin a connection task (and its cap permit) forever.
//!
//! ## Shutdown (C9)
//!
//! Signal handlers are installed first — [`ShutdownSignals::install`] is
//! called by startup before any other phase that can block — and the future
//! they produce is handed to [`run`]. A signal that arrives before serving
//! starts is therefore already recorded when `run` is entered: `run` sees
//! the shutdown future ready, drains nothing, and returns `Ok`, so the
//! process exits 0. While serving, a signal stops the accept loop; live
//! connections then drain for up to [`SHUTDOWN_DRAIN`] (the 5 s of C9's
//! drain order) before the remainder are aborted. The later drain steps —
//! SEC-9's task tracker and D-S14's `store.shutdown()` — belong to their
//! own slices and slot in after `run` returns.

use std::{
	future::Future, io, num::NonZeroUsize, pin::Pin, sync::Arc, task::{Context, Poll}, time::Duration
};

use axum::Router;
use hyper::server::conn::http1;
use hyper_util::{
	rt::{TokioIo, TokioTimer}, service::TowerToHyperService
};
use tokio::{
	net::TcpListener, sync::Semaphore, task::JoinSet, time::{Instant, Sleep}
};
use tokio_rustls::TlsAcceptor;

use crate::tls::TlsConfig;

/// Environment variable that sets the connection cap directly, replacing the
/// `RLIMIT_NOFILE`-derived default: a positive integer, read once at startup.
pub const MAX_CONNECTIONS_ENV: &str = "WEFT_MAX_CONNECTIONS";

/// Environment variable that sets the header deadline, in whole seconds:
/// a positive integer, read once at startup.
pub const HEADER_READ_TIMEOUT_ENV: &str = "WEFT_HEADER_READ_TIMEOUT_SECS";

/// Environment variable that sets the keep-alive (read-idle) deadline, in
/// whole seconds: a positive integer, read once at startup.
pub const KEEP_ALIVE_TIMEOUT_ENV: &str = "WEFT_KEEP_ALIVE_TIMEOUT_SECS";

/// Environment variable that sets the write-stall timeout, in whole seconds:
/// a positive integer, read once at startup.
pub const WRITE_STALL_TIMEOUT_ENV: &str = "WEFT_WRITE_STALL_TIMEOUT_SECS";

/// Default header deadline when `WEFT_HEADER_READ_TIMEOUT_SECS` is unset:
/// hyper's own default, made explicit so it is a decision rather than a
/// library accident.
pub const DEFAULT_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Default keep-alive deadline when `WEFT_KEEP_ALIVE_TIMEOUT_SECS` is unset.
pub const DEFAULT_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(30);

/// Default write-stall timeout when `WEFT_WRITE_STALL_TIMEOUT_SECS` is unset.
pub const DEFAULT_WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(10);

/// The connection cap when the `RLIMIT_NOFILE` soft limit cannot be read
/// (a non-Unix platform, or a failed `getrlimit`).
pub const FALLBACK_MAX_CONNECTIONS: usize = 1_024;

/// Floor under the derived connection cap: a process whose soft limit is
/// pathologically small still serves a usable number of connections.
pub const MIN_MAX_CONNECTIONS: usize = 16;

/// Ceiling over the derived connection cap: half of a very large soft limit
/// (container runtimes commonly grant ~1M) is not a meaningful pre-auth
/// bound, so the derivation stops here.
pub const MAX_MAX_CONNECTIONS: usize = 65_536;

/// How long live connections may drain after the shutdown signal before the
/// remainder are aborted: the 5 s of C9's drain order.
pub const SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);

/// The pre-auth listener limits [`run`] enforces, read once at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServeLimits {
	max_connections: NonZeroUsize,
	header_read_timeout: Duration,
	keep_alive_timeout: Duration,
	write_stall_timeout: Duration,
}

impl ServeLimits {
	/// Limits with an explicit connection cap and the default deadlines.
	#[must_use]
	pub const fn new(max_connections: NonZeroUsize) -> Self {
		Self { max_connections, header_read_timeout: DEFAULT_HEADER_READ_TIMEOUT, keep_alive_timeout: DEFAULT_KEEP_ALIVE_TIMEOUT, write_stall_timeout: DEFAULT_WRITE_STALL_TIMEOUT }
	}

	/// The defaults: the `RLIMIT_NOFILE`-derived connection cap (see
	/// [`default_max_connections`]) and the default deadlines.
	#[must_use]
	pub fn defaults() -> Self {
		Self::new(default_max_connections())
	}

	/// These limits with a different header deadline (tests, embedders).
	#[must_use]
	pub const fn with_header_read_timeout(mut self, timeout: Duration) -> Self {
		self.header_read_timeout = timeout;
		self
	}

	/// These limits with a different keep-alive deadline (tests, embedders).
	#[must_use]
	pub const fn with_keep_alive_timeout(mut self, timeout: Duration) -> Self {
		self.keep_alive_timeout = timeout;
		self
	}

	/// These limits with a different write-stall timeout (tests, embedders).
	#[must_use]
	pub const fn with_write_stall_timeout(mut self, timeout: Duration) -> Self {
		self.write_stall_timeout = timeout;
		self
	}

	/// The limits the four `WEFT_*` values describe (`None` for an unset
	/// variable, which leaves that limit at its default).
	///
	/// # Errors
	///
	/// A message naming the variable when a value is not a positive integer,
	/// so a malformed setting fails startup instead of silently falling back
	/// to a default the operator did not ask for.
	pub fn from_env_values(max_connections: Option<&str>, header_read_timeout_secs: Option<&str>, keep_alive_timeout_secs: Option<&str>, write_stall_timeout_secs: Option<&str>) -> Result<Self, String> {
		let mut limits = Self::defaults();
		if let Some(raw) = max_connections {
			limits.max_connections = raw.parse().map_err(|e| format!("{MAX_CONNECTIONS_ENV}={raw:?} is not a positive integer: {e}"))?;
		}
		if let Some(raw) = header_read_timeout_secs {
			limits.header_read_timeout = parse_timeout_secs(HEADER_READ_TIMEOUT_ENV, raw)?;
		}
		if let Some(raw) = keep_alive_timeout_secs {
			limits.keep_alive_timeout = parse_timeout_secs(KEEP_ALIVE_TIMEOUT_ENV, raw)?;
		}
		if let Some(raw) = write_stall_timeout_secs {
			limits.write_stall_timeout = parse_timeout_secs(WRITE_STALL_TIMEOUT_ENV, raw)?;
		}
		Ok(limits)
	}

	/// The limits the process environment describes; see
	/// [`from_env_values`](Self::from_env_values).
	///
	/// # Errors
	///
	/// A set variable whose value is not a positive integer.
	pub fn from_env() -> Result<Self, String> {
		Self::from_env_values(std::env::var(MAX_CONNECTIONS_ENV).ok().as_deref(), std::env::var(HEADER_READ_TIMEOUT_ENV).ok().as_deref(), std::env::var(KEEP_ALIVE_TIMEOUT_ENV).ok().as_deref(), std::env::var(WRITE_STALL_TIMEOUT_ENV).ok().as_deref())
	}

	/// The most connections served at once; a connection beyond this waits
	/// in the listen backlog, unread, until one in service closes.
	#[must_use]
	pub const fn max_connections(self) -> usize {
		self.max_connections.get()
	}

	/// How long a peer may take to deliver a complete request header block.
	#[must_use]
	pub const fn header_read_timeout(self) -> Duration {
		self.header_read_timeout
	}

	/// How long a connection may make no read progress before it is closed.
	#[must_use]
	pub const fn keep_alive_timeout(self) -> Duration {
		self.keep_alive_timeout
	}

	/// How long a connection's writes may make no progress before it is
	/// closed.
	#[must_use]
	pub const fn write_stall_timeout(self) -> Duration {
		self.write_stall_timeout
	}
}

/// Parse one `*_SECS` limit value: a positive whole number of seconds.
fn parse_timeout_secs(name: &str, raw: &str) -> Result<Duration, String> {
	let secs: u64 = raw.parse().map_err(|e| format!("{name}={raw:?} is not a positive integer number of seconds: {e}"))?;
	if secs == 0 {
		return Err(format!("{name}={raw:?} is not a positive integer number of seconds: zero would refuse every connection"));
	}
	Ok(Duration::from_secs(secs))
}

/// The default connection cap, derived from this process's `RLIMIT_NOFILE`
/// soft limit: [`default_max_connections_for`].
#[must_use]
pub fn default_max_connections() -> NonZeroUsize {
	default_max_connections_for(nofile_soft_limit())
}

/// The derivation behind [`default_max_connections`].
///
/// Split out so the rule is testable without touching the process's real
/// limit: half the soft limit (connection sockets share the descriptor
/// budget with the store's own files), clamped to
/// `MIN_MAX_CONNECTIONS..=MAX_MAX_CONNECTIONS`; [`FALLBACK_MAX_CONNECTIONS`]
/// when the limit is unknown.
#[must_use]
pub fn default_max_connections_for(soft_limit: Option<u64>) -> NonZeroUsize {
	let derived = soft_limit.map_or(FALLBACK_MAX_CONNECTIONS, |soft| {
		let half = usize::try_from(soft / 2).unwrap_or(usize::MAX);
		half.clamp(MIN_MAX_CONNECTIONS, MAX_MAX_CONNECTIONS)
	});
	NonZeroUsize::new(derived).unwrap_or(NonZeroUsize::MIN)
}

/// This process's `RLIMIT_NOFILE` soft limit, when it can be read.
#[cfg(unix)]
fn nofile_soft_limit() -> Option<u64> {
	let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
	// SAFETY: getrlimit only writes through the pointer, and `limit` is a
	// valid, owned rlimit for the duration of the call.
	(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } == 0).then_some(limit.rlim_cur)
}

/// This process's `RLIMIT_NOFILE` soft limit, when it can be read.
#[cfg(not(unix))]
const fn nofile_soft_limit() -> Option<u64> {
	None
}

/// The process's shutdown signal handlers, installed before serving (C9:
/// "handlers are installed first").
#[cfg(unix)]
pub struct ShutdownSignals {
	sigint: tokio::signal::unix::Signal,
	sigterm: tokio::signal::unix::Signal,
}

/// The process's shutdown signal handlers, installed before serving (C9:
/// "handlers are installed first").
#[cfg(not(unix))]
pub struct ShutdownSignals {
	ctrl_c: Pin<Box<dyn Future<Output = Result<(), io::Error>> + Send>>,
}

impl ShutdownSignals {
	/// Register the SIGINT and SIGTERM handlers *now*, replacing the default
	/// terminate-on-signal behaviour, and return the handle whose
	/// [`wait`](Self::wait) future resolves when either arrives. Because
	/// registration happens here — before startup opens the store or binds
	/// the listener — a signal delivered at any later point, including
	/// before [`run`] is entered, is recorded by the stream and seen by the
	/// first `wait` poll; it cannot kill the process mid-startup.
	///
	/// # Errors
	///
	/// The OS refused to register a handler.
	#[cfg(unix)]
	pub fn install() -> io::Result<Self> {
		use tokio::signal::unix::{signal, SignalKind};
		Ok(Self { sigint: signal(SignalKind::interrupt())?, sigterm: signal(SignalKind::terminate())? })
	}

	/// Register the Ctrl-C handler *now*; see the Unix [`install`](Self::install).
	///
	/// # Errors
	///
	/// The OS refused to register a handler.
	#[cfg(not(unix))]
	pub fn install() -> io::Result<Self> {
		Ok(Self { ctrl_c: Box::pin(tokio::signal::ctrl_c()) })
	}

	/// Resolve when the first shutdown signal arrives. Signals that arrived
	/// before this is first polled still resolve it: tokio's signal streams
	/// record deliveries made after registration.
	#[cfg(unix)]
	pub async fn wait(&mut self) {
		tokio::select! {
			_ = self.sigint.recv() => {},
			_ = self.sigterm.recv() => {},
		}
	}

	/// Resolve when the first shutdown signal arrives.
	#[cfg(not(unix))]
	pub async fn wait(&mut self) {
		let _ = (&mut self.ctrl_c).await;
	}
}

/// Serve `router` on `listener` under `limits` until `shutdown` resolves —
/// the one serve path (SEC-4).
///
/// The accept loop takes a connection-cap permit before each `accept`, so
/// at most `limits.max_connections()` connections exist at once and each is
/// counted before any byte is read from it (pre-auth). Each connection is
/// served as HTTP/1.1 only, with the header deadline on hyper's builder and
/// the keep-alive / write-stall deadlines on the connection's IO.
///
/// When `shutdown` resolves — including when it is already resolved on
/// entry, which is what a signal before serving looks like — the accept
/// loop stops, live connections drain for up to [`SHUTDOWN_DRAIN`], and
/// `Ok(())` is returned.
///
/// # Errors
///
/// The listener failed to accept (a per-connection failure is logged and
/// ends only that connection, never the server).
///
/// # Panics
///
/// Never in practice: the connection-cap semaphore is leaked into an
/// `Arc` for the whole call and is never closed, so acquiring a permit
/// from it cannot fail.
pub async fn run(listener: TcpListener, router: Router, limits: ServeLimits, shutdown: impl Future<Output = ()>) -> io::Result<()> {
	serve(listener, router, limits, shutdown, Transport::Plain).await
}

/// Serve `router` on `listener` exactly as [`run`] does, except that each
/// accepted connection first completes a TLS handshake with `tls`
/// (SEC-5: rustls with the ring provider; see [`crate::tls`]).
///
/// The handshake runs inside the connection task, after the cap permit is
/// taken, and is bounded by the keep-alive deadline: a peer that stalls
/// mid-handshake (or never speaks) is cut exactly like a peer that stalls
/// mid-request, and after the handshake the same keep-alive/write-stall
/// deadlines as [`run`] apply to the encrypted connection. A failed
/// handshake ends only that connection; it is logged like any other
/// connection error.
///
/// # Errors
///
/// As [`run`]: the listener failed to accept.
pub async fn run_tls(listener: TcpListener, router: Router, limits: ServeLimits, tls: &TlsConfig, shutdown: impl Future<Output = ()>) -> io::Result<()> {
	serve(listener, router, limits, shutdown, Transport::Tls(tls.acceptor())).await
}

/// How an accepted connection's bytes are read: as they arrive, or through
/// a TLS session first. The accept loop is shared, so the two listeners
/// can never drift apart in their limits or their shutdown behaviour.
#[derive(Clone)]
enum Transport {
	Plain,
	Tls(TlsAcceptor),
}

/// The accept loop behind [`run`] and [`run_tls`]; see [`run`] for the
/// contract.
async fn serve(listener: TcpListener, router: Router, limits: ServeLimits, shutdown: impl Future<Output = ()>, transport: Transport) -> io::Result<()> {
	tokio::pin!(shutdown);
	let permits = Arc::new(Semaphore::new(limits.max_connections()));
	let mut connections = JoinSet::new();
	loop {
		let permit = tokio::select! {
			biased;
			() = &mut shutdown => break,
			permit = permits.clone().acquire_owned() => permit.expect("the semaphore is never closed"),
		};
		let (stream, _peer) = tokio::select! {
			biased;
			() = &mut shutdown => break,
			accepted = listener.accept() => accepted?,
		};
		let service = TowerToHyperService::new(router.clone());
		let transport = transport.clone();
		connections.spawn(async move {
			// The permit moves into the connection task: the slot is held
			// for the connection's whole life and freed when it closes.
			let _permit = permit;
			let served = match transport {
				Transport::Plain => {
					let io = DeadlineIo::new(stream, limits.keep_alive_timeout(), limits.write_stall_timeout());
					http1::Builder::new().timer(TokioTimer::new()).header_read_timeout(limits.header_read_timeout()).serve_connection(io, service).await.map_err(io::Error::other)
				}
				Transport::Tls(acceptor) => match tokio::time::timeout(limits.keep_alive_timeout(), acceptor.accept(stream)).await {
					Ok(Ok(tls_stream)) => {
						let io = DeadlineIo::new(tls_stream, limits.keep_alive_timeout(), limits.write_stall_timeout());
						http1::Builder::new().timer(TokioTimer::new()).header_read_timeout(limits.header_read_timeout()).serve_connection(io, service).await.map_err(io::Error::other)
					}
					Ok(Err(err)) => Err(err),
					Err(_elapsed) => Err(io::Error::new(io::ErrorKind::TimedOut, "the TLS handshake made no progress within the keep-alive deadline")),
				},
			};
			if let Err(err) = served {
				tracing::debug!(error = %err, "connection closed with an error");
			}
		});
		// Reap finished connection tasks as we go, so a long-lived server
		// does not accumulate completed join handles.
		while let Some(joined) = connections.try_join_next() {
			if let Err(err) = joined {
				tracing::debug!(error = %err, "connection task failed");
			}
		}
	}
	// Drain (C9): stop accepting (the listener drops with this function),
	// give live connections the drain window, then abort what is left.
	if tokio::time::timeout(SHUTDOWN_DRAIN, async { while connections.join_next().await.is_some() {} }).await.is_err() {
		connections.abort_all();
		while connections.join_next().await.is_some() {}
	}
	Ok(())
}

/// A connection's IO with the two progress deadlines SEC-4 owns: no read
/// progress for `keep_alive_timeout` closes the connection, and no write
/// progress for `write_stall_timeout` closes it. Available input or output
/// capacity is always taken before a deadline is consulted, so a deadline
/// fires only on a genuinely stalled peer, never on one whose bytes were
/// already waiting.
struct DeadlineIo<T> {
	inner: TokioIo<T>,
	keep_alive_timeout: Duration,
	write_stall_timeout: Duration,
	read_deadline: Pin<Box<Sleep>>,
	write_deadline: Pin<Box<Sleep>>,
}

impl<T> DeadlineIo<T> {
	fn new(stream: T, keep_alive_timeout: Duration, write_stall_timeout: Duration) -> Self {
		Self { inner: TokioIo::new(stream), keep_alive_timeout, write_stall_timeout, read_deadline: Box::pin(tokio::time::sleep(keep_alive_timeout)), write_deadline: Box::pin(tokio::time::sleep(write_stall_timeout)) }
	}

	fn reset_read_deadline(&mut self) {
		self.read_deadline.as_mut().reset(Instant::now() + self.keep_alive_timeout);
	}

	fn reset_write_deadline(&mut self) {
		self.write_deadline.as_mut().reset(Instant::now() + self.write_stall_timeout);
	}
}

impl<T> hyper::rt::Read for DeadlineIo<T>
where
	T: tokio::io::AsyncRead + Unpin,
{
	fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: hyper::rt::ReadBufCursor<'_>) -> Poll<Result<(), io::Error>> {
		match Pin::new(&mut self.inner).poll_read(cx, buf) {
			Poll::Ready(Ok(())) => {
				self.reset_read_deadline();
				Poll::Ready(Ok(()))
			}
			Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
			Poll::Pending => {
				if self.read_deadline.as_mut().poll(cx).is_ready() {
					return Poll::Ready(Err(io::Error::new(io::ErrorKind::TimedOut, "connection made no read progress within the keep-alive deadline")));
				}
				Poll::Pending
			}
		}
	}
}

impl<T> hyper::rt::Write for DeadlineIo<T>
where
	T: tokio::io::AsyncWrite + Unpin,
{
	fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, io::Error>> {
		match Pin::new(&mut self.inner).poll_write(cx, buf) {
			Poll::Ready(Ok(written)) => {
				self.reset_write_deadline();
				Poll::Ready(Ok(written))
			}
			Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
			Poll::Pending => {
				if self.write_deadline.as_mut().poll(cx).is_ready() {
					return Poll::Ready(Err(io::Error::new(io::ErrorKind::TimedOut, "connection made no write progress within the write-stall timeout")));
				}
				Poll::Pending
			}
		}
	}

	fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
		match Pin::new(&mut self.inner).poll_flush(cx) {
			Poll::Ready(Ok(())) => {
				self.reset_write_deadline();
				Poll::Ready(Ok(()))
			}
			Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
			Poll::Pending => {
				if self.write_deadline.as_mut().poll(cx).is_ready() {
					return Poll::Ready(Err(io::Error::new(io::ErrorKind::TimedOut, "connection made no write progress within the write-stall timeout")));
				}
				Poll::Pending
			}
		}
	}

	fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
		Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use hyper::rt::{Read as _, Write as _};

	use super::*;

	#[test]
	fn the_derived_cap_is_half_the_soft_limit() {
		assert_eq!(default_max_connections_for(Some(1_024)).get(), 512);
		assert_eq!(default_max_connections_for(Some(8_192)).get(), 4_096);
	}

	#[test]
	fn the_derived_cap_is_floored_and_capped() {
		assert_eq!(default_max_connections_for(Some(8)).get(), MIN_MAX_CONNECTIONS);
		assert_eq!(default_max_connections_for(Some(0)).get(), MIN_MAX_CONNECTIONS);
		assert_eq!(default_max_connections_for(Some(1_048_576)).get(), MAX_MAX_CONNECTIONS);
		assert_eq!(default_max_connections_for(Some(u64::MAX)).get(), MAX_MAX_CONNECTIONS);
	}

	#[test]
	fn an_unknown_soft_limit_falls_back() {
		assert_eq!(default_max_connections_for(None).get(), FALLBACK_MAX_CONNECTIONS);
	}

	#[test]
	fn env_values_override_and_reject_garbage() {
		let limits = ServeLimits::from_env_values(Some("7"), Some("3"), Some("4"), Some("5")).expect("valid values");
		assert_eq!(limits.max_connections(), 7);
		assert_eq!(limits.header_read_timeout(), Duration::from_secs(3));
		assert_eq!(limits.keep_alive_timeout(), Duration::from_secs(4));
		assert_eq!(limits.write_stall_timeout(), Duration::from_secs(5));

		let unset = ServeLimits::from_env_values(None, None, None, None).expect("defaults");
		assert_eq!(unset.header_read_timeout(), DEFAULT_HEADER_READ_TIMEOUT);
		assert_eq!(unset.keep_alive_timeout(), DEFAULT_KEEP_ALIVE_TIMEOUT);
		assert_eq!(unset.write_stall_timeout(), DEFAULT_WRITE_STALL_TIMEOUT);

		for bad in [Some("0"), Some("-1"), Some("many"), Some("1.5")] {
			assert!(ServeLimits::from_env_values(bad, None, None, None).is_err(), "{bad:?} is refused");
			assert!(ServeLimits::from_env_values(None, bad, None, None).is_err(), "{bad:?} is refused");
		}
		assert!(ServeLimits::from_env_values(None, None, Some("0"), None).is_err(), "a zero keep-alive deadline is refused");
	}

	/// A write to a peer that never reads stalls once the buffer fills, and
	/// the write-stall deadline turns it into a `TimedOut` error instead of
	/// a connection pinned forever.
	#[tokio::test]
	async fn a_stalled_write_times_out() {
		let (stream, _peer) = tokio::io::duplex(64);
		let mut io = DeadlineIo::new(stream, Duration::from_secs(60), Duration::from_millis(50));
		let chunk = [0_u8; 1_024];
		let mut written = 0_usize;
		let stalled = tokio::time::timeout(
			Duration::from_secs(5),
			std::future::poll_fn(|cx| {
				// Write until the buffer fills; from then on every poll is
				// Pending until the write-stall deadline turns it into an error.
				while written < chunk.len() {
					match Pin::new(&mut io).poll_write(cx, &chunk[written..]) {
						Poll::Ready(Ok(0)) => return Poll::Ready(Ok(written)),
						Poll::Ready(Ok(n)) => written += n,
						Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
						Poll::Pending => return Poll::Pending,
					}
				}
				Poll::Ready(Ok(written))
			}),
		)
		.await
		.expect("the deadline fires well inside the test timeout");
		let err = stalled.expect_err("a peer that never reads cannot accept the write");
		assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
	}

	/// A read from a peer that never sends makes no progress, and the
	/// keep-alive deadline turns it into a `TimedOut` error.
	#[tokio::test]
	async fn an_idle_read_times_out() {
		let (stream, _peer) = tokio::io::duplex(64);
		let mut io = DeadlineIo::new(stream, Duration::from_millis(50), Duration::from_secs(60));
		let mut storage = [0_u8; 64];
		let idle = tokio::time::timeout(
			Duration::from_secs(5),
			std::future::poll_fn(|cx| {
				let mut buf = hyper::rt::ReadBuf::new(&mut storage);
				Pin::new(&mut io).poll_read(cx, buf.unfilled())
			}),
		)
		.await
		.expect("the deadline fires well inside the test timeout");
		let err = idle.expect_err("a peer that never sends cannot satisfy the read");
		assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
	}
}
