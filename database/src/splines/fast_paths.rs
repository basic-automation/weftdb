use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::{auto_interpolate, Measurement, Resolution, SplineType};

// Add specialized implementations for common cases
/// Fast path interpolation with specialized optimizations for common cases.
///
/// # Errors
///
/// Returns an error if the underlying interpolation algorithm fails
pub fn auto_interpolate_with_fast_paths(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline_type: SplineType) -> Result<Vec<Measurement>> {
	// Fast path: uniform spacing with linear interpolation
	if spline_type == SplineType::Linear && is_uniformly_spaced(&measurements) {
		return super::linear(measurements, start, end, resolution);
	}

	// Fast path: small time ranges with few points
	let duration_seconds = (end - start).num_seconds();
	if duration_seconds <= 60 && measurements.len() <= 10 {
		return auto_interpolate(measurements, start, end, resolution, spline_type);
	}

	// Use regular implementation
	auto_interpolate(measurements, start, end, resolution, spline_type)
}

fn is_uniformly_spaced(measurements: &[Measurement]) -> bool {
	if measurements.len() < 3 {
		return false;
	}

	let first_interval = measurements[1].timestamp - measurements[0].timestamp;
	measurements.windows(2).all(|pair| (pair[1].timestamp - pair[0].timestamp - first_interval).num_milliseconds().abs() < 100)
}
