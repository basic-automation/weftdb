use anyhow::{bail, Result};

use crate::{
	database::traits::Inputs, types::{
		database::{
			helpers::safe_usize_to_f64, traits::{aspect_structure::AspectStructure, DatabaseStructure}
		}, Transaction, TxId
	}, AspectId, Batch, Database, DatasetId, Error, InputMeasurement, Measurement, CACHE
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
	async fn capture_measurement(&self, aspect_id: AspectId, dataset_id: DatasetId, input_measurement: InputMeasurement) -> Result<TxId> {
		let mut aspect = self.get_aspect(aspect_id).await?;
		let measurement_db = aspect.measurements().await?;
		let conn = measurement_db.connect()?;
		let tx_id = TxId::new();

		let measurement = Measurement::from_input_measurement(dataset_id, &input_measurement);

		// Use INSERT ... ON CONFLICT for atomic upsert with concurrent writes support
		// This handles the case where a measurement with the same timestamp already exists
		let upsert_sql = r#"
                        INSERT INTO measurements (id, dataset_id, timestamp, value) 
                        VALUES (?, ?, ?, ?) 
                        ON CONFLICT(timestamp) DO UPDATE SET 
                                value = (CAST(excluded.value AS REAL) + CAST(measurements.value AS REAL)) / 2.0,
                                id = excluded.id
                "#;

		conn.execute(&upsert_sql, turso::params![tx_id.as_uuid().to_string(), dataset_id.as_uuid().to_string(), measurement.timestamp().timestamp_millis().to_string(), measurement.value().to_string()]).await?;

		// Record the transaction - we don't know if it was an insert or update, but that's okay
		self.record_transaction(&format!("Captured measurement at {} with value {} for dataset {} (upsert operation)", measurement.timestamp(), measurement.value(), dataset_id)).await
	}

	/// Capture new measurements for a given aspect
	/// If a measurement with the same timestamp already exists, an error is returned.
	/// Uses Turso's concurrent writes feature for better performance.
	///
	/// # Errors
	/// - if measurement with the same timestamp already exists
	async fn capture_new_measurement(&self, aspect_id: AspectId, dataset_id: DatasetId, input_measurement: InputMeasurement) -> Result<TxId> {
		let mut aspect = self.get_aspect(aspect_id).await?;
		let measurement_db = aspect.measurements().await?;
		let conn = measurement_db.connect()?;
		let tx_id = TxId::new();

		let measurement = Measurement::from_input_measurement(dataset_id, &input_measurement);

		// Use INSERT with ON CONFLICT DO NOTHING, then check if any rows were affected
		// This is atomic and handles concurrent writes safely
		let insert_sql = r#"
            INSERT INTO measurements (id, dataset_id, timestamp, value) 
            VALUES (?, ?, ?, ?) 
            ON CONFLICT(timestamp) DO NOTHING
        "#;

		let rows_affected = conn.execute(&insert_sql, turso::params![tx_id.as_uuid().to_string(), dataset_id.as_uuid().to_string(), measurement.timestamp().timestamp_millis().to_string(), measurement.value().to_string()]).await?;

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

		let mut aspect = self.get_aspect(aspect_id).await?;
		let measurement_db = aspect.measurements().await?;
		let conn = measurement_db.connect()?;

		// Generate all TxIds upfront
		let all_tx_ids: Vec<TxId> = (0..input_measurements.len()).map(|_| TxId::new()).collect();

		// Compute min/max upfront
		let min_new = input_measurements.iter().map(InputMeasurement::timestamp).min().unwrap();
		let max_new = input_measurements.iter().map(InputMeasurement::timestamp).max().unwrap();

		// Print initial progress message for large batches
		if input_measurements.len() > 100_000 {
			println!("Loading {} measurements for aspect '{}'...", input_measurements.len(), aspect.name());
		}

		// Use concurrent writes - no need for transactions or complex retry logic
		// Process in parallel chunks for better performance
		let chunk_size = if input_measurements.len() > 100_000 { 1000 } else { 500 };
		let chunks: Vec<_> = input_measurements.chunks(chunk_size).enumerate().collect();

		for (chunk_idx, chunk) in chunks {
			// Progress reporting for large batches
			if input_measurements.len() > 100_000 && chunk_idx % 100 == 0 && chunk_idx > 0 {
				if let (Ok(processed_f64), Ok(len_f64)) = (safe_usize_to_f64(chunk_idx * chunk_size), safe_usize_to_f64(input_measurements.len())) {
					let pct = (processed_f64 / len_f64) * 100.0;
					println!("  {} - {:.0}%", aspect.name(), pct);
				} else {
					println!("  {} - processed {} / {} measurements", aspect.name(), chunk_idx * chunk_size, input_measurements.len());
				}
			}

			// Process chunk using concurrent writes
			self.capture_measurement_chunk(&conn, dataset_id, chunk, &all_tx_ids, chunk_idx * chunk_size).await?;
		}

		// Invalidate cache
		let cache_key = format!("aspect_measurements_{}", aspect.id().as_uuid());
		CACHE.invalidate_aspect_cache(&cache_key).await;

		// Update earliest and latest in metadata
		self.update_aspect_timestamps(&aspect.id(), min_new, max_new).await?;

		Ok(all_tx_ids)
	}

	/// Helper using individual execute calls for maximum compatibility
	async fn capture_measurement_chunk(&self, conn: &turso::Connection, dataset_id: DatasetId, chunk: &[InputMeasurement], all_tx_ids: &[TxId], tx_id_offset: usize) -> Result<()> {
		// Use the fixed measurements table with dataset_id
		let upsert_sql = r#"
            INSERT INTO measurements (id, dataset_id, timestamp, value) 
            VALUES (?, ?, ?, ?) 
            ON CONFLICT(timestamp) DO UPDATE SET 
                value = (CAST(excluded.value AS REAL) + CAST(measurements.value AS REAL)) / 2.0,
                id = excluded.id
        "#;

		// Execute each measurement individually
		for (i, input_measurement) in chunk.iter().enumerate() {
			let tx_id = &all_tx_ids[tx_id_offset + i];
			let measurement = Measurement::from_input_measurement(dataset_id, input_measurement);

			conn.execute(&upsert_sql, turso::params![tx_id.as_uuid().to_string(), dataset_id.as_uuid().to_string(), measurement.timestamp().timestamp_millis().to_string(), measurement.value().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to insert measurement: {e}")))?;
		}

		Ok(())
	}

	/// Batch insert new measurements for better performance using Turso's concurrent writes
	/// Skips measurements with timestamps that already exist (no error thrown)
	/// Uses INSERT ... ON CONFLICT DO NOTHING for automatic conflict resolution
	///
	/// # Errors
	/// - if aspect not found
	/// - if unable to insert measurements into database
	///
	/// # Returns
	/// - Vector of TxIds for measurements that were actually inserted (skipped measurements won't have TxIds in result)
	///
	/// # Panics
	/// - if measurements vector is empty when computing min/max (this is already checked)
	async fn batch_capture_new_measurements(&self, aspect_id: AspectId, dataset_id: DatasetId, input_measurements: Vec<InputMeasurement>) -> Result<Vec<TxId>> {
		if input_measurements.is_empty() {
			return Ok(Vec::new());
		}

		let mut aspect = self.get_aspect(aspect_id).await?;
		let measurement_db = aspect.measurements().await?;
		let conn = measurement_db.connect()?;

		// Generate all TxIds upfront
		let all_tx_ids: Vec<TxId> = (0..input_measurements.len()).map(|_| TxId::new()).collect();

		// Compute min/max upfront
		let min_new = input_measurements.iter().map(InputMeasurement::timestamp).min().unwrap();
		let max_new = input_measurements.iter().map(InputMeasurement::timestamp).max().unwrap();

		// Print initial progress message for large batches
		if input_measurements.len() > 100_000 {
			println!("Loading {} new measurements for aspect '{}'...", input_measurements.len(), aspect.name());
		}

		// Use concurrent writes - no need for transactions or complex retry logic
		// Process in parallel chunks for better performance
		let chunk_size = if input_measurements.len() > 100_000 { 1000 } else { 500 };
		let chunks: Vec<_> = input_measurements.chunks(chunk_size).enumerate().collect();

		let mut successful_tx_ids = Vec::new();

		for (chunk_idx, chunk) in chunks {
			// Progress reporting for large batches
			if input_measurements.len() > 100_000 && chunk_idx % 100 == 0 && chunk_idx > 0 {
				if let (Ok(processed_f64), Ok(len_f64)) = (safe_usize_to_f64(chunk_idx * chunk_size), safe_usize_to_f64(input_measurements.len())) {
					let pct = (processed_f64 / len_f64) * 100.0;
					println!("  {} - {:.0}%", aspect.name(), pct);
				} else {
					println!("  {} - processed {} / {} measurements", aspect.name(), chunk_idx * chunk_size, input_measurements.len());
				}
			}

			// Process chunk using concurrent writes and collect successful TxIds
			let chunk_successful_tx_ids = self.capture_new_measurement_chunk(&conn, dataset_id, chunk, &all_tx_ids, chunk_idx * chunk_size).await?;
			successful_tx_ids.extend(chunk_successful_tx_ids);
		}

		// Invalidate cache
		let cache_key = format!("aspect_measurements_{}", aspect.id().as_uuid());
		CACHE.invalidate_aspect_cache(&cache_key).await;

		// Update earliest and latest in metadata
		self.update_aspect_timestamps(&aspect.id(), min_new, max_new).await?;

		// Print summary for large batches
		if input_measurements.len() > 100_000 {
			let skipped = input_measurements.len() - successful_tx_ids.len();
			println!("  {} - Completed: {} inserted, {} skipped (duplicates)", aspect.name(), successful_tx_ids.len(), skipped);
		}

		for tx_id in &successful_tx_ids {
			let tx = Transaction::new(Some(*tx_id), "Batch capture of new measurements".to_string());
			self.log_transaction(&tx).await?;
		}

		Ok(successful_tx_ids)
	}

	/// Helper for new measurements using individual execute calls
	async fn capture_new_measurement_chunk(&self, conn: &turso::Connection, dataset_id: DatasetId, chunk: &[InputMeasurement], all_tx_ids: &[TxId], tx_id_offset: usize) -> Result<Vec<TxId>> {
		// Use the fixed measurements table with dataset_id
		let insert_sql = r#"
            INSERT INTO measurements (id, dataset_id, timestamp, value) 
            VALUES (?, ?, ?, ?) 
            ON CONFLICT(timestamp) DO NOTHING
        "#;

		let mut successful_tx_ids = Vec::new();

		// Execute each measurement individually and check results
		for (i, input_measurement) in chunk.iter().enumerate() {
			let tx_id = &all_tx_ids[tx_id_offset + i];
			let measurement = Measurement::from_input_measurement(dataset_id, input_measurement);

			let rows_affected = conn.execute(&insert_sql, turso::params![tx_id.as_uuid().to_string(), dataset_id.as_uuid().to_string(), measurement.timestamp().timestamp_millis().to_string(), measurement.value().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to insert measurement: {e}")))?;

			// Only add TxId if the insert was successful (rows_affected > 0)
			if rows_affected > 0 {
				successful_tx_ids.push(*tx_id);
			}
		}

		Ok(successful_tx_ids)
	}

	async fn insert_unprocessed_batch(&self, aspect_id: AspectId, batch: &Batch) -> Result<TxId> {
		let mut aspect = self.get_aspect(aspect_id).await?;
		let unprocessed_batches_db = aspect.unprocessed_batches().await?;
		let conn = unprocessed_batches_db.connect()?;
		let tx_id = TxId::new();

		// Use INSERT ... ON CONFLICT DO NOTHING for concurrent writes support
		// This automatically skips the insert if a batch with the same ID already exists
		let insert_sql = r"
                        INSERT INTO batches (
                                id, 
                                aspect_id, 
                                database_id, 
                                size, 
                                resolution, 
                                status,
                                batch_hash, 
                                created_at, 
                                processed_at,
                                updated_at,
                                metadata_json, 
                                measurements_json
                        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                        ON CONFLICT(id) DO NOTHING
                ";

		// Generate current timestamp for created_at and updated_at
		let current_timestamp = chrono::Utc::now().timestamp_millis();

		let rows_affected = conn
			.execute(
				insert_sql,
				turso::params![
					batch.batch_id().as_uuid().to_string(),
					batch.metadata.aspect.as_uuid().to_string(),
					batch.metadata.database_info.to_json_str()?,
					batch.metadata.size.to_string(),
					batch.metadata.resolution.to_string(),
					"unprocessed", // Default status since Batch doesn't have this field
					batch.batch_hash().unwrap_or(&String::new()).clone(),
					current_timestamp.to_string(),
					Option::<String>::None, // processed_at is None for unprocessed batches
					current_timestamp.to_string(),
					serde_json::to_string(&batch.metadata)?,
					serde_json::to_string(&batch.measurements)?
				],
			)
			.await?;

		// Check if the insert was successful or skipped
		if rows_affected > 0 {
			// Record the transaction for successful insert
			self.record_transaction(&format!("Inserted unprocessed batch {} for aspect {}", batch.batch_id().as_uuid().to_string(), batch.metadata.aspect)).await
		} else {
			// Batch already exists, log that it was skipped
			self.record_transaction(&format!("Skipped duplicate batch {} for aspect {} (already exists)", batch.batch_id().as_uuid().to_string(), batch.metadata.aspect)).await?;
			Ok(tx_id)
		}
	}

	/// Batch insert unprocessed batches for better performance using Turso's concurrent writes
	/// Skips batches with IDs that already exist (no error thrown)
	/// Uses INSERT ... ON CONFLICT DO NOTHING for automatic conflict resolution
	///
	/// # Errors
	/// - if unable to connect to unprocessed batches database
	/// - if unable to insert batches into database
	///
	/// # Returns
	/// - Vector of TxIds for batches that were actually inserted (skipped batches won't have TxIds in result)
	async fn batch_insert_unprocessed_batches(&self, aspect_id: AspectId, batches: Vec<Batch>) -> Result<Vec<TxId>> {
		if batches.is_empty() {
			return Ok(Vec::new());
		}

		let mut aspect = self.get_aspect(aspect_id).await?;
		let unprocessed_batches_db = aspect.unprocessed_batches().await?;
		let conn = unprocessed_batches_db.connect()?;

		// Print initial progress message for large batches
		if batches.len() > 1000 {
			println!("Loading {} unprocessed batches...", batches.len());
		}

		// Use concurrent writes - no need for transactions or complex retry logic
		// Process in chunks for better performance
		let chunk_size = if batches.len() > 10000 { 500 } else { 100 };
		let chunks: Vec<_> = batches.chunks(chunk_size).enumerate().collect();

		let mut successful_tx_ids = Vec::new();

		for (chunk_idx, chunk) in chunks {
			// Progress reporting for large batches
			if batches.len() > 1000 && chunk_idx % 10 == 0 && chunk_idx > 0 {
				if let (Ok(processed_f64), Ok(len_f64)) = (safe_usize_to_f64(chunk_idx * chunk_size), safe_usize_to_f64(batches.len())) {
					let pct = (processed_f64 / len_f64) * 100.0;
					println!("  Batches - {:.0}%", pct);
				} else {
					println!("  Batches - processed {} / {}", chunk_idx * chunk_size, batches.len());
				}
			}

			// Process chunk using concurrent writes and collect successful TxIds
			let chunk_successful_tx_ids = self.insert_batch_chunk(&conn, chunk).await?;
			successful_tx_ids.extend(chunk_successful_tx_ids);
		}

		// Print summary for large batches
		if batches.len() > 1000 {
			let skipped = batches.len() - successful_tx_ids.len();
			println!("  Batches - Completed: {} inserted, {} skipped (duplicates)", successful_tx_ids.len(), skipped);
		}

		Ok(successful_tx_ids)
	}

	/// Helper to process a chunk of batches using concurrent writes
	/// Returns TxIds of batches that were actually inserted (skips duplicates)
	async fn insert_batch_chunk(&self, conn: &turso::Connection, chunk: &[Batch]) -> Result<Vec<TxId>> {
		// Use INSERT ... ON CONFLICT DO NOTHING for concurrent writes
		// This automatically skips batches with existing IDs
		let insert_sql = r"
                        INSERT INTO batches (
                                id, 
                                aspect_id, 
                                database_id, 
                                size, 
                                resolution, 
                                batch_hash, 
                                created_at, 
                                processed_at,
                                updated_at,
                                metadata_json, 
                                measurements_json
                        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                        ON CONFLICT(id) DO NOTHING
                ";

		let mut successful_tx_ids = Vec::new();

		// Execute each batch individually and check results
		for batch in chunk {
			let tx_id = TxId::new();
			let current_timestamp = chrono::Utc::now().timestamp_millis();

			let rows_affected = conn
				.execute(
					insert_sql,
					turso::params![
						batch.batch_id().as_uuid().to_string(),
						batch.metadata.aspect.as_uuid().to_string(),
						batch.metadata.database_info.to_json_str()?,
						batch.metadata.size.to_string(),
						batch.metadata.resolution.to_string(),
						batch.batch_hash().unwrap_or(&String::new()).clone(),
						current_timestamp.to_string(),
						Option::<String>::None, // processed_at is None for unprocessed batches
						current_timestamp.to_string(),
						serde_json::to_string(&batch.metadata)?,
						serde_json::to_string(&batch.measurements)?
					],
				)
				.await
				.map_err(|e| Error::DatabaseError(format!("Failed to insert batch: {e}")))?;

			// Only add TxId if the insert was successful (rows_affected > 0)
			if rows_affected > 0 {
				successful_tx_ids.push(tx_id);
			}
		}

		Ok(successful_tx_ids)
	}

	async fn insert_processed_batch(&self, aspect_id: AspectId, batch: &Batch) -> Result<TxId> {
		let mut aspect = self.get_aspect(aspect_id).await?;
		let processed_batches_db = aspect.processed_batches().await?;
		let conn = processed_batches_db.connect()?;
		let tx_id = TxId::new();

		// Use INSERT ... ON CONFLICT DO NOTHING for concurrent writes support
		// This automatically skips the insert if a batch with the same ID already exists
		let insert_sql = r"
                        INSERT INTO batches (
                                id, aspect_id, database_id, size, resolution, 
                                batch_hash, created_at, processed_at, updated_at,
                                metadata_json, measurements_json
                        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                        ON CONFLICT(id) DO NOTHING
                ";

		let current_timestamp = chrono::Utc::now().timestamp_millis();

		let rows_affected = conn
			.execute(
				insert_sql,
				turso::params![
					batch.batch_id().as_uuid().to_string(),
					batch.metadata.aspect.as_uuid().to_string(),
					batch.metadata.database_info.to_json_str()?,
					batch.metadata.size.to_string(),
					batch.metadata.resolution.to_string(),
					batch.batch_hash().unwrap_or(&String::new()).clone(),
					current_timestamp.to_string(),
					current_timestamp.to_string(), // processed_at
					current_timestamp.to_string(),
					serde_json::to_string(&batch.metadata)?,
					serde_json::to_string(&batch.measurements)?
				],
			)
			.await?;

		if rows_affected > 0 {
			self.record_transaction(&format!("Inserted processed batch {} for aspect {}", batch.batch_id().as_uuid().to_string(), batch.metadata.aspect.as_uuid().to_string())).await
		} else {
			// Batch already exists, log that it was skipped
			self.record_transaction(&format!("Skipped duplicate processed batch {} for aspect {} (already exists)", batch.batch_id().as_uuid().to_string(), batch.metadata.aspect.as_uuid().to_string())).await?;
			Ok(tx_id)
		}
	}

	async fn batch_insert_processed_batches(&self, aspect_id: AspectId, batches: Vec<Batch>) -> Result<Vec<TxId>> {
		if batches.is_empty() {
			return Ok(Vec::new());
		}
		let mut aspect = self.get_aspect(aspect_id).await?;
		let processed_batches_db = aspect.processed_batches().await?;
		let conn = processed_batches_db.connect()?;

		// Print initial progress message for large batches
		if batches.len() > 1000 {
			println!("Loading {} processed batches...", batches.len());
		}

		// Use concurrent writes - no need for transactions or complex retry logic
		// Process in chunks for better performance
		let chunk_size = if batches.len() > 10000 { 500 } else { 100 };
		let chunks: Vec<_> = batches.chunks(chunk_size).enumerate().collect();

		let mut successful_tx_ids = Vec::new();

		for (chunk_idx, chunk) in chunks {
			// Progress reporting for large batches
			if batches.len() > 1000 && chunk_idx % 10 == 0 && chunk_idx > 0 {
				if let (Ok(processed_f64), Ok(len_f64)) = (safe_usize_to_f64(chunk_idx * chunk_size), safe_usize_to_f64(batches.len())) {
					let pct = (processed_f64 / len_f64) * 100.0;
					println!("  Processed Batches - {:.0}%", pct);
				} else {
					println!("  Processed Batches - processed {} / {}", chunk_idx * chunk_size, batches.len());
				}
			}

			// Process chunk using concurrent writes and collect successful TxIds
			// Reuse the same insert_batch_chunk helper but with processed batches database connection
			let chunk_successful_tx_ids = self.insert_batch_chunk(&conn, chunk).await?;
			successful_tx_ids.extend(chunk_successful_tx_ids);
		}

		// Print summary for large batches
		if batches.len() > 1000 {
			let skipped = batches.len() - successful_tx_ids.len();
			println!("  Processed Batches - Completed: {} inserted, {} skipped (duplicates)", successful_tx_ids.len(), skipped);
		}

		Ok(successful_tx_ids)
	}
}
