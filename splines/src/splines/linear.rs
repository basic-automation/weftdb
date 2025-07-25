use std::io::Write;

use anyhow::{Result, bail};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive, Zero};
use chrono::{DateTime, Utc};
use wide::f64x4;

use super::SIMD_BATCH_SIZE;
use crate::{
	Error, Point, Resolution, Spline, helpers::{InterpolationState, batch}, is_uniformly_spaced
};

pub async fn linear(points: &Vec<Point>, start: &DateTime<Utc>, end: &DateTime<Utc>, resolution: &Resolution) -> Result<Vec<Point>> {
	Spline::Linear.pre_check(points, start, end)?;

	if points.len() == 2 {
		println!("Using fast path for two-point linear interpolation");
		return batch(points, start, end, &Spline::Linear, resolution, |state| Box::pin(linear_two_point_fast(state))).await;
	}

	if is_uniformly_spaced(&points, resolution) {
		println!("Using fast path for uniformly spaced data");
		return batch(points, start, end, &Spline::Linear, resolution, |state| Box::pin(linear_uniform_fast(state))).await;
	}

	println!("Using standard linear interpolation");
	batch(points, start, end, &Spline::Linear, resolution, |state| Box::pin(linear_interpolate(state))).await
}

pub async fn linear_interpolate(state: &mut InterpolationState) -> Result<()> {
	let Some(input_points) = &state.input_points else {
		bail!("No input points provided for interpolation");
	};
	let Some(batch_times) = &state.batch_times else {
		bail!("No batch times provided for interpolation");
	};

	let spline = crate::splines::LinearSpline::new(input_points, &state.resolution)?;

	// Initialize result if storing in memory
	if state.temp_file.is_none() && state.result.is_none() {
		state.result = Some(Vec::new());
	}

	for current_time in batch_times.iter() {
		// Process all times, remove .take(batch_size)
		let value = spline.evaluate(current_time)?;

		if state.temp_file.is_none() {
			state.result.as_mut().unwrap().push(Point { timestamp: *current_time, value });
		} else if let Some(writer) = state.temp_file.as_mut() {
			let mut writer = writer.lock().await;
			writeln!(writer, "{},{}", current_time.to_rfc3339(), value)?;
		}
	}

	if let Some(writer) = state.temp_file.as_mut() {
		let mut writer = writer.lock().await;
		writer.flush().map_err(|e| Error::IOError(e.to_string()))?;
	}
	Ok(())
}

async fn linear_uniform_fast(state: &mut InterpolationState) -> Result<()> {
    let Some(input_points) = &state.input_points else {
        bail!("No input points provided for interpolation");
    };
    let Some(batch_times) = &state.batch_times else {
        bail!("No batch times provided for interpolation");
    };

    if !is_uniformly_spaced(input_points, &state.resolution) {
        println!("Data not uniformly spaced, falling back to linear_interpolate");
        return linear_interpolate(state).await;
    }

    // Initialize result if storing in memory
    if state.temp_file.is_none() && state.result.is_none() {
        state.result = Some(Vec::new());
    }

    let mut result = Vec::new();

    // Pre-compute uniform interval
    let uniform_interval = state.resolution.difference(&input_points[1].timestamp, &input_points[0].timestamp)?;

    if uniform_interval == 0 {
        // Constant value interpolation for identical timestamps
        let constant_value = &input_points[0].value;
        for &current_time in batch_times {
            result.push(Point { timestamp: current_time, value: constant_value.clone() });
        }
    } else {
        // Linear interpolation for uniformly spaced points
        let uniform_interval = BigDecimal::from_i64(uniform_interval).ok_or(Error::DecimalConversionError)?;
        let data_start = input_points[0].timestamp;
        let data_end = input_points[input_points.len() - 1].timestamp;

        for &current_time in batch_times {
            let value = if current_time < data_start {
                // Extrapolate backward using first segment
                let dt = BigDecimal::from_i64(state.resolution.difference(&current_time, &data_start)?).ok_or(Error::DecimalConversionError)?;
                let slope = (&input_points[1].value - &input_points[0].value) / &uniform_interval;
                &input_points[0].value + slope * dt
            } else if current_time > data_end {
                let n = input_points.len(); // Define n
                let dt = BigDecimal::from_i64(state.resolution.difference(&current_time, &input_points[n - 1].timestamp)?).ok_or(Error::DecimalConversionError)?;
                let slope = (&input_points[n - 1].value - &input_points[n - 2].value) / &uniform_interval;
                &input_points[n - 1].value + slope * dt
            } else {
                // Interpolate using uniform spacing
                let time_from_start = state.resolution.difference(&current_time, &data_start)?;
                let segment_index = if time_from_start >= 0 && uniform_interval > BigDecimal::zero() { 
                    (time_from_start / uniform_interval.clone()).to_usize().unwrap_or(0).min(input_points.len().saturating_sub(2)) 
                } else { 
                    0 
                };
                let segment_start_time = input_points[segment_index].timestamp;
                let dt = BigDecimal::from_i64(state.resolution.difference(&current_time, &segment_start_time)?).ok_or(Error::DecimalConversionError)?;
                let slope = (&input_points[segment_index + 1].value - &input_points[segment_index].value) / &uniform_interval;
                &input_points[segment_index].value + slope * dt
            };
            result.push(Point { timestamp: current_time, value });
        }
    }

    // Store results based on state
    if let Some(writer) = state.temp_file.as_mut() {
        let mut writer = writer.lock().await;
        for point in result {
            writeln!(writer, "{},{}", point.timestamp.to_rfc3339(), point.value)?;
        }
        writer.flush().map_err(|e| Error::IOError(e.to_string()))?;
    } else {
        state.result = Some(result);
    }

    Ok(())
}

/// Optimized two-point linear interpolation
async fn linear_two_point_fast(state: &mut InterpolationState) -> Result<()> {
	let Some(input_points) = &state.input_points else {
		bail!("No input points provided for interpolation");
	};
	let Some(batch_times) = &state.batch_times else {
		bail!("No batch times provided for interpolation");
	};

	if input_points.len() != 2 {
		bail!(Error::InsufficientPointsError);
	}

	// Initialize result if storing in memory
	if state.temp_file.is_none() && state.result.is_none() {
		state.result = Some(Vec::new());
	}

	let mut result = Vec::new();

	// Check for identical timestamps
	let dt = BigDecimal::from_i64(state.resolution.difference(&input_points[1].timestamp, &input_points[0].timestamp)?).ok_or(Error::DecimalConversionError)?;

	if dt == BigDecimal::zero() {
		// Constant value interpolation for identical timestamps
		let constant_value = &input_points[0].value;
		for current_time in batch_times {
			result.push(Point { timestamp: *current_time, value: constant_value.clone() });
		}
	} else {
		// Linear interpolation: pre-compute slope once
		let dy = &input_points[1].value - &input_points[0].value;
		let slope = dy / dt;
		let base_value = &input_points[0].value;
		let base_time = input_points[0].timestamp;

		for current_time in batch_times {
			let time_diff = BigDecimal::from_i64(state.resolution.difference(current_time, &base_time)?).ok_or(Error::DecimalConversionError)?;
			let value = base_value + &slope * time_diff;
			result.push(Point { timestamp: *current_time, value });
		}
	}

	// Store results based on state
	if let Some(writer) = state.temp_file.as_mut() {
		let mut writer = writer.lock().await;
		for point in result {
			writeln!(writer, "{},{}", point.timestamp.to_rfc3339(), point.value)?;
		}
		writer.flush().map_err(|e| Error::IOError(e.to_string()))?;
	} else {
		state.result = Some(result);
	}

	Ok(())
}

/// SIMD-optimized linear interpolation for batch processing
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient points (< 2 points)
/// - Timestamp conversion fails
/// - `BigDecimal` operations fail
pub fn linear_simd(points: &[Point], target_times: &[DateTime<Utc>], resolution: Resolution) -> Result<Vec<Point>> {
	if points.len() < 2 {
		bail!(Error::InsufficientPointsError);
	}

	if target_times.is_empty() {
		return Ok(Vec::new());
	}

	// Helper to convert DateTime<Utc> to f64 based on resolution
	let time_to_f64 = |t: &DateTime<Utc>| -> f64 { resolution.to_base(t).unwrap_or(0) as f64 };

	// Convert to f64 arrays for SIMD processing
	#[allow(clippy::cast_precision_loss)]
	let input_times: Vec<f64> = points.iter().map(|m| time_to_f64(&m.timestamp)).collect();
	let input_values: Vec<f64> = points.iter().map(|m| round_to_places(m.value.to_f64().unwrap_or(0.0), 10)).collect();

	#[allow(clippy::cast_precision_loss)]
	let targets: Vec<f64> = target_times.iter().map(time_to_f64).collect();

	// Process in SIMD batches
	let mut results = Vec::with_capacity(target_times.len());

	for chunk in targets.chunks(SIMD_BATCH_SIZE) {
		let mut padded_targets = [0.0; SIMD_BATCH_SIZE];
		let chunk_size = chunk.len();

		// Copy actual values and pad with last value if needed
		padded_targets[..chunk_size].copy_from_slice(chunk);

		// Fill remaining slots with the last value
		if chunk_size < SIMD_BATCH_SIZE {
			let last_value = chunk.last().copied().unwrap_or(0.0);
			padded_targets[chunk_size..].fill(last_value);
		}

		let target_simd = f64x4::from(padded_targets);
		let result_simd = simd_linear_interpolate(&input_times, &input_values, target_simd);

		// Extract results (only take the actual chunk size)
		let result_array = result_simd.to_array();
		results.extend_from_slice(&result_array[..chunk_size]);
	}

	// Convert back to points
	let interpolated = results.into_iter().enumerate().map(|(i, value)| Point { timestamp: target_times[i], value: BigDecimal::from_f64(round_to_places(value, 10)).unwrap_or_else(BigDecimal::zero) }).collect();

	Ok(interpolated)
}

fn simd_linear_interpolate(input_times: &[f64], input_values: &[f64], target_times: f64x4) -> f64x4 {
	let target_times_array = target_times.to_array();
	let mut results = [0.0; 4];
	for i in 0..4 {
		let t = target_times_array[i];
		let idx = if t <= input_times[0] {
			0
		} else if t >= input_times[input_times.len() - 1] {
			input_times.len().saturating_sub(2)
		} else {
			input_times.partition_point(|&it| it < t).saturating_sub(1)
		};
		let t0 = input_times[idx];
		let t1 = input_times[idx + 1];
		let v0 = input_values[idx];
		let v1 = input_values[idx + 1];
		let result = if t <= input_times[0] {
			let slope = (v1 - v0) / (t1 - t0);
			v0 + slope * (t - t0)
		} else if t >= input_times[input_times.len() - 1] {
			let slope = (v1 - v0) / (t1 - t0);
			v1 + slope * (t - t1)
		} else {
			let alpha = if (t1 - t0).abs() < f64::EPSILON { 0.0 } else { (t - t0) / (t1 - t0) };
			v0 + alpha * (v1 - v0)
		};
		results[i] = round_to_places(result, 10);
	}
	f64x4::new(results) // Changed from f64x4::from_array to f64x4::new
}

fn round_to_places(value: f64, places: i32) -> f64 {
	let multiplier = 10.0_f64.powi(places);
	(value * multiplier).round() / multiplier
}
