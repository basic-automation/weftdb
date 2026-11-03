//! Startup phase 3 — the router state, which in practice means the segment store.
//!
//! A server without `WEFT_SEGMENT_STORE_ROOT` is a legitimate server: it serves
//! the stateless interpolation/downsample API and answers `503` on the storage
//! endpoints. Readiness (`GET /ready`) reports which mode is active, so nothing
//! here needs to pretend a store exists.

use std::sync::Arc;

use weft_server::{audit, store_open, AppState, AuditLog, AUDIT_FILE_ENV};

/// Environment variable naming the segment-store root directory (optional).
pub(crate) const STORE_ROOT_ENV: &str = "WEFT_SEGMENT_STORE_ROOT";

/// Build the router state, opening a segment store when `WEFT_SEGMENT_STORE_ROOT`
/// is set and logging which mode the server runs in.
///
/// The store is opened by [`store_open::open_from_env`]: with the options the
/// environment describes, and under `WEFT_ON_AMBIGUOUS_COMMIT=exit` with a task that exits
/// the process once it is write-poisoned.
///
/// The audit log (SEC-9) opens here too, after the store root has passed
/// its filesystem check: its default home is inside that root, so opening
/// it any earlier would either write into a root that is about to be
/// refused or fail with a message that hides the refusal that matters.
///
/// # Errors
///
/// Propagates a failure to open the segment store at the configured root.
pub(crate) async fn build() -> anyhow::Result<AppState> {
	let (state, store_root) = match std::env::var(STORE_ROOT_ENV) {
		Ok(root) => {
			// PAC-5b's store filesystem check: a root the filesystem cannot
			// hold a store on is refused here, exit 78 (a filesystem refusal,
			// plan C7), before the open writes anything into it.
			if let Err(problem) = store_open::check_store_root(std::path::Path::new(&root)) {
				eprintln!("weft-server: {problem}");
				std::process::exit(weft_server::settings::CONFIG_EXIT_CODE);
			}
			let store = Arc::new(store_open::open_from_env(&root).await?);
			let report = store.open_report();
			let how = if report.created_new { "created".to_string() } else { report.migrated_from.map_or_else(|| "opened".to_string(), |layout| format!("migrated from layout {layout}")) };
			println!("segment store: {how} at {root} (storage endpoints live; store {}, layout {})", store.store_uuid(), store.store_format().layout_version);
			if report.recovery_report_only {
				println!("segment store: its layout is newer than this build's; it opened without migrating");
			}
			(AppState::new().with_store(store), Some(root))
		}
		Err(_) => {
			println!("segment store: not configured (set {STORE_ROOT_ENV} to enable the /api/v1/storage endpoints)");
			(AppState::new(), None)
		}
	};
	Ok(state.with_audit_log(open_audit_log(store_root.as_deref())))
}

/// Open the fsynced audit log (SEC-9, [`AuditLog`]): the path
/// `WEFT_AUDIT_FILE` names, else the store root's `audit.jsonl`, else
/// `./weft-audit.jsonl` ([`audit::resolve_path`]). A log that cannot be
/// opened — or one left with a torn final record by a crash — refuses
/// the start with exit 78, the filesystem-refusal class (plan C7): the
/// server never comes up unaudited by accident.
fn open_audit_log(store_root: Option<&str>) -> Arc<AuditLog> {
	let path = audit::resolve_path(std::env::var(AUDIT_FILE_ENV).ok().as_deref(), store_root);
	match AuditLog::open(&path) {
		Ok(log) => {
			println!("audit: fsynced append-only JSONL at {} (set {AUDIT_FILE_ENV} to change); audited operations fail closed when it cannot be written (SEC-9)", log.path().display());
			Arc::new(log)
		}
		Err(problem) => {
			eprintln!("weft-server: {problem}");
			std::process::exit(weft_server::settings::CONFIG_EXIT_CODE);
		}
	}
}
