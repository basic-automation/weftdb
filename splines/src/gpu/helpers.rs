use anyhow::Result;
use bigdecimal::{BigDecimal, ToPrimitive, Zero};
use chrono::{DateTime, Utc};
use bigdecimal::FromPrimitive;

use crate::{Point, Resolution, splines::{DAYS_IN_MONTH, DAYS_IN_YEAR}};

/// Convert points to GPU-compatible f32 format
pub fn convert_points_to_gpu_format(points: &[Point], resolution: Resolution) -> Result<(Vec<f32>, Vec<f32>)> {
    if points.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    // Use a fixed base time (Unix epoch) for consistency
    let base_time = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
    
    let mut times = Vec::with_capacity(points.len());
    let mut values = Vec::with_capacity(points.len());

    for point in points {
        // Convert timestamp to f32 offset from base time
        let duration = point.timestamp - base_time;
        let time_offset = match resolution {
            Resolution::Nanoseconds => duration.num_nanoseconds().unwrap_or(0) as f32,
            Resolution::Microseconds => duration.num_microseconds().unwrap_or(0) as f32,
            Resolution::Milliseconds => duration.num_milliseconds() as f32,
            Resolution::Seconds => duration.num_seconds() as f32,
            Resolution::Minutes => duration.num_minutes() as f32,
            Resolution::Hours => duration.num_hours() as f32,
            Resolution::Days => duration.num_days() as f32,
            Resolution::Weeks => duration.num_weeks() as f32,
            Resolution::Months => duration.num_days() as f32 / DAYS_IN_MONTH as f32,
            Resolution::Years => duration.num_days() as f32 / DAYS_IN_YEAR as f32,
        };

        times.push(time_offset);
        
        // Convert BigDecimal to f32
        let value = point.value.to_f32().unwrap_or(0.0);
        values.push(value);
    }

    Ok((times, values))
}

/// Convert DateTime array to GPU-compatible f32 format
pub fn convert_datetimes_to_gpu_format(times: &[DateTime<Utc>], resolution: Resolution) -> Result<Vec<f32>> {
    if times.is_empty() {
        return Ok(Vec::new());
    }

    // Use the same fixed base time (Unix epoch) for consistency
    let base_time = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
    
    let mut gpu_times = Vec::with_capacity(times.len());

    for &time in times {
        // Convert timestamp to f32 offset from base time
        let duration = time - base_time;
        let time_offset = match resolution {
            Resolution::Nanoseconds => duration.num_nanoseconds().unwrap_or(0) as f32,
            Resolution::Microseconds => duration.num_microseconds().unwrap_or(0) as f32,
            Resolution::Milliseconds => duration.num_milliseconds() as f32,
            Resolution::Seconds => duration.num_seconds() as f32,
            Resolution::Minutes => duration.num_minutes() as f32,
            Resolution::Hours => duration.num_hours() as f32,
            Resolution::Days => duration.num_days() as f32,
            Resolution::Weeks => duration.num_weeks() as f32,
            Resolution::Months => duration.num_days() as f32 / DAYS_IN_MONTH as f32,
            Resolution::Years => duration.num_days() as f32 / DAYS_IN_YEAR as f32,
        };

        gpu_times.push(time_offset);
    }

    Ok(gpu_times)
}

/// Convert GPU results back to Points
pub fn convert_gpu_results_to_points(gpu_results: Vec<f32>, target_times: Vec<DateTime<Utc>>) -> Result<Vec<Point>> {
    if gpu_results.len() != target_times.len() {
        anyhow::bail!("GPU results and target times must have the same length");
    }

    let mut points = Vec::with_capacity(gpu_results.len());
    
    for (value, timestamp) in gpu_results.into_iter().zip(target_times.into_iter()) {
        points.push(Point {
            timestamp,
            value: BigDecimal::from_f64(value as f64).unwrap_or(BigDecimal::zero()),
        });
    }

    Ok(points)
}
