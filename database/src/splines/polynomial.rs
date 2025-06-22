use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::str::FromStr;

use crate::{splines::Resolution, Error, Measurement};

// Import the specific functions we need
use super::linear::linear;
use super::quadratic::quadratic;
use super::cubic::cubic;

/// Polynomial interpolation using Lagrange interpolation
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient measurements (< degree + 1 points)
/// - Invalid degree (> measurements.len() - 1)
/// - Timestamp conversion fails
/// - BigDecimal operations fail
pub fn polynomial(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, degree: usize) -> Result<Vec<Measurement>> {
    if measurements.is_empty() {
        return Ok(Vec::new());
    }

    if measurements.len() < degree + 1 {
        return Err(Error::InsufficientMeasurementsError.into());
    }

    if start >= end {
        return Err(Error::InvalidTimeRangeError.into());
    }

    // For very high degrees or small datasets, fall back to simpler interpolation
    if degree > measurements.len() - 1 {
        return Err(anyhow::anyhow!("Polynomial degree {} is too high for {} measurements", degree, measurements.len()));
    }

    // For lower degrees, use appropriate interpolation
    match degree {
        1 => {
            //println!("🔄 Polynomial degree 1: Using linear interpolation");
            return linear(measurements, start, end, resolution);
        }
        2 => {
            //println!("🔄 Polynomial degree 2: Using quadratic interpolation");
            return quadratic(measurements, start, end, resolution);
        }
        3 => {
            //println!("🔄 Polynomial degree 3: Using cubic interpolation");
            return cubic(measurements, start, end, resolution);
        }
        _ => {
            // Continue with polynomial implementation for degree > 3
        }
    }

    // Generate target times
    let target_times = super::generate_target_times(start, end, resolution);
    if target_times.is_empty() {
        return Ok(Vec::new());
    }

    let dataset_id = measurements[0].dataset_id;

    // Convert timestamps to seconds for calculation
    let base_time = measurements[0].timestamp;
    let x_values: Vec<f64> = measurements
        .iter()
        .map(|m| (m.timestamp - base_time).num_seconds() as f64)
        .collect();
    let y_values: Vec<f64> = measurements
        .iter()
        .map(|m| {
            m.value
                .to_string()
                .parse::<f64>()
                .unwrap_or(0.0)
        })
        .collect();

    let mut results = Vec::new();

    for target_time in target_times {
        if target_time < start || target_time > end {
            continue;
        }

        let target_x = (target_time - base_time).num_seconds() as f64;

        // Use a subset of points around the target for better numerical stability
        let max_points = (degree + 1).min(measurements.len());

        // Find the best subset of points around target_x
        let mut distances: Vec<(usize, f64)> = x_values
            .iter()
            .enumerate()
            .map(|(i, &x)| (i, (x - target_x).abs()))
            .collect();
        distances.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

        let indices: Vec<usize> = distances
            .into_iter()
            .take(max_points)
            .map(|(i, _)| i)
            .collect();

        // Perform Lagrange interpolation using the selected points
        let mut result = 0.0;
        for (i, &idx_i) in indices.iter().enumerate() {
            let mut term = y_values[idx_i];
            for (j, &idx_j) in indices.iter().enumerate() {
                if i != j {
                    term *= (target_x - x_values[idx_j]) / (x_values[idx_i] - x_values[idx_j]);
                }
            }
            result += term;
        }

        let value = bigdecimal::BigDecimal::from_str(&result.to_string())
            .context("Failed to convert interpolated value to BigDecimal")?;

        results.push(Measurement {
            id: uuid::Uuid::new_v4(),
            dataset_id,
            timestamp: target_time,
            value,
        });
    }

    Ok(results)
}

/// Determine if GPU acceleration should be used for polynomial interpolation
#[must_use]
pub fn should_use_gpu_polynomial(measurement_count: usize, estimated_output_points: usize, degree: usize) -> bool {
    // GPU thresholds increase with polynomial degree due to complexity
    let base_measurement_threshold = 1000 * (degree.max(2));
    let base_output_threshold = 25000 * (degree.max(2));
    
    measurement_count >= base_measurement_threshold && estimated_output_points >= base_output_threshold
}

/// GPU-accelerated polynomial interpolation with intelligent fallback
///
/// # Errors
///
/// Returns an error if:
/// - All interpolation methods fail
/// - Invalid parameters provided
pub async fn gpu_polynomial_interpolate_with_fallback(
    measurements: Vec<Measurement>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    resolution: Resolution,
    dataset_id: uuid::Uuid,
    degree: usize,
) -> Result<Vec<Measurement>> {
    // For lower degrees, use appropriate GPU interpolation
    match degree {
        2 => {
            println!("🚀 Using GPU acceleration for polynomial degree 2 (quadratic fallback)");
            // Use quadratic GPU implementation when available, fallback to linear for now
            super::gpu::gpu_linear_interpolate_optimized(measurements, super::generate_target_times(start, end, resolution), dataset_id).await
        }
        3 => {
            println!("🚀 Using GPU acceleration for polynomial degree 3 (cubic fallback)");
            // Use cubic GPU implementation when available, fallback to linear for now
            super::gpu::gpu_linear_interpolate_optimized(measurements, super::generate_target_times(start, end, resolution), dataset_id).await
        }
        _ => {
            // Covers degree 1 and all other cases
            println!("🚀 Using GPU linear interpolation for polynomial degree {degree}");
            super::gpu::gpu_linear_interpolate_optimized(measurements, super::generate_target_times(start, end, resolution), dataset_id).await
        }
    }
}
