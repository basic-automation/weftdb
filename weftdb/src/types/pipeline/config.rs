use serde::{Deserialize, Serialize};
use splimes::Spline;

/// Pipeline configuration stored in pipeline.db.
///
/// Note: Resolution is NOT stored here - it comes from the Aspect.
/// This avoids duplication and ensures consistency.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineConfig {
	/// The spline interpolation method for analysis
	pub spline_method: Spline,
	/// The batch size for processing
	pub batch_size: usize,
}

impl PipelineConfig {
	/// Creates a new pipeline configuration.
	#[must_use]
	pub const fn new(spline_method: Spline, batch_size: usize) -> Self {
		Self { spline_method, batch_size }
	}
}

impl Default for PipelineConfig {
	fn default() -> Self {
		Self { spline_method: Spline::Linear, batch_size: 24 }
	}
}
