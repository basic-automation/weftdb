use anyhow::Result;
use chrono::{DateTime, Duration, Utc};

use crate::{Error, Measurement};

pub mod cubic;
pub mod gpu;
pub mod linear;
pub mod parallel;
pub mod polynomial;
pub mod quadratic;
pub mod simd;

// Only export the main async interpolation function
pub use auto_interpolate_async as auto_interpolate;

/* #[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Nanoseconds,
    Microseconds,
    Milliseconds,
    Seconds,
    Minutes,
    Hours,
    Days,
    Weeks,
    Months,
    Years,
}

impl Resolution {
    #[must_use]
    pub const fn to_step(self) -> chrono::Duration {
	match self {
	    Self::Nanoseconds => chrono::Duration::nanoseconds(1),
	    Self::Microseconds => chrono::Duration::microseconds(1),
	    Self::Milliseconds => chrono::Duration::milliseconds(1),
	    Self::Seconds => chrono::Duration::seconds(1),
	    Self::Minutes => chrono::Duration::minutes(1),
	    Self::Hours => chrono::Duration::hours(1),
	    Self::Days => chrono::Duration::days(1),
	    Self::Weeks => chrono::Duration::weeks(1),
	    Self::Months => chrono::Duration::days(30), // Approximate 30-day month
	    Self::Years => chrono::Duration::days(365), // Approximate 365-day year
	}
    }
} */

/* #[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplineType {
    Linear,
    Quadratic,
    Cubic,
    Polynomial(usize),
}

impl SplineType {
    #[must_use]
    pub const fn degree(&self) -> usize {
	match self {
	    Self::Linear => 1,
	    Self::Quadratic => 2,
	    Self::Cubic => 3,
	    Self::Polynomial(degree) => *degree,
	}
    }

    #[must_use]
    pub const fn number_of_points_required(&self) -> usize {
	match self {
	    Self::Linear => 2,
	    Self::Quadratic => 3,
	    Self::Cubic => 4,
	    Self::Polynomial(degree) => *degree + 1, // Degree n requires n+1 points
	}
    }
} */

/* /// Apply fast path optimization to spline types based on dataset characteristics
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
#[must_use]
pub const fn apply_fast_path_optimization(spline_type: SplineType, measurement_count: usize, _complexity_factor: usize) -> SplineType {
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

	// Cubic optimizations
	SplineType::Cubic => {
	    if measurement_count > 5000 {
		// Very large datasets: degrade to linear
		SplineType::Linear
	    } else if measurement_count >= 2500 {
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
		SplineType::Polynomial(degree)
	    }
	}
    }
} */

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
pub fn streaming_interpolate(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline_type: SplineType, chunk_size: usize) -> Result<Vec<Measurement>> {
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
		let chunk_measurements: Vec<Measurement> = measurements.iter().filter(|m| m.timestamp >= (chunk_start_time - buffer_duration) && m.timestamp <= (chunk_end_time + buffer_duration)).cloned().collect();

		if chunk_measurements.len() < 2 {
			// Not enough measurements for this chunk, skip
			continue;
		}

		// Interpolate this chunk
		let chunk_result = parallel::optimized_interpolate(chunk_measurements, chunk_start_time, chunk_end_time, resolution, optimized_spline_type)?;

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

/* /// Main async interpolation function with GPU acceleration support
///
/// This is the primary entry point for all interpolation operations in the library.
/// It automatically selects the optimal interpolation strategy (GPU, CPU, SIMD, Parallel)
/// based on dataset characteristics and performance benchmarks.
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient measurements for interpolation
/// - Invalid time range
/// - GPU initialization fails (with CPU fallback)
/// - All interpolation methods fail
pub async fn auto_interpolate_async(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline_type: SplineType) -> Result<Vec<Measurement>> {
    if measurements.is_empty() {
	return Ok(Vec::new());
    }

    if start >= end {
	return Err(Error::InvalidTimeRangeError.into());
    }

    let estimated_output_points = estimate_output_points(start, end, resolution);

    // Suppress verbose logging during benchmarks by checking environment
    let verbose_logging = std::env::var("DSP_VERBOSE").is_ok() || std::env::var("RUST_LOG").is_ok();

    if verbose_logging {
	println!("🔍 Auto-interpolate: {} measurements -> {} estimated points using {:?}",
	    measurements.len(), estimated_output_points, spline_type);
    }

    // Check if GPU acceleration should be used
    let use_gpu = should_use_gpu_interpolation(measurements.len(), estimated_output_points);

    if use_gpu {
	if verbose_logging {
	    //println!("🚀 Using GPU acceleration");
	}

	// Generate target times for GPU
	let target_times = generate_target_times(start, end, resolution);

	// Try GPU interpolation with fallback - clone measurements to avoid ownership issues
	match gpu::gpu_interpolate_with_fallback(measurements.clone(), target_times, spline_type).await {
	    Ok(result) => return Ok(result),
	    Err(gpu_error) => {
		if verbose_logging {
		    println!("⚠️ GPU failed, falling back to CPU: {gpu_error}");
		}
	    }
	}
    } else if verbose_logging {
	println!("🔧 Using CPU implementation (dataset size: {}, output points: {})",
	    measurements.len(), estimated_output_points);
    }

    // CPU implementation with optimal strategy selection
    parallel::optimized_interpolate_async(measurements, start, end, resolution, spline_type).await
} */

/* /// Estimate output points based on time range and resolution (make public)
#[must_use]
pub fn estimate_output_points(start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> usize {
	let duration = end.signed_duration_since(start);

	let total_nanoseconds = duration.num_nanoseconds().unwrap_or(0).max(0); // Ensure non-negative

	#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
	let total_nanoseconds_usize = total_nanoseconds as usize; // Safe after max(0) check

	match resolution {
		Resolution::Nanoseconds => total_nanoseconds_usize,
		Resolution::Microseconds => total_nanoseconds_usize / 1_000,
		Resolution::Milliseconds => total_nanoseconds_usize / 1_000_000,
		Resolution::Seconds => total_nanoseconds_usize / 1_000_000_000,
		Resolution::Minutes => total_nanoseconds_usize / 60_000_000_000,
		Resolution::Hours => total_nanoseconds_usize / 3_600_000_000_000,
		Resolution::Days => total_nanoseconds_usize / 86_400_000_000_000,
		Resolution::Weeks => total_nanoseconds_usize / 604_800_000_000_000,    // 7 days
		Resolution::Months => total_nanoseconds_usize / 2_592_000_000_000_000, // 30 days
		Resolution::Years => total_nanoseconds_usize / 31_536_000_000_000_000, // 365 days
	}
} */

/* /// Generate target times for interpolation
fn generate_target_times(start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Vec<DateTime<Utc>> {
	let mut target_times = Vec::new();
	let mut current = start;

	let step = match resolution {
		Resolution::Nanoseconds => Duration::nanoseconds(1),
		Resolution::Microseconds => Duration::microseconds(1),
		Resolution::Milliseconds => Duration::milliseconds(1),
		Resolution::Seconds => Duration::seconds(1),
		Resolution::Minutes => Duration::minutes(1),
		Resolution::Hours => Duration::hours(1),
		Resolution::Days => Duration::days(1),
		Resolution::Weeks => Duration::weeks(1),
		Resolution::Months => Duration::days(30), // Approximate
		Resolution::Years => Duration::days(365), // Approximate
	};

	while current <= end {
		target_times.push(current);
		current += step;
	}

	target_times
} */

/// Determine if GPU should be used with optimized thresholds
#[must_use]
pub fn should_use_gpu_interpolation(measurement_count: usize, estimated_output_points: usize) -> bool {
	gpu::should_use_gpu_interpolation(measurement_count, estimated_output_points)
}

/// Calculate GPU efficiency score with optimized thresholds
#[allow(dead_code)]
fn calculate_gpu_efficiency_score(measurement_count: usize, estimated_output_points: usize) -> f64 {
	let base_score = match estimated_output_points {
		0..=25_000 => 0.4,
		25_001..=50_000 => 1.2,
		50_001..=100_000 => 1.8,
		100_001..=500_000 => 2.5,
		_ => 3.0,
	};

	// Enhanced bonus for dense output (GPU parallelism advantage)
	let density_bonus = if measurement_count > 0 {
		#[allow(clippy::cast_precision_loss)]
		let output_density = estimated_output_points as f64 / measurement_count as f64;
		if output_density >= 20.0 {
			0.4
		} else if output_density >= 10.0 {
			0.2
		} else {
			0.0
		}
	} else {
		0.0
	};

	// Enhanced bonus for larger input datasets
	let input_bonus = match measurement_count {
		1000..=5000 => 0.2,
		5001..=10000 => 0.3,
		_ => 0.4,
	};

	base_score + density_bonus + input_bonus
}
