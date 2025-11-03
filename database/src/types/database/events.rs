use std::collections::HashMap;

use anyhow::{bail, Result};
use uuid::Uuid;

use super::helpers::safe_ratio;
use crate::{database::traits::AspectStructure, types::database::traits::database_structure::DatabaseStructure, Database, Event, DATABASES, AspectId};

const EVENT_CHUNK_SIZE: usize = 100;

impl Database {
	/// Store an event in the database
	///
	/// # Errors
	/// - if database not found
	/// - if unable to insert event
	pub async fn store_event(&self, event: &Event) -> Result<()> {
		let metadata_db = &self.metadata;
		let metadata_db_path = &self.metadata_path;

		// Serialize the event manifestations
		let manifestations_json = serde_json::to_string(event.manifestations()).map_err(|e| anyhow::anyhow!(format!("Failed to serialize event manifestations: {e}")))?;

		let event_id = event.id().to_string();
		let event_name = event.name().to_string();
		let conn = Self::begin_concurrent(metadata_db, metadata_db_path).await?;

		let res = conn.as_ref().execute("INSERT INTO events (id, database_id, name, manifestations, created_at) VALUES (?, ?, ?, ?, ?)", turso::params![event_id.clone(), self.id().as_uuid().to_string(), event_name.clone(), manifestations_json.clone(), chrono::Utc::now().timestamp_millis()]).await;
		match res {
			Ok(_) => println!("Stored event {} in database {}", event_id, self.id()),
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!(format!("Failed to insert event: {e}")));
			}
		}

		Self::commit_concurrent(&conn).await?;

		Ok(())
	}

	/// Get unprocessed events for the database
	///
	/// # Errors
	/// - if database not found
	/// - if unable to query events
	pub async fn get_unprocessed_events(&self) -> Result<Vec<Event>> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| anyhow::anyhow!("Database not found".to_string()))?
		};

		let Some(metadata_turso_db) = db_info.metadata() else {
			bail!("Metadata database not found");
		};

		let conn = metadata_turso_db.connect()?;
		let mut rows = conn.query("SELECT id, name, manifestations FROM events WHERE database_id = ? AND status = 'unprocessed' ORDER BY created_at", turso::params![self.id().as_uuid().to_string()]).await.map_err(|e| anyhow::anyhow!(format!("Failed to query events: {e}")))?;

		let mut events = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| anyhow::anyhow!(format!("Failed to get row: {e}")))? {
			let event_id_str = Self::value_to_string(&row.get_value(0)?, "Event ID").await?;
			let event_name = Self::value_to_string(&row.get_value(1)?, "Event name").await?;
			let manifestations_json = Self::value_to_string(&row.get_value(2)?, "Manifestations").await?;

			let event_id = crate::EventID::from_uuid(Uuid::parse_str(&event_id_str)?);
			let manifestations: HashMap<crate::ManifestationId, crate::Manifestation> = serde_json::from_str(&manifestations_json).map_err(|e| anyhow::anyhow!(format!("Failed to deserialize manifestations: {e}")))?;

			let mut event = Event::new(event_name, None);
			event.set_id(event_id);
			event.set_manifestations(manifestations);
			events.push(event);
		}

		Ok(events)
	}

	/// Mark an event as processed
	///
	/// # Errors
	/// - if database not found
	/// - if unable to update event status
	pub async fn mark_event_processed(&self, event: &Event) -> Result<()> {
		let metadata_db = &self.metadata;
		let metadata_db_path = &self.metadata_path;

		let conn = Self::begin_concurrent(metadata_db, metadata_db_path).await?;

		let res = conn.as_ref().execute("UPDATE events SET status = 'processed', processed_at = ? WHERE id = ? AND status = 'unprocessed'", turso::params![chrono::Utc::now().timestamp_millis(), event.id().to_string()]).await;
		let rows_affected = match res {
			Ok(rows) => {
				println!("Marked event {} as processed", event.id());
				rows
			}
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!(format!("Failed to mark event as processed: {e}")));
			}
		};

		Self::commit_concurrent(&conn).await?;

		// Verify that an event was actually updated
		if rows_affected == 0 {
			return Err(anyhow::Error::msg("No matching unprocessed event found to mark as processed"));
		}

		Ok(())
	}

	/// Store multiple events efficiently using batch operations
	///
	/// # Errors
	/// - if database not found
	/// - if unable to serialize or insert events
	pub async fn store_events(&self, events: &[Event]) -> Result<()> {
		if events.is_empty() {
			return Ok(());
		}

		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| anyhow::anyhow!("Database not found".to_string()))?
		};

		let Some(metadata_turso_db) = db_info.metadata() else {
			bail!("Metadata database not found");
		};

		// Process in chunks, with each chunk in its own transaction
		let total_events = events.len();
		let mut processed = 0;

		for (chunk_idx, chunk) in events.chunks(EVENT_CHUNK_SIZE).enumerate() {
			let mut conn = metadata_turso_db.connect()?;
			let tx = conn.transaction().await.map_err(|e| anyhow::anyhow!(format!("Failed to begin transaction: {e}")))?;

			let result = async {
				for event in chunk {
					// Serialize the event manifestations
					let manifestations_json = serde_json::to_string(event.manifestations()).map_err(|e| anyhow::anyhow!(format!("Failed to serialize event manifestations: {e}")))?;

					let event_id = event.id().to_string();
					let event_name = event.name().to_string();

					tx.execute("INSERT INTO events (id, database_id, name, manifestations, status, created_at) VALUES (?, ?, ?, ?, 'processed', ?)", turso::params![event_id, self.id().as_uuid().to_string(), event_name, manifestations_json, chrono::Utc::now().timestamp_millis()]).await.map_err(|e| anyhow::anyhow!(format!("Failed to insert event in transaction: {e}")))?;
				}
				Ok::<(), anyhow::Error>(())
			}
			.await;

			// Commit or rollback transaction based on result
			match result {
				Ok(()) => {
					tx.commit().await.map_err(|e| anyhow::anyhow!(format!("Failed to commit event transaction: {e}")))?;
					processed += chunk.len();

					// Progress reporting for large event storage operations
					if total_events > 100 && chunk_idx % 10 == 0 && chunk_idx > 0 {
						println!("Stored {processed} / {total_events} events");
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

	/// Get event count statistics
	///
	/// # Errors
	/// - if database not found
	/// - if unable to query event statistics
	pub async fn get_event_stats(&self) -> Result<EventStats> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| anyhow::anyhow!("Database not found".to_string()))?
		};

		let Some(metadata_turso_db) = db_info.metadata() else {
			bail!("Metadata database not found");
		};

		let conn = metadata_turso_db.connect()?;
		let mut rows = conn.query("SELECT status, COUNT(*) as count FROM events WHERE database_id = ? GROUP BY status", turso::params![self.id().as_uuid().to_string()]).await.map_err(|e| anyhow::anyhow!(format!("Failed to query event stats: {e}")))?;

		let mut stats = EventStats::default();
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

	/// Remove/cleanup processed events older than specified days
	///
	/// # Errors
	/// - if database not found
	/// - if unable to delete processed events
	pub async fn cleanup_processed_events(&self, older_than_days: i64) -> Result<usize> {
		let metadata_db = self.metadata();
		let metadata_db_path = self.metadata_path();

		let cutoff_time = chrono::Utc::now() - chrono::Duration::days(older_than_days);
		let conn = Self::begin_concurrent(metadata_db, metadata_db_path).await?;

		let res = conn.as_ref().execute("DELETE FROM events WHERE database_id = ? AND status = 'processed' AND processed_at < ?", turso::params![self.id().as_uuid().to_string(), cutoff_time.timestamp_millis()]).await;
		let rows_affected = match res {
			Ok(rows) => {
				println!("Cleaned up {} processed events older than {} days", rows, older_than_days);
				rows
			}
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!(format!("Failed to cleanup processed events: {e}")));
			}
		};

		usize::try_from(rows_affected).map_err(|_| anyhow::anyhow!("Too many rows affected".to_string()))
	}

	/// Remove all processed events (immediate cleanup)
	///
	/// # Errors
	/// - if database not found
	/// - if unable to delete processed events
	pub async fn cleanup_all_processed_events(&self) -> Result<usize> {
		let metadata_db = self.metadata();
		let metadata_db_path = self.metadata_path();

		let conn = Self::begin_concurrent(metadata_db, metadata_db_path).await?;

		let res = conn.as_ref().execute("DELETE FROM events WHERE database_id = ? AND status = 'processed'", turso::params![self.id().as_uuid().to_string()]).await;
		let rows_affected = match res {
			Ok(rows) => {
				println!("Cleaned up {} processed events", rows);
				rows
			}
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!(format!("Failed to cleanup all processed events: {e}")));
			}
		};

		Self::commit_concurrent(&conn).await?;

		usize::try_from(rows_affected).map_err(|_| anyhow::anyhow!("Too many rows affected".to_string()))
	}

	/// Get processed events (for cleanup verification)
	///
	/// # Errors
	/// - if database not found
	/// - if unable to query processed events
	pub async fn get_processed_events(&self) -> Result<Vec<Event>> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| anyhow::anyhow!("Database not found".to_string()))?
		};

		let Some(metadata_turso_db) = db_info.metadata() else {
			bail!("Metadata database not found");
		};

		let conn = metadata_turso_db.connect()?;
		let mut rows = conn.query("SELECT id, name, manifestations FROM events WHERE database_id = ? AND status = 'processed' ORDER BY processed_at", turso::params![self.id().as_uuid().to_string()]).await.map_err(|e| anyhow::anyhow!(format!("Failed to query processed events: {e}")))?;

		let mut events = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| anyhow::anyhow!(format!("Failed to get row: {e}")))? {
			let event_id_str = Self::value_to_string(&row.get_value(0)?, "Event ID").await?;
			let event_name = Self::value_to_string(&row.get_value(1)?, "Event name").await?;
			let manifestations_json = Self::value_to_string(&row.get_value(2)?, "Manifestations").await?;

			let event_id = crate::EventID::from_uuid(Uuid::parse_str(&event_id_str)?);
			let manifestations: HashMap<crate::ManifestationId, crate::Manifestation> = serde_json::from_str(&manifestations_json).map_err(|e| anyhow::anyhow!(format!("Failed to deserialize manifestations: {e}")))?;

			let mut event = Event::new(event_name, None);
			event.set_id(event_id);
			event.set_manifestations(manifestations);
			events.push(event);
		}

		Ok(events)
	}

	/// Get processed events in queue order (oldest first)
	/// This represents events that are ready to be processed into correlations/signals
	///
	/// # Errors
	/// - if database not found
	/// - if unable to query processed events
	pub async fn get_processed_events_queue(&self) -> Result<Vec<Event>> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| anyhow::anyhow!("Database not found".to_string()))?
		};

		let Some(metadata_turso_db) = db_info.metadata() else {
			bail!("Metadata database not found");
		};

		let conn = metadata_turso_db.connect()?;
		let mut rows = conn.query("SELECT id, name, manifestations FROM events WHERE database_id = ? AND status = 'processed' ORDER BY processed_at ASC", turso::params![self.id().as_uuid().to_string()]).await.map_err(|e| anyhow::anyhow!(format!("Failed to query processed events queue: {e}")))?;

		let mut events = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| anyhow::anyhow!(format!("Failed to get row: {e}")))? {
			let event_id_str = Self::value_to_string(&row.get_value(0)?, "Event ID").await?;
			let event_name = Self::value_to_string(&row.get_value(1)?, "Event name").await?;
			let manifestations_json = Self::value_to_string(&row.get_value(2)?, "Manifestations").await?;

			let event_id = crate::EventID::from_uuid(Uuid::parse_str(&event_id_str)?);
			let manifestations: HashMap<crate::ManifestationId, crate::Manifestation> = serde_json::from_str(&manifestations_json).map_err(|e| anyhow::anyhow!(format!("Failed to deserialize manifestations: {e}")))?;

			let mut event = Event::new(event_name, None);
			event.set_id(event_id);
			event.set_manifestations(manifestations);
			events.push(event);
		}

		Ok(events)
	}

	/// Remove a processed event from the queue (after it has been processed into correlations/signals)
	///
	/// # Errors
	/// - if database not found
	/// - if unable to delete processed event
	pub async fn dequeue_processed_event(&self, event: &Event) -> Result<()> {
		let metadata_db = self.metadata();
		let metadata_db_path = self.metadata_path();
		let conn = Self::begin_concurrent(metadata_db, metadata_db_path).await?;

		let res = conn.as_ref().execute("DELETE FROM events WHERE id = ? AND status = 'processed'", turso::params![event.id().to_string()]).await;
		let rows_affected = match res {
			Ok(rows) => {
				println!("Dequeued processed event {}", event.id());
				rows
			}
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!(format!("Failed to dequeue processed event: {e}")));
			}
		};

		// Verify that an event was actually deleted
		if rows_affected == 0 {
			return Err(anyhow::Error::msg("No matching processed event found to dequeue"));
		}

		Ok(())
	}

	/// Clear all processed events from the queue for the database
	/// This is used for cleanup after all correlations/signals have been generated
	///
	/// # Errors
	/// - if database not found
	/// - if unable to delete processed events
	pub async fn clear_processed_events_queue(&self) -> Result<usize> {
		let metadata_db = self.metadata();
		let metadata_db_path = self.metadata_path();
		let conn = Self::begin_concurrent(metadata_db, metadata_db_path).await?;
		let res = conn.as_ref().execute("DELETE FROM events WHERE database_id = ? AND status = 'processed'", turso::params![self.id().as_uuid().to_string()]).await;
		let rows_affected = match res {
			Ok(rows) => {
				println!("Cleared {} processed events from queue", rows);
				rows
			}
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!(format!("Failed to clear processed events queue: {e}")));
			}
		};

		Self::commit_concurrent(&conn).await?;

		usize::try_from(rows_affected).map_err(|_| anyhow::anyhow!("Too many rows affected".to_string()))
	}

	/// Clear all events (both processed and unprocessed) from the database
	/// This is used for testing and cleanup purposes
	///
	/// # Errors
	/// - if database not found
	/// - if unable to delete events
	pub async fn clear_all_events(&self, aspect_id: AspectId) -> Result<usize> {
		let mut aspect = self.get_aspect(aspect_id).await?;
		let event_db_path = &aspect.events_path();
		let event_db = &aspect.events().await?;

		let conn = Self::begin_concurrent(event_db, event_db_path).await?;

		let res = conn.as_ref().execute("DELETE FROM events WHERE database_id = ?", turso::params![self.id().as_uuid().to_string()]).await;
		let rows = match res {
			Ok(rows) => {
				println!("Deleted {} rows from events", rows);
				rows
			}
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!(format!("Failed to clear all events: {e}")));
			}
		};

		Self::commit_concurrent(&conn).await?;

		usize::try_from(rows).map_err(|_| anyhow::anyhow!("Too many rows affected".to_string()))
	}
}

#[async_trait::async_trait]
impl super::traits::EventDatabase for Database {
	async fn store_event(&self, event: &Event) -> Result<()> {
		self.store_event(event).await
	}

	async fn get_unprocessed_events(&self) -> Result<Vec<Event>> {
		self.get_unprocessed_events().await
	}

	async fn mark_event_processed(&self, event: &Event) -> Result<()> {
		self.mark_event_processed(event).await
	}

	async fn store_events(&self, events: &[Event]) -> Result<()> {
		self.store_events(events).await
	}

	async fn get_event_stats(&self) -> Result<EventStats> {
		self.get_event_stats().await
	}

	async fn cleanup_processed_events(&self, older_than_days: i64) -> Result<usize> {
		self.cleanup_processed_events(older_than_days).await
	}

	async fn cleanup_all_processed_events(&self) -> Result<usize> {
		self.cleanup_all_processed_events().await
	}

	async fn get_processed_events(&self) -> Result<Vec<Event>> {
		self.get_processed_events().await
	}

	async fn get_processed_events_queue(&self) -> Result<Vec<Event>> {
		self.get_processed_events_queue().await
	}

	async fn dequeue_processed_event(&self, event: &Event) -> Result<()> {
		self.dequeue_processed_event(event).await
	}

	async fn clear_processed_events_queue(&self) -> Result<usize> {
		self.clear_processed_events_queue().await
	}

	async fn clear_all_events(&self, aspect_id: AspectId) -> Result<usize> {
		self.clear_all_events(aspect_id).await
	}
}
#[derive(Debug, Default, Clone)]
pub struct EventStats {
	pub unprocessed_count: usize,
	pub processed_count: usize,
}

impl EventStats {
	#[must_use]
	pub const fn total_count(&self) -> usize {
		self.unprocessed_count + self.processed_count
	}

	/// Check if there are events that need cleanup
	#[must_use]
	pub const fn needs_cleanup(&self) -> bool {
		self.processed_count > 0
	}

	/// Get the ratio of processed to total events
	/// # Errors
	/// - if counts exceed f64 precision limits during conversion
	pub fn processed_ratio(&self) -> std::result::Result<f64, crate::Error> {
		safe_ratio(self.processed_count, self.total_count())
	}
}
