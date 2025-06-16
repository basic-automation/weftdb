use std::{hint::black_box, str::FromStr};

use bigdecimal::BigDecimal;
use chrono::{DateTime, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use database::{auto_interpolate, optimized_interpolate, Measurement, Resolution, SplineType};
use uuid::Uuid;

fn create_benchmark_measurements(count: usize, dataset_id: Uuid, start_time: DateTime<Utc>, interval_seconds: i64) -> Vec<Measurement> {
	(0..count).map(|i| Measurement { id: Uuid::new_v4(), dataset_id, timestamp: start_time + chrono::Duration::seconds(i as i64 * interval_seconds), value: BigDecimal::from_str(&format!("{}.{}", i * 10, i % 10)).unwrap() }).collect()
}

fn bench_linear_interpolation(c: &mut Criterion) {
	let dataset_id = Uuid::new_v4();
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	let mut group = c.benchmark_group("linear_interpolation");

	for &size in &[10, 50, 100, 500, 1000, 5000] {
		let measurements = create_benchmark_measurements(size, dataset_id, start_time, 10);
		let interpolation_start = start_time + chrono::Duration::seconds(5);
		let interpolation_end = start_time + chrono::Duration::seconds((size as i64 - 1) * 10 - 5);

		group.throughput(Throughput::Elements(size as u64));
		group.bench_with_input(BenchmarkId::new("points", size), &size, |b, _| {
			b.iter(|| {
				let result = auto_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(SplineType::Linear));
				black_box(result)
			});
		});
	}
	group.finish();
}

fn bench_cubic_interpolation(c: &mut Criterion) {
	let dataset_id = Uuid::new_v4();
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	let mut group = c.benchmark_group("cubic_interpolation");

	for &size in &[10, 50, 100, 500, 1000] {
		let measurements = create_benchmark_measurements(size, dataset_id, start_time, 10);
		let interpolation_start = start_time + chrono::Duration::seconds(5);
		let interpolation_end = start_time + chrono::Duration::seconds((size as i64 - 1) * 10 - 5);

		group.throughput(Throughput::Elements(size as u64));
		group.bench_with_input(BenchmarkId::new("points", size), &size, |b, _| {
			b.iter(|| {
				let result = auto_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(SplineType::Cubic));
				black_box(result)
			});
		});
	}
	group.finish();
}

fn bench_quadratic_interpolation(c: &mut Criterion) {
	let dataset_id = Uuid::new_v4();
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	let mut group = c.benchmark_group("quadratic_interpolation");

	for &size in &[10, 50, 100, 500, 1000] {
		let measurements = create_benchmark_measurements(size, dataset_id, start_time, 10);
		let interpolation_start = start_time + chrono::Duration::seconds(5);
		let interpolation_end = start_time + chrono::Duration::seconds((size as i64 - 1) * 10 - 5);

		group.throughput(Throughput::Elements(size as u64));
		group.bench_with_input(BenchmarkId::new("points", size), &size, |b, _| {
			b.iter(|| {
				let result = auto_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(SplineType::Quadratic));
				black_box(result)
			});
		});
	}
	group.finish();
}

fn bench_polynomial_interpolation(c: &mut Criterion) {
	let dataset_id = Uuid::new_v4();
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	let mut group = c.benchmark_group("polynomial_interpolation");

	for &degree in &[2, 3, 4, 5] {
		let measurements = create_benchmark_measurements(100, dataset_id, start_time, 10);
		let interpolation_start = start_time + chrono::Duration::seconds(5);
		let interpolation_end = start_time + chrono::Duration::seconds(995);

		group.bench_with_input(BenchmarkId::new("degree", degree), &degree, |b, &degree| {
			b.iter(|| {
				let result = auto_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(SplineType::Polynomial(degree)));
				black_box(result)
			});
		});
	}
	group.finish();
}

fn bench_interpolation_resolutions(c: &mut Criterion) {
	let dataset_id = Uuid::new_v4();
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
	let measurements = create_benchmark_measurements(100, dataset_id, start_time, 60); // Every minute

	let mut group = c.benchmark_group("interpolation_resolutions");

	let resolutions = [("Seconds", Resolution::Seconds), ("Minutes", Resolution::Minutes), ("Hours", Resolution::Hours)];

	for (name, resolution) in resolutions {
		let interpolation_start = start_time + chrono::Duration::minutes(5);
		let interpolation_end = start_time + chrono::Duration::minutes(95);

		group.bench_with_input(BenchmarkId::new("resolution", name), &resolution, |b, &resolution| {
			b.iter(|| {
				let result = auto_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(resolution), black_box(SplineType::Linear));
				black_box(result)
			});
		});
	}
	group.finish();
}

fn bench_interpolation_vs_extrapolation(c: &mut Criterion) {
	let dataset_id = Uuid::new_v4();
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	// Create measurements from 100s to 900s
	let measurements = create_benchmark_measurements(9, dataset_id, start_time + chrono::Duration::seconds(100), 100);

	let mut group = c.benchmark_group("interpolation_vs_extrapolation");

	// Pure interpolation (within data range)
	group.bench_function("interpolation_only", |b| {
		let interpolation_start = start_time + chrono::Duration::seconds(200);
		let interpolation_end = start_time + chrono::Duration::seconds(800);
		b.iter(|| {
			let result = auto_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(SplineType::Linear));
			black_box(result)
		});
	});

	// Mixed interpolation and extrapolation
	group.bench_function("interpolation_and_extrapolation", |b| {
		let interpolation_start = start_time; // Before first measurement
		let interpolation_end = start_time + chrono::Duration::seconds(1000); // After last measurement
		b.iter(|| {
			let result = auto_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(SplineType::Linear));
			black_box(result)
		});
	});

	group.finish();
}

fn bench_spline_type_comparison(c: &mut Criterion) {
	let dataset_id = Uuid::new_v4();
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
	let measurements = create_benchmark_measurements(50, dataset_id, start_time, 10);

	let interpolation_start = start_time + chrono::Duration::seconds(5);
	let interpolation_end = start_time + chrono::Duration::seconds(485);

	let mut group = c.benchmark_group("spline_type_comparison");

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

/// Benchmark optimized interpolation performance across spline types
fn bench_optimized_spline_comparison(c: &mut Criterion) {
	let dataset_id = Uuid::new_v4();
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
	let measurements = create_benchmark_measurements(1000, dataset_id, start_time, 10); // Large enough for optimizations

	let interpolation_start = start_time + chrono::Duration::seconds(50);
	let interpolation_end = start_time + chrono::Duration::seconds(9950);

	let mut group = c.benchmark_group("optimized_spline_comparison");

	let spline_types = [("Linear", SplineType::Linear), ("Quadratic", SplineType::Quadratic), ("Cubic", SplineType::Cubic), ("Polynomial_2", SplineType::Polynomial(2)), ("Polynomial_3", SplineType::Polynomial(3))];

	for (name, spline_type) in spline_types {
		group.bench_with_input(BenchmarkId::new("spline_type", name), &spline_type, |b, &spline_type| {
			b.iter(|| {
				let result = optimized_interpolate(black_box(measurements.clone()), black_box(interpolation_start), black_box(interpolation_end), black_box(Resolution::Seconds), black_box(spline_type));
				black_box(result)
			});
		});
	}
	group.finish();
}

criterion_group!(benches, bench_linear_interpolation, bench_cubic_interpolation, bench_quadratic_interpolation, bench_polynomial_interpolation, bench_interpolation_resolutions, bench_interpolation_vs_extrapolation, bench_spline_type_comparison, bench_optimized_spline_comparison);
criterion_main!(benches);
