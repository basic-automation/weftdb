//! Parallel and optimized interpolation processing for large datasets
//!
//! This module provides high-performance interpolation with automatic algorithm
//! selection based on dataset characteristics and benchmark-driven optimizations.

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use rayon::prelude::*;

use super::{auto_interpolate, Resolution, SplineType};
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

/// Enhanced fast path interpolation with SIMD and parallel optimization
///
/// Combines intelligent algorithm selection with hardware acceleration:
/// - SIMD for dense output (>512 points)
/// - Parallel processing for large datasets (>1000 measurements)
/// - Hybrid SIMD+Parallel for maximum performance
/// - Algorithm degradation for optimal speed/accuracy balance
///
/// # Performance Targets
/// - **Small datasets**: 4K+ elem/s (standard algorithms)
/// - **Medium + dense**: 45K+ elem/s (SIMD acceleration)
/// - **Large datasets**: 20K+ elem/s (parallel processing)
/// - **Huge datasets**: 25K+ elem/s (streaming approach)
///
/// # Errors
///
/// Returns an error if the underlying interpolation algorithm fails or
/// if there are insufficient measurements for the requested spline type.
pub fn fast_path_interpolate(
    measurements: Vec<Measurement>, 
    start: DateTime<Utc>, 
    end: DateTime<Utc>, 
    resolution: Resolution, 
    spline_type: SplineType
) -> Result<Vec<Measurement>> {
    let measurement_count = measurements.len();
    
    // Calculate output density for SIMD decision
    let time_span = (end - start).num_seconds();
    let step_ms = resolution.to_step().num_milliseconds();
    let estimated_output_points = if step_ms > 0 {
        usize::try_from((time_span * 1000 / step_ms).max(1)).unwrap_or(1000)
    } else {
        1000
    };

    // **STRATEGY 1: Small datasets - Standard algorithms**
    if measurement_count < 200 {
        return execute_standard_algorithm_fast(measurements, start, end, resolution, spline_type);
    }

    // **STRATEGY 2: Medium datasets + Dense output - SIMD Fast Path**
    if measurement_count < 1000 && estimated_output_points >= 512 {
        return execute_simd_fast_path(measurements, start, end, resolution, spline_type, estimated_output_points);
    }

    // **STRATEGY 3: Large datasets - Parallel + SIMD Fast Path**
    if measurement_count < 5000 {
        return execute_parallel_simd_fast_path(measurements, start, end, resolution, spline_type, estimated_output_points);
    }

    // **STRATEGY 4: Huge datasets - Streaming with degradation**
    execute_streaming_fast_path(measurements, start, end, resolution, spline_type, measurement_count)
}

/// Execute standard algorithm with fast path optimizations
fn execute_standard_algorithm_fast(
    measurements: Vec<Measurement>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    resolution: Resolution,
    spline_type: SplineType,
) -> Result<Vec<Measurement>> {
    // Apply minor optimizations even for small datasets
    let optimized_type = match spline_type {
        SplineType::Polynomial(degree) if degree > 3 => SplineType::Cubic, // Limit complexity
        _ => spline_type,
    };

    // **FIX: Use direct algorithm calls to prevent recursion**
    match optimized_type {
        SplineType::Linear => super::linear::linear(measurements, start, end, resolution),
        SplineType::Quadratic => super::quadratic::quadratic(measurements, start, end, resolution),
        SplineType::Cubic => super::cubic::cubic(measurements, start, end, resolution),
        SplineType::Polynomial(degree) => super::polynomial::polynomial(measurements, start, end, resolution, degree),
    }
}

/// Execute SIMD fast path for medium datasets with dense output
fn execute_simd_fast_path(
    measurements: Vec<Measurement>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    resolution: Resolution,
    spline_type: SplineType,
    estimated_output_points: usize,
) -> Result<Vec<Measurement>> {
    // Apply fast path algorithm degradation for SIMD efficiency
    let simd_optimized_type = match spline_type {
        SplineType::Polynomial(degree) if degree > 3 => SplineType::Cubic,
        SplineType::Cubic if estimated_output_points > 2000 => SplineType::Quadratic, // Heavy degradation for very dense
        _ => spline_type,
    };

    // Generate target times for SIMD processing
    let target_times = generate_target_times_fast(start, end, resolution);
    
    // Verify we have enough target points to justify SIMD overhead
    if target_times.len() < 64 {
        // **FIX: Use direct algorithm call to prevent recursion**
        return match simd_optimized_type {
            SplineType::Linear => super::linear::linear(measurements, start, end, resolution),
            SplineType::Quadratic => super::quadratic::quadratic(measurements, start, end, resolution),
            SplineType::Cubic => super::cubic::cubic(measurements, start, end, resolution),
            SplineType::Polynomial(degree) => super::polynomial::polynomial(measurements, start, end, resolution, degree),
        };
    }

    // Use SIMD batch processing for hardware acceleration
    super::simd::auto_interpolate_simd(&measurements, &target_times, simd_optimized_type)
}

/// Execute parallel + SIMD fast path for large datasets
fn execute_parallel_simd_fast_path(
    measurements: Vec<Measurement>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    resolution: Resolution,
    spline_type: SplineType,
    estimated_output_points: usize,
) -> Result<Vec<Measurement>> {
    // Aggressive fast path optimizations for large datasets
    let parallel_optimized_type = match (spline_type, measurements.len()) {
        (SplineType::Polynomial(degree), n) if degree > 2 && n > 2000 => SplineType::Quadratic,
        (SplineType::Cubic, n) if n > 3000 => SplineType::Quadratic,
        (SplineType::Polynomial(degree), _) if degree > 5 => SplineType::Cubic,
        _ => spline_type,
    };

    // **PARALLEL + SIMD**: Use parallel SIMD for dense output scenarios
    if estimated_output_points >= 1024 {
        let target_times = generate_target_times_fast(start, end, resolution);
        return super::simd::auto_interpolate_simd_parallel(&measurements, &target_times, parallel_optimized_type);
    }

    // **PARALLEL ONLY**: Use standard parallel processing for large datasets
    if measurements.len() >= 1500 {
        // **FIX: Use direct parallel calls to prevent infinite recursion**
        return match parallel_optimized_type {
            SplineType::Polynomial(degree) => {
                polynomial_parallel(measurements, start, end, resolution, degree)
            },
            SplineType::Quadratic => {
                quadratic_parallel(measurements, start, end, resolution)  
            },
            _ => {
                // **FIX: Use auto_interpolate instead of recursive parallel_interpolate call**
                auto_interpolate(measurements, start, end, resolution, parallel_optimized_type)
            }
        };
    }

    // **OPTIMIZED SINGLE-THREADED**: Use direct algorithm calls
    match parallel_optimized_type {
        SplineType::Linear => super::linear::linear(measurements, start, end, resolution),
        SplineType::Quadratic => super::quadratic::quadratic(measurements, start, end, resolution),
        SplineType::Cubic => super::cubic::cubic(measurements, start, end, resolution),
        SplineType::Polynomial(degree) => super::polynomial::polynomial(measurements, start, end, resolution, degree),
    }
}

/// Execute streaming fast path for huge datasets with maximum degradation
fn execute_streaming_fast_path(
    measurements: Vec<Measurement>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    resolution: Resolution,
    spline_type: SplineType,
    measurement_count: usize,
) -> Result<Vec<Measurement>> {
    // Maximum algorithm degradation for streaming performance
    let streaming_optimized_type = match (spline_type, measurement_count) {
        (_, n) if n > 20000 => SplineType::Linear, // Linear only for massive datasets
        (SplineType::Polynomial(_), n) if n > 10000 => SplineType::Linear,
        (SplineType::Cubic, n) if n > 15000 => SplineType::Linear,
        // Nest the patterns as suggested by Clippy
        (SplineType::Polynomial(_) | SplineType::Cubic, _) => SplineType::Quadratic,
        _ => spline_type,
    };

    // Calculate optimal chunk size with fast path considerations
    let chunk_size = calculate_fast_path_chunk_size(measurement_count);
    
    streaming_interpolate(
        measurements, 
        start, 
        end, 
        resolution, 
        streaming_optimized_type, 
        chunk_size
    )
}

/// Generate target times for SIMD processing (fast version)
fn generate_target_times_fast(start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Vec<DateTime<Utc>> {
    let step = resolution.to_step();
    let mut times = Vec::new();
    let mut current = start;
    
    // Limit to prevent excessive memory usage
    let max_points = 50_000; // Reduced safety limit for fast path
    let mut count = 0;
    
    while current <= end && count < max_points {
        times.push(current);
        current += step;
        count += 1;
    }
    
    times
}

/// Calculate fast path chunk size
const fn calculate_fast_path_chunk_size(measurement_count: usize) -> usize {
    match measurement_count {
        0..=5000 => 500,       // Smaller chunks for faster processing
        5001..=20000 => 1000,  // Medium chunks
        20001..=50000 => 2000, // Large chunks
        _ => 5000,             // Very large chunks for huge datasets
    }
}

/// Streaming interpolation for huge datasets with memory efficiency
pub fn streaming_interpolate(
    measurements: Vec<Measurement>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    resolution: Resolution,
    spline_type: SplineType,
    chunk_size: usize,
) -> Result<Vec<Measurement>> {
    let measurement_count = measurements.len();
    
    // For smaller datasets, use regular parallel processing
    if measurement_count <= chunk_size * 2 {
        return auto_interpolate(measurements, start, end, resolution, spline_type);
    }
    
    // Sort measurements by timestamp for streaming
    let mut sorted_measurements = measurements;
    sorted_measurements.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    
    let total_duration = end - start;
    let num_chunks = (measurement_count / chunk_size).max(1).min(100); // Limit chunks
    
    // Create time-based chunks for streaming
    let time_chunks: Vec<(DateTime<Utc>, DateTime<Utc>)> = (0..num_chunks)
        .map(|i| {
            let i_i32 = i32::try_from(i).unwrap_or(i32::MAX);
            let num_chunks_i32 = i32::try_from(num_chunks).unwrap_or(i32::MAX);
            
            let chunk_start_time = start + total_duration * i_i32 / num_chunks_i32;
            let chunk_end_time = if i == num_chunks - 1 {
                end
            } else {
                let next_i_i32 = i32::try_from(i + 1).unwrap_or(i32::MAX);
                start + total_duration * next_i_i32 / num_chunks_i32
            };
            
            (chunk_start_time, chunk_end_time)
        })
        .collect();
    
    // Process chunks sequentially to maintain memory efficiency
    let mut all_results = Vec::new();
    
    for (chunk_start, chunk_end) in time_chunks {
        // Extract measurements for this time chunk
        let chunk_measurements: Vec<Measurement> = sorted_measurements
            .iter()
            .filter(|m| m.timestamp >= chunk_start && m.timestamp <= chunk_end)
            .cloned()
            .collect();
        
        // Skip chunks with insufficient data
        if chunk_measurements.len() < 2 {
            continue;
        }
        
        // Use direct algorithm call to prevent recursion
        let chunk_results = match spline_type {
            SplineType::Linear => super::linear::linear(chunk_measurements, chunk_start, chunk_end, resolution),
            SplineType::Quadratic => super::quadratic::quadratic(chunk_measurements, chunk_start, chunk_end, resolution),
            SplineType::Cubic => super::cubic::cubic(chunk_measurements, chunk_start, chunk_end, resolution),
            SplineType::Polynomial(degree) => super::polynomial::polynomial(chunk_measurements, chunk_start, chunk_end, resolution, degree),
        }?;
        
        all_results.extend(chunk_results);
    }
    
    // Sort and deduplicate final results
    all_results.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    all_results.dedup_by(|a, b| a.timestamp == b.timestamp && a.dataset_id == b.dataset_id);
    
    // Filter to exact time range
    all_results.retain(|m| m.timestamp >= start && m.timestamp <= end);
    
    Ok(all_results)
}

/// Parallel polynomial interpolation for large datasets
pub fn polynomial_parallel(
    measurements: Vec<Measurement>, 
    start: DateTime<Utc>, 
    end: DateTime<Utc>, 
    resolution: Resolution, 
    degree: usize
) -> Result<Vec<Measurement>> {
    // Use if-else instead of match for simple equality check
    if degree == 2 {
        quadratic_parallel(measurements, start, end, resolution)
    } else {
        if measurements.len() < degree + 1 {
            return Err(anyhow::anyhow!("Insufficient measurements for polynomial degree"));
        }

        // For large datasets, use parallel coefficient computation
        if measurements.len() > 500 {
            return polynomial_parallel_chunked(&measurements, start, end, resolution, degree);
        }

        // Use standard polynomial for smaller datasets
        super::polynomial::polynomial(measurements, start, end, resolution, degree)
    }
}

/// Parallel quadratic interpolation
pub fn quadratic_parallel(
    measurements: Vec<Measurement>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    resolution: Resolution,
) -> Result<Vec<Measurement>> {
    // For large datasets, use parallel processing
    if measurements.len() > 1000 {
        parallel_interpolate(measurements, start, end, resolution, SplineType::Quadratic)
    } else {
        super::quadratic::quadratic(measurements, start, end, resolution)
    }
}

/// Parallel polynomial interpolation with chunking
fn polynomial_parallel_chunked(
    measurements: &[Measurement],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    resolution: Resolution,
    degree: usize,
) -> Result<Vec<Measurement>> {
    // Use parallel processing for large polynomial interpolation
    let chunk_size = 1000;
    let chunks: Vec<&[Measurement]> = measurements.chunks(chunk_size).collect();
    
    let results: Result<Vec<Vec<Measurement>>> = chunks
        .par_iter()
        .map(|chunk| {
            super::polynomial::polynomial(chunk.to_vec(), start, end, resolution, degree)
        })
        .collect();
    
    let chunk_results = results?;
    Ok(merge_interpolation_chunks(chunk_results, start, end))
}

/// Merge interpolation chunks and remove overlapping points
fn merge_interpolation_chunks(
    chunk_results: Vec<Vec<Measurement>>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Vec<Measurement> {
    let mut all_results: Vec<Measurement> = chunk_results.into_iter().flatten().collect();
    
    // Sort by timestamp
    all_results.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    
    // Remove duplicates (keep first occurrence)
    all_results.dedup_by(|a, b| {
        a.timestamp == b.timestamp && a.dataset_id == b.dataset_id
    });
    
    // Filter to exact time range
    all_results.retain(|m| m.timestamp >= start && m.timestamp <= end);
    
    all_results
}
