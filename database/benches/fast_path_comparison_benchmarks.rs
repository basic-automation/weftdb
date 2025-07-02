use std::{hint::black_box, str::FromStr};

use bigdecimal::BigDecimal;
use chrono::TimeZone;
use criterion::{criterion_group, criterion_main, Criterion};
use database::{auto_interpolate, Measurement, Resolution, SplineType};
use tokio::runtime::Runtime;
use uuid::Uuid;

fn create_test_measurements(count: usize, interval_minutes: i64) -> Vec<Measurement> {
	let dataset_id = Uuid::new_v4();
	let start_time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

	(0..count).map(|i| Measurement { id: Uuid::new_v4(), dataset_id, timestamp: start_time + chrono::Duration::minutes(i as i64 * interval_minutes), value: BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap() }).collect()
}

fn benchmark_algorithm_selection(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// Compare different algorithmic approaches
	let measurements = create_test_measurements(800, 1);
	let start = measurements[0].timestamp;
	let end = start + chrono::Duration::minutes(15);

	let algorithms = vec![("standard_linear", SplineType::Linear), ("standard_quadratic", SplineType::Quadratic), ("standard_cubic", SplineType::Cubic)];

	for (name, spline_type) in algorithms {
		c.bench_function(name, |b| b.iter(|| rt.block_on(async { black_box(auto_interpolate(black_box(measurements.clone()), black_box(start), black_box(end), black_box(Resolution::Seconds), black_box(spline_type)).await.unwrap()) })));
	}
}

criterion_group!(benches, benchmark_algorithm_selection);
criterion_main!(benches);

#[cfg(test)]
mod tests {
	#[test]
	fn test_performance_analysis() {
		super::analyze_performance_characteristics();
	}
}
