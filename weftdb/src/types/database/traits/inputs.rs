use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::{cache::Connection, AspectId, Batch, BatchId, Correlation, CorrelationID, DatasetId, DictionaryMetadata, Event, EventID, InputMeasurement, Pattern, PatternID, TxId, UnbatchedEntry};

/// Trait for database structure operations
/// This trait defines the operations related to managing the structure of the database.
///
/// Add a Subject for observation -> track various aspects of the subject
#[async_trait::async_trait]
#[allow(clippy::too_many_arguments)]
pub trait Inputs {
	//
	// Unbatched Measurements Queue
	//

	/// Enqueue a measurement timestamp as unbatched (to be included in future batch creation)
	async fn enqueue_unbatched_measurement(&self, aspect_id: &AspectId, data_timestamp: DateTime<Utc>) -> Result<()>;

	/// Enqueue multiple measurement timestamps as unbatched (bulk insert)
	///
	/// A timestamp that is already queued keeps its one entry, and the entry's `queued_at`
	/// moves strictly forward, so a queue consumer that read the entry before this call
	/// leaves it queued when it dequeues what it read (see [`UnbatchedEntry`]).
	async fn enqueue_unbatched_measurements(&self, aspect_id: &AspectId, data_timestamps: &[DateTime<Utc>]) -> Result<()>;

	/// Dequeue unbatched measurements after they have been included in batches
	///
	/// This removes the timestamps whatever their `queued_at`. A queue consumer dequeues
	/// with [`dequeue_unbatched_entries`](Self::dequeue_unbatched_entries) instead: this
	/// would also remove a timestamp queued again after the consumer read it, such as by
	/// an ingest whose row committed in the meantime, and that row would never be batched.
	async fn dequeue_unbatched_measurements(&self, aspect_id: &AspectId, data_timestamps: &[DateTime<Utc>]) -> Result<()>;

	/// Dequeue queue entries a consumer read (with
	/// [`Outputs::get_unbatched_entries`](crate::database::traits::Outputs::get_unbatched_entries))
	/// and batched. Each entry is removed only while it is still queued under the
	/// `queued_at` it was read with: one queued again since then stays queued for the
	/// consumer's next run (crash-consistency design, S18).
	async fn dequeue_unbatched_entries(&self, aspect_id: &AspectId, entries: &[UnbatchedEntry]) -> Result<()>;

	/// Clear all unbatched measurements for an aspect
	async fn clear_unbatched_measurements(&self, aspect_id: &AspectId) -> Result<()>;

	//
	// Measurements
	//

	/// Capture a new measurement for a given aspect
	/// If a measurement with the same timestamp already exists, the average of the two values is stored.
	///
	/// # Crash consistency (rows mode)
	///
	/// The timestamp is queued for batching before the row is inserted (write-ahead), and
	/// queued again once the row is committed. The first keeps the row queued if the call
	/// dies after the commit; the second keeps a queue consumer that ran in between, and
	/// read the timestamp before its row existed, from dequeuing it for good (a consumer
	/// dequeues only the entries it read; see [`UnbatchedEntry`]). Everything after the
	/// commit (queuing again, the checkpoint, the dirty-region marking, the transaction
	/// log) is best-effort: a failure is logged and the call still returns `Ok`, since the row is
	/// stored. Residuals until rows-mode aspects move to the segment store
	/// (crash-consistency design, S19):
	///
	/// - **Duplicates on retry.** If the process dies after the commit, the row is stored
	///   although the call never returned `Ok`, and a retry stores it a second time: there
	///   is no idempotency key in rows mode.
	/// - **A consumer during the call, then a crash.** The row is left unqueued only if a
	///   consumer ran during the call and the call then died, or could not queue the
	///   timestamp again, between its commit and the second enqueue.
	async fn capture_measurement(&self, aspect_id: &AspectId, dataset_id: &DatasetId, input_measurement: &InputMeasurement) -> Result<TxId>;

	/// Capture new measurements for a given aspect
	/// If a measurement with the same timestamp already exists, an error is returned.
	async fn capture_new_measurement(&self, aspect_id: &AspectId, dataset_id: &DatasetId, input_measurement: &InputMeasurement) -> Result<TxId>;

	/// Capture multiple measurements for a given aspect
	/// If a measurement with the same timestamp already exists, the average of the two values is stored.
	///
	/// # Crash consistency (rows mode)
	///
	/// Every timestamp is queued for batching before the first row is inserted
	/// (write-ahead), and each chunk's timestamps are queued again once that chunk is
	/// committed. The rows are inserted in chunks (2,500 rows, or 5,000 above 100,000),
	/// each committed on its own. The write-ahead enqueue keeps the committed chunks of a
	/// call that fails or dies midway queued; the second enqueue keeps a queue consumer
	/// that ran during the call, and read timestamps before their rows existed, from
	/// dequeuing them for good (a consumer dequeues only the entries it read; see
	/// [`UnbatchedEntry`]). Residuals until rows-mode aspects move to the segment store
	/// (crash-consistency design, S19):
	///
	/// - **Partial prefix.** An error or crash mid-call leaves the chunks committed before
	///   it stored, although the call does not return `Ok`.
	/// - **Duplicates on retry.** There is no idempotency key, so retrying such a call, or
	///   one whose `Ok` was lost, stores the already committed rows a second time.
	/// - **A consumer during the call, then a crash.** A chunk's rows are left unqueued
	///   only if a consumer ran during the call and the call then died, or could not queue
	///   them again, between the chunk's commit and its second enqueue.
	///
	/// A queued timestamp whose row never lands is handled by the consumer like any other:
	/// inside the stored range its windows are built from the interpolated series, as a
	/// full rebuild builds them, and it is dequeued; past the range it waits until rows
	/// reach its windows; before the range it has no window, and it waits until a run
	/// finds nothing else queued, which clears it.
	///
	/// Everything after a chunk's commit (queuing again, and after the last chunk the
	/// checkpoint, the dirty-region marking and the aspect's earliest and latest
	/// timestamps) is best-effort: a failure is logged and the call still returns `Ok`, so a stored batch
	/// is not retried into a duplicate.
	async fn batch_capture_measurements(&self, aspect_id: AspectId, dataset_id: DatasetId, input_measurements: Vec<InputMeasurement>) -> Result<Vec<TxId>>;

	/// Capture a chunk of measurements - generates `TxIds` internally
	async fn capture_measurement_chunk(&self, aspect_id: &AspectId, dataset_id: DatasetId, chunk: &[InputMeasurement]) -> Result<Vec<TxId>>;

	/// Capture multiple measurements for a given aspect
	/// If a measurement with the same timestamp already exists, the measurement is skipped.
	async fn batch_capture_new_measurements(&self, aspect_id: &AspectId, dataset_id: &DatasetId, input_measurements: Vec<InputMeasurement>) -> Result<Vec<TxId>>;

	/// Capture a chunk of new measurements with batch processing
	async fn capture_new_measurement_chunk(&self, aspect_id: &AspectId, db: &turso::Database, db_path: &str, dataset_id: &DatasetId, chunk: &[InputMeasurement], all_tx_ids: &[TxId], tx_id_offset: usize) -> Result<Vec<TxId>>;

	//
	// Unprocessed Batches
	//

	/// insert unprocessed batch for a given aspect
	///
	/// A batch whose `(aspect_id, batch_hash)` is already queued or already processed is
	/// skipped, and the call still returns `Ok`, so a queue consumer that crashed before it
	/// dequeued its timestamps can re-run without queuing its batches twice.
	async fn insert_unprocessed_batch(&self, aspect_id: &AspectId, batch: &Batch) -> Result<TxId>;

	/// Capture multiple unprocessed batches for a given aspect, skipping, as
	/// [`insert_unprocessed_batch`](Self::insert_unprocessed_batch) does, every batch that
	/// is already queued or processed (and repeats within `batches`).
	async fn batch_insert_unprocessed_batches(&self, aspect_id: &AspectId, batches: Vec<Batch>) -> Result<Vec<TxId>>;

	// Capture a chunk of batches - works for both processed and unprocessed batches
	/// The only difference is the database connection passed in
	async fn insert_batch_chunk(&self, conn: &mut Connection, chunk: &[Batch]) -> Result<Vec<TxId>>;

	/// remove unprocessed batch for a given aspect
	async fn remove_unprocessed_batch(&self, aspect_id: &AspectId, batch_id: &BatchId) -> Result<TxId>;

	/// Bulk remove unprocessed batches for a given aspect (single transaction)
	async fn bulk_remove_unprocessed_batches(&self, aspect_id: &AspectId, batch_ids: &[BatchId]) -> Result<()>;

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

	/// Bulk insert processed batches using a single transaction with multi-row INSERT
	async fn bulk_insert_processed_batches(&self, aspect_id: &AspectId, batches: &[Batch]) -> Result<()>;

	/// remove processed batch for a given aspect
	async fn remove_processed_batch(&self, aspect_id: &AspectId, batch_id: &BatchId) -> Result<TxId>;

	/// Bulk remove processed batches for a given aspect (single transaction)
	async fn bulk_remove_processed_batches(&self, aspect_id: &AspectId, batch_ids: &[BatchId]) -> Result<()>;

	/// clear all processed batches for a given aspect
	async fn clear_processed_batches(&self, aspect_id: &AspectId) -> Result<TxId>;

	/// cleanup processed batches older than the specified timestamp for a given aspect
	async fn cleanup_processed_batches(&self, aspect_id: &AspectId, older_than: chrono::DateTime<chrono::Utc>) -> Result<TxId>;

	/// Move batches from unprocessed to processed in bulk (atomic operation)
	/// This is more efficient than separate insert + delete for each batch
	async fn move_batches_to_processed(&self, aspect_id: &AspectId, batches: &[Batch]) -> Result<()>;

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

	//
	// Compression
	//

	/// Replace measurements in a time range with new compressed measurements.
	/// This is an atomic delete + insert operation used during compression.
	///
	/// # Arguments
	/// * `aspect_id` - The aspect containing the measurements
	/// * `dataset_id` - The dataset ID for the new measurements
	/// * `start` - Start of the time range (inclusive)
	/// * `end` - End of the time range (inclusive)
	/// * `new_measurements` - The compressed measurements to insert
	async fn replace_measurements_in_range(&self, aspect_id: &AspectId, dataset_id: &DatasetId, start: DateTime<Utc>, end: DateTime<Utc>, new_measurements: Vec<InputMeasurement>) -> Result<()>;
}
