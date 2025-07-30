use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use splimes::{Point, Resolution, Spline};

use crate::{types::AnalysisResult, AspectId, Database, CACHE};

impl Database {
	/// Analyze a single point in time for an aspect using interpolation
	///
	/// # Errors
	/// - if aspect not found
	/// - if unable to retrieve measurements
	/// - if interpolation fails
	///
	/// # Panics
	/// - if measurements collection is empty after validation
	/// - if min/max time calculations fail on valid measurements
	pub async fn analyze_point(&self, aspect: AspectId, time: DateTime<Utc>, resolution: Resolution, method: Spline) -> Result<Point> {
		// Check cache first for point analysis
		let cache_key = format!("point_{}_{}_{}_{:?}_{:?}", aspect.as_uuid(), time.timestamp(), time.timestamp_subsec_nanos(), resolution, method);
		if let Some(cached_result) = CACHE.get_point_analysis(&cache_key).await {
			return Ok(Point {
				timestamp: cached_result.timestamp, // Add missing timestamp
				value: cached_result.value,
			});
		}

		// Get measurements for this aspect
		let measurements = Self::get_aspect_measurements(aspect).await?;

		if measurements.is_empty() {
			bail!("No measurements found for aspect");
		}

		// Find min and max times in the data
		let min_time = measurements.iter().map(|m| m.timestamp).min().unwrap();
		let max_time = measurements.iter().map(|m| m.timestamp).max().unwrap();

		// Create a window around the target time
		let window_duration = chrono::Duration::minutes(10); // 10-minute window
		let mut window_start = time - window_duration;
		let mut window_end = time + window_duration;

		// Ensure the window includes actual data
		if window_start > max_time {
			window_start = min_time;
			window_end = time + chrono::Duration::minutes(5);
		} else if window_end < min_time {
			window_start = time - chrono::Duration::minutes(5);
			window_end = max_time;
		} else {
			window_start = window_start.min(min_time);
			window_end = window_end.max(max_time);
		}

		// Final safety check to ensure start is before end
		if window_start >= window_end {
			window_start = min_time;
			window_end = max_time;
		}

		// Use the GPU-aware auto_interpolate function
		let interpolated = splimes::auto_interpolate(&mut Self::measurements_to_points(&measurements), window_start, window_end, resolution, method).await?;

		// Find the measurement closest to the target time
		let Some(closest) = interpolated.iter().min_by_key(|m| (m.timestamp - time).num_milliseconds().abs()) else { bail!("No valid points found in interpolation") };

		// Cache the result
		let analysis_result = AnalysisResult { timestamp: time, value: closest.value.clone(), method: format!("{method:?}"), resolution: format!("{resolution:?}") };
		CACHE.store_point_analysis(&cache_key, &analysis_result).await;

		Ok(closest.clone())
	}

	/// Interpolates/extrapolates the `DataPoint`[] for a given time range, resolution, & spline type.
	/// Uses intelligent measurement collection and caching with GPU acceleration when beneficial.
	///
	/// # Errors
	/// - if interpolation fails
	pub async fn analyze_range(aspect: AspectId, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, method: Spline) -> Result<Vec<Point>> {
		// Get measurements for this aspect
		let measurements = Self::get_aspect_measurements(aspect).await?;

		if measurements.is_empty() {
			bail!("No measurements found for aspect");
		}

		// Use the GPU-aware auto_interpolate function
		splimes::auto_interpolate(&mut Self::measurements_to_points(&measurements), start, end, resolution, method).await
	}
}
