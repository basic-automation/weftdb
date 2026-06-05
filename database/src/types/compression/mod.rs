//! Dataset compression module for reducing raw measurement storage.
//!
//! This module provides compression functionality for time-series measurement data.
//! Compression can be triggered via [`Aspect::compress()`] and is configured through
//! [`CompressionConfig`] stored in the Aspect metadata.
//!
//! ## Compression Modes
//!
//! Two compression modes are available and can be combined:
//!
//! 1. **Time-based**: Data within a "pure" time range (e.g., last 7 years) stays
//!    uncompressed. Older data gets progressively more compressed based on tiers.
//!
//! 2. **Size-based**: Compress to fit within a maximum size (e.g., 10GB),
//!    compressing oldest data most aggressively. Takes precedence over time-based.
//!
//! ## Algorithm
//!
//! Compression works by:
//! 1. Interpolating data to a coarser resolution (reducing point count)
//! 2. Applying slope-change simplification to remove redundant points
//! 3. Higher aggressiveness = coarser resolution = fewer points
//!
//! ## Example
//!
//! ```rust,ignore
//! use database::{CompressionConfig, TimeBasedCompressionConfig};
//! use chrono::Duration;
//!
//! // Create aspect with compression config
//! let config = CompressionConfig::time_based(
//!     TimeBasedCompressionConfig::default_seven_years()
//! );
//!
//! let aspect = database.track_aspect(
//!     &subject_id,
//!     "temperature",
//!     &Resolution::Hours,
//!     Some(config),
//! ).await?;
//!
//! // Run compression
//! let summary = aspect.compress(&database).await?;
//! ```

pub mod algorithm;
pub mod config;
pub mod size;

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use config::{AggressivenessScaling, CompressionConfig, SizeBasedCompressionConfig, TimeBasedCompressionConfig};

/// Detailed compression phase information for progress tracking
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[derive(Default)]
pub enum CompressionPhase {
	/// Compression is initializing
	#[default]
 Initializing,
	/// Time-based compression in progress
	TimeBased {
		/// Current tier number (1-indexed)
		tier: u32,
		/// Total number of tiers to process
		of_tiers: u32,
	},
	/// Size-based compression in progress
	SizeBased {
		/// Current iteration number (1-indexed)
		iteration: u32,
	},
	/// Final cleanup and verification
	Finalizing,
	/// Compression complete
	Complete,
}


impl std::fmt::Display for CompressionPhase {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Initializing => write!(f, "Initializing"),
			Self::TimeBased { tier, of_tiers } => write!(f, "Time-based tier {tier} of {of_tiers}"),
			Self::SizeBased { iteration } => write!(f, "Size-based iteration {iteration}"),
			Self::Finalizing => write!(f, "Finalizing"),
			Self::Complete => write!(f, "Complete"),
		}
	}
}

/// Progress update sent to UI during compression
#[derive(Debug, Clone)]
pub struct CompressionProgress {
	/// Current compression phase
	pub phase: CompressionPhase,
	/// Current tier number (for time-based) or iteration (for size-based)
	pub current_tier: u32,
	/// Total number of tiers to process
	pub total_tiers: u32,
	/// Current aggressiveness level (0.0 - 1.0)
	pub aggressiveness: f64,
	/// Start of the time range being compressed
	pub time_range_start: DateTime<Utc>,
	/// End of the time range being compressed
	pub time_range_end: DateTime<Utc>,
}

/// Callback type for progress updates during compression
pub type ProgressCallback = Arc<dyn Fn(CompressionProgress) + Send + Sync>;

/// Result of compressing a single tier or iteration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierCompressionResult {
	/// Tier number (1-indexed) or iteration number for size-based
	pub tier_number: u32,
	/// Phase this result is from
	pub phase: CompressionPhase,
	/// Number of measurements before compression
	pub original_count: usize,
	/// Number of measurements after compression
	pub compressed_count: usize,
	/// Compression ratio achieved
	pub compression_ratio: f64,
	/// Aggressiveness used for this tier
	pub aggressiveness: f64,
	/// Start of the time range compressed
	pub time_range_start: DateTime<Utc>,
	/// End of the time range compressed
	pub time_range_end: DateTime<Utc>,
	/// Time taken for this tier in milliseconds
	pub duration_ms: u64,
}

/// Extended compression summary with detailed tier results
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DetailedCompressionSummary {
	/// Basic compression summary
	pub summary: CompressionSummary,
	/// Per-tier results for detailed tracking
	pub tier_results: Vec<TierCompressionResult>,
	/// When compression started
	pub started_at: DateTime<Utc>,
	/// When compression completed
	pub completed_at: DateTime<Utc>,
	/// Total duration in milliseconds
	pub total_duration_ms: u64,
}

/// Information about the last compression run (for UI display when idle)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastCompressionInfo {
	/// Unique ID for this compression run
	pub id: String,
	/// When the compression completed
	pub completed_at: DateTime<Utc>,
	/// Number of measurements before compression
	pub original_count: usize,
	/// Number of measurements after compression
	pub compressed_count: usize,
	/// Overall compression ratio
	pub compression_ratio: f64,
	/// Final size in bytes
	pub final_size_bytes: u64,
	/// Number of time-based tiers processed
	pub time_based_tiers: usize,
	/// Number of size-based iterations
	pub size_based_iterations: usize,
	/// Duration of compression in milliseconds
	pub duration_ms: u64,
}

/// A region that needs recompression (e.g., new data inserted into compressed range)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirtyRegion {
	/// Unique ID for this dirty region
	pub id: i64,
	/// Start of the dirty region
	pub region_start: DateTime<Utc>,
	/// End of the dirty region
	pub region_end: DateTime<Utc>,
	/// When this region was marked dirty
	pub marked_at: DateTime<Utc>,
	/// Reason for marking dirty
	pub reason: String,
}

/// Result of compressing a single time range.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompressionResult {
	/// Number of measurements before compression.
	pub original_count: usize,
	/// Number of measurements after compression.
	pub compressed_count: usize,
	/// Compression ratio (1.0 - compressed/original).
	pub compression_ratio: f64,
	/// Start of the compressed time range.
	pub time_range_start: DateTime<Utc>,
	/// End of the compressed time range.
	pub time_range_end: DateTime<Utc>,
}

impl CompressionResult {
	/// Create a new compression result.
	#[must_use]
	#[allow(clippy::cast_precision_loss)]
	pub fn new(original_count: usize, compressed_count: usize, start: DateTime<Utc>, end: DateTime<Utc>) -> Self {
		let compression_ratio = if original_count > 0 { 1.0 - (compressed_count as f64 / original_count as f64) } else { 0.0 };
		Self { original_count, compressed_count, compression_ratio, time_range_start: start, time_range_end: end }
	}
}

/// Summary of a complete compression operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompressionSummary {
	/// Results from time-based compression (if configured).
	pub time_based_results: Vec<CompressionResult>,
	/// Results from size-based compression (if size constraint required additional compression).
	pub size_based_results: Vec<CompressionResult>,
	/// Final size in bytes after all compression.
	pub final_size_bytes: u64,
	/// Total measurements before compression across all ranges.
	pub total_original_count: usize,
	/// Total measurements after compression across all ranges.
	pub total_compressed_count: usize,
}

impl CompressionSummary {
	/// Calculate totals from individual results.
	pub fn calculate_totals(&mut self) {
		self.total_original_count = self.time_based_results.iter().map(|r| r.original_count).sum::<usize>() + self.size_based_results.iter().map(|r| r.original_count).sum::<usize>();
		self.total_compressed_count = self.time_based_results.iter().map(|r| r.compressed_count).sum::<usize>() + self.size_based_results.iter().map(|r| r.compressed_count).sum::<usize>();
	}

	/// Get the overall compression ratio.
	#[must_use]
	#[allow(clippy::cast_precision_loss)]
	pub fn overall_compression_ratio(&self) -> f64 {
		if self.total_original_count > 0 {
			1.0 - (self.total_compressed_count as f64 / self.total_original_count as f64)
		} else {
			0.0
		}
	}
}

/// Calculate aggressiveness for a given data age based on tier configuration.
///
/// # Arguments
/// * `data_age` - How old the data is
/// * `pure_duration` - Duration of data to keep uncompressed
/// * `tier_duration` - Duration of each compression tier
/// * `max_tiers` - Maximum number of tiers
/// * `scaling` - How aggressiveness scales across tiers
///
/// # Returns
/// Aggressiveness value from 0.0 (no compression) to 0.95 (max compression)
#[must_use]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_possible_wrap)]
pub fn calculate_tier_aggressiveness(data_age: chrono::Duration, pure_duration: chrono::Duration, tier_duration: chrono::Duration, max_tiers: u32, scaling: &AggressivenessScaling) -> f64 {
	// If within pure duration, no compression
	if data_age <= pure_duration {
		return 0.0;
	}

	// Calculate which tier this data falls into
	let age_past_pure = data_age - pure_duration;
	let tier_millis = tier_duration.num_milliseconds().max(1) as f64;
	let age_millis = age_past_pure.num_milliseconds().max(0) as f64;
	let tier = ((age_millis / tier_millis).floor() as u32).min(max_tiers);

	if tier == 0 {
		return 0.0;
	}

	let aggressiveness = match scaling {
		AggressivenessScaling::Linear => {
			// Linear: tier 1 = 0.1, tier 2 = 0.2, etc. (with max_tiers = 10)
			f64::from(tier) / f64::from(max_tiers)
		}
		AggressivenessScaling::Exponential { base } => {
			// Exponential: 1 - (1 - base)^tier
			// With base 0.5: tier 1 = 0.5, tier 2 = 0.75, tier 3 = 0.875
			1.0 - (1.0 - base).powi(tier as i32)
		}
		AggressivenessScaling::Custom(tiers) => {
			// Use custom tier values
			tiers.get(tier as usize - 1).copied().unwrap_or(0.95)
		}
	};

	// Cap at 0.95 to always keep some data structure
	aggressiveness.min(0.95)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_tier_aggressiveness_within_pure() {
		let aggressiveness = calculate_tier_aggressiveness(
			chrono::Duration::days(365),     // 1 year old
			chrono::Duration::days(365 * 7), // 7 year pure
			chrono::Duration::days(365),
			10,
			&AggressivenessScaling::default(),
		);
		assert!(aggressiveness.abs() < f64::EPSILON);
	}

	#[test]
	fn test_tier_aggressiveness_exponential() {
		// 8 years old, 7 year pure = 1 year past pure = tier 1
		let aggressiveness = calculate_tier_aggressiveness(chrono::Duration::days(365 * 8), chrono::Duration::days(365 * 7), chrono::Duration::days(365), 10, &AggressivenessScaling::Exponential { base: 0.5 });
		assert!((aggressiveness - 0.5).abs() < 0.01);
	}

	#[test]
	fn test_tier_aggressiveness_linear() {
		// 8 years old = tier 1 of 10 = 0.1
		let aggressiveness = calculate_tier_aggressiveness(chrono::Duration::days(365 * 8), chrono::Duration::days(365 * 7), chrono::Duration::days(365), 10, &AggressivenessScaling::Linear);
		assert!((aggressiveness - 0.1).abs() < 0.01);
	}

	#[test]
	fn test_compression_result() {
		let result = CompressionResult::new(1000, 250, Utc::now() - chrono::Duration::days(30), Utc::now());
		assert_eq!(result.original_count, 1000);
		assert_eq!(result.compressed_count, 250);
		assert!((result.compression_ratio - 0.75).abs() < 0.01);
	}
}
