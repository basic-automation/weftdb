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

use weft_server::{AppState, ServeLimits, ShutdownSignals, SERVICE, VERSION};

/// Bind the listener the server will serve on, and print the banner naming the
/// address it actually landed on (an `:0` request may resolve to any port).
///
/// # Errors
///
/// The address is in use or cannot be bound.
pub(crate) async fn bind(addr: SocketAddr) -> anyhow::Result<tokio::net::TcpListener> {
	let listener = tokio::net::TcpListener::bind(addr).await?;
	let local = listener.local_addr()?;
	println!("{SERVICE} v{VERSION} listening on http://{local}");
	Ok(listener)
}

/// The address the bound listener is actually serving on.
pub(crate) fn local_addr(listener: &tokio::net::TcpListener) -> anyhow::Result<SocketAddr> {
	Ok(listener.local_addr()?)
}

/// Serve `state`'s router on `listener` under `limits` until a shutdown
/// signal arrives on `shutdown` (registered before every other startup
/// phase, so a signal sent before this runs is already recorded and serving
/// exits at once, cleanly — C9).
///
/// The OTLP provider, when one was built, is shut down on the way out so any
/// buffered spans are flushed — best effort, because a collector that has gone
/// away must not turn a clean shutdown into a failed one.
///
/// # Errors
///
/// Whatever the serve path reports: a listener that stopped accepting, or an
/// io error on accept.
pub(crate) async fn run(listener: tokio::net::TcpListener, state: AppState, otel_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>, limits: ServeLimits, shutdown: &mut ShutdownSignals) -> anyhow::Result<()> {
	let serve_result = weft_server::serve::run(listener, weft_server::app_with_state(state), limits, shutdown.wait()).await;
	if let Some(provider) = otel_provider {
		// Best-effort flush of buffered spans on shutdown.
		let _ = provider.shutdown();
	}
	Ok(serve_result?)
}
