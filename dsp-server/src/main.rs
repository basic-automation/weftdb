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

use std::{net::SocketAddr, sync::Arc, time::Duration};

use database::SegmentStore;
use dsp_server::{app_with_state, spawn_reconcile_daemon, AppState, ReconcileDaemonConfig, SERVICE, VERSION};

/// Default bind address when `DSP_SERVER_ADDR` is unset.
const DEFAULT_ADDR: &str = "127.0.0.1:8080";

/// Environment variable naming the segment-store root directory (optional).
const STORE_ROOT_ENV: &str = "DSP_SEGMENT_STORE_ROOT";

/// Environment variable enabling the background reconcile daemon: its sweep
/// interval in seconds. Unset or `0` disables the daemon.
const RECONCILE_INTERVAL_ENV: &str = "DSP_RECONCILE_INTERVAL_SECS";

/// Environment variable for the daemon's out-of-order backlog trigger threshold
/// (defaults to 1 — reconcile any aspect with at least one out-of-order segment).
const RECONCILE_THRESHOLD_ENV: &str = "DSP_RECONCILE_THRESHOLD";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let addr: SocketAddr = std::env::var("DSP_SERVER_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string()).parse()?;

	let state = build_state().await?;
	spawn_reconcile_daemon_if_configured(&state)?;

	let listener = tokio::net::TcpListener::bind(addr).await?;
	let local = listener.local_addr()?;
	println!("{SERVICE} v{VERSION} listening on http://{local}");

	axum::serve(listener, app_with_state(state)).await?;
	Ok(())
}

/// Start the background reconcile daemon (roadmap Phase 4.6) when a store is
/// configured and `DSP_RECONCILE_INTERVAL_SECS` names a positive interval.
///
/// A no-op when no store is attached (nothing to reconcile) or the interval is
/// unset/`0`/unparseable-as-positive. The daemon runs detached for the process
/// lifetime; its handle is deliberately dropped.
///
/// # Errors
///
/// Propagates a non-numeric `DSP_RECONCILE_THRESHOLD` (a malformed operator config
/// should fail loudly at start rather than silently default).
fn spawn_reconcile_daemon_if_configured(state: &AppState) -> anyhow::Result<()> {
	let Some(store) = state.store() else { return Ok(()) };
	let interval_secs: u64 = match std::env::var(RECONCILE_INTERVAL_ENV) {
		Ok(raw) => raw.parse().map_err(|e| anyhow::anyhow!("{RECONCILE_INTERVAL_ENV}={raw:?} is not a non-negative integer: {e}"))?,
		Err(_) => 0,
	};
	if interval_secs == 0 {
		return Ok(());
	}
	let threshold: usize = match std::env::var(RECONCILE_THRESHOLD_ENV) {
		Ok(raw) => raw.parse().map_err(|e| anyhow::anyhow!("{RECONCILE_THRESHOLD_ENV}={raw:?} is not a non-negative integer: {e}"))?,
		Err(_) => 1,
	};
	let config = ReconcileDaemonConfig { interval: Duration::from_secs(interval_secs), threshold };
	// The daemon runs detached for the process lifetime; its handle is dropped on purpose.
	drop(spawn_reconcile_daemon(Arc::clone(store), state.metrics().clone(), config));
	println!("reconcile daemon: enabled (every {interval_secs}s, threshold {threshold} out-of-order segment(s))");
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
