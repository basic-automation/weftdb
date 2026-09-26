//! # High-Performance Time-Series Database
//!
//! A high-performance time-series database with advanced interpolation capabilities,
//! optimized for real-time sensor data processing and analysis. Built on top of
//! [Turso](https://turso.tech/) (libSQL) with MVCC support for concurrent writes.
//!
//! ## Overview
//!
//! This crate provides a hierarchical data model for organizing time-series data:
//!
//! - **Database**: The top-level container that holds all data
//! - **Subject**: A logical grouping (e.g., a sensor, device, or asset like "pump-station-3")
//! - **Aspect**: A specific measurement type for a subject (e.g., "temperature", "price", "open")
//! - **Measurement**: Individual timestamped data points with `BigDecimal` precision
//!
//! The crate also provides advanced features for pattern recognition and event prediction:
//!
//! - **Batches**: Groups of measurements processed together
//! - **Patterns**: Extracted recurring shapes in the data
//! - **Dictionaries**: Collections of patterns with similarity constraints
//! - **Events**: Detected occurrences (e.g., "5% monthly increase")
//! - **Correlations**: Links between patterns and events
//! - **Signals**: Predictions based on pattern-event correlations
//!
//! ## Quick Start
//!
//! ### Creating a Database and Capturing Measurements
//!
//! ```rust,ignore
//! use weftdb::{Database, DatabaseStructure, Inputs, InputMeasurement, Resolution, Spline};
//! use bigdecimal::BigDecimal;
//! use std::str::FromStr;
//! use chrono::{TimeZone, Utc};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // Create a new database (creates folder structure on disk)
//!     let db = Database::new("my_sensors").await?;
//!
//!     // Create a subject to track
//!     let sensor = db.observe_subject("temperature_sensor_001").await?;
//!
//!     // Track a specific aspect with time resolution
//!     let temperature = db.track_aspect(
//!         &sensor.id(),
//!         "ambient_temp",
//!         &Resolution::Seconds
//!     ).await?;
//!
//!     // Capture measurements
//!     let dataset_id = weftdb::DatasetId::new();
//!     let measurements = vec![
//!         InputMeasurement::new(
//!             Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap(),
//!             BigDecimal::from_str("22.5").unwrap()
//!         ),
//!         InputMeasurement::new(
//!             Utc.with_ymd_and_hms(2024, 1, 1, 12, 1, 0).unwrap(),
//!             BigDecimal::from_str("22.8").unwrap()
//!         ),
//!         InputMeasurement::new(
//!             Utc.with_ymd_and_hms(2024, 1, 1, 12, 2, 0).unwrap(),
//!             BigDecimal::from_str("23.1").unwrap()
//!         ),
//!     ];
//!
//!     // Batch insert for optimal performance
//!     db.batch_capture_measurements(
//!         temperature.id(),
//!         dataset_id,
//!         measurements
//!     ).await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! ### Querying and Interpolating Data
//!
//! ```rust,ignore
//! use weftdb::{Database, DatabaseStructure, Outputs, Resolution, Spline};
//! use chrono::{TimeZone, Utc};
//! use futures::StreamExt;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // Open an existing database
//!     let db = Database::existing("my_sensors").await?;
//!
//!     // Get the aspect we want to query
//!     let subjects = db.list_subjects().await?;
//!     let sensor_id = subjects.iter()
//!         .find(|(_, name)| name == "temperature_sensor_001")
//!         .map(|(id, _)| *id)
//!         .expect("Sensor not found");
//!
//!     let aspects = db.get_subject_aspects(&sensor_id).await?;
//!     let temp_aspect = aspects.iter()
//!         .find(|a| a.name() == "ambient_temp")
//!         .expect("Aspect not found");
//!
//!     // Query a specific point in time (interpolates if needed)
//!     let query_time = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 30).unwrap();
//!     let point = db.analyze_point(
//!         &temp_aspect.id(),
//!         query_time,
//!         &Resolution::Seconds,
//!         &Spline::Linear
//!     ).await?;
//!
//!     println!("Temperature at {:?}: {}", point.timestamp, point.value);
//!
//!     // Query a range of data
//!     let start = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();
//!     let end = Utc.with_ymd_and_hms(2024, 1, 1, 12, 5, 0).unwrap();
//!
//!     let mut stream = db.analyze_range(
//!         &temp_aspect.id(),
//!         start,
//!         end,
//!         Resolution::Seconds,
//!         Spline::Linear
//!     ).await?;
//!
//!     while let Some(result) = stream.next().await {
//!         let point = result?;
//!         println!("{}: {}", point.timestamp, point.value);
//!     }
//!
//!     Ok(())
//! }
//! ```
//!
//! ### Working with Events and Patterns
//!
//! ```rust,ignore
//! use weftdb::{
//!     Database, DatabaseStructure, Inputs, Outputs,
//!     Event, Manifestation, Pattern, Correlation,
//!     Dictionary, DictionaryConstraints, Resolution
//! };
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let db = Database::existing("my_sensors").await?;
//!     // ... get aspect_id ...
//!
//!     // Create an event to track (e.g., temperature spike)
//!     let mut event = Event::new(
//!         None,
//!         "Temperature Spike".to_string(),
//!         Some("Temperature exceeded threshold".to_string()),
//!         None
//!     );
//!
//!     // Add manifestations (occurrences of the event)
//!     let db_info = db.get_database_info().await?;
//!     let manifestation = Manifestation::new(
//!         db_info.id().as_uuid(),
//!         chrono::Utc::now() - chrono::Duration::hours(1),
//!         chrono::Utc::now(),
//!     );
//!     event.add_manifestation(manifestation);
//!
//!     // Store the event
//!     // db.insert_unprocessed_event(&aspect_id, &event).await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! ## Core Types
//!
//! | Type | Description |
//! |------|-------------|
//! | [`Database`] | Main database handle for all operations |
//! | [`Subject`] | Logical grouping of related aspects |
//! | [`Aspect`] | A specific measurement type with its own storage |
//! | [`InputMeasurement`] | Timestamped value to be stored |
//! | [`Measurement`] | Stored measurement with ID |
//! | [`Resolution`] | Time precision (Nanoseconds to Years) |
//! | [`Spline`] | Interpolation method (Linear, Cubic, etc.) |
//!
//! ## Pattern Recognition Types
//!
//! | Type | Description |
//! |------|-------------|
//! | [`Batch`] | Group of measurements for processing |
//! | [`Pattern`] | Extracted recurring shape with occurrences |
//! | [`Dictionary`] | Collection of patterns with constraints |
//! | [`Event`] | Detected occurrence with manifestations |
//! | [`Correlation`] | Link between a pattern and an event |
//! | [`Signal`] | Prediction based on correlation |
//!
//! ## Key Traits
//!
//! The database operations are organized into traits:
//!
//! - [`DatabaseStructure`]: Database creation and management
//! - [`Inputs`](crate::database::traits::Inputs): Data insertion operations
//! - [`Outputs`]: Data querying operations
//! - [`Config`]: Path and configuration management
//!
//! ## Features
//!
//! - **High Performance**: Optimized for real-time data processing with batch operations
//! - **Advanced Interpolation**: Linear, cubic, quadratic, and polynomial spline methods
//! - **GPU Acceleration**: Automatic strategy selection for large datasets
//! - **MVCC Concurrency**: Concurrent writes with `BEGIN CONCURRENT` transactions
//! - **Pattern Recognition**: Extract and match recurring patterns in time-series data
//! - **Event Detection**: Detect significant occurrences and predict future events
//! - **Flexible Resolution**: Support from nanoseconds to years
//! - **Caching**: Intelligent caching of interpolation results
//!
//! ## Architecture
//!
//! The database uses a file-based storage structure:
//!
//! ```text
//! {data_dir}/
//! └── {database_name}/
//!     ├── metadata.db           # Database metadata
//!     └── {subject_name}/
//!         └── {aspect_name}/
//!             ├── measurements.db
//!             ├── unprocessed_batches.db
//!             ├── processed_batches.db
//!             ├── patterns.db
//!             ├── events.db
//!             ├── correlations.db
//!             └── dictionaries/
//!                 └── {dictionary_name}.db
//! ```
//!
//! ## Performance Benchmarks
//!
//! | Operation                | Time (ms) | Notes                               |
//! |--------------------------|-----------|-------------------------------------|
//! | Insert (single)          | 0.1-0.5   | With MVCC concurrent writes         |
//! | Insert (batch of 1000)   | 10-50     | Bulk insert optimization            |
//! | Query (point lookup)     | 0.1-1     | Indexed lookup with caching         |
//! | Query (range scan)       | 1-100     | Depends on range size               |
//! | Interpolation (1K points)| 5-50      | Depends on method and hardware      |
//! | GPU Interpolation (1M)   | 100-500   | Requires compatible GPU             |
//!
//! ## Compatibility
//!
//! - **Operating Systems**: Linux, macOS, Windows
//! - **Rust Version**: 1.75.0 or later (requires 2024 edition features)
//! - **Hardware**: `x86_64`, ARM64 architectures
//! - **GPU Support**: Via wgpu (Vulkan, Metal, DX12)

#![recursion_limit = "1024"]
#![warn(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(
    clippy::multiple_crate_versions,
    clippy::used_underscore_binding,
    clippy::similar_names,
    clippy::module_name_repetitions,
    clippy::module_inception,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::option_if_let_else,
    clippy::needless_continue,
    clippy::manual_let_else
)]

mod types;

// Export all public types from the types module
// Re-export types from splimes that are commonly used
pub use splimes::{Point, Resolution, Spline};
pub use types::*;
