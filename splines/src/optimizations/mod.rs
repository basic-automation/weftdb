use std::{
	fs::File, io::{BufRead, BufReader, BufWriter, Write}
};

use anyhow::{Result, bail};
use bigdecimal::{BigDecimal, FromPrimitive};
use chrono::{DateTime, Utc};
pub use fast_path::apply_fast_path;
use rayon::prelude::*;
use sysinfo::System;
use tempfile::NamedTempFile;

use super::TargetTimesIterator;
use crate::{Error, Point, Resolution, Spline, cubic, cubic_simd, generate_target_times, linear, linear_simd, polynomial, polynomial_simd, quadratic, quadratic_simd};

mod fast_path;

/// Threshold for switching to parallel processing
//const PARALLEL_THRESHOLD: usize = 1000; // ← Lower threshold based on your results
const SIMD_THRESHOLD: usize = 200; // ← Adjust based on SIMD performance
const SIMD_THRESHOLD_PLUS_ONE: usize = SIMD_THRESHOLD + 1; // For SIMD, we need at least one more than the threshold

/// Optimized interpolation with intelligent algorithm selection
///
/// This function provides the highest level of optimization by automatically
/// selecting the best interpolation strategy based on data characteristics.
///
/// # Errors
///
/// Returns an error if the underlying interpolation algorithm fails
pub async fn cpu_interpolate(points: Vec<Point>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline: Spline) -> Result<Vec<Point>> {
	let input_count = points.len();
	let target_times = generate_target_times(start, end, resolution);
	let output_count = target_times.len();

	match spline {
		Spline::Linear => {
			match (input_count, output_count) {
				(input, output) if input < SIMD_THRESHOLD && output < SIMD_THRESHOLD_PLUS_ONE => {
					// Use scalar linear interpolation
					linear(&points, &start, &end, &resolution)
				}
				_ => {
					// Use SIMD linear interpolation
					parallel_interpolate(&points, &start, &end, spline, resolution)
				}
			}
		}
		Spline::Quadratic => {
			match (input_count, output_count) {
				(input, output) if input < SIMD_THRESHOLD && output < SIMD_THRESHOLD_PLUS_ONE => {
					// Use scalar quadratic interpolation
					quadratic(points, start, end, resolution)
				}
				_ => {
					// Use SIMD quadratic interpolation
					parallel_interpolate(&points, &start, &end, spline, resolution)
				}
			}
		}
		Spline::Cubic => {
			match (input_count, output_count) {
				(input, output) if input < SIMD_THRESHOLD && output < SIMD_THRESHOLD_PLUS_ONE => {
					// Use scalar cubic interpolation
					cubic(points, start, end, resolution)
				}
				_ => {
					// Use SIMD cubic interpolation
					parallel_interpolate(&points, &start, &end, spline, resolution)
				}
			}
		}
		Spline::Polynomial(degree, bounds_factor) => {
			match (input_count, output_count) {
				(input, output) if input < SIMD_THRESHOLD && output < SIMD_THRESHOLD_PLUS_ONE => {
					// Use scalar polynomial interpolation
					polynomial(points, start, end, resolution, degree, bounds_factor)
				}
				_ => {
					// Use SIMD polynomial interpolation
					parallel_interpolate(&points, &start, &end, spline, resolution)
				}
			}
		}
	}
}

/// Enhanced SIMD interpolation with parallel processing for very large datasets
///
/// # Errors
///
/// Returns an error if the underlying SIMD interpolation fails
pub fn parallel_interpolate(points: &[Point], start: &DateTime<Utc>, end: &DateTime<Utc>, spline: Spline, resolution: Resolution) -> Result<Vec<Point>> {
	if points.len() < 2 {
		bail!(Error::InsufficientPointsError);
	}

	if start >= end {
		bail!(Error::InvalidTimeRangeError);
	}

	let mut system = System::new_all();
	let mut result = Vec::new();
	let mut temp_file: Option<BufWriter<File>> = None;
	let mut temp_path: Option<tempfile::TempPath> = None; // Use TempPath to persist file

	let total_memory = system.total_memory(); // In bytes
	let memory_threshold = (total_memory as f64 * 0.8) as u64; // 80% of total memory
	let point_size = std::mem::size_of::<Point>() as u64; // ~24 bytes

	// Use TargetTimesIterator for memory-efficient time generation
	let rounded_start = resolution.round(start)?;
	let rounded_end = resolution.round(end)?;
	let time_iter = TargetTimesIterator::new(rounded_start, rounded_end, resolution);
	if let Ok(estimated_points) = time_iter.estimate_len() {
		result.reserve(estimated_points);
		if estimated_points > 1_000_000 {
			// Force tempfile for very large datasets
			let temp = NamedTempFile::new().map_err(|e| Error::IOError(e.to_string()))?;
			temp_path = Some(temp.into_temp_path()); // Persist the file
			temp_file = Some(BufWriter::new(File::create(temp_path.as_ref().unwrap()).map_err(|e| Error::IOError(e.to_string()))?));
		}
	}

	// Split target times into chunks and process in parallel
	let chunk_size = num_cpus::get() * 4; // Adjust chunk size based on available CPUs
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
		let res: Result<Vec<Vec<Point>>> = batch_times
			.into_iter()
			.take(batch_size)
			.collect::<Vec<_>>()
			.par_chunks(chunk_size)
			.map(|chunk| match spline {
				Spline::Linear => linear_simd(points, chunk, resolution),
				Spline::Quadratic => quadratic_simd(points, chunk, resolution),
				Spline::Cubic => cubic_simd(points, chunk, resolution),
				Spline::Polynomial(degree, bounds_factor) => polynomial_simd(points, chunk, resolution, degree, bounds_factor),
			})
			.collect();

		let chunk_results = res.map_err(|e| e)?;

		for points in chunk_results {
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
			} else {
			}
		}
	}

	Ok(result)
}
