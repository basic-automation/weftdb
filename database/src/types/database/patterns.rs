use anyhow::Result;

use super::helpers::safe_ratio;
use crate::{types::database::traits::database_structure::DatabaseStructure, AspectId, Database, Pattern, DATABASES};

const PATTERN_CHUNK_SIZE: usize = 100;

impl Database {
	/// Store a pattern in the database
	///
	/// # Errors
	/// - if database not found
	/// - if unable to insert pattern
	pub async fn store_pattern(&self, pattern: &Pattern) -> Result<()> {
		let metadata_db = &self.metadata;
		let metadata_db_path = self.metadata_path();
		let conn = Self::begin_concurrent(metadata_db, &metadata_db_path).await?;

		// Serialize the pattern occurrences and relatives
		let occurrences_json = serde_json::to_string(&pattern.occurrences()).map_err(|e| anyhow::anyhow!(format!("Failed to serialize pattern occurrences: {e}")))?;
		let relatives_json = serde_json::to_string(&pattern.relatives()).map_err(|e| anyhow::anyhow!(format!("Failed to serialize pattern relatives: {e}")))?;
		let pattern_id = pattern.id().to_string();

		let res = conn.as_ref().execute("INSERT INTO patterns (id, aspect_id, database_id, occurrences, relatives, created_at) VALUES (?, ?, ?, ?, ?, ?)", turso::params![pattern_id.clone(), pattern.occurrences()[0].database_info().id().as_uuid().to_string(), self.id().as_uuid().to_string(), occurrences_json.clone(), relatives_json.clone(), chrono::Utc::now().timestamp_millis()]).await;
		match res {
			Ok(_) => println!("Inserted pattern {}", pattern_id),
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!(format!("Failed to insert pattern: {e}")));
			}
		}

		Self::commit_concurrent(&conn).await?;
		Ok(())
	}

	/// Get unprocessed patterns for an aspect
	///
	/// # Errors
	/// - if database not found
	/// - if unable to query patterns
	pub async fn get_unprocessed_patterns(&self, aspect_id: &AspectId) -> Result<Vec<Pattern>> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| anyhow::anyhow!("Database not found".to_string()))?
		};

		let metadata_turso_db = db_info.metadata.as_ref().unwrap();

		let conn = metadata_turso_db.connect()?;
		let mut rows = conn.query("SELECT id, occurrences, relatives FROM patterns WHERE aspect_id = ? AND database_id = ? AND status = 'unprocessed' ORDER BY created_at", turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| anyhow::anyhow!(format!("Failed to query patterns: {e}")))?;

		let mut patterns = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| anyhow::anyhow!(format!("Failed to get row: {e}")))? {
			let pattern_id_str = Self::value_to_string(&row.get_value(0)?, "Pattern ID").await?;
			let occurrences_json = Self::value_to_string(&row.get_value(1)?, "Occurrences").await?;
			let relatives_json = Self::value_to_string(&row.get_value(2)?, "Relatives").await?;

			let pattern_id = crate::PatternID::from_string(&pattern_id_str)?;
			let occurrences: Vec<crate::Occurrence> = serde_json::from_str(&occurrences_json).map_err(|e| anyhow::anyhow!(format!("Failed to deserialize occurrences: {e}")))?;
			let relatives: Vec<crate::Relative> = serde_json::from_str(&relatives_json).map_err(|e| anyhow::anyhow!(format!("Failed to deserialize relatives: {e}")))?;

			let pattern = Pattern::new(pattern_id, occurrences, relatives);
			patterns.push(pattern);
		}

		Ok(patterns)
	}

	/// Mark a pattern as processed
	///
	/// # Errors
	/// - if database not found
	/// - if unable to update pattern status
	pub async fn mark_pattern_processed(&self, pattern: &Pattern) -> Result<()> {
		let metadata_db = &self.metadata;
		let metadata_db_path = &self.metadata_path;
		let conn = Self::begin_concurrent(metadata_db, metadata_db_path).await?;
		let res = conn.as_ref().execute("UPDATE patterns SET status = 'processed', processed_at = ? WHERE id = ? AND status = 'unprocessed'", turso::params![chrono::Utc::now().timestamp_millis(), pattern.id().to_string()]).await;
		let rows_affected = match res {
			Ok(rows) => {
				println!("Mark pattern processed affected rows: {}", rows);
				rows
			}
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!(format!("Failed to update pattern status: {e}")));
			}
		};
		Self::commit_concurrent(&conn).await?;

		// Verify that a pattern was actually updated
		if rows_affected == 0 {
			return Err(anyhow::Error::msg("No matching unprocessed pattern found to mark as processed"));
		}

		Ok(())
	}

	/// Store multiple patterns efficiently using batch operations
	///
	/// # Errors
	/// - if database not found
	/// - if unable to serialize or insert patterns
	pub async fn store_patterns(&self, patterns: &[Pattern], aspect_id: &AspectId) -> Result<()> {
		if patterns.is_empty() {
			return Ok(());
		}

		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| anyhow::anyhow!("Database not found".to_string()))?
		};

		let metadata_turso_db = db_info.metadata.as_ref().unwrap();

		// Process in chunks, with each chunk in its own transaction
		let total_patterns = patterns.len();
		let mut processed = 0;

		for (chunk_idx, chunk) in patterns.chunks(PATTERN_CHUNK_SIZE).enumerate() {
			let mut conn = metadata_turso_db.connect()?;
			let tx = conn.transaction().await.map_err(|e| anyhow::anyhow!(format!("Failed to begin transaction: {e}")))?;

			let result = async {
				for pattern in chunk {
					// Serialize the pattern occurrences and relatives
					let occurrences_json = serde_json::to_string(&pattern.occurrences()).map_err(|e| anyhow::anyhow!(format!("Failed to serialize pattern occurrences: {e}")))?;
					let relatives_json = serde_json::to_string(&pattern.relatives()).map_err(|e| anyhow::anyhow!(format!("Failed to serialize pattern relatives: {e}")))?;

					let pattern_id = pattern.id().to_string();

					tx.execute("INSERT INTO patterns (id, aspect_id, database_id, occurrences, relatives, status, created_at) VALUES (?, ?, ?, ?, ?, 'processed', ?)", turso::params![pattern_id, aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string(), occurrences_json, relatives_json, chrono::Utc::now().timestamp_millis()]).await.map_err(|e| anyhow::anyhow!(format!("Failed to insert pattern in transaction: {e}")))?;
				}
				Ok::<(), anyhow::Error>(())
			}
			.await;

			// Commit or rollback transaction based on result
			match result {
				Ok(()) => {
					tx.commit().await.map_err(|e| anyhow::anyhow!(format!("Failed to commit pattern transaction: {e}")))?;
					processed += chunk.len();

					// Progress reporting for large pattern storage operations
					if total_patterns > 100 && chunk_idx % 10 == 0 && chunk_idx > 0 {
						println!("Stored {processed} / {total_patterns} patterns");
					}
				}
				Err(e) => {
					let _ = tx.rollback().await;
					return Err(e);
				}
			}
		}
		Ok(())
	}

	/// Get pattern count statistics for an aspect
	///
	/// # Errors
	/// - if database not found
	/// - if unable to query pattern statistics
	pub async fn get_pattern_stats(&self, aspect_id: &AspectId) -> Result<PatternStats> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| anyhow::anyhow!("Database not found".to_string()))?
		};

		let metadata_turso_db = db_info.metadata.as_ref().unwrap();

		let conn = metadata_turso_db.connect()?;
		let mut rows = conn.query("SELECT status, COUNT(*) as count FROM patterns WHERE aspect_id = ? AND database_id = ? GROUP BY status", turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| anyhow::anyhow!(format!("Failed to query pattern stats: {e}")))?;

		let mut stats = PatternStats::default();
		while let Some(row) = rows.next().await.map_err(|e| anyhow::anyhow!(format!("Failed to get row: {e}")))? {
			let status = Self::value_to_string(&row.get_value(0)?, "Status").await?;
			let count_str = Self::value_to_string(&row.get_value(1)?, "Count").await?;
			let count: usize = count_str.parse().map_err(|e| anyhow::anyhow!(format!("Failed to parse count: {e}")))?;

			match status.as_str() {
				"unprocessed" => stats.unprocessed_count = count,
				"processed" => stats.processed_count = count,
				_ => {} // Ignore unknown statuses
			}
		}

		Ok(stats)
	}

	/// Remove/cleanup processed patterns older than specified days
	///
	/// # Errors
	/// - if database not found
	/// - if unable to delete processed patterns
	pub async fn cleanup_processed_patterns(&self, aspect_id: &AspectId, older_than_days: i64) -> Result<usize> {
		let metadata_db = &self.metadata;
		let metadata_db_path = self.metadata_path();
		let cutoff_time = chrono::Utc::now() - chrono::Duration::days(older_than_days);
		let conn = Self::begin_concurrent(metadata_db, &metadata_db_path).await?;

		let res = conn.as_ref().execute("DELETE FROM patterns WHERE aspect_id = ? AND database_id = ? AND status = 'processed' AND processed_at < ?", turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string(), cutoff_time.timestamp_millis()]).await;
		let rows_affected = match res {
			Ok(rows) => {
				println!("Cleanup processed patterns affected rows: {}", rows);
				rows
			}
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!(format!("Failed to cleanup processed patterns: {e}")));
			}
		};

		Self::commit_concurrent(&conn).await?;
		Ok(usize::try_from(rows_affected).map_err(|_| anyhow::anyhow!("Too many rows affected".to_string()))?)
	}

	/// Remove all processed patterns for an aspect (immediate cleanup)
	///
	/// # Errors
	/// - if database not found
	/// - if unable to delete processed patterns
	pub async fn cleanup_all_processed_patterns(&self, aspect_id: &AspectId) -> Result<usize> {
		let metadata_db = &self.metadata;
		let metadata_db_path = &self.metadata_path;
		let conn = Self::begin_concurrent(metadata_db, &metadata_db_path).await?;

		let res = conn.as_ref().execute("DELETE FROM patterns WHERE aspect_id = ? AND database_id = ? AND status = 'processed'", turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await;
		let rows_affected = match res {
			Ok(rows) => {
				println!("Cleanup all processed patterns affected rows: {}", rows);
				rows
			}
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!(format!("Failed to cleanup all processed patterns: {e}")));
			}
		};

		Self::commit_concurrent(&conn).await?;
		Ok(usize::try_from(rows_affected).map_err(|_| anyhow::anyhow!("Too many rows affected".to_string()))?)
	}

	/// Get processed patterns for an aspect (for cleanup verification)
	///
	/// # Errors
	/// - if database not found
	/// - if unable to query processed patterns
	pub async fn get_processed_patterns(&self, aspect_id: &AspectId) -> Result<Vec<Pattern>> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| anyhow::anyhow!("Database not found".to_string()))?
		};

		let metadata_turso_db = db_info.metadata.as_ref().unwrap();

		let conn = metadata_turso_db.connect()?;
		let mut rows = conn.query("SELECT id, occurrences, relatives FROM patterns WHERE aspect_id = ? AND database_id = ? AND status = 'processed' ORDER BY processed_at", turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| anyhow::anyhow!(format!("Failed to query processed patterns: {e}")))?;

		let mut patterns = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| anyhow::anyhow!(format!("Failed to get row: {e}")))? {
			let pattern_id_str = Self::value_to_string(&row.get_value(0)?, "Pattern ID").await?;
			let occurrences_json = Self::value_to_string(&row.get_value(1)?, "Occurrences").await?;
			let relatives_json = Self::value_to_string(&row.get_value(2)?, "Relatives").await?;

			let pattern_id = crate::PatternID::from_string(&pattern_id_str)?;
			let occurrences: Vec<crate::Occurrence> = serde_json::from_str(&occurrences_json).map_err(|e| anyhow::anyhow!(format!("Failed to deserialize occurrences: {e}")))?;
			let relatives: Vec<crate::Relative> = serde_json::from_str(&relatives_json).map_err(|e| anyhow::anyhow!(format!("Failed to deserialize relatives: {e}")))?;

			let pattern = Pattern::new(pattern_id, occurrences, relatives);
			patterns.push(pattern);
		}

		Ok(patterns)
	}

	/// Get processed patterns in queue order (oldest first)
	/// This represents patterns that are ready to be processed into correlations/signals
	///
	/// # Errors
	/// - if database not found
	/// - if unable to query processed patterns
	pub async fn get_processed_patterns_queue(&self, aspect_id: &AspectId) -> Result<Vec<Pattern>> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| anyhow::anyhow!("Database not found".to_string()))?
		};

		let metadata_turso_db = db_info.metadata.as_ref().unwrap();

		let conn = metadata_turso_db.connect()?;
		let mut rows = conn.query("SELECT id, occurrences, relatives FROM patterns WHERE aspect_id = ? AND database_id = ? AND status = 'processed' ORDER BY processed_at ASC", turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| anyhow::anyhow!(format!("Failed to query processed patterns queue: {e}")))?;

		let mut patterns = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| anyhow::anyhow!(format!("Failed to get row: {e}")))? {
			let pattern_id_str = Self::value_to_string(&row.get_value(0)?, "Pattern ID").await?;
			let occurrences_json = Self::value_to_string(&row.get_value(1)?, "Occurrences").await?;
			let relatives_json = Self::value_to_string(&row.get_value(2)?, "Relatives").await?;

			let pattern_id = crate::PatternID::from_string(&pattern_id_str)?;
			let occurrences: Vec<crate::Occurrence> = serde_json::from_str(&occurrences_json).map_err(|e| anyhow::anyhow!(format!("Failed to deserialize occurrences: {e}")))?;
			let relatives: Vec<crate::Relative> = serde_json::from_str(&relatives_json).map_err(|e| anyhow::anyhow!(format!("Failed to deserialize relatives: {e}")))?;

			let pattern = Pattern::new(pattern_id, occurrences, relatives);
			patterns.push(pattern);
		}

		Ok(patterns)
	}

	/// Remove a processed pattern from the queue (after it has been processed into correlations/signals)
	///
	/// # Errors
	/// - if database not found
	/// - if unable to delete processed pattern
	pub async fn dequeue_processed_pattern(&self, pattern: &Pattern) -> Result<()> {
		let metadata_db = &self.metadata;
		let metadata_db_path = &self.metadata_path;

		let conn = Self::begin_concurrent(metadata_db, &metadata_db_path).await?;

		let res = conn.as_ref().execute("DELETE FROM patterns WHERE id = ? AND status = 'processed'", turso::params![pattern.id().to_string()]).await;
		let rows_affected = match res {
			Ok(rows) => {
				println!("Dequeue processed pattern affected rows: {}", rows);
				rows
			}
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!(format!("Failed to dequeue processed pattern: {e}")));
			}
		};

		Self::commit_concurrent(&conn).await?;

		// Verify that a pattern was actually deleted
		if rows_affected == 0 {
			return Err(anyhow::Error::msg("No matching processed pattern found to dequeue"));
		}

		Ok(())
	}

	/// Clear all processed patterns from the queue for an aspect
	/// This is used for cleanup after all correlations/signals have been generated
	///
	/// # Errors
	/// - if database not found
	/// - if unable to delete processed patterns
	pub async fn clear_processed_patterns_queue(&self, aspect_id: &AspectId) -> Result<usize> {
		let metadata_db = &self.metadata;
		let metadata_db_path = &self.metadata_path;

		let conn = Self::begin_concurrent(metadata_db, &metadata_db_path).await?;
		let res = conn.as_ref().execute("DELETE FROM patterns WHERE aspect_id = ? AND database_id = ? AND status = 'processed'", turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await;
		let rows_affected = match res {
			Ok(rows) => {
				println!("Clear processed patterns queue affected rows: {}", rows);
				rows
			}
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!(format!("Failed to clear processed patterns queue: {e}")));
			}
		};

		Self::commit_concurrent(&conn).await?;
		Ok(usize::try_from(rows_affected).map_err(|_| anyhow::anyhow!("Too many rows affected".to_string()))?)
	}
}

#[derive(Debug, Default, Clone)]
pub struct PatternStats {
	pub unprocessed_count: usize,
	pub processed_count: usize,
}

impl PatternStats {
	#[must_use]
	pub const fn total_count(&self) -> usize {
		self.unprocessed_count + self.processed_count
	}

	/// Check if there are patterns that need cleanup
	#[must_use]
	pub const fn needs_cleanup(&self) -> bool {
		self.processed_count > 0
	}

	/// Get the ratio of processed to total patterns
	/// # Errors
	/// - if counts exceed f64 precision limits during conversion
	pub fn processed_ratio(&self) -> std::result::Result<f64, crate::Error> {
		safe_ratio(self.processed_count, self.total_count())
	}
}
