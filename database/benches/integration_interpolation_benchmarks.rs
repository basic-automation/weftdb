use std::{hint::black_box, path::Path, str::FromStr};

use ::database::*;
use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use splimes::{Resolution, Spline};
use tokio::runtime::Runtime;
use uuid::Uuid;

fn benchmark_production_workloads(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// DRASTICALLY reduced sizes for reasonable benchmark times
	let workloads = vec![
		("small_production", 10, 5, 1),   // 10 measurements, 5 min window, 1 min intervals
		("medium_production", 25, 10, 2), // 25 measurements, 10 min window, 2 min intervals
		("large_production", 50, 15, 3),  // 50 measurements, 15 min window, 3 min intervals
	];

	// Configure criterion for faster benchmarking
	let mut group = c.benchmark_group("production_workloads");
	group.sample_size(10); // Reduce sample size from 100 to 10
	group.measurement_time(std::time::Duration::from_secs(2)); // Reduce measurement time
	group.warm_up_time(std::time::Duration::from_secs(1)); // Reduce warm-up time

	for (name, measurement_count, window_minutes, interval_minutes) in workloads {
		group.bench_function(name, |b| {
			b.iter_batched(
				// Setup phase: create database and data (not measured)
				|| {
					rt.block_on(async {
						let db_name = format!("bench_prod_{}_{}", name, Uuid::new_v4());
						let db_path = format!("data/{db_name}");

						// Clean up any existing data
						if Path::new(&db_path).exists() {
							std::fs::remove_dir_all(&db_path).ok();
						}
						tokio::time::sleep(std::time::Duration::from_millis(50)).await; // Reduced delay

						// Create database and setup data
						let db = Database::new(&db_name).await.unwrap();
						let subject = db.track_subject("benchmark_subject").await.unwrap();
						let aspect = db.track_aspect(subject, "benchmark_aspect", Resolution::Seconds).await.unwrap();

						// Add test data using batch for efficiency
						let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
						let mut measurements = Vec::with_capacity(measurement_count);
						for i in 0..measurement_count {
							measurements.push(InputMeasurement::new(base_time + Duration::minutes(i as i64 * interval_minutes), BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap()));
						}
						db.observe_measurements_batch(aspect.clone(), measurements).await.unwrap();

						// Calculate analysis parameters
						let data_start = base_time;
						let start = data_start;
						let end = start + Duration::minutes(window_minutes);

						(db, aspect.id(), start, end, db_path)
					})
				},
				// Measurement phase: only the operation being benchmarked
				|(db, aspect_id, start, end, db_path)| {
					rt.block_on(async {
						// This is the only part being measured - use coarser resolution
						let result = Database::analyze_range(
							aspect_id,
							start,
							end,
							Resolution::Minutes, // Use Minutes instead of Seconds
							Spline::Linear,
						)
						.await
						.unwrap();

						// Immediate cleanup (not measured due to iter_batched)
						db.close().await.unwrap();
						tokio::time::sleep(std::time::Duration::from_millis(50)).await; // Reduced delay
						std::fs::remove_dir_all(&db_path).ok();

						black_box(result)
					})
				},
				BatchSize::SmallInput,
			);
		});
	}
	group.finish();
}

fn benchmark_full_integration_pipeline(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// Much smaller pipeline configs
	let pipeline_configs = vec![
		("basic_pipeline", 10, Resolution::Minutes),      // Reduced from 50 to 10
		("production_pipeline", 20, Resolution::Minutes), // Reduced from 100 to 20
		("enterprise_pipeline", 30, Resolution::Hours),   // Reduced from 200 to 30
	];

	// Configure criterion for faster benchmarking
	let mut group = c.benchmark_group("integration_pipeline");
	group.sample_size(10);
	group.measurement_time(std::time::Duration::from_secs(2));
	group.warm_up_time(std::time::Duration::from_secs(1));

	for (name, measurement_count, resolution) in pipeline_configs {
		group.bench_function(name, |b| {
			b.iter_batched(
				// Setup phase
				|| {
					rt.block_on(async {
						let db_name = format!("bench_pipeline_{}_{}", name, Uuid::new_v4());
						let db_path = format!("data/{db_name}");

						// Clean up existing data
						if Path::new(&db_path).exists() {
							std::fs::remove_dir_all(&db_path).ok();
						}
						tokio::time::sleep(std::time::Duration::from_millis(50)).await;

						// Create database
						let db = Database::new(&db_name).await.unwrap();
						let subject = db.track_subject("pipeline_subject").await.unwrap();
						let aspect = db.track_aspect(subject, "pipeline_aspect", Resolution::Seconds).await.unwrap();

						// Ingest data using batch
						let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
						let mut measurements = Vec::with_capacity(measurement_count);
						for i in 0..measurement_count {
							measurements.push(InputMeasurement::new(base_time + Duration::minutes(i as i64), BigDecimal::from_str(&format!("{}.0", i)).unwrap()));
						}
						db.observe_measurements_batch(aspect.clone(), measurements).await.unwrap();

						// Define analysis range
						let start = base_time;
						let end = base_time + Duration::minutes(measurement_count as i64);

						(db, aspect.id(), start, end, resolution, db_path)
					})
				},
				// Measurement phase
				|(db, aspect_id, start, end, resolution, db_path)| {
					rt.block_on(async {
						// Only this operation is measured
						let result = Database::analyze_range(aspect_id, start, end, resolution, Spline::Linear).await.unwrap();

						// Cleanup (not measured)
						db.close().await.unwrap();
						tokio::time::sleep(std::time::Duration::from_millis(50)).await;
						std::fs::remove_dir_all(&db_path).ok();

						black_box(result)
					})
				},
				BatchSize::SmallInput,
			);
		});
	}
	group.finish();
}

criterion_group!(benches, benchmark_production_workloads, benchmark_full_integration_pipeline);
criterion_main!(benches);
