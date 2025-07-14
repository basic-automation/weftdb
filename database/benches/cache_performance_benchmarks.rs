use std::str::FromStr;

use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use database::*;
use tokio::runtime::Runtime;
use uuid::Uuid;

fn benchmark_cache_miss_vs_hit(c: &mut Criterion) {
	// Suppress verbose logging during benchmarks
	std::env::remove_var("DSP_VERBOSE");
	std::env::remove_var("RUST_LOG");

	let rt = Runtime::new().unwrap();

	let db_name = format!("cache_bench_{}", Uuid::new_v4());
	let aspect_id = rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
		let db_id = new(&db_name).await.unwrap();
		let subject_id = add_subject(db_id, "cache_subject").await.unwrap();
		let aspect_id = track_aspect(subject_id, "cache_aspect").await.unwrap();

		// Add test data
		let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
		for i in 0..1000 {
			let measurement = InputMeasurement::new(base_time + Duration::seconds(i * 10), BigDecimal::from_str(&format!("{}.{}", i / 10, i % 10)).unwrap());
			capture_measurement(aspect_id, measurement).await.unwrap();
		}

		aspect_id
	});

	let mut group = c.benchmark_group("cache_performance");

	// Benchmark cache miss (first call)
	group.bench_function("cache_miss", |b| {
		b.iter(|| {
			rt.block_on(async {
				// Use different timestamps to avoid cache hits
				let target_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap() + Duration::seconds(fastrand::i64(0..10000));
				let result = analyze_point(aspect_id, target_time, Resolution::Seconds, SplineType::Linear).await.unwrap();
				black_box(result)
			})
		})
	});

	// Benchmark cache hit (repeated calls with same parameters)
	let fixed_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 30, 0).unwrap();

	// Prime the cache
	rt.block_on(async {
		let _ = analyze_point(aspect_id, fixed_time, Resolution::Seconds, SplineType::Linear).await;
	});

	group.bench_function("cache_hit", |b| {
		b.iter(|| {
			rt.block_on(async {
				let result = analyze_point(aspect_id, fixed_time, Resolution::Seconds, SplineType::Linear).await.unwrap();
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

	let db_name = format!("cache_invalidation_{}", Uuid::new_v4());
	let aspect_id = rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
		let db_id = new(&db_name).await.unwrap();
		let subject_id = add_subject(db_id, "invalidation_subject").await.unwrap();
		track_aspect(subject_id, "invalidation_aspect").await.unwrap()
	});

	c.bench_function("cache_invalidation_overhead", |b| {
		let mut counter = 0;
		b.iter(|| {
			rt.block_on(async {
				// Add new measurement (triggers cache invalidation)
				let measurement = InputMeasurement::new(Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap() + Duration::seconds(counter), BigDecimal::from_str(&format!("{}.0", counter % 100)).unwrap());

				let tx_id = capture_measurement(aspect_id, measurement).await.unwrap();
				black_box(tx_id)
			});
			counter += 1;
		})
	});

	rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

fn benchmark_concurrent_cache_access(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	let db_name = format!("cache_concurrent_{}", Uuid::new_v4());
	let aspect_id = rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
		let db_id = new(&db_name).await.unwrap();
		let subject_id = add_subject(db_id, "concurrent_subject").await.unwrap();
		let aspect_id = track_aspect(subject_id, "concurrent_aspect").await.unwrap();

		// Add test data
		let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
		for i in 0..500 {
			let measurement = InputMeasurement::new(base_time + Duration::minutes(i), BigDecimal::from_str(&format!("{}.0", i)).unwrap());
			capture_measurement(aspect_id, measurement).await.unwrap();
		}

		aspect_id
	});

	let concurrency_levels = vec![1, 2, 4, 8, 16];

	for &concurrency in &concurrency_levels {
		c.bench_with_input(BenchmarkId::new("concurrent_cache_reads", concurrency), &concurrency, |b, &concurrency| {
			b.iter(|| {
				rt.block_on(async {
					let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();

					let tasks: Vec<_> = (0..concurrency)
						.map(|i| {
							let target_time = base_time + Duration::minutes(i * 10);
							tokio::spawn(async move { analyze_point(aspect_id, target_time, Resolution::Seconds, SplineType::Linear).await.unwrap() })
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

	let dataset_sizes = vec![100, 1000, 5000, 10000];

	for &size in &dataset_sizes {
		c.bench_with_input(BenchmarkId::new("cache_with_dataset_size", size), &size, |b, &size| {
			// Create a new database for each size to avoid data conflicts
			let db_name = format!("cache_memory_{}_{}", size, Uuid::new_v4());
			let aspect_id = rt.block_on(async {
				std::fs::remove_dir_all(format!("data/{db_name}")).ok();
				let db_id = new(&db_name).await.unwrap();
				let subject_id = add_subject(db_id, "memory_subject").await.unwrap();
				track_aspect(subject_id, "memory_aspect").await.unwrap()
			});

			// Setup data for this benchmark
			rt.block_on(async {
				let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
				for i in 0..size {
					let measurement = InputMeasurement::new(base_time + Duration::seconds(i * 10), BigDecimal::from_str(&format!("{}.{}", i / 100, i % 100)).unwrap());
					capture_measurement(aspect_id, measurement).await.unwrap();
				}
			});

			b.iter(|| {
				rt.block_on(async {
					// Use a time that's definitely within the dataset bounds
					let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
					// Dataset spans from base_time to base_time + (size * 10) seconds
					// Pick a time roughly in the middle of the dataset
					let target_time = base_time + Duration::seconds((size / 2) * 10);
					let result = analyze_point(aspect_id, target_time, Resolution::Seconds, SplineType::Linear).await.unwrap();
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

	let db_name = format!("cache_eviction_{}", Uuid::new_v4());
	let aspect_id = rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
		let db_id = new(&db_name).await.unwrap();
		let subject_id = add_subject(db_id, "eviction_subject").await.unwrap();
		let aspect_id = track_aspect(subject_id, "eviction_aspect").await.unwrap();

		// Add base dataset
		let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
		for i in 0..1000 {
			let measurement = InputMeasurement::new(base_time + Duration::minutes(i), BigDecimal::from_str(&format!("{}.0", i)).unwrap());
			capture_measurement(aspect_id, measurement).await.unwrap();
		}

		aspect_id
	});

	c.bench_function("cache_pressure_simulation", |b| {
		b.iter(|| {
			rt.block_on(async {
				let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();

				// Simulate cache pressure by accessing many different time points
				for i in 0..50 {
					let target_time = base_time + Duration::minutes(i * 20);
					let _result = analyze_point(aspect_id, target_time, Resolution::Seconds, SplineType::Linear).await.unwrap();
				}

				black_box(50)
			})
		})
	});

	rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

criterion_group!(cache_benches, benchmark_cache_miss_vs_hit, benchmark_cache_invalidation, benchmark_concurrent_cache_access, benchmark_cache_memory_usage, benchmark_cache_eviction_strategies);

criterion_main!(cache_benches);
