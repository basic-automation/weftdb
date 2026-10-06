use anyhow::Result;

use crate::{AspectId, Batch, BatchId, InputMeasurement, MeasurementId, TxId};

/// Trait for database structure operations
/// This trait defines the operations related to managing the structure of the database.
///
/// Add a Subject for observation -> track various aspects of the subject
#[async_trait::async_trait]
pub trait Modifiers {
	// Measurements

	/// Remove all measurements for an aspect
	async fn warning_clear_measurements(&self, aspect_id: &AspectId) -> Result<TxId>;

	/// Remove a specific measurement for an aspect
	async fn uncapture_measurement(&self, aspect_id: &AspectId, measurement_id: &MeasurementId) -> Result<TxId>;

	/// Modify an existing measurement for an aspect
	async fn recapture_measurement(&self, aspect_id: &AspectId, measurement_id: &MeasurementId, new_measurement: InputMeasurement) -> Result<TxId>;

	// Unprocessed Batches

	/// Remove all unprocessed batches for an aspect
	async fn warning_clear_unprocessed_batches(&self, aspect_id: &AspectId) -> Result<TxId>;

	/// Remove a specific unprocessed batch
	async fn remove_unprocessed_batch(&self, aspect_id: &AspectId, batch_id: &BatchId) -> Result<TxId>;

	/// Modify a specific unprocessed batch
	async fn modify_unprocessed_batch(&self, aspect_id: &AspectId, batch_id: &BatchId, new_batch: Batch) -> Result<TxId>;

	// Processed Batches

	/// Remove all processed batches for an aspect
	async fn warning_clear_processed_batches(&self, aspect_id: &AspectId) -> Result<TxId>;

	/// Remove a processed batch from the queue (after it has been processed into patterns)
	async fn remove_processed_batch(&self, aspect_id: &AspectId, batch_id: &BatchId) -> Result<TxId>;

	/// Modify a specific processed batch
	async fn modify_processed_batch(&self, aspect_id: &AspectId, batch_id: &BatchId, new_batch: Batch) -> Result<TxId>;
}
