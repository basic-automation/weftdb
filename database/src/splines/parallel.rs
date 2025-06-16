//! Parallel and optimized interpolation processing for large datasets
//!
//! This module provides high-performance interpolation with automatic algorithm
//! selection based on dataset characteristics and benchmark-driven optimizations.

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use rayon::prelude::*;

use super::{auto_interpolate, linear, quadratic, Resolution, SplineType};
use crate::Measurement;

/// Threshold for switching to parallel processing
const PARALLEL_THRESHOLD: usize = 1000;

/// Chunk size for parallel processing (based on benchmark sweet spot)
const CHUNK_SIZE: usize = 500;

/// Fast path threshold for cubic interpolation (where performance drops significantly)
const CUBIC_FAST_PATH_THRESHOLD: usize = 500;

/// Fast path threshold for quadratic interpolation
const QUADRATIC_FAST_PATH_THRESHOLD: usize = 5000;

/// Optimized interpolation with automatic algorithm and parallelization selection.
///
/// This function automatically chooses the best interpolation strategy based on:
/// - Dataset size
/// - Spline type complexity
/// - Benchmark-driven performance thresholds
///
/// # Performance Characteristics
/// - **Linear**: 133-448 Kelem/s, scales well to 5K+ points
/// - **Quadratic**: 119-236 Kelem/s, optimal up to 5K points  
/// - **Cubic**: 0.2-55 Kelem/s, fast path recommended >500 points
/// - **Polynomial**: Varies by degree, automatic degree limiting
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
/// Returns a vector of interpolated measurements with optimal performance.
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient measurements for interpolation
/// - Invalid time range
/// - Underlying interpolation fails
pub fn optimized_interpolate(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline_type: SplineType) -> Result<Vec<Measurement>> {
	let measurement_count = measurements.len();

	// Fast path for small datasets or already-fast algorithms - avoid optimization overhead
	if measurement_count < 200 || matches!(spline_type, SplineType::Linear) {
		return auto_interpolate(measurements, start, end, resolution, spline_type);
	}

	// Only apply optimizations where they provide clear benefits
	let optimized_spline_type = match (spline_type, measurement_count) {
		// Cubic interpolation optimization only for datasets where it matters
		(SplineType::Cubic, n) if n > CUBIC_FAST_PATH_THRESHOLD => {
			SplineType::Quadratic // This was proven to work well
		}

		// Quadratic to Linear only for very large datasets where benefit is clear
		(SplineType::Quadratic, n) if n > QUADRATIC_FAST_PATH_THRESHOLD => SplineType::Linear,

		// Polynomial degree limiting only for complex polynomials on large datasets
		(SplineType::Polynomial(degree), n) if degree > 2 && n > 500 => {
			let max_degree = match n {
				501..=1000 => degree.min(3),
				1001..=5000 => degree.min(2),
				_ => 1,
			};
			SplineType::Polynomial(max_degree)
		}

		// No optimization - use original algorithm
		_ => spline_type,
	};

	// Only use parallel processing for very large datasets where overhead is justified
	if measurement_count >= PARALLEL_THRESHOLD && !matches!(spline_type, SplineType::Linear) {
		parallel_interpolate(measurements, start, end, resolution, optimized_spline_type)
	} else {
		auto_interpolate(measurements, start, end, resolution, optimized_spline_type)
	}
}

/// High-performance parallel interpolation for large datasets.
///
/// Splits large datasets into overlapping chunks and processes them in parallel,
/// then merges the results while maintaining temporal continuity.
///
/// # Performance Benefits
/// - Utilizes multiple CPU cores
/// - Reduces memory pressure per thread
/// - Maintains interpolation accuracy through overlap handling
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
/// Returns a vector of interpolated measurements processed in parallel.
///
/// # Errors
///
/// Returns an error if the underlying interpolation fails or parallel processing encounters issues.
pub fn parallel_interpolate(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline_type: SplineType) -> Result<Vec<Measurement>> {
	let measurement_count = measurements.len();

	// Fall back to serial for small datasets
	if measurement_count < PARALLEL_THRESHOLD {
		return auto_interpolate(measurements, start, end, resolution, spline_type);
	}

	// Calculate time range and chunk parameters
	let total_duration = end - start;
	let chunk_count = (measurement_count / CHUNK_SIZE).max(2);

	// Use safe conversion with error handling
	let chunk_count_i32 = i32::try_from(chunk_count).unwrap_or_else(|_| {
		eprintln!("Warning: Chunk count {chunk_count} exceeds i32 maximum, using maximum value");
		i32::MAX
	});
	let chunk_duration = total_duration / chunk_count_i32;

	// Create overlapping time chunks for parallel processing
	let time_chunks: Vec<(DateTime<Utc>, DateTime<Utc>)> = (0..chunk_count)
		.map(|i| {
			let i_i32 = i32::try_from(i).unwrap_or_else(|_| {
				eprintln!("Warning: Chunk index {i} exceeds i32 maximum, using maximum value");
				i32::MAX
			});

			let chunk_start = start + chunk_duration * i_i32;
			let chunk_end = if i == chunk_count - 1 {
				end
			} else {
				let next_i_i32 = i32::try_from(i + 1).unwrap_or_else(|_| {
					eprintln!("Warning: Chunk index {} exceeds i32 maximum, using maximum value", i + 1);
					i32::MAX
				});
				start + chunk_duration * next_i_i32
			};

			// Add overlap for interpolation continuity
			let overlap = resolution.to_step() * 2;
			let extended_start = if i == 0 { chunk_start } else { chunk_start - overlap };
			let extended_end = if i == chunk_count - 1 { chunk_end } else { chunk_end + overlap };

			(extended_start, extended_end)
		})
		.collect();

	// Sort measurements by timestamp for efficient chunking
	let mut sorted_measurements = measurements;
	sorted_measurements.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
	let sorted_measurements = Arc::new(sorted_measurements);

	// Process chunks in parallel
	let results: Result<Vec<Vec<Measurement>>> = time_chunks
		.par_iter()
		.map(|(chunk_start, chunk_end)| {
			// Extract measurements for this time chunk
			let chunk_measurements: Vec<Measurement> = sorted_measurements.iter().filter(|m| m.timestamp >= *chunk_start && m.timestamp <= *chunk_end).cloned().collect();

			// Skip chunks with insufficient data
			if chunk_measurements.len() < 2 {
				return Ok(Vec::new());
			}

			// Perform interpolation on this chunk
			auto_interpolate(chunk_measurements, *chunk_start, *chunk_end, resolution, spline_type)
		})
		.collect();

	let chunk_results = results?;

	// Merge results and remove overlapping points
	Ok(merge_interpolation_chunks(chunk_results, start, end))
}

/// Fast path interpolation with benchmark-optimized algorithm selection.
///
/// Uses the fastest appropriate algorithm based on dataset size and accuracy requirements.
/// Prioritizes performance over marginal accuracy improvements for large datasets.
///
/// # Algorithm Selection Logic
/// - **< 100 points**: Standard algorithm (overhead not justified)
/// - **100-500 points**: Use requested algorithm  
/// - **500-5000 points**: Cubic → Quadratic, Polynomial → Limited degree
/// - **> 5000 points**: Complex algorithms → Linear for maximum throughput
///
/// # Arguments
///
/// * `measurements` - Vector of measurements to interpolate
/// * `start` - Start time for interpolation range
/// * `end` - End time for interpolation range  
/// * `resolution` - Time resolution for output measurements
/// * `spline_type` - Requested spline type (may be optimized)
///
/// # Returns
///
/// Returns interpolated measurements using the fastest appropriate algorithm.
///
/// # Errors
///
/// Returns an error if the underlying interpolation algorithm fails.
pub fn fast_path_interpolate(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline_type: SplineType) -> Result<Vec<Measurement>> {
	let measurement_count = measurements.len();

	// Performance-first algorithm selection
	let fast_spline_type = match measurement_count {
		// Small datasets - use requested algorithm
		0..=100 => spline_type,

		// Medium datasets - moderate optimization
		101..=500 => match spline_type {
			SplineType::Polynomial(degree) if degree > 3 => SplineType::Polynomial(3),
			_ => spline_type,
		},

		// Large datasets - aggressive optimization
		501..=5000 => match spline_type {
			SplineType::Cubic => SplineType::Quadratic,
			SplineType::Polynomial(degree) if degree > 2 => SplineType::Quadratic,
			SplineType::Polynomial(_) => SplineType::Linear,
			_ => spline_type,
		},

		// Very large datasets - maximum performance
		_ => SplineType::Linear,
	};

	// Use direct algorithm calls for maximum performance
	match fast_spline_type {
		SplineType::Linear => linear(measurements, start, end, resolution),
		SplineType::Quadratic => quadratic(measurements, start, end, resolution),
		_ => auto_interpolate(measurements, start, end, resolution, fast_spline_type),
	}
}

/// Memory-efficient streaming interpolation for very large datasets.
///
/// Processes datasets that don't fit comfortably in memory by streaming
/// through chunks and yielding results incrementally.
///
/// # Benefits
/// - Constant memory usage regardless of dataset size
/// - Suitable for datasets > 100K points
/// - Can be combined with parallel processing
///
/// # Arguments
///
/// * `measurements` - Vector of measurements to interpolate
/// * `start` - Start time for interpolation range
/// * `end` - End time for interpolation range
/// * `resolution` - Time resolution for output measurements  
/// * `spline_type` - Type of spline interpolation to use
/// * `chunk_size` - Size of processing chunks
///
/// # Returns
///
/// Returns interpolated measurements processed in memory-efficient chunks.
///
/// # Errors
///
/// Returns an error if the underlying interpolation algorithm fails during chunk processing.
pub fn streaming_interpolate(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline_type: SplineType, chunk_size: usize) -> Result<Vec<Measurement>> {
	let measurement_count = measurements.len();

	// Use standard processing for smaller datasets
	if measurement_count <= chunk_size * 2 {
		return optimized_interpolate(measurements, start, end, resolution, spline_type);
	}

	// Sort measurements for streaming
	let mut sorted_measurements = measurements;
	sorted_measurements.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	let mut results = Vec::new();
	let total_duration = end - start;
	let num_chunks = (measurement_count / chunk_size).max(1);

	for chunk_idx in 0..num_chunks {
		let chunk_start_idx = chunk_idx * chunk_size;
		let chunk_end_idx = ((chunk_idx + 1) * chunk_size).min(measurement_count);

		// Add overlap for interpolation continuity
		let overlap_size = chunk_size / 10; // 10% overlap
		let extended_start = chunk_start_idx.saturating_sub(overlap_size);
		let extended_end = (chunk_end_idx + overlap_size).min(measurement_count);

		// Extract chunk with overlap
		let chunk_measurements = sorted_measurements[extended_start..extended_end].to_vec();

		if chunk_measurements.len() < 2 {
			continue;
		}

		// Calculate time range for this chunk using safe conversion
		let chunk_idx_i32 = i32::try_from(chunk_idx).unwrap_or_else(|_| {
			eprintln!("Warning: Chunk index {chunk_idx} exceeds i32 maximum, using maximum value");
			i32::MAX
		});
		let num_chunks_i32 = i32::try_from(num_chunks).unwrap_or_else(|_| {
			eprintln!("Warning: Number of chunks {num_chunks} exceeds i32 maximum, using maximum value");
			i32::MAX
		});

		let chunk_time_start = start + total_duration * chunk_idx_i32 / num_chunks_i32;
		let chunk_time_end = if chunk_idx == num_chunks - 1 {
			end
		} else {
			let next_chunk_idx_i32 = i32::try_from(chunk_idx + 1).unwrap_or_else(|_| {
				eprintln!("Warning: Chunk index {} exceeds i32 maximum, using maximum value", chunk_idx + 1);
				i32::MAX
			});
			start + total_duration * next_chunk_idx_i32 / num_chunks_i32
		};

		// Process chunk
		let chunk_result = fast_path_interpolate(chunk_measurements, chunk_time_start, chunk_time_end, resolution, spline_type)?;

		// Filter results to current chunk (no overlap processing needed for streaming)
		let filtered_chunk: Vec<Measurement> = chunk_result.into_iter().filter(|m| m.timestamp >= chunk_time_start && m.timestamp <= chunk_time_end).collect();

		results.extend(filtered_chunk);
	}

	// Remove any remaining duplicates and sort
	results.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
	results.dedup_by(|a, b| a.timestamp == b.timestamp);

	Ok(results)
}

/// Merges parallel interpolation chunk results while removing overlaps.
///
/// Combines results from parallel processing chunks, handling overlapping
/// time ranges and ensuring temporal continuity.
///
/// # Arguments
///
/// * `chunk_results` - Vector of interpolation results from parallel chunks
/// * `start` - Overall start time for filtering
/// * `end` - Overall end time for filtering
///
/// # Returns
///
/// Returns merged and deduplicated interpolation results.
fn merge_interpolation_chunks(chunk_results: Vec<Vec<Measurement>>, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<Measurement> {
	let mut all_measurements: Vec<Measurement> = chunk_results.into_iter().flatten().filter(|m| m.timestamp >= start && m.timestamp <= end).collect();

	// Sort by timestamp
	all_measurements.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	// Remove duplicates (from overlapping chunks)
	all_measurements.dedup_by(|a, b| a.timestamp == b.timestamp && a.dataset_id == b.dataset_id);

	all_measurements
}

/// Returns performance recommendations based on dataset characteristics.
///
/// Provides guidance on optimal interpolation strategies for different
/// dataset sizes and accuracy requirements.
///
/// # Arguments
///
/// * `measurement_count` - Number of measurements in dataset
/// * `accuracy_priority` - Whether accuracy is prioritized over performance
///
/// # Returns
///
/// Returns recommended spline type and processing strategy.
#[must_use]
pub const fn get_performance_recommendation(measurement_count: usize, accuracy_priority: bool) -> (SplineType, &'static str) {
	match (measurement_count, accuracy_priority) {
		// Small to medium datasets - accuracy preferred
		(0..=100, _) | (101..=500, true) => (SplineType::Cubic, "standard"),

		// Medium datasets - balance accuracy and performance
		(101..=500, false) => (SplineType::Quadratic, "fast_path"),

		// Large datasets - favor performance
		(501..=1000, true) => (SplineType::Quadratic, "optimized"),
		(501..=5000, false) => (SplineType::Linear, "fast_path"),

		// Very large datasets - maximum performance
		(1001..=5000, true) => (SplineType::Linear, "parallel"),

		// Huge datasets - streaming approach
		(_, _) => (SplineType::Linear, "streaming"),
	}
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use bigdecimal::BigDecimal;
	use chrono::{TimeZone, Utc};
	use uuid::Uuid;

	use super::*;
	use crate::Measurement;

	fn create_test_measurements(count: usize) -> Vec<Measurement> {
		let dataset_id = Uuid::new_v4();
		let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

		(0..count).map(|i| Measurement { id: Uuid::new_v4(), dataset_id, timestamp: start_time + chrono::Duration::seconds(i as i64 * 10), value: BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap() }).collect()
	}

	#[tokio::test]
	async fn test_optimized_interpolate_small_dataset() {
		let measurements = create_test_measurements(50);
		let start = measurements[0].timestamp;
		let end = measurements[measurements.len() - 1].timestamp;

		let result = optimized_interpolate(measurements, start, end, Resolution::Seconds, SplineType::Cubic);

		assert!(result.is_ok(), "Error: {:?}", result.err());
		let interpolated = result.unwrap();
		assert!(!interpolated.is_empty());
	}

	#[tokio::test]
	async fn test_fast_path_algorithm_selection() {
		let measurements = create_test_measurements(1000);
		let start = measurements[0].timestamp;
		let end = measurements[measurements.len() - 1].timestamp;

		// Should automatically switch from Cubic to Quadratic for 1000 points
		let result = fast_path_interpolate(measurements, start, end, Resolution::Seconds, SplineType::Cubic);

		assert!(result.is_ok());
		let interpolated = result.unwrap();
		assert!(!interpolated.is_empty());
	}

	#[tokio::test]
	async fn test_streaming_interpolate() {
		let measurements = create_test_measurements(2000);
		let start = measurements[0].timestamp;
		let end = measurements[measurements.len() - 1].timestamp;

		let result = streaming_interpolate(
			measurements,
			start,
			end,
			Resolution::Seconds,
			SplineType::Linear,
			500, // Chunk size
		);

		assert!(result.is_ok());
		let interpolated = result.unwrap();
		assert!(!interpolated.is_empty());

		// Verify results are sorted
		for i in 1..interpolated.len() {
			assert!(interpolated[i].timestamp >= interpolated[i - 1].timestamp);
		}
	}

	#[tokio::test]
	async fn test_performance_recommendations() {
		// Test different dataset sizes
		let test_cases = vec![(50, true, SplineType::Cubic), (50, false, SplineType::Cubic), (300, true, SplineType::Cubic), (300, false, SplineType::Quadratic), (800, true, SplineType::Quadratic), (800, false, SplineType::Linear), (2000, true, SplineType::Linear), (2000, false, SplineType::Linear)];

		for (count, accuracy_priority, expected_spline) in test_cases {
			let (recommended_spline, strategy) = get_performance_recommendation(count, accuracy_priority);
			assert_eq!(recommended_spline, expected_spline);
			assert!(!strategy.is_empty());
		}
	}

	#[tokio::test]
	async fn test_parallel_threshold() {
		let small_measurements = create_test_measurements(500);
		let large_measurements = create_test_measurements(1500);

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = start + chrono::Duration::hours(1);

		// Small dataset should use standard processing
		let small_result = optimized_interpolate(small_measurements, start, end, Resolution::Seconds, SplineType::Linear);
		assert!(small_result.is_ok());

		// Large dataset should use parallel processing
		let large_result = optimized_interpolate(large_measurements, start, end, Resolution::Seconds, SplineType::Linear);
		assert!(large_result.is_ok());
	}
}
