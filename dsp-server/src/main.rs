//! `dsp-server` binary entry point: bind the [`dsp_server::app`] router and serve.
//!
//! The bind address defaults to `127.0.0.1:8080` and can be overridden with the
//! `DSP_SERVER_ADDR` environment variable (e.g. `0.0.0.0:9000`). Keeping the
//! wiring this thin means the router under test is exactly the router served.

use std::net::SocketAddr;

use dsp_server::{app, SERVICE, VERSION};

/// Default bind address when `DSP_SERVER_ADDR` is unset.
const DEFAULT_ADDR: &str = "127.0.0.1:8080";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let addr: SocketAddr = std::env::var("DSP_SERVER_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string()).parse()?;

	let listener = tokio::net::TcpListener::bind(addr).await?;
	let local = listener.local_addr()?;
	println!("{SERVICE} v{VERSION} listening on http://{local}");

	axum::serve(listener, app()).await?;
	Ok(())
}
