use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use database::splines::{auto_interpolate, Resolution, SplineType};
use database::Measurement;
use bigdecimal::BigDecimal;
use chrono::{DateTime, TimeZone, Utc};
use std::str::FromStr;
use uuid::Uuid;

fn create_test_measurements(count: usize) -> Vec<Measurement> {
    let dataset_id = Uuid::new_v4();
    let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

    (0..count)
        .map(|i| Measurement {
            id: Uuid::new_v4(),
            dataset_id,
            timestamp: start_time + chrono::Duration::seconds(i as i64 * 10),
            value: BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap(),
        })
        .collect()
}

fn benchmark_strategy_selection(c: &mut Criterion) {
    let mut group = c.benchmark_group("auto_interpolate_strategy_selection");

    // Test different dataset sizes to verify correct strategy selection
    let dataset_sizes = vec![
        (50, "small_standard"),      // Should use standard algorithms
        (500, "medium_simd"),        // Should use SIMD for dense output  
        (2000, "large_parallel"),    // Should use parallel processing
        (6000, "huge_streaming"),    // Should use streaming approach
    ];

    for (size, strategy_name) in dataset_sizes {
        group.throughput(Throughput::Elements(size as u64));
        
        let measurements = create_test_measurements(size);
        let start = measurements[0].timestamp;
        let end = if size < 1000 {
            // Dense output for smaller datasets to trigger SIMD
            start + chrono::Duration::hours(1)
        } else {
            // Normal span for larger datasets
            measurements[measurements.len() - 1].timestamp
        };

        group.bench_with_input(
            BenchmarkId::new("dataset_size", strategy_name),
            &(measurements, start, end),
            |b, (measurements, start, end)| {
                b.iter(|| {
                    auto_interpolate(
                        black_box(measurements.clone()),
                        black_box(*start),
                        black_box(*end),
                        black_box(Resolution::Seconds),
                        black_box(SplineType::Linear),
                    )
                });
            },
        );
    }

    group.finish();
}

fn benchmark_algorithm_complexity_impact(c: &mut Criterion) {
    let mut group = c.benchmark_group("auto_interpolate_algorithm_complexity");

    let spline_types = vec![
        (SplineType::Linear, "linear_complexity_1"),
        (SplineType::Quadratic, "quadratic_complexity_2"),
        (SplineType::Cubic, "cubic_complexity_3"),
        (SplineType::Polynomial(5), "polynomial_complexity_5"),
    ];

    // Use medium dataset size to test complexity-based optimization
    let measurements = create_test_measurements(1500);
    let start = measurements[0].timestamp;
    let end = measurements[measurements.len() - 1].timestamp;

    for (spline_type, name) in spline_types {
        group.bench_with_input(
            BenchmarkId::new("algorithm_complexity", name),
            &spline_type,
            |b, spline_type| {
                b.iter(|| {
                    auto_interpolate(
                        black_box(measurements.clone()),
                        black_box(start),
                        black_box(end),
                        black_box(Resolution::Seconds),
                        black_box(*spline_type),
                    )
                });
            },
        );
    }

    group.finish();
}

fn benchmark_output_density_impact(c: &mut Criterion) {
    let mut group = c.benchmark_group("auto_interpolate_output_density");

    let measurements = create_test_measurements(500); // Medium dataset
    let start = measurements[0].timestamp;

    let density_scenarios = vec![
        (chrono::Duration::minutes(2), Resolution::Seconds, "sparse_120_points"),  // < 256 points
        (chrono::Duration::minutes(6), Resolution::Seconds, "dense_360_points"),   // > 256 points  
        (chrono::Duration::hours(2), Resolution::Seconds, "very_dense_7200_points"), // >> 256 points
    ];

    for (duration, resolution, scenario_name) in density_scenarios {
        let end = start + duration;
        
        group.bench_with_input(
            BenchmarkId::new("output_density", scenario_name),
            &(end, resolution),
            |b, (end, resolution)| {
                b.iter(|| {
                    auto_interpolate(
                        black_box(measurements.clone()),
                        black_box(start),
                        black_box(*end),
                        black_box(*resolution),
                        black_box(SplineType::Linear),
                    )
                });
            },
        );
    }

    group.finish();
}

fn benchmark_strategy_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("auto_interpolate_vs_direct");

    let measurements = create_test_measurements(200);
    let start = measurements[0].timestamp;
    let end = measurements[measurements.len() - 1].timestamp;

    // Compare auto_interpolate overhead vs direct algorithm calls
    group.bench_function("auto_interpolate_with_strategy_selection", |b| {
        b.iter(|| {
            auto_interpolate(
                black_box(measurements.clone()),
                black_box(start),
                black_box(end),
                black_box(Resolution::Seconds),
                black_box(SplineType::Linear),
            )
        });
    });

    group.bench_function("direct_linear_call", |b| {
        b.iter(|| {
            database::splines::linear(
                black_box(measurements.clone()),
                black_box(start),
                black_box(end),
                black_box(Resolution::Seconds),
            )
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    benchmark_strategy_selection,
    benchmark_algorithm_complexity_impact,
    benchmark_output_density_impact,
    benchmark_strategy_overhead
);
criterion_main!(benches);