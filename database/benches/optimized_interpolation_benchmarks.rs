use std::{hint::black_box, str::FromStr};

use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, Criterion};
use database::*;
use splimes::{Resolution, Spline};
use tokio::runtime::Runtime;
use uuid::Uuid;

fn benchmark_optimization_strategies(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let optimization_configs = vec![
        ("cpu_standard", 200, 10, Resolution::Minutes),        // CPU standard path
        ("simd_optimized", 500, 15, Resolution::Seconds),      // SIMD optimization
        ("parallel_optimized", 1000, 20, Resolution::Seconds), // Parallel optimization
        ("gpu_optimized", 1500, 60, Resolution::Seconds),      // GPU optimization
    ];

    for (name, measurement_count, window_minutes, resolution) in optimization_configs {
        c.bench_function(name, |b| {
            b.iter(|| {
                rt.block_on(async {
                    // Create unique database for each iteration
                    let db_name = format!("bench_opt_{}_{}", name, Uuid::new_v4());
                    std::fs::remove_dir_all(format!("data/{db_name}")).ok();
                    
                    let db = Database::new(&db_name).await.unwrap();
                    let subject = db.track_subject("opt_subject").await.unwrap();
                    let aspect = db.track_aspect(subject, "opt_aspect").await.unwrap();

                    // Add test data
                    let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
                    for i in 0..measurement_count {
                        let measurement = InputMeasurement::new(
                            start_time + Duration::minutes(i as i64),
                            BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap()
                        );
                        db.observe_measurement(aspect.clone(), measurement).await.unwrap();
                    }

                    // Calculate proper time range
                    let data_start = start_time;
                    let data_end = start_time + Duration::minutes(measurement_count as i64 - 1);
                    let data_span_minutes = (data_end - data_start).num_minutes();
                    let actual_window_minutes = window_minutes.max(data_span_minutes + 10);

                    let start = data_start;
                    let end = start + Duration::minutes(actual_window_minutes);

                    // Perform analysis using Database API
                    let result = Database::analyze_range(
                        aspect.id(),
                        start,
                        end,
                        resolution,
                        Spline::Linear
                    ).await.unwrap();

                    // Cleanup
                    std::fs::remove_dir_all(format!("data/{db_name}")).ok();
                    
                    black_box(result)
                })
            })
        });
    }
}

fn benchmark_memory_efficiency(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    // Test memory efficiency with different dataset sizes
    let memory_configs = vec![
        ("small_memory", 100, 5),
        ("medium_memory", 1000, 30),
        ("large_memory", 2000, 60),
    ];

    // Create a single shared database for all memory efficiency benchmarks
    let db_name = format!("bench_mem_shared_{}", Uuid::new_v4());
    let (db, subject) = rt.block_on(async {
        // Clean up any existing test data
        std::fs::remove_dir_all(format!("data/{db_name}")).ok();
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        
        let db = Database::new(&db_name).await.unwrap();
        let subject = db.track_subject("mem_shared_subject").await.unwrap();
        (db, subject)
    });

    for (name, measurement_count, window_minutes) in memory_configs {
        c.bench_function(name, |b| {
            // Create a unique aspect for each config
            let aspect_id = rt.block_on(async {
                let aspect = db.track_aspect(subject.clone(), &format!("mem_aspect_{}", name)).await.unwrap();

                // Add test data for this config
                let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
                for i in 0..measurement_count {
                    let measurement = InputMeasurement::new(
                        start_time + Duration::minutes(i as i64),
                        BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap()
                    );
                    db.observe_measurement(aspect.clone(), measurement).await.unwrap();
                }

                aspect.id()
            });

            b.iter(|| {
                rt.block_on(async {
                    // Calculate proper time range
                    let data_start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
                    let data_end = data_start + Duration::minutes(measurement_count as i64 - 1);
                    let data_span_minutes = (data_end - data_start).num_minutes();
                    let actual_window_minutes = window_minutes.max(data_span_minutes + 10);

                    let start = data_start;
                    let end = start + Duration::minutes(actual_window_minutes);

                    // Perform analysis using Database API
                    let result = Database::analyze_range(
                        aspect_id,
                        start,
                        end,
                        Resolution::Minutes,
                        Spline::Linear
                    ).await.unwrap();
                    
                    black_box(result)
                })
            });
        });
    }

    // Cleanup after all benchmarks
    rt.block_on(async {
        std::fs::remove_dir_all(format!("data/{db_name}")).ok();
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    });
}

criterion_group!(benches, benchmark_optimization_strategies, benchmark_memory_efficiency);
criterion_main!(benches);
