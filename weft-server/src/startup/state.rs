//! Startup phase 3 — the router state, which in practice means the segment store.
//!
//! A server without `WEFT_SEGMENT_STORE_ROOT` is a legitimate server: it serves
//! the stateless interpolation/downsample API and answers `503` on the storage
//! endpoints. Readiness (`GET /ready`) reports which mode is active, so nothing
//! here needs to pretend a store exists.

use std::sync::Arc;

use weft_server::{store_open, AppState};

/// Environment variable naming the segment-store root directory (optional).
const STORE_ROOT_ENV: &str = "WEFT_SEGMENT_STORE_ROOT";

/// Build the router state, opening a segment store when `WEFT_SEGMENT_STORE_ROOT`
/// is set and logging which mode the server runs in.
///
/// The store is opened by [`store_open::open_from_env`]: with the options the
/// environment describes, and under `WEFT_ON_AMBIGUOUS_COMMIT=exit` with a task that exits
/// the process once it is write-poisoned.
///
/// # Errors
///
/// Propagates a failure to open the segment store at the configured root.
pub(crate) async fn build() -> anyhow::Result<AppState> {
	match std::env::var(STORE_ROOT_ENV) {
		Ok(root) => {
			let store = Arc::new(store_open::open_from_env(&root).await?);
			let report = store.open_report();
			let how = if report.created_new { "created".to_string() } else { report.migrated_from.map_or_else(|| "opened".to_string(), |layout| format!("migrated from layout {layout}")) };
			println!("segment store: {how} at {root} (storage endpoints live; store {}, layout {})", store.store_uuid(), store.store_format().layout_version);
			if report.recovery_report_only {
				println!("segment store: its layout is newer than this build's; it opened without migrating");
			}
			Ok(AppState::new().with_store(store))
		}
		Err(_) => {
			println!("segment store: not configured (set {STORE_ROOT_ENV} to enable the /api/v1/storage endpoints)");
			Ok(AppState::new())
		}
	}
}
