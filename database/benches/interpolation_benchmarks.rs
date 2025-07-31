use std::{hint::black_box, str::FromStr};

use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, Criterion};
use database::*;
use splimes::{Resolution, Spline};
use tokio::runtime::Runtime;

fn benchmark_interpolation_sizes(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    // Reduced sizes to avoid SQLite issues and make benchmarks more stable
    let sizes = vec![100, 500, 1000]; // Removed 2000 which was causing issues

    for size in sizes {
        c.bench_function(&format!("interpolation_size_{}", size), |b| {
            // Create a single database per benchmark function
            let db_name = format!("bench_size_{}", size);
            let (_db, aspect_id) = rt.block_on(async {
                // Clean up any existing test data
                std::fs::remove_dir_all(format!("data/{db_name}")).ok();
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                
                let db = Database::new(&db_name).await.unwrap();
                let subject = db.track_subject("bench_subject").await.unwrap();
                let aspect = db.track_aspect(subject, "bench_aspect").await.unwrap();

                // Add test data once
                let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
                for i in 0..size {
                    let measurement = InputMeasurement::new(
                        start_time + Duration::minutes(i as i64),
                        BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap()
                    );
                    db.observe_measurement(aspect.clone(), measurement).await.unwrap();
                }

                (db, aspect.id())
            });

            b.iter(|| {
                rt.block_on(async {
                    // Perform interpolation analysis
                    let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
                    let end_time = start_time + Duration::minutes(10);
                    let result = Database::analyze_range(
                        aspect_id,
                        start_time,
                        end_time,
                        Resolution::Minutes,
                        Spline::Linear
                    ).await.unwrap();
                    
                    black_box(result)
                })
            });

            // Cleanup after benchmark
            rt.block_on(async {
                std::fs::remove_dir_all(format!("data/{db_name}")).ok();
            });
        });
    }
}

fn benchmark_interpolation_resolutions(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let resolutions = vec![
        ("seconds", Resolution::Seconds, 5),  // 5 minutes for seconds
        ("minutes", Resolution::Minutes, 60), // 1 hour for minutes
    ];

    for (name, resolution, duration_amount) in resolutions {
        c.bench_function(&format!("interpolation_resolution_{}", name), |b| {
            // Create a single database per benchmark function
            let db_name = format!("bench_res_{}", name);
            let (_db, aspect_id) = rt.block_on(async {
                // Clean up any existing test data
                std::fs::remove_dir_all(format!("data/{db_name}")).ok();
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                
                let db = Database::new(&db_name).await.unwrap();
                let subject = db.track_subject("bench_subject").await.unwrap();
                let aspect = db.track_aspect(subject, "bench_aspect").await.unwrap();

                // Add test data once - reduced size
                let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
                for i in 0..200 {
                    let measurement = InputMeasurement::new(
                        start_time + Duration::minutes(i),
                        BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap()
                    );
                    db.observe_measurement(aspect.clone(), measurement).await.unwrap();
                }

                (db, aspect.id())
            });

            b.iter(|| {
                rt.block_on(async {
                    let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
                    // Calculate end time based on resolution
                    let interpolation_end = match resolution {
                        Resolution::Seconds => start_time + Duration::minutes(duration_amount),
                        Resolution::Minutes => start_time + Duration::minutes(duration_amount),
                        Resolution::Hours => start_time + Duration::hours(duration_amount),
                        Resolution::Days => start_time + Duration::days(duration_amount),
                        _ => start_time + Duration::minutes(duration_amount),
                    };

                    // Perform interpolation analysis
                    let result = Database::analyze_range(
                        aspect_id,
                        start_time,
                        interpolation_end,
                        resolution,
                        Spline::Linear
                    ).await.unwrap();
                    
                    black_box(result)
                })
            });

            // Cleanup after benchmark
            rt.block_on(async {
                std::fs::remove_dir_all(format!("data/{db_name}")).ok();
            });
        });
    }
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
            let measurement = InputMeasurement::new(
                start_time + Duration::minutes(i),
                BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap()
            );
            db.observe_measurement(aspect.clone(), measurement).await.unwrap();
        }

        (db, aspect.id())
    });

    let spline_types = vec![
        ("linear", Spline::Linear),
        ("quadratic", Spline::Quadratic),
        ("cubic", Spline::Cubic),
    ];

    for (name, spline_type) in spline_types {
        c.bench_function(&format!("spline_type_{}", name), |b| {
            b.iter(|| {
                rt.block_on(async {
                    // Perform interpolation analysis
                    let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
                    let end_time = start_time + Duration::minutes(10);
                    let result = Database::analyze_range(
                        aspect_id,
                        start_time,
                        end_time,
                        Resolution::Minutes,
                        spline_type
                    ).await.unwrap();
                    
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
