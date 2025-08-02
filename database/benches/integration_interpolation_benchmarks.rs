use std::{hint::black_box, path::Path, str::FromStr};

use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, Criterion};
use database::{Database, InputMeasurement};
use splimes::{Resolution, Spline};
use tokio::runtime::Runtime;
use uuid::Uuid;

fn benchmark_production_workloads(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// Simulate production workload patterns with reduced sizes for stability
	let workloads = vec![
		("iot_sensor_data", 50, 1, 30, Resolution::Seconds),    // Further reduced
		("financial_ticks", 100, 1, 60, Resolution::Seconds),   // Further reduced
		("monitoring_metrics", 40, 2, 60, Resolution::Minutes), // Further reduced
	];

	for (name, measurement_count, interval_minutes, window_minutes, resolution) in workloads {
		c.bench_function(name, |b| {
			// Create a single database per benchmark function
			let db_name = format!("bench_prod_{}_{}", name, Uuid::new_v4());
			let db_path = format!("data/{db_name}");
			let (_db, aspect_id) = rt.block_on(async {
				// Clean up any existing test data if exists
				if Path::new(&db_path).exists() {
					if let Err(e) = std::fs::remove_dir_all(&db_path) {
						eprintln!("Warning: Failed to remove existing directory {}: {}", db_path, e);
					}
					tokio::time::sleep(std::time::Duration::from_millis(50)).await;
				}

				// Create database and setup data
				let db = Database::new(&db_name).await.unwrap();
				let subject = db.track_subject("benchmark_subject").await.unwrap();
				let aspect = db.track_aspect(subject, "benchmark_aspect").await.unwrap();

				// Add test data once using batch for efficiency
				let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
				let mut measurements = Vec::with_capacity(measurement_count);
				for i in 0..measurement_count {
					measurements.push(InputMeasurement::new(base_time + Duration::minutes(i as i64 * interval_minutes), BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap()));
				}
				db.observe_measurements_batch(aspect.clone(), measurements).await.unwrap();

				(db, aspect.id())
			});

			b.iter(|| {
				rt.block_on(async {
					// Calculate proper time range
					let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
					let data_start = base_time;
					let data_end = base_time + Duration::minutes((measurement_count as i64 - 1) * interval_minutes);
					let data_span_minutes = (data_end - data_start).num_minutes();
					let actual_window_minutes = window_minutes.max(data_span_minutes + 10);

					let start = data_start;
					let end = start + Duration::minutes(actual_window_minutes);

					// Perform analysis using Database API
					let result = Database::analyze_range(
						aspect_id,
						start,
						end,
						resolution,
						Spline::Linear, // Production typically uses linear
					)
					.await
					.unwrap();

					black_box(result)
				})
			});

			// Cleanup after all iterations
			rt.block_on(async {
				_db.close().await.unwrap();

				// Delay to ensure handles are released
				tokio::time::sleep(std::time::Duration::from_millis(200)).await;

				if Path::new(&db_path).exists() {
					if let Err(e) = std::fs::remove_dir_all(&db_path) {
						eprintln!("Warning: Failed to remove directory after benchmark {}: {}", db_path, e);
					}
				}
			});
		});
	}
}

fn benchmark_full_integration_pipeline(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// Define pipeline configurations with reduced sizes
	let pipelines = vec![
		("real_time_small", 50, 1, 30, Resolution::Seconds), // Reduced size
		("batch_medium", 100, 5, 60, Resolution::Minutes),   // Reduced size
	];

	for (name, measurement_count, interval_minutes, window_minutes, resolution) in pipelines {
		let mut group = c.benchmark_group("integration_pipeline");
		group.sample_size(10);
		group.measurement_time(std::time::Duration::from_secs(10));

		group.bench_function(name, |b| {
			b.iter(|| {
				rt.block_on(async {
					// Generate unique name for each iteration to avoid conflicts
					let db_name = format!("bench_pipe_{}_{}", name, Uuid::new_v4());
					let db_path = format!("data/{db_name}");

					// Clean up if exists
					if Path::new(&db_path).exists() {
						if let Err(e) = std::fs::remove_dir_all(&db_path) {
							eprintln!("Warning: Failed to remove directory {}: {}", db_path, e);
						}
						// Increased delay to ensure file system sync and handles release
						tokio::time::sleep(std::time::Duration::from_millis(200)).await;
					}

					// Create database
					let db = match Database::new(&db_name).await {
						Ok(db) => db,
						Err(e) => panic!("Failed to create database: {}", e),
					};

					// Setup subject and aspect
					let subject = db.track_subject("pipeline_subject").await.unwrap();
					let aspect = db.track_aspect(subject, "pipeline_aspect").await.unwrap();

					// Ingest data using batch if possible
					let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
					let mut measurements = Vec::with_capacity(measurement_count);
					for i in 0..measurement_count {
						measurements.push(InputMeasurement::new(base_time + Duration::minutes(i as i64 * interval_minutes), BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap()));
					}
					db.observe_measurements_batch(aspect.clone(), measurements).await.unwrap();

					// Calculate time range
					let data_start = base_time;
					let data_end = base_time + Duration::minutes((measurement_count as i64 - 1) * interval_minutes);
					let data_span_minutes = (data_end - data_start).num_minutes();
					let actual_window_minutes = window_minutes.max(data_span_minutes + 10);

					let start = data_start;
					let end = start + Duration::minutes(actual_window_minutes);

					// Perform analysis
					let result = Database::analyze_range(aspect.id(), start, end, resolution, Spline::Linear).await.unwrap();

					// Explicit cleanup in each iteration
					db.close().await.unwrap();

					// Small delay before deletion
					tokio::time::sleep(std::time::Duration::from_millis(200)).await;
					if let Err(e) = std::fs::remove_dir_all(&db_path) {
						eprintln!("Warning: Failed to remove directory after iteration {}: {}", db_path, e);
					}

					black_box(result)
				})
			});
		});
		group.finish();
	}
}

criterion_group!(benches, benchmark_production_workloads, benchmark_full_integration_pipeline);
criterion_main!(benches);
