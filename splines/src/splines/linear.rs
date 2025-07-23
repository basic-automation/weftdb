use std::{
	fs::File, io::{BufRead, BufReader, BufWriter, Write}
};

use anyhow::{Result, bail};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive, Zero};
use chrono::{DateTime, Utc};
use sysinfo::System;
use tempfile::NamedTempFile;
use wide::f64x4;

use super::{LinearSpline, SIMD_BATCH_SIZE};
use crate::{Error, Point, Resolution, Spline, TargetTimesIterator, is_uniformly_spaced};

/// Performs linear interpolation on points data.
///
/// Takes a vector of `points`, a start date/time, an end date/time, a `Resolution`,
/// and returns a vector of interpolated or extrapolated points using linear interpolation.
///
/// The `Resolution` is used to determine the time step for the interpolation or extrapolation.
///
/// # Errors
///
/// Returns an error if:
/// - points are empty or have fewer than 2 points
/// - points have inconsistent dataset IDs
/// - Invalid time range (start >= end)
/// - Timestamp conversion or `BigDecimal` operations fail
pub fn linear(points: &Vec<Point>, start: &DateTime<Utc>, end: &DateTime<Utc>, resolution: &Resolution) -> Result<Vec<Point>> {
	if points.is_empty() {
		bail!(Error::InsufficientPointsError);
	}

	if start >= end {
		bail!(Error::InvalidTimeRangeError);
	}

	if points.len() < Spline::Linear.number_of_points_required() {
		bail!(Error::InsufficientPointsError);
	}

	if points.len() == 2 {
		println!("Linear: Fast path for two-point interpolation");
		return linear_two_point_fast(points, start, end, resolution);
	}

	let mut sorted_points = points.clone();
	sorted_points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	if is_uniformly_spaced(&sorted_points, resolution) {
		println!("Linear: Fast path for uniformly spaced data");
		return linear_uniform_fast(&sorted_points, start, end, resolution);
	}

	println!("Linear: Using standard linear interpolation");

	let spline = LinearSpline::new(&sorted_points, resolution)?;
	let rounded_start = resolution.round(start)?;
	let rounded_end = resolution.round(end)?;

	let mut system = System::new_all();
	let mut result = Vec::new();
	let mut temp_file: Option<BufWriter<File>> = None;
	let mut temp_path: Option<tempfile::TempPath> = None; // Use TempPath to persist file

	let total_memory = system.total_memory(); // In bytes
	let memory_threshold = (total_memory as f64 * 0.8) as u64; // 80% of total memory
	let point_size = std::mem::size_of::<Point>() as u64; // ~24 bytes

	// Use TargetTimesIterator for memory-efficient time generation
	let time_iter = TargetTimesIterator::new(rounded_start, rounded_end, *resolution);
	if let Ok(estimated_points) = time_iter.estimate_len() {
		result.reserve(estimated_points);
		if estimated_points > 1_000_000 {
			// Force tempfile for very large datasets
			let temp = NamedTempFile::new().map_err(|e| Error::IOError(e.to_string()))?;
			temp_path = Some(temp.into_temp_path()); // Persist the file
			temp_file = Some(BufWriter::new(File::create(temp_path.as_ref().unwrap()).map_err(|e| Error::IOError(e.to_string()))?));
		}
	}

	for batch_times in time_iter {
		system.refresh_memory();
		let available_memory = system.available_memory();
		let batch_size = if available_memory < memory_threshold {
			let max_points = (available_memory / point_size).max(50) as usize;
			batch_times.len().min(max_points / 2) // Conservative scaling
		} else {
			batch_times.len()
		};

		let mut batch = Vec::with_capacity(batch_size);

		// Process only up to batch_size from the current batch
		for current_time in batch_times.into_iter().take(batch_size) {
			let value = spline.evaluate(&current_time)?;

			if available_memory < memory_threshold && temp_file.is_none() {
				let temp = NamedTempFile::new().map_err(|e| Error::IOError(e.to_string()))?;
				temp_path = Some(temp.into_temp_path()); // Persist the file
				temp_file = Some(BufWriter::new(File::create(temp_path.as_ref().unwrap()).map_err(|e| Error::IOError(e.to_string()))?));
			}

			if let Some(writer) = temp_file.as_mut() {
				// Write timestamp in RFC3339 format
				writeln!(writer, "{},{}", current_time.to_rfc3339(), value)?;
			} else {
				// Store in memory
				batch.push(Point { timestamp: current_time, value });
			}
		}

		if temp_file.is_none() {
			result.extend(batch);
		}
		if let Some(writer) = temp_file.as_mut() {
			writer.flush().map_err(|e| Error::IOError(e.to_string()))?;
		}
	}

	// Read back from tempfile if used
	if let Some(path) = &temp_path {
		let file = File::open(path).map_err(|e| Error::IOError(e.to_string()))?;
		let reader = BufReader::new(file);
		for line in reader.lines() {
			let line = line.map_err(|e| Error::IOError(e.to_string()))?;
			let parts: Vec<&str> = line.split(',').collect();
			if parts.len() == 2 {
				let timestamp = DateTime::parse_from_rfc3339(parts[0]).map_err(|e| Error::IOError(format!("Failed to parse timestamp '{}': {}", parts[0], e)))?.with_timezone(&Utc);
				let value = parts[1].parse::<f64>().map_err(|e| Error::IOError(format!("Failed to parse value '{}': {}", parts[1], e)))?;
				let value = BigDecimal::from_f64(value).unwrap_or_default();
				result.push(Point { timestamp, value });
			}
		}
	}

	Ok(result)
}

/// Fast path for uniformly spaced data
fn linear_uniform_fast(points: &[Point], start: &DateTime<Utc>, end: &DateTime<Utc>, resolution: &Resolution) -> Result<Vec<Point>> {
	let step = resolution.to_step();
	let mut result = Vec::new();

	// Pre-compute uniform interval - check for zero interval
	let uniform_interval = resolution.difference(&points[1].timestamp, &points[0].timestamp)?;

	// Handle case where points have identical or nearly identical timestamps
	if uniform_interval == 0 {
		// All points have the same timestamp - use constant value interpolation
		let constant_value = &points[0].value;
		let mut current_time = *start;

		while current_time <= *end {
			result.push(Point { timestamp: current_time, value: constant_value.clone() });
			current_time += step;
		}

		return Ok(result);
	}

	let uniform_interval = match BigDecimal::from_i64(uniform_interval) {
		Some(interval) => interval,
		None => bail!(Error::DecimalConversionError),
	};

	const BASE_BATCH_SIZE: usize = 100; // Smaller base batch size
	let mut system = System::new_all();
	let mut temp_file: Option<BufWriter<File>> = None;
	let mut temp_path: Option<tempfile::TempPath> = None; // Use TempPath to persist file

	let total_memory = system.total_memory(); // In bytes
	let memory_threshold = (total_memory as f64 * 0.8) as u64; // 80% of total memory
	let point_size = std::mem::size_of::<Point>() as u64; // ~24 bytes

	// Use TargetTimesIterator for memory-efficient time generation
	let rounded_start = resolution.round(start)?;
	let rounded_end = resolution.round(end)?;
	let time_iter = TargetTimesIterator::new(rounded_start, rounded_end, *resolution);
	if let Ok(estimated_points) = time_iter.estimate_len() {
		if estimated_points < BASE_BATCH_SIZE * 2 {
			// For small datasets, reserve memory and process directly
			result.reserve(estimated_points);
		} else if estimated_points > 1_000_000 {
			// Force tempfile for very large datasets
			let temp = NamedTempFile::new().map_err(|e| Error::IOError(e.to_string()))?;
			temp_path = Some(temp.into_temp_path()); // Persist the file
			temp_file = Some(BufWriter::new(File::create(temp_path.as_ref().unwrap()).map_err(|e| Error::IOError(e.to_string()))?));
		}
	}

	let data_start = points[0].timestamp;
	let data_end = points[points.len() - 1].timestamp;

	for batch_times in time_iter {
		system.refresh_memory();
		let available_memory = system.available_memory();
		let batch_size = if available_memory < memory_threshold {
			let max_points = (available_memory / point_size).max(50) as usize;
			batch_times.len().min(max_points / 2) // Conservative scaling
		} else {
			batch_times.len()
		};

		let mut batch = Vec::with_capacity(batch_size);

		// Process only up to batch_size from the current batch
		for current_time in batch_times.into_iter().take(batch_size) {
			let value = if current_time < data_start || current_time > data_end {
				// Extrapolation - use boundary segments
				if current_time < data_start {
					// Extrapolate backward using first segment
					let dt = resolution.difference(&current_time, &data_start)?;
					let dt = match BigDecimal::from_i64(dt) {
						Some(value) => value,
						None => bail!(Error::DecimalConversionError),
					};
					let slope = (&points[1].value - &points[0].value) / &uniform_interval;
					&points[0].value + slope * dt
				} else {
					// Extrapolate forward using last segment
					let n = points.len();
					let dt = resolution.difference(&current_time, &points[n - 1].timestamp)?;
					let dt = match BigDecimal::from_i64(dt) {
						Some(value) => value,
						None => bail!(Error::DecimalConversionError),
					};
					let slope = (&points[n - 1].value - &points[n - 2].value) / &uniform_interval;
					&points[n - 2].value + slope * dt
				}
			} else {
				// Interpolation - use uniform spacing optimization with safe casting
				let time_from_start = resolution.difference(&current_time, &data_start)?;
				let segment_index = if time_from_start >= 0 && uniform_interval > BigDecimal::zero() { (time_from_start / uniform_interval.clone()).to_usize().unwrap_or(0).min(points.len().saturating_sub(2)) } else { 0 };
				let segment_start_time = points[segment_index].timestamp;
				let dt = resolution.difference(&current_time, &segment_start_time)?;
				let slope = (&points[segment_index + 1].value - &points[segment_index].value) / &uniform_interval;
				&points[segment_index].value + slope * dt
			};

			if available_memory < memory_threshold && temp_file.is_none() {
				let temp = NamedTempFile::new().map_err(|e| Error::IOError(e.to_string()))?;
				temp_path = Some(temp.into_temp_path()); // Persist the file
				temp_file = Some(BufWriter::new(File::create(temp_path.as_ref().unwrap()).map_err(|e| Error::IOError(e.to_string()))?));
			}

			if let Some(writer) = temp_file.as_mut() {
				// Write timestamp in RFC3339 format
				writeln!(writer, "{},{}", current_time.to_rfc3339(), value)?;
			} else {
				// Store in memory
				batch.push(Point { timestamp: current_time, value });
			}
		}

		if temp_file.is_none() {
			result.extend(batch);
		}
		if let Some(writer) = temp_file.as_mut() {
			writer.flush().map_err(|e| Error::IOError(e.to_string()))?;
		}
	}

	// Read back from tempfile if used
	if let Some(path) = &temp_path {
		let file = File::open(path).map_err(|e| Error::IOError(e.to_string()))?;
		let reader = BufReader::new(file);
		for line in reader.lines() {
			let line = line.map_err(|e| Error::IOError(e.to_string()))?;
			let parts: Vec<&str> = line.split(',').collect();
			if parts.len() == 2 {
				let timestamp = DateTime::parse_from_rfc3339(parts[0]).map_err(|e| Error::IOError(format!("Failed to parse timestamp '{}': {}", parts[0], e)))?.with_timezone(&Utc);
				let value = parts[1].parse::<f64>().map_err(|e| Error::IOError(format!("Failed to parse value '{}': {}", parts[1], e)))?;
				let value = BigDecimal::from_f64(value).unwrap_or_default();
				result.push(Point { timestamp, value });
			}
		}
	}

	Ok(result)
}

/// Optimized two-point linear interpolation
fn linear_two_point_fast(points: &[Point], start: &DateTime<Utc>, end: &DateTime<Utc>, resolution: &Resolution) -> Result<Vec<Point>> {
	let mut result = Vec::new();

	// Check for identical timestamps first
	let dt = BigDecimal::from_i64(resolution.difference(&points[1].timestamp, &points[0].timestamp)?).ok_or(Error::DecimalConversionError)?;

	if dt == BigDecimal::zero() {
		// Handle identical timestamps - use constant value interpolation
		let constant_value = &points[0].value;
		let rounded_start = resolution.round(start)?;
		let rounded_end = resolution.round(end)?;
		let time_iter = TargetTimesIterator::new(rounded_start, rounded_end, *resolution);

		for batch_times in time_iter {
			for current_time in batch_times {
				result.push(Point { timestamp: current_time, value: constant_value.clone() });
			}
		}

		return Ok(result);
	}

	// Pre-compute slope once (safe now that we know dt != 0)
	let dy = &points[1].value - &points[0].value;
	let slope = dy / dt;
	let base_value = &points[0].value;
	let base_time = points[0].timestamp;

	const BASE_BATCH_SIZE: usize = 100; // Smaller base batch size
	let mut system = System::new_all();
	let mut temp_file: Option<BufWriter<File>> = None;
	let mut temp_path: Option<tempfile::TempPath> = None; // Use TempPath to persist file

	let total_memory = system.total_memory(); // In bytes
	let memory_threshold = (total_memory as f64 * 0.8) as u64; // 80% of total memory
	let point_size = std::mem::size_of::<Point>() as u64; // ~24 bytes

	// Use TargetTimesIterator for memory-efficient time generation
	let rounded_start = resolution.round(start)?;
	let rounded_end = resolution.round(end)?;
	let time_iter = TargetTimesIterator::new(rounded_start, rounded_end, *resolution);
	if let Ok(estimated_points) = time_iter.estimate_len() {
		if estimated_points < BASE_BATCH_SIZE * 2 {
			// For small datasets, reserve memory and process directly
			result.reserve(estimated_points);
		} else if estimated_points > 1_000_000 {
			// Force tempfile for very large datasets
			let temp = NamedTempFile::new().map_err(|e| Error::IOError(e.to_string()))?;
			temp_path = Some(temp.into_temp_path()); // Persist the file
			temp_file = Some(BufWriter::new(File::create(temp_path.as_ref().unwrap()).map_err(|e| Error::IOError(e.to_string()))?));
		}
	}

	for batch_times in time_iter {
		system.refresh_memory();
		let available_memory = system.available_memory();
		let batch_size = if available_memory < memory_threshold {
			let max_points = (available_memory / point_size).max(50) as usize;
			batch_times.len().min(max_points / 2) // Conservative scaling
		} else {
			batch_times.len()
		};

		let mut batch = Vec::with_capacity(batch_size);

		// Process only up to batch_size from the current batch
		for current_time in batch_times.into_iter().take(batch_size) {
			let time_diff = BigDecimal::from_i64(resolution.difference(&current_time, &base_time)?).ok_or(Error::DecimalConversionError)?;
			let value = base_value + &slope * time_diff;

			if available_memory < memory_threshold && temp_file.is_none() {
				let temp = NamedTempFile::new().map_err(|e| Error::IOError(e.to_string()))?;
				temp_path = Some(temp.into_temp_path()); // Persist the file
				temp_file = Some(BufWriter::new(File::create(temp_path.as_ref().unwrap()).map_err(|e| Error::IOError(e.to_string()))?));
			}

			if let Some(writer) = temp_file.as_mut() {
				// Write timestamp in RFC3339 format
				writeln!(writer, "{},{}", current_time.to_rfc3339(), value)?;
			} else {
				// Store in memory
				batch.push(Point { timestamp: current_time, value });
			}
		}

		if temp_file.is_none() {
			result.extend(batch);
		}
		if let Some(writer) = temp_file.as_mut() {
			writer.flush().map_err(|e| Error::IOError(e.to_string()))?;
		}
	}

	// Read back from tempfile if used
	if let Some(path) = &temp_path {
		let file = File::open(path).map_err(|e| Error::IOError(e.to_string()))?;
		let reader = BufReader::new(file);
		for line in reader.lines() {
			let line = line.map_err(|e| Error::IOError(e.to_string()))?;
			let parts: Vec<&str> = line.split(',').collect();
			if parts.len() == 2 {
				let timestamp = DateTime::parse_from_rfc3339(parts[0]).map_err(|e| Error::IOError(format!("Failed to parse timestamp '{}': {}", parts[0], e)))?.with_timezone(&Utc);
				let value = parts[1].parse::<f64>().map_err(|e| Error::IOError(format!("Failed to parse value '{}': {}", parts[1], e)))?;
				let value = BigDecimal::from_f64(value).unwrap_or_default();
				result.push(Point { timestamp, value });
			}
		}
	}

	Ok(result)
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
	let input_values: Vec<f64> = points.iter().map(|m| m.value.to_f64().unwrap_or(0.0)).collect();

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
	let interpolated = results.into_iter().enumerate().map(|(i, value)| Point { timestamp: target_times[i], value: BigDecimal::from_f64(value).unwrap_or_else(BigDecimal::zero) }).collect();

	Ok(interpolated)
}

/// SIMD linear interpolation core function
fn simd_linear_interpolate(input_times: &[f64], input_values: &[f64], target_times: f64x4) -> f64x4 {
	// Find the segment index for each of the 4 target times.
	// `partition_point` is faster than a linear scan, returning the index
	// of the first element `x` for which `f(x)` is false.
	// We subtract 1 to get the index of the start of the segment.
	let indices: [usize; 4] = target_times.to_array().map(|t| {
		let idx = input_times.partition_point(|&it| it < t);
		// Handle extrapolation cases properly
		if idx == 0 {
			0 // Before first point - use first segment
		} else if idx >= input_times.len() {
			input_times.len().saturating_sub(2) // After last point - use last segment
		} else {
			idx.saturating_sub(1) // Normal case - use segment before the partition point
		}
	});

	// Gather values from the input slices based on the found indices.
	// This loads the start and end points of the segments for all 4 lanes.
	let t0 = f64x4::new([input_times[indices[0]], input_times[indices[1]], input_times[indices[2]], input_times[indices[3]]]);
	let t1 = f64x4::new([input_times[indices[0] + 1], input_times[indices[1] + 1], input_times[indices[2] + 1], input_times[indices[3] + 1]]);
	let v0 = f64x4::new([input_values[indices[0]], input_values[indices[1]], input_values[indices[2]], input_values[indices[3]]]);
	let v1 = f64x4::new([input_values[indices[0] + 1], input_values[indices[1] + 1], input_values[indices[2] + 1], input_values[indices[3] + 1]]);

	// Perform linear interpolation using SIMD operations.
	// alpha = (target - t0) / (t1 - t0)
	let alpha = (target_times - t0) / (t1 - t0);

	// result = v0 + alpha * (v1 - v0)
	// Using mul_add for a potential fused multiply-add (FMA) optimization.
	alpha.mul_add(v1 - v0, v0)
}
