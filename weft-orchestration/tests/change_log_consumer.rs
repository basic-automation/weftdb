//! The change-log consumer for seal-backed aspects (slice D-S20): a
//! seal-backed aspect's ingest feeds the seal store's `segment_changes`
//! log, never the per-timestamp queue, and
//! `build_incremental_unprocessed_queue` batches the windows the log's
//! spans touch and acknowledges the rows it batched.

use std::str::FromStr;

use anyhow::Result;
use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use serial_test::serial;
use splimes::{Resolution, Spline};
use uuid::Uuid;
use weft_orchestration::batch_utils::build_incremental_unprocessed_queue;
use weftdb::{
	database::traits::{AspectStructure, DatabaseStructure, Inputs, Outputs}, Database, DatasetId, InputMeasurement
};

const BATCH_SIZE: usize = 5;

fn bd(s: &str) -> BigDecimal {
	BigDecimal::from_str(s).expect("decimal")
}

fn minutes(range: std::ops::Range<i64>) -> Vec<InputMeasurement> {
	let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
	range.map(|m| InputMeasurement::new(base + Duration::minutes(m), bd(&format!("{m}.5")))).collect()
}

#[serial]
#[tokio::test]
async fn a_seal_backed_aspect_is_batched_from_its_change_log() -> Result<()> {
	let dir = tempfile::tempdir()?;
	std::env::set_var("WEFT_DATA_DIR", dir.path().to_str().unwrap());
	std::env::set_var("WEFT_LEGACY_DEFAULT_STORAGE", "segments");
	let name = format!("change_log_{}", Uuid::new_v4().simple());
	let db = Database::new(&name).await?;
	let subject = db.observe_subject("subject").await?;
	let aspect = db.track_aspect(&subject.id(), "aspect", &Resolution::Minutes, None).await?.id();
	assert!(db.is_seal_backed(&aspect).await?);

	// First ingest: the queue stays empty and one change row appears.
	db.batch_capture_measurements(aspect, DatasetId::new(), minutes(0..12)).await?;
	assert_eq!(db.count_unbatched_measurements(&aspect).await?, 0, "seal-backed ingest never queues");
	assert_eq!(db.seal_changes(&aspect).await?.len(), 1);

	// First consumer run: batches are built and the log is drained.
	build_incremental_unprocessed_queue(&db, &aspect, &Resolution::Minutes, &Spline::Linear, BATCH_SIZE).await?;
	let first_count = db.count_unprocessed_batches(&aspect).await?;
	assert_eq!(first_count, 8, "12 points at batch size 5 make 8 sliding batches");
	assert!(db.seal_changes(&aspect).await?.is_empty(), "the run acknowledged the change it batched");

	// A second ingest is a second change row; the incremental run
	// batches its windows.
	db.batch_capture_measurements(aspect, DatasetId::new(), minutes(12..20)).await?;
	assert_eq!(db.seal_changes(&aspect).await?.len(), 1);
	build_incremental_unprocessed_queue(&db, &aspect, &Resolution::Minutes, &Spline::Linear, BATCH_SIZE).await?;
	let second_count = db.count_unprocessed_batches(&aspect).await?;
	// 15, not 16: the window starting at minute 8 (points 8..=12) is
	// flagged by neither consumer — the per-timestamp formula a queued
	// timestamp at minute 12 flags starts at minute 9 (a window's end is
	// exclusive in `calculate_affected_windows`), and the change-log
	// span formula reproduces that exactly. The window is built by the
	// next full rebuild, as under the queue.
	assert_eq!(second_count, 15, "the incremental run adds the 7 windows the new span flags");
	// The change stays logged: its span's tail windows end past the
	// stored data (minute 19 is the last point), exactly as the queue
	// keeps the minute-19 entry queued. It drains once later data makes
	// those windows buildable.
	assert_eq!(db.seal_changes(&aspect).await?.len(), 1, "a change whose tail windows are past the data stays logged");

	// A third ingest extends the data; the next run builds the second
	// change's tail windows and acknowledges it, leaving only the third.
	db.batch_capture_measurements(aspect, DatasetId::new(), minutes(20..28)).await?;
	let pending = db.seal_changes(&aspect).await?;
	assert_eq!(pending.len(), 2);
	build_incremental_unprocessed_queue(&db, &aspect, &Resolution::Minutes, &Spline::Linear, BATCH_SIZE).await?;
	let remaining = db.seal_changes(&aspect).await?;
	assert_eq!(remaining.len(), 1, "the second change drained, the third is the new tail");
	assert_eq!(remaining[0].epoch, pending[1].epoch);
	assert!(db.count_unprocessed_batches(&aspect).await? > second_count);

	std::env::remove_var("WEFT_LEGACY_DEFAULT_STORAGE");
	Ok(())
}

/// Extraction drains both batch tables, so empty tables after a run do not
/// mean the aspect was never batched. The consumer used to treat them so and
/// rebuilt every window of the whole history on each run (the fanctl soak's
/// prepare step grew about 3 MB of transient heap per virtual hour from it);
/// the batching marker keeps those runs incremental, and clearing the batches
/// resets it.
#[serial]
#[tokio::test]
async fn a_drained_aspect_is_not_rebuilt_from_its_whole_history() -> Result<()> {
	let dir = tempfile::tempdir()?;
	std::env::set_var("WEFT_DATA_DIR", dir.path().to_str().unwrap());
	std::env::set_var("WEFT_LEGACY_DEFAULT_STORAGE", "segments");
	let name = format!("drained_{}", Uuid::new_v4().simple());
	let db = Database::new(&name).await?;
	let subject = db.observe_subject("subject").await?;
	let aspect = db.track_aspect(&subject.id(), "aspect", &Resolution::Minutes, None).await?.id();
	assert!(!db.batching_started(&aspect).await?, "a new aspect has not been batched");
	assert!(weft_orchestration::batch_utils::is_first_run(&db, &aspect).await?);

	db.batch_capture_measurements(aspect, DatasetId::new(), minutes(0..12)).await?;
	build_incremental_unprocessed_queue(&db, &aspect, &Resolution::Minutes, &Spline::Linear, BATCH_SIZE).await?;
	assert_eq!(db.count_unprocessed_batches(&aspect).await?, 8);
	assert!(db.batching_started(&aspect).await?, "storing the first batches sets the marker");

	// Drain the batches the way consumption does, without a clear.
	db.cleanup_unprocessed_batches(&aspect, Utc::now() + Duration::hours(1)).await?;
	assert_eq!((db.count_unprocessed_batches(&aspect).await?, db.count_processed_batches(&aspect).await?), (0, 0));
	assert!(!weft_orchestration::batch_utils::is_first_run(&db, &aspect).await?, "empty tables on a batched aspect are not a first run");
	build_incremental_unprocessed_queue(&db, &aspect, &Resolution::Minutes, &Spline::Linear, BATCH_SIZE).await?;
	assert_eq!(db.count_unprocessed_batches(&aspect).await?, 0, "with nothing new, the run rebuilds nothing");

	// New data is batched incrementally: only the windows its span flags.
	db.batch_capture_measurements(aspect, DatasetId::new(), minutes(12..20)).await?;
	build_incremental_unprocessed_queue(&db, &aspect, &Resolution::Minutes, &Spline::Linear, BATCH_SIZE).await?;
	assert_eq!(db.count_unprocessed_batches(&aspect).await?, 7, "the 7 windows the new span flags, not all 16 of the history");

	// Clearing is an explicit reset: the next run is a first run again.
	db.clear_unprocessed_batches(&aspect).await?;
	assert!(!db.batching_started(&aspect).await?, "a clear resets the marker");
	build_incremental_unprocessed_queue(&db, &aspect, &Resolution::Minutes, &Spline::Linear, BATCH_SIZE).await?;
	assert_eq!(db.count_unprocessed_batches(&aspect).await?, 16, "after a clear the whole history is rebuilt: 20 points make 16 windows");
	assert!(db.batching_started(&aspect).await?);

	std::env::remove_var("WEFT_LEGACY_DEFAULT_STORAGE");
	Ok(())
}
