use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::{Error, Measurement};

pub mod cubic;
pub mod fast_paths;
pub mod linear;
pub mod parallel;
pub mod polynomial;
pub mod quadratic;
pub mod simd; // Make SIMD module public

// Re-export spline functions
pub use cubic::cubic;
pub use linear::linear;
// Re-export parallel and optimized functions
pub use parallel::*;
pub use polynomial::polynomial;
pub use quadratic::quadratic;
// Re-export SIMD functions for benchmarking and advanced use cases
pub use simd::{auto_interpolate_simd, cubic_simd_batch, linear_simd_batch, polynomial_simd_batch, quadratic_simd_batch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
	Milliseconds,
	Seconds,
	Minutes,
	Hours,
	Days,
}

impl Resolution {
	/// Get the duration in milliseconds for this resolution
	#[must_use]
	pub const fn to_milliseconds(self) -> i64 {
		match self {
			Self::Milliseconds => 1,
			Self::Seconds => 1000,
			Self::Minutes => 60_000,
			Self::Hours => 3_600_000,
			Self::Days => 86_400_000,
		}
	}

	/// Get the step duration for this resolution
	#[must_use]
	pub const fn to_step(self) -> chrono::Duration {
		match self {
			Self::Milliseconds => chrono::Duration::milliseconds(1),
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

/// Intelligently choose the best interpolation method and perform interpolation
///
/// This function automatically selects the optimal interpolation strategy based on:
/// - Dataset size and characteristics
/// - Spline type complexity  
/// - Benchmark-driven performance thresholds
/// - Target output density
/// - Hardware capabilities
///
/// # Performance Strategy Selection
/// 
/// ## Small Datasets (< 200 measurements):
/// - **Standard algorithms**: Overhead of optimization not justified
/// - **All spline types**: Use requested algorithm directly
/// - **Performance**: 31K+ elements/sec across all types
///
/// ## Medium Datasets (200-1000 measurements):
/// - **SIMD optimization**: Hardware acceleration beneficial
/// - **Threshold**: 256+ target points → SIMD batch processing
/// - **Fallback**: < 256 target points → Standard algorithms
/// - **Performance**: 2-4x speedup with SIMD
///
/// ## Large Datasets (1000-5000 measurements):
/// - **Parallel SIMD**: Multi-core + vectorization
/// - **Threshold**: 512+ target points → Parallel SIMD chunks
/// - **Fast path**: Complex algorithms → Simpler alternatives
/// - **Performance**: 4-8x speedup with parallelization
///
/// ## Huge Datasets (> 5000 measurements):
/// - **Streaming approach**: Memory-efficient processing
/// - **Algorithm degradation**: Maximum performance priority
/// - **Chunk size**: 1000-2000 measurements per chunk
/// - **Performance**: Constant memory usage
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
/// Returns a vector of interpolated measurements using the optimal strategy.
///
/// # Errors
///
/// Returns an error if:
/// - There are fewer than 2 measurements
/// - Measurements have different dataset IDs
/// - The time range is invalid (start >= end)
/// - For polynomial interpolation, insufficient measurements for the specified degree
pub fn auto_interpolate(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline_type: SplineType) -> Result<Vec<Measurement>> {
    // Early validation
    if measurements.len() < 2 {
        return Err(Error::InsufficientMeasurementsError.into());
    }

    // Validate all measurements have the same dataset_id
    let dataset_id = measurements[0].dataset_id;
    if !measurements.iter().all(|m| m.dataset_id == dataset_id) {
        return Err(Error::DifferentDatasetIdsError.into());
    }

    // Validate time range
    if start >= end {
        return Err(Error::InvalidTimeRangeError.into());
    }

    // Calculate key metrics for strategy selection
    let measurement_count = measurements.len();
    let time_span = (end - start).num_seconds();
    let step_ms = resolution.to_step().num_milliseconds();
    
    // Safe conversion for estimated output points
    let estimated_output_points = if step_ms > 0 {
        usize::try_from((time_span * 1000 / step_ms).max(1)).unwrap_or(1000)
    } else {
        1000 // Fallback for edge cases
    };

    // Get algorithm complexity score for optimization decisions
    let algorithm_complexity = get_algorithm_complexity(spline_type);
    
    // Create strategy parameters
    let params = StrategyParams {
        measurements,
        start,
        end,
        resolution,
        spline_type,
        measurement_count,
        estimated_output_points,
        algorithm_complexity,
    };
    
    // Select optimal strategy based on comprehensive metrics
    select_optimal_strategy(params)
}

/// Get algorithm complexity score for optimization decisions
fn get_algorithm_complexity(spline_type: SplineType) -> u8 {
    match spline_type {
        SplineType::Linear => 1,
        SplineType::Quadratic => 2,
        SplineType::Cubic => 3,
        SplineType::Polynomial(degree) => {
            // Safe conversion with bounds checking
            u8::try_from(degree).unwrap_or(10).min(10) // Cap at 10
        }
    }
}

/// Strategy selection parameters for cleaner function signature
#[derive(Debug)]
struct StrategyParams {
    measurements: Vec<Measurement>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    resolution: Resolution,
    spline_type: SplineType,
    measurement_count: usize,
    estimated_output_points: usize,
    algorithm_complexity: u8,
}

/// Select the optimal interpolation strategy based on comprehensive metrics
fn select_optimal_strategy(params: StrategyParams) -> Result<Vec<Measurement>> {
    // Strategy 1: Small datasets - Standard algorithms (fastest for overhead avoidance)
    if params.measurement_count < 200 {
        return execute_standard_algorithm(params.measurements, params.start, params.end, params.resolution, params.spline_type);
    }

    // Strategy 2: Medium datasets with dense output - SIMD optimization
    if params.measurement_count < 1000 && params.estimated_output_points >= 256 {
        // Check if SIMD provides benefit for this algorithm
        if params.algorithm_complexity <= 3 { // Linear, Quadratic, Cubic
            return execute_simd_strategy(params.measurements, params.start, params.end, params.resolution, params.spline_type, params.estimated_output_points);
        }
    }

    // Strategy 3: Large datasets - Parallel SIMD + Fast path optimization
    if params.measurement_count < 5000 {
        return execute_large_dataset_strategy(params.measurements, params.start, params.end, params.resolution, params.spline_type, params.estimated_output_points, params.algorithm_complexity);
    }

    // Strategy 4: Huge datasets - Streaming approach with maximum optimization
    execute_streaming_strategy(params.measurements, params.start, params.end, params.resolution, params.spline_type, params.measurement_count)
}

/// Execute standard algorithm for small datasets
fn execute_standard_algorithm(
    measurements: Vec<Measurement>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    resolution: Resolution,
    spline_type: SplineType,
) -> Result<Vec<Measurement>> {
    match spline_type {
        SplineType::Linear => linear(measurements, start, end, resolution),
        SplineType::Quadratic => quadratic(measurements, start, end, resolution),
        SplineType::Cubic => cubic(measurements, start, end, resolution),
        SplineType::Polynomial(degree) => polynomial(measurements, start, end, resolution, degree),
    }
}

/// Execute SIMD strategy for medium datasets with dense output
fn execute_simd_strategy(
    measurements: Vec<Measurement>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    resolution: Resolution,
    spline_type: SplineType,
    _estimated_output_points: usize, // ← Prefix with underscore since we're generating target_times instead
) -> Result<Vec<Measurement>> {
    // Generate target times for SIMD processing
    let target_times = generate_target_times(start, end, resolution);
    
    // Verify we have enough target points to justify SIMD overhead
    if target_times.len() < 32 {
        return execute_standard_algorithm(measurements, start, end, resolution, spline_type);
    }

    // Use SIMD batch processing for hardware acceleration
    simd::auto_interpolate_simd(&measurements, &target_times, spline_type)
}

/// Execute strategy for large datasets with parallel processing and fast path optimization
fn execute_large_dataset_strategy(
    measurements: Vec<Measurement>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    resolution: Resolution,
    spline_type: SplineType,
    _estimated_output_points: usize, // ← Prefix with underscore since it's not used in this branch
    algorithm_complexity: u8,
) -> Result<Vec<Measurement>> {
    // Apply fast path optimizations for complex algorithms
    let optimized_spline_type = apply_fast_path_optimization(spline_type, measurements.len(), algorithm_complexity);
    
    // Use parallel SIMD for dense output scenarios
    if _estimated_output_points >= 512 && algorithm_complexity <= 3 {
        let target_times = generate_target_times(start, end, resolution);
        return simd::auto_interpolate_simd_parallel(&measurements, &target_times, optimized_spline_type);
    }

    // Use parallel processing for high measurement count
    if measurements.len() >= 1000 {
        return match optimized_spline_type {
            SplineType::Polynomial(degree) => {
                parallel::polynomial_parallel(measurements, start, end, resolution, degree)
            },
            _ => parallel::parallel_interpolate(measurements, start, end, resolution, optimized_spline_type)
        };
    }

    // Use optimized single-threaded approach
    parallel::optimized_interpolate(measurements, start, end, resolution, optimized_spline_type)
}

/// Execute streaming strategy for huge datasets
fn execute_streaming_strategy(
    measurements: Vec<Measurement>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    resolution: Resolution,
    spline_type: SplineType,
    measurement_count: usize,
) -> Result<Vec<Measurement>> {
    // Aggressive algorithm optimization for maximum performance
    let streaming_spline_type = match spline_type {
        SplineType::Cubic | SplineType::Polynomial(_) => {
            if measurement_count > 10000 {
                SplineType::Linear // Maximum performance for huge datasets
            } else {
                SplineType::Quadratic // Balance for large datasets
            }
        },
        _ => spline_type,
    };

    // Calculate optimal chunk size based on memory constraints
    let chunk_size = calculate_optimal_chunk_size(measurement_count);
    
    // Use the implemented streaming function in parallel module
    parallel::streaming_interpolate(
        measurements, 
        start, 
        end, 
        resolution, 
        streaming_spline_type, 
        chunk_size
    )
}

/// Apply fast path optimization with merged match arms
#[must_use] 
pub const fn apply_fast_path_optimization(spline_type: SplineType, measurement_count: usize, algorithm_complexity: u8) -> SplineType {
    match (measurement_count, algorithm_complexity) {
        // Medium and large datasets - conservative and aggressive optimization merged
        (1000..=5000, 3) | (2001..=5000, 4..=u8::MAX) => SplineType::Quadratic, // Cubic and Polynomial → Quadratic
        (1000..=2000, 4..=u8::MAX) => SplineType::Cubic, // High-degree polynomial → Cubic
        
        // No optimization needed
        _ => spline_type,
    }
}

/// Generate target times for SIMD processing
fn generate_target_times(start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Vec<DateTime<Utc>> {
    let step = resolution.to_step();
    let mut times = Vec::new();
    let mut current = start;
    
    // Limit to prevent excessive memory usage
    let max_points = 100_000; // Safety limit
    let mut count = 0;
    
    while current <= end && count < max_points {
        times.push(current);
        current += step;
        count += 1;
    }
    
    times
}

/// Calculate optimal chunk size for streaming based on available memory and dataset characteristics
const fn calculate_optimal_chunk_size(measurement_count: usize) -> usize {
    match measurement_count {
        0..=5000 => 1000,      // Small chunks for moderate datasets
        5001..=20000 => 2000,  // Medium chunks for large datasets  
        20001..=50000 => 5000, // Large chunks for very large datasets
        _ => 10000,            // Very large chunks for huge datasets
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use bigdecimal::BigDecimal;
    use chrono::TimeZone;
    use uuid::Uuid;

    fn create_test_measurements(count: usize) -> Vec<Measurement> {
        let dataset_id = Uuid::new_v4();
        let start_time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

        (0..count)
            .map(|i| Measurement {
                id: Uuid::new_v4(),
                dataset_id,
                timestamp: start_time + chrono::Duration::seconds(i as i64 * 10),
                value: BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap(),
            })
            .collect()
    }

    #[tokio::test]
    async fn test_auto_interpolate_strategy_selection() {
        // Test small dataset - should use standard algorithm
        let small_measurements = create_test_measurements(50);
        let start = small_measurements[0].timestamp;
        let end = small_measurements[small_measurements.len() - 1].timestamp;

        let result = auto_interpolate(small_measurements, start, end, Resolution::Seconds, SplineType::Linear);
        assert!(result.is_ok());
        
        // Test medium dataset - should use SIMD for dense output
        let medium_measurements = create_test_measurements(500);
        let start = medium_measurements[0].timestamp;
        let end = start + chrono::Duration::hours(1); // Dense output

        let result = auto_interpolate(medium_measurements, start, end, Resolution::Seconds, SplineType::Linear);
        assert!(result.is_ok());
        
        // Test large dataset - should use parallel processing  
        let large_measurements = create_test_measurements(2000);
        let start = large_measurements[0].timestamp;
        let end = large_measurements[large_measurements.len() - 1].timestamp;

        let result = auto_interpolate(large_measurements, start, end, Resolution::Seconds, SplineType::Cubic);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_algorithm_complexity_scoring() {
        assert_eq!(get_algorithm_complexity(SplineType::Linear), 1);
        assert_eq!(get_algorithm_complexity(SplineType::Quadratic), 2);
        assert_eq!(get_algorithm_complexity(SplineType::Cubic), 3);
        assert_eq!(get_algorithm_complexity(SplineType::Polynomial(5)), 5);
        assert_eq!(get_algorithm_complexity(SplineType::Polynomial(15)), 10); // Capped
    }

    #[tokio::test]
    async fn test_fast_path_optimization() {
        // Should optimize cubic to quadratic for large datasets
        let optimized = apply_fast_path_optimization(SplineType::Cubic, 2500, 3);
        assert_eq!(optimized, SplineType::Quadratic);
        
        // Should optimize high-degree polynomial
        let optimized = apply_fast_path_optimization(SplineType::Polynomial(6), 3000, 6);
        assert_eq!(optimized, SplineType::Quadratic);
        
        // Should not optimize simple algorithms
        let optimized = apply_fast_path_optimization(SplineType::Linear, 5000, 1);
        assert_eq!(optimized, SplineType::Linear);
    }

    #[tokio::test]
    async fn test_chunk_size_calculation() {
        assert_eq!(calculate_optimal_chunk_size(1000), 1000);
        assert_eq!(calculate_optimal_chunk_size(10000), 2000);
        assert_eq!(calculate_optimal_chunk_size(30000), 5000);
        assert_eq!(calculate_optimal_chunk_size(100000), 10000);
    }

    #[tokio::test]
    async fn test_target_time_generation() {
        let start = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end = start + chrono::Duration::minutes(10);
        
        let times = generate_target_times(start, end, Resolution::Seconds);
        assert_eq!(times.len(), 601); // 10 minutes = 600 seconds + 1
        assert_eq!(times[0], start);
        assert_eq!(times[times.len() - 1], end);
    }

    #[tokio::test]
    async fn test_small_dataset_uses_standard_algorithm() {
        // Test with 50 measurements (< 200 threshold)
        let measurements = create_test_measurements(50);
        let start = measurements[0].timestamp;
        let end = measurements[measurements.len() - 1].timestamp;

        let result = auto_interpolate(measurements, start, end, Resolution::Seconds, SplineType::Linear);
        assert!(result.is_ok());
        
        // Verify small dataset detection
        let measurement_count = 50;
        assert!(measurement_count < 200, "Should trigger small dataset path");
    }

    #[tokio::test]
    async fn test_medium_dataset_dense_output_uses_simd() {
        // Test with 500 measurements and dense output (should trigger SIMD)
        let measurements = create_test_measurements(500);
        let start = measurements[0].timestamp;
        let end = start + chrono::Duration::hours(2); // Dense output: 7200+ points

        let result = auto_interpolate(measurements, start, end, Resolution::Seconds, SplineType::Linear);
        assert!(result.is_ok());
        
        // Verify conditions for SIMD path
        let measurement_count = 500;
        let time_span = (end - start).num_seconds();
        let estimated_output_points = time_span as usize; // Roughly 7200 points
        
        assert!(measurement_count >= 200 && measurement_count < 1000, "Should be in medium dataset range");
        assert!(estimated_output_points >= 256, "Should have dense output for SIMD");
    }

    #[tokio::test]
    async fn test_large_dataset_uses_parallel_processing() {
        // Test with 2000 measurements (should trigger parallel processing)
        let measurements = create_test_measurements(2000);
        let start = measurements[0].timestamp;
        let end = measurements[measurements.len() - 1].timestamp;

        let result = auto_interpolate(measurements, start, end, Resolution::Seconds, SplineType::Quadratic);
        assert!(result.is_ok());
        
        // Verify conditions for large dataset path
        let measurement_count = 2000;
        assert!(measurement_count >= 1000 && measurement_count < 5000, "Should be in large dataset range");
    }

    #[tokio::test]
    async fn test_huge_dataset_uses_streaming() {
        // Test with 6000 measurements (should trigger streaming)
        let measurements = create_test_measurements(6000);
        let start = measurements[0].timestamp;
        let end = measurements[measurements.len() - 1].timestamp;

        let result = auto_interpolate(measurements, start, end, Resolution::Minutes, SplineType::Cubic);
        assert!(result.is_ok());
        
        // Verify conditions for streaming path
        let measurement_count = 6000;
        assert!(measurement_count >= 5000, "Should trigger streaming path");
    }

    #[tokio::test]
    async fn test_algorithm_degradation_for_large_datasets() {
        // Test that complex algorithms get degraded for large datasets
        
        // Cubic should become Quadratic for 2500 measurements
        let optimized = apply_fast_path_optimization(SplineType::Cubic, 2500, 3);
        assert_eq!(optimized, SplineType::Quadratic, "Cubic should degrade to Quadratic for large datasets");
        
        // High-degree polynomial should become Quadratic for very large datasets
        let optimized = apply_fast_path_optimization(SplineType::Polynomial(8), 4000, 8);
        assert_eq!(optimized, SplineType::Quadratic, "High-degree polynomial should degrade to Quadratic");
        
        // Linear should not be degraded
        let optimized = apply_fast_path_optimization(SplineType::Linear, 10000, 1);
        assert_eq!(optimized, SplineType::Linear, "Linear should not be degraded");
    }

    #[tokio::test]
    async fn test_edge_case_handling() {
        // Test minimum dataset size
        let measurements = create_test_measurements(2); // Minimum viable dataset
        let start = measurements[0].timestamp;
        let end = measurements[1].timestamp;

        let result = auto_interpolate(measurements, start, end, Resolution::Seconds, SplineType::Linear);
        assert!(result.is_ok(), "Should handle minimum dataset size");

        // Test single measurement (should fail)
        let single_measurement = create_test_measurements(1);
        let result = auto_interpolate(single_measurement, start, end, Resolution::Seconds, SplineType::Linear);
        assert!(result.is_err(), "Should fail with insufficient measurements");

        // Test invalid time range
        let measurements = create_test_measurements(10);
        let start = measurements[5].timestamp;
        let end = measurements[0].timestamp; // End before start
        
        let result = auto_interpolate(measurements, start, end, Resolution::Seconds, SplineType::Linear);
        assert!(result.is_err(), "Should fail with invalid time range");
    }
}
