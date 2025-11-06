use std::str::FromStr;

use anyhow::Result;

use super::helpers::safe_ratio;
use crate::{
	types::database::traits::{aspect_structure::AspectStructure, database_structure::DatabaseStructure}, AspectId, Batch, BatchId, BatchedMeasurement, Error, DATABASES
};

const BATCH_CHUNK_SIZE: usize = 100;

impl super::Database {
	/// Get or create a batches database and ensure the table structure exists
	///
	/// # Errors
	/// - if unable to create database
	/// - if unable to create table structure
	async fn get_or_create_batches_database(batches_db_path: &str) -> Result<turso::Database> {
		let batches_turso_db = match Self::get_turso_database(batches_db_path).await {
			Ok(db) => db,
			Err(_) => Self::create_turso_database(batches_db_path).await?,
		};

		// Ensure the batches table exists
		let conn = batches_turso_db.connect()?;
		conn.execute(
			"CREATE TABLE IF NOT EXISTS batches (
				id TEXT PRIMARY KEY,
				aspect_id TEXT NOT NULL,
				database_id TEXT NOT NULL,
				size INTEGER NOT NULL,
				resolution TEXT NOT NULL,
				measurements TEXT NOT NULL,
				batch_hash TEXT NOT NULL,
				status TEXT NOT NULL DEFAULT 'unprocessed',
				created_at INTEGER NOT NULL,
				processed_at INTEGER
			)",
			turso::params![],
		)
		.await
		.map_err(|e| Error::DatabaseError(format!("Failed to create batches table: {e}")))?;

		// Create index for efficient querying
		conn.execute("CREATE INDEX IF NOT EXISTS idx_batches_aspect ON batches(aspect_id, status)", turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to create batches index: {e}")))?;

		Ok(batches_turso_db)
	}

	/// Store a batch in the database
	///
	/// # Errors
	/// - if database not found
	/// - if unable to insert batch
	pub async fn store_batch(&self, batch: &Batch) -> Result<()> {
		// Get database info and subject info
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| Error::DatabaseError("Database not found".to_string()))?
		};

		// Find the subject and aspect
		let (subject_name, aspect_name) = db_info.subjects.values().find_map(|subject| subject.aspects().get(&batch.metadata.aspect).map(|aspect| (subject.name().to_string(), aspect.name()))).ok_or_else(|| anyhow::anyhow!("Aspect not found"))?;

		// Create batches.db path within aspect folder
		let batches_db_path = format!("{}/{subject_name}/{aspect_name}/batches.db", db_info.path());

		// Get or create the batches database
		let batches_turso_db = Self::get_or_create_batches_database(&batches_db_path).await?;

		// Serialize the batch measurements with proper error handling
		let measurements_json = serde_json::to_string(&batch.measurements).map_err(|e| Error::DatabaseError(format!("Failed to serialize batch measurements: {e}")))?;

		// Generate a hash from the original measurements for identification
		let batch_hash = format!("{:x}", md5::compute(&measurements_json));
		let batch_id = uuid::Uuid::new_v4().to_string();
		let conn = batches_turso_db.connect()?;

		// Insert batch with transaction safety
		conn.execute("INSERT INTO batches (id, aspect_id, database_id, size, resolution, measurements, batch_hash, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)", turso::params![batch_id, batch.metadata.aspect.as_uuid().to_string(), self.id().as_uuid().to_string(), i64::try_from(batch.metadata.size).map_err(|_| Error::DatabaseError("Batch size too large".to_string()))?, format!("{:?}", batch.metadata.resolution), measurements_json, batch_hash, chrono::Utc::now().timestamp_millis()]).await.map_err(|e| Error::DatabaseError(format!("Failed to insert batch: {e}")))?;

		// Invalidate relevant caches
		let cache_key = format!("aspect_batches_{}", batch.metadata.aspect.as_uuid());
		self.cache.lock().await.invalidate(&cache_key).await;

		Ok(())
	}

	/// Get unprocessed batches for an aspect
	///
	/// # Errors
	/// - if database not found
	/// - if unable to query batches
	pub async fn get_unprocessed_batches(&self, aspect_id: &AspectId) -> Result<Vec<Batch>> {
		// Get database info and subject info
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| Error::DatabaseError("Database not found".to_string()))?
		};

		// Find the subject and aspect
		let (subject_name, aspect_name) = db_info.subjects.values().find_map(|subject| subject.aspects().get(aspect_id).map(|aspect| (subject.name().to_string(), aspect.name().to_string()))).ok_or_else(|| Error::DatabaseError("Subject or aspect not found".to_string()))?;

		// Create batches.db path within aspect folder
		let batches_db_path = format!("{}/{subject_name}/{aspect_name}/batches.db", db_info.path());

		// Get or create the batches database
		let batches_turso_db = Self::get_or_create_batches_database(&batches_db_path).await?;

		let conn = batches_turso_db.connect()?;
		let mut rows = conn.query("SELECT id, size, resolution, measurements, batch_hash FROM batches WHERE aspect_id = ? AND database_id = ? AND status = 'unprocessed' ORDER BY created_at", turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query batches: {e}")))?;

		let mut batches = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get row: {e}")))? {
			let batch_id_str = Self::value_to_string(&row.get_value(0)?, "Batch ID").await?;
			let size_str = Self::value_to_string(&row.get_value(1)?, "Size").await?;
			let resolution_str = Self::value_to_string(&row.get_value(2)?, "Resolution").await?;
			let measurements_json = Self::value_to_string(&row.get_value(3)?, "Measurements").await?;
			let batch_hash = Self::value_to_string(&row.get_value(4)?, "Batch Hash").await?;

			let size: usize = size_str.parse().map_err(|e| Error::DatabaseError(format!("Failed to parse batch size: {e}")))?;
			let measurements: Vec<BatchedMeasurement> = serde_json::from_str(&measurements_json).map_err(|e| Error::DatabaseError(format!("Failed to deserialize measurements: {e}")))?;

			// Parse resolution with proper error handling
			let resolution = Self::parse_resolution(&resolution_str)?;

			let mut batch = Batch::new(size, measurements, resolution, *aspect_id, db_info.clone());
			// Store the batch hash for identification
			batch.set_batch_hash(Some(batch_hash));
			batch.set_batch_id(BatchId::from_str(&batch_id_str)?);
			batches.push(batch);
		}

		Ok(batches)
	}

	/// Marks a batch as processed by moving it from the unprocessed to processed database.
	///
	/// # Errors
	///
	/// Returns an error if the database operations fail or the batch cannot be moved.
	pub async fn mark_batch_processed(&self, batch: &Batch) -> Result<()> {
		// Get aspect information for database path building
		let aspect = batch.metadata.aspect;
		let mut aspect_data = self.get_aspect(aspect).await?;

		// Move batch from unprocessed to processed database
		// 1. Insert into processed_batches database
		let processed_batches_db = aspect_data.processed_batches().await?;
		let processed_conn = processed_batches_db.connect()?;

		let batch_id = batch.batch_id();
		let metadata_json = serde_json::to_string(&batch.metadata).map_err(|e| Error::DatabaseError(format!("Failed to serialize batch metadata: {e}")))?;
		let measurements_json = serde_json::to_string(&batch.measurements).map_err(|e| Error::DatabaseError(format!("Failed to serialize batch measurements: {e}")))?;

		// Insert into processed batches database using concurrent writes
		let insert_sql = r"
        INSERT INTO batches (id, aspect_id, database_id, size, resolution, batch_hash, created_at, metadata_json, measurements_json)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            updated_at = strftime('%s', 'now') * 1000,
            metadata_json = excluded.metadata_json,
            measurements_json = excluded.measurements_json
    ";

		processed_conn.execute(insert_sql, turso::params![batch_id.as_uuid().to_string(), batch.metadata.aspect.as_uuid().to_string(), self.id().as_uuid().to_string(), i64::try_from(batch.measurements.len()).unwrap_or(i64::MAX), serde_json::to_string(&batch.metadata.resolution).unwrap_or_default(), batch.batch_hash().cloned(), chrono::Utc::now().timestamp_millis(), metadata_json, measurements_json]).await.map_err(|e| Error::DatabaseError(format!("Failed to insert batch into processed database: {e}")))?;

		// 2. Remove from unprocessed_batches database
		let unprocessed_batches_db = aspect_data.unprocessed_batches().await?;
		let unprocessed_conn = unprocessed_batches_db.connect()?;

		let rows_affected = unprocessed_conn.execute("DELETE FROM batches WHERE id = ?", turso::params![batch_id.as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to remove batch from unprocessed database: {e}")))?;

		// Verify that a batch was actually moved
		if rows_affected == 0 {
			// Try fallback with batch hash if ID deletion failed
			if let Some(batch_hash) = batch.batch_hash() {
				let rows_affected = unprocessed_conn.execute("DELETE FROM batches WHERE batch_hash = ? AND aspect_id = ? AND database_id = ?", turso::params![batch_hash.clone(), batch.metadata.aspect.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to remove batch by hash from unprocessed database: {e}")))?;

				if rows_affected == 0 {
					return Err(anyhow::Error::msg("No matching unprocessed batch found to mark as processed"));
				}
			} else {
				return Err(anyhow::Error::msg("No matching unprocessed batch found to mark as processed"));
			}
		}

		// Invalidate relevant caches
		let cache_key = format!("aspect_batches_{}", batch.metadata.aspect.as_uuid());
		self.cache.lock().await.invalidate(&cache_key).await;

		Ok(())
	}

	/// Mark a batch as processed using batch ID
	///
	/// # Errors
	/// - if database not found
	/// - if unable to update batch status
	///
	/// Note: This function requires looking up the aspect from the batch ID,
	/// which means it needs to query potentially multiple batches databases.
	/// Consider using `mark_batch_processed()` instead if you have the batch object.
	pub async fn mark_batch_processed_by_id(&self, batch_id: &str, aspect_id: &AspectId) -> Result<()> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| Error::DatabaseError("Database not found".to_string()))?
		};

		// Find the subject and aspect
		let (subject_name, aspect_name) = db_info.subjects.values().find_map(|subject| subject.aspects().get(aspect_id).map(|aspect| (subject.name().to_string(), aspect.name().to_string()))).ok_or_else(|| Error::DatabaseError("Subject or aspect not found".to_string()))?;

		// Get batches database
		let batches_db_path = format!("{}/{subject_name}/{aspect_name}/batches.db", db_info.path());
		let batches_turso_db = Self::get_or_create_batches_database(&batches_db_path).await?;

		let conn = batches_turso_db.connect()?;
		let rows_affected = conn.execute("UPDATE batches SET status = 'processed', processed_at = ? WHERE id = ? AND status = 'unprocessed'", turso::params![chrono::Utc::now().timestamp_millis(), batch_id]).await.map_err(|e| Error::DatabaseError(format!("Failed to update batch status by ID: {e}")))?;

		// Verify that a batch was actually updated
		if rows_affected == 0 {
			return Err(anyhow::Error::msg(format!("No matching unprocessed batch found with ID: {batch_id}")));
		}

		Ok(())
	}

	/// Mark a batch as processed using batch hash
	///
	/// # Errors
	/// - if database not found
	/// - if unable to update batch status
	pub async fn mark_batch_processed_by_hash(&self, batch_hash: &str, aspect_id: &AspectId) -> Result<()> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| Error::DatabaseError("Database not found".to_string()))?
		};

		// Find the subject and aspect
		let (subject_name, aspect_name) = db_info.subjects.values().find_map(|subject| subject.aspects().get(aspect_id).map(|aspect| (subject.name().to_string(), aspect.name().to_string()))).ok_or_else(|| Error::DatabaseError("Subject or aspect not found".to_string()))?;

		// Get batches database
		let batches_db_path = format!("{}/{subject_name}/{aspect_name}/batches.db", db_info.path());
		let batches_turso_db = Self::get_or_create_batches_database(&batches_db_path).await?;

		let conn = batches_turso_db.connect()?;
		let rows_affected = conn.execute("UPDATE batches SET status = 'processed', processed_at = ? WHERE batch_hash = ? AND aspect_id = ? AND database_id = ? AND status = 'unprocessed'", turso::params![chrono::Utc::now().timestamp_millis(), batch_hash, aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to update batch status by hash: {e}")))?;

		// Verify that a batch was actually updated
		if rows_affected == 0 {
			return Err(anyhow::Error::msg(format!("No matching unprocessed batch found with hash: {batch_hash}")));
		}

		Ok(())
	}

	/// Store multiple batches efficiently using batch operations
	///
	/// # Errors
	/// - if database not found
	/// - if unable to serialize or insert batches
	pub async fn store_batches(&self, batches: &[Batch]) -> Result<()> {
		if batches.is_empty() {
			return Ok(());
		}

		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| Error::DatabaseError("Database not found".to_string()))?
		};

		// Group batches by aspect to process them in their respective databases
		let mut batches_by_aspect: std::collections::HashMap<AspectId, Vec<&Batch>> = std::collections::HashMap::new();
		for batch in batches {
			batches_by_aspect.entry(batch.metadata.aspect).or_default().push(batch);
		}

		let total_batches = batches.len();
		let mut processed = 0;

		// Process each aspect's batches in its own database
		for (aspect_id, aspect_batches) in batches_by_aspect {
			// Find the subject and aspect
			let (subject_name, aspect_name) = db_info.subjects.values().find_map(|subject| subject.aspects().get(&aspect_id).map(|aspect| (subject.name().to_string(), aspect.name().to_string()))).ok_or_else(|| Error::DatabaseError("Subject or aspect not found".to_string()))?;

			// Get batches database for this aspect
			let batches_db_path = format!("{}/{subject_name}/{aspect_name}/batches.db", db_info.path());
			let batches_turso_db = Self::get_or_create_batches_database(&batches_db_path).await?;

			// Process in chunks, with each chunk in its own transaction
			for (chunk_idx, chunk) in aspect_batches.chunks(BATCH_CHUNK_SIZE).enumerate() {
				let mut conn = batches_turso_db.connect()?;
				let tx = conn.transaction().await.map_err(|e| Error::DatabaseError(format!("Failed to begin transaction: {e}")))?;

				let result = async {
					for batch in chunk {
						// Serialize the batch measurements
						let measurements_json = serde_json::to_string(&batch.measurements).map_err(|e| Error::DatabaseError(format!("Failed to serialize batch measurements: {e}")))?;
						let batch_id = uuid::Uuid::new_v4().to_string();

						// Generate a hash from the original measurements for identification
						let batch_hash = format!("{:x}", md5::compute(&measurements_json));

						tx.execute("INSERT INTO batches (id, aspect_id, database_id, size, resolution, measurements, batch_hash, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)", turso::params![batch_id, batch.metadata.aspect.as_uuid().to_string(), self.id().as_uuid().to_string(), i64::try_from(batch.metadata.size).map_err(|_| Error::DatabaseError("Batch size too large".to_string()))?, format!("{:?}", batch.metadata.resolution), measurements_json, batch_hash, chrono::Utc::now().timestamp_millis()]).await.map_err(|e| Error::DatabaseError(format!("Failed to insert batch in transaction: {e}")))?;
					}
					Ok::<(), anyhow::Error>(())
				}
				.await;

				// Commit or rollback transaction based on result
				match result {
					Ok(()) => {
						tx.commit().await.map_err(|e| Error::DatabaseError(format!("Failed to commit batch transaction: {e}")))?;
						processed += chunk.len();

						// Progress reporting for large batch storage operations
						if total_batches > 1000 && chunk_idx % 10 == 0 && chunk_idx > 0 {
							println!("Stored {processed} / {total_batches} batches");
						}
					}
					Err(e) => {
						let _ = tx.rollback().await;
						return Err(e);
					}
				}
			}
		}

		// Invalidate caches for all affected aspects
		let mut invalidated_aspects = std::collections::HashSet::new();
		for batch in batches {
			if invalidated_aspects.insert(batch.metadata.aspect) {
				let cache_key = format!("aspect_batches_{}", batch.metadata.aspect.as_uuid());
				self.cache.lock().await.invalidate(&cache_key).await;
			}
		}

		Ok(())
	}

	/// Helper method to parse resolution strings
	fn parse_resolution(resolution_str: &str) -> Result<splimes::Resolution> {
		match resolution_str {
			"Nanoseconds" => Ok(splimes::Resolution::Nanoseconds),
			"Microseconds" => Ok(splimes::Resolution::Microseconds),
			"Milliseconds" => Ok(splimes::Resolution::Milliseconds),
			"Seconds" => Ok(splimes::Resolution::Seconds),
			"Minutes" => Ok(splimes::Resolution::Minutes),
			"Hours" => Ok(splimes::Resolution::Hours),
			"Days" => Ok(splimes::Resolution::Days),
			"Weeks" => Ok(splimes::Resolution::Weeks),
			"Months" => Ok(splimes::Resolution::Months),
			"Years" => Ok(splimes::Resolution::Years),
			_ => Err(anyhow::Error::msg(format!("Invalid resolution value: {resolution_str}"))),
		}
	}

	/// Get batch count statistics for an aspect
	///
	/// # Errors
	/// - if database not found
	/// - if unable to query batch statistics
	pub async fn get_batch_stats(&self, aspect_id: &AspectId) -> Result<BatchStats> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| Error::DatabaseError("Database not found".to_string()))?
		};

		// Find the subject and aspect
		let (subject_name, aspect_name) = db_info.subjects.values().find_map(|subject| subject.aspects().get(aspect_id).map(|aspect| (subject.name().to_string(), aspect.name().to_string()))).ok_or_else(|| Error::DatabaseError("Subject or aspect not found".to_string()))?;

		// Create batches.db path within aspect folder
		let batches_db_path = format!("{}/{subject_name}/{aspect_name}/batches.db", db_info.path());
		let batches_turso_db = Self::get_or_create_batches_database(&batches_db_path).await?;

		let conn = batches_turso_db.connect()?;
		let mut rows = conn.query("SELECT status, COUNT(*) as count FROM batches WHERE aspect_id = ? AND database_id = ? GROUP BY status", turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query batch stats: {e}")))?;

		let mut stats = BatchStats::default();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get row: {e}")))? {
			let status = Self::value_to_string(&row.get_value(0)?, "Status").await?;
			let count_str = Self::value_to_string(&row.get_value(1)?, "Count").await?;
			let count: usize = count_str.parse().map_err(|e| Error::DatabaseError(format!("Failed to parse count: {e}")))?;

			match status.as_str() {
				"unprocessed" => stats.unprocessed_count = count,
				"processed" => stats.processed_count = count,
				_ => {} // Ignore unknown statuses
			}
		}

		Ok(stats)
	}

	/// Remove/cleanup processed batches older than specified days
	///
	/// # Errors
	/// - if database not found
	/// - if unable to delete processed batches
	pub async fn cleanup_processed_batches(&self, aspect_id: &AspectId, older_than_days: i64) -> Result<usize> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| Error::DatabaseError("Database not found".to_string()))?
		};

		// Find the subject and aspect
		let (subject_name, aspect_name) = db_info.subjects.values().find_map(|subject| subject.aspects().get(aspect_id).map(|aspect| (subject.name().to_string(), aspect.name().to_string()))).ok_or_else(|| Error::DatabaseError("Subject or aspect not found".to_string()))?;

		// Create batches.db path within aspect folder
		let batches_db_path = format!("{}/{subject_name}/{aspect_name}/batches.db", db_info.path());
		let batches_turso_db = Self::get_or_create_batches_database(&batches_db_path).await?;

		let cutoff_time = chrono::Utc::now() - chrono::Duration::days(older_than_days);
		let conn = batches_turso_db.connect()?;
		let rows_affected = conn.execute("DELETE FROM batches WHERE aspect_id = ? AND database_id = ? AND status = 'processed' AND processed_at < ?", turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string(), cutoff_time.timestamp_millis()]).await.map_err(|e| Error::DatabaseError(format!("Failed to cleanup processed batches: {e}")))?;

		// Invalidate relevant caches
		let cache_key = format!("aspect_batches_{}", aspect_id.as_uuid());
		self.cache.lock().await.invalidate(&cache_key).await;

		Ok(usize::try_from(rows_affected).map_err(|_| Error::DatabaseError("Too many rows affected".to_string()))?)
	}

	/// Remove all processed batches for an aspect (immediate cleanup)
	///
	/// # Errors
	/// - if database not found
	/// - if unable to delete processed batches
	pub async fn cleanup_all_processed_batches(&self, aspect_id: &AspectId) -> Result<usize> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| Error::DatabaseError("Database not found".to_string()))?
		};

		// Find the subject and aspect
		let (subject_name, aspect_name) = db_info.subjects.values().find_map(|subject| subject.aspects().get(aspect_id).map(|aspect| (subject.name().to_string(), aspect.name().to_string()))).ok_or_else(|| Error::DatabaseError("Subject or aspect not found".to_string()))?;

		// Create batches.db path within aspect folder
		let batches_db_path = format!("{}/{subject_name}/{aspect_name}/batches.db", db_info.path());
		let batches_turso_db = Self::get_or_create_batches_database(&batches_db_path).await?;

		let conn = batches_turso_db.connect()?;
		let rows_affected = conn.execute("DELETE FROM batches WHERE aspect_id = ? AND database_id = ? AND status = 'processed'", turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to cleanup all processed batches: {e}")))?;

		// Invalidate relevant caches
		let cache_key = format!("aspect_batches_{}", aspect_id.as_uuid());
		self.cache.lock().await.invalidate(&cache_key).await;

		Ok(usize::try_from(rows_affected).map_err(|_| Error::DatabaseError("Too many rows affected".to_string()))?)
	}

	/// Get processed batches for an aspect (for cleanup verification)
	///
	/// # Errors
	/// - if database not found
	/// - if unable to query processed batches
	pub async fn get_processed_batches(&self, aspect_id: &AspectId) -> Result<Vec<Batch>> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| Error::DatabaseError("Database not found".to_string()))?
		};

		// Find the subject and aspect
		let (subject_name, aspect_name) = db_info.subjects.values().find_map(|subject| subject.aspects().get(aspect_id).map(|aspect| (subject.name().to_string(), aspect.name().to_string()))).ok_or_else(|| Error::DatabaseError("Subject or aspect not found".to_string()))?;

		// Create batches.db path within aspect folder
		let batches_db_path = format!("{}/{subject_name}/{aspect_name}/batches.db", db_info.path());
		let batches_turso_db = Self::get_or_create_batches_database(&batches_db_path).await?;

		let conn = batches_turso_db.connect()?;
		let mut rows = conn.query("SELECT id, size, resolution, measurements, batch_hash FROM batches WHERE aspect_id = ? AND database_id = ? AND status = 'processed' ORDER BY processed_at", turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query processed batches: {e}")))?;

		let mut batches = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get row: {e}")))? {
			let batch_id_str = Self::value_to_string(&row.get_value(0)?, "Batch ID").await?;
			let size_str = Self::value_to_string(&row.get_value(1)?, "Size").await?;
			let resolution_str = Self::value_to_string(&row.get_value(2)?, "Resolution").await?;
			let measurements_json = Self::value_to_string(&row.get_value(3)?, "Measurements").await?;
			let batch_hash = Self::value_to_string(&row.get_value(4)?, "Batch Hash").await?;

			let size: usize = size_str.parse().map_err(|e| Error::DatabaseError(format!("Failed to parse batch size: {e}")))?;
			let measurements: Vec<BatchedMeasurement> = serde_json::from_str(&measurements_json).map_err(|e| Error::DatabaseError(format!("Failed to deserialize measurements: {e}")))?;

			// Parse resolution with proper error handling
			let resolution = Self::parse_resolution(&resolution_str)?;

			let mut batch = Batch::new(size, measurements, resolution, *aspect_id, db_info.clone());
			// Store the batch hash and ID for identification
			batch.set_batch_hash(Some(batch_hash));
			batch.set_batch_id(BatchId::from_str(&batch_id_str)?);
			batches.push(batch);
		}

		Ok(batches)
	}

	/// Get processed batches in queue order (oldest first)
	/// This represents batches that are ready to be processed into patterns
	///
	/// # Errors
	/// - if database not found
	/// - if unable to query processed batches
	pub async fn get_processed_batches_queue(&self, aspect_id: &AspectId) -> Result<Vec<Batch>> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| Error::DatabaseError("Database not found".to_string()))?
		};

		// Find the subject and aspect
		let (subject_name, aspect_name) = db_info.subjects.values().find_map(|subject| subject.aspects().get(aspect_id).map(|aspect| (subject.name().to_string(), aspect.name().to_string()))).ok_or_else(|| Error::DatabaseError("Subject or aspect not found".to_string()))?;

		// Create batches.db path within aspect folder
		let batches_db_path = format!("{}/{subject_name}/{aspect_name}/batches.db", db_info.path());
		let batches_turso_db = Self::get_or_create_batches_database(&batches_db_path).await?;

		let conn = batches_turso_db.connect()?;
		let mut rows = conn.query("SELECT id, size, resolution, measurements, batch_hash FROM batches WHERE aspect_id = ? AND database_id = ? AND status = 'processed' ORDER BY processed_at ASC", turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query processed batches queue: {e}")))?;

		let mut batches = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get row: {e}")))? {
			let batch_id_str = Self::value_to_string(&row.get_value(0)?, "Batch ID").await?;
			let size_str = Self::value_to_string(&row.get_value(1)?, "Size").await?;
			let resolution_str = Self::value_to_string(&row.get_value(2)?, "Resolution").await?;
			let measurements_json = Self::value_to_string(&row.get_value(3)?, "Measurements").await?;
			let batch_hash = Self::value_to_string(&row.get_value(4)?, "Batch Hash").await?;

			let size: usize = size_str.parse().map_err(|e| Error::DatabaseError(format!("Failed to parse batch size: {e}")))?;
			let measurements: Vec<BatchedMeasurement> = serde_json::from_str(&measurements_json).map_err(|e| Error::DatabaseError(format!("Failed to deserialize measurements: {e}")))?;

			// Parse resolution with proper error handling
			let resolution = Self::parse_resolution(&resolution_str)?;

			let mut batch = Batch::new(size, measurements, resolution, *aspect_id, db_info.clone());
			// Store the batch hash and ID for identification
			batch.set_batch_hash(Some(batch_hash));
			batch.set_batch_id(BatchId::from_str(&batch_id_str)?);
			batches.push(batch);
		}

		Ok(batches)
	}

	/// Removes a processed batch from the processed batches database.
	///
	/// # Errors
	///
	/// Returns an error if the database operations fail or the batch cannot be removed.
	pub async fn dequeue_processed_batch(&self, batch: &Batch) -> Result<()> {
		// Get aspect information for database access
		let aspect = batch.metadata.aspect;
		let mut aspect_data = self.get_aspect(aspect).await?;

		// Remove from processed_batches database
		let processed_batches_db = aspect_data.processed_batches().await?;
		let processed_conn = processed_batches_db.connect()?;

		let batch_id = batch.batch_id();

		// Delete from processed batches database
		let rows_affected = processed_conn.execute("DELETE FROM batches WHERE id = ?", turso::params![batch_id.as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to dequeue processed batch by ID: {e}")))?;

		// If no rows affected with batch ID, try with batch hash as fallback
		if rows_affected == 0 {
			if let Some(batch_hash) = batch.batch_hash() {
				let rows_affected = processed_conn.execute("DELETE FROM batches WHERE batch_hash = ? AND aspect_id = ? AND database_id = ?", turso::params![batch_hash.clone(), batch.metadata.aspect.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to dequeue processed batch by hash: {e}")))?;

				if rows_affected == 0 {
					return Err(anyhow::Error::msg("No matching processed batch found to dequeue"));
				}
			} else {
				return Err(anyhow::Error::msg("No matching processed batch found to dequeue"));
			}
		}

		// Invalidate relevant caches
		let cache_key = format!("aspect_batches_{}", batch.metadata.aspect.as_uuid());
		self.cache.lock().await.invalidate(&cache_key).await;

		Ok(())
	}

	/// Clear all processed batches from the queue for an aspect
	/// This is used for cleanup after all patterns have been generated
	///
	/// # Errors
	/// - if database not found
	/// - if unable to delete processed batches
	pub async fn clear_processed_batches_queue(&self, aspect_id: &AspectId) -> Result<usize> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| Error::DatabaseError("Database not found".to_string()))?
		};

		// Find the subject and aspect
		let (subject_name, aspect_name) = db_info.subjects.values().find_map(|subject| subject.aspects().get(aspect_id).map(|aspect| (subject.name().to_string(), aspect.name().to_string()))).ok_or_else(|| Error::DatabaseError("Subject or aspect not found".to_string()))?;

		// Create batches.db path within aspect folder
		let batches_db_path = format!("{}/{subject_name}/{aspect_name}/batches.db", db_info.path());
		let batches_turso_db = Self::get_or_create_batches_database(&batches_db_path).await?;

		let conn = batches_turso_db.connect()?;
		let rows_affected = conn.execute("DELETE FROM batches WHERE aspect_id = ? AND database_id = ? AND status = 'processed'", turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to clear processed batches queue: {e}")))?;

		// Invalidate relevant caches
		let cache_key = format!("aspect_batches_{}", aspect_id.as_uuid());
		self.cache.lock().await.invalidate(&cache_key).await;

		Ok(usize::try_from(rows_affected).map_err(|_| Error::DatabaseError("Too many rows affected".to_string()))?)
	}
}

#[derive(Debug, Default, Clone)]
pub struct BatchStats {
	pub unprocessed_count: usize,
	pub processed_count: usize,
}

impl BatchStats {
	#[must_use]
	pub const fn total_count(&self) -> usize {
		self.unprocessed_count + self.processed_count
	}

	/// Check if there are batches that need cleanup
	#[must_use]
	pub const fn needs_cleanup(&self) -> bool {
		self.processed_count > 0
	}

	/// Get the ratio of processed to total batches
	/// # Errors
	/// - if counts exceed f64 precision limits during conversion
	pub fn processed_ratio(&self) -> std::result::Result<f64, Error> {
		safe_ratio(self.processed_count, self.total_count())
	}
}
