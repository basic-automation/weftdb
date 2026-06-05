//! Pipeline API for dataset processing and signal generation.
//!
//! This module provides a high-level API for processing time-series data,
//! extracting patterns, detecting events, and generating prediction signals.
//!
//! # Overview
//!
//! A Pipeline is now part of an Aspect and is persisted in `pipeline.db`.
//! Each Aspect can have exactly one Pipeline, but the Pipeline can have
//! multiple dictionaries for pattern matching.
//!
//! # Example
//!
//! ```ignore
//! use database_orchestration::{Pipeline, event_detector_fn};
//! use database::{Database, Resolution};
//! use splimes::Spline;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let database = Database::existing("MyDatabase").await?;
//!     let aspect_id = /* get aspect id */;
//!
//!     let mut pipeline = Pipeline::builder(database, aspect_id)
//!         .batch_size(24)
//!         .add_dictionary("primary", "Main dictionary", constraints)
//!         .with_monthly_increase_detector(0.05)
//!         .build()
//!         .await?;
//!
//!     pipeline.run().await?;
//!     Ok(())
//! }
//! ```

use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};

use anyhow::Result;
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use database::{
	database::traits::{DatabaseStructure, Inputs, Outputs, PipelineInputs, PipelineOutputs}, AspectId, Database, DetectorMetadata, DetectorType, EventID, PipelineConfig as DbPipelineConfig, PipelineState as DbPipelineState, Resolution
};
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use splimes::Spline;

use crate::{Dictionary, DictionaryConstraints, Event, SignalType, Signals, SIGNALS_QUEUE};

/// Unique identifier for an event detector.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DetectorId(String);

impl DetectorId {
	/// Creates a new detector ID from a string.
	#[must_use]
	pub fn new(id: impl Into<String>) -> Self {
		Self(id.into())
	}

	/// Returns the detector ID as a string slice.
	#[must_use]
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl std::fmt::Display for DetectorId {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

impl From<&str> for DetectorId {
	fn from(s: &str) -> Self {
		Self::new(s)
	}
}

impl From<String> for DetectorId {
	fn from(s: String) -> Self {
		Self::new(s)
	}
}

/// Type alias for async event detector functions.
///
/// An event detector receives:
/// - `&Database` - the database to query measurements from
/// - `&AspectId` - the aspect to analyze
/// - `&Resolution` - the resolution for analysis
/// - `&Spline` - the interpolation method
///
/// Returns a `Vec<Event>` of detected events with their manifestations.
pub type EventDetectorFn = Arc<dyn for<'a> Fn(&'a Database, &'a AspectId, &'a Resolution, &'a Spline) -> Pin<Box<dyn Future<Output = Result<Vec<Event>>> + Send + 'a>> + Send + Sync>;

/// Helper macro to create an `EventDetectorFn` from an async function.
///
/// **Important**: This macro works best with standalone async functions. If you need
/// to capture additional parameters (like threshold values), create the detector
/// function inline using `Arc::new` with an `async move` block instead. See the
/// examples below.
///
/// # Example with Standalone Function
///
/// ```ignore
/// use database_orchestration::{event_detector_fn, EventDetectorFn};
///
/// async fn my_detector(
///     db: &Database,
///     aspect: &AspectId,
///     resolution: &Resolution,
///     method: &Spline,
/// ) -> Result<Vec<Event>> {
///     // Detection logic here
///     Ok(vec![])
/// }
///
/// let detector_fn: EventDetectorFn = event_detector_fn!(my_detector);
/// ```
///
/// # Example with Captured Parameters
///
/// When you need to capture additional parameters, use this pattern instead:
///
/// ```ignore
/// use database_orchestration::pipeline::EventDetectorFn;
/// use std::sync::Arc;
///
/// let threshold = 0.05;
/// let detector_fn: EventDetectorFn = Arc::new(move |db, aspect, res, method| {
///     Box::pin(async move {
///         detectors::detect_monthly_increase(db, aspect, res, method, threshold).await
///     })
/// });
/// ```
#[macro_export]
macro_rules! event_detector_fn {
	($func:expr) => {
		std::sync::Arc::new(move |db, aspect, res, method| Box::pin($func(db, aspect, res, method)) as std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Vec<database::Event>>> + Send + '_>>) as $crate::pipeline::EventDetectorFn
	};
}

/// Metadata for a registered event detector.
#[derive(Clone)]
pub struct EventDetector {
	id: DetectorId,
	name: String,
	description: Option<String>,
	detector_type: DetectorType,
	config_json: Option<String>,
	detector_fn: EventDetectorFn,
}

impl EventDetector {
	/// Creates a new event detector with metadata.
	pub fn new(id: impl Into<String>, name: impl Into<String>, description: Option<String>, detector_fn: EventDetectorFn) -> Self {
		Self { id: DetectorId::new(id), name: name.into(), description, detector_type: DetectorType::Custom, config_json: None, detector_fn }
	}

	/// Creates a new builtin event detector with configuration.
	pub fn builtin(id: impl Into<String>, name: impl Into<String>, description: Option<String>, builtin_type: impl Into<String>, config_json: Option<String>, detector_fn: EventDetectorFn) -> Self {
		Self { id: DetectorId::new(id), name: name.into(), description, detector_type: DetectorType::Builtin(builtin_type.into()), config_json, detector_fn }
	}

	/// Returns the detector's unique ID.
	#[must_use]
	pub const fn id(&self) -> &DetectorId {
		&self.id
	}

	/// Returns the detector's display name.
	#[must_use]
	pub fn name(&self) -> &str {
		&self.name
	}

	/// Returns the detector's description.
	#[must_use]
	pub fn description(&self) -> Option<&str> {
		self.description.as_deref()
	}

	/// Returns the detector type.
	#[must_use]
	pub const fn detector_type(&self) -> &DetectorType {
		&self.detector_type
	}

	/// Runs the detector.
	///
	/// # Errors
	///
	/// Returns an error if the detector function fails.
	pub async fn detect(&self, database: &Database, aspect_id: &AspectId, resolution: &Resolution, method: &Spline) -> Result<Vec<Event>> {
		(self.detector_fn)(database, aspect_id, resolution, method).await
	}

	/// Converts this detector to metadata for persistence.
	fn to_metadata(&self) -> DetectorMetadata {
		DetectorMetadata::new(self.id.as_str(), &self.name, self.description.clone(), self.detector_type.clone(), self.config_json.clone())
	}
}

/// Configuration for a dictionary in the pipeline.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DictionaryConfig {
	/// Dictionary name (unique within the pipeline).
	pub name: String,
	/// Dictionary description.
	pub description: String,
	/// Dictionary constraints for pattern matching.
	pub constraints: DictionaryConstraints,
}

impl DictionaryConfig {
	/// Creates a new dictionary configuration.
	#[must_use]
	pub fn new(name: impl Into<String>, description: impl Into<String>, constraints: DictionaryConstraints) -> Self {
		Self { name: name.into(), description: description.into(), constraints }
	}
}

/// Runtime configuration for activating a dormant pipeline.
///
/// This allows configuring pipelines at runtime without rebuilding them.
/// Useful for running multiple pipelines in parallel with different configurations.
///
/// # Example
///
/// ```ignore
/// let run_config = PipelineRunConfig::new()
///     .with_dictionary("primary", "Main dictionary", constraints)
///     .with_peak_detector("Peak Detection");
///
/// let mut pipeline = Pipeline::load_or_create(database, &aspect_id).await?;
/// pipeline.apply_runtime_config(run_config).await?;
/// pipeline.run().await?;
/// ```
#[derive(Clone, Default)]
pub struct PipelineRunConfig {
	/// Dictionaries to add to the pipeline.
	pub dictionaries: Vec<DictionaryConfig>,
	/// Detectors to register (as closures that create `EventDetector`).
	detector_factories: Vec<Arc<dyn Fn() -> EventDetector + Send + Sync>>,
	/// Optional batch size override.
	pub batch_size: Option<usize>,
	/// Optional spline method override.
	pub spline_method: Option<Spline>,
}

impl std::fmt::Debug for PipelineRunConfig {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("PipelineRunConfig").field("dictionaries", &self.dictionaries).field("detector_count", &self.detector_factories.len()).field("batch_size", &self.batch_size).field("spline_method", &self.spline_method).finish()
	}
}

impl PipelineRunConfig {
	/// Creates a new empty runtime configuration.
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// Adds a dictionary configuration.
	#[must_use]
	pub fn with_dictionary(mut self, name: impl Into<String>, description: impl Into<String>, constraints: DictionaryConstraints) -> Self {
		self.dictionaries.push(DictionaryConfig::new(name, description, constraints));
		self
	}

	/// Adds a dictionary with the provided configuration.
	#[must_use]
	pub fn with_dictionary_config(mut self, config: DictionaryConfig) -> Self {
		self.dictionaries.push(config);
		self
	}

	/// Registers a custom event detector.
	#[must_use]
	pub fn with_detector(mut self, detector: EventDetector) -> Self {
		self.detector_factories.push(Arc::new(move || detector.clone()));
		self
	}

	/// Registers a peak detection event detector.
	#[must_use]
	pub fn with_peak_detector(self, name: impl Into<String>) -> Self {
		let name_str = name.into();
		let id = format!("peak_{}", name_str.to_lowercase().replace(' ', "_"));
		let name_clone = name_str.clone();
		let config_json = serde_json::to_string(&serde_json::json!({
		    "event_name": name_str
		}))
		.ok();

		let detector_fn: EventDetectorFn = Arc::new(move |db, aspect, resolution, method| {
			let name = name_clone.clone();
			Box::pin(async move { crate::detectors::detect_peaks(db, aspect, resolution, method, &name).await })
		});

		let detector = EventDetector::builtin(id, name_str.clone(), Some(format!("Detects peak values for {name_str}")), "peak", config_json, detector_fn);

		self.with_detector(detector)
	}

	/// Registers a valley detection event detector.
	#[must_use]
	pub fn with_valley_detector(self, name: impl Into<String>) -> Self {
		let name_str = name.into();
		let id = format!("valley_{}", name_str.to_lowercase().replace(' ', "_"));
		let name_clone = name_str.clone();
		let config_json = serde_json::to_string(&serde_json::json!({
		    "event_name": name_str
		}))
		.ok();

		let detector_fn: EventDetectorFn = Arc::new(move |db, aspect, resolution, method| {
			let name = name_clone.clone();
			Box::pin(async move { crate::detectors::detect_valleys(db, aspect, resolution, method, &name).await })
		});

		let detector = EventDetector::builtin(id, name_str.clone(), Some(format!("Detects valley (minimum) values for {name_str}")), "valley", config_json, detector_fn);

		self.with_detector(detector)
	}

	/// Registers the built-in monthly price increase detector.
	#[must_use]
	pub fn with_monthly_increase_detector(self, threshold_percent: f64) -> Self {
		let threshold = threshold_percent;
		let id = format!("monthly_increase_{:.0}pct", threshold_percent * 100.0);
		let name = format!("{:.0}% Monthly Increase Detector", threshold_percent * 100.0);
		let config_json = serde_json::to_string(&serde_json::json!({
		    "threshold_percent": threshold_percent
		}))
		.ok();

		let detector_fn: EventDetectorFn = Arc::new(move |db, aspect, resolution, method| {
			let threshold = threshold;
			Box::pin(async move { crate::detectors::detect_monthly_increase(db, aspect, resolution, method, threshold).await })
		});

		let detector = EventDetector::builtin(id, name, Some(format!("Detects when price increases {:.0}% or more from start to end of month", threshold_percent * 100.0)), "monthly_increase", config_json, detector_fn);

		self.with_detector(detector)
	}

	/// Sets the batch size override.
	#[must_use]
	pub const fn batch_size(mut self, size: usize) -> Self {
		self.batch_size = Some(size);
		self
	}

	/// Sets the spline method override.
	#[must_use]
	pub const fn spline_method(mut self, method: Spline) -> Self {
		self.spline_method = Some(method);
		self
	}

	/// Returns true if this configuration is empty (no dictionaries or detectors).
	#[must_use]
	pub fn is_empty(&self) -> bool {
		self.dictionaries.is_empty() && self.detector_factories.is_empty() && self.batch_size.is_none() && self.spline_method.is_none()
	}

	/// Returns the number of detectors configured.
	#[must_use]
	pub fn detector_count(&self) -> usize {
		self.detector_factories.len()
	}

	/// Creates all configured detectors.
	fn create_detectors(&self) -> Vec<EventDetector> {
		self.detector_factories.iter().map(|f| f()).collect()
	}
}

/// A pipeline for processing time-series data and generating prediction signals.
///
/// The pipeline orchestrates the full data processing workflow:
/// 1. Data preparation (batching)
/// 2. Pattern extraction (for all dictionaries)
/// 3. Event detection
/// 4. Correlation creation
/// 5. Signal generation
///
/// # Persistence
///
/// Pipeline configuration and state are stored in `pipeline.db` within the Aspect directory.
/// Detector functions cannot be persisted and must be re-registered after loading.
///
/// # Example
///
/// ```ignore
/// let mut pipeline = Pipeline::builder(database, aspect_id)
///     .add_dictionary("primary", "Main patterns", constraints)
///     .with_monthly_increase_detector(0.05)
///     .build()
///     .await?;
///
/// pipeline.run().await?;
///
/// let result = pipeline.query_probability(&event_id, &signal_type, Utc::now()).await?;
/// ```
pub struct Pipeline {
	database: Database,
	aspect_id: AspectId,
	resolution: Resolution,
	config: DbPipelineConfig,
	state: DbPipelineState,
	/// Dictionaries keyed by name.
	dictionaries: HashMap<String, Dictionary>,
	/// Dictionary configurations for persistence tracking.
	dictionary_configs: HashMap<String, DictionaryConfig>,
	signals: Signals,
	event_detectors: HashMap<DetectorId, EventDetector>,
}

impl Pipeline {
	/// Creates a new pipeline builder.
	#[must_use]
	pub const fn builder(database: Database, aspect_id: AspectId) -> PipelineBuilder {
		PipelineBuilder::new(database, aspect_id)
	}

	/// Loads an existing pipeline from the Aspect's pipeline.db.
	///
	/// This restores the pipeline configuration and state from the database.
	/// Note: Event detector functions must be re-registered after loading
	/// since functions cannot be serialized.
	///
	/// # Errors
	///
	/// Returns an error if:
	/// - The pipeline database cannot be accessed
	/// - No pipeline configuration exists for this aspect
	///
	/// # Example
	///
	/// ```ignore
	/// let mut pipeline = Pipeline::load(database.clone(), &aspect_id).await?;
	///
	/// // Re-register detectors after loading
	/// pipeline.register_detector(EventDetector::new(
	///     "my_detector",
	///     "My Detector",
	///     None,
	///     event_detector_fn!(my_detector_fn),
	/// ));
	/// ```
	pub async fn load(database: Database, aspect_id: &AspectId) -> Result<Self> {
		// Get resolution from aspect
		let resolution = database.get_aspect_resolution(aspect_id).await?;

		// Load config and state from pipeline.db
		let config = database.load_pipeline_config(aspect_id).await?.ok_or_else(|| anyhow::anyhow!("No pipeline configuration found for aspect {aspect_id}"))?;

		let state = database.load_pipeline_state(aspect_id).await?.unwrap_or_default();

		// Load dictionary names from database
		let dictionary_names = database.list_pipeline_dictionaries(aspect_id).await?;

		// Load detector metadata (for warning about unregistered detectors)
		let detector_metadata = database.list_pipeline_detectors(aspect_id).await?;

		if !detector_metadata.is_empty() {
			tracing::warn!(
			    aspect_id = %aspect_id,
			    detector_count = detector_metadata.len(),
			    "Pipeline loaded with {} detector(s) that need re-registration: {:?}",
			    detector_metadata.len(),
			    detector_metadata.iter().map(database::DetectorMetadata::detector_id).collect::<Vec<_>>()
			);
		}

		// Create empty dictionaries for now - they'll be loaded when needed
		// (Dictionary pattern data is stored separately in the aspect databases)
		let dictionaries = HashMap::new();
		let dictionary_configs = HashMap::new();

		tracing::info!(
		    aspect_id = %aspect_id,
		    run_count = state.run_count,
		    last_run = ?state.last_run,
		    dictionary_count = dictionary_names.len(),
		    detector_count = detector_metadata.len(),
		    "Loaded pipeline from database"
		);

		Ok(Self {
			database,
			aspect_id: *aspect_id,
			resolution,
			config,
			state,
			dictionaries,
			dictionary_configs,
			signals: Signals::new(),
			event_detectors: HashMap::new(), // Must be re-registered
		})
	}

	/// Loads an existing pipeline or creates a default dormant one.
	///
	/// This is the preferred way to get a pipeline for an aspect. If no pipeline
	/// exists, a default one is created with no dictionaries or detectors.
	/// Use `apply_runtime_config()` to configure the pipeline for execution.
	///
	/// # Errors
	///
	/// Returns an error if database operations fail.
	///
	/// # Example
	///
	/// ```ignore
	/// let mut pipeline = Pipeline::load_or_create(database, &aspect_id).await?;
	///
	/// // Apply runtime configuration
	/// let config = PipelineRunConfig::new()
	///     .with_peak_detector("Price Peaks")
	///     .with_dictionary("primary", "Main patterns", constraints);
	/// pipeline.apply_runtime_config(config).await?;
	///
	/// pipeline.run().await?;
	/// ```
	pub async fn load_or_create(database: Database, aspect_id: &AspectId) -> Result<Self> {
		// Try to load existing pipeline
		if let Some(config) = database.load_pipeline_config(aspect_id).await? {
			// Pipeline exists, load it
			let resolution = database.get_aspect_resolution(aspect_id).await?;
			let state = database.load_pipeline_state(aspect_id).await?.unwrap_or_default();
			let dictionary_names = database.list_pipeline_dictionaries(aspect_id).await?;
			let detector_metadata = database.list_pipeline_detectors(aspect_id).await?;

			if !detector_metadata.is_empty() {
				tracing::debug!(
				    aspect_id = %aspect_id,
				    detector_count = detector_metadata.len(),
				    "Pipeline has {} detector(s) that need re-registration",
				    detector_metadata.len()
				);
			}

			tracing::debug!(
			    aspect_id = %aspect_id,
			    run_count = state.run_count,
			    dictionary_count = dictionary_names.len(),
			    "Loaded existing pipeline"
			);

			return Ok(Self { database, aspect_id: *aspect_id, resolution, config, state, dictionaries: HashMap::new(), dictionary_configs: HashMap::new(), signals: Signals::new(), event_detectors: HashMap::new() });
		}

		// Create new default pipeline
		let resolution = database.get_aspect_resolution(aspect_id).await?;
		let config = DbPipelineConfig::default();
		let state = DbPipelineState::default();

		// Save the default config to database
		database.save_pipeline_config(aspect_id, &config).await?;
		database.save_pipeline_state(aspect_id, &state).await?;

		tracing::info!(
		    aspect_id = %aspect_id,
		    "Created new default pipeline"
		);

		Ok(Self { database, aspect_id: *aspect_id, resolution, config, state, dictionaries: HashMap::new(), dictionary_configs: HashMap::new(), signals: Signals::new(), event_detectors: HashMap::new() })
	}

	/// Applies a runtime configuration to this pipeline.
	///
	/// This allows configuring dictionaries and detectors without rebuilding
	/// the pipeline. Useful for parallel execution of multiple pipelines
	/// with different configurations.
	///
	/// # Errors
	///
	/// Returns an error if database operations fail.
	///
	/// # Example
	///
	/// ```ignore
	/// let config = PipelineRunConfig::new()
	///     .with_peak_detector("Peak Detection")
	///     .with_dictionary("primary", "Main patterns", constraints)
	///     .batch_size(48);
	///
	/// pipeline.apply_runtime_config(config).await?;
	/// ```
	pub async fn apply_runtime_config(&mut self, config: PipelineRunConfig) -> Result<()> {
		// Apply batch size if specified
		if let Some(batch_size) = config.batch_size {
			self.config.batch_size = batch_size;
		}

		// Apply spline method if specified
		if let Some(spline) = config.spline_method {
			self.config.spline_method = spline;
		}

		// Create detectors first (before moving dictionaries)
		let detectors = config.create_detectors();

		// Add dictionaries
		for dict_config in config.dictionaries {
			let dictionary = Dictionary::new(dict_config.name.clone(), dict_config.description.clone(), dict_config.constraints.clone());
			self.dictionaries.insert(dict_config.name.clone(), dictionary);
			self.dictionary_configs.insert(dict_config.name.clone(), dict_config.clone());
			self.database.add_pipeline_dictionary(&self.aspect_id, &dict_config.name).await?;
		}

		// Register detectors
		for detector in detectors {
			self.register_detector(detector).await?;
		}

		// Save updated config
		self.save().await?;

		tracing::debug!(
		    aspect_id = %self.aspect_id,
		    dictionary_count = self.dictionaries.len(),
		    detector_count = self.event_detectors.len(),
		    "Applied runtime configuration"
		);

		Ok(())
	}

	/// Returns true if this pipeline is dormant (no dictionaries or detectors).
	#[must_use]
	pub fn is_dormant(&self) -> bool {
		self.dictionaries.is_empty() && self.event_detectors.is_empty()
	}

	/// Returns true if this pipeline is configured and ready to run.
	#[must_use]
	pub fn is_configured(&self) -> bool {
		!self.is_dormant()
	}

	/// Saves the pipeline configuration and state to the database.
	///
	/// # Errors
	///
	/// Returns an error if database operations fail.
	pub async fn save(&self) -> Result<()> {
		// Save config
		self.database.save_pipeline_config(&self.aspect_id, &self.config).await?;

		// Save state
		self.database.save_pipeline_state(&self.aspect_id, &self.state).await?;

		// Save detector metadata
		for detector in self.event_detectors.values() {
			let metadata = detector.to_metadata();
			self.database.add_pipeline_detector(&self.aspect_id, &metadata).await?;
		}

		tracing::info!(
		    aspect_id = %self.aspect_id,
		    run_count = self.state.run_count,
		    "Pipeline state saved to database"
		);
		Ok(())
	}

	/// Checks if a pipeline exists for the given aspect.
	///
	/// # Errors
	///
	/// Returns an error if database operations fail.
	pub async fn exists(database: &Database, aspect_id: &AspectId) -> Result<bool> {
		let config = database.load_pipeline_config(aspect_id).await?;
		Ok(config.is_some())
	}

	/// Deletes the pipeline configuration and state for this aspect.
	///
	/// This removes the pipeline data from pipeline.db but does not delete
	/// the associated patterns, events, or other data.
	///
	/// # Errors
	///
	/// Returns an error if database operations fail.
	pub async fn delete(&self) -> Result<()> {
		// Use the database method to delete all pipeline data
		self.database.delete_pipeline(&self.aspect_id).await?;

		tracing::info!(
		    aspect_id = %self.aspect_id,
		    "Pipeline deleted from database"
		);
		Ok(())
	}

	/// Runs the full pipeline: batching → processing → patterns → events → correlations → signals.
	///
	/// This is a convenience method that runs all pipeline steps in sequence.
	/// For more control, use the individual step methods.
	///
	/// # Errors
	///
	/// Returns an error if any pipeline step fails.
	pub async fn run(&mut self) -> Result<()> {
		let start_time = std::time::Instant::now();
		tracing::info!(aspect_id = %self.aspect_id, "Starting pipeline run");

		self.prepare_data().await?;
		self.extract_patterns().await?;
		self.detect_events().await?;
		self.correlate_events().await?;
		self.generate_signals().await?;

		// Update state
		self.state.record_run();

		// Auto-save state after successful run
		self.save().await?;

		tracing::info!(
		    elapsed = ?start_time.elapsed(),
		    run_count = self.state.run_count,
		    "Pipeline run completed"
		);
		Ok(())
	}

	/// Runs only the data preparation steps (batching and processing).
	///
	/// Uses incremental batch creation by default, which only creates batches for
	/// new measurements that haven't been processed yet. On first run (when no batches
	/// exist), falls back to full batch creation.
	///
	/// # Errors
	///
	/// Returns an error if database operations fail.
	pub async fn prepare_data(&mut self) -> Result<()> {
		tracing::info!("Preparing data...");
		crate::batch_utils::build_incremental_unprocessed_queue(&self.database, &self.aspect_id, &self.resolution, &self.config.spline_method, self.config.batch_size).await?;

		crate::build_processed_batch_queue(&self.database, &self.aspect_id).await?;
		tracing::info!("Data preparation completed");
		Ok(())
	}

	/// Runs compression on the aspect's raw measurements.
	///
	/// Delegates to `Aspect::compress()` which applies the compression configuration
	/// stored in the Aspect's metadata. Compression is permanent/lossy and replaces
	/// the original measurements with compressed versions.
	///
	/// # Compression Modes
	///
	/// - **Time-based**: Data within a "pure" time range stays uncompressed;
	///   older data gets progressively more compressed based on tiers.
	/// - **Size-based**: Compress to fit within a maximum size (takes precedence).
	///
	/// # Errors
	///
	/// Returns an error if:
	/// - The aspect has no compression config set
	/// - Database operations fail
	///
	/// # Example
	///
	/// ```ignore
	/// // Configure compression on the aspect first
	/// let mut aspect = database.get_aspect(&aspect_id).await?;
	/// aspect.set_compression_config(Some(CompressionConfig::time_based(
	///     TimeBasedCompressionConfig::default_seven_years()
	/// ))).await?;
	///
	/// // Then run compression via pipeline
	/// let mut pipeline = Pipeline::load_or_create(database, &aspect_id).await?;
	/// let summary = pipeline.compress_data().await?;
	/// println!("Compressed {} -> {} measurements", summary.total_original_count, summary.total_compressed_count);
	/// ```
	pub async fn compress_data(&mut self) -> Result<database::compression::CompressionSummary> {
		compress_aspect(&self.database, &self.aspect_id).await
	}

	/// Runs the full pipeline with an optional compression step.
	///
	/// This is a convenience method that runs compression before the standard pipeline
	/// steps if the aspect has a compression configuration.
	///
	/// # Errors
	///
	/// Returns an error if any pipeline step fails.
	pub async fn run_with_compression(&mut self) -> Result<()> {
		let start_time = std::time::Instant::now();
		tracing::info!(aspect_id = %self.aspect_id, "Starting pipeline run with compression");

		// Check if compression is configured
		let aspect = self.database.get_aspect(&self.aspect_id).await?;
		if aspect.compression_config().is_some() {
			self.compress_data().await?;
		} else {
			tracing::debug!("No compression config set, skipping compression step");
		}

		// Run standard pipeline steps
		self.prepare_data().await?;
		self.extract_patterns().await?;
		self.detect_events().await?;
		self.correlate_events().await?;
		self.generate_signals().await?;

		// Update state
		self.state.record_run();
		self.save().await?;

		tracing::info!(
			elapsed = ?start_time.elapsed(),
			run_count = self.state.run_count,
			"Pipeline run with compression completed"
		);
		Ok(())
	}

	/// Runs data preparation with full batch rebuild (ignores incremental processing).
	///
	/// This forces a complete rebuild of all batches, useful when the resolution or
	/// batch size has changed and existing batches are invalid.
	///
	/// # Errors
	///
	/// Returns an error if database operations fail.
	pub async fn prepare_data_full_rebuild(&mut self) -> Result<()> {
		tracing::info!("Preparing data (full rebuild)...");

		// Clear the unbatched queue since we're rebuilding everything
		database::database::traits::Inputs::clear_unbatched_measurements(&self.database, &self.aspect_id).await?;

		// Clear existing batches
		self.database.clear_unprocessed_batches(&self.aspect_id).await?;
		self.database.clear_processed_batches(&self.aspect_id).await?;

		// Rebuild from scratch
		crate::batch_utils::build_unprocessed_queue(&self.database, &self.aspect_id, &self.resolution, &self.config.spline_method, self.config.batch_size).await?;

		crate::build_processed_batch_queue(&self.database, &self.aspect_id).await?;
		tracing::info!("Data preparation (full rebuild) completed");
		Ok(())
	}

	/// Runs pattern extraction into all dictionaries.
	///
	/// # Errors
	///
	/// Returns an error if pattern extraction fails.
	pub async fn extract_patterns(&mut self) -> Result<()> {
		if self.dictionaries.is_empty() {
			tracing::debug!("No dictionaries registered, skipping pattern extraction");
			return Ok(());
		}

		tracing::info!(dictionary_count = self.dictionaries.len(), "Extracting patterns into {} dictionaries...", self.dictionaries.len());

		for (name, dictionary) in &mut self.dictionaries {
			tracing::debug!(dictionary_name = %name, "Loading dictionary");
			crate::load_dictionary(&self.database, &self.aspect_id, dictionary).await?;

			tracing::debug!(dictionary_name = %name, "Building patterns");
			crate::build_patterns_queue(&self.database, &self.aspect_id, dictionary).await?;

			// Reload dictionary to get merged patterns
			crate::load_dictionary(&self.database, &self.aspect_id, dictionary).await?;

			tracing::info!(
			    dictionary_name = %name,
			    pattern_count = dictionary.len(),
			    "Dictionary pattern extraction completed"
			);
		}

		let total_patterns: usize = self.dictionaries.values().map(database::Dictionary::len).sum();
		tracing::info!(total_patterns = total_patterns, "Pattern extraction completed for all dictionaries");
		Ok(())
	}

	/// Runs all registered event detectors.
	///
	/// # Errors
	///
	/// Returns an error if event detection or database operations fail.
	pub async fn detect_events(&self) -> Result<()> {
		if self.event_detectors.is_empty() {
			tracing::debug!("No event detectors registered, skipping event detection");
			return Ok(());
		}

		tracing::info!(detector_count = self.event_detectors.len(), "Running event detectors...");

		for detector in self.event_detectors.values() {
			tracing::debug!(
			    detector_id = %detector.id(),
			    detector_name = %detector.name(),
			    "Running detector"
			);

			let events = detector.detect(&self.database, &self.aspect_id, &self.resolution, &self.config.spline_method).await?;

			for event in events {
				if !event.manifestations().is_empty() {
					self.database.insert_unprocessed_event(&self.aspect_id, &event).await?;
					tracing::info!(
					    detector_id = %detector.id(),
					    event_name = %event.name(),
					    manifestations = event.manifestations().len(),
					    "Stored event from detector"
					);
				}
			}
		}

		tracing::info!("Event detection completed");
		Ok(())
	}

	/// Runs event-pattern correlation for all dictionaries.
	///
	/// # Errors
	///
	/// Returns an error if correlation creation fails.
	pub async fn correlate_events(&self) -> Result<()> {
		if self.dictionaries.is_empty() {
			tracing::debug!("No dictionaries registered, skipping correlation");
			return Ok(());
		}

		tracing::info!("Correlating events with patterns...");

		for (name, dictionary) in &self.dictionaries {
			tracing::debug!(dictionary_name = %name, "Creating correlations");
			crate::create_correlations_for_events(&self.database, dictionary, &self.aspect_id).await?;
		}

		tracing::info!("Event correlation completed");
		Ok(())
	}

	/// Generates and filters signals.
	///
	/// # Errors
	///
	/// Returns an error if signal creation fails.
	pub async fn generate_signals(&mut self) -> Result<()> {
		tracing::info!("Generating signals...");
		crate::create_signals(&self.database, &self.aspect_id).await?;

		// Get latest measurement time for filtering
		let latest_time = self.database.get_latest_measurement(&self.aspect_id).await?;
		crate::filter_expired_signals(&self.database, &self.aspect_id, latest_time).await?;

		// Copy signals from global queue to local
		let signals_lock = SIGNALS_QUEUE.lock().await;
		self.signals = signals_lock.clone();
		let signal_count = self.signals.len();
		drop(signals_lock);

		tracing::info!(signal_count = signal_count, "Signal generation completed");
		Ok(())
	}

	/// Filters expired signals at a specific point in time.
	///
	/// # Errors
	///
	/// Returns an error if filtering fails.
	pub async fn filter_signals_at(&mut self, query_time: DateTime<Utc>) -> Result<()> {
		crate::filter_expired_signals(&self.database, &self.aspect_id, Some(query_time)).await?;

		// Refresh local signals copy
		let signals_lock = SIGNALS_QUEUE.lock().await;
		self.signals = signals_lock.clone();
		drop(signals_lock);

		Ok(())
	}

	/// Calculates probability for a specific event at a given time.
	///
	/// # Errors
	///
	/// Returns an error if probability calculation fails.
	pub async fn query_probability(&self, event_id: &EventID, signal_type: &SignalType, query_time: DateTime<Utc>) -> Result<ProbabilityResult> {
		let sum = self.signals.probability_sum(event_id, signal_type, query_time, &self.database, &self.aspect_id).await?;

		let average = self.signals.probability_average(event_id, signal_type, query_time, &self.database, &self.aspect_id).await?;

		let event_based = self.signals.event_probability(event_id, signal_type, query_time, &self.database, &self.aspect_id).await?;

		Ok(ProbabilityResult { sum, average, event_based })
	}

	// ==================== Dictionary Management ====================

	/// Adds a dictionary to the pipeline.
	///
	/// If a dictionary with the same name already exists, it will be replaced.
	///
	/// # Errors
	///
	/// Returns an error if database operations fail.
	pub async fn add_dictionary(&mut self, config: DictionaryConfig) -> Result<()> {
		let name = config.name.clone();

		// Create the dictionary instance
		let dictionary = Dictionary::new(config.name.clone(), config.description.clone(), config.constraints.clone());

		// Store locally
		self.dictionaries.insert(name.clone(), dictionary);
		self.dictionary_configs.insert(name.clone(), config);

		// Persist to database
		self.database.add_pipeline_dictionary(&self.aspect_id, &name).await?;

		tracing::debug!(dictionary_name = %name, "Added dictionary to pipeline");
		Ok(())
	}

	/// Removes a dictionary from the pipeline by name.
	///
	/// Returns `true` if the dictionary was found and removed.
	///
	/// # Errors
	///
	/// Returns an error if database operations fail.
	pub async fn remove_dictionary(&mut self, name: &str) -> Result<bool> {
		let was_present = self.dictionaries.remove(name).is_some();
		self.dictionary_configs.remove(name);

		if was_present {
			self.database.remove_pipeline_dictionary(&self.aspect_id, name).await?;
			tracing::debug!(dictionary_name = %name, "Removed dictionary from pipeline");
		}

		Ok(was_present)
	}

	/// Returns the names of all registered dictionaries.
	#[must_use]
	pub fn dictionary_names(&self) -> Vec<&str> {
		self.dictionaries.keys().map(std::string::String::as_str).collect()
	}

	/// Returns a reference to a dictionary by name.
	#[must_use]
	pub fn get_dictionary(&self, name: &str) -> Option<&Dictionary> {
		self.dictionaries.get(name)
	}

	/// Returns a mutable reference to a dictionary by name.
	#[must_use]
	pub fn get_dictionary_mut(&mut self, name: &str) -> Option<&mut Dictionary> {
		self.dictionaries.get_mut(name)
	}

	/// Returns the number of registered dictionaries.
	#[must_use]
	pub fn dictionary_count(&self) -> usize {
		self.dictionaries.len()
	}

	// ==================== Event Detector Management ====================

	/// Registers an event detector.
	///
	/// If a detector with the same ID already exists, it will be replaced.
	/// The detector metadata is automatically persisted to the database.
	///
	/// # Errors
	///
	/// Returns an error if database operations fail.
	pub async fn register_detector(&mut self, detector: EventDetector) -> Result<()> {
		let id = detector.id().clone();
		let metadata = detector.to_metadata();

		self.event_detectors.insert(id.clone(), detector);

		// Persist metadata
		self.database.add_pipeline_detector(&self.aspect_id, &metadata).await?;

		tracing::debug!(detector_id = %id, "Registered event detector");
		Ok(())
	}

	/// Registers an event detector without persisting (for use during build).
	fn register_detector_local(&mut self, detector: EventDetector) {
		let id = detector.id().clone();
		self.event_detectors.insert(id.clone(), detector);
		tracing::debug!(detector_id = %id, "Registered event detector (local)");
	}

	/// Removes an event detector by ID.
	///
	/// Returns `true` if the detector was found and removed.
	///
	/// # Errors
	///
	/// Returns an error if database operations fail.
	pub async fn remove_detector(&mut self, id: &DetectorId) -> Result<bool> {
		let removed = self.event_detectors.remove(id).is_some();

		if removed {
			self.database.remove_pipeline_detector(&self.aspect_id, id.as_str()).await?;
			tracing::debug!(detector_id = %id, "Removed event detector");
		}

		Ok(removed)
	}

	/// Returns a list of all registered detector IDs.
	#[must_use]
	pub fn detector_ids(&self) -> Vec<&DetectorId> {
		self.event_detectors.keys().collect()
	}

	/// Returns information about a specific detector.
	#[must_use]
	pub fn get_detector(&self, id: &DetectorId) -> Option<&EventDetector> {
		self.event_detectors.get(id)
	}

	/// Returns the number of registered event detectors.
	#[must_use]
	pub fn detector_count(&self) -> usize {
		self.event_detectors.len()
	}

	/// Clears all registered event detectors.
	///
	/// # Errors
	///
	/// Returns an error if database operations fail.
	pub async fn clear_detectors(&mut self) -> Result<()> {
		let detector_ids: Vec<_> = self.event_detectors.keys().cloned().collect();

		for id in detector_ids {
			self.database.remove_pipeline_detector(&self.aspect_id, id.as_str()).await?;
		}

		self.event_detectors.clear();
		tracing::debug!("Cleared all event detectors");
		Ok(())
	}

	// ==================== Accessors ====================

	/// Returns the resolution (from the Aspect).
	#[must_use]
	pub const fn resolution(&self) -> &Resolution {
		&self.resolution
	}

	/// Returns a reference to the signals.
	#[must_use]
	pub const fn signals(&self) -> &Signals {
		&self.signals
	}

	/// Returns a reference to the pipeline configuration.
	#[must_use]
	pub const fn config(&self) -> &DbPipelineConfig {
		&self.config
	}

	/// Returns the pipeline state.
	#[must_use]
	pub const fn state(&self) -> &DbPipelineState {
		&self.state
	}

	/// Returns the aspect ID this pipeline is associated with.
	#[must_use]
	pub const fn aspect_id(&self) -> &AspectId {
		&self.aspect_id
	}

	/// Returns a reference to the database.
	#[must_use]
	pub const fn database(&self) -> &Database {
		&self.database
	}

	/// Returns all events from the database for this aspect.
	///
	/// # Errors
	///
	/// Returns an error if database operations fail.
	pub async fn get_events(&self) -> Result<Vec<Event>> {
		let events: Vec<Event> = Outputs::get_unprocessed_events(&self.database, &self.aspect_id).await?.try_collect().await?;
		Ok(events)
	}

	// ==================== Backwards Compatibility ====================
	// These methods provide backwards compatibility for single-dictionary usage.

	/// Returns a reference to the first dictionary (for backwards compatibility).
	///
	/// # Panics
	///
	/// Panics if no dictionaries are registered.
	#[must_use]
	pub fn dictionary(&self) -> &Dictionary {
		self.dictionaries.values().next().expect("No dictionaries registered")
	}

	/// Returns a mutable reference to the first dictionary (for backwards compatibility).
	///
	/// # Panics
	///
	/// Panics if no dictionaries are registered.
	#[must_use]
	pub fn dictionary_mut(&mut self) -> &mut Dictionary {
		self.dictionaries.values_mut().next().expect("No dictionaries registered")
	}
}

/// Builder for constructing a Pipeline with custom configuration.
pub struct PipelineBuilder {
	database: Database,
	aspect_id: AspectId,
	spline_method: Spline,
	batch_size: usize,
	dictionary_configs: Vec<DictionaryConfig>,
	event_detectors: Vec<EventDetector>,
}

impl PipelineBuilder {
	const fn new(database: Database, aspect_id: AspectId) -> Self {
		Self { database, aspect_id, spline_method: Spline::Linear, batch_size: 24, dictionary_configs: Vec::new(), event_detectors: Vec::new() }
	}

	/// Sets the spline interpolation method.
	#[must_use]
	pub const fn spline_method(mut self, method: Spline) -> Self {
		self.spline_method = method;
		self
	}

	/// Sets the batch size for processing.
	#[must_use]
	pub const fn batch_size(mut self, size: usize) -> Self {
		self.batch_size = size;
		self
	}

	/// Adds a dictionary configuration.
	#[must_use]
	pub fn add_dictionary(mut self, name: impl Into<String>, description: impl Into<String>, constraints: DictionaryConstraints) -> Self {
		self.dictionary_configs.push(DictionaryConfig::new(name, description, constraints));
		self
	}

	/// Adds a dictionary with the provided configuration.
	#[must_use]
	pub fn with_dictionary(mut self, config: DictionaryConfig) -> Self {
		self.dictionary_configs.push(config);
		self
	}

	/// Registers an event detector with full metadata.
	#[must_use]
	pub fn with_detector(mut self, detector: EventDetector) -> Self {
		self.event_detectors.push(detector);
		self
	}

	/// Registers an event detector function with auto-generated metadata.
	///
	/// # Example
	///
	/// ```ignore
	/// let pipeline = Pipeline::builder(database, aspect_id)
	///     .with_detector_fn("my_detector", "My Detector", event_detector_fn!(my_func))
	///     .build()
	///     .await?;
	/// ```
	#[must_use]
	pub fn with_detector_fn(self, id: impl Into<String>, name: impl Into<String>, detector_fn: EventDetectorFn) -> Self {
		let detector = EventDetector::new(id, name, None, detector_fn);
		self.with_detector(detector)
	}

	/// Registers the built-in monthly price increase detector.
	///
	/// This detector finds months where the price increased by the given
	/// percentage or more from start to end of the month.
	///
	/// # Arguments
	///
	/// * `threshold_percent` - The threshold as a decimal (e.g., 0.05 for 5%)
	#[must_use]
	pub fn with_monthly_increase_detector(self, threshold_percent: f64) -> Self {
		let threshold = threshold_percent;
		let id = format!("monthly_increase_{:.0}pct", threshold_percent * 100.0);
		let name = format!("{:.0}% Monthly Increase Detector", threshold_percent * 100.0);
		let config_json = serde_json::to_string(&serde_json::json!({
		    "threshold_percent": threshold_percent
		}))
		.ok();

		let detector_fn: EventDetectorFn = Arc::new(move |db, aspect, resolution, method| {
			let threshold = threshold;
			Box::pin(async move { crate::detectors::detect_monthly_increase(db, aspect, resolution, method, threshold).await })
		});

		let detector = EventDetector::builtin(id, name, Some(format!("Detects when price increases {:.0}% or more from start to end of month", threshold_percent * 100.0)), "monthly_increase", config_json, detector_fn);

		self.with_detector(detector)
	}

	/// Registers a peak detection event detector.
	///
	/// This detector finds local maxima that equal the global maximum value.
	#[must_use]
	pub fn with_peak_detector(self, name: impl Into<String>) -> Self {
		let name_str = name.into();
		let id = format!("peak_{}", name_str.to_lowercase().replace(' ', "_"));
		let name_clone = name_str.clone();
		let config_json = serde_json::to_string(&serde_json::json!({
		    "event_name": name_str
		}))
		.ok();

		let detector_fn: EventDetectorFn = Arc::new(move |db, aspect, resolution, method| {
			let name = name_clone.clone();
			Box::pin(async move { crate::detectors::detect_peaks(db, aspect, resolution, method, &name).await })
		});

		let detector = EventDetector::builtin(id, name_str.clone(), Some(format!("Detects peak values for {name_str}")), "peak", config_json, detector_fn);

		self.with_detector(detector)
	}

	/// Registers a valley detection event detector.
	///
	/// This detector finds local minima that equal the global minimum value.
	#[must_use]
	pub fn with_valley_detector(self, name: impl Into<String>) -> Self {
		let name_str = name.into();
		let id = format!("valley_{}", name_str.to_lowercase().replace(' ', "_"));
		let name_clone = name_str.clone();
		let config_json = serde_json::to_string(&serde_json::json!({
		    "event_name": name_str
		}))
		.ok();

		let detector_fn: EventDetectorFn = Arc::new(move |db, aspect, resolution, method| {
			let name = name_clone.clone();
			Box::pin(async move { crate::detectors::detect_valleys(db, aspect, resolution, method, &name).await })
		});

		let detector = EventDetector::builtin(id, name_str.clone(), Some(format!("Detects valley (minimum) values for {name_str}")), "valley", config_json, detector_fn);

		self.with_detector(detector)
	}

	/// Builds the pipeline.
	///
	/// # Errors
	///
	/// Returns an error if pipeline initialization fails.
	pub async fn build(self) -> Result<Pipeline> {
		// Get resolution from aspect
		let resolution = self.database.get_aspect_resolution(&self.aspect_id).await?;

		let config = DbPipelineConfig::new(self.spline_method, self.batch_size);
		let state = DbPipelineState::default();

		// Create dictionaries from configs
		let mut dictionaries = HashMap::new();
		let mut dictionary_configs = HashMap::new();

		for dict_config in &self.dictionary_configs {
			let dictionary = Dictionary::new(dict_config.name.clone(), dict_config.description.clone(), dict_config.constraints.clone());
			dictionaries.insert(dict_config.name.clone(), dictionary);
			dictionary_configs.insert(dict_config.name.clone(), dict_config.clone());
		}

		let mut pipeline = Pipeline { database: self.database, aspect_id: self.aspect_id, resolution, config, state, dictionaries, dictionary_configs, signals: Signals::new(), event_detectors: HashMap::new() };

		// Register detectors locally (will be persisted on first save)
		for detector in self.event_detectors {
			pipeline.register_detector_local(detector);
		}

		// Persist initial state
		pipeline.save().await?;

		// Persist dictionary names
		for name in pipeline.dictionaries.keys() {
			pipeline.database.add_pipeline_dictionary(&pipeline.aspect_id, name).await?;
		}

		Ok(pipeline)
	}
}

/// Result of a probability query.
#[derive(Debug, Clone)]
pub struct ProbabilityResult {
	/// Sum of all signal probabilities.
	pub sum: Option<BigDecimal>,
	/// Average of all signal probabilities.
	pub average: Option<BigDecimal>,
	/// Event-based probability (time since last manifestation).
	pub event_based: Option<BigDecimal>,
}

impl ProbabilityResult {
	/// Returns the best available probability estimate.
	///
	/// Prefers event-based probability, then average, then sum.
	#[must_use]
	pub fn best_estimate(&self) -> Option<&BigDecimal> {
		self.event_based.as_ref().or(self.average.as_ref()).or(self.sum.as_ref())
	}

	/// Returns true if all probability values are None.
	#[must_use]
	pub const fn is_empty(&self) -> bool {
		self.sum.is_none() && self.average.is_none() && self.event_based.is_none()
	}
}

impl std::fmt::Display for ProbabilityResult {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let sum_str = self.sum.as_ref().map_or_else(|| "N/A".to_string(), |v| format!("{v:.4}"));
		let avg_str = self.average.as_ref().map_or_else(|| "N/A".to_string(), |v| format!("{v:.4}"));
		let event_str = self.event_based.as_ref().map_or_else(|| "N/A".to_string(), |v| format!("{v:.4}"));

		write!(f, "ProbabilityResult {{ sum: {sum_str}, average: {avg_str}, event_based: {event_str} }}")
	}
}

/// Helper function to compress aspect data.
///
/// This is extracted as a free function to avoid type recursion issues with async
/// methods on complex structs.
async fn compress_aspect(database: &Database, aspect_id: &AspectId) -> Result<database::compression::CompressionSummary> {
	tracing::info!(aspect_id = %aspect_id, "Running compression...");

	let mut aspect = database.get_aspect(aspect_id).await?;
	let summary = aspect.compress(database).await?;

	tracing::info!(
		original = summary.total_original_count,
		compressed = summary.total_compressed_count,
		ratio = %summary.overall_compression_ratio(),
		"Compression completed"
	);

	Ok(summary)
}
