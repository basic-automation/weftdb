use std::str::FromStr;

use bigdecimal::BigDecimal;
use chrono::{DateTime, TimeZone, Utc};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use database::{
	splines::{simd::*, Resolution, SplineType}, Measurement
};
use uuid::Uuid;

fn create_test_measurements(count: usize, dataset_id: Uuid) -> Vec<Measurement> {
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	(0..count)
		.map(|i| {
			let timestamp = start_time + chrono::Duration::seconds(i as i64);
			let value = BigDecimal::from_str(&format!("{}.{}", i * 10, i % 100)).unwrap();

			Measurement { id: Uuid::new_v4(), dataset_id, timestamp, value }
		})
		.collect()
}

fn create_target_times(start: DateTime<Utc>, count: usize, step_ms: i64) -> Vec<DateTime<Utc>> {
	(0..count).map(|i| start + chrono::Duration::milliseconds(i as i64 * step_ms)).collect()
}

fn benchmark_simd_linear(c: &mut Criterion) {
	let mut group = c.benchmark_group("simd_linear_interpolation");

	let dataset_id = Uuid::new_v4();
	let measurements = create_test_measurements(1000, dataset_id);
	let start_time = measurements[0].timestamp;

	for target_count in [32, 64, 128, 256, 512, 1024].iter() {
		let target_times = create_target_times(start_time, *target_count, 500);

		group.throughput(Throughput::Elements(*target_count as u64));
		group.bench_with_input(BenchmarkId::new("simd_batch", target_count), target_count, |b, _| b.iter(|| linear_simd_batch(black_box(&measurements), black_box(&target_times), black_box(dataset_id))));
	}

	group.finish();
}

fn benchmark_simd_quadratic(c: &mut Criterion) {
	let mut group = c.benchmark_group("simd_quadratic_interpolation");

	let dataset_id = Uuid::new_v4();
	let measurements = create_test_measurements(1000, dataset_id);
	let start_time = measurements[0].timestamp;

	for target_count in [32, 64, 128, 256, 512, 1024].iter() {
		let target_times = create_target_times(start_time, *target_count, 500);

		group.throughput(Throughput::Elements(*target_count as u64));
		group.bench_with_input(BenchmarkId::new("simd_batch", target_count), target_count, |b, _| b.iter(|| quadratic_simd_batch(black_box(&measurements), black_box(&target_times), black_box(dataset_id))));
	}

	group.finish();
}

fn benchmark_simd_cubic(c: &mut Criterion) {
	let mut group = c.benchmark_group("simd_cubic_interpolation");

	let dataset_id = Uuid::new_v4();
	let measurements = create_test_measurements(1000, dataset_id);
	let start_time = measurements[0].timestamp;

	for target_count in [32, 64, 128, 256, 512, 1024].iter() {
		let target_times = create_target_times(start_time, *target_count, 500);

		group.throughput(Throughput::Elements(*target_count as u64));
		group.bench_with_input(BenchmarkId::new("simd_batch", target_count), target_count, |b, _| b.iter(|| cubic_simd_batch(black_box(&measurements), black_box(&target_times), black_box(dataset_id))));
	}

	group.finish();
}

fn benchmark_simd_polynomial(c: &mut Criterion) {
	let mut group = c.benchmark_group("simd_polynomial_interpolation");

	let dataset_id = Uuid::new_v4();
	let measurements = create_test_measurements(1000, dataset_id);
	let start_time = measurements[0].timestamp;

	for degree in [2, 3, 4, 5].iter() {
		let target_times = create_target_times(start_time, 256, 500);

		group.throughput(Throughput::Elements(256));
		group.bench_with_input(BenchmarkId::new("polynomial_degree", degree), degree, |b, &degree| b.iter(|| polynomial_simd_batch(black_box(&measurements), black_box(&target_times), black_box(degree), black_box(dataset_id))));
	}

	group.finish();
}

fn benchmark_simd_vs_scalar(c: &mut Criterion) {
	let mut group = c.benchmark_group("simd_vs_scalar_comparison");

	let dataset_id = Uuid::new_v4();
	let measurements = create_test_measurements(1000, dataset_id);
	let start_time = measurements[0].timestamp;
	let target_times = create_target_times(start_time, 512, 500);

	// SIMD Linear
	group.bench_function("simd_linear", |b| b.iter(|| linear_simd_batch(black_box(&measurements), black_box(&target_times), black_box(dataset_id))));

	// Scalar Linear (via auto_interpolate)
	group.bench_function("scalar_linear", |b| b.iter(|| database::splines::linear::linear(black_box(measurements.clone()), black_box(target_times[0]), black_box(target_times[target_times.len() - 1]), black_box(Resolution::Milliseconds))));

	// SIMD Cubic
	group.bench_function("simd_cubic", |b| b.iter(|| cubic_simd_batch(black_box(&measurements), black_box(&target_times), black_box(dataset_id))));

	// Scalar Cubic
	group.bench_function("scalar_cubic", |b| b.iter(|| database::splines::cubic::cubic(black_box(measurements.clone()), black_box(target_times[0]), black_box(target_times[target_times.len() - 1]), black_box(Resolution::Milliseconds))));

	group.finish();
}

fn benchmark_auto_interpolate_simd(c: &mut Criterion) {
	let mut group = c.benchmark_group("auto_interpolate_simd");

	let dataset_id = Uuid::new_v4();
	let measurements = create_test_measurements(1000, dataset_id);
	let start_time = measurements[0].timestamp;
	let target_times = create_target_times(start_time, 256, 500);

	let spline_types = vec![("Linear", SplineType::Linear), ("Quadratic", SplineType::Quadratic), ("Cubic", SplineType::Cubic), ("Polynomial_3", SplineType::Polynomial(3)), ("Polynomial_5", SplineType::Polynomial(5))];

	for (name, spline_type) in spline_types {
		group.bench_with_input(BenchmarkId::new("spline_type", name), &spline_type, |b, spline_type| b.iter(|| auto_interpolate_simd(black_box(measurements.clone()), black_box(target_times.clone()), black_box(*spline_type))));
	}

	group.finish();
}

fn benchmark_simd_threshold_performance(c: &mut Criterion) {
	let mut group = c.benchmark_group("simd_threshold_analysis");

	let dataset_id = Uuid::new_v4();
	let measurements = create_test_measurements(1000, dataset_id);
	let start_time = measurements[0].timestamp;

	// Test different target sizes around the SIMD threshold (32)
	for target_count in [8, 16, 24, 32, 48, 64, 96, 128].iter() {
		let target_times = create_target_times(start_time, *target_count, 500);

		group.throughput(Throughput::Elements(*target_count as u64));
		group.bench_with_input(BenchmarkId::new("target_count", target_count), target_count, |b, _| b.iter(|| linear_simd_batch(black_box(&measurements), black_box(&target_times), black_box(dataset_id))));
	}

	group.finish();
}

fn benchmark_large_dataset_simd(c: &mut Criterion) {
	let mut group = c.benchmark_group("large_dataset_simd_performance");

	let dataset_id = Uuid::new_v4();
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
	let target_times = create_target_times(start_time, 1024, 500);

	// Test with different measurement dataset sizes
	for measurement_count in [100, 500, 1000, 2000, 5000].iter() {
		let measurements = create_test_measurements(*measurement_count, dataset_id);

		group.throughput(Throughput::Elements(1024));
		group.bench_with_input(BenchmarkId::new("measurement_count", measurement_count), measurement_count, |b, _| b.iter(|| linear_simd_batch(black_box(&measurements), black_box(&target_times), black_box(dataset_id))));
	}

	group.finish();
}

criterion_group!(simd_benches, benchmark_simd_linear, benchmark_simd_quadratic, benchmark_simd_cubic, benchmark_simd_polynomial, benchmark_simd_vs_scalar, benchmark_auto_interpolate_simd, benchmark_simd_threshold_performance, benchmark_large_dataset_simd);

criterion_main!(simd_benches);
