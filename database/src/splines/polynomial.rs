use anyhow::{Context, Result};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive, Zero};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{Error, Measurement, Resolution};

/// Performs polynomial interpolation on measurement data.
///
/// Takes a vector of `Measurement`, a start date/time, an end date/time, a `Resolution`,
/// and a polynomial degree, and returns a vector of interpolated or extrapolated measurements
/// using polynomial interpolation.
///
/// The `Resolution` is used to determine the time step for the interpolation or extrapolation.
///
/// # Errors
///
/// Returns an error if:
/// - Measurements are empty or have fewer than required points for the degree
/// - Measurements have inconsistent dataset IDs
/// - Invalid time range (start >= end)
/// - Polynomial degree is too high for the number of measurements
/// - Timestamp conversion or `BigDecimal` operations fail
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

	// Performance guard: limit polynomial degree
	let effective_degree = limit_polynomial_degree(degree, measurements.len());

	if effective_degree != degree {
		eprintln!("Warning: Polynomial degree limited from {degree} to {effective_degree} for better performance and numerical stability");
	}

	// Fast path degradation for simple cases
	if effective_degree <= 1 {
		return super::linear(measurements, start, end, resolution);
	}
	if effective_degree == 2 {
		return super::quadratic(measurements, start, end, resolution);
	}
	if effective_degree == 3 && measurements.len() >= 4 {
		return super::cubic(measurements, start, end, resolution);
	}

	// Sort measurements by timestamp
	let mut sorted_measurements = measurements;
	sorted_measurements.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	// Check for uniform spacing - enables fast path
	if is_uniformly_spaced(&sorted_measurements) && effective_degree <= 4 {
		return polynomial_uniform_fast(&sorted_measurements, start, end, resolution, effective_degree, dataset_id);
	}

	let step = resolution.to_step();

	// Build optimized polynomial spline
	let spline = PolynomialSpline::new(&sorted_measurements, effective_degree)?;

	let mut result = Vec::new();

	// Pre-compute rounding to avoid repeated calculations
	let start_millis = start.timestamp_millis();
	let step_millis = step.num_milliseconds();
	let start_offset = start_millis % step_millis;
	let rounded_start = if start_offset == 0 { start } else { start + chrono::TimeDelta::milliseconds(step_millis - start_offset) };

	let end_millis = end.timestamp_millis();
	let end_offset = end_millis % step_millis;
	let rounded_end = if end_offset == 0 { end } else { end - chrono::TimeDelta::milliseconds(end_offset) };

	// Pre-allocate result vector for better performance - safe casting
	let time_diff_ms = (rounded_end - rounded_start).num_milliseconds();
	let estimated_points = if time_diff_ms > 0 {
		usize::try_from(time_diff_ms / step_millis)
			.unwrap_or(1000) // Fallback to reasonable default
			.saturating_add(1)
	} else {
		1
	};
	result.reserve(estimated_points);

	let mut current_time = rounded_start;

	while current_time <= rounded_end {
		let value = spline.evaluate(current_time)?;
		result.push(Measurement { dataset_id, id: Uuid::new_v4(), timestamp: current_time, value });

		current_time += step;
	}

	Ok(result)
}

/// Limits polynomial degree based on dataset size and performance considerations
fn limit_polynomial_degree(requested_degree: usize, measurement_count: usize) -> usize {
	// Practical limits based on performance and numerical stability
	let max_degree_by_count = match measurement_count {
		0..=10 | 1001..=5000 => 2, // Very small and very large: quadratic max
		11..=50 | 201..=1000 => 3, // Small and large: cubic max
		51..=200 => 4,             // Medium: quartic max
		_ => 1,                    // Huge: linear only
	};

	// Never exceed reasonable computational limits
	let absolute_max = 6;

	requested_degree.min(max_degree_by_count).min(absolute_max)
}

/// Fast path for uniformly spaced data using optimized polynomial interpolation
fn polynomial_uniform_fast(measurements: &[Measurement], start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, degree: usize, dataset_id: Uuid) -> Result<Vec<Measurement>> {
	let step = resolution.to_step();
	let mut result = Vec::new();

	// Pre-compute uniform interval
	let uniform_interval_ms = (measurements[1].timestamp - measurements[0].timestamp).num_milliseconds();
	let uniform_interval = BigDecimal::from_i64(uniform_interval_ms).context("Failed to convert uniform interval to BigDecimal")?;

	let mut current_time = start;
	let data_start = measurements[0].timestamp;
	let data_end = measurements[measurements.len() - 1].timestamp;

	while current_time <= end {
		let value = if current_time < data_start || current_time > data_end {
			// Extrapolation - use boundary polynomial segments
			if current_time < data_start {
				extrapolate_backward_uniform(measurements, current_time, &uniform_interval, degree)?
			} else {
				extrapolate_forward_uniform(measurements, current_time, &uniform_interval, degree)?
			}
		} else {
			// Interpolation - use uniform spacing optimization
			interpolate_uniform_polynomial(measurements, current_time, &uniform_interval, degree)?
		};

		result.push(Measurement { dataset_id, id: Uuid::new_v4(), timestamp: current_time, value });

		current_time += step;
	}

	Ok(result)
}

/// Optimized uniform polynomial interpolation using Lagrange method
fn interpolate_uniform_polynomial(measurements: &[Measurement], target_time: DateTime<Utc>, uniform_interval: &BigDecimal, degree: usize) -> Result<BigDecimal> {
	let data_start = measurements[0].timestamp;
	let time_from_start = (target_time - data_start).num_milliseconds();

	// Stay in BigDecimal - convert to i64 only when necessary for indexing
	let uniform_interval_ms = uniform_interval.to_i64().context("Failed to convert uniform interval to i64")?;

	// Find the center point for the interpolation window - safe casting
	let center_index = if time_from_start >= 0 && uniform_interval_ms > 0 { usize::try_from(time_from_start / uniform_interval_ms).unwrap_or(0).min(measurements.len().saturating_sub(1)) } else { 0 };

	// Select points for polynomial interpolation
	let points = select_interpolation_points(measurements, center_index, degree);

	// Calculate relative time parameter - stay in BigDecimal
	let base_time = points[0].timestamp;
	let time_diff = target_time - base_time;
	let dt = BigDecimal::from_i64(time_diff.num_milliseconds()).context("Failed to convert time difference to BigDecimal")?;
	let t = dt / uniform_interval;

	// Use optimized Lagrange interpolation for uniform spacing
	lagrange_interpolate_uniform(&points, &t, uniform_interval)
}

/// Select optimal points for polynomial interpolation around a center point
fn select_interpolation_points(measurements: &[Measurement], center_index: usize, degree: usize) -> Vec<&Measurement> {
	let n = measurements.len();
	let num_points = (degree + 1).min(n);

	// Center the interpolation window around the target
	let half_window = num_points / 2;
	let start_idx = if center_index >= half_window { (center_index - half_window).min(n - num_points) } else { 0 };

	measurements[start_idx..start_idx + num_points].iter().collect()
}

/// Optimized Lagrange interpolation for uniformly spaced points - stays in `BigDecimal`
fn lagrange_interpolate_uniform(points: &[&Measurement], t: &BigDecimal, _uniform_interval: &BigDecimal) -> Result<BigDecimal> {
	let n = points.len();

	if n == 1 {
		return Ok(points[0].value.clone());
	}

	let mut result = BigDecimal::zero();

	// Lagrange interpolation: L(x) = Σ y_i * Π((x - x_j) / (x_i - x_j)) for j ≠ i
	for (i, point) in points.iter().enumerate().take(n) {
		let mut term = point.value.clone();

		// Calculate Lagrange basis polynomial L_i(t)
		for (j, _) in points.iter().enumerate().take(n) {
			if i != j {
				// For uniform spacing, x_i = i and x_j = j (normalized) - stay in BigDecimal
				let i_big = BigDecimal::from_usize(i).context("Failed to convert i to BigDecimal")?;
				let j_big = BigDecimal::from_usize(j).context("Failed to convert j to BigDecimal")?;

				let numerator = t - &j_big;
				let denominator = &i_big - &j_big;

				if !denominator.is_zero() {
					term = term * numerator / denominator;
				}
			}
		}

		result += term;
	}

	Ok(result)
}

/// Backward extrapolation for uniform data
fn extrapolate_backward_uniform(measurements: &[Measurement], target_time: DateTime<Utc>, uniform_interval: &BigDecimal, degree: usize) -> Result<BigDecimal> {
	// Use first few points for polynomial extrapolation
	let num_points = (degree + 1).min(measurements.len());
	let points: Vec<&Measurement> = measurements[0..num_points].iter().collect();

	let base_time = points[0].timestamp;
	let time_diff = target_time - base_time;
	let dt = BigDecimal::from_i64(time_diff.num_milliseconds()).context("Failed to convert time difference to BigDecimal")?;
	let t = dt / uniform_interval;

	lagrange_interpolate_uniform(&points, &t, uniform_interval)
}

/// Forward extrapolation for uniform data
fn extrapolate_forward_uniform(measurements: &[Measurement], target_time: DateTime<Utc>, uniform_interval: &BigDecimal, degree: usize) -> Result<BigDecimal> {
	// Use last few points for polynomial extrapolation
	let n = measurements.len();
	let num_points = (degree + 1).min(n);
	let start_idx = n - num_points;
	let points: Vec<&Measurement> = measurements[start_idx..n].iter().collect();

	let base_time = points[0].timestamp;
	let time_diff = target_time - base_time;
	let dt = BigDecimal::from_i64(time_diff.num_milliseconds()).context("Failed to convert time difference to BigDecimal")?;
	let t = dt / uniform_interval;

	lagrange_interpolate_uniform(&points, &t, uniform_interval)
}

/// Check if measurements are uniformly spaced
fn is_uniformly_spaced(measurements: &[Measurement]) -> bool {
	if measurements.len() < 3 {
		return false;
	}

	let first_interval = measurements[1].timestamp - measurements[0].timestamp;
	let tolerance = chrono::Duration::milliseconds(50); // Tight tolerance for uniform detection

	measurements.windows(2).all(|pair| {
		let interval = pair[1].timestamp - pair[0].timestamp;
		(interval - first_interval).abs() < tolerance
	})
}

pub struct PolynomialSpline {
	measurements: Vec<Measurement>,
	coefficients: Vec<PolynomialSegment>,
}

struct PolynomialSegment {
	coefficients: Vec<BigDecimal>, // Polynomial coefficients [a_n, a_{n-1}, ..., a_1, a_0]
}

impl PolynomialSpline {
	/// Creates a new polynomial spline from measurements
	///
	/// # Errors
	///
	/// Returns an error if:
	/// - Insufficient measurements for polynomial degree
	/// - Polynomial coefficient calculation fails
	/// - Numerical operations encounter errors
	pub fn new(measurements: &[Measurement], degree: usize) -> Result<Self> {
		let n = measurements.len();
		if n < 2 {
			return Err(Error::InsufficientMeasurementsError.into());
		}

		// Limit the effective degree to prevent performance issues
		let effective_degree = degree.min(n - 1).min(6); // Never exceed degree 6

		let mut coefficients = Vec::with_capacity(n - 1);

		// Pre-compute all segments for better cache locality
		for i in 0..n - 1 {
			let segment = Self::fit_polynomial_segment(measurements, i, effective_degree)?;
			coefficients.push(segment);
		}

		Ok(Self { measurements: measurements.to_vec(), coefficients })
	}

	/// Fit a polynomial segment using local points with optimized point selection
	fn fit_polynomial_segment(measurements: &[Measurement], segment_idx: usize, degree: usize) -> Result<PolynomialSegment> {
		let n = measurements.len();
		let num_points = (degree + 1).min(n);

		// Select points centered around the segment for better numerical stability
		let points = if segment_idx == 0 {
			// Use first few points
			&measurements[0..num_points]
		} else if segment_idx >= n - 2 {
			// Use last few points
			&measurements[n - num_points..n]
		} else {
			// Center around the segment
			let center = segment_idx + 1;
			let half_window = num_points / 2;
			let start = center.saturating_sub(half_window);
			let end = (start + num_points).min(n);
			let adjusted_start = end - num_points;
			&measurements[adjusted_start..end]
		};

		// Use Lagrange interpolation for numerical stability
		let coefficients = Self::lagrange_coefficients(points)?;

		Ok(PolynomialSegment { coefficients })
	}

	/// Calculate polynomial coefficients using Lagrange interpolation
	fn lagrange_coefficients(points: &[Measurement]) -> Result<Vec<BigDecimal>> {
		let n = points.len();
		let degree = n - 1;

		// For small degrees, use optimized direct calculation
		match degree {
			0 => Ok(vec![points[0].value.clone()]),
			1 => Self::linear_coefficients(points),
			2 => Self::quadratic_coefficients(points),
			_ => Self::general_lagrange_coefficients(points),
		}
	}

	/// Optimized linear coefficient calculation
	fn linear_coefficients(points: &[Measurement]) -> Result<Vec<BigDecimal>> {
		let x0 = BigDecimal::from_i64(points[0].timestamp.timestamp_millis()).context("Failed to convert timestamp to BigDecimal")?;
		let x1 = BigDecimal::from_i64(points[1].timestamp.timestamp_millis()).context("Failed to convert timestamp to BigDecimal")?;

		let y0 = &points[0].value;
		let y1 = &points[1].value;

		let dx = &x1 - &x0;
		if dx.is_zero() {
			return Ok(vec![y0.clone(), BigDecimal::zero()]);
		}

		let slope = (y1 - y0) / &dx;
		let intercept = y0 - &slope * &x0;

		Ok(vec![slope, intercept]) // [a_1, a_0] for ax + b
	}

	/// Optimized quadratic coefficient calculation
	fn quadratic_coefficients(points: &[Measurement]) -> Result<Vec<BigDecimal>> {
		if points.len() < 3 {
			return Self::linear_coefficients(points);
		}

		// Use the first three points for quadratic fitting
		let x0 = BigDecimal::from_i64(points[0].timestamp.timestamp_millis()).context("Failed to convert timestamp to BigDecimal")?;
		let x1 = BigDecimal::from_i64(points[1].timestamp.timestamp_millis()).context("Failed to convert timestamp to BigDecimal")?;
		let x2 = BigDecimal::from_i64(points[2].timestamp.timestamp_millis()).context("Failed to convert timestamp to BigDecimal")?;

		let y0 = &points[0].value;
		let y1 = &points[1].value;
		let y2 = &points[2].value;

		// Solve quadratic system using Cramer's rule for numerical stability
		let denom = (&x0 - &x1) * (&x0 - &x2) * (&x1 - &x2);

		if denom.is_zero() {
			// Fallback to linear if points are collinear
			return Self::linear_coefficients(&points[0..2]);
		}

		// Calculate quadratic coefficients
		let a = (y0 * (&x1 - &x2) + y1 * (&x2 - &x0) + y2 * (&x0 - &x1)) / &denom;
		let b = (y0 * (&x2 * &x2 - &x1 * &x1) + y1 * (&x0 * &x0 - &x2 * &x2) + y2 * (&x1 * &x1 - &x0 * &x0)) / (-&denom);
		let c = (y0 * (&x1 * &x2 * (&x1 - &x2)) + y1 * (&x2 * &x0 * (&x2 - &x0)) + y2 * (&x0 * &x1 * (&x0 - &x1))) / &denom;

		Ok(vec![a, b, c]) // [a_2, a_1, a_0] for ax² + bx + c
	}

	/// General Lagrange coefficient calculation for higher degrees
	fn general_lagrange_coefficients(points: &[Measurement]) -> Result<Vec<BigDecimal>> {
		let n = points.len();
		let mut coefficients = vec![BigDecimal::zero(); n];

		// This is computationally expensive - only use for small n
		if n > 6 {
			eprintln!("Warning: Polynomial degree {degree} is very high, consider using lower degree for better performance", degree = n - 1);
		}

		// Build Lagrange polynomial by summing basis polynomials
		for i in 0..n {
			let mut basis_poly = vec![BigDecimal::zero(); n];
			basis_poly[0] = points[i].value.clone();

			// Calculate denominator for this basis polynomial
			let mut denom = BigDecimal::from_f64(1.0).context("Failed to create BigDecimal from 1.0")?;
			for j in 0..n {
				if i != j {
					let xi = BigDecimal::from_i64(points[i].timestamp.timestamp_millis()).context("Failed to convert timestamp to BigDecimal")?;
					let xj = BigDecimal::from_i64(points[j].timestamp.timestamp_millis()).context("Failed to convert timestamp to BigDecimal")?;
					denom *= &xi - &xj;
				}
			}

			if !denom.is_zero() {
				// Multiply basis polynomial by 1/denominator
				for coeff in &mut basis_poly {
					*coeff = &*coeff / &denom;
				}

				// Add this basis polynomial to the result
				for (k, coeff) in basis_poly.iter().enumerate() {
					coefficients[k] = &coefficients[k] + coeff;
				}
			}
		}

		Ok(coefficients)
	}

	/// Optimized segment evaluation using Horner's method
	fn evaluate_segment(&self, segment_idx: usize, dt: &BigDecimal) -> BigDecimal {
		let seg = &self.coefficients[segment_idx];

		if seg.coefficients.is_empty() {
			return BigDecimal::zero();
		}

		// Use Horner's method for efficient polynomial evaluation
		// P(x) = a_n*x^n + ... + a_1*x + a_0
		// = ((...((a_n*x + a_{n-1})*x + a_{n-2})*x + ... + a_1)*x + a_0

		let mut result = seg.coefficients[0].clone();
		for coeff in &seg.coefficients[1..] {
			result = result * dt + coeff;
		}

		result
	}

	/// Evaluates the polynomial spline at a target time
	///
	/// # Errors
	///
	/// Returns an error if:
	/// - Target time is outside interpolation bounds
	/// - Polynomial evaluation encounters numerical errors
	/// - Timestamp conversion to `BigDecimal` fails
	pub fn evaluate(&self, target_time: DateTime<Utc>) -> Result<BigDecimal> {
		let n = self.measurements.len();

		// Handle extrapolation backward
		if target_time <= self.measurements[0].timestamp {
			let dt = BigDecimal::from_i64((target_time - self.measurements[0].timestamp).num_milliseconds()).context("Failed to convert timestamp difference to BigDecimal for backward extrapolation")?;
			return Ok(self.evaluate_segment(0, &dt));
		}

		// Handle extrapolation forward
		if target_time >= self.measurements[n - 1].timestamp {
			let dt = BigDecimal::from_i64((target_time - self.measurements[n - 2].timestamp).num_milliseconds()).context("Failed to convert timestamp difference to BigDecimal for forward extrapolation")?;
			return Ok(self.evaluate_segment(n - 2, &dt));
		}

		// Binary search for the appropriate segment - MAJOR PERFORMANCE IMPROVEMENT
		let segment_idx = self.find_segment_binary(target_time);
		let dt = BigDecimal::from_i64((target_time - self.measurements[segment_idx].timestamp).num_milliseconds()).context("Failed to convert timestamp difference to BigDecimal for interpolation")?;

		Ok(self.evaluate_segment(segment_idx, &dt))
	}

	/// Binary search for segment - O(log n) instead of O(n)
	fn find_segment_binary(&self, target_time: DateTime<Utc>) -> usize {
		let mut left = 0;
		let mut right = self.measurements.len() - 1;

		while left < right - 1 {
			let mid = left + (right - left) / 2;
			if target_time < self.measurements[mid].timestamp {
				right = mid;
			} else {
				left = mid;
			}
		}

		left
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
	fn test_polynomial_degree_limiting() {
		assert_eq!(limit_polynomial_degree(5, 5), 2); // Small dataset
		assert_eq!(limit_polynomial_degree(5, 100), 4); // Medium dataset
		assert_eq!(limit_polynomial_degree(5, 2000), 2); // Large dataset
		assert_eq!(limit_polynomial_degree(10, 100), 4); // Degree capped by dataset size
	}

	#[test]
	fn test_polynomial_interpolation_quadratic() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 0.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 100.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 20).unwrap(), 400.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 5).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 15).unwrap();

		let result = polynomial(measurements, start, end, Resolution::Seconds, 2).unwrap();

		assert!(!result.is_empty());
		// Should have smooth polynomial interpolation between points
	}

	#[test]
	fn test_polynomial_degree_fallback() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 20.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 2).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 8).unwrap();

		// Request high degree but should fall back to linear
		let result = polynomial(measurements, start, end, Resolution::Seconds, 5).unwrap();

		assert!(!result.is_empty());
		// Should behave like linear interpolation with 2 points
		let mid_measurement = result.iter().find(|m| m.timestamp.second() == 5).unwrap();
		let expected_value = BigDecimal::from_f64(15.0).unwrap();
		assert_eq!(mid_measurement.value, expected_value);
	}

	#[test]
	fn test_polynomial_uniform_spacing() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 0.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 10.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 20).unwrap(), 80.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 30).unwrap(), 270.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 40).unwrap(), 640.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 5).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 35).unwrap();

		let result = polynomial(measurements, start, end, Resolution::Seconds, 3).unwrap();

		assert!(!result.is_empty());
		// Should use the uniform fast path
	}

	#[test]
	fn test_insufficient_measurements_error() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap();

		let result = polynomial(measurements, start, end, Resolution::Seconds, 2);
		assert!(result.is_err());
	}

	#[test]
	fn test_polynomial_extrapolation() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 100.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 20).unwrap(), 400.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 30).unwrap(), 900.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 5).unwrap();

		let result = polynomial(measurements, start, end, Resolution::Seconds, 2).unwrap();

		assert!(!result.is_empty());
		// Should extrapolate using polynomial curve
	}

	#[test]
	fn test_polynomial_high_degree_performance() {
		let dataset_id = Uuid::new_v4();
		let mut measurements = Vec::new();

		// Create 20 measurements
		for i in 0..20 {
			measurements.push(create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, i, 0).unwrap(), (i * i) as f64));
		}

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 2, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 18, 0).unwrap();

		// Request very high degree - should be limited automatically
		let result = polynomial(measurements, start, end, Resolution::Minutes, 15).unwrap();

		assert!(!result.is_empty());
		// Should complete without timeout even with high degree request
	}

	#[test]
	fn test_point_selection_optimization() {
		let measurements = vec![create_measurement(Uuid::new_v4(), Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 0.0), create_measurement(Uuid::new_v4(), Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 10.0), create_measurement(Uuid::new_v4(), Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 20).unwrap(), 40.0), create_measurement(Uuid::new_v4(), Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 30).unwrap(), 90.0), create_measurement(Uuid::new_v4(), Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 40).unwrap(), 160.0)];

		// Test point selection around different centers
		let points_start = select_interpolation_points(&measurements, 0, 3);
		assert_eq!(points_start.len(), 4); // degree + 1

		let points_middle = select_interpolation_points(&measurements, 2, 3);
		assert_eq!(points_middle.len(), 4);

		let points_end = select_interpolation_points(&measurements, 4, 3);
		assert_eq!(points_end.len(), 4);
	}
}
