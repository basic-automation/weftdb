use std::{
	fs::File, io::{BufRead, BufReader, BufWriter, Write}
};

use anyhow::bail;
use bigdecimal::{BigDecimal, FromPrimitive};
use chrono::{DateTime, Utc};
pub use helpers::{convert_datetimes_to_gpu_format_f32, convert_datetimes_to_gpu_format_f64, convert_gpu_results_to_points_f32, convert_gpu_results_to_points_f64, convert_points_to_gpu_format_f32, convert_points_to_gpu_format_f64, get_max_buffer_size};
use sysinfo::System;
use tempfile::NamedTempFile;
pub use types::{GpuInterpolator, Method};

use crate::{Error, Point, Resolution, Result, Spline, TargetTimesIterator};

mod helpers;
mod shaders;
mod types;

// Use a conservative GPU buffer limit to account for all buffers
const F64_SIZE: usize = std::mem::size_of::<f64>(); // 8 bytes
const F32_SIZE: usize = std::mem::size_of::<f32>(); // 4 bytes
const NUMBER_OF_BUFFERS: usize = 4; // Input times, input values, target times, results

/// GPU-accelerated interpolation with nanosecond precision support and automatic batching
pub async fn gpu_interpolate(points: Vec<Point>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline: Spline) -> Result<Vec<Point>> {
	if points.is_empty() {
		return Ok(Vec::new());
	}

	// Convert spline type to GPU method
	let method = match spline {
		Spline::Linear => Method::Linear,
		Spline::Quadratic => Method::Quadratic,
		Spline::Cubic => Method::Cubic,
		Spline::Polynomial(degree, _) => Method::Polynomial(degree),
	};

	// Create GPU interpolator
	let mut interpolator = GpuInterpolator::new().await?;

	if interpolator.supports_f64() { gpu_interpolate_f64(&points, &start, &end, &resolution, &method, &mut interpolator).await } else { gpu_interpolate_f32(&points, &start, &end, &resolution, &method, &mut interpolator).await }
}

pub async fn gpu_interpolate_f64(points: &[Point], start: &DateTime<Utc>, end: &DateTime<Utc>, resolution: &Resolution, method: &Method, interpolator: &mut GpuInterpolator) -> Result<Vec<Point>> {
	if points.len() < 2 {
		bail!(Error::InsufficientPointsError);
	}

	if start >= end {
		bail!(Error::InvalidTimeRangeError);
	}

	const BASE_BATCH_SIZE: usize = 100; // Smaller base batch size
	let mut system = System::new_all();
	let mut result = Vec::new();
	let mut temp_file: Option<BufWriter<File>> = None;
	let mut temp_path: Option<tempfile::TempPath> = None; // Use TempPath to persist file

	let total_memory = system.total_memory(); // In bytes
	let memory_threshold = (total_memory as f64 * 0.8) as u64; // 80% of total memory
	let point_size = std::mem::size_of::<Point>() as u64; // ~24 bytes

	let element_size = F64_SIZE;
	let (input_times, input_values) = convert_points_to_gpu_format_f64(&points, resolution)?;
	let base_time = points[0].timestamp;

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

		// Convert target times to GPU format
		let target_times_gpu = convert_datetimes_to_gpu_format_f64(&batch_times[..batch_size], resolution, base_time)?;

		// Calculate conservative batch size for GPU
		let max_single_buffer_size = get_max_buffer_size().await? as usize / NUMBER_OF_BUFFERS / element_size;
		let max_targets_per_batch = (max_single_buffer_size / element_size).min(target_times_gpu.len());

		for target_batch in target_times_gpu.chunks(max_targets_per_batch) {
			// Perform interpolation
			let batch_results: Vec<f64> = interpolator.interpolate_f64(&input_times, &input_values, target_batch, method)?;

			// Convert GPU results to points
			let points = convert_gpu_results_to_points_f64(batch_results, &batch_times[..batch_size.min(target_batch.len())])?;

			if available_memory < memory_threshold && temp_file.is_none() {
				let temp = NamedTempFile::new().map_err(|e| Error::IOError(e.to_string()))?;
				temp_path = Some(temp.into_temp_path()); // Persist the file
				temp_file = Some(BufWriter::new(File::create(temp_path.as_ref().unwrap()).map_err(|e| Error::IOError(e.to_string()))?));
			}

			if let Some(writer) = temp_file.as_mut() {
				// Write timestamp in RFC3339 format
				for point in points {
					writeln!(writer, "{},{}", point.timestamp.to_rfc3339(), point.value)?;
				}
			} else {
				// Store in memory
				batch.extend(points);
			}
		}

		if temp_file.is_none() {
			result.extend(batch.into_iter());
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

pub async fn gpu_interpolate_f32(points: &[Point], start: &DateTime<Utc>, end: &DateTime<Utc>, resolution: &Resolution, method: &Method, interpolator: &mut GpuInterpolator) -> Result<Vec<Point>> {
	if points.len() < 2 {
		bail!(Error::InsufficientPointsError);
	}

	if start >= end {
		bail!(Error::InvalidTimeRangeError);
	}

	const BASE_BATCH_SIZE: usize = 100; // Smaller base batch size
	let mut system = System::new_all();
	let mut result = Vec::new();
	let mut temp_file: Option<BufWriter<File>> = None;
	let mut temp_path: Option<tempfile::TempPath> = None; // Use TempPath to persist file

	let total_memory = system.total_memory(); // In bytes
	let memory_threshold = (total_memory as f64 * 0.8) as u64; // 80% of total memory
	let point_size = std::mem::size_of::<Point>() as u64; // ~24 bytes

	let element_size = F32_SIZE;
	let (input_times, input_values) = convert_points_to_gpu_format_f32(&points, resolution)?;
	let base_time = points[0].timestamp;

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

		// Convert target times to GPU format
		let target_times_gpu = convert_datetimes_to_gpu_format_f32(&batch_times[..batch_size], resolution, base_time)?;

		// Calculate conservative batch size for GPU
		let max_single_buffer_size = get_max_buffer_size().await? as usize / NUMBER_OF_BUFFERS / element_size;
		let max_targets_per_batch = (max_single_buffer_size / element_size).min(target_times_gpu.len());

		for target_batch in target_times_gpu.chunks(max_targets_per_batch) {
			// Perform interpolation
			let batch_results: Vec<f32> = interpolator.interpolate_f32(&input_times, &input_values, target_batch, method)?;

			// Convert GPU results to points
			let points = convert_gpu_results_to_points_f32(batch_results, &batch_times[..batch_size.min(target_batch.len())])?;

			if available_memory < memory_threshold && temp_file.is_none() {
				let temp = NamedTempFile::new().map_err(|e| Error::IOError(e.to_string()))?;
				temp_path = Some(temp.into_temp_path()); // Persist the file
				temp_file = Some(BufWriter::new(File::create(temp_path.as_ref().unwrap()).map_err(|e| Error::IOError(e.to_string()))?));
			}

			if let Some(writer) = temp_file.as_mut() {
				// Write timestamp in RFC3339 format
				for point in points {
					writeln!(writer, "{},{}", point.timestamp.to_rfc3339(), point.value)?;
				}
			} else {
				// Store in memory
				batch.extend(points);
			}
		}

		if temp_file.is_none() {
			result.extend(batch.into_iter());
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
