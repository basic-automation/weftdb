use std::{hint::black_box, str::FromStr};

use bigdecimal::BigDecimal;
use chrono::{DateTime, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use database::{auto_interpolate, fast_path_interpolate, optimized_interpolate, streaming_interpolate, Measurement, Resolution, SplineType};
use uuid::Uuid;

fn create_benchmark_measurements(count: usize, dataset_id: Uuid, start_time: DateTime<Utc>, interval_seconds: i64) -> Vec<Measurement> {
	(0..count).map(|i| Measurement { id: Uuid::new_v4(), dataset_id, timestamp: start_time + chrono::Duration::seconds(i as i64 * interval_seconds), value: BigDecimal::from_str(&format!("{}.{}", i * 10, i % 10)).unwrap() }).collect()
}

/// Benchmark optimized interpolation vs standard interpolation across dataset sizes
fn bench_optimized_vs_standard_performance(c: &mut Criterion) {
	let mut group = c.benchmark_group("optimized_vs_standard");

	// Test sizes that trigger different optimization paths
	let test_sizes = [100, 500, 1000, 2000, 5000];

	for &size in &test_sizes {
		let dataset_id = Uuid::new_v4();
		let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let measurements = create_benchmark_measurements(size, dataset_id, start_time, 10);

		let interpolation_start = start_time + chrono::Duration::seconds(50);
		let interpolation_end = start_time + chrono::Duration::seconds((size as i64 - 1) * 10 - 50);

		group.throughput(Throughput::Elements(size as u64));

		// Standard auto_interpolate
		group.bench_with_input(BenchmarkId::new("standard", size), &size, |b, _| {
			b.iter(|| {
				let result = auto_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(SplineType::Cubic));
				black_box(result)
			});
		});

		// Optimized interpolate
		group.bench_with_input(BenchmarkId::new("optimized", size), &size, |b, _| {
			b.iter(|| {
				let result = optimized_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(SplineType::Cubic));
				black_box(result)
			});
		});
	}

	group.finish();
}

/// Benchmark fast path optimizations for different spline types
fn bench_fast_path_optimizations(c: &mut Criterion) {
	let mut group = c.benchmark_group("fast_path_optimizations");

	let dataset_id = Uuid::new_v4();
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	// Test size that triggers fast path optimizations (>500 for cubic)
	let size = 1000;
	let measurements = create_benchmark_measurements(size, dataset_id, start_time, 10);

	let interpolation_start = start_time + chrono::Duration::seconds(50);
	let interpolation_end = start_time + chrono::Duration::seconds((size as i64 - 1) * 10 - 50);

	let spline_types = [("Cubic", SplineType::Cubic), ("Quadratic", SplineType::Quadratic), ("Linear", SplineType::Linear), ("Polynomial_3", SplineType::Polynomial(3)), ("Polynomial_5", SplineType::Polynomial(5))];

	for (name, spline_type) in spline_types {
		// Standard implementation
		group.bench_with_input(BenchmarkId::new("standard", name), &spline_type, |b, &spline_type| {
			b.iter(|| {
				let result = auto_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(spline_type));
				black_box(result)
			});
		});

		// Fast path implementation
		group.bench_with_input(BenchmarkId::new("fast_path", name), &spline_type, |b, &spline_type| {
			b.iter(|| {
				let result = fast_path_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(spline_type));
				black_box(result)
			});
		});
	}

	group.finish();
}

/// Benchmark parallel processing thresholds
fn bench_parallel_thresholds(c: &mut Criterion) {
	let mut group = c.benchmark_group("parallel_thresholds");

	let dataset_id = Uuid::new_v4();
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	// Test sizes around the parallel threshold (1000)
	let test_sizes = [500, 1000, 1500, 2000, 3000];

	for &size in &test_sizes {
		let measurements = create_benchmark_measurements(size, dataset_id, start_time, 10);

		let interpolation_start = start_time + chrono::Duration::seconds(50);
		let interpolation_end = start_time + chrono::Duration::seconds((size as i64 - 1) * 10 - 50);

		group.throughput(Throughput::Elements(size as u64));
		group.bench_with_input(BenchmarkId::new("size", size), &size, |b, _| {
			b.iter(|| {
				let result = optimized_interpolate(
					black_box(measurements.clone()),
					black_box(interpolation_start),
					black_box(interpolation_end),
					black_box(Resolution::Seconds),
					black_box(SplineType::Linear), // Use linear for consistent parallel behavior
				);
				black_box(result)
			});
		});
	}

	group.finish();
}

/// Benchmark streaming interpolation for very large datasets
fn bench_streaming_interpolation(c: &mut Criterion) {
	let mut group = c.benchmark_group("streaming_interpolation");

	let dataset_id = Uuid::new_v4();
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	// Test large dataset sizes
	let test_sizes = [5000, 10000, 20000];
	let chunk_sizes = [500, 1000, 2000];

	for &size in &test_sizes {
		let measurements = create_benchmark_measurements(size, dataset_id, start_time, 10);

		let interpolation_start = start_time + chrono::Duration::seconds(50);
		let interpolation_end = start_time + chrono::Duration::seconds((size as i64 - 1) * 10 - 50);

		for &chunk_size in &chunk_sizes {
			group.throughput(Throughput::Elements(size as u64));
			group.bench_with_input(BenchmarkId::new(format!("size_{}_chunk_{}", size, chunk_size), size), &size, |b, _| {
				b.iter(|| {
					let result = streaming_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(SplineType::Linear), black_box(chunk_size));
					black_box(result)
				});
			});
		}
	}

	group.finish();
}

/// Benchmark algorithm selection effectiveness
fn bench_algorithm_selection_effectiveness(c: &mut Criterion) {
	let mut group = c.benchmark_group("algorithm_selection");

	let dataset_id = Uuid::new_v4();
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	// Test the algorithm selection logic with different dataset sizes
	let test_cases = [
		(500, SplineType::Cubic, "cubic_500"),
		(1000, SplineType::Cubic, "cubic_1000"), // Should switch to quadratic
		(2000, SplineType::Quadratic, "quadratic_2000"),
		(6000, SplineType::Quadratic, "quadratic_6000"), // Should switch to linear
		(1000, SplineType::Polynomial(5), "poly5_1000"), // Should limit degree
	];

	for (size, requested_spline, name) in test_cases {
		let measurements = create_benchmark_measurements(size, dataset_id, start_time, 10);

		let interpolation_start = start_time + chrono::Duration::seconds(50);
		let interpolation_end = start_time + chrono::Duration::seconds((size as i64 - 1) * 10 - 50);

		group.throughput(Throughput::Elements(size as u64));

		// Requested algorithm (no optimization)
		group.bench_with_input(BenchmarkId::new("requested", name), &size, |b, _| {
			b.iter(|| {
				let result = auto_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(requested_spline));
				black_box(result)
			});
		});

		// Optimized algorithm selection
		group.bench_with_input(BenchmarkId::new("optimized", name), &size, |b, _| {
			b.iter(|| {
				let result = optimized_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(requested_spline));
				black_box(result)
			});
		});
	}

	group.finish();
}

/// Benchmark memory usage patterns (indirect through timing)
fn bench_memory_usage_patterns(c: &mut Criterion) {
	let mut group = c.benchmark_group("memory_patterns");

	let dataset_id = Uuid::new_v4();
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	// Test different processing strategies for large datasets
	let size = 10000;
	let measurements = create_benchmark_measurements(size, dataset_id, start_time, 10);

	let interpolation_start = start_time + chrono::Duration::seconds(50);
	let interpolation_end = start_time + chrono::Duration::seconds((size as i64 - 1) * 10 - 50);

	group.throughput(Throughput::Elements(size as u64));

	// Standard processing (all in memory)
	group.bench_function("standard_memory", |b| {
		b.iter(|| {
			let result = auto_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(SplineType::Linear));
			black_box(result)
		});
	});

	// Streaming processing (chunked)
	group.bench_function("streaming_memory", |b| {
		b.iter(|| {
			let result = streaming_interpolate(
				black_box(measurements.clone()),
				black_box(interpolation_start),
				black_box(interpolation_end),
				black_box(Resolution::Seconds),
				black_box(SplineType::Linear),
				black_box(1000), // 1K chunk size
			);
			black_box(result)
		});
	});

	// Optimized processing (automatic selection)
	group.bench_function("optimized_memory", |b| {
		b.iter(|| {
			let result = optimized_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(SplineType::Linear));
			black_box(result)
		});
	});

	group.finish();
}

/// Benchmark resolution impact on optimized functions
fn bench_resolution_impact(c: &mut Criterion) {
	let mut group = c.benchmark_group("optimized_resolutions");

	let dataset_id = Uuid::new_v4();
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
	let measurements = create_benchmark_measurements(1000, dataset_id, start_time, 60); // Every minute

	let interpolation_start = start_time + chrono::Duration::minutes(5);
	let interpolation_end = start_time + chrono::Duration::minutes(55);

	let resolutions = [("Seconds", Resolution::Seconds), ("Minutes", Resolution::Minutes), ("Hours", Resolution::Hours)];

	for (name, resolution) in resolutions {
		group.bench_with_input(BenchmarkId::new("resolution", name), &resolution, |b, &resolution| {
			b.iter(|| {
				let result = optimized_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(resolution), black_box(SplineType::Quadratic));
				black_box(result)
			});
		});
	}

	group.finish();
}

criterion_group!(optimized_benches, bench_optimized_vs_standard_performance, bench_fast_path_optimizations, bench_parallel_thresholds, bench_streaming_interpolation, bench_algorithm_selection_effectiveness, bench_memory_usage_patterns, bench_resolution_impact);
criterion_main!(optimized_benches);
