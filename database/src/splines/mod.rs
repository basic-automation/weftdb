use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::{Error, Measurement};

pub mod cubic;
pub mod fast_paths;
pub mod linear;
pub mod parallel;
pub mod polynomial;
pub mod quadratic;
pub mod simd; // Make SIMD module public

// Re-export spline functions
pub use cubic::cubic;
pub use linear::linear;
// Re-export parallel and optimized functions
pub use parallel::*;
pub use polynomial::polynomial;
pub use quadratic::quadratic;
// Re-export SIMD functions for benchmarking and advanced use cases
pub use simd::{auto_interpolate_simd, cubic_simd_batch, linear_simd_batch, polynomial_simd_batch, quadratic_simd_batch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
	Milliseconds,
	Seconds,
	Minutes,
	Hours,
	Days,
}

impl Resolution {
	/// Get the duration in milliseconds for this resolution
	#[must_use]
	pub const fn to_milliseconds(self) -> i64 {
		match self {
			Self::Milliseconds => 1,
			Self::Seconds => 1000,
			Self::Minutes => 60_000,
			Self::Hours => 3_600_000,
			Self::Days => 86_400_000,
		}
	}

	/// Get the step duration for this resolution
	#[must_use]
	pub const fn to_step(self) -> chrono::Duration {
		match self {
			Self::Milliseconds => chrono::Duration::milliseconds(1),
			Self::Seconds => chrono::Duration::seconds(1),
			Self::Minutes => chrono::Duration::minutes(1),
			Self::Hours => chrono::Duration::hours(1),
			Self::Days => chrono::Duration::days(1),
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplineType {
	Linear,
	Quadratic,
	Cubic,
	Polynomial(usize),
}

/// Automatically choose the best interpolation method and perform interpolation
///
/// # Errors
///
/// Returns an error if:
/// - There are fewer than 2 measurements
/// - Measurements have different dataset IDs
/// - The time range is invalid (start >= end)
/// - For polynomial interpolation, insufficient measurements for the specified degree
pub fn auto_interpolate(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline_type: SplineType) -> Result<Vec<Measurement>> {
	if measurements.len() < 2 {
		return Err(Error::InsufficientMeasurementsError.into());
	}

	// Validate all measurements have the same dataset_id
	let dataset_id = measurements[0].dataset_id;
	if !measurements.iter().all(|m| m.dataset_id == dataset_id) {
		return Err(Error::DifferentDatasetIdsError.into());
	}

	// Validate time range
	if start >= end {
		return Err(Error::InvalidTimeRangeError.into());
	}

	match spline_type {
		SplineType::Linear => linear(measurements, start, end, resolution),
		SplineType::Quadratic => quadratic(measurements, start, end, resolution),
		SplineType::Cubic => cubic(measurements, start, end, resolution),
		SplineType::Polynomial(degree) => polynomial(measurements, start, end, resolution, degree),
	}
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use bigdecimal::BigDecimal;
	use chrono::TimeZone;
	use uuid::Uuid;

	use super::*;

	fn create_test_measurements(count: usize) -> Vec<Measurement> {
		let dataset_id = Uuid::new_v4();
		let start_time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

		(0..count).map(|i| Measurement { id: Uuid::new_v4(), dataset_id, timestamp: start_time + chrono::Duration::seconds(i as i64 * 10), value: BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap() }).collect()
	}

	#[tokio::test]
	async fn test_auto_interpolate_linear() {
		let measurements = create_test_measurements(5);
		let start = measurements[0].timestamp;
		let end = measurements[measurements.len() - 1].timestamp;

		let result = auto_interpolate(measurements, start, end, Resolution::Seconds, SplineType::Linear);
		assert!(result.is_ok());

		let interpolated = result.unwrap();
		assert!(!interpolated.is_empty());
	}

	#[tokio::test]
	async fn test_auto_interpolate_quadratic() {
		let measurements = create_test_measurements(5);
		let start = measurements[0].timestamp;
		let end = measurements[measurements.len() - 1].timestamp;

		let result = auto_interpolate(measurements, start, end, Resolution::Seconds, SplineType::Quadratic);
		assert!(result.is_ok());

		let interpolated = result.unwrap();
		assert!(!interpolated.is_empty());
	}

	#[tokio::test]
	async fn test_auto_interpolate_cubic() {
		let measurements = create_test_measurements(5);
		let start = measurements[0].timestamp;
		let end = measurements[measurements.len() - 1].timestamp;

		let result = auto_interpolate(measurements, start, end, Resolution::Seconds, SplineType::Cubic);
		assert!(result.is_ok());

		let interpolated = result.unwrap();
		assert!(!interpolated.is_empty());
	}

	#[tokio::test]
	async fn test_auto_interpolate_polynomial() {
		let measurements = create_test_measurements(6);
		let start = measurements[0].timestamp;
		let end = measurements[measurements.len() - 1].timestamp;

		let result = auto_interpolate(measurements, start, end, Resolution::Seconds, SplineType::Polynomial(3));
		assert!(result.is_ok());

		let interpolated = result.unwrap();
		assert!(!interpolated.is_empty());
	}

	#[tokio::test]
	async fn test_auto_interpolate_polynomial_various_degrees() {
		let measurements = create_test_measurements(10);
		let start = measurements[0].timestamp;
		let end = measurements[measurements.len() - 1].timestamp;

		for degree in 2..=5 {
			let result = auto_interpolate(measurements.clone(), start, end, Resolution::Seconds, SplineType::Polynomial(degree));
			assert!(result.is_ok(), "Failed for polynomial degree {}", degree);

			let interpolated = result.unwrap();
			assert!(!interpolated.is_empty());
		}
	}

	#[tokio::test]
	async fn test_auto_interpolate_empty_measurements() {
		let measurements = vec![];
		let start = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 1, 0, 0).unwrap();

		let result = auto_interpolate(measurements, start, end, Resolution::Seconds, SplineType::Linear);
		assert!(result.is_err());
	}

	#[tokio::test]
	async fn test_auto_interpolate_single_measurement() {
		let measurements = create_test_measurements(1);
		let start = measurements[0].timestamp;
		let end = measurements[0].timestamp + chrono::Duration::hours(1);

		let result = auto_interpolate(measurements, start, end, Resolution::Seconds, SplineType::Linear);
		assert!(result.is_err());
	}

	#[tokio::test]
	async fn test_auto_interpolate_dataset_consistency() {
		let mut measurements = create_test_measurements(5);
		// Change one measurement's dataset_id to make them inconsistent
		measurements[2].dataset_id = Uuid::new_v4();

		let start = measurements[0].timestamp;
		let end = measurements[measurements.len() - 1].timestamp;

		let result = auto_interpolate(measurements, start, end, Resolution::Seconds, SplineType::Linear);
		assert!(result.is_err());
	}

	#[tokio::test]
	async fn test_auto_interpolate_time_bounds() {
		let measurements = create_test_measurements(5);
		let start = measurements[measurements.len() - 1].timestamp;
		let end = measurements[0].timestamp; // Invalid: start >= end

		let result = auto_interpolate(measurements, start, end, Resolution::Seconds, SplineType::Linear);
		assert!(result.is_err());
	}

	#[tokio::test]
	async fn test_auto_interpolate_different_resolutions() {
		let measurements = create_test_measurements(5);
		let start = measurements[0].timestamp;
		let end = measurements[measurements.len() - 1].timestamp;

		for resolution in [Resolution::Milliseconds, Resolution::Seconds, Resolution::Minutes, Resolution::Hours] {
			let result = auto_interpolate(measurements.clone(), start, end, resolution, SplineType::Linear);
			assert!(result.is_ok(), "Failed for resolution: {:?}", resolution);

			let interpolated = result.unwrap();
			assert!(!interpolated.is_empty());
		}
	}

	#[tokio::test]
	async fn test_auto_interpolate_spline_type_dispatch() {
		let measurements = create_test_measurements(8);
		let start = measurements[0].timestamp;
		let end = measurements[measurements.len() - 1].timestamp;

		let spline_types = vec![SplineType::Linear, SplineType::Quadratic, SplineType::Cubic, SplineType::Polynomial(2), SplineType::Polynomial(3), SplineType::Polynomial(4)];

		for spline_type in spline_types {
			let result = auto_interpolate(measurements.clone(), start, end, Resolution::Seconds, spline_type);
			assert!(result.is_ok(), "Failed for spline type: {:?}", spline_type);

			let interpolated = result.unwrap();
			assert!(!interpolated.is_empty());

			// Verify all results have the same dataset_id
			let expected_dataset_id = measurements[0].dataset_id;
			assert!(interpolated.iter().all(|m| m.dataset_id == expected_dataset_id));
		}
	}
}
