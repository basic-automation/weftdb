//! Pipeline persistence traits for database operations.

use anyhow::Result;

use crate::{AspectId, DetectorMetadata, PipelineConfig, PipelineState};

/// Trait for pipeline write operations.
#[async_trait::async_trait]
pub trait PipelineInputs {
	/// Save or update pipeline configuration.
	/// Creates the config row if it doesn't exist, updates it otherwise.
	async fn save_pipeline_config(&self, aspect_id: &AspectId, config: &PipelineConfig) -> Result<()>;

	/// Save or update pipeline state.
	/// Creates the state row if it doesn't exist, updates it otherwise.
	async fn save_pipeline_state(&self, aspect_id: &AspectId, state: &PipelineState) -> Result<()>;

	/// Delete pipeline configuration and state.
	/// This fully removes the pipeline from the database.
	async fn delete_pipeline(&self, aspect_id: &AspectId) -> Result<()>;

	/// Add a dictionary to the pipeline.
	/// Does nothing if the dictionary is already added.
	async fn add_pipeline_dictionary(&self, aspect_id: &AspectId, dictionary_name: &str) -> Result<()>;

	/// Remove a dictionary from the pipeline.
	async fn remove_pipeline_dictionary(&self, aspect_id: &AspectId, dictionary_name: &str) -> Result<()>;

	/// Clear all dictionaries from the pipeline.
	async fn clear_pipeline_dictionaries(&self, aspect_id: &AspectId) -> Result<()>;

	/// Save detector metadata for re-registration hints.
	/// Replaces existing metadata if `detector_id` already exists.
	async fn add_pipeline_detector(&self, aspect_id: &AspectId, metadata: &DetectorMetadata) -> Result<()>;

	/// Remove detector metadata.
	async fn remove_pipeline_detector(&self, aspect_id: &AspectId, detector_id: &str) -> Result<()>;

	/// Clear all detector metadata.
	async fn clear_pipeline_detectors(&self, aspect_id: &AspectId) -> Result<()>;
}

/// Trait for pipeline read operations.
#[async_trait::async_trait]
pub trait PipelineOutputs {
	/// Load pipeline configuration.
	/// Returns None if no configuration exists yet.
	async fn load_pipeline_config(&self, aspect_id: &AspectId) -> Result<Option<PipelineConfig>>;

	/// Load pipeline state.
	/// Returns None if no state exists yet.
	async fn load_pipeline_state(&self, aspect_id: &AspectId) -> Result<Option<PipelineState>>;

	/// List all dictionary names used by this pipeline.
	async fn list_pipeline_dictionaries(&self, aspect_id: &AspectId) -> Result<Vec<String>>;

	/// List all detector metadata for re-registration hints.
	async fn list_pipeline_detectors(&self, aspect_id: &AspectId) -> Result<Vec<DetectorMetadata>>;

	/// Get specific detector metadata by ID.
	async fn get_pipeline_detector(&self, aspect_id: &AspectId, detector_id: &str) -> Result<Option<DetectorMetadata>>;

	/// Check if the pipeline has been configured (has a config row).
	async fn is_pipeline_configured(&self, aspect_id: &AspectId) -> Result<bool>;
}
