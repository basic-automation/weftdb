//! `weft-server` binary entry point: build the [`weft_server::app_with_state`]
//! router and serve.
//!
//! The bind address defaults to `127.0.0.1:8080` and can be overridden with the
//! `WEFT_SERVER_ADDR` environment variable (e.g. `0.0.0.0:9000`). Keeping the
//! wiring this thin means the router under test is exactly the router served.
//!
//! ## Optional segment store
//!
//! Set `WEFT_SEGMENT_STORE_ROOT` to a directory to open a [`SegmentStore`] rooted
//! there (creating the Storage v2 layout if absent). With it set, the stored-range
//! query endpoints (`/api/v1/storage/...`) go live; without it the server serves
//! only the stateless interpolation/downsample API and those endpoints answer
//! `503`. Readiness (`GET /ready`) reports which mode is active.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use weft_server::{app_with_state, spawn_backup_daemon, spawn_reconcile_daemon, AppState, BackupDaemonConfig, ReconcileDaemonConfig, SERVICE, VERSION};
use weftdb::SegmentStore;

/// Default bind address when `WEFT_SERVER_ADDR` is unset.
const DEFAULT_ADDR: &str = "127.0.0.1:8080";

/// Environment variable naming the segment-store root directory (optional).
const STORE_ROOT_ENV: &str = "WEFT_SEGMENT_STORE_ROOT";

/// Environment variable enabling the background reconcile daemon: its sweep
/// interval in seconds. Unset or `0` disables the daemon.
const RECONCILE_INTERVAL_ENV: &str = "WEFT_RECONCILE_INTERVAL_SECS";

/// Environment variable for the daemon's out-of-order backlog trigger threshold
/// (defaults to 1 — reconcile any aspect with at least one out-of-order segment).
const RECONCILE_THRESHOLD_ENV: &str = "WEFT_RECONCILE_THRESHOLD";

/// Environment variable selecting **hot/cold** sweep mode: when truthy
/// (`1`/`true`/`yes`/`on`, case-insensitive), the daemon reconciles every aspect's
/// cold segments on each tick and defers only the hot tail until the backlog reaches
/// the threshold. Unset or falsey keeps the all-or-nothing threshold sweep.
const RECONCILE_HOT_COLD_ENV: &str = "WEFT_RECONCILE_HOT_COLD";

/// Environment variable enabling the background **cross-segment overlap merge**: when
/// truthy (`1`/`true`/`yes`/`on`), each daemon tick also merges time-overlapping
/// segment groups (late data that re-entered an already-covered window). Independent
/// of the intra-segment mode.
const RECONCILE_OVERLAPS_ENV: &str = "WEFT_RECONCILE_OVERLAPS";

/// Environment variable naming the split-not-rewrite floor in **bytes** for the
/// daemon's overlap merge (roadmap Phase 4.6). When set alongside
/// `WEFT_RECONCILE_OVERLAPS`, an overlap component whose cold prefix clears this many
/// bytes and outweighs its hot suffix is split off rather than fully rewritten. When
/// unset, the merge uses the default 50 MiB floor.
const RECONCILE_SPLIT_MIN_BYTES_ENV: &str = "WEFT_RECONCILE_SPLIT_MIN_BYTES";

/// Environment variable naming the segment-count cap for the daemon's squash pass
/// (roadmap Phase 4.6). When set, each tick also squashes every aspect whose segment
/// count exceeds it into one segment, bounding split-path fragmentation. When unset,
/// no squash runs.
const RECONCILE_MAX_SPLITS_ENV: &str = "WEFT_RECONCILE_MAX_SPLITS";

/// Environment variable naming the **target segment size in rows** for the daemon's
/// size-aware compaction pass (roadmap Phase 4.6). When set, each tick also coalesces
/// every aspect's segments toward ~this many rows per segment (leaving already-large
/// segments untouched), holding fragmentation near the read-optimal size rather than
/// folding to one (`WEFT_RECONCILE_MAX_SPLITS`). When unset, no size-aware compaction runs.
const COMPACT_TARGET_ROWS_ENV: &str = "WEFT_COMPACT_TARGET_ROWS";

/// Environment variable enabling the background **control-plane backup** daemon
/// (roadmap Phase 7.4): its snapshot interval in seconds. Unset or `0` disables it.
/// Each tick writes a verified `VACUUM INTO` snapshot of the four control-plane DBs
/// into a fresh `backup-<unix_millis>` directory.
const BACKUP_INTERVAL_ENV: &str = "WEFT_BACKUP_INTERVAL_SECS";

/// Environment variable naming the directory backups are written under. Shared with
/// the manual `POST /api/v1/storage/backup` endpoint, so hand-taken and daemon-taken
/// snapshots live together. Unset → `<store_root>/backups`.
const BACKUP_DIR_ENV: &str = "WEFT_BACKUP_DIR";

/// Environment variable naming how many **daemon-generated** snapshots to retain: after
/// each successful backup the oldest `backup-<digits>` directories beyond this many are
/// removed. Unset → retain everything. An operator's own `?label=`-named snapshot is
/// never a prune candidate.
const BACKUP_KEEP_ENV: &str = "WEFT_BACKUP_KEEP";

/// Environment variable naming the OTLP collector endpoint. When set (e.g.
/// `http://localhost:4317`), `weft-server` exports its tracing spans to that collector
/// over OTLP/gRPC in addition to the `fmt` log subscriber (roadmap Phase 3). Unset
/// leaves tracing on the `fmt` path alone — no exporter, no network dependency.
const OTEL_ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	// The OTLP provider (when configured) is held for the process lifetime: dropping it
	// early would tear down the batch span processor and stop export. It is shut down on
	// a clean exit to flush any buffered spans.
	let otel_provider = init_tracing();
	let addr: SocketAddr = std::env::var("WEFT_SERVER_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string()).parse()?;

	let state = build_state().await?;
	spawn_reconcile_daemon_if_configured(&state)?;
	spawn_backup_daemon_if_configured(&state)?;

	let listener = tokio::net::TcpListener::bind(addr).await?;
	let local = listener.local_addr()?;
	println!("{SERVICE} v{VERSION} listening on http://{local}");

	let serve_result = axum::serve(listener, app_with_state(state)).await;
	if let Some(provider) = otel_provider {
		// Best-effort flush of buffered spans on shutdown.
		let _ = provider.shutdown();
	}
	serve_result?;
	Ok(())
}

/// Install the process-wide tracing subscriber (roadmap Phase 3): a `fmt` layer
/// filtered by `RUST_LOG` (defaulting to `info`) that logs **span close** events, so
/// each span prints its recorded fields and its busy/idle duration on completion — the
/// "where did the time go" signal Phase 3 targets. All the request/compute/storage/
/// ingest/reconcile spans nest under the per-request `request` root.
///
/// When `OTEL_EXPORTER_OTLP_ENDPOINT` is set, an **OTLP/gRPC exporter** is attached
/// beside the `fmt` layer (the same shipped spans are exported to a collector like
/// Jaeger/Tempo/an OTel Collector) and the returned [`SdkTracerProvider`] is handed to
/// the caller to hold for the process lifetime and shut down on exit. When it is unset
/// there is no exporter and no network dependency — tracing stays on the `fmt` path.
///
/// `try_init` is a no-op when a subscriber is already installed, so this never panics.
fn init_tracing() -> Option<opentelemetry_sdk::trace::SdkTracerProvider> {
	use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _, Layer as _};

	let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
	let fmt_layer = tracing_subscriber::fmt::layer().with_target(false).with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE);

	let (otel_layer, provider) = match build_otlp_provider() {
		Some(provider) => {
			use opentelemetry::trace::TracerProvider as _;
			let tracer = provider.tracer(SERVICE);
			(Some(tracing_opentelemetry::layer().with_tracer(tracer).boxed()), Some(provider))
		}
		None => (None, None),
	};

	let _ = tracing_subscriber::registry().with(filter).with(fmt_layer).with(otel_layer).try_init();
	if provider.is_some() {
		println!("otlp trace export: enabled (exporting spans to {})", std::env::var(OTEL_ENDPOINT_ENV).unwrap_or_default());
	}
	provider
}

/// Build an OTLP tracer provider when `OTEL_EXPORTER_OTLP_ENDPOINT` is set (roadmap
/// Phase 3): an OTLP/gRPC `SpanExporter` at that endpoint, fed by a batch span
/// processor, tagged with the `weft-server` service resource. Returns `None` when the
/// env var is unset (export disabled) or the exporter cannot be built (logged, then
/// tracing falls back to the `fmt`-only path — a misconfigured collector must not stop
/// the server from starting).
fn build_otlp_provider() -> Option<opentelemetry_sdk::trace::SdkTracerProvider> {
	use opentelemetry_otlp::WithExportConfig as _;

	let endpoint = std::env::var(OTEL_ENDPOINT_ENV).ok()?;
	let exporter = match opentelemetry_otlp::SpanExporter::builder().with_tonic().with_endpoint(&endpoint).build() {
		Ok(exporter) => exporter,
		Err(err) => {
			eprintln!("otlp trace export: disabled — could not build the OTLP exporter for {endpoint:?}: {err}");
			return None;
		}
	};
	let resource = opentelemetry_sdk::Resource::builder().with_service_name(SERVICE).build();
	Some(opentelemetry_sdk::trace::SdkTracerProvider::builder().with_batch_exporter(exporter).with_resource(resource).build())
}

/// Start the background reconcile daemon (roadmap Phase 4.6) when a store is
/// configured and `WEFT_RECONCILE_INTERVAL_SECS` names a positive interval.
///
/// A no-op when no store is attached (nothing to reconcile) or the interval is
/// unset/`0`/unparseable-as-positive. The daemon runs detached for the process
/// lifetime; its handle is deliberately dropped.
///
/// # Errors
///
/// Propagates a non-numeric `WEFT_RECONCILE_THRESHOLD` (a malformed operator config
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
	let split_min_bytes: Option<u64> = match std::env::var(RECONCILE_SPLIT_MIN_BYTES_ENV) {
		Ok(raw) => Some(raw.parse().map_err(|e| anyhow::anyhow!("{RECONCILE_SPLIT_MIN_BYTES_ENV}={raw:?} is not a non-negative integer: {e}"))?),
		Err(_) => None,
	};
	let squash_max_segments: Option<usize> = match std::env::var(RECONCILE_MAX_SPLITS_ENV) {
		Ok(raw) => Some(raw.parse().map_err(|e| anyhow::anyhow!("{RECONCILE_MAX_SPLITS_ENV}={raw:?} is not a non-negative integer: {e}"))?),
		Err(_) => None,
	};
	let compact_target_rows: Option<usize> = match std::env::var(COMPACT_TARGET_ROWS_ENV) {
		Ok(raw) => Some(raw.parse().map_err(|e| anyhow::anyhow!("{COMPACT_TARGET_ROWS_ENV}={raw:?} is not a non-negative integer: {e}"))?),
		Err(_) => None,
	};
	let config = ReconcileDaemonConfig { interval: Duration::from_secs(interval_secs), threshold, hot_cold, overlaps, split_min_bytes, squash_max_segments, compact_target_rows };
	// The daemon runs detached for the process lifetime; its handle is dropped on purpose.
	drop(spawn_reconcile_daemon(Arc::clone(store), state.metrics().clone(), config));
	let mode = if hot_cold { "hot/cold" } else { "threshold" };
	let overlap_note = match (overlaps, split_min_bytes) {
		(true, Some(min)) => format!(" + overlap merge (split floor {min} B)"),
		(true, None) => " + overlap merge".to_string(),
		(false, _) => String::new(),
	};
	let squash_note = squash_max_segments.map_or_else(String::new, |max| format!(" + squash (max {max} segment(s))"));
	let compact_note = compact_target_rows.map_or_else(String::new, |target| format!(" + compact (target {target} row(s)/segment)"));
	println!("reconcile daemon: enabled ({mode} mode{overlap_note}{squash_note}{compact_note}, every {interval_secs}s, threshold {threshold} out-of-order segment(s))");
	Ok(())
}

/// Start the background control-plane backup daemon (roadmap Phase 7.4) when a store is
/// configured and `WEFT_BACKUP_INTERVAL_SECS` names a positive interval.
///
/// A no-op when no store is attached (nothing to back up) or the interval is
/// unset/`0`/unparseable-as-positive. The daemon runs detached for the process lifetime;
/// its handle is deliberately dropped.
///
/// # Errors
///
/// Propagates a non-numeric `WEFT_BACKUP_INTERVAL_SECS`/`WEFT_BACKUP_KEEP` (a malformed
/// operator config should fail loudly at start rather than silently default).
fn spawn_backup_daemon_if_configured(state: &AppState) -> anyhow::Result<()> {
	let Some(store) = state.store() else { return Ok(()) };
	let interval_secs: u64 = match std::env::var(BACKUP_INTERVAL_ENV) {
		Ok(raw) => raw.parse().map_err(|e| anyhow::anyhow!("{BACKUP_INTERVAL_ENV}={raw:?} is not a non-negative integer: {e}"))?,
		Err(_) => 0,
	};
	if interval_secs == 0 {
		return Ok(());
	}
	let keep: Option<usize> = match std::env::var(BACKUP_KEEP_ENV) {
		Ok(raw) => Some(raw.parse().map_err(|e| anyhow::anyhow!("{BACKUP_KEEP_ENV}={raw:?} is not a non-negative integer: {e}"))?),
		Err(_) => None,
	};
	// The same base the manual endpoint resolves, so both paths write to one place.
	let base = std::env::var_os(BACKUP_DIR_ENV).map_or_else(|| store.root().join("backups"), std::path::PathBuf::from);
	let config = BackupDaemonConfig { interval: Duration::from_secs(interval_secs), base: base.clone(), keep };
	// The daemon runs detached for the process lifetime; its handle is dropped on purpose.
	drop(spawn_backup_daemon(Arc::clone(store), state.metrics().clone(), config));
	let keep_note = keep.map_or_else(|| " (retaining every snapshot)".to_string(), |n| format!(" (retaining the newest {n} generated snapshot(s))"));
	println!("backup daemon: enabled (every {interval_secs}s into {}{keep_note})", base.display());
	Ok(())
}

/// Build the router state, opening a segment store when `WEFT_SEGMENT_STORE_ROOT`
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
