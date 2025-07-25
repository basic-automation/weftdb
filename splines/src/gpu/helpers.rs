use anyhow::{Context, Result};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use chrono::{DateTime, Utc};

use crate::{
    Point, Resolution, splines::{DAYS_IN_MONTH, DAYS_IN_YEAR}
};

pub async fn get_max_buffer_size() -> Result<u64> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
    let adapter = instance.request_adapter(&wgpu::RequestAdapterOptions::default()).await.unwrap();
    let limits = adapter.limits();
    Ok(limits.max_buffer_size)
}

pub fn convert_points_to_gpu_format_f64(points: &[Point], resolution: &Resolution) -> Result<(Vec<f64>, Vec<f64>)> {
    if points.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    let base_time = points[0].timestamp;
    let mut times = Vec::with_capacity(points.len());
    let mut values = Vec::with_capacity(points.len());

    for point in points {
        let duration = point.timestamp.signed_duration_since(base_time);
        let time_offset = match resolution {
            Resolution::Nanoseconds => duration.num_nanoseconds().context("Invalid duration")? as f64,
            Resolution::Microseconds => duration.num_microseconds().context("Invalid duration")? as f64,
            Resolution::Milliseconds => duration.num_milliseconds() as f64,
            Resolution::Seconds => duration.num_seconds() as f64,
            Resolution::Minutes => duration.num_minutes() as f64,
            Resolution::Hours => duration.num_hours() as f64,
            Resolution::Days => duration.num_days() as f64,
            Resolution::Weeks => duration.num_weeks() as f64,
            Resolution::Months => duration.num_days() as f64 / DAYS_IN_MONTH as f64,
            Resolution::Years => duration.num_days() as f64 / DAYS_IN_YEAR as f64,
        };
        times.push(time_offset);
        let value = point.value.to_f64().context("Invalid value")?;
        values.push(value.clamp(-1e6, 1e6)); // Normalize values to prevent overflow
    }

    Ok((times, values))
}

pub fn convert_points_to_gpu_format_f32(points: &[Point], resolution: &Resolution) -> Result<(Vec<f32>, Vec<f32>)> {
    if points.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    let base_time = points[0].timestamp;
    let mut times = Vec::with_capacity(points.len());
    let mut values = Vec::with_capacity(points.len());

    for point in points {
        let duration = point.timestamp.signed_duration_since(base_time);
        let time_offset = match resolution {
            Resolution::Nanoseconds => duration.num_nanoseconds().context("Invalid duration")? as f32,
            Resolution::Microseconds => duration.num_microseconds().context("Invalid duration")? as f32,
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
        let value = point.value.to_f32().context("Invalid value")?;
        values.push(value.clamp(-1e6, 1e6)); // Normalize values to prevent overflow
    }

    Ok((times, values))
}

pub fn convert_datetimes_to_gpu_format_f64(target_times: &[DateTime<Utc>], resolution: &Resolution, base_time: DateTime<Utc>) -> Result<Vec<f64>> {
    let mut times = Vec::with_capacity(target_times.len());
    for &target_time in target_times {
        let duration = target_time.signed_duration_since(base_time);
        let time_offset = match resolution {
            Resolution::Nanoseconds => duration.num_nanoseconds().context("Invalid duration")? as f64,
            Resolution::Microseconds => duration.num_microseconds().context("Invalid duration")? as f64,
            Resolution::Milliseconds => duration.num_milliseconds() as f64,
            Resolution::Seconds => duration.num_seconds() as f64,
            Resolution::Minutes => duration.num_minutes() as f64,
            Resolution::Hours => duration.num_hours() as f64,
            Resolution::Days => duration.num_days() as f64,
            Resolution::Weeks => duration.num_weeks() as f64,
            Resolution::Months => duration.num_days() as f64 / DAYS_IN_MONTH as f64,
            Resolution::Years => duration.num_days() as f64 / DAYS_IN_YEAR as f64,
        };
        times.push(time_offset);
    }
    Ok(times)
}

pub fn convert_datetimes_to_gpu_format_f32(target_times: &[DateTime<Utc>], resolution: &Resolution, base_time: DateTime<Utc>) -> Result<Vec<f32>> {
    let mut times = Vec::with_capacity(target_times.len());
    for &target_time in target_times {
        let duration = target_time.signed_duration_since(base_time);
        let time_offset = match resolution {
            Resolution::Nanoseconds => duration.num_nanoseconds().context("Invalid duration")? as f32,
            Resolution::Microseconds => duration.num_microseconds().context("Invalid duration")? as f32,
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
    }
    Ok(times)
}

pub fn convert_gpu_results_to_points_f64(results: Vec<f64>, target_times: &[DateTime<Utc>]) -> Result<Vec<Point>> {
    Ok(results.into_iter().zip(target_times).map(|(value, timestamp)| Point { timestamp: timestamp.clone(), value: BigDecimal::from_f64(value).context("Invalid value").unwrap() }).collect())
}

pub fn convert_gpu_results_to_points_f32(results: Vec<f32>, target_times: &[DateTime<Utc>]) -> Result<Vec<Point>> {
    Ok(results.into_iter().zip(target_times).map(|(value, timestamp)| Point { timestamp: timestamp.clone(), value: BigDecimal::from_f32(value).context("Invalid value").unwrap() }).collect())
}