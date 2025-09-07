use std::{hint::black_box, str::FromStr};

use ::database::*;
use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, Criterion};
use splimes::{Resolution, Spline};
use tokio::runtime::Runtime;
use uuid::Uuid;

fn benchmark_optimization_strategies(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// MUCH smaller, more realistic dataset sizes
	let optimization_configs = vec![
		("cpu_standard", 50, 5, Resolution::Minutes),         // Reduced from 200 to 50
		("simd_optimized", 100, 10, Resolution::Minutes),     // Reduced from 500 to 100, changed to Minutes
		("parallel_optimized", 200, 15, Resolution::Minutes), // Reduced from 1000 to 200, changed to Minutes
		("gpu_optimized", 300, 20, Resolution::Minutes),      // Reduced from 1500 to 300, changed to Minutes
	];

	// Create shared database infrastructure to reduce setup overhead
	let shared_db_name = format!("bench_opt_shared_{}", Uuid::new_v4());
	let (shared_db, shared_subject) = rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{shared_db_name}")).ok();
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;

		let db = Database::new(&shared_db_name).await.unwrap();
		let subject = db.track_subject("opt_subject").await.unwrap();
		(db, subject)
	});

	for (name, measurement_count, window_minutes, resolution) in optimization_configs {
		// Pre-setup aspect and data for this benchmark
		let aspect_id = rt.block_on(async {
			let aspect = shared_db.track_aspect(shared_subject.clone(), &format!("opt_aspect_{}", name), Resolution::Seconds).await.unwrap();

			// Use batch insertion for much better performance
			let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
			let mut measurements = Vec::with_capacity(measurement_count);
			for i in 0..measurement_count {
				measurements.push(InputMeasurement::new(start_time + Duration::minutes(i as i64), BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap()));
			}
			shared_db.observe_measurements_batch(aspect.clone(), measurements).await.unwrap();

			aspect.id()
		});

		c.bench_function(name, |b| {
			b.iter(|| {
				rt.block_on(async {
					// Much smaller analysis window
					let data_start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
					let analysis_end = data_start + Duration::minutes(window_minutes);

					// Perform analysis using the pre-setup data
					let result = Database::analyze_range(aspect_id, data_start, analysis_end, resolution, Spline::Linear).await.unwrap();

					black_box(result)
				})
			})
		});
	}

	// Single cleanup at the end
	rt.block_on(async {
		shared_db.close().await.unwrap();
		tokio::time::sleep(std::time::Duration::from_millis(200)).await;
		std::fs::remove_dir_all(format!("data/{shared_db_name}")).ok();
	});
}

fn benchmark_memory_efficiency(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// Even smaller configs for memory efficiency tests
	let memory_configs = vec![
		("small_efficient", 25, 3),   // Reduced from 100 to 25
		("medium_efficient", 50, 5),  // Reduced from 500 to 50
		("large_efficient", 100, 10), // Reduced from 1000 to 100
	];

	let db_name = format!("bench_mem_{}", Uuid::new_v4());
	std::fs::remove_dir_all(format!("data/{db_name}")).ok();

	// Setup shared database and subject
	let (db, subject) = rt.block_on(async {
		let db = Database::new(&db_name).await.unwrap();
		let subject = db.track_subject("mem_subject").await.unwrap();
		(db, subject)
	});

	for (name, measurement_count, window_minutes) in memory_configs {
		// Pre-setup data for this configuration
		let aspect_id = rt.block_on(async {
			let aspect = db.track_aspect(subject.clone(), &format!("mem_aspect_{}", name), Resolution::Seconds).await.unwrap();

			// Use batch insertion
			let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
			let mut measurements = Vec::with_capacity(measurement_count);
			for i in 0..measurement_count {
				measurements.push(InputMeasurement::new(start_time + Duration::minutes(i as i64), BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap()));
			}
			db.observe_measurements_batch(aspect.clone(), measurements).await.unwrap();

			aspect.id()
		});

		c.bench_function(&format!("memory_efficiency_{}", name), |b| {
			b.iter(|| {
				rt.block_on(async {
					let data_start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
					let end = data_start + Duration::minutes(window_minutes);

					// Use Minutes resolution to reduce output point count
					let result = Database::analyze_range(aspect_id, data_start, end, Resolution::Minutes, Spline::Linear).await.unwrap();

					black_box(result)
				})
			});
		});
	}

	// Cleanup after all benchmarks
	rt.block_on(async {
		db.close().await.unwrap();
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

criterion_group!(benches, benchmark_optimization_strategies, benchmark_memory_efficiency);
criterion_main!(benches);
