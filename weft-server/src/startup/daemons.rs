//! Startup phase 4 — the background tasks: the maintenance scheduler (the
//! reconcile daemon, on by default since ROB-19), the backup daemon, and the
//! one-shot GPU calibration.
//!
//! None of them is required for the server to serve. Since ROB-3 they are
//! not spawned detached with their handles dropped: each is registered in
//! the [`Daemons`] registry the startup sequence owns, which supervises
//! them — a daemon that panics is restarted and the restart is recorded —
//! and stops them all on the registry's shared shutdown watch when serving
//! ends. What they share besides the registry is the store handle and the
//! metrics handle already in the router state, so a tick and a request
//! observe the same counters.
//!
//! A malformed setting in this phase is the one exception to "background tasks
//! never fail startup": an interval or threshold that is not a number stops the
//! server, because the operator meant to enable a daemon and did not say how.

use std::{sync::Arc, time::Duration};

use weft_server::{
	gpu::{self, CalibrationMode, GPU_CALIBRATE_ENV}, spawn_backup_daemon, spawn_reconcile_daemon, AppState, BackupDaemonConfig, Daemons, ReconcileDaemonConfig
};

/// Await one spawned daemon task, re-raising its panic in the calling
/// (supervised) task: the registry's supervision restarts a daemon on a
/// panic, and a `JoinError` swallowed here would look like a clean finish
/// and defeat it. A cancellation (the registry's shutdown abort) is a
/// clean stop.
async fn await_supervised(handle: tokio::task::JoinHandle<()>) {
	if let Err(err) = handle.await {
		if err.is_panic() {
			std::panic::resume_unwind(err.into_panic());
		}
	}
}

/// Environment variable setting the background maintenance scheduler's sweep
/// interval in seconds (ROB-19: the scheduler is decision 17's daemon, on by
/// default). Unset → [`DEFAULT_MAINTENANCE_INTERVAL_SECS`]; `0` disables the
/// scheduler.
const RECONCILE_INTERVAL_ENV: &str = "WEFT_RECONCILE_INTERVAL_SECS";

/// How often the maintenance scheduler sweeps when
/// `WEFT_RECONCILE_INTERVAL_SECS` is unset (ROB-19): the maintenance and
/// compaction daemon is **on by default** once D-S9 has landed (release plan
/// decision 17), so an operator who configures nothing still gets scheduled
/// reconciliation and compaction; `WEFT_RECONCILE_INTERVAL_SECS=0` is the
/// opt-out.
pub(crate) const DEFAULT_MAINTENANCE_INTERVAL_SECS: u64 = 60;

/// The compaction target, in rows per segment, the maintenance scheduler
/// coalesces toward when `WEFT_COMPACT_TARGET_ROWS` is unset (ROB-19 —
/// compaction on by default). Between the checkpoint floor (8,192 rows) and
/// the ~200k-row single segment the `downsample_range` knee measured reading
/// slowly: large enough that ordinary ingest batches are not rewritten every
/// tick (the fragmentation gate skips aspects already at their ideal count),
/// small enough to keep several segments per aspect for cross-segment reads.
pub(crate) const DEFAULT_COMPACT_TARGET_ROWS: usize = 65_536;

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

/// Environment variable naming the **target segment size in rows** for the scheduler's
/// size-aware compaction pass (roadmap Phase 4.6). Each tick coalesces every aspect's
/// segments toward ~this many rows per segment (leaving already-large segments
/// untouched), holding fragmentation near the read-optimal size rather than folding
/// to one (`WEFT_RECONCILE_MAX_SPLITS`). When unset, the pass runs at
/// [`DEFAULT_COMPACT_TARGET_ROWS`] (ROB-19: compaction is on by default).
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

/// Resolve the maintenance scheduler's configuration from the environment
/// (read through `get`, so tests can drive it without touching the process
/// environment). `Ok(None)` is the one opt-out: `WEFT_RECONCILE_INTERVAL_SECS=0`.
///
/// ROB-19 defaults: an unset interval resolves to
/// [`DEFAULT_MAINTENANCE_INTERVAL_SECS`] and an unset compaction target to
/// [`DEFAULT_COMPACT_TARGET_ROWS`] — the scheduler and its compaction pass run
/// unless the operator turns them off.
///
/// # Errors
///
/// A non-numeric value in any numeric variable (a malformed operator config
/// should fail loudly at start rather than silently default).
fn resolve_maintenance_config(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Option<ReconcileDaemonConfig>> {
	let numeric = |var: &str| -> anyhow::Result<Option<u64>> {
		match get(var) {
			Some(raw) => raw.parse::<u64>().map(Some).map_err(|e| anyhow::anyhow!("{var}={raw:?} is not a non-negative integer: {e}")),
			None => Ok(None),
		}
	};
	let interval_secs = numeric(RECONCILE_INTERVAL_ENV)?.unwrap_or(DEFAULT_MAINTENANCE_INTERVAL_SECS);
	if interval_secs == 0 {
		return Ok(None);
	}
	let threshold = usize::try_from(numeric(RECONCILE_THRESHOLD_ENV)?.unwrap_or(1)).unwrap_or(usize::MAX);
	let truthy = |var: &str| get(var).map(|raw| matches!(raw.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")).unwrap_or(false);
	let hot_cold = truthy(RECONCILE_HOT_COLD_ENV);
	let overlaps = truthy(RECONCILE_OVERLAPS_ENV);
	let split_min_bytes = numeric(RECONCILE_SPLIT_MIN_BYTES_ENV)?;
	let squash_max_segments = numeric(RECONCILE_MAX_SPLITS_ENV)?.map(|raw| usize::try_from(raw).unwrap_or(usize::MAX));
	let compact_target_rows = Some(match numeric(COMPACT_TARGET_ROWS_ENV)? {
		Some(raw) => usize::try_from(raw).unwrap_or(usize::MAX),
		None => DEFAULT_COMPACT_TARGET_ROWS,
	});
	Ok(Some(ReconcileDaemonConfig { interval: Duration::from_secs(interval_secs), threshold, hot_cold, overlaps, split_min_bytes, squash_max_segments, compact_target_rows }))
}

/// Start the background maintenance scheduler (the reconcile daemon, roadmap
/// Phase 4.6) when a store is configured — **by default** (ROB-19, release plan
/// decision 17): with nothing configured it sweeps every
/// [`DEFAULT_MAINTENANCE_INTERVAL_SECS`] seconds and compacts toward
/// [`DEFAULT_COMPACT_TARGET_ROWS`] rows per segment.
/// `WEFT_RECONCILE_INTERVAL_SECS=0` opts out.
///
/// A no-op when no store is attached (nothing to maintain) or the interval is
/// `0`. The daemon is registered in `daemons` (ROB-3): supervised, restarted
/// after a panic, and stopped by the registry's shutdown watch.
///
/// # Errors
///
/// Propagates a non-numeric numeric setting (a malformed operator config
/// should fail loudly at start rather than silently default).
pub(crate) fn spawn_reconcile_if_configured(state: &AppState, daemons: &Daemons) -> anyhow::Result<()> {
	let Some(store) = state.store() else { return Ok(()) };
	// Owned, so the registry's factory can hold it for the process lifetime.
	let store = Arc::clone(store);
	let Some(config) = resolve_maintenance_config(|var| std::env::var(var).ok())? else { return Ok(()) };
	let interval_secs = config.interval.as_secs();
	let threshold = config.threshold;
	let hot_cold = config.hot_cold;
	let overlaps = config.overlaps;
	let split_min_bytes = config.split_min_bytes;
	let squash_max_segments = config.squash_max_segments;
	let compact_target_rows = config.compact_target_rows;
	// Registered with the Daemons registry (ROB-3): the factory re-spawns
	// the daemon after a panic, under the registry's supervision.
	let metrics = state.metrics().clone();
	drop(daemons.spawn("reconcile", move || {
		let handle = spawn_reconcile_daemon(Arc::clone(&store), metrics.clone(), config);
		await_supervised(handle)
	}));
	let mode = if hot_cold { "hot/cold" } else { "threshold" };
	let overlap_note = match (overlaps, split_min_bytes) {
		(true, Some(min)) => format!(" + overlap merge (split floor {min} B)"),
		(true, None) => " + overlap merge".to_string(),
		(false, _) => String::new(),
	};
	let squash_note = squash_max_segments.map_or_else(String::new, |max| format!(" + squash (max {max} segment(s))"));
	let compact_note = compact_target_rows.map_or_else(String::new, |target| format!(" + compact (target {target} row(s)/segment)"));
	println!("maintenance scheduler: enabled ({mode} mode{overlap_note}{squash_note}{compact_note}, every {interval_secs}s, threshold {threshold} out-of-order segment(s))");
	Ok(())
}

/// Start the background control-plane backup daemon (roadmap Phase 7.4) when a store is
/// configured and `WEFT_BACKUP_INTERVAL_SECS` names a positive interval.
///
/// A no-op when no store is attached (nothing to back up) or the interval is
/// unset/`0`/unparseable-as-positive. The daemon is registered in
/// `daemons` (ROB-3): supervised, restarted after a panic, and stopped by
/// the registry's shutdown watch.
///
/// # Errors
///
/// Propagates a non-numeric `WEFT_BACKUP_INTERVAL_SECS`/`WEFT_BACKUP_KEEP` (a malformed
/// operator config should fail loudly at start rather than silently default).
pub(crate) fn spawn_backup_if_configured(state: &AppState, daemons: &Daemons) -> anyhow::Result<()> {
	let Some(store) = state.store() else { return Ok(()) };
	// Owned, so the registry's factory can hold it for the process lifetime.
	let store = Arc::clone(store);
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
	// Registered with the Daemons registry (ROB-3), as the reconcile daemon.
	let metrics = state.metrics().clone();
	drop(daemons.spawn("backup", move || {
		let handle = spawn_backup_daemon(Arc::clone(&store), metrics.clone(), config.clone());
		await_supervised(handle)
	}));
	let keep_note = keep.map_or_else(|| " (retaining every snapshot)".to_string(), |n| format!(" (retaining the newest {n} generated snapshot(s))"));
	println!("backup daemon: enabled (every {interval_secs}s into {}{keep_note})", base.display());
	Ok(())
}

/// Start the GPU calibration ([`gpu::calibrate`]) in the background on tokio's blocking
/// pool unless `WEFT_GPU_CALIBRATE` opts out; it prints what it found when it finishes.
/// Returns at once: the server serves while it runs, interpolating on the CPU until the
/// measured thresholds are in force. Never fails: a missing GPU, a software adapter, a
/// failed calibration, or even a panicking one only leaves splimes' defaults in force.
pub(crate) fn spawn_gpu_calibration_if_enabled(daemons: &Daemons) {
	let mode = CalibrationMode::from_env_value(std::env::var(GPU_CALIBRATE_ENV).ok().as_deref());
	if mode == CalibrationMode::Off {
		println!("gpu calibration: skipped ({GPU_CALIBRATE_ENV} is off); interpolation stays on the CPU with the default thresholds");
		return;
	}
	println!("gpu calibration: timing the interpolation backends in the background (several seconds, on the CPU until it finishes; set {GPU_CALIBRATE_ENV}=0 to skip)");
	// A one-shot registration (ROB-3): tracked by the registry and stopped
	// by its shutdown, but never restarted — calibration is a single
	// measurement, and `spawn_in_background` already reports its own
	// failure without stopping the server.
	drop(daemons.spawn_once("gpu-calibration", async move {
		let _ = gpu::spawn_in_background(move || gpu::calibrate(mode)).await;
	}));
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use super::*;

	fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
		let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (String::from(*k), String::from(*v))).collect();
		move |var| map.get(var).cloned()
	}

	/// ROB-19: with nothing configured, the scheduler resolves to ON — the
	/// default interval, and a compaction pass at the default target rows.
	#[test]
	fn maintenance_scheduler_is_on_by_default_with_compaction() {
		let config = resolve_maintenance_config(env(&[])).expect("resolves").expect("enabled by default");
		assert_eq!(config.interval, Duration::from_secs(DEFAULT_MAINTENANCE_INTERVAL_SECS));
		assert_eq!(config.compact_target_rows, Some(DEFAULT_COMPACT_TARGET_ROWS), "compaction runs by default");
		assert_eq!(config.threshold, 1);
		assert!(!config.hot_cold && !config.overlaps);
		assert_eq!(config.split_min_bytes, None);
		assert_eq!(config.squash_max_segments, None);
	}

	/// ROB-19: `WEFT_RECONCILE_INTERVAL_SECS=0` is the opt-out, and an explicit
	/// interval and compaction target still win over the defaults.
	#[test]
	fn maintenance_scheduler_opt_out_and_overrides() {
		assert!(resolve_maintenance_config(env(&[(RECONCILE_INTERVAL_ENV, "0")])).expect("resolves").is_none(), "0 disables the scheduler");
		let config = resolve_maintenance_config(env(&[(RECONCILE_INTERVAL_ENV, "5"), (COMPACT_TARGET_ROWS_ENV, "1000"), (RECONCILE_THRESHOLD_ENV, "3")])).expect("resolves").expect("enabled");
		assert_eq!(config.interval, Duration::from_secs(5));
		assert_eq!(config.compact_target_rows, Some(1000));
		assert_eq!(config.threshold, 3);
	}

	/// A malformed numeric setting still stops the server (the phase-4
	/// exception), for the interval and for the compaction target alike.
	#[test]
	fn maintenance_scheduler_rejects_malformed_numbers() {
		assert!(resolve_maintenance_config(env(&[(RECONCILE_INTERVAL_ENV, "soon")])).is_err());
		assert!(resolve_maintenance_config(env(&[(COMPACT_TARGET_ROWS_ENV, "many")])).is_err());
		assert!(resolve_maintenance_config(env(&[(RECONCILE_THRESHOLD_ENV, "some")])).is_err());
	}
}
