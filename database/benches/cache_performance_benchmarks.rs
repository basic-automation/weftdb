use std::str::FromStr;

use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use database::*;
use splimes::{Resolution, Spline};
use tokio::runtime::Runtime;
use uuid::Uuid;

fn benchmark_cache_miss_vs_hit(c: &mut Criterion) {
	// Suppress verbose logging during benchmarks
	std::env::remove_var("DSP_VERBOSE");
	std::env::remove_var("RUST_LOG");

	let rt = Runtime::new().unwrap();

	// Create a separate database for this benchmark group
	let db_name = format!("cache_bench_{}", Uuid::new_v4());
	let (db, aspect_id) = rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;

		let db = Database::new(&db_name).await.unwrap();
		let subject = db.track_subject("cache_subject").await.unwrap();
		let aspect = db.track_aspect(subject, "cache_aspect").await.unwrap();

		// Add test data
		let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
		for i in 0..100 {
			// Further reduced dataset size
			let measurement = InputMeasurement::new(base_time + Duration::seconds(i * 10), BigDecimal::from_str(&format!("{}.{}", i / 10, i % 10)).unwrap());
			db.observe_measurement(aspect.clone(), measurement).await.unwrap();
		}

		(db, aspect.id())
	});

	let mut group = c.benchmark_group("cache_performance");

	// Set measurement parameters for consistency
	group.measurement_time(std::time::Duration::from_secs(5));
	group.sample_size(20); // Reduced sample size

	// Benchmark cache miss (first call)
	group.bench_function("cache_miss", |b| {
		let mut counter = 0;
		b.iter(|| {
			rt.block_on(async {
				// Use different timestamps to avoid cache hits
				let target_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap() + Duration::seconds(counter * 13); // Use prime number to avoid patterns
				counter += 1;
				let result = db.analyze_point(aspect_id, target_time, Resolution::Seconds, Spline::Linear).await.unwrap();
				black_box(result)
			})
		})
	});

	// Benchmark cache hit (repeated calls with same parameters)
	let fixed_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 5, 0).unwrap();

	// Prime the cache
	rt.block_on(async {
		let _ = db.analyze_point(aspect_id, fixed_time, Resolution::Seconds, Spline::Linear).await;
	});

	group.bench_function("cache_hit", |b| {
		b.iter(|| {
			rt.block_on(async {
				let result = db.analyze_point(aspect_id, fixed_time, Resolution::Seconds, Spline::Linear).await.unwrap();
				black_box(result)
			})
		})
	});

	group.finish();

	rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

fn benchmark_cache_invalidation(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// Create a single database for this benchmark to avoid file conflicts
	let db_name = format!("cache_invalidation_{}", Uuid::new_v4());
	let (db, aspect) = rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;

		let db = Database::new(&db_name).await.unwrap();
		let subject = db.track_subject("invalidation_subject").await.unwrap();
		let aspect = db.track_aspect(subject, "invalidation_aspect").await.unwrap();

		(db, aspect)
	});

	c.bench_function("cache_invalidation_overhead", |b| {
		let mut counter = 0;
		b.iter(|| {
			rt.block_on(async {
				// Add new measurement (triggers cache invalidation)
				let measurement = InputMeasurement::new(Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap() + Duration::seconds(counter), BigDecimal::from_str(&format!("{}.0", counter)).unwrap());
				counter += 1;

				let tx_id = db.observe_measurement(aspect.clone(), measurement).await.unwrap();
				black_box(tx_id)
			})
		})
	});

	rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

fn benchmark_concurrent_cache_access(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// Create a single database for concurrent access testing
	let db_name = format!("cache_concurrent_{}", Uuid::new_v4());
	let (db, aspect_id) = rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;

		let db = Database::new(&db_name).await.unwrap();
		let subject = db.track_subject("concurrent_subject").await.unwrap();
		let aspect = db.track_aspect(subject, "concurrent_aspect").await.unwrap();

		// Add test data
		let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
		for i in 0..60 {
			// Reasonable dataset
			let measurement = InputMeasurement::new(base_time + Duration::minutes(i), BigDecimal::from_str(&format!("{}.0", i)).unwrap());
			db.observe_measurement(aspect.clone(), measurement).await.unwrap();
		}

		(db, aspect.id())
	});

	let concurrency_levels = vec![1, 2];

	for &concurrency in &concurrency_levels {
		c.bench_with_input(BenchmarkId::new("concurrent_cache_reads", concurrency), &concurrency, |b, &concurrency| {
			b.iter(|| {
				rt.block_on(async {
					let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();

					let tasks: Vec<_> = (0..concurrency)
						.map(|i| {
							let db_clone = db.clone();
							let target_time = base_time + Duration::minutes(i * 10);
							tokio::spawn(async move { db_clone.analyze_point(aspect_id, target_time, Resolution::Seconds, Spline::Linear).await.unwrap() })
						})
						.collect();

					let results = futures::future::join_all(tasks).await;
					black_box(results.len())
				})
			})
		});
	}

	rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

fn benchmark_cache_memory_usage(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	let dataset_sizes = vec![50, 100];

	for &size in &dataset_sizes {
		c.bench_with_input(BenchmarkId::new("cache_with_dataset_size", size), &size, |b, &size| {
			// Create a single database for this size
			let db_name = format!("cache_memory_{}_{}", size, Uuid::new_v4());
			let (db, aspect_id) = rt.block_on(async {
				std::fs::remove_dir_all(format!("data/{db_name}")).ok();
				tokio::time::sleep(std::time::Duration::from_millis(100)).await;

				let db = Database::new(&db_name).await.unwrap();
				let subject = db.track_subject("memory_subject").await.unwrap();
				let aspect = db.track_aspect(subject, "memory_aspect").await.unwrap();

				let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
				for i in 0..size {
					let measurement = InputMeasurement::new(base_time + Duration::seconds(i * 10), BigDecimal::from_str(&format!("{}.{}", i / 10, i % 10)).unwrap());
					db.observe_measurement(aspect.clone(), measurement).await.unwrap();
				}

				(db, aspect.id())
			});

			b.iter(|| {
				rt.block_on(async {
					// Use a time that's definitely within the dataset bounds
					let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
					let target_time = base_time + Duration::seconds((size / 2) * 10);
					let result = db.analyze_point(aspect_id, target_time, Resolution::Seconds, Spline::Linear).await.unwrap();
					black_box(result)
				})
			});

			rt.block_on(async {
				std::fs::remove_dir_all(format!("data/{db_name}")).ok();
			});
		});
	}
}

fn benchmark_cache_eviction_strategies(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// Create a single database for eviction testing
	let db_name = format!("cache_eviction_{}", Uuid::new_v4());
	let (db, aspect_id) = rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;

		let db = Database::new(&db_name).await.unwrap();
		let subject = db.track_subject("eviction_subject").await.unwrap();
		let aspect = db.track_aspect(subject, "eviction_aspect").await.unwrap();

		// Add base dataset
		let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
		for i in 0..100 {
			let measurement = InputMeasurement::new(base_time + Duration::minutes(i), BigDecimal::from_str(&format!("{}.0", i)).unwrap());
			db.observe_measurement(aspect.clone(), measurement).await.unwrap();
		}

		(db, aspect.id())
	});

	c.bench_function("cache_pressure_simulation", |b| {
		b.iter(|| {
			rt.block_on(async {
				let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();

				// Simulate cache pressure by accessing many different time points
				for i in 0..20 {
					let target_time = base_time + Duration::minutes(i * 5);
					let _result = db.analyze_point(aspect_id, target_time, Resolution::Seconds, Spline::Linear).await.unwrap();
				}

				black_box(20)
			})
		})
	});

	rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

criterion_group!(cache_benches, benchmark_cache_miss_vs_hit, benchmark_cache_invalidation, benchmark_concurrent_cache_access, benchmark_cache_memory_usage, benchmark_cache_eviction_strategies);

criterion_main!(cache_benches);
