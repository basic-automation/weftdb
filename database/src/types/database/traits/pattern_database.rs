use anyhow::Result;

use crate::{database::patterns::PatternStats, AspectId, Pattern};

/// Trait for pattern database operations
#[async_trait::async_trait]
pub trait PatternDatabase {
	/// Store a pattern in the database
	async fn store_pattern(&self, pattern: &Pattern) -> Result<()>;

	/// Get unprocessed patterns for an aspect
	async fn get_unprocessed_patterns(&self, aspect_id: &AspectId) -> Result<Vec<Pattern>>;

	/// Mark a pattern as processed
	async fn mark_pattern_processed(&self, pattern: &Pattern) -> Result<()>;

	/// Store multiple patterns efficiently using batch operations
	async fn store_patterns(&self, patterns: &[Pattern], aspect_id: &AspectId) -> Result<()>;

	/// Get pattern count statistics for an aspect
	async fn get_pattern_stats(&self, aspect_id: &AspectId) -> Result<PatternStats>;

	/// Remove/cleanup processed patterns older than specified days
	async fn cleanup_processed_patterns(&self, aspect_id: &AspectId, older_than_days: i64) -> Result<usize>;

	/// Remove all processed patterns for an aspect (immediate cleanup)
	async fn cleanup_all_processed_patterns(&self, aspect_id: &AspectId) -> Result<usize>;

	/// Get processed patterns for an aspect (for cleanup verification)
	async fn get_processed_patterns(&self, aspect_id: &AspectId) -> Result<Vec<Pattern>>;

	/// Get processed patterns in queue order (oldest first)
	async fn get_processed_patterns_queue(&self, aspect_id: &AspectId) -> Result<Vec<Pattern>>;

	/// Remove a processed pattern from the queue
	async fn dequeue_processed_pattern(&self, pattern: &Pattern) -> Result<()>;

	/// Clear all processed patterns from the queue for an aspect
	async fn clear_processed_patterns_queue(&self, aspect_id: &AspectId) -> Result<usize>;
}
