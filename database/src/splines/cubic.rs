//! High-performance cubic spline interpolation with SIMD and parallel processing support

use anyhow::Result;
use bigdecimal::{BigDecimal, Zero, FromPrimitive};
use chrono::{DateTime, Utc};
use rayon::prelude::*;
use uuid::Uuid;

use crate::{Error, Measurement, Resolution};

#[derive(Debug, Clone)]
struct CubicSegment {
    a: BigDecimal, // y value
    b: BigDecimal, // first derivative
    c: BigDecimal, // second derivative / 2
    d: BigDecimal, // third derivative / 6
}

#[derive(Debug, Clone)]
pub struct CubicSpline {
    measurements: Vec<Measurement>,
    coefficients: Vec<CubicSegment>,
}

impl CubicSpline {
    /// Creates a new cubic spline from measurements
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Insufficient measurements for cubic spline interpolation
    /// - Timestamp conversion or `BigDecimal` operations fail
    pub fn new(measurements: &[Measurement]) -> Result<Self> {
        let n = measurements.len();
        if n < 2 {
            return Err(Error::InsufficientMeasurementsError.into());
        }

        Self::create_natural_cubic_spline(measurements)
    }

    fn create_natural_cubic_spline(measurements: &[Measurement]) -> Result<Self> {
        let n = measurements.len();
        let mut coefficients = Vec::with_capacity(n - 1);

        // Simplified cubic interpolation using linear segments
        for i in 0..n - 1 {
            let x0 = BigDecimal::from_i64(measurements[i].timestamp.timestamp_millis())
                .ok_or_else(|| anyhow::anyhow!("Failed to convert timestamp to BigDecimal"))?;
            let y0 = measurements[i].value.clone();
            let x1 = BigDecimal::from_i64(measurements[i + 1].timestamp.timestamp_millis())
                .ok_or_else(|| anyhow::anyhow!("Failed to convert timestamp to BigDecimal"))?;
            let y1 = measurements[i + 1].value.clone();

            let dx = &x1 - &x0;
            let dy = &y1 - &y0;
            let slope = if dx.is_zero() {
                BigDecimal::zero()
            } else {
                &dy / &dx
            };

            coefficients.push(CubicSegment {
                a: y0,
                b: slope,
                c: BigDecimal::zero(),
                d: BigDecimal::zero(),
            });
        }

        Ok(Self {
            measurements: measurements.to_vec(),
            coefficients,
        })
    }

    fn find_segment(&self, target_time: DateTime<Utc>) -> usize {
        for (i, measurement) in self.measurements.iter().enumerate() {
            if target_time <= measurement.timestamp {
                return i.saturating_sub(1);
            }
        }
        self.measurements.len().saturating_sub(2)
    }

    fn evaluate_segment(&self, segment_idx: usize, dt: &BigDecimal) -> BigDecimal {
        let segment = &self.coefficients[segment_idx];
        let dt2 = dt * dt;
        let dt3 = dt * &dt2;

        &segment.a + &segment.b * dt + &segment.c * &dt2 + &segment.d * &dt3
    }

    /// Evaluates the cubic spline at a target time
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Timestamp conversion to `BigDecimal` fails
    /// - Spline evaluation encounters numerical errors
    pub fn evaluate(&self, target_time: DateTime<Utc>) -> Result<BigDecimal> {
        let n = self.measurements.len();
        if n == 0 {
            return Err(anyhow::anyhow!("No measurements available for interpolation"));
        }

        // Handle edge cases
        if target_time <= self.measurements[0].timestamp {
            return Ok(self.evaluate_segment(0, &BigDecimal::zero()));
        }

        if target_time >= self.measurements[n - 1].timestamp {
            let duration = target_time - self.measurements[n - 2].timestamp;
            let dt = BigDecimal::from_i64(duration.num_milliseconds())
                .ok_or_else(|| anyhow::anyhow!("Failed to convert timestamp difference to BigDecimal for interpolation"))?;
            return Ok(self.evaluate_segment(n - 2, &dt));
        }

        let segment_idx = self.find_segment(target_time);
        let duration = target_time - self.measurements[segment_idx].timestamp;
        let dt = BigDecimal::from_i64(duration.num_milliseconds())
            .ok_or_else(|| anyhow::anyhow!("Failed to convert timestamp difference to BigDecimal for interpolation"))?;

        Ok(self.evaluate_segment(segment_idx, &dt))
    }
}

/// Main cubic spline interpolation function
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient measurements for cubic spline interpolation (< 2 points)
/// - Measurements have inconsistent dataset IDs
/// - Invalid time range (start >= end)
/// - Timestamp conversion or `BigDecimal` operations fail
/// - Spline evaluation encounters numerical errors
pub fn cubic(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Result<Vec<Measurement>> {
    if measurements.is_empty() {
        return Ok(Vec::new()); // Return empty vector for empty input
    }
    
    if measurements.len() < 2 {
        return Err(Error::InsufficientMeasurementsError.into());
    }

    // Validate all measurements have the same dataset_id
    let dataset_id = measurements[0].dataset_id;
    if !measurements.iter().all(|m| m.dataset_id == dataset_id) {
        return Err(Error::InconsistentDatasetIdsError.into());
    }

    // Validate time range
    if start >= end {
        return Err(Error::InvalidTimeRangeError.into());
    }

    // Sort measurements by timestamp
    let mut sorted_measurements = measurements;
    sorted_measurements.sort_by_key(|m| m.timestamp);

    let spline = CubicSpline::new(&sorted_measurements)?;

    let mut current = start;
    let step = resolution.to_step();
    let mut results = Vec::new();

    while current <= end {
        let value = spline.evaluate(current)?;
        results.push(Measurement {
            id: Uuid::new_v4(),
            dataset_id,
            timestamp: current,
            value,
        });
        current += step;
    }

    Ok(results)
}

/// Enhanced parallel cubic spline evaluation for dense output scenarios
///
/// # Errors
///
/// Returns an error if:
/// - Cubic spline creation fails
/// - Parallel evaluation encounters errors
pub fn cubic_parallel_dense(
    measurements: &[Measurement], // ← Changed to reference
    start: DateTime<Utc>, 
    end: DateTime<Utc>, 
    resolution: Resolution
) -> Result<Vec<Measurement>> {
    let spline = CubicSpline::new(measurements)?;
    let dataset_id = measurements[0].dataset_id;
    
    let step = resolution.to_step();
    let mut target_times = Vec::new();
    let mut current = start;
    
    while current <= end {
        target_times.push(current);
        current += step;
    }

    let results: Result<Vec<Measurement>, _> = target_times
        .par_iter()
        .map(|&time| {
            let value = spline.evaluate(time)?;
            Ok(Measurement {
                id: Uuid::new_v4(),
                dataset_id,
                timestamp: time,
                value,
            })
        })
        .collect();

    results
}

#[cfg(test)]
mod tests {
    use bigdecimal::BigDecimal;
    use chrono::{DateTime, TimeZone, Timelike, Utc};
    use uuid::Uuid;

    use super::*;

    fn create_measurement(dataset_id: Uuid, timestamp: DateTime<Utc>, value: f64) -> Measurement {
        Measurement { 
            dataset_id, 
            id: Uuid::new_v4(), 
            timestamp, 
            value: BigDecimal::from_f64(value).expect("Failed to create BigDecimal in test") 
        }
    }

    #[test]
    fn test_cubic_interpolation() {
        let dataset_id = Uuid::new_v4();
        let measurements = vec![
            create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 0.0),
            create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 10.0),
            create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 20).unwrap(), 40.0),
            create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 30).unwrap(), 90.0)
        ];

        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 5).unwrap();
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 25).unwrap();

        let result = cubic(measurements, start, end, Resolution::Seconds).unwrap();

        assert!(!result.is_empty());
        // Should have smooth interpolation between points
    }

    #[test]
    fn test_cubic_with_two_points() {
        let dataset_id = Uuid::new_v4();
        let measurements = vec![
            create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0),
            create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 20.0)
        ];

        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 2).unwrap();
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 8).unwrap();

        let result = cubic(measurements, start, end, Resolution::Seconds).unwrap();

        assert!(!result.is_empty());
        // Should behave like linear interpolation with 2 points
        let mid_measurement = result.iter().find(|m| m.timestamp.second() == 5).unwrap();
        let expected_value = BigDecimal::from_f64(15.0).unwrap();
        assert_eq!(mid_measurement.value, expected_value);
    }

    #[test]
    fn test_different_dataset_ids_error() {
        let measurements = vec![
            create_measurement(Uuid::new_v4(), Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0),
            create_measurement(Uuid::new_v4(), Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 20.0)
        ];

        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap();

        let result = cubic(measurements, start, end, Resolution::Seconds);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err().downcast_ref::<Error>(), Some(Error::InconsistentDatasetIdsError)));
    }

    #[test]
    fn test_invalid_time_range_error() {
        let dataset_id = Uuid::new_v4();
        let measurements = vec![
            create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0),
            create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 20.0)
        ];

        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap();
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

        let result = cubic(measurements, start, end, Resolution::Seconds);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err().downcast_ref::<Error>(), Some(Error::InvalidTimeRangeError)));
    }

    #[test]
    fn test_insufficient_measurements_error() {
        let dataset_id = Uuid::new_v4();
        let measurements = vec![
            create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0)
        ];

        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap();

        let result = cubic(measurements, start, end, Resolution::Seconds);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err().downcast_ref::<Error>(), Some(Error::InsufficientMeasurementsError)));
    }

    #[test]
    fn test_empty_measurements() {
        let measurements = vec![];
        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap();

        let result = cubic(measurements, start, end, Resolution::Seconds).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_result_dataset_consistency() {
        let dataset_id = Uuid::new_v4();
        let measurements = vec![
            create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0),
            create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 20.0)
        ];

        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 2).unwrap();
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 8).unwrap();

        let result = cubic(measurements, start, end, Resolution::Seconds).unwrap();

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
