use std::{hint::black_box, str::FromStr, time::Instant};

use bigdecimal::BigDecimal;
use chrono::{TimeZone, Utc};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use database::{auto_interpolate, fast_path_interpolate, Measurement, Resolution, SplineType};
use uuid::Uuid;

fn create_test_measurements(count: usize) -> Vec<Measurement> {
    let dataset_id = Uuid::new_v4();
    let start_time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

    (0..count).map(|i| Measurement { id: Uuid::new_v4(), dataset_id, timestamp: start_time + chrono::Duration::seconds(i as i64 * 10), value: BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap() }).collect()
}

/// Benchmark fast path vs auto_interpolate - SIMPLIFIED VERSION
fn benchmark_fast_path_vs_auto_interpolate(c: &mut Criterion) {
    let mut group = c.benchmark_group("fast_path_vs_auto_interpolate");

    // Shorter timeouts to prevent hanging
    group.measurement_time(std::time::Duration::from_secs(5));
    group.sample_size(10);

    // Simplified test scenarios with controlled output sizes
    let test_scenarios = vec![
        (100, "small", Resolution::Minutes, chrono::Duration::minutes(30)),        // ~30 points
        (500, "medium_sparse", Resolution::Minutes, chrono::Duration::hours(2)),   // ~120 points
        (500, "medium_dense", Resolution::Minutes, chrono::Duration::minutes(30)), // ~30 points but more input data
        (1500, "large", Resolution::Minutes, chrono::Duration::hours(1)),          // ~60 points
        (2000, "very_large", Resolution::Minutes, chrono::Duration::minutes(30)),  // ~30 points, lots of input data
    ];

    for (size, scenario_name, resolution, duration) in test_scenarios {
        let measurements = create_test_measurements(size);
        let start = measurements[0].timestamp;
        let end = start + duration;

        println!("🔍 Testing {} (n={}, duration={:?}, resolution={:?})", scenario_name, size, duration, resolution);

        // Benchmark auto_interpolate (current strategy)
        group.bench_with_input(BenchmarkId::new("auto_interpolate", scenario_name), &(measurements.clone(), start, end, resolution), |b, (measurements, start, end, resolution)| {
            b.iter(|| auto_interpolate(black_box(measurements.clone()), black_box(*start), black_box(*end), black_box(*resolution), black_box(SplineType::Linear)));
        });

        // Benchmark fast_path_interpolate (new strategy)
        group.bench_with_input(BenchmarkId::new("fast_path", scenario_name), &(measurements, start, end, resolution), |b, (measurements, start, end, resolution)| {
            b.iter(|| fast_path_interpolate(black_box(measurements.clone()), black_box(*start), black_box(*end), black_box(*resolution), black_box(SplineType::Linear)));
        });
    }

    group.finish();
}

/// Quick spline type comparison
fn benchmark_fast_path_spline_types(c: &mut Criterion) {
    let mut group = c.benchmark_group("fast_path_spline_types");
    group.measurement_time(std::time::Duration::from_secs(5));
    group.sample_size(10);

    let spline_types = vec![(SplineType::Linear, "linear"), (SplineType::Quadratic, "quadratic"), (SplineType::Cubic, "cubic"), (SplineType::Polynomial(4), "polynomial_4")];

    // Use controlled dataset - 1200 measurements, 60 output points
    let measurements = create_test_measurements(1200);
    let start = measurements[0].timestamp;
    let end = start + chrono::Duration::hours(1); // 60 minutes = 60 points at 1-minute resolution

    for (spline_type, type_name) in spline_types {
        println!("🔍 Testing spline type: {}", type_name);

        // auto_interpolate
        group.bench_with_input(BenchmarkId::new("auto_interpolate", type_name), &spline_type, |b, spline_type| {
            b.iter(|| {
                auto_interpolate(
                    black_box(measurements.clone()),
                    black_box(start),
                    black_box(end),
                    black_box(Resolution::Minutes), // Use minutes to control output size
                    black_box(*spline_type),
                )
            });
        });

        // fast_path_interpolate
        group.bench_with_input(BenchmarkId::new("fast_path", type_name), &spline_type, |b, spline_type| {
            b.iter(|| {
                fast_path_interpolate(
                    black_box(measurements.clone()),
                    black_box(start),
                    black_box(end),
                    black_box(Resolution::Minutes), // Use minutes to control output size
                    black_box(*spline_type),
                )
            });
        });
    }

    group.finish();
}

criterion_group!(benches, benchmark_fast_path_vs_auto_interpolate, benchmark_fast_path_spline_types);
criterion_main!(benches);

#[cfg(test)]
mod tests {
    #[test]
    fn test_performance_analysis() {
        super::analyze_performance_characteristics();
    }
}
