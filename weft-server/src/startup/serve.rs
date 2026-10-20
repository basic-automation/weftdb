//! Startup phase 5 — bind, banner, serve.
//!
//! The listener is bound before the request guard is chosen, because the guard
//! depends on the address actually bound (`127.0.0.1:0` asks for loopback and may
//! land on a different port). Keeping bind separate from serve is also what lets
//! a later phase — graceful shutdown, or a second listener — slot in between them.

use std::net::SocketAddr;

use weft_server::{app_with_state, AppState, SERVICE, VERSION};

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

/// Serve `state`'s router on `listener` until the server stops.
///
/// The OTLP provider, when one was built, is shut down on the way out so any
/// buffered spans are flushed — best effort, because a collector that has gone
/// away must not turn a clean shutdown into a failed one.
///
/// # Errors
///
/// Whatever `axum::serve` reports: a listener that stopped accepting, an io error
/// on the accepted socket, or a server that failed to bind its next accept.
pub(crate) async fn run(listener: tokio::net::TcpListener, state: AppState, otel_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>) -> anyhow::Result<()> {
	let serve_result = axum::serve(listener, app_with_state(state)).await;
	if let Some(provider) = otel_provider {
		// Best-effort flush of buffered spans on shutdown.
		let _ = provider.shutdown();
	}
	Ok(serve_result?)
}
