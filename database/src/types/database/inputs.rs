use anyhow::Result;

use crate::{
	cache::Connection, database::traits::{AspectStructure, Inputs}, types::{
		database::{
			helpers::safe_usize_to_f64, traits::{config::Config, connection::Connection as ConnectionTrait, DatabaseStructure}
		}, TxId
	}, AspectId, Batch, BatchId, Correlation, CorrelationID, Database, DatasetId, DictionaryMetadata, Error, Event, EventID, InputMeasurement, Measurement, Pattern, PatternID
};

#[async_trait::async_trait]
impl Inputs for Database {
	//
	// Unbatched Measurements Queue
	//

	/// Enqueue a measurement timestamp as unbatched (to be included in future batch creation)
	async fn enqueue_unbatched_measurement(&self, aspect_id: &AspectId, data_timestamp: chrono::DateTime<chrono::Utc>) -> Result<()> {
		self.enqueue_unbatched_measurements(aspect_id, &[data_timestamp]).await
	}

	/// Enqueue multiple measurement timestamps as unbatched (bulk insert with INSERT OR IGNORE for deduplication)
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
			// Use INSERT OR IGNORE to deduplicate (aspect_id + data_timestamp combo)
			let bulk_sql = format!("INSERT OR IGNORE INTO unbatched_measurements (aspect_id, data_timestamp, queued_at) VALUES {}", placeholders.join(", "));

			let mut params: Vec<String> = Vec::with_capacity(chunk.len() * 3);
			for ts in chunk {
				params.push(aspect_id_str.clone());
				params.push(ts.timestamp_millis().to_string());
				params.push(queued_at.to_string());
			}

			let res = conn.as_ref().execute(&bulk_sql, turso::params_from_iter(params)).await;
			if let Err(e) = res {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to enqueue unbatched measurements: {e}"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;
		Ok(())
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
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to dequeue unbatched measurements: {e}"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;
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
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to clear unbatched measurements: {e}"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;
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
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure 1: `{e}`"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;

		// Checkpoint WAL to ensure measurement is persisted
		Self::checkpoint_wal_passive(&db).await?;

		// Enqueue measurement for incremental batch processing
		self.enqueue_unbatched_measurement(aspect_id, data_timestamp).await?;

		// Check if this measurement falls in a previously compressed range and mark dirty if so
		// This is best-effort - errors are logged but don't fail the insert
		if let Err(e) = self.mark_dirty_region_if_needed(aspect_id, data_timestamp).await {
			tracing::debug!("Failed to check dirty region for measurement: {e}");
		}

		self.record_transaction(&format!("Captured measurement at {} with value {} for dataset {}", measurement.timestamp(), measurement.value(), dataset_id)).await
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
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure 2: `{e}`"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;

		// Checkpoint WAL to ensure measurement is persisted
		Self::checkpoint_wal_passive(&db).await?;

		// Enqueue measurement for incremental batch processing
		self.enqueue_unbatched_measurement(aspect_id, data_timestamp).await?;

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
		tracing::debug!("[batch_capture] Got DB path, starting chunk loop ({} chunks of {})", total_measurements / chunk_size + 1, chunk_size);
		let dataset_id_str = dataset_id.as_uuid().to_string();

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

			let _ = Self::commit_concurrent(&conn).await;
			all_tx_ids.extend(chunk_tx_ids);

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

		// Use PASSIVE checkpoint during imports to avoid blocking subsequent operations
		// TRUNCATE checkpoint requires exclusive access which can cause contention with MVCC
		tracing::debug!("[batch_capture] Starting final PASSIVE checkpoint...");
		Self::checkpoint_wal_passive(&db).await?;
		tracing::debug!("[batch_capture] Final checkpoint complete");

		// Enqueue all measurement timestamps for incremental batch processing
		tracing::debug!("[batch_capture] Enqueuing {} unbatched measurements...", input_measurements.len());
		let all_timestamps: Vec<chrono::DateTime<chrono::Utc>> = input_measurements.iter().map(InputMeasurement::timestamp).collect();
		self.enqueue_unbatched_measurements(&aspect_id, &all_timestamps).await?;
		tracing::debug!("[batch_capture] Unbatched measurements enqueued");

		// Check if any measurements in this batch fall within previously compressed ranges
		// This is best-effort - errors are logged but don't fail the import
		if let Err(e) = self.mark_dirty_regions_for_batch(&aspect_id, min_new, max_new).await {
			tracing::debug!("[batch_capture] Failed to check dirty regions for batch: {e}");
		}

		// Update earliest and latest in metadata
		tracing::debug!("[batch_capture] Updating aspect timestamps...");
		self.update_aspect_timestamps(&aspect_id, min_new, max_new).await?;
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

		let _ = Self::commit_concurrent(&conn).await;

		// Enqueue all measurement timestamps for incremental batch processing
		let chunk_timestamps: Vec<chrono::DateTime<chrono::Utc>> = chunk.iter().map(InputMeasurement::timestamp).collect();
		self.enqueue_unbatched_measurements(aspect_id, &chunk_timestamps).await?;

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
					Self::rollback_concurrent(&conn).await?;
					return Err(anyhow::anyhow!("Failed to insert in chunk: {e}"));
				}
			}

			let _ = Self::commit_concurrent(&conn).await;
		}

		// Enqueue all successfully inserted measurement timestamps for incremental batch processing
		if !successful_timestamps.is_empty() {
			self.enqueue_unbatched_measurements(aspect_id, &successful_timestamps).await?;
		}

		Ok(successful)
	}

	/// Insert unprocessed batch (implement missing trait method)
	async fn insert_unprocessed_batch(&self, aspect_id: &AspectId, batch: &Batch) -> Result<TxId> {
		let tx_id = TxId::new();
		let measurements_json = serde_json::to_string(&batch.measurements).map_err(|e| Error::DatabaseError(format!("Failed to serialize: {e}")))?;
		let batch_hash = format!("{:x}", md5::compute(&measurements_json));
		let batch_id = batch.id().to_string();
		let db = self.get_unprocessed_batches_db(aspect_id).await?;
		let db_path = self.get_unprocessed_batches_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		// Simple INSERT - no unique constraints with MVCC
		let insert_sql = r"
			INSERT INTO batches (id, aspect_id, database_id, size, resolution, measurements, batch_hash, status, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
		";

		let batch_metadata_size: i64 = match i64::try_from(batch.metadata.size) {
			Ok(size) => size,
			Err(e) => {
				return Err(anyhow::anyhow!("Batch size conversion error: {e}"));
			}
		};

		let res = conn.as_ref().execute(insert_sql, turso::params![batch_id.clone(), aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string(), batch_metadata_size, format!("{}", batch.metadata.resolution), measurements_json.clone(), batch_hash.clone(), "unprocessed", chrono::Utc::now().timestamp_millis()]).await;
		match res {
			Ok(_) => {}
			Err(e) => {
				tracing::warn!("Failed to insert unprocessed batch {batch_id}: {e}");
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to insert unprocessed batch: {e}"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;

		Ok(tx_id)
	}

	/// Batch insert unprocessed batches (implement missing trait method)
	async fn batch_insert_unprocessed_batches(&self, aspect_id: &AspectId, batches: Vec<Batch>) -> Result<Vec<TxId>> {
		let total = batches.len();
		let mut tx_ids = Vec::with_capacity(total);
		let report_interval = std::cmp::max(1000, total / 10); // Report every 1000 or 10% (whichever is larger)

		for (i, b) in batches.into_iter().enumerate() {
			let tx = self.insert_unprocessed_batch(aspect_id, &b).await?;
			tx_ids.push(tx);

			// Report progress intermittently
			if (i + 1) % report_interval == 0 || i + 1 == total {
				tracing::debug!("Inserted unprocessed batches: {}/{}", i + 1, total);
			}
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
			Self::rollback_concurrent(&conn).await?;
			return Err(anyhow::anyhow!("Failed to remove unprocessed batch: {e}"));
		}

		let _ = Self::commit_concurrent(&conn).await;

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
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to clear unprocessed batches: {e}"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;
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
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to cleanup unprocessed batches: {e}"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;
		let log = format!("Cleaned up unprocessed batches older than {older_than} for aspect {aspect_id}");
		Ok(self.record_transaction(&log).await?)
	}

	/// Insert processed batch (implement missing trait method)
	async fn insert_processed_batch(&self, aspect_id: &AspectId, batch: &Batch) -> Result<TxId> {
		let tx_id = TxId::new();
		let measurements_json = serde_json::to_string(&batch.measurements).map_err(|e| Error::DatabaseError(format!("Failed to serialize: {e}")))?;
		let batch_hash = format!("{:x}", md5::compute(&measurements_json));
		let batch_id = batch.batch_id().to_string();
		let db = self.get_processed_batches_db(aspect_id).await?;
		let db_path = self.get_processed_batches_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		let insert_sql = r"INSERT INTO batches (id, aspect_id, database_id, size, resolution, measurements, batch_hash, status, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)";

		let batch_metadata_size: i64 = match i64::try_from(batch.metadata.size) {
			Ok(size) => size,
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Batch size conversion error: {e}"));
			}
		};

		let res = conn.as_ref().execute(insert_sql, turso::params![batch_id.clone(), aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string(), batch_metadata_size, format!("{}", batch.metadata.resolution), measurements_json.clone(), batch_hash.clone(), "processed", chrono::Utc::now().timestamp_millis()]).await;
		if let Err(e) = res {
			tracing::warn!("Failed to insert processed batch {batch_id}: {e}");
			Self::rollback_concurrent(&conn).await?;
			return Err(anyhow::anyhow!("Failed to insert processed batch: {e}"));
		}

		let _ = Self::commit_concurrent(&conn).await;

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
			Self::rollback_concurrent(&conn).await?;
			return Err(anyhow::anyhow!("Failed to remove processed batch: {e}"));
		}

		let _ = Self::commit_concurrent(&conn).await;

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

		let _ = Self::commit_concurrent(&conn).await;
		Ok(())
	}

	/// clear all processed batches for a given aspect
	async fn clear_processed_batches(&self, aspect_id: &AspectId) -> Result<TxId> {
		let db = self.get_processed_batches_db(aspect_id).await?;
		let db_path = self.get_processed_batches_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let delete_sql = r"DELETE FROM batches";

		let res = conn.as_ref().execute(delete_sql, turso::params![]).await;
		match res {
			Ok(_) => tracing::debug!("Cleared all processed batches for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to clear processed batches: {e}"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;
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
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to cleanup processed batches: {e}"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;
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
				let batch_hash = format!("{:x}", md5::compute(&measurements_json));
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

		let _ = Self::commit_concurrent(&conn).await;
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

		let _ = Self::commit_concurrent(&conn).await;
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
				Self::rollback_concurrent(&conn).await?;
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
					Self::rollback_concurrent(&conn).await?;
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
					Self::rollback_concurrent(&conn).await?;
					return Err(anyhow::anyhow!("Failed to insert relative: {e}"));
				}
			}
		}

		let _ = Self::commit_concurrent(&conn).await;

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
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to remove pattern: {e}"));
			}
		}
		let _ = Self::commit_concurrent(&conn).await;
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
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to clear patterns: {e}"));
			}
		}
		let _ = Self::commit_concurrent(&conn).await;
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

		let _ = Self::commit_concurrent(&conn).await;

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
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to remove event: {e}"));
			}
		}
		let _ = Self::commit_concurrent(&conn).await;
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
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to clear events: {e}"));
			}
		}
		let _ = Self::commit_concurrent(&conn).await;
		let log = format!("Cleared all events for aspect {aspect_id}");
		let _ = self.record_transaction(&log).await?;
		Ok(TxId::new())
	}

	async fn set_dictionary_metadata(&self, aspect_id: &AspectId, dictionary_name: &str, metadata: &DictionaryMetadata) -> Result<TxId> {
		let tx_id = TxId::new();
		let aspect = self.get_aspect(aspect_id).await?;
		let db_name = &self.name;
		let db_path = Self::aspect_dictionaries_db_path(db_name.as_str(), aspect.subject_name(), aspect.name(), dictionary_name);
		let (db, _was_new) = Self::get_or_create_turso_database(&db_path).await?;
		let conn = Self::begin_concurrent(&db, db_name, Some(self.cache.clone())).await?;

		// Ensure dictionary tables exist (create if not exists)
		// Note: No PRIMARY KEY on TEXT columns or indexes to support MVCC
		conn.as_ref()
			.execute(
				r"CREATE TABLE IF NOT EXISTS dictionary_metadata (
			id TEXT NOT NULL,
			name TEXT NOT NULL,
			description TEXT,
			created_at INTEGER NOT NULL
		)",
				turso::params![],
			)
			.await?;

		conn.as_ref()
			.execute(
				r"CREATE TABLE IF NOT EXISTS dictionary_constraints (
			id INTEGER PRIMARY KEY AUTOINCREMENT,
			dictionary_id TEXT NOT NULL,
			steps_count INTEGER,
			steps_interpolation TEXT
		)",
				turso::params![],
			)
			.await?;

		conn.as_ref()
			.execute(
				r"CREATE TABLE IF NOT EXISTS dictionary_variabilities (
			id INTEGER PRIMARY KEY AUTOINCREMENT,
			dictionary_id TEXT NOT NULL,
			variability_type TEXT NOT NULL,
			variability_value TEXT NOT NULL
		)",
				turso::params![],
			)
			.await?;

		conn.as_ref()
			.execute(
				r"CREATE TABLE IF NOT EXISTS dictionary_patterns (
			id INTEGER PRIMARY KEY AUTOINCREMENT,
			dictionary_id TEXT NOT NULL,
			pattern_id TEXT NOT NULL,
			added_at INTEGER NOT NULL
		)",
				turso::params![],
			)
			.await?;

		// Insert dictionary metadata (no ON CONFLICT since no unique constraint - app handles duplicates)
		let insert_sql = r"INSERT INTO dictionary_metadata (id, name, description, created_at) VALUES (?, ?, ?, ?)";
		let res = conn.as_ref().execute(insert_sql, turso::params![metadata.id.as_uuid().to_string(), dictionary_name, metadata.description.clone(), chrono::Utc::now().timestamp_millis()]).await;
		match res {
			Ok(_) => (),
			Err(e) => {
				tracing::warn!("Failed to set metadata for dictionary '{dictionary_name}' for aspect {aspect_id}: {e}");
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to set dictionary metadata: {e}"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;

		let log = format!("Set metadata for dictionary '{dictionary_name}' for aspect {aspect_id}");
		let _ = self.record_transaction(&log).await?;

		Ok(tx_id)
	}

	/// insert pattern into dictionary for a given aspect
	async fn insert_pattern_into_dictionary(&self, aspect_id: &AspectId, dictionary_name: &str, pattern: &Pattern) -> Result<TxId> {
		let tx_id = TxId::new();
		let aspect = self.get_aspect(aspect_id).await?;
		let db_name = &self.name;
		let db_path = Self::aspect_dictionaries_db_path(db_name.as_str(), aspect.subject_name(), aspect.name(), dictionary_name);
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
				Self::rollback_concurrent(&conn).await?;
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
					Self::rollback_concurrent(&conn).await?;
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
					Self::rollback_concurrent(&conn).await?;
					return Err(anyhow::anyhow!("Failed to insert relative: {e}"));
				}
			}
		}

		let _ = Self::commit_concurrent(&conn).await;

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
				Self::rollback_concurrent(&conn).await?;
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
					Self::rollback_concurrent(&conn).await?;
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
					Self::rollback_concurrent(&conn).await?;
					return Err(anyhow::anyhow!("Failed to insert correlation occurrence: {e}"));
				}
			}
		}

		let _ = Self::commit_concurrent(&conn).await;

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
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to update correlation: {e}"));
			}
		}

		// Delete existing error rates and re-insert
		let delete_err_sql = r"DELETE FROM correlation_error_rates WHERE correlation_id = ?";
		let res = conn.as_ref().execute(delete_err_sql, turso::params![correlation.id().to_string()]).await;
		if let Err(e) = res {
			let id = correlation.id();
			tracing::warn!("Failed to delete error rates for correlation '{id}': {e}");
			Self::rollback_concurrent(&conn).await?;
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
					Self::rollback_concurrent(&conn).await?;
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
			Self::rollback_concurrent(&conn).await?;
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
					Self::rollback_concurrent(&conn).await?;
					return Err(anyhow::anyhow!("Failed to insert correlation occurrence: {e}"));
				}
			}
		}

		let _ = Self::commit_concurrent(&conn).await;

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
			Self::rollback_concurrent(&conn).await?;
			return Err(anyhow::anyhow!("Failed to delete correlation error rates: {e}"));
		}

		// Delete occurrences
		let delete_occ_sql = r"DELETE FROM correlation_occurrences WHERE correlation_id = ?";
		let res = conn.as_ref().execute(delete_occ_sql, turso::params![correlation_id.to_string()]).await;
		if let Err(e) = res {
			tracing::warn!("Failed to delete occurrences for correlation '{correlation_id}': {e}");
			Self::rollback_concurrent(&conn).await?;
			return Err(anyhow::anyhow!("Failed to delete correlation occurrences: {e}"));
		}

		// Delete the correlation itself
		let delete_sql = r"DELETE FROM correlations WHERE id = ?";
		let res = conn.as_ref().execute(delete_sql, turso::params![correlation_id.to_string()]).await;
		match res {
			Ok(_) => tracing::debug!("Removed correlation {correlation_id} for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to remove correlation: {e}"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;
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
		let _ = Self::commit_concurrent(&conn).await;
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
			Self::rollback_concurrent(&conn).await?;
			return Err(anyhow::anyhow!("Failed to delete unprocessed event manifestations: {e}"));
		}

		// Delete the unprocessed event itself
		let delete_sql = r"DELETE FROM events WHERE id = ?";
		let res = conn.as_ref().execute(delete_sql, turso::params![event_id.to_string()]).await;
		match res {
			Ok(_) => tracing::debug!("Removed unprocessed event {event_id} for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to remove unprocessed event: {e}"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;
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
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to clear unprocessed event manifestations: {e}"));
			}
		}

		let delete_sql = r"DELETE FROM events";
		let res = conn.as_ref().execute(delete_sql, turso::params![]).await;
		match res {
			Ok(_) => tracing::debug!("Cleared all unprocessed events for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to clear unprocessed events: {e}"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;
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
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to cleanup unprocessed events: {e}"));
			}
		}
		let _ = Self::commit_concurrent(&conn).await;
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
		let _ = Self::commit_concurrent(&conn).await;
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
			Self::rollback_concurrent(&conn).await?;
			return Err(anyhow::anyhow!("Failed to delete processed event manifestations: {e}"));
		}

		// Delete the processed event itself
		let delete_sql = r"DELETE FROM events WHERE id = ?";
		let res = conn.as_ref().execute(delete_sql, turso::params![event_id.to_string()]).await;
		match res {
			Ok(_) => tracing::debug!("Removed processed event {event_id} for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to remove processed event: {e}"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;
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
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to clear processed event manifestations: {e}"));
			}
		}

		let delete_sql = r"DELETE FROM events";
		let res = conn.as_ref().execute(delete_sql, turso::params![]).await;
		match res {
			Ok(_) => tracing::debug!("Cleared all processed events for aspect {aspect_id}"),
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to clear processed events: {e}"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;
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
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to cleanup processed events: {e}"));
			}
		}
		let _ = Self::commit_concurrent(&conn).await;
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
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to delete measurements in range: {e}"));
			}

			let _ = Self::commit_concurrent(&conn).await;
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
			Self::rollback_concurrent(&conn).await?;
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
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to insert compressed measurements: {e}"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;
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
		let mut rows = conn
			.as_ref()
			.query(
				"SELECT time_range_start, time_range_end FROM compression_tier_results WHERE time_range_start <= ? AND time_range_end >= ? LIMIT 1",
				turso::params![timestamp.timestamp_millis(), timestamp.timestamp_millis()],
			)
			.await?;

		let compressed_range = if let Some(row) = rows.next().await? {
			let start_millis = *row.get_value(0)?.as_integer().unwrap_or(&0);
			let end_millis = *row.get_value(1)?.as_integer().unwrap_or(&0);
			Some((start_millis, end_millis))
		} else {
			None
		};

		// Need to commit this read transaction before starting a new write
		let _ = Self::commit_concurrent(&conn).await;

		if let Some((start_millis, end_millis)) = compressed_range {
			// Open a new connection for the write
			let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

			let now = chrono::Utc::now();

			// Insert the dirty region
			conn.as_ref()
				.execute(
					"INSERT INTO dirty_regions (region_start, region_end, marked_at, reason) VALUES (?, ?, ?, ?)",
					turso::params![
						start_millis,
						end_millis,
						now.timestamp_millis(),
						"New measurement inserted into compressed range".to_string(),
					],
				)
				.await?;

			// Update dirty_regions_count in compression_state
			conn.as_ref()
				.execute(
					"UPDATE compression_state SET dirty_regions_count = (SELECT COUNT(*) FROM dirty_regions) WHERE id = 1",
					turso::params![],
				)
				.await?;

			let _ = Self::commit_concurrent(&conn).await;

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
		let mut rows = conn
			.as_ref()
			.query(
				"SELECT DISTINCT time_range_start, time_range_end FROM compression_tier_results WHERE time_range_start <= ? AND time_range_end >= ?",
				turso::params![max_timestamp.timestamp_millis(), min_timestamp.timestamp_millis()],
			)
			.await?;

		let mut compressed_ranges = Vec::new();
		while let Some(row) = rows.next().await? {
			let start_millis = *row.get_value(0)?.as_integer().unwrap_or(&0);
			let end_millis = *row.get_value(1)?.as_integer().unwrap_or(&0);
			compressed_ranges.push((start_millis, end_millis));
		}

		// Need to commit this read transaction before starting a new write
		let _ = Self::commit_concurrent(&conn).await;

		if !compressed_ranges.is_empty() {
			let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
			let now = chrono::Utc::now();

			for (start_millis, end_millis) in &compressed_ranges {
				// Insert the dirty region
				conn.as_ref()
					.execute(
						"INSERT INTO dirty_regions (region_start, region_end, marked_at, reason) VALUES (?, ?, ?, ?)",
						turso::params![
							*start_millis,
							*end_millis,
							now.timestamp_millis(),
							"Batch insert overlapped with compressed range".to_string(),
						],
					)
					.await?;
			}

			// Update dirty_regions_count in compression_state
			conn.as_ref()
				.execute(
					"UPDATE compression_state SET dirty_regions_count = (SELECT COUNT(*) FROM dirty_regions) WHERE id = 1",
					turso::params![],
				)
				.await?;

			let _ = Self::commit_concurrent(&conn).await;

			tracing::debug!(
				aspect_id = %aspect_id,
				dirty_regions_count = compressed_ranges.len(),
				"Marked dirty regions for batch insert overlapping compressed ranges"
			);
		}

		Ok(())
	}
}
