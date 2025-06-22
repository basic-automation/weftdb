use std::hint::black_box;
use std::str::FromStr;

use bigdecimal::BigDecimal;
use chrono::{DateTime, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use database::{
    auto_interpolate, Measurement, Resolution, SplineType,
    splines::gpu::test_gpu_availability,
    splines::linear::linear,
};
use fake::{Fake, Faker};
use uuid::Uuid;

fn create_measurement(dataset_id: Uuid, timestamp: DateTime<Utc>, value: f64) -> Measurement {
    Measurement { 
        id: Uuid::new_v4(), 
        dataset_id, 
        timestamp, 
        value: BigDecimal::from_str(&value.to_string()).unwrap() 
    }
}

fn generate_synthetic_measurements(count: usize, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<Measurement> {
    let dataset_id = Uuid::new_v4();
    let duration = end.signed_duration_since(start);
    let step = duration / count as i32;

    (0..count)
        .map(|i| {
            let timestamp = start + step * i as i32;
            let value: f64 = Faker.fake();
            create_measurement(dataset_id, timestamp, value * 100.0)
        })
        .collect()
}

/// Test GPU availability before running benchmarks (only once)
fn check_gpu_availability() -> bool {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        test_gpu_availability().await.unwrap_or(false)
    })
}

/// Large-scale GPU vs CPU comparison - Testing 5K to 200K measurements
fn gpu_vs_cpu_large_scale(c: &mut Criterion) {
    // Check GPU availability first (one-time check)
    let gpu_available = check_gpu_availability();
    if !gpu_available {
        println!("⚠️  Skipping GPU benchmarks - GPU not available");
        return;
    }

    println!("🚀 GPU available - running large-scale benchmarks (5K-200K measurements)");

    let mut group = c.benchmark_group("gpu_vs_cpu_large_scale");
    group.sample_size(10); // Reduce sample size for large benchmarks
    group.measurement_time(std::time::Duration::from_secs(20)); // Longer measurement time

    // Test configurations - large datasets with various output densities
    let test_configs = [
        (5_000, 100_000),   // 5K measurements → 100K output points
        (10_000, 150_000),  // 10K measurements → 150K output points
        (25_000, 200_000),  // 25K measurements → 200K output points
    ];

    for (measurement_count, output_points) in test_configs {
        println!("🔍 Testing: {} measurements → {} output points", measurement_count, output_points);

        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end = start + chrono::Duration::seconds(output_points as i64);
        let measurements = generate_synthetic_measurements(measurement_count, start, end);

        group.throughput(Throughput::Elements(output_points as u64));

        // CPU Linear benchmark
        group.bench_with_input(
            BenchmarkId::new("cpu_linear", format!("{}k_to_{}k", measurement_count / 1000, output_points / 1000)), 
            &(measurements.clone(), start, end), 
            |b, (measurements, start, end)| {
                b.iter(|| {
                    black_box(linear(
                        black_box(measurements.clone()), 
                        black_box(*start), 
                        black_box(*end), 
                        black_box(Resolution::Seconds)
                    ).unwrap())
                });
            }
        );

        // GPU with auto_interpolate benchmark
        group.bench_with_input(
            BenchmarkId::new("gpu_auto", format!("{}k_to_{}k", measurement_count / 1000, output_points / 1000)), 
            &(measurements.clone(), start, end), 
            |b, (measurements, start, end)| {
                let rt = tokio::runtime::Runtime::new().unwrap();
                b.iter(|| {
                    rt.block_on(async {
                        black_box(auto_interpolate(
                            black_box(measurements.clone()),
                            black_box(*start),
                            black_box(*end),
                            black_box(Resolution::Seconds),
                            black_box(SplineType::Linear),
                        ).await.unwrap())
                    })
                });
            }
        );
    }

    group.finish();
}

/// GPU crossover point analysis - Find where GPU becomes beneficial
fn gpu_crossover_analysis(c: &mut Criterion) {
    let gpu_available = check_gpu_availability();
    if !gpu_available {
        println!("⚠️  Skipping GPU crossover analysis - GPU not available");
        return;
    }

    println!("📊 Running GPU crossover analysis");

    let mut group = c.benchmark_group("gpu_crossover_analysis");
    group.sample_size(10);
    group.measurement_time(std::time::Duration::from_secs(15));

    // Focus on the crossover region where GPU might become beneficial
    let crossover_configs = [
        (1_000, 25_000),   // At expected threshold
        (2_000, 50_000),   // Above expected threshold
        (5_000, 100_000),  // High performance region
    ];

    for (measurement_count, output_points) in crossover_configs {
        println!("🎯 Crossover test: {}K → {}K", measurement_count / 1000, output_points / 1000);

        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end = start + chrono::Duration::seconds(output_points as i64);
        let measurements = generate_synthetic_measurements(measurement_count, start, end);

        let config_name = format!("{}k_to_{}k", measurement_count / 1000, output_points / 1000);

        group.throughput(Throughput::Elements(output_points as u64));

        // CPU benchmark
        group.bench_with_input(
            BenchmarkId::new("cpu", &config_name), 
            &(measurements.clone(), start, end), 
            |b, (measurements, start, end)| {
                b.iter(|| {
                    black_box(linear(
                        black_box(measurements.clone()), 
                        black_box(*start), 
                        black_box(*end), 
                        black_box(Resolution::Seconds)
                    ).unwrap())
                });
            }
        );

        // GPU benchmark
        group.bench_with_input(
            BenchmarkId::new("gpu", &config_name), 
            &(measurements.clone(), start, end), 
            |b, (measurements, start, end)| {
                let rt = tokio::runtime::Runtime::new().unwrap();
                b.iter(|| {
                    rt.block_on(async {
                        black_box(auto_interpolate(
                            black_box(measurements.clone()),
                            black_box(*start),
                            black_box(*end),
                            black_box(Resolution::Seconds),
                            black_box(SplineType::Linear),
                        ).await.unwrap())
                    })
                });
            }
        );
    }

    group.finish();
}

/// Test all spline types with GPU acceleration
fn gpu_spline_type_comparison(c: &mut Criterion) {
    let gpu_available = check_gpu_availability();
    if !gpu_available {
        println!("⚠️  Skipping GPU spline comparison - GPU not available");
        return;
    }

    println!("🔍 Testing GPU acceleration across spline types");

    let mut group = c.benchmark_group("gpu_spline_type_comparison");
    group.sample_size(10);
    group.measurement_time(std::time::Duration::from_secs(15));

    let measurement_count = 2_000;
    let output_points = 50_000;

    let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
    let end = start + chrono::Duration::seconds(output_points as i64);
    let measurements = generate_synthetic_measurements(measurement_count, start, end);

    let spline_types = [
        ("Linear", SplineType::Linear), 
        ("Quadratic", SplineType::Quadratic), 
        ("Cubic", SplineType::Cubic)
    ];

    for (name, spline_type) in spline_types {
        println!("🎯 Testing GPU {} interpolation", name);

        group.throughput(Throughput::Elements(output_points as u64));

        // CPU benchmark
        group.bench_with_input(
            BenchmarkId::new("cpu", name), 
            &spline_type, 
            |b, &spline_type| {
                b.iter(|| {
                    black_box(match spline_type {
                        SplineType::Linear => linear(measurements.clone(), start, end, Resolution::Seconds),
                        SplineType::Quadratic => database::splines::quadratic::quadratic(measurements.clone(), start, end, Resolution::Seconds),
                        SplineType::Cubic => database::splines::cubic::cubic(measurements.clone(), start, end, Resolution::Seconds),
                        SplineType::Polynomial(degree) => database::splines::polynomial::polynomial(measurements.clone(), start, end, Resolution::Seconds, degree),
                    }.unwrap())
                });
            }
        );

        // GPU benchmark
        group.bench_with_input(
            BenchmarkId::new("gpu", name), 
            &spline_type, 
            |b, &spline_type| {
                let rt = tokio::runtime::Runtime::new().unwrap();
                b.iter(|| {
                    rt.block_on(async {
                        black_box(auto_interpolate(
                            black_box(measurements.clone()), 
                            black_box(start), 
                            black_box(end), 
                            black_box(Resolution::Seconds), 
                            black_box(spline_type)
                        ).await.unwrap())
                    })
                });
            }
        );
    }

    group.finish();
}

criterion_group!(benches, gpu_vs_cpu_large_scale, gpu_crossover_analysis, gpu_spline_type_comparison);
criterion_main!(benches);
