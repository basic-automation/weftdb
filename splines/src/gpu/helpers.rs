use anyhow::{Result, bail};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use chrono::{DateTime, Utc};

use crate::{Error, Point};

/// Convert points to GPU-compatible f32 format
pub fn convert_points_to_gpu_format(points: &[Point]) -> (Vec<f32>, Vec<f32>) {
	let mut input_times = Vec::with_capacity(points.len());
	let mut input_values = Vec::with_capacity(points.len());

	let base_time = points[0].timestamp;

	for point in points {
		// Convert timestamp to seconds offset from base time
		let time_offset = (point.timestamp - base_time).num_seconds() as f32;
		input_times.push(time_offset);

		// Convert BigDecimal to f32
		let value = point.value.to_f64().unwrap_or(0.0) as f32;
		input_values.push(value);
	}

	(input_times, input_values)
}

/// Convert `DateTime`<Utc> to GPU-compatible f32 format
pub fn convert_datetimes_to_gpu_format(times: &[DateTime<Utc>]) -> Vec<f32> {
	if times.is_empty() {
		return Vec::new();
	}

	let base_time = times[0];
	let mut gpu_times = Vec::with_capacity(times.len());

	for &time in times {
		let time_offset = (time - base_time).num_seconds() as f32;
		gpu_times.push(time_offset);
	}

	gpu_times
}

/// Convert GPU results back to Points
pub fn convert_gpu_results_to_points(gpu_results: Vec<f32>, target_times: Vec<DateTime<Utc>>) -> Result<Vec<Point>> {
	if gpu_results.len() != target_times.len() {
		bail!(Error::InvalidGpuOutputError(format!("GPU results length {} does not match target times length {}", gpu_results.len(), target_times.len())));
	}

	let results = gpu_results
		.into_iter()
		.zip(target_times)
		.map(|(value, timestamp)| {
			let big_decimal_value = BigDecimal::from_f64(f64::from(value)).unwrap_or_else(|| BigDecimal::from(0));
			Point { timestamp, value: big_decimal_value }
		})
		.collect();

	Ok(results)
}
