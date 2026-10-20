//! The backup and restore-drill endpoints — the operational half of the write
//! path, and the only one that produces files outside the store root.
//!
//! Both follow the grammar the backup daemon already uses (roadmap SOA-7): the
//! label is `manual-<utc-rfc3339>`, staging names are refused as labels, and an
//! unlabelled run never evicts the daemon's own snapshots. The drill rehearses a
//! restore into a scratch directory and reports what it found, because a backup
//! nobody has restored is a hope, not a plan.

use axum::{
	extract::{Query, State}, http::StatusCode, response::{IntoResponse, Response}, Json
};
use serde::{Deserialize, Serialize};
use weftdb::{
	durable::{RealFs, StoreFs}, VerifyMode, RESTORE_DRILL_PREFIX
};

use crate::{state::AppState, storage::StorageError};

/// Query parameters for `POST /api/v1/storage/restore/drill`.
#[derive(Debug, Clone, Deserialize)]
pub struct RestoreDrillParams {
	/// The backup subdirectory to rehearse restoring, under the same backup root the
	/// snapshot endpoint writes to, in the same label grammar
	/// (`[A-Za-z0-9][A-Za-z0-9._-]{0,99}`, not ending with `.`). Unlike the backup
	/// endpoint, the drill accepts a daemon snapshot's `backup-<digits>`: it only reads it.
	pub label: String,
}

/// Response body for `POST /api/v1/storage/restore/drill` — the outcome of a rehearsed
/// restore.
#[derive(Debug, Clone, Serialize)]
pub struct RestoreDrillResponse {
	/// The backup directory that was rehearsed.
	pub label: String,
	/// Per-database verification of each restored file, read back at its restore
	/// destination.
	pub databases: Vec<BackupDbReport>,
	/// Total rows verified readable across the restored control plane.
	pub total_rows: i64,
	/// Total on-disk size of the restored files in bytes.
	pub total_bytes: u64,
	/// Always `true` when the call returns `200`: the backup restored and every restored
	/// database opened and scanned. A failure is reported as a non-`200` instead.
	pub restorable: bool,
	/// Why the rehearsal copy could not be removed afterwards, or `null` when it was.
	/// The drill's answer stands either way; a copy left behind is a
	/// `.restore-drill-*` directory beside the backups. The backup daemon's sweep removes
	/// it once it is an hour old; where the daemon is not enabled, remove it by hand.
	pub cleanup_error: Option<String>,
}

/// Handle `POST /api/v1/storage/restore/drill?label=`: **rehearse** restoring a backup.
///
/// The Phase 7.4 drill. Restores the named backup into a throwaway directory, verifies
/// every restored database at its destination (it opens, and every row of every table
/// reads), reports what came back, and then deletes the rehearsal copy. The live store is
/// never touched and nothing is overwritten — the whole point is that an operator can
/// answer "is my backup actually restorable?" on a running system without risking it. If
/// the copy cannot be deleted, the response says so in `cleanup_error` (and the backup
/// daemon's sweep, when the daemon is enabled, removes the copy once it is an hour old).
///
/// Restoring *over* a live control plane is deliberately not offered here: the library
/// primitive refuses to clobber, and choosing a new store root is a deployment decision
/// (point `WEFT_SEGMENT_STORE_ROOT` at the restored copy and restart), not an HTTP call.
///
/// # Errors
///
/// [`StorageError::Unconfigured`] when no store is attached, [`StorageError::BadRequest`]
/// when the label is malformed (see [`BackupParams::label`] for the grammar) or names a
/// staging directory (an unfinished backup, prune or drill), [`StorageError::NotFound`]
/// when no such backup exists, [`StorageError::AlreadyExists`] when the rehearsal
/// directory is already taken by another drill, and [`StorageError::Internal`] when the
/// backup will not restore or verify — which is a failed drill, and the answer the
/// caller asked for.
pub async fn restore_drill(State(state): State<AppState>, Query(params): Query<RestoreDrillParams>) -> Result<Response, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	if !valid_backup_label(&params.label) {
		return Err(invalid_backup_label(&params.label));
	}
	if weftdb::is_staging_name(&params.label) {
		return Err(StorageError::BadRequest(format!("`{}` names an unfinished backup, prune or drill (a name starting with one of {:?}), not a backup", params.label, weftdb::STAGING_PREFIXES)));
	}
	let base = std::env::var_os("WEFT_BACKUP_DIR").map_or_else(|| store.root().join("backups"), std::path::PathBuf::from);
	let backup_dir = base.join(&params.label);
	if !backup_dir.is_dir() {
		return Err(StorageError::NotFound(format!("no backup named `{}` under {}", params.label, base.display())));
	}
	drop(store);

	// A throwaway destination beside the backups: same volume (so the rehearsal is sized
	// like the real thing) and never the live store root. Once claimed, it is cleaned up
	// on every path, success or failure.
	let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|err| StorageError::Internal(err.to_string()))?.as_millis();
	let target = base.join(format!("{RESTORE_DRILL_PREFIX}{millis}"));
	let response = drill_response(&RealFs, params.label, &backup_dir, &target).await?;
	Ok((StatusCode::OK, Json(response)).into_response())
}

/// Rehearse restoring `backup_dir` into the throwaway directory `target` through `fs`,
/// remove `target` again whatever the outcome, and build the drill's answer.
///
/// `target` is claimed first, by creating it with a call that fails if anything is
/// already there, so two drills never share a rehearsal directory. A drill that finds it
/// taken (another drill started in the same millisecond) answers `409 already_exists`
/// and leaves it alone, instead of restoring into it and then removing the other drill's
/// copy. The new directory's entry is not fsynced: the copy is removed when the drill
/// ends, and one a crash leaves behind is swept like any other stale drill.
async fn drill_response(fs: &dyn StoreFs, label: String, backup_dir: &std::path::Path, target: &std::path::Path) -> Result<RestoreDrillResponse, StorageError> {
	match fs.create_dir(target).await {
		Ok(()) => {}
		Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
			return Err(StorageError::AlreadyExists(format!("restore drill for `{label}`: its rehearsal directory {} already exists; nothing was restored into it or removed — retry the drill, which picks a fresh directory", target.display())));
		}
		Err(err) => return Err(StorageError::Internal(format!("restore drill for `{label}` could not create its rehearsal directory {}: {err}", target.display()))),
	}
	let outcome = weftdb::restore_control_plane_with(fs, backup_dir, target).await;
	// The drill's answer stands whether or not its copy can be removed, but a copy left
	// behind is disk an operator is paying for: say so instead of dropping the error.
	let cleanup_error = fs.remove_dir_all(target).await.err().map(|err| format!("could not remove the rehearsal copy {}: {err}", target.display()));
	if let Some(cleanup) = &cleanup_error {
		tracing::warn!(label = %label, error = %cleanup, "restore drill left its rehearsal copy behind; the backup daemon's sweep removes it once stale, if the daemon is enabled");
	}
	let report = outcome.map_err(|err| {
		let also = cleanup_error.as_deref().map_or_else(String::new, |cleanup| format!(" (and it {cleanup})"));
		StorageError::Internal(format!("restore drill for `{label}` FAILED: {err:#}{also}"))
	})?;

	let databases = report.restored.iter().map(|r| BackupDbReport { name: r.dest.file_name().map_or_else(|| "?".to_string(), |n| n.to_string_lossy().into_owned()), tables: r.tables, rows: r.rows, bytes: r.bytes }).collect();
	Ok(RestoreDrillResponse { label, databases, total_rows: report.total_rows(), total_bytes: report.total_bytes(), restorable: true, cleanup_error })
}

/// Query parameters for `POST /api/v1/storage/backup`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BackupParams {
	/// A subdirectory name for this snapshot under the backup root, in the grammar
	/// `[A-Za-z0-9][A-Za-z0-9._-]{0,99}` and not ending with `.`, so an API caller can never
	/// traverse out of the backup root and every platform names the directory exactly as
	/// given (Windows would strip a trailing `.`); anything else → `400`. Absent → a
	/// `manual-<unix_millis>` name is generated. The backup daemon's grammar (`backup-` and
	/// digits only) is reserved for the daemon, because backup retention counts and prunes
	/// exactly those names; a label matching it → `400`. A label already taken → `409`
	/// with code `already_exists`. No snapshot taken through this endpoint is ever pruned
	/// by retention.
	pub label: Option<String>,
	/// How each snapshot copy is verified: `source` (default) cross-checks it against a
	/// fresh read of the live control plane — the strongest check, but it assumes a
	/// **quiescent** store; `snapshot` verifies the copy on its own terms (it opens and
	/// every row is readable) without re-reading the source, which is what an **online**
	/// backup taken while ingest continues needs. Anything else → `400`.
	pub verify: Option<String>,
}

/// One control-plane database's entry in a [`BackupResponse`].
#[derive(Debug, Clone, Serialize)]
pub struct BackupDbReport {
	/// The snapshot file name (e.g. `segment_index.db`).
	pub name: String,
	/// The number of user tables whose row counts were verified equal to the source.
	pub tables: usize,
	/// The number of rows verified in this database's snapshot.
	pub rows: i64,
	/// The on-disk size of this snapshot file in bytes.
	pub bytes: u64,
}

/// Response body for `POST /api/v1/storage/backup` — the outcome of an online
/// control-plane snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct BackupResponse {
	/// The directory the four snapshot files were written to.
	pub dir: String,
	/// Per-database verified snapshot reports (`segment_index`, `metadata`,
	/// `aspect_catalog`, `catalog`).
	pub databases: Vec<BackupDbReport>,
	/// Total rows verified across all four control-plane databases.
	pub total_rows: i64,
	/// Total on-disk size of the four snapshot files in bytes.
	pub total_bytes: u64,
	/// Which verification was applied (`source` or `snapshot`), so a caller knows what
	/// `total_rows` proves: equality with the live source, or the copy's own readable
	/// contents.
	pub verify: &'static str,
}

/// Parse the `?verify=` selector into a [`VerifyMode`].
///
/// `source` (the default) is the quiescent-store cross-check; `snapshot` is the
/// concurrent-write-safe copy-only check. Returns `None` for an unknown token so the
/// caller can answer `400` rather than silently picking a mode.
fn parse_verify_mode(token: &str) -> Option<VerifyMode> {
	match token.trim().to_ascii_lowercase().as_str() {
		"source" | "source_match" | "quiescent" => Some(VerifyMode::SourceMatch),
		"snapshot" | "snapshot_only" | "online" => Some(VerifyMode::SnapshotOnly),
		_ => None,
	}
}

/// The wire token for a [`VerifyMode`], echoed in [`BackupResponse::verify`].
const fn verify_token(mode: VerifyMode) -> &'static str {
	match mode {
		VerifyMode::SourceMatch => "source",
		VerifyMode::SnapshotOnly => "snapshot",
	}
}

/// The longest backup label [`valid_backup_label`] accepts, in bytes; a label is ASCII,
/// so this is also its length in characters.
const MAX_BACKUP_LABEL_LEN: usize = 100;

/// The error for a `label` that [`valid_backup_label`] rejects.
fn invalid_backup_label(label: &str) -> StorageError {
	StorageError::BadRequest(format!("invalid backup label `{label}` — use 1 to {MAX_BACKUP_LABEL_LEN} letters, digits, '.', '_' or '-', starting with a letter or digit and not ending with '.'"))
}

/// Validate a backup `label` against the grammar frozen for 1.0:
/// `[A-Za-z0-9][A-Za-z0-9._-]{0,99}`, not ending with `.`. A label is therefore 1 to
/// [`MAX_BACKUP_LABEL_LEN`] ASCII characters naming a single subdirectory under the
/// backup root, the same one on every platform, and can never be an absolute path or a
/// `../` traversal. Both the backup and the restore-drill endpoints apply it.
///
/// The trailing-dot rule matters on Windows, which strips trailing dots from a path
/// component: `backup-123.` would otherwise pass the reserved-grammar check in
/// [`backup_store`] and still create the directory `backup-123`, which retention counts
/// and prunes. With no trailing dot or space, no platform names a label as anything but
/// itself. The first character is a letter or digit: a leading `.` would make a hidden
/// directory and could take a staging prefix (`.partial-`, `.deleting-`,
/// `.restore-drill-`), and a leading `-` reads as an option to the command-line tools an
/// operator points at the backup directory. The length bound keeps the label, and the
/// `.partial-{label}-{nonce}` and `.deleting-{label}-{nonce}` names built from it, well
/// inside every filesystem's 255-byte name limit.
fn valid_backup_label(label: &str) -> bool {
	let bytes = label.as_bytes();
	bytes.first().is_some_and(u8::is_ascii_alphanumeric) && bytes.len() <= MAX_BACKUP_LABEL_LEN && bytes.last() != Some(&b'.') && bytes.iter().all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Handle `POST /api/v1/storage/backup`: snapshot the store's four control-plane DBs.
///
/// Takes an online, consistent snapshot via Turso's `VACUUM INTO` (roadmap **Phase
/// 7.4**), verifying each copy before returning.
///
/// The snapshot lands in `<base>/<label>`, where `<base>` is `WEFT_BACKUP_DIR` if set
/// else `<store_root>/backups`, and `<label>` is the (validated) `?label=` or a generated
/// `manual-<unix_millis>` ([`manual_label`](crate::backup_daemon::manual_label)), which
/// backup retention never counts or prunes. The backup is built in a `.partial-*`
/// directory beside it and renamed into place, with a `MANIFEST.json`, only once complete
/// and durable, so a directory that already exists is rejected (`409 already_exists`). A
/// backup that fails removes its build directory again, except in one window: if the final
/// fsync of `<base>` fails after the rename, the response is a `500` although the complete
/// backup is already under its label (so a retry with the same label is a `409`). The
/// `.weftseg` measurement frames are **not** part of this backup — control plane only, per
/// the storage boundary (hard-constraint #3).
///
/// # Errors
///
/// [`StorageError::Unconfigured`] when no store is attached,
/// [`StorageError::BadRequest`] when `label` is malformed (see [`BackupParams::label`]
/// for the grammar), in the reserved `backup-<digits>` form, or reserved for a staging
/// directory (`.partial-*`, `.deleting-*`, `.restore-drill-*`),
/// [`StorageError::AlreadyExists`] when the target dir already exists, and
/// [`StorageError::Internal`] on a backup/verify failure.
pub async fn backup_store(State(state): State<AppState>, Query(params): Query<BackupParams>) -> Result<Response, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	let metrics = state.metrics().clone();
	drop(state);
	let mode = match params.verify.as_deref() {
		Some(token) => parse_verify_mode(token).ok_or_else(|| StorageError::BadRequest(format!("unknown verify mode `{token}` — use `source` (quiescent cross-check) or `snapshot` (concurrent-write-safe copy-only check)")))?,
		None => VerifyMode::default(),
	};
	let base = std::env::var_os("WEFT_BACKUP_DIR").map_or_else(|| store.root().join("backups"), std::path::PathBuf::from);
	let sub = if let Some(label) = params.label {
		if !valid_backup_label(&label) {
			return Err(invalid_backup_label(&label));
		}
		// Retention counts and prunes `backup-<digits>` directories as the daemon's
		// snapshots, so a caller-chosen label in that form could crowd genuine ones out.
		// `valid_backup_label` has refused a trailing `.`, so the label checked here is
		// the directory name on every platform.
		if crate::backup_daemon::is_generated_label(&label) {
			return Err(StorageError::BadRequest(format!("backup label `{label}` is reserved: `backup-<digits>` names are generated by the backup daemon and managed by backup retention — choose another label, or omit `label` to have one generated")));
		}
		if weftdb::is_staging_name(&label) {
			return Err(StorageError::BadRequest(format!("backup label `{label}` is reserved: names starting with one of {:?} are unfinished backups, prunes and drills, which are swept", weftdb::STAGING_PREFIXES)));
		}
		label
	} else {
		// Not the daemon's `backup-<digits>`: an API caller taking unlabelled backups must
		// not push the daemon's snapshots out of the retained set.
		let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|err| StorageError::Internal(err.to_string()))?.as_millis();
		crate::backup_daemon::manual_label(millis)
	};
	let dest = base.join(&sub);
	if dest.exists() {
		return Err(StorageError::AlreadyExists(format!("backup target already exists: {} (choose a fresh label)", dest.display())));
	}
	let backup = store.backup_control_plane_with_verify(&dest, mode).await.map_err(|err| StorageError::Internal(err.to_string()))?;
	drop(store);
	metrics.record_backup(backup.total_bytes());
	let databases = [("segment_index.db", &backup.segment_index), ("metadata.db", &backup.metadata), ("aspect_catalog.db", &backup.aspect_catalog), ("catalog.db", &backup.registry)].into_iter().map(|(name, report)| BackupDbReport { name: name.to_string(), tables: report.tables, rows: report.rows, bytes: report.bytes }).collect();
	let response = BackupResponse { dir: backup.dir.display().to_string(), databases, total_rows: backup.total_rows(), total_bytes: backup.total_bytes(), verify: verify_token(mode) };
	Ok((StatusCode::OK, Json(response)).into_response())
}

#[cfg(test)]

mod tests {

	use std::sync::Arc;

	use axum::{
		body::Body, http::{Request, StatusCode}
	};
	use tempfile::TempDir;
	use tower::ServiceExt;
	use weftdb::SegmentStore;

	use crate::{
		app_with_state, test_fs::{FsOp, RecordingFs, INJECTED_CLEANUP_FAILURE}, test_support::*, AppState
	};

	#[test]
	fn valid_backup_label_rejects_traversal_and_bad_chars() {
		use super::valid_backup_label;
		assert!(valid_backup_label("nightly"));
		assert!(valid_backup_label("2026-07-21_run.1"));
		assert!(!valid_backup_label(""));
		assert!(!valid_backup_label("."));
		assert!(!valid_backup_label(".."));
		assert!(!valid_backup_label("a/b"));
		assert!(!valid_backup_label("a\\b"));
		assert!(!valid_backup_label("a b"));
		// Windows strips a trailing dot, so `backup-1.` would create `backup-1`.
		assert!(!valid_backup_label("backup-1."));
		assert!(!valid_backup_label("nightly.."));
		assert!(!valid_backup_label(".hidden"));
		assert!(!valid_backup_label(".restore-drill-1"));
		assert!(valid_backup_label("a.b"), "a dot inside the label is fine");
		// The frozen grammar: `[A-Za-z0-9][A-Za-z0-9._-]{0,99}`, not ending with `.`.
		assert!(valid_backup_label("A"), "one letter is a label");
		assert!(valid_backup_label("7"), "one digit is a label");
		assert!(valid_backup_label("nightly-") && valid_backup_label("nightly_"), "only a trailing `.` is refused");
		assert!(valid_backup_label("backup-123"), "the grammar admits a daemon name; the backup endpoint reserves it separately");
		assert!(!valid_backup_label("-nightly"), "a label starts with a letter or digit, so it never reads as a command-line option");
		assert!(!valid_backup_label("_nightly"));
		assert!(!valid_backup_label(".partial-x"));
		assert!(valid_backup_label(&"a".repeat(100)), "100 characters is the limit");
		assert!(!valid_backup_label(&"a".repeat(101)), "101 characters is one too many");
		assert!(!valid_backup_label("café"), "ASCII only");
		assert!(!valid_backup_label("a:b") && !valid_backup_label("a\0b") && !valid_backup_label("a\tb"));
	}

	#[tokio::test]
	async fn backup_endpoint_snapshots_control_plane_and_guards_label() {
		let dir = TempDir::new().unwrap();
		let sc = weft_physical_type::AspectSchema::new(weft_physical_type::PhysicalType::F64, "0".parse().unwrap(), weft_physical_type::TimeUnit::Seconds);
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
		store.declare("price", &sc).await.expect("declares");
		store.seal("price", &sc, &[0_i64, 10, 20], &["1".parse().unwrap(), "2".parse().unwrap(), "3".parse().unwrap()]).await.expect("seals");

		// One shared state so the /metrics counter reflects the backup below.
		let state = AppState::new().with_store(store.clone());

		// A traversal label is rejected before touching disk.
		let router = app_with_state(state.clone());
		let bad = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/backup?label=..").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(bad.status(), StatusCode::BAD_REQUEST, "traversal label rejected");

		// A valid label snapshots all four control-plane DBs under <root>/backups/<label>.
		let router = app_with_state(state.clone());
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/backup?label=nightly").body(Body::empty()).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert_eq!(status, StatusCode::OK, "body: {json}");
		assert_eq!(json["databases"].as_array().unwrap().len(), 4, "four control-plane DBs snapshotted");
		assert!(json["total_rows"].as_i64().unwrap() >= 2, "at least the seal's index + rollup rows");
		assert!(json["total_bytes"].as_u64().unwrap() > 0, "the snapshot files have a non-zero footprint");
		assert!(json["databases"][0]["bytes"].as_u64().unwrap() > 0, "each snapshot file reports its size");
		let backup_dir = dir.path().join("backups").join("nightly");
		for name in ["segment_index.db", "metadata.db", "aspect_catalog.db", "catalog.db"] {
			assert!(backup_dir.join(name).exists(), "{name} written to disk");
		}

		// Re-using the same label collides with the existing dir → 409 `already_exists`
		// (a backup is published under a fresh name), and the existing backup is kept.
		let (status, body) = post_empty(app_with_state(state.clone()), "/api/v1/storage/backup?label=nightly").await;
		assert_eq!(status, StatusCode::CONFLICT, "an existing backup dir is a conflict; body: {body}");
		assert_eq!(body["code"], "already_exists", "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("already exists"), "body: {body}");
		assert!(backup_dir.join(weftdb::BACKUP_MANIFEST).exists(), "the existing backup is untouched");

		// /metrics reflects the one successful snapshot (the failed dup did not bump it).
		let router = app_with_state(state.clone());
		let metrics = router.oneshot(Request::builder().method("GET").uri("/metrics").body(Body::empty()).unwrap()).await.unwrap();
		let mbytes = axum::body::to_bytes(metrics.into_body(), usize::MAX).await.unwrap();
		let mtext = String::from_utf8(mbytes.to_vec()).unwrap();
		assert!(mtext.contains("weft_backup_snapshots_total 1"), "one snapshot recorded; got:\n{mtext}");
		assert!(mtext.contains("weft_backup_bytes_written_total"), "bytes-written counter present");
		assert_eq!(json["verify"], "source", "the default verification is the quiescent source cross-check");

		// The concurrent-write-safe mode verifies each copy on its own terms (no source
		// re-read) and says so in the response — what an online backup needs.
		let router = app_with_state(state.clone());
		let online = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/backup?label=online&verify=snapshot").body(Body::empty()).unwrap()).await.unwrap();
		let ostatus = online.status();
		let obytes = axum::body::to_bytes(online.into_body(), usize::MAX).await.unwrap();
		let ojson: serde_json::Value = serde_json::from_slice(&obytes).unwrap();
		assert_eq!(ostatus, StatusCode::OK, "body: {ojson}");
		assert_eq!(ojson["verify"], "snapshot", "the response says which check was applied");
		assert_eq!(ojson["databases"].as_array().unwrap().len(), 4);
		assert!(ojson["total_rows"].as_i64().unwrap() >= 2, "rows scanned out of the copies themselves");

		// An unknown verify token is a 400 rather than a silent default, and writes nothing.
		let router = app_with_state(state.clone());
		let bad_mode = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/backup?label=other&verify=maybe").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(bad_mode.status(), StatusCode::BAD_REQUEST, "unknown verify mode rejected");
		assert!(!dir.path().join("backups").join("other").exists(), "a rejected request writes nothing");

		// The Phase 7.4 drill over HTTP: rehearse restoring the snapshot we just took,
		// without touching the live store.
		let router = app_with_state(state.clone());
		let drill = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/restore/drill?label=nightly").body(Body::empty()).unwrap()).await.unwrap();
		let dstatus = drill.status();
		let dbytes = axum::body::to_bytes(drill.into_body(), usize::MAX).await.unwrap();
		let djson: serde_json::Value = serde_json::from_slice(&dbytes).unwrap();
		assert_eq!(dstatus, StatusCode::OK, "body: {djson}");
		assert_eq!(djson["restorable"], true, "the backup restored and verified");
		assert_eq!(djson["cleanup_error"], serde_json::Value::Null, "the rehearsal copy was removed");
		assert_eq!(djson["databases"].as_array().unwrap().len(), 4, "all four control-plane DBs came back");
		assert!(djson["total_rows"].as_i64().unwrap() >= 2, "rows read back out of the restored copies");
		// The rehearsal left nothing behind, and the live store is untouched.
		let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("backups")).unwrap().filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with(".restore-drill-")).collect();
		assert!(leftovers.is_empty(), "the drill cleaned up its rehearsal dir; found {leftovers:?}");
		assert!(dir.path().join("segment_index.db").exists(), "the live control plane is still in place");

		// An unknown backup is a 404, and a traversal label is a 400.
		let router = app_with_state(state.clone());
		let missing = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/restore/drill?label=no-such-backup").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(missing.status(), StatusCode::NOT_FOUND, "drilling a backup that does not exist");
		let router = app_with_state(state.clone());
		let traversal = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/restore/drill?label=..").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(traversal.status(), StatusCode::BAD_REQUEST, "traversal label rejected on the drill too");
		drop(store);
	}

	/// The regression for `drill-dir-leak`: the drill discarded its cleanup error, so a
	/// rehearsal copy it could not remove was left on disk with nobody told.
	#[tokio::test]
	async fn a_failed_drill_cleanup_is_reported_in_the_response() {
		let dir = TempDir::new().unwrap();
		let sc = weft_physical_type::AspectSchema::new(weft_physical_type::PhysicalType::F64, "0".parse().unwrap(), weft_physical_type::TimeUnit::Seconds);
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("price", &sc).await.expect("declares");
		let base = dir.path().join("backups");
		store.backup_control_plane_with_verify(base.join("nightly"), weftdb::VerifyMode::SnapshotOnly).await.expect("backs up");
		drop(store);

		let target = base.join(".restore-drill-1");
		let fs = RecordingFs::failing_remove_dir_all();
		let response = super::drill_response(&fs, "nightly".to_string(), &base.join("nightly"), &target).await.expect("the backup itself restores");
		assert!(response.restorable);
		let cleanup = response.cleanup_error.as_deref().expect("the failed cleanup is reported, not swallowed");
		assert!(cleanup.contains(INJECTED_CLEANUP_FAILURE) && cleanup.contains(".restore-drill-1"), "names the cause and the leftover: {cleanup}");
		assert!(fs.ops().contains(&FsOp::RemoveDirAll(target.clone())), "the drill tried to remove its copy");
		assert_eq!(serde_json::to_value(&response).unwrap()["cleanup_error"], serde_json::json!(cleanup), "it is in the JSON body");
		assert!(target.exists(), "the copy really was left behind, for the sweep");

		// A drill whose restore fails reports a failed cleanup in its error as well.
		let err = super::drill_response(&fs, "missing".to_string(), &base.join("missing"), &base.join(".restore-drill-2")).await.unwrap_err();
		let super::StorageError::Internal(message) = err else { panic!("a failed drill is an internal error") };
		assert!(message.contains("FAILED") && message.contains(INJECTED_CLEANUP_FAILURE), "{message}");
	}

	#[tokio::test]
	async fn staging_names_are_refused_as_backup_and_drill_labels() {
		let dir = TempDir::new().unwrap();
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
		let state = AppState::new().with_store(store.clone());
		for label in [".partial-backup-1-00", ".deleting-backup-1-00", ".restore-drill-1"] {
			// A backup under such a name would be swept as litter an hour later.
			let uri = format!("/api/v1/storage/backup?label={label}");
			let response = app_with_state(state.clone()).oneshot(Request::builder().method("POST").uri(&uri).body(Body::empty()).unwrap()).await.unwrap();
			assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}: a staging name is reserved");
			assert!(!dir.path().join("backups").join(label).exists(), "{uri}: nothing was written");

			// And a directory under such a name is an unfinished backup, prune or drill.
			std::fs::create_dir_all(dir.path().join("backups").join(label)).unwrap();
			let uri = format!("/api/v1/storage/restore/drill?label={label}");
			let response = app_with_state(state.clone()).oneshot(Request::builder().method("POST").uri(&uri).body(Body::empty()).unwrap()).await.unwrap();
			assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}: a staging name is never a backup");
		}
		drop(store);
	}

	/// Regression: backup retention counts and prunes `backup-<digits>` directories as the
	/// daemon's snapshots, so a caller must not be able to create one with a stamp of its
	/// choosing (a far-future one would crowd genuine snapshots out).
	#[tokio::test]
	async fn backup_endpoint_reserves_the_generated_label_grammar() {
		let dir = TempDir::new().unwrap();
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
		let state = AppState::new().with_store(store.clone());
		let backups = dir.path().join("backups");
		for label in ["backup-99999999999999", "backup-0", "backup-1753000000000"] {
			let (status, body) = post_empty(app_with_state(state.clone()), &format!("/api/v1/storage/backup?label={label}")).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "{label} must be refused; body: {body}");
			assert!(body["error"].as_str().unwrap().contains("reserved"), "body: {body}");
			assert!(!backups.join(label).exists(), "a refused label writes nothing");
		}
		// Regression: Windows strips trailing dots from a path component, so `backup-<n>.`
		// would create the reserved `backup-<n>` there while passing the reserved-name
		// check here. A trailing (or leading) dot is refused on every platform.
		for label in ["backup-1753000000000.", "backup-1753000000000..", ".backup-1753000000000"] {
			let (status, body) = post_empty(app_with_state(state.clone()), &format!("/api/v1/storage/backup?label={label}&verify=snapshot")).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "{label} must be refused; body: {body}");
			assert!(body["error"].as_str().unwrap().contains("ending with '.'"), "body: {body}");
		}
		assert!(!backups.exists() || std::fs::read_dir(&backups).unwrap().next().is_none(), "refused labels write nothing");
		// Near misses are ordinary operator labels.
		let (status, body) = post_empty(app_with_state(state.clone()), "/api/v1/storage/backup?label=backup-nightly&verify=snapshot").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		// An unlabelled backup gets a `manual-<millis>` name, outside the daemon's grammar,
		// and that snapshot can be drilled.
		let (status, body) = post_empty(app_with_state(state.clone()), "/api/v1/storage/backup?verify=snapshot").await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		let generated = std::path::Path::new(body["dir"].as_str().unwrap()).file_name().unwrap().to_string_lossy().into_owned();
		assert!(generated.strip_prefix("manual-").is_some_and(|stamp| stamp.parse::<u128>().is_ok()), "{generated}");
		assert!(!crate::backup_daemon::is_generated_label(&generated), "{generated} is not a daemon name");
		let (status, body) = post_empty(app_with_state(state.clone()), &format!("/api/v1/storage/restore/drill?label={generated}")).await;
		drop(state);
		drop(store);
		assert_eq!(status, StatusCode::OK, "a generated snapshot can still be drilled; body: {body}");
	}

	/// Regression: unlabelled API backups used to be named `backup-<now>`, which retention
	/// counts as the newest daemon snapshots, so `WEFT_BACKUP_KEEP` of them in a row got
	/// every older daemon snapshot pruned at the next tick. Retention now ignores them.
	#[tokio::test]
	async fn unlabelled_backups_never_evict_daemon_snapshots() {
		let dir = TempDir::new().unwrap();
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
		let state = AppState::new().with_store(store.clone());
		let backups = dir.path().join("backups");
		let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
		let daemon: Vec<std::path::PathBuf> = [3_000_u128, 2_000].into_iter().map(|age| backups.join(crate::backup_daemon::generated_label(now - age))).collect();
		// Complete backups (all four control-plane files), so retention counts them.
		for snapshot in &daemon {
			tokio::fs::create_dir_all(snapshot).await.unwrap();
			for file in weftdb::CONTROL_PLANE_FILES {
				tokio::fs::write(snapshot.join(file), b"x").await.unwrap();
			}
		}
		let keep = 2;
		for _ in 0..keep {
			let (status, body) = post_empty(app_with_state(state.clone()), "/api/v1/storage/backup?verify=snapshot").await;
			assert_eq!(status, StatusCode::OK, "body: {body}");
			// A fresh millisecond, so the next unlabelled name is fresh too.
			tokio::time::sleep(std::time::Duration::from_millis(5)).await;
		}
		drop(state);
		drop(store);
		let removed = crate::backup_daemon::prune_generated_backups(&backups, keep).await.unwrap();
		assert_eq!(removed, 0, "the daemon's snapshots are still the only {keep} it counts");
		for snapshot in &daemon {
			assert!(snapshot.exists(), "{} was evicted by unlabelled API backups", snapshot.display());
		}
	}

	/// SEC-1's retention-poisoning case end to end: with `keep = 2`, three attempts to plant
	/// far-future `backup-<digits>` snapshots through `?label=` all fail, so the next prune
	/// keeps both genuine daemon snapshots.
	#[tokio::test]
	async fn poisoned_labels_never_evict_daemon_snapshots() {
		let dir = TempDir::new().unwrap();
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
		let state = AppState::new().with_store(store.clone());
		let backups = dir.path().join("backups");
		let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
		let daemon: Vec<std::path::PathBuf> = [3_000_u128, 2_000].into_iter().map(|age| backups.join(crate::backup_daemon::generated_label(now - age))).collect();
		for snapshot in &daemon {
			tokio::fs::create_dir_all(snapshot).await.unwrap();
			for file in weftdb::CONTROL_PLANE_FILES {
				tokio::fs::write(snapshot.join(file), b"x").await.unwrap();
			}
		}
		for label in ["backup-99999999999999", "backup-99999999999998.", "backup-99999999999997"] {
			let (status, body) = post_empty(app_with_state(state.clone()), &format!("/api/v1/storage/backup?label={label}&verify=snapshot")).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "{label} must be refused; body: {body}");
		}
		drop(state);
		drop(store);
		let mut names: Vec<String> = std::fs::read_dir(&backups).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
		names.sort();
		let mut expected: Vec<String> = daemon.iter().map(|snapshot| snapshot.file_name().unwrap().to_string_lossy().into_owned()).collect();
		expected.sort();
		assert_eq!(names, expected, "the refused labels created nothing");
		let removed = crate::backup_daemon::prune_generated_backups(&backups, 2).await.unwrap();
		assert_eq!(removed, 0, "both genuine snapshots are kept");
		for snapshot in &daemon {
			assert!(snapshot.exists(), "{} was pruned", snapshot.display());
		}
	}

	/// SEC-1 / FRE-7b: the backup label grammar frozen for 1.0 is
	/// `[A-Za-z0-9][A-Za-z0-9._-]{0,99}`, not ending with `.`, on the backup and the drill
	/// endpoints alike. A drill still accepts a daemon name, because it only reads.
	#[tokio::test]
	async fn backup_and_drill_labels_follow_the_frozen_grammar() {
		let dir = TempDir::new().unwrap();
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
		let state = AppState::new().with_store(store.clone());
		let backups = dir.path().join("backups");
		let too_long = "a".repeat(101);
		for label in ["-nightly", "_nightly", ".partial-x", too_long.as_str()] {
			let (status, body) = post_empty(app_with_state(state.clone()), &format!("/api/v1/storage/backup?label={label}&verify=snapshot")).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "{label}: body: {body}");
			assert!(body["error"].as_str().unwrap().contains("invalid backup label"), "{label}: body: {body}");
			assert!(!backups.join(label).exists(), "{label}: a refused label writes nothing");
			// A directory under such a name is refused by the drill too, before it is looked up.
			std::fs::create_dir_all(backups.join(label)).unwrap();
			let (status, body) = post_empty(app_with_state(state.clone()), &format!("/api/v1/storage/restore/drill?label={label}")).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "{label}: the drill uses the same grammar; body: {body}");
		}
		// The longest label the grammar admits is an ordinary backup.
		let longest = "a".repeat(100);
		let (status, body) = post_empty(app_with_state(state.clone()), &format!("/api/v1/storage/backup?label={longest}&verify=snapshot")).await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		// A daemon snapshot can be drilled: the reservation guards creation only.
		store.backup_control_plane_with_verify(backups.join("backup-123"), weftdb::VerifyMode::SnapshotOnly).await.expect("backs up");
		let (status, body) = post_empty(app_with_state(state.clone()), "/api/v1/storage/restore/drill?label=backup-123").await;
		drop(state);
		drop(store);
		assert_eq!(status, StatusCode::OK, "a drill of backup-<digits> is accepted; body: {body}");
		assert_eq!(body["restorable"], true, "body: {body}");
	}

	/// Regression: a drill restored into its rehearsal directory even when that directory
	/// already existed (another drill started in the same millisecond), and then removed
	/// it, deleting the other drill's copy. It is now a `409 already_exists` that leaves
	/// the directory as it found it.
	#[tokio::test]
	async fn a_drill_whose_rehearsal_directory_exists_is_a_conflict_and_leaves_it_alone() {
		use axum::response::IntoResponse as _;

		let dir = TempDir::new().unwrap();
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		let base = dir.path().join("backups");
		store.backup_control_plane_with_verify(base.join("nightly"), weftdb::VerifyMode::SnapshotOnly).await.expect("backs up");
		drop(store);
		let target = base.join(".restore-drill-1");
		std::fs::create_dir_all(&target).unwrap();
		std::fs::write(target.join("in-use"), b"another drill's copy").unwrap();

		let fs = RecordingFs::default();
		let err = super::drill_response(&fs, "nightly".to_string(), &base.join("nightly"), &target).await.expect_err("an existing rehearsal directory is never reused");
		let response = err.into_response();
		let status = response.status();
		let body: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
		assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
		assert_eq!(body["code"], "already_exists", "body: {body}");
		assert!(target.join("in-use").exists(), "the other drill's copy is untouched");
		for name in weftdb::CONTROL_PLANE_FILES {
			assert!(!target.join(name).exists(), "nothing was restored into it: {name}");
		}
		assert!(!fs.ops().iter().any(|op| matches!(op, FsOp::RemoveDirAll(_))), "nothing was removed: {:?}", fs.ops());
	}

	#[test]
	fn verify_mode_tokens_round_trip_and_reject_junk() {
		use weftdb::VerifyMode;

		use super::{parse_verify_mode, verify_token};
		assert_eq!(parse_verify_mode("source"), Some(VerifyMode::SourceMatch));
		assert_eq!(parse_verify_mode("QUIESCENT"), Some(VerifyMode::SourceMatch));
		assert_eq!(parse_verify_mode(" snapshot "), Some(VerifyMode::SnapshotOnly));
		assert_eq!(parse_verify_mode("online"), Some(VerifyMode::SnapshotOnly));
		assert_eq!(parse_verify_mode("yes"), None, "an unknown token must 400, not silently pick a mode");
		assert_eq!(verify_token(VerifyMode::SourceMatch), "source");
		assert_eq!(verify_token(VerifyMode::SnapshotOnly), "snapshot");
	}
}
