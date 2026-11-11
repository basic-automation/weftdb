//! Change log for seal-backed aspects (slice D-S20): every seal-store
//! commit records exactly one `segment_changes` row — its epoch, time
//! span and row count — in the commit's own index transaction, and the
//! per-timestamp unbatched queue is never touched for a seal-backed
//! aspect. A consumer reads the rows and acknowledges them by epoch.

use std::str::FromStr;

use anyhow::Result;
use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use serial_test::serial;
use tempfile::TempDir;
use uuid::Uuid;
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weftdb::{
	database::traits::{AspectStructure, DatabaseStructure, Inputs, Outputs}, Database, DatasetId, IngestBatch, InputMeasurement, SegmentStore
};

fn bd(s: &str) -> BigDecimal {
	BigDecimal::from_str(s).expect("decimal")
}

#[tokio::test]
async fn seal_and_ingest_each_record_one_change_and_ack_deletes_by_epoch() -> Result<()> {
	let dir = TempDir::new()?;
	let store = SegmentStore::open(dir.path()).await?;
	store.declare("price", &AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)).await?;

	// A plain seal is one commit: one change row, carrying its span.
	store.seal_declared("price", &[100, 110, 120], &[bd("1.5"), bd("2.5"), bd("3.5")]).await?;
	let changes = store.changes_since("price", 0).await?;
	assert_eq!(changes.len(), 1, "one seal, one change row");
	assert_eq!(changes[0].min_ts, Some(100));
	assert_eq!(changes[0].max_ts, Some(120));
	assert_eq!(changes[0].row_count, 3);
	let first_epoch = changes[0].epoch;
	assert!(first_epoch > 0);

	// An ingest batch cut into one segment is still one commit.
	let receipt = store.ingest("price", &IngestBatch::dense("k1", vec![200, 210], vec![bd("4.5"), bd("5.5")])).await?;
	assert!(!receipt.duplicate);
	let changes = store.changes_since("price", 0).await?;
	assert_eq!(changes.len(), 2, "one ingest, one more change row");
	assert_eq!(changes[1].min_ts, Some(200));
	assert_eq!(changes[1].max_ts, Some(210));
	assert_eq!(changes[1].row_count, 2);
	assert!(changes[1].epoch > first_epoch, "epochs increase per commit");

	// A replayed ingest commits nothing, so it records nothing.
	let replay = store.ingest("price", &IngestBatch::dense("k1", vec![200, 210], vec![bd("4.5"), bd("5.5")])).await?;
	assert!(replay.duplicate);
	assert_eq!(store.changes_since("price", 0).await?.len(), 2, "a replay records no change");

	// Reading since an epoch returns only later changes.
	let later = store.changes_since("price", first_epoch).await?;
	assert_eq!(later.len(), 1);
	assert_eq!(later[0].epoch, changes[1].epoch);

	// Acknowledging through an epoch deletes exactly those rows.
	store.ack_changes("price", first_epoch).await?;
	let remaining = store.changes_since("price", 0).await?;
	assert_eq!(remaining.len(), 1);
	assert_eq!(remaining[0].epoch, changes[1].epoch);
	store.ack_changes("price", changes[1].epoch).await?;
	assert!(store.changes_since("price", 0).await?.is_empty());
	Ok(())
}

async fn setup() -> Result<(TempDir, Database)> {
	let temp_dir = tempfile::tempdir()?;
	std::env::set_var("WEFT_DATA_DIR", temp_dir.path().to_str().unwrap());
	let db_name = format!("change_db_{}", Uuid::new_v4());
	let db = Database::new(&db_name).await?;
	Ok((temp_dir, db))
}

#[serial]
#[tokio::test]
async fn seal_backed_ingest_feeds_the_change_log_not_the_queue() -> Result<()> {
	std::env::set_var("WEFT_LEGACY_DEFAULT_STORAGE", "segments");
	let (_dir, db) = setup().await?;
	let subject = db.observe_subject("s").await?;
	let aspect = db.track_aspect(&subject.id(), "sealed", &splimes::Resolution::Milliseconds, None).await?;
	let aspect_id = aspect.id();
	assert!(db.is_seal_backed(&aspect_id).await?);

	let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
	let dataset = DatasetId::new();
	let rows: Vec<InputMeasurement> = (0..5).map(|i| InputMeasurement::new(base + Duration::seconds(i), bd(&format!("{}.5", i + 1)))).collect();
	db.batch_capture_measurements(aspect_id, dataset, rows.clone()).await?;

	// The queue stays empty; the change log holds one row for the call.
	assert_eq!(db.count_unbatched_measurements(&aspect_id).await?, 0, "seal-backed ingest never queues per-timestamp entries");
	assert!(db.get_unbatched_entries(&aspect_id).await?.is_empty());
	let changes = db.seal_changes(&aspect_id).await?;
	assert_eq!(changes.len(), 1, "one batch call, one change row");
	assert_eq!(changes[0].row_count, 5);
	assert_eq!(changes[0].min_ts, Some(base.timestamp_millis()));
	assert_eq!(changes[0].max_ts, Some((base + Duration::seconds(4)).timestamp_millis()));

	// A retried call replays the commit: no new change row.
	db.batch_capture_measurements(aspect_id, dataset, rows).await?;
	assert_eq!(db.seal_changes(&aspect_id).await?.len(), 1, "a replay records no change");

	// A single capture is another commit and another row.
	db.capture_measurement(&aspect_id, &dataset, &InputMeasurement::new(base + Duration::seconds(10), bd("99.25"))).await?;
	let changes = db.seal_changes(&aspect_id).await?;
	assert_eq!(changes.len(), 2);
	assert_eq!(changes[1].row_count, 1);
	assert_eq!(db.count_unbatched_measurements(&aspect_id).await?, 0);

	// Acknowledgement drains the log through the epoch.
	db.ack_seal_changes(&aspect_id, changes[0].epoch).await?;
	let remaining = db.seal_changes(&aspect_id).await?;
	assert_eq!(remaining.len(), 1);
	assert_eq!(remaining[0].epoch, changes[1].epoch);

	std::env::remove_var("WEFT_LEGACY_DEFAULT_STORAGE");
	Ok(())
}

#[serial]
#[tokio::test]
async fn rows_mode_ingest_still_feeds_the_queue_and_no_change_log() -> Result<()> {
	std::env::remove_var("WEFT_LEGACY_DEFAULT_STORAGE");
	let (_dir, db) = setup().await?;
	let subject = db.observe_subject("s").await?;
	let aspect = db.track_aspect(&subject.id(), "plain", &splimes::Resolution::Milliseconds, None).await?;
	let aspect_id = aspect.id();
	assert!(!db.is_seal_backed(&aspect_id).await?);

	// A consumer has read the queue (captures queue only once one has).
	assert_eq!(db.count_unbatched_measurements(&aspect_id).await?, 0);
	let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
	let rows: Vec<InputMeasurement> = (0..3).map(|i| InputMeasurement::new(base + Duration::seconds(i), bd("1.5"))).collect();
	db.batch_capture_measurements(aspect_id, DatasetId::new(), rows).await?;

	assert_eq!(db.count_unbatched_measurements(&aspect_id).await?, 3, "rows mode keeps the per-timestamp queue");
	assert!(db.seal_changes(&aspect_id).await?.is_empty(), "rows mode has no change log");
	Ok(())
}
