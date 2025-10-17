use anyhow::Result;

use crate::{AspectId, Batch, DatasetId, InputMeasurement, TxId};

/// Trait for database structure operations
/// This trait defines the operations related to managing the structure of the database.
///
/// Add a Subject for observation -> track various aspects of the subject
#[async_trait::async_trait]
pub trait Inputs {
	// Measurements

	/// Capture a new measurement for a given aspect
	/// If a measurement with the same timestamp already exists, the average of the two values is stored.
	async fn capture_measurement(&self, aspect_id: AspectId, dataset_id: DatasetId, input_measurement: InputMeasurement) -> Result<TxId>;

	/// Capture new measurements for a given aspect
	/// If a measurement with the same timestamp already exists, an error is returned.
	async fn capture_new_measurement(&self, aspect_id: AspectId, dataset_id: DatasetId, input_measurement: InputMeasurement) -> Result<TxId>;

	/// Capture multiple measurements for a given aspect
	/// If a measurement with the same timestamp already exists, the average of the two values is stored.
	async fn batch_capture_measurements(&self, aspect_id: AspectId, dataset_id: DatasetId, input_measurements: Vec<InputMeasurement>) -> Result<Vec<TxId>>;

	/// Capture a chunk of measurements
	async fn capture_measurement_chunk(&self, conn: &turso::Connection, dataset_id: DatasetId, chunk: &[InputMeasurement], all_tx_ids: &[TxId], tx_id_offset: usize) -> Result<()>;

	/// Capture multiple measurements for a given aspect
	/// If a measurement with the same timestamp already exists, the measurement is skipped.
	async fn batch_capture_new_measurements(&self, aspect_id: AspectId, dataset_id: DatasetId, input_measurements: Vec<InputMeasurement>) -> Result<Vec<TxId>>;

	/// Capture a chunk of new measurements with batch processing
	async fn capture_new_measurement_chunk(&self, conn: &turso::Connection, dataset_id: DatasetId, chunk: &[InputMeasurement], all_tx_ids: &[TxId], tx_id_offset: usize) -> Result<Vec<TxId>>;

	// Unprocessed Batches

	/// insert unprocessed batch for a given aspect
	async fn insert_unprocessed_batch(&self, aspect_id: AspectId, batch: &Batch) -> Result<TxId>;

	// Capture multiple unprocessed batches for a given aspect
	async fn batch_insert_unprocessed_batches(&self, aspect_id: AspectId, batches: Vec<Batch>) -> Result<Vec<TxId>>;

	// Capture a chunk of batches - works for both processed and unprocessed batches
	/// The only difference is the database connection passed in
	async fn insert_batch_chunk(&self, conn: &turso::Connection, chunk: &[Batch]) -> Result<Vec<TxId>>;

	// Processed Batches

	/// insert processed batch for a given aspect
	async fn insert_processed_batch(&self, aspect_id: AspectId, batch: &Batch) -> Result<TxId>;

	// Capture multiple processed batches for a given aspect
	async fn batch_insert_processed_batches(&self, aspect_id: AspectId, batches: Vec<Batch>) -> Result<Vec<TxId>>;
}
