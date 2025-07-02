use std::{hint::black_box, str::FromStr};

use bigdecimal::BigDecimal;
use chrono::{TimeZone, Utc};
use criterion::{criterion_group, criterion_main, Criterion};
use database::{auto_interpolate, Measurement, Resolution, SplineType};
use tokio::runtime::Runtime;
use uuid::Uuid;

fn create_test_measurements(count: usize, interval_minutes: i64) -> Vec<Measurement> {
	let dataset_id = Uuid::new_v4();
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	(0..count).map(|i| Measurement { id: Uuid::new_v4(), dataset_id, timestamp: start_time + chrono::Duration::minutes(i as i64 * interval_minutes), value: BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap() }).collect()
}

fn benchmark_interpolation_sizes(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	let sizes = vec![100, 500, 1000, 2000];

	for size in sizes {
		let measurements = create_test_measurements(size, 1); // 1 minute intervals
		let start_time = measurements[0].timestamp;
		let end_time = start_time + chrono::Duration::minutes(10); // 10 minute window

		c.bench_function(&format!("interpolation_size_{}", size), |b| b.iter(|| rt.block_on(async { black_box(auto_interpolate(black_box(measurements.clone()), black_box(start_time), black_box(end_time), black_box(Resolution::Minutes), black_box(SplineType::Linear)).await.unwrap()) })));
	}
}

fn benchmark_interpolation_resolutions(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();
	let measurements = create_test_measurements(200, 1);
	let start_time = measurements[0].timestamp;

	let resolutions = vec![
		("seconds", Resolution::Seconds, 5),  // 5 minutes for seconds
		("minutes", Resolution::Minutes, 60), // 1 hour for minutes
		("hours", Resolution::Hours, 4),      // 4 hours for hours
		("days", Resolution::Days, 1),        // 1 day for days
	];

	for (name, resolution, duration_amount) in resolutions {
		let interpolation_end = match resolution {
			Resolution::Seconds => start_time + chrono::Duration::minutes(duration_amount),
			Resolution::Minutes => start_time + chrono::Duration::minutes(duration_amount),
			Resolution::Hours => start_time + chrono::Duration::hours(duration_amount),
			Resolution::Days => start_time + chrono::Duration::days(duration_amount),
			_ => start_time + chrono::Duration::minutes(duration_amount),
		};

		c.bench_function(&format!("interpolation_resolution_{}", name), |b| b.iter(|| rt.block_on(async { black_box(auto_interpolate(black_box(measurements.clone()), black_box(start_time), black_box(interpolation_end), black_box(resolution), black_box(SplineType::Linear)).await.unwrap()) })));
	}
}

fn benchmark_spline_types(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();
	let measurements = create_test_measurements(300, 1);
	let start_time = measurements[0].timestamp;
	let end_time = start_time + chrono::Duration::minutes(10);

	let spline_types = vec![("linear", SplineType::Linear), ("quadratic", SplineType::Quadratic), ("cubic", SplineType::Cubic), ("polynomial_2", SplineType::Polynomial(2)), ("polynomial_3", SplineType::Polynomial(3))];

	for (name, spline_type) in spline_types {
		c.bench_function(&format!("spline_type_{}", name), |b| b.iter(|| rt.block_on(async { black_box(auto_interpolate(black_box(measurements.clone()), black_box(start_time), black_box(end_time), black_box(Resolution::Minutes), black_box(spline_type)).await.unwrap()) })));
	}
}

criterion_group!(benches, benchmark_interpolation_sizes, benchmark_interpolation_resolutions, benchmark_spline_types);
criterion_main!(benches);
