use std::{hint::black_box, str::FromStr};

use bigdecimal::BigDecimal;
use chrono::{TimeZone, Utc};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use database::{auto_interpolate, fast_path_interpolate, optimized_interpolate, Measurement, Resolution, SplineType};
use uuid::Uuid;

fn create_benchmark_measurements(count: usize, dataset_id: Uuid, start_time: chrono::DateTime<chrono::Utc>, interval_seconds: i64) -> Vec<Measurement> {
	(0..count).map(|i| Measurement { id: Uuid::new_v4(), dataset_id, timestamp: start_time + chrono::Duration::seconds(i as i64 * interval_seconds), value: BigDecimal::from_str(&format!("{}.{}", i * 10, i % 10)).unwrap() }).collect()
}

fn bench_auto_interpolate_with_different_data_sizes(c: &mut Criterion) {
	let mut group = c.benchmark_group("auto_interpolate_data_sizes");

	for &size in &[50, 100, 200, 500] {
		let dataset_id = Uuid::new_v4();
		let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let measurements = create_benchmark_measurements(size, dataset_id, start_time, 10);

		let interpolation_start = start_time + chrono::Duration::seconds(50);
		let interpolation_end = start_time + chrono::Duration::seconds((size as i64 - 1) * 10 - 50);

		group.throughput(Throughput::Elements(size as u64));
		group.bench_with_input(BenchmarkId::new("measurements", size), &size, |b, _| {
			b.iter(|| {
				let result = auto_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(SplineType::Linear));
				black_box(result)
			});
		});
	}
	group.finish();
}

fn bench_auto_interpolate_spline_comparison(c: &mut Criterion) {
	let dataset_id = Uuid::new_v4();
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
	let measurements = create_benchmark_measurements(100, dataset_id, start_time, 10);

	let interpolation_start = start_time + chrono::Duration::seconds(50);
	let interpolation_end = start_time + chrono::Duration::seconds(950);

	let mut group = c.benchmark_group("auto_interpolate_spline_types");

	let spline_types = [("Linear", SplineType::Linear), ("Quadratic", SplineType::Quadratic), ("Cubic", SplineType::Cubic), ("Polynomial_2", SplineType::Polynomial(2)), ("Polynomial_3", SplineType::Polynomial(3))];

	for (name, spline_type) in spline_types {
		group.bench_with_input(BenchmarkId::new("spline_type", name), &spline_type, |b, &spline_type| {
			b.iter(|| {
				let result = auto_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(spline_type));
				black_box(result)
			});
		});
	}
	group.finish();
}

fn bench_auto_interpolate_resolution_comparison(c: &mut Criterion) {
	let dataset_id = Uuid::new_v4();
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
	let measurements = create_benchmark_measurements(60, dataset_id, start_time, 60); // Every minute

	let interpolation_start = start_time + chrono::Duration::minutes(5);
	let interpolation_end = start_time + chrono::Duration::minutes(55);

	let mut group = c.benchmark_group("auto_interpolate_resolutions");

	let resolutions = [("Seconds", Resolution::Seconds), ("Minutes", Resolution::Minutes)];

	for (name, resolution) in resolutions {
		group.bench_with_input(BenchmarkId::new("resolution", name), &resolution, |b, &resolution| {
			b.iter(|| {
				let result = auto_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(resolution), black_box(SplineType::Linear));
				black_box(result)
			});
		});
	}
	group.finish();
}

/// Benchmark optimized interpolation with different data sizes
fn bench_optimized_interpolate_data_sizes(c: &mut Criterion) {
	let mut group = c.benchmark_group("optimized_interpolate_data_sizes");

	for &size in &[50, 100, 200, 500, 1000, 2000] {
		let dataset_id = Uuid::new_v4();
		let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let measurements = create_benchmark_measurements(size, dataset_id, start_time, 10);

		let interpolation_start = start_time + chrono::Duration::seconds(50);
		let interpolation_end = start_time + chrono::Duration::seconds((size as i64 - 1) * 10 - 50);

		group.throughput(Throughput::Elements(size as u64));
		group.bench_with_input(BenchmarkId::new("measurements", size), &size, |b, _| {
			b.iter(|| {
				let result = optimized_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(SplineType::Linear));
				black_box(result)
			});
		});
	}
	group.finish();
}

/// Benchmark fast path vs standard for large datasets
fn bench_fast_path_vs_standard(c: &mut Criterion) {
	let mut group = c.benchmark_group("fast_path_vs_standard");

	let dataset_id = Uuid::new_v4();
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
	let size = 2000; // Large enough to trigger optimizations
	let measurements = create_benchmark_measurements(size, dataset_id, start_time, 10);

	let interpolation_start = start_time + chrono::Duration::seconds(50);
	let interpolation_end = start_time + chrono::Duration::seconds((size as i64 - 1) * 10 - 50);

	// Test with Cubic (should be optimized to Quadratic in fast path)
	group.bench_function("standard_cubic", |b| {
		b.iter(|| {
			let result = auto_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(SplineType::Cubic));
			black_box(result)
		});
	});

	group.bench_function("fast_path_cubic", |b| {
		b.iter(|| {
			let result = fast_path_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(SplineType::Cubic));
			black_box(result)
		});
	});

	group.finish();
}

// Update the criterion_group! macro to include new benchmarks
criterion_group!(integration_benches, bench_auto_interpolate_with_different_data_sizes, bench_auto_interpolate_spline_comparison, bench_auto_interpolate_resolution_comparison, bench_optimized_interpolate_data_sizes, bench_fast_path_vs_standard);
criterion_main!(integration_benches);
