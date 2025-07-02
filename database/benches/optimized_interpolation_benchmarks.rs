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

fn benchmark_optimization_strategies(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	let optimization_configs = vec![
		("cpu_standard", 200, 10, Resolution::Minutes),        // CPU standard path
		("simd_optimized", 500, 15, Resolution::Seconds),      // SIMD optimization
		("parallel_optimized", 1000, 20, Resolution::Seconds), // Parallel optimization
		("gpu_optimized", 1500, 60, Resolution::Seconds),      // GPU optimization
	];

	for (name, measurement_count, window_minutes, resolution) in optimization_configs {
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

		c.bench_function(name, |b| b.iter(|| rt.block_on(async { black_box(auto_interpolate(black_box(measurements.clone()), black_box(start), black_box(end), black_box(resolution), black_box(SplineType::Linear)).await.unwrap()) })));
	}
}

fn benchmark_memory_efficiency(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// Test memory efficiency with different dataset sizes
	let memory_configs = vec![
		("small_memory", 100, 5),
		("medium_memory", 1000, 30),
		("large_memory", 2000, 60), // Reduced from 5000 to 2000
	];

	for (name, measurement_count, window_minutes) in memory_configs {
		let measurements = create_test_measurements(measurement_count, 1);

		// FIXED: Same logic to ensure proper time range
		let data_start = measurements[0].timestamp;
		let data_end = measurements[measurements.len() - 1].timestamp;
		let data_span_minutes = (data_end - data_start).num_minutes();
		let actual_window_minutes = window_minutes.max(data_span_minutes + 10);

		let start = data_start;
		let end = start + chrono::Duration::minutes(actual_window_minutes);

		// Validate the time range
		assert!(end > start, "End time must be after start time for memory benchmark {}", name);

		c.bench_function(name, |b| {
			b.iter(|| {
				rt.block_on(async {
					black_box(
						auto_interpolate(
							black_box(measurements.clone()),
							black_box(start),
							black_box(end),
							black_box(Resolution::Minutes), // Use minutes to control output size
							black_box(SplineType::Linear),
						)
						.await
						.unwrap(),
					)
				})
			})
		});
	}
}

criterion_group!(benches, benchmark_optimization_strategies, benchmark_memory_efficiency);
criterion_main!(benches);
