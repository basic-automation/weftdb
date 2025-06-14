use anyhow::{Context, Result};
use bigdecimal::{BigDecimal, FromPrimitive, Zero};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{Error, Measurement, Resolution};

/// polynomial takes a vector of `Measurement`, a start date/time, an end date/time, a `Resolution`, and a degree, and returns a vector of interpolated or extrapolated (or both) `Measurement` using polynomial spline interpolation.
/// The `Resolution` is used to determine the time step for the interpolation or extrapolation.
/// The degree determines the polynomial order (1=linear, 2=quadratic, 3=cubic, etc.)
pub fn polynomial(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, degree: usize) -> Result<Vec<Measurement>> {
	if measurements.is_empty() {
		return Ok(vec![]);
	}
	let dataset_id = measurements[0].dataset_id;
	if measurements.iter().any(|m| m.dataset_id != dataset_id) {
		return Err(Error::InconsistentDatasetIdsError.into());
	}
	if start >= end {
		return Err(Error::InvalidTimeRangeError.into());
	}
	if measurements.len() < 2 {
		return Err(Error::InsufficientMeasurementsError.into());
	}
	if degree == 0 {
		return Err(anyhow::anyhow!("Polynomial degree must be at least 1"));
	}

	// Sort measurements by timestamp
	let mut sorted_measurements = measurements;
	sorted_measurements.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	let step = resolution.to_step();

	// Build polynomial spline coefficients
	let spline = PolynomialSpline::new(&sorted_measurements, degree)?;

	let mut result = Vec::new();

	// round start and end to the nearest step, but ensure we don't go before the requested start time
	let start_millis = start.timestamp_millis();
	let step_millis = step.num_milliseconds();
	let start_offset = start_millis % step_millis;
	let rounded_start = if start_offset == 0 { start } else { start + chrono::TimeDelta::milliseconds(step_millis - start_offset) };

	let end_millis = end.timestamp_millis();
	let end_offset = end_millis % step_millis;
	let rounded_end = if end_offset == 0 { end } else { end - chrono::TimeDelta::milliseconds(end_offset) };

	let mut current_time = rounded_start;

	while current_time <= rounded_end {
		let value = spline.evaluate(current_time)?;
		result.push(Measurement { dataset_id, id: Uuid::new_v4(), timestamp: current_time, value });

		current_time += step;
	}

	Ok(result)
}

struct PolynomialSpline {
	measurements: Vec<Measurement>,
	coefficients: Vec<PolynomialSegment>,
}

struct PolynomialSegment {
	coefficients: Vec<BigDecimal>, // coefficients[0] is constant, coefficients[1] is linear, etc.
}

impl PolynomialSpline {
	fn new(measurements: &[Measurement], degree: usize) -> Result<Self> {
		let n = measurements.len();
		if n < 2 {
			return Err(Error::InsufficientMeasurementsError.into());
		}
		if degree == 0 {
			return Err(anyhow::anyhow!("Polynomial degree must be at least 1"));
		}

		let mut coefficients = Vec::with_capacity(n - 1);

		if n == 2 || degree == 1 {
			// Linear interpolation for 2 points or degree 1
			for i in 0..n - 1 {
				let segment = Self::fit_linear(&measurements[i], &measurements[i + 1])?;
				coefficients.push(segment);
			}
		} else {
			// Higher order polynomial interpolation
			for i in 0..n - 1 {
				let end_idx = std::cmp::min(i + degree + 1, n);
				let points_to_use = &measurements[i..end_idx];

				if points_to_use.len() > degree {
					let segment = Self::fit_polynomial(points_to_use, degree)?;
					coefficients.push(segment);
				} else {
					// Fall back to linear if not enough points
					let segment = Self::fit_linear(&measurements[i], &measurements[i + 1])?;
					coefficients.push(segment);
				}
			}
		}

		Ok(Self { measurements: measurements.to_vec(), coefficients })
	}

	fn fit_linear(p1: &Measurement, p2: &Measurement) -> Result<PolynomialSegment> {
		let dt = BigDecimal::from_i64((p2.timestamp - p1.timestamp).num_milliseconds()).context("Failed to convert timestamp difference to BigDecimal")?;
		let dy = p2.value.clone() - p1.value.clone();
		let slope = dy / dt;

		Ok(PolynomialSegment { coefficients: vec![p1.value.clone(), slope] })
	}

	fn fit_polynomial(points: &[Measurement], degree: usize) -> Result<PolynomialSegment> {
		let n = points.len();
		if n < degree + 1 {
			return Err(anyhow::anyhow!("Not enough points for polynomial of degree {}", degree));
		}

		// Convert timestamps to relative time from first point
		let base_time = points[0].timestamp;
		let mut times = Vec::with_capacity(n);
		let mut values = Vec::with_capacity(n);

		for point in points {
			let t = BigDecimal::from_i64((point.timestamp - base_time).num_milliseconds()).context("Failed to convert timestamp to BigDecimal")?;
			times.push(t);
			values.push(point.value.clone());
		}

		// Use Vandermonde matrix to solve for polynomial coefficients
		// For polynomial of degree d: y = a₀ + a₁t + a₂t² + ... + aₐtᵈ
		let matrix_size = std::cmp::min(degree + 1, n);
		let mut matrix = vec![vec![BigDecimal::zero(); matrix_size]; matrix_size];
		let mut rhs = vec![BigDecimal::zero(); matrix_size];

		// Build Vandermonde matrix
		for i in 0..matrix_size {
			rhs[i] = values[i].clone();
			for j in 0..matrix_size {
				if j == 0 {
					matrix[i][j] = BigDecimal::from_f64(1.0).context("Failed to create BigDecimal from 1.0")?;
				} else {
					matrix[i][j] = Self::power(&times[i], j)?;
				}
			}
		}

		// Solve using Gaussian elimination
		let coefficients = Self::solve_linear_system(matrix, rhs)?;

		Ok(PolynomialSegment { coefficients })
	}

	fn power(base: &BigDecimal, exp: usize) -> Result<BigDecimal> {
		if exp == 0 {
			return BigDecimal::from_f64(1.0).context("Failed to create BigDecimal from 1.0");
		}

		let mut result = base.clone();
		for _ in 1..exp {
			result *= base.clone();
		}
		Ok(result)
	}

	fn solve_linear_system(mut matrix: Vec<Vec<BigDecimal>>, mut rhs: Vec<BigDecimal>) -> Result<Vec<BigDecimal>> {
		let n = matrix.len();

		// Forward elimination
		for i in 0..n {
			// Find pivot
			let mut max_row = i;
			for k in (i + 1)..n {
				if matrix[k][i].abs() > matrix[max_row][i].abs() {
					max_row = k;
				}
			}

			// Swap rows
			matrix.swap(i, max_row);
			rhs.swap(i, max_row);

			// Check for singular matrix
			if matrix[i][i].is_zero() {
				return Err(anyhow::anyhow!("Singular matrix encountered in polynomial fitting"));
			}

			// Make all rows below this one 0 in current column
			for k in (i + 1)..n {
				let factor = matrix[k][i].clone() / matrix[i][i].clone();
				for j in i..n {
					matrix[k][j] = matrix[k][j].clone() - factor.clone() * matrix[i][j].clone();
				}
				rhs[k] = rhs[k].clone() - factor * rhs[i].clone();
			}
		}

		// Back substitution
		let mut solution = vec![BigDecimal::zero(); n];
		for i in (0..n).rev() {
			solution[i] = rhs[i].clone();
			for j in (i + 1)..n {
				solution[i] = solution[i].clone() - matrix[i][j].clone() * solution[j].clone();
			}
			solution[i] = solution[i].clone() / matrix[i][i].clone();
		}

		Ok(solution)
	}

	fn evaluate_segment(&self, segment_idx: usize, dt: &BigDecimal) -> BigDecimal {
		let seg = &self.coefficients[segment_idx];
		let mut result = BigDecimal::zero();

		for (i, coeff) in seg.coefficients.iter().enumerate() {
			if i == 0 {
				result += coeff.clone();
			} else {
				let dt_power = Self::power(dt, i).unwrap_or_else(|_| BigDecimal::zero());
				result += coeff.clone() * dt_power;
			}
		}

		result
	}

	fn evaluate(&self, target_time: DateTime<Utc>) -> Result<BigDecimal> {
		let n = self.measurements.len();

		// Handle extrapolation
		if target_time <= self.measurements[0].timestamp {
			// Extrapolate using first segment
			let dt = BigDecimal::from_i64((target_time - self.measurements[0].timestamp).num_milliseconds()).context("Failed to convert timestamp difference to BigDecimal for backward extrapolation")?;
			return Ok(self.evaluate_segment(0, &dt));
		}

		if target_time >= self.measurements[n - 1].timestamp {
			// Extrapolate using last segment
			let dt = BigDecimal::from_i64((target_time - self.measurements[n - 2].timestamp).num_milliseconds()).context("Failed to convert timestamp difference to BigDecimal for forward extrapolation")?;
			return Ok(self.evaluate_segment(n - 2, &dt));
		}

		// Find the appropriate segment for interpolation
		for i in 0..n - 1 {
			if target_time >= self.measurements[i].timestamp && target_time <= self.measurements[i + 1].timestamp {
				let dt = BigDecimal::from_i64((target_time - self.measurements[i].timestamp).num_milliseconds()).context(format!("Failed to convert timestamp difference to BigDecimal for interpolation segment {i}"))?;
				return Ok(self.evaluate_segment(i, &dt));
			}
		}

		// Fallback
		Ok(self.measurements[0].value.clone())
	}
}

#[cfg(test)]
mod tests {
	use bigdecimal::BigDecimal;
	use chrono::{DateTime, TimeZone, Timelike, Utc};
	use uuid::Uuid;

	use super::*;

	fn create_measurement(dataset_id: Uuid, timestamp: DateTime<Utc>, value: f64) -> Measurement {
		Measurement { dataset_id, id: Uuid::new_v4(), timestamp, value: BigDecimal::from_f64(value).expect("Failed to create BigDecimal in test") }
	}

	#[test]
	fn test_polynomial_degree_1() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 20.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 2).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 8).unwrap();

		let result = polynomial(measurements, start, end, Resolution::Seconds, 1).unwrap();

		assert!(!result.is_empty());
		// Should behave like linear interpolation with degree 1
		let mid_measurement = result.iter().find(|m| m.timestamp.second() == 5).unwrap();
		let expected_value = BigDecimal::from_f64(15.0).unwrap();
		assert_eq!(mid_measurement.value, expected_value);
	}

	#[test]
	fn test_polynomial_degree_2() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 0.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 100.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 20).unwrap(), 400.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 5).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 15).unwrap();

		let result = polynomial(measurements, start, end, Resolution::Seconds, 2).unwrap();

		assert!(!result.is_empty());
		// Should have quadratic interpolation between points
	}

	#[test]
	fn test_polynomial_degree_3() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 0.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 10.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 20).unwrap(), 80.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 30).unwrap(), 270.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 5).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 25).unwrap();

		let result = polynomial(measurements, start, end, Resolution::Seconds, 3).unwrap();

		assert!(!result.is_empty());
		// Should have cubic interpolation between points
	}

	#[test]
	fn test_polynomial_extrapolation_backward() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 100.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 20).unwrap(), 200.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 30).unwrap(), 300.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 5).unwrap();

		let result = polynomial(measurements, start, end, Resolution::Seconds, 2).unwrap();

		assert!(!result.is_empty());
		// Should extrapolate smoothly backwards
	}

	#[test]
	fn test_polynomial_extrapolation_forward() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 0.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 100.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 20).unwrap(), 400.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 25).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 35).unwrap();

		let result = polynomial(measurements, start, end, Resolution::Seconds, 2).unwrap();

		assert!(!result.is_empty());
		// Should extrapolate smoothly forwards
	}

	#[test]
	fn test_polynomial_zero_degree_error() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 20.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap();

		let result = polynomial(measurements, start, end, Resolution::Seconds, 0);
		assert!(result.is_err());
	}

	#[test]
	fn test_different_dataset_ids_error() {
		let measurements = vec![create_measurement(Uuid::new_v4(), Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0), create_measurement(Uuid::new_v4(), Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 20.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap();

		let result = polynomial(measurements, start, end, Resolution::Seconds, 1);
		assert!(result.is_err());
		assert!(matches!(result.unwrap_err().downcast_ref::<Error>(), Some(Error::InconsistentDatasetIdsError)));
	}

	#[test]
	fn test_invalid_time_range_error() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 20.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

		let result = polynomial(measurements, start, end, Resolution::Seconds, 1);
		assert!(result.is_err());
		assert!(matches!(result.unwrap_err().downcast_ref::<Error>(), Some(Error::InvalidTimeRangeError)));
	}

	#[test]
	fn test_insufficient_measurements_error() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap();

		let result = polynomial(measurements, start, end, Resolution::Seconds, 1);
		assert!(result.is_err());
		assert!(matches!(result.unwrap_err().downcast_ref::<Error>(), Some(Error::InsufficientMeasurementsError)));
	}

	#[test]
	fn test_empty_measurements() {
		let measurements = vec![];
		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap();

		let result = polynomial(measurements, start, end, Resolution::Seconds, 1).unwrap();
		assert!(result.is_empty());
	}

	#[test]
	fn test_result_dataset_consistency() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 20.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 2).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 8).unwrap();

		let result = polynomial(measurements, start, end, Resolution::Seconds, 1).unwrap();

		// All results should have the same dataset_id
		for measurement in &result {
			assert_eq!(measurement.dataset_id, dataset_id);
		}

		// All results should have unique IDs
		let mut ids: Vec<Uuid> = result.iter().map(|m| m.id).collect();
		ids.sort();
		ids.dedup();
		assert_eq!(ids.len(), result.len());
	}
}
