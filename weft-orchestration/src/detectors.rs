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
//! | [`detect_monthly_increase_in_window`] | Bounded-window variant of `detect_monthly_increase` |
//! | [`detect_peaks_in_window`] | Bounded-window variant of `detect_peaks` |
//! | [`detect_all_peaks_in_window`] | Bounded-window variant of `detect_all_peaks` |
//! | [`detect_valleys_in_window`] | Bounded-window variant of `detect_valleys` |
//! | [`detect_all_valleys_in_window`] | Bounded-window variant of `detect_all_valleys` |
//! | [`detect_threshold_crossing_up_in_window`] | Bounded-window variant of `detect_threshold_crossing_up` |
//! | [`detect_threshold_crossing_down_in_window`] | Bounded-window variant of `detect_threshold_crossing_down` |
//! | [`detect_drawdown_in_window`] | Bounded-window variant of `detect_drawdown` |
//!
//! ## Usage with Pipeline (Recommended)
//!
//! The easiest way to use detectors is through the Pipeline builder:
//!
//! ```ignore
//! use weft_orchestration::Pipeline;
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
//! use weft_orchestration::detectors;
//! use weftdb::{Database, Resolution};
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
//! use weft_orchestration::{Event, Manifestation, EventDetector, event_detector_fn};
//! use weftdb::{Database, AspectId, Resolution, DatabaseStructure, Outputs};
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
use chrono::{DateTime, Datelike, Utc};
use futures::StreamExt;
use splimes::Spline;
use weftdb::{
	database::traits::{DatabaseStructure, Outputs}, AspectId, Database, Resolution
};

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
	detect_monthly_increase_in_window(database, aspect, resolution, method, threshold_percent, None).await
}

/// # Errors
///
/// Returns an error if database operations fail, or if the aspect has no measurement to bound the analysis range with.
/// Bounded-window variant of [`detect_monthly_increase`].
///
/// `Some(since)` moves the **start** of the analysed range to `since`. The end stays the aspect's
/// latest measurement, so a window shortens the head of a series and never its tail. Read `since`
/// as the start of the range rather than a filter applied to a full-history result: a feature is
/// reported only when the points that define it are both inside the range.
/// Months are grouped from the points that fall inside the range, so the first month in a
/// window is usually partial: its start and end prices are read from fewer days, and its
/// percentage can differ from the same month read over the full history.
/// `None` analyses the whole series and matches [`detect_monthly_increase`] exactly.
///
/// A `since` at or after the aspect's latest measurement leaves a range too small for the
/// detector to judge, so it returns an empty result rather than an error.
pub async fn detect_monthly_increase_in_window(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, threshold_percent: f64, since: Option<DateTime<Utc>>) -> Result<Vec<Event>> {
	let Some((start_time, end_time)) = scan_range(database, aspect, since).await? else {
		return Ok(vec![]);
	};

	let mut point_stream = Outputs::analyze_range(database, aspect, start_time, end_time, *resolution, *method).await?;

	let mut points = Vec::new();
	while let Some(result) = point_stream.next().await {
		points.push(result?);
	}
	points.sort_by_key(|a| a.timestamp);

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

		month_points.sort_by_key(|a| a.timestamp);

		let start_price = &month_points[0].value;
		let end_price = &month_points[month_points.len() - 1].value;
		let end_timestamp = month_points[month_points.len() - 1].timestamp;

		let price_diff = end_price - start_price;
		let percentage_increase = &price_diff / start_price;

		if percentage_increase >= threshold {
			let database_info = database.get_database_info().await?;
			let manifestation = Manifestation::new(database_info.id().as_uuid(), month_points[0].timestamp, end_timestamp);
			drop(database_info);
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
	detect_peaks_in_window(database, aspect, resolution, method, name, None).await
}

/// # Errors
///
/// Returns an error if database operations fail, or if the aspect has no measurement to bound the analysis range with.
/// Bounded-window variant of [`detect_peaks`].
///
/// `Some(since)` moves the **start** of the analysed range to `since`. The end stays the aspect's
/// latest measurement, so a window shortens the head of a series and never its tail. Read `since`
/// as the start of the range rather than a filter applied to a full-history result: a feature is
/// reported only when the points that define it are both inside the range.
/// A peak is judged against its neighbours inside the range, so a peak whose earlier
/// neighbour lies before `since` is not reported.
/// `None` analyses the whole series and matches [`detect_peaks`] exactly.
///
/// A `since` at or after the aspect's latest measurement leaves a range too small for the
/// detector to judge, so it returns an empty result rather than an error.
pub async fn detect_peaks_in_window(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, name: &str, since: Option<DateTime<Utc>>) -> Result<Vec<Event>> {
	let Some((start_time, end_time)) = scan_range(database, aspect, since).await? else {
		return Ok(vec![]);
	};

	let mut point_stream = Outputs::analyze_range(database, aspect, start_time, end_time, *resolution, *method).await?;

	let mut points = Vec::new();
	while let Some(result) = point_stream.next().await {
		points.push(result?);
	}
	points.sort_by_key(|a| a.timestamp);

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
			drop(database_info);
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
	detect_all_peaks_in_window(database, aspect, resolution, method, name, None).await
}

/// # Errors
///
/// Returns an error if database operations fail, or if the aspect has no measurement to bound the analysis range with.
/// Bounded-window variant of [`detect_all_peaks`].
///
/// `Some(since)` moves the **start** of the analysed range to `since`. The end stays the aspect's
/// latest measurement, so a window shortens the head of a series and never its tail. Read `since`
/// as the start of the range rather than a filter applied to a full-history result: a feature is
/// reported only when the points that define it are both inside the range.
/// A peak is judged against its neighbours inside the range, so a peak whose earlier
/// neighbour lies before `since` is not reported.
/// `None` analyses the whole series and matches [`detect_all_peaks`] exactly.
///
/// A `since` at or after the aspect's latest measurement leaves a range too small for the
/// detector to judge, so it returns an empty result rather than an error.
pub async fn detect_all_peaks_in_window(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, name: &str, since: Option<DateTime<Utc>>) -> Result<Vec<Event>> {
	let Some((start_time, end_time)) = scan_range(database, aspect, since).await? else {
		return Ok(vec![]);
	};

	let mut point_stream = Outputs::analyze_range(database, aspect, start_time, end_time, *resolution, *method).await?;

	let mut points = Vec::new();
	while let Some(result) = point_stream.next().await {
		points.push(result?);
	}
	points.sort_by_key(|a| a.timestamp);

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
			drop(database_info);
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
	detect_valleys_in_window(database, aspect, resolution, method, name, None).await
}

/// # Errors
///
/// Returns an error if database operations fail, or if the aspect has no measurement to bound the analysis range with.
/// Bounded-window variant of [`detect_valleys`].
///
/// `Some(since)` moves the **start** of the analysed range to `since`. The end stays the aspect's
/// latest measurement, so a window shortens the head of a series and never its tail. Read `since`
/// as the start of the range rather than a filter applied to a full-history result: a feature is
/// reported only when the points that define it are both inside the range.
/// A valley is judged against its neighbours inside the range, so a valley whose earlier
/// neighbour lies before `since` is not reported.
/// `None` analyses the whole series and matches [`detect_valleys`] exactly.
///
/// A `since` at or after the aspect's latest measurement leaves a range too small for the
/// detector to judge, so it returns an empty result rather than an error.
pub async fn detect_valleys_in_window(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, name: &str, since: Option<DateTime<Utc>>) -> Result<Vec<Event>> {
	let Some((start_time, end_time)) = scan_range(database, aspect, since).await? else {
		return Ok(vec![]);
	};

	let mut point_stream = Outputs::analyze_range(database, aspect, start_time, end_time, *resolution, *method).await?;

	let mut points = Vec::new();
	while let Some(result) = point_stream.next().await {
		points.push(result?);
	}
	points.sort_by_key(|a| a.timestamp);

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
			drop(database_info);
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
	detect_all_valleys_in_window(database, aspect, resolution, method, name, None).await
}

/// # Errors
///
/// Returns an error if database operations fail, or if the aspect has no measurement to bound the analysis range with.
/// Bounded-window variant of [`detect_all_valleys`].
///
/// `Some(since)` moves the **start** of the analysed range to `since`. The end stays the aspect's
/// latest measurement, so a window shortens the head of a series and never its tail. Read `since`
/// as the start of the range rather than a filter applied to a full-history result: a feature is
/// reported only when the points that define it are both inside the range.
/// A valley is judged against its neighbours inside the range, so a valley whose earlier
/// neighbour lies before `since` is not reported.
/// `None` analyses the whole series and matches [`detect_all_valleys`] exactly.
///
/// A `since` at or after the aspect's latest measurement leaves a range too small for the
/// detector to judge, so it returns an empty result rather than an error.
pub async fn detect_all_valleys_in_window(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, name: &str, since: Option<DateTime<Utc>>) -> Result<Vec<Event>> {
	let Some((start_time, end_time)) = scan_range(database, aspect, since).await? else {
		return Ok(vec![]);
	};

	let mut point_stream = Outputs::analyze_range(database, aspect, start_time, end_time, *resolution, *method).await?;

	let mut points = Vec::new();
	while let Some(result) = point_stream.next().await {
		points.push(result?);
	}
	points.sort_by_key(|a| a.timestamp);

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
			drop(database_info);
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
	detect_threshold_crossing_up_in_window(database, aspect, resolution, method, threshold, name, None).await
}

/// # Errors
///
/// Returns an error if database operations fail, or if the aspect has no measurement to bound the analysis range with.
/// Bounded-window variant of [`detect_threshold_crossing_up`].
///
/// `Some(since)` moves the **start** of the analysed range to `since`. The end stays the aspect's
/// latest measurement, so a window shortens the head of a series and never its tail. Read `since`
/// as the start of the range rather than a filter applied to a full-history result: a feature is
/// reported only when the points that define it are both inside the range.
/// A crossing needs a point on each side of the threshold, so a crossing whose point **below**
/// the threshold precedes `since` is not reported.
/// `None` analyses the whole series and matches [`detect_threshold_crossing_up`] exactly.
///
/// A `since` at or after the aspect's latest measurement leaves a range too small for the
/// detector to judge, so it returns an empty result rather than an error.
pub async fn detect_threshold_crossing_up_in_window(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, threshold: BigDecimal, name: &str, since: Option<DateTime<Utc>>) -> Result<Vec<Event>> {
	let Some((start_time, end_time)) = scan_range(database, aspect, since).await? else {
		return Ok(vec![]);
	};

	let mut point_stream = Outputs::analyze_range(database, aspect, start_time, end_time, *resolution, *method).await?;

	let mut points = Vec::new();
	while let Some(result) = point_stream.next().await {
		points.push(result?);
	}
	points.sort_by_key(|a| a.timestamp);

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
			drop(database_info);
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
	detect_threshold_crossing_down_in_window(database, aspect, resolution, method, threshold, name, None).await
}

/// # Errors
///
/// Returns an error if database operations fail, or if the aspect has no measurement to bound the analysis range with.
/// Bounded-window variant of [`detect_threshold_crossing_down`].
///
/// `Some(since)` moves the **start** of the analysed range to `since`. The end stays the aspect's
/// latest measurement, so a window shortens the head of a series and never its tail. Read `since`
/// as the start of the range rather than a filter applied to a full-history result: a feature is
/// reported only when the points that define it are both inside the range.
/// A crossing needs a point on each side of the threshold, so a crossing whose point **above**
/// the threshold precedes `since` is not reported.
/// `None` analyses the whole series and matches [`detect_threshold_crossing_down`] exactly.
///
/// A `since` at or after the aspect's latest measurement leaves a range too small for the
/// detector to judge, so it returns an empty result rather than an error.
pub async fn detect_threshold_crossing_down_in_window(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, threshold: BigDecimal, name: &str, since: Option<DateTime<Utc>>) -> Result<Vec<Event>> {
	let Some((start_time, end_time)) = scan_range(database, aspect, since).await? else {
		return Ok(vec![]);
	};

	let mut point_stream = Outputs::analyze_range(database, aspect, start_time, end_time, *resolution, *method).await?;

	let mut points = Vec::new();
	while let Some(result) = point_stream.next().await {
		points.push(result?);
	}
	points.sort_by_key(|a| a.timestamp);

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
			drop(database_info);
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
	detect_drawdown_in_window(database, aspect, resolution, method, drop_percent, name, None).await
}

/// # Errors
///
/// Returns an error if database operations fail, or if the aspect has no measurement to bound the analysis range with.
/// Bounded-window variant of [`detect_drawdown`].
///
/// `Some(since)` moves the **start** of the analysed range to `since`. The end stays the aspect's
/// latest measurement, so a window shortens the head of a series and never its tail. Read `since`
/// as the start of the range rather than a filter applied to a full-history result: a feature is
/// reported only when the points that define it are both inside the range.
/// `running_high` restarts from the first point in the range, so a window can **miss** a
/// decline a full scan reports — the earlier high that makes the fall large enough sits outside
/// the range — and the run of declines it reports depends on where the window opens. It never
/// reports a larger decline than a full scan would at the same point, because the windowed high
/// is never higher than the full-history one.
/// `None` analyses the whole series and matches [`detect_drawdown`] exactly.
///
/// A `since` at or after the aspect's latest measurement leaves a range too small for the
/// detector to judge, so it returns an empty result rather than an error.
pub async fn detect_drawdown_in_window(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, drop_percent: f64, name: &str, since: Option<DateTime<Utc>>) -> Result<Vec<Event>> {
	let Some((start_time, end_time)) = scan_range(database, aspect, since).await? else {
		return Ok(vec![]);
	};

	let mut point_stream = Outputs::analyze_range(database, aspect, start_time, end_time, *resolution, *method).await?;

	let mut points = Vec::new();
	while let Some(result) = point_stream.next().await {
		points.push(result?);
	}
	points.sort_by_key(|a| a.timestamp);

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
			let drawdown = (&running_high - &point.value) / &running_high;
			if drawdown >= threshold {
				let database_info = database.get_database_info().await?;
				let manifestation = Manifestation::new(database_info.id().as_uuid(), high_timestamp, point.timestamp);
				drop(database_info);
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

/// The range a windowed scan covers: `since` — or the aspect's earliest measurement when
/// `since` is `None` — through the aspect's latest measurement, or `None` when the window
/// opens at or after the last measurement.
///
/// Returning `None` instead of handing [`Outputs::analyze_range`] a start after its end keeps a
/// stale `since` from producing whatever a reversed range happens to produce. A window that
/// opens past the end of the series contains no measurements, so there is nothing to report.
///
/// # Errors
///
/// Returns an error if the database cannot name the aspect's latest measurement, or — when
/// `since` is `None` — its earliest.
async fn scan_range(database: &Database, aspect: &AspectId, since: Option<DateTime<Utc>>) -> Result<Option<(DateTime<Utc>, DateTime<Utc>)>> {
	let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;
	let start_time = match since {
		Some(since) => since,
		None => database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?,
	};
	Ok((start_time < end_time).then_some((start_time, end_time)))
}

#[cfg(test)]
mod window_tests {
	//! What a bounded window keeps, and the one way it legitimately disagrees with a
	//! full-history scan. Written because the `*_in_window` variants had no test and
	//! `since` had no recorded boundary rule.

	use chrono::{DateTime, TimeZone, Utc};
	use splimes::Spline;
	use weftdb::{
		database::traits::{AspectStructure, DatabaseStructure, Inputs}, AspectId, Database, DatasetId, InputMeasurement, Resolution
	};

	use super::*;
	use crate::test_support::remove_database;

	/// Capture `values` one hour apart from 2025-01-01 00:00 UTC and return the aspect.
	async fn hourly_aspect(db: &Database, name: &str, values: &[i64]) -> AspectId {
		let subject = db.observe_subject("WindowSubject").await.unwrap();
		let aspect = db.track_aspect(&subject.id(), name, &Resolution::Hours, None).await.unwrap();
		let start = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
		for (index, value) in values.iter().enumerate() {
			let hours = i64::try_from(index).expect("a test index always fits an i64");
			let measurement = InputMeasurement::new(start + chrono::Duration::hours(hours), bigdecimal::BigDecimal::from(*value));
			db.capture_measurement(&aspect.id(), &DatasetId::new(), &measurement).await.unwrap();
		}
		aspect.id()
	}

	fn hour(index: i64) -> DateTime<Utc> {
		Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::hours(index)
	}

	/// The `(start, end)` span of every manifestation, which is how a detector records
	/// the pair of points a feature was found across.
	/// The spans a detector run returned, as `(start, end)` pairs, sorted
	/// chronologically.
	///
	/// The analysis hands events back newest-first, so the sort is the tests' own
	/// bookkeeping: these cases pin *which* events a window keeps, not the order
	/// they arrive in.
	fn spans(events: &[Event]) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
		let mut found = Vec::new();
		for event in events {
			for manifestation in event.manifestations().values() {
				found.push((*manifestation.start(), *manifestation.end()));
			}
		}
		found.sort();
		found
	}

	#[tokio::test]
	async fn a_window_that_starts_before_a_crossing_matches_a_full_scan() {
		remove_database("orch_window_equivalence");
		let db = Database::new("orch_window_equivalence").await.unwrap();
		let aspect = hourly_aspect(&db, "Level", &[1, 2, 3, 10, 11]).await;
		let threshold = bigdecimal::BigDecimal::from(5);

		let full = detect_threshold_crossing_up(&db, &aspect, &Resolution::Hours, &Spline::Linear, threshold.clone(), "crossing").await.unwrap();
		// The crossing is the pair (3 @ hour 2, 10 @ hour 3): a span, not a point.
		assert_eq!(spans(&full), vec![(hour(2), hour(3))], "the fixture should produce exactly one crossing");

		// `since` at the very beginning of the series bounds the scan without dropping anything.
		let windowed = detect_threshold_crossing_up_in_window(&db, &aspect, &Resolution::Hours, &Spline::Linear, threshold, "crossing", Some(hour(0))).await.unwrap();
		assert_eq!(spans(&windowed), spans(&full), "a window starting at the series start must agree with a full scan");
		drop(db);
	}

	#[tokio::test]
	async fn a_crossing_whose_earlier_point_precedes_the_window_is_not_reported() {
		remove_database("orch_window_boundary");
		let db = Database::new("orch_window_boundary").await.unwrap();
		let aspect = hourly_aspect(&db, "Level", &[1, 2, 3, 10, 11]).await;
		let threshold = bigdecimal::BigDecimal::from(5);

		let full = detect_threshold_crossing_up(&db, &aspect, &Resolution::Hours, &Spline::Linear, threshold.clone(), "crossing").await.unwrap();
		assert_eq!(spans(&full).len(), 1, "the fixture should produce exactly one crossing");

		// `since` on the hour the level arrives leaves only the points at or above the
		// threshold, and a crossing needs the point *below* it too. This is the boundary
		// rule the doc comment states: `since` starts the analysis range, it does not
		// filter a full-history result down to the window.
		let windowed = detect_threshold_crossing_up_in_window(&db, &aspect, &Resolution::Hours, &Spline::Linear, threshold, "crossing", Some(hour(3))).await.unwrap();
		assert!(spans(&windowed).is_empty(), "a crossing whose earlier point is before `since` must not be reported, got {:?}", spans(&windowed));
		drop(db);
	}

	#[tokio::test]
	async fn a_window_drops_features_that_lie_before_it() {
		remove_database("orch_window_trims");
		let db = Database::new("orch_window_trims").await.unwrap();
		// Two crossings upward through 5: hours 2->3 and hours 4->5.
		let aspect = hourly_aspect(&db, "Level", &[8, 1, 2, 6, 2, 7]).await;
		let threshold = bigdecimal::BigDecimal::from(5);

		let full = detect_threshold_crossing_up(&db, &aspect, &Resolution::Hours, &Spline::Linear, threshold.clone(), "crossing").await.unwrap();
		assert_eq!(spans(&full), vec![(hour(2), hour(3)), (hour(4), hour(5))], "the fixture should produce two crossings");

		let windowed = detect_threshold_crossing_up_in_window(&db, &aspect, &Resolution::Hours, &Spline::Linear, threshold, "crossing", Some(hour(3))).await.unwrap();
		assert_eq!(spans(&windowed), vec![(hour(4), hour(5))], "a window from hour 3 keeps only the later crossing");
		drop(db);
	}

	/// Capture `values` at `day_offsets[i]` days after `start`, for the detectors that group
	/// points by calendar month and therefore need to straddle one.
	async fn dated_aspect(db: &Database, name: &str, start: DateTime<Utc>, day_offsets: &[i64], values: &[i64]) -> AspectId {
		let subject = db.observe_subject("WindowSubject").await.unwrap();
		let aspect = db.track_aspect(&subject.id(), name, &Resolution::Days, None).await.unwrap();
		for (offset, value) in day_offsets.iter().zip(values) {
			let measurement = InputMeasurement::new(start + chrono::Duration::days(*offset), bigdecimal::BigDecimal::from(*value));
			db.capture_measurement(&aspect.id(), &DatasetId::new(), &measurement).await.unwrap();
		}
		aspect.id()
	}

	#[tokio::test]
	async fn a_window_keeps_only_the_peak_inside_it() {
		remove_database("orch_window_peaks");
		let db = Database::new("orch_window_peaks").await.unwrap();
		// Two equal maxima, at hours 1 and 3, each flanked by lower points.
		let aspect = hourly_aspect(&db, "Level", &[1, 5, 1, 5, 1]).await;

		let full = detect_peaks(&db, &aspect, &Resolution::Hours, &Spline::Linear, "peak").await.unwrap();
		assert_eq!(spans(&full), vec![(hour(0), hour(2)), (hour(2), hour(4))], "the fixture should produce two peaks");

		let windowed = detect_peaks_in_window(&db, &aspect, &Resolution::Hours, &Spline::Linear, "peak", Some(hour(2))).await.unwrap();
		assert_eq!(spans(&windowed), vec![(hour(2), hour(4))], "a window opening at hour 2 keeps only the later peak");
		drop(db);
	}

	#[tokio::test]
	async fn a_window_keeps_only_the_local_peak_inside_it() {
		remove_database("orch_window_all_peaks");
		let db = Database::new("orch_window_all_peaks").await.unwrap();
		// The second bump is lower, so only `detect_all_peaks` is expected to report both.
		let aspect = hourly_aspect(&db, "Level", &[1, 5, 1, 4, 1]).await;

		let full = detect_all_peaks(&db, &aspect, &Resolution::Hours, &Spline::Linear, "peak").await.unwrap();
		assert_eq!(spans(&full), vec![(hour(0), hour(2)), (hour(2), hour(4))], "the fixture should produce two local peaks");

		let windowed = detect_all_peaks_in_window(&db, &aspect, &Resolution::Hours, &Spline::Linear, "peak", Some(hour(2))).await.unwrap();
		assert_eq!(spans(&windowed), vec![(hour(2), hour(4))], "a window opening at hour 2 keeps only the later local peak");
		drop(db);
	}

	#[tokio::test]
	async fn a_window_keeps_only_the_valley_inside_it() {
		remove_database("orch_window_valleys");
		let db = Database::new("orch_window_valleys").await.unwrap();
		let aspect = hourly_aspect(&db, "Level", &[5, 1, 5, 1, 5]).await;

		let full = detect_valleys(&db, &aspect, &Resolution::Hours, &Spline::Linear, "valley").await.unwrap();
		assert_eq!(spans(&full), vec![(hour(0), hour(2)), (hour(2), hour(4))], "the fixture should produce two valleys");

		let windowed = detect_valleys_in_window(&db, &aspect, &Resolution::Hours, &Spline::Linear, "valley", Some(hour(2))).await.unwrap();
		assert_eq!(spans(&windowed), vec![(hour(2), hour(4))], "a window opening at hour 2 keeps only the later valley");
		drop(db);
	}

	#[tokio::test]
	async fn a_window_keeps_only_the_local_valley_inside_it() {
		remove_database("orch_window_all_valleys");
		let db = Database::new("orch_window_all_valleys").await.unwrap();
		let aspect = hourly_aspect(&db, "Level", &[5, 1, 5, 2, 5]).await;

		let full = detect_all_valleys(&db, &aspect, &Resolution::Hours, &Spline::Linear, "valley").await.unwrap();
		assert_eq!(spans(&full), vec![(hour(0), hour(2)), (hour(2), hour(4))], "the fixture should produce two local valleys");

		let windowed = detect_all_valleys_in_window(&db, &aspect, &Resolution::Hours, &Spline::Linear, "valley", Some(hour(2))).await.unwrap();
		assert_eq!(spans(&windowed), vec![(hour(2), hour(4))], "a window opening at hour 2 keeps only the later local valley");
		drop(db);
	}

	#[tokio::test]
	async fn a_window_keeps_only_the_downward_crossing_inside_it() {
		remove_database("orch_window_cross_down");
		let db = Database::new("orch_window_cross_down").await.unwrap();
		// Two crossings downward through 5: hours 0->1 and hours 2->3.
		let aspect = hourly_aspect(&db, "Level", &[10, 3, 8, 2, 1]).await;
		let threshold = bigdecimal::BigDecimal::from(5);

		let full = detect_threshold_crossing_down(&db, &aspect, &Resolution::Hours, &Spline::Linear, threshold.clone(), "crossing").await.unwrap();
		assert_eq!(spans(&full), vec![(hour(0), hour(1)), (hour(2), hour(3))], "the fixture should produce two downward crossings");

		let windowed = detect_threshold_crossing_down_in_window(&db, &aspect, &Resolution::Hours, &Spline::Linear, threshold, "crossing", Some(hour(2))).await.unwrap();
		assert_eq!(spans(&windowed), vec![(hour(2), hour(3))], "a window opening at hour 2 keeps only the later crossing");
		drop(db);
	}

	#[tokio::test]
	async fn a_window_can_miss_a_decline_whose_high_lies_before_it() {
		remove_database("orch_window_drawdown");
		let db = Database::new("orch_window_drawdown").await.unwrap();
		// 120 -> 95 is a 20.8% fall from the high; from the 100 at hour 1 it is only 5%.
		let aspect = hourly_aspect(&db, "Level", &[120, 100, 95]).await;

		let full = detect_drawdown(&db, &aspect, &Resolution::Hours, &Spline::Linear, 0.2, "drawdown").await.unwrap();
		assert_eq!(spans(&full), vec![(hour(0), hour(2))], "the fixture should produce one drawdown from the hour-0 high");

		let windowed = detect_drawdown_in_window(&db, &aspect, &Resolution::Hours, &Spline::Linear, 0.2, "drawdown", Some(hour(1))).await.unwrap();
		assert!(spans(&windowed).is_empty(), "a window that drops the high must report nothing, got {:?}", spans(&windowed));
		drop(db);
	}

	#[tokio::test]
	async fn a_window_that_cuts_a_month_short_reports_no_increase() {
		remove_database("orch_window_monthly");
		let db = Database::new("orch_window_monthly").await.unwrap();
		let december = Utc.with_ymd_and_hms(2024, 12, 1, 0, 0, 0).unwrap();
		// Flat through December, then 100 -> 120 across the first half of January: a 20% month.
		let aspect = dated_aspect(&db, "Price", december, &[0, 14, 31, 45], &[100, 100, 100, 120]).await;

		let full = detect_monthly_increase(&db, &aspect, &Resolution::Days, &Spline::Linear, 0.1).await.unwrap();
		assert_eq!(spans(&full), vec![(december + chrono::Duration::days(31), december + chrono::Duration::days(45))], "the fixture should report January's rise from the 1st to the 15th");

		// Opening the window on the 10th leaves January starting near 113, so the month gains 6%.
		let windowed = detect_monthly_increase_in_window(&db, &aspect, &Resolution::Days, &Spline::Linear, 0.1, Some(december + chrono::Duration::days(40))).await.unwrap();
		assert!(spans(&windowed).is_empty(), "a partial month must be judged on its own points, got {:?}", spans(&windowed));
		drop(db);
	}

	#[tokio::test]
	async fn a_window_opening_after_the_last_measurement_reports_nothing() {
		remove_database("orch_window_past_end");
		let db = Database::new("orch_window_past_end").await.unwrap();
		let aspect = hourly_aspect(&db, "Level", &[1, 5, 1, 5, 1]).await;

		let full = detect_peaks(&db, &aspect, &Resolution::Hours, &Spline::Linear, "peak").await.unwrap();
		assert!(!spans(&full).is_empty(), "the fixture should produce a peak");

		// A caller holding a stale `since` gets an empty result, not a reversed range.
		let windowed = detect_peaks_in_window(&db, &aspect, &Resolution::Hours, &Spline::Linear, "peak", Some(hour(9))).await.unwrap();
		assert!(spans(&windowed).is_empty(), "a window opening after the last measurement must report nothing, got {:?}", spans(&windowed));
		drop(db);
	}
}
