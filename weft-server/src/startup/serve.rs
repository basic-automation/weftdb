//! Startup phase 5 — bind, banner, serve.
//!
//! The listener is bound before the request guard is chosen, because the guard
//! depends on the address actually bound (`127.0.0.1:0` asks for loopback and may
//! land on a different port). Keeping bind separate from serve is also what lets
//! a later phase — graceful shutdown, or a second listener — slot in between them.
//!
//! Serving itself is not done here: [`weft_server::serve::run`] is the one
//! serve path (SEC-4), carrying the pre-auth listener limits and the shutdown
//! drain. This module only adapts startup's state to it and shuts the OTLP
//! provider down on the way out.

use std::net::SocketAddr;

use weft_server::{AppState, ServeLimits, ShutdownSignals, TlsConfig, SERVICE, VERSION};

/// Bind the listener the server will serve on, and print the banner naming
/// the address it actually landed on (an `:0` request may resolve to any
/// port) and the scheme it will speak (`https` when `tls` is configured,
/// SEC-5).
///
/// # Errors
///
/// The address is in use or cannot be bound.
pub(crate) async fn bind(addr: SocketAddr, tls: bool) -> anyhow::Result<tokio::net::TcpListener> {
	let listener = tokio::net::TcpListener::bind(addr).await?;
	let local = listener.local_addr()?;
	let scheme = if tls { "https" } else { "http" };
	println!("{SERVICE} v{VERSION} listening on {scheme}://{local}");
	Ok(listener)
}

/// The address the bound listener is actually serving on.
pub(crate) fn local_addr(listener: &tokio::net::TcpListener) -> anyhow::Result<SocketAddr> {
	Ok(listener.local_addr()?)
}

/// Serve `state`'s router on `listener` under `limits` until a shutdown
/// signal arrives on `shutdown` (registered before every other startup
/// phase, so a signal sent before this runs is already recorded and serving
/// exits at once, cleanly — C9). With `tls` the connections are wrapped in
/// TLS on the same serve path ([`weft_server::serve::run_tls`], SEC-5).
///
/// The OTLP provider, when one was built, is shut down on the way out so any
/// buffered spans are flushed — best effort, because a collector that has gone
/// away must not turn a clean shutdown into a failed one.
///
/// # Errors
///
/// Whatever the serve path reports: a listener that stopped accepting, or an
/// io error on accept.
pub(crate) async fn run(listener: tokio::net::TcpListener, state: AppState, otel_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>, limits: ServeLimits, tls: Option<TlsConfig>, shutdown: &mut ShutdownSignals) -> anyhow::Result<()> {
	let router = weft_server::app_with_state(state);
	let serve_result = match &tls {
		Some(tls) => weft_server::serve::run_tls(listener, router, limits, tls, shutdown.wait()).await,
		None => weft_server::serve::run(listener, router, limits, shutdown.wait()).await,
	};
	if let Some(provider) = otel_provider {
		// Best-effort flush of buffered spans on shutdown.
		let _ = provider.shutdown();
	}
	Ok(serve_result?)
}

/// The probe-only admin listener (ROB-23r): a second listener that serves
/// `/health` and `/ready` and nothing else, so a supervisor's probes do not
/// share an address, a firewall rule or an origin with the data plane.
///
/// It exists only when `WEFT_ADMIN_ADDR` is set
/// ([`config::admin_addr`](super::config::admin_addr)); an unset variable means
/// no listener at all, which is how every deployment running today is set up.
pub(crate) struct AdminListener {
	/// The serve task, so shutdown waits for its drain rather than leaving a
	/// listener accepting probes after the store has closed.
	task: tokio::task::JoinHandle<std::io::Result<()>>,
	/// Flipped to `true` to ask the serve path to drain.
	stop: tokio::sync::watch::Sender<bool>,
	/// The address it actually bound, for the line printed when it stops.
	addr: SocketAddr,
}

impl AdminListener {
	/// Bind `addr` and serve the probe-only router on it under the same
	/// pre-auth listener limits as the data plane — the same
	/// [`weft_server::serve::run`] (SEC-4), so the connection cap, the
	/// header-read timeout and the drain are enforced here too rather than
	/// being quietly absent on the port nobody watches.
	///
	/// No host guard, no auth: the loopback bind is the protection, and
	/// [`weft_server::probe_app_with_state`] serves only the probe routes.
	/// An HTTP/1.0 probe with no `Host` header passes, which is the case that
	/// gets a `403` through the data listener.
	///
	/// # Errors
	///
	/// The address is in use or cannot be bound.
	pub(crate) async fn bind(state: AppState, addr: SocketAddr, limits: ServeLimits) -> anyhow::Result<Self> {
		let listener = tokio::net::TcpListener::bind(addr).await?;
		let local = listener.local_addr()?;
		let (stop, running) = tokio::sync::watch::channel(false);
		let task = tokio::spawn(async move { weft_server::serve::run(listener, weft_server::probe_app_with_state(state), limits, wait_for_stop(running)).await });
		println!("{SERVICE} v{VERSION} admin listener on http://{local} (probes only: /health, /ready, /healthz, /readyz)");
		Ok(Self { task, stop, addr: local })
	}

	/// Ask the listener to drain and wait for it, so the store is never closed
	/// under a probe still in flight.
	///
	/// A serve path that stopped on its own is reported rather than swallowed:
	/// an operator whose probe port died would otherwise keep receiving
	/// "healthy" from a server nobody can probe.
	///
	/// # Errors
	///
	/// The serve task panicked, or its accept loop ended on an io error.
	pub(crate) async fn shutdown(self) -> anyhow::Result<()> {
		let Self { task, stop, addr } = self;
		let _ = stop.send(true);
		match task.await {
			Ok(result) => {
				result?;
				println!("admin listener on http://{addr} stopped");
				Ok(())
			}
			Err(join) => Err(anyhow::anyhow!("the admin listener's serve task died: {join}")),
		}
	}
}

/// Resolve when [`AdminListener::shutdown`] asks the serve path to drain — or
/// when the sender goes away with it, which means startup is unwinding and the
/// listener should stop with everything else.
async fn wait_for_stop(mut running: tokio::sync::watch::Receiver<bool>) {
	while !*running.borrow_and_update() {
		if running.changed().await.is_err() {
			return;
		}
	}
}
