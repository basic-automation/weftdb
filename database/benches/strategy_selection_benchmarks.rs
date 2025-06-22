use std::hint::black_box;
use criterion::{criterion_group, criterion_main, Criterion};
use database::{auto_interpolate, Measurement, Resolution, SplineType};
use std::str::FromStr;
use bigdecimal::BigDecimal;
use chrono::{TimeZone, Utc};
use uuid::Uuid;
use tokio::runtime::Runtime;

fn create_test_measurements(count: usize, interval_minutes: i64) -> Vec<Measurement> {
    let dataset_id = Uuid::new_v4();
    let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

    (0..count)
        .map(|i| Measurement {
            id: Uuid::new_v4(),
            dataset_id,
            timestamp: start_time + chrono::Duration::minutes(i as i64 * interval_minutes),
            value: BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap(),
        })
        .collect()
}

fn benchmark_strategy_selection(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    
    // Test different dataset sizes to trigger different strategies
    let strategies = vec![
        ("small_dataset", 150, 10),    // Small dataset, CPU standard
        ("medium_dataset", 600, 20),   // Medium dataset, SIMD/parallel
        ("large_dataset", 1200, 60),   // Large dataset, GPU acceleration
    ];

    for (name, measurement_count, window_minutes) in strategies {
        let measurements = create_test_measurements(measurement_count, 1);
        
        // FIXED: Ensure proper time range calculation
        let data_start = measurements[0].timestamp;
        let data_end = measurements[measurements.len() - 1].timestamp;
        let data_span_minutes = (data_end - data_start).num_minutes();
        
        // Use either the requested window or extend beyond the data, whichever is larger
        let actual_window_minutes = window_minutes.max(data_span_minutes + 10);
        
        let start = data_start;
        let end = start + chrono::Duration::minutes(actual_window_minutes);

        // Validate the time range
        assert!(end > start, "End time must be after start time for benchmark {}", name);

        c.bench_function(name, |b| {
            b.iter(|| {
                rt.block_on(async {
                    black_box(auto_interpolate(
                        black_box(measurements.clone()),
                        black_box(start),
                        black_box(end),
                        black_box(Resolution::Seconds),
                        black_box(SplineType::Linear),
                    ).await.unwrap())
                })
            })
        });
    }
}

criterion_group!(benches, benchmark_strategy_selection);
criterion_main!(benches);
