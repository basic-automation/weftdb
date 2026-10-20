//! Startup phase 4 — the background tasks the operator asked for: the reconcile
//! daemon, the backup daemon, and the one-shot GPU calibration.
//!
//! None of them is required for the server to serve, and none of them is joined:
//! each runs detached for the process lifetime, its handle dropped on purpose.
//! What they share is the store handle and the metrics handle already in the
//! router state, so a tick and a request observe the same counters.
//!
//! A malformed setting in this phase is the one exception to "background tasks
//! never fail startup": an interval or threshold that is not a number stops the
//! server, because the operator meant to enable a daemon and did not say how.

use std::{sync::Arc, time::Duration};

use weft_server::{
	gpu::{self, CalibrationMode, GPU_CALIBRATE_ENV}, spawn_backup_daemon, spawn_reconcile_daemon, AppState, BackupDaemonConfig, ReconcileDaemonConfig
};

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
pub(crate) fn spawn_reconcile_if_configured(state: &AppState) -> anyhow::Result<()> {
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
pub(crate) fn spawn_backup_if_configured(state: &AppState) -> anyhow::Result<()> {
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

/// Start the GPU calibration ([`gpu::calibrate`]) in the background on tokio's blocking
/// pool unless `WEFT_GPU_CALIBRATE` opts out; it prints what it found when it finishes.
/// Returns at once: the server serves while it runs, interpolating on the CPU until the
/// measured thresholds are in force. Never fails: a missing GPU, a software adapter, a
/// failed calibration, or even a panicking one only leaves splimes' defaults in force.
pub(crate) fn spawn_gpu_calibration_if_enabled() {
	let mode = CalibrationMode::from_env_value(std::env::var(GPU_CALIBRATE_ENV).ok().as_deref());
	if mode == CalibrationMode::Off {
		println!("gpu calibration: skipped ({GPU_CALIBRATE_ENV} is off); interpolation stays on the CPU with the default thresholds");
		return;
	}
	println!("gpu calibration: timing the interpolation backends in the background (several seconds, on the CPU until it finishes; set {GPU_CALIBRATE_ENV}=0 to skip)");
	// Detached for the process lifetime, like the daemons; its handle is dropped on purpose.
	drop(gpu::spawn_in_background(move || gpu::calibrate(mode)));
}
