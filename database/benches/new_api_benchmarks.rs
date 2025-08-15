use std::{hint::black_box, str::FromStr};

use ::database::*;
use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use splimes::{Resolution, Spline};
use tokio::runtime::Runtime;
use uuid::Uuid;

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
				let aspect = db.track_aspect(subject.clone(), &aspect_name, Resolution::Seconds).await.unwrap();
				counter += 1;
				black_box(aspect)
			})
		});
	});

	// Cleanup
	rt.block_on(async {
		db.close().await.unwrap();
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

fn benchmark_data_insertion(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	let db_name = format!("bench_insertion_{}", Uuid::new_v4());
	let (db, aspect_id) = rt.block_on(async {
		let db = Database::new(&db_name).await.unwrap();
		let subject = db.track_subject("bench_subject").await.unwrap();
		let aspect = db.track_aspect(subject, "bench_aspect", Resolution::Seconds).await.unwrap();
		(db, aspect.id())
	});

	c.bench_function("single_measurement_insertion", |b| {
		let mut counter = 0;
		b.iter(|| {
			rt.block_on(async {
				let time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap() + Duration::seconds(counter);
				let measurement = InputMeasurement::new(time, BigDecimal::from_str("42.0").unwrap());
				let aspect = db.get_aspect(&aspect_id).await.unwrap();
				db.observe_measurement(aspect, measurement).await.unwrap();
				counter += 1;
			})
		});
	});

	// Cleanup
	rt.block_on(async {
		db.close().await.unwrap();
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

fn benchmark_batch_insertion(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	let db_name = format!("bench_batch_{}", Uuid::new_v4());
	let (db, aspect) = rt.block_on(async {
		let db = Database::new(&db_name).await.unwrap();
		let subject = db.track_subject("bench_subject").await.unwrap();
		let aspect = db.track_aspect(subject, "bench_aspect", Resolution::Seconds).await.unwrap();
		(db, aspect)
	});

	c.bench_function("batch_measurement_insertion", |b| {
		b.iter(|| {
			rt.block_on(async {
				let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
				let mut measurements = Vec::new();
				for i in 0..100 {
					let measurement = InputMeasurement::new(start_time + Duration::seconds(i), BigDecimal::from_str(&format!("{}.0", i)).unwrap());
					measurements.push(measurement);
				}
				db.observe_measurements_batch(aspect.clone(), measurements).await.unwrap();
			})
		});
	});

	// Cleanup
	rt.block_on(async {
		db.close().await.unwrap();
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

fn benchmark_point_analysis(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	let db_name = format!("bench_point_{}", Uuid::new_v4());
	let (db, aspect_id) = rt.block_on(async {
		let db = Database::new(&db_name).await.unwrap();
		let subject = db.track_subject("bench_subject").await.unwrap();
		let aspect = db.track_aspect(subject, "bench_aspect", Resolution::Seconds).await.unwrap();

		// Add some test data
		let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
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

	// Cleanup
	rt.block_on(async {
		db.close().await.unwrap();
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

fn benchmark_range_analysis(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	let db_name = format!("bench_range_{}", Uuid::new_v4());
	let (db, aspect_id) = rt.block_on(async {
		let db = Database::new(&db_name).await.unwrap();
		let subject = db.track_subject("bench_subject").await.unwrap();
		let aspect = db.track_aspect(subject, "bench_aspect", Resolution::Seconds).await.unwrap();

		// Add test data
		let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		for i in 0..1000 {
			let measurement = InputMeasurement::new(base_time + Duration::seconds(i * 60), BigDecimal::from_str(&format!("{}.0", i)).unwrap());
			db.observe_measurement(aspect.clone(), measurement).await.unwrap();
		}

		(db, aspect.id())
	});

	c.bench_function("range_analysis", |b| {
		b.iter(|| {
			rt.block_on(async {
				let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
				let end = start + Duration::hours(5);
				let result = Database::analyze_range(aspect_id, start, end, Resolution::Minutes, Spline::Linear).await.unwrap();
				black_box(result)
			})
		});
	});

	// Cleanup
	rt.block_on(async {
		db.close().await.unwrap();
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

fn benchmark_cache_usage(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	let db_name = format!("bench_cache_{}", Uuid::new_v4());
	let (db, aspect_id) = rt.block_on(async {
		let db = Database::new(&db_name).await.unwrap();
		let subject = db.track_subject("cache_subject").await.unwrap();
		let aspect = db.track_aspect(subject, "cache_aspect", Resolution::Seconds).await.unwrap();

		// Add test data
		let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		for i in 0..100 {
			let measurement = InputMeasurement::new(base_time + Duration::minutes(i), BigDecimal::from_str(&format!("{}.0", i)).unwrap());
			db.observe_measurement(aspect.clone(), measurement).await.unwrap();
		}

		(db, aspect.id())
	});

	c.bench_function("cache_usage", |b| {
		b.iter(|| {
			rt.block_on(async {
				let query_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 30, 0).unwrap();
				let result = db.analyze_point(aspect_id, query_time, Resolution::Seconds, Spline::Linear).await.unwrap();
				black_box(result)
			})
		});
	});

	// Cleanup
	rt.block_on(async {
		db.close().await.unwrap();
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

fn benchmark_concurrent_access(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	let db_name = format!("bench_concurrent_{}", Uuid::new_v4());
	let (db, aspect_id) = rt.block_on(async {
		let db = Database::new(&db_name).await.unwrap();
		let subject = db.track_subject("concurrent_subject").await.unwrap();
		let aspect = db.track_aspect(subject, "concurrent_aspect", Resolution::Seconds).await.unwrap();

		// Add test data
		let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		for i in 0..200 {
			let measurement = InputMeasurement::new(base_time + Duration::seconds(i * 30), BigDecimal::from_str(&format!("{}.0", i)).unwrap());
			db.observe_measurement(aspect.clone(), measurement).await.unwrap();
		}

		(db, aspect.id())
	});

	c.bench_function("concurrent_access", |b| {
		b.iter(|| {
			rt.block_on(async {
				let mut handles = vec![];
				for i in 0..5 {
					let db_clone = db.clone();
					let query_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap() + Duration::minutes(i * 10);
					let handle = tokio::spawn(async move { db_clone.analyze_point(aspect_id, query_time, Resolution::Seconds, Spline::Linear).await.unwrap() });
					handles.push(handle);
				}

				let results: Vec<_> = futures::future::join_all(handles).await.into_iter().collect::<Result<Vec<_>, _>>().unwrap();
				black_box(results)
			})
		});
	});

	// Cleanup
	rt.block_on(async {
		db.close().await.unwrap();
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

criterion_group!(benches, benchmark_aspect_creation, benchmark_data_insertion, benchmark_batch_insertion, benchmark_point_analysis, benchmark_range_analysis, benchmark_cache_usage, benchmark_concurrent_access);
criterion_main!(benches);
