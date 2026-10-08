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

	/// Fetch measurements for a specific time chunk (used by `analyze_range` for chunked streaming)
	async fn fetch_measurements_for_chunk(&self, aspect_id: &AspectId, chunk_start: DateTime<Utc>, chunk_end: DateTime<Utc>) -> Result<Vec<Measurement>>;

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

	/// The registration of the aspect's dictionary `dictionary_name`: its id, description,
	/// steps and variabilities. `None` when there is none, because the dictionary has no
	/// database yet, its database has no tables yet, or nothing complete is registered in
	/// it.
	///
	/// A registration that was read is cached for up to ten minutes, per `Database`
	/// handle. [`set_dictionary_metadata`](crate::database::traits::Inputs::set_dictionary_metadata)
	/// on the same handle invalidates it, and a read that overlapped that write does not
	/// cache what it read. A registration written any other way, through
	/// [`AspectStructure::new_dictionary`](crate::database::traits::AspectStructure::new_dictionary),
	/// another handle or another process, can be answered from the cache until the entry
	/// expires.
	///
	/// # Errors
	///
	/// An [`InvalidDictionaryName`](crate::InvalidDictionaryName) for a name that cannot
	/// name a dictionary's file, a failed read, or a stored value that does not parse, such
	/// as a step interpolation splimes rejects.
	async fn get_dictionary_metadata(&self, aspect_id: &AspectId, dictionary_name: &str) -> Result<Option<DictionaryMetadata>>;

	/// The registration of each of the aspect's dictionaries, by name, as
	/// [`get_dictionary_metadata`](Self::get_dictionary_metadata) reads it.
	///
	/// The aspect's dictionaries are the regular files `<name>.db` in its `dictionaries/`
	/// directory whose `<name>` is a valid dictionary name; symlinks, directories and other
	/// entries are skipped. Each of them is opened, which sets its journal mode and can
	/// create the dictionary tables in it, so keep copies and backups out of that directory
	/// or give them another extension. A dictionary without a registration is not listed,
	/// and one whose registration cannot be read (such as a stored step interpolation
	/// splimes rejects) is logged as a warning and skipped, so the readable ones are still
	/// listed; `get_dictionary_metadata` reports why it cannot be read.
	///
	/// # Errors
	///
	/// When the aspect is unknown or its dictionaries directory cannot be read.
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
