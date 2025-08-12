use std::str::FromStr;

use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use database::*;
use splimes::{Resolution, Spline};
use tokio::runtime::Runtime;
use uuid::Uuid;

fn benchmark_database_creation(c: &mut Criterion) {
	// Suppress verbose logging during benchmarks
	std::env::remove_var("DSP_VERBOSE");
	std::env::remove_var("RUST_LOG");

	let rt = Runtime::new().unwrap();

	c.bench_function("new_database_creation", |b| {
		b.iter(|| {
			rt.block_on(async {
				let db_name = format!("bench_db_{}", Uuid::new_v4());
				// Clean up any existing data
				std::fs::remove_dir_all(format!("data/{db_name}")).ok();
				// Add a small delay to ensure filesystem cleanup
				tokio::time::sleep(std::time::Duration::from_millis(200)).await;

				let db = Database::new(&db_name).await.unwrap();

				black_box(db.id());

				// Close before cleanup
				db.close().await.unwrap();
				tokio::time::sleep(std::time::Duration::from_millis(200)).await;

				// Cleanup immediately after
				std::fs::remove_dir_all(format!("data/{db_name}")).ok();
			})
		})
	});
}

fn benchmark_subject_creation(c: &mut Criterion) {
	// Suppress verbose logging during benchmarks
	std::env::remove_var("DSP_VERBOSE");
	std::env::remove_var("RUST_LOG");

	let rt = Runtime::new().unwrap();

	c.bench_function("subject_creation", |b| {
		b.iter_batched(
			|| {
				// Setup: Create fresh database for each iteration
				rt.block_on(async {
					let db_name = format!("bench_subject_{}", Uuid::new_v4());
					std::fs::remove_dir_all(format!("data/{db_name}")).ok();
					tokio::time::sleep(std::time::Duration::from_millis(100)).await;

					let db = Database::new(&db_name).await.unwrap();
					(db, db_name)
				})
			},
			|(db, db_name)| {
				// Benchmark: Create subject
				rt.block_on(async {
					let subject_name = format!("subject_{}", Uuid::new_v4());
					let subject = db.track_subject(&subject_name).await.unwrap();

					// Cleanup immediately
					std::fs::remove_dir_all(format!("data/{db_name}")).ok();
					tokio::time::sleep(std::time::Duration::from_millis(100)).await;

					black_box(subject.id())
				})
			},
			criterion::BatchSize::SmallInput,
		);
	});
}

fn benchmark_aspect_creation(c: &mut Criterion) {
	// Suppress verbose logging during benchmarks
	std::env::remove_var("DSP_VERBOSE");
	std::env::remove_var("RUST_LOG");

	let rt = Runtime::new().unwrap();

	// Create a single shared database and subject for all aspect creation benchmarks
	let (db, subject, db_name) = rt.block_on(async {
		let db_name = format!("bench_aspect_shared_{}", Uuid::new_v4());
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();

		let db = Database::new(&db_name).await.unwrap();
		let subject = db.track_subject("bench_subject").await.unwrap();
		(db, subject, db_name)
	});

	c.bench_function("aspect_creation", |b| {
		let mut counter = 0;
		b.iter(|| {
			rt.block_on(async {
				let aspect_name = format!("aspect_{}_{}", counter, Uuid::new_v4());
				let aspect = db.track_aspect(subject.clone(), &aspect_name).await.unwrap();
				black_box(aspect.id())
			});
			counter += 1;
		})
	});

	// Cleanup after all benchmarks
	rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

fn benchmark_measurement_capture(c: &mut Criterion) {
	// Suppress verbose logging during benchmarks
	std::env::remove_var("DSP_VERBOSE");
	std::env::remove_var("RUST_LOG");

	let rt = Runtime::new().unwrap();

	let db_name = format!("bench_measurement_{}", Uuid::new_v4());
	let (db, aspect) = rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
		let db = Database::new(&db_name).await.unwrap();
		let subject = db.track_subject("bench_subject").await.unwrap();
		let aspect = db.track_aspect(subject, "bench_aspect").await.unwrap();
		(db, aspect)
	});

	c.bench_function("measurement_capture", |b| {
		let mut counter = 0;
		b.iter(|| {
			rt.block_on(async {
				let measurement = InputMeasurement::new(Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap() + Duration::seconds(counter), BigDecimal::from_str("25.5").unwrap());
				let tx_id = db.observe_measurement(aspect.clone(), measurement).await.unwrap();
				black_box(tx_id)
			});
			counter += 1;
		})
	});

	rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

fn benchmark_bulk_measurement_capture(c: &mut Criterion) {
	// Suppress verbose logging during benchmarks
	std::env::remove_var("DSP_VERBOSE");
	std::env::remove_var("RUST_LOG");

	let rt = Runtime::new().unwrap();

	let db_name = format!("bench_bulk_{}", Uuid::new_v4());
	let (db, aspect) = rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
		let db = Database::new(&db_name).await.unwrap();
		let subject = db.track_subject("bench_subject").await.unwrap();
		let aspect = db.track_aspect(subject, "bench_aspect").await.unwrap();
		(db, aspect)
	});

	let sizes = vec![10, 100, 1000];

	let mut group = c.benchmark_group("bulk_measurement_capture");
	for size in sizes {
		if size == 1000 {
			group.sample_size(10);
		} else {
			group.sample_size(100);
		}
		group.bench_with_input(BenchmarkId::new("bulk", size), &size, |b, &size| {
			b.iter(|| {
				rt.block_on(async {
					let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();

					for i in 0..size {
						let measurement = InputMeasurement::new(base_time + Duration::seconds(i), BigDecimal::from_str(&format!("{}.0", i)).unwrap());
						db.observe_measurement(aspect.clone(), measurement).await.unwrap();
					}

					black_box(size)
				})
			})
		});
	}
	group.finish();

	rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

fn benchmark_point_analysis(c: &mut Criterion) {
	// Suppress verbose logging during benchmarks
	std::env::remove_var("DSP_VERBOSE");
	std::env::remove_var("RUST_LOG");

	let rt = Runtime::new().unwrap();

	let db_name = format!("bench_analysis_{}", Uuid::new_v4());
	let (db, aspect_id) = rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
		let db = Database::new(&db_name).await.unwrap();
		let subject = db.track_subject("bench_subject").await.unwrap();
		let aspect = db.track_aspect(subject, "bench_aspect").await.unwrap();

		// Add test data
		let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
		for i in 0..100 {
			let measurement = InputMeasurement::new(base_time + Duration::minutes(i), BigDecimal::from_str(&format!("{}.{}", i / 10, i % 10)).unwrap());
			db.observe_measurement(aspect.clone(), measurement).await.unwrap();
		}

		(db, aspect.id())
	});

	let spline_types = vec![Spline::Linear, Spline::Quadratic, Spline::Cubic];

	for spline_type in spline_types {
		c.bench_with_input(BenchmarkId::new("point_analysis", format!("{:?}", spline_type)), &spline_type, |b, &spline_type| {
			b.iter(|| {
				rt.block_on(async {
					let target_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 45, 30).unwrap();
					let result = db.analyze_point(aspect_id, target_time, Resolution::Seconds, spline_type).await.unwrap();
					black_box(result)
				})
			})
		});
	}

	rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

fn benchmark_range_analysis(c: &mut Criterion) {
	// Suppress verbose logging during benchmarks
	std::env::remove_var("DSP_VERBOSE");
	std::env::remove_var("RUST_LOG");

	let rt = Runtime::new().unwrap();

	let db_name = format!("bench_range_{}", Uuid::new_v4());
	let (_db, aspect_id) = rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
		let db = Database::new(&db_name).await.unwrap();
		let subject = db.track_subject("bench_subject").await.unwrap();
		let aspect = db.track_aspect(subject, "bench_aspect").await.unwrap();

		// Add test data
		let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
		for i in 0..1000 {
			let measurement = InputMeasurement::new(base_time + Duration::seconds(i * 10), BigDecimal::from_str(&format!("{}.{}", i / 100, i % 100)).unwrap());
			db.observe_measurement(aspect.clone(), measurement).await.unwrap();
		}

		(db, aspect.id())
	});

	let resolutions = vec![Resolution::Seconds, Resolution::Minutes, Resolution::Hours];

	for resolution in resolutions {
		c.bench_with_input(BenchmarkId::new("range_analysis", format!("{:?}", resolution)), &resolution, |b, &resolution| {
			b.iter(|| {
				rt.block_on(async {
					let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
					let end_time = start_time + Duration::hours(1);
					let result = Database::analyze_range(aspect_id, start_time, end_time, resolution, Spline::Linear).await.unwrap();
					black_box(result.len())
				})
			})
		});
	}

	rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

fn benchmark_cache_performance(c: &mut Criterion) {
	// Suppress verbose logging during benchmarks
	std::env::remove_var("DSP_VERBOSE");
	std::env::remove_var("RUST_LOG");

	let rt = Runtime::new().unwrap();

	let db_name = format!("bench_cache_{}", Uuid::new_v4());
	let (db, aspect_id) = rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
		let db = Database::new(&db_name).await.unwrap();
		let subject = db.track_subject("cache_subject").await.unwrap();
		let aspect = db.track_aspect(subject, "cache_aspect").await.unwrap();

		// Add test data
		let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
		for i in 0..100 {
			let measurement = InputMeasurement::new(base_time + Duration::minutes(i), BigDecimal::from_str(&format!("{}.0", i)).unwrap());
			db.observe_measurement(aspect.clone(), measurement).await.unwrap();
		}

		(db, aspect.id())
	});

	c.bench_function("cache_miss_vs_hit", |b| {
		b.iter(|| {
			rt.block_on(async {
				let target_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 45, 0).unwrap();

				// This should use cache after first call
				let result = db.analyze_point(aspect_id, target_time, Resolution::Seconds, Spline::Linear).await.unwrap();

				black_box(result)
			})
		})
	});

	rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

fn benchmark_concurrent_operations(c: &mut Criterion) {
	// Suppress verbose logging during benchmarks
	std::env::remove_var("DSP_VERBOSE");
	std::env::remove_var("RUST_LOG");

	let rt = Runtime::new().unwrap();

	let db_name = format!("bench_concurrent_{}", Uuid::new_v4());
	let (db, aspect) = rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
		let db = Database::new(&db_name).await.unwrap();
		let subject = db.track_subject("concurrent_subject").await.unwrap();
		let aspect = db.track_aspect(subject, "concurrent_aspect").await.unwrap();
		(db, aspect)
	});

	c.bench_function("concurrent_measurement_capture", |b| {
		b.iter(|| {
			rt.block_on(async {
				let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();

				let futures: Vec<_> = (0..10)
					.map(|i| {
						let measurement = InputMeasurement::new(base_time + Duration::seconds(i), BigDecimal::from_str(&format!("{}.0", i)).unwrap());
						db.observe_measurement(aspect.clone(), measurement)
					})
					.collect();

				let results = futures::future::join_all(futures).await;
				black_box(results.len())
			})
		})
	});

	rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

criterion_group!(benches, benchmark_database_creation, benchmark_subject_creation, benchmark_aspect_creation, benchmark_measurement_capture, benchmark_bulk_measurement_capture, benchmark_point_analysis, benchmark_range_analysis, benchmark_cache_performance, benchmark_concurrent_operations);

criterion_main!(benches);
