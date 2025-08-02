use std::{hint::black_box, path::Path, str::FromStr};
use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, Criterion};
use database::{Database, InputMeasurement};
use splimes::{Resolution, Spline};
use tokio::runtime::Runtime;
use uuid::Uuid;

fn benchmark_interpolation_sizes(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let sizes = vec![100, 1000, 10000];

    for size in sizes {
        c.bench_function(&format!("interpolation_size_{}", size), |b| {
            b.iter(|| {
                rt.block_on(async {
                    // Generate unique name for each iteration
                    let db_name = format!("bench_interp_{}_{}", size, Uuid::new_v4());
                    let db_path = format!("data/{db_name}");

                    // Clean up if exists
                    if Path::new(&db_path).exists() {
                        if let Err(e) = std::fs::remove_dir_all(&db_path) {
                            eprintln!("Warning: Failed to remove directory {}: {}", db_path, e);
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }

                    // Create database
                    let db = Database::new(&db_name).await.unwrap();

                    // Setup subject and aspect (assuming similar to other benchmarks)
                    let subject = db.track_subject("interp_subject").await.unwrap();
                    let aspect = db.track_aspect(subject, "interp_aspect").await.unwrap();

                    // Generate and insert test data
                    let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
                    let mut measurements = Vec::with_capacity(size);
                    for i in 0..size {
                        measurements.push(InputMeasurement::new(
                            base_time + Duration::seconds(i as i64),
                            BigDecimal::from_str(&format!("{}.0", i)).unwrap()
                        ));
                    }
                    db.observe_measurements_batch(aspect.clone(), measurements).await.unwrap();

                    // Perform interpolation
                    let start = base_time;
                    let end = base_time + Duration::seconds((size - 1) as i64);
                    let result = Database::analyze_range(
                        aspect.id(),
                        start,
                        end,
                        Resolution::Seconds,
                        Spline::Linear,
                    ).await.unwrap();

                    // Cleanup
                    db.close().await.unwrap();
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

fn benchmark_interpolation_resolutions(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    // Create a single database for all resolutions with unique name
    let db_name = format!("bench_res_shared_{}", Uuid::new_v4());
    let (db, aspect_id) = rt.block_on(async {
        let db = Database::new(&db_name).await.unwrap();
        let subject = db.track_subject("interp_subject").await.unwrap();
        let aspect = db.track_aspect(subject, "interp_aspect").await.unwrap();

        // Generate and insert test data (fixed size 1000)
        let size = 1000;
        let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let mut measurements = Vec::with_capacity(size);
        for i in 0..size {
            measurements.push(InputMeasurement::new(
                base_time + Duration::seconds(i as i64),
                BigDecimal::from_str(&format!("{}.0", i)).unwrap()
            ));
        }
        db.observe_measurements_batch(aspect.clone(), measurements).await.unwrap();

        (db, aspect.id())
    });

    let resolutions = vec![Resolution::Seconds, Resolution::Minutes, Resolution::Hours];

    for res in resolutions {
        c.bench_function(&format!("interpolation_resolution_{:?}", res), |b| {
            b.iter(|| {
                rt.block_on(async {
                    // Perform interpolation
                    let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
                    let end = start + Duration::seconds(999);
                    let result = Database::analyze_range(
                        aspect_id,
                        start,
                        end,
                        res,
                        Spline::Linear,
                    ).await.unwrap();

                    black_box(result)
                })
            });
        });
    }

    // Cleanup after all benchmarks
    rt.block_on(async {
        db.close().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        std::fs::remove_dir_all(format!("data/{db_name}")).ok();
    });
}

fn benchmark_spline_types(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// Create a single database for all spline types
	let db_name = "bench_spline_shared";
	let (_db, aspect_id) = rt.block_on(async {
		// Clean up any existing test data
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
		tokio::time::sleep(std::time::Duration::from_millis(200)).await;

		let db = Database::new(db_name).await.unwrap();
		let subject = db.track_subject("bench_subject").await.unwrap();
		let aspect = db.track_aspect(subject, "bench_aspect").await.unwrap();

		// Add test data once - reduced size
		let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		for i in 0..100 {
			let measurement = InputMeasurement::new(start_time + Duration::minutes(i), BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap());
			db.observe_measurement(aspect.clone(), measurement).await.unwrap();
		}

		(db, aspect.id())
	});

	let spline_types = vec![("linear", Spline::Linear), ("quadratic", Spline::Quadratic), ("cubic", Spline::Cubic)];

	for (name, spline_type) in spline_types {
		c.bench_function(&format!("spline_type_{}", name), |b| {
			b.iter(|| {
				rt.block_on(async {
					// Perform interpolation analysis
					let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
					let end_time = start_time + Duration::minutes(10);
					let result = Database::analyze_range(aspect_id, start_time, end_time, Resolution::Minutes, spline_type).await.unwrap();

					black_box(result)
				})
			})
		});
	}

	// Cleanup after all benchmarks
	rt.block_on(async {
		std::fs::remove_dir_all(format!("data/{db_name}")).ok();
	});
}

criterion_group!(benches, benchmark_interpolation_sizes, benchmark_interpolation_resolutions, benchmark_spline_types);
criterion_main!(benches);
