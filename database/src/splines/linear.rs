use anyhow::{Context, Result};
use bigdecimal::{BigDecimal, FromPrimitive, Zero};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{Error, Measurement, Resolution};

/// Performs linear interpolation on measurement data.
///
/// Takes a vector of `Measurement`, a start date/time, an end date/time, a `Resolution`,
/// and returns a vector of interpolated or extrapolated measurements using linear interpolation.
///
/// The `Resolution` is used to determine the time step for the interpolation or extrapolation.
///
/// # Errors
///
/// Returns an error if:
/// - Measurements are empty or have fewer than 2 points
/// - Measurements have inconsistent dataset IDs
/// - Invalid time range (start >= end)
/// - Timestamp conversion or `BigDecimal` operations fail
pub fn linear(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Result<Vec<Measurement>> {
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

	// Fast path for very small datasets
	if measurements.len() == 2 {
		return linear_two_point_fast(&measurements, start, end, resolution, dataset_id);
	}

	// Sort measurements by timestamp
	let mut sorted_measurements = measurements;
	sorted_measurements.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	// Check for uniform spacing - enables fast path
	if is_uniformly_spaced(&sorted_measurements) {
		return linear_uniform_fast(&sorted_measurements, start, end, resolution, dataset_id);
	}

	let step = resolution.to_step();

	// Build optimized linear spline
	let spline = LinearSpline::new(&sorted_measurements)?;

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

/// Fast path for uniformly spaced data
fn linear_uniform_fast(measurements: &[Measurement], start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, dataset_id: Uuid) -> Result<Vec<Measurement>> {
	let step = resolution.to_step();
	let mut result = Vec::new();

	// Pre-compute uniform interval - check for zero interval
	let uniform_interval_ms = (measurements[1].timestamp - measurements[0].timestamp).num_milliseconds();

	// Handle case where measurements have identical or nearly identical timestamps
	if uniform_interval_ms == 0 {
		// All measurements have the same timestamp - use constant value interpolation
		let constant_value = &measurements[0].value;
		let mut current_time = start;

		while current_time <= end {
			result.push(Measurement { dataset_id, id: Uuid::new_v4(), timestamp: current_time, value: constant_value.clone() });
			current_time += step;
		}

		return Ok(result);
	}

	let uniform_interval = BigDecimal::from_i64(uniform_interval_ms).context("Failed to convert uniform interval to BigDecimal")?;

	let mut current_time = start;
	let data_start = measurements[0].timestamp;
	let data_end = measurements[measurements.len() - 1].timestamp;

	while current_time <= end {
		let value = if current_time < data_start || current_time > data_end {
			// Extrapolation - use boundary segments
			if current_time < data_start {
				// Extrapolate backward using first segment
				let dt = BigDecimal::from_i64((current_time - data_start).num_milliseconds()).context("Failed to convert time difference to BigDecimal")?;
				let slope = (&measurements[1].value - &measurements[0].value) / &uniform_interval;
				&measurements[0].value + slope * dt
			} else {
				// Extrapolate forward using last segment
				let n = measurements.len();
				let dt = BigDecimal::from_i64((current_time - measurements[n - 2].timestamp).num_milliseconds()).context("Failed to convert time difference to BigDecimal")?;
				let slope = (&measurements[n - 1].value - &measurements[n - 2].value) / &uniform_interval;
				&measurements[n - 2].value + slope * dt
			}
		} else {
			// Interpolation - use uniform spacing optimization with safe casting
			let time_from_start = (current_time - data_start).num_milliseconds();
			let segment_index = if time_from_start >= 0 && uniform_interval_ms > 0 { usize::try_from(time_from_start / uniform_interval_ms).unwrap_or(0).min(measurements.len().saturating_sub(2)) } else { 0 };

			let segment_start_time = measurements[segment_index].timestamp;
			let dt = BigDecimal::from_i64((current_time - segment_start_time).num_milliseconds()).context("Failed to convert segment time difference to BigDecimal")?;
			let slope = (&measurements[segment_index + 1].value - &measurements[segment_index].value) / &uniform_interval;
			&measurements[segment_index].value + slope * dt
		};

		result.push(Measurement { dataset_id, id: Uuid::new_v4(), timestamp: current_time, value });

		current_time += step;
	}

	Ok(result)
}

/// Optimized two-point linear interpolation
fn linear_two_point_fast(measurements: &[Measurement], start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, dataset_id: Uuid) -> Result<Vec<Measurement>> {
	let step = resolution.to_step();
	let mut result = Vec::new();

	// Check for identical timestamps first
	let dt_millis = (measurements[1].timestamp - measurements[0].timestamp).num_milliseconds();

	if dt_millis == 0 {
		// Handle identical timestamps - use constant value interpolation
		let constant_value = &measurements[0].value;
		let mut current_time = start;

		while current_time <= end {
			result.push(Measurement { dataset_id, id: Uuid::new_v4(), timestamp: current_time, value: constant_value.clone() });
			current_time += step;
		}

		return Ok(result);
	}

	// Pre-compute slope once (safe now that we know dt_millis != 0)
	let dt = BigDecimal::from_i64(dt_millis).context("Failed to convert timestamp difference to BigDecimal")?;
	let dy = &measurements[1].value - &measurements[0].value;
	let slope = dy / dt;
	let base_value = &measurements[0].value;
	let base_time = measurements[0].timestamp;

	let mut current_time = start;

	while current_time <= end {
		let time_diff = BigDecimal::from_i64((current_time - base_time).num_milliseconds()).context("Failed to convert time difference to BigDecimal")?;
		let value = base_value + &slope * time_diff;

		result.push(Measurement { dataset_id, id: Uuid::new_v4(), timestamp: current_time, value });

		current_time += step;
	}

	Ok(result)
}

/// Check if measurements are uniformly spaced
fn is_uniformly_spaced(measurements: &[Measurement]) -> bool {
	if measurements.len() < 3 {
		return false;
	}

	let first_interval = measurements[1].timestamp - measurements[0].timestamp;

	// If the first interval is zero, this is not uniformly spaced (it's constant time)
	if first_interval.num_milliseconds() == 0 {
		return false;
	}

	let tolerance = chrono::Duration::milliseconds(50); // Tight tolerance for uniform detection

	measurements.windows(2).all(|pair| {
		let interval = pair[1].timestamp - pair[0].timestamp;
		(interval - first_interval).abs() < tolerance
	})
}

/// Linear spline implementation for efficient interpolation
struct LinearSpline {
	segments: Vec<LinearSegment>,
	time_bounds: Vec<DateTime<Utc>>,
}

impl LinearSpline {
	fn new(measurements: &[Measurement]) -> Result<Self> {
		let mut segments = Vec::with_capacity(measurements.len().saturating_sub(1));
		let mut time_bounds = Vec::with_capacity(measurements.len());

		for i in 0..measurements.len() - 1 {
			let segment = LinearSegment::fit_linear(&measurements[i], &measurements[i + 1])?;
			segments.push(segment);
			time_bounds.push(measurements[i].timestamp);
		}
		time_bounds.push(measurements[measurements.len() - 1].timestamp);

		Ok(Self { segments, time_bounds })
	}

	fn evaluate(&self, target_time: DateTime<Utc>) -> Result<BigDecimal> {
		// Handle boundary cases
		if target_time <= self.time_bounds[0] {
			return Ok(self.segments[0].intercept.clone());
		}
		if target_time >= self.time_bounds[self.time_bounds.len() - 1] {
			return Ok(self.segments[self.segments.len() - 1].evaluate_at_end());
		}

		// Find the appropriate segment
		for (i, &bound_time) in self.time_bounds.iter().enumerate().skip(1) {
			if target_time <= bound_time {
				let segment_idx = i - 1;
				let dt_millis = (target_time - self.time_bounds[segment_idx]).num_milliseconds();
				let dt = BigDecimal::from_i64(dt_millis).context("Failed to convert time to BigDecimal")?;
				return Ok(self.segments[segment_idx].evaluate(dt));
			}
		}

		// Fallback to last segment
		let last_idx = self.segments.len() - 1;
		let dt_millis = (target_time - self.time_bounds[last_idx]).num_milliseconds();
		let dt = BigDecimal::from_i64(dt_millis).context("Failed to convert time to BigDecimal")?;
		Ok(self.segments[last_idx].evaluate(dt))
	}
}

/// Linear segment with slope and intercept
#[derive(Debug, Clone)]
struct LinearSegment {
	slope: BigDecimal,
	intercept: BigDecimal,
	end_value: BigDecimal, // Store end value for identical timestamps
}

impl LinearSegment {
	/// Create a linear segment between two measurements
	fn fit_linear(p1: &Measurement, p2: &Measurement) -> Result<Self> {
		let dt_millis = (p2.timestamp - p1.timestamp).num_milliseconds();

		if dt_millis == 0 {
			// Handle identical timestamps - create constant segment
			return Ok(Self {
				slope: BigDecimal::zero(),
				intercept: p1.value.clone(),
				end_value: p1.value.clone(), // Use first value for consistency
			});
		}

		let dt = BigDecimal::from_i64(dt_millis).context("Failed to convert time difference to BigDecimal")?;
		let dy = &p2.value - &p1.value;
		let slope = dy / dt;

		// y = mx + b, where b is the y-intercept at the start time
		let intercept = p1.value.clone();
		let end_value = p2.value.clone();

		Ok(Self { slope, intercept, end_value })
	}

	/// Evaluate the linear function at time offset dt (in milliseconds)
	fn evaluate(&self, dt: BigDecimal) -> BigDecimal {
		&self.intercept + &self.slope * dt
	}

	/// Get the end value of this segment
	fn evaluate_at_end(&self) -> BigDecimal {
		self.end_value.clone()
	}
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use bigdecimal::BigDecimal;
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;
    use crate::{Measurement, Resolution};
    use super::linear;

    fn create_test_measurements() -> Vec<Measurement> {
        let dataset_id = Uuid::new_v4();
        vec![
            Measurement {
                id: Uuid::new_v4(),
                dataset_id,
                timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(),
                value: BigDecimal::from_str("10.0").unwrap(),
            },
            Measurement {
                id: Uuid::new_v4(),
                dataset_id,
                timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 5, 0).unwrap(), // 5 minutes later
                value: BigDecimal::from_str("20.0").unwrap(),
            },
            Measurement {
                id: Uuid::new_v4(),
                dataset_id,
                timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 10, 0).unwrap(), // 10 minutes later
                value: BigDecimal::from_str("30.0").unwrap(),
            },
        ]
    }

    #[test]
    fn test_linear_interpolation() {
        let measurements = create_test_measurements();
        let start = measurements[0].timestamp;
        let end = measurements[2].timestamp;

        let result = linear(measurements, start, end, Resolution::Minutes);
        assert!(result.is_ok());

        let interpolated = result.unwrap();
        assert!(!interpolated.is_empty());
        
        // Should have 11 points (0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10 minutes)
        assert_eq!(interpolated.len(), 11);
    }

    #[test]
    fn test_linear_multiple_segments() {
        let dataset_id = Uuid::new_v4();
        let measurements = vec![
            Measurement {
                id: Uuid::new_v4(),
                dataset_id,
                timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(),
                value: BigDecimal::from_str("0.0").unwrap(),
            },
            Measurement {
                id: Uuid::new_v4(),
                dataset_id,
                timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 2, 0).unwrap(),
                value: BigDecimal::from_str("10.0").unwrap(),
            },
            Measurement {
                id: Uuid::new_v4(),
                dataset_id,
                timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 4, 0).unwrap(),
                value: BigDecimal::from_str("15.0").unwrap(),
            },
        ];

        let start = measurements[0].timestamp;
        let end = measurements[2].timestamp;

        let result = linear(measurements, start, end, Resolution::Minutes);
        assert!(result.is_ok());

        let interpolated = result.unwrap();
        assert_eq!(interpolated.len(), 5); // 0, 1, 2, 3, 4 minutes
    }

    #[test]
    fn test_linear_extrapolation_forward() {
        let measurements = create_test_measurements();
        let start = measurements[0].timestamp;
        let end = measurements[2].timestamp + chrono::Duration::minutes(5); // Extend 5 minutes beyond

        let result = linear(measurements, start, end, Resolution::Minutes);
        assert!(result.is_ok());

        let interpolated = result.unwrap();
        assert_eq!(interpolated.len(), 16); // 0 to 15 minutes
    }

    #[test]
    fn test_linear_extrapolation_backward() {
        let measurements = create_test_measurements();
        let start = measurements[0].timestamp - chrono::Duration::minutes(5); // Start 5 minutes before
        let end = measurements[2].timestamp;

        let result = linear(measurements, start, end, Resolution::Minutes);
        assert!(result.is_ok());

        let interpolated = result.unwrap();
        assert_eq!(interpolated.len(), 16); // -5 to 10 minutes
    }

    #[test]
    fn test_empty_measurements() {
        let measurements = vec![];
        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 1, 0, 0).unwrap();

        let result = linear(measurements, start, end, Resolution::Minutes);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_insufficient_measurements_error() {
        let dataset_id = Uuid::new_v4();
        let measurements = vec![Measurement {
            id: Uuid::new_v4(),
            dataset_id,
            timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(),
            value: BigDecimal::from_str("10.0").unwrap(),
        }];

        let start = measurements[0].timestamp;
        let end = measurements[0].timestamp + chrono::Duration::minutes(10);

        let result = linear(measurements, start, end, Resolution::Minutes);
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_time_range_error() {
        let measurements = create_test_measurements();
        let start = measurements[2].timestamp; // End time
        let end = measurements[0].timestamp;   // Start time (invalid: start > end)

        let result = linear(measurements, start, end, Resolution::Minutes);
        assert!(result.is_err());
    }

    #[test]
    fn test_identical_timestamps() {
        let dataset_id = Uuid::new_v4();
        let timestamp = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let measurements = vec![
            Measurement {
                id: Uuid::new_v4(),
                dataset_id,
                timestamp,
                value: BigDecimal::from_str("10.0").unwrap(),
            },
            Measurement {
                id: Uuid::new_v4(),
                dataset_id,
                timestamp,
                value: BigDecimal::from_str("20.0").unwrap(),
            },
        ];

        let result = linear(measurements, timestamp, timestamp + chrono::Duration::minutes(1), Resolution::Minutes);
        assert!(result.is_ok());
        
        let interpolated = result.unwrap();
        assert_eq!(interpolated.len(), 2); // 0 and 1 minute
    }

    #[test]
    fn test_different_dataset_ids_error() {
        let measurements = vec![
            Measurement {
                id: Uuid::new_v4(),
                dataset_id: Uuid::new_v4(), // Different dataset ID
                timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(),
                value: BigDecimal::from_str("10.0").unwrap(),
            },
            Measurement {
                id: Uuid::new_v4(),
                dataset_id: Uuid::new_v4(), // Different dataset ID
                timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 5, 0).unwrap(),
                value: BigDecimal::from_str("20.0").unwrap(),
            },
        ];

        let start = measurements[0].timestamp;
        let end = measurements[1].timestamp;

        let result = linear(measurements, start, end, Resolution::Minutes);
        assert!(result.is_err());
    }

    #[test]
    fn test_result_dataset_consistency() {
        let measurements = create_test_measurements();
        let dataset_id = measurements[0].dataset_id;
        let start = measurements[0].timestamp;
        let end = measurements[2].timestamp;

        let result = linear(measurements, start, end, Resolution::Minutes);
        assert!(result.is_ok());

        let interpolated = result.unwrap();
        for measurement in interpolated {
            assert_eq!(measurement.dataset_id, dataset_id);
        }
    }

    #[test] 
    fn test_different_resolutions() {
        let measurements = create_test_measurements();
        let start = measurements[0].timestamp;
        let end = measurements[0].timestamp + chrono::Duration::minutes(5); // ← SHORT 5-minute window

        // Test with controlled resolution that won't generate massive output
        let resolutions = vec![
            (Resolution::Minutes, 6),   // 0, 1, 2, 3, 4, 5 minutes = 6 points
            (Resolution::Seconds, 301), // 5 minutes * 60 + 1 = 301 points  
        ];

        for (resolution, expected_count) in resolutions {
            let result = linear(measurements.clone(), start, end, resolution);
            assert!(result.is_ok(), "Resolution {:?} should work", resolution);
            
            let interpolated = result.unwrap();
            assert_eq!(interpolated.len(), expected_count, 
                "Resolution {:?} should produce {} points, got {}", 
                resolution, expected_count, interpolated.len());
        }
    }
}
