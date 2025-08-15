use std::{hint::black_box, path::Path, str::FromStr};

use ::database::*;
use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, Criterion};
use splimes::{Resolution, Spline};
use tokio::runtime::Runtime;
use uuid::Uuid;

fn benchmark_production_workloads(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	let workloads = vec![
		("small_production", 100, 30, 5),    // 100 measurements, 30 min window, 5 min intervals
		("medium_production", 500, 120, 15), // 500 measurements, 2 hour window, 15 min intervals
		("large_production", 1000, 240, 30), // 1000 measurements, 4 hour window, 30 min intervals
	];

	for (name, measurement_count, window_minutes, interval_minutes) in workloads {
		c.bench_function(&format!("production_workload_{}", name), |b| {
			// Move db_path outside the async block so it's accessible in cleanup
			let db_name = format!("bench_prod_{}_{}", name, Uuid::new_v4());
			let db_path = format!("data/{db_name}");

			let (db, aspect_id) = rt.block_on(async {
				if Path::new(&db_path).exists() {
					if let Err(e) = std::fs::remove_dir_all(&db_path) {
						eprintln!("Warning: Failed to remove existing directory {}: {}", db_path, e);
					}
					tokio::time::sleep(std::time::Duration::from_millis(50)).await;
				}

				// Create database and setup data
				let db = Database::new(&db_name).await.unwrap();
				let subject = db.track_subject("benchmark_subject").await.unwrap();
				let aspect = db.track_aspect(subject, "benchmark_aspect", Resolution::Seconds).await.unwrap();

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
					let resolution = Resolution::Minutes;

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
				db.close().await.unwrap();

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

	let pipeline_configs = vec![("basic_pipeline", 50, Resolution::Seconds), ("production_pipeline", 100, Resolution::Minutes), ("enterprise_pipeline", 200, Resolution::Hours)];

	for (name, measurement_count, resolution) in pipeline_configs {
		c.bench_function(&format!("integration_pipeline_{}", name), |b| {
			b.iter(|| {
				rt.block_on(async {
					// Create unique database for each iteration
					let db_name = format!("bench_pipeline_{}_{}", name, Uuid::new_v4());
					let db_path = format!("data/{db_name}");

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
					let aspect = db.track_aspect(subject, "pipeline_aspect", Resolution::Seconds).await.unwrap();

					// Ingest data using batch if possible
					let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
					let mut measurements = Vec::with_capacity(measurement_count);
					for i in 0..measurement_count {
						measurements.push(InputMeasurement::new(base_time + Duration::minutes(i as i64), BigDecimal::from_str(&format!("{}.0", i)).unwrap()));
					}
					db.observe_measurements_batch(aspect.clone(), measurements).await.unwrap();

					// Define analysis range
					let start = base_time;
					let end = base_time + Duration::minutes(measurement_count as i64);

					// Perform analysis - use static method call
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
	}
}

criterion_group!(benches, benchmark_production_workloads, benchmark_full_integration_pipeline);
criterion_main!(benches);
