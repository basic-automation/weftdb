// Debug test to examine batch processing issue
use std::fs::remove_dir_all;

use ::database::database::traits::{AspectStructure, Inputs};
use anyhow::Result;
use bigdecimal::BigDecimal;
use chrono::{TimeZone, Utc};
use database::{
	database::{config::DEFAULT_DATA_DIR, traits::DatabaseStructure}, Database, DatasetId, InputMeasurement
};
use splimes::{Resolution, Spline};

use crate::batch_utils::build_unprocessed_queue;

#[tokio::test]
async fn debug_batch_processing() -> Result<()> {
	println!("=== Starting debug batch processing test ===");
	// Create a test database with a smaller dataset first
	let db_path = format!("{DEFAULT_DATA_DIR}/debug_batch_test");
	remove_dir_all(&db_path).ok();

	let db = Database::new("debug_batch_test").await.unwrap();
	let test_subject = db.observe_subject("TestSubject").await.unwrap();
	let test_aspect = db.track_aspect(test_subject.id(), "TestAspect", Resolution::Minutes).await.unwrap();
	let start_time = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();

	// Create a small test dataset
	for i in 1..=20 {
		let timestamp = start_time + chrono::Duration::minutes(i64::from(i));
		let value = BigDecimal::from(i % 5); // Simple repeating pattern
		let measurement = InputMeasurement::new(timestamp, value);
		db.capture_measurement(test_aspect.id(), DatasetId::new(), measurement).await.unwrap();
	}

	println!("Created 20 measurements");

	// Build batches with size 5
	build_unprocessed_queue(&db, &test_aspect.id(), &Resolution::Minutes, &Spline::Linear, 5).await?;

	// Get unprocessed batches
	let unprocessed_batches = db.get_unprocessed_batches(&test_aspect.id()).await?;
	println!("Unprocessed batches: {}", unprocessed_batches.len());

	// Check if batches have proper IDs and hashes
	for (i, batch) in unprocessed_batches.iter().take(5).enumerate() {
		println!(
			"Batch {}: ID={:?}, Hash={:?}, Size={}",
			i,
			batch.batch_id().as_uuid().to_string().get(..8), // Show first 8 chars of ID
			batch.batch_hash().map(|s| &s[..8]),             // Show first 8 chars of hash
			batch.size()
		);
	}

	// Try processing one batch
	let mut first_batch = unprocessed_batches.into_iter().next().unwrap();
	println!("Before processing - Hash: {:?}", first_batch.batch_hash().map(|s| &s[..8]));

	// Process the batch (this modifies it)
	first_batch.process()?;
	println!("After processing - Hash: {:?}", first_batch.batch_hash().map(|s| &s[..8]));

	// Try to mark it as processed
	match db.mark_batch_processed(&first_batch).await {
		Ok(()) => println!("Successfully marked batch as processed"),
		Err(e) => println!("Failed to mark batch as processed: {e}"),
	}

	Ok(())
}
