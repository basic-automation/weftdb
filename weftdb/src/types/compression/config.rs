//! Compression configuration types for dataset compression.
//!
//! This module defines the configuration structures for both time-based
//! and size-based compression modes.

use chrono::Duration;
use serde::{Deserialize, Serialize};
use splimes::{Resolution, Spline};

/// Aggressiveness scaling strategy for compression tiers.
///
/// Determines how compression aggressiveness increases with data age.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AggressivenessScaling {
	/// Linear scaling: aggressiveness = tier / `max_tiers`
	Linear,
	/// Exponential scaling: aggressiveness = 1 - (1 - base)^tier
	/// With base 0.5: tier 1 = 0.5, tier 2 = 0.75, tier 3 = 0.875, etc.
	Exponential {
		/// Base value for exponential calculation (typically 0.5)
		base: f64,
	},
	/// Custom fixed aggressiveness values per tier
	Custom(Vec<f64>),
}

impl Default for AggressivenessScaling {
	fn default() -> Self {
		Self::Exponential { base: 0.5 }
	}
}

/// Time-based compression configuration.
///
/// Data within the "pure" time range stays uncompressed; older data
/// gets progressively more compressed based on tiers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimeBasedCompressionConfig {
	/// Duration of data to keep uncompressed (e.g., 7 years).
	/// Data newer than this threshold will not be compressed.
	#[serde(with = "duration_serde")]
	pub pure_duration: Duration,

	/// Duration of each compression tier (e.g., 1 year per tier).
	/// Data is grouped into tiers based on how far past the `pure_duration` it is.
	#[serde(with = "duration_serde")]
	pub tier_duration: Duration,

	/// Maximum number of compression tiers (limits aggressiveness).
	/// Data older than `pure_duration` + (`max_tiers` * `tier_duration`) gets max compression.
	pub max_tiers: u32,

	/// How aggressiveness scales across tiers.
	pub scaling: AggressivenessScaling,
}

impl TimeBasedCompressionConfig {
	/// Create a new time-based compression config.
	#[must_use]
	pub fn new(pure_duration: Duration, tier_duration: Duration) -> Self {
		Self { pure_duration, tier_duration, max_tiers: 10, scaling: AggressivenessScaling::default() }
	}

	/// Create with 7-year pure zone and 1-year tiers (common default).
	#[must_use]
	pub fn default_seven_years() -> Self {
		Self::new(Duration::days(365 * 7), Duration::days(365))
	}

	/// Set custom scaling strategy.
	#[must_use]
	pub fn with_scaling(mut self, scaling: AggressivenessScaling) -> Self {
		self.scaling = scaling;
		self
	}

	/// Set maximum number of tiers.
	#[must_use]
	pub const fn with_max_tiers(mut self, max_tiers: u32) -> Self {
		self.max_tiers = max_tiers;
		self
	}
}

/// Size-based compression configuration.
///
/// Compress to fit within a maximum size, compressing oldest data most aggressively.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SizeBasedCompressionConfig {
	/// Target maximum size in bytes (e.g., 10GB = 10 * 1024^3).
	pub target_size_bytes: u64,

	/// Minimum aggressiveness to apply when size exceeded (0.0-1.0).
	pub min_aggressiveness: f64,

	/// Maximum aggressiveness to apply (0.0-1.0).
	pub max_aggressiveness: f64,

	/// How aggressiveness scales with data age within the compression pass.
	pub scaling: AggressivenessScaling,

	/// Number of iterations to approach target size.
	/// Each iteration compresses more aggressively if target not reached.
	pub max_iterations: u32,
}

impl SizeBasedCompressionConfig {
	/// Create a new size-based compression config with the given target size.
	#[must_use]
	pub fn new(target_size_bytes: u64) -> Self {
		Self { target_size_bytes, min_aggressiveness: 0.1, max_aggressiveness: 0.95, scaling: AggressivenessScaling::default(), max_iterations: 5 }
	}

	/// Create with 10GB target.
	#[must_use]
	pub fn default_10gb() -> Self {
		Self::new(10 * 1024 * 1024 * 1024)
	}

	/// Create with 1GB target.
	#[must_use]
	pub fn default_1gb() -> Self {
		Self::new(1024 * 1024 * 1024)
	}
}

/// Main compression configuration.
///
/// Both time-based and size-based can be configured simultaneously.
/// Execution order:
/// 1. Apply time-based compression first (if configured)
/// 2. Check if size constraint is met
/// 3. If size still exceeds target, apply size-based compression (more aggressive)
///
/// Size-based takes precedence in conflicts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompressionConfig {
	/// Whether compression is enabled.
	pub enabled: bool,

	/// Time-based compression configuration (applied first).
	pub time_based: Option<TimeBasedCompressionConfig>,

	/// Size-based compression configuration (takes precedence if size exceeded).
	pub size_based: Option<SizeBasedCompressionConfig>,

	/// Coarsest resolution at maximum aggressiveness (e.g., Days).
	/// Data will be interpolated to this resolution when aggressiveness = 1.0.
	pub base_resolution: Resolution,

	/// Spline method for interpolation during compression.
	pub interpolation_method: Spline,
}

impl Default for CompressionConfig {
	fn default() -> Self {
		Self { enabled: false, time_based: None, size_based: None, base_resolution: Resolution::Days, interpolation_method: Spline::Linear }
	}
}

impl CompressionConfig {
	/// Create a new compression config with time-based compression only.
	#[must_use]
	pub fn time_based(config: TimeBasedCompressionConfig) -> Self {
		Self { enabled: true, time_based: Some(config), size_based: None, ..Default::default() }
	}

	/// Create a new compression config with size-based compression only.
	#[must_use]
	pub fn size_based(config: SizeBasedCompressionConfig) -> Self {
		Self { enabled: true, time_based: None, size_based: Some(config), ..Default::default() }
	}

	/// Create a new compression config with both modes.
	#[must_use]
	pub fn combined(time_based: TimeBasedCompressionConfig, size_based: SizeBasedCompressionConfig) -> Self {
		Self { enabled: true, time_based: Some(time_based), size_based: Some(size_based), ..Default::default() }
	}

	/// Set the base resolution (coarsest resolution at max aggressiveness).
	#[must_use]
	pub const fn with_base_resolution(mut self, resolution: Resolution) -> Self {
		self.base_resolution = resolution;
		self
	}

	/// Set the interpolation method.
	#[must_use]
	pub const fn with_interpolation_method(mut self, method: Spline) -> Self {
		self.interpolation_method = method;
		self
	}

	/// Check if any compression mode is configured.
	#[must_use]
	pub const fn has_compression(&self) -> bool {
		self.enabled && (self.time_based.is_some() || self.size_based.is_some())
	}
}

/// Custom serialization for `chrono::Duration` since it doesn't implement Serialize by default.
mod duration_serde {
	use chrono::Duration;
	use serde::{Deserialize, Deserializer, Serialize, Serializer};

	#[derive(Serialize, Deserialize)]
	struct DurationRepr {
		secs: i64,
		nanos: i32,
	}

	pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		let repr = DurationRepr { secs: duration.num_seconds(), nanos: duration.subsec_nanos() };
		repr.serialize(serializer)
	}

	pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
	where
		D: Deserializer<'de>,
	{
		let repr = DurationRepr::deserialize(deserializer)?;
		Ok(Duration::seconds(repr.secs) + Duration::nanoseconds(i64::from(repr.nanos)))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_time_based_config_creation() {
		let config = TimeBasedCompressionConfig::default_seven_years();
		assert_eq!(config.pure_duration, Duration::days(365 * 7));
		assert_eq!(config.tier_duration, Duration::days(365));
		assert_eq!(config.max_tiers, 10);
	}

	#[test]
	fn test_size_based_config_creation() {
		let config = SizeBasedCompressionConfig::default_10gb();
		assert_eq!(config.target_size_bytes, 10 * 1024 * 1024 * 1024);
		assert!((config.min_aggressiveness - 0.1).abs() < f64::EPSILON);
		assert!((config.max_aggressiveness - 0.95).abs() < f64::EPSILON);
	}

	#[test]
	fn test_compression_config_combined() {
		let config = CompressionConfig::combined(TimeBasedCompressionConfig::default_seven_years(), SizeBasedCompressionConfig::default_10gb());
		assert!(config.enabled);
		assert!(config.time_based.is_some());
		assert!(config.size_based.is_some());
		assert!(config.has_compression());
	}

	#[test]
	fn test_config_serialization() {
		let config = CompressionConfig::time_based(TimeBasedCompressionConfig::default_seven_years());
		let json = serde_json::to_string(&config).expect("Failed to serialize");
		let deserialized: CompressionConfig = serde_json::from_str(&json).expect("Failed to deserialize");
		assert_eq!(config, deserialized);
	}
}
