//! # High-Performance Time-Series Database
//!
//! A high-performance time-series database with advanced interpolation capabilities,
//! optimized for real-time sensor data processing and analysis.
//!
//! ## Quick Start
//!
//! ```rust
//! use database::*;
//! use bigdecimal::BigDecimal;
//! use std::str::FromStr;
//! use chrono::TimeZone;
//!
//! # #[tokio::main]
//! # async fn main() -> anyhow::Result<()> {
//! # // Clean up any existing test data first
//! # std::fs::remove_dir_all("data/my_experiment").ok();
//! #
//! // Create a new database
//! let db = Database::new("my_experiment").await?;
//!
//! // Add a subject
//! let subject = db.track_subject("participant_001").await?;
//!
//! // Track an aspect (e.g., heart rate)
//! let aspect = db.track_aspect(subject, "heart_rate").await?;
//!
//! // Capture multiple measurements for interpolation
//! let measurements = vec![
//!     InputMeasurement::new(
//!         chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(),
//!         BigDecimal::from_str("70.0").unwrap()
//!     ),
//!     InputMeasurement::new(
//!         chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 1, 0).unwrap(),
//!         BigDecimal::from_str("72.5").unwrap()
//!     ),
//!     InputMeasurement::new(
//!         chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 2, 0).unwrap(),
//!         BigDecimal::from_str("75.0").unwrap()
//!     ),
//! ];
//!
//! for measurement in measurements {
//!     db.observe_measurement(aspect.clone(), measurement).await?;
//! }
//!
//! // Analyze data point (interpolate between existing measurements)
//! let time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 1, 30).unwrap();
//! let data_point = db.analyze_point(aspect.id(), time, Resolution::Seconds, Spline::Linear).await?;
//!
//! println!("Interpolated value: {}", data_point.value);
//! # std::fs::remove_dir_all("data/my_experiment").ok();
//! # Ok(())
//! # }
//! ```

#![warn(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions, clippy::used_underscore_binding, clippy::similar_names, clippy::module_name_repetitions, clippy::module_inception)]

pub use splimes::{Point, Resolution, Spline};
use types::{CACHE, DATABASES};

pub mod types;

// Re-export commonly used types
pub use types::{Aspect, AspectId, Database, DatabaseId, DatabaseInfo, Dataset, Error, InputMeasurement, Measurement, Subject, SubjectId, TxId};
pub const DEFAULT_DATA_DIR: &str = "data";
