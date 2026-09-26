use anyhow::Result;

use crate::{AspectId, Event};

/// Event statistics struct
#[derive(Debug, Clone, Default)]
pub struct EventStats {
	pub total_events: usize,
	pub unprocessed_events: usize,
	pub processed_events: usize,
}

/// Trait for event database operations
#[async_trait::async_trait]
pub trait EventDatabase {
	/// Store an event in the database
	async fn store_event(&self, event: &Event) -> Result<()>;

	/// Get unprocessed events for the database
	async fn get_unprocessed_events(&self) -> Result<Vec<Event>>;

	/// Mark an event as processed
	async fn mark_event_processed(&self, event: &Event) -> Result<()>;

	/// Store multiple events efficiently using batch operations
	async fn store_events(&self, events: &[Event]) -> Result<()>;

	/// Get event count statistics
	async fn get_event_stats(&self) -> Result<EventStats>;

	/// Remove/cleanup processed events older than specified days
	async fn cleanup_processed_events(&self, older_than_days: i64) -> Result<usize>;

	/// Remove all processed events (immediate cleanup)
	async fn cleanup_all_processed_events(&self) -> Result<usize>;

	/// Get processed events (for cleanup verification)
	async fn get_processed_events(&self) -> Result<Vec<Event>>;

	/// Get processed events in queue order (oldest first)
	async fn get_processed_events_queue(&self) -> Result<Vec<Event>>;

	/// Remove a processed event from the queue
	async fn dequeue_processed_event(&self, event: &Event) -> Result<()>;

	/// Clear all processed events from the queue
	async fn clear_processed_events_queue(&self) -> Result<usize>;

	/// Clear all events (both processed and unprocessed)
	async fn clear_all_events(&self, aspect_id: &AspectId) -> Result<usize>;
}
