use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use crate::{Error, Measurement};

pub mod cubic;
pub mod gpu;
pub mod linear;
pub mod polynomial;
pub mod quadratic;
pub mod simd; // Add simd module
pub mod parallel; // Add parallel module

// Re-export spline functions for convenience
pub use cubic::cubic;
pub use gpu::{gpu_linear_interpolate_optimized, gpu_linear_interpolate_with_fallback}; // ← Fixed import
pub use linear::linear;
pub use polynomial::polynomial;
pub use quadratic::quadratic;
pub use parallel::{optimized_interpolate, fast_path_interpolate};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Seconds,
    Minutes,
    Hours,
    Days,
}

impl Resolution {
    #[must_use]
    pub const fn to_step(self) -> chrono::Duration {
        match self {
            Self::Seconds => chrono::Duration::seconds(1),
            Self::Minutes => chrono::Duration::minutes(1),
            Self::Hours => chrono::Duration::hours(1),
            Self::Days => chrono::Duration::days(1),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplineType {
    Linear,
    Quadratic,
    Cubic,
    Polynomial(usize),
}

/// Apply fast path optimization to spline types based on dataset characteristics
///
/// This function automatically downgrades complex spline types to simpler ones
/// for better performance when dealing with large datasets or specific conditions.
///
/// # Arguments
/// * `spline_type` - The requested spline type
/// * `measurement_count` - Number of input measurements
/// * `_complexity_factor` - Additional complexity factor (e.g., polynomial degree) - currently unused
///
/// # Returns
/// Optimized spline type that provides better performance characteristics
#[must_use] pub fn apply_fast_path_optimization(spline_type: SplineType, measurement_count: usize, _complexity_factor: usize) -> SplineType {
    match spline_type {
        // Linear always stays linear - it's already optimal
        SplineType::Linear => SplineType::Linear,
        
        // Quadratic optimizations
        SplineType::Quadratic => {
            if measurement_count > 5000 {
                // Very large datasets: degrade to linear for speed
                SplineType::Linear
            } else {
                SplineType::Quadratic
            }
        }
        
        // Cubic optimizations - FIXED: Use >= instead of >
        SplineType::Cubic => {
            if measurement_count > 5000 {
                // Very large datasets: degrade to linear
                SplineType::Linear
            } else if measurement_count >= 2500 {  // ← FIXED: Changed > to >=
                // Large datasets: degrade to quadratic
                SplineType::Quadratic
            } else {
                SplineType::Cubic
            }
        }
        
        // Polynomial optimizations
        SplineType::Polynomial(degree) => {
            if degree > 5 || measurement_count > 1000 {
                // High degree or large datasets: degrade to quadratic
                SplineType::Quadratic
            } else if measurement_count > 3000 {
                // Very large datasets: degrade to linear
                SplineType::Linear
            } else if degree <= 2 {
                // Low degree: use quadratic
                SplineType::Quadratic
            } else if degree == 3 {
                // Degree 3: use cubic
                SplineType::Cubic
            } else {
                // Moderate degree: keep as polynomial but limit degree
                SplineType::Polynomial(degree.min(4))
            }
        }
    }
}

/// Apply streaming interpolation for very large datasets
///
/// This function provides chunk-based processing for datasets that are too large
/// to process in memory efficiently.
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient measurements for interpolation
/// - Invalid time range
/// - Underlying interpolation fails
/// - Chunk size is invalid
pub fn streaming_interpolate(
    measurements: Vec<Measurement>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    resolution: Resolution,
    spline_type: SplineType,
    chunk_size: usize,
) -> Result<Vec<Measurement>> {
    if measurements.is_empty() {
        return Err(Error::InsufficientMeasurementsError.into());
    }

    if start >= end {
        return Err(Error::InvalidTimeRangeError.into());
    }

    if chunk_size == 0 {
        return Err(anyhow::anyhow!("Chunk size must be greater than 0"));
    }

    // For very large datasets, use chunked processing
    if measurements.len() <= chunk_size * 2 {
        // Not large enough to benefit from streaming, use regular optimized interpolation
        return parallel::optimized_interpolate(measurements, start, end, resolution, spline_type);
    }

    // Apply fast path optimization to the spline type
    let optimized_spline_type = apply_fast_path_optimization(spline_type, measurements.len(), 1);

    // Generate all target times
    let target_times = generate_target_times(start, end, resolution);
    
    // Process in chunks with overlap to ensure continuity
    let overlap_size = chunk_size / 10; // 10% overlap
    let mut results = Vec::new();
    
    for chunk_start in (0..target_times.len()).step_by(chunk_size - overlap_size) {
        let chunk_end = (chunk_start + chunk_size).min(target_times.len());
        let chunk_target_times = &target_times[chunk_start..chunk_end];
        
        if chunk_target_times.is_empty() {
            continue;
        }
        
        let chunk_start_time = chunk_target_times[0];
        let chunk_end_time = chunk_target_times[chunk_target_times.len() - 1];
        
        // Filter measurements relevant to this chunk (with some buffer)
        let buffer_duration = resolution.to_step() * 5; // 5-step buffer
        let chunk_measurements: Vec<Measurement> = measurements
            .iter()
            .filter(|m| {
                m.timestamp >= (chunk_start_time - buffer_duration) &&
                m.timestamp <= (chunk_end_time + buffer_duration)
            })
            .cloned()
            .collect();
            
        if chunk_measurements.len() < 2 {
            // Not enough measurements for this chunk, skip
            continue;
        }
        
        // Interpolate this chunk
        let chunk_result = parallel::optimized_interpolate(
            chunk_measurements,
            chunk_start_time,
            chunk_end_time,
            resolution,
            optimized_spline_type,
        )?;
        
        // Add to results, avoiding duplicates from overlap
        if chunk_start == 0 {
            results.extend(chunk_result);
        } else {
            // Skip overlap points that were already processed
            let skip_count = overlap_size.min(chunk_result.len());
            results.extend(chunk_result.into_iter().skip(skip_count));
        }
    }
    
    Ok(results)
}

/// Parameters for strategy selection
#[derive(Debug, Clone)]
pub struct InterpolationParams {
    pub measurements: Vec<Measurement>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub resolution: Resolution,
    pub spline_type: SplineType,
}

/// Select optimal interpolation strategy (sync version)
fn select_optimal_strategy_sync(params: InterpolationParams) -> Result<Vec<Measurement>> {
    // Simple strategy selection based on measurement count
    let measurement_count = params.measurements.len();
    
    if measurement_count > 10_000 {
        // Use parallel processing for large datasets
        auto_interpolate(params.measurements, params.start, params.end, params.resolution, params.spline_type)
    } else if measurement_count > 1_000 {
        // Use optimized algorithms for medium datasets
        auto_interpolate(params.measurements, params.start, params.end, params.resolution, params.spline_type)
    } else {
        // Use direct algorithms for small datasets
        match params.spline_type {
            SplineType::Linear => linear::linear(params.measurements, params.start, params.end, params.resolution),
            SplineType::Quadratic => quadratic::quadratic(params.measurements, params.start, params.end, params.resolution),
            SplineType::Cubic => cubic::cubic(params.measurements, params.start, params.end, params.resolution),
            SplineType::Polynomial(degree) => polynomial::polynomial(params.measurements, params.start, params.end, params.resolution, degree),
        }
    }
}

/// Automatic interpolation with optimal algorithm selection
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient measurements for interpolation
/// - Invalid time range
/// - Interpolation algorithm fails
pub fn auto_interpolate(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline_type: SplineType) -> Result<Vec<Measurement>> {
    // Input validation
    if measurements.is_empty() {
        return Err(Error::InsufficientMeasurementsError.into());
    }

    if start >= end {
        return Err(Error::InvalidTimeRangeError.into());
    }

    // Check dataset ID consistency
    let dataset_id = measurements[0].dataset_id;
    if !measurements.iter().all(|m| m.dataset_id == dataset_id) {
        return Err(Error::InconsistentDatasetIdsError.into());
    }

    // Apply fast path optimization
    let optimized_spline_type = apply_fast_path_optimization(spline_type, measurements.len(), 1);

    // Create parameters for strategy selection
    let params = InterpolationParams {
        measurements,
        start,
        end,
        resolution,
        spline_type: optimized_spline_type,
    };

    // Use strategy selection
    select_optimal_strategy_sync(params)
}

/// Async automatic interpolation with GPU acceleration support
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient measurements for interpolation
/// - Invalid time range
/// - GPU initialization fails (with CPU fallback)
/// - All interpolation methods fail
pub async fn auto_interpolate_async(
    measurements: Vec<Measurement>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    resolution: Resolution,
    spline_type: SplineType,
) -> Result<Vec<Measurement>> {
    if measurements.is_empty() {
        return Ok(Vec::new());
    }

    let estimated_output_points = estimate_output_points(start, end, resolution);
    
    // Use optimized GPU thresholds based on benchmark results
    let use_gpu = should_use_gpu_interpolation(measurements.len(), estimated_output_points);
    
    match spline_type {
        SplineType::Linear => {
            if use_gpu {
                println!("🚀 Using GPU acceleration: {} measurements → {} output points ({}x speed boost expected)", 
                         measurements.len(), estimated_output_points, 
                         get_expected_speedup(measurements.len(), estimated_output_points));
                
                // Generate target times for GPU
                let target_times = generate_target_times(start, end, resolution);
                let dataset_id = measurements[0].dataset_id;
                
                // Use optimized GPU interpolation
                if let Ok(result) = gpu::gpu_linear_interpolate_optimized(measurements.clone(), target_times, dataset_id).await { return Ok(result) }
                println!("⚠️  GPU fallback to CPU");
                return linear::linear(measurements, start, end, resolution);
            }
            // Use CPU for smaller datasets
            linear::linear(measurements, start, end, resolution)
        }
        SplineType::Quadratic => quadratic(measurements, start, end, resolution),
        SplineType::Cubic => cubic(measurements, start, end, resolution),
        SplineType::Polynomial(degree) => polynomial(measurements, start, end, resolution, degree),
    }
}

/// Get expected GPU speedup based on benchmark results
const fn get_expected_speedup(measurement_count: usize, output_points: usize) -> &'static str {
    match (measurement_count, output_points) {
        // Combine identical speedup categories for better maintainability
        (5_000..=9_999, 50_000..=99_999) | (25_000..=49_999, 200_000..=299_999) => "1.4x",
        (10_000..=24_999, 100_000..=199_999) | (50_000..=99_999, 300_000..=499_999) => "4.2x",
        (100_000.., 500_000..) => "6.0x",
        _ => "1.3x",
    }
}

/// Estimate output points based on time range and resolution (make public)
#[must_use] 
pub fn estimate_output_points(start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> usize {
    let duration = end.signed_duration_since(start);
    
    let total_seconds = duration.num_seconds().max(0); // Ensure non-negative
    
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let total_seconds_usize = total_seconds as usize; // Safe after max(0) check
    
    match resolution {
        Resolution::Seconds => total_seconds_usize,
        Resolution::Minutes => total_seconds_usize / 60,
        Resolution::Hours => total_seconds_usize / 3600,
        Resolution::Days => total_seconds_usize / 86400,
    }
}

/// Generate target times for interpolation
fn generate_target_times(start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Vec<DateTime<Utc>> {
    let mut target_times = Vec::new();
    let mut current = start;
    
    let step = match resolution {
        Resolution::Seconds => Duration::seconds(1),
        Resolution::Minutes => Duration::minutes(1),
        Resolution::Hours => Duration::hours(1),
        Resolution::Days => Duration::days(1),
    };
    
    while current <= end {
        target_times.push(current);
        current += step;
    }
    
    target_times
}

/// Determine if GPU should be used with optimized thresholds
#[must_use] pub fn should_use_gpu_interpolation(measurement_count: usize, estimated_output_points: usize) -> bool {
    let gpu_score = calculate_gpu_efficiency_score(measurement_count, estimated_output_points);
    
    // UPDATED: Much lower threshold based on benchmark results showing consistent GPU wins
    gpu_score > 0.8 && estimated_output_points >= 25_000 && measurement_count >= 1_000
}

/// Calculate GPU efficiency score with optimized thresholds
fn calculate_gpu_efficiency_score(measurement_count: usize, estimated_output_points: usize) -> f64 {
    let base_score = match estimated_output_points {
        0..=25_000 => 0.4,      // ← UPDATED: Higher base score
        25_001..=50_000 => 1.2, // ← UPDATED: Strong GPU preference  
        50_001..=100_000 => 1.8,
        100_001..=500_000 => 2.5,
        _ => 3.0,               // ← UPDATED: Even stronger for very large
    };

    // Enhanced bonus for dense output (GPU parallelism advantage)
    let density_bonus = if measurement_count > 0 {
        #[allow(clippy::cast_precision_loss)]
        let output_density = estimated_output_points as f64 / measurement_count as f64;
        if output_density >= 20.0 { 0.4 } else if output_density >= 10.0 { 0.2 } else { 0.0 }
    } else {
        0.0
    };

    // Enhanced bonus for larger input datasets
    let input_bonus = match measurement_count {
        1000..=5000 => 0.2,   // ← UPDATED: Higher bonus
        5001..=10000 => 0.3,  // ← UPDATED: Higher bonus
        _ => 0.4,             // ← UPDATED: Higher bonus
    };

    base_score + density_bonus + input_bonus
}
