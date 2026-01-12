use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Pipeline runtime state stored in pipeline.db.
///
/// Tracks execution history and version information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineState {
	/// Last successful run timestamp
	pub last_run: Option<DateTime<Utc>>,
	/// Total number of pipeline runs
	pub run_count: u64,
	/// Schema version for future migrations
	pub version: u32,
}

impl PipelineState {
	/// Current schema version.
	pub const CURRENT_VERSION: u32 = 1;

	/// Creates a new pipeline state with default values.
	#[must_use]
	pub const fn new() -> Self {
		Self { last_run: None, run_count: 0, version: Self::CURRENT_VERSION }
	}

	/// Increments the run count and updates the last run timestamp.
	pub fn record_run(&mut self) {
		self.run_count += 1;
		self.last_run = Some(Utc::now());
	}
}

impl Default for PipelineState {
	fn default() -> Self {
		Self::new()
	}
}
