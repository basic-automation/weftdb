use std::{hint::black_box, str::FromStr};

use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, Criterion};
use database::*;
use splimes::{Resolution, Spline};
use tokio::runtime::Runtime;
use uuid::Uuid;

fn benchmark_production_workloads(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    // Simulate production workload patterns with reduced sizes for stability
    let workloads = vec![
        ("iot_sensor_data", 50, 1, 30, Resolution::Seconds),     // Further reduced
        ("financial_ticks", 100, 1, 60, Resolution::Seconds),    // Further reduced
        ("monitoring_metrics", 40, 2, 60, Resolution::Minutes),  // Further reduced
    ];

    for (name, measurement_count, interval_minutes, window_minutes, resolution) in workloads {
        c.bench_function(name, |b| {
            // Create a single database per benchmark function
            let db_name = format!("bench_prod_{}_{}", name, Uuid::new_v4());
            let (_db, aspect_id) = rt.block_on(async {
                // Clean up any existing test data
                if let Err(e) = std::fs::remove_dir_all(format!("data/{db_name}")) {
                    eprintln!("Warning: Failed to remove existing directory: {}", e);
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                
                // Create database and setup data
                let db = Database::new(&db_name).await.unwrap();
                let subject = db.track_subject("benchmark_subject").await.unwrap();
                let aspect = db.track_aspect(subject, "benchmark_aspect").await.unwrap();

                // Add test data once
                let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
                for i in 0..measurement_count {
                    let measurement = InputMeasurement::new(
                        base_time + Duration::minutes(i as i64 * interval_minutes),
                        BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap()
                    );
                    db.observe_measurement(aspect.clone(), measurement).await.unwrap();
                }

                (db, aspect.id())
            });

            b.iter(|| {
                rt.block_on(async {
                    // Calculate proper time range
                    let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
                    let data_start = base_time;
                    let data_end = base_time + Duration::minutes((measurement_count as i64 - 1) * interval_minutes);
                    let data_span_minutes = (data_end - data_start).num_minutes();
                    let actual_window_minutes = window_minutes.max(data_span_minutes + 10);

                    let start = data_start;
                    let end = start + Duration::minutes(actual_window_minutes);

                    // Perform analysis using Database API
                    let result = Database::analyze_range(
                        aspect_id,
                        start,
                        end,
                        resolution,
                        Spline::Linear, // Production typically uses linear
                    ).await.unwrap();

                    black_box(result)
                })
            });

            // Cleanup after all iterations
            rt.block_on(async {
                if let Err(e) = std::fs::remove_dir_all(format!("data/{db_name}")) {
                    eprintln!("Warning: Failed to remove directory after benchmark: {}", e);
                }
            });
        });
    }
}

fn benchmark_full_integration_pipeline(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    // Test the complete integration pipeline with realistic but smaller scenarios
    let integration_scenarios = vec![
        ("real_time_small", 15, 1, 30, Resolution::Seconds, Spline::Linear),      // Further reduced
        ("batch_medium", 50, 1, 120, Resolution::Minutes, Spline::Quadratic),     // Further reduced
        // Remove the analytics_large benchmark that's causing issues
    ];

    for (name, measurement_count, interval_minutes, window_minutes, resolution, spline_type) in integration_scenarios {
        c.bench_function(&format!("integration_pipeline_{}", name), |b| {
            // Create a single database per benchmark function
            let db_name = format!("bench_pipeline_{}", name);
            let (_db, aspect_id) = rt.block_on(async {
                // Clean up any existing test data
                std::fs::remove_dir_all(format!("data/{db_name}")).ok();
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                
                // Create database and setup data
                let db = Database::new(&db_name).await.unwrap();
                let subject = db.track_subject("pipeline_subject").await.unwrap();
                let aspect = db.track_aspect(subject, "pipeline_aspect").await.unwrap();

                // Add test data once
                let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
                for i in 0..measurement_count {
                    let measurement = InputMeasurement::new(
                        base_time + Duration::minutes(i as i64 * interval_minutes),
                        BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap()
                    );
                    db.observe_measurement(aspect.clone(), measurement).await.unwrap();
                }

                (db, aspect.id())
            });

            b.iter(|| {
                rt.block_on(async {
                    // Calculate proper time range
                    let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
                    let data_start = base_time;
                    let data_end = base_time + Duration::minutes((measurement_count as i64 - 1) * interval_minutes);
                    let data_span_minutes = (data_end - data_start).num_minutes();
                    let actual_window_minutes = window_minutes.max(data_span_minutes + 10);

                    let start = data_start;
                    let end = start + Duration::minutes(actual_window_minutes);

                    // Perform analysis using Database API
                    let result = Database::analyze_range(
                        aspect_id,
                        start,
                        end,
                        resolution,
                        spline_type,
                    ).await.unwrap();

                    black_box(result)
                })
            });

            // Cleanup after all iterations
            rt.block_on(async {
                std::fs::remove_dir_all(format!("data/{db_name}")).ok();
            });
        });
    }
}

criterion_group!(benches, benchmark_production_workloads, benchmark_full_integration_pipeline);
criterion_main!(benches);
