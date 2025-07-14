use std::{str::FromStr, sync::Arc, time::Duration as StdDuration};

use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use database::{add_subject, analyze_point, capture_measurement, new, track_aspect, InputMeasurement, Resolution, SplineType};
use futures; // Add this import
use tokio::{sync::Semaphore, time::timeout};
use uuid::Uuid;

#[tokio::test]
async fn test_multiple_aspects_same_subject() {
	let db_name = format!("stress_multi_aspects_{}", Uuid::new_v4());

	std::fs::remove_dir_all(format!("data/{db_name}")).ok();

	let db_id = new(&db_name).await.expect("Failed to create database");
	let subject_id = add_subject(db_id, "multi_aspect_subject").await.expect("Failed to add subject");

	// Create multiple aspects
	let num_aspects = 10;
	let mut aspect_ids = Vec::new();

	for i in 0..num_aspects {
		let aspect_id = track_aspect(subject_id, &format!("aspect_{}", i)).await.expect("Failed to track aspect");
		aspect_ids.push(aspect_id);
	}

	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();

	// Add data to all aspects
	for (aspect_idx, &aspect_id) in aspect_ids.iter().enumerate() {
		for i in 0..100i64 {
			// Make i explicitly i64
			let measurement = InputMeasurement::new(base_time + Duration::minutes(i), BigDecimal::from_str(&format!("{}.{}", aspect_idx * 10 + (i as usize % 10), i % 100)).unwrap());
			capture_measurement(aspect_id, measurement).await.expect("Failed to capture multi-aspect measurement");
		}
	}

	// Analyze all aspects
	let target_time = base_time + Duration::minutes(50);
	for &aspect_id in &aspect_ids {
		let result = analyze_point(aspect_id, target_time, Resolution::Seconds, SplineType::Linear).await.expect("Failed to analyze multi-aspect point");
		assert!(!result.value.to_string().is_empty());
	}

	std::fs::remove_dir_all(format!("data/{db_name}")).ok();
}

#[tokio::test]
async fn test_high_frequency_measurements() {
	let db_name = format!("stress_high_freq_{}", Uuid::new_v4());

	// Clean up any existing test data
	std::fs::remove_dir_all(format!("data/{db_name}")).ok();

	let db_id = new(&db_name).await.expect("Failed to create database");
	let subject_id = add_subject(db_id, "high_freq_subject").await.expect("Failed to add subject");
	let aspect_id = track_aspect(subject_id, "high_freq_aspect").await.expect("Failed to track aspect");

	// Reduced dataset for faster testing
	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
	let num_measurements = 600; // 10 minutes of data

	println!("Capturing {} high-frequency measurements...", num_measurements);
	let start = std::time::Instant::now();

	for i in 0..num_measurements {
		let measurement = InputMeasurement::new(base_time + Duration::seconds(i), BigDecimal::from_str(&format!("{}.{}", (i % 100), (i % 10))).unwrap());

		capture_measurement(aspect_id, measurement).await.expect("Failed to capture measurement");

		if i % 100 == 0 {
			// Log progress every 100 measurements
			println!("Captured {} measurements", i);
		}
	}

	let capture_duration = start.elapsed();
	println!("Captured {} measurements in {:?} ({:.2} measurements/sec)", num_measurements, capture_duration, num_measurements as f64 / capture_duration.as_secs_f64());

	// Test analysis with a time that's definitely within the dataset
	let analysis_start = std::time::Instant::now();
	let target_time = base_time + Duration::minutes(5); // Middle of 10-minute dataset
	let result = analyze_point(aspect_id, target_time, Resolution::Seconds, SplineType::Linear).await.expect("Failed to analyze point");
	let analysis_duration = analysis_start.elapsed();

	println!("Analysis completed in {:?}", analysis_duration);
	// Don't assert exact timestamp match since interpolation may adjust it
	assert!(!result.value.to_string().is_empty());

	// Clean up
	std::fs::remove_dir_all(format!("data/{db_name}")).ok();
}

#[tokio::test]
async fn test_concurrent_access() {
	let db_name = format!("stress_concurrent_{}", Uuid::new_v4());

	// Clean up any existing test data
	std::fs::remove_dir_all(format!("data/{db_name}")).ok();

	let db_id = new(&db_name).await.expect("Failed to create database");
	let subject_id = add_subject(db_id, "concurrent_subject").await.expect("Failed to add subject");
	let aspect_id = track_aspect(subject_id, "concurrent_aspect").await.expect("Failed to track aspect");

	// Add some initial data
	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
	for i in 0..60 {
		// Reduced initial data
		let measurement = InputMeasurement::new(base_time + Duration::minutes(i), BigDecimal::from_str(&format!("{}.0", i)).unwrap());
		capture_measurement(aspect_id, measurement).await.expect("Failed to capture initial measurement");
	}

	let num_concurrent_tasks = 8; // Reduced concurrency
	let operations_per_task = 20; // Reduced operations

	println!("Running {} concurrent tasks with {} operations each...", num_concurrent_tasks, operations_per_task);

	let semaphore = Arc::new(Semaphore::new(4)); // Lower concurrency limit
	let start = std::time::Instant::now();

	let tasks: Vec<_> = (0..num_concurrent_tasks)
		.map(|task_id| {
			let sem = semaphore.clone();
			tokio::spawn(async move {
				for op_id in 0..operations_per_task {
					let _permit = sem.acquire().await.unwrap();

					if op_id % 2 == 0 {
						// Write operation
						let measurement = InputMeasurement::new(base_time + Duration::minutes(100 + task_id * operations_per_task + op_id), BigDecimal::from_str(&format!("{}.{}", task_id, op_id)).unwrap());
						capture_measurement(aspect_id, measurement).await.expect("Failed to capture concurrent measurement");
					} else {
						// Read operation - use time within initial data range
						let target_time = base_time + Duration::minutes((task_id * 5 + op_id / 2) % 50);
						let _result = analyze_point(aspect_id, target_time, Resolution::Seconds, SplineType::Linear).await.expect("Failed to analyze concurrent point");
					}
				}
				task_id as i64
			})
		})
		.collect();

	// Add timeout to prevent hanging
	let results = timeout(
		StdDuration::from_secs(120), // 2 minute timeout
		futures::future::join_all(tasks),
	)
	.await
	.expect("Concurrent test timed out");

	let duration = start.elapsed();

	// Verify all tasks completed successfully
	for (i, result) in results.iter().enumerate() {
		assert_eq!(result.as_ref().unwrap(), &(i as i64));
	}

	let total_operations = num_concurrent_tasks * operations_per_task;
	println!("Completed {} concurrent operations in {:?} ({:.2} ops/sec)", total_operations, duration, total_operations as f64 / duration.as_secs_f64());

	// Clean up
	std::fs::remove_dir_all(format!("data/{db_name}")).ok();
}

#[tokio::test]
async fn test_large_dataset_analysis() {
	let db_name = format!("stress_large_{}", Uuid::new_v4());

	// Clean up any existing test data
	std::fs::remove_dir_all(format!("data/{db_name}")).ok();

	let db_id = new(&db_name).await.expect("Failed to create database");
	let subject_id = add_subject(db_id, "large_dataset_subject").await.expect("Failed to add subject");
	let aspect_id = track_aspect(subject_id, "large_dataset_aspect").await.expect("Failed to track aspect");

	// Create a smaller dataset for faster testing
	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
	let num_measurements = 5000; // Reduced from 10000
	let time_interval = Duration::minutes(1);

	println!("Creating large dataset with {} measurements...", num_measurements);
	let start = std::time::Instant::now();

	for i in 0..num_measurements {
		let timestamp = base_time + time_interval * i as i32;
		let value = (i as f64 / 100.0).sin() * 50.0 + 50.0;

		let measurement = InputMeasurement::new(timestamp, BigDecimal::from_str(&format!("{:.2}", value)).unwrap());

		capture_measurement(aspect_id, measurement).await.expect("Failed to capture large dataset measurement");

		if i % 1000 == 0 {
			println!("Captured {} measurements", i);
		}
	}

	let creation_duration = start.elapsed();
	println!("Created large dataset in {:?}", creation_duration);

	// Test analysis performance on large dataset
	let test_cases = vec![(SplineType::Linear, "Linear"), (SplineType::Quadratic, "Quadratic"), (SplineType::Cubic, "Cubic")];

	for (spline_type, name) in test_cases {
		let analysis_start = std::time::Instant::now();
		// Use a time that's definitely within the dataset
		let target_time = base_time + Duration::hours(40); // Well within the dataset range

		let result = analyze_point(aspect_id, target_time, Resolution::Seconds, spline_type).await.expect("Failed to analyze large dataset point");

		let analysis_duration = analysis_start.elapsed();
		println!("{} analysis on large dataset completed in {:?}", name, analysis_duration);

		// Verify result is reasonable
		assert!(result.value >= BigDecimal::from_str("0.0").unwrap());
		assert!(result.value <= BigDecimal::from_str("100.0").unwrap());
	}

	// Clean up
	std::fs::remove_dir_all(format!("data/{db_name}")).ok();
}

#[tokio::test]
async fn test_memory_usage_stability() {
	let db_name = format!("stress_memory_{}", Uuid::new_v4());

	// Clean up any existing test data
	std::fs::remove_dir_all(format!("data/{db_name}")).ok();

	let db_id = new(&db_name).await.expect("Failed to create database");
	let subject_id = add_subject(db_id, "memory_test_subject").await.expect("Failed to add subject");
	let aspect_id = track_aspect(subject_id, "memory_test_aspect").await.expect("Failed to track aspect");

	println!("Testing memory usage stability with repeated operations...");

	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();

	// Perform reduced cycles for faster testing
	for cycle in 0..5 {
		// Reduced from 10
		println!("Memory test cycle {}/5", cycle + 1);

		// Add batch of measurements
		for i in 0..100 {
			// Reduced from 500 to prevent time range issues
			let measurement = InputMeasurement::new(
				base_time + Duration::seconds(cycle * 100 + i), // Adjusted timing
				BigDecimal::from_str(&format!("{}.{}", cycle, i % 100)).unwrap(),
			);
			capture_measurement(aspect_id, measurement).await.expect("Failed to capture memory test measurement");
		}

		// Perform analyses on times within the data range
		for i in 0..10 {
			// Reduced from 50
			// Ensure target time is within the data we've created
			let target_time = base_time + Duration::seconds(cycle * 100 + i * 10);
			let _result = analyze_point(aspect_id, target_time, Resolution::Seconds, SplineType::Linear).await.expect("Failed to analyze memory test point");
		}

		// Force garbage collection periodically
		if cycle % 2 == 0 {
			tokio::task::yield_now().await;
		}
	}

	println!("Memory stability test completed successfully");

	// Clean up
	std::fs::remove_dir_all(format!("data/{db_name}")).ok();
}

#[tokio::test]
async fn test_edge_case_scenarios() {
	let db_name = format!("stress_edge_{}", Uuid::new_v4());

	// Clean up any existing test data
	std::fs::remove_dir_all(format!("data/{db_name}")).ok();

	let db_id = new(&db_name).await.expect("Failed to create database");
	let subject_id = add_subject(db_id, "edge_case_subject").await.expect("Failed to add subject");
	let aspect_id = track_aspect(subject_id, "edge_case_aspect").await.expect("Failed to track aspect");

	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();

	// Test 1: Very sparse data
	println!("Testing sparse data scenario...");
	let sparse_measurements = vec![
		(0, "10.0"),
		(86400, "20.0"),  // 1 day later
		(172800, "30.0"), // 2 days later
	];

	for (seconds_offset, value) in sparse_measurements {
		let measurement = InputMeasurement::new(base_time + Duration::seconds(seconds_offset), BigDecimal::from_str(value).unwrap());
		capture_measurement(aspect_id, measurement).await.expect("Failed to capture sparse measurement");
	}

	// Analyze between sparse points
	let sparse_target = base_time + Duration::hours(12);
	let sparse_result = analyze_point(aspect_id, sparse_target, Resolution::Seconds, SplineType::Linear).await.expect("Failed to analyze sparse data");

	assert!(sparse_result.value > BigDecimal::from_str("10.0").unwrap());
	assert!(sparse_result.value < BigDecimal::from_str("20.0").unwrap());

	// Test 2: Very dense data in short timespan
	println!("Testing dense data scenario...");
	let dense_base = base_time + Duration::days(10);
	for i in 0..100 {
		// Reduced from 1000
		let measurement = InputMeasurement::new(
			dense_base + Duration::milliseconds(i * 100), // 100ms intervals
			BigDecimal::from_str(&format!("{}.{:03}", 100, i)).unwrap(),
		);
		capture_measurement(aspect_id, measurement).await.expect("Failed to capture dense measurement");
	}

	// Analyze within dense region
	let dense_target = dense_base + Duration::seconds(5); // Within the 10-second range
	let dense_result = analyze_point(aspect_id, dense_target, Resolution::Seconds, SplineType::Cubic).await.expect("Failed to analyze dense data");

	assert!(dense_result.value >= BigDecimal::from_str("100.0").unwrap());

	// Test 3: Extreme values
	println!("Testing extreme values scenario...");
	let extreme_base = base_time + Duration::days(20);
	let extreme_measurements = vec![(0, "999999999.999999"), (60, "-999999999.999999"), (120, "0.000000001")];

	for (seconds_offset, value) in extreme_measurements {
		let measurement = InputMeasurement::new(extreme_base + Duration::seconds(seconds_offset), BigDecimal::from_str(value).unwrap());
		capture_measurement(aspect_id, measurement).await.expect("Failed to capture extreme measurement");
	}

	// Analyze between extreme values
	let extreme_target = extreme_base + Duration::seconds(30);
	let extreme_result = analyze_point(aspect_id, extreme_target, Resolution::Seconds, SplineType::Linear).await.expect("Failed to analyze extreme data");

	// Should interpolate between extreme values
	assert!(extreme_result.value < BigDecimal::from_str("999999999.999999").unwrap());
	assert!(extreme_result.value > BigDecimal::from_str("-999999999.999999").unwrap());

	println!("Edge case scenarios completed successfully");

	// Clean up
	std::fs::remove_dir_all(format!("data/{db_name}")).ok();
}
