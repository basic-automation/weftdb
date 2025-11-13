use std::str::FromStr;

use ::database::{
	database::traits::{AspectStructure, DatabaseStructure, Inputs, Outputs}, Database, DatasetId, InputMeasurement
};
use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
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
		let db = Database::new(&db_name).await.unwrap();
		let subject = db.observe_subject("cache_subject").await.unwrap();
		let aspect = db.track_aspect(&subject.id(), "cache_aspect", &Resolution::Seconds).await.unwrap();

		// Add initial measurements
		let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		for i in 0..50 {
			let measurement = InputMeasurement::new(base_time + Duration::seconds(i * 60), BigDecimal::from_str(&format!("{i}.0")).unwrap());
			db.capture_measurement(&aspect.id(), &DatasetId::new(), &measurement).await.unwrap();
		}
		(db, aspect.id())
	});

	let mut group = c.benchmark_group("cache_performance");

	// Cache miss benchmark
	group.bench_function("cache_miss", |b| {
		b.iter(|| {
			rt.block_on(async {
				let unique_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 30, 0).unwrap() + Duration::nanoseconds(chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) % 1000);
				let result = db.analyze_point(&aspect_id, unique_time, &Resolution::Seconds, &Spline::Linear).await.unwrap();
				black_box(result)
			})
		});
	});

	// Cache hit benchmark
	group.bench_function("cache_hit", |b| {
		let cached_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 15, 0).unwrap();
		// Prime the cache
		rt.block_on(async {
			let _ = db.analyze_point(&aspect_id, cached_time, &Resolution::Seconds, &Spline::Linear).await.unwrap();
		});

		b.iter(|| {
			rt.block_on(async {
				let result = db.analyze_point(&aspect_id, cached_time, &Resolution::Seconds, &Spline::Linear).await.unwrap();
				black_box(result)
			})
		});
	});

	group.finish();

	// Cleanup
	rt.block_on(async {
		db.close().await.unwrap();
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

fn benchmark_cache_invalidation(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// Configure for faster benchmarking
	let mut group = c.benchmark_group("cache_invalidation_group");
	group.sample_size(20); // Reduce from 100 to 20 samples
	group.measurement_time(std::time::Duration::from_secs(2)); // Reduce measurement time
	group.warm_up_time(std::time::Duration::from_secs(1)); // Reduce warm-up time

	group.bench_function("cache_invalidation", |b| {
		b.iter(|| {
			rt.block_on(async {
				let db_name = format!("cache_invalidation_{}", Uuid::new_v4());
				let db = Database::new(&db_name).await.unwrap();
				let subject = db.observe_subject("invalidation_subject").await.unwrap();
				let aspect = db.track_aspect(&subject.id(), "invalidation_aspect", &Resolution::Seconds).await.unwrap();

				// Add some measurements and analyze
				let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
				for i in 0..10 {
					let measurement = InputMeasurement::new(base_time + Duration::seconds(i * 60), BigDecimal::from_str(&format!("{i}.0")).unwrap());
					db.capture_measurement(&aspect.id(), &DatasetId::new(), &measurement).await.unwrap();
				}

				let analyze_time = base_time + Duration::seconds(300);
				let result = db.analyze_point(&aspect.id(), analyze_time, &Resolution::Seconds, &Spline::Linear).await.unwrap();

				db.close().await.unwrap();
				std::fs::remove_dir_all(format!("data/{db_name}")).ok();
				black_box(result)
			})
		});
	});
	group.finish();
}

fn benchmark_concurrent_cache_access(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// Configure for faster benchmarking
	let mut group = c.benchmark_group("concurrent_cache_group");
	group.sample_size(10); // Reduce sample count further
	group.measurement_time(std::time::Duration::from_secs(2)); // Reduce measurement time
	group.warm_up_time(std::time::Duration::from_secs(1)); // Reduce warm-up time

	group.bench_function("concurrent_cache_access", |b| {
		b.iter(|| {
			rt.block_on(async {
				let db_name = format!("concurrent_cache_{}", Uuid::new_v4());
				let db = Database::new(&db_name).await.unwrap();
				let subject = db.observe_subject("concurrent_subject").await.unwrap();
				let aspect = db.track_aspect(&subject.id(), "concurrent_aspect", &Resolution::Seconds).await.unwrap();

				// Add measurements
				let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
				for i in 0..20 {
					let measurement = InputMeasurement::new(base_time + Duration::seconds(i * 30), BigDecimal::from_str(&format!("{i}.0")).unwrap());
					db.capture_measurement(&aspect.id(), &DatasetId::new(), &measurement).await.unwrap();
				} // Simulate concurrent access
				let mut handles = vec![];
				for i in 0..5 {
					let db_clone = db.clone();
					let aspect_id = aspect.id();
					let analyze_time = base_time + Duration::seconds(i * 60);
					let handle = tokio::spawn(async move { db_clone.analyze_point(&aspect_id, analyze_time, &Resolution::Seconds, &Spline::Linear).await });
					handles.push(handle);
				}

				let results: Vec<_> = futures::future::join_all(handles).await.into_iter().collect::<Result<Vec<_>, _>>().unwrap();

				db.close().await.unwrap();
				std::fs::remove_dir_all(format!("data/{db_name}")).ok();
				black_box(results)
			})
		});
	});
	group.finish();
}

fn benchmark_cache_memory_usage(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// Configure for slow benchmarks with high variance (memory operations can be variable)
	let mut group = c.benchmark_group("cache_memory_group");
	group.sample_size(10);
	group.measurement_time(std::time::Duration::from_secs(50)); // Increased for variable memory operations
	group.warm_up_time(std::time::Duration::from_secs(4));
	group.sampling_mode(criterion::SamplingMode::Flat); // Use flat sampling for variable execution times

	let memory_sizes = vec![10, 50, 100];

	for size in memory_sizes {
		group.bench_function(format!("cache_memory_{size}_measurements"), |b| {
			b.iter(|| {
				rt.block_on(async {
					let db_name = format!("memory_cache_{}_{}", size, Uuid::new_v4());
					let db = Database::new(&db_name).await.unwrap();
					let subject = db.observe_subject("memory_subject").await.unwrap();
					let aspect = db.track_aspect(&subject.id(), "memory_aspect", &Resolution::Seconds).await.unwrap();

					// Add measurements
					let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
					for i in 0..size {
						let measurement = InputMeasurement::new(base_time + Duration::seconds(i64::from(i) * 60), BigDecimal::from_str(&format!("{i}.0")).unwrap());
						db.capture_measurement(&aspect.id(), &DatasetId::new(), &measurement).await.unwrap();
					} // Perform several analyses to test memory usage
					let mut results = vec![];
					for i in 0..10 {
						let analyze_time = base_time + Duration::seconds(i * 300);
						let result = db.analyze_point(&aspect.id(), analyze_time, &Resolution::Seconds, &Spline::Linear).await.unwrap();
						results.push(result);
					}

					db.close().await.unwrap();
					std::fs::remove_dir_all(format!("data/{db_name}")).ok();
					black_box(results)
				})
			});
		});
	}
	group.finish();
}

fn benchmark_cache_eviction_strategies(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// Configure for very slow benchmark (cache eviction takes 1-2+ seconds per iteration)
	let mut group = c.benchmark_group("cache_eviction_group");
	group.sample_size(10);
	group.measurement_time(std::time::Duration::from_secs(30)); // Increased to address 26.4s warning
	group.warm_up_time(std::time::Duration::from_secs(3));

	group.bench_function("cache_eviction", |b| {
		b.iter(|| {
			rt.block_on(async {
				let db_name = format!("eviction_cache_{}", Uuid::new_v4());
				let db = Database::new(&db_name).await.unwrap();
				let subject = db.observe_subject("eviction_subject").await.unwrap();
				let aspect = db.track_aspect(&subject.id(), "eviction_aspect", &Resolution::Seconds).await.unwrap();

				// Add many measurements to trigger eviction
				let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
				for i in 0..200 {
					let measurement = InputMeasurement::new(base_time + Duration::seconds(i * 30), BigDecimal::from_str(&format!("{i}.0")).unwrap());
					db.capture_measurement(&aspect.id(), &DatasetId::new(), &measurement).await.unwrap();
				} // Perform many different analyses to test eviction
				let mut results = vec![];
				for i in 0..50 {
					let analyze_time = base_time + Duration::seconds(i * 120);
					let result = db.analyze_point(&aspect.id(), analyze_time, &Resolution::Seconds, &Spline::Linear).await.unwrap();
					results.push(result);
				}

				db.close().await.unwrap();
				std::fs::remove_dir_all(format!("data/{db_name}")).ok();
				black_box(results)
			})
		});
	});
	group.finish();
}

criterion_group!(cache_benches, benchmark_cache_miss_vs_hit, benchmark_cache_invalidation, benchmark_concurrent_cache_access, benchmark_cache_memory_usage, benchmark_cache_eviction_strategies);

criterion_main!(cache_benches);
