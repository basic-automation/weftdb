use std::pin::Pin;

use anyhow::Result;
use chrono::{DateTime, Utc};
use futures::Stream;
use splimes::{Point, Resolution, Spline};

use crate::{AspectId, Batch, BatchId, Measurement};

/// Trait for database analysis and output operations
#[async_trait::async_trait]
pub trait Outputs {
	// Measurements

	async fn analyze_point(&self, aspect_id: &AspectId, time: DateTime<Utc>, resolution: &Resolution, method: &Spline) -> Result<Point>;

	async fn analyze_range(&self, aspect_id: &AspectId, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, method: Spline) -> Result<Pin<Box<dyn Stream<Item = Result<Point>> + Send + 'static>>>;

	async fn get_raw_measurements(&self, aspect_id: &AspectId, start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>, max_per_page: usize, page: usize) -> Result<Vec<Measurement>>;

	async fn get_measurements_count(&self, aspect_id: &AspectId) -> Result<usize>;

	async fn parse_measurement_row(&self, row: turso::Row) -> Result<Measurement>;

	async fn get_boundary_measurements(&self, aspect_id: &AspectId) -> Result<Vec<Measurement>>;

	async fn fetch_measurements_for_range(&self, aspect_id: &AspectId, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Vec<Measurement>>;

	// UnprocessedBatches

	async fn get_unprocessed_batch(&self, aspect_id: &AspectId, batch_id: &BatchId) -> Result<Batch>;

	/// Get unprocessed batches in queue order (oldest first)
	/// This represents unprocessed batches that are ready to be processed into processed batches
	async fn get_unprocessed_batches(&self, aspect_id: &AspectId) -> Result<Pin<Box<dyn Stream<Item = Result<Batch>> + Send + 'static>>>;

	async fn parse_batch_row(row: turso::Row) -> Result<Batch>;
}
