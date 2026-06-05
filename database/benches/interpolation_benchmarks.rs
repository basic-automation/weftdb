use std::{hint::black_box, path::Path, str::FromStr};

use ::database::{
	database::traits::{AspectStructure, DatabaseStructure, Inputs, Outputs}, Database, DatasetId, InputMeasurement
};
use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, Criterion};
use splimes::{Resolution, Spline};
use tokio::runtime::Runtime;
use uuid::Uuid;

fn benchmark_interpolation_sizes(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	let sizes = vec![50, 200, 500]; // Reduced sizes for faster benchmarks

	// Configure criterion for very slow benchmarks
	let mut group = c.benchmark_group("interpolation_sizes");
	group.sample_size(10); // Minimum 10 samples required by Criterion
	group.measurement_time(std::time::Duration::from_secs(36)); // Increased to address 32-35s warnings
	group.warm_up_time(std::time::Duration::from_secs(5)); // Longer warm-up for database ops

	for size in sizes {
		group.bench_function(format!("size_{size}"), |b| {
			b.iter(|| {
				rt.block_on(async {
					// Generate unique name for each iteration
					let db_name = format!("bench_interp_{}_{}", size, Uuid::new_v4());
					let db_path = format!("{}/{}", ::database::data_dir(), db_name);

					// Clean up if exists
					if Path::new(&db_path).exists() {
						if let Err(e) = std::fs::remove_dir_all(&db_path) {
							eprintln!("Warning: Failed to remove directory {db_path}: {e}");
						}
						tokio::time::sleep(std::time::Duration::from_millis(200)).await;
					}

					// Create database
					let db = Database::new(&db_name).await.unwrap();

					// Setup subject and aspect (assuming similar to other benchmarks)
					let subject = db.observe_subject("interp_subject").await.unwrap();
					let aspect = db.track_aspect(&subject.id(), "interp_aspect", &Resolution::Seconds, None).await.unwrap();

					// Generate and insert test data
					let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
					let mut measurements = Vec::with_capacity(size);
					for i in 0..size {
						measurements.push(InputMeasurement::new(base_time + Duration::seconds(i as i64), BigDecimal::from_str(&format!("{i}.0")).unwrap()));
					}
					db.batch_capture_measurements(aspect.id(), DatasetId::new(), measurements).await.unwrap();

					// Perform interpolation
					let start = base_time;
					let end = base_time + Duration::seconds((size - 1) as i64);
					let result = db.analyze_range(&aspect.id(), start, end, Resolution::Seconds, Spline::Linear).await.unwrap();

					// Cleanup
					db.close().await.unwrap();
					tokio::time::sleep(std::time::Duration::from_millis(200)).await;
					if let Err(e) = std::fs::remove_dir_all(&db_path) {
						eprintln!("Warning: Failed to remove directory after iteration {db_path}: {e}");
					}

					black_box(result)
				})
			});
		});
	}
	group.finish();
}

fn benchmark_interpolation_resolutions(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// Create a single database for all resolutions with unique name
	let db_name = format!("bench_res_shared_{}", Uuid::new_v4());
	let (db, aspect_id) = rt.block_on(async {
		let db = Database::new(&db_name).await.unwrap();
		let subject = db.observe_subject("interp_subject").await.unwrap();
		let aspect = db.track_aspect(&subject.id(), "interp_aspect", &Resolution::Seconds, None).await.unwrap();

		// Generate and insert test data (reduced size from 1000 to 200)
		let size = 200;
		let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let mut measurements = Vec::with_capacity(size);
		for i in 0..size {
			measurements.push(InputMeasurement::new(base_time + Duration::seconds(i as i64), BigDecimal::from_str(&format!("{i}.0")).unwrap()));
		}
		db.batch_capture_measurements(aspect.id(), DatasetId::new(), measurements).await.unwrap();

		(db, aspect.id())
	});

	let resolutions = vec![Resolution::Seconds, Resolution::Minutes, Resolution::Hours];

	// Configure criterion for very slow benchmarks
	let mut group = c.benchmark_group("interpolation_resolutions");
	group.sample_size(10); // Minimum 10 samples required by Criterion
	group.measurement_time(std::time::Duration::from_secs(15)); // Increase measurement time
	group.warm_up_time(std::time::Duration::from_secs(5)); // Longer warm-up for database ops

	for res in resolutions {
		group.bench_function(format!("{res:?}"), |b| {
			b.iter(|| {
				rt.block_on(async {
					// Perform interpolation
					let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
					let end = start + Duration::seconds(199); // Adjusted for reduced data size
					let result = db.analyze_range(&aspect_id, start, end, res, Spline::Linear).await.unwrap();

					black_box(result)
				})
			});
		});
	}
	group.finish();

	// Cleanup after all benchmarks
	rt.block_on(async {
		db.close().await.unwrap();
		tokio::time::sleep(std::time::Duration::from_millis(200)).await;
		let db_path = format!("{}/{}", ::database::data_dir(), db_name);
		std::fs::remove_dir_all(&db_path).ok();
	});
}

fn benchmark_spline_types(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// Create a single database for all spline types
	let db_name = "bench_spline_shared";
	let (db, aspect_id) = rt.block_on(async {
		// Clean up any existing test data
		let db_path = format!("{}/{}", ::database::data_dir(), db_name);
		std::fs::remove_dir_all(&db_path).ok();
		tokio::time::sleep(std::time::Duration::from_millis(200)).await;

		let db = Database::new(db_name).await.unwrap();
		let subject = db.observe_subject("bench_subject").await.unwrap();
		let aspect = db.track_aspect(&subject.id(), "bench_aspect", &Resolution::Seconds, None).await.unwrap();

		// Add test data once - further reduced size for faster benchmarks
		let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let mut measurements = Vec::with_capacity(50);
		for i in 0..50 {
			measurements.push(InputMeasurement::new(start_time + Duration::minutes(i), BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap()));
		}
		db.batch_capture_measurements(aspect.id(), DatasetId::new(), measurements).await.unwrap();

		(db, aspect.id())
	});

	let spline_types = vec![("linear", Spline::Linear), ("quadratic", Spline::Quadratic), ("cubic", Spline::Cubic)];

	// Configure criterion for slow benchmarks
	let mut group = c.benchmark_group("spline_types");
	group.sample_size(10); // Minimum 10 samples required by Criterion
	group.measurement_time(std::time::Duration::from_secs(16)); // Increased to address 12-14s warnings
	group.warm_up_time(std::time::Duration::from_secs(3)); // Warm-up for database ops

	for (name, spline_type) in spline_types {
		group.bench_function(name, |b| {
			b.iter(|| {
				rt.block_on(async {
					// Perform interpolation analysis
					let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
					let end_time = start_time + Duration::minutes(10);

					let result = db.analyze_range(&aspect_id, start_time, end_time, Resolution::Minutes, spline_type).await.unwrap();

					black_box(result)
				})
			});
		});
	}
	group.finish();

	// Cleanup after all benchmarks
	rt.block_on(async {
		db.close().await.unwrap();
		let db_path = format!("{}/{}", ::database::data_dir(), db_name);
		std::fs::remove_dir_all(&db_path).ok();
	});
}

criterion_group!(benches, benchmark_interpolation_sizes, benchmark_interpolation_resolutions, benchmark_spline_types);
criterion_main!(benches);
