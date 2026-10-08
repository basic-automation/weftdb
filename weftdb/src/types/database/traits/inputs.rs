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
	///
	/// # Errors
	///
	/// Writing an entry that a concurrent transaction is also writing (another enqueue of
	/// the same timestamp, or a consumer's dequeue) loses an MVCC write-write conflict,
	/// returned as [`crate::Error::TransientMvccError`]: nothing was written, and a retry
	/// can succeed. Ingest retries it.
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
	///
	/// The entries are removed in short transactions (at most 5,000 entries each, one
	/// synced commit each), each retried when it loses an MVCC conflict to an enqueue of
	/// the same timestamps. An error leaves the earlier transactions committed and the rest
	/// of the entries queued.
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
	/// dequeues only the entries it read; see [`UnbatchedEntry`]). Both enqueues retry an
	/// MVCC conflict with a concurrent write to the same queue entry. Everything after the
	/// commit (queuing again, the checkpoint, the dirty-region marking, the transaction
	/// log) is best-effort: a failure is logged and the call still returns `Ok`, since the
	/// row is stored. Residuals until rows-mode aspects move to the segment store
	/// (crash-consistency design, S19):
	///
	/// - **Duplicates on retry.** If the process dies after the commit, the row is stored
	///   although the call never returned `Ok`, and a retry stores it a second time: there
	///   is no idempotency key in rows mode.
	/// - **A consumer during the call, then a crash.** The row is left unqueued only if a
	///   consumer ran during the call and the call then died, or could not queue the
	///   timestamp again within its retries, between its commit and the second enqueue.
	///
	/// A consumer that reads the timestamp during the call batches the windows around it
	/// as they are before or after the row: the first as earlier runs batched them, the
	/// second as the next run rebuilds them, so the batch dedupe (see
	/// [`insert_unprocessed_batch`](Self::insert_unprocessed_batch)) skips the repeat.
	async fn capture_measurement(&self, aspect_id: &AspectId, dataset_id: &DatasetId, input_measurement: &InputMeasurement) -> Result<TxId>;

	/// Capture new measurements for a given aspect
	/// If a measurement with the same timestamp already exists, an error is returned.
	///
	/// # Errors
	///
	/// The timestamp is queued for batching only after the row is committed (this path has
	/// no write-ahead enqueue), so an error from the enqueue (or the checkpoint before it)
	/// fails the call although the row is stored. The enqueue is an upsert, which writes an
	/// entry that is already queued (crash-consistency design, S18), so it can lose an MVCC
	/// write-write conflict to a queue consumer dequeuing that entry; such a conflict is
	/// retried for about three seconds before it fails the call.
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
	/// [`UnbatchedEntry`]). Both enqueues retry an MVCC conflict with a concurrent write to
	/// the same queue entries (a consumer's dequeue, or another ingest of the same
	/// timestamps). A consumer that batched windows from rows already committed rebuilds
	/// them after the second enqueue into identical batches, which the batch dedupe skips,
	/// whether they are still queued, processed, or already extracted (within the
	/// extracted-batch record's retention; see
	/// [`remove_extracted_batches`](Self::remove_extracted_batches)). Residuals until
	/// rows-mode aspects move to the segment store (crash-consistency design, S19):
	///
	/// - **Partial prefix.** An error or crash mid-call leaves the chunks committed before
	///   it stored, although the call does not return `Ok`.
	/// - **Duplicates on retry.** There is no idempotency key, so retrying such a call, or
	///   one whose `Ok` was lost, stores the already committed rows a second time.
	/// - **A consumer during the call, then a crash.** A chunk's rows are left unqueued
	///   only if a consumer ran during the call and the call then died, or could not queue
	///   them again within its retries, between the chunk's commit and its second enqueue.
	/// - **A consumer between two chunks of a backfill or gap fill.** A consumer run
	///   between two chunks of a call whose rows lie inside the stored range (or before
	///   it, with chunks still to come between the committed ones and the stored rows)
	///   batches the windows that span committed rows and rows still to come with the
	///   latter interpolated, and the next run batches those windows again with all their
	///   rows: two different batches, and occurrences, for one window, both kept by the
	///   dedupe. Past the stored range a window short of points is not batched, so an
	///   append is not affected.
	///
	/// A queued timestamp whose row never lands is handled by the consumer like any other:
	/// inside the stored range its windows are built from the interpolated series, as a
	/// full rebuild builds them, and it is dequeued; past the range it waits until rows
	/// reach its windows (the consumer skips such windows without interpolating them, so
	/// the timestamps of an abandoned append, up to a whole call's worth, cost each later
	/// run only the reading of their queue entries); before the range it has no window,
	/// and it waits until a run finds nothing else queued, which clears it.
	///
	/// The write-ahead enqueue upserts all of the call's timestamps in one transaction. A
	/// consumer dequeuing some of the same timestamps (a re-import of timestamps that are
	/// still queued) waits for that whole transaction, and either side can run out of
	/// conflict retries: the consumer run fails, or the call fails before it stores any
	/// row. Both are safe to re-run.
	///
	/// Everything after a chunk's commit (queuing again, and after the last chunk the
	/// checkpoint, the dirty-region marking and the aspect's earliest and latest
	/// timestamps) is best-effort: a failure is logged and the call still returns `Ok`, so a stored batch
	/// is not retried into a duplicate.
	async fn batch_capture_measurements(&self, aspect_id: AspectId, dataset_id: DatasetId, input_measurements: Vec<InputMeasurement>) -> Result<Vec<TxId>>;

	/// Capture a chunk of measurements - generates `TxIds` internally
	///
	/// # Errors
	///
	/// The chunk's timestamps are queued only after its rows are committed, so an error
	/// from the enqueue fails the call although the rows are stored; an MVCC conflict with
	/// a consumer dequeuing the same entries is retried first, as for
	/// [`capture_new_measurement`](Self::capture_new_measurement).
	async fn capture_measurement_chunk(&self, aspect_id: &AspectId, dataset_id: DatasetId, chunk: &[InputMeasurement]) -> Result<Vec<TxId>>;

	/// Capture multiple measurements for a given aspect
	/// If a measurement with the same timestamp already exists, the measurement is skipped.
	async fn batch_capture_new_measurements(&self, aspect_id: &AspectId, dataset_id: &DatasetId, input_measurements: Vec<InputMeasurement>) -> Result<Vec<TxId>>;

	/// Capture a chunk of new measurements with batch processing
	///
	/// # Errors
	///
	/// The rows are committed one by one and their timestamps queued after the last one,
	/// so an error from the enqueue fails the call although the rows are stored; an MVCC
	/// conflict with a consumer dequeuing the same entries is retried first, as for
	/// [`capture_new_measurement`](Self::capture_new_measurement).
	async fn capture_new_measurement_chunk(&self, aspect_id: &AspectId, db: &turso::Database, db_path: &str, dataset_id: &DatasetId, chunk: &[InputMeasurement], all_tx_ids: &[TxId], tx_id_offset: usize) -> Result<Vec<TxId>>;

	//
	// Unprocessed Batches
	//

	/// insert unprocessed batch for a given aspect
	///
	/// A batch whose `(aspect_id, batch_hash)` is already queued, processed, or extracted
	/// within the retention of the extracted-batch record (see
	/// [`remove_extracted_batches`](Self::remove_extracted_batches)) is skipped, and the
	/// call still returns `Ok`. So a queue consumer that rebuilds windows it already
	/// batched (after a crash before its dequeue, or once an ingest that ran during it
	/// queues its rows again) stores no second copy, and extraction yields no second
	/// occurrence of the same window.
	async fn insert_unprocessed_batch(&self, aspect_id: &AspectId, batch: &Batch) -> Result<TxId>;

	/// Capture multiple unprocessed batches for a given aspect, skipping, as
	/// [`insert_unprocessed_batch`](Self::insert_unprocessed_batch) does, every batch that
	/// is already queued, processed, or extracted (and repeats within `batches`).
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
	///
	/// A plain delete: the queue consumer may queue a removed batch again. Pattern
	/// extraction removes the batches it consumed with
	/// [`remove_extracted_batches`](Self::remove_extracted_batches) instead.
	async fn bulk_remove_processed_batches(&self, aspect_id: &AspectId, batch_ids: &[BatchId]) -> Result<()>;

	/// Remove processed batches that pattern extraction consumed, in one transaction that
	/// also records each one's `batch_hash` in the processed batches DB
	/// (`extracted_batches`), so that
	/// [`insert_unprocessed_batch`](Self::insert_unprocessed_batch) does not queue the same
	/// batch again (crash-consistency design, S18). Without the record, a window the
	/// consumer rebuilt after its batch was extracted (after a consumer crash, or an ingest
	/// that ran during a consumer run) was extracted again, as a second occurrence of the
	/// same span.
	///
	/// The record is bounded: the same transaction deletes the records older than the
	/// retention, `WEFT_EXTRACTED_BATCH_RETENTION_SECS` (a positive number of seconds;
	/// 48 hours by default), and [`clear_processed_batches`](Self::clear_processed_batches)
	/// deletes them all. Extraction consumes about one batch per resolution step, so the
	/// record holds about one row per step of the retention. The rebuilds it guards
	/// against come on the next consumer run after an ingest that overlapped a pipeline
	/// run, or on the re-run after a consumer crash, so the retention must exceed the
	/// longest interval between pipeline runs of an aspect plus the longest ingest call:
	/// a window rebuilt after its record expired is extracted again.
	async fn remove_extracted_batches(&self, aspect_id: &AspectId, batch_ids: &[BatchId]) -> Result<()>;

	/// clear all processed batches for a given aspect
	///
	/// This also clears the record of the batches extraction consumed (see
	/// [`remove_extracted_batches`](Self::remove_extracted_batches)), so that a full
	/// rebuild, which clears the batches to batch and extract every window again, is not
	/// refused by the batch dedupe.
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
