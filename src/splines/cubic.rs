use bigdecimal::{BigDecimal, FromPrimitive, Zero};
use chrono::{DateTime, Utc};
use uuid::Uuid;
use anyhow::{Result, Context};

use crate::{Measurement, Resolution, Error};

/// cubic takes a vector of `Measurement`, a start date/time, an end date/time, a `Resolution`, and returns a vector of interpolated or extrapolated (or both) `Measurement` using cubic spline interpolation.
/// The `Resolution` is used to determine the time step for the interpolation or extrapolation.
pub fn cubic(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Result<Vec<Measurement>> {
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

    // Sort measurements by timestamp
    let mut sorted_measurements = measurements;
    sorted_measurements.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

    let step = resolution.to_step();

    // Build cubic spline coefficients
    let spline = CubicSpline::new(&sorted_measurements)?;

    let mut result = Vec::new();

    // round start and end to the nearest step
    let start_millis = start.timestamp_millis();
    let step_millis = step.num_milliseconds();
    let start_offset = start_millis % step_millis;
    let start = if start_offset == 0 { start } else { start - chrono::TimeDelta::milliseconds(start_offset) };

    let end_millis = end.timestamp_millis();
    let end_offset = end_millis % step_millis;
    let end = if end_offset == 0 { end } else { end + chrono::TimeDelta::milliseconds(step_millis - end_offset) };

    let mut current_time = start;

    while current_time <= end {
        let value = spline.evaluate(current_time)?;
        result.push(Measurement { dataset_id, id: Uuid::new_v4(), timestamp: current_time, value });

        current_time += step;
    }

    Ok(result)
}

struct CubicSpline {
    measurements: Vec<Measurement>,
    coefficients: Vec<SplineSegment>,
}

struct SplineSegment {
    a: BigDecimal, // cubic coefficient
    b: BigDecimal, // quadratic coefficient
    c: BigDecimal, // linear coefficient
    d: BigDecimal, // constant coefficient
}

impl CubicSpline {
    fn new(measurements: &[Measurement]) -> Result<Self> {
        let num_points = measurements.len();
        if num_points < 2 {
            return Err(Error::InsufficientPointsForCubicSplineError.into());
        }

        let mut coefficients = Vec::with_capacity(num_points - 1);

        if num_points == 2 {
            // Linear interpolation for 2 points
            let dt = BigDecimal::from_i64((measurements[1].timestamp - measurements[0].timestamp).num_milliseconds())
                .context("Failed to convert timestamp difference to BigDecimal")?;
            let dy = measurements[1].value.clone() - measurements[0].value.clone();
            let slope = dy / dt;

            coefficients.push(SplineSegment { 
                a: BigDecimal::zero(), 
                b: BigDecimal::zero(), 
                c: slope, 
                d: measurements[0].value.clone() 
            });
        } else {
            // Natural cubic spline for 3+ points
            let intervals: Result<Vec<BigDecimal>> = (0..num_points - 1)
                .map(|i| {
                    BigDecimal::from_i64((measurements[i + 1].timestamp - measurements[i].timestamp).num_milliseconds())
                        .context(format!("Failed to convert timestamp difference to BigDecimal for segment {i}"))
                })
                .collect();
            let intervals = intervals?;

            let delta: Vec<BigDecimal> = (0..num_points - 1)
                .map(|i| (measurements[i + 1].value.clone() - measurements[i].value.clone()) / intervals[i].clone())
                .collect();

            // Solve tridiagonal system for second derivatives
            let mut second_derivatives = vec![BigDecimal::zero(); num_points];

            if num_points > 2 {
                let three = BigDecimal::from_f64(3.0).context("Failed to create BigDecimal from 3.0")?;
                let two = BigDecimal::from_f64(2.0).context("Failed to create BigDecimal from 2.0")?;
                let six = BigDecimal::from_f64(6.0).context("Failed to create BigDecimal from 6.0")?;

                let mut alpha = vec![BigDecimal::zero(); num_points - 1];
                for i in 1..num_points - 1 {
                    alpha[i] = three.clone() * (delta[i].clone() / intervals[i].clone() - delta[i - 1].clone() / intervals[i - 1].clone());
                }

                let mut lower_diag = vec![BigDecimal::from_f64(1.0).context("Failed to create BigDecimal from 1.0")?; num_points];
                let mut mu = vec![BigDecimal::zero(); num_points];
                let mut intermediate = vec![BigDecimal::zero(); num_points];

                for i in 1..num_points - 1 {
                    lower_diag[i] = two.clone() * (intervals[i - 1].clone() + intervals[i].clone()) - intervals[i - 1].clone() * mu[i - 1].clone();
                    mu[i] = intervals[i].clone() / lower_diag[i].clone();
                    intermediate[i] = (alpha[i].clone() - intervals[i - 1].clone() * intermediate[i - 1].clone()) / lower_diag[i].clone();
                }

                for i in (0..num_points - 1).rev() {
                    second_derivatives[i] = intermediate[i].clone() - mu[i].clone() * second_derivatives[i + 1].clone();
                }

                // Calculate cubic coefficients for each segment
                for i in 0..num_points - 1 {
                    let cubic_coeff = (second_derivatives[i + 1].clone() - second_derivatives[i].clone()) / (six.clone() * intervals[i].clone());
                    let quadratic_coeff = second_derivatives[i].clone() / two.clone();
                    let linear_coeff = delta[i].clone() - intervals[i].clone() * (second_derivatives[i + 1].clone() + two.clone() * second_derivatives[i].clone()) / six.clone();
                    let constant_coeff = measurements[i].value.clone();

                    coefficients.push(SplineSegment { 
                        a: cubic_coeff, 
                        b: quadratic_coeff, 
                        c: linear_coeff, 
                        d: constant_coeff 
                    });
                }
            }
        }

        Ok(Self { measurements: measurements.to_vec(), coefficients })
    }

    fn evaluate(&self, target_time: DateTime<Utc>) -> Result<BigDecimal> {
        let n = self.measurements.len();

        // Handle extrapolation
        if target_time <= self.measurements[0].timestamp {
            // Extrapolate using first segment
            let dt = BigDecimal::from_i64((target_time - self.measurements[0].timestamp).num_milliseconds())
                .context("Failed to convert timestamp difference to BigDecimal for backward extrapolation")?;
            return Ok(self.evaluate_segment(0, dt));
        }

        if target_time >= self.measurements[n - 1].timestamp {
            // Extrapolate using last segment
            let dt = BigDecimal::from_i64((target_time - self.measurements[n - 2].timestamp).num_milliseconds())
                .context("Failed to convert timestamp difference to BigDecimal for forward extrapolation")?;
            return Ok(self.evaluate_segment(n - 2, dt));
        }

        // Find the appropriate segment for interpolation
        for i in 0..n - 1 {
            if target_time >= self.measurements[i].timestamp && target_time <= self.measurements[i + 1].timestamp {
                let dt = BigDecimal::from_i64((target_time - self.measurements[i].timestamp).num_milliseconds())
                    .context(format!("Failed to convert timestamp difference to BigDecimal for interpolation segment {i}"))?;
                return Ok(self.evaluate_segment(i, dt));
            }
        }

        // Fallback
        Ok(self.measurements[0].value.clone())
    }

    fn evaluate_segment(&self, segment_idx: usize, dt: BigDecimal) -> BigDecimal {
        let seg = &self.coefficients[segment_idx];
        let dt2 = dt.clone() * dt.clone();
        let dt3 = dt2.clone() * dt.clone();

        seg.a.clone() * dt3 + seg.b.clone() * dt2 + seg.c.clone() * dt + seg.d.clone()
    }
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
    fn test_cubic_extrapolation_backward() {
        let dataset_id = Uuid::new_v4();
        let measurements = vec![
            create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 100.0), 
            create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 20).unwrap(), 400.0), 
            create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 30).unwrap(), 900.0)
        ];

        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 5).unwrap();

        let result = cubic(measurements, start, end, Resolution::Seconds).unwrap();

        assert!(!result.is_empty());
        // Should extrapolate smoothly backwards
    }

    #[test]
    fn test_cubic_extrapolation_forward() {
        let dataset_id = Uuid::new_v4();
        let measurements = vec![
            create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 0.0), 
            create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 100.0), 
            create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 20).unwrap(), 400.0)
        ];

        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 25).unwrap();
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 35).unwrap();

        let result = cubic(measurements, start, end, Resolution::Seconds).unwrap();

        assert!(!result.is_empty());
        // Should extrapolate smoothly forwards
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
        let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0)];

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
