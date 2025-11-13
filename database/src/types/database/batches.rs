use std::str::FromStr;

use anyhow::Result;
use splimes::Resolution;

use super::{helpers::safe_ratio, Database};
use crate::{
	types::database::traits::{aspect_structure::AspectStructure, connection::Connection, database_structure::DatabaseStructure}, AspectId, Batch, BatchId, BatchedMeasurement, Error, DATABASES
};

impl Database {
	/// Get batch count statistics for an aspect
	///
	/// # Errors
	/// - if database not found
	/// - if unable to query batch statistics
	pub async fn get_unprocessed_batch_stats(&self, aspect_id: &AspectId) -> Result<BatchStats> {
		let mut aspect = self.get_aspect(aspect_id).await?;
		let db_path = aspect.unprocessed_batches_path();
		let db = aspect.unprocessed_batches().await?;

		let conn = Self::begin_concurrent(&db, db_path.as_str(), Some(self.cache.clone())).await?;
		let mut rows = conn.as_ref().query("SELECT status, COUNT(*) as count FROM batches WHERE aspect_id = ? AND database_id = ? GROUP BY status", turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query batch stats: {e}")))?;

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

		let _ = Self::commit_concurrent(&conn).await;

		Ok(stats)
	}

	/// Remove/cleanup processed batches older than specified days
	///
	/// # Errors
	/// - if database not found
	/// - if unable to delete processed batches
	pub async fn cleanup_processed_batches(&self, aspect_id: &AspectId, older_than_days: i64) -> Result<usize> {
		let mut aspect = self.get_aspect(aspect_id).await?;
		let batches_db_path = aspect.processed_batches_path();
		let db = aspect.processed_batches().await?;

		let cutoff_time = chrono::Utc::now() - chrono::Duration::days(older_than_days);
		let conn = Self::begin_concurrent(&db, batches_db_path.as_str(), Some(self.cache.clone())).await?;
		let rows_affected = conn.as_ref().execute("DELETE FROM batches WHERE aspect_id = ? AND database_id = ? AND status = 'processed' AND processed_at < ?", turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string(), cutoff_time.timestamp_millis()]).await.map_err(|e| Error::DatabaseError(format!("Failed to cleanup processed batches: {e}")))?;
		let _ = Self::commit_concurrent(&conn).await;

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
		let mut aspect = self.get_aspect(aspect_id).await?;
		let batches_db_path = aspect.processed_batches_path();
		let db = aspect.processed_batches().await?;

		let conn = Self::begin_concurrent(&db, batches_db_path.as_str(), Some(self.cache.clone())).await?;
		let rows_affected = conn.as_ref().execute("DELETE FROM batches WHERE aspect_id = ? AND database_id = ? AND status = 'processed'", turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to cleanup all processed batches: {e}")))?;
		let _ = Self::commit_concurrent(&conn).await;

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

		let mut aspect = self.get_aspect(aspect_id).await?;
		let db_path = aspect.processed_batches_path();
		let db = aspect.processed_batches().await?;

		let conn = Self::begin_concurrent(&db, db_path.as_str(), Some(self.cache.clone())).await?;
		let mut rows = conn.as_ref().query("SELECT id, size, resolution, measurements, batch_hash FROM batches WHERE aspect_id = ? AND database_id = ? AND status = 'processed' ORDER BY processed_at", turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query processed batches: {e}")))?;

		let mut batches = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get row: {e}")))? {
			let batch_id_str = Self::value_to_string(&row.get_value(0)?, "Batch ID").await?;
			let batch_id = BatchId::from_uuid(uuid::Uuid::parse_str(&batch_id_str).map_err(|e| Error::DatabaseError(format!("Failed to parse batch ID UUID: {e}")))?);
			let size_str = Self::value_to_string(&row.get_value(1)?, "Size").await?;
			let resolution_str = Self::value_to_string(&row.get_value(2)?, "Resolution").await?;
			let measurements_json = Self::value_to_string(&row.get_value(3)?, "Measurements").await?;
			let batch_hash = Self::value_to_string(&row.get_value(4)?, "Batch Hash").await?;

			let size: usize = size_str.parse().map_err(|e| Error::DatabaseError(format!("Failed to parse batch size: {e}")))?;
			let measurements: Vec<BatchedMeasurement> = serde_json::from_str(&measurements_json).map_err(|e| Error::DatabaseError(format!("Failed to deserialize measurements: {e}")))?;

			// Parse resolution with proper error handling
			let resolution = Resolution::from_str(&resolution_str)?;

			let mut batch = Batch::new(size, measurements, resolution, *aspect_id, db_info.clone());
			// Store the batch hash and ID for identification
			batch.set_batch_hash(Some(batch_hash));
			batch.set_batch_id(&batch_id);
			batches.push(batch);
		}

		let _ = Self::commit_concurrent(&conn).await;

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

		let mut aspect = self.get_aspect(aspect_id).await?;

		// Create batches.db path within aspect folder
		let db_path = aspect.processed_batches_path();
		let db = aspect.processed_batches().await?;
		let conn = Self::begin_concurrent(&db, db_path.as_str(), Some(self.cache.clone())).await?;

		let mut rows = conn.as_ref().query("SELECT id, size, resolution, measurements, batch_hash FROM batches WHERE aspect_id = ? AND database_id = ? AND status = 'processed' ORDER BY processed_at ASC", turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query processed batches queue: {e}")))?;

		let mut batches = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get row: {e}")))? {
			let batch_id_str = Self::value_to_string(&row.get_value(0)?, "Batch ID").await?;
			let batch_id = BatchId::from_uuid(uuid::Uuid::parse_str(&batch_id_str).map_err(|e| Error::DatabaseError(format!("Failed to parse batch ID UUID: {e}")))?);
			let size_str = Self::value_to_string(&row.get_value(1)?, "Size").await?;
			let resolution_str = Self::value_to_string(&row.get_value(2)?, "Resolution").await?;
			let measurements_json = Self::value_to_string(&row.get_value(3)?, "Measurements").await?;
			let batch_hash = Self::value_to_string(&row.get_value(4)?, "Batch Hash").await?;

			let size: usize = size_str.parse().map_err(|e| Error::DatabaseError(format!("Failed to parse batch size: {e}")))?;
			let measurements: Vec<BatchedMeasurement> = serde_json::from_str(&measurements_json).map_err(|e| Error::DatabaseError(format!("Failed to deserialize measurements: {e}")))?;

			// Parse resolution with proper error handling
			let resolution = Resolution::from_str(&resolution_str)?;

			let mut batch = Batch::new(size, measurements, resolution, *aspect_id, db_info.clone());
			// Store the batch hash and ID for identification
			batch.set_batch_hash(Some(batch_hash));
			batch.set_batch_id(&batch_id);
			batches.push(batch);
		}

		let _ = Self::commit_concurrent(&conn).await;

		Ok(batches)
	}

	/// Removes a processed batch from the processed batches database.
	///
	/// # Errors
	///
	/// Returns an error if the database operations fail or the batch cannot be removed.
	pub async fn dequeue_processed_batch(&self, batch: &Batch) -> Result<()> {
		// Get aspect information for database access
		let aspect = &batch.metadata.aspect;
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
		let mut aspect = self.get_aspect(aspect_id).await?;

		// Create batches.db path within aspect folder
		let db_path = aspect.processed_batches_path();
		let db = aspect.processed_batches().await?;

		let conn = Self::begin_concurrent(&db, db_path.as_str(), Some(self.cache.clone())).await?;
		let rows_affected = conn.as_ref().execute("DELETE FROM batches WHERE aspect_id = ? AND database_id = ? AND status = 'processed'", turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to clear processed batches queue: {e}")))?;
		let _ = Self::commit_concurrent(&conn).await;

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
