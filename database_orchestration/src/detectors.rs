//! Built-in event detector functions.
//!
//! This module provides ready-to-use event detectors for common time-series patterns.
//! Each detector function follows the standard [`EventDetectorFn`](crate::EventDetectorFn)
//! signature and can be used directly or registered with the [`Pipeline`](crate::Pipeline).
//!
//! ## Available Detectors
//!
//! | Function | Description |
//! |----------|-------------|
//! | [`detect_monthly_increase`] | Detects months with price increase above threshold |
//! | [`detect_peaks`] | Finds local maxima equal to the global maximum |
//! | [`detect_all_peaks`] | Finds all local maxima (regardless of global max) |
//! | [`detect_valleys`] | Finds local minima equal to the global minimum |
//! | [`detect_all_valleys`] | Finds all local minima |
//! | [`detect_threshold_crossing_up`] | Detects upward threshold crossings |
//! | [`detect_threshold_crossing_down`] | Detects downward threshold crossings |
//! | [`detect_drawdown`] | Detects significant price drops from peaks |
//!
//! ## Usage with Pipeline (Recommended)
//!
//! The easiest way to use detectors is through the Pipeline builder:
//!
//! ```ignore
//! use database_orchestration::Pipeline;
//!
//! let mut pipeline = Pipeline::builder(database, aspect_id)
//!     .with_monthly_increase_detector(0.05)  // 5% monthly increase
//!     .with_peak_detector("Price Peaks")
//!     .with_valley_detector("Price Valleys")
//!     .build()
//!     .await?;
//!
//! pipeline.run().await?;
//! ```
//!
//! ## Direct Usage
//!
//! Detectors can also be called directly for standalone analysis:
//!
//! ```ignore
//! use database_orchestration::detectors;
//! use database::{Database, Resolution};
//! use splimes::Spline;
//!
//! let events = detectors::detect_monthly_increase(
//!     &database,
//!     &aspect_id,
//!     &Resolution::Hours,
//!     &Spline::Linear,
//!     0.05  // 5% threshold
//! ).await?;
//!
//! for event in events {
//!     println!("Found event: {} with {} manifestations",
//!         event.name(),
//!         event.manifestations().len()
//!     );
//! }
//! ```
//!
//! ## Creating Custom Detectors
//!
//! You can create custom detectors following the same pattern. A detector function
//! receives database access and returns a list of detected events:
//!
//! ```ignore
//! use database_orchestration::{Event, Manifestation, EventDetector, event_detector_fn};
//! use database::{Database, AspectId, Resolution, DatabaseStructure, Outputs};
//! use splimes::Spline;
//! use anyhow::Result;
//!
//! async fn detect_custom_pattern(
//!     database: &Database,
//!     aspect: &AspectId,
//!     resolution: &Resolution,
//!     method: &Spline,
//! ) -> Result<Vec<Event>> {
//!     // Get the data range
//!     let start = database.get_earliest_measurement(aspect).await?
//!         .ok_or_else(|| anyhow::anyhow!("No data"))?;
//!     let end = database.get_latest_measurement(aspect).await?
//!         .ok_or_else(|| anyhow::anyhow!("No data"))?;
//!
//!     // Analyze the data
//!     let mut stream = database.analyze_range(
//!         aspect, start, end, *resolution, *method
//!     ).await?;
//!
//!     let mut event = Event::new(
//!         None,
//!         "Custom Pattern".to_string(),
//!         Some("Detects my custom pattern".to_string()),
//!         None
//!     );
//!
//!     // Your detection logic here...
//!     // Add manifestations when pattern is detected:
//!     // event.add_manifestation(manifestation);
//!
//!     if event.manifestations().is_empty() {
//!         Ok(vec![])
//!     } else {
//!         Ok(vec![event])
//!     }
//! }
//!
//! // Register with pipeline
//! let mut pipeline = Pipeline::builder(database, aspect_id)
//!     .with_detector(EventDetector::new(
//!         "custom_pattern",
//!         "Custom Pattern Detector",
//!         Some("Detects my custom pattern".to_string()),
//!         event_detector_fn!(detect_custom_pattern),
//!     ))
//!     .build()
//!     .await?;
//! ```

use std::collections::HashMap;

use anyhow::Result;
use bigdecimal::{BigDecimal, FromPrimitive};
use chrono::Datelike;
use database::{
	database::traits::{DatabaseStructure, Outputs}, AspectId, Database, Resolution
};
use futures::StreamExt;
use splimes::Spline;

use crate::{Event, Manifestation};

/// Detects monthly price increases above a threshold percentage.
///
/// Analyzes price movements from the beginning to end of each calendar month.
/// Creates an event manifestation when the increase exceeds the threshold.
///
/// # Arguments
///
/// * `database` - The database to query measurements from
/// * `aspect` - The aspect to analyze
/// * `resolution` - The resolution for analysis
/// * `method` - The spline interpolation method
/// * `threshold_percent` - The threshold as a decimal (e.g., 0.05 for 5%)
///
/// # Returns
///
/// A vector containing a single event with manifestations for each month that
/// exceeded the threshold, or an empty vector if no months qualified.
///
/// # Errors
///
/// Returns an error if database operations fail.
///
/// # Example
///
/// ```ignore
/// // Detect months with 5% or greater price increase
/// let events = detect_monthly_increase(&db, &aspect, &resolution, &method, 0.05).await?;
/// ```
pub async fn detect_monthly_increase(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, threshold_percent: f64) -> Result<Vec<Event>> {
	let start_time = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
	let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;

	let mut point_stream = Outputs::analyze_range(database, aspect, start_time, end_time, *resolution, *method).await?;

	let mut points = Vec::new();
	while let Some(result) = point_stream.next().await {
		points.push(result?);
	}
	points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	let mut event = Event::new(None, format!("{:.0}% Monthly Price Increase - {aspect}", threshold_percent * 100.0), Some(format!("Detects when price increases {:.0}% or more from start to end of month", threshold_percent * 100.0)), None);

	let threshold = BigDecimal::from_f64(threshold_percent).ok_or_else(|| anyhow::anyhow!("Invalid threshold percentage"))?;

	// Group points by month
	let mut months_data: HashMap<(i32, u32), Vec<&splimes::Point>> = HashMap::new();
	for point in &points {
		months_data.entry((point.timestamp.year(), point.timestamp.month())).or_default().push(point);
	}

	// Analyze each month
	for (_month_key, mut month_points) in months_data {
		if month_points.len() < 2 {
			continue;
		}

		month_points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

		let start_price = &month_points[0].value;
		let end_price = &month_points[month_points.len() - 1].value;
		let end_timestamp = month_points[month_points.len() - 1].timestamp;

		let price_diff = end_price - start_price;
		let percentage_increase = &price_diff / start_price;

		if percentage_increase >= threshold {
			let database_info = database.get_database_info().await?;
			let manifestation = Manifestation::new(database_info.id().as_uuid(), month_points[0].timestamp, end_timestamp);
			event.add_manifestation(manifestation);
		}
	}

	if event.manifestations().is_empty() {
		Ok(vec![])
	} else {
		tracing::debug!(
		    event_name = %event.name(),
		    manifestations = event.manifestations().len(),
		    "Monthly increase detector found events"
		);
		Ok(vec![event])
	}
}

/// Detects peak values (local maxima that equal the global maximum).
///
/// Scans through all data points and identifies local maxima where the value
/// equals the global maximum value across the entire dataset.
///
/// # Arguments
///
/// * `database` - The database to query measurements from
/// * `aspect` - The aspect to analyze
/// * `resolution` - The resolution for analysis
/// * `method` - The spline interpolation method
/// * `name` - The name for the event
///
/// # Returns
///
/// A vector containing a single event with manifestations for each detected peak,
/// or an empty vector if no peaks were found.
///
/// # Errors
///
/// Returns an error if database operations fail.
pub async fn detect_peaks(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, name: &str) -> Result<Vec<Event>> {
	let start_time = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
	let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;

	let mut point_stream = Outputs::analyze_range(database, aspect, start_time, end_time, *resolution, *method).await?;

	let mut points = Vec::new();
	while let Some(result) = point_stream.next().await {
		points.push(result?);
	}
	points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	if points.len() < 3 {
		return Ok(vec![]);
	}

	let global_max = points.iter().map(|p| &p.value).max().ok_or_else(|| anyhow::anyhow!("No points found"))?;

	let mut event = Event::new(None, name.to_string(), Some(format!("Detects peak values for aspect {aspect}")), None);

	for i in 1..points.len().saturating_sub(1) {
		let prev_value = &points[i - 1].value;
		let curr_value = &points[i].value;
		let next_value = &points[i + 1].value;

		// Local maximum that equals global maximum
		if curr_value == global_max && curr_value > prev_value && curr_value > next_value {
			let database_info = database.get_database_info().await?;
			let manifestation = Manifestation::new(database_info.id().as_uuid(), points[i - 1].timestamp, points[i + 1].timestamp);
			event.add_manifestation(manifestation);
		}
	}

	if event.manifestations().is_empty() {
		Ok(vec![])
	} else {
		tracing::debug!(
		    event_name = %event.name(),
		    manifestations = event.manifestations().len(),
		    "Peak detector found events"
		);
		Ok(vec![event])
	}
}

/// Detects all local peaks (local maxima), not just global maximum.
///
/// Unlike `detect_peaks` which only finds peaks equal to the global maximum,
/// this function finds all local maxima regardless of their value.
///
/// # Arguments
///
/// * `database` - The database to query measurements from
/// * `aspect` - The aspect to analyze
/// * `resolution` - The resolution for analysis
/// * `method` - The spline interpolation method
/// * `name` - The name for the event
///
/// # Returns
///
/// A vector containing a single event with manifestations for each local peak,
/// or an empty vector if no peaks were found.
///
/// # Errors
///
/// Returns an error if database operations fail.
pub async fn detect_all_peaks(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, name: &str) -> Result<Vec<Event>> {
	let start_time = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
	let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;

	let mut point_stream = Outputs::analyze_range(database, aspect, start_time, end_time, *resolution, *method).await?;

	let mut points = Vec::new();
	while let Some(result) = point_stream.next().await {
		points.push(result?);
	}
	points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	if points.len() < 3 {
		return Ok(vec![]);
	}

	let mut event = Event::new(None, name.to_string(), Some(format!("Detects all local peak values for aspect {aspect}")), None);

	for i in 1..points.len().saturating_sub(1) {
		let prev_value = &points[i - 1].value;
		let curr_value = &points[i].value;
		let next_value = &points[i + 1].value;

		// Any local maximum
		if curr_value > prev_value && curr_value > next_value {
			let database_info = database.get_database_info().await?;
			let manifestation = Manifestation::new(database_info.id().as_uuid(), points[i - 1].timestamp, points[i + 1].timestamp);
			event.add_manifestation(manifestation);
		}
	}

	if event.manifestations().is_empty() {
		Ok(vec![])
	} else {
		tracing::debug!(
		    event_name = %event.name(),
		    manifestations = event.manifestations().len(),
		    "All peaks detector found events"
		);
		Ok(vec![event])
	}
}

/// Detects valley values (local minima that equal the global minimum).
///
/// Scans through all data points and identifies local minima where the value
/// equals the global minimum value across the entire dataset.
///
/// # Arguments
///
/// * `database` - The database to query measurements from
/// * `aspect` - The aspect to analyze
/// * `resolution` - The resolution for analysis
/// * `method` - The spline interpolation method
/// * `name` - The name for the event
///
/// # Returns
///
/// A vector containing a single event with manifestations for each detected valley,
/// or an empty vector if no valleys were found.
///
/// # Errors
///
/// Returns an error if database operations fail.
pub async fn detect_valleys(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, name: &str) -> Result<Vec<Event>> {
	let start_time = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
	let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;

	let mut point_stream = Outputs::analyze_range(database, aspect, start_time, end_time, *resolution, *method).await?;

	let mut points = Vec::new();
	while let Some(result) = point_stream.next().await {
		points.push(result?);
	}
	points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	if points.len() < 3 {
		return Ok(vec![]);
	}

	let global_min = points.iter().map(|p| &p.value).min().ok_or_else(|| anyhow::anyhow!("No points found"))?;

	let mut event = Event::new(None, name.to_string(), Some(format!("Detects valley (local minimum) values for aspect {aspect}")), None);

	for i in 1..points.len().saturating_sub(1) {
		let prev_value = &points[i - 1].value;
		let curr_value = &points[i].value;
		let next_value = &points[i + 1].value;

		// Local minimum that equals global minimum
		if curr_value == global_min && curr_value < prev_value && curr_value < next_value {
			let database_info = database.get_database_info().await?;
			let manifestation = Manifestation::new(database_info.id().as_uuid(), points[i - 1].timestamp, points[i + 1].timestamp);
			event.add_manifestation(manifestation);
		}
	}

	if event.manifestations().is_empty() {
		Ok(vec![])
	} else {
		tracing::debug!(
		    event_name = %event.name(),
		    manifestations = event.manifestations().len(),
		    "Valley detector found events"
		);
		Ok(vec![event])
	}
}

/// Detects all local valleys (local minima), not just global minimum.
///
/// Unlike `detect_valleys` which only finds valleys equal to the global minimum,
/// this function finds all local minima regardless of their value.
///
/// # Arguments
///
/// * `database` - The database to query measurements from
/// * `aspect` - The aspect to analyze
/// * `resolution` - The resolution for analysis
/// * `method` - The spline interpolation method
/// * `name` - The name for the event
///
/// # Returns
///
/// A vector containing a single event with manifestations for each local valley,
/// or an empty vector if no valleys were found.
///
/// # Errors
///
/// Returns an error if database operations fail.
pub async fn detect_all_valleys(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, name: &str) -> Result<Vec<Event>> {
	let start_time = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
	let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;

	let mut point_stream = Outputs::analyze_range(database, aspect, start_time, end_time, *resolution, *method).await?;

	let mut points = Vec::new();
	while let Some(result) = point_stream.next().await {
		points.push(result?);
	}
	points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	if points.len() < 3 {
		return Ok(vec![]);
	}

	let mut event = Event::new(None, name.to_string(), Some(format!("Detects all local valley values for aspect {aspect}")), None);

	for i in 1..points.len().saturating_sub(1) {
		let prev_value = &points[i - 1].value;
		let curr_value = &points[i].value;
		let next_value = &points[i + 1].value;

		// Any local minimum
		if curr_value < prev_value && curr_value < next_value {
			let database_info = database.get_database_info().await?;
			let manifestation = Manifestation::new(database_info.id().as_uuid(), points[i - 1].timestamp, points[i + 1].timestamp);
			event.add_manifestation(manifestation);
		}
	}

	if event.manifestations().is_empty() {
		Ok(vec![])
	} else {
		tracing::debug!(
		    event_name = %event.name(),
		    manifestations = event.manifestations().len(),
		    "All valleys detector found events"
		);
		Ok(vec![event])
	}
}

/// Detects threshold crossings (when value crosses above a specified level).
///
/// Creates a manifestation each time the value crosses from below the threshold
/// to at or above the threshold.
///
/// # Arguments
///
/// * `database` - The database to query measurements from
/// * `aspect` - The aspect to analyze
/// * `resolution` - The resolution for analysis
/// * `method` - The spline interpolation method
/// * `threshold` - The threshold value to detect crossings
/// * `name` - The name for the event
///
/// # Returns
///
/// A vector containing a single event with manifestations for each threshold crossing,
/// or an empty vector if no crossings were found.
///
/// # Errors
///
/// Returns an error if database operations fail.
pub async fn detect_threshold_crossing_up(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, threshold: BigDecimal, name: &str) -> Result<Vec<Event>> {
	let start_time = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
	let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;

	let mut point_stream = Outputs::analyze_range(database, aspect, start_time, end_time, *resolution, *method).await?;

	let mut points = Vec::new();
	while let Some(result) = point_stream.next().await {
		points.push(result?);
	}
	points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	if points.len() < 2 {
		return Ok(vec![]);
	}

	let mut event = Event::new(None, name.to_string(), Some(format!("Detects when value crosses above {threshold}")), None);

	for i in 1..points.len() {
		let prev_value = &points[i - 1].value;
		let curr_value = &points[i].value;

		// Crossed above threshold
		if prev_value < &threshold && curr_value >= &threshold {
			let database_info = database.get_database_info().await?;
			let manifestation = Manifestation::new(database_info.id().as_uuid(), points[i - 1].timestamp, points[i].timestamp);
			event.add_manifestation(manifestation);
		}
	}

	if event.manifestations().is_empty() {
		Ok(vec![])
	} else {
		tracing::debug!(
		    event_name = %event.name(),
		    manifestations = event.manifestations().len(),
		    threshold = %threshold,
		    "Threshold crossing (up) detector found events"
		);
		Ok(vec![event])
	}
}

/// Detects threshold crossings (when value crosses below a specified level).
///
/// Creates a manifestation each time the value crosses from above the threshold
/// to at or below the threshold.
///
/// # Arguments
///
/// * `database` - The database to query measurements from
/// * `aspect` - The aspect to analyze
/// * `resolution` - The resolution for analysis
/// * `method` - The spline interpolation method
/// * `threshold` - The threshold value to detect crossings
/// * `name` - The name for the event
///
/// # Returns
///
/// A vector containing a single event with manifestations for each threshold crossing,
/// or an empty vector if no crossings were found.
///
/// # Errors
///
/// Returns an error if database operations fail.
pub async fn detect_threshold_crossing_down(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, threshold: BigDecimal, name: &str) -> Result<Vec<Event>> {
	let start_time = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
	let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;

	let mut point_stream = Outputs::analyze_range(database, aspect, start_time, end_time, *resolution, *method).await?;

	let mut points = Vec::new();
	while let Some(result) = point_stream.next().await {
		points.push(result?);
	}
	points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	if points.len() < 2 {
		return Ok(vec![]);
	}

	let mut event = Event::new(None, name.to_string(), Some(format!("Detects when value crosses below {threshold}")), None);

	for i in 1..points.len() {
		let prev_value = &points[i - 1].value;
		let curr_value = &points[i].value;

		// Crossed below threshold
		if prev_value > &threshold && curr_value <= &threshold {
			let database_info = database.get_database_info().await?;
			let manifestation = Manifestation::new(database_info.id().as_uuid(), points[i - 1].timestamp, points[i].timestamp);
			event.add_manifestation(manifestation);
		}
	}

	if event.manifestations().is_empty() {
		Ok(vec![])
	} else {
		tracing::debug!(
		    event_name = %event.name(),
		    manifestations = event.manifestations().len(),
		    threshold = %threshold,
		    "Threshold crossing (down) detector found events"
		);
		Ok(vec![event])
	}
}

/// Detects percentage drops from recent highs (drawdowns).
///
/// Creates a manifestation when the value drops by the specified percentage
/// from a recent local maximum.
///
/// # Arguments
///
/// * `database` - The database to query measurements from
/// * `aspect` - The aspect to analyze
/// * `resolution` - The resolution for analysis
/// * `method` - The spline interpolation method
/// * `drop_percent` - The drop percentage as a decimal (e.g., 0.10 for 10%)
/// * `name` - The name for the event
///
/// # Returns
///
/// A vector containing a single event with manifestations for each detected drawdown,
/// or an empty vector if no drawdowns were found.
///
/// # Errors
///
/// Returns an error if database operations fail.
pub async fn detect_drawdown(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, drop_percent: f64, name: &str) -> Result<Vec<Event>> {
	let start_time = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
	let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;

	let mut point_stream = Outputs::analyze_range(database, aspect, start_time, end_time, *resolution, *method).await?;

	let mut points = Vec::new();
	while let Some(result) = point_stream.next().await {
		points.push(result?);
	}
	points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	if points.len() < 2 {
		return Ok(vec![]);
	}

	let threshold = BigDecimal::from_f64(drop_percent).ok_or_else(|| anyhow::anyhow!("Invalid drop percentage"))?;

	let mut event = Event::new(None, name.to_string(), Some(format!("Detects {:.0}% drawdowns from recent highs", drop_percent * 100.0)), None);

	let mut running_high = points[0].value.clone();
	let mut high_timestamp = points[0].timestamp;

	for point in &points[1..] {
		// Update running high
		if point.value > running_high {
			running_high = point.value.clone();
			high_timestamp = point.timestamp;
		} else {
			// Check for drawdown
			let drop = (&running_high - &point.value) / &running_high;
			if drop >= threshold {
				let database_info = database.get_database_info().await?;
				let manifestation = Manifestation::new(database_info.id().as_uuid(), high_timestamp, point.timestamp);
				event.add_manifestation(manifestation);

				// Reset the high to current value to detect subsequent drawdowns
				running_high = point.value.clone();
				high_timestamp = point.timestamp;
			}
		}
	}

	if event.manifestations().is_empty() {
		Ok(vec![])
	} else {
		tracing::debug!(
		    event_name = %event.name(),
		    manifestations = event.manifestations().len(),
		    drop_percent = drop_percent,
		    "Drawdown detector found events"
		);
		Ok(vec![event])
	}
}
