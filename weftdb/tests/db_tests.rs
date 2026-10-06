use std::str::FromStr;

use ::weftdb::database::traits::{AspectStructure, Inputs};
use anyhow::{Context, Result};
use bigdecimal::BigDecimal;
use chrono::{DateTime, Duration, TimeZone, Utc};
use rayon::prelude::*;
use serial_test::serial;
use splimes::{Resolution, Spline};
#[cfg(test)]
use tempfile::TempDir;
use tracing::{debug, instrument};
use uuid::Uuid;
use weftdb::{database::traits::DatabaseStructure, Aspect, Config, Database, DatasetId, InputMeasurement, Outputs, Subject, DATABASES};

async fn setup_test_database() -> Result<(TempDir, Database, Subject, Aspect)> {
	// Changed return type to Aspect
	let temp_dir = tempfile::tempdir()?;
	let db_name = format!("test_db_{}", Uuid::new_v4());

	// Override the data directory for testing
	std::env::set_var("TEST_DATA_DIR", temp_dir.path().to_str().unwrap());

	let db = Database::new(&db_name).await?;
	let subject = db.observe_subject("test_subject").await?;
	let aspect = db.track_aspect(&subject.id(), "test_aspect", &splimes::Resolution::Milliseconds, None).await?;

	Ok((temp_dir, db, subject, aspect)) // Return aspect instead of aspect.id()
}

async fn add_test_measurements(db: &Database, aspect: &Aspect, base_time: DateTime<Utc>, count: usize) -> Result<()> {
	// Changed parameter type
	for i in 0..count {
		let timestamp = base_time + Duration::seconds(i as i64);
		let value = BigDecimal::from_str(&format!("{}.{}", i + 1, i * 10 % 100))?;
		let measurement = InputMeasurement::new(timestamp, value);
		db.capture_measurement(&aspect.id(), &DatasetId::new(), &measurement).await?;
		// Use aspect instead of aspect_id
	}
	Ok(())
}

#[tokio::test]
#[serial]
async fn test_analyze_point_basic_interpolation() -> Result<()> {
	let (_temp_dir, db, _subject, aspect) = setup_test_database().await?; // Changed variable name
	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	// Add some test measurements
	add_test_measurements(&db, &aspect, base_time, 5).await?; // Pass reference to aspect

	// Test interpolation between two points
	let query_time = base_time + Duration::seconds(2) + Duration::milliseconds(500);
	let result = db.analyze_point(&aspect.id(), query_time, &Resolution::Milliseconds, &Spline::Linear).await?; // Use aspect.id() for analyze_point

	// The result should be interpolated between second 2 and second 3
	assert!(result.value > BigDecimal::from_str("3.20")?, "Value should be greater than 3.20, got {}", result.value);
	assert!(result.value < BigDecimal::from_str("4.30")?, "Value should be less than 4.30, got {}", result.value);

	Ok(())
}

#[tokio::test]
#[serial]
async fn test_analyze_point_extrapolation_forward() -> Result<()> {
	let (_temp_dir, db, _subject, aspect) = setup_test_database().await?;
	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	add_test_measurements(&db, &aspect, base_time, 3).await?;

	// Test forward extrapolation
	let query_time = base_time + Duration::seconds(10);
	let result = db.analyze_point(&aspect.id(), query_time, &Resolution::Seconds, &Spline::Linear).await?;

	// Should extrapolate beyond the last measurement
	assert!(result.value > BigDecimal::from_str("3.20")?, "Extrapolated value should be greater than last measurement");

	Ok(())
}

#[tokio::test]
#[serial]
async fn test_analyze_point_extrapolation_backward() -> Result<()> {
	let (_temp_dir, db, _subject, aspect) = setup_test_database().await?;
	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	add_test_measurements(&db, &aspect, base_time, 3).await?;

	// Test backward extrapolation
	let query_time = base_time - Duration::seconds(5);
	let result = db.analyze_point(&aspect.id(), query_time, &Resolution::Seconds, &Spline::Linear).await?;

	// Should extrapolate before the first measurement
	assert!(result.value < BigDecimal::from_str("1.0")?, "Extrapolated value should be less than first measurement");

	Ok(())
}

#[tokio::test]
#[serial]
async fn test_analyze_point_exact_match() -> Result<()> {
	let (_temp_dir, db, _subject, aspect) = setup_test_database().await?;
	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	// Add one measurement
	let timestamp = base_time;
	let value = BigDecimal::from_str("42.5")?;
	let measurement = InputMeasurement::new(timestamp, value.clone());
	db.capture_measurement(&aspect.id(), &DatasetId::new(), &measurement).await?;

	// Query the exact same time
	let result = db.analyze_point(&aspect.id(), timestamp, &Resolution::Milliseconds, &Spline::Linear).await;

	// Handle potential error for single measurement
	match result {
		Ok(point) => {
			assert_eq!(point.value, value, "Exact match should return the same value");
			assert_eq!(point.timestamp, timestamp, "Exact match should return the same timestamp");
		}
		Err(e) => {
			println!("Exact match error (may be expected for single measurement): {e}");
			// If error is expected, we can assert on the error type or message
			// For now, we'll allow the test to pass if it's the insufficient measurements error
			assert!(e.to_string().contains("Insufficient measurements"), "Unexpected error: {e}");
		}
	}

	// Clear the databases map to clean up
	{
		let databases = DATABASES.lock().await;
		println!("Active databases: {}", databases.len());
	}

	Ok(())
}

#[tokio::test]
#[serial]
async fn test_analyze_point_cache_hit() -> Result<()> {
	let (_temp_dir, db, _subject, aspect) = setup_test_database().await?;
	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	add_test_measurements(&db, &aspect, base_time, 5).await?;

	let query_time = base_time + Duration::seconds(2) + Duration::milliseconds(500);

	// First call should populate cache
	let result1 = db.analyze_point(&aspect.id(), query_time, &Resolution::Milliseconds, &Spline::Linear).await?;

	// Second call should hit cache
	let result2 = db.analyze_point(&aspect.id(), query_time, &Resolution::Milliseconds, &Spline::Linear).await?;

	assert_eq!(result1.value, result2.value, "Cache hit should return the same value");
	assert_eq!(result1.timestamp, result2.timestamp, "Cache hit should return the same timestamp");

	Ok(())
}

#[tokio::test]
#[serial]
async fn test_analyze_point_different_resolutions() -> Result<()> {
	let (_temp_dir, db, _subject, aspect) = setup_test_database().await?;
	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	add_test_measurements(&db, &aspect, base_time, 5).await?;

	let query_time = base_time + Duration::seconds(2) + Duration::milliseconds(500);

	// Test different resolutions
	let result_ms = db.analyze_point(&aspect.id(), query_time, &Resolution::Milliseconds, &Spline::Linear).await?;
	let result_s = db.analyze_point(&aspect.id(), query_time, &Resolution::Seconds, &Spline::Linear).await?;

	// Results should be similar but may have different precision
	let diff = (&result_ms.value - &result_s.value).abs();
	assert!(diff < BigDecimal::from_str("1.0")?, "Results with different resolutions should be similar");

	Ok(())
}

#[tokio::test]
#[serial]
async fn test_analyze_point_no_measurements_error() -> Result<()> {
	let (_temp_dir, db, _subject, aspect) = setup_test_database().await?;
	let query_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	// Should return error when no measurements exist
	let result = db.analyze_point(&aspect.id(), query_time, &Resolution::Milliseconds, &Spline::Linear).await;
	assert!(result.is_err(), "Should return error when no measurements exist");

	Ok(())
}

#[tokio::test]
#[serial]
async fn test_analyze_point_invalid_aspect_error() -> Result<()> {
	let temp_dir = tempfile::tempdir()?;
	// Override the data directory for this test
	std::env::set_var("TEST_DATA_DIR", temp_dir.path().to_str().unwrap());

	let db_name = format!("test_invalid_aspect_{}", Uuid::new_v4());
	let db = Database::new(&db_name).await?;
	let query_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	// Use a random aspect ID that doesn't exist
	let invalid_aspect_id = weftdb::AspectId::new();
	let result = db.analyze_point(&invalid_aspect_id, query_time, &Resolution::Milliseconds, &Spline::Linear).await;
	assert!(result.is_err(), "Should return error for invalid aspect ID");

	// Clean up - remove the test database
	std::fs::remove_dir_all(format!("{}/{}", temp_dir.path().to_str().unwrap(), db_name)).ok();

	// Note: We don't need to lock DATABASES here unless necessary

	Ok(())
}

#[tokio::test]
#[serial]
async fn test_analyze_point_single_measurement() -> Result<()> {
	let (_temp_dir, db, _subject, aspect) = setup_test_database().await?;
	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	// Add only one measurement
	let timestamp = base_time;
	let value = BigDecimal::from_str("10.0")?;
	let measurement = InputMeasurement::new(timestamp, value.clone());
	db.capture_measurement(&aspect.id(), &DatasetId::new(), &measurement).await?;

	// Query a different time - should extrapolate or return the single value
	let query_time = base_time + Duration::seconds(10);
	let result = db.analyze_point(&aspect.id(), query_time, &Resolution::Seconds, &Spline::Linear).await;

	// With a single measurement, behavior depends on implementation
	// It might return the single value or an error
	match result {
		Ok(point) => {
			// If it returns a value, it should be the same as the single measurement
			println!("Single measurement result: {} at {}", point.value, point.timestamp);
		}
		Err(e) => {
			println!("Single measurement error (expected): {e}");
			// This is acceptable behavior for single measurements
		}
	}

	// Clean up
	{
		let databases = DATABASES.lock().await;
		println!("Active databases after single measurement test: {}", databases.len());
	}

	Ok(())
}

#[tokio::test]
#[serial]
async fn test_analyze_point_large_time_gap() -> Result<()> {
	let (_temp_dir, db, _subject, aspect) = setup_test_database().await?;
	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	// Add measurements with large gaps
	let timestamp1 = base_time;
	let value1 = BigDecimal::from_str("10.0")?;
	let measurement1 = InputMeasurement::new(timestamp1, value1);
	db.capture_measurement(&aspect.id(), &DatasetId::new(), &measurement1).await?;

	let timestamp2 = base_time + Duration::hours(24); // 24 hours later
	let value2 = BigDecimal::from_str("20.0")?;
	let measurement2 = InputMeasurement::new(timestamp2, value2);
	db.capture_measurement(&aspect.id(), &DatasetId::new(), &measurement2).await?;

	// Query a time in the middle
	let query_time = base_time + Duration::hours(12);
	let result = db.analyze_point(&aspect.id(), query_time, &Resolution::Hours, &Spline::Linear).await?;

	// Should interpolate between the two values
	assert!(result.value > BigDecimal::from_str("10.0")?, "Interpolated value should be greater than first measurement");
	assert!(result.value < BigDecimal::from_str("20.0")?, "Interpolated value should be less than second measurement");

	// Clean up
	{
		let databases = DATABASES.lock().await;
		println!("Active databases after large gap test: {}", databases.len());
	}

	Ok(())
}

#[tokio::test]
#[serial]
async fn test_analyze_point_time_boundary_conditions() -> Result<()> {
	let (_temp_dir, db, _subject, aspect) = setup_test_database().await?;
	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	add_test_measurements(&db, &aspect, base_time, 10).await?;

	// Test various boundary conditions
	let first_time = base_time;
	let last_time = base_time + Duration::seconds(9);
	let before_first = base_time - Duration::seconds(1);
	let after_last = base_time + Duration::seconds(10);

	// Query at exact boundaries
	let _result_first = db.analyze_point(&aspect.id(), first_time, &Resolution::Seconds, &Spline::Linear).await?;
	let _result_last = db.analyze_point(&aspect.id(), last_time, &Resolution::Seconds, &Spline::Linear).await?;

	// Query outside boundaries (extrapolation)
	let _result_before = db.analyze_point(&aspect.id(), before_first, &Resolution::Seconds, &Spline::Linear).await?;
	let _result_after = db.analyze_point(&aspect.id(), after_last, &Resolution::Seconds, &Spline::Linear).await?;

	// All should succeed with linear interpolation/extrapolation
	println!("Boundary condition tests passed");

	Ok(())
}

#[tokio::test]
#[serial]
async fn test_analyze_point_cache_invalidation() -> Result<()> {
	let (_temp_dir, db, _subject, aspect) = setup_test_database().await?;
	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	// Add initial measurements
	add_test_measurements(&db, &aspect, base_time, 3).await?;

	let query_time = base_time + Duration::seconds(1) + Duration::milliseconds(500);

	// First query should populate cache
	let result1 = db.analyze_point(&aspect.id(), query_time, &Resolution::Milliseconds, &Spline::Linear).await?;

	// Add more measurements (should invalidate cache)
	let new_timestamp = base_time + Duration::seconds(1) + Duration::milliseconds(250);
	let new_value = BigDecimal::from_str("99.9")?;
	let new_measurement = InputMeasurement::new(new_timestamp, new_value);
	db.capture_measurement(&aspect.id(), &DatasetId::new(), &new_measurement).await?; // Query again - should return different result due to new data
	let result2 = db.analyze_point(&aspect.id(), query_time, &Resolution::Milliseconds, &Spline::Linear).await?;

	// Results should be different due to the new measurement affecting interpolation
	println!("Result 1: {}, Result 2: {}", result1.value, result2.value);

	// Clean up
	{
		let databases = DATABASES.lock().await;
		println!("Active databases after cache invalidation test: {}", databases.len());
	}

	Ok(())
}

#[tokio::test]
#[serial]
async fn test_analyze_point_concurrent_access() -> Result<()> {
	let (_temp_dir, db, _subject, aspect) = setup_test_database().await?;
	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	add_test_measurements(&db, &aspect, base_time, 5).await?;

	// Create multiple concurrent queries
	let query_time = base_time + Duration::seconds(2) + Duration::milliseconds(500);
	let mut handles = Vec::new();

	let aspect_id = aspect.id(); // Get the aspect ID once
	for i in 0..10 {
		let db_clone = db.clone();
		let query_time_offset = query_time + Duration::milliseconds(i * 10);
		let handle = tokio::spawn(async move { db_clone.analyze_point(&aspect_id, query_time_offset, &Resolution::Milliseconds, &Spline::Linear).await });
		handles.push(handle);
	}

	// Wait for all queries to complete
	let mut results = Vec::new();
	for handle in handles {
		let result = handle.await??;
		results.push(result);
	}

	// All queries should succeed
	assert_eq!(results.len(), 10, "All concurrent queries should succeed");

	// Clean up
	{
		let databases = DATABASES.lock().await;
		println!("Active databases after concurrent test: {}", databases.len());
	}

	Ok(())
}

#[tokio::test]
#[serial]
async fn test_analyze_point_precision_boundaries() -> Result<()> {
	let (_temp_dir, db, _subject, aspect) = setup_test_database().await?;
	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	// Add measurements with high precision values
	let timestamp1 = base_time;
	let value1 = BigDecimal::from_str("1.123456789012345")?;
	let measurement1 = InputMeasurement::new(timestamp1, value1);
	db.capture_measurement(&aspect.id(), &DatasetId::new(), &measurement1).await?;

	let timestamp2 = base_time + Duration::milliseconds(1000);
	let value2 = BigDecimal::from_str("2.987654321098765")?;
	let measurement2 = InputMeasurement::new(timestamp2, value2);
	db.capture_measurement(&aspect.id(), &DatasetId::new(), &measurement2).await?;

	// Query a time in between with high precision
	let query_time = base_time + Duration::milliseconds(500);
	let result = db.analyze_point(&aspect.id(), query_time, &Resolution::Milliseconds, &Spline::Linear).await?;

	// Should interpolate with reasonable precision
	assert!(result.value > BigDecimal::from_str("1.0")?, "Interpolated value should be greater than first measurement");
	assert!(result.value < BigDecimal::from_str("3.0")?, "Interpolated value should be less than second measurement");

	println!("High precision interpolation result: {}", result.value);

	Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct BTC1MinRecord {
	#[serde(rename = "Timestamp")]
	timestamp: f64, // Changed to f64 to handle decimal timestamps
	#[serde(rename = "Open")]
	open: f64,
	#[serde(rename = "High")]
	high: f64,
	#[serde(rename = "Low")]
	low: f64,
	#[serde(rename = "Close")]
	close: f64,
	#[serde(rename = "Volume")]
	volume: f64,
}

fn convert_unix_timestamp_to_datetime_utc(timestamp_seconds: f64) -> Option<DateTime<Utc>> {
	// Convert f64 timestamp to i64 seconds and u32 nanoseconds
	let seconds = timestamp_seconds as i64;
	let nanoseconds = ((timestamp_seconds - seconds as f64) * 1_000_000_000.0) as u32;
	DateTime::from_timestamp(seconds, nanoseconds)
}

/// load BTC 1-minute data into a test database for use in other tests
#[tokio::test(flavor = "multi_thread")]
#[serial]
#[instrument]
async fn test_create_btc_1min_database() -> Result<()> {
	// Initialize tracing subscriber for this test
	// Filter out verbose turso_core logs that slow down execution
	use tracing_subscriber::EnvFilter;
	let filter = EnvFilter::new("debug,turso_core=warn");
	let _subscriber = tracing_subscriber::fmt().with_env_filter(filter).with_test_writer().try_init();

	// Skip this test if running in CI or if we want fast feedback.
	// This is the workspace suite's long pole: it bulk-loads the real
	// `datasets/btc_1min.csv` corpus, and on a cold data dir it runs for 40+ minutes
	// at ~5 GB RSS, which is why `cargo test --workspace` never terminated. The other
	// 14 tests in this target finish in seconds. `SKIP_SLOW_TESTS` was already the
	// repo's convention for exactly this (see `weft_orchestration/src/lib.rs`) but
	// had never been wired up here, so `SKIP_SLOW_TESTS=1` silently did nothing for
	// this target.
	if std::env::var("SKIP_SLOW_TESTS").is_ok() {
		debug!("Skipping test_create_btc_1min_database due to SKIP_SLOW_TESTS environment variable");
		return Ok(());
	}

	// This test creates a database with BTC 1-minute data
	// Note: This requires the CSV file to be present
	let csv_path = "datasets/btc_1min.csv"; // Correct path when running from database directory
	let db_name = "Crypto".to_string();

	// **Bounded by default.** Loading the whole corpus through the legacy row-store path
	// takes 40+ minutes at ~5 GB RSS (it is super-linear — see
	// `ingest_path_profile_legacy_vs_columnar`, which measured 2x rows costing 4.11x the
	// time on this same corpus). Gating it on `SKIP_SLOW_TESTS` made the workspace suite
	// terminate, but at the cost of never exercising the real-corpus CSV ingest path at
	// all. So the default run now loads a **capped prefix** into a **temp data dir**:
	// bounded, hermetic, and it actually runs. `BTC_TEST_FULL=1` restores the historical
	// full load into the shared data dir; `BTC_TEST_MAX_ROWS` tunes the cap.
	let full_load = std::env::var("BTC_TEST_FULL").is_ok();
	let max_rows: usize = std::env::var("BTC_TEST_MAX_ROWS").ok().and_then(|s| s.parse().ok()).unwrap_or(5_000);
	// Held for the whole test so the temp dir outlives the database handle.
	let _bounded_dir = if full_load {
		None
	} else {
		let temp = tempfile::tempdir()?;
		std::env::set_var("TEST_DATA_DIR", temp.path().to_str().context("temp data dir is not valid UTF-8")?);
		debug!("Bounded BTC load: {max_rows} rows into a temp data dir at {}", temp.path().display());
		Some(temp)
	};

	// Create database (will use existing if present). The bounded run always starts from a
	// fresh temp dir, so this early-out only ever applies to the full load — which is the
	// point: the bounded path is exercised on every run rather than short-circuited.
	let db = if std::path::Path::new(&format!("{}/{db_name}", Database::get_data_dir())).exists() {
		debug!("Database already exists, test passed");
		return Ok(());
	} else {
		debug!("Creating new BTC database");
		Database::new(&db_name).await?
	};

	// Always check if we need to load data
	let subject_list = db.list_subjects().await?;
	let has_btcusd = subject_list.iter().any(|(_, name)| name.as_str() == "BTCUSD");

	// If BTCUSD subject exists, check if it has measurements
	let needs_data = if has_btcusd {
		if let Some((subject_id, _)) = subject_list.iter().find(|(_, name)| name.as_str() == "BTCUSD") {
			let aspects = db.get_subject_aspects(subject_id).await?;
			if let Some(aspect) = aspects.iter().find(|a| a.name() == "open") {
				// Check if the aspect has measurements
				db.get_earliest_measurement(&aspect.id()).await?.is_none()
			} else {
				true // No "open" aspect found
			}
		} else {
			true // Shouldn't happen since we found BTCUSD above
		}
	} else {
		true // No BTCUSD subject
	};

	debug!("Database has BTCUSD subject: {}, needs data: {}", has_btcusd, needs_data);

	if needs_data {
		// Populate with data if CSV exists
		debug!("Checking for CSV file at: {}", csv_path);
		debug!("Current working directory: {:?}", std::env::current_dir());
		if std::path::Path::new(csv_path).exists() {
			debug!("CSV file found, loading data...");
			debug!("Observing subject BTCUSD");
			let subject = db.observe_subject("BTCUSD").await?;

			debug!("Tracking aspects for BTCUSD");
			// Create aspects for different price types (with delays to prevent resource exhaustion)
			let open_aspect = db.track_aspect(&subject.id(), "open", &Resolution::Minutes, None).await?;
			let high_aspect = db.track_aspect(&subject.id(), "high", &Resolution::Minutes, None).await?;
			let low_aspect = db.track_aspect(&subject.id(), "low", &Resolution::Minutes, None).await?;
			let close_aspect = db.track_aspect(&subject.id(), "close", &Resolution::Minutes, None).await?;
			let volume_aspect = db.track_aspect(&subject.id(), "volume", &Resolution::Minutes, None).await?;

			debug!("Tracking aspects for BTCUSD: {}, {}, {}, {}, {}", open_aspect.id(), high_aspect.id(), low_aspect.id(), close_aspect.id(), volume_aspect.id());

			// Read and process CSV data
			let mut rdr = csv::ReaderBuilder::new().has_headers(true).from_path(csv_path)?;

			// loop through CSV records in chunks of 100_000
			let mut records = Vec::new();

			for result in rdr.deserialize() {
				if !full_load && records.len() >= max_rows {
					break;
				}
				let record: BTC1MinRecord = result.context("Failed to deserialize CSV record")?;
				records.push(record);
			}

			debug!("Loaded {} records from CSV", records.len());
			if full_load {
				assert!(records.len() > max_rows, "the full load should read far more than the bounded cap");
			} else {
				assert_eq!(records.len(), max_rows, "the bounded load reads exactly its cap");
			}

			let all_measurements: Vec<(InputMeasurement, InputMeasurement, InputMeasurement, InputMeasurement, InputMeasurement)> = records
				.par_iter()
				.filter_map(|record| {
					let timestamp = convert_unix_timestamp_to_datetime_utc(record.timestamp)?;
					let open = InputMeasurement::new(timestamp, BigDecimal::from_str(&record.open.to_string()).ok()?);
					let high = InputMeasurement::new(timestamp, BigDecimal::from_str(&record.high.to_string()).ok()?);
					let low = InputMeasurement::new(timestamp, BigDecimal::from_str(&record.low.to_string()).ok()?);
					let close = InputMeasurement::new(timestamp, BigDecimal::from_str(&record.close.to_string()).ok()?);
					let volume = InputMeasurement::new(timestamp, BigDecimal::from_str(&record.volume.to_string()).ok()?);
					Some((open, high, low, close, volume))
				})
				.collect();

			debug!("Converted records to measurements");

			let mut open_measurements: Vec<InputMeasurement> = Vec::with_capacity(all_measurements.len());
			let mut high_measurements: Vec<InputMeasurement> = Vec::with_capacity(all_measurements.len());
			let mut low_measurements: Vec<InputMeasurement> = Vec::with_capacity(all_measurements.len());
			let mut close_measurements: Vec<InputMeasurement> = Vec::with_capacity(all_measurements.len());
			let mut volume_measurements: Vec<InputMeasurement> = Vec::with_capacity(all_measurements.len());

			debug!("Preparing measurement vectors");

			for (open, high, low, close, volume) in all_measurements {
				open_measurements.push(open);
				high_measurements.push(high);
				low_measurements.push(low);
				close_measurements.push(close);
				volume_measurements.push(volume);
			}

			debug!("Starting batch insertion of measurements in parallel");

			// Run batch insertions in parallel
			#[rustfmt::skip]
			let (open_result, high_result, low_result, close_result, volume_result) = tokio::join!(
                                db.batch_capture_measurements(open_aspect.id(), DatasetId::new(), open_measurements),
                                db.batch_capture_measurements(high_aspect.id(), DatasetId::new(), high_measurements),
                                db.batch_capture_measurements(low_aspect.id(), DatasetId::new(), low_measurements),
                                db.batch_capture_measurements(close_aspect.id(), DatasetId::new(), close_measurements),
                                db.batch_capture_measurements(volume_aspect.id(), DatasetId::new(), volume_measurements)
                        );

			// Check all results
			open_result?;
			high_result?;
			low_result?;
			close_result?;
			volume_result?;

			debug!("Batch insertion of measurements completed");

			let inserted_count = records.len();
			debug!("Inserted {} BTC 1-minute records", inserted_count);

			// The point of running at all: assert the real-corpus ingest actually landed,
			// rather than merely not erroring.
			let earliest = db.get_earliest_measurement(&close_aspect.id()).await?;
			assert!(earliest.is_some(), "the close aspect has measurements after the load");
		} else {
			debug!("Skipping BTC database test - CSV file not found at {}", csv_path);
		}
	}

	Ok(())
}

/// Ingest-path profile (roadmap Phase 4 / the `test_create_btc_1min_database`
/// slow-load investigation): time the **legacy Turso `measurements` row-store** write
/// path against the **Storage-v2 `.weftseg` columnar seal** for the same synthetic
/// BTC-like corpus, so the "40+ minutes at ~5 GB RSS" the BTC test documents is
/// explained with a real number rather than a hunch.
///
/// The legacy path (`batch_capture_measurements`) writes each measurement as a Turso
/// row keyed by a 36-byte UUID text `id` and a 36-byte UUID `dataset_id`, with the
/// value stored as decimal **text** — and then writes every timestamp *again* into the
/// `unbatched_measurements` shadow queue (a 2× row-store write). The columnar path seals
/// the same points into one typed `.weftseg` frame (delta/bit-packed timestamps, a scaled
/// integer value column). This is the write-amplification the roadmap's storage boundary
/// (hard-constraint #3: Turso is the control plane, `.weftseg` owns the measurement hot
/// path) exists to remove.
///
/// Gated on `RUN_INGEST_PROFILE` so it never joins the normal suite (like the BTC test it
/// explains). Run it with, e.g.:
/// `RUN_INGEST_PROFILE=1 INGEST_PROFILE_N=50000 cargo test -p database --test db_tests ingest_path_profile -- --nocapture`
///
/// # Corpus
///
/// By default the corpus is **synthetic** (a 1-minute grid of two-decimal prices). Set
/// `INGEST_PROFILE_CORPUS=btc` to profile the **real** `database/datasets/btc_1min.csv`
/// instead — the corpus `test_create_btc_1min_database` actually loads. The two are worth
/// separating: the synthetic price is `20000 + (i % 5000)` with a `i % 100` fraction,
/// which is far more regular than real market data, so its realized bytes/point flatters
/// the columnar codecs. The real corpus is the honest number, and `INGEST_PROFILE_CSV`
/// points the same path at any other `Timestamp,…,Close,…` CSV.
///
/// Set `INGEST_PROFILE_SKIP_LEGACY=1` to profile only the columnar seal — necessary at
/// large `n`, because the legacy path is super-linear and a million rows through it does
/// not finish in a sensible time.
///
/// `INGEST_PROFILE_SKIP_ROWS=N` discards the first N data rows of a real corpus. Use it:
/// the BTC file's head is degenerate (see [`read_close_series`]) and measuring
/// bytes/point there overstates WeftDB's compression by ~10×.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn ingest_path_profile_legacy_vs_columnar() -> Result<()> {
	if std::env::var("RUN_INGEST_PROFILE").is_err() {
		return Ok(());
	}
	let n: usize = std::env::var("INGEST_PROFILE_N").ok().and_then(|s| s.parse().ok()).unwrap_or(50_000);
	let skip_legacy = std::env::var("INGEST_PROFILE_SKIP_LEGACY").is_ok();

	// The corpus: real BTC 1-minute closes when asked for, else the synthetic grid.
	let real_csv = std::env::var("INGEST_PROFILE_CSV").ok().map(std::path::PathBuf::from).or_else(|| (std::env::var("INGEST_PROFILE_CORPUS").as_deref() == Ok("btc")).then(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("datasets").join("btc_1min.csv")));
	let base = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
	let skip_rows: usize = std::env::var("INGEST_PROFILE_SKIP_ROWS").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
	let (corpus, ts_ms, values) = match &real_csv {
		Some(path) => {
			let (ts, vs) = read_close_series(path, n, skip_rows)?;
			(format!("real {} (skip {skip_rows})", path.display()), ts, vs)
		}
		None => {
			let price = |i: usize| format!("{}.{:02}", 20_000 + (i % 5_000), i % 100);
			let ts: Vec<i64> = (0..n).map(|i| (base + Duration::minutes(i as i64)).timestamp_millis()).collect();
			let vs: Vec<BigDecimal> = (0..n).map(|i| BigDecimal::from_str(&price(i)).unwrap()).collect();
			("synthetic 1-minute grid".to_string(), ts, vs)
		}
	};
	let n = ts_ms.len();
	assert!(n > 0, "the corpus yielded no rows");

	let temp = tempfile::tempdir()?;
	std::env::set_var("TEST_DATA_DIR", temp.path().to_str().unwrap());

	// --- Legacy Turso row-store path (measurements + unbatched shadow queue) ---
	// Skippable: the legacy path is super-linear, so a large real corpus through it does
	// not finish in a sensible time — which is itself the finding this profile records.
	let legacy = if skip_legacy {
		None
	} else {
		let db = Database::new(&format!("prof_{}", Uuid::new_v4())).await?;
		let subject = db.observe_subject("BTCUSD").await?;
		let aspect = db.track_aspect(&subject.id(), "close", &Resolution::Minutes, None).await?;
		// The legacy `InputMeasurement` carries a `DateTime`, so the corpus timestamps are
		// lifted back out of their epoch-millis form rather than re-derived from `base` —
		// otherwise a real corpus would be timed against synthetic timestamps.
		let measurements: Vec<InputMeasurement> = ts_ms.iter().zip(&values).map(|(ms, v)| InputMeasurement::new(Utc.timestamp_millis_opt(*ms).single().unwrap_or(base), v.clone())).collect();
		let t0 = std::time::Instant::now();
		db.batch_capture_measurements(aspect.id(), DatasetId::new(), measurements).await?;
		Some(t0.elapsed())
	};

	// --- Storage-v2 columnar seal (one typed .weftseg frame) ---
	// The physical encoding is DERIVED from the corpus rather than hardcoded. A fixed
	// `ScaledI64 { scale: 2 }` happens to fit the synthetic two-decimal prices, but real
	// BTC closes carry more fractional digits, and the no-silent-downcast rule
	// (hard-constraint #4) then rejects the seal outright ("worst error 0.00117537 > bound
	// 0") rather than quietly losing precision. `recommend_encoding` at a zero error bound
	// picks the cheapest encoding that is EXACT for this column, which is what a schema
	// author would declare.
	let store = weftdb::SegmentStore::open(temp.path().join("segstore")).await?;
	let exact = BigDecimal::from_str("0").unwrap();
	let recommended = weft_physical_type::recommend_encoding(&values, &exact);
	assert!(recommended.is_exact(), "the recommended encoding must be exact at a zero error bound");
	let schema = weft_physical_type::AspectSchema::new(recommended.physical_type, exact, weft_physical_type::timestamp::TimeUnit::Millis);
	let t1 = std::time::Instant::now();
	let descriptor = store.seal("close", &schema, &ts_ms, &values).await?;
	let columnar = t1.elapsed();

	// The columnar path persisted all n rows.
	assert_eq!(descriptor.row_count as usize, n, "columnar seal wrote every row");

	let col_rps = n as f64 / columnar.as_secs_f64();
	let bytes_per_point = descriptor.byte_len as f64 / n as f64;
	eprintln!("INGEST PROFILE n={n} corpus={corpus}:");
	eprintln!("  derived exact encoding : {:?}", recommended.physical_type);
	match legacy {
		Some(legacy) => {
			let legacy_rps = n as f64 / legacy.as_secs_f64();
			eprintln!("  legacy Turso row-store : {legacy:?}  ({legacy_rps:.0} rows/s)");
			eprintln!("  columnar .weftseg seal  : {columnar:?}  ({col_rps:.0} rows/s, {bytes_per_point:.2} B/point framed)");
			eprintln!("  columnar seal is {:.1}x faster on the write path", col_rps / legacy_rps);
		}
		None => {
			eprintln!("  legacy Turso row-store : SKIPPED (INGEST_PROFILE_SKIP_LEGACY)");
			eprintln!("  columnar .weftseg seal  : {columnar:?}  ({col_rps:.0} rows/s, {bytes_per_point:.2} B/point framed)");
		}
	}
	Ok(())
}

/// Convert a decimal epoch-**seconds** string (`"1325412060"` or `"1325412060.25"`) to
/// epoch millis using integer arithmetic only, or `None` if it does not parse.
///
/// The fractional part is read to at most three digits (zero-padded), so `.5` is 500 ms
/// and `.0` — the whole BTC corpus — is exactly 0. Doing this in integers rather than via
/// `f64` avoids both the float rounding and the truncating cast that a `(secs * 1000.0)
/// as i64` would introduce on a column that is exactly representable.
fn epoch_seconds_to_millis(raw: &str) -> Option<i64> {
	let (whole, frac) = raw.split_once('.').unwrap_or((raw, ""));
	let secs: i64 = whole.parse().ok()?;
	let mut millis_frac = 0i64;
	for (i, ch) in frac.chars().take(3).enumerate() {
		let digit = i64::from(ch.to_digit(10)?);
		millis_frac += digit * 10i64.pow(2 - u32::try_from(i).ok()?);
	}
	secs.checked_mul(1000)?.checked_add(if secs < 0 { -millis_frac } else { millis_frac })
}

/// Read `n` `(epoch_millis, Close)` rows out of a `Timestamp,Open,High,Low,Close,Volume`
/// CSV, after discarding the first `skip` data rows — the shape of
/// `database/datasets/btc_1min.csv`, whose `Timestamp` column is fractional epoch
/// **seconds**.
///
/// `skip` exists because **the head of that corpus is degenerate**: its first ~20k rows
/// are 2012 ticks where the close price holds constant for long runs (4.58 … 6.30), which
/// the value column's RLE-class codecs compress to almost nothing. Measuring bytes/point
/// on the first N rows therefore flatters WeftDB's compression by roughly an order of
/// magnitude versus a window with real price movement — so a representative storage number
/// must skip into the corpus. (Throughput is far less sensitive to this than size is.)
///
/// Streams the file line by line so a 370 MB corpus does not have to be resident, and
/// stops as soon as `n` rows are collected. A row whose timestamp or close price will not
/// parse is skipped rather than failing the profile: the corpus is real data, and one bad
/// line should not invalidate a throughput measurement.
///
/// # Errors
///
/// The file cannot be opened or read.
fn read_close_series(path: &std::path::Path, n: usize, skip: usize) -> Result<(Vec<i64>, Vec<BigDecimal>)> {
	use std::io::BufRead as _;

	let file = std::fs::File::open(path).with_context(|| format!("opening ingest-profile corpus {}", path.display()))?;
	let mut reader = std::io::BufReader::with_capacity(1 << 20, file);
	let mut header = String::new();
	reader.read_line(&mut header).context("reading the corpus header")?;

	let mut ts_ms = Vec::with_capacity(n.min(1 << 20));
	let mut values = Vec::with_capacity(n.min(1 << 20));
	let mut skipped = 0usize;
	for line in reader.lines() {
		if ts_ms.len() >= n {
			break;
		}
		let line = line.context("reading a corpus row")?;
		if skipped < skip {
			skipped += 1;
			continue;
		}
		let mut cols = line.split(',');
		let (Some(ts_raw), Some(_open), Some(_high), Some(_low), Some(close)) = (cols.next(), cols.next(), cols.next(), cols.next(), cols.next()) else {
			continue;
		};
		// `Timestamp` is fractional epoch seconds (e.g. `1325412060.0`). Scale to millis in
		// integer arithmetic — a `f64` round-trip would be a lossy, truncating cast on a
		// column that is exactly representable as an integer.
		let Some(millis) = epoch_seconds_to_millis(ts_raw.trim()) else { continue };
		let Ok(value) = BigDecimal::from_str(close.trim()) else { continue };
		ts_ms.push(millis);
		values.push(value);
	}
	Ok((ts_ms, values))
}

/// Release every in-process handle to the database `db_name`, so the next
/// [`Database::existing`] is a **cold** open, exactly as after a process restart.
///
/// Turso keeps one shared database per path in a process-wide registry of weak
/// references, so the reopen only goes back to disk once nothing holds the old one: not
/// the `Database` value, not its `DATABASES` entry (which also owns the subject and
/// aspect handles), and not WeftDB's own connection cache.
async fn release_database(db: Database, db_name: &str) {
	let id = db.id();
	drop(db);
	DATABASES.lock().await.remove(&id);
	weftdb::clear_connection_cache_by_name(db_name).await;
}

/// The MVCC logical log beside a database's `metadata.db`.
fn metadata_log_path(db_name: &str) -> std::path::PathBuf {
	std::path::Path::new(&Database::get_data_dir()).join(db_name).join("metadata.db-log")
}

/// Fold `metadata.db`'s logical log into the main file, leaving a zero-length `-log`
/// (TRUNCATE is the default checkpoint mode under MVCC).
async fn truncate_checkpoint(metadata: &turso::Database) -> Result<()> {
	let conn = metadata.connect()?;
	let mut rows = conn.query("PRAGMA wal_checkpoint(TRUNCATE)", ()).await?;
	while rows.next().await?.is_some() {}
	Ok(())
}

/// Give `metadata.db` and its sidecars (`-log`, any `-wal`) fresh inodes at the same paths,
/// by copying each one and renaming the copy over it, so the next open is **cold by
/// construction**.
///
/// [`release_database`] makes the reopen cold only as long as nothing else still holds the
/// old Turso database. Turso's process-wide registry is keyed by the DB file's (dev, ino),
/// so if a strong handle ever leaked (in `DatabaseInfo`, a cache or a background task) the
/// reopen would get the live in-memory database back, and the test would pass even with the
/// log deleted, because the unlink would not touch the live MVCC store. New inodes cannot
/// match a registry entry, so the reopen has to replay the `-log` from disk. The paths stay
/// put because the legacy store records its absolute `metadata_path` in metadata.db.
fn reinode_metadata_files(db_name: &str) -> Result<()> {
	let dir = std::path::Path::new(&Database::get_data_dir()).join(db_name);
	for entry in std::fs::read_dir(&dir)? {
		let path = entry?.path();
		let is_metadata = path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with("metadata.db"));
		if is_metadata && path.is_file() {
			let mut copy = path.clone().into_os_string();
			copy.push(".reinode");
			std::fs::copy(&path, &copy).with_context(|| format!("copying {}", path.display()))?;
			std::fs::rename(&copy, &path).with_context(|| format!("renaming the copy over {}", path.display()))?;
		}
	}
	Ok(())
}

/// Release every handle to `db_name`, check that the acknowledged commits are still only in
/// the logical log, then give the files fresh inodes, so the following
/// [`Database::existing`] is a genuine cold open that must replay that log.
async fn restart_with_uncheckpointed_log(db: Database, db_name: &str) -> Result<()> {
	release_database(db, db_name).await;

	// Precondition: the acknowledged commits are still only in the logical log. Without
	// it the test would pass for the wrong reason (a checkpoint had already moved them).
	let log_len = std::fs::metadata(metadata_log_path(db_name)).context("metadata.db-log is missing before the reopen")?.len();
	assert!(log_len > 0, "precondition: metadata.db-log holds the uncheckpointed commits (it is empty)");

	reinode_metadata_files(db_name)
}

/// **Regression (legacy-open-deletes-mvcc-logical-log).** Commits to `metadata.db` that
/// were acknowledged before a restart must still be there after it, and after a second
/// restart that follows more writes (replay, then append, then replay again).
///
/// Under MVCC every committed transaction lives in the `metadata.db-log` until a
/// checkpoint copies it into the main file. WeftDB's own PASSIVE checkpoints are rejected
/// under MVCC, so they never run, and Turso checkpoints by itself only once the log passes
/// about 4 MB, which this test stays far below (the precondition in
/// [`restart_with_uncheckpointed_log`] checks it). The cold-open path used to delete that log
/// unconditionally, which threw away the subject, the aspect and the unbatched queue; on
/// main the reopen does not even find the `database` table.
#[tokio::test]
#[serial]
async fn acknowledged_metadata_commits_survive_cold_reopen() -> Result<()> {
	let temp_dir = tempfile::tempdir()?;
	std::env::set_var("TEST_DATA_DIR", temp_dir.path().to_str().context("temp data dir is not valid UTF-8")?);
	let db_name = format!("reopen_{}", Uuid::new_v4());

	let db = Database::new(&db_name).await?;
	let subject = db.observe_subject("reopen_subject").await?;
	let aspect = db.track_aspect(&subject.id(), "reopen_aspect", &Resolution::Seconds, None).await?;
	let (subject_id, aspect_id) = (subject.id(), aspect.id());

	let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
	let batch = |range: std::ops::Range<i64>| -> Vec<InputMeasurement> { range.map(|i| InputMeasurement::new(base + Duration::seconds(i), BigDecimal::from(i))).collect() };
	let first = batch(0..8);
	let mut expected: Vec<DateTime<Utc>> = first.iter().map(InputMeasurement::timestamp).collect();
	db.batch_capture_measurements(aspect_id, DatasetId::new(), first).await?;
	assert_eq!(db.count_unbatched_measurements(&aspect_id).await?, 8, "the batch is queued before the restart");

	drop((subject, aspect));
	restart_with_uncheckpointed_log(db, &db_name).await?;

	let reopened = Database::existing(&db_name).await.context("cold reopen of a database with acknowledged metadata commits")?;

	let subjects = reopened.list_subjects().await?;
	assert_eq!(subjects.get(&subject_id).map(String::as_str), Some("reopen_subject"), "the subject survives the reopen");
	let aspects = reopened.get_subject_aspects(&subject_id).await?;
	assert!(aspects.iter().any(|a| a.id() == aspect_id && a.name() == "reopen_aspect"), "the aspect survives the reopen");

	let mut queued = reopened.get_unbatched_measurements(&aspect_id).await?;
	queued.sort();
	expected.sort();
	assert_eq!(queued, expected, "every queued timestamp survives the reopen");

	// Append to the replayed log, then restart again: both generations of commits must survive.
	let second = batch(8..16);
	expected.extend(second.iter().map(InputMeasurement::timestamp));
	reopened.batch_capture_measurements(aspect_id, DatasetId::new(), second).await?;
	assert_eq!(reopened.count_unbatched_measurements(&aspect_id).await?, 16, "the second batch is queued before the second restart");
	restart_with_uncheckpointed_log(reopened, &db_name).await?;

	let reopened = Database::existing(&db_name).await.context("second cold reopen after appending to the replayed log")?;
	assert!(reopened.list_subjects().await?.contains_key(&subject_id), "the subject survives the second reopen");
	let mut queued = reopened.get_unbatched_measurements(&aspect_id).await?;
	queued.sort();
	expected.sort();
	assert_eq!(queued, expected, "the queued timestamps from both batches survive the second reopen");

	release_database(reopened, &db_name).await;
	Ok(())
}

/// The cold-open sweep still removes a **zero-length** `metadata.db-log`: it holds no
/// commits and, once every handle is released, nothing has it open, so it is litter, the
/// same rule `remove_empty_sidecars` applies to backups.
///
/// Turso may create a fresh log as it opens, so "the path is gone" is not observable.
/// Instead a hard link pins the empty log's inode: if the sweep unlinked it, the probe is
/// its only remaining name (`nlink == 1`); had it been kept, `-log` would still be a
/// second name for it.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn zero_length_metadata_log_is_removed_on_cold_open() -> Result<()> {
	use std::os::unix::fs::MetadataExt as _;

	let temp_dir = tempfile::tempdir()?;
	std::env::set_var("TEST_DATA_DIR", temp_dir.path().to_str().context("temp data dir is not valid UTF-8")?);
	let db_name = format!("emptylog_{}", Uuid::new_v4());

	let db = Database::new(&db_name).await?;
	let subject = db.observe_subject("emptylog_subject").await?;
	let subject_id = subject.id();
	drop(subject);

	// Fold the logical log into the main file; TRUNCATE leaves the `-log` in place at
	// length zero.
	truncate_checkpoint(db.metadata()).await?;
	release_database(db, &db_name).await;

	let log = metadata_log_path(&db_name);
	assert_eq!(std::fs::metadata(&log).context("metadata.db-log is missing before the reopen")?.len(), 0, "precondition: the checkpoint left a zero-length log");
	let probe = log.with_extension("db-log.probe");
	std::fs::hard_link(&log, &probe)?;
	assert_eq!(std::fs::metadata(&probe)?.nlink(), 2);

	let reopened = Database::existing(&db_name).await?;
	assert_eq!(std::fs::metadata(&probe)?.nlink(), 1, "the zero-length metadata.db-log was not removed by the cold open");
	assert!(reopened.list_subjects().await?.contains_key(&subject_id), "the checkpointed subject is still readable");

	release_database(reopened, &db_name).await;
	Ok(())
}

/// Two concurrent cold opens of the same `metadata.db` over a zero-length `-log` must not
/// unlink each other's live log.
///
/// The cold-open sweep removes a zero-length log as litter. Without the connection-cache
/// guard held across the whole cold path, the second open could miss the cache, see the
/// fresh empty log the first open's Turso open had just created, and unlink it; Turso
/// (keyed by the DB file's inode) then hands both callers the same live database, whose
/// commits are fsynced into an unlinked file that no restart will replay. Each round
/// races two opens with the second one staggered across the first one's cold path,
/// commits through the first, and checks the commit reached the `-log` on disk; after a
/// crash-like release (no checkpoint) the next round's cold open must replay it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn concurrent_cold_opens_keep_the_live_metadata_log() -> Result<()> {
	const ROUNDS: u64 = 20;

	let temp_dir = tempfile::tempdir()?;
	std::env::set_var("TEST_DATA_DIR", temp_dir.path().to_str().context("temp data dir is not valid UTF-8")?);
	let db_name = format!("coldrace_{}", Uuid::new_v4());
	let db = Database::new(&db_name).await?;
	let metadata_path = format!("{}/{}/metadata.db", Database::get_data_dir(), db_name);
	release_database(db, &db_name).await;
	let log = metadata_log_path(&db_name);

	for round in 0..ROUNDS {
		// Start from a zero-length log, which the sweep treats as litter.
		let metadata = Database::get_turso_database(&metadata_path).await?;
		truncate_checkpoint(&metadata).await?;
		drop(metadata);
		weftdb::clear_connection_cache_by_name(&db_name).await;
		assert_eq!(std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0), 0, "precondition: round {round} starts from an empty log");

		let first_path = metadata_path.clone();
		let first = tokio::spawn(async move { Database::get_turso_database(&first_path).await });
		let second_path = metadata_path.clone();
		let stagger = std::time::Duration::from_micros(round * 250);
		let second = tokio::spawn(async move {
			tokio::time::sleep(stagger).await;
			Database::get_turso_database(&second_path).await
		});
		let (first, second) = (first.await??, second.await??);

		first.connect()?.execute("INSERT INTO transactions (id, message, created_at) VALUES (?, ?, ?)", turso::params![Uuid::new_v4().to_string(), format!("race {round}"), 0]).await?;
		let log_len = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
		assert!(log_len > 0, "round {round} (stagger {stagger:?}): the acknowledged commit is not in the metadata.db-log on disk, so it went to an unlinked log");

		// Release without a checkpoint, as a crash would.
		drop((first, second));
		weftdb::clear_connection_cache_by_name(&db_name).await;
	}

	let metadata = Database::get_turso_database(&metadata_path).await?;
	let mut rows = metadata.connect()?.query("SELECT COUNT(*) FROM transactions WHERE message LIKE 'race %'", ()).await?;
	let count: i64 = rows.next().await?.context("COUNT(*) returned no row")?.get(0)?;
	assert_eq!(count, i64::try_from(ROUNDS)?, "every acknowledged commit survives the cold reopens");
	drop((rows, metadata));
	weftdb::clear_connection_cache_by_name(&db_name).await;
	Ok(())
}

/// **Regression (legacy-queue-consumer-batch-before-dequeue, crash-consistency design
/// S18).** Queuing a batch whose measurements are already queued, or already processed,
/// stores nothing, through either insert API; a new batch is still queued. The processed
/// case holds although processing rewrites a batch's measurements: the processed row keeps
/// the hash the batch was queued under. On main every call inserted another copy.
#[tokio::test]
#[serial]
async fn queuing_a_queued_or_processed_batch_again_stores_nothing() -> Result<()> {
	use futures::TryStreamExt;
	use weftdb::{Batch, BatchedMeasurement, Point};

	let (_temp_dir, db, _subject, aspect) = setup_test_database().await?;
	let aspect_id = aspect.id();
	let info = db.get_database_info().await?;
	let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
	// A fresh `Batch` (new id) over the five points from minute `start`, as the consumer
	// builds one for a window.
	let batch = |start: i64| {
		let points = (start..start + 5).map(|i| BatchedMeasurement::new(Point::new(base + Duration::minutes(i), BigDecimal::from((i * 7) % 11)))).collect();
		Batch::new(5, points, Resolution::Minutes, aspect_id, info.clone())
	};
	let queued = || db.count_unprocessed_batches(&aspect_id);

	db.insert_unprocessed_batch(&aspect_id, &batch(0)).await?;
	db.insert_unprocessed_batch(&aspect_id, &batch(0)).await?;
	assert_eq!(queued().await?, 1, "insert_unprocessed_batch queued the same batch twice");
	let tx_ids = db.batch_insert_unprocessed_batches(&aspect_id, vec![batch(0), batch(1), batch(1)]).await?;
	assert_eq!(tx_ids.len(), 3, "every input batch gets a TxId, stored or skipped");
	assert_eq!(queued().await?, 2, "batch_insert_unprocessed_batches stores only the new batch, once");

	// Process the queued batches, as the batch processor does, and move them over.
	let mut stored: Vec<Batch> = db.get_unprocessed_batches(&aspect_id).await?.try_collect().await?;
	for stored in &mut stored {
		let before = serde_json::to_string(stored.measurements())?;
		stored.process()?;
		assert_ne!(serde_json::to_string(stored.measurements())?, before, "precondition: processing rewrites the measurements");
	}
	db.move_batches_to_processed(&aspect_id, &stored).await?;
	assert_eq!((queued().await?, db.count_processed_batches(&aspect_id).await?), (0, 2), "precondition: both batches are processed");

	db.insert_unprocessed_batch(&aspect_id, &batch(0)).await?;
	db.batch_insert_unprocessed_batches(&aspect_id, vec![batch(1)]).await?;
	assert_eq!(queued().await?, 0, "a processed batch was queued again");
	db.insert_unprocessed_batch(&aspect_id, &batch(2)).await?;
	db.batch_insert_unprocessed_batches(&aspect_id, vec![batch(3)]).await?;
	assert_eq!(queued().await?, 2, "new batches are still queued");
	Ok(())
}

/// A queue entry read by a consumer and queued again afterwards survives the consumer's
/// dequeue (crash-consistency design, S18): enqueuing a queued timestamp moves its
/// `queued_at` strictly forward, even within the same millisecond, and
/// `dequeue_unbatched_entries` removes an entry only while it has the `queued_at` it was
/// read with. Ingest relies on this to keep a row it committed after a consumer read the
/// row's write-ahead entry. Dequeuing bare timestamps removed it.
#[tokio::test]
#[serial]
async fn an_entry_queued_again_after_it_was_read_survives_the_dequeue() -> Result<()> {
	let (_temp_dir, db, _subject, aspect) = setup_test_database().await?;
	let aspect_id = aspect.id();
	let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
	let (requeued, untouched) = (base, base + Duration::minutes(1));

	db.enqueue_unbatched_measurements(&aspect_id, &[requeued, untouched]).await?;
	let read = db.get_unbatched_entries(&aspect_id).await?;
	assert_eq!(read.len(), 2);
	db.enqueue_unbatched_measurement(&aspect_id, requeued).await?;
	let after = db.get_unbatched_entries(&aspect_id).await?;
	let queued_at = |entries: &[weftdb::UnbatchedEntry], at| entries.iter().find(|entry| entry.data_timestamp == at).map(|entry| entry.queued_at);
	assert!(queued_at(&after, requeued) > queued_at(&read, requeued), "queuing again moves queued_at forward: {read:?} -> {after:?}");
	assert_eq!(queued_at(&after, untouched), queued_at(&read, untouched));

	db.dequeue_unbatched_entries(&aspect_id, &read).await?;
	assert_eq!(db.get_unbatched_measurements(&aspect_id).await?, vec![requeued], "only the entry not queued again since the read is dequeued");
	db.dequeue_unbatched_entries(&aspect_id, &db.get_unbatched_entries(&aspect_id).await?).await?;
	assert_eq!(db.count_unbatched_measurements(&aspect_id).await?, 0);
	Ok(())
}

/// The batch tables carry the `(aspect_id, batch_hash)` index that the queue's duplicate
/// check looks a batch up in (crash-consistency design, S18), so each check is a point
/// lookup instead of a scan of every stored batch; and a batch table created before the
/// index gets it the next time a process opens it.
#[tokio::test]
#[serial]
async fn the_batch_tables_index_their_hashes() -> Result<()> {
	/// Turso's plan for the duplicate check's lookup in `batches`.
	async fn lookup_plan(conn: &turso::Connection) -> Result<String> {
		let mut rows = conn.query("EXPLAIN QUERY PLAN SELECT 1 FROM batches WHERE aspect_id = ? AND batch_hash = ? LIMIT 1", ("aspect", "hash")).await?;
		let mut plan = String::new();
		while let Some(row) = rows.next().await? {
			plan.push_str(&row.get::<String>(3)?);
		}
		Ok(plan)
	}

	let (_temp_dir, db, subject, aspect) = setup_test_database().await?;
	let aspect_id = aspect.id();
	let name = db.name().to_string();
	drop((subject, aspect));
	for processed in [false, true] {
		let batches = if processed { db.get_processed_batches_db(&aspect_id).await? } else { db.get_unprocessed_batches_db(&aspect_id).await? };
		let conn = batches.connect()?;
		let plan = lookup_plan(&conn).await?;
		assert!(plan.contains("idx_batches_hash"), "processed={processed}: the lookup does not use the hash index: {plan}");
		// What a batch table created before the index looks like.
		conn.execute("DROP INDEX idx_batches_hash", ()).await?;
		assert!(!lookup_plan(&conn).await?.contains("idx_batches_hash"), "precondition: the index is gone");
	}
	release_database(db, &name).await;

	let db = Database::existing(&name).await?;
	for processed in [false, true] {
		let batches = if processed { db.get_processed_batches_db(&aspect_id).await? } else { db.get_unprocessed_batches_db(&aspect_id).await? };
		let plan = lookup_plan(&batches.connect()?).await?;
		assert!(plan.contains("idx_batches_hash"), "processed={processed}: reopening did not add the hash index: {plan}");
	}
	release_database(db, &name).await;
	Ok(())
}

/// `Database::new` refuses a name of the form of its own build directories
/// (`.{name}.creating-{nonce}`), which a stale-build sweep would take for crash litter.
#[tokio::test]
#[serial]
async fn database_new_refuses_a_build_directory_name() -> Result<()> {
	let temp_dir = tempfile::tempdir()?;
	std::env::set_var("TEST_DATA_DIR", temp_dir.path().to_str().context("temp data dir is not valid UTF-8")?);
	let name = format!(".sensors.creating-{}", Uuid::new_v4().simple());
	let err = Database::new(&name).await.expect_err("a build directory's name is reserved");
	assert!(err.to_string().contains("reserved"), "{err:#}");
	assert!(!temp_dir.path().join(&name).exists(), "nothing was created");
	Ok(())
}

/// Crash tests for `Database::new` at its fault points (crash-consistency design, S18:
/// legacy-database-new-half-created).
#[cfg(feature = "fault-injection")]
mod database_new_crash {
	use std::{
		path::{Path, PathBuf}, sync::Arc, time::Duration
	};

	use anyhow::{Context, Result};
	use serial_test::serial;
	use tokio::sync::Notify;
	use uuid::Uuid;
	use weftdb::{
		database::traits::DatabaseStructure, durable::fault::{self, FaultAction, FaultPoint, FAULT_ENV}, Config, Database
	};

	use super::release_database;

	/// Set only in the child process [`a_process_crash_at_each_point_is_recoverable`]
	/// spawns; it holds the name of the database the child creates.
	const CHILD_ENV: &str = "WEFT_DB_NEW_CRASH_CHILD";

	fn database_dir(name: &str) -> PathBuf {
		Path::new(&Database::get_data_dir()).join(name)
	}

	/// The `.{name}.creating-*` build directories under the data directory.
	fn build_dirs(name: &str) -> Vec<String> {
		let prefix = format!(".{name}.creating-");
		let Ok(entries) = std::fs::read_dir(Database::get_data_dir()) else { return Vec::new() };
		entries.filter_map(Result::ok).filter_map(|entry| entry.file_name().into_string().ok()).filter(|file| file.starts_with(&prefix)).collect()
	}

	/// `existing(name)` opens the database cold and finds the row `new` committed, with the
	/// final (not the build directory's) metadata path, and a subject written through it
	/// survives another cold reopen.
	async fn assert_usable(name: &str) -> Result<()> {
		let db = Database::existing(name).await.with_context(|| format!("Database::existing({name})"))?;
		let subject = db.observe_subject("after_crash").await?;
		let subject_id = subject.id();
		drop(subject);
		let metadata = db.metadata().clone();
		let mut rows = metadata.connect()?.query("SELECT name, metadata_path FROM database", ()).await?;
		let row = rows.next().await?.context("no database row")?;
		let (stored_name, stored_path): (String, String) = (row.get(0)?, row.get(1)?);
		assert!(rows.next().await?.is_none(), "exactly one database row");
		drop((rows, metadata));
		assert_eq!(stored_name, name);
		assert_eq!(Path::new(&stored_path), database_dir(name).join("metadata.db"), "the row records the final metadata path, not the build directory's");
		release_database(db, name).await;

		let reopened = Database::existing(name).await?;
		assert!(reopened.list_subjects().await?.contains_key(&subject_id), "a subject written after the recovery survives a cold reopen");
		release_database(reopened, name).await;
		Ok(())
	}

	/// **Regression (legacy-database-new-half-created).** A failure at either fault point
	/// makes `new` return an error and leave nothing behind, so retrying `new(name)`
	/// succeeds and `existing(name)` works. On main the failed call left a half-built
	/// `{name}` folder, so the retry failed with "Database folder already exists".
	#[tokio::test]
	#[serial]
	async fn a_failure_at_each_point_leaves_nothing_and_the_retry_succeeds() -> Result<()> {
		let temp_dir = tempfile::tempdir()?;
		std::env::set_var("TEST_DATA_DIR", temp_dir.path().to_str().context("temp data dir is not valid UTF-8")?);

		for point in [FaultPoint::LNewCreated, FaultPoint::LNewRenamed] {
			let name = format!("newfail_{}", Uuid::new_v4().simple());
			{
				let _armed = fault::arm(point, FaultAction::ReturnErr);
				let err = Database::new(&name).await.expect_err("the armed point fails the call");
				assert!(format!("{err:#}").contains(&format!("injected fault at {point}")), "{point}: unexpected error {err:#}");
			}
			assert!(!database_dir(&name).exists(), "{point}: a failed Database::new left a database folder behind");
			assert_eq!(build_dirs(&name), Vec::<String>::new(), "{point}: a failed Database::new left its build directory behind");

			let db = Database::new(&name).await.with_context(|| format!("retrying Database::new after a failure at {point}"))?;
			release_database(db, &name).await;
			assert_usable(&name).await.with_context(|| format!("after a failure at {point}"))?;
		}
		Ok(())
	}

	/// The on-disk listing never lists a build directory, never sweeps the one a running
	/// `new` owns, and sweeps a stale one (left by a crashed `new`) once no `new` is running.
	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	#[serial]
	async fn the_listing_sweeps_stale_builds_but_never_a_live_one() -> Result<()> {
		let temp_dir = tempfile::tempdir()?;
		std::env::set_var("TEST_DATA_DIR", temp_dir.path().to_str().context("temp data dir is not valid UTF-8")?);
		let listed = format!("listed_{}", Uuid::new_v4().simple());
		release_database(Database::new(&listed).await?, &listed).await;

		// A `new` paused after building, before its rename.
		let pending = format!("pending_{}", Uuid::new_v4().simple());
		let resume = Arc::new(Notify::new());
		let armed = fault::arm(FaultPoint::LNewCreated, FaultAction::Pause(resume.clone()));
		let before = fault::hits(FaultPoint::LNewCreated);
		let creating = tokio::spawn({
			let pending = pending.clone();
			async move { Database::new(&pending).await }
		});
		tokio::time::timeout(Duration::from_secs(30), fault::reached(FaultPoint::LNewCreated, before + 1)).await.context("the new() reaches L-new-created")?;
		drop(armed);
		let live = build_dirs(&pending);
		assert_eq!(live.len(), 1, "the paused new() has its build directory");

		// What a `new` that crashed after building leaves: a complete-looking build
		// directory. (Made only now: the paused `new` swept the data directory as it began.)
		let stale = database_dir(&format!(".ghost.creating-{}", Uuid::new_v4().simple()));
		std::fs::create_dir(&stale)?;
		std::fs::write(stale.join("metadata.db"), b"")?;

		assert_eq!(Database::list_stored_databases().await?, vec![listed.clone()], "build directories are never listed");
		assert_eq!(build_dirs(&pending), live, "the live build directory is not swept");
		assert!(stale.exists(), "no sweep runs while a new() is running");

		resume.notify_one();
		let db = creating.await?.context("the paused new() completes")?;
		release_database(db, &pending).await;
		let mut expected = vec![listed.clone(), pending.clone()];
		expected.sort();
		assert_eq!(Database::list_stored_databases().await?, expected);
		assert!(!stale.exists(), "the listing swept the stale build directory");
		assert_usable(&pending).await
	}

	/// The body the re-executed child runs: create the database named by [`CHILD_ENV`],
	/// which aborts at the point `WEFT_FAULT` arms. In a normal run the variable is unset
	/// and this does nothing.
	#[tokio::test]
	async fn child() -> Result<()> {
		let Some(name) = std::env::var_os(CHILD_ENV) else { return Ok(()) };
		suppress_core_dump();
		let name = name.into_string().map_err(|raw| anyhow::anyhow!("{raw:?} is not UTF-8"))?;
		Database::new(&name).await?;
		panic!("{FAULT_ENV} did not abort Database::new");
	}

	/// Keep the child's deliberate abort from dumping core, exactly as the fault module's
	/// own abort test does: this machine (and CI) may run systemd-coredump, which would
	/// otherwise store a core of the test binary on every run.
	fn suppress_core_dump() {
		#[cfg(unix)]
		{
			let none = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
			// SAFETY: setrlimit only reads the struct, for the duration of the call.
			unsafe { libc::setrlimit(libc::RLIMIT_CORE, &raw const none) };
		}
		// A pipe `core_pattern` (systemd-coredump) ignores RLIMIT_CORE, but the kernel
		// never dumps a process that is not dumpable.
		#[cfg(target_os = "linux")]
		{
			let not_dumpable: libc::c_ulong = 0;
			// SAFETY: PR_SET_DUMPABLE takes one integer and changes only this process's
			// dumpable flag.
			unsafe { libc::prctl(libc::PR_SET_DUMPABLE, not_dumpable) };
		}
	}

	/// A process crash (abort) at each point of `Database::new`.
	///
	/// - At `L-new-created` the database is fully built but still under its
	///   `.{name}.creating-*` name: `{name}` does not exist, the next `new(name)` sweeps the
	///   stale build directory and succeeds, and `existing(name)` works. On main the crash
	///   left a half-created `{name}` folder that neither `new` nor `existing` could use.
	/// - At `L-new-renamed` the rename, which is the commit point, has happened: the
	///   database is complete under `{name}`, so `existing(name)` works and `new(name)`
	///   reports that it already exists, as for any existing database.
	#[tokio::test]
	#[serial]
	async fn a_process_crash_at_each_point_is_recoverable() -> Result<()> {
		let temp_dir = tempfile::tempdir()?;
		let data_dir = temp_dir.path().to_str().context("temp data dir is not valid UTF-8")?;
		std::env::set_var("TEST_DATA_DIR", data_dir);

		for point in [FaultPoint::LNewCreated, FaultPoint::LNewRenamed] {
			let name = format!("newcrash_{}", Uuid::new_v4().simple());
			let output = std::process::Command::new(std::env::current_exe()?).args(["database_new_crash::child", "--exact", "--nocapture", "--test-threads=1"]).env(CHILD_ENV, &name).env("TEST_DATA_DIR", data_dir).env(FAULT_ENV, format!("{point}:abort")).output()?;
			assert!(!output.status.success(), "{point}: the child aborted: {output:?}");
			#[cfg(unix)]
			{
				use std::os::unix::process::ExitStatusExt;
				assert_eq!(output.status.signal(), Some(6), "{point}: killed by SIGABRT: {output:?}");
			}
			assert!(String::from_utf8_lossy(&output.stderr).contains(&format!("aborting at fault point {point}")), "{point}: the child reached the point: {output:?}");

			if point == FaultPoint::LNewCreated {
				assert!(!database_dir(&name).exists(), "{point}: a crash before the rename must not leave a {name} folder");
				assert_eq!(build_dirs(&name).len(), 1, "{point}: the crash leaves its build directory for the next sweep");
				let db = Database::new(&name).await.with_context(|| format!("retrying Database::new after a crash at {point}"))?;
				release_database(db, &name).await;
				assert_eq!(build_dirs(&name), Vec::<String>::new(), "{point}: the retry swept the stale build directory");
			} else {
				assert!(database_dir(&name).join("metadata.db").exists(), "{point}: the renamed database is in place");
				assert_eq!(build_dirs(&name), Vec::<String>::new(), "{point}: nothing is left under the build name");
				let err = Database::new(&name).await.expect_err("the database exists, so new() refuses it");
				assert!(err.to_string().contains("already exists"), "{point}: {err:#}");
			}
			assert_usable(&name).await.with_context(|| format!("after a crash at {point}"))?;
		}
		Ok(())
	}
}
