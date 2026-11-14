#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]
// Every test here keeps a `TempDir` alive while its assertions reopen paths derived from it, so
// the early-drop suggestion this lint makes would delete the tree underneath the test.
#![allow(clippy::significant_drop_tightening)]
//! Acceptance tests for `POST /api/v1/storage/restore/into` — restoring a
//! backup into a caller-chosen new root under the restore base.
//!
//! Four scenarios, each pinning a behaviour the handler must provide:
//!
//! 1. A restore refused for insufficient disk reports the documented status (507)
//!    and leaves no `.restoring-*` sibling behind.
//! 2. A failure after staging began leaves no `.restoring-*` sibling behind.
//! 3. A completed restore-into produces the requested root name (e.g. `backup-123`)
//!    and that root is visible to listing and survives a prune pass.
//! 4. A restore base configured *inside* the backup base is refused.
//!
//! Each test must fail without the handler change (pin the behaviour, not the plumbing).

use std::{path::Path, str::FromStr, sync::Arc};

use axum::{
	body::Body, http::{Request, StatusCode}, Router
};
use bigdecimal::BigDecimal;
use serial_test::serial;
use tower::ServiceExt as _;
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weft_server::{app_with_state, AppState};
use weftdb::{BackupMode, SegmentStore};

/// A lossless `f64` schema in seconds — the shape the store's own backup tests seal under.
fn schema() -> AspectSchema {
	AspectSchema::new(PhysicalType::F64, dec("0"), TimeUnit::Seconds)
}

fn dec(text: &str) -> BigDecimal {
	BigDecimal::from_str(text).expect("a decimal literal")
}

/// A helper: prepare a store at `root` with one aspect and one sealed frame.
async fn store_with_frame(root: &Path) -> SegmentStore {
	let store = SegmentStore::open(root).await.expect("opens a store");
	store.declare("price", &schema()).await.expect("declares an aspect");
	store.seal_declared("price", &[0, 10, 20], &[dec("1"), dec("2"), dec("3")]).await.expect("seals a frame");
	store
}

/// A helper: back the store at `live` up into `backup_dir`.
async fn whole_store_backup(live: &Path, backup_dir: &Path) {
	let store = SegmentStore::open(live).await.expect("opens the live store");
	let report = store.backup_store(backup_dir, BackupMode::Copy).await.expect("backs the store up");
	assert!(!report.frames.is_empty(), "the backup carries at least one frame");
	drop(store);
}

/// Build a router over a fresh store at `root`, ready to serve.
async fn router_with_store(root: &Path) -> Router {
	app_with_state(AppState::new().with_store(Arc::new(SegmentStore::open(root).await.expect("store opens"))))
}

/// Send `request` and return its status and parsed JSON body.
async fn send_json(router: Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
	let response = router.oneshot(request).await.expect("the router answers every request");
	let status = response.status();
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("a body fits").to_vec();
	let body = serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("{status} answer is not JSON ({error}); body was {:?}", String::from_utf8_lossy(&bytes)));
	(status, body)
}

/// A POST to the restore-into endpoint with query params.
fn restore_into_request(label: &str, name: &str) -> Request<Body> {
	let uri = format!("/api/v1/storage/restore/into?label={label}&name={name}");
	Request::builder().method("POST").uri(uri).header(axum::http::header::CONTENT_TYPE, "application/json").body(Body::empty()).unwrap()
}

/// Convert a Path to a String for env-variable setting.
fn path_to_env(path: &Path) -> String {
	path.display().to_string()
}

/// Check `entries` contains `target`.
fn list_contains(entries: &[String], target: &str) -> bool {
	entries.iter().any(|e| e == target)
}

/// List the entry file names under `dir` that match `predicate`.
fn matching_entry_names(dir: &Path, predicate: fn(&str) -> bool) -> Vec<String> {
	let mut names = Vec::new();
	for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
		if let Ok(name) = entry.file_name().into_string() {
			if predicate(&name) {
				names.push(name);
			}
		}
	}
	names
}

// ---------------------------------------------------------------------------
// Test 1: insufficient disk → 507, no `.restoring-*`
// ---------------------------------------------------------------------------

#[serial]
#[tokio::test]
async fn insufficient_disk_returns_507_and_leaves_no_staging() {
	let dir = tempfile::tempdir().expect("tempdir");
	let live = dir.path().join("live");
	let backups = dir.path().join("backups");

	// Seed a store with one frame and back it up.
	{
		let store = store_with_frame(&live).await;
		store.shutdown().await.expect("shuts down");
	}
	whole_store_backup(&live, &backups.join("test-backup")).await;

	// Prepare a "live-http" store for the router (different from the real live store).
	let live_http = dir.path().join("live-http");
	{
		let s = SegmentStore::open(&live_http).await.expect("opens http store");
		s.declare("dummy", &schema()).await.expect("declares");
		s.seal_declared("dummy", &[0], &[dec("1")]).await.expect("seals");
		s.shutdown().await.expect("shuts down");
	}

	// Set an impossibly high disk reserve so the disk guard refuses.
	std::env::set_var("WEFT_DISK_RESERVE_BYTES", "999999999999999");
	// Place the restore base outside the backup tree so the containment check passes.
	let restore_base = dir.path().join("restored");
	std::env::set_var("WEFT_RESTORE_BASE_DIR", path_to_env(&restore_base));

	// The handler resolves the backup base as `WEFT_BACKUP_DIR`, falling back to
	// `<live root>/backups` (`weft-server/src/backup_routes.rs:145`). These fixtures keep
	// their snapshots in a tree of their own, so point the variable at it.
	std::env::set_var("WEFT_BACKUP_DIR", path_to_env(&backups));

	let router = router_with_store(&live_http).await;

	let (status, body) = send_json(router, restore_into_request("test-backup", "restored-root")).await;
	assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE, "disk guard refuses: {body}");
	assert_eq!(body["code"], "insufficient_storage", "the code is the documented one: {body}");

	// Verify no `.restoring-*` directory was left behind.
	let entries = matching_entry_names(&restore_base, |n| n.starts_with(".restoring-"));
	assert!(entries.is_empty(), "no .restoring-* sibling was left: {entries:?}");
}

// ---------------------------------------------------------------------------
// Test 2: failure after staging began → no `.restoring-*`
// ---------------------------------------------------------------------------

#[serial]
#[tokio::test]
async fn failed_restore_cleans_up_staging() {
	let dir = tempfile::tempdir().expect("tempdir");
	let live = dir.path().join("live");
	let backups = dir.path().join("backups");

	// Seed a store and take a whole-store backup.
	{
		let store = store_with_frame(&live).await;
		store.shutdown().await.expect("shuts down");
	}
	whole_store_backup(&live, &backups.join("test-backup")).await;

	// Corrupt the backup: read the manifest and remove the first referenced file.
	let manifest_path = backups.join("test-backup").join("MANIFEST.json");
	let manifest_text = tokio::fs::read(&manifest_path).await.expect("reads manifest");
	let manifest = serde_json::from_slice::<serde_json::Value>(&manifest_text).expect("parses manifest");
	let Some(first_file) = manifest["files"].as_array().expect("manifest has files").iter().next() else {
		panic!("backup has files to remove");
	};
	let name = first_file["name"].as_str().expect("file name");
	std::fs::remove_file(backups.join("test-backup").join(name)).expect("removes the file");

	// Prepare a "live-http" store for the router.
	let live_http = dir.path().join("live-http");
	{
		let s = SegmentStore::open(&live_http).await.expect("opens http store");
		s.declare("dummy", &schema()).await.expect("declares");
		s.seal_declared("dummy", &[0], &[dec("1")]).await.expect("seals");
		s.shutdown().await.expect("shuts down");
	}

	// Disable disk guard for this test (we want the restore to start and fail).
	std::env::set_var("WEFT_DISK_RESERVE_BYTES", "0");
	let restore_base = dir.path().join("restored");
	std::env::set_var("WEFT_RESTORE_BASE_DIR", path_to_env(&restore_base));

	// The handler resolves the backup base as `WEFT_BACKUP_DIR`, falling back to
	// `<live root>/backups` (`weft-server/src/backup_routes.rs:145`). These fixtures keep
	// their snapshots in a tree of their own, so point the variable at it.
	std::env::set_var("WEFT_BACKUP_DIR", path_to_env(&backups));

	let router = router_with_store(&live_http).await;
	let (status, body) = send_json(router, restore_into_request("test-backup", "restored-root")).await;
	assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "restore fails with 500: {body}");

	// Verify no `.restoring-*` directory was left behind.
	let entries = matching_entry_names(&restore_base, |n| n.starts_with(".restoring-"));
	assert!(entries.is_empty(), "no .restoring-* sibling was left after failure: {entries:?}");
}

// ---------------------------------------------------------------------------
// Test 3: completed restore produces the name, survives listing and prune
// ---------------------------------------------------------------------------

#[serial]
#[tokio::test]
async fn completed_restore_produces_the_chosen_root_and_survives_prune() {
	let dir = tempfile::tempdir().expect("tempdir");
	let live = dir.path().join("live");
	let backups = dir.path().join("backups");
	let restore_base = dir.path().join("restored");

	// Seed a store and take a whole-store backup.
	{
		let store = store_with_frame(&live).await;
		store.shutdown().await.expect("shuts down");
	}
	whole_store_backup(&live, &backups.join("test-backup")).await;

	// Prepare a "live-http" store for the router.
	let live_http = dir.path().join("live-http");
	{
		let s = SegmentStore::open(&live_http).await.expect("opens http store");
		s.declare("dummy", &schema()).await.expect("declares");
		s.seal_declared("dummy", &[0], &[dec("1")]).await.expect("seals");
		s.shutdown().await.expect("shuts down");
	}

	// A second snapshot under a *generated* label, the shape the daemon's own tick writes:
	// `is_generated_label` (`weft-server/src/backup_daemon.rs:138`) accepts only
	// `backup-<digits>`, and `is_complete_backup` (`weftdb/src/types/backup.rs:634`) accepts any
	// directory holding a manifest, so this one is a genuine prune candidate while `test-backup`
	// is not.
	whole_store_backup(&live, &backups.join("backup-20260101")).await;

	// Normalise env: reserve 0 (no disk guard interference), restore base set.
	std::env::set_var("WEFT_DISK_RESERVE_BYTES", "0");
	std::env::set_var("WEFT_RESTORE_BASE_DIR", path_to_env(&restore_base));

	// The handler resolves the backup base as `WEFT_BACKUP_DIR`, falling back to
	// `<live root>/backups` (`weft-server/src/backup_routes.rs:145`). These fixtures keep
	// their snapshots in a tree of their own, so point the variable at it.
	std::env::set_var("WEFT_BACKUP_DIR", path_to_env(&backups));

	let router = router_with_store(&live_http).await;
	let (status, body) = send_json(router, restore_into_request("test-backup", "backup-123")).await;
	assert_eq!(status, StatusCode::OK, "restore succeeds: {body}");
	assert_eq!(body["restored_root"].as_str().expect("restored_root is a string"), restore_base.join("backup-123").display().to_string(), "the restored root is the expected path");
	assert_eq!(body["mode"], "whole_store", "the backup is a whole-store backup");
	assert!(body["rows_verified"].as_u64().unwrap_or(0) > 0, "rows were verified");

	let restored_root = restore_base.join("backup-123");
	assert!(restored_root.is_dir(), "the restored root directory exists");

	// The restored root is visible to filesystem listing.
	let entries = matching_entry_names(&restore_base, |_| true);
	assert!(list_contains(&entries, "backup-123"), "backup-123 is listed in the restore base: {entries:?}");

	// The restored root survives a retention pass. This is the guarantee the containment check
	// exists for: `prune_generated_backups` deletes generated `backup-<digits>` directories it
	// counts inside the base it is pointed at, so a restored root is only safe because the
	// restore base is a different directory from the backup base. Point the real daemon sweep at
	// the backup base with `keep = 0` — it must remove the generated snapshot there and leave `backup-123`
	// under the restore base alone.
	let pruned = weft_server::backup_daemon::prune_generated_backups(&backups, 0).await.expect("a retention pass runs");
	assert_eq!(pruned, 1, "only the generated `backup-20260101` snapshot retires, pruned {pruned}");
	assert!(restored_root.is_dir(), "the retention pass must not touch a restored root in another base: {restored_root:?}");
	assert!(list_contains(&matching_entry_names(&backups, |n| n == "test-backup"), "test-backup"), "a hand-labelled snapshot in the backup base is not a generated name and must survive: {backups:?}");
}

// ---------------------------------------------------------------------------
// Test 4: restore base inside backup base is refused
// ---------------------------------------------------------------------------

#[serial]
#[tokio::test]
async fn restore_base_inside_backup_base_is_refused() {
	let dir = tempfile::tempdir().expect("tempdir");
	let live = dir.path().join("live");
	let backups = dir.path().join("backups");

	// Place the restore base *inside* the backup base — the handler must refuse.
	let restore_base = backups.join("subdir");

	{
		let store = store_with_frame(&live).await;
		store.shutdown().await.expect("shuts down");
	}
	whole_store_backup(&live, &backups.join("test-backup")).await;

	// Prepare a "live-http" store for the router.
	let live_http = dir.path().join("live-http");
	{
		let s = SegmentStore::open(&live_http).await.expect("opens http store");
		s.declare("dummy", &schema()).await.expect("declares");
		s.seal_declared("dummy", &[0], &[dec("1")]).await.expect("seals");
		s.shutdown().await.expect("shuts down");
	}

	// Set env so the handler reads the nested restore base.
	std::env::set_var("WEFT_DISK_RESERVE_BYTES", "0");
	std::env::set_var("WEFT_RESTORE_BASE_DIR", path_to_env(&restore_base));

	// The handler resolves the backup base as `WEFT_BACKUP_DIR`, falling back to
	// `<live root>/backups` (`weft-server/src/backup_routes.rs:145`). These fixtures keep
	// their snapshots in a tree of their own, so point the variable at it.
	std::env::set_var("WEFT_BACKUP_DIR", path_to_env(&backups));

	let router = router_with_store(&live_http).await;
	let (status, body) = send_json(router, restore_into_request("test-backup", "sub-restored")).await;
	assert_eq!(status, StatusCode::BAD_REQUEST, "restore base inside backup base is 400: {body}");
	assert_eq!(body["code"], "bad_request", "the code is bad_request: {body}");
	let message = body["error"].as_str().expect("message is a string");
	assert!(message.contains("restore base") || message.contains("backup base"), "message mentions the conflict: {message}");
}
