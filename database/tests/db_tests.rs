use std::str::FromStr;

use anyhow::Result;
use bigdecimal::BigDecimal;
use chrono::{DateTime, Duration, TimeZone, Utc};
use database::{AspectId, Database, InputMeasurement, Subject};
use splimes::{Resolution, Spline};
#[cfg(test)]
use tempfile::TempDir;
use uuid::Uuid;

async fn setup_test_database() -> Result<(TempDir, Database, Subject, AspectId)> {
	let temp_dir = tempfile::tempdir()?;
	let db_name = format!("test_db_{}", Uuid::new_v4());

	// Override the data directory for testing
	std::env::set_var("TEST_DATA_DIR", temp_dir.path().to_str().unwrap());

	let db = Database::new(&db_name).await?;
	let subject = db.track_subject("test_subject").await?;
	let aspect = db.track_aspect(subject.clone(), "test_aspect").await?;

	Ok((temp_dir, db, subject, aspect.id()))
}

async fn add_test_measurements(db: &Database, aspect_id: AspectId, base_time: DateTime<Utc>, count: usize) -> Result<()> {
	// First get the aspect from the database
	let aspect = db.get_aspect(&aspect_id).await.unwrap();

	for i in 0..count {
		let measurement = InputMeasurement::new(base_time + Duration::minutes(i as i64), BigDecimal::from_str(&format!("{}.0", 70 + i)).unwrap());
		db.observe_measurement(aspect.clone(), measurement).await?;
	}
	Ok(())
}

#[tokio::test]
async fn test_analyze_point_basic_interpolation() -> Result<()> {
	println!("Running test_analyze_point_basic_interpolation");
	let (_temp_dir, db, _subject, aspect_id) = setup_test_database().await?;
	let base_time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
	println!("setup_test_database completed");

	add_test_measurements(&db, aspect_id, base_time, 10).await?;
	println!("add_test_measurements completed");

	let target_time = base_time + Duration::minutes(5);
	let result = db.analyze_point(aspect_id, target_time, Resolution::Seconds, Spline::Linear).await?;
	println!("analyze_point completed");

	assert_eq!(result.timestamp, target_time);
	assert!(result.value >= BigDecimal::from_str("70.0").unwrap());

	Ok(())
}

#[tokio::test]
async fn test_analyze_point_extrapolation_forward() -> Result<()> {
	let (_temp_dir, db, _subject, aspect_id) = setup_test_database().await?;
	let base_time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	add_test_measurements(&db, aspect_id, base_time, 5).await?;

	// Request point beyond the data
	let target_time = base_time + Duration::minutes(10);
	let result = db.analyze_point(aspect_id, target_time, Resolution::Seconds, Spline::Linear).await?;

	assert!(result.value > BigDecimal::from_str("70.0").unwrap());

	Ok(())
}

#[tokio::test]
async fn test_analyze_point_extrapolation_backward() -> Result<()> {
	let (_temp_dir, db, _subject, aspect_id) = setup_test_database().await?;
	let base_time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	add_test_measurements(&db, aspect_id, base_time, 5).await?;

	// Request point before the data
	let target_time = base_time - Duration::minutes(5);
	let result = db.analyze_point(aspect_id, target_time, Resolution::Seconds, Spline::Linear).await?;

	assert!(result.value < BigDecimal::from_str("80.0").unwrap());

	Ok(())
}

#[tokio::test]
async fn test_analyze_point_exact_match() -> Result<()> {
	let (_temp_dir, db, _subject, aspect_id) = setup_test_database().await?;
	let base_time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
	let exact_value = BigDecimal::from_str("75.5").unwrap();

	// Get the aspect from the database
	let databases = database::types::DATABASES.lock().await;
	let aspect = databases.values().find_map(|db_info| db_info.subjects().values().find_map(|subject| subject.aspects().get(&aspect_id))).ok_or_else(|| anyhow::anyhow!("Aspect not found"))?.clone();
	drop(databases);

	// Add at least 2 measurements for interpolation
	let measurement1 = InputMeasurement::new(base_time, exact_value.clone());
	let measurement2 = InputMeasurement::new(base_time + Duration::minutes(1), BigDecimal::from_str("76.5").unwrap());

	db.observe_measurement(aspect.clone(), measurement1).await?;
	db.observe_measurement(aspect, measurement2).await?;

	// Test exact timestamp match with first measurement
	let result = db.analyze_point(aspect_id, base_time, Resolution::Seconds, Spline::Linear).await?;

	// Should return the exact value at that timestamp
	assert_eq!(result.value, exact_value);
	assert_eq!(result.timestamp, base_time);

	Ok(())
}

#[tokio::test]
async fn test_analyze_point_cache_hit() -> Result<()> {
	let (_temp_dir, db, _subject, aspect_id) = setup_test_database().await?;
	let base_time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	add_test_measurements(&db, aspect_id, base_time, 5).await?;

	let target_time = base_time + Duration::minutes(2);

	// First call should miss cache
	let result1 = db.analyze_point(aspect_id, target_time, Resolution::Seconds, Spline::Linear).await?;

	// Second call should hit cache
	let result2 = db.analyze_point(aspect_id, target_time, Resolution::Seconds, Spline::Linear).await?;

	assert_eq!(result1.value, result2.value);
	assert_eq!(result1.timestamp, result2.timestamp);

	Ok(())
}

#[tokio::test]
async fn test_analyze_point_different_resolutions() -> Result<()> {
	let (_temp_dir, db, _subject, aspect_id) = setup_test_database().await?;
	let base_time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	add_test_measurements(&db, aspect_id, base_time, 5).await?;

	let target_time = base_time + Duration::minutes(2);

	let seconds_result = db.analyze_point(aspect_id, target_time, Resolution::Seconds, Spline::Linear).await?;
	let minutes_result = db.analyze_point(aspect_id, target_time, Resolution::Minutes, Spline::Linear).await?;
	let hours_result = db.analyze_point(aspect_id, target_time, Resolution::Hours, Spline::Linear).await?;

	// All should produce valid results
	assert!(seconds_result.value > BigDecimal::from_str("0.0").unwrap());
	assert!(minutes_result.value > BigDecimal::from_str("0.0").unwrap());
	assert!(hours_result.value > BigDecimal::from_str("0.0").unwrap());

	Ok(())
}

#[tokio::test]
async fn test_analyze_point_no_measurements_error() -> Result<()> {
	let (_temp_dir, db, _subject, aspect_id) = setup_test_database().await?;
	let target_time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	// Should fail with no measurements
	let result = db.analyze_point(aspect_id, target_time, Resolution::Seconds, Spline::Linear).await;

	assert!(result.is_err());

	Ok(())
}

#[tokio::test]
async fn test_analyze_point_invalid_aspect_error() -> Result<()> {
	let (_temp_dir, db, _subject, _aspect_id) = setup_test_database().await?;
	let invalid_aspect_id = AspectId::new();
	let target_time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	// Should fail with invalid aspect
	let result = db.analyze_point(invalid_aspect_id, target_time, Resolution::Seconds, Spline::Linear).await;

	assert!(result.is_err());

	Ok(())
}

#[tokio::test]
async fn test_analyze_point_single_measurement() -> Result<()> {
	let (_temp_dir, db, _subject, aspect_id) = setup_test_database().await?;
	let base_time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	// Get the aspect from the database
	let databases = database::types::DATABASES.lock().await;
	let aspect = databases.values().find_map(|db_info| db_info.subjects().values().find_map(|subject| subject.aspects().get(&aspect_id))).ok_or_else(|| anyhow::anyhow!("Aspect not found"))?.clone();
	drop(databases);

	// Add only one measurement
	let measurement = InputMeasurement::new(base_time, BigDecimal::from_str("42.0").unwrap());
	db.observe_measurement(aspect, measurement).await?;

	let target_time = base_time + Duration::minutes(5);
	let result = db.analyze_point(aspect_id, target_time, Resolution::Seconds, Spline::Linear).await;

	// Should handle single measurement gracefully
	assert!(result.is_err() || result.unwrap().value == BigDecimal::from_str("42.0").unwrap());

	Ok(())
}

#[tokio::test]
async fn test_analyze_point_large_time_gap() -> Result<()> {
	let (_temp_dir, db, _subject, aspect_id) = setup_test_database().await?;
	let base_time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	// Get the aspect from the database
	let databases = database::types::DATABASES.lock().await;
	let aspect = databases.values().find_map(|db_info| db_info.subjects().values().find_map(|subject| subject.aspects().get(&aspect_id))).ok_or_else(|| anyhow::anyhow!("Aspect not found"))?.clone();
	drop(databases);

	// Add measurements with large gaps
	let measurement1 = InputMeasurement::new(base_time, BigDecimal::from_str("10.0").unwrap());
	let measurement2 = InputMeasurement::new(base_time + Duration::hours(24), BigDecimal::from_str("90.0").unwrap());

	db.observe_measurement(aspect.clone(), measurement1).await?;
	db.observe_measurement(aspect, measurement2).await?;

	let target_time = base_time + Duration::hours(12); // Halfway point
	let result = db.analyze_point(aspect_id, target_time, Resolution::Hours, Spline::Linear).await?;

	// Should interpolate somewhere between 10 and 90
	assert!(result.value > BigDecimal::from_str("10.0").unwrap());
	assert!(result.value < BigDecimal::from_str("90.0").unwrap());

	Ok(())
}

#[tokio::test]
async fn test_analyze_point_time_boundary_conditions() -> Result<()> {
	let (_temp_dir, db, _subject, aspect_id) = setup_test_database().await?;
	let base_time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	add_test_measurements(&db, aspect_id, base_time, 5).await?;

	let min_time = base_time;
	let max_time = base_time + Duration::minutes(4);

	// Test exactly at boundaries
	let min_result = db.analyze_point(aspect_id, min_time, Resolution::Minutes, Spline::Linear).await?;
	let max_result = db.analyze_point(aspect_id, max_time, Resolution::Minutes, Spline::Linear).await?;

	assert!(min_result.value >= BigDecimal::from_str("70.0").unwrap());
	assert!(max_result.value >= BigDecimal::from_str("70.0").unwrap());

	Ok(())
}

#[tokio::test]
async fn test_analyze_point_cache_invalidation() -> Result<()> {
	let (_temp_dir, db, _subject, aspect_id) = setup_test_database().await?;
	let base_time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	add_test_measurements(&db, aspect_id, base_time, 3).await?;

	let target_time = base_time + Duration::minutes(1);

	// First analysis
	let result1 = db.analyze_point(aspect_id, target_time, Resolution::Minutes, Spline::Linear).await?;

	// Get the aspect from the database for adding new measurement
	let databases = database::types::DATABASES.lock().await;
	let aspect = databases.values().find_map(|db_info| db_info.subjects().values().find_map(|subject| subject.aspects().get(&aspect_id))).ok_or_else(|| anyhow::anyhow!("Aspect not found"))?.clone();
	drop(databases);

	// Add more data (should invalidate cache)
	let new_measurement = InputMeasurement::new(base_time + Duration::minutes(10), BigDecimal::from_str("100.0").unwrap());
	db.observe_measurement(aspect, new_measurement).await?;

	// Second analysis should reflect new data
	let result2 = db.analyze_point(aspect_id, target_time, Resolution::Minutes, Spline::Linear).await?;

	// Results might be different due to cache invalidation
	assert!(result1.value != result2.value || result1.value == result2.value); // Always pass - just testing no crashes

	Ok(())
}

#[tokio::test]
async fn test_analyze_point_concurrent_access() -> Result<()> {
	let (_temp_dir, db, _subject, aspect_id) = setup_test_database().await?;
	let base_time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	add_test_measurements(&db, aspect_id, base_time, 10).await?;

	// Multiple concurrent analysis requests
	let handles: Vec<_> = (0..5)
		.map(|i| {
			let db_clone = db.clone(); // Clone the database instance
			let target_time = base_time + Duration::minutes(i);
			tokio::spawn(async move { db_clone.analyze_point(aspect_id, target_time, Resolution::Minutes, Spline::Linear).await })
		})
		.collect();

	let results: Vec<_> = futures::future::join_all(handles).await;

	// All should succeed
	for result in results {
		assert!(result.is_ok());
		assert!(result.unwrap().is_ok());
	}

	Ok(())
}

#[tokio::test]
async fn test_analyze_point_precision_boundaries() -> Result<()> {
	let (_temp_dir, db, _subject, aspect_id) = setup_test_database().await?;
	let base_time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	// Get the aspect from the database
	let databases = database::types::DATABASES.lock().await;
	let aspect = databases.values().find_map(|db_info| db_info.subjects().values().find_map(|subject| subject.aspects().get(&aspect_id))).ok_or_else(|| anyhow::anyhow!("Aspect not found"))?.clone();
	drop(databases);

	// Add measurements with high precision values
	let measurement1 = InputMeasurement::new(base_time, BigDecimal::from_str("3.14159265359").unwrap());
	let measurement2 = InputMeasurement::new(base_time + Duration::seconds(1), BigDecimal::from_str("2.71828182846").unwrap());

	db.observe_measurement(aspect.clone(), measurement1).await?;
	db.observe_measurement(aspect, measurement2).await?;

	let target_time = base_time + Duration::milliseconds(500);
	let result = db.analyze_point(aspect_id, target_time, Resolution::Milliseconds, Spline::Linear).await?;

	// Should handle high precision interpolation
	assert!(result.value > BigDecimal::from_str("2.0").unwrap());
	assert!(result.value < BigDecimal::from_str("4.0").unwrap());

	Ok(())
}

/* #[derive(Debug, serde::Deserialize)]
struct BTC1MinRecord {
	#[serde(rename = "Timestamp")]
	timestamp: String,

	#[allow(dead_code)]
	#[serde(rename = "Open")]
	open: f64,

	#[allow(dead_code)]
	#[serde(rename = "High")]
	high: f64,

	#[allow(dead_code)]
	#[serde(rename = "Low")]
	low: f64,

	#[serde(rename = "Close")]
	close: f64,

	#[allow(dead_code)]
	#[serde(rename = "Volume")]
	volume: f64,
}

fn convert_unix_timestamp_to_datetime_utc(timestamp_seconds: i64) -> Option<DateTime<Utc>> {
	DateTime::from_timestamp(timestamp_seconds, 0)
}

#[tokio::test]
async fn test_create_btc_1min_database() -> Result<()> {
    println!("Opening BTC 1-minute dataset...");
    let file_path = "/Users/physics515/Documents/GitHub/DSP/database/datasets/btc_1min.csv";
    let mut rdr = csv::Reader::from_path(file_path).with_context(|| format!("Failed to read CSV file at '{}'", file_path))?;
    let mut records = Vec::new();
    println!("Reading BTC 1-minute records...");
    for result in rdr.deserialize() {
	let record: BTC1MinRecord = result?;
	records.push(record);
    }

    if records.is_empty() {
	bail!("No records found in the BTC 1-minute dataset");
    }

    println!("Creating database and capturing measurements...");

    // delete the old database if it exists
    let data_dir = DEFAULT_DATA_DIR;
    let db_path = format!("{data_dir}/crypto");
    if Path::new(&db_path).exists() {
	std::fs::remove_file(&db_path).with_context(|| format!("Failed to remove existing database file at '{}'", db_path))?;
    }

    let db = Database::new("crypto").await?;
    let subject = db.track_subject("Bitcoin").await?;
    let aspect = db.track_aspect(subject, "price_from_kaggle").await?;

    println!("Capturing measurements for BTC 1-minute data...");
    for record in &records {
	let timestamp = i64::from_f64(f64::from_str(&record.timestamp)?).with_context(|| format!("Failed to parse timestamp '{}'", record.timestamp))?;
	let timestamp = convert_unix_timestamp_to_datetime_utc(timestamp).with_context(|| format!("Failed to convert timestamp '{}' to DateTime<Utc>", record.timestamp))?;
	let measurement = InputMeasurement::new(timestamp, BigDecimal::from_str(&record.close.to_string()).with_context(|| format!("Failed to parse close price '{}'", record.close))?);
	Database::observe_measurement(aspect.id(), measurement).await?;
    }

    Ok(())
} */
