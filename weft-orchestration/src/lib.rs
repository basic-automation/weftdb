// Increase recursion limit for complex async type checking
#![recursion_limit = "512"]

//! # Dataset Management - Time-Series Pattern Recognition and Signal Generation
//!
//! A high-level library for processing time-series data, extracting patterns,
//! detecting events, and generating prediction signals. Built on top of the
//! [`weftdb`] crate.
//!
//! ## Overview
//!
//! This crate provides a complete pipeline for time-series analysis:
//!
//! 1. **Data Preparation**: Batch and process raw measurements
//! 2. **Pattern Extraction**: Identify recurring patterns across multiple dictionaries
//! 3. **Event Detection**: Detect significant occurrences (peaks, valleys, thresholds)
//! 4. **Correlation**: Link patterns to events
//! 5. **Signal Generation**: Create predictions for future events
//!
//! ## Pipeline Architecture
//!
//! Each **Aspect** can have exactly one **Pipeline**, which is persisted in the Aspect's
//! `pipeline.db` database. This ensures:
//!
//! - **No conflicts**: Only one pipeline processes measurements for each aspect
//! - **Automatic persistence**: Configuration and state are saved to the database
//! - **Incremental processing**: Only new measurements are batched on subsequent runs
//!
//! A Pipeline can have **multiple dictionaries** for different pattern matching strategies.
//!
//! ## Quick Start with Pipeline API
//!
//! The [`Pipeline`] API provides a fluent builder pattern for the entire workflow:
//!
//! ```rust,ignore
//! use weft_orchestration::{Pipeline, DictionaryConfig, DictionaryConstraints, Steps, Variability, VariablilityType};
//! use weftdb::{Database, DatabaseStructure, Resolution};
//! use splimes::Spline;
//! use bigdecimal::BigDecimal;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // Open an existing database with measurement data
//!     let database = Database::existing("my_sensor_data").await?;
//!
//!     // Get the aspect to analyze (resolution is set on the Aspect)
//!     let subjects = database.list_subjects().await?;
//!     let subject_id = subjects[0].0;
//!     let aspects = database.get_subject_aspects(&subject_id).await?;
//!     let aspect_id = aspects[0].id();
//!
//!     // Build and configure the pipeline
//!     // Note: Resolution comes from the Aspect, not the Pipeline
//!     let mut pipeline = Pipeline::builder(database.clone(), aspect_id)
//!         // Set processing parameters
//!         .spline_method(Spline::Linear)
//!         .batch_size(24)  // 24-hour batches
//!
//!         // Add one or more dictionaries for pattern extraction
//!         .add_dictionary(
//!             "SensorPatterns",
//!             "Patterns extracted from sensor data",
//!             DictionaryConstraints::new(
//!                 Some(Steps::new(10, Spline::Linear)),
//!                 Some(vec![
//!                     VariablilityType::MaximumStatic(Variability::new(
//!                         BigDecimal::from(1) / BigDecimal::from(10)
//!                     ))
//!                 ])
//!             )
//!         )
//!
//!         // Register event detectors
//!         .with_monthly_increase_detector(0.05)  // 5% monthly increase
//!         .with_peak_detector("Sensor Peaks")
//!
//!         .build()
//!         .await?;
//!
//!     // Run the complete pipeline
//!     pipeline.run().await?;
//!
//!     // Query prediction probability
//!     let events = pipeline.get_events().await?;
//!     if let Some(event) = events.first() {
//!         let result = pipeline.query_probability(
//!             event.id(),
//!             &dataset_management::SignalType::PredictStart,
//!             chrono::Utc::now()
//!         ).await?;
//!
//!         println!("Prediction probability: {}", result);
//!     }
//!
//!     Ok(())
//! }
//! ```
//!
//! ## Step-by-Step Processing
//!
//! For more control, you can run individual pipeline steps:
//!
//! ```rust,ignore
//! use dataset_management::Pipeline;
//!
//! // Build the pipeline with multiple dictionaries
//! let mut pipeline = Pipeline::builder(database, aspect_id)
//!     .add_dictionary("coarse", "Coarse patterns", coarse_constraints)
//!     .add_dictionary("fine", "Fine-grained patterns", fine_constraints)
//!     .with_peak_detector("My Peaks")
//!     .build()
//!     .await?;
//!
//! // Run steps individually
//! pipeline.prepare_data().await?;      // Batch and process measurements (incremental)
//! pipeline.extract_patterns().await?;  // Extract patterns into ALL dictionaries
//! pipeline.detect_events().await?;     // Run registered event detectors
//! pipeline.correlate_events().await?;  // Create pattern-event correlations
//! pipeline.generate_signals().await?;  // Generate prediction signals
//!
//! // Access results from a specific dictionary
//! if let Some(dict) = pipeline.get_dictionary("coarse") {
//!     println!("Coarse patterns found: {}", dict.len());
//! }
//! println!("Total dictionaries: {}", pipeline.dictionary_count());
//! println!("Signals generated: {}", pipeline.signals().len());
//! ```
//!
//! ## Custom Event Detectors
//!
//! Create custom event detectors for domain-specific patterns:
//!
//! ```rust,ignore
//! use dataset_management::{Pipeline, EventDetector, Event, event_detector_fn};
//! use weftdb::{Database, AspectId, Resolution};
//! use splimes::Spline;
//! use std::sync::Arc;
//!
//! // Define a custom detector function
//! async fn detect_anomalies(
//!     db: &Database,
//!     aspect: &AspectId,
//!     resolution: &Resolution,
//!     method: &Spline,
//! ) -> anyhow::Result<Vec<Event>> {
//!     // Your custom detection logic here
//!     // Access data via db.analyze_range(), db.get_raw_measurements(), etc.
//!     Ok(vec![])
//! }
//!
//! // Register with the pipeline using the macro
//! let mut pipeline = Pipeline::builder(database, aspect_id)
//!     .with_detector(EventDetector::new(
//!         "anomaly_detector",
//!         "Anomaly Detection",
//!         Some("Detects unusual patterns in the data".to_string()),
//!         event_detector_fn!(detect_anomalies),
//!     ))
//!     .build()
//!     .await?;
//!
//! // Or register after building (async method)
//! pipeline.register_detector(EventDetector::new(
//!     "another_detector",
//!     "Another Detector",
//!     None,
//!     Arc::new(|db, aspect, res, method| {
//!         Box::pin(async move { Ok(vec![]) })
//!     }),
//! )).await?;
//! ```
//!
//! ## Built-in Event Detectors
//!
//! The crate provides several ready-to-use detectors in the [`detectors`] module:
//!
//! | Function | Description |
//! |----------|-------------|
//! | [`detect_monthly_increase`] | Detects months with price increase above threshold |
//! | [`detect_peaks`] | Finds local maxima equal to global maximum |
//! | [`detect_all_peaks`] | Finds all local maxima |
//! | [`detect_valleys`] | Finds local minima equal to global minimum |
//! | [`detect_all_valleys`] | Finds all local minima |
//! | [`detect_threshold_crossing_up`] | Detects upward threshold crossings |
//! | [`detect_threshold_crossing_down`] | Detects downward threshold crossings |
//! | [`detect_drawdown`] | Detects significant price drops |
//!
//! ## Pipeline Persistence
//!
//! Pipelines are automatically persisted to the Aspect's `pipeline.db` database.
//! Configuration, state, dictionary names, and detector metadata are all stored.
//!
//! **Important**: Detector *functions* cannot be serialized. After loading a pipeline,
//! you must re-register any custom detectors. Builtin detectors store their type and
//! configuration for potential future auto-reconstruction.
//!
//! ```rust,ignore
//! use dataset_management::Pipeline;
//!
//! // Check if a pipeline exists for this aspect
//! if Pipeline::exists(&database, &aspect_id).await? {
//!     // Load existing pipeline from database
//!     let mut pipeline = Pipeline::load(database.clone(), &aspect_id).await?;
//!
//!     // Re-register custom detectors (functions can't be serialized)
//!     // Builtin detectors will show a warning about needing re-registration
//!     pipeline.register_detector(/* ... */).await?;
//!
//!     // Continue from where we left off (incremental processing)
//!     pipeline.run().await?;
//! } else {
//!     // Create new pipeline
//!     let mut pipeline = Pipeline::builder(database, aspect_id)
//!         .add_dictionary("patterns", "My patterns", constraints)
//!         .build()
//!         .await?;
//!     pipeline.run().await?;
//!     // State is auto-saved to pipeline.db after run()
//! }
//!
//! // Force a full rebuild (ignores incremental processing)
//! pipeline.prepare_data_full_rebuild().await?;
//! ```
//!
//! ## Multiple Dictionaries
//!
//! A Pipeline can manage multiple dictionaries with different constraints:
//!
//! ```rust,ignore
//! use dataset_management::{Pipeline, DictionaryConfig, DictionaryConstraints};
//!
//! let mut pipeline = Pipeline::builder(database, aspect_id)
//!     // Add dictionaries at build time
//!     .add_dictionary("hourly", "Hourly patterns", hourly_constraints)
//!     .add_dictionary("daily", "Daily patterns", daily_constraints)
//!     .build()
//!     .await?;
//!
//! // Or add dictionaries later
//! pipeline.add_dictionary(DictionaryConfig::new(
//!     "weekly",
//!     "Weekly patterns",
//!     weekly_constraints
//! )).await?;
//!
//! // Access specific dictionaries
//! let hourly = pipeline.get_dictionary("hourly");
//! let daily = pipeline.get_dictionary_mut("daily");
//!
//! // List all dictionary names
//! for name in pipeline.dictionary_names() {
//!     println!("Dictionary: {}", name);
//! }
//!
//! // Remove a dictionary
//! pipeline.remove_dictionary("weekly").await?;
//! ```
//!
//! ## Lower-Level Functions
//!
//! For even more control, use the individual functions directly:
//!
//! ```rust,ignore
//! use dataset_management::{
//!     build_processed_batch_queue,
//!     build_patterns_queue,
//!     load_dictionary,
//!     create_correlations_for_events,
//!     create_signals,
//!     filter_expired_signals,
//!     Dictionary, DictionaryConstraints,
//! };
//! use dataset_management::batch_utils::build_unprocessed_queue;
//!
//! // Manual step-by-step processing
//! build_unprocessed_queue(&database, &aspect_id, &resolution, &method, batch_size).await?;
//! build_processed_batch_queue(&database, &aspect_id).await?;
//!
//! let mut dictionary = Dictionary::new(
//!     "MyDictionary".to_string(),
//!     "Description".to_string(),
//!     DictionaryConstraints::default()
//! );
//! load_dictionary(&database, &aspect_id, &mut dictionary).await?;
//! build_patterns_queue(&database, &aspect_id, &mut dictionary).await?;
//!
//! create_correlations_for_events(&database, &dictionary, &aspect_id).await?;
//! create_signals(&database, &aspect_id).await?;
//! filter_expired_signals(&database, &aspect_id, Some(chrono::Utc::now())).await?;
//! ```
//!
//! ## Key Types
//!
//! ### Pipeline Types
//!
//! | Type | Description |
//! |------|-------------|
//! | [`Pipeline`] | Main pipeline orchestrator |
//! | [`PipelineBuilder`] | Fluent builder for pipeline configuration |
//! | [`DictionaryConfig`] | Configuration for adding dictionaries |
//! | [`EventDetector`] | Registered event detector with metadata |
//! | [`EventDetectorFn`] | Type alias for detector function |
//! | [`DetectorId`] | Unique identifier for detectors |
//!
//! ### Pipeline Persistence Types (from `database`)
//!
//! | Type | Description |
//! |------|-------------|
//! | [`PipelineConfig`](weftdb::PipelineConfig) | Stored pipeline settings (spline method, batch size) |
//! | [`PipelineState`](weftdb::PipelineState) | Run state (last run, count, version) |
//! | [`DetectorMetadata`](weftdb::DetectorMetadata) | Persisted detector info (type, config JSON) |
//! | [`DetectorType`](weftdb::DetectorType) | Enum: Builtin, Custom, or Script |
//!
//! ### Data Types (re-exported from `database`)
//!
//! | Type | Description |
//! |------|-------------|
//! | [`Dictionary`] | Collection of patterns with constraints |
//! | [`DictionaryConstraints`] | Rules for pattern matching |
//! | [`Pattern`] | Extracted recurring shape |
//! | [`Event`] | Detected occurrence with manifestations |
//! | [`Correlation`] | Link between pattern and event |
//! | [`Signal`] | Prediction based on correlation |
//! | [`SignalType`] | Type of prediction signal |
//!
//! ## Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────┐
//! │                    Pipeline (per Aspect)                         │
//! │              Persisted to: <aspect>/pipeline.db                  │
//! ├──────────────────────────────────────────────────────────────────┤
//! │                                                                  │
//! │  ┌─────────────┐   ┌─────────────┐   ┌─────────────────────────┐ │
//! │  │ Measurements│ → │   Batches   │ → │ Processed Batches       │ │
//! │  │  (aspect)   │   │(incremental)│   │  (only affected ones)   │ │
//! │  └─────────────┘   └─────────────┘   └─────────────────────────┘ │
//! │                                              ↓                   │
//! │  ┌─────────────────────────────────────────────────────────────┐ │
//! │  │                    Dictionaries (multiple)                  │ │
//! │  │  ┌───────────┐  ┌───────────┐  ┌───────────┐                │ │
//! │  │  │  hourly   │  │   daily   │  │  weekly   │  ...           │ │
//! │  │  │ (patterns)│  │ (patterns)│  │ (patterns)│                │ │
//! │  │  └───────────┘  └───────────┘  └───────────┘                │ │
//! │  └─────────────────────────────────────────────────────────────┘ │
//! │                              ↓                                   │
//! │  ┌─────────────────────────────────────────────────────────────┐ │
//! │  │                    Event Detectors                          │ │
//! │  │  ┌────────────┐  ┌────────────┐  ┌────────────┐             │ │
//! │  │  │Monthly Incr│  │Peak Detect │  │Custom Det. │  ...        │ │
//! │  │  └────────────┘  └────────────┘  └────────────┘             │ │
//! │  └─────────────────────────────────────────────────────────────┘ │
//! │                              ↓                                   │
//! │  ┌─────────────┐   ┌─────────────┐   ┌─────────────────────────┐ │
//! │  │   Events    │ → │Correlations │ → │       Signals           │ │
//! │  └─────────────┘   └─────────────┘   └─────────────────────────┘ │
//! │                                              ↓                   │
//! │  ┌─────────────────────────────────────────────────────────────┐ │
//! │  │              query_probability(event, time)                 │ │
//! │  │                         → ProbabilityResult                 │ │
//! │  └─────────────────────────────────────────────────────────────┘ │
//! └──────────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Performance Tips
//!
//! - Use appropriate batch sizes (typically 24-100 for hourly data)
//! - Set dictionary constraints to control pattern granularity
//! - Use `SKIP_SLOW_TESTS=1` environment variable to skip long-running tests
//! - Parallel processing is automatic via rayon for CPU-bound operations
//! - Memory-aware batching prevents exhaustion during large dataset processing
//!
//! ## Example: Financial Data Analysis
//!
//! ```rust,ignore
//! use dataset_management::{Pipeline, DictionaryConstraints, Steps, Variability, VariablilityType};
//! use weftdb::{Database, DatabaseStructure, Resolution};
//! use splimes::Spline;
//! use bigdecimal::BigDecimal;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let database = Database::existing("Crypto").await?;
//!
//!     // Find the pump-station-3 subject
//!     let subjects = database.list_subjects().await?;
//!     let subject = subjects.iter()
//!         .find(|(_, name)| name == "pump-station-3")
//!         .map(|(id, _)| *id)
//!         .expect("pump-station-3 not found");
//!
//!     // Get the "open" price aspect
//!     let aspects = database.get_subject_aspects(&subject).await?;
//!     let open_aspect = aspects.iter()
//!         .find(|a| a.name() == "open")
//!         .expect("open aspect not found");
//!
//!     // Build a pipeline for pressure-series analysis
//!     let mut pipeline = Pipeline::builder(database.clone(), open_aspect.id())
//!         .resolution(Resolution::Hours)
//!         .batch_size(24)  // Daily patterns
//!         .dictionary_constraints(DictionaryConstraints::new(
//!             Some(Steps::new(10, Spline::Linear)),
//!             Some(vec![VariablilityType::MaximumStatic(
//!                 Variability::new(BigDecimal::from(1) / BigDecimal::from(10))
//!             )])
//!         ))
//!         .with_monthly_increase_detector(0.05)
//!         .with_peak_detector("Pressure Peaks")
//!         .build()
//!         .await?;
//!
//!     // Run analysis
//!     pipeline.run().await?;
//!
//!     println!("Patterns found: {}", pipeline.dictionary().len());
//!     println!("Signals generated: {}", pipeline.signals().len());
//!
//!     Ok(())
//! }
//! ```

#![warn(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions, clippy::used_underscore_binding, clippy::similar_names, clippy::module_name_repetitions, clippy::module_inception, clippy::cast_precision_loss)]

use std::{collections::HashMap, sync::LazyLock};

use anyhow::Result;
use bigdecimal::{BigDecimal, FromPrimitive, Zero};
use chrono::Datelike;
use futures::{StreamExt, TryStreamExt};
use rayon::prelude::*;
use splimes::Spline;
use tokio::sync::Mutex;
pub use types::*;
use weftdb::{
	database::traits::{AspectStructure, DatabaseStructure, Inputs, Outputs}, AspectId, Database, DictionaryId, Resolution
};

pub mod batch_utils;
pub mod detectors;
pub mod pipeline;
pub mod types;

#[cfg(test)]
mod debug_batch_test;
#[cfg(test)]
mod memory_test;
#[cfg(test)]
mod pattern_fix_test;

// Re-export main Pipeline API
// Re-export built-in detectors
// Re-export compression types for convenience
pub use detectors::{detect_all_peaks, detect_all_valleys, detect_drawdown, detect_monthly_increase, detect_peaks, detect_threshold_crossing_down, detect_threshold_crossing_up, detect_valleys};
pub use pipeline::{DetectorId, DictionaryConfig, EventDetector, EventDetectorFn, Pipeline, PipelineBuilder, PipelineRunConfig, ProbabilityResult};
pub use weftdb::compression::{AggressivenessScaling, CompressionConfig, CompressionResult, CompressionSummary, SizeBasedCompressionConfig, TimeBasedCompressionConfig};

/// Result of running a single pipeline.
#[derive(Debug)]
pub struct PipelineRunResult {
	/// The aspect ID that was processed.
	pub aspect_id: AspectId,
	/// The pipeline state after running (run count, last run).
	pub run_count: u64,
	/// Number of signals generated.
	pub signal_count: usize,
	/// Time taken to run the pipeline.
	pub elapsed: std::time::Duration,
	/// Error message if the pipeline failed.
	pub error: Option<String>,
}

impl PipelineRunResult {
	/// Returns true if the pipeline run was successful.
	#[must_use]
	pub const fn is_success(&self) -> bool {
		self.error.is_none()
	}
}

/// Runs pipelines for multiple aspects in parallel.
///
/// This function is the recommended way to run pipelines across multiple aspects.
/// It loads or creates pipelines for each aspect, applies the provided runtime
/// configurations, and executes them in parallel.
///
/// # Arguments
///
/// * `database` - The database containing the aspects
/// * `configs` - A map of aspect IDs to runtime configurations. Aspects not in this
///   map will be skipped.
///
/// # Returns
///
/// A vector of results, one for each aspect that was processed.
///
/// # Errors
///
/// Individual pipeline failures are captured in the `PipelineRunResult::error` field.
/// This function only returns an error if it fails to list aspects.
///
/// # Example
///
/// ```ignore
/// use weft_orchestration::{run_all_pipelines, PipelineRunConfig};
/// use weftdb::{Database, AspectId};
/// use std::collections::HashMap;
///
/// let database = Database::existing("MyData").await?;
///
/// // Create configurations for each aspect you want to run
/// let mut configs = HashMap::new();
///
/// // Get all aspects for a subject
/// let aspects = database.get_subject_aspects(&subject_id).await?;
/// for aspect in aspects {
///     let config = PipelineRunConfig::new()
///         .with_peak_detector(format!("{} Peaks", aspect.name()))
///         .with_dictionary("patterns", "Main patterns", constraints.clone());
///     configs.insert(aspect.id(), config);
/// }
///
/// // Run all pipelines in parallel
/// let results = run_all_pipelines(&database, configs).await?;
///
/// for result in results {
///     if result.is_success() {
///         println!("Aspect {} completed: {} signals", result.aspect_id, result.signal_count);
///     } else {
///         println!("Aspect {} failed: {}", result.aspect_id, result.error.unwrap());
///     }
/// }
/// ```
pub async fn run_all_pipelines<S: ::std::hash::BuildHasher>(database: &Database, configs: HashMap<AspectId, PipelineRunConfig, S>) -> Result<Vec<PipelineRunResult>> {
	if configs.is_empty() {
		return Ok(Vec::new());
	}

	tracing::info!(aspect_count = configs.len(), "Running pipelines for {} aspects in parallel", configs.len());

	// Create futures for each pipeline
	let futures: Vec<_> = configs
		.into_iter()
		.map(|(aspect_id, config)| {
			let db = database.clone();
			async move {
				let start = std::time::Instant::now();

				// Load or create pipeline
				let mut pipeline = match Pipeline::load_or_create(db, &aspect_id).await {
					Ok(p) => p,
					Err(e) => {
						return PipelineRunResult { aspect_id, run_count: 0, signal_count: 0, elapsed: start.elapsed(), error: Some(format!("Failed to load/create pipeline: {e}")) };
					}
				};

				// Apply runtime configuration if not empty
				if !config.is_empty() {
					if let Err(e) = pipeline.apply_runtime_config(config).await {
						return PipelineRunResult { aspect_id, run_count: 0, signal_count: 0, elapsed: start.elapsed(), error: Some(format!("Failed to apply config: {e}")) };
					}
				}

				// Skip dormant pipelines
				if pipeline.is_dormant() {
					tracing::debug!(aspect_id = %aspect_id, "Skipping dormant pipeline");
					return PipelineRunResult { aspect_id, run_count: 0, signal_count: 0, elapsed: start.elapsed(), error: Some("Pipeline is dormant (no dictionaries or detectors)".to_string()) };
				}

				// Run the pipeline
				match pipeline.run().await {
					Ok(()) => {
						let signal_count = pipeline.signals().len();
						let run_count = pipeline.state().run_count;

						tracing::info!(
						    aspect_id = %aspect_id,
						    run_count = run_count,
						    signal_count = signal_count,
						    elapsed = ?start.elapsed(),
						    "Pipeline completed successfully"
						);

						PipelineRunResult { aspect_id, run_count, signal_count, elapsed: start.elapsed(), error: None }
					}
					Err(e) => {
						tracing::error!(
						    aspect_id = %aspect_id,
						    error = %e,
						    "Pipeline failed"
						);
						PipelineRunResult { aspect_id, run_count: 0, signal_count: 0, elapsed: start.elapsed(), error: Some(format!("Pipeline execution failed: {e}")) }
					}
				}
			}
		})
		.collect();

	// Run all futures in parallel
	let results = futures::future::join_all(futures).await;

	// Summary logging
	let successful = results.iter().filter(|r| r.is_success()).count();
	let failed = results.len() - successful;
	let total_signals: usize = results.iter().map(|r| r.signal_count).sum();

	tracing::info!(successful = successful, failed = failed, total_signals = total_signals, "Parallel pipeline execution completed");

	Ok(results)
}

/// Runs pipelines for all aspects under a subject in parallel.
///
/// This is a convenience wrapper around `run_all_pipelines` that automatically
/// discovers all aspects for a subject.
///
/// # Arguments
///
/// * `database` - The database containing the subject
/// * `subject_id` - The subject whose aspects should be processed
/// * `config_fn` - A function that creates a `PipelineRunConfig` for each aspect
///
/// # Errors
///
/// Returns an error if listing aspects fails. Individual pipeline failures
/// are captured in the `PipelineRunResult::error` field.
///
/// # Example
///
/// ```ignore
/// use weft_orchestration::{run_subject_pipelines, PipelineRunConfig};
///
/// let results = run_subject_pipelines(&database, &subject_id, |aspect| {
///     PipelineRunConfig::new()
///         .with_peak_detector(format!("{} Peaks", aspect.name()))
///         .batch_size(24)
/// }).await?;
/// ```
pub async fn run_subject_pipelines<F>(database: &Database, subject_id: &weftdb::SubjectId, config_fn: F) -> Result<Vec<PipelineRunResult>>
where
	F: Fn(&weftdb::Aspect) -> PipelineRunConfig,
{
	let aspects = database.get_subject_aspects(subject_id).await?;

	let configs: HashMap<AspectId, PipelineRunConfig> = aspects.iter().map(|aspect| (aspect.id(), config_fn(aspect))).collect();

	run_all_pipelines(database, configs).await
}

pub const BATCH_SIZE: [usize; 1] = [100];
pub static DEFAULT_ERROR_RATE: LazyLock<weftdb::Distance> = LazyLock::new(|| {
	weftdb::Distance::new(
		BigDecimal::zero(),
		splimes::Resolution::Seconds, // Use seconds as the canonical unit for error rates
	)
});

static SIGNALS_QUEUE: LazyLock<Mutex<Signals>> = LazyLock::new(|| Mutex::new(Signals::new()));

type ExpiredSignal = (weftdb::CorrelationID, weftdb::ManifestationId, SignalType, chrono::DateTime<chrono::Utc>);

fn collect_expired_signals(signals: &Signals, events: &[weftdb::Event], correlations: &[weftdb::Correlation], current_time: chrono::DateTime<chrono::Utc>) -> (Vec<ExpiredSignal>, usize, usize) {
	let mut signals_to_remove = Vec::new();
	let mut expired_historical = 0usize;
	let mut expired_forward = 0usize;

	for signal in signals.values() {
		let mut signal_event_id = None;
		for correlation in correlations {
			if correlation.id() == signal.correlation_id() {
				signal_event_id = Some(correlation.event_id().clone());
				break;
			}
		}

		if let Some(event_id) = signal_event_id {
			if let Some(event) = events.iter().find(|e| *e.id() == event_id) {
				if let Some((_, predicted_manifestation)) = event.manifestations().iter().find(|(_, m)| m.id() == signal.manifestation_id()) {
					let prediction_point = match signal.signal_type() {
						SignalType::PredictStart | SignalType::PredictMid => predicted_manifestation.midpoint(),
						SignalType::PredictEnd => *predicted_manifestation.end(),
					};

					if current_time >= prediction_point {
						signals_to_remove.push((signal.correlation_id().clone(), signal.manifestation_id().clone(), signal.signal_type().clone(), prediction_point));
						expired_historical += 1;
					}
				} else if let Some(correlation) = correlations.iter().find(|c| c.id() == signal.correlation_id()) {
					if let Some(avg_dist) = correlation.average_distance() {
						use bigdecimal::ToPrimitive;
						if let Some(distance_minutes) = avg_dist.value().to_i64() {
							let duration = match avg_dist.units() {
								Resolution::Hours => chrono::Duration::hours(distance_minutes),
								Resolution::Days => chrono::Duration::days(distance_minutes),
								Resolution::Seconds => chrono::Duration::seconds(distance_minutes),
								_ => chrono::Duration::minutes(distance_minutes),
							};
							let predicted_time = *signal.manifestation_date() + duration;
							if current_time >= predicted_time {
								signals_to_remove.push((signal.correlation_id().clone(), signal.manifestation_id().clone(), signal.signal_type().clone(), predicted_time));
								expired_forward += 1;
							}
						}
					}
				}
			}
		}
	}

	(signals_to_remove, expired_historical, expired_forward)
}

/// Builds a processed batch queue from unprocessed batches in the database.
///
/// This function retrieves unprocessed batches, processes them in parallel,
/// and marks them as processed in the database.
///
/// # Errors
///
/// Returns an error if:
/// - Database operations fail (getting batches, marking as processed)
/// - Batch processing fails
pub async fn build_processed_batch_queue(database: &Database, aspect_id: &weftdb::AspectId) -> Result<()> {
	use std::time::Instant;

	let mut stream = database.get_unprocessed_batches(aspect_id).await?;

	// Process batches in chunks to avoid loading all into memory
	let chunk_size = 1000;
	let mut processed_count = 0;
	let mut total_cpu_time = std::time::Duration::ZERO;
	let mut total_db_time = std::time::Duration::ZERO;

	loop {
		let mut chunk: Vec<Batch> = stream.by_ref().take(chunk_size).try_collect().await?;
		if chunk.is_empty() {
			break;
		}

		tracing::debug!(chunk_size = chunk.len(), "Processing chunk of batches");

		// Process batches in parallel (batch.process() is synchronous)
		let cpu_start = Instant::now();
		chunk.par_iter_mut().for_each(|batch| {
			let _ = batch.process();
		});
		let cpu_elapsed = cpu_start.elapsed();
		total_cpu_time += cpu_elapsed;

		// Use bulk operation: single transaction for INSERT + single transaction for DELETE
		// This replaces 2000 individual transactions with 2 bulk transactions
		let db_start = Instant::now();
		database.move_batches_to_processed(aspect_id, &chunk).await?;
		let db_elapsed = db_start.elapsed();
		total_db_time += db_elapsed;

		processed_count += chunk.len();
		tracing::info!(processed_count = processed_count, cpu_ms = cpu_elapsed.as_millis(), db_ms = db_elapsed.as_millis(), "Processed batches chunk");
	}

	tracing::info!(total_cpu_secs = total_cpu_time.as_secs_f64(), total_db_secs = total_db_time.as_secs_f64(), "Batch processing completed");
	Ok(())
}

/// Builds a patterns queue from processed batches in the database.
///
/// This function extracts patterns from processed batches and imports them into the dictionary
/// with merge logic applied before storing to the database.
///
/// # Errors
///
/// Returns an error if:
/// - Database operations fail (getting batches, storing patterns, dequeuing batches)
/// - Pattern creation fails due to invalid batch data
/// - Dictionary import fails during pattern merging
///
/// # Panics
///
/// This function will panic if active measurements exist in a batch but have no timestamps,
/// which should not happen under normal circumstances.
pub async fn build_patterns_queue(database: &Database, aspect_id: &weftdb::AspectId, dictionary: &mut Dictionary) -> Result<()> {
	let mut stream = database.get_processed_batches(aspect_id).await?;

	// Process batches in chunks to avoid loading all into memory
	let chunk_size = 1000;
	let mut processed_batch_ids = Vec::new();
	let mut processed_count = 0;

	loop {
		let chunk: Vec<Batch> = stream.by_ref().take(chunk_size).try_collect().await?;
		if chunk.is_empty() {
			break;
		}

		tracing::debug!(chunk_size = chunk.len(), "Processing chunk of batches");

		// Process each batch in the chunk
		for batch in &chunk {
			processed_count += 1;

			// Progress reporting every 1000 batches
			if processed_count % 1000 == 0 {
				tracing::debug!(processed_count = processed_count, "Processed batches into patterns");
			}

			// Generate a new pattern ID for this batch
			let pattern_id = PatternID::new();

			// Get the first and last timestamps from active measurements
			let active_measurements: Vec<_> = batch.measurements().iter().filter(|m| m.is_active()).collect();

			if active_measurements.is_empty() {
				continue; // Skip batches with no active measurements
			}

			let beginning = active_measurements.iter().map(|m| m.get_measurement_timestamp()).min().copied().unwrap();
			let end = active_measurements.iter().map(|m| m.get_measurement_timestamp()).max().copied().unwrap();

			// Create an occurrence for this pattern
			let occurrence = Occurrence::new(batch.metadata.aspect, batch.metadata.resolution, batch.metadata.size, batch.metadata.database_info.clone(), pattern_id, beginning, end);

			// Extract relatives from active measurements that have analysis, or generate from vectors
			let relatives: Vec<weftdb::Relative> = active_measurements.iter().find_map(|measurement| measurement.analysis()?.relative()).map_or_else(
				|| {
					// If no analysis data, generate relatives from measurement vectors
					let vectors: Vec<_> = active_measurements.iter().filter_map(|measurement| measurement.vector()).collect();
					if vectors.is_empty() {
						Vec::new()
					} else {
						// Calculate max_x and max_y from all vectors
						let max_x = vectors.iter().map(|v| v.location()).max().cloned().unwrap_or_else(|| BigDecimal::from(0));
						let max_y = vectors.iter().map(|v| v.amplitude()).max().cloned().unwrap_or_else(|| BigDecimal::from(0));

						// Create relatives from vectors
						vectors.iter().map(|vector| weftdb::Relative::new((*vector).clone(), max_x.clone(), max_y.clone())).collect()
					}
				},
				|_first_relative| active_measurements.iter().filter_map(|measurement| measurement.analysis()?.relative().cloned()).collect(),
			);

			// Only create pattern if we have relatives
			if !relatives.is_empty() {
				let pattern = Pattern::new(pattern_id, vec![occurrence], relatives);

				// Import pattern into dictionary (this applies merge logic)
				dictionary.import_pattern(pattern)?;
			}
		}

		// Collect batch IDs for later dequeuing (only for successfully processed batches)
		processed_batch_ids.extend(chunk.into_iter().map(|batch| *batch.batch_id()));
	}

	tracing::info!(processed_count = processed_count, "Batch processing into patterns completed");

	// Get all patterns from dictionary after merging
	let merged_patterns: Vec<Pattern> = dictionary.patterns().to_vec();
	if !merged_patterns.is_empty() {
		// Store patterns in the specified dictionary - handle case where dictionary schema doesn't exist
		match database.batch_insert_patterns_into_dictionary(aspect_id, dictionary.name(), merged_patterns.clone()).await {
			Ok(_tx_ids) => {
				tracing::info!(pattern_count = merged_patterns.len(), dictionary = dictionary.name(), batch_count = processed_count, "Stored patterns in database dictionary");
			}
			Err(e) => {
				tracing::warn!(error = %e, dictionary = dictionary.name(), "Failed to store patterns in database dictionary");
				tracing::warn!(pattern_count = merged_patterns.len(), "Patterns are still available in memory dictionary");
			}
		}
	}

	// Remove processed batches from database queue using bulk operation
	// This is ~1000x faster than individual deletes for large batch counts
	if !processed_batch_ids.is_empty() {
		tracing::debug!(batch_count = processed_batch_ids.len(), "Removing processed batches from queue");
		database.bulk_remove_processed_batches(aspect_id, &processed_batch_ids).await?;
		tracing::debug!(batch_count = processed_batch_ids.len(), "Removed processed batches from queue");
	}

	Ok(())
}

/// Optimized dictionary loading with memory-aware parallelism using rayon
/// Dynamically calculates batch sizes based on available memory and removes patterns as processed
///
/// # Errors
///
/// Returns an error if:
/// - Database operations fail (getting patterns, dequeuing patterns)
/// - Pattern import fails during dictionary loading
/// - Memory calculation fails
pub async fn load_dictionary(database: &Database, aspect_id: &weftdb::AspectId, dictionary: &mut Dictionary) -> Result<()> {
	// Check if dictionary metadata exists, create it if not
	match database.get_dictionary_metadata(aspect_id, dictionary.name()).await {
		Ok(Some(_)) => {
			// Dictionary metadata exists, proceed normally
		}
		Ok(None) => {
			// Dictionary metadata doesn't exist, create it
			let metadata = weftdb::DictionaryMetadata { id: DictionaryId::new(), name: dictionary.name().to_string(), description: dictionary.description().to_string(), constraints: dictionary.constraints().clone() };

			// Store dictionary metadata
			database.set_dictionary_metadata(aspect_id, dictionary.name(), &metadata).await?;
		}
		Err(e) => {
			// If dictionary metadata retrieval fails, try to create it anyway
			tracing::warn!(error = %e, "Failed to check dictionary metadata, attempting to create");
			let metadata = weftdb::DictionaryMetadata { id: DictionaryId::new(), name: dictionary.name().to_string(), description: dictionary.description().to_string(), constraints: dictionary.constraints().clone() };

			// Try to store dictionary metadata - if this fails, the database might not support it yet
			if let Err(store_err) = database.set_dictionary_metadata(aspect_id, dictionary.name(), &metadata).await {
				tracing::warn!(error = %store_err, "Failed to store dictionary metadata");
				tracing::warn!("Continuing without dictionary metadata (dictionary will still function)");
			}
		}
	}

	// Get processed patterns from the specific dictionary - handle case where dictionary doesn't exist
	let patterns: Vec<weftdb::Pattern> = match database.get_dictionary_patterns(aspect_id, dictionary.name()).await {
		Ok(stream) => stream.try_collect().await.unwrap_or_else(|e| {
			tracing::warn!(error = %e, dictionary = dictionary.name(), "Failed to collect patterns from dictionary");
			Vec::new()
		}),
		Err(e) => {
			tracing::warn!(error = %e, dictionary = dictionary.name(), "Failed to get patterns from dictionary");
			tracing::warn!("Dictionary may not exist yet - returning empty pattern list");
			Vec::new()
		}
	};
	let pattern_count = patterns.len();

	let start_time = std::time::Instant::now();
	tracing::info!(pattern_count = pattern_count, "Loading patterns into dictionary with memory-aware batching");

	// For smaller pattern sets, process directly from the queue
	if pattern_count <= 1000 {
		for pattern in &patterns {
			dictionary.import_pattern(pattern.clone())?;
		}
	} else {
		// Get available memory information
		let available_memory_mb = get_available_memory_mb();
		tracing::debug!(available_memory_mb = available_memory_mb, "Available memory");

		// Calculate safe batch size based on available memory
		// Assume each pattern uses ~1MB when processed (conservative estimate)
		// Use only 25% of available memory for safety
		let safe_memory_mb = available_memory_mb / 4;
		let estimated_pattern_size_mb = 1; // Conservative estimate per pattern
		let memory_based_batch_size = (safe_memory_mb / estimated_pattern_size_mb).clamp(10, 200);

		let cpu_count = num_cpus::get();
		let optimal_batch_size = (memory_based_batch_size / cpu_count).max(5);

		tracing::debug!(optimal_batch_size = optimal_batch_size, memory_based_batch_size = memory_based_batch_size, "Using batch size");

		// Process in memory-aware chunks, removing from database queue as processed
		let mut processed_patterns = Vec::new();

		for chunk in patterns.chunks(memory_based_batch_size) {
			// Release the lock while processing
			let chunk_patterns: Vec<Dictionary> = chunk
				.chunks(optimal_batch_size)
				.collect::<Vec<_>>()
				.par_iter()
				.map(|chunk_patterns| {
					let mut chunk_dict = Dictionary::new(format!("Chunk Dictionary {}", uuid::Uuid::new_v4()), "Temporary dictionary for parallel processing".to_string(), dictionary.constraints().clone());
					for pattern in *chunk_patterns {
						if let Err(e) = chunk_dict.import_pattern(pattern.clone()) {
							tracing::error!(error = %e, "Failed to import pattern in chunk");
						}
					}

					chunk_dict
				})
				.collect();

			// Merge chunk dictionaries
			for chunk_dict in chunk_patterns {
				dictionary.merge_dictionary(chunk_dict)?;
			}

			// Track processed patterns for removal from database queue
			processed_patterns.extend_from_slice(chunk);

			let remaining = pattern_count - processed_patterns.len();
			if remaining > 0 {
				// println!("Processed {} patterns, {} remaining", pattern_count - remaining, remaining);
			}

			// Yield to prevent blocking other tasks
			tokio::task::yield_now().await;
		}

		// Patterns are now stored in dictionary databases and should persist,
		// so we don't remove them after loading into memory
		// for pattern in processed_patterns {
		//     database.dequeue_processed_pattern(&pattern).await?;
		// }
	}

	let final_elapsed = start_time.elapsed();
	let final_rate = pattern_count as f64 / final_elapsed.as_secs_f64();
	tracing::info!(pattern_count = pattern_count, elapsed = ?final_elapsed, rate = final_rate, "Dictionary loading completed");
	Ok(())
}

/// Get available system memory in MB
/// Returns a conservative estimate to prevent memory exhaustion
fn get_available_memory_mb() -> usize {
	#[cfg(target_os = "windows")]
	{
		use std::mem;

		#[repr(C)]
		struct MemoryStatusEx {
			dw_length: u32,
			dw_memory_load: u32,
			ull_total_phys: u64,
			ull_avail_phys: u64,
			ull_total_page_file: u64,
			ull_avail_page_file: u64,
			ull_total_virtual: u64,
			ull_avail_virtual: u64,
			ull_avail_extended_virtual: u64,
		}

		extern "system" {
			fn GlobalMemoryStatusEx(lpBuffer: *mut MemoryStatusEx) -> i32;
		}

		let mut mem_status = MemoryStatusEx { dw_length: u32::try_from(mem::size_of::<MemoryStatusEx>()).unwrap(), dw_memory_load: 0, ull_total_phys: 0, ull_avail_phys: 0, ull_total_page_file: 0, ull_avail_page_file: 0, ull_total_virtual: 0, ull_avail_virtual: 0, ull_avail_extended_virtual: 0 };

		unsafe {
			if GlobalMemoryStatusEx(&raw mut mem_status) != 0 {
				return (mem_status.ull_avail_phys / (1024 * 1024)) as usize;
			}
		}
	}

	#[cfg(target_os = "linux")]
	{
		if let Ok(contents) = std::fs::read_to_string("/proc/meminfo") {
			for line in contents.lines() {
				if line.starts_with("MemAvailable:") {
					if let Some(value) = line.split_whitespace().nth(1) {
						if let Ok(kb) = value.parse::<usize>() {
							return kb / 1024; // Convert KB to MB
						}
					}
				}
			}
		}
	}

	#[cfg(target_os = "macos")]
	{
		// Fallback for macOS - could implement using sysctl if needed
		return 4096; // 4GB conservative fallback
	}

	// Conservative fallback if we can't determine memory
	2048 // 2GB conservative fallback
}

/// Detects 5% price increases from month start to month end
///
/// This function analyzes price movements to detect when the price rises 5% or more
/// from the beginning of a calendar month to the end of that same month. Each detected
/// increase creates an event with a manifestation at the end of the month when the
/// 5% threshold is confirmed.
///
/// This is useful for determining if buying at the start of each month would be profitable.
///
/// # Parameters
/// - `database`: Database instance to query measurements from and store events
/// - `aspect`: The aspect ID to analyze for price movements
/// - `resolution`: The resolution for data analysis
/// - `method`: The spline interpolation method to use
///
/// # Algorithm
/// 1. Retrieve all measurements for the aspect within the date range
/// 2. Group measurements by calendar month
/// 3. For each month, find the first price (beginning) and last price (end)
/// 4. Calculate percentage increase from start to end of month
/// 5. If increase is 5% or higher, create an event at the end of the month
/// 6. Store events in the database
///
/// # Errors
///
/// Returns an error if:
/// - No earliest or latest measurements are found
/// - Database operations fail (streaming data, getting database info, storing events)
///
/// # Panics
///
/// This function will panic if the 5% threshold value (0.05) cannot be converted to `BigDecimal`,
/// which should not happen under normal circumstances.
pub async fn create_event_and_manifestations(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline) -> Result<()> {
	use std::collections::HashMap;
	let start_time: chrono::DateTime<chrono::Utc> = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
	let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;

	// Use optimized bulk analysis to get all points
	let mut point_stream = Outputs::analyze_range(database, aspect, start_time, end_time, *resolution, *method).await?;

	let mut points = Vec::new();
	while let Some(result) = point_stream.next().await {
		let point = result?;
		points.push(point);
	}

	// Sort points by timestamp to ensure proper chronological order
	points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	let mut event = Event::new(None, format!("5% Monthly Price Increase - {aspect}"), Some(format!("Detects when price increases 5% or more from start to end of month for aspect {aspect}")), None);
	let threshold_percentage = BigDecimal::from_f64(0.05).unwrap(); // 5% threshold

	// Group points by month and analyze each month
	let mut months_data: HashMap<(i32, u32), Vec<&splimes::Point>> = HashMap::new();

	// Group all points by year and month
	for point in &points {
		let year = point.timestamp.year();
		let month = point.timestamp.month();
		months_data.entry((year, month)).or_default().push(point);
	}

	// Analyze each month for 5% increases
	for ((_year, _month), month_points) in months_data {
		if month_points.len() < 2 {
			continue; // Need at least 2 points to compare start and end
		}

		// Sort points within the month by timestamp
		let mut sorted_month_points = month_points;
		sorted_month_points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

		let start_price = &sorted_month_points[0].value;
		let end_price = &sorted_month_points[sorted_month_points.len() - 1].value;
		let end_timestamp = sorted_month_points[sorted_month_points.len() - 1].timestamp;

		// Calculate percentage increase from start to end of month
		// Formula: (end_price - start_price) / start_price
		let price_diff = end_price - start_price;
		let percentage_increase = &price_diff / start_price;

		// If increase is 5% or more, create a manifestation
		if percentage_increase >= threshold_percentage {
			let database_info = database.get_database_info().await.expect("Database info should be available");
			let manifestation = Manifestation::new(
				database_info.id().as_uuid(),
				sorted_month_points[0].timestamp, // Start of the month
				end_timestamp,                    // End of the month
			);

			event.add_manifestation(manifestation);
		}
	}

	// Store all detected events in the database
	if !event.manifestations().is_empty() {
		database.insert_unprocessed_event(aspect, &event).await?;
		tracing::info!(
			event_name = %event.name(),
			manifestations = event.manifestations().len(),
			"Stored event in database"
		);
	}

	Ok(())
}

/// Creates correlations between events and patterns for signal generation.
///
/// This function correlates all events with all patterns in the dictionary,
/// creating the foundation for signal prediction.
///
/// # Errors
///
/// Returns an error if:
/// - Database operations fail (getting events, marking events processed)
/// - Correlation creation fails
pub async fn create_correlations_for_events(database: &Database, dictionary: &Dictionary, aspect_id: &AspectId) -> Result<()> {
	// Get patterns directly from database (not from dictionary object which may have filtered patterns)
	let patterns: Vec<weftdb::Pattern> = database.get_dictionary_patterns(aspect_id, dictionary.name()).await?.try_collect().await?;
	let events: Vec<weftdb::Event> = database.get_unprocessed_events(aspect_id).await?.try_collect().await?;
	let aspect = database.get_aspect(aspect_id).await?;

	// Exit early if no patterns or events to correlate
	if patterns.is_empty() || events.is_empty() {
		tracing::debug!("No patterns or events found to correlate");
		return Ok(());
	}

	tracing::info!(events = events.len(), patterns = patterns.len(), "Correlating events with patterns");

	// For each event, correlate with all patterns
	for event in &events {
		for pattern in &patterns {
			// Calculate the average distance between consecutive pattern occurrences/event manifestations
			// This represents the cycle period (e.g., 4 hours for a 4-hour cycle)
			// Per documentation: average_distance is the typical time between recurrences
			let mut total_distance: i64 = 0;
			let mut distance_count: i64 = 0;
			let mut pattern_resolution = splimes::Resolution::Minutes; // Default, will be overwritten

			// Collect and sort manifestation times
			let mut manifestation_times: Vec<chrono::DateTime<chrono::Utc>> = event.manifestations().values().map(|m| *m.start()).collect();
			manifestation_times.sort();

			// Calculate distances between consecutive manifestations (the cycle period)
			if manifestation_times.len() >= 2 {
				// Get resolution from the first occurrence if available
				if let Some(occurrence) = pattern.occurrences().first() {
					pattern_resolution = *occurrence.resolution();
				}

				for window in manifestation_times.windows(2) {
					if let Ok(diff) = pattern_resolution.difference(&window[1], &window[0]) {
						if diff > 0 {
							total_distance += diff;
							distance_count += 1;
						}
					}
				}
			}

			// Calculate average_distance if we have valid distances
			let average_distance = if distance_count > 0 {
				let avg = total_distance / distance_count;
				tracing::debug!(
					pattern_id = %pattern.id(),
					event_id = %event.id(),
					average_distance = avg,
					resolution = ?pattern_resolution,
					consecutive_pairs = distance_count,
					"Correlation calculated"
				);
				Some(weftdb::Distance::new(bigdecimal::BigDecimal::from(avg), pattern_resolution))
			} else {
				tracing::debug!(
					pattern_id = %pattern.id(),
					event_id = %event.id(),
					"No consecutive pairs found for correlation"
				);
				None
			};

			// Assign constant to local variable before borrowing to avoid clippy warning
			let error_rate = HashMap::new();

			let correlation = Correlation::new(None, DictionaryId::from_uuid(pattern.occurrences()[0].database_info().id().as_uuid()), aspect.subject_id(), aspect_id, pattern.id(), event.id().clone(), error_rate, pattern.occurrences().clone(), average_distance);
			database.insert_correlation(aspect_id, &correlation).await?;
		}
	}

	tracing::info!("Pattern-Event correlation completed successfully");
	Ok(())
}

async fn process_correlation_signals(correlation: &weftdb::Correlation, events: &[weftdb::Event]) -> Result<usize> {
	let mut signals_count = 0usize;

	if let Some(event) = events.iter().find(|e| e.id() == correlation.event_id()) {
		// For each manifestation of the event, create signals based on pattern occurrences
		for manifestation in event.manifestations().values() {
			for occurrence in correlation.occurrences() {
				let manifestation_start = *manifestation.start();
				let manifestation_midpoint = manifestation.midpoint();
				let manifestation_end = *manifestation.end();

				let occurrence_end = *occurrence.end();
				let pattern_resolution = *occurrence.resolution();
				let time_diff_start = pattern_resolution.difference(&manifestation_start, &occurrence_end)?;
				let time_diff_midpoint = pattern_resolution.difference(&manifestation_midpoint, &occurrence_end)?;
				let time_diff_end = pattern_resolution.difference(&manifestation_end, &occurrence_end)?;

				if occurrence_end >= manifestation_start {
					continue;
				}

				let distance_start = weftdb::Distance::new(BigDecimal::from(time_diff_start), pattern_resolution);
				let distance_midpoint = weftdb::Distance::new(BigDecimal::from(time_diff_midpoint), pattern_resolution);
				let distance_end = weftdb::Distance::new(BigDecimal::from(time_diff_end), pattern_resolution);

				let signal_types = vec![(SignalType::PredictStart, distance_start), (SignalType::PredictMid, distance_midpoint), (SignalType::PredictEnd, distance_end)];

				for (signal_type, distance) in signal_types {
					let manifestation_id = manifestation.id().clone();
					let signal = Signal::new(correlation.id().clone(), manifestation_id, correlation.event_id().clone(), occurrence_end, signal_type, distance.clone());
					SIGNALS_QUEUE.lock().await.insert(signal);
					signals_count += 1;
				}
			}
		}

		// Forward-looking signals: use minimum historical distance
		let latest_manifestation = event.manifestations().values().map(|m| *m.end()).max();
		if let Some(latest_manifest_time) = latest_manifestation {
			let mut min_distance: Option<i64> = None;
			for manifestation in event.manifestations().values() {
				for occurrence in correlation.occurrences() {
					let occurrence_end = *occurrence.end();
					let manifestation_start = *manifestation.start();
					if occurrence_end < manifestation_start {
						let pattern_resolution = *occurrence.resolution();
						if let Ok(diff) = pattern_resolution.difference(&manifestation_start, &occurrence_end) {
							if diff > 0 {
								min_distance = Some(min_distance.map_or(diff, |current| current.min(diff)));
							}
						}
					}
				}
			}

			if let Some(pred_distance) = min_distance {
				for occurrence in correlation.occurrences() {
					let occurrence_end = *occurrence.end();
					if occurrence_end > latest_manifest_time {
						let pattern_resolution = *occurrence.resolution();
						let distance = weftdb::Distance::new(BigDecimal::from(pred_distance), pattern_resolution);
						let signal = Signal::new(correlation.id().clone(), weftdb::ManifestationId::new(), correlation.event_id().clone(), occurrence_end, SignalType::PredictStart, distance);
						SIGNALS_QUEUE.lock().await.insert(signal);
						signals_count += 1;
					}
				}
			}
		}
	}

	Ok(signals_count)
}

/// Creates prediction signals from correlations and events.
///
/// This function generates signals for each correlation-manifestation pair,
/// representing predictions of when events will occur.
///
/// # Errors
///
/// Returns an error if:
/// - Database operations fail (getting events)
/// - Time difference calculations fail
/// - Signal creation fails
pub async fn create_signals(database: &Database, aspect_id: &AspectId) -> Result<()> {
	// Get correlations and events from database
	let correlations: Vec<weftdb::Correlation> = Outputs::get_correlations(database, aspect_id).await?.try_collect().await?;
	let events: Vec<weftdb::Event> = Outputs::get_unprocessed_events(database, aspect_id).await?.try_collect().await?;

	// Exit early if no correlations or events to process
	if correlations.is_empty() || events.is_empty() {
		tracing::debug!("No correlations or events found to create signals");
		return Ok(());
	}

	tracing::info!(correlations = correlations.len(), events = events.len(), "Creating signals");

	let mut signals_count = 0usize;
	for correlation in &correlations {
		signals_count += process_correlation_signals(correlation, &events).await?;
	}

	tracing::info!(signals_created = signals_count, "Signal creation completed");
	Ok(())
}

/// Filters out expired signals based on event resolution times at a specific time.
///
/// This is the implementation function for `filter_expired_signals` that allows
/// specifying a custom current time (useful for testing).
///
/// # Errors
///
/// Returns an error if:
/// - Database operations fail (getting events, correlations, updating correlations)
/// - Signal removal or error correction fails
/// - Time calculations or conversions fail
pub async fn filter_expired_signals(database: &Database, aspect_id: &AspectId, current_time: Option<chrono::DateTime<chrono::Utc>>) -> Result<()> {
	use chrono::Utc;

	let current_time = current_time.unwrap_or_else(Utc::now);
	let mut signals_lock = SIGNALS_QUEUE.lock().await;

	if signals_lock.is_empty() {
		tracing::debug!("No signals found to filter");
		return Ok(());
	}

	let initial_count = signals_lock.len();
	tracing::info!(query_time = %current_time.format("%Y-%m-%d %H:%M:%S"), "Starting filter_expired_signals");
	tracing::debug!(total_signals = initial_count, "Filtering expired signals");

	// Debug: show sample signals before filtering
	let sample_signals: Vec<_> = signals_lock.values().take(5).collect();
	for (idx, sig) in sample_signals.iter().enumerate() {
		tracing::debug!(
			index = idx,
			manifestation_date = %sig.manifestation_date(),
			distance_value = %sig.distance().value(),
			distance_units = ?sig.distance().units(),
			signal_type = ?sig.signal_type(),
			"Sample signal before filtering"
		);
	}

	// Get events and correlations once to avoid database locks during processing
	let events: Vec<weftdb::Event> = Outputs::get_unprocessed_events(database, aspect_id).await?.try_collect().await?;
	let correlations: Vec<weftdb::Correlation> = Outputs::get_correlations(database, aspect_id).await?.try_collect().await?;

	tracing::debug!(events = events.len(), correlations = correlations.len(), "Found events and correlations");

	let (signals_to_remove, expired_historical, expired_forward) = collect_expired_signals(&signals_lock, &events, &correlations, current_time);

	// Correlations loaded from database, no lock to release

	tracing::debug!(total = signals_to_remove.len(), historical = expired_historical, forward_looking = expired_forward, "Removing expired signals");

	// Remove expired signals with error correction
	// Use a HashMap to track correlations by ID so we only update each once
	let mut removed_count = 0;
	let mut correlations_map: std::collections::HashMap<weftdb::CorrelationID, weftdb::Correlation> = std::collections::HashMap::new();

	// Pre-populate the map with correlations that have signals to remove
	for (correlation_id, _, _, _) in &signals_to_remove {
		if !correlations_map.contains_key(correlation_id) {
			if let Some(correlation) = correlations.iter().find(|c| c.id() == correlation_id) {
				correlations_map.insert(correlation_id.clone(), correlation.clone());
			}
		}
	}

	for (correlation_id, manifestation_id, signal_type, resolution_time) in signals_to_remove {
		// Get the mutable correlation from our map
		if let Some(correlation) = correlations_map.get_mut(&correlation_id) {
			// Use error correction when removing the signal
			if let Ok(Some(_removed_signal)) = signals_lock.remove_with_error_correction(correlation, &manifestation_id, &signal_type, resolution_time) {
				removed_count += 1;
			}
		}
	}

	// Batch update only unique modified correlations
	let unique_correlations: Vec<_> = correlations_map.into_values().collect();
	tracing::debug!(unique_correlations = unique_correlations.len(), "Updating modified correlations");
	for correlation in unique_correlations {
		database.update_correlation(aspect_id, &correlation).await?;
	}

	let remaining_count = signals_lock.len();

	// Release locks
	drop(signals_lock);

	tracing::info!(removed = removed_count, remaining = remaining_count, initial = initial_count, "Filtered expired signals");

	Ok(())
}

#[cfg(test)]
mod tests {

	use ::weftdb::database::traits::{AspectStructure, Inputs};
	use batch_utils::*;
	use bigdecimal::{BigDecimal, FromPrimitive};
	use chrono::{TimeZone, Utc};
	use rand::Rng;
	use serde_json::json;
	use serial_test::serial;
	use splimes::Spline;
	use weftdb::{data_dir, AspectId, Database, DatasetId, InputMeasurement, Resolution};

	use super::*;

	#[tokio::test]
	#[serial]
	async fn test_api() -> Result<()> {
		// Initialize tracing subscriber for test output
		// Filter to reduce noise: INFO for external crates, DEBUG for our code
		let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,dataset_management=debug,database=debug,splimes=info,turso_core=warn"))).with_test_writer().try_init();

		// Skip this test if running in CI or if we want fast feedback
		if std::env::var("SKIP_SLOW_TESTS").is_ok() {
			tracing::info!("Skipping test_api due to SKIP_SLOW_TESTS environment variable");
			return Ok(());
		}

		// Debug: Show where we're looking for the database
		tracing::info!(path = %format!("{}/Crypto", data_dir()), "Looking for Crypto database");
		tracing::debug!(metadata_path = %format!("{}/Crypto/metadata.db", data_dir()), "Metadata file location");

		// Try to get the Crypto database, skip test if it doesn't exist or doesn't have the required data
		let database = match Database::existing("Crypto").await {
			Ok(db) => db,
			Err(e) => {
				tracing::warn!(error = ?e, "Skipping test_api - Crypto database not found (run database tests first)");
				return Ok(());
			}
		};

		let subjects = database.list_subjects().await?;
		let Some(subject_id) = subjects.iter().find(|(_, name)| name.as_str() == "BTCUSD").map(|(id, _)| *id) else {
			tracing::warn!("Skipping test_api - BTCUSD subject not found in Crypto database");
			return Ok(());
		};

		let aspects = database.get_subject_aspects(&subject_id).await?;
		let Some(aspect) = aspects.iter().find(|a| a.name() == "open") else {
			tracing::warn!(available_aspects = ?aspects.iter().map(weftdb::Aspect::name).collect::<Vec<_>>(), "Skipping test_api - 'open' aspect not found");
			return Ok(());
		};
		let aspect_id = aspect.id();

		// Check if the aspect has any measurements before proceeding
		if database.get_earliest_measurement(&aspect.id()).await?.is_none() {
			tracing::warn!("Skipping test_api - No measurements found for 'open' aspect in Crypto database");
			return Ok(());
		}

		let resolution = Resolution::Hours;
		let method = Spline::Linear;
		let batch_size = 24;

		tracing::info!(aspect_id = %aspect.id(), "Aspect ID");

		// start timer
		let timer = std::time::Instant::now();
		tracing::info!("Starting build_unprocessed_queue...");

		build_unprocessed_queue(&database, &aspect.id(), &resolution, &method, batch_size).await?;

		tracing::info!(elapsed = ?timer.elapsed(), "Time taken for build_unprocessed_queue");

		// start timer for processed queue
		let timer = std::time::Instant::now();
		tracing::info!("Starting build_processed_queue...");

		build_processed_batch_queue(&database, &aspect.id()).await?;

		tracing::info!(elapsed = ?timer.elapsed(), "Time taken for build_processed_queue");

		let timer = std::time::Instant::now();
		tracing::info!("Starting get_processed_batches_queue...");

		let processed_batches: Vec<Batch> = database.get_processed_batches(&aspect.id()).await?.try_collect().await?;

		tracing::info!(elapsed = ?timer.elapsed(), "Time taken for get_processed_batches_queue");

		// print random batch from processed queue for verification
		let length = processed_batches.len();
		if length > 0 {
			let random_index = rand::rng().random_range(0..length);
			tracing::debug!(batch = %json!(&processed_batches[random_index]), "Random processed batch");
			output_denk_format_batch(&processed_batches[random_index]);
		}

		#[rustfmt::skip]
		let contraints = DictionaryConstraints::new(
                        Some(Steps::new(
                                10,
                                Spline::Linear
                        )),
                        Some(
                                vec![
                                        VariablilityType::MaximumStatic(Variability::new(
                                                BigDecimal::from_f64(0.1).unwrap()
                                        ))
                                ]
                        )
                );

		let mut dictionary = Dictionary::new("TestDictionary".to_string(), "A dictionary for testing purposes".to_string(), contraints);
		let timer = std::time::Instant::now();
		tracing::info!("Starting load_dictionary...");
		load_dictionary(&database, &aspect.id(), &mut dictionary).await?;
		tracing::info!(elapsed = ?timer.elapsed(), "Time taken for load_dictionary");
		tracing::info!(pattern_count = dictionary.len(), "Dictionary now contains patterns");

		let _timer = std::time::Instant::now();
		tracing::info!("Starting build_processed_queue...");

		build_processed_batch_queue(&database, &aspect.id()).await?;

		tracing::info!("Finished building processed batch queue");

		let processed_batches: Vec<Batch> = database.get_processed_batches(&aspect.id()).await?.try_collect().await?;
		let length = processed_batches.len();
		tracing::info!(count = length, "Processed batches count");
		if length > 0 {
			let random_index = rand::rng().random_range(0..length);
			tracing::debug!(batch = %json!(&processed_batches[random_index]), "Random processed batch");
			output_denk_format_batch(&processed_batches[random_index]);
		}

		build_patterns_queue(&database, &aspect_id, &mut dictionary).await?;
		let patterns: Vec<weftdb::Pattern> = Outputs::get_dictionary_patterns(&database, &aspect.id(), "TestDictionary").await?.try_collect().await?;
		let length = patterns.len();
		tracing::info!(count = length, "Patterns count");
		if length > 0 {
			let random_index = rand::rng().random_range(0..length);
			tracing::debug!(pattern = %json!(&patterns[random_index]), "Random pattern");
			output_denk_format_pattern(&patterns[random_index]);
		}

		// Load the newly stored patterns into the dictionary
		load_dictionary(&database, &aspect.id(), &mut dictionary).await?;
		tracing::info!(pattern_count = dictionary.len(), "Dictionary patterns after loading from database");

		let timer = std::time::Instant::now();
		tracing::info!("Starting build_events_5_percent_queue...");

		create_event_and_manifestations(&database, &aspect.id(), &resolution, &method).await?;

		tracing::info!(elapsed = ?timer.elapsed(), "Time taken for build_events_5_percent_queue");

		let timer = std::time::Instant::now();
		tracing::info!("Starting create_correlations_for_events...");
		create_correlations_for_events(&database, &dictionary, &aspect.id()).await?;
		tracing::info!(elapsed = ?timer.elapsed(), "Time taken for create_correlations_for_events");

		// print the number of correlations with more than 1 occurrence (using streaming)
		let mut correlation_stream = database.get_correlations(&aspect.id()).await?;
		let mut multi_occurrence_count = 0usize;
		while let Some(result) = correlation_stream.next().await {
			if let Ok(correlation) = result {
				if correlation.occurrences().len() > 1 {
					multi_occurrence_count += 1;
				}
			}
		}
		tracing::info!(count = multi_occurrence_count, "Number of correlations with more than 1 occurrence");

		// print the number of patterns with more than 1 occurrence
		tracing::info!(count = dictionary.patterns().iter().filter(|p| p.occurrences().len() > 1).count(), "Number of patterns with more than 1 occurrence");

		let timer = std::time::Instant::now();
		tracing::info!("Starting create_signals...");
		create_signals(&database, &aspect.id()).await?;
		tracing::info!(elapsed = ?timer.elapsed(), "Time taken for create_signals");

		// print the number of signals created
		let signals_lock = SIGNALS_QUEUE.lock().await;
		tracing::info!(count = signals_lock.len(), "Number of signals created");
		drop(signals_lock);

		let timer = std::time::Instant::now();
		tracing::info!("Starting filter_expired_signals...");
		// Use the latest measurement time as the query time, not Utc::now()
		// This prevents all historical signals from being filtered as "expired"
		let latest_time = database.get_latest_measurement(&aspect.id()).await?;
		filter_expired_signals(&database, &aspect.id(), latest_time).await?;
		tracing::info!(elapsed = ?timer.elapsed(), "Time taken for filter_expired_signals");

		// print the number of signals after filtering
		let signals_lock = SIGNALS_QUEUE.lock().await;
		tracing::info!(count = signals_lock.len(), "Number of signals after filtering");

		// print a random signal probability for verification

		tracing::debug!("Preparing to calculate sample signal probability...");

		// Find a signal with non-zero error rates for more meaningful testing
		let mut random_signal = None;
		let correlation_to_modify = signals_lock.values().next().map(|signal| {
			random_signal = Some(signal.clone());
			signal.correlation_id().clone()
		});

		// If we found a signal, check and potentially modify its correlation's error rates
		if let (Some(signal), Some(correlation_id)) = (&random_signal, &correlation_to_modify) {
			if let Ok(mut correlation) = database.get_correlation(&aspect.id(), correlation_id).await {
				// Check if error rates are zero
				let has_nonzero = correlation.error_rate().values().any(|err| !err.value().is_zero());

				if !has_nonzero {
					tracing::debug!(correlation_id = %correlation.id(), "Setting test error rate for correlation to make testing more meaningful");
					correlation.set_error_rate(signal.signal_type().clone(), weftdb::Distance::new(BigDecimal::from_f64(0.1).unwrap(), splimes::Resolution::Seconds));

					// Update the correlation in the database
					database.update_correlation(&aspect.id(), &correlation).await?;
				}
			}
		}

		// Use the signal we found
		let random_signal = random_signal.unwrap_or_else(|| panic!("No signals found"));

		// get the correlation id, manifestation id, event id, and signal type from the actual signal
		let random_correlation_id = random_signal.correlation_id().clone();
		let random_manifestation_id = random_signal.manifestation_id().clone();
		let random_event_id = random_signal.event_id().clone();
		let signal_type = random_signal.signal_type().clone();
		let random_correlation = database.get_correlation(&aspect.id(), &random_correlation_id).await?;
		let random_correlation_error_rate = random_correlation.error_rate().clone();

		// Get the error rate for the specific signal type we're using
		let error_rate = random_correlation_error_rate.get(&signal_type).or_else(|| random_correlation_error_rate.values().next()).cloned().unwrap_or_else(|| weftdb::Distance::new(BigDecimal::from(0), splimes::Resolution::Seconds));

		let timer = std::time::Instant::now();
		tracing::info!("Calculating sample signal probability...");

		// get oct. 1st 2025 DateTime<Utc>
		let sample_datetime = Utc.with_ymd_and_hms(2025, 12, 15, 0, 0, 0).unwrap();

		let sig_sum_probability = signals_lock.probability_sum(&random_event_id, &signal_type, sample_datetime, &database, &aspect.id()).await?.unwrap_or_else(|| BigDecimal::from(0));
		let sig_avg_probability = signals_lock.probability_average(&random_event_id, &signal_type, sample_datetime, &database, &aspect.id()).await?.unwrap_or_else(|| BigDecimal::from(0));
		let sig_event_probability = signals_lock.event_probability(&random_event_id, &signal_type, sample_datetime, &database, &aspect.id()).await?;

		tracing::info!(elapsed = ?timer.elapsed(), "Time taken for sample signal probability calculation");

		let event_prob_str = sig_event_probability.map_or_else(|| "N/A".to_string(), |p| format!("{p:.4}"));
		tracing::info!(
			correlation_id = %random_correlation_id,
			manifestation_id = %random_manifestation_id,
			signal_type = ?signal_type,
			sum = %format!("{:.4}", sig_sum_probability),
			average = %format!("{:.4}", sig_avg_probability),
			event_based = %event_prob_str,
			error_rate = %error_rate.value(),
			"Sample signal probability"
		);

		drop(signals_lock);

		tracing::info!("Test completed successfully!");
		Ok(())
	}

	/// Test using the new Pipeline API - mirrors `test_api` but uses the fluent builder pattern
	#[tokio::test]
	#[serial]
	async fn test_pipeline_api() -> Result<()> {
		// Initialize tracing subscriber for test output
		let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,dataset_management=debug,database=debug,splimes=info,turso_core=warn"))).with_test_writer().try_init();

		// Skip this test if running in CI or if we want fast feedback
		if std::env::var("SKIP_SLOW_TESTS").is_ok() {
			tracing::info!("Skipping test_pipeline_api due to SKIP_SLOW_TESTS environment variable");
			return Ok(());
		}

		// Debug: Show where we're looking for the database
		tracing::info!(path = %format!("{}/Crypto", data_dir()), "Looking for Crypto database");

		// Try to get the Crypto database, skip test if it doesn't exist or doesn't have the required data
		let database = match Database::existing("Crypto").await {
			Ok(db) => db,
			Err(e) => {
				tracing::warn!(error = ?e, "Skipping test_pipeline_api - Crypto database not found (run database tests first)");
				return Ok(());
			}
		};

		let subjects = database.list_subjects().await?;
		let Some(subject_id) = subjects.iter().find(|(_, name)| name.as_str() == "BTCUSD").map(|(id, _)| *id) else {
			tracing::warn!("Skipping test_pipeline_api - BTCUSD subject not found in Crypto database");
			return Ok(());
		};

		let aspects = database.get_subject_aspects(&subject_id).await?;
		let Some(aspect) = aspects.iter().find(|a| a.name() == "open") else {
			tracing::warn!(available_aspects = ?aspects.iter().map(weftdb::Aspect::name).collect::<Vec<_>>(), "Skipping test_pipeline_api - 'open' aspect not found");
			return Ok(());
		};
		let aspect_id = aspect.id();

		// Check if the aspect has any measurements before proceeding
		if database.get_earliest_measurement(&aspect.id()).await?.is_none() {
			tracing::warn!("Skipping test_pipeline_api - No measurements found for 'open' aspect in Crypto database");
			return Ok(());
		}

		tracing::info!(aspect_id = %aspect_id, "Aspect ID");

		// Build the pipeline using the fluent builder API
		let timer = std::time::Instant::now();
		tracing::info!("Building pipeline with Pipeline::builder...");

		#[rustfmt::skip]
		let dictionary_constraints = DictionaryConstraints::new(
			Some(Steps::new(
				10,
				Spline::Linear
			)),
			Some(
				vec![
					VariablilityType::MaximumStatic(Variability::new(
						BigDecimal::from_f64(0.1).unwrap()
					))
				]
			)
		);

		let mut pipeline = Pipeline::builder(database.clone(), aspect_id).spline_method(Spline::Linear).batch_size(24).add_dictionary("TestDictionary", "A dictionary for testing purposes", dictionary_constraints).with_monthly_increase_detector(0.05).with_peak_detector("Pressure Peaks").build().await?;

		tracing::info!(elapsed = ?timer.elapsed(), "Pipeline built");
		tracing::info!(detector_count = pipeline.detector_count(), "Registered event detectors");

		// Run the full pipeline
		let timer = std::time::Instant::now();
		tracing::info!("Running pipeline.run()...");

		pipeline.run().await?;

		tracing::info!(elapsed = ?timer.elapsed(), "Pipeline run completed");

		// Inspect pipeline results
		tracing::info!(
			dictionary_patterns = pipeline.get_dictionary("TestDictionary").map_or(0, weftdb::Dictionary::len),
			signals_count = pipeline.signals().len(),
			run_count = pipeline.state().run_count,
			last_run = ?pipeline.state().last_run,
			"Pipeline results"
		);

		// Get processed batches to verify data preparation worked
		let processed_batches: Vec<Batch> = database.get_processed_batches(&aspect_id).await?.try_collect().await?;
		let length = processed_batches.len();
		tracing::info!(count = length, "Processed batches count");
		if length > 0 {
			let random_index = rand::rng().random_range(0..length);
			tracing::debug!(batch = %json!(&processed_batches[random_index]), "Random processed batch");
			output_denk_format_batch(&processed_batches[random_index]);
		}

		// Get patterns to verify pattern extraction worked
		let patterns: Vec<weftdb::Pattern> = Outputs::get_dictionary_patterns(&database, &aspect_id, "TestDictionary").await?.try_collect().await?;
		let length = patterns.len();
		tracing::info!(count = length, "Patterns count");
		if length > 0 {
			let random_index = rand::rng().random_range(0..length);
			tracing::debug!(pattern = %json!(&patterns[random_index]), "Random pattern");
			output_denk_format_pattern(&patterns[random_index]);
		}

		// Check correlations
		let mut correlation_stream = database.get_correlations(&aspect_id).await?;
		let mut multi_occurrence_count = 0usize;
		while let Some(result) = correlation_stream.next().await {
			if let Ok(correlation) = result {
				if correlation.occurrences().len() > 1 {
					multi_occurrence_count += 1;
				}
			}
		}
		tracing::info!(count = multi_occurrence_count, "Number of correlations with more than 1 occurrence");

		// Check patterns with multiple occurrences
		let dict_multi_occ_count = pipeline.get_dictionary("TestDictionary").map_or(0, |d| d.patterns().iter().filter(|p| p.occurrences().len() > 1).count());
		tracing::info!(count = dict_multi_occ_count, "Number of patterns with more than 1 occurrence");

		// Query probability for a random signal if signals exist
		if pipeline.signals().is_empty() {
			tracing::info!("No signals generated - skipping probability query");
		} else {
			let random_signal = pipeline.signals().values().next().unwrap();
			let random_event_id = random_signal.event_id().clone();
			let signal_type = random_signal.signal_type().clone();

			// If the correlation has zero error rates, set a test error rate
			let correlation_id = random_signal.correlation_id().clone();
			if let Ok(mut correlation) = database.get_correlation(&aspect_id, &correlation_id).await {
				let has_nonzero = correlation.error_rate().values().any(|err| !err.value().is_zero());
				if !has_nonzero {
					tracing::debug!(correlation_id = %correlation.id(), "Setting test error rate for correlation");
					correlation.set_error_rate(signal_type.clone(), weftdb::Distance::new(BigDecimal::from_f64(0.1).unwrap(), splimes::Resolution::Seconds));
					database.update_correlation(&aspect_id, &correlation).await?;
				}
			}

			let timer = std::time::Instant::now();
			tracing::info!("Calculating sample signal probability using pipeline.query_probability...");

			let sample_datetime = Utc.with_ymd_and_hms(2025, 12, 15, 0, 0, 0).unwrap();

			// Use the pipeline's query_probability method
			let prob_result = pipeline.query_probability(&random_event_id, &signal_type, sample_datetime).await?;

			tracing::info!(elapsed = ?timer.elapsed(), "Time taken for probability calculation");
			tracing::info!(
				event_id = %random_event_id,
				signal_type = ?signal_type,
				result = %prob_result,
				"Sample signal probability"
			);

			// Also test individual probability methods for comparison
			let sig_sum_probability = pipeline.signals().probability_sum(&random_event_id, &signal_type, sample_datetime, &database, &aspect_id).await?.unwrap_or_else(|| BigDecimal::from(0));
			let sig_avg_probability = pipeline.signals().probability_average(&random_event_id, &signal_type, sample_datetime, &database, &aspect_id).await?.unwrap_or_else(|| BigDecimal::from(0));
			let sig_event_probability = pipeline.signals().event_probability(&random_event_id, &signal_type, sample_datetime, &database, &aspect_id).await?;

			let event_prob_str = sig_event_probability.map_or_else(|| "N/A".to_string(), |p| format!("{p:.4}"));
			tracing::info!(
				sum = %format!("{:.4}", sig_sum_probability),
				average = %format!("{:.4}", sig_avg_probability),
				event_based = %event_prob_str,
				"Direct probability calculation comparison"
			);
		}

		// Verify pipeline state was saved
		let state_exists = Pipeline::exists(&database, &aspect_id).await?;
		tracing::info!(state_saved = state_exists, "Pipeline state persistence");

		// Test loading the pipeline from saved state
		if state_exists {
			let timer = std::time::Instant::now();
			tracing::info!("Testing Pipeline::load from saved state...");

			let mut loaded_pipeline = Pipeline::load(database.clone(), &aspect_id).await?;

			// Re-register detectors (they can't be serialized)
			// Use Arc::new with async move block for proper lifetime handling when capturing values
			let threshold = 0.05;
			let detector_fn: pipeline::EventDetectorFn = std::sync::Arc::new(move |db, aspect, res, method| Box::pin(async move { detectors::detect_monthly_increase(db, aspect, res, method, threshold).await }));
			loaded_pipeline.register_detector(EventDetector::new("monthly_increase_5pct", "5% Monthly Increase Detector", Some("Detects when price increases 5% or more from start to end of month".to_string()), detector_fn)).await?;

			tracing::info!(
				elapsed = ?timer.elapsed(),
				run_count = loaded_pipeline.state().run_count,
				detector_count = loaded_pipeline.detector_count(),
				"Pipeline loaded from saved state"
			);
		}

		tracing::info!("Pipeline API test completed successfully!");
		Ok(())
	}

	#[tokio::test]
	#[serial]
	async fn test_api_precise() -> Result<()> {
		// Initialize tracing subscriber for test output
		// Filter to reduce noise: INFO for external crates, DEBUG for our code
		let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,dataset_management=debug,database=debug,splimes=info,turso_core=warn"))).with_test_writer().try_init();

		// Skip this test if running in CI or if we want fast feedback
		if std::env::var("SKIP_SLOW_TESTS").is_ok() {
			tracing::info!("Skipping test_api_precise due to SKIP_SLOW_TESTS environment variable");
			return Ok(());
		}
		let db = fake_database().await;
		let subjects = db.list_subjects().await?;
		let subject = subjects.iter().find(|(_, name)| name.as_str() == "TestSubject").map(|(id, _)| *id).ok_or_else(|| anyhow::anyhow!("Subject 'TestSubject' not found"))?;
		let aspects = db.get_subject_aspects(&subject).await?;
		let aspect = aspects.iter().find(|a| a.name() == "TestAspect").ok_or_else(|| anyhow::anyhow!("Aspect 'TestAspect' not found"))?;
		let resolution = Resolution::Minutes;
		let method = Spline::Linear;
		let batch_size = 60;

		tracing::info!(aspect_id = %aspect.id(), "Aspect ID");

		build_unprocessed_queue(&db, &aspect.id(), &resolution, &method, batch_size).await?;
		build_processed_batch_queue(&db, &aspect.id()).await?;
		let processed_batches: Vec<Batch> = db.get_processed_batches(&aspect.id()).await?.try_collect().await?;
		let length = processed_batches.len();
		tracing::info!(count = length, "Processed batches count");
		if length > 0 {
			let random_index = rand::rng().random_range(0..length);
			tracing::debug!(batch = %json!(&processed_batches[random_index]), "Random processed batch");
			output_denk_format_batch(&processed_batches[random_index]);
		}

		#[rustfmt::skip]
                let mut dictionary = Dictionary::new(
                        "TestDictionary".to_string(), 
                        "A dictionary for testing purposes".to_string(), 
                        DictionaryConstraints::new( 
                                Some(Steps::new( 
                                        10,
                                        Spline::Linear 
                                )), 
                                Some(vec![
                                        VariablilityType::AveragePercentile(Variability::new(BigDecimal::from_f64(0.000_000_001).unwrap())),
                                ]) ));

		load_dictionary(&db, &aspect.id(), &mut dictionary).await?;

		tracing::info!(pattern_count = dictionary.len(), "Dictionary now contains patterns");

		// Show the pattern after dictionary import (should have 10 steps)
		if !dictionary.is_empty() {
			let first_pattern = &dictionary.patterns()[0];
			tracing::debug!(relatives_count = first_pattern.relatives().len(), "First pattern relatives");
			tracing::debug!("Pattern after dictionary import:");
			output_denk_format_pattern(first_pattern);
		}

		build_patterns_queue(&db, &aspect.id(), &mut dictionary).await?;
		let patterns: Vec<weftdb::Pattern> = Outputs::get_dictionary_patterns(&db, &aspect.id(), "TestDictionary").await?.try_collect().await?;
		let length = patterns.len();
		tracing::info!(count = length, "Patterns count");
		if length > 0 {
			let random_index = rand::rng().random_range(0..length);
			tracing::debug!(pattern = %json!(&patterns[random_index]), "Random pattern");
			output_denk_format_pattern(&patterns[random_index]);
		}

		// Peak detection event already created in fake_database()
		// No need to call create_event_and_manifestations here

		let timer = std::time::Instant::now();
		tracing::info!("Starting create_correlations_for_events...");
		create_correlations_for_events(&db, &dictionary, &aspect.id()).await?;
		tracing::info!(elapsed = ?timer.elapsed(), "Time taken for create_correlations_for_events");

		// print the number of correlations with more than 1 occurrence (using streaming)
		let mut correlation_stream = db.get_correlations(&aspect.id()).await?;
		let mut multi_occurrence_count = 0usize;
		while let Some(result) = correlation_stream.next().await {
			if let Ok(correlation) = result {
				if correlation.occurrences().len() > 1 {
					multi_occurrence_count += 1;
				}
			}
		}
		tracing::info!(count = multi_occurrence_count, "Number of correlations with more than 1 occurrence");

		// print the number of patterns with more than 1 occurrence
		tracing::info!(count = dictionary.patterns().iter().filter(|p| p.occurrences().len() > 1).count(), "Number of patterns with more than 1 occurrence");

		let timer = std::time::Instant::now();
		tracing::info!("Starting create_signals...");
		create_signals(&db, &aspect.id()).await?;
		tracing::info!(elapsed = ?timer.elapsed(), "Time taken for create_signals");

		// print the number of signals created
		let signals_lock = SIGNALS_QUEUE.lock().await;
		tracing::info!(count = signals_lock.len(), "Number of signals created");
		drop(signals_lock);

		let start_time = Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap();

		// Filter expired signals up to the last known data point (hour 64)
		// This removes signals whose predicted events have already occurred in the historical data.
		// The last peak was at hour 61, so signals predicting peaks at hours 1, 5, 9, ..., 61 are all resolved.
		let last_known_time = start_time + chrono::Duration::hours(64);
		tracing::debug!(last_known_time = %last_known_time, "Filtering expired signals up to last known time");
		filter_expired_signals(&db, &aspect.id(), Some(last_known_time)).await?;

		let signals_lock = SIGNALS_QUEUE.lock().await;
		tracing::info!(count = signals_lock.len(), "Number of signals after filtering");
		// Debug: show sample signals AFTER filtering to understand what remains
		for (idx, sig) in signals_lock.values().take(5).enumerate() {
			tracing::debug!(idx = idx, manifestation_date = %sig.manifestation_date(), signal_type = ?sig.signal_type(), "Post-filter signal");
		}
		drop(signals_lock);

		// Calculate and print the probability for the Signals in the queue for hours 65-70
		let events = get_events_queue(&db, &aspect.id()).await?;
		let event_id_option = events.first().map(|e| e.id().clone());
		drop(events);

		if let Some(event_id) = event_id_option {
			// Get the first correlation to show error rate
			let correlations: Vec<weftdb::Correlation> = db.get_correlations(&aspect.id()).await?.try_collect().await?;
			let signal_type = SignalType::PredictStart;
			let error_rate = correlations.first().and_then(|c| c.error_rate().get(&signal_type).cloned()).unwrap_or_else(|| weftdb::Distance::new(BigDecimal::from(0), splimes::Resolution::Seconds));

			for hour in 65..=70 {
				let query_time = start_time + chrono::Duration::hours(hour);

				// Note: Not filtering signals during future queries - probability should accumulate
				// E.g., at hour 69 (8 hours after peak at 61), prob = 8hr/4hr = 2.0

				// print the number of signals
				let signals_lock = SIGNALS_QUEUE.lock().await;
				tracing::debug!(hour = hour, count = signals_lock.len(), "Number of signals for hour");

				let Some(sum_probability) = signals_lock.probability_sum(&event_id, &signal_type, query_time, &db, &aspect.id()).await? else {
					tracing::debug!(event_id = %event_id, hour = hour, "No signals found for event at hour");
					drop(signals_lock);
					continue;
				};

				let Some(avg_probability) = signals_lock.probability_average(&event_id, &signal_type, query_time, &db, &aspect.id()).await? else {
					tracing::debug!(event_id = %event_id, hour = hour, "No signals found for event at hour");
					drop(signals_lock);
					continue;
				};

				// Also calculate event-based probability (time since last manifestation)
				let event_prob = signals_lock.event_probability(&event_id, &signal_type, query_time, &db, &aspect.id()).await?;

				// Format output nicely
				let event_prob_str = event_prob.map_or_else(|| "N/A".to_string(), |p| format!("{p:.4}"));
				tracing::info!(
					hour = hour,
					event_based = %event_prob_str,
					average = %format!("{:.4}", avg_probability),
					sum = %format!("{:.4}", sum_probability),
					error_rate = %error_rate.value(),
					"Peak Event Probability"
				);
				drop(signals_lock);
			}
		}
		Ok(())
	}

	/// Test using the Pipeline API with `fake_database` - mirrors `test_api_precise`
	#[tokio::test]
	#[serial]
	async fn test_pipeline_api_precise() -> Result<()> {
		// Initialize tracing subscriber for test output
		let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,dataset_management=debug,database=debug,splimes=info,turso_core=warn"))).with_test_writer().try_init();

		// Skip this test if running in CI or if we want fast feedback
		if std::env::var("SKIP_SLOW_TESTS").is_ok() {
			tracing::info!("Skipping test_pipeline_api_precise due to SKIP_SLOW_TESTS environment variable");
			return Ok(());
		}

		let db = fake_database().await;
		let subjects = db.list_subjects().await?;
		let subject = subjects.iter().find(|(_, name)| name.as_str() == "TestSubject").map(|(id, _)| *id).ok_or_else(|| anyhow::anyhow!("Subject 'TestSubject' not found"))?;
		let aspects = db.get_subject_aspects(&subject).await?;
		let aspect = aspects.iter().find(|a| a.name() == "TestAspect").ok_or_else(|| anyhow::anyhow!("Aspect 'TestAspect' not found"))?;
		let aspect_id = aspect.id();

		tracing::info!(aspect_id = %aspect_id, "Aspect ID");

		// Build the pipeline using the fluent builder API
		// Note: fake_database already creates the peak detection event, so we don't register a detector here
		#[rustfmt::skip]
		let dictionary_constraints = DictionaryConstraints::new(
			Some(Steps::new(
				10,
				Spline::Linear
			)),
			Some(vec![
				VariablilityType::AveragePercentile(Variability::new(BigDecimal::from_f64(0.000_000_001).unwrap())),
			])
		);

		let mut pipeline = Pipeline::builder(db.clone(), aspect_id)
			.spline_method(Spline::Linear)
			.batch_size(60)
			.add_dictionary("TestDictionary", "A dictionary for testing purposes", dictionary_constraints)
			// Note: Not registering peak detector since fake_database already creates events
			.build()
			.await?;

		tracing::info!("Running pipeline steps individually...");

		// Run data preparation
		let timer = std::time::Instant::now();
		pipeline.prepare_data().await?;
		tracing::info!(elapsed = ?timer.elapsed(), "Data preparation completed");

		// Check processed batches
		let processed_batches: Vec<Batch> = db.get_processed_batches(&aspect_id).await?.try_collect().await?;
		let length = processed_batches.len();
		tracing::info!(count = length, "Processed batches count");
		if length > 0 {
			let random_index = rand::rng().random_range(0..length);
			tracing::debug!(batch = %json!(&processed_batches[random_index]), "Random processed batch");
			output_denk_format_batch(&processed_batches[random_index]);
		}

		// Run pattern extraction
		let timer = std::time::Instant::now();
		pipeline.extract_patterns().await?;
		tracing::info!(elapsed = ?timer.elapsed(), pattern_count = pipeline.get_dictionary("TestDictionary").map_or(0, weftdb::Dictionary::len), "Pattern extraction completed");

		// Show the pattern after dictionary import
		if let Some(dict) = pipeline.get_dictionary("TestDictionary") {
			if !dict.is_empty() {
				let first_pattern = &dict.patterns()[0];
				tracing::debug!(relatives_count = first_pattern.relatives().len(), "First pattern relatives");
				tracing::debug!("Pattern after dictionary import:");
				output_denk_format_pattern(first_pattern);
			}
		}

		// Check patterns in database
		let patterns: Vec<weftdb::Pattern> = Outputs::get_dictionary_patterns(&db, &aspect_id, "TestDictionary").await?.try_collect().await?;
		let length = patterns.len();
		tracing::info!(count = length, "Patterns count");
		if length > 0 {
			let random_index = rand::rng().random_range(0..length);
			tracing::debug!(pattern = %json!(&patterns[random_index]), "Random pattern");
			output_denk_format_pattern(&patterns[random_index]);
		}

		// Peak detection event already created in fake_database()
		// Skip detect_events step since we're using pre-created events

		// Run correlation creation
		let timer = std::time::Instant::now();
		tracing::info!("Starting correlate_events via pipeline...");
		pipeline.correlate_events().await?;
		tracing::info!(elapsed = ?timer.elapsed(), "Correlation creation completed");

		// Check correlations
		let mut correlation_stream = db.get_correlations(&aspect_id).await?;
		let mut multi_occurrence_count = 0usize;
		while let Some(result) = correlation_stream.next().await {
			if let Ok(correlation) = result {
				if correlation.occurrences().len() > 1 {
					multi_occurrence_count += 1;
				}
			}
		}
		tracing::info!(count = multi_occurrence_count, "Number of correlations with more than 1 occurrence");
		let dict_multi_occ_count_2 = pipeline.get_dictionary("TestDictionary").map_or(0, |d| d.patterns().iter().filter(|p| p.occurrences().len() > 1).count());
		tracing::info!(count = dict_multi_occ_count_2, "Number of patterns with more than 1 occurrence");

		// Run signal generation
		let timer = std::time::Instant::now();
		tracing::info!("Starting generate_signals via pipeline...");
		pipeline.generate_signals().await?;
		tracing::info!(elapsed = ?timer.elapsed(), signals_count = pipeline.signals().len(), "Signal generation completed");

		// Define timeline
		let start_time = Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap();

		// Filter expired signals up to the last known data point (hour 64)
		let last_known_time = start_time + chrono::Duration::hours(64);
		tracing::debug!(last_known_time = %last_known_time, "Filtering expired signals up to last known time");
		pipeline.filter_signals_at(last_known_time).await?;

		tracing::info!(count = pipeline.signals().len(), "Number of signals after filtering");
		// Debug: show sample signals AFTER filtering
		for (idx, sig) in pipeline.signals().values().take(5).enumerate() {
			tracing::debug!(idx = idx, manifestation_date = %sig.manifestation_date(), signal_type = ?sig.signal_type(), "Post-filter signal");
		}

		// Calculate and print the probability for the Signals in the queue for hours 65-70
		let events = get_events_queue(&db, &aspect_id).await?;
		let event_id_option = events.first().map(|e| e.id().clone());
		drop(events);

		if let Some(event_id) = event_id_option {
			// Get the first correlation to show error rate
			let correlations: Vec<weftdb::Correlation> = db.get_correlations(&aspect_id).await?.try_collect().await?;
			let signal_type = SignalType::PredictStart;
			let error_rate = correlations.first().and_then(|c| c.error_rate().get(&signal_type).cloned()).unwrap_or_else(|| weftdb::Distance::new(BigDecimal::from(0), splimes::Resolution::Seconds));

			for hour in 65..=70 {
				let query_time = start_time + chrono::Duration::hours(hour);

				tracing::debug!(hour = hour, count = pipeline.signals().len(), "Number of signals for hour");

				// Use pipeline's query_probability method
				let prob_result = pipeline.query_probability(&event_id, &signal_type, query_time).await?;

				if prob_result.is_empty() {
					tracing::debug!(event_id = %event_id, hour = hour, "No signals found for event at hour");
					continue;
				}

				// Format output nicely
				let event_prob_str = prob_result.event_based.as_ref().map_or_else(|| "N/A".to_string(), |p| format!("{p:.4}"));
				let avg_str = prob_result.average.as_ref().map_or_else(|| "N/A".to_string(), |p| format!("{p:.4}"));
				let sum_str = prob_result.sum.as_ref().map_or_else(|| "N/A".to_string(), |p| format!("{p:.4}"));

				tracing::info!(
					hour = hour,
					event_based = %event_prob_str,
					average = %avg_str,
					sum = %sum_str,
					error_rate = %error_rate.value(),
					"Peak Event Probability (via Pipeline)"
				);
			}
		}

		tracing::info!("Pipeline API precise test completed successfully!");
		Ok(())
	}

	/// Generate test data points with peaks every 4 hours
	fn generate_test_points() -> Vec<(i32, BigDecimal)> {
		(0..=64).map(|i| {
			let value = if i % 4 == 1 { BigDecimal::from(1) } else { BigDecimal::from(0) };
			(i, value)
		})
		.collect()
	}

	async fn fake_database() -> Database {
		// Cleanup existing test database if it exists
		use std::fs::remove_dir_all;

		use weftdb::{clear_connection_cache_by_name, DATABASES};

		// Clear any cached database entry for TestDB before recreating
		{
			let mut databases = DATABASES.lock().await;
			// Find and remove any existing TestDB entry by name
			let keys_to_remove: Vec<_> = databases.iter().filter(|(_, info)| info.name() == "TestDB").map(|(id, _)| *id).collect();
			for key in keys_to_remove {
				databases.remove(&key);
			}
		}

		// Also clear the connection cache to release file handles
		clear_connection_cache_by_name("TestDB").await;

		let db_path = format!("{}/TestDB", data_dir());
		remove_dir_all(&db_path).ok();

		let db = Database::new("TestDB").await.unwrap();
		let test_subject = db.observe_subject("TestSubject").await.unwrap();
		let test_aspect = db.track_aspect(&test_subject.id(), "TestAspect", &Resolution::Seconds, None).await.unwrap();
		let start_time = Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap();

		for (i, value) in generate_test_points() {
			let timestamp = start_time + chrono::Duration::hours(i64::from(i));
			let measurement = InputMeasurement::new(timestamp, value);
			db.capture_measurement(&test_aspect.id(), &DatasetId::new(), &measurement).await.unwrap();
		}

		// create a peak detection event for testing
		create_peak_detection_events(&db, &test_aspect.id(), &Resolution::Hours, &Spline::Linear, "Peak Detection Test").await.unwrap();
		tracing::info!("Events in database after peak detection:");
		let events = get_events_queue(&db, &test_aspect.id()).await.unwrap();
		tracing::info!(events = events.len(), "Events in database");
		drop(events);

		db
	}

	#[tokio::test]
	#[serial]
	async fn test_batch_processing() -> Result<()> {
		use std::fs::remove_dir_all;

		// Skip this test if running in CI or if we want fast feedback
		if std::env::var("SKIP_SLOW_TESTS").is_ok() {
			tracing::info!("Skipping test_batch_processing due to SKIP_SLOW_TESTS environment variable");
			return Ok(());
		}

		let db_path = format!("{}/test_bath_processing", data_dir());
		remove_dir_all(&db_path).ok();

		let db = Database::new("test_bath_processing").await.unwrap();
		let test_subject = db.observe_subject("TestSubject").await.unwrap();
		let test_aspect = db.track_aspect(&test_subject.id(), "TestAspect", &Resolution::Seconds, None).await.unwrap();
		let start_time = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
		#[rustfmt::skip]
		let points = vec![
                        (1, BigDecimal::from(1)),
                        (2, BigDecimal::from(2)),
                        (3, BigDecimal::from(3)),
                        (4, BigDecimal::from(2)),
                        (5, BigDecimal::from(3)),
                        (6, BigDecimal::from(5)),
                        (7, BigDecimal::from(3)),
                        (8, BigDecimal::from(2)),
                        (9, BigDecimal::from(1)),
                        (10, BigDecimal::from(1)),
                        (11, BigDecimal::from(0)),
                        (12, BigDecimal::from(-1)),
                        (13, BigDecimal::from(0)),
                        (14, BigDecimal::from(1)),
                        (15, BigDecimal::from(0))
                ];

		for (i, value) in points {
			let timestamp = start_time + chrono::Duration::minutes(i64::from(i));
			let measurement = InputMeasurement::new(timestamp, value);
			db.capture_measurement(&test_aspect.id(), &DatasetId::new(), &measurement).await.unwrap();
		}

		let aspect = test_aspect;
		let resolution = Resolution::Minutes;
		let method = Spline::Linear;
		let batch_size = 10;

		// First, create unprocessed batches from the measurements
		build_unprocessed_queue(&db, &aspect.id(), &resolution, &method, batch_size).await?;

		// Check how many batches were stored in the database
		let unprocessed_batches: Vec<weftdb::Batch> = db.get_unprocessed_batches(&aspect.id()).await?.try_collect().await?;
		tracing::info!(count = unprocessed_batches.len(), "Unprocessed batches");

		// Process the unprocessed batches
		build_processed_batch_queue(&db, &aspect.id()).await?;

		let processed_batches: Vec<Batch> = db.get_processed_batches(&aspect.id()).await?.try_collect().await?;
		tracing::info!(count = processed_batches.len(), "Processed batches");

		// With 15 points and batch size 10, sliding window creates: 15 - 10 + 1 = 6 overlapping batches
		assert_eq!(processed_batches.len(), 6);

		Ok(())
	}

	#[tokio::test]
	#[serial]
	async fn test_specific_process_batch() -> Result<()> {
		use std::fs::remove_dir_all;

		// Skip this test if running in CI or if we want fast feedback
		if std::env::var("SKIP_SLOW_TESTS").is_ok() {
			tracing::info!("Skipping test_specific_process_batch due to SKIP_SLOW_TESTS environment variable");
			return Ok(());
		}

		let db_path = format!("{}/test_specific_process_batch", data_dir());
		remove_dir_all(&db_path).ok();

		let db = Database::new("test_specific_process_batch").await.unwrap();
		let test_subject = db.observe_subject("TestSubject").await.unwrap();
		let test_aspect = db.track_aspect(&test_subject.id(), "TestAspect", &Resolution::Seconds, None).await.unwrap();
		let start_time = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
		#[rustfmt::skip]
		let points = vec![
                        (1, BigDecimal::from(1)),
                        (2, BigDecimal::from(2)),
                        (3, BigDecimal::from(3)),
                        (4, BigDecimal::from(2)),
                        (5, BigDecimal::from(3)),
                        (6, BigDecimal::from(5)),
                        (7, BigDecimal::from(3)),
                        (8, BigDecimal::from(2)),
                        (9, BigDecimal::from(1)),
                        (10, BigDecimal::from(1)),
                        (11, BigDecimal::from(0)),
                        (12, BigDecimal::from(-1)),
                        (13, BigDecimal::from(0)),
                        (14, BigDecimal::from(1)),
                        (15, BigDecimal::from(0))
                ];

		for (i, value) in points {
			let timestamp = start_time + chrono::Duration::minutes(i64::from(i));
			let measurement = InputMeasurement::new(timestamp, value);
			db.capture_measurement(&test_aspect.id(), &DatasetId::new(), &measurement).await.unwrap();
		}

		let aspect = test_aspect;
		let resolution = Resolution::Minutes;
		let method = Spline::Linear;
		let batch_size = 10;

		// First, create unprocessed batches from the measurements
		build_unprocessed_queue(&db, &aspect.id(), &resolution, &method, batch_size).await?;

		// Check how many batches were stored in the database
		let unprocessed_batches: Vec<weftdb::Batch> = db.get_unprocessed_batches(&aspect.id()).await?.try_collect().await?;
		tracing::info!(count = unprocessed_batches.len(), "Unprocessed batches");

		// Process the unprocessed batches
		build_processed_batch_queue(&db, &aspect.id()).await?;

		let processed_batches: Vec<Batch> = db.get_processed_batches(&aspect.id()).await?.try_collect().await?;
		tracing::info!(count = processed_batches.len(), "Processed batches");

		// With 15 points and batch size 10, sliding window creates: 15 - 10 + 1 = 6 overlapping batches
		assert_eq!(processed_batches.len(), 6);

		Ok(())
	}

	async fn create_peak_detection_events(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, name: &str) -> Result<()> {
		let start_time = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
		let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;

		// Use optimized bulk analysis to get all points
		let mut point_stream = Outputs::analyze_range(database, aspect, start_time, end_time, *resolution, *method).await?;

		let mut points = Vec::new();
		while let Some(result) = point_stream.next().await {
			let point = result?;
			points.push(point);
		}

		// Sort points by timestamp to ensure proper chronological order
		points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

		// Find the global maximum value
		let global_max = points.iter().map(|p| &p.value).max().unwrap();

		let mut event = Event::new(None, name.to_string(), Some(format!("Detects peak values (local maxima that are also global maximum) for aspect {aspect}")), None);

		// Check each point to see if it's a peak
		for i in 1..points.len() - 1 {
			let prev_value = &points[i - 1].value;
			let curr_value = &points[i].value;
			let next_value = &points[i + 1].value;

			// Check if current point is a local peak AND equals global maximum
			if curr_value == global_max && curr_value > prev_value && curr_value > next_value {
				let database_info = database.get_database_info().await.expect("Database info should be available");

				// Create manifestation spanning from the previous point (start of rise) to next point (start of decline)
				let manifestation = Manifestation::new(
					database_info.id().as_uuid(),
					points[i - 1].timestamp, // Start of rise to peak
					points[i + 1].timestamp, // Start of decline from peak
				);

				event.add_manifestation(manifestation);
			}
		}

		// Only add the event if we found at least one peak
		if event.manifestations().is_empty() {
			tracing::info!("No peaks detected in the dataset");
		} else {
			let manifestation_count = event.manifestations().len();
			Inputs::insert_unprocessed_event(database, aspect, &event).await?;
			tracing::info!(manifestations = manifestation_count, "Added peak detection event");
		}

		Ok(())
	}

	#[allow(dead_code)]
	pub async fn get_events_queue(database: &Database, aspect_id: &AspectId) -> Result<Vec<Event>> {
		let events: Vec<Event> = Outputs::get_unprocessed_events(database, aspect_id).await?.try_collect().await?;
		Ok(events)
	}

	fn output_denk_format_batch(batch: &Batch) {
		tracing::debug!("----- DENK FORMAT OUTPUT BEGIN -----");
		for (count, measurement) in batch.clone().into_iter().enumerate() {
			if count == 0 || count == batch.size() - 1 {
				let vector = measurement.vector().unwrap();
				let amplitude = vector.amplitude().round(2);
				tracing::debug!(location = %vector.location(), amplitude = %amplitude, "DENK point");
			}
		}
		tracing::debug!("----- DENK FORMAT OUTPUT END -----");
	}

	fn output_denk_format_pattern(pattern: &Pattern) {
		tracing::debug!("----- DENK FORMAT OUTPUT BEGIN -----");
		tracing::debug!(pattern_id = %pattern.id(), "Pattern");
		for relative in pattern.relatives() {
			tracing::debug!(location = %relative.vector().location().round(2), amplitude = %relative.vector().amplitude().round(2), "DENK relative");
		}
		tracing::debug!("----- DENK FORMAT OUTPUT END -----");
		let zero = BigDecimal::zero();
		tracing::debug!(
			max_x = %pattern.relatives().iter().map(|r| r.vector().location()).max().unwrap_or(&zero),
			max_y = %pattern.relatives().iter().map(|r| r.vector().amplitude()).max().unwrap_or(&zero),
			"Pattern bounds"
		);
	}

	/// Test for the new Pipeline API using fake database
	#[tokio::test]
	#[serial]
	async fn test_pipeline_api_with_fake_db() -> Result<()> {
		// Initialize tracing
		let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,dataset_management=debug,database=debug"))).with_test_writer().try_init();

		// Skip this test if running in CI
		if std::env::var("SKIP_SLOW_TESTS").is_ok() {
			tracing::info!("Skipping test_pipeline_api_with_fake_db due to SKIP_SLOW_TESTS environment variable");
			return Ok(());
		}

		// Use the fake database for testing
		let db = fake_database().await;
		let subjects = db.list_subjects().await?;
		let subject = subjects.iter().find(|(_, name)| name.as_str() == "TestSubject").map(|(id, _)| *id).ok_or_else(|| anyhow::anyhow!("Subject 'TestSubject' not found"))?;
		let aspects = db.get_subject_aspects(&subject).await?;
		let aspect = aspects.iter().find(|a| a.name() == "TestAspect").ok_or_else(|| anyhow::anyhow!("Aspect 'TestAspect' not found"))?;

		tracing::info!("=== Testing Pipeline Builder ===");

		// Build a pipeline using the new API
		let mut pipeline = Pipeline::builder(db.clone(), aspect.id())
			.spline_method(Spline::Linear)
			.batch_size(60)
			.add_dictionary("PipelineTestDictionary", "Pipeline test dictionary", DictionaryConstraints::new(Some(Steps::new(10, Spline::Linear)), Some(vec![VariablilityType::AveragePercentile(Variability::new(BigDecimal::from_f64(0.000_000_001).unwrap()))])))
			// Register the built-in peak detector
			.with_peak_detector("Pipeline Peak Detection")
			.build()
			.await?;

		tracing::info!(detector_count = pipeline.detector_count(), "Pipeline built with detectors");

		// Verify detectors are registered
		assert_eq!(pipeline.detector_count(), 1);
		assert!(pipeline.get_detector(&DetectorId::new("peak_pipeline_peak_detection")).is_some());

		tracing::info!("=== Running Pipeline ===");

		// Run the full pipeline
		pipeline.run().await?;

		// Verify pipeline completed
		assert!(pipeline.state().run_count > 0);
		assert!(pipeline.state().last_run.is_some());

		tracing::info!(
			run_count = pipeline.state().run_count,
			last_run = ?pipeline.state().last_run,
			pattern_count = pipeline.get_dictionary("PipelineTestDictionary").map_or(0, weftdb::Dictionary::len),
			signal_count = pipeline.signals().len(),
			"Pipeline completed"
		);

		tracing::info!("=== Testing Persistence ===");

		// Test state persistence
		assert!(Pipeline::exists(&db, &aspect.id()).await?);

		// Load the pipeline state
		let mut loaded_pipeline = Pipeline::load(db.clone(), &aspect.id()).await?;

		// Verify loaded state matches
		assert_eq!(loaded_pipeline.state().run_count, pipeline.state().run_count);
		assert_eq!(loaded_pipeline.config().batch_size, pipeline.config().batch_size);

		tracing::info!(run_count = loaded_pipeline.state().run_count, "Loaded pipeline state");

		// Detectors need to be re-registered after loading
		assert_eq!(loaded_pipeline.detector_count(), 0);

		// Re-register the detector
		loaded_pipeline.register_detector(EventDetector::builtin("peak_pipeline_peak_detection", "Pipeline Peak Detection", Some("Detects peak values".to_string()), "peak_detector", None, std::sync::Arc::new(move |db, aspect, resolution, method| Box::pin(async move { crate::detectors::detect_peaks(db, aspect, resolution, method, "Pipeline Peak Detection").await })))).await?;

		assert_eq!(loaded_pipeline.detector_count(), 1);

		tracing::info!("=== Testing Detector Management ===");

		// Test removing a detector
		let removed = loaded_pipeline.remove_detector(&DetectorId::new("peak_pipeline_peak_detection")).await?;
		assert!(removed);
		assert_eq!(loaded_pipeline.detector_count(), 0);

		// Test clear_detectors
		loaded_pipeline.register_detector(EventDetector::builtin("test1", "Test 1", None, "test_builtin", None, std::sync::Arc::new(|_, _, _, _| Box::pin(async { Ok(vec![]) })))).await?;
		loaded_pipeline.register_detector(EventDetector::builtin("test2", "Test 2", None, "test_builtin", None, std::sync::Arc::new(|_, _, _, _| Box::pin(async { Ok(vec![]) })))).await?;
		assert_eq!(loaded_pipeline.detector_count(), 2);
		loaded_pipeline.clear_detectors().await?;
		assert_eq!(loaded_pipeline.detector_count(), 0);

		// Cleanup: delete the pipeline
		loaded_pipeline.delete().await?;
		assert!(!Pipeline::exists(&db, &aspect.id()).await?);

		tracing::info!("test_pipeline_api completed successfully!");
		Ok(())
	}

	#[tokio::test]
	#[serial]
	async fn test_load_or_create_pipeline() -> Result<()> {
		use std::fs::remove_dir_all;

		use weftdb::clear_connection_cache_by_name;

		tracing::info!("=== Testing Pipeline::load_or_create ===");

		// Cleanup
		{
			let mut databases = weftdb::DATABASES.lock().await;
			let keys_to_remove: Vec<_> = databases.iter().filter(|(_, info)| info.name() == "TestLoadOrCreate").map(|(id, _)| *id).collect();
			for key in keys_to_remove {
				databases.remove(&key);
			}
		}
		clear_connection_cache_by_name("TestLoadOrCreate").await;
		let db_path = format!("{}/TestLoadOrCreate", data_dir());
		remove_dir_all(&db_path).ok();

		// Create database with test data
		let db = Database::new("TestLoadOrCreate").await?;
		let subject = db.observe_subject("TestSubject").await?;
		let aspect = db.track_aspect(&subject.id(), "TestAspect", &Resolution::Hours, None).await?;

		// Add some measurements
		let start_time = chrono::Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap();
		for i in 0..10 {
			let timestamp = start_time + chrono::Duration::hours(i);
			let value = if i % 4 == 1 { BigDecimal::from(1) } else { BigDecimal::from(0) };
			let measurement = InputMeasurement::new(timestamp, value);
			db.capture_measurement(&aspect.id(), &DatasetId::new(), &measurement).await?;
		}

		// Test load_or_create on a new aspect (should create default)
		let pipeline = Pipeline::load_or_create(db.clone(), &aspect.id()).await?;
		assert!(pipeline.is_dormant(), "New pipeline should be dormant");
		assert!(Pipeline::exists(&db, &aspect.id()).await?, "Pipeline should exist after creation");

		tracing::info!("Pipeline created as dormant, testing runtime config...");

		// Test applying runtime config
		let mut pipeline = Pipeline::load_or_create(db.clone(), &aspect.id()).await?;
		let config = PipelineRunConfig::new().with_peak_detector("Test Peaks").batch_size(4);

		pipeline.apply_runtime_config(config).await?;
		assert!(!pipeline.is_dormant(), "Pipeline should be configured after applying config");
		assert_eq!(pipeline.detector_count(), 1);
		assert_eq!(pipeline.config().batch_size, 4);

		tracing::info!("Runtime config applied successfully");

		// Cleanup
		pipeline.delete().await?;

		tracing::info!("test_load_or_create_pipeline completed successfully!");
		Ok(())
	}

	#[tokio::test]
	#[serial]
	async fn test_run_subject_pipelines() -> Result<()> {
		use std::fs::remove_dir_all;

		use weftdb::clear_connection_cache_by_name;

		tracing::info!("=== Testing run_subject_pipelines ===");

		// Cleanup
		{
			let mut databases = weftdb::DATABASES.lock().await;
			let keys_to_remove: Vec<_> = databases.iter().filter(|(_, info)| info.name() == "TestParallelPipelines").map(|(id, _)| *id).collect();
			for key in keys_to_remove {
				databases.remove(&key);
			}
		}
		clear_connection_cache_by_name("TestParallelPipelines").await;
		let db_path = format!("{}/TestParallelPipelines", data_dir());
		remove_dir_all(&db_path).ok();

		// Create database with multiple aspects
		let db = Database::new("TestParallelPipelines").await?;
		let subject = db.observe_subject("TestSubject").await?;

		// Create 3 aspects
		let aspect1 = db.track_aspect(&subject.id(), "Aspect1", &Resolution::Hours, None).await?;
		let aspect2 = db.track_aspect(&subject.id(), "Aspect2", &Resolution::Hours, None).await?;
		let aspect3 = db.track_aspect(&subject.id(), "Aspect3", &Resolution::Hours, None).await?;

		// Add measurements to each aspect
		let start_time = chrono::Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap();
		for aspect in [&aspect1, &aspect2, &aspect3] {
			for i in 0..20 {
				let timestamp = start_time + chrono::Duration::hours(i);
				let value = if i % 4 == 1 { BigDecimal::from(1) } else { BigDecimal::from(0) };
				let measurement = InputMeasurement::new(timestamp, value);
				db.capture_measurement(&aspect.id(), &DatasetId::new(), &measurement).await?;
			}
		}

		tracing::info!("Created 3 aspects with measurements");

		// Run all pipelines for the subject in parallel
		let results = crate::run_subject_pipelines(&db, &subject.id(), |aspect| PipelineRunConfig::new().with_peak_detector(format!("{} Peaks", aspect.name())).batch_size(4)).await?;

		tracing::info!(result_count = results.len(), "Pipeline run completed");

		assert_eq!(results.len(), 3, "Should have 3 results");

		// All pipelines should have completed (some may have errors if no patterns found)
		for result in &results {
			tracing::info!(
				aspect_id = %result.aspect_id,
				success = result.is_success(),
				signal_count = result.signal_count,
				elapsed = ?result.elapsed,
				error = ?result.error,
				"Pipeline result"
			);
		}

		tracing::info!("test_run_subject_pipelines completed successfully!");
		Ok(())
	}
}
