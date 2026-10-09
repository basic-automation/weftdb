use std::future::Future;

use anyhow::Result;

use crate::{
	cache::Connection, database::traits::{AspectStructure, Inputs}, error::is_transient_mvcc_error, types::{
		database::{
			helpers::safe_usize_to_f64, traits::{config::Config, connection::Connection as ConnectionTrait, DatabaseStructure}
		}, durable::{fault, FaultPoint}, TxId
	}, Aspect, AspectId, Batch, BatchId, Correlation, CorrelationID, Database, DatasetId, DictionaryConstraints, DictionaryId, DictionaryMetadata, Error, Event, EventID, InputMeasurement, Measurement, Pattern, PatternID, UnbatchedEntry
};

/// How often a write to the unbatched queue is attempted when it loses an MVCC
/// write-write conflict over the same entries (crash-consistency design, S18): ingest's
/// enqueues and a consumer's dequeue upsert and delete the same `(aspect_id,
/// data_timestamp)` rows, and two ingests into one aspect can queue the same timestamps.
/// With the backoff of [`queue_write_backoff`] the attempts span about three seconds.
///
/// That is many times a dequeue transaction (at most [`DEQUEUE_CHUNK`] entries) and the
/// second enqueue after a chunk's commit (one chunk's timestamps), which are what an
/// ingest and a consumer normally meet. It is not a bound on the write-ahead enqueue: that
/// upserts every timestamp of a `batch_capture_measurements` call in one transaction, so
/// a dequeue that meets it (only when the call re-imports timestamps that are still
/// queued and that the consumer read) waits for the whole of it and can run out of
/// attempts, failing the consumer run; the other way round the whole upsert is retried
/// and can fail the call before any row is stored. Both are recoverable: the entries
/// stay queued, and the batch dedupe makes the consumer's re-run safe.
const QUEUE_WRITE_ATTEMPTS: u32 = 10;

/// The most queue entries one dequeue transaction removes. A consumer dequeues in several
/// short transactions rather than one per run, so an ingest whose enqueue conflicts with
/// it waits for one of them, never for the whole dequeue, and a conflict retries one of
/// them. Each transaction is a synced commit, so this also sets the dequeue's commit count:
/// one per this many entries (a run used to dequeue in a single commit).
const DEQUEUE_CHUNK: usize = 5_000;

/// The environment variable that sets, in whole seconds, how long the processed batches DB
/// remembers a batch pattern extraction consumed (its `extracted_batches` row), and so
/// how long the queue's duplicate check recognises that batch (crash-consistency design,
/// S18). Unset, zero or not a number: [`DEFAULT_EXTRACTED_BATCH_RETENTION_SECS`].
const EXTRACTED_BATCH_RETENTION_ENV: &str = "WEFT_EXTRACTED_BATCH_RETENTION_SECS";

/// The default retention of the extracted-batch record: 48 hours.
///
/// The record is only consulted when the consumer rebuilds a window whose batch was
/// already extracted, which happens on the first consumer run after an ingest that
/// overlapped a pipeline run queues its committed rows again, or on the re-run after a
/// consumer crash. So it has to outlive one interval between pipeline runs, plus the
/// ingest; 48 hours covers a daily schedule with a day to spare. Extraction consumes
/// about one batch per resolution step, so the record holds about one row per step of the
/// retention (2,880 rows for a minute-resolution aspect, 172,800 for a second-resolution
/// one); before it was bounded it kept a row for every step ever extracted.
const DEFAULT_EXTRACTED_BATCH_RETENTION_SECS: i64 = 48 * 60 * 60;

/// The extracted-batch retention in milliseconds, from [`EXTRACTED_BATCH_RETENTION_ENV`],
/// read once per process.
fn extracted_batch_retention_millis() -> i64 {
	static RETENTION: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
	*RETENTION.get_or_init(|| {
		let raw = std::env::var(EXTRACTED_BATCH_RETENTION_ENV).ok();
		let secs = parse_extracted_batch_retention(raw.as_deref());
		if raw.is_some_and(|raw| raw.trim().parse::<i64>().ok() != Some(secs)) {
			tracing::warn!("{EXTRACTED_BATCH_RETENTION_ENV} is not a positive number of seconds; keeping extracted batches for the default {DEFAULT_EXTRACTED_BATCH_RETENTION_SECS} s");
		}
		secs.saturating_mul(1000)
	})
}

/// The retention, in seconds, that a raw [`EXTRACTED_BATCH_RETENTION_ENV`] value sets: a
/// positive whole number, else the default.
fn parse_extracted_batch_retention(raw: Option<&str>) -> i64 {
	raw.and_then(|raw| raw.trim().parse::<i64>().ok()).filter(|&secs| secs > 0).unwrap_or(DEFAULT_EXTRACTED_BATCH_RETENTION_SECS)
}

/// The wait before the retry that follows attempt `attempt` (from 1) of a queue write:
/// doubling from 20 ms up to 500 ms, plus up to half again of jitter, so that two writers
/// that conflicted do not retry in step.
fn queue_write_backoff(attempt: u32) -> std::time::Duration {
	let base = std::cmp::min(10u64 << attempt.min(6), 500);
	std::time::Duration::from_millis(base + fastrand::u64(0..=base / 2))
}

/// Whether `e` is an MVCC conflict that retrying the whole transaction can resolve: a
/// write-write conflict with another open transaction (Turso 0.8 reports it only as a
/// generic error, by its message), a stale snapshot or aborted commit dependency
/// (`BusySnapshot`), or a busy database.
fn is_mvcc_conflict(e: &turso::Error) -> bool {
	match e {
		turso::Error::Busy(_) | turso::Error::BusySnapshot(_) => true,
		turso::Error::Error(message) => message.contains("Write-write conflict"),
		_ => false,
	}
}

/// The error of a failed queue write: an [`Error::TransientMvccError`] when `e` is a
/// conflict that a retry can resolve (see [`is_mvcc_conflict`]), so that the retry loops
/// recognise it, and a plain error otherwise.
fn queue_write_error(context: &str, e: &turso::Error) -> anyhow::Error {
	if is_mvcc_conflict(e) {
		Error::TransientMvccError(format!("{context}: {e}")).into()
	} else {
		anyhow::anyhow!("{context}: {e}")
	}
}

/// `COMMIT` a queue write; on failure roll back and return the error classified as by
/// [`queue_write_error`]. (`commit_concurrent` returns the error as text, so a conflict
/// at the commit could not be told apart from any other failure.)
async fn commit_queue_write(conn: &Connection, context: &str) -> Result<()> {
	if let Err(e) = conn.as_ref().execute("COMMIT", turso::params![]).await {
		let _ = conn.as_ref().execute("ROLLBACK", turso::params![]).await;
		return Err(queue_write_error(&format!("{context}: the commit failed and was rolled back"), &e));
	}
	Ok(())
}

/// Run `write`, a whole queue-write transaction, again while it fails with a transient
/// MVCC conflict, up to [`QUEUE_WRITE_ATTEMPTS`] times in all, and return its last result.
/// Each attempt is a fresh transaction on a fresh snapshot, and a failed one wrote
/// nothing.
async fn retry_queue_write<F, Fut>(what: &str, mut write: F) -> Result<()>
where
	F: FnMut() -> Fut + Send,
	Fut: std::future::Future<Output = Result<()>> + Send,
{
	let mut attempt = 1;
	loop {
		match write().await {
			Err(e) if attempt < QUEUE_WRITE_ATTEMPTS && is_transient_mvcc_error(&e) => {
				tracing::debug!("{what} lost an MVCC conflict (attempt {attempt} of {QUEUE_WRITE_ATTEMPTS}), retrying: {e}");
				tokio::time::sleep(queue_write_backoff(attempt)).await;
				attempt += 1;
			}
			done => return done,
		}
	}
}

#[async_trait::async_trait]
impl Inputs for Database {
	//
	// Unbatched Measurements Queue
	//

	/// Enqueue a measurement timestamp as unbatched (to be included in future batch creation)
	async fn enqueue_unbatched_measurement(&self, aspect_id: &AspectId, data_timestamp: chrono::DateTime<chrono::Utc>) -> Result<()> {
		self.enqueue_unbatched_measurements(aspect_id, &[data_timestamp]).await
	}

	/// Enqueue multiple measurement timestamps as unbatched (bulk upsert, one entry per timestamp)
	///
	/// A timestamp that is already queued keeps its one entry, but its `queued_at` moves
	/// strictly forward (to the later of now and one past its old value). The
	/// queue consumer dequeues an entry only while it has the `queued_at` it was read with
	/// ([`dequeue_unbatched_entries`](Inputs::dequeue_unbatched_entries)), so an enqueue
	/// that lands after the consumer's read, such as ingest's second enqueue once its rows
	/// are committed, keeps the timestamp queued for the next run (crash-consistency
	/// design, S18). This used to be `INSERT OR IGNORE`, which left the old `queued_at`.
	///
	/// One attempt: an MVCC write-write conflict with another write to the same entries
	/// (a concurrent enqueue of the same timestamps, or a consumer's dequeue) is returned as
	/// an [`Error::TransientMvccError`], after which nothing was written and a retry can
	/// succeed; ingest retries it.
	async fn enqueue_unbatched_measurements(&self, aspect_id: &AspectId, data_timestamps: &[chrono::DateTime<chrono::Utc>]) -> Result<()> {
		if data_timestamps.is_empty() {
			return Ok(());
		}

		let metadata_db = self.metadata();
		let metadata_db_path = self.metadata_path();
		let conn = Self::begin_concurrent(metadata_db, metadata_db_path, Some(self.cache.clone())).await?;

		// Process in chunks to avoid SQLite variable limits
		let chunk_size = 500;
		let aspect_id_str = aspect_id.as_uuid().to_string();
		let queued_at = chrono::Utc::now().timestamp_millis();

		for chunk in data_timestamps.chunks(chunk_size) {
			let placeholder = "(?, ?, ?)";
			let placeholders: Vec<&str> = (0..chunk.len()).map(|_| placeholder).collect();
			// One entry per (aspect_id, data_timestamp), the table's UNIQUE key; a repeat moves
			// the entry's queued_at strictly forward (see above).
			let bulk_sql = format!("INSERT INTO unbatched_measurements (aspect_id, data_timestamp, queued_at) VALUES {} ON CONFLICT(aspect_id, data_timestamp) DO UPDATE SET queued_at = MAX(excluded.queued_at, unbatched_measurements.queued_at + 1)", placeholders.join(", "));

			let mut params: Vec<String> = Vec::with_capacity(chunk.len() * 3);
			for ts in chunk {
				params.push(aspect_id_str.clone());
				params.push(ts.timestamp_millis().to_string());
				params.push(queued_at.to_string());
			}

			if let Err(e) = conn.as_ref().execute(&bulk_sql, turso::params_from_iter(params)).await {
				// The conflict, not a failed rollback of the transaction it ended, is the error.
				let _ = Self::rollback_concurrent(&conn).await;
				return Err(queue_write_error("Failed to enqueue unbatched measurements", &e));
			}
		}

		commit_queue_write(&conn, "Failed to enqueue unbatched measurements").await
	}

	/// Dequeue unbatched measurements after they have been included in batches
	async fn dequeue_unbatched_measurements(&self, aspect_id: &AspectId, data_timestamps: &[chrono::DateTime<chrono::Utc>]) -> Result<()> {
		if data_timestamps.is_empty() {
			return Ok(());
		}

		let metadata_db = self.metadata();
		let metadata_db_path = self.metadata_path();
		let conn = Self::begin_concurrent(metadata_db, metadata_db_path, Some(self.cache.clone())).await?;

		// Process in chunks to avoid SQLite variable limits
		let chunk_size = 500;
		let aspect_id_str = aspect_id.as_uuid().to_string();

		for chunk in data_timestamps.chunks(chunk_size) {
			let placeholders: Vec<&str> = (0..chunk.len()).map(|_| "?").collect();
			let delete_sql = format!("DELETE FROM unbatched_measurements WHERE aspect_id = ? AND data_timestamp IN ({})", placeholders.join(", "));

			let mut params: Vec<String> = Vec::with_capacity(chunk.len() + 1);
			params.push(aspect_id_str.clone());
			for ts in chunk {
				params.push(ts.timestamp_millis().to_string());
			}

			let res = conn.as_ref().execute(&delete_sql, turso::params_from_iter(params)).await;
			if let Err(e) = res {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to dequeue unbatched measurements: {e}"));
			}
		}

		Self::commit_concurrent(&conn).await?;
		Ok(())
	}

	/// Dequeue the queue entries a consumer read, each only while it still has the
	/// `queued_at` it was read with.
	///
	/// The entries are removed in transactions of at most 5,000 (`DEQUEUE_CHUNK`), so that an
	/// ingest queuing one of the same timestamps meanwhile waits for one short transaction,
	/// not for the whole dequeue. A transaction that loses an MVCC conflict to such an
	/// enqueue is retried: it re-reads the entries, so one queued again in the meantime no
	/// longer matches and stays queued. Each transaction runs one `DELETE` per entry,
	/// prepared once: Turso seeks the `(aspect_id, data_timestamp)` key for an equality,
	/// but not for an `IN` list, which it answers by scanning the aspect's entries.
	///
	/// On an error the transactions before the failed one stay committed; the entries left
	/// queued are batched again by the next run, whose duplicate check skips the batches
	/// already stored.
	async fn dequeue_unbatched_entries(&self, aspect_id: &AspectId, entries: &[UnbatchedEntry]) -> Result<()> {
		for chunk in entries.chunks(DEQUEUE_CHUNK) {
			retry_queue_write("Dequeuing unbatched measurements", || self.dequeue_entry_chunk(aspect_id, chunk)).await?;
		}
		Ok(())
	}

	/// Clear all unbatched measurements for an aspect
	async fn clear_unbatched_measurements(&self, aspect_id: &AspectId) -> Result<()> {
		let metadata_db = self.metadata();
		let metadata_db_path = self.metadata_path();
		let conn = Self::begin_concurrent(metadata_db, metadata_db_path, Some(self.cache.clone())).await?;

		let delete_sql = "DELETE FROM unbatched_measurements WHERE aspect_id = ?";
		let res = conn.as_ref().execute(delete_sql, turso::params![aspect_id.as_uuid().to_string()]).await;
		match res {
			Ok(_) => tracing::debug!("Cleared all unbatched measurements for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to clear unbatched measurements: {e}"));
			}
		}

		Self::commit_concurrent(&conn).await?;
		Ok(())
	}

	/// Capture a new measurement for a given aspect
	/// If a measurement with the same timestamp already exists, the average of the two values is stored.
	/// Uses Turso's concurrent writes feature for better performance and conflict resolution.
	/// Also enqueues the measurement timestamp for incremental batch processing.
	///
	/// # Errors
	/// - if aspect not found
	/// - if unable to insert measurement into database
	async fn capture_measurement(&self, aspect_id: &AspectId, dataset_id: &DatasetId, input_measurement: &InputMeasurement) -> Result<TxId> {
		let db = self.get_measurement_db(aspect_id).await?;
		let db_path = self.get_measurement_db_path(aspect_id).await?;
		let tx_id = TxId::new();

		let measurement = Measurement::from_input_measurement(dataset_id, input_measurement);
		let data_timestamp = measurement.timestamp();

		// Write-ahead enqueue (crash-consistency design, S18): queue the timestamp before the
		// row is inserted. Enqueuing only after the insert left a committed but unqueued
		// row, which the incremental build could miss, whenever the call failed or crashed
		// in between. A consumer can now read the timestamp before its row exists, so it is
		// queued again after the commit (below). A timestamp whose row never lands is
		// handled by the consumer like any other (see the trait docs).
		self.enqueue_retrying(aspect_id, &[data_timestamp]).await?;
		fault::hit(FaultPoint::LEnqueued).await?;

		// Simple INSERT - no unique constraints with MVCC, duplicates handled at app level
		let insert_sql = r"
			INSERT INTO measurements (id, dataset_id, timestamp, value) 
			VALUES (?, ?, ?, ?)
		";

		// Execute with BEGIN CONCURRENT and retry on lock/conflict
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		let res = conn.as_ref().execute(insert_sql, turso::params![tx_id.as_uuid().to_string(), dataset_id.as_uuid().to_string(), measurement.timestamp().timestamp_millis(), measurement.value().to_string()]).await;
		match res {
			Ok(_) => tracing::debug!("Successfully inserted measurement for dataset {dataset_id}"),
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("SQL execution failure 1: `{e}`"));
			}
		}

		Self::commit_concurrent(&conn).await?;
		self.forget_cached_earliest_measurement(aspect_id).await;

		// The row is stored. Every step from here on is best-effort: an error must not fail a
		// call whose row is committed, since the client would retry it into a duplicate
		// (crash-consistency design, legacy-committed-before-ack).
		self.requeue_committed(aspect_id, &[data_timestamp]).await;

		// Checkpoint WAL to ensure measurement is persisted
		if let Err(e) = Self::checkpoint_wal_passive(&db).await {
			tracing::debug!("Failed to checkpoint after capturing a measurement; it is committed: {e}");
		}

		// Check if this measurement falls in a previously compressed range and mark dirty if so
		if let Err(e) = self.mark_dirty_region_if_needed(aspect_id, data_timestamp).await {
			tracing::debug!("Failed to check dirty region for measurement: {e}");
		}

		// The transaction log is an audit trail; the measurement row's own id stands in when
		// it cannot be written.
		match self.record_transaction(&format!("Captured measurement at {} with value {} for dataset {}", measurement.timestamp(), measurement.value(), dataset_id)).await {
			Ok(logged) => Ok(logged),
			Err(e) => {
				tracing::warn!("Failed to record the transaction for a measurement of aspect {aspect_id}; the measurement is stored: {e}");
				Ok(tx_id)
			}
		}
	}

	/// Capture new measurements for a given aspect
	/// Simple INSERT - duplicates handled at application level
	/// Uses Turso's concurrent writes feature for better performance.
	/// Also enqueues the measurement timestamp for incremental batch processing.
	async fn capture_new_measurement(&self, aspect_id: &AspectId, dataset_id: &DatasetId, input_measurement: &InputMeasurement) -> Result<TxId> {
		let db = self.get_measurement_db(aspect_id).await?;
		let db_path = self.get_measurement_db_path(aspect_id).await?;
		let tx_id = TxId::new();

		let measurement = Measurement::from_input_measurement(dataset_id, input_measurement);
		let data_timestamp = measurement.timestamp();

		// Simple INSERT - no unique constraints with MVCC
		let insert_sql = r"
			INSERT INTO measurements (id, dataset_id, timestamp, value) 
			VALUES (?, ?, ?, ?)
		";

		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		let res = conn.as_ref().execute(insert_sql, turso::params![tx_id.as_uuid().to_string(), dataset_id.as_uuid().to_string(), measurement.timestamp().timestamp_millis(), measurement.value().to_string()]).await;
		match res {
			Ok(rows) => tracing::debug!("Inserted {rows} rows"),
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("SQL execution failure 2: `{e}`"));
			}
		}

		Self::commit_concurrent(&conn).await?;

		// Checkpoint WAL to ensure measurement is persisted
		Self::checkpoint_wal_passive(&db).await?;

		// Enqueue measurement for incremental batch processing. The enqueue is an upsert, so
		// it writes an entry that is already queued and can lose an MVCC conflict to a
		// consumer's dequeue of it; retry that, as the write-ahead paths do (crash-consistency
		// design, S18). This path still enqueues only after the commit (see the trait docs).
		self.enqueue_retrying(aspect_id, &[data_timestamp]).await?;

		self.record_transaction(&format!("Inserted new measurement at {} for dataset {}", measurement.timestamp(), dataset_id)).await
	}

	/// Batch insert measurements for better performance using Turso's concurrent writes
	/// Uses simple INSERT - app handles duplicates since MVCC doesn't support unique constraints
	///
	/// # Errors
	/// - if aspect not found
	/// - if unable to insert measurements into database
	///
	/// # Panics
	/// - if measurements vector is empty when computing min/max (this is already checked)
	async fn batch_capture_measurements(&self, aspect_id: AspectId, dataset_id: DatasetId, input_measurements: Vec<InputMeasurement>) -> Result<Vec<TxId>> {
		if input_measurements.is_empty() {
			return Ok(Vec::new());
		}

		// Compute min/max upfront
		let min_new = input_measurements.iter().map(InputMeasurement::timestamp).min().unwrap();
		let max_new = input_measurements.iter().map(InputMeasurement::timestamp).max().unwrap();

		// Print initial progress message for large batches
		if input_measurements.len() > 100_000 {
			tracing::info!("Loading {} measurements for aspect '{}'...", input_measurements.len(), aspect_id);
		}

		// Use smaller chunks to avoid SQLite performance issues with very large SQL statements
		// 50,000 placeholders in a single INSERT can cause parsing slowdowns
		let chunk_size = if input_measurements.len() > 100_000 { 5_000 } else { 2_500 };
		let total_measurements = input_measurements.len();
		let mut all_tx_ids = Vec::with_capacity(total_measurements);

		// Get DB connection info once upfront to avoid repeated lookups
		tracing::debug!("[batch_capture] Getting measurement DB for aspect {}", aspect_id);
		let db = self.get_measurement_db(&aspect_id).await?;
		tracing::debug!("[batch_capture] Got measurement DB, getting path...");
		let db_path = self.get_measurement_db_path(&aspect_id).await?;
		let dataset_id_str = dataset_id.as_uuid().to_string();

		// Write-ahead enqueue (crash-consistency design, S18): queue every timestamp before
		// the first chunk is inserted. The chunks commit one by one, so a failure or crash
		// mid-call leaves a committed prefix; enqueuing only after the loop left that prefix
		// unqueued, and the incremental build could miss it. A consumer can now read the
		// timestamps before their rows exist, so each chunk's are queued again after it
		// commits (below). Timestamps whose rows never land are handled by the consumer like
		// any others (see the trait docs).
		tracing::debug!("[batch_capture] Enqueuing {} unbatched measurements...", input_measurements.len());
		let all_timestamps: Vec<chrono::DateTime<chrono::Utc>> = input_measurements.iter().map(InputMeasurement::timestamp).collect();
		self.enqueue_retrying(&aspect_id, &all_timestamps).await?;
		fault::hit(FaultPoint::LEnqueued).await?;
		tracing::debug!("[batch_capture] Unbatched measurements enqueued");

		tracing::debug!("[batch_capture] Starting chunk loop ({} chunks of {})", total_measurements / chunk_size + 1, chunk_size);

		for (chunk_idx, chunk) in input_measurements.chunks(chunk_size).enumerate() {
			let start = chunk_idx * chunk_size;

			// Progress reporting for large batches (report every ~10%)
			let report_interval = std::cmp::max(1, total_measurements / chunk_size / 10);
			if total_measurements > 100_000 && chunk_idx % report_interval == 0 && chunk_idx > 0 {
				if let (Ok(processed_f64), Ok(len_f64)) = (safe_usize_to_f64(start), safe_usize_to_f64(total_measurements)) {
					let pct = (processed_f64 / len_f64) * 100.0;
					tracing::info!("  {aspect_id} - {pct:.1}%");
				} else {
					tracing::info!("  {aspect_id} - processed {start} / {total_measurements} measurements");
				}
			}

			// Log first few chunks to diagnose blocking
			if chunk_idx < 3 {
				tracing::debug!("[batch_capture] Chunk {}: starting begin_concurrent...", chunk_idx);
			}

			// Inline bulk insert to reuse db handle
			let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

			if chunk_idx < 3 {
				tracing::debug!("[batch_capture] Chunk {}: begin_concurrent succeeded, building INSERT...", chunk_idx);
			}

			// Generate TxIds for this chunk only
			let chunk_tx_ids: Vec<TxId> = (0..chunk.len()).map(|_| TxId::new()).collect();

			// Build bulk INSERT statement with all measurements
			let placeholder_str = "(?, ?, ?, ?)";
			let mut placeholders_str = String::with_capacity(chunk.len() * (placeholder_str.len() + 2));
			for i in 0..chunk.len() {
				if i > 0 {
					placeholders_str.push_str(", ");
				}
				placeholders_str.push_str(placeholder_str);
			}

			let bulk_sql = format!("INSERT INTO measurements (id, dataset_id, timestamp, value) VALUES {placeholders_str}");

			// Prepare all parameters
			let mut params: Vec<String> = Vec::with_capacity(chunk.len() * 4);
			for (i, input_measurement) in chunk.iter().enumerate() {
				let measurement = Measurement::from_input_measurement(&dataset_id, input_measurement);
				params.push(chunk_tx_ids[i].as_uuid().to_string());
				params.push(dataset_id_str.clone());
				params.push(measurement.timestamp().timestamp_millis().to_string());
				params.push(measurement.value().to_string());
			}

			if chunk_idx < 3 {
				tracing::debug!("[batch_capture] Chunk {}: executing INSERT for {} measurements...", chunk_idx, chunk.len());
			}

			// Execute the bulk insert
			conn.as_ref().execute(&bulk_sql, turso::params_from_iter(params)).await.map_err(|e| Error::DatabaseError(format!("Failed to bulk insert measurements: {e}")))?;

			if chunk_idx < 3 {
				tracing::debug!("[batch_capture] Chunk {}: INSERT succeeded, committing...", chunk_idx);
			}

			Self::commit_concurrent(&conn).await?;
			self.forget_cached_earliest_measurement(&aspect_id).await;
			all_tx_ids.extend(chunk_tx_ids);
			fault::hit(FaultPoint::LChunk(u32::try_from(chunk_idx).unwrap_or(u32::MAX))).await?;
			let chunk_timestamps: Vec<chrono::DateTime<chrono::Utc>> = chunk.iter().map(InputMeasurement::timestamp).collect();
			self.requeue_committed(&aspect_id, &chunk_timestamps).await;

			// Periodic PASSIVE checkpoint every 100 chunks to prevent WAL from growing too large
			// PASSIVE doesn't block, unlike TRUNCATE
			if total_measurements > 100_000 && chunk_idx > 0 && chunk_idx % 100 == 0 {
				// Inline passive checkpoint
				if let Ok(chk_conn) = db.connect() {
					if let Ok(mut rows) = chk_conn.query("PRAGMA wal_checkpoint(PASSIVE)", turso::params![]).await {
						while let Ok(Some(_)) = rows.next().await {}
					}
				}
			}
		}

		// Invalidate cache
		let cache_key = format!("aspect_measurements_{}", aspect_id.as_uuid());
		self.cache.lock().await.invalidate(&cache_key).await;

		// Every row is committed and queued by now, so the steps from here on are
		// best-effort: their error must not turn a stored batch into a failed call that the
		// client retries, which in rows mode stores it twice.

		// Use PASSIVE checkpoint during imports to avoid blocking subsequent operations
		// TRUNCATE checkpoint requires exclusive access which can cause contention with MVCC
		tracing::debug!("[batch_capture] Starting final PASSIVE checkpoint...");
		match Self::checkpoint_wal_passive(&db).await {
			Ok(()) => tracing::debug!("[batch_capture] Final checkpoint complete"),
			Err(e) => tracing::debug!("[batch_capture] Final checkpoint failed; the measurements are committed: {e}"),
		}

		// Check if any measurements in this batch fall within previously compressed ranges
		// This is best-effort - errors are logged but don't fail the import
		if let Err(e) = self.mark_dirty_regions_for_batch(&aspect_id, min_new, max_new).await {
			tracing::debug!("[batch_capture] Failed to check dirty regions for batch: {e}");
		}

		// Update earliest and latest in metadata. Nothing reads these columns back (the
		// earliest and latest measurement are computed from the rows), so a failure is
		// only logged.
		tracing::debug!("[batch_capture] Updating aspect timestamps...");
		if let Err(e) = self.update_aspect_timestamps(&aspect_id, min_new, max_new).await {
			tracing::warn!("[batch_capture] Failed to update aspect timestamps for {aspect_id}; the measurements are stored: {e}");
		}
		tracing::info!("[batch_capture] Batch complete: {} measurements imported for aspect {}", total_measurements, aspect_id);
		Ok(all_tx_ids)
	}

	/// Helper: Process a chunk of measurements with bulk insert (trait method)
	async fn capture_measurement_chunk(&self, aspect_id: &AspectId, dataset_id: DatasetId, chunk: &[InputMeasurement]) -> Result<Vec<TxId>> {
		if chunk.is_empty() {
			return Ok(Vec::new());
		}

		let db = self.get_measurement_db(aspect_id).await?;
		let db_path = self.get_measurement_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// Generate TxIds for this chunk only
		let chunk_tx_ids: Vec<TxId> = (0..chunk.len()).map(|_| TxId::new()).collect();

		// Build bulk INSERT statement with all measurements
		let placeholder_str = "(?, ?, ?, ?)";
		let mut placeholders_str = String::with_capacity(chunk.len() * (placeholder_str.len() + 2));
		for i in 0..chunk.len() {
			if i > 0 {
				placeholders_str.push_str(", ");
			}
			placeholders_str.push_str(placeholder_str);
		}

		let bulk_sql = format!("INSERT INTO measurements (id, dataset_id, timestamp, value) VALUES {placeholders_str}");

		// Flatten all parameters into a single vector with pre-allocated capacity
		let dataset_id_str = dataset_id.as_uuid().to_string();
		let mut params = Vec::with_capacity(chunk.len() * 4);
		for (i, input_measurement) in chunk.iter().enumerate() {
			let measurement = Measurement::from_input_measurement(&dataset_id, input_measurement);
			params.push(chunk_tx_ids[i].as_uuid().to_string());
			params.push(dataset_id_str.clone());
			params.push(measurement.timestamp().timestamp_millis().to_string());
			params.push(measurement.value().to_string());
		}

		// Execute the bulk insert with all parameters
		conn.as_ref().execute(&bulk_sql, turso::params_from_iter(params)).await.map_err(|e| Error::DatabaseError(format!("Failed to bulk insert measurements: {e}")))?;

		Self::commit_concurrent(&conn).await?;

		// Enqueue all measurement timestamps for incremental batch processing, retrying an
		// MVCC conflict over an entry that is already queued (see capture_new_measurement).
		let chunk_timestamps: Vec<chrono::DateTime<chrono::Utc>> = chunk.iter().map(InputMeasurement::timestamp).collect();
		self.enqueue_retrying(aspect_id, &chunk_timestamps).await?;

		Ok(chunk_tx_ids)
	}

	/// Batch insert new measurements - skips duplicates (implement missing trait method)
	async fn batch_capture_new_measurements(&self, aspect_id: &AspectId, dataset_id: &DatasetId, input_measurements: Vec<InputMeasurement>) -> Result<Vec<TxId>> {
		let mut tx_ids = Vec::new();
		for m in input_measurements {
			if let Ok(tx) = self.capture_new_measurement(aspect_id, dataset_id, &m).await {
				tx_ids.push(tx);
			}
		}
		Ok(tx_ids)
	}

	/// Capture new measurement chunk (implement missing trait method - simple loop)
	async fn capture_new_measurement_chunk(&self, aspect_id: &AspectId, db: &turso::Database, db_path: &str, dataset_id: &DatasetId, chunk: &[InputMeasurement], all_tx_ids: &[TxId], tx_id_offset: usize) -> Result<Vec<TxId>> {
		let mut successful = Vec::new();
		let mut successful_timestamps = Vec::new();
		for (i, m) in chunk.iter().enumerate() {
			let tx_id = &all_tx_ids[tx_id_offset + i];
			let measurement = Measurement::from_input_measurement(dataset_id, m);
			// Simple INSERT - no unique constraints with MVCC
			let insert_sql = r"
				INSERT INTO measurements (id, dataset_id, timestamp, value) 
				VALUES (?, ?, ?, ?)
			";

			let conn = Self::begin_concurrent(db, db_path, Some(self.cache.clone())).await?;
			let res = conn.as_ref().execute(insert_sql, turso::params![tx_id.as_uuid().to_string(), dataset_id.as_uuid().to_string(), measurement.timestamp().timestamp_millis(), measurement.value().to_string()]).await;
			match res {
				Ok(rows) => {
					tracing::debug!("Inserted measurement for tx_id {tx_id}: {rows} rows affected");
					successful.push(*tx_id);
					successful_timestamps.push(m.timestamp());
				}
				Err(e) => {
					Self::rollback_after_error(&conn).await;
					return Err(anyhow::anyhow!("Failed to insert in chunk: {e}"));
				}
			}

			Self::commit_concurrent(&conn).await?;
		}

		// Enqueue all successfully inserted measurement timestamps for incremental batch
		// processing, retrying an MVCC conflict over an entry that is already queued (see
		// capture_new_measurement).
		if !successful_timestamps.is_empty() {
			self.enqueue_retrying(aspect_id, &successful_timestamps).await?;
		}

		Ok(successful)
	}

	/// Insert unprocessed batch (implement missing trait method)
	///
	/// Skips the insert, and still returns `Ok`, when a batch with the same
	/// `(aspect_id, batch_hash)` is already queued, processed, or extracted
	/// (crash-consistency design, S18). The consumer rebuilds a window it already batched
	/// whenever a timestamp in it is queued again: after a consumer crash between its
	/// inserts and its dequeue, and after an ingest that ran during a consumer run queues
	/// its committed rows again (see [`UnbatchedEntry`]). With the same rows the rebuilt
	/// batch is byte-identical; without this check it was queued, processed and turned
	/// into a pattern occurrence a second time. The hash is the MD5 of the batch's
	/// measurements, which include their timestamps, so two different windows never share
	/// one, while a window rebuilt over changed rows (a gap filled since) gets a new hash
	/// and is queued. Extracted batches are known by the hashes
	/// [`remove_extracted_batches`](Inputs::remove_extracted_batches) recorded when it
	/// deleted them, for as long as it keeps them (48 hours by default; see there). Every
	/// check is a point lookup on a hash index, the queued one first
	/// (`queue_batch_unless_known` says why the order matters).
	///
	/// The index is not unique (a hash can be `NULL`, and batches stored before the dedupe
	/// may repeat), so two consumers racing on the same aspect can still both insert a
	/// batch; the consumer runs once per aspect.
	async fn insert_unprocessed_batch(&self, aspect_id: &AspectId, batch: &Batch) -> Result<TxId> {
		let tx_id = TxId::new();
		let db = self.get_unprocessed_batches_db(aspect_id).await?;
		let db_path = self.get_unprocessed_batches_db_path(aspect_id).await?;
		let processed = self.get_processed_batches_db(aspect_id).await?.connect()?;
		self.queue_batch_unless_known(aspect_id, batch, &db, &db_path, &processed).await?;
		Ok(tx_id)
	}

	/// Batch insert unprocessed batches (implement missing trait method)
	///
	/// Applies the same dedupe as [`insert_unprocessed_batch`](Self::insert_unprocessed_batch)
	/// to every batch, which also drops repeats within `batches` (a repeat finds the first
	/// one queued). Every input batch still gets a `TxId`, inserted or skipped. A full
	/// rebuild stores through this, so it too skips the windows already extracted; a
	/// rebuild meant to extract every window again clears the processed batches first
	/// (`Pipeline::prepare_data_full_rebuild` does), which clears that record.
	async fn batch_insert_unprocessed_batches(&self, aspect_id: &AspectId, batches: Vec<Batch>) -> Result<Vec<TxId>> {
		let total = batches.len();
		let mut tx_ids = Vec::with_capacity(total);
		let report_interval = std::cmp::max(1000, total / 10); // Report every 1000 or 10% (whichever is larger)
		let mut skipped = 0usize;
		let db = self.get_unprocessed_batches_db(aspect_id).await?;
		let db_path = self.get_unprocessed_batches_db_path(aspect_id).await?;
		let processed = self.get_processed_batches_db(aspect_id).await?.connect()?;

		for (i, b) in batches.into_iter().enumerate() {
			if !self.queue_batch_unless_known(aspect_id, &b, &db, &db_path, &processed).await? {
				skipped += 1;
			}
			tx_ids.push(TxId::new());

			// Report progress intermittently
			if (i + 1) % report_interval == 0 || i + 1 == total {
				tracing::debug!("Inserted unprocessed batches: {}/{}", i + 1, total);
			}
		}
		if skipped > 0 {
			tracing::debug!("Skipped {skipped} of {total} unprocessed batches for aspect {aspect_id}: already queued, processed or extracted");
		}
		Ok(tx_ids)
	}

	/// Insert batch chunk (implement missing trait method - simple loop)
	async fn insert_batch_chunk(&self, conn: &mut Connection, chunk: &[Batch]) -> Result<Vec<TxId>> {
		let mut tx_ids = Vec::new();
		for b in chunk {
			let tx_id = TxId::new();
			let insert_sql = r"INSERT INTO batches (id, aspect_id, database_id, size, resolution, measurements, batch_hash, status, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)";
			let batch_metadata_size: i64 = match i64::try_from(b.metadata.size) {
				Ok(size) => size,
				Err(e) => {
					return Err(anyhow::anyhow!("Batch size conversion error: {e}"));
				}
			};

			let batch_measurements_len: i64 = match i64::try_from(b.measurements.len()) {
				Ok(len) => len,
				Err(e) => {
					return Err(anyhow::anyhow!("Batch measurements length conversion error: {e}"));
				}
			};

			let res = conn.as_ref().execute(insert_sql, turso::params![b.id().to_string(), b.metadata.aspect.as_uuid().to_string(), self.id().as_uuid().to_string(), batch_metadata_size, format!("{}", b.metadata.resolution), "{}", batch_measurements_len, "stub_hash", "unprocessed", chrono::Utc::now().timestamp_millis()]).await;
			if let Err(e) = res {
				return Err(anyhow::anyhow!("Failed to insert batch chunk: {e}"));
			}
			tx_ids.push(tx_id);
		}
		Ok(tx_ids)
	}

	async fn remove_unprocessed_batch(&self, aspect_id: &AspectId, batch_id: &BatchId) -> Result<TxId> {
		let db = self.get_unprocessed_batches_db(aspect_id).await?;
		let db_path = self.get_unprocessed_batches_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let delete_sql = r"DELETE FROM batches WHERE id = ?";

		let res = conn.as_ref().execute(delete_sql, turso::params![batch_id.to_string()]).await;
		if let Err(e) = res {
			Self::rollback_after_error(&conn).await;
			return Err(anyhow::anyhow!("Failed to remove unprocessed batch: {e}"));
		}

		Self::commit_concurrent(&conn).await?;

		let log = format!("Removed unprocessed batch {batch_id} for aspect {aspect_id}");
		Ok(self.record_transaction(&log).await?)
	}

	async fn clear_unprocessed_batches(&self, aspect_id: &AspectId) -> Result<TxId> {
		let db = self.get_unprocessed_batches_db(aspect_id).await?;
		let db_path = self.get_unprocessed_batches_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let delete_sql = r"DELETE FROM batches";

		let res = conn.as_ref().execute(delete_sql, turso::params![]).await;
		match res {
			Ok(_) => tracing::debug!("Cleared all unprocessed batches for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to clear unprocessed batches: {e}"));
			}
		}

		Self::commit_concurrent(&conn).await?;
		let log = format!("Cleared all unprocessed batches for aspect {aspect_id}");
		Ok(self.record_transaction(&log).await?)
	}

	async fn cleanup_unprocessed_batches(&self, aspect_id: &AspectId, older_than: chrono::DateTime<chrono::Utc>) -> Result<TxId> {
		let db = self.get_unprocessed_batches_db(aspect_id).await?;
		let db_path = self.get_unprocessed_batches_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let delete_sql = r"DELETE FROM batches WHERE created_at < ?";

		let res = conn.as_ref().execute(delete_sql, turso::params![older_than.timestamp_millis()]).await;
		match res {
			Ok(deleted) => tracing::debug!("Cleaned up {deleted} unprocessed batches older than {older_than} for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to cleanup unprocessed batches: {e}"));
			}
		}

		Self::commit_concurrent(&conn).await?;
		let log = format!("Cleaned up unprocessed batches older than {older_than} for aspect {aspect_id}");
		Ok(self.record_transaction(&log).await?)
	}

	/// Insert processed batch (implement missing trait method)
	async fn insert_processed_batch(&self, aspect_id: &AspectId, batch: &Batch) -> Result<TxId> {
		let tx_id = TxId::new();
		let measurements_json = serde_json::to_string(&batch.measurements).map_err(|e| Error::DatabaseError(format!("Failed to serialize: {e}")))?;
		let batch_hash = Self::processed_batch_hash(batch, &measurements_json);
		let batch_id = batch.batch_id().to_string();
		let db = self.get_processed_batches_db(aspect_id).await?;
		let db_path = self.get_processed_batches_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		let insert_sql = r"INSERT INTO batches (id, aspect_id, database_id, size, resolution, measurements, batch_hash, status, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)";

		let batch_metadata_size: i64 = match i64::try_from(batch.metadata.size) {
			Ok(size) => size,
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Batch size conversion error: {e}"));
			}
		};

		let res = conn.as_ref().execute(insert_sql, turso::params![batch_id.clone(), aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string(), batch_metadata_size, format!("{}", batch.metadata.resolution), measurements_json.clone(), batch_hash.clone(), "processed", chrono::Utc::now().timestamp_millis()]).await;
		if let Err(e) = res {
			tracing::warn!("Failed to insert processed batch {batch_id}: {e}");
			Self::rollback_after_error(&conn).await;
			return Err(anyhow::anyhow!("Failed to insert processed batch: {e}"));
		}

		Self::commit_concurrent(&conn).await?;

		Ok(tx_id)
	}

	/// Batch insert processed batches (implement missing trait method)
	async fn batch_insert_processed_batches(&self, aspect_id: &AspectId, batches: Vec<Batch>) -> Result<Vec<TxId>> {
		let total = batches.len();
		let mut tx_ids = Vec::with_capacity(total);
		let report_interval = std::cmp::max(1000, total / 10); // Report every 1000 or 10% (whichever is larger)

		for (i, b) in batches.into_iter().enumerate() {
			let tx = self.insert_processed_batch(aspect_id, &b).await?;
			tx_ids.push(tx);

			// Report progress intermittently
			if (i + 1) % report_interval == 0 || i + 1 == total {
				tracing::debug!("Inserted processed batches: {}/{}", i + 1, total);
			}
		}
		Ok(tx_ids)
	}

	/// remove processed batch for a given aspect
	async fn remove_processed_batch(&self, aspect_id: &AspectId, batch_id: &BatchId) -> Result<TxId> {
		let db = self.get_processed_batches_db(aspect_id).await?;
		let db_path = self.get_processed_batches_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let delete_sql = r"DELETE FROM batches WHERE id = ?";

		let res = conn.as_ref().execute(delete_sql, turso::params![batch_id.to_string()]).await;
		if let Err(e) = res {
			Self::rollback_after_error(&conn).await;
			return Err(anyhow::anyhow!("Failed to remove processed batch: {e}"));
		}

		Self::commit_concurrent(&conn).await?;

		let log = format!("Removed processed batch {batch_id} for aspect {aspect_id}");
		Ok(self.record_transaction(&log).await?)
	}

	/// Bulk remove processed batches (single transaction with WHERE IN)
	async fn bulk_remove_processed_batches(&self, aspect_id: &AspectId, batch_ids: &[BatchId]) -> Result<()> {
		if batch_ids.is_empty() {
			return Ok(());
		}

		let db = self.get_processed_batches_db(aspect_id).await?;
		let db_path = self.get_processed_batches_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// Process in sub-chunks to avoid SQLite variable limits
		let sub_chunk_size = 500;
		for sub_chunk in batch_ids.chunks(sub_chunk_size) {
			let placeholders: Vec<&str> = (0..sub_chunk.len()).map(|_| "?").collect();
			let delete_sql = format!("DELETE FROM batches WHERE id IN ({})", placeholders.join(", "));

			let params: Vec<String> = sub_chunk.iter().map(std::string::ToString::to_string).collect();
			conn.as_ref().execute(&delete_sql, turso::params_from_iter(params)).await.map_err(|e| Error::DatabaseError(format!("Failed to bulk delete processed batches: {e}")))?;
		}

		Self::commit_concurrent(&conn).await?;
		Ok(())
	}

	/// Remove processed batches that pattern extraction consumed, recording each one's
	/// `batch_hash` in `extracted_batches` in the same transaction (crash-consistency
	/// design, S18). The same transaction deletes the records older than the retention
	/// (`WEFT_EXTRACTED_BATCH_RETENTION_SECS`, 48 hours by default), which keeps the record
	/// bounded; the delete scans the record, which that bound keeps small.
	async fn remove_extracted_batches(&self, aspect_id: &AspectId, batch_ids: &[BatchId]) -> Result<()> {
		if batch_ids.is_empty() {
			return Ok(());
		}

		let db = self.get_processed_batches_db(aspect_id).await?;
		let db_path = self.get_processed_batches_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		let extracted_at = chrono::Utc::now().timestamp_millis();
		let expired_before = extracted_at.saturating_sub(extracted_batch_retention_millis());

		let removed = async {
			conn.as_ref().execute("DELETE FROM extracted_batches WHERE extracted_at < ?", turso::params![expired_before]).await?;
			// Sub-chunks keep each statement under SQLite's variable limit.
			for sub_chunk in batch_ids.chunks(500) {
				let placeholders: Vec<&str> = (0..sub_chunk.len()).map(|_| "?").collect();
				let placeholders = placeholders.join(", ");
				let ids: Vec<String> = sub_chunk.iter().map(std::string::ToString::to_string).collect();
				conn.as_ref().execute(&format!("INSERT INTO extracted_batches (batch_hash, extracted_at) SELECT batch_hash, {extracted_at} FROM batches WHERE batch_hash IS NOT NULL AND id IN ({placeholders})"), turso::params_from_iter(ids.clone())).await?;
				conn.as_ref().execute(&format!("DELETE FROM batches WHERE id IN ({placeholders})"), turso::params_from_iter(ids)).await?;
			}
			Ok::<_, turso::Error>(())
		}
		.await;
		if let Err(e) = removed {
			let _ = Self::rollback_concurrent(&conn).await;
			return Err(Error::DatabaseError(format!("Failed to remove extracted batches: {e}")).into());
		}

		Self::commit_concurrent(&conn).await
	}

	/// clear all processed batches for a given aspect, and the record of the batches
	/// extraction consumed
	async fn clear_processed_batches(&self, aspect_id: &AspectId) -> Result<TxId> {
		let db = self.get_processed_batches_db(aspect_id).await?;
		let db_path = self.get_processed_batches_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// The extracted-batch record goes with the batches: a full rebuild clears them to
		// batch, process and extract every window again, which the duplicate check would
		// otherwise refuse for every window extracted before.
		let mut res = conn.as_ref().execute(r"DELETE FROM batches", turso::params![]).await;
		if res.is_ok() {
			res = conn.as_ref().execute(r"DELETE FROM extracted_batches", turso::params![]).await;
		}
		match res {
			Ok(_) => tracing::debug!("Cleared all processed batches for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to clear processed batches: {e}"));
			}
		}

		Self::commit_concurrent(&conn).await?;
		let log = format!("Cleared all processed batches for aspect {aspect_id}");
		Ok(self.record_transaction(&log).await?)
	}

	/// cleanup processed batches older than the specified timestamp for a given aspect
	async fn cleanup_processed_batches(&self, aspect_id: &AspectId, older_than: chrono::DateTime<chrono::Utc>) -> Result<TxId> {
		let db = self.get_processed_batches_db(aspect_id).await?;
		let db_path = self.get_processed_batches_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let delete_sql = r"DELETE FROM batches WHERE created_at < ?";

		let res = conn.as_ref().execute(delete_sql, turso::params![older_than.timestamp_millis()]).await;
		match res {
			Ok(deleted) => tracing::debug!("Cleaned up {deleted} processed batches older than {older_than} for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to cleanup processed batches: {e}"));
			}
		}

		Self::commit_concurrent(&conn).await?;
		let log = format!("Cleaned up processed batches older than {older_than} for aspect {aspect_id}");
		Ok(self.record_transaction(&log).await?)
	}

	/// Bulk insert processed batches using multi-row INSERT (single transaction)
	async fn bulk_insert_processed_batches(&self, aspect_id: &AspectId, batches: &[Batch]) -> Result<()> {
		if batches.is_empty() {
			return Ok(());
		}

		let db = self.get_processed_batches_db(aspect_id).await?;
		let db_path = self.get_processed_batches_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// Process in sub-chunks of 100 to avoid SQLite variable limits
		// SQLite has a limit of ~32,766 variables per statement
		// Each batch has 9 columns, so max ~3600 batches per INSERT
		// Using 100 for safety and to keep individual statements fast
		let sub_chunk_size = 100;
		let now = chrono::Utc::now().timestamp_millis();
		let db_id_str = self.id().as_uuid().to_string();
		let aspect_id_str = aspect_id.as_uuid().to_string();

		for sub_chunk in batches.chunks(sub_chunk_size) {
			let placeholder = "(?, ?, ?, ?, ?, ?, ?, ?, ?)";
			let placeholders: Vec<&str> = (0..sub_chunk.len()).map(|_| placeholder).collect();
			let bulk_sql = format!("INSERT INTO batches (id, aspect_id, database_id, size, resolution, measurements, batch_hash, status, created_at) VALUES {}", placeholders.join(", "));

			let mut params: Vec<String> = Vec::with_capacity(sub_chunk.len() * 9);
			for batch in sub_chunk {
				let measurements_json = serde_json::to_string(&batch.measurements).map_err(|e| Error::DatabaseError(format!("Failed to serialize: {e}")))?;
				let batch_hash = Self::processed_batch_hash(batch, &measurements_json);
				let batch_metadata_size: i64 = i64::try_from(batch.metadata.size).unwrap_or(0);

				params.push(batch.batch_id().to_string());
				params.push(aspect_id_str.clone());
				params.push(db_id_str.clone());
				params.push(batch_metadata_size.to_string());
				params.push(format!("{}", batch.metadata.resolution));
				params.push(measurements_json);
				params.push(batch_hash);
				params.push("processed".to_string());
				params.push(now.to_string());
			}

			conn.as_ref().execute(&bulk_sql, turso::params_from_iter(params)).await.map_err(|e| Error::DatabaseError(format!("Failed to bulk insert processed batches: {e}")))?;
		}

		Self::commit_concurrent(&conn).await?;
		Ok(())
	}

	/// Bulk remove unprocessed batches (single transaction with WHERE IN)
	async fn bulk_remove_unprocessed_batches(&self, aspect_id: &AspectId, batch_ids: &[BatchId]) -> Result<()> {
		if batch_ids.is_empty() {
			return Ok(());
		}

		let db = self.get_unprocessed_batches_db(aspect_id).await?;
		let db_path = self.get_unprocessed_batches_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// Process in sub-chunks to avoid SQLite variable limits
		let sub_chunk_size = 500;
		for sub_chunk in batch_ids.chunks(sub_chunk_size) {
			let placeholders: Vec<&str> = (0..sub_chunk.len()).map(|_| "?").collect();
			let delete_sql = format!("DELETE FROM batches WHERE id IN ({})", placeholders.join(", "));

			let params: Vec<String> = sub_chunk.iter().map(std::string::ToString::to_string).collect();
			conn.as_ref().execute(&delete_sql, turso::params_from_iter(params)).await.map_err(|e| Error::DatabaseError(format!("Failed to bulk delete unprocessed batches: {e}")))?;
		}

		Self::commit_concurrent(&conn).await?;
		Ok(())
	}

	/// Move batches from unprocessed to processed in bulk
	/// Does bulk INSERT into processed + bulk DELETE from unprocessed
	async fn move_batches_to_processed(&self, aspect_id: &AspectId, batches: &[Batch]) -> Result<()> {
		if batches.is_empty() {
			return Ok(());
		}

		// Get batch IDs before the insert (need them for the delete)
		let batch_ids: Vec<BatchId> = batches.iter().map(|b| *b.batch_id()).collect();

		// Bulk insert into processed
		self.bulk_insert_processed_batches(aspect_id, batches).await?;

		// Bulk delete from unprocessed
		self.bulk_remove_unprocessed_batches(aspect_id, &batch_ids).await?;

		Ok(())
	}

	//
	// Patterns
	//

	/// insert pattern for a given aspect
	async fn insert_pattern(&self, aspect_id: &AspectId, pattern: &Pattern) -> Result<TxId> {
		let tx_id = TxId::new();
		let db = self.get_patterns_db(aspect_id).await?;
		let db_path = self.get_patterns_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// Insert into patterns table
		let insert_patterns_sql = r"INSERT INTO patterns (id, sum_value, abs_sum_value, max_value, min_value, abs_max_value, avg_value, abs_avg_value) VALUES (?, ?, ?, ?, ?, ?, ?, ?)";
		let res = conn.as_ref().execute(insert_patterns_sql, turso::params![pattern.id().to_string(), pattern.sum().to_string(), pattern.abs_sum().to_string(), pattern.max().to_string(), pattern.min().to_string(), pattern.abs_max().to_string(), pattern.avg().to_string(), pattern.abs_avg().to_string()]).await;
		match res {
			Ok(_) => (),
			Err(e) => {
				let id = pattern.id();
				tracing::warn!("Failed to insert pattern '{id}' for aspect {aspect_id}: {e}");
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to insert pattern: {e}"));
			}
		}

		// Insert occurrences
		for occurrence in pattern.occurrences() {
			let database_info_json = serde_json::to_string(occurrence.database_info())?;
			let size = i64::try_from(occurrence.size()).map_err(|_| anyhow::anyhow!("Occurrence size too large for i64"))?;
			let insert_occ_sql = r"INSERT INTO pattern_occurrences (pattern_id, aspect_id, resolution, size, database_info, beginning_timestamp, end_timestamp) VALUES (?, ?, ?, ?, ?, ?, ?)";
			let res = conn.as_ref().execute(insert_occ_sql, turso::params![pattern.id().to_string(), aspect_id.to_string(), occurrence.resolution().to_string(), size, database_info_json, occurrence.beginning().timestamp_millis(), occurrence.end().timestamp_millis()]).await;
			match res {
				Ok(_) => (),
				Err(e) => {
					let id = pattern.id();
					tracing::warn!("Failed to insert occurrence for pattern '{id}': {e}");
					Self::rollback_after_error(&conn).await;
					return Err(anyhow::anyhow!("Failed to insert occurrence: {e}"));
				}
			}
		}

		// Insert relatives
		for (i, relative) in pattern.relatives().iter().enumerate() {
			let relative_index = i64::try_from(i).map_err(|_| anyhow::anyhow!("Relative index too large for i64"))?;
			let insert_rel_sql = r"INSERT INTO pattern_relatives (pattern_id, relative_index, vector_location, vector_amplitude, max_x, max_y) VALUES (?, ?, ?, ?, ?, ?)";
			let res = conn.as_ref().execute(insert_rel_sql, turso::params![pattern.id().to_string(), relative_index, relative.vector().location().to_string(), relative.vector().amplitude().to_string(), relative.max_x().to_string(), relative.max_y().to_string()]).await;

			match res {
				Ok(_) => (),
				Err(e) => {
					let id = pattern.id();
					tracing::warn!("Failed to insert relative {i} for pattern '{id}': {e}");
					Self::rollback_after_error(&conn).await;
					return Err(anyhow::anyhow!("Failed to insert relative: {e}"));
				}
			}
		}

		Self::commit_concurrent(&conn).await?;

		let log = format!("Inserted pattern '{}' for aspect {}", pattern.id(), aspect_id);
		let _ = self.record_transaction(&log).await?;

		Ok(tx_id)
	}

	/// Capture multiple patterns for a given aspect
	async fn batch_insert_patterns(&self, aspect_id: &AspectId, patterns: Vec<Pattern>) -> Result<Vec<TxId>> {
		let mut tx_ids = Vec::new();
		for p in patterns {
			let tx = self.insert_pattern(aspect_id, &p).await?;
			tx_ids.push(tx);
		}
		Ok(tx_ids)
	}

	/// remove pattern for a given aspect
	async fn remove_pattern(&self, aspect_id: &AspectId, pattern: &PatternID) -> Result<TxId> {
		let db = self.get_patterns_db(aspect_id).await?;
		let db_path = self.get_patterns_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		let delete_sql = r"DELETE FROM patterns WHERE id = ?";
		let res = conn.as_ref().execute(delete_sql, turso::params![pattern.to_string()]).await;
		match res {
			Ok(_) => tracing::debug!("Removed pattern '{pattern}' for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to remove pattern: {e}"));
			}
		}
		Self::commit_concurrent(&conn).await?;
		let log = format!("Removed pattern '{pattern}' for aspect {aspect_id}");
		Ok(self.record_transaction(&log).await?)
	}

	/// clear all patterns for a given aspect
	async fn clear_patterns(&self, aspect_id: &AspectId) -> Result<TxId> {
		let db = self.get_patterns_db(aspect_id).await?;
		let db_path = self.get_patterns_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		let delete_sql = r"DELETE FROM patterns";
		let res = conn.as_ref().execute(delete_sql, turso::params![]).await;
		match res {
			Ok(_) => tracing::debug!("Cleared all patterns for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to clear patterns: {e}"));
			}
		}
		Self::commit_concurrent(&conn).await?;
		let log = format!("Cleared all patterns for aspect {aspect_id}");
		Ok(self.record_transaction(&log).await?)
	}

	//
	// Events
	//

	/// Store an event in the database
	///
	/// # Errors
	/// - if database not found
	/// - if unable to insert event
	async fn insert_event(&self, aspect_id: &AspectId, event: &Event) -> Result<TxId> {
		let tx_id = TxId::new();
		let db = self.get_events_db(aspect_id).await?;
		let db_path = self.get_events_db_path(aspect_id).await?;

		// Serialize the event manifestations
		let manifestations_json = serde_json::to_string(event.manifestations()).map_err(|e| anyhow::anyhow!(format!("Failed to serialize event manifestations: {e}")))?;

		let event_id = event.id().to_string();
		let event_name = event.name().to_string();
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let res = conn.as_ref().execute("INSERT INTO events (id, database_id, name, manifestations, created_at) VALUES (?, ?, ?, ?, ?)", turso::params![event_id.clone(), self.id().as_uuid().to_string(), event_name.clone(), manifestations_json.clone(), chrono::Utc::now().timestamp_millis()]).await;
		match res {
			Ok(_) => tracing::debug!("Stored event {} in database {}", event_id, self.id()),
			Err(e) => {
				let _ = Self::rollback_concurrent(&conn).await;
				return Err(anyhow::anyhow!(format!("Failed to insert event: {e}")));
			}
		}

		Self::commit_concurrent(&conn).await?;

		let log = format!("Inserted event {event_id} for aspect {aspect_id}");
		let _ = self.record_transaction(&log).await?;

		Ok(tx_id)
	}

	/// Batch insert events
	async fn batch_insert_events(&self, aspect_id: &AspectId, events: Vec<Event>) -> Result<Vec<TxId>> {
		let mut tx_ids = Vec::new();
		for e in events {
			let tx = self.insert_event(aspect_id, &e).await?;
			tx_ids.push(tx);
		}
		Ok(tx_ids)
	}

	/// Remove an event from the database
	async fn remove_event(&self, aspect_id: &AspectId, event_id: &EventID) -> Result<TxId> {
		let db = self.get_events_db(aspect_id).await?;
		let db_path = self.get_events_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		let delete_sql = r"DELETE FROM events WHERE id = ?";
		let res = conn.as_ref().execute(delete_sql, turso::params![event_id.to_string()]).await;
		match res {
			Ok(_) => tracing::debug!("Removed event {event_id} for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to remove event: {e}"));
			}
		}
		Self::commit_concurrent(&conn).await?;
		let log = format!("Removed event {event_id} for aspect {aspect_id}");
		let _ = self.record_transaction(&log).await?;
		Ok(TxId::new())
	}

	/// Clear all events for a given aspect
	async fn clear_events(&self, aspect_id: &AspectId) -> Result<TxId> {
		let db = self.get_events_db(aspect_id).await?;
		let db_path = self.get_events_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		let delete_sql = r"DELETE FROM events";
		let res = conn.as_ref().execute(delete_sql, turso::params![]).await;
		match res {
			Ok(_) => tracing::debug!("Cleared all events for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to clear events: {e}"));
			}
		}
		Self::commit_concurrent(&conn).await?;
		let log = format!("Cleared all events for aspect {aspect_id}");
		let _ = self.record_transaction(&log).await?;
		Ok(TxId::new())
	}

	async fn set_dictionary_metadata(&self, aspect_id: &AspectId, dictionary_name: &str, metadata: &DictionaryMetadata) -> Result<TxId> {
		let tx_id = TxId::new();
		// Refuse a step method that `get_dictionary_metadata` could not load, before the
		// dictionary's database file exists.
		Self::check_dictionary_constraints(dictionary_name, &metadata.constraints)?;
		let aspect = self.get_aspect(aspect_id).await?;
		let db_name = &self.name;
		let db_path = Self::aspect_dictionaries_db_path(db_name.as_str(), aspect.subject_name(), aspect.name(), dictionary_name)?;
		let (db, _was_new) = Self::get_or_create_turso_database(&db_path).await?;
		// Create the schema FIRST, in an exclusive transaction. Turso rejects DDL inside
		// `BEGIN CONCURRENT` ("DDL statements require an exclusive transaction"), so the
		// `CREATE TABLE`s that used to live below — on the concurrent connection — aborted this
		// function before a single table existed. The half-created database file then defeated
		// the only recovery path (`Aspect::dictionary` re-wireframes only when the file is
		// absent, and `get_or_create_turso_database` reports `was_new = false` for anything
		// already in its connection cache), so the dictionary stayed permanently schema-less and
		// a later `SELECT ... FROM patterns` failed with "no such table: patterns".
		//
		// Routing through the wireframe also fixes a second defect: the inline DDL created only
		// four of the seven dictionary tables, omitting `patterns`, `pattern_occurrences` and
		// `pattern_relatives` — so merely swapping the transaction type would not have been enough.
		Aspect::ensure_dictionary_tables(&db).await?;
		let conn = Self::begin_concurrent(&db, db_name, Some(self.cache.clone())).await?;

		// The whole registration: metadata, steps and variabilities. It used to insert only the
		// metadata row, so `get_dictionary_metadata` (which needs the constraints row) never
		// found the dictionary, and every `load_dictionary` inserted another metadata row.
		if let Err(e) = Self::replace_dictionary_registration(&conn, &metadata.id, dictionary_name, &metadata.description, &metadata.constraints).await {
			tracing::warn!("Failed to set metadata for dictionary '{dictionary_name}' for aspect {aspect_id}: {e}");
			Self::rollback_after_error(&conn).await;
			// Keep `e` as the source: a write-write conflict is what makes the write retryable.
			let message = format!("Failed to set dictionary metadata: {e}");
			return Err(e.context(message));
		}

		Self::commit_concurrent(&conn).await?;
		// After the commit, and as a new generation: a read whose snapshot predates the commit
		// then does not cache the registration this replaced.
		self.cache.lock().await.invalidate_generation(&Self::dictionary_metadata_cache_key(aspect_id, dictionary_name)).await;

		let log = format!("Set metadata for dictionary '{dictionary_name}' for aspect {aspect_id}");
		let _ = self.record_transaction(&log).await?;

		Ok(tx_id)
	}

	async fn register_dictionary_if_absent(&self, aspect_id: &AspectId, dictionary_name: &str, metadata: &DictionaryMetadata) -> Result<bool> {
		Self::check_dictionary_constraints(dictionary_name, &metadata.constraints)?;
		retry_once_if_transient(dictionary_name, || self.register_dictionary_if_absent_once(aspect_id, dictionary_name, metadata)).await
	}

	/// insert pattern into dictionary for a given aspect
	async fn insert_pattern_into_dictionary(&self, aspect_id: &AspectId, dictionary_name: &str, pattern: &Pattern) -> Result<TxId> {
		let tx_id = TxId::new();
		let aspect = self.get_aspect(aspect_id).await?;
		let db_name = &self.name;
		let db_path = Self::aspect_dictionaries_db_path(db_name.as_str(), aspect.subject_name(), aspect.name(), dictionary_name)?;
		let (db, _was_new) = Self::get_or_create_turso_database(&db_path).await?;
		let conn = Self::begin_concurrent(&db, db_name, Some(self.cache.clone())).await?;

		// Insert into patterns table (the main patterns table with pattern data)
		let insert_patterns_sql = r"INSERT INTO patterns (id, sum_value, abs_sum_value, max_value, min_value, abs_max_value, avg_value, abs_avg_value) VALUES (?, ?, ?, ?, ?, ?, ?, ?)";
		let res = conn.as_ref().execute(insert_patterns_sql, turso::params![pattern.id().to_string(), pattern.sum().to_string(), pattern.abs_sum().to_string(), pattern.max().to_string(), pattern.min().to_string(), pattern.abs_max().to_string(), pattern.avg().to_string(), pattern.abs_avg().to_string()]).await;
		match res {
			Ok(_) => (),
			Err(e) => {
				let id = pattern.id();
				tracing::warn!("Failed to insert pattern '{id}' into dictionary '{dictionary_name}' for aspect {aspect_id}: {e}");
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to insert pattern into dictionary: {e}"));
			}
		}

		// Insert occurrences
		for (index, occurrence) in pattern.occurrences().iter().enumerate() {
			let occurrence_index = i64::try_from(index).map_err(|_| anyhow::anyhow!("Occurrence index too large for i64"))?;
			let database_info_json = serde_json::to_string(occurrence.database_info())?;
			let size = i64::try_from(occurrence.size()).map_err(|_| anyhow::anyhow!("Occurrence size too large for i64"))?;
			let insert_occ_sql = r"INSERT INTO pattern_occurrences (pattern_id, occurrence_index, aspect_id, resolution, size, database_info, beginning_timestamp, end_timestamp) VALUES (?, ?, ?, ?, ?, ?, ?, ?)";
			let res = conn.as_ref().execute(insert_occ_sql, turso::params![pattern.id().to_string(), occurrence_index, aspect_id.to_string(), occurrence.resolution().to_string(), size, database_info_json, occurrence.beginning().timestamp_millis(), occurrence.end().timestamp_millis()]).await;
			match res {
				Ok(_) => (),
				Err(e) => {
					let id = pattern.id();
					tracing::warn!("Failed to insert occurrence for pattern '{id}' in dictionary '{dictionary_name}': {e}");
					Self::rollback_after_error(&conn).await;
					return Err(anyhow::anyhow!("Failed to insert occurrence: {e}"));
				}
			}
		}

		// Insert relatives - serialize the relative as JSON for the relative_value column
		for (i, relative) in pattern.relatives().iter().enumerate() {
			let relative_index = i64::try_from(i).map_err(|_| anyhow::anyhow!("Relative index too large for i64"))?;
			let relative_value_json = serde_json::to_string(relative)?;
			let insert_rel_sql = r"INSERT INTO pattern_relatives (pattern_id, relative_index, relative_value) VALUES (?, ?, ?)";
			let res = conn.as_ref().execute(insert_rel_sql, turso::params![pattern.id().to_string(), relative_index, relative_value_json]).await;
			match res {
				Ok(_) => (),
				Err(e) => {
					let id = pattern.id();
					tracing::warn!("Failed to insert relative {i} for pattern '{id}' in dictionary '{dictionary_name}': {e}");
					Self::rollback_after_error(&conn).await;
					return Err(anyhow::anyhow!("Failed to insert relative: {e}"));
				}
			}
		}

		Self::commit_concurrent(&conn).await?;

		let log = format!("Inserted pattern '{}' into dictionary '{}' for aspect {}", pattern.id(), dictionary_name, aspect_id);
		let _ = self.record_transaction(&log).await?;

		Ok(tx_id)
	}

	/// Capture multiple patterns into dictionary for a given aspect
	async fn batch_insert_patterns_into_dictionary(&self, aspect_id: &AspectId, dictionary_name: &str, patterns: Vec<Pattern>) -> Result<Vec<TxId>> {
		let mut tx_ids = Vec::new();
		for p in patterns {
			let tx = self.insert_pattern_into_dictionary(aspect_id, dictionary_name, &p).await?;
			tx_ids.push(tx);
		}
		Ok(tx_ids)
	}

	//
	// Correlations
	//

	/// insert correlation for a given aspect
	async fn insert_correlation(&self, aspect_id: &AspectId, correlation: &Correlation) -> Result<TxId> {
		let tx_id = TxId::new();
		let mut aspect = self.get_aspect(aspect_id).await?;
		let db = aspect.correlations().await?;
		let db_path = aspect.correlations_path();
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// Insert into correlations table with average_distance
		let (avg_dist_value, avg_dist_units) = correlation.average_distance().map_or((None, None), |dist| (Some(dist.value().to_string()), Some(dist.units().to_string())));
		let insert_sql = r"INSERT INTO correlations (id, dictionary_id, subject_id, aspect_id, pattern_id, event_id, average_distance_value, average_distance_units, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
		let res = conn.as_ref().execute(insert_sql, turso::params![correlation.id().to_string(), correlation.dictionary_id().to_string(), correlation.subject_id().to_string(), correlation.aspect_id().to_string(), correlation.pattern_id().to_string(), correlation.event_id().to_string(), avg_dist_value, avg_dist_units, chrono::Utc::now().timestamp_millis(), chrono::Utc::now().timestamp_millis()]).await;
		match res {
			Ok(_) => (),
			Err(e) => {
				let id = correlation.id();
				tracing::warn!("Failed to insert correlation '{id}' for aspect {aspect_id}: {e}");
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to insert correlation: {e}"));
			}
		}

		// Insert error rates if any
		for (signal_type, distance) in correlation.error_rate() {
			let insert_err_sql = r"INSERT INTO correlation_error_rates (correlation_id, signal_type, error_rate_value, error_rate_units) VALUES (?, ?, ?, ?)";
			let res = conn.as_ref().execute(insert_err_sql, turso::params![correlation.id().to_string(), signal_type.to_string(), distance.value().to_string(), distance.units().to_string()]).await;
			match res {
				Ok(_) => (),
				Err(e) => {
					let id = correlation.id();
					tracing::warn!("Failed to insert error rate for correlation '{id}': {e}");
					Self::rollback_after_error(&conn).await;
					return Err(anyhow::anyhow!("Failed to insert correlation error rate: {e}"));
				}
			}
		}

		// Insert occurrences
		for (index, occurrence) in correlation.occurrences().iter().enumerate() {
			let occurrence_index = i64::try_from(index).map_err(|_| anyhow::anyhow!("Occurrence index too large for i64"))?;
			let size = i64::try_from(occurrence.size()).map_err(|_| anyhow::anyhow!("Occurrence size too large for i64"))?;
			let database_info_json = serde_json::to_string(occurrence.database_info())?;
			let insert_occ_sql = r"INSERT INTO correlation_occurrences (correlation_id, occurrence_index, aspect_id, resolution, size, database_info, pattern_id, beginning_timestamp, end_timestamp) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)";
			let res = conn.as_ref().execute(insert_occ_sql, turso::params![correlation.id().to_string(), occurrence_index, occurrence.aspect().to_string(), occurrence.resolution().to_string(), size, database_info_json, occurrence.pattern_id().to_string(), occurrence.beginning().timestamp_millis(), occurrence.end().timestamp_millis()]).await;
			match res {
				Ok(_) => (),
				Err(e) => {
					let id = correlation.id();
					tracing::warn!("Failed to insert occurrence {index} for correlation '{id}': {e}");
					Self::rollback_after_error(&conn).await;
					return Err(anyhow::anyhow!("Failed to insert correlation occurrence: {e}"));
				}
			}
		}

		Self::commit_concurrent(&conn).await?;

		let log = format!("Inserted correlation '{}' for aspect {}", correlation.id(), aspect_id);
		let _ = self.record_transaction(&log).await?;

		Ok(tx_id)
	}

	async fn update_correlation(&self, aspect_id: &AspectId, correlation: &Correlation) -> Result<TxId> {
		let tx_id = TxId::new();
		let mut aspect = self.get_aspect(aspect_id).await?;
		let db = aspect.correlations().await?;
		let db_path = aspect.correlations_path();
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// Update correlations table with all fields including average_distance
		let (avg_dist_value, avg_dist_units) = correlation.average_distance().map_or((None, None), |dist| (Some(dist.value().to_string()), Some(dist.units().to_string())));
		let update_sql = r"UPDATE correlations SET dictionary_id = ?, subject_id = ?, aspect_id = ?, pattern_id = ?, event_id = ?, average_distance_value = ?, average_distance_units = ?, updated_at = ? WHERE id = ?";
		let res = conn.as_ref().execute(update_sql, turso::params![correlation.dictionary_id().to_string(), correlation.subject_id().to_string(), correlation.aspect_id().to_string(), correlation.pattern_id().to_string(), correlation.event_id().to_string(), avg_dist_value, avg_dist_units, chrono::Utc::now().timestamp_millis(), correlation.id().to_string()]).await;
		match res {
			Ok(_) => (),
			Err(e) => {
				let id = correlation.id();
				tracing::warn!("Failed to update correlation '{id}' for aspect {aspect_id}: {e}");
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to update correlation: {e}"));
			}
		}

		// Delete existing error rates and re-insert
		let delete_err_sql = r"DELETE FROM correlation_error_rates WHERE correlation_id = ?";
		let res = conn.as_ref().execute(delete_err_sql, turso::params![correlation.id().to_string()]).await;
		if let Err(e) = res {
			let id = correlation.id();
			tracing::warn!("Failed to delete error rates for correlation '{id}': {e}");
			Self::rollback_after_error(&conn).await;
			return Err(anyhow::anyhow!("Failed to delete correlation error rates: {e}"));
		}

		// Insert updated error rates
		for (signal_type, distance) in correlation.error_rate() {
			let insert_err_sql = r"INSERT INTO correlation_error_rates (correlation_id, signal_type, error_rate_value, error_rate_units) VALUES (?, ?, ?, ?)";
			let res = conn.as_ref().execute(insert_err_sql, turso::params![correlation.id().to_string(), signal_type.to_string(), distance.value().to_string(), distance.units().to_string()]).await;
			match res {
				Ok(_) => (),
				Err(e) => {
					let id = correlation.id();
					tracing::warn!("Failed to insert error rate for correlation '{id}': {e}");
					Self::rollback_after_error(&conn).await;
					return Err(anyhow::anyhow!("Failed to insert correlation error rate: {e}"));
				}
			}
		}

		// Delete existing occurrences and re-insert
		let delete_occ_sql = r"DELETE FROM correlation_occurrences WHERE correlation_id = ?";
		let res = conn.as_ref().execute(delete_occ_sql, turso::params![correlation.id().to_string()]).await;
		if let Err(e) = res {
			let id = correlation.id();
			tracing::warn!("Failed to delete occurrences for correlation '{id}': {e}");
			Self::rollback_after_error(&conn).await;
			return Err(anyhow::anyhow!("Failed to delete correlation occurrences: {e}"));
		}

		// Insert updated occurrences
		for (index, occurrence) in correlation.occurrences().iter().enumerate() {
			let occurrence_index = i64::try_from(index).map_err(|_| anyhow::anyhow!("Occurrence index too large for i64"))?;
			let size = i64::try_from(occurrence.size()).map_err(|_| anyhow::anyhow!("Occurrence size too large for i64"))?;
			let database_info_json = serde_json::to_string(occurrence.database_info())?;
			let insert_occ_sql = r"INSERT INTO correlation_occurrences (correlation_id, occurrence_index, aspect_id, resolution, size, database_info, pattern_id, beginning_timestamp, end_timestamp) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)";
			let res = conn.as_ref().execute(insert_occ_sql, turso::params![correlation.id().to_string(), occurrence_index, occurrence.aspect().to_string(), occurrence.resolution().to_string(), size, database_info_json, occurrence.pattern_id().to_string(), occurrence.beginning().timestamp(), occurrence.end().timestamp()]).await;
			match res {
				Ok(_) => (),
				Err(e) => {
					let id = correlation.id();
					tracing::warn!("Failed to insert occurrence {index} for correlation '{id}': {e}");
					Self::rollback_after_error(&conn).await;
					return Err(anyhow::anyhow!("Failed to insert correlation occurrence: {e}"));
				}
			}
		}

		Self::commit_concurrent(&conn).await?;

		// Update the cache with the new correlation data
		let cache_key = format!("correlation_{}_{}", aspect_id.as_uuid(), correlation.id().to_uuid());
		self.cache.lock().await.store(&cache_key, correlation.clone()).await;

		let log = format!("Updated correlation '{}' for aspect {}", correlation.id(), aspect_id);
		let _ = self.record_transaction(&log).await?;

		Ok(tx_id)
	}

	async fn remove_correlation(&self, aspect_id: &AspectId, correlation_id: &CorrelationID) -> Result<TxId> {
		let db = self.get_correlations_db(aspect_id).await?;
		let db_path = self.get_correlations_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// Delete error rates first
		let delete_err_sql = r"DELETE FROM correlation_error_rates WHERE correlation_id = ?";
		let res = conn.as_ref().execute(delete_err_sql, turso::params![correlation_id.to_string()]).await;
		if let Err(e) = res {
			tracing::warn!("Failed to delete error rates for correlation '{correlation_id}': {e}");
			Self::rollback_after_error(&conn).await;
			return Err(anyhow::anyhow!("Failed to delete correlation error rates: {e}"));
		}

		// Delete occurrences
		let delete_occ_sql = r"DELETE FROM correlation_occurrences WHERE correlation_id = ?";
		let res = conn.as_ref().execute(delete_occ_sql, turso::params![correlation_id.to_string()]).await;
		if let Err(e) = res {
			tracing::warn!("Failed to delete occurrences for correlation '{correlation_id}': {e}");
			Self::rollback_after_error(&conn).await;
			return Err(anyhow::anyhow!("Failed to delete correlation occurrences: {e}"));
		}

		// Delete the correlation itself
		let delete_sql = r"DELETE FROM correlations WHERE id = ?";
		let res = conn.as_ref().execute(delete_sql, turso::params![correlation_id.to_string()]).await;
		match res {
			Ok(_) => tracing::debug!("Removed correlation {correlation_id} for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to remove correlation: {e}"));
			}
		}

		Self::commit_concurrent(&conn).await?;
		let log = format!("Removed correlation {correlation_id} for aspect {aspect_id}");
		let _ = self.record_transaction(&log).await?;
		Ok(TxId::new())
	}

	//
	// Unprocessed Events
	//

	async fn insert_unprocessed_event(&self, aspect_id: &AspectId, event: &Event) -> Result<TxId> {
		let tx_id = TxId::new();
		let db = self.get_unprocessed_events_db(aspect_id).await?;
		let db_path = self.get_unprocessed_events_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		let _manifestations_json = serde_json::to_string(event.manifestations()).map_err(|e| anyhow::anyhow!(format!("Failed to serialize event manifestations: {e}")))?;
		let event_id = event.id().to_string();
		let description = event.description().clone().unwrap_or_default();

		let res = conn.as_ref().execute("INSERT INTO events (id, name, description, created_at) VALUES (?, ?, ?, ?)", turso::params![event_id.clone(), event.name().to_string(), description, chrono::Utc::now().timestamp_millis()]).await;
		match res {
			Ok(_) => tracing::debug!("Inserted unprocessed event {} in database {}", event_id, self.id()),
			Err(e) => {
				let _ = Self::rollback_concurrent(&conn).await;
				return Err(anyhow::anyhow!(format!("Failed to insert unprocessed event: {e}")));
			}
		}

		// insert manifestations
		for (manifestation_id, manifestation) in event.manifestations() {
			let _manifestation_json = serde_json::to_string(manifestation).map_err(|e| anyhow::anyhow!(format!("Failed to serialize event manifestation: {e}")))?;
			let res = conn.as_ref().execute("INSERT INTO event_manifestations (id, event_id, dataset_id, start_timestamp, end_timestamp) VALUES (?, ?, ?, ?, ?)", turso::params![manifestation_id.to_uuid().to_string(), event_id.clone(), manifestation.dataset_id().to_string(), manifestation.start().timestamp_millis(), manifestation.end().timestamp_millis()]).await;
			match res {
				Ok(_) => (),
				Err(e) => {
					let _ = Self::rollback_concurrent(&conn).await;
					return Err(anyhow::anyhow!(format!("Failed to insert unprocessed event manifestation: {e}")));
				}
			}
		}
		Self::commit_concurrent(&conn).await?;
		let log = format!("Inserted unprocessed event {event_id} for aspect {aspect_id}");
		let _ = self.record_transaction(&log).await?;
		Ok(tx_id)
	}

	async fn remove_unprocessed_event(&self, aspect_id: &AspectId, event_id: &EventID) -> Result<TxId> {
		let db = self.get_unprocessed_events_db(aspect_id).await?;
		let db_path = self.get_unprocessed_events_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// Delete manifestations first
		let delete_manifestations_sql = r"DELETE FROM event_manifestations WHERE event_id = ?";
		let res = conn.as_ref().execute(delete_manifestations_sql, turso::params![event_id.to_string()]).await;
		if let Err(e) = res {
			tracing::warn!("Failed to delete manifestations for unprocessed event '{event_id}': {e}");
			Self::rollback_after_error(&conn).await;
			return Err(anyhow::anyhow!("Failed to delete unprocessed event manifestations: {e}"));
		}

		// Delete the unprocessed event itself
		let delete_sql = r"DELETE FROM events WHERE id = ?";
		let res = conn.as_ref().execute(delete_sql, turso::params![event_id.to_string()]).await;
		match res {
			Ok(_) => tracing::debug!("Removed unprocessed event {event_id} for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to remove unprocessed event: {e}"));
			}
		}

		Self::commit_concurrent(&conn).await?;
		let log = format!("Removed unprocessed event {event_id} for aspect {aspect_id}");
		let _ = self.record_transaction(&log).await?;
		Ok(TxId::new())
	}

	async fn clear_unprocessed_events(&self, aspect_id: &AspectId) -> Result<TxId> {
		let db = self.get_unprocessed_events_db(aspect_id).await?;
		let db_path = self.get_unprocessed_events_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		let delete_manifestations_sql = r"DELETE FROM event_manifestations";
		let res = conn.as_ref().execute(delete_manifestations_sql, turso::params![]).await;
		match res {
			Ok(_) => tracing::debug!("Cleared all unprocessed event manifestations for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to clear unprocessed event manifestations: {e}"));
			}
		}

		let delete_sql = r"DELETE FROM events";
		let res = conn.as_ref().execute(delete_sql, turso::params![]).await;
		match res {
			Ok(_) => tracing::debug!("Cleared all unprocessed events for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to clear unprocessed events: {e}"));
			}
		}

		Self::commit_concurrent(&conn).await?;
		let log = format!("Cleared all unprocessed events for aspect {aspect_id}");
		let _ = self.record_transaction(&log).await?;
		Ok(TxId::new())
	}

	async fn cleanup_unprocessed_events(&self, aspect_id: &AspectId, older_than: chrono::DateTime<chrono::Utc>) -> Result<TxId> {
		let db = self.get_unprocessed_events_db(aspect_id).await?;
		let db_path = self.get_unprocessed_events_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		let delete_sql = r"DELETE FROM events WHERE created_at < ?";
		let res = conn.as_ref().execute(delete_sql, turso::params![older_than.timestamp_millis()]).await;
		match res {
			Ok(deleted) => tracing::debug!("Cleaned up {deleted} unprocessed events older than {older_than} for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to cleanup unprocessed events: {e}"));
			}
		}
		Self::commit_concurrent(&conn).await?;
		let log = format!("Cleaned up unprocessed events older than {older_than} for aspect {aspect_id}");
		let _ = self.record_transaction(&log).await?;
		Ok(TxId::new())
	}

	//
	// Processed Events
	//
	async fn insert_processed_event(&self, aspect_id: &AspectId, event: &Event) -> Result<TxId> {
		let tx_id = TxId::new();
		let db = self.get_processed_events_db(aspect_id).await?;
		let db_path = self.get_processed_events_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		let _manifestations_json = serde_json::to_string(event.manifestations()).map_err(|e| anyhow::anyhow!(format!("Failed to serialize event manifestations: {e}")))?;
		let event_id = event.id().to_string();
		let description = event.description().clone().unwrap_or_default();
		let res = conn.as_ref().execute("INSERT INTO events (id, name, description, created_at) VALUES (?, ?, ?, ?)", turso::params![event_id.clone(), event.name().to_string(), description, chrono::Utc::now().timestamp_millis()]).await;
		match res {
			Ok(_) => tracing::debug!("Inserted processed event {} in database {}", event_id, self.id()),
			Err(e) => {
				let _ = Self::rollback_concurrent(&conn).await;
				return Err(anyhow::anyhow!(format!("Failed to insert processed event: {e}")));
			}
		}

		// insert manifestations
		for (manifestation_id, manifestation) in event.manifestations() {
			let _manifestation_json = serde_json::to_string(manifestation).map_err(|e| anyhow::anyhow!(format!("Failed to serialize event manifestation: {e}")))?;
			let res = conn.as_ref().execute("INSERT INTO event_manifestations (id, event_id, dataset_id, start_timestamp, end_timestamp) VALUES (?, ?, ?, ?, ?)", turso::params![manifestation_id.to_uuid().to_string(), event_id.clone(), manifestation.dataset_id().to_string(), manifestation.start().timestamp_millis(), manifestation.end().timestamp_millis()]).await;
			match res {
				Ok(_) => (),
				Err(e) => {
					let _ = Self::rollback_concurrent(&conn).await;
					return Err(anyhow::anyhow!(format!("Failed to insert processed event manifestation: {e}")));
				}
			}
		}
		Self::commit_concurrent(&conn).await?;
		let log = format!("Inserted processed event {event_id} for aspect {aspect_id}");
		let _ = self.record_transaction(&log).await?;
		Ok(tx_id)
	}

	async fn remove_processed_event(&self, aspect_id: &AspectId, event_id: &EventID) -> Result<TxId> {
		let db = self.get_processed_events_db(aspect_id).await?;
		let db_path = self.get_processed_events_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// Delete manifestations first
		let delete_manifestations_sql = r"DELETE FROM event_manifestations WHERE event_id = ?";
		let res = conn.as_ref().execute(delete_manifestations_sql, turso::params![event_id.to_string()]).await;
		if let Err(e) = res {
			tracing::warn!("Failed to delete manifestations for processed event '{event_id}': {e}");
			Self::rollback_after_error(&conn).await;
			return Err(anyhow::anyhow!("Failed to delete processed event manifestations: {e}"));
		}

		// Delete the processed event itself
		let delete_sql = r"DELETE FROM events WHERE id = ?";
		let res = conn.as_ref().execute(delete_sql, turso::params![event_id.to_string()]).await;
		match res {
			Ok(_) => tracing::debug!("Removed processed event {event_id} for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to remove processed event: {e}"));
			}
		}

		Self::commit_concurrent(&conn).await?;
		let log = format!("Removed processed event {event_id} for aspect {aspect_id}");
		let _ = self.record_transaction(&log).await?;
		Ok(TxId::new())
	}

	async fn clear_processed_events(&self, aspect_id: &AspectId) -> Result<TxId> {
		let db = self.get_processed_events_db(aspect_id).await?;
		let db_path = self.get_processed_events_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		let delete_manifestations_sql = r"DELETE FROM event_manifestations";
		let res = conn.as_ref().execute(delete_manifestations_sql, turso::params![]).await;
		match res {
			Ok(_) => tracing::debug!("Cleared all processed event manifestations for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to clear processed event manifestations: {e}"));
			}
		}

		let delete_sql = r"DELETE FROM events";
		let res = conn.as_ref().execute(delete_sql, turso::params![]).await;
		match res {
			Ok(_) => tracing::debug!("Cleared all processed events for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to clear processed events: {e}"));
			}
		}

		Self::commit_concurrent(&conn).await?;
		let log = format!("Cleared all processed events for aspect {aspect_id}");
		let _ = self.record_transaction(&log).await?;
		Ok(TxId::new())
	}

	async fn cleanup_processed_events(&self, aspect_id: &AspectId, older_than: chrono::DateTime<chrono::Utc>) -> Result<TxId> {
		let db = self.get_processed_events_db(aspect_id).await?;
		let db_path = self.get_processed_events_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		let delete_sql = r"DELETE FROM events WHERE created_at < ?";
		let res = conn.as_ref().execute(delete_sql, turso::params![older_than.timestamp_millis()]).await;
		match res {
			Ok(deleted) => tracing::debug!("Cleaned up {deleted} processed events older than {older_than} for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to cleanup processed events: {e}"));
			}
		}
		Self::commit_concurrent(&conn).await?;
		let log = format!("Cleaned up processed events older than {older_than} for aspect {aspect_id}");
		let _ = self.record_transaction(&log).await?;
		Ok(TxId::new())
	}

	//
	// Compression
	//

	/// Replace measurements in a time range with new compressed measurements.
	/// This is an atomic delete + insert operation used during compression.
	async fn replace_measurements_in_range(&self, aspect_id: &AspectId, dataset_id: &DatasetId, start: chrono::DateTime<chrono::Utc>, end: chrono::DateTime<chrono::Utc>, new_measurements: Vec<InputMeasurement>) -> Result<()> {
		if new_measurements.is_empty() {
			// Just delete the range if no new measurements
			let db = self.get_measurement_db(aspect_id).await?;
			let db_path = self.get_measurement_db_path(aspect_id).await?;
			let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

			let delete_sql = "DELETE FROM measurements WHERE timestamp >= ? AND timestamp <= ?";
			let res = conn.as_ref().execute(delete_sql, turso::params![start.timestamp_millis(), end.timestamp_millis()]).await;
			if let Err(e) = res {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to delete measurements in range: {e}"));
			}

			Self::commit_concurrent(&conn).await?;
			Self::checkpoint_wal_passive(&db).await?;
			return Ok(());
		}

		let db = self.get_measurement_db(aspect_id).await?;
		let db_path = self.get_measurement_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// Delete existing measurements in range
		let delete_sql = "DELETE FROM measurements WHERE timestamp >= ? AND timestamp <= ?";
		let res = conn.as_ref().execute(delete_sql, turso::params![start.timestamp_millis(), end.timestamp_millis()]).await;
		if let Err(e) = res {
			Self::rollback_after_error(&conn).await;
			return Err(anyhow::anyhow!("Failed to delete measurements in range: {e}"));
		}

		// Insert new measurements in chunks
		let chunk_size = 500;
		let dataset_id_str = dataset_id.as_uuid().to_string();

		for chunk in new_measurements.chunks(chunk_size) {
			let placeholder = "(?, ?, ?, ?)";
			let placeholders: Vec<&str> = (0..chunk.len()).map(|_| placeholder).collect();
			let bulk_sql = format!("INSERT INTO measurements (id, dataset_id, timestamp, value) VALUES {}", placeholders.join(", "));

			let mut params: Vec<String> = Vec::with_capacity(chunk.len() * 4);
			for m in chunk {
				let measurement = Measurement::from_input_measurement(dataset_id, m);
				let tx_id = TxId::new();
				params.push(tx_id.as_uuid().to_string());
				params.push(dataset_id_str.clone());
				params.push(measurement.timestamp().timestamp_millis().to_string());
				params.push(measurement.value().to_string());
			}

			let res = conn.as_ref().execute(&bulk_sql, turso::params_from_iter(params)).await;
			if let Err(e) = res {
				Self::rollback_after_error(&conn).await;
				return Err(anyhow::anyhow!("Failed to insert compressed measurements: {e}"));
			}
		}

		Self::commit_concurrent(&conn).await?;
		Self::checkpoint_wal_passive(&db).await?;

		// Invalidate cache
		let cache_key = format!("aspect_measurements_{}", aspect_id.as_uuid());
		self.cache.lock().await.invalidate(&cache_key).await;

		tracing::debug!(
			aspect_id = %aspect_id,
			start = %start,
			end = %end,
			new_count = new_measurements.len(),
			"Replaced measurements in range"
		);

		Ok(())
	}
}

/// Helper methods for dirty region tracking (not part of trait)
impl Database {
	/// A batch's identity for the queue dedupe: its measurements as stored (JSON) and
	/// their MD5, the `batch_hash` column.
	fn batch_identity(batch: &Batch) -> Result<(String, String)> {
		let measurements_json = serde_json::to_string(&batch.measurements).map_err(|e| Error::DatabaseError(format!("Failed to serialize: {e}")))?;
		let batch_hash = format!("{:x}", md5::compute(&measurements_json));
		Ok((measurements_json, batch_hash))
	}

	/// The `batch_hash` a processed batch is stored under: the hash it was queued under,
	/// when it came from the unprocessed queue, so that
	/// [`insert_unprocessed_batch`](Inputs::insert_unprocessed_batch) recognises it. Processing
	/// rewrites the measurements (level transform, analysis), so a hash of the processed
	/// measurements would never match the batch the consumer rebuilds.
	fn processed_batch_hash(batch: &Batch, measurements_json: &str) -> String {
		batch.batch_hash().cloned().unwrap_or_else(|| format!("{:x}", md5::compute(measurements_json)))
	}

	/// Whether the `batches` table of the DB `conn` is open on (the unprocessed or the
	/// processed batches) holds `batch_hash` for `aspect_id`: a point lookup on its
	/// `(aspect_id, batch_hash)` index, read in `conn`'s open transaction if it has one,
	/// else in a statement of its own.
	async fn batch_hash_stored(conn: &turso::Connection, aspect_id: &AspectId, batch_hash: &str) -> Result<bool> {
		let rows = conn.query("SELECT 1 FROM batches WHERE aspect_id = ? AND batch_hash = ? LIMIT 1", turso::params![aspect_id.as_uuid().to_string(), batch_hash.to_string()]).await;
		Self::lookup_found(rows, "batches").await
	}

	/// Whether the processed batches DB `conn` is open on records `batch_hash` as extracted
	/// (`extracted_batches`, which has no `aspect_id` column: the DB belongs to one
	/// aspect): a point lookup on its `batch_hash` index, in a statement of its own.
	async fn batch_hash_extracted(conn: &turso::Connection, batch_hash: &str) -> Result<bool> {
		let rows = conn.query("SELECT 1 FROM extracted_batches WHERE batch_hash = ? LIMIT 1", turso::params![batch_hash.to_string()]).await;
		Self::lookup_found(rows, "extracted_batches").await
	}

	/// Whether a point lookup in `table` found a row.
	async fn lookup_found(rows: std::result::Result<turso::Rows, turso::Error>, table: &str) -> Result<bool> {
		let failed = |e: turso::Error| Error::DatabaseError(format!("Failed to look up batch hash in {table}: {e}"));
		Ok(rows.map_err(failed)?.next().await.map_err(failed)?.is_some())
	}

	/// Whether a batch with `batch_hash` is processed (in the processed batches) or was
	/// extracted (recorded in `extracted_batches` when extraction deleted it), checked in
	/// that order, each in a statement of its own on `processed`.
	async fn processed_or_extracted(processed: &turso::Connection, aspect_id: &AspectId, batch_hash: &str) -> Result<Option<&'static str>> {
		if Self::batch_hash_stored(processed, aspect_id, batch_hash).await? {
			return Ok(Some("processed"));
		}
		Ok(Self::batch_hash_extracted(processed, batch_hash).await?.then_some("extracted"))
	}

	/// Insert `batch` into the unprocessed batches (`db`) unless its
	/// `(aspect_id, batch_hash)` is already queued there, processed, or extracted
	/// (`processed`, a connection to the processed batches DB with no open transaction),
	/// and return whether it was inserted.
	///
	/// The checks follow a batch's way through the tables, each as of when it runs: the
	/// queued check first, inside the insert's transaction, then the processed batches,
	/// then the extracted ones. A batch moves forward by being written to the next table
	/// no later than it is deleted from the previous one (`move_batches_to_processed`
	/// inserts the processed batch before it deletes the queued one;
	/// `remove_extracted_batches` records the hash in the transaction that deletes the
	/// processed batch), so a batch that moves between two checks is still seen by the
	/// later one. Checking in any other order could miss a moving batch in every table.
	async fn queue_batch_unless_known(&self, aspect_id: &AspectId, batch: &Batch, db: &turso::Database, db_path: &str, processed: &turso::Connection) -> Result<bool> {
		let (measurements_json, batch_hash) = Self::batch_identity(batch)?;
		let conn = Self::begin_concurrent(db, db_path, Some(self.cache.clone())).await?;
		let known = match Self::batch_hash_stored(conn.as_ref(), aspect_id, &batch_hash).await {
			Ok(true) => Ok(Some("queued")),
			Ok(false) => Self::processed_or_extracted(processed, aspect_id, &batch_hash).await,
			Err(e) => Err(e),
		};
		match known {
			Ok(None) => {}
			Ok(Some(state)) => {
				let _ = Self::rollback_concurrent(&conn).await;
				tracing::debug!("Batch {} for aspect {aspect_id} is already {state} (hash {batch_hash}); not queuing it again", batch.id());
				return Ok(false);
			}
			Err(e) => {
				let _ = Self::rollback_concurrent(&conn).await;
				return Err(e);
			}
		}
		self.insert_unprocessed_batch_row(&conn, aspect_id, batch, measurements_json, batch_hash).await?;
		Self::commit_concurrent(&conn).await?;
		Ok(true)
	}

	/// Queue `timestamps` again once their rows are committed (crash-consistency design,
	/// S18).
	///
	/// Ingest queued them before inserting the rows (the write-ahead enqueue), so a queue
	/// consumer that ran in between may have read them, built their windows without the
	/// rows and dequeued them. Enqueuing again re-adds a dequeued timestamp, and moves a
	/// still-queued one's `queued_at` past what that consumer read, so that its dequeue
	/// leaves it (see [`UnbatchedEntry`]); either way the next run batches the rows.
	///
	/// It runs after the rows are committed, so like every post-commit step it is
	/// best-effort and never fails the call (the client would retry it into duplicate
	/// rows). An MVCC write-write conflict with a consumer dequeuing the same entries is
	/// retried, as by [`enqueue_retrying`](Self::enqueue_retrying); a failure that outlasts
	/// the retries is logged, and the rows stay covered by the write-ahead entry unless a
	/// consumer dequeued it during this call.
	async fn requeue_committed(&self, aspect_id: &AspectId, timestamps: &[chrono::DateTime<chrono::Utc>]) {
		if let Err(e) = self.enqueue_retrying(aspect_id, timestamps).await {
			tracing::warn!("Could not queue {} committed timestamps of aspect {aspect_id} again; they stay queued unless a batch consumer ran during this ingest: {e}", timestamps.len());
		}
	}

	/// Enqueue `timestamps`, retrying an MVCC write-write conflict with another write to the
	/// same queue entries (see [`QUEUE_WRITE_ATTEMPTS`]): a consumer's dequeue, or another
	/// ingest queuing the same timestamps. The enqueue is an upsert, so unlike the
	/// `INSERT OR IGNORE` it replaced it writes an entry that is already queued, and such a
	/// conflict would otherwise fail the call. Retrying is safe: a failed attempt wrote
	/// nothing.
	async fn enqueue_retrying(&self, aspect_id: &AspectId, timestamps: &[chrono::DateTime<chrono::Utc>]) -> Result<()> {
		retry_queue_write("Enqueuing unbatched measurements", || self.enqueue_unbatched_measurements(aspect_id, timestamps)).await
	}

	/// One transaction of [`dequeue_unbatched_entries`](Inputs::dequeue_unbatched_entries):
	/// delete each of `entries` that still has the `queued_at` it was read with. A
	/// conflict is returned as an [`Error::TransientMvccError`].
	async fn dequeue_entry_chunk(&self, aspect_id: &AspectId, entries: &[UnbatchedEntry]) -> Result<()> {
		let conn = Self::begin_concurrent(self.metadata(), self.metadata_path(), Some(self.cache.clone())).await?;
		let aspect_id_str = aspect_id.as_uuid().to_string();

		let deleted = async {
			let mut delete = conn.as_ref().prepare("DELETE FROM unbatched_measurements WHERE aspect_id = ? AND data_timestamp = ? AND queued_at = ?").await?;
			for entry in entries {
				delete.execute(turso::params![aspect_id_str.clone(), entry.data_timestamp.timestamp_millis(), entry.queued_at]).await?;
			}
			Ok::<_, turso::Error>(())
		}
		.await;
		if let Err(e) = deleted {
			let _ = Self::rollback_concurrent(&conn).await;
			return Err(queue_write_error("Failed to dequeue unbatched measurements", &e));
		}

		commit_queue_write(&conn, "Failed to dequeue unbatched measurements").await
	}

	/// INSERT one unprocessed batch row inside `conn`'s open transaction. On error the
	/// transaction is rolled back.
	async fn insert_unprocessed_batch_row(&self, conn: &Connection, aspect_id: &AspectId, batch: &Batch, measurements_json: String, batch_hash: String) -> Result<()> {
		let batch_id = batch.id().to_string();
		// Simple INSERT - no unique constraints with MVCC
		let insert_sql = r"
			INSERT INTO batches (id, aspect_id, database_id, size, resolution, measurements, batch_hash, status, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
		";

		let batch_metadata_size: i64 = match i64::try_from(batch.metadata.size) {
			Ok(size) => size,
			Err(e) => {
				let _ = Self::rollback_concurrent(conn).await;
				return Err(anyhow::anyhow!("Batch size conversion error: {e}"));
			}
		};

		let res = conn.as_ref().execute(insert_sql, turso::params![batch_id.clone(), aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string(), batch_metadata_size, format!("{}", batch.metadata.resolution), measurements_json, batch_hash, "unprocessed", chrono::Utc::now().timestamp_millis()]).await;
		if let Err(e) = res {
			tracing::warn!("Failed to insert unprocessed batch {batch_id}: {e}");
			Self::rollback_after_error(conn).await;
			return Err(anyhow::anyhow!("Failed to insert unprocessed batch: {e}"));
		}
		Ok(())
	}

	/// Check if a timestamp falls within a previously compressed range and mark it as dirty if so.
	///
	/// This is called after inserting measurements to track when new data is inserted
	/// into time ranges that have already been compressed. The dirty regions can then
	/// be recompressed to maintain data integrity.
	///
	/// # Arguments
	/// * `aspect_id` - The aspect ID to check
	/// * `timestamp` - The timestamp to check
	///
	/// # Errors
	/// Returns Ok(()) if no error occurs. Errors are logged but not propagated to avoid
	/// breaking the insert flow for this non-critical tracking operation.
	pub async fn mark_dirty_region_if_needed(&self, aspect_id: &AspectId, timestamp: chrono::DateTime<chrono::Utc>) -> Result<()> {
		let db = self.get_measurement_db(aspect_id).await?;
		let db_path = self.get_measurement_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// Query compression_tier_results to see if timestamp falls within a compressed range
		let mut rows = conn.as_ref().query("SELECT time_range_start, time_range_end FROM compression_tier_results WHERE time_range_start <= ? AND time_range_end >= ? LIMIT 1", turso::params![timestamp.timestamp_millis(), timestamp.timestamp_millis()]).await?;

		let compressed_range = if let Some(row) = rows.next().await? {
			let start_millis = *row.get_value(0)?.as_integer().unwrap_or(&0);
			let end_millis = *row.get_value(1)?.as_integer().unwrap_or(&0);
			Some((start_millis, end_millis))
		} else {
			None
		};

		// Need to commit this read transaction before starting a new write
		Self::commit_concurrent(&conn).await?;

		if let Some((start_millis, end_millis)) = compressed_range {
			// Open a new connection for the write
			let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

			let now = chrono::Utc::now();

			// Insert the dirty region
			conn.as_ref().execute("INSERT INTO dirty_regions (region_start, region_end, marked_at, reason) VALUES (?, ?, ?, ?)", turso::params![start_millis, end_millis, now.timestamp_millis(), "New measurement inserted into compressed range".to_string(),]).await?;

			// Update dirty_regions_count in compression_state
			conn.as_ref().execute("UPDATE compression_state SET dirty_regions_count = (SELECT COUNT(*) FROM dirty_regions) WHERE id = 1", turso::params![]).await?;

			Self::commit_concurrent(&conn).await?;

			tracing::debug!(
				aspect_id = %aspect_id,
				timestamp = %timestamp,
				region_start_millis = start_millis,
				region_end_millis = end_millis,
				"Marked dirty region for new measurement in compressed range"
			);
		}

		Ok(())
	}

	/// Batch check if any timestamps fall within previously compressed ranges and mark them as dirty.
	///
	/// More efficient than calling `mark_dirty_region_if_needed` for each timestamp when
	/// doing bulk inserts.
	///
	/// # Arguments
	/// * `aspect_id` - The aspect ID to check
	/// * `timestamps` - The timestamps to check (should be the min and max of the batch for efficiency)
	///
	/// # Errors
	/// Returns Ok(()) if no error occurs.
	pub async fn mark_dirty_regions_for_batch(&self, aspect_id: &AspectId, min_timestamp: chrono::DateTime<chrono::Utc>, max_timestamp: chrono::DateTime<chrono::Utc>) -> Result<()> {
		let db = self.get_measurement_db(aspect_id).await?;
		let db_path = self.get_measurement_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// Find all compressed ranges that overlap with the batch range
		let mut rows = conn.as_ref().query("SELECT DISTINCT time_range_start, time_range_end FROM compression_tier_results WHERE time_range_start <= ? AND time_range_end >= ?", turso::params![max_timestamp.timestamp_millis(), min_timestamp.timestamp_millis()]).await?;

		let mut compressed_ranges = Vec::new();
		while let Some(row) = rows.next().await? {
			let start_millis = *row.get_value(0)?.as_integer().unwrap_or(&0);
			let end_millis = *row.get_value(1)?.as_integer().unwrap_or(&0);
			compressed_ranges.push((start_millis, end_millis));
		}

		// Need to commit this read transaction before starting a new write
		Self::commit_concurrent(&conn).await?;

		if !compressed_ranges.is_empty() {
			let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
			let now = chrono::Utc::now();

			for (start_millis, end_millis) in &compressed_ranges {
				// Insert the dirty region
				conn.as_ref().execute("INSERT INTO dirty_regions (region_start, region_end, marked_at, reason) VALUES (?, ?, ?, ?)", turso::params![*start_millis, *end_millis, now.timestamp_millis(), "Batch insert overlapped with compressed range".to_string(),]).await?;
			}

			// Update dirty_regions_count in compression_state
			conn.as_ref().execute("UPDATE compression_state SET dirty_regions_count = (SELECT COUNT(*) FROM dirty_regions) WHERE id = 1", turso::params![]).await?;

			Self::commit_concurrent(&conn).await?;

			tracing::debug!(
				aspect_id = %aspect_id,
				dirty_regions_count = compressed_ranges.len(),
				"Marked dirty regions for batch insert overlapping compressed ranges"
			);
		}

		Ok(())
	}
}

/// A dictionary's registration: its `dictionary_metadata` row and the
/// `dictionary_constraints` and `dictionary_variabilities` rows keyed by its id, all in the
/// dictionary's own `<aspect>/dictionaries/<name>.db`.
impl Database {
	/// Refuse constraints that [`get_dictionary_metadata`](crate::Outputs::get_dictionary_metadata)
	/// could not load back: the step interpolation is stored as the [`Spline`](splimes::Spline)'s
	/// text, and splimes validates it again as it parses it.
	///
	/// # Errors
	///
	/// `Invalid step interpolation for dictionary '<name>': <reason>`, e.g. `invalid polynomial
	/// degree 9: must be between 1 and 8`.
	pub(crate) fn check_dictionary_constraints(dictionary_name: &str, constraints: &DictionaryConstraints) -> Result<()> {
		if let Some(steps) = constraints.steps() {
			steps.interpolation().validate().map_err(|e| anyhow::anyhow!("Invalid step interpolation for dictionary '{dictionary_name}': {e}"))?;
		}
		Ok(())
	}

	/// One attempt of [`register_dictionary_if_absent`](Inputs::register_dictionary_if_absent).
	async fn register_dictionary_if_absent_once(&self, aspect_id: &AspectId, dictionary_name: &str, metadata: &DictionaryMetadata) -> Result<bool> {
		let aspect = self.get_aspect(aspect_id).await?;
		let db_path = Self::aspect_dictionaries_db_path(self.name.as_str(), aspect.subject_name(), aspect.name(), dictionary_name)?;
		let (db, _was_new) = Self::get_or_create_turso_database(&db_path).await?;
		Aspect::ensure_dictionary_tables(&db).await?;
		let conn = Self::begin_concurrent(&db, &self.name, Some(self.cache.clone())).await?;

		// Check again in this transaction: a registration committed since the caller read
		// none is kept. Only rows without constraints are replaced.
		let registered = match Self::has_complete_registration(&conn, dictionary_name).await {
			Ok(true) => Ok(false),
			Ok(false) => Self::replace_dictionary_registration(&conn, &metadata.id, dictionary_name, &metadata.description, &metadata.constraints).await.map(|()| true),
			Err(e) => Err(e),
		};
		let registered = match registered {
			Ok(registered) => registered,
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				let message = format!("Failed to register dictionary '{dictionary_name}': {e}");
				return Err(e.context(message));
			}
		};
		Self::commit_concurrent(&conn).await?;
		if registered {
			self.cache.lock().await.invalidate_generation(&Self::dictionary_metadata_cache_key(aspect_id, dictionary_name)).await;
			let log = format!("Registered dictionary '{dictionary_name}' for aspect {aspect_id}");
			let _ = self.record_transaction(&log).await?;
		}
		Ok(registered)
	}

	/// Whether `name` has a complete registration, a `dictionary_metadata` row with a
	/// `dictionary_constraints` row, on `conn`, a transaction on the dictionary's database.
	/// Whether its stored values parse does not matter here.
	///
	/// # Errors
	///
	/// A failed query.
	pub(crate) async fn has_complete_registration(conn: &Connection, name: &str) -> Result<bool> {
		let mut rows = conn.as_ref().query("SELECT 1 FROM dictionary_metadata m JOIN dictionary_constraints c ON c.dictionary_id = m.id WHERE m.name = ? LIMIT 1", turso::params![name]).await?;
		Ok(rows.next().await?.is_some())
	}

	/// Write `name`'s registration on `conn`, a `BEGIN CONCURRENT` transaction on the
	/// dictionary's database that the caller commits, or rolls back on an error.
	///
	/// The tables have no unique constraint (no indexes under MVCC), so this keeps a name to
	/// one registration itself: it deletes every metadata row with that name, and the
	/// constraints and variabilities of each, before it inserts the new ones. A dictionary
	/// without steps stores `NULL` in both step columns. Variabilities are stored one row
	/// each, in order; an empty list stores none, and so reads back as `None`.
	///
	/// # Errors
	///
	/// The first statement that fails.
	pub(crate) async fn replace_dictionary_registration(conn: &Connection, id: &DictionaryId, name: &str, description: &str, constraints: &DictionaryConstraints) -> Result<()> {
		let mut replaced = Vec::new();
		let mut rows = conn.as_ref().query("SELECT id FROM dictionary_metadata WHERE name = ?", turso::params![name]).await?;
		while let Some(row) = rows.next().await? {
			replaced.push(row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Dictionary ID is not text".to_string()))?.clone());
		}
		drop(rows);
		for old_id in replaced {
			conn.as_ref().execute("DELETE FROM dictionary_constraints WHERE dictionary_id = ?", turso::params![old_id.as_str()]).await?;
			conn.as_ref().execute("DELETE FROM dictionary_variabilities WHERE dictionary_id = ?", turso::params![old_id.as_str()]).await?;
		}
		conn.as_ref().execute("DELETE FROM dictionary_metadata WHERE name = ?", turso::params![name]).await?;

		let id = id.as_uuid().to_string();
		conn.as_ref().execute("INSERT INTO dictionary_metadata (id, name, description, created_at) VALUES (?, ?, ?, ?)", turso::params![id.as_str(), name, description, chrono::Utc::now().timestamp_millis()]).await?;

		let steps_count = constraints.steps().as_ref().map(|s| s.count().to_string());
		let steps_interpolation = constraints.steps().as_ref().map(|s| s.interpolation().to_string());
		conn.as_ref().execute("INSERT INTO dictionary_constraints (dictionary_id, steps_count, steps_interpolation) VALUES (?, ?, ?)", turso::params![id.as_str(), steps_count, steps_interpolation]).await?;

		for variability in constraints.variabilities().iter().flatten() {
			conn.as_ref().execute("INSERT INTO dictionary_variabilities (dictionary_id, variability_type, variability_value) VALUES (?, ?, ?)", turso::params![id.as_str(), variability.kind(), variability.variability().value().to_string()]).await?;
		}
		Ok(())
	}
}

/// How long [`retry_once_if_transient`] waits before its second attempt, in milliseconds:
/// a random delay in this range, like `begin_concurrent`'s first backoff.
const RETRY_DELAY_MS: std::ops::RangeInclusive<u64> = 10..=50;

/// Run `attempt`, and once more if it fails with a transient MVCC error
/// ([`is_transient_mvcc_error`]): a write-write conflict or a stale snapshot, after which
/// the transaction is gone and a new one can succeed. The second attempt's result is
/// returned as it is.
///
/// It waits a short random delay ([`RETRY_DELAY_MS`]) first. A write-write conflict also
/// fires against another transaction's uncommitted write, and a second attempt made at
/// once, while that writer is still open, would most likely conflict again; after the
/// delay it usually sees the winner's commit (and `register_dictionary_if_absent` then
/// finds its registration).
async fn retry_once_if_transient<T, F, Fut>(dictionary_name: &str, mut attempt: F) -> Result<T>
where
	F: FnMut() -> Fut + Send,
	Fut: Future<Output = Result<T>> + Send,
	T: Send,
{
	match attempt().await {
		Err(e) if is_transient_mvcc_error(&e) => {
			tracing::debug!(error = %e, dictionary = dictionary_name, "Registering the dictionary conflicted with another write; trying once more");
			tokio::time::sleep(std::time::Duration::from_millis(fastrand::u64(RETRY_DELAY_MS))).await;
			attempt().await
		}
		result => result,
	}
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicUsize, Ordering};

	use super::*;

	fn conflict() -> anyhow::Error {
		anyhow::Error::new(turso::Error::Error("Write-write conflict".to_string())).context("Failed to register dictionary 'd': Write-write conflict")
	}

	#[tokio::test]
	async fn a_transient_failure_is_tried_once_more_after_a_delay() {
		let attempts = AtomicUsize::new(0);
		let started = std::time::Instant::now();
		let result = retry_once_if_transient("d", || async {
			if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
				Err(conflict())
			} else {
				Ok(true)
			}
		})
		.await;
		assert!(result.expect("the second attempt succeeds"));
		assert_eq!(attempts.load(Ordering::SeqCst), 2);
		assert!(started.elapsed() >= std::time::Duration::from_millis(*RETRY_DELAY_MS.start()), "it waited before trying again: {:?}", started.elapsed());
	}

	#[tokio::test]
	async fn only_once_and_only_for_a_transient_failure() {
		let attempts = AtomicUsize::new(0);
		let err = retry_once_if_transient("d", || async {
			attempts.fetch_add(1, Ordering::SeqCst);
			Err::<bool, _>(conflict())
		})
		.await
		.expect_err("still conflicting");
		assert!(is_transient_mvcc_error(&err), "the second conflict is returned: {err:#}");
		assert_eq!(attempts.load(Ordering::SeqCst), 2, "tried twice, not more");

		let attempts = AtomicUsize::new(0);
		retry_once_if_transient("d", || async {
			attempts.fetch_add(1, Ordering::SeqCst);
			Err::<bool, _>(anyhow::anyhow!("no such table: dictionary_metadata"))
		})
		.await
		.expect_err("not transient");
		assert_eq!(attempts.load(Ordering::SeqCst), 1, "a failure that is not transient is not retried");
	}

	/// `WEFT_EXTRACTED_BATCH_RETENTION_SECS` takes a positive whole number of seconds;
	/// anything else keeps the default, so a typo cannot switch the record off (or make
	/// extraction delete every record as soon as it is written).
	#[test]
	fn the_extracted_batch_retention_is_a_positive_number_of_seconds() {
		assert_eq!(parse_extracted_batch_retention(None), DEFAULT_EXTRACTED_BATCH_RETENTION_SECS);
		assert_eq!(parse_extracted_batch_retention(Some("3600")), 3600);
		assert_eq!(parse_extracted_batch_retention(Some(" 600 ")), 600);
		for invalid in ["", "0", "-5", "1.5", "2d", "forever"] {
			assert_eq!(parse_extracted_batch_retention(Some(invalid)), DEFAULT_EXTRACTED_BATCH_RETENTION_SECS, "{invalid:?}");
		}
	}
}
