//! Pipeline API for dataset processing and signal generation.
//!
//! This module provides a high-level API for processing time-series data,
//! extracting patterns, detecting events, and generating prediction signals.
//!
//! # Example
//!
//! ```ignore
//! use dataset_management::{Pipeline, event_detector_fn};
//! use database::{Database, Resolution};
//! use splimes::Spline;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let database = Database::existing("MyDatabase").await?;
//!     let aspect_id = /* get aspect id */;
//!
//!     let mut pipeline = Pipeline::builder(database, aspect_id)
//!         .resolution(Resolution::Hours)
//!         .batch_size(24)
//!         .with_monthly_increase_detector(0.05)
//!         .build()
//!         .await?;
//!
//!     pipeline.run().await?;
//!     Ok(())
//! }
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use database::{
    database::traits::{DatabaseStructure, Inputs, Outputs},
    AspectId, Database, EventID, Resolution,
};
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use splimes::Spline;

use crate::{
    Dictionary, DictionaryConstraints, Event, SignalType, Signals, Steps, Variability,
    VariablilityType, SIGNALS_QUEUE,
};

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
pub type EventDetectorFn = Arc<
    dyn for<'a> Fn(
            &'a Database,
            &'a AspectId,
            &'a Resolution,
            &'a Spline,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<Event>>> + Send + 'a>>
        + Send
        + Sync,
>;

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
/// use dataset_management::{event_detector_fn, EventDetectorFn};
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
/// use dataset_management::pipeline::EventDetectorFn;
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
        std::sync::Arc::new(move |db, aspect, res, method| {
            Box::pin($func(db, aspect, res, method)) as std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Vec<database::Event>>> + Send + '_>>
        }) as $crate::pipeline::EventDetectorFn
    };
}

/// Metadata for a registered event detector.
#[derive(Clone)]
pub struct EventDetector {
    id: DetectorId,
    name: String,
    description: Option<String>,
    detector_fn: EventDetectorFn,
}

impl EventDetector {
    /// Creates a new event detector with metadata.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        description: Option<String>,
        detector_fn: EventDetectorFn,
    ) -> Self {
        Self {
            id: DetectorId::new(id),
            name: name.into(),
            description,
            detector_fn,
        }
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

    /// Runs the detector.
    ///
    /// # Errors
    ///
    /// Returns an error if the detector function fails.
    pub async fn detect(
        &self,
        database: &Database,
        aspect_id: &AspectId,
        resolution: &Resolution,
        method: &Spline,
    ) -> Result<Vec<Event>> {
        (self.detector_fn)(database, aspect_id, resolution, method).await
    }
}

/// Serializable pipeline configuration that can be persisted.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PipelineConfig {
    /// The resolution for data analysis.
    pub resolution: Resolution,
    /// The spline interpolation method.
    pub spline_method: Spline,
    /// The batch size for processing.
    pub batch_size: usize,
    /// Dictionary constraints for pattern matching.
    pub dictionary_constraints: DictionaryConstraints,
    /// Dictionary name.
    pub dictionary_name: String,
    /// Dictionary description.
    pub dictionary_description: String,
    /// IDs of registered event detectors (for persistence reference).
    pub detector_ids: Vec<DetectorId>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            resolution: Resolution::Hours,
            spline_method: Spline::Linear,
            batch_size: 24,
            dictionary_constraints: DictionaryConstraints::new(
                Some(Steps::new(10, Spline::Linear)),
                Some(vec![VariablilityType::MaximumStatic(Variability::new(
                    BigDecimal::from(1) / BigDecimal::from(10),
                ))]),
            ),
            dictionary_name: "DefaultDictionary".to_string(),
            dictionary_description: "Auto-generated dictionary".to_string(),
            detector_ids: Vec::new(),
        }
    }
}

/// Serializable pipeline state for persistence.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PipelineState {
    /// Pipeline configuration.
    pub config: PipelineConfig,
    /// The aspect ID this pipeline is associated with.
    pub aspect_id: AspectId,
    /// The database name.
    pub database_name: String,
    /// Last run timestamp.
    pub last_run: Option<DateTime<Utc>>,
    /// Pipeline run count.
    pub run_count: u64,
    /// Version for future compatibility.
    pub version: u32,
}

impl PipelineState {
    /// Current state version for serialization compatibility.
    pub const CURRENT_VERSION: u32 = 1;
}

/// A pipeline for processing time-series data and generating prediction signals.
///
/// The pipeline orchestrates the full data processing workflow:
/// 1. Data preparation (batching)
/// 2. Pattern extraction
/// 3. Event detection
/// 4. Correlation creation
/// 5. Signal generation
///
/// # Example
///
/// ```ignore
/// let mut pipeline = Pipeline::builder(database, aspect_id)
///     .resolution(Resolution::Hours)
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
    config: PipelineConfig,
    dictionary: Dictionary,
    signals: Signals,
    event_detectors: HashMap<DetectorId, EventDetector>,
    state: PipelineState,
}

impl Pipeline {
    /// Creates a new pipeline builder.
    #[must_use]
    pub fn builder(database: Database, aspect_id: AspectId) -> PipelineBuilder {
        PipelineBuilder::new(database, aspect_id)
    }

    /// Loads an existing pipeline state from a file.
    ///
    /// This restores the pipeline configuration and state from a previous run.
    /// Note: Event detector functions must be re-registered after loading
    /// since functions cannot be serialized.
    ///
    /// The state file is stored in the database directory as `pipeline_state_{aspect_id}.json`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The pipeline state file cannot be found
    /// - Deserialization fails
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
        // Get database info to determine state file path
        let db_info = database.get_database_info().await?;
        let state_file_path = Self::state_file_path(db_info.name(), aspect_id);

        // Load pipeline state from file
        let state_json = tokio::fs::read_to_string(&state_file_path)
            .await
            .map_err(|e| anyhow::anyhow!("No pipeline state found for aspect {aspect_id}: {e}"))?;

        let state: PipelineState = serde_json::from_str(&state_json)?;

        // Recreate dictionary from config
        let dictionary = Dictionary::new(
            state.config.dictionary_name.clone(),
            state.config.dictionary_description.clone(),
            state.config.dictionary_constraints.clone(),
        );

        tracing::info!(
            aspect_id = %aspect_id,
            run_count = state.run_count,
            last_run = ?state.last_run,
            detector_count = state.config.detector_ids.len(),
            state_file = %state_file_path.display(),
            "Loaded pipeline state"
        );

        Ok(Self {
            database,
            aspect_id: *aspect_id,
            config: state.config.clone(),
            dictionary,
            signals: Signals::new(),
            event_detectors: HashMap::new(), // Must be re-registered
            state,
        })
    }

    /// Saves the pipeline state to a file.
    ///
    /// This persists the pipeline configuration so it can be restored later.
    /// The state file is stored in the database directory as `pipeline_state_{aspect_id}.json`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Serialization fails
    /// - File write fails
    pub async fn save(&self) -> Result<()> {
        let db_info = self.database.get_database_info().await?;
        let state_file_path = Self::state_file_path(db_info.name(), &self.aspect_id);

        // Ensure parent directory exists
        if let Some(parent) = state_file_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let state_json = serde_json::to_string_pretty(&self.state)?;
        tokio::fs::write(&state_file_path, state_json).await?;

        tracing::info!(
            aspect_id = %self.aspect_id,
            run_count = self.state.run_count,
            state_file = %state_file_path.display(),
            "Pipeline state saved"
        );
        Ok(())
    }

    /// Returns the path to the state file for a given database and aspect.
    fn state_file_path(database_name: &str, aspect_id: &AspectId) -> std::path::PathBuf {
        let data_dir = std::path::PathBuf::from(database::DEFAULT_DATA_DIR);
        data_dir
            .join(database_name)
            .join(format!("pipeline_state_{aspect_id}.json"))
    }

    /// Checks if a saved pipeline state exists for the given database and aspect.
    ///
    /// # Errors
    ///
    /// Returns an error if retrieving database information fails.
    pub async fn state_exists(database: &Database, aspect_id: &AspectId) -> Result<bool> {
        let db_info = database.get_database_info().await?;
        let state_file_path = Self::state_file_path(db_info.name(), aspect_id);
        Ok(state_file_path.exists())
    }

    /// Deletes the saved pipeline state for this pipeline.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be deleted.
    pub async fn delete_state(&self) -> Result<()> {
        let db_info = self.database.get_database_info().await?;
        let state_file_path = Self::state_file_path(db_info.name(), &self.aspect_id);

        if state_file_path.exists() {
            tokio::fs::remove_file(&state_file_path).await?;
            tracing::info!(
                aspect_id = %self.aspect_id,
                state_file = %state_file_path.display(),
                "Pipeline state deleted"
            );
        }
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
        self.state.last_run = Some(Utc::now());
        self.state.run_count += 1;

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
    /// # Errors
    ///
    /// Returns an error if database operations fail.
    pub async fn prepare_data(&mut self) -> Result<()> {
        tracing::info!("Preparing data...");
        crate::batch_utils::build_unprocessed_queue(
            &self.database,
            &self.aspect_id,
            &self.config.resolution,
            &self.config.spline_method,
            self.config.batch_size,
        )
        .await?;

        crate::build_processed_batch_queue(&self.database, &self.aspect_id).await?;
        tracing::info!("Data preparation completed");
        Ok(())
    }

    /// Runs pattern extraction into the dictionary.
    ///
    /// # Errors
    ///
    /// Returns an error if pattern extraction fails.
    pub async fn extract_patterns(&mut self) -> Result<()> {
        tracing::info!("Extracting patterns...");
        crate::load_dictionary(&self.database, &self.aspect_id, &mut self.dictionary).await?;
        crate::build_patterns_queue(&self.database, &self.aspect_id, &mut self.dictionary).await?;
        // Reload dictionary to get merged patterns
        crate::load_dictionary(&self.database, &self.aspect_id, &mut self.dictionary).await?;
        tracing::info!(
            pattern_count = self.dictionary.len(),
            "Pattern extraction completed"
        );
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

        tracing::info!(
            detector_count = self.event_detectors.len(),
            "Running event detectors..."
        );

        for detector in self.event_detectors.values() {
            tracing::debug!(
                detector_id = %detector.id(),
                detector_name = %detector.name(),
                "Running detector"
            );

            let events = detector
                .detect(
                    &self.database,
                    &self.aspect_id,
                    &self.config.resolution,
                    &self.config.spline_method,
                )
                .await?;

            for event in events {
                if !event.manifestations().is_empty() {
                    self.database
                        .insert_unprocessed_event(&self.aspect_id, &event)
                        .await?;
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

    /// Runs event-pattern correlation.
    ///
    /// # Errors
    ///
    /// Returns an error if correlation creation fails.
    pub async fn correlate_events(&self) -> Result<()> {
        tracing::info!("Correlating events with patterns...");
        crate::create_correlations_for_events(&self.database, &self.dictionary, &self.aspect_id)
            .await?;
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
        let latest_time = self
            .database
            .get_latest_measurement(&self.aspect_id)
            .await?;
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
    pub async fn query_probability(
        &self,
        event_id: &EventID,
        signal_type: &SignalType,
        query_time: DateTime<Utc>,
    ) -> Result<ProbabilityResult> {
        let sum = self
            .signals
            .probability_sum(
                event_id,
                signal_type,
                query_time,
                &self.database,
                &self.aspect_id,
            )
            .await?;

        let average = self
            .signals
            .probability_average(
                event_id,
                signal_type,
                query_time,
                &self.database,
                &self.aspect_id,
            )
            .await?;

        let event_based = self
            .signals
            .event_probability(
                event_id,
                signal_type,
                query_time,
                &self.database,
                &self.aspect_id,
            )
            .await?;

        Ok(ProbabilityResult {
            sum,
            average,
            event_based,
        })
    }

    // ==================== Event Detector Management ====================

    /// Registers an event detector.
    ///
    /// If a detector with the same ID already exists, it will be replaced.
    pub fn register_detector(&mut self, detector: EventDetector) {
        let id = detector.id().clone();
        if !self.config.detector_ids.contains(&id) {
            self.config.detector_ids.push(id.clone());
        }
        // Also update the state config
        if !self.state.config.detector_ids.contains(&id) {
            self.state.config.detector_ids.push(id.clone());
        }
        self.event_detectors.insert(id.clone(), detector);
        tracing::debug!(detector_id = %id, "Registered event detector");
    }

    /// Removes an event detector by ID.
    ///
    /// Returns `true` if the detector was found and removed.
    pub fn remove_detector(&mut self, id: &DetectorId) -> bool {
        self.config.detector_ids.retain(|d| d != id);
        self.state.config.detector_ids.retain(|d| d != id);
        let removed = self.event_detectors.remove(id).is_some();
        if removed {
            tracing::debug!(detector_id = %id, "Removed event detector");
        }
        removed
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
    pub fn clear_detectors(&mut self) {
        self.event_detectors.clear();
        self.config.detector_ids.clear();
        self.state.config.detector_ids.clear();
        tracing::debug!("Cleared all event detectors");
    }

    // ==================== Accessors ====================

    /// Returns a reference to the dictionary.
    #[must_use]
    pub const fn dictionary(&self) -> &Dictionary {
        &self.dictionary
    }

    /// Returns a mutable reference to the dictionary.
    #[must_use]
    pub const fn dictionary_mut(&mut self) -> &mut Dictionary {
        &mut self.dictionary
    }

    /// Returns a reference to the signals.
    #[must_use]
    pub const fn signals(&self) -> &Signals {
        &self.signals
    }

    /// Returns a reference to the pipeline configuration.
    #[must_use]
    pub const fn config(&self) -> &PipelineConfig {
        &self.config
    }

    /// Returns the pipeline state.
    #[must_use]
    pub const fn state(&self) -> &PipelineState {
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
        let events: Vec<Event> = Outputs::get_unprocessed_events(&self.database, &self.aspect_id)
            .await?
            .try_collect()
            .await?;
        Ok(events)
    }
}

/// Builder for constructing a Pipeline with custom configuration.
pub struct PipelineBuilder {
    database: Database,
    aspect_id: AspectId,
    config: PipelineConfig,
    event_detectors: HashMap<DetectorId, EventDetector>,
}

impl PipelineBuilder {
    fn new(database: Database, aspect_id: AspectId) -> Self {
        Self {
            database,
            aspect_id,
            config: PipelineConfig::default(),
            event_detectors: HashMap::new(),
        }
    }

    /// Sets the resolution for data analysis.
    #[must_use]
    pub const fn resolution(mut self, resolution: Resolution) -> Self {
        self.config.resolution = resolution;
        self
    }

    /// Sets the spline interpolation method.
    #[must_use]
    pub const fn spline_method(mut self, method: Spline) -> Self {
        self.config.spline_method = method;
        self
    }

    /// Sets the batch size for processing.
    #[must_use]
    pub const fn batch_size(mut self, size: usize) -> Self {
        self.config.batch_size = size;
        self
    }

    /// Sets the dictionary constraints.
    #[must_use]
    pub fn dictionary_constraints(mut self, constraints: DictionaryConstraints) -> Self {
        self.config.dictionary_constraints = constraints;
        self
    }

    /// Sets the dictionary name.
    #[must_use]
    pub fn dictionary_name(mut self, name: impl Into<String>) -> Self {
        self.config.dictionary_name = name.into();
        self
    }

    /// Sets the dictionary description.
    #[must_use]
    pub fn dictionary_description(mut self, description: impl Into<String>) -> Self {
        self.config.dictionary_description = description.into();
        self
    }

    /// Registers an event detector with full metadata.
    #[must_use]
    pub fn with_detector(mut self, detector: EventDetector) -> Self {
        let id = detector.id().clone();
        if !self.config.detector_ids.contains(&id) {
            self.config.detector_ids.push(id.clone());
        }
        self.event_detectors.insert(id, detector);
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
    pub fn with_detector_fn(
        self,
        id: impl Into<String>,
        name: impl Into<String>,
        detector_fn: EventDetectorFn,
    ) -> Self {
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

        let detector_fn: EventDetectorFn = Arc::new(move |db, aspect, resolution, method| {
            let threshold = threshold;
            Box::pin(async move {
                crate::detectors::detect_monthly_increase(db, aspect, resolution, method, threshold)
                    .await
            })
        });

        let detector = EventDetector::new(
            id,
            name,
            Some(format!(
                "Detects when price increases {:.0}% or more from start to end of month",
                threshold_percent * 100.0
            )),
            detector_fn,
        );

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

        let detector_fn: EventDetectorFn = Arc::new(move |db, aspect, resolution, method| {
            let name = name_clone.clone();
            Box::pin(async move {
                crate::detectors::detect_peaks(db, aspect, resolution, method, &name).await
            })
        });

        let detector = EventDetector::new(
            id,
            name_str.clone(),
            Some(format!("Detects peak values for {name_str}")),
            detector_fn,
        );

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

        let detector_fn: EventDetectorFn = Arc::new(move |db, aspect, resolution, method| {
            let name = name_clone.clone();
            Box::pin(async move {
                crate::detectors::detect_valleys(db, aspect, resolution, method, &name).await
            })
        });

        let detector = EventDetector::new(
            id,
            name_str.clone(),
            Some(format!("Detects valley (minimum) values for {name_str}")),
            detector_fn,
        );

        self.with_detector(detector)
    }

    /// Builds the pipeline.
    ///
    /// # Errors
    ///
    /// Returns an error if pipeline initialization fails.
    pub async fn build(self) -> Result<Pipeline> {
        let dictionary = Dictionary::new(
            self.config.dictionary_name.clone(),
            self.config.dictionary_description.clone(),
            self.config.dictionary_constraints.clone(),
        );

        let database_name = self
            .database
            .get_database_info()
            .await?
            .name()
            .to_string();

        let state = PipelineState {
            config: self.config.clone(),
            aspect_id: self.aspect_id,
            database_name,
            last_run: None,
            run_count: 0,
            version: PipelineState::CURRENT_VERSION,
        };

        Ok(Pipeline {
            database: self.database,
            aspect_id: self.aspect_id,
            config: self.config,
            dictionary,
            signals: Signals::new(),
            event_detectors: self.event_detectors,
            state,
        })
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
        self.event_based
            .as_ref()
            .or(self.average.as_ref())
            .or(self.sum.as_ref())
    }

    /// Returns true if all probability values are None.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.sum.is_none() && self.average.is_none() && self.event_based.is_none()
    }
}

impl std::fmt::Display for ProbabilityResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sum_str = self
            .sum
            .as_ref()
            .map_or_else(|| "N/A".to_string(), |v| format!("{v:.4}"));
        let avg_str = self
            .average
            .as_ref()
            .map_or_else(|| "N/A".to_string(), |v| format!("{v:.4}"));
        let event_str = self
            .event_based
            .as_ref()
            .map_or_else(|| "N/A".to_string(), |v| format!("{v:.4}"));

        write!(
            f,
            "ProbabilityResult {{ sum: {sum_str}, average: {avg_str}, event_based: {event_str} }}"
        )
    }
}
