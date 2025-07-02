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

fn benchmark_production_workloads(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// Simulate production workload patterns - FIXED TIME RANGES
	let workloads = vec![
		("iot_sensor_data", 1000, 1, 60, Resolution::Seconds),    // IoT: 1000 measurements, 1-min intervals, 60-min window
		("financial_ticks", 2000, 1, 120, Resolution::Seconds),   // Financial: 2000 measurements, 1-min intervals, 120-min window (FIXED: increased window)
		("monitoring_metrics", 800, 2, 120, Resolution::Minutes), // Monitoring: 800 measurements, 2-min intervals, 120-min window
	];

	for (name, measurement_count, interval_minutes, window_minutes, resolution) in workloads {
		let measurements = create_test_measurements(measurement_count, interval_minutes);

		// FIXED: Ensure we have a proper time range that extends beyond the measurements
		let data_start = measurements[0].timestamp;
		let data_end = measurements[measurements.len() - 1].timestamp;

		// Calculate the data span
		let data_span_minutes = (data_end - data_start).num_minutes();

		// Use either the requested window or extend beyond the data, whichever is larger
		let actual_window_minutes = window_minutes.max(data_span_minutes + 10);

		let start = data_start;
		let end = start + chrono::Duration::minutes(actual_window_minutes);

		// Validate the time range
		assert!(end > start, "End time must be after start time for benchmark {}", name);

		c.bench_function(name, |b| {
			b.iter(|| {
				rt.block_on(async {
					black_box(
						auto_interpolate(
							black_box(measurements.clone()),
							black_box(start),
							black_box(end),
							black_box(resolution),
							black_box(SplineType::Linear), // Production typically uses linear
						)
						.await
						.unwrap(),
					)
				})
			})
		});
	}
}

fn benchmark_full_integration_pipeline(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// Test the complete integration pipeline with realistic scenarios - FIXED TIME RANGES
	let integration_scenarios = vec![
		("real_time_small", 50, 1, 60, Resolution::Seconds, SplineType::Linear),    // 50 measurements, 1-min intervals, 60-min window (FIXED: increased window)
		("batch_medium", 500, 1, 600, Resolution::Minutes, SplineType::Quadratic),  // 500 measurements, 1-min intervals, 600-min window (FIXED: increased window)
		("analytics_large", 1000, 1, 1200, Resolution::Seconds, SplineType::Cubic), // 1000 measurements, 1-min intervals, 1200-min window (FIXED: increased window)
	];

	let mut group = c.benchmark_group("integration_pipeline");

	for (name, measurement_count, interval_minutes, window_minutes, resolution, spline_type) in integration_scenarios {
		let measurements = create_test_measurements(measurement_count, interval_minutes);

		// FIXED: Same logic to ensure proper time range
		let data_start = measurements[0].timestamp;
		let data_end = measurements[measurements.len() - 1].timestamp;
		let data_span_minutes = (data_end - data_start).num_minutes();
		let actual_window_minutes = window_minutes.max(data_span_minutes + 10);

		let start = data_start;
		let end = start + chrono::Duration::minutes(actual_window_minutes);

		// Validate the time range
		assert!(end > start, "End time must be after start time for integration scenario {}", name);

		group.bench_function(name, |b| b.iter(|| rt.block_on(async { black_box(auto_interpolate(black_box(measurements.clone()), black_box(start), black_box(end), black_box(resolution), black_box(spline_type)).await.unwrap()) })));
	}

	group.finish();
}

criterion_group!(benches, benchmark_production_workloads, benchmark_full_integration_pipeline);
criterion_main!(benches);
