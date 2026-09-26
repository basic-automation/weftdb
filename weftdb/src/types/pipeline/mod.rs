//! Pipeline types for the database crate.
//!
//! This module contains types related to pipeline configuration and state,
//! which are persisted in the aspect's `pipeline.db` database.

mod config;
mod detector;
mod state;

pub use config::PipelineConfig;
pub use detector::{DetectorMetadata, DetectorType};
pub use state::PipelineState;
