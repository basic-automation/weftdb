//! `dsp-server` binary entry point: build the [`dsp_server::app_with_state`]
//! router and serve.
//!
//! The bind address defaults to `127.0.0.1:8080` and can be overridden with the
//! `DSP_SERVER_ADDR` environment variable (e.g. `0.0.0.0:9000`). Keeping the
//! wiring this thin means the router under test is exactly the router served.
//!
//! ## Optional segment store
//!
//! Set `DSP_SEGMENT_STORE_ROOT` to a directory to open a [`SegmentStore`] rooted
//! there (creating the Storage v2 layout if absent). With it set, the stored-range
//! query endpoints (`/api/v1/storage/...`) go live; without it the server serves
//! only the stateless interpolation/downsample API and those endpoints answer
//! `503`. Readiness (`GET /ready`) reports which mode is active.

use std::{net::SocketAddr, sync::Arc};

use database::SegmentStore;
use dsp_server::{app_with_state, AppState, SERVICE, VERSION};

/// Default bind address when `DSP_SERVER_ADDR` is unset.
const DEFAULT_ADDR: &str = "127.0.0.1:8080";

/// Environment variable naming the segment-store root directory (optional).
const STORE_ROOT_ENV: &str = "DSP_SEGMENT_STORE_ROOT";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let addr: SocketAddr = std::env::var("DSP_SERVER_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string()).parse()?;

	let state = build_state().await?;

	let listener = tokio::net::TcpListener::bind(addr).await?;
	let local = listener.local_addr()?;
	println!("{SERVICE} v{VERSION} listening on http://{local}");

	axum::serve(listener, app_with_state(state)).await?;
	Ok(())
}

/// Build the router state, opening a segment store when `DSP_SEGMENT_STORE_ROOT`
/// is set and logging which mode the server runs in.
///
/// # Errors
///
/// Propagates a failure to open the segment store at the configured root.
async fn build_state() -> anyhow::Result<AppState> {
	match std::env::var(STORE_ROOT_ENV) {
		Ok(root) => {
			let store = SegmentStore::open(&root).await?;
			println!("segment store: opened at {root} (storage endpoints live)");
			Ok(AppState::new().with_store(Arc::new(store)))
		}
		Err(_) => {
			println!("segment store: not configured (set {STORE_ROOT_ENV} to enable the /api/v1/storage endpoints)");
			Ok(AppState::new())
		}
	}
}
