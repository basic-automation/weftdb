use std::{hint::black_box, str::FromStr};

use bigdecimal::BigDecimal;
use chrono::{DateTime, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use database::{
    splines::{linear, gpu_linear_interpolate_with_fallback, gpu::test_gpu_availability, Resolution}, 
    Measurement
};
use fake::{Fake, Faker};
use uuid::Uuid;

fn create_measurement(dataset_id: Uuid, timestamp: DateTime<Utc>, value: f64) -> Measurement {
    Measurement {
        id: Uuid::new_v4(),
        dataset_id,
        timestamp,
        value: BigDecimal::from_str(&value.to_string()).unwrap(),
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
        match test_gpu_availability().await {
            Ok(available) => available,
            Err(_) => false
        }
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
        (5_000, 100_000),    // 5K measurements → 100K output points
        (10_000, 150_000),   // 10K measurements → 150K output points  
        (25_000, 200_000),   // 25K measurements → 200K output points
        (50_000, 250_000),   // 50K measurements → 250K output points
        (100_000, 300_000),  // 100K measurements → 300K output points
        (200_000, 500_000),  // 200K measurements → 500K output points
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
                        black_box(Resolution::Seconds),
                    ).unwrap())
                });
            },
        );
        
        // GPU with optimized fallback benchmark
        group.bench_with_input(
            BenchmarkId::new("gpu_linear_optimized", format!("{}k_to_{}k", measurement_count / 1000, output_points / 1000)),
            &(measurements.clone(), start, end),
            |b, (measurements, start, end)| {
                let rt = tokio::runtime::Runtime::new().unwrap();
                b.iter_custom(|iters| {
                    let measurements = measurements.clone();
                    let start = *start;
                    let end = *end;
                    
                    let start_time = std::time::Instant::now();
                    rt.block_on(async {
                        for _ in 0..iters {
                            // Generate target times for GPU
                            let mut target_times = Vec::new();
                            let mut current = start;
                            let step = chrono::Duration::seconds(1);
                            while current <= end {
                                target_times.push(current);
                                current = current + step;
                                
                                // Safety check to prevent infinite loops
                                if target_times.len() >= output_points {
                                    break;
                                }
                            }
                            
                            let dataset_id = measurements[0].dataset_id;
                            black_box(gpu_linear_interpolate_with_fallback(
                                black_box(measurements.clone()),
                                black_box(target_times),
                                black_box(dataset_id),
                            ).await.unwrap());
                        }
                    });
                    start_time.elapsed()
                });
            },
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
        (5_000, 50_000),     // Below expected threshold
        (5_000, 100_000),    // At potential threshold
        (5_000, 200_000),    // Above expected threshold
        (10_000, 100_000),   // More input data
        (10_000, 200_000),   // More input + output
        (20_000, 200_000),   // High input density
        (50_000, 500_000),   // Very large scale
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
                        black_box(Resolution::Seconds),
                    ).unwrap())
                });
            },
        );
        
        // GPU benchmark
        group.bench_with_input(
            BenchmarkId::new("gpu", &config_name),
            &(measurements.clone(), start, end),
            |b, (measurements, start, end)| {
                let rt = tokio::runtime::Runtime::new().unwrap();
                b.iter_custom(|iters| {
                    let measurements = measurements.clone();
                    let start = *start;
                    let end = *end;
                    
                    let start_time = std::time::Instant::now();
                    rt.block_on(async {
                        for _ in 0..iters {
                            let mut target_times = Vec::new();
                            let mut current = start;
                            let step = chrono::Duration::seconds(1);
                            while current <= end && target_times.len() < output_points {
                                target_times.push(current);
                                current = current + step;
                            }
                            
                            let dataset_id = measurements[0].dataset_id;
                            black_box(gpu_linear_interpolate_with_fallback(
                                black_box(measurements.clone()),
                                black_box(target_times),
                                black_box(dataset_id),
                            ).await.unwrap());
                        }
                    });
                    start_time.elapsed()
                });
            },
        );
    }
    
    group.finish();
}

/// Test GPU scaling with fixed input, varying output
fn gpu_output_scaling(c: &mut Criterion) {
    let gpu_available = check_gpu_availability();
    if !gpu_available {
        println!("⚠️  Skipping GPU output scaling - GPU not available");
        return;
    }
    
    println!("📈 Testing GPU output scaling");
    
    let mut group = c.benchmark_group("gpu_output_scaling");
    group.sample_size(10);
    group.measurement_time(std::time::Duration::from_secs(10));
    
    // Fixed input size, varying output density
    let fixed_input = 10_000;
    let output_sizes = [50_000, 100_000, 200_000, 300_000, 500_000, 750_000, 1_000_000];
    
    let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
    let measurements = generate_synthetic_measurements(
        fixed_input, 
        start, 
        start + chrono::Duration::hours(24)
    );

    for &output_points in &output_sizes {
        println!("📊 Testing {} output points", output_points);
        
        let end = start + chrono::Duration::seconds(output_points as i64);
        let config_name = format!("{}k_out", output_points / 1000);
        
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
                        black_box(Resolution::Seconds),
                    ).unwrap())
                });
            },
        );
        
        // GPU benchmark
        group.bench_with_input(
            BenchmarkId::new("gpu", &config_name),
            &(measurements.clone(), start, end),
            |b, (measurements, start, end)| {
                let rt = tokio::runtime::Runtime::new().unwrap();
                b.iter_custom(|iters| {
                    let measurements = measurements.clone();
                    let start = *start;
                    let end = *end;
                    
                    let start_time = std::time::Instant::now();
                    rt.block_on(async {
                        for _ in 0..iters {
                            let mut target_times = Vec::new();
                            let mut current = start;
                            let step = chrono::Duration::seconds(1);
                            while current <= end && target_times.len() < output_points {
                                target_times.push(current);
                                current = current + step;
                            }
                            
                            let dataset_id = measurements[0].dataset_id;
                            black_box(gpu_linear_interpolate_with_fallback(
                                black_box(measurements.clone()),
                                black_box(target_times),
                                black_box(dataset_id),
                            ).await.unwrap());
                        }
                    });
                    start_time.elapsed()
                });
            },
        );
    }
    
    group.finish();
}

/// Test GPU scaling with varying input, fixed output
fn gpu_input_scaling(c: &mut Criterion) {
    let gpu_available = check_gpu_availability();
    if !gpu_available {
        println!("⚠️  Skipping GPU input scaling - GPU not available");
        return;
    }
    
    println!("📈 Testing GPU input scaling");
    
    let mut group = c.benchmark_group("gpu_input_scaling");
    group.sample_size(10);
    group.measurement_time(std::time::Duration::from_secs(10));
    
    // Fixed output size, varying input density
    let fixed_output = 200_000;
    let input_sizes = [5_000, 10_000, 25_000, 50_000, 100_000, 150_000, 200_000];
    
    let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
    let end = start + chrono::Duration::seconds(fixed_output as i64);

    for &input_count in &input_sizes {
        println!("📊 Testing {} input measurements", input_count);
        
        let measurements = generate_synthetic_measurements(input_count, start, end);
        let config_name = format!("{}k_in", input_count / 1000);
        
        group.throughput(Throughput::Elements(fixed_output as u64));
        
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
                        black_box(Resolution::Seconds),
                    ).unwrap())
                });
            },
        );
        
        // GPU benchmark
        group.bench_with_input(
            BenchmarkId::new("gpu", &config_name),
            &(measurements.clone(), start, end),
            |b, (measurements, start, end)| {
                let rt = tokio::runtime::Runtime::new().unwrap();
                b.iter_custom(|iters| {
                    let measurements = measurements.clone();
                    let start = *start;
                    let end = *end;
                    
                    let start_time = std::time::Instant::now();
                    rt.block_on(async {
                        for _ in 0..iters {
                            let mut target_times = Vec::new();
                            let mut current = start;
                            let step = chrono::Duration::seconds(1);
                            while current <= end && target_times.len() < fixed_output {
                                target_times.push(current);
                                current = current + step;
                            }
                            
                            let dataset_id = measurements[0].dataset_id;
                            black_box(gpu_linear_interpolate_with_fallback(
                                black_box(measurements.clone()),
                                black_box(target_times),
                                black_box(dataset_id),
                            ).await.unwrap());
                        }
                    });
                    start_time.elapsed()
                });
            },
        );
    }
    
    group.finish();
}

criterion_group!(
    benches, 
    gpu_vs_cpu_large_scale,
    gpu_crossover_analysis,
    gpu_output_scaling,
    gpu_input_scaling
);
criterion_main!(benches);
