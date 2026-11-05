#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]
//! Seal-backed legacy aspects (slice D-S19): an aspect created while
//! `WEFT_LEGACY_DEFAULT_STORAGE=segments` stores through the seal store,
//! reads back through it, replays a retried batch instead of duplicating
//! it, and widens its declared encoding when a value needs it — while an
//! aspect created without the variable, or with a compression config,
//! stays in rows mode.

use std::str::FromStr;

use anyhow::Result;
use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use futures::StreamExt;
use serial_test::serial;
use tempfile::TempDir;
use uuid::Uuid;
use weftdb::{
	database::traits::{AspectStructure, DatabaseStructure, Inputs, Outputs}, Database, DatasetId, InputMeasurement, StorageMode
};

fn bd(s: &str) -> BigDecimal {
	BigDecimal::from_str(s).expect("decimal")
}

async fn setup() -> Result<(TempDir, Database)> {
	let temp_dir = tempfile::tempdir()?;
	std::env::set_var("TEST_DATA_DIR", temp_dir.path().to_str().unwrap());
	let db_name = format!("seal_db_{}", Uuid::new_v4());
	let db = Database::new(&db_name).await?;
	Ok((temp_dir, db))
}

#[tokio::test]
#[serial]
async fn seal_backed_aspect_round_trips_replays_and_widens() -> Result<()> {
	std::env::set_var("WEFT_LEGACY_DEFAULT_STORAGE", "segments");
	let (_dir, db) = setup().await?;
	let subject = db.observe_subject("s").await?;
	let aspect = db.track_aspect(&subject.id(), "sealed", &splimes::Resolution::Milliseconds, None).await?;
	let aspect_id = aspect.id();
	assert_eq!(aspect.storage_mode(), StorageMode::Segments, "the aspect is created seal-backed");
	assert_eq!(db.aspect_storage_mode(&aspect_id).await?, StorageMode::Segments);

	let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
	let dataset = DatasetId::new();
	let rows: Vec<InputMeasurement> = (0..5).map(|i| InputMeasurement::new(base + Duration::seconds(i), bd(&format!("{}.5", i + 1)))).collect();

	// One batch ingest: all-or-nothing, and a retry replays it.
	let tx1 = db.batch_capture_measurements(aspect_id, dataset, rows.clone()).await?;
	let tx2 = db.batch_capture_measurements(aspect_id, dataset, rows.clone()).await?;
	assert_eq!(tx1, tx2, "a retry of the same call returns the same synthetic TxIds");
	assert_eq!(db.get_measurements_count(&aspect_id).await?, 5, "the replay stored nothing twice");

	// Raw reads come back in timestamp order with the stored values.
	let raw: Vec<_> = db.get_raw_measurements(&aspect_id, None, None, 100, 0).await?.collect::<Vec<_>>().await.into_iter().collect::<Result<Vec<_>>>()?;
	assert_eq!(raw.len(), 5);
	for (i, m) in raw.iter().enumerate() {
		assert_eq!(m.timestamp(), base + Duration::seconds(i64::try_from(i).expect("fits an i64")));
		assert_eq!(*m.value(), bd(&format!("{}.5", i + 1)));
	}

	// Range, boundary, earliest and latest agree.
	let ranged: Vec<_> = db.fetch_measurements_for_range(&aspect_id, base + Duration::seconds(1), base + Duration::seconds(3)).await?.collect::<Vec<_>>().await.into_iter().collect::<Result<Vec<_>>>()?;
	assert_eq!(ranged.len(), 3, "the range read is inclusive, as rows mode's was");
	assert_eq!(db.get_earliest_measurement(&aspect_id).await?, Some(base));
	assert_eq!(db.get_latest_measurement(&aspect_id).await?, Some(base + Duration::seconds(4)));
	let boundary: Vec<_> = db.get_boundary_measurements(&aspect_id).await?.collect::<Vec<_>>().await.into_iter().collect::<Result<Vec<_>>>()?;
	assert_eq!(boundary.len(), 4, "earliest two and latest two");
	assert_eq!(boundary.first().map(weftdb::Measurement::timestamp), Some(base));
	assert_eq!(boundary.last().map(weftdb::Measurement::timestamp), Some(base + Duration::seconds(4)));

	// A single capture is a one-row ingest.
	db.capture_measurement(&aspect_id, &dataset, &InputMeasurement::new(base + Duration::seconds(10), bd("99.25"))).await?;
	assert_eq!(db.get_measurements_count(&aspect_id).await?, 6);
	assert_eq!(db.get_latest_measurement(&aspect_id).await?, Some(base + Duration::seconds(10)));

	// Widening: a value Decimal128 cannot hold exactly widens the aspect's
	// declared encoding to BigDecimalText, and reads back exactly, beside
	// the frames sealed before the widening.
	let wide = "12345678901234567890123456789012345678901234567890.5";
	db.batch_capture_measurements(aspect_id, dataset, vec![InputMeasurement::new(base + Duration::seconds(20), bd(wide))]).await?;
	let store = db.seal_store().await?;
	let schema = store.schema_for(&weftdb::legacy_seal::aspect_key(&aspect_id)).await?.expect("the aspect is declared");
	drop(store);
	assert_eq!(schema.value, weft_physical_type::PhysicalType::BigDecimalText, "the declaration widened through widen_value_encoding");
	let all: Vec<_> = db.get_raw_measurements(&aspect_id, None, None, 100, 0).await?.collect::<Vec<_>>().await.into_iter().collect::<Result<Vec<_>>>()?;
	assert_eq!(all.len(), 7);
	assert_eq!(*all.last().expect("the wide row").value(), bd(wide), "the wide value round-trips exactly");
	assert_eq!(*all.first().expect("the first row").value(), bd("1.5"), "pre-widening frames still read");

	// The frames live under the database's segments_v2 root.
	let db_dir = std::path::Path::new(db.metadata_path()).parent().expect("db dir").to_path_buf();
	assert!(db_dir.join("segments_v2").join("segments").exists(), "frames live in segments_v2/segments");

	std::env::remove_var("WEFT_LEGACY_DEFAULT_STORAGE");
	Ok(())
}

#[tokio::test]
#[serial]
async fn rows_mode_is_the_default_and_compression_forces_it() -> Result<()> {
	std::env::remove_var("WEFT_LEGACY_DEFAULT_STORAGE");
	let (_dir, db) = setup().await?;
	let subject = db.observe_subject("s").await?;
	let plain = db.track_aspect(&subject.id(), "plain", &splimes::Resolution::Milliseconds, None).await?;
	assert_eq!(plain.storage_mode(), StorageMode::Rows);
	assert_eq!(db.aspect_storage_mode(&plain.id()).await?, StorageMode::Rows);

	// Even with the variable set, a compression config keeps rows mode.
	std::env::set_var("WEFT_LEGACY_DEFAULT_STORAGE", "segments");
	let compressed = db.track_aspect(&subject.id(), "compressed", &splimes::Resolution::Milliseconds, Some(weftdb::CompressionConfig::default())).await;
	drop(subject);
	std::env::remove_var("WEFT_LEGACY_DEFAULT_STORAGE");
	if let Ok(compressed) = &compressed {
		assert_eq!(compressed.storage_mode(), StorageMode::Rows, "compression deletes ranges, so it stays rows mode");
		assert_eq!(db.aspect_storage_mode(&compressed.id()).await?, StorageMode::Rows);
	}
	drop(compressed);

	// The rows-mode aspect still stores and reads as before.
	let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
	db.batch_capture_measurements(plain.id(), DatasetId::new(), vec![InputMeasurement::new(base, bd("3.5"))]).await?;
	assert_eq!(db.get_measurements_count(&plain.id()).await?, 1);
	Ok(())
}
