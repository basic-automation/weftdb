use std::pin::Pin;

use anyhow::Result;
use chrono::{DateTime, Utc};
use futures::Stream;
use splimes::{Point, Resolution, Spline};

use crate::{AspectId, Batch, BatchId, Correlation, CorrelationID, DictionaryMetadata, Event, EventID, Measurement, Pattern, PatternID};

/// Trait for database analysis and output operations
#[async_trait::async_trait]
pub trait Outputs {
	//
	// Unbatched Measurements Queue
	//

	/// Get all unbatched measurement timestamps for an aspect (measurements not yet included in batches)
	async fn get_unbatched_measurements(&self, aspect_id: &AspectId) -> Result<Vec<DateTime<Utc>>>;

	/// Count unbatched measurements for an aspect
	async fn count_unbatched_measurements(&self, aspect_id: &AspectId) -> Result<u64>;

	/// Count unprocessed batches for an aspect
	async fn count_unprocessed_batches(&self, aspect_id: &AspectId) -> Result<u64>;

	/// Count processed batches for an aspect
	async fn count_processed_batches(&self, aspect_id: &AspectId) -> Result<u64>;

	// Measurements

	async fn analyze_point(&self, aspect_id: &AspectId, time: DateTime<Utc>, resolution: &Resolution, method: &Spline) -> Result<Point>;

	async fn analyze_range(&self, aspect_id: &AspectId, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, method: Spline) -> Result<Pin<Box<dyn Stream<Item = Result<Point>> + Send + 'static>>>;

	async fn get_raw_measurements(&self, aspect_id: &AspectId, start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>, max_per_page: usize, page: usize) -> Result<Pin<Box<dyn Stream<Item = Result<Measurement>> + Send + 'static>>>;

	async fn get_measurements_count(&self, aspect_id: &AspectId) -> Result<usize>;

	async fn parse_measurement_row(&self, row: turso::Row) -> Result<Measurement>;

	async fn get_boundary_measurements(&self, aspect_id: &AspectId) -> Result<Pin<Box<dyn Stream<Item = Result<Measurement>> + Send + 'static>>>;

	async fn fetch_measurements_for_range(&self, aspect_id: &AspectId, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Pin<Box<dyn Stream<Item = Result<Measurement>> + Send + 'static>>>;

	//
	// UnprocessedBatches
	//

	async fn get_unprocessed_batch(&self, aspect_id: &AspectId, batch_id: &BatchId) -> Result<Batch>;

	/// Get unprocessed batches in queue order (oldest first)
	/// This represents unprocessed batches that are ready to be processed into processed batches
	async fn get_unprocessed_batches(&self, aspect_id: &AspectId) -> Result<Pin<Box<dyn Stream<Item = Result<Batch>> + Send + 'static>>>;

	//
	// ProcessedBatches
	//

	async fn get_processed_batch(&self, aspect_id: &AspectId, batch_id: &BatchId) -> Result<Batch>;

	/// Get processed batches in queue order (oldest first)
	async fn get_processed_batches(&self, aspect_id: &AspectId) -> Result<Pin<Box<dyn Stream<Item = Result<Batch>> + Send + 'static>>>;

	//
	// Dictionaries
	//

	async fn get_dictionary_metadata(&self, aspect_id: &AspectId, dictionary_name: &str) -> Result<Option<DictionaryMetadata>>;

	async fn list_dictionaries(&self, aspect_id: &AspectId) -> Result<Vec<DictionaryMetadata>>;

	async fn get_dictionary_pattern(&self, aspect_id: &AspectId, dictionary_name: &str, pattern_id: &PatternID) -> Result<Pattern>;

	async fn get_dictionary_patterns(&self, aspect_id: &AspectId, dictionary_name: &str) -> Result<Pin<Box<dyn Stream<Item = Result<Pattern>> + Send + 'static>>>;

	//
	// Correlations
	//

	async fn get_correlation(&self, aspect_id: &AspectId, correlation_id: &CorrelationID) -> Result<Correlation>;

	async fn get_correlations(&self, aspect_id: &AspectId) -> Result<Pin<Box<dyn Stream<Item = Result<Correlation>> + Send + 'static>>>;

	//
	// Unprocessed Events
	//

	async fn get_unprocessed_event(&self, aspect_id: &AspectId, event_id: &EventID) -> Result<Event>;

	async fn get_unprocessed_events(&self, aspect_id: &AspectId) -> Result<Pin<Box<dyn Stream<Item = Result<Event>> + Send + 'static>>>;

	//
	// Processed Events
	//

	async fn get_processed_event(&self, aspect_id: &AspectId, event_id: &EventID) -> Result<Event>;

	async fn get_processed_events(&self, aspect_id: &AspectId) -> Result<Pin<Box<dyn Stream<Item = Result<Event>> + Send + 'static>>>;
}
