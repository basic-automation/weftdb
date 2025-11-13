use anyhow::{bail, Result};

use crate::{
	cache::Connection, database::traits::Inputs, types::{
		database::{
			helpers::safe_usize_to_f64, traits::{aspect_structure::AspectStructure, connection::Connection as ConnectionTrait, DatabaseStructure}
		}, TxId
	}, AspectId, Batch, BatchId, Database, DatasetId, Error, InputMeasurement, Measurement
};

#[async_trait::async_trait]
impl Inputs for Database {
	/// Capture a new measurement for a given aspect
	/// If a measurement with the same timestamp already exists, the average of the two values is stored.
	/// Uses Turso's concurrent writes feature for better performance and conflict resolution.
	///
	/// # Errors
	/// - if aspect not found
	/// - if unable to insert measurement into database
	async fn capture_measurement(&self, aspect_id: &AspectId, dataset_id: &DatasetId, input_measurement: &InputMeasurement) -> Result<TxId> {
		let mut aspect = self.get_aspect(aspect_id).await?;
		let measurement_db = aspect.measurements().await?;
		let cache_key = format!("measurement_db_{aspect_id}_{dataset_id}");
		let tx_id = TxId::new();

		let measurement = Measurement::from_input_measurement(dataset_id, input_measurement);

		// Use INSERT ... ON CONFLICT for atomic upsert with concurrent writes support
		// This handles the case where a measurement with the same timestamp already exists
		let upsert_sql = r"
			INSERT INTO measurements (id, dataset_id, timestamp, value) 
			VALUES (?, ?, ?, ?) 
			ON CONFLICT(timestamp) DO UPDATE SET 
				value = (CAST(excluded.value AS REAL) + CAST(measurements.value AS REAL)) / 2.0,
				id = excluded.id
		";

		// Execute with BEGIN CONCURRENT and retry on lock/conflict
		let conn = Self::begin_concurrent(&measurement_db, &cache_key, Some(self.cache.clone())).await?;
		let res = conn.as_ref().execute(upsert_sql, turso::params![tx_id.as_uuid().to_string(), dataset_id.as_uuid().to_string(), measurement.timestamp().timestamp_millis().to_string(), measurement.value().to_string()]).await;
		match res {
			Ok(_) => println!("Successfully upserted measurement for dataset {dataset_id}"),
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure 1: `{}`", e));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;

		// Record the transaction - we don't know if it was an insert or update, but that's okay
		self.record_transaction(&format!("Captured measurement at {} with value {} for dataset {} (upsert operation)", measurement.timestamp(), measurement.value(), dataset_id)).await
	}

	/// Capture new measurements for a given aspect
	/// If a measurement with the same timestamp already exists, an error is returned.
	/// Uses Turso's concurrent writes feature for better performance.
	///
	/// # Errors
	/// - if measurement with the same timestamp already exists
	async fn capture_new_measurement(&self, aspect_id: &AspectId, dataset_id: &DatasetId, input_measurement: &InputMeasurement) -> Result<TxId> {
		let mut aspect = self.get_aspect(aspect_id).await?;
		let measurement_db = aspect.measurements().await?;
		let measurement_db_path = aspect.aspect_path();
		let tx_id = TxId::new();

		let measurement = Measurement::from_input_measurement(dataset_id, input_measurement);

		// Use INSERT with ON CONFLICT DO NOTHING, then check if any rows were affected
		// This is atomic and handles concurrent writes safely
		let insert_sql = r"
			INSERT INTO measurements (id, dataset_id, timestamp, value) 
			VALUES (?, ?, ?, ?) 
			ON CONFLICT(timestamp) DO NOTHING
		";

		let conn = Self::begin_concurrent(&measurement_db, measurement_db_path, Some(self.cache.clone())).await?;
		let res = conn.as_ref().execute(insert_sql, turso::params![tx_id.as_uuid().to_string(), dataset_id.as_uuid().to_string(), measurement.timestamp().timestamp_millis().to_string(), measurement.value().to_string()]).await;
		let rows_affected = match res {
			Ok(rows) => {
				println!("Inserted {rows} rows");
				rows
			}
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure 2: `{}`", e));
			}
		};

		let _ = Self::commit_concurrent(&conn).await;

		// Check if the insert actually happened (rows_affected > 0 means it was inserted)
		if rows_affected == 0 {
			bail!("Measurement with timestamp {} already exists", measurement.timestamp());
		}

		self.record_transaction(&format!("Inserted new measurement at {} for dataset {}", measurement.timestamp(), dataset_id)).await
	}

	/// Batch insert measurements for better performance using Turso's concurrent writes
	/// Uses INSERT ... ON CONFLICT for automatic conflict resolution with averaging
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

		let mut aspect = self.get_aspect(&aspect_id).await?;
		let measurement_db = aspect.measurements().await?;

		// Generate all TxIds upfront
		let all_tx_ids: Vec<TxId> = (0..input_measurements.len()).map(|_| TxId::new()).collect();

		// Compute min/max upfront
		let min_new = input_measurements.iter().map(InputMeasurement::timestamp).min().unwrap();
		let max_new = input_measurements.iter().map(InputMeasurement::timestamp).max().unwrap();

		// Print initial progress message for large batches
		if input_measurements.len() > 100_000 {
			println!("Loading {} measurements for aspect '{}'...", input_measurements.len(), aspect.name());
		}

		// Process in chunks with single transaction per chunk to reduce contention
		let chunk_size = if input_measurements.len() > 100_000 { 1000 } else { 500 };

		for (chunk_idx, chunk) in input_measurements.chunks(chunk_size).enumerate() {
			// Progress reporting for large batches
			if input_measurements.len() > 100_000 && chunk_idx % 100 == 0 && chunk_idx > 0 {
				if let (Ok(processed_f64), Ok(len_f64)) = (safe_usize_to_f64(chunk_idx * chunk_size), safe_usize_to_f64(input_measurements.len())) {
					let pct = (processed_f64 / len_f64) * 100.0;
					println!("  {} - {:.0}%", aspect.name(), pct);
				} else {
					println!("  {} - processed {} / {} measurements", aspect.name(), chunk_idx * chunk_size, input_measurements.len());
				}
			}

			// Process chunk with robust retry
			let mut conn = measurement_db.connect()?;
			self.capture_measurement_chunk(&mut conn, dataset_id, chunk, &all_tx_ids, chunk_idx * chunk_size).await?;
			tokio::task::yield_now().await;
		}

		// Invalidate cache
		let cache_key = format!("aspect_measurements_{}", aspect.id().as_uuid());
		self.cache.lock().await.invalidate(&cache_key).await;

		// Update earliest and latest in metadata
		self.update_aspect_timestamps(&aspect.id(), min_new, max_new).await?;

		Ok(all_tx_ids)
	}

	/// Helper: Process a chunk of measurements with extended retries and exponential backoff
	async fn capture_measurement_chunk(&self, conn: &mut turso::Connection, dataset_id: DatasetId, chunk: &[InputMeasurement], all_tx_ids: &[TxId], tx_id_offset: usize) -> Result<()> {
		if chunk.is_empty() {
			return Ok(());
		}

		let mut attempt = 0;
		let max_attempts = 15;

		loop {
			tokio::task::yield_now().await;
			let tx = match conn.transaction().await {
				Ok(tx) => tx,
				Err(e) => {
					let em = e.to_string().to_lowercase();
					if attempt < max_attempts && (em.contains("locked") || em.contains("busy")) {
						attempt += 1;
						let attempt_min_8: u32 = u32::try_from(attempt.min(8)).unwrap_or(8);
						let sleep_ms = 200u64 * (1u64 << attempt_min_8);
						tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
						continue;
					}
					return Err(Error::DatabaseError(format!("Failed to begin chunk transaction: {e}")).into());
				}
			};

			let result = async {
				for (i, input_measurement) in chunk.iter().enumerate() {
					let tx_id = &all_tx_ids[tx_id_offset + i];
					let measurement = Measurement::from_input_measurement(&dataset_id, input_measurement);

					let upsert_sql = r"
						INSERT INTO measurements (id, dataset_id, timestamp, value) 
						VALUES (?, ?, ?, ?) 
						ON CONFLICT(timestamp) DO UPDATE SET 
							value = (CAST(excluded.value AS REAL) + CAST(measurements.value AS REAL)) / 2.0,
							id = excluded.id
					";

					tx.execute(upsert_sql, turso::params![tx_id.as_uuid().to_string(), dataset_id.as_uuid().to_string(), measurement.timestamp().timestamp_millis().to_string(), measurement.value().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to insert measurement in chunk: {e}")))?;
				}
				Ok::<(), anyhow::Error>(())
			}
			.await;

			match result {
				Ok(()) => {
					// Commit the transaction
					match tx.commit().await {
						Ok(()) => {
							break;
						}
						Err(e) => {
							// Transaction is consumed by commit(), can't rollback
							let em = e.to_string().to_lowercase();
							if attempt < max_attempts && (em.contains("locked") || em.contains("busy")) {
								attempt += 1;
								let attempt_min_8: u32 = u32::try_from(attempt.min(8)).unwrap_or(8);
								let sleep_ms = 400u64 * (1u64 << attempt_min_8);
								tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
								continue;
							}
							return Err(Error::DatabaseError(format!("Failed to commit chunk transaction: {e}")).into());
						}
					}
				}
				Err(e) => {
					let _ = tx.rollback().await;
					let em = e.to_string().to_lowercase();
					if em.contains("locked") || em.contains("busy") || em.contains("conflict") {
						attempt += 1;
						if attempt >= max_attempts {
							return Err(Error::DatabaseError(format!("Failed to process chunk after retries: {e}")).into());
						}
						let attempt_min_8: u32 = u32::try_from(attempt.min(8)).unwrap_or(8);
						let sleep_ms = 600u64 * (1u64 << attempt_min_8);
						tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
						continue;
					}
					return Err(e);
				}
			}
		}

		Ok(())
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
	async fn capture_new_measurement_chunk(&self, db: &turso::Database, db_path: &str, dataset_id: &DatasetId, chunk: &[InputMeasurement], all_tx_ids: &[TxId], tx_id_offset: usize) -> Result<Vec<TxId>> {
		let mut successful = Vec::new();
		for (i, m) in chunk.iter().enumerate() {
			let tx_id = &all_tx_ids[tx_id_offset + i];
			let measurement = Measurement::from_input_measurement(dataset_id, m);
			let insert_sql = r"
				INSERT INTO measurements (id, dataset_id, timestamp, value) 
				VALUES (?, ?, ?, ?) 
				ON CONFLICT(timestamp) DO NOTHING
			";

			let mut rows_affected = 0;

			let conn = Self::begin_concurrent(db, db_path, Some(self.cache.clone())).await?;
			let res = conn.as_ref().execute(insert_sql, turso::params![tx_id.as_uuid().to_string(), dataset_id.as_uuid().to_string(), measurement.timestamp().timestamp_millis().to_string(), measurement.value().to_string()]).await;
			rows_affected = match res {
				Ok(rows) => {
					println!("Inserted measurement for tx_id {tx_id}: {rows_affected} rows affected");
					rows
				}
				Err(e) => {
					Self::rollback_concurrent(&conn).await?;
					return Err(anyhow::anyhow!("Failed to insert in chunk: {e}"));
				}
			};

			let _ = Self::commit_concurrent(&conn).await;

			if rows_affected > 0 {
				successful.push(*tx_id);
			}
		}
		Ok(successful)
	}

	/// Insert unprocessed batch (implement missing trait method)
	async fn insert_unprocessed_batch(&self, aspect_id: &AspectId, batch: &Batch) -> Result<TxId> {
		let tx_id = TxId::new();
		let measurements_json = serde_json::to_string(&batch.measurements).map_err(|e| Error::DatabaseError(format!("Failed to serialize: {e}")))?;
		let batch_hash = format!("{:x}", md5::compute(&measurements_json));
		let batch_id = batch.id().to_string();
		let mut aspect = self.get_aspect(aspect_id).await?;
		let conn = Self::begin_concurrent(&aspect.measurements().await?, &format!("{}/measurements.db", aspect.aspect_path()), Some(self.cache.clone())).await?;
		let insert_sql = r"
			INSERT INTO batches (id, aspect_id, database_id, size, resolution, measurements, batch_hash, status, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
			ON CONFLICT(id) DO NOTHING
		";

		let batch_metadata_size: i64 = match i64::try_from(batch.metadata.size) {
			Ok(size) => size,
			Err(e) => {
				return Err(anyhow::anyhow!("Batch size conversion error: {e}"));
			}
		};

		let res = conn.as_ref().execute(insert_sql, turso::params![batch_id.clone(), aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string(), batch_metadata_size, format!("{:?}", batch.metadata.resolution), measurements_json.clone(), batch_hash.clone(), "unprocessed", chrono::Utc::now().timestamp_millis()]).await;
		match res {
			Ok(_) => println!("Inserted unprocessed batch {batch_id}"),
			Err(e) => {
				println!("Failed to insert unprocessed batch {batch_id}: {e}");
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to insert unprocessed batch: {e}"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;

		Ok(tx_id)
	}

	/// Batch insert unprocessed batches (implement missing trait method)
	async fn batch_insert_unprocessed_batches(&self, aspect_id: &AspectId, batches: Vec<Batch>) -> Result<Vec<TxId>> {
		let mut tx_ids = Vec::new();
		for b in batches {
			let tx = self.insert_unprocessed_batch(aspect_id, &b).await?;
			tx_ids.push(tx);
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

			let res = conn.as_ref().execute(insert_sql, turso::params![b.id().to_string(), b.metadata.aspect.as_uuid().to_string(), self.id().as_uuid().to_string(), batch_metadata_size, format!("{:?}", b.metadata.resolution), "{}", batch_measurements_len, "stub_hash", "unprocessed", chrono::Utc::now().timestamp_millis()]).await;
			match res {
				Ok(_) => println!("Inserted batch chunk {}", b.id()),
				Err(e) => {
					return Err(anyhow::anyhow!("Failed to insert batch chunk: {e}"));
				}
			}
			tx_ids.push(tx_id);
		}
		Ok(tx_ids)
	}

	async fn remove_unprocessed_batch(&self, aspect_id: &AspectId, batch_id: &BatchId) -> Result<TxId> {
		let tx_id = TxId::new();

		let mut aspect = self.get_aspect(aspect_id).await?;
		let db = aspect.unprocessed_batches().await?;
		let db_path = aspect.unprocessed_batches_path();
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let delete_sql = r"DELETE FROM batches WHERE id = ?";

		let res = conn.as_ref().execute(delete_sql, turso::params![batch_id.to_string()]).await;
		match res {
			Ok(_) => println!("Removed unprocessed batch {batch_id}"),
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to remove unprocessed batch: {e}"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;
		Ok(tx_id)
	}

	/// Insert processed batch (implement missing trait method)
	async fn insert_processed_batch(&self, aspect_id: &AspectId, batch: &Batch) -> Result<TxId> {
		let tx_id = TxId::new();
		let measurements_json = serde_json::to_string(&batch.measurements).map_err(|e| Error::DatabaseError(format!("Failed to serialize: {e}")))?;
		let batch_hash = format!("{:x}", md5::compute(&measurements_json));
		let batch_id = batch.batch_id().to_string();
		let mut aspect = self.get_aspect(aspect_id).await?;
		let conn = Self::begin_concurrent(&aspect.measurements().await?, &format!("{}/measurements.db", aspect.aspect_path()), Some(self.cache.clone())).await?;
		let insert_sql = r"INSERT INTO batches (id, aspect_id, database_id, size, resolution, measurements, batch_hash, status, created_at, processed_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

		let batch_metadata_size: i64 = match i64::try_from(batch.metadata.size) {
			Ok(size) => size,
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Batch size conversion error: {e}"));
			}
		};

		let res = conn.as_ref().execute(insert_sql, turso::params![batch_id.clone(), aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string(), batch_metadata_size, format!("{:?}", batch.metadata.resolution), measurements_json.clone(), batch_hash.clone(), "processed", chrono::Utc::now().timestamp_millis(), chrono::Utc::now().timestamp_millis()]).await;
		match res {
			Ok(_) => println!("Inserted processed batch {batch_id}"),
			Err(e) => {
				println!("Failed to insert processed batch {batch_id}: {e}");
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to insert processed batch: {e}"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;

		Ok(tx_id)
	}

	/// Batch insert processed batches (implement missing trait method)
	async fn batch_insert_processed_batches(&self, aspect_id: &AspectId, batches: Vec<Batch>) -> Result<Vec<TxId>> {
		let mut tx_ids = Vec::new();
		for b in batches {
			let tx = self.insert_processed_batch(aspect_id, &b).await?;
			tx_ids.push(tx);
		}
		Ok(tx_ids)
	}
}
