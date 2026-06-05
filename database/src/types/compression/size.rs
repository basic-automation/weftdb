//! Size estimation utilities for compression.
//!
//! This module provides functions to estimate measurement storage size
//! and calculate compression requirements.

use anyhow::Result;

/// Estimated bytes per measurement row in `SQLite`.
///
/// Based on:
/// - UUID id (36 bytes as TEXT)
/// - UUID `dataset_id` (36 bytes as TEXT)
/// - timestamp (8 bytes as INTEGER)
/// - value (~20 bytes avg as TEXT `BigDecimal`)
/// - `SQLite` row overhead (~20%)
pub const ESTIMATED_BYTES_PER_MEASUREMENT: u64 = 120;

/// Size estimation result.
#[derive(Debug, Clone)]
pub struct SizeEstimate {
	/// Number of measurements.
	pub measurement_count: usize,
	/// Estimated size in bytes based on measurement count.
	pub estimated_bytes: u64,
	/// Actual file size on disk (if available).
	pub actual_file_size: Option<u64>,
}

impl SizeEstimate {
	/// Create a new size estimate from measurement count.
	#[must_use]
	pub const fn from_count(measurement_count: usize) -> Self {
		Self { measurement_count, estimated_bytes: measurement_count as u64 * ESTIMATED_BYTES_PER_MEASUREMENT, actual_file_size: None }
	}

	/// Create with actual file size.
	#[must_use]
	pub const fn with_actual_size(mut self, actual_size: u64) -> Self {
		self.actual_file_size = Some(actual_size);
		self
	}

	/// Get the best available size estimate.
	#[must_use]
	pub fn best_estimate(&self) -> u64 {
		self.actual_file_size.unwrap_or(self.estimated_bytes)
	}
}

/// Compression requirement calculation result.
#[derive(Debug, Clone)]
pub struct CompressionRequirement {
	/// Whether compression is needed to meet target size.
	pub needs_compression: bool,
	/// Target measurement count to achieve target size.
	pub target_measurement_count: usize,
	/// Required reduction ratio (0.0 to 1.0).
	pub required_reduction_ratio: f64,
	/// Suggested aggressiveness based on required reduction.
	pub suggested_aggressiveness: f64,
}

/// Calculate how much compression is needed to reach target size.
///
/// # Arguments
/// * `current_size` - Current size in bytes
/// * `target_size` - Target size in bytes
/// * `measurement_count` - Current number of measurements
///
/// # Returns
/// `CompressionRequirement` with calculated values
#[must_use]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn calculate_required_compression(current_size: u64, target_size: u64, measurement_count: usize) -> CompressionRequirement {
	if current_size <= target_size {
		return CompressionRequirement { needs_compression: false, target_measurement_count: measurement_count, required_reduction_ratio: 0.0, suggested_aggressiveness: 0.0 };
	}

	// Calculate target measurement count (precision loss is acceptable for size estimates)
	let size_ratio = target_size as f64 / current_size as f64;
	let target_count = (measurement_count as f64 * size_ratio).floor() as usize;

	// Required reduction ratio
	let required_reduction = 1.0 - size_ratio;

	// Suggested aggressiveness (slightly higher than required reduction to account for overhead)
	let suggested_aggressiveness = (required_reduction * 1.2).min(0.95);

	CompressionRequirement {
		needs_compression: true,
		target_measurement_count: target_count.max(2), // Keep at least 2 points
		required_reduction_ratio: required_reduction,
		suggested_aggressiveness,
	}
}

/// Estimate file size after compression.
///
/// # Arguments
/// * `original_count` - Original measurement count
/// * `aggressiveness` - Compression aggressiveness (0.0 to 1.0)
///
/// # Returns
/// Estimated size in bytes after compression
#[must_use]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn estimate_compressed_size(original_count: usize, aggressiveness: f64) -> u64 {
	// Very rough estimate: aggressiveness maps roughly to reduction ratio
	// aggressiveness 0 = 0% reduction, aggressiveness 1 = ~95% reduction
	let reduction_factor = aggressiveness * 0.95;
	let remaining_count = (original_count as f64 * (1.0 - reduction_factor)).ceil() as usize;
	remaining_count.max(2) as u64 * ESTIMATED_BYTES_PER_MEASUREMENT
}

/// Get actual file size from filesystem.
///
/// # Errors
///
/// Returns an error if the file cannot be accessed.
pub async fn get_file_size(path: &str) -> Result<u64> {
	let metadata = tokio::fs::metadata(path).await?;
	Ok(metadata.len())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_size_estimate_from_count() {
		let estimate = SizeEstimate::from_count(1000);
		assert_eq!(estimate.measurement_count, 1000);
		assert_eq!(estimate.estimated_bytes, 1000 * ESTIMATED_BYTES_PER_MEASUREMENT);
		assert!(estimate.actual_file_size.is_none());
	}

	#[test]
	fn test_compression_requirement_no_compression_needed() {
		let req = calculate_required_compression(1024, 2048, 100);
		assert!(!req.needs_compression);
		assert_eq!(req.target_measurement_count, 100);
		assert!(req.required_reduction_ratio.abs() < f64::EPSILON);
	}

	#[test]
	fn test_compression_requirement_50_percent() {
		let req = calculate_required_compression(2000, 1000, 100);
		assert!(req.needs_compression);
		assert_eq!(req.target_measurement_count, 50);
		assert!((req.required_reduction_ratio - 0.5).abs() < 0.01);
	}

	#[test]
	fn test_estimate_compressed_size() {
		let original = 1000;

		// No compression
		let size = estimate_compressed_size(original, 0.0);
		assert_eq!(size, original as u64 * ESTIMATED_BYTES_PER_MEASUREMENT);

		// Max compression - should keep at least 2 points
		let size = estimate_compressed_size(original, 1.0);
		assert!(size >= 2 * ESTIMATED_BYTES_PER_MEASUREMENT);
	}
}
