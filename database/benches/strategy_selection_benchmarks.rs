use std::{hint::black_box, str::FromStr};

use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, Criterion};
use database::*;
use splimes::{Resolution, Spline};
use tokio::runtime::Runtime;
use uuid::Uuid;

fn benchmark_strategy_selection(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    // Test different dataset sizes to trigger different strategies
    let strategies = vec![
        ("small_dataset", 150, 10),  // Small dataset, CPU standard
        ("medium_dataset", 600, 20), // Medium dataset, SIMD/parallel
        ("large_dataset", 1200, 60), // Large dataset, GPU acceleration
    ];

    for (name, measurement_count, window_minutes) in strategies {
        c.bench_function(name, |b| {
            b.iter(|| {
                rt.block_on(async {
                    // Create unique database for each iteration
                    let db_name = format!("bench_strategy_{}_{}", name, Uuid::new_v4());
                    std::fs::remove_dir_all(format!("data/{db_name}")).ok();
                    
                    let db = Database::new(&db_name).await.unwrap();
                    let subject = db.track_subject("strategy_subject").await.unwrap();
                    let aspect = db.track_aspect(subject, "strategy_aspect").await.unwrap();

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

                    // Use either the requested window or extend beyond the data, whichever is larger
                    let actual_window_minutes = window_minutes.max(data_span_minutes + 10);

                    let start = data_start;
                    let end = start + Duration::minutes(actual_window_minutes);

                    // Validate the time range
                    assert!(end > start, "End time must be after start time for benchmark {}", name);

                    // Perform analysis using Database API
                    let result = Database::analyze_range(
                        aspect.id(),
                        start,
                        end,
                        Resolution::Seconds,
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

criterion_group!(benches, benchmark_strategy_selection);
criterion_main!(benches);
