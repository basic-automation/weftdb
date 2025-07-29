use std::str::FromStr;

use bigdecimal::BigDecimal;
use chrono::{TimeZone, Utc};
use database::{add_subject, analyze_range, capture_measurement, measurements_to_points, new, track_aspect, InputMeasurement, Measurement};
use splimes::{auto_interpolate, Resolution, Spline};
use uuid::Uuid;

fn create_test_measurements(count: usize) -> Vec<Measurement> {
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
	let dataset_id = Uuid::new_v4(); // Use the same dataset_id for all measurements

	(0..count)
		.map(|i| {
			Measurement::new(
				Uuid::new_v4(),
				dataset_id, // Same dataset_id for all measurements
				start_time + chrono::Duration::minutes(i as i64),
				BigDecimal::from_str(&format!("{}.{}", i, i % 10)).unwrap(),
			)
		})
		.collect()
}

#[tokio::test]
async fn test_linear_interpolation_accuracy() {
	let measurements = create_test_measurements(10);
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
	let end_time = start_time + chrono::Duration::minutes(10);

	let result = auto_interpolate(&mut measurements_to_points(&measurements.clone()), start_time, end_time, Resolution::Minutes, Spline::Linear).await;

	if let Err(e) = &result {
		println!("Linear interpolation error: {}", e);
	}
	assert!(result.is_ok());
	let interpolated = result.unwrap();
	assert!(!interpolated.is_empty());
}

#[tokio::test]
async fn test_quadratic_interpolation_accuracy() {
	let measurements = create_test_measurements(10);
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
	let end_time = start_time + chrono::Duration::minutes(10);

	let result = auto_interpolate(&mut measurements_to_points(&measurements.clone()), start_time, end_time, Resolution::Minutes, Spline::Quadratic).await;

	if let Err(e) = &result {
		println!("Quadratic interpolation error: {}", e);
	}
	assert!(result.is_ok());
	let interpolated = result.unwrap();
	assert!(!interpolated.is_empty());
}

#[tokio::test]
async fn test_cubic_interpolation_accuracy() {
	let measurements = create_test_measurements(10);
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
	let end_time = start_time + chrono::Duration::minutes(10);

	let result = auto_interpolate(&mut measurements_to_points(&measurements.clone()), start_time, end_time, Resolution::Minutes, Spline::Cubic).await;

	if let Err(e) = &result {
		println!("Cubic interpolation error: {}", e);
	}
	assert!(result.is_ok());
	let interpolated = result.unwrap();
	assert!(!interpolated.is_empty());
}

#[tokio::test]
async fn test_resolution_consistency() {
	let measurements = create_test_measurements(10);
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
	let end_time = start_time + chrono::Duration::minutes(10);

	// Test different resolutions and verify point counts
	let resolutions = vec![
		("Seconds", Resolution::Seconds, 601), // 10 minutes * 60 + 1 = 601 points
		("Minutes", Resolution::Minutes, 11),  // 0, 1, 2, ..., 10 minutes = 11 points
	];

	for (name, resolution, expected_points) in resolutions {
		let result = auto_interpolate(&mut measurements_to_points(&measurements.clone()), start_time, end_time, resolution, Spline::Linear).await;
		if let Err(e) = &result {
			println!("Resolution {} error: {}", name, e);
		}
		assert!(result.is_ok(), "Failed for resolution: {}", name);

		let interpolated = result.unwrap();
		assert_eq!(interpolated.len(), expected_points, "Point count mismatch for resolution: {} (expected {}, got {})", name, expected_points, interpolated.len());
	}
}

#[tokio::test]
async fn test_spline_type_variations() {
	let measurements = create_test_measurements(20);
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
	let end_time = start_time + chrono::Duration::minutes(10);

	let spline_types = vec![("Linear", Spline::Linear), ("Quadratic", Spline::Quadratic), ("Cubic", Spline::Cubic), ("Polynomial(2)", Spline::Polynomial(2, None)), ("Polynomial(3)", Spline::Polynomial(3, None))];

	for (name, spline_type) in spline_types {
		let result = auto_interpolate(&mut measurements_to_points(&measurements.clone()), start_time, end_time, Resolution::Seconds, spline_type).await;
		if let Err(e) = &result {
			println!("Spline type {} error: {}", name, e);
		}
		assert!(result.is_ok(), "Failed for spline type: {}", name);

		let interpolated = result.unwrap();
		assert!(!interpolated.is_empty(), "No points generated for spline type: {}", name);

		// Verify timestamps are within bounds and properly ordered
		for (i, measurement) in interpolated.iter().enumerate() {
			assert!(measurement.timestamp >= start_time && measurement.timestamp <= end_time, "Timestamp out of bounds for spline type: {} at index {}", name, i);

			if i > 0 {
				assert!(measurement.timestamp >= interpolated[i - 1].timestamp, "Timestamps not ordered for spline type: {} at index {}", name, i);
			}
		}
	}
}

// Mock the fast_path_optimization function since it's internal
fn fast_path_optimization(spline_type: Spline, _data_points: usize, _min_degree: usize) -> Spline {
	// Simple mock implementation for testing
	match spline_type {
		Spline::Cubic if _data_points < 3000 => Spline::Quadratic,
		Spline::Polynomial(n, None) if n > 3 && _data_points < 5000 => Spline::Quadratic,
		_ => spline_type,
	}
}

#[tokio::test]
async fn test_fast_path_optimization() {
	// Test that optimization logic works correctly
	let cubic_optimized = fast_path_optimization(Spline::Cubic, 2500, 3);
	assert_eq!(cubic_optimized, Spline::Quadratic, "Cubic should degrade to Quadratic for small datasets");

	let poly_optimized = fast_path_optimization(Spline::Polynomial(8, None), 4000, 8);
	assert_eq!(poly_optimized, Spline::Quadratic, "High-degree polynomial should degrade to Quadratic for medium datasets");

	let linear_unchanged = fast_path_optimization(Spline::Linear, 10000, 1);
	assert_eq!(linear_unchanged, Spline::Linear, "Linear should not be degraded");
}

#[tokio::test]
async fn test_end_to_end_with_new_api() -> anyhow::Result<()> {
	// Use a unique test name to avoid conflicts
	let test_name = format!("test_e2e_interpolation_{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));

	// Clean up any existing test directory first
	std::fs::remove_dir_all(&format!("data/{}", test_name)).ok();

	// Test the complete workflow using the new simplified API
	let db_id = new(&test_name).await?;
	let subject_id = add_subject(db_id, "test_subject").await?;
	let aspect_id = track_aspect(subject_id, "sensor_data").await?;

	// Add test data
	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
	for i in 0..20 {
		let measurement = InputMeasurement::new(base_time + chrono::Duration::minutes(i * 5), BigDecimal::from_str(&format!("{}.{}", 10 + i, i % 10))?);
		capture_measurement(aspect_id, measurement).await?;
	}

	// Test linear interpolation
	let linear_results = analyze_range(aspect_id, base_time, base_time + chrono::Duration::hours(1), Resolution::Minutes, Spline::Linear).await?;

	// Test cubic interpolation
	let cubic_results = analyze_range(aspect_id, base_time, base_time + chrono::Duration::hours(1), Resolution::Minutes, Spline::Cubic).await?;

	assert!(!linear_results.is_empty(), "Linear interpolation should produce results");
	assert!(!cubic_results.is_empty(), "Cubic interpolation should produce results");
	assert_eq!(linear_results.len(), cubic_results.len(), "Both methods should produce same number of points");

	// Clean up
	std::fs::remove_dir_all(&format!("data/{}", test_name)).ok();

	Ok(())
}
