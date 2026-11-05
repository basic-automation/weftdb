//! ROB-5: Tests that the legacy queue consumer returns errors instead of panicking.
//!
//! These tests verify that all panic sites in the consumer path have been converted
//! to return proper errors. Each test uses `tokio::spawn` to ensure that a panic
//! in the spawned task is caught as a `JoinError`, causing a test failure rather than
//! losing the test process.

use anyhow::Result;
use bigdecimal::BigDecimal;
use chrono::{TimeZone, Utc};
use serial_test::serial;
use splimes::{Resolution, Spline};
use weft_orchestration::{build_patterns_queue, DictionaryConstraints};
use weftdb::{
	database::traits::{AspectStructure, DatabaseStructure, Inputs}, AspectId, Database, DatasetId, Dictionary, InputMeasurement
};

/// Helper to create a test database with measurements
async fn setup_test_db(name: &str) -> Result<(Database, AspectId)> {
	let db = Database::new(name).await?;
	let subject = db.observe_subject("test_subject").await?;
	let aspect = db.track_aspect(&subject.id(), "test_aspect", &Resolution::Minutes, None).await?;
	Ok((db, aspect.id()))
}

/// Test that build_patterns_queue handles empty active measurements without panicking
/// (fixes lib.rs:819-820 unwrap on min/max)
#[tokio::test]
#[serial]
async fn test_build_patterns_queue_no_panic_on_empty_batch() -> Result<()> {
	let uuid_suffix = uuid::Uuid::new_v4().simple().to_string();
	let db_name = format!("test_patterns_empty_{uuid_suffix}");
	let (db, aspect_id) = setup_test_db(&db_name).await?;

	// Create a batch with no active measurements - this would previously panic at the unwrap
	// We need to manually insert a processed batch since normal flow would skip empty batches
	let start_time = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

	// Insert some measurements to make it past the early guards
	for i in 0..3 {
		let timestamp = start_time + chrono::Duration::minutes(i);
		let measurement = InputMeasurement::new(timestamp, BigDecimal::from(100 + i));
		db.capture_measurement(&aspect_id, &DatasetId::new(), &measurement).await?;
	}

	// Build batches
	weft_orchestration::batch_utils::build_unprocessed_queue(&db, &aspect_id, &Resolution::Minutes, &Spline::Linear, 5).await?;
	weft_orchestration::build_processed_batch_queue(&db, &aspect_id).await?;

	// Create a dictionary and spawn the test in a task to catch panics
	let constraints = DictionaryConstraints::new(None, None);
	let mut dictionary = Dictionary::new("test_dict".to_string(), "test".to_string(), constraints);

	// This should not panic - the is_empty() guard protects the min/max operations
	// Spawn in a task to catch any panics as JoinError
	let handle = tokio::spawn(async move { build_patterns_queue(&db, &aspect_id, &mut dictionary).await });

	let join_result = handle.await;
	assert!(join_result.is_ok(), "Task panicked: {:?}", join_result.err());
	let call_result = join_result.unwrap();
	assert!(call_result.is_ok(), "build_patterns_queue returned an error: {:?}", call_result.err());

	Ok(())
}

/// Test that Pipeline::dictionary() returns None instead of panicking
/// when no dictionaries are registered (fixes pipeline.rs:1212)
#[tokio::test]
#[serial]
async fn test_pipeline_dictionary_no_panic() -> Result<()> {
	let uuid_suffix = uuid::Uuid::new_v4().simple().to_string();
	let db_name = format!("test_pipeline_dict_{uuid_suffix}");
	let (db, aspect_id) = setup_test_db(&db_name).await?;

	// Create a pipeline without adding any dictionaries
	let pipeline = weft_orchestration::Pipeline::builder(db, aspect_id).build().await?;

	// This should return None instead of panicking
	// Spawn in a task to catch any panics as JoinError
	let handle = tokio::spawn(async move { pipeline.dictionary().is_none() });

	let join_result = handle.await;
	assert!(join_result.is_ok(), "Task panicked: {:?}", join_result.err());
	assert!(join_result.unwrap(), "Expected None for empty dictionaries, got Some");

	Ok(())
}

/// Test that Pipeline::dictionary_mut() returns None instead of panicking
/// when no dictionaries are registered (fixes pipeline.rs:1222)
#[tokio::test]
#[serial]
async fn test_pipeline_dictionary_mut_no_panic() -> Result<()> {
	let uuid_suffix = uuid::Uuid::new_v4().simple().to_string();
	let db_name = format!("test_pipeline_dict_mut_{uuid_suffix}");
	let (db, aspect_id) = setup_test_db(&db_name).await?;

	// Create a pipeline without adding any dictionaries
	let pipeline = weft_orchestration::Pipeline::builder(db, aspect_id).build().await?;

	// This should return None instead of panicking
	// Spawn in a task to catch any panics as JoinError
	let handle = tokio::spawn(async move {
		let mut pipeline = pipeline;
		pipeline.dictionary_mut().is_none()
	});

	let join_result = handle.await;
	assert!(join_result.is_ok(), "Task panicked: {:?}", join_result.err());
	assert!(join_result.unwrap(), "Expected None for empty dictionaries, got Some");

	Ok(())
}

/// Test that the consumer handles batches with measurements correctly and doesn't panic
/// on min/max operations (regression test for lib.rs:819-820)
#[tokio::test]
#[serial]
async fn test_patterns_queue_with_valid_measurements() -> Result<()> {
	let uuid_suffix = uuid::Uuid::new_v4().simple().to_string();
	let db_name = format!("test_patterns_valid_{uuid_suffix}");
	let (db, aspect_id) = setup_test_db(&db_name).await?;

	// Insert measurements
	let start_time = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
	for i in 0..20 {
		let timestamp = start_time + chrono::Duration::minutes(i);
		let measurement = InputMeasurement::new(timestamp, BigDecimal::from(100 + i));
		db.capture_measurement(&aspect_id, &DatasetId::new(), &measurement).await?;
	}

	// Build unprocessed and processed queues
	weft_orchestration::batch_utils::build_unprocessed_queue(&db, &aspect_id, &Resolution::Minutes, &Spline::Linear, 10).await?;
	weft_orchestration::build_processed_batch_queue(&db, &aspect_id).await?;

	// Create dictionary
	let constraints = DictionaryConstraints::new(None, None);
	let mut dictionary = Dictionary::new("test_dict".to_string(), "test".to_string(), constraints);

	// Build patterns - should not panic
	// Spawn in a task to catch any panics as JoinError
	let handle = tokio::spawn(async move { build_patterns_queue(&db, &aspect_id, &mut dictionary).await });

	let join_result = handle.await;
	assert!(join_result.is_ok(), "Task panicked: {:?}", join_result.err());
	let call_result = join_result.unwrap();
	assert!(call_result.is_ok(), "build_patterns_queue returned an error on valid input: {:?}", call_result.err());

	Ok(())
}
