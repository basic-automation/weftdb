//! PAC-13's offline restore, end to end: a backup this server wrote, put back on disk as a
//! store root nothing is serving, and then read the way a server reads a store.
//!
//! D-S16 landed the restore as a library call, so an operator who lost a store had a
//! function they could not run. These tests run the command as the binary runs it — through
//! [`restore::execute`] with the same [`Request`] the parser builds, against a real store and
//! a real backup directory — and then check the two things the operator actually needs: the
//! rows come back, and the frame adoption that the first open would do has really happened.

use std::{path::Path, str::FromStr};

use bigdecimal::BigDecimal;
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weft_server::restore::{self, Request};
use weftdb::{BackupMode, SegmentStore, VerifyMode};

/// A lossless `f64` schema in seconds — the shape the store's own backup tests seal under.
fn schema() -> AspectSchema {
	AspectSchema::new(PhysicalType::F64, dec("0"), TimeUnit::Seconds)
}

fn dec(text: &str) -> BigDecimal {
	BigDecimal::from_str(text).expect("a decimal literal")
}

/// A request for `backup` into `into`, with the scope pinned so a developer's environment
/// cannot change what the test asserts.
fn request(backup: &Path, into: &Path, adopt: bool) -> Request {
	Request { backup: backup.to_path_buf(), into: into.to_path_buf(), adopt, database: Some("default".into()), subject: Some("default".into()), json: false }
}

/// Open a store at `root`, give it one aspect and one sealed frame, and close it cleanly.
///
/// Returns the store's UUID, which the restore has to carry into the new root.
async fn store_with_a_frame(root: &Path) -> String {
	let store = SegmentStore::open(root).await.expect("opens a store");
	store.declare("price", &schema()).await.expect("declares an aspect");
	store.seal_declared("price", &[0, 10, 20], &[dec("1"), dec("2"), dec("3")]).await.expect("seals a frame");
	let uuid = store.store_uuid().to_string();
	store.shutdown().await.expect("shuts down cleanly");
	drop(store);
	uuid
}

/// Take a whole-store backup of the store at `root` into `backup`.
async fn whole_store_backup(root: &Path, backup: &Path) {
	let store = SegmentStore::open(root).await.expect("opens the live store");
	let report = store.backup_store(backup, BackupMode::Copy).await.expect("backs the store up");
	assert_eq!(report.frames.len(), 1, "the backup carries the one sealed frame");
	drop(store);
}

/// Read every `price` row back out of the store at `root`, the way a server does.
async fn rows_in(root: &Path) -> (Vec<i64>, Vec<String>) {
	let store = SegmentStore::open(root).await.expect("the restored root opens");
	let (times, values) = store.read_time_range("price", i64::MIN, i64::MAX).await.expect("reads the aspect back");
	let values = values.into_iter().map(|value| value.expect("a non-null value").to_plain_string()).collect();
	drop(store);
	(times, values)
}

/// PAC-13: `weft-server restore` with `--adopt` puts a whole-store backup back as a root
/// that opens and reads, and clears the restore intent marker by opening it.
#[tokio::test]
async fn a_whole_store_backup_restores_into_a_fresh_root_and_reads_back() {
	let dir = tempfile::TempDir::new().expect("tempdir");
	let live = dir.path().join("live");
	let backup = dir.path().join("backups/backup-2026-11-01T00-00-00Z");
	let restored = dir.path().join("restored");
	let uuid = store_with_a_frame(&live).await;
	whole_store_backup(&live, &backup).await;

	let report = restore::execute(&request(&backup, &restored, true)).await.expect("restores");
	assert_eq!(report.mode, "whole-store", "a whole-store backup restores whole");
	assert_eq!(report.root, restored);
	assert_eq!(report.frames, 1, "the report counts the frame it verified");
	assert_eq!(report.intent.as_ref().expect("a whole-store restore publishes an intent").store_uuid, uuid, "the restored root is the same store");
	let names: Vec<&str> = report.files.iter().map(|file| file.name.as_str()).collect();
	assert_eq!(names, weftdb::CONTROL_PLANE_FILES, "every control-plane file is reported, in the library's order");
	assert!(report.rows > 0, "the control plane holds the catalog rows the restore verified");
	let adoption = report.adoption.expect("--adopt opens the root");
	assert!(adoption.restore_mode, "the root still carried its intent marker, so this open was the recovery");
	assert_eq!((adoption.adopted, adoption.held, adoption.quarantined, adoption.suspected, adoption.orphans), (0, 0, 0, 0, 0), "the restored index already knows its frame, so the pass adopts nothing and quarantines nothing");
	assert!(!adoption.incompatible_store, "a store this build wrote is a store this build can read");
	assert!(!restored.join(weftdb::RESTORE_INTENT_FILE).exists(), "the first open completes the restore and removes the marker");

	// The promise, measured where an operator measures it: the data is there.
	assert_eq!(rows_in(&restored).await, (vec![0, 10, 20], vec!["1".to_string(), "2".to_string(), "3".to_string()]));

	// The backup is a copy, and a restore reads it without consuming it: a second restore
	// to another root is the same operation, which is what a drill looks like.
	let second = dir.path().join("drill");
	let again = restore::execute(&request(&backup, &second, false)).await.expect("restores again");
	assert_eq!(again.frames, 1);
	assert_eq!(rows_in(&second).await, (vec![0, 10, 20], vec!["1".to_string(), "2".to_string(), "3".to_string()]));
}

/// PAC-13: without `--adopt` the restore stops at the disk state the first open expects,
/// so an operator who restores and then serves gets adoption at that open, not never.
#[tokio::test]
async fn a_restore_without_adopt_leaves_the_intent_marker_for_the_first_open() {
	let dir = tempfile::TempDir::new().expect("tempdir");
	let live = dir.path().join("live");
	let backup = dir.path().join("backup-nightly");
	let restored = dir.path().join("restored");
	let uuid = store_with_a_frame(&live).await;
	whole_store_backup(&live, &backup).await;

	let report = restore::execute(&request(&backup, &restored, false)).await.expect("restores");
	assert!(report.adoption.is_none(), "nothing opened the root, so nothing is reported about adoption");
	let intent = weftdb::read_restore_intent(&restored).await.expect("reads").expect("the marker is published with the root");
	assert_eq!(intent.store_uuid, uuid);
	assert_eq!(rows_in(&restored).await, (vec![0, 10, 20], vec!["1".to_string(), "2".to_string(), "3".to_string()]), "serving the root is the adoption pass");
	assert!(!restored.join(weftdb::RESTORE_INTENT_FILE).exists(), "the open that served it cleared the marker");
}

/// PAC-13: a control-plane backup has no frames, so the same command restores the four
/// databases and says so — with an intent only a whole-store backup can carry.
#[tokio::test]
async fn a_control_plane_backup_restores_without_frames() {
	let dir = tempfile::TempDir::new().expect("tempdir");
	let live = dir.path().join("live");
	let backup = dir.path().join("backup-cp");
	let restored = dir.path().join("restored");
	store_with_a_frame(&live).await;
	{
		let store = SegmentStore::open(&live).await.expect("opens the live store");
		store.backup_control_plane_with_verify(&backup, VerifyMode::SnapshotOnly).await.expect("backs up the control plane");
		drop(store);
	}

	let report = restore::execute(&request(&backup, &restored, true)).await.expect("restores");
	assert_eq!(report.mode, "control-plane");
	assert_eq!(report.frames, 0, "a control-plane backup carries no frames");
	assert!(report.intent.is_none(), "no whole-store manifest, so no store identity to report");
	let adoption = report.adoption.expect("--adopt opens the root");
	assert!(!adoption.restore_mode, "a control-plane restore publishes no marker, so this open is an ordinary one");
	let store = SegmentStore::open(&restored).await.expect("the restored control plane opens");
	assert_eq!(store.schema_for("price").await.expect("asks").expect("the catalog came back").value, PhysicalType::F64, "the catalog survived the round trip");
	drop(store);
}
