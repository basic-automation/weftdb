use chrono::{DateTime, TimeDelta, Utc};
pub use cubic::*;
pub use linear::*;
pub use polynomial::*;
pub use quadratic::*;
use anyhow::Result;

use crate::Measurement;

mod cubic;
mod linear;
mod polynomial;
mod quadratic;

#[derive(Copy, Clone)]
pub enum Resolution {
    Microseconds,
    Milliseconds,
    Seconds,
    Minutes,
    Hours,
    Days,
    Weeks,
    Months,
    Years,
}

impl Resolution {
    #[must_use] pub const fn to_step(&self) -> TimeDelta {
        match self {
            Self::Microseconds => TimeDelta::microseconds(1),
            Self::Milliseconds => TimeDelta::milliseconds(1),
            Self::Seconds => TimeDelta::seconds(1),
            Self::Minutes => TimeDelta::minutes(1),
            Self::Hours => TimeDelta::hours(1),
            Self::Days => TimeDelta::days(1),
            Self::Weeks => TimeDelta::weeks(1),
            Self::Months => TimeDelta::days(30), // Approximation
            Self::Years => TimeDelta::days(365), // Approximation
        }
    }
}

#[derive(Copy, Clone)]
pub enum SplineType {
    Linear,
    Quadratic,
    Cubic,
    Polynomial(usize),
}

/// Automatically selects and applies the appropriate spline interpolation method.
///
/// This function provides a unified interface for all spline interpolation types,
/// automatically dispatching to the correct implementation based on the `SplineType`.
///
/// # Arguments
///
/// * `measurements` - Vector of measurements to interpolate
/// * `start` - Start time for interpolation range
/// * `end` - End time for interpolation range  
/// * `resolution` - Time resolution for output measurements
/// * `spline_type` - Type of spline interpolation to use
///
/// # Returns
///
/// Returns a vector of interpolated measurements at the specified resolution.
///
/// # Errors
///
/// This function will return an error if:
/// - The underlying spline function fails
/// - Invalid time range (start >= end)
/// - Insufficient measurements for the chosen spline type
/// - Inconsistent dataset IDs across measurements
/// - Invalid polynomial degree (for polynomial splines)
///
/// # Examples
///
/// ```rust
/// # use database::{auto_interpolate, SplineType, Resolution, Measurement};
/// # use bigdecimal::BigDecimal;
/// # use chrono::{DateTime, Utc, TimeZone};
/// # use uuid::Uuid;
/// # use std::str::FromStr;
/// let dataset_id = Uuid::new_v4();
/// let measurements = vec![
///     Measurement {
///         id: Uuid::new_v4(),
///         dataset_id,
///         timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(),
///         value: BigDecimal::from_str("10.0").unwrap(),
///     },
///     Measurement {
///         id: Uuid::new_v4(),
///         dataset_id,
///         timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 1, 0, 0).unwrap(),
///         value: BigDecimal::from_str("20.0").unwrap(),
///     },
/// ];
///
/// let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
/// let end = Utc.with_ymd_and_hms(2023, 1, 1, 1, 0, 0).unwrap();
///
/// let result = auto_interpolate(
///     measurements,
///     start,
///     end,
///     Resolution::Minutes,
///     SplineType::Linear
/// ).unwrap();
/// ```
pub fn auto_interpolate(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline_type: SplineType) -> Result<Vec<Measurement>> {
    match spline_type {
        SplineType::Linear => linear(measurements, start, end, resolution),
        SplineType::Quadratic => quadratic(measurements, start, end, resolution),
        SplineType::Cubic => cubic(measurements, start, end, resolution),
        SplineType::Polynomial(degree) => polynomial(measurements, start, end, resolution, degree),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use crate::Measurement;
    use bigdecimal::BigDecimal;
    use std::str::FromStr;
    use uuid::Uuid;

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
                timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 1, 0, 0).unwrap(),
                value: BigDecimal::from_str("20.0").unwrap(),
            },
            Measurement {
                id: Uuid::new_v4(),
                dataset_id,
                timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 2, 0, 0).unwrap(),
                value: BigDecimal::from_str("30.0").unwrap(),
            },
        ]
    }

    #[test]
    fn test_auto_interpolate_linear() {
        let measurements = create_test_measurements();
        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 2, 0, 0).unwrap();
        
        let result = auto_interpolate(
            measurements,
            start,
            end,
            Resolution::Hours,
            SplineType::Linear,
        ).unwrap();
        
        assert!(!result.is_empty());
        // Verify that linear interpolation was called
        assert_eq!(result.len(), 3); // Assuming linear returns 3 points
    }

    #[test]
    fn test_auto_interpolate_quadratic() {
        let measurements = create_test_measurements();
        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 2, 0, 0).unwrap();
        
        let result = auto_interpolate(
            measurements,
            start,
            end,
            Resolution::Hours,
            SplineType::Quadratic,
        ).unwrap();
        
        assert!(!result.is_empty());
    }

    #[test]
    fn test_auto_interpolate_cubic() {
        let measurements = create_test_measurements();
        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 2, 0, 0).unwrap();
        
        let result = auto_interpolate(
            measurements,
            start,
            end,
            Resolution::Hours,
            SplineType::Cubic,
        ).unwrap();
        
        assert!(!result.is_empty());
    }

    #[test]
    fn test_auto_interpolate_polynomial() {
        let measurements = create_test_measurements();
        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 2, 0, 0).unwrap();
        
        let result = auto_interpolate(
            measurements,
            start,
            end,
            Resolution::Hours,
            SplineType::Polynomial(2),
        ).unwrap();
        
        assert!(!result.is_empty());
    }

    #[test]
    fn test_auto_interpolate_different_resolutions() {
        let measurements = create_test_measurements();
        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 2, 0, 0).unwrap();
        
        // Test different resolutions
        for resolution in [Resolution::Minutes, Resolution::Hours, Resolution::Days] {
            let result = auto_interpolate(
                measurements.clone(),
                start,
                end,
                resolution,
                SplineType::Linear,
            ).unwrap();
            
            assert!(!result.is_empty());
        }
    }

    #[test]
    fn test_auto_interpolate_empty_measurements() {
        let measurements = vec![];
        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 2, 0, 0).unwrap();
        
        let result = auto_interpolate(
            measurements,
            start,
            end,
            Resolution::Hours,
            SplineType::Linear,
        ).unwrap();
        
        assert!(result.is_empty());
    }

    #[test]
    fn test_auto_interpolate_single_measurement() {
        let dataset_id = Uuid::new_v4();
        let measurements = vec![
            Measurement {
                id: Uuid::new_v4(),
                dataset_id,
                timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(),
                value: BigDecimal::from_str("10.0").unwrap(),
            },
        ];
        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 2, 0, 0).unwrap();
        
        let result = auto_interpolate(
            measurements,
            start,
            end,
            Resolution::Hours,
            SplineType::Cubic,
        );
        
        // Should return an error for insufficient measurements
        assert!(result.is_err());
    }

    #[test]
    fn test_auto_interpolate_polynomial_various_degrees() {
        let measurements = create_test_measurements();
        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 2, 0, 0).unwrap();
        
        for degree in [1, 2, 3, 4] {
            let result = auto_interpolate(
                measurements.clone(),
                start,
                end,
                Resolution::Hours,
                SplineType::Polynomial(degree),
            ).unwrap();
            
            assert!(!result.is_empty());
        }
    }

    #[test]
    fn test_auto_interpolate_spline_type_dispatch() {
        let measurements = create_test_measurements();
        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 2, 0, 0).unwrap();
        
        // Test that all spline types work
        let spline_types = vec![
            SplineType::Linear,
            SplineType::Quadratic,
            SplineType::Cubic,
            SplineType::Polynomial(2),
        ];
        
        for spline_type in spline_types {
            let result = auto_interpolate(
                measurements.clone(),
                start,
                end,
                Resolution::Hours,
                spline_type,
            ).unwrap();
            
            assert!(!result.is_empty());
        }
    }

    #[test]
    fn test_auto_interpolate_time_bounds() {
        let measurements = create_test_measurements();
        let start = Utc.with_ymd_and_hms(2023, 1, 1, 2, 0, 0).unwrap(); // start > end
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        
        let result = auto_interpolate(
            measurements,
            start,
            end,
            Resolution::Hours,
            SplineType::Cubic,
        );
        
        // Should return an error for invalid time range
        assert!(result.is_err());
    }

    #[test]
    fn test_auto_interpolate_dataset_consistency() {
        let measurements = create_test_measurements();
        let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2023, 1, 1, 2, 0, 0).unwrap();
        
        let result = auto_interpolate(
            measurements,
            start,
            end,
            Resolution::Hours,
            SplineType::Linear,
        ).unwrap();
        
        // All results should have the same dataset_id
        if let Some(first_measurement) = result.first() {
            let expected_dataset_id = first_measurement.dataset_id;
            for measurement in &result {
                assert_eq!(measurement.dataset_id, expected_dataset_id);
            }
        }
    }
}
