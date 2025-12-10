use anyhow::Result;

use crate::{cache::Connection, AspectId, Batch, BatchId, Correlation, CorrelationID, DatasetId, DictionaryMetadata, Event, EventID, InputMeasurement, Pattern, PatternID, TxId};

/// Trait for database structure operations
/// This trait defines the operations related to managing the structure of the database.
///
/// Add a Subject for observation -> track various aspects of the subject
#[async_trait::async_trait]
pub trait Inputs {
	//
	// Measurements
	//

	/// Capture a new measurement for a given aspect
	/// If a measurement with the same timestamp already exists, the average of the two values is stored.
	async fn capture_measurement(&self, aspect_id: &AspectId, dataset_id: &DatasetId, input_measurement: &InputMeasurement) -> Result<TxId>;

	/// Capture new measurements for a given aspect
	/// If a measurement with the same timestamp already exists, an error is returned.
	async fn capture_new_measurement(&self, aspect_id: &AspectId, dataset_id: &DatasetId, input_measurement: &InputMeasurement) -> Result<TxId>;

	/// Capture multiple measurements for a given aspect
	/// If a measurement with the same timestamp already exists, the average of the two values is stored.
	async fn batch_capture_measurements(&self, aspect_id: AspectId, dataset_id: DatasetId, input_measurements: Vec<InputMeasurement>) -> Result<Vec<TxId>>;

	/// Capture a chunk of measurements - generates `TxIds` internally
	async fn capture_measurement_chunk(&self, aspect_id: &AspectId, dataset_id: DatasetId, chunk: &[InputMeasurement]) -> Result<Vec<TxId>>;

	/// Capture multiple measurements for a given aspect
	/// If a measurement with the same timestamp already exists, the measurement is skipped.
	async fn batch_capture_new_measurements(&self, aspect_id: &AspectId, dataset_id: &DatasetId, input_measurements: Vec<InputMeasurement>) -> Result<Vec<TxId>>;

	/// Capture a chunk of new measurements with batch processing
	async fn capture_new_measurement_chunk(&self, db: &turso::Database, db_path: &str, dataset_id: &DatasetId, chunk: &[InputMeasurement], all_tx_ids: &[TxId], tx_id_offset: usize) -> Result<Vec<TxId>>;

	//
	// Unprocessed Batches
	//

	/// insert unprocessed batch for a given aspect
	async fn insert_unprocessed_batch(&self, aspect_id: &AspectId, batch: &Batch) -> Result<TxId>;

	// Capture multiple unprocessed batches for a given aspect
	async fn batch_insert_unprocessed_batches(&self, aspect_id: &AspectId, batches: Vec<Batch>) -> Result<Vec<TxId>>;

	// Capture a chunk of batches - works for both processed and unprocessed batches
	/// The only difference is the database connection passed in
	async fn insert_batch_chunk(&self, conn: &mut Connection, chunk: &[Batch]) -> Result<Vec<TxId>>;

	/// remove unprocessed batch for a given aspect
	async fn remove_unprocessed_batch(&self, aspect_id: &AspectId, batch_id: &BatchId) -> Result<TxId>;

	/// clear all unprocessed batches for a given aspect
	async fn clear_unprocessed_batches(&self, aspect_id: &AspectId) -> Result<TxId>;

	/// cleanup unprocessed batches older than the specified timestamp for a given aspect
	async fn cleanup_unprocessed_batches(&self, aspect_id: &AspectId, older_than: chrono::DateTime<chrono::Utc>) -> Result<TxId>;

	//
	// Processed Batches
	//

	/// insert processed batch for a given aspect
	async fn insert_processed_batch(&self, aspect_id: &AspectId, batch: &Batch) -> Result<TxId>;

	// Capture multiple processed batches for a given aspect
	async fn batch_insert_processed_batches(&self, aspect_id: &AspectId, batches: Vec<Batch>) -> Result<Vec<TxId>>;

	/// remove processed batch for a given aspect
	async fn remove_processed_batch(&self, aspect_id: &AspectId, batch_id: &BatchId) -> Result<TxId>;

	/// clear all processed batches for a given aspect
	async fn clear_processed_batches(&self, aspect_id: &AspectId) -> Result<TxId>;

	/// cleanup processed batches older than the specified timestamp for a given aspect
	async fn cleanup_processed_batches(&self, aspect_id: &AspectId, older_than: chrono::DateTime<chrono::Utc>) -> Result<TxId>;

	//
	// Patterns
	//

	/// insert pattern for a given aspect
	async fn insert_pattern(&self, aspect_id: &AspectId, pattern: &Pattern) -> Result<TxId>;

	/// Capture multiple patterns for a given aspect
	async fn batch_insert_patterns(&self, aspect_id: &AspectId, patterns: Vec<Pattern>) -> Result<Vec<TxId>>;

	/// remove pattern for a given aspect
	async fn remove_pattern(&self, aspect_id: &AspectId, pattern_id: &PatternID) -> Result<TxId>;

	/// clear all patterns for a given aspect
	async fn clear_patterns(&self, aspect_id: &AspectId) -> Result<TxId>;

	//
	// Events
	//

	/// insert event for a given aspect
	async fn insert_event(&self, aspect_id: &AspectId, event: &Event) -> Result<TxId>;

	/// Capture multiple events for a given aspect
	async fn batch_insert_events(&self, aspect_id: &AspectId, events: Vec<Event>) -> Result<Vec<TxId>>;

	/// remove event for a given aspect
	async fn remove_event(&self, aspect_id: &AspectId, event_id: &EventID) -> Result<TxId>;

	/// clear all events for a given aspect
	async fn clear_events(&self, aspect_id: &AspectId) -> Result<TxId>;

	//
	// Dictionary
	//

	/// update the metadata for a given dictionary
	async fn set_dictionary_metadata(&self, aspect_id: &AspectId, dictionary_name: &str, metadata: &DictionaryMetadata) -> Result<TxId>;

	/// insert pattern into dictionary for a given aspect
	async fn insert_pattern_into_dictionary(&self, aspect_id: &AspectId, dictionary_name: &str, pattern: &Pattern) -> Result<TxId>;

	/// Capture multiple patterns into dictionary for a given aspect
	async fn batch_insert_patterns_into_dictionary(&self, aspect_id: &AspectId, dictionary_name: &str, patterns: Vec<Pattern>) -> Result<Vec<TxId>>;

	//
	// Correlations
	//

	/// insert correlation for a given aspect
	async fn insert_correlation(&self, aspect_id: &AspectId, correlation: &Correlation) -> Result<TxId>;

	/// update correlation for a given aspect
	async fn update_correlation(&self, aspect_id: &AspectId, correlation: &Correlation) -> Result<TxId>;

	/// remove correlation for a given aspect
	async fn remove_correlation(&self, aspect_id: &AspectId, correlation_id: &CorrelationID) -> Result<TxId>;

	//
	// Unprocessed Events
	//

	async fn insert_unprocessed_event(&self, aspect_id: &AspectId, event: &Event) -> Result<TxId>;

	async fn remove_unprocessed_event(&self, aspect_id: &AspectId, event_id: &EventID) -> Result<TxId>;

	async fn clear_unprocessed_events(&self, aspect_id: &AspectId) -> Result<TxId>;

	async fn cleanup_unprocessed_events(&self, aspect_id: &AspectId, older_than: chrono::DateTime<chrono::Utc>) -> Result<TxId>;

	//
	// Processed Events
	//

	async fn insert_processed_event(&self, aspect_id: &AspectId, event: &Event) -> Result<TxId>;

	async fn remove_processed_event(&self, aspect_id: &AspectId, event_id: &EventID) -> Result<TxId>;

	async fn clear_processed_events(&self, aspect_id: &AspectId) -> Result<TxId>;

	async fn cleanup_processed_events(&self, aspect_id: &AspectId, older_than: chrono::DateTime<chrono::Utc>) -> Result<TxId>;
}
