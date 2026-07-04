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

/// Environment variable selecting **hot/cold** sweep mode: when truthy
/// (`1`/`true`/`yes`/`on`, case-insensitive), the daemon reconciles every aspect's
/// cold segments on each tick and defers only the hot tail until the backlog reaches
/// the threshold. Unset or falsey keeps the all-or-nothing threshold sweep.
const RECONCILE_HOT_COLD_ENV: &str = "DSP_RECONCILE_HOT_COLD";

/// Environment variable enabling the background **cross-segment overlap merge**: when
/// truthy (`1`/`true`/`yes`/`on`), each daemon tick also merges time-overlapping
/// segment groups (late data that re-entered an already-covered window). Independent
/// of the intra-segment mode.
const RECONCILE_OVERLAPS_ENV: &str = "DSP_RECONCILE_OVERLAPS";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	init_tracing();
	let addr: SocketAddr = std::env::var("DSP_SERVER_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string()).parse()?;

	let state = build_state().await?;
	spawn_reconcile_daemon_if_configured(&state)?;

	let listener = tokio::net::TcpListener::bind(addr).await?;
	let local = listener.local_addr()?;
	println!("{SERVICE} v{VERSION} listening on http://{local}");

	axum::serve(listener, app_with_state(state)).await?;
	Ok(())
}

/// Install the process-wide tracing subscriber (roadmap Phase 3): a `fmt` layer
/// filtered by `RUST_LOG` (defaulting to `info`) that logs **span close** events, so
/// each compute-path span (`interpolate.engine`, `downsample.reduce`) prints its
/// recorded fields and its busy/idle duration on completion — the "where did the time
/// go" signal Phase 3 targets. `try_init` is a no-op when a subscriber is already
/// installed, so this never panics.
fn init_tracing() {
	let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
	let _ = tracing_subscriber::fmt().with_env_filter(filter).with_target(false).with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE).try_init();
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
	let truthy = |var: &str| std::env::var(var).map(|raw| matches!(raw.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")).unwrap_or(false);
	let hot_cold = truthy(RECONCILE_HOT_COLD_ENV);
	let overlaps = truthy(RECONCILE_OVERLAPS_ENV);
	let config = ReconcileDaemonConfig { interval: Duration::from_secs(interval_secs), threshold, hot_cold, overlaps };
	// The daemon runs detached for the process lifetime; its handle is dropped on purpose.
	drop(spawn_reconcile_daemon(Arc::clone(store), state.metrics().clone(), config));
	let mode = if hot_cold { "hot/cold" } else { "threshold" };
	let overlap_note = if overlaps { " + overlap merge" } else { "" };
	println!("reconcile daemon: enabled ({mode} mode{overlap_note}, every {interval_secs}s, threshold {threshold} out-of-order segment(s))");
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
