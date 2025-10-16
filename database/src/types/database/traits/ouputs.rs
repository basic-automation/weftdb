use std::pin::Pin;

use anyhow::Result;
use chrono::{DateTime, Utc};
use futures::Stream;
use splimes::{Point, Resolution, Spline};

use crate::AspectId;

/// Trait for database analysis and output operations
#[async_trait::async_trait]
pub trait Outputs {
	// Measurements

	async fn analyze_point(&self, aspect: AspectId, time: DateTime<Utc>, resolution: Resolution, method: Spline) -> Result<Point>;

	async fn analyze_range(&self, aspect: AspectId, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, method: Spline) -> Result<Pin<Box<dyn Stream<Item = Result<Point>> + Send + 'static>>>;

	// UnprocessedBatches

	async fn get_unprocessed_batch(&self, aspect_id: &AspectId, batch_id: &BatchId) -> Result<Batch>;

	async fn get_unprocessed_batches(&self, aspect_id: &AspectId) -> Result<Pin<Box<dyn Stream<Item = Result<Batch>> + Send + 'static>>>;

	/// Get unprocessed batches in queue order (oldest first)
	/// This represents unprocessed batches that are ready to be processed into processed batches
	async fn get_unprocessed_batch_queue(&self, aspect_id: &AspectId) -> Result<Pin<Box<dyn Stream<Item = Result<Batch>> + Send + 'static>>>;
}
