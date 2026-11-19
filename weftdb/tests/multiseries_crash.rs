//! **Multi-series crash cell** (TAG-12, issue #341).
//!
//! # Invariant
//!
//! After an abort at [`FaultPoint::SFrameWritten`] during an interleaved write across 8
//! series (aspects), every aspect's committed frames are consistent, independently
//! recoverable, and readable. No aspect shows a value from a frame that never committed,
//! and no aspect loses a value whose frame committed. Each aspect's data matches what a
//! single-threaded replay of its committed frames would produce.
//!
//! This is a process-kill test: the child re-executes this binary, writes a first round
//! of measurements to every aspect, arms the `SFrameWritten` fault point, and then writes
//! to one more aspect — which aborts. The parent then reopens the store cold and verifies
//! per-aspect consistency.

#![cfg(feature = "fault-injection")]

use std::str::FromStr;

use anyhow::{Context, Result};
use bigdecimal::BigDecimal;
use chrono::{TimeZone, Utc};
use serial_test::serial;
use uuid::Uuid;
use weftdb::{
	clear_connection_cache_by_name, database::traits::DatabaseStructure, durable::fault::{self, FaultAction, FaultPoint, FAULT_ENV}, Database, DatasetId, InputMeasurement, Outputs
};

/// The env var naming the test database. The child reads this to know which database to
/// reopen.
const CHILD_ENV: &str = "WEFT_MULTISERIES_CRASH_CHILD";

/// The number of aspects (series) the test writes across.
const N_SERIES: usize = 8;

/// Rows per batch per aspect.
const BATCH_ROWS: usize = 3;

/// The base timestamps for each aspect's first and second batch. Aspects are spaced
/// far enough apart that a full range read on each is unambiguous.
fn batch_timestamps(aspect_index: usize, batch: usize) -> [i64; BATCH_ROWS] {
	let base = (aspect_index * 10_000 + batch * 1000) as i64;
	[base, base + 1, base + 2]
}

/// The value for a given aspect's row in a given batch — each is unique and
/// deterministically derivable.
fn batch_value(aspect_index: usize, batch: usize, row: usize) -> BigDecimal {
	BigDecimal::from_str(&format!("{}.{:02}{:02}", aspect_index * 100 + batch * 10 + row, aspect_index, batch * 3 + row,)).expect("valid BigDecimal")
}

/// The planned measurements for one batch of one aspect.
fn planned_batch(aspect_index: usize, batch: usize) -> Vec<InputMeasurement> {
	let ts = batch_timestamps(aspect_index, batch);
	let mut measurements = Vec::with_capacity(BATCH_ROWS);
	for (i, t) in ts.iter().enumerate() {
		let dt = Utc.timestamp_millis_opt(*t).single().expect("valid timestamp");
		let value = batch_value(aspect_index, batch, i);
		measurements.push(InputMeasurement::new(dt, value));
	}
	measurements
}

// ── Helper shared by parent and child ──────────────────────────────────────

/// Keep the child's deliberate abort from dumping core, exactly as the fault
/// module's own abort test does.
fn suppress_core_dump() {
	#[cfg(unix)]
	{
		let none = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
		// SAFETY: setrlimit only reads the struct, for the duration of the call.
		unsafe { libc::setrlimit(libc::RLIMIT_CORE, &raw const none) };
	}
	#[cfg(target_os = "linux")]
	{
		let not_dumpable: libc::c_ulong = 0;
		// SAFETY: PR_SET_DUMPABLE takes one integer and changes only this process's
		// dumpable flag.
		unsafe { libc::prctl(libc::PR_SET_DUMPABLE, not_dumpable) };
	}
}

/// Release every in-process handle to `db_name`, so the next
/// [`Database::existing`] is a **cold** open.
async fn release_database(id: weftdb::DatabaseId, db_name: &str) {
	weftdb::DATABASES.lock().await.remove(&id);
	clear_connection_cache_by_name(db_name).await;
}

// ── Child (re-executed in the subprocess) ──────────────────────────────────

/// The body the re-executed child runs: open the database named by [`CHILD_ENV`],
/// write a first round of measurements to every aspect, arm the `SFrameWritten` fault
/// point, then write a second round to aspect 0 — which aborts. In a normal run the
/// variable is unset and this does nothing.
#[tokio::test]
async fn child() -> Result<()> {
	let Some(name) = std::env::var_os(CHILD_ENV) else {
		return Ok(());
	};
	suppress_core_dump();

	let name = name.into_string().map_err(|raw| anyhow::anyhow!("{raw:?} is not UTF-8"))?;
	let db = Database::existing(&name).await?;

	// Write round 1 to every aspect (3 rows each). No fault is armed, so all
	// batches commit.
	for i in 0..N_SERIES {
		let aspect_name = format!("series_{i}");
		let aspect = db.get_aspect_by_name(&aspect_name).await?;
		let measurements = planned_batch(i, 0);
		db.batch_capture_measurements(aspect.id(), DatasetId::new(), measurements).await?;
	}

	// Arm the fault: the next frame write aborts.
	let _armed = fault::arm(FaultPoint::SFrameWritten, FaultAction::Abort);

	// This write triggers the abort at SFrameWritten — the frame reaches disk
	// but never commits.
	let aspect0 = db.get_aspect_by_name("series_0").await?;
	let measurements = planned_batch(0, 1);
	db.batch_capture_measurements(aspect0.id(), DatasetId::new(), measurements).await?;

	panic!("{FAULT_ENV} did not abort batch_capture_measurements");
}

// ── Parent (the test proper) ───────────────────────────────────────────────

/// A process crash (abort) at [`FaultPoint::SFrameWritten`] mid-write across 8
/// series leaves every series consistent with its committed frames.
///
/// After the crash:
/// - Every aspect has the first batch's three rows committed.
/// - No aspect has the second batch's data (the abort at SFrameWritten fires
///   before the commit, so the second batch never reached the index).
/// - Reads per aspect return exactly the planned first-batch values, with no
///   extras, no missing rows, and no cross-contamination.
#[serial]
#[tokio::test]
async fn a_multiseries_crash_at_sframe_written_is_recoverable() -> Result<()> {
	let temp_dir = tempfile::tempdir()?;
	let data_dir = temp_dir.path().to_str().context("temp data dir is not valid UTF-8")?;
	std::env::set_var("WEFT_DATA_DIR", data_dir);
	// Default storage is already seal-backed since #768; set explicitly to
	// avoid depending on a future default change.
	std::env::set_var("WEFT_LEGACY_DEFAULT_STORAGE", "segments");

	// ── Set up ────────────────────────────────────────────────────────────
	let name = format!("multiseries_crash_{}", Uuid::new_v4().simple());
	let db = Database::new(&name).await?;
	let subject = db.observe_subject("crash_subject").await?;

	let mut aspect_ids = Vec::with_capacity(N_SERIES);
	for i in 0..N_SERIES {
		let aspect_name = format!("series_{i}");
		let aspect = db.track_aspect(&subject.id(), &aspect_name, &splimes::Resolution::Milliseconds, None).await?;
		aspect_ids.push(aspect.id());
	}

	let db_id = db.id();
	drop(db);
	release_database(db_id, &name).await;

	// ── Spawn child ───────────────────────────────────────────────────────
	let output = std::process::Command::new(std::env::current_exe()?)
		.args(["multiseries_crash::child", "--exact", "--nocapture", "--test-threads=1"])
		.env(CHILD_ENV, &name)
		.env("WEFT_DATA_DIR", data_dir)
		.env("WEFT_LEGACY_DEFAULT_STORAGE", "segments")
		// No WEFT_FAULT — the child arms the fault mid-way.
		.output()?;

	assert!(!output.status.success(), "the child aborted: {output:?}");
	#[cfg(unix)]
	{
		use std::os::unix::process::ExitStatusExt;
		assert_eq!(output.status.signal(), Some(6), "killed by SIGABRT: {output:?}");
	}
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(stderr.contains("aborting at fault point S-frame-written"), "the child reached SFrameWritten: {output:?}");

	// ── Reopen and verify ─────────────────────────────────────────────────
	let db = Database::existing(&name).await?;

	for i in 0..N_SERIES {
		let aspect_name = format!("series_{i}");
		let aspect = db.get_aspect_by_name(&aspect_name).await?;
		let aspect_id = aspect_ids[i];

		// Read all measurements for this aspect.
		let measurements: Vec<_> = db.get_raw_measurements(&aspect_id, None, None, 1000, 0).await?.collect::<Vec<_>>().await;

		// Every aspect must have exactly the first batch: 3 rows, no
		// cross-contamination, no phantom data from the aborting second batch.
		assert_eq!(measurements.len(), BATCH_ROWS, "aspect {i} ({aspect_name}): got {} rows, expected {BATCH_ROWS} (the abort fired before the second batch committed)", measurements.len(),);

		// Verify each row matches the planned first-batch data.
		let planned = planned_batch(i, 0);
		// Sort the measurements by timestamp for deterministic comparison.
		let mut sorted = measurements.into_iter().collect::<std::result::Result<Vec<_>, _>>()?;
		sorted.sort_by_key(|m| m.timestamp());

		for (row_index, measurement) in sorted.iter().enumerate() {
			let expected_ts = batch_timestamps(i, 0)[row_index];
			let expected_value = batch_value(i, 0, row_index);

			assert_eq!(measurement.timestamp().timestamp_millis(), expected_ts, "aspect {i} ({aspect_name}), row {row_index}: timestamp mismatch");
			assert_eq!(measurement.value(), &expected_value, "aspect {i} ({aspect_name}), row {row_index}: value mismatch");
		}
	}

	let released_id = db.id();
	drop(db);
	release_database(released_id, &name).await;

	Ok(())
}
