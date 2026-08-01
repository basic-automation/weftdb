//! Automatic background control-plane backup (roadmap **Phase 7.4**).
//!
//! The manual `POST /api/v1/storage/backup` endpoint (see [`manage`](crate::manage))
//! lets an operator take one online, verified snapshot of the store's four
//! control-plane databases over Turso's `VACUUM INTO`. This module turns that same
//! primitive into a **background timer**, mirroring the
//! [`reconcile_daemon`](crate::reconcile_daemon): on an interval it snapshots the
//! control plane into a fresh timestamped directory under a configured base, and
//! optionally prunes the oldest snapshots so an unattended daemon cannot fill the disk.
//!
//! Per the storage boundary (hard-constraint #3) this backs up the **control plane
//! only** — the `.dspseg` measurement frames are not part of the snapshot.
//!
//! # Retention safety
//!
//! Pruning only ever removes directories the daemon itself generated —
//! `backup-<digits>`, the shape [`generated_label`] writes. A snapshot an operator
//! took by hand with `POST …/storage/backup?label=nightly` is never a prune candidate,
//! so a retention setting cannot silently delete a deliberately-kept backup.
//!
//! The daemon holds only `Arc` handles (the store and the metrics registry), so it is
//! a detached side task; the router and its handlers are untouched.

use std::{
	path::{Path, PathBuf},
	sync::Arc,
	time::Duration,
};

use database::{ControlPlaneBackup, SegmentStore, VerifyMode};
use tracing::Instrument as _;

use crate::metrics::SharedMetrics;

/// Configuration for the background backup daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupDaemonConfig {
	/// How often the daemon snapshots the control plane.
	pub interval: Duration,
	/// The directory each snapshot's `backup-<unix_millis>` subdirectory is created
	/// under (`DSP_BACKUP_DIR` if set, else `<store_root>/backups` — the same base the
	/// manual endpoint uses, so hand-taken and daemon-taken snapshots live together).
	pub base: PathBuf,
	/// How many **generated** snapshots to retain. After each successful tick the
	/// oldest `backup-<digits>` directories beyond this many are removed. `None`
	/// retains everything (the operator prunes out of band).
	pub keep: Option<usize>,
}

/// The label a daemon tick writes its snapshot under: `backup-<unix_millis>`, matching
/// the manual endpoint's generated (unlabelled) form.
///
/// `millis` is the wall-clock stamp; the caller passes it so the naming is testable
/// without a clock.
#[must_use]
pub fn generated_label(millis: u128) -> String {
	format!("backup-{millis}")
}

/// True for a directory name this daemon generated — `backup-` followed by at least one
/// digit and nothing else. The prune predicate: an operator's own `?label=nightly`
/// snapshot does not match and is never removed.
fn is_generated_label(name: &str) -> bool {
	name.strip_prefix("backup-").is_some_and(|stamp| !stamp.is_empty() && stamp.chars().all(|c| c.is_ascii_digit()))
}

/// Pick a fresh snapshot directory under `base` for the stamp `millis`.
///
/// `VACUUM INTO` needs a non-existing destination file, so a tick must not reuse a
/// directory. Two ticks inside the same millisecond (or a restart landing on an existing
/// stamp) are disambiguated with a `-1`, `-2`, … suffix; the suffixed form still matches
/// [`is_generated_label`] only when the suffix keeps it all-digits, so the plain and
/// suffixed names are both prunable.
fn fresh_dir(base: &Path, millis: u128) -> PathBuf {
	let first = base.join(generated_label(millis));
	if !first.exists() {
		return first;
	}
	// A same-millisecond collision: walk forward one millisecond at a time rather than
	// adding a non-numeric suffix, so every generated name stays `backup-<digits>` and
	// therefore stays prunable and sortable.
	let mut stamp = millis + 1;
	loop {
		let candidate = base.join(generated_label(stamp));
		if !candidate.exists() {
			return candidate;
		}
		stamp += 1;
	}
}

/// List the daemon-generated snapshot directories under `base`, oldest first.
///
/// Returns `(stamp, path)` pairs sorted by the numeric stamp, so "oldest" is the
/// snapshot's own recorded time rather than a filesystem mtime (which a copy or a
/// restore would perturb). Non-matching entries (an operator's labelled backup, a stray
/// file) are ignored. A missing `base` yields an empty list.
///
/// # Errors
///
/// Propagates a failure reading `base` (other than its absence).
pub async fn list_generated_backups(base: &Path) -> anyhow::Result<Vec<(u128, PathBuf)>> {
	let mut entries = match tokio::fs::read_dir(base).await {
		Ok(entries) => entries,
		Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
		Err(err) => return Err(anyhow::Error::new(err).context(format!("reading backup base {}", base.display()))),
	};
	let mut found = Vec::new();
	while let Some(entry) = entries.next_entry().await? {
		if !entry.file_type().await?.is_dir() {
			continue;
		}
		let name = entry.file_name();
		let Some(name) = name.to_str() else { continue };
		if !is_generated_label(name) {
			continue;
		}
		let Ok(stamp) = name.trim_start_matches("backup-").parse::<u128>() else { continue };
		found.push((stamp, entry.path()));
	}
	found.sort_unstable_by_key(|(stamp, _)| *stamp);
	Ok(found)
}

/// Remove the oldest daemon-generated snapshots under `base` until at most `keep`
/// remain, returning how many directories were removed.
///
/// Only `backup-<digits>` directories are candidates (see [`is_generated_label`]), so a
/// hand-labelled snapshot is never pruned. `keep = 0` removes every generated snapshot.
///
/// # Errors
///
/// Propagates a failure listing `base` or removing a snapshot directory.
pub async fn prune_generated_backups(base: &Path, keep: usize) -> anyhow::Result<usize> {
	let found = list_generated_backups(base).await?;
	let excess = found.len().saturating_sub(keep);
	let mut removed = 0usize;
	for (_, path) in found.into_iter().take(excess) {
		tokio::fs::remove_dir_all(&path).await.map_err(|err| anyhow::Error::new(err).context(format!("pruning backup {}", path.display())))?;
		removed += 1;
	}
	Ok(removed)
}

/// Run one backup tick: snapshot the control plane into a fresh directory under
/// `base`, record it in `metrics`, and return the [`ControlPlaneBackup`].
///
/// Factored out of the timer loop so the per-tick behaviour is unit-testable without
/// waiting on a real interval. The snapshot is recorded in the same
/// `dsp_backup_snapshots_total`/`dsp_backup_bytes_written_total` counters the manual
/// endpoint uses, so an operator sees one backup cadence whichever path took it.
///
/// Verification runs in [`VerifyMode::SnapshotOnly`]: the daemon backs up a **live,
/// ingesting** store, so a seal committing between a database's vacuum and its
/// verification would make a source-matching row-count check fail spuriously. The
/// snapshot-only check never re-reads the source and instead proves the copy opens and
/// every row of it is readable — the strongest statement available without stopping
/// writers.
///
/// # Errors
///
/// Propagates a clock failure, or any
/// [`SegmentStore::backup_control_plane_with_verify`](database::SegmentStore::backup_control_plane_with_verify)
/// backup/verify failure.
pub async fn backup_tick(store: &SegmentStore, metrics: &SharedMetrics, base: &Path) -> anyhow::Result<ControlPlaneBackup> {
	let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis();
	let dest = fresh_dir(base, millis);
	let span = tracing::info_span!("backup.tick", dir = %dest.display(), verify = "snapshot", rows = tracing::field::Empty, bytes = tracing::field::Empty);
	let backup = store.backup_control_plane_with_verify(&dest, VerifyMode::SnapshotOnly).instrument(span.clone()).await?;
	span.record("rows", backup.total_rows());
	span.record("bytes", backup.total_bytes());
	metrics.record_backup(backup.total_bytes());
	Ok(backup)
}

/// Spawn the background backup daemon on `config.interval`.
///
/// Returns the task handle; dropping it leaves the daemon running detached for the
/// process lifetime, which is the intended deployment shape.
///
/// The first snapshot fires one interval after start (the immediate `interval` tick is
/// consumed first), so start-up is not stampeded by an eager backup. Each snapshot logs
/// a one-line summary; a failed snapshot is logged and the loop continues (a transient
/// control-plane error must not kill the daemon, and the next tick retries into a fresh
/// directory).
#[must_use]
pub fn spawn_backup_daemon(store: Arc<SegmentStore>, metrics: SharedMetrics, config: BackupDaemonConfig) -> tokio::task::JoinHandle<()> {
	tokio::spawn(async move {
		let mut ticker = tokio::time::interval(config.interval);
		// Consume the immediate first tick so the first snapshot waits one interval.
		ticker.tick().await;
		loop {
			ticker.tick().await;
			match backup_tick(&store, &metrics, &config.base).await {
				Ok(backup) => {
					println!("backup daemon: snapshotted control plane to {} ({} row(s), {} byte(s))", backup.dir.display(), backup.total_rows(), backup.total_bytes());
					// Retention runs only after a successful snapshot, so a run of failures
					// can never prune the last good backup away.
					if let Some(keep) = config.keep {
						match prune_generated_backups(&config.base, keep).await {
							Ok(removed) if removed > 0 => println!("backup daemon: pruned {removed} snapshot(s), keeping the newest {keep}"),
							Ok(_) => {},
							Err(err) => eprintln!("backup daemon: prune failed: {err}"),
						}
					}
				},
				Err(err) => eprintln!("backup daemon: snapshot failed: {err}"),
			}
		}
	})
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use bigdecimal::BigDecimal;
	use database::SegmentStore;
	use dsp_physical_type::{AspectSchema, PhysicalType, TimeUnit};
	use tempfile::TempDir;

	use super::*;
	use crate::metrics::Metrics;

	fn schema() -> AspectSchema {
		AspectSchema::new(PhysicalType::F64, "0".parse().unwrap(), TimeUnit::Seconds)
	}

	fn bd(s: &str) -> BigDecimal {
		s.parse().unwrap()
	}

	#[test]
	fn generated_labels_are_recognised_and_operator_labels_are_not() {
		assert!(is_generated_label(&generated_label(1_753_000_000_000)));
		assert!(is_generated_label("backup-0"));
		assert!(!is_generated_label("backup-"), "the stamp must be present");
		assert!(!is_generated_label("nightly"), "an operator label is never a prune candidate");
		assert!(!is_generated_label("backup-nightly"), "a non-numeric stamp is not generated");
		assert!(!is_generated_label("backup-12a"), "a partly-numeric stamp is not generated");
		assert!(!is_generated_label("prod-backup-12"), "the prefix must be at the start");
	}

	#[tokio::test]
	async fn tick_snapshots_the_control_plane_and_records_metrics() {
		let dir = TempDir::new().unwrap();
		let store = SegmentStore::open(dir.path()).await.unwrap();
		store.declare("a", &schema()).await.unwrap();
		store.seal("a", &schema(), &[0_i64, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.unwrap();
		let metrics: SharedMetrics = Arc::new(Metrics::default());
		let base = dir.path().join("backups");

		let backup = backup_tick(&store, &metrics, &base).await.unwrap();
		let snap = metrics.snapshot();
		drop(store);

		assert!(backup.dir.starts_with(&base), "the snapshot landed under the configured base");
		assert!(is_generated_label(backup.dir.file_name().unwrap().to_str().unwrap()), "the tick uses a generated, prunable label");
		for name in ["segment_index.db", "metadata.db", "aspect_catalog.db", "catalog.db"] {
			assert!(backup.dir.join(name).exists(), "{name} written to disk");
		}
		assert!(backup.total_rows() > 0, "the declared aspect + sealed segment are in the control plane");
		assert_eq!(snap.backup.snapshots, 1, "the daemon tick records the same counter as the endpoint");
		assert_eq!(snap.backup.bytes_written, backup.total_bytes());
	}

	#[tokio::test]
	async fn two_ticks_land_in_distinct_directories() {
		let dir = TempDir::new().unwrap();
		let store = SegmentStore::open(dir.path()).await.unwrap();
		store.declare("a", &schema()).await.unwrap();
		let metrics: SharedMetrics = Arc::new(Metrics::default());
		let base = dir.path().join("backups");

		let first = backup_tick(&store, &metrics, &base).await.unwrap();
		let second = backup_tick(&store, &metrics, &base).await.unwrap();
		let listed = list_generated_backups(&base).await.unwrap();
		drop(store);

		assert_ne!(first.dir, second.dir, "a second tick never reuses a directory (VACUUM INTO needs a fresh file)");
		assert_eq!(listed.len(), 2, "both snapshots are listed");
		assert_eq!(listed[0].1, first.dir, "listing is oldest-first by stamp");
		assert_eq!(listed[1].1, second.dir);
		assert_eq!(metrics.snapshot().backup.snapshots, 2);
	}

	#[tokio::test]
	async fn prune_keeps_the_newest_and_never_touches_an_operator_label() {
		let dir = TempDir::new().unwrap();
		let base = dir.path().join("backups");
		// Three generated snapshots (stamps out of creation order to prove the sort is by
		// stamp, not by mtime) plus one hand-labelled operator backup.
		for name in ["backup-300", "backup-100", "backup-200", "nightly"] {
			tokio::fs::create_dir_all(base.join(name)).await.unwrap();
			tokio::fs::write(base.join(name).join("catalog.db"), b"x").await.unwrap();
		}

		let removed = prune_generated_backups(&base, 2).await.unwrap();
		assert_eq!(removed, 1, "one snapshot beyond the newest two");
		assert!(!base.join("backup-100").exists(), "the oldest generated snapshot was pruned");
		assert!(base.join("backup-200").exists());
		assert!(base.join("backup-300").exists());
		assert!(base.join("nightly").exists(), "an operator's labelled backup is never pruned");

		// Retention is idempotent once the count is at the limit.
		assert_eq!(prune_generated_backups(&base, 2).await.unwrap(), 0);
	}

	#[tokio::test]
	async fn prune_on_a_missing_base_is_a_no_op() {
		let dir = TempDir::new().unwrap();
		let base = dir.path().join("never-created");
		assert!(list_generated_backups(&base).await.unwrap().is_empty());
		assert_eq!(prune_generated_backups(&base, 3).await.unwrap(), 0);
	}

	#[tokio::test]
	async fn tick_then_prune_holds_the_retention_bound() {
		let dir = TempDir::new().unwrap();
		let store = SegmentStore::open(dir.path()).await.unwrap();
		store.declare("a", &schema()).await.unwrap();
		let metrics: SharedMetrics = Arc::new(Metrics::default());
		let base = dir.path().join("backups");

		// Four ticks with a keep of 2 leaves exactly the newest two on disk — the loop
		// body's contract, exercised without waiting on a timer.
		let mut dirs = Vec::new();
		for _ in 0..4 {
			dirs.push(backup_tick(&store, &metrics, &base).await.unwrap().dir);
			prune_generated_backups(&base, 2).await.unwrap();
		}
		let listed = list_generated_backups(&base).await.unwrap();
		drop(store);

		assert_eq!(listed.len(), 2, "retention held at two snapshots");
		assert_eq!(listed[0].1, dirs[2], "the newest two survive");
		assert_eq!(listed[1].1, dirs[3]);
		assert_eq!(metrics.snapshot().backup.snapshots, 4, "every tick was still counted");
	}
}
