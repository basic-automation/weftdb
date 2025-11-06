use std::{pin::Pin, str::FromStr};

use anyhow::{bail, Result};
use bigdecimal::{BigDecimal, Zero};
use chrono::{DateTime, Utc};
use futures::Stream;
use splimes::{Point, Resolution, Spline};
use uuid::Uuid;

use crate::{
	database::traits::{AspectStructure, DatabaseStructure, Outputs}, types::database::traits::connection::Connection, AnalysisResult, AspectId, Batch, BatchId, BatchMetatdata, BatchedMeasurement, Database, DatasetId, Error, Measurement, MeasurementId
};

#[async_trait::async_trait]
impl Outputs for Database {
	async fn get_raw_measurements(&self, aspect_id: AspectId, start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>, max_per_page: usize, page: usize) -> Result<Vec<Measurement>> {
		// Calculate offset for pagination (0-based page indexing)
		let offset = page * max_per_page;

		// For small page sizes with caching, check cache first (only for full range queries)
		// Only cache if requesting first page, reasonable page size, and no time filtering
		if page == 0 && max_per_page <= 10_000 && start.is_none() && end.is_none() {
			let cache_key = format!("aspect_measurements_{}_{}", aspect_id.as_uuid(), max_per_page);
			if let Some(cached) = self.cache.lock().await.get::<Vec<Measurement>>(&cache_key).await {
				return Ok(cached);
			}
		}

		// Get from database with proper scope management
		let mut aspect = self.get_aspect(aspect_id).await?;
		let measurement_db = aspect.measurements().await?;

		// Build query and parameters based on time range
		let (query_sql, params): (String, Vec<String>) = match (start, end) {
			(None, None) => {
				// Return all points
				(
					r"
                    SELECT id, dataset_id, timestamp, value 
                    FROM measurements 
                    ORDER BY timestamp ASC 
                    LIMIT ? OFFSET ?
                    "
					.to_string(),
					vec![max_per_page.to_string(), offset.to_string()],
				)
			}
			(Some(start_time), None) => {
				// From start to end of data
				(
					r"
                    SELECT id, dataset_id, timestamp, value 
                    FROM measurements 
                    WHERE timestamp >= ?
                    ORDER BY timestamp ASC 
                    LIMIT ? OFFSET ?
                    "
					.to_string(),
					vec![start_time.timestamp_millis().to_string(), max_per_page.to_string(), offset.to_string()],
				)
			}
			(None, Some(end_time)) => {
				// From beginning to end
				(
					r"
                    SELECT id, dataset_id, timestamp, value 
                    FROM measurements 
                    WHERE timestamp <= ?
                    ORDER BY timestamp ASC 
                    LIMIT ? OFFSET ?
                    "
					.to_string(),
					vec![end_time.timestamp_millis().to_string(), max_per_page.to_string(), offset.to_string()],
				)
			}
			(Some(start_time), Some(end_time)) => {
				// Specific range
				(
					r"
                    SELECT id, dataset_id, timestamp, value 
                    FROM measurements 
                    WHERE timestamp >= ? AND timestamp <= ?
                    ORDER BY timestamp ASC 
                    LIMIT ? OFFSET ?
                    "
					.to_string(),
					vec![start_time.timestamp_millis().to_string(), end_time.timestamp_millis().to_string(), max_per_page.to_string(), offset.to_string()],
				)
			}
		};

		let cache_key = aspect.measurements_path();
		let conn = Self::begin_concurrent(&measurement_db, &cache_key, Some(self.cache.clone())).await?;

		// Convert String parameters to turso::Value
		let turso_params: Vec<turso::Value> = params.into_iter().map(turso::Value::from).collect();

		let mut rows = conn.as_ref().query(&query_sql, turso_params).await.map_err(|e| Error::DatabaseError(format!("Failed to query measurements: {e}")))?;

		let mut measurements = Vec::with_capacity(max_per_page.min(1000));

		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get row: {e}")))? {
			let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("ID is not text".to_string()))?.clone();
			let dataset_id_str = row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Dataset ID is not text".to_string()))?.clone();
			let timestamp_millis_str = row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Timestamp is not text".to_string()))?.clone();
			let value_str = row.get_value(3)?.as_text().ok_or_else(|| Error::DatabaseError("Value is not text".to_string()))?.clone();

			// Parse the UUIDs from string
			let id = MeasurementId::from_string(&id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for measurement ID: {e}")))?;

			let dataset_id = DatasetId::from_str(&dataset_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for dataset ID: {e}")))?;

			let timestamp_millis: i64 = timestamp_millis_str.parse().map_err(|e| Error::DatabaseError(format!("Invalid timestamp format: {e}")))?;

			let timestamp = DateTime::from_timestamp_millis(timestamp_millis).ok_or_else(|| Error::DatabaseError("Invalid timestamp".to_string()))?;

			let value = BigDecimal::from_str(&value_str).map_err(|e| Error::DatabaseError(format!("Invalid value format: {e}")))?;

			measurements.push(Measurement::new(id, dataset_id, timestamp, value));
		}

		// Handle case where requested range is outside of available data
		if measurements.is_empty() && (start.is_some() || end.is_some()) {
			// Check if we have any data at all and if the requested range is outside
			return self.get_boundary_measurements(aspect_id).await;
		}

		// Cache the results only for first page, reasonable sizes, and full range queries
		if page == 0 && max_per_page <= 10_000 && !measurements.is_empty() && start.is_none() && end.is_none() {
			let cache_key = format!("aspect_measurements_{}_{}", aspect_id.as_uuid(), max_per_page);
			self.cache.lock().await.store(&cache_key, measurements.clone()).await;
		}

		let _ = Self::commit_concurrent(&conn).await;

		Ok(measurements)
	}

	/// Helper function to get boundary measurements (earliest 2 and latest 2 points)
	/// Used when requested range is outside of available data
	async fn get_boundary_measurements(&self, aspect_id: AspectId) -> Result<Vec<Measurement>> {
		let mut aspect = self.get_aspect(aspect_id).await?;
		let measurement_db = aspect.measurements().await?;
		let conn = measurement_db.connect()?;

		let mut all_measurements: Vec<Measurement> = Vec::new();

		// Get earliest 2 measurements
		let earliest_sql = r"
        SELECT id, dataset_id, timestamp, value 
        FROM measurements 
        ORDER BY timestamp ASC 
        LIMIT 2
    ";

		let mut rows = conn.query(earliest_sql, turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to query earliest measurements: {e}")))?;

		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get earliest row: {e}")))? {
			let measurement: Measurement = self.parse_measurement_row(row).await?;
			all_measurements.push(measurement);
		}

		// Get latest 2 measurements (avoid duplicates if we have <= 2 total measurements)
		let latest_sql = r"
        SELECT id, dataset_id, timestamp, value 
        FROM measurements 
        ORDER BY timestamp DESC 
        LIMIT 2
    ";

		let mut rows = conn.query(latest_sql, turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to query latest measurements: {e}")))?;

		let mut latest_measurements: Vec<Measurement> = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get latest row: {e}")))? {
			let measurement: Measurement = self.parse_measurement_row(row).await?;
			latest_measurements.push(measurement);
		}

		// Add latest measurements, avoiding duplicates
		for latest in latest_measurements.into_iter().rev() {
			// Reverse to maintain chronological order
			if !all_measurements.iter().any(|m: &Measurement| m.id() == latest.id()) {
				all_measurements.push(latest);
			}
		}

		// Sort by timestamp to maintain chronological order
		all_measurements.sort_by_key(|m: &Measurement| m.timestamp());

		Ok(all_measurements)
	}

	/// Helper function to parse a measurement row
	async fn parse_measurement_row(&self, row: turso::Row) -> Result<Measurement> {
		let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("ID is not text".to_string()))?.clone();
		let dataset_id_str = row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Dataset ID is not text".to_string()))?.clone();
		let timestamp_millis_str = row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Timestamp is not text".to_string()))?.clone();
		let value_str = row.get_value(3)?.as_text().ok_or_else(|| Error::DatabaseError("Value is not text".to_string()))?.clone();

		let id = MeasurementId::from_string(&id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for measurement ID: {e}")))?;
		let dataset_id = DatasetId::from_str(&dataset_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for dataset ID: {e}")))?;
		let timestamp_millis: i64 = timestamp_millis_str.parse().map_err(|e| Error::DatabaseError(format!("Invalid timestamp format: {e}")))?;
		let timestamp = DateTime::from_timestamp_millis(timestamp_millis).ok_or_else(|| Error::DatabaseError("Invalid timestamp".to_string()))?;
		let value = BigDecimal::from_str(&value_str).map_err(|e| Error::DatabaseError(format!("Invalid value format: {e}")))?;

		Ok(Measurement::new(id, dataset_id, timestamp, value))
	}

	/// Get the total count of measurements for an aspect (useful for pagination)
	async fn get_measurements_count(&self, aspect_id: AspectId) -> Result<usize> {
		let mut aspect = self.get_aspect(aspect_id).await?;
		let measurement_db = aspect.measurements().await?;

		let count_sql = "SELECT COUNT(*) FROM measurements";
		let conn = measurement_db.connect()?;
		let mut rows = conn.query(count_sql, turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to count measurements: {e}")))?;

		if let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get count row: {e}")))? {
			let count_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Count is not text".to_string()))?.clone();
			let count: usize = count_str.parse().map_err(|e| Error::DatabaseError(format!("Invalid count format: {e}")))?;
			Ok(count)
		} else {
			Ok(0)
		}
	}

	/// Analyze a single point in time for an aspect using interpolation
	/// Now uses pagination to efficiently load measurements for interpolation
	///
	/// # Errors
	/// - if aspect not found
	/// - if unable to retrieve measurements
	/// - if interpolation fails
	async fn analyze_point(&self, aspect_id: AspectId, time: DateTime<Utc>, resolution: Resolution, method: Spline) -> Result<Point> {
		// Check cache first for point analysis
		let cache_key = format!("point_{}_{}_{}_{:?}_{:?}", aspect_id.as_uuid(), time.timestamp(), time.timestamp_subsec_nanos(), resolution, method);
		if let Some(cached_result) = self.cache.lock().await.get::<AnalysisResult>(&cache_key).await {
			return Ok(Point { timestamp: cached_result.timestamp(), value: cached_result.value().clone() });
		}

		// Get measurements efficiently using pagination with time range optimization
		// For point analysis, fetch data around the target time for better efficiency
		let window = resolution.to_step() * 100; // Get a reasonable window around the target
		let range_start = time - window;
		let range_end = time + window;

		let initial_page_size = 10_000;
		let mut all_measurements = Vec::new();
		let mut page = 0;
		let mut found_target_range = false;

		// Fetch measurements until we have enough data around our target time
		loop {
			let measurements = self.get_raw_measurements(aspect_id, Some(range_start), Some(range_end), initial_page_size, page).await?;

			if measurements.is_empty() {
				// No data in the time window, try getting boundary measurements
				if page == 0 {
					let boundary_measurements = self.get_boundary_measurements(aspect_id).await?;
					if !boundary_measurements.is_empty() {
						all_measurements = boundary_measurements;
					}
				}
				break;
			}

			// Check if our target time falls within this page's time range
			let first_time = measurements.first().map(super::super::measurement::Measurement::timestamp);
			let last_time = measurements.last().map(super::super::measurement::Measurement::timestamp);

			if let (Some(first), Some(last)) = (first_time, last_time) {
				if time >= first && time <= last {
					found_target_range = true;
				}
			}

			all_measurements.extend(measurements);

			// If we found our target range and have sufficient data for the interpolation method, we can stop
			let min_points_needed = match method {
				Spline::Linear => 2,
				Spline::Quadratic => 3,
				Spline::Cubic => 4,
				Spline::Polynomial(degree, _) => degree + 1,
			};

			if found_target_range && all_measurements.len() >= min_points_needed {
				break;
			}

			// If we haven't found the target range yet, or need more points, continue
			page += 1;

			// Safety check to prevent infinite loops
			if page > 10 {
				// Reduced since we're using a time window
				break;
			}
		}

		if all_measurements.is_empty() {
			bail!("No measurements found for aspect");
		}

		// Sort measurements by timestamp to ensure proper ordering - fix type annotation
		all_measurements.sort_by_key(|m: &Measurement| m.timestamp());

		// Convert to Points for splimes
		let mut points: Vec<Point> = all_measurements.iter().map(|m| Point { timestamp: m.timestamp(), value: m.value().clone() }).collect();

		// Use splimes::auto_interpolate which handles both interpolation and extrapolation
		// We just need a single point, so set end_time slightly after our target
		let end_time = time + resolution.to_step();
		let interpolated = splimes::auto_interpolate(&mut points, time, end_time, resolution, method).await?;

		// Get the interpolated point (should be the first and likely only point)
		let point = interpolated.into_iter().next().unwrap_or_else(|| Point { timestamp: time, value: BigDecimal::zero() });

		// Cache the result
		let method_description = if all_measurements.len() < 2 {
			format!("{method:?} (insufficient data)")
		} else {
			// Determine if this was interpolation or extrapolation
			let first_time = all_measurements.first().unwrap().timestamp();
			let last_time = all_measurements.last().unwrap().timestamp();

			if time < first_time {
				format!("{method:?} (backward extrapolation)")
			} else if time > last_time {
				format!("{method:?} (forward extrapolation)")
			} else {
				format!("{method:?}")
			}
		};

		let analysis_result = AnalysisResult::new(time, point.value.clone(), method_description, format!("{resolution:?}"));
		self.cache.lock().await.store(&cache_key, analysis_result).await;

		Ok(point)
	}

	/// Interpolates/extrapolates the `DataPoint`[] for a given time range, resolution, & spline type.
	/// Uses intelligent measurement collection with pagination for memory efficiency.
	///
	/// # Errors
	/// - if interpolation fails
	async fn analyze_range(&self, aspect_id: AspectId, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, method: Spline) -> Result<Pin<Box<dyn Stream<Item = Result<Point>> + Send + 'static>>> {
		// Pre-fetch all measurements for the range to avoid async issues in the stream
		// For very large ranges, this could be optimized further with lazy loading
		let all_measurements = self.fetch_measurements_for_range(aspect_id, start, end).await?;

		if all_measurements.is_empty() {
			// Try boundary measurements if no data in range
			let boundary_measurements = self.get_boundary_measurements(aspect_id).await?;
			if boundary_measurements.is_empty() {
				return Err(anyhow::anyhow!("No measurements found for aspect"));
			}

			// Use boundary measurements for extrapolation
			let mut points: Vec<Point> = boundary_measurements.iter().map(|m| Point { timestamp: m.timestamp(), value: m.value().clone() }).collect();

			// Interpolate the entire range at once
			let interpolated = splimes::auto_interpolate(&mut points, start, end, resolution, method).await?;

			// Convert to stream
			let point_stream = futures::stream::iter(interpolated.into_iter().map(Ok));
			return Ok(Box::pin(point_stream));
		}

		// Convert measurements to points
		let mut points: Vec<Point> = all_measurements.iter().map(|m| Point { timestamp: m.timestamp(), value: m.value().clone() }).collect();

		// For large datasets, we'll process in chunks to avoid memory issues
		let total_duration = end - start;
		let step_duration = resolution.to_step();
		let total_ns = total_duration.num_nanoseconds().unwrap_or(i64::MAX);
		let step_ns = step_duration.num_nanoseconds().unwrap_or(1);
		let expected_points = if step_ns > 0 { (total_ns / step_ns) + 1 } else { 1 };

		// If expected output is reasonable, process all at once
		if expected_points < 1_000_000 {
			// Small enough to process all at once
			let interpolated = splimes::auto_interpolate(&mut points, start, end, resolution, method).await?;
			let point_stream = futures::stream::iter(interpolated.into_iter().map(Ok));
			return Ok(Box::pin(point_stream));
		}

		// For very large outputs, process in time-based chunks
		let chunk_size = 100_000; // Target points per chunk
		let chunk_count = (expected_points + chunk_size - 1) / chunk_size; // Ceiling division
		let chunk_duration_ns = total_ns / chunk_count;
		let chunk_duration = chrono::Duration::nanoseconds(chunk_duration_ns);

		// Calculate overlap needed for interpolation method
		let overlap_points = match method {
			Spline::Linear => 1,
			Spline::Quadratic => 2,
			Spline::Cubic => 3,
			Spline::Polynomial(d, _) => d,
		};
		let overlap_duration = step_duration * i32::try_from(overlap_points).unwrap_or(3);

		Ok(Box::pin(futures::stream::unfold((start, None::<std::vec::IntoIter<Point>>), move |(mut current, mut current_iter)| {
			let points_clone = points.clone();
			let end_time = end;
			let resolution_val = resolution;
			let method_val = method;

			async move {
				// Return next point from current chunk if available
				if let Some(ref mut iter) = current_iter {
					if let Some(point) = iter.next() {
						return Some((Ok(point), (current, current_iter)));
					}
				}

				// If we're done, return None
				if current >= end_time {
					return None;
				}

				// Process next chunk
				let chunk_end = (current + chunk_duration).min(end_time);
				let fetch_start = (current - overlap_duration).max(start);
				let fetch_end = (chunk_end + overlap_duration).min(end_time);

				// Filter points for this chunk's time range
				let mut chunk_points: Vec<Point> = points_clone.iter().filter(|p| p.timestamp >= fetch_start && p.timestamp <= fetch_end).cloned().collect();

				if chunk_points.is_empty() {
					// No data for this chunk, skip to next
					current = chunk_end;
					return Some((Ok(Point { timestamp: current, value: BigDecimal::zero() }), (current, None)));
				}

				// Interpolate this chunk
				match splimes::auto_interpolate(&mut chunk_points, current, chunk_end, resolution_val, method_val).await {
					Ok(interpolated) => {
						current = chunk_end;
						let mut new_iter = interpolated.into_iter();
						// Return first point and set up iterator for the rest
						new_iter.next().map_or_else(|| Some((Ok(Point { timestamp: current, value: BigDecimal::zero() }), (current, None))), |first_point| Some((Ok(first_point), (current, Some(new_iter)))))
					}
					Err(e) => Some((Err(e), (current, None))),
				}
			}
		})))
	}

	/// Helper function to fetch measurements for a time range using pagination
	/// Efficiently loads all measurements within the specified time range
	async fn fetch_measurements_for_range(&self, aspect_id: AspectId, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Vec<Measurement>> {
		let mut all_measurements = Vec::new();
		let page_size = 10_000; // Reasonable page size for range queries
		let mut page = 0;

		loop {
			let measurements = self.get_raw_measurements(aspect_id, Some(start), Some(end), page_size, page).await?;

			if measurements.is_empty() {
				break; // No more data
			}

			all_measurements.extend(measurements.clone());

			// If we got less than a full page, we've reached the end
			if measurements.len() < page_size {
				break;
			}

			page += 1;

			// Safety check to prevent infinite loops
			if page > 1000 {
				// Maximum 10M measurements
				break;
			}
		}

		// Sort by timestamp to ensure proper ordering for interpolation - fix type annotation
		all_measurements.sort_by_key(|m: &Measurement| m.timestamp());

		Ok(all_measurements)
	}

	async fn get_unprocessed_batch(&self, aspect_id: &AspectId, batch_id: &BatchId) -> Result<Batch> {
		// Check cache first using aspect_id and batch_id as cache key
		let cache_key = format!("unprocessed_batch_{}_{}", aspect_id.as_uuid(), batch_id.as_uuid());

		// Try to get from cache first
		if let Some(cached_batches) = self.cache.lock().await.get::<Vec<Batch>>(&cache_key).await {
			// Since we're looking for a specific batch, find it in the cached results
			if let Some(batch) = cached_batches.into_iter().find(|b| *b.batch_id() == *batch_id) {
				return Ok(batch);
			}
			// If not found in cache, fall through to database query
		}

		// Get from database
		let mut aspect = self.get_aspect(*aspect_id).await?;
		let unprocessed_batches_db = aspect.unprocessed_batches().await?;
		let conn = unprocessed_batches_db.connect()?;

		let query_sql = r"
			SELECT id, aspect_id, database_id, size, resolution, batch_hash, 
				created_at, processed_at, updated_at, metadata_json, measurements_json
			FROM batches 
			WHERE id = ?
		";

		let mut rows = conn.query(query_sql, vec![turso::Value::from(batch_id.as_uuid().to_string())]).await.map_err(|e| Error::DatabaseError(format!("Failed to query batch: {e}")))?;

		if let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get batch row: {e}")))? {
			let metadata_json_str = row.get_value(9)?.as_text().ok_or_else(|| Error::DatabaseError("Metadata JSON is not text".to_string()))?.clone();
			let measurements_json_str = row.get_value(10)?.as_text().ok_or_else(|| Error::DatabaseError("Measurements JSON is not text".to_string()))?.clone();

			let metadata: BatchMetatdata = serde_json::from_str(&metadata_json_str).map_err(|e| Error::DatabaseError(format!("Failed to parse batch metadata JSON: {e}")))?;

			let measurements: Vec<BatchedMeasurement> = serde_json::from_str(&measurements_json_str).map_err(|e| Error::DatabaseError(format!("Failed to parse batch measurements JSON: {e}")))?;

			let batch_hash_str = row.get_value(5)?.as_text().cloned();

			let batch = Batch { metadata, measurements, batch_id: *batch_id, batch_hash: batch_hash_str };

			// Cache the single batch (as a vec with one element)
			self.cache.lock().await.store(&cache_key, vec![batch.clone()]).await;

			Ok(batch)
		} else {
			Err(anyhow::anyhow!("Batch with ID {} not found", batch_id))
		}
	}

	/// Get unprocessed batches in queue order (oldest first)
	/// This represents unprocessed batches that are ready to be processed into processed batches
	async fn get_unprocessed_batches(&self, aspect_id: &AspectId) -> Result<Pin<Box<dyn Stream<Item = Result<Batch>> + Send + 'static>>> {
		// Check cache first
		let cache_key = format!("unprocessed_batches_{}", aspect_id.as_uuid());
		if let Some(cached_batches) = self.cache.lock().await.get::<Vec<Batch>>(&cache_key).await {
			// Convert cached batches to stream
			let batch_stream = futures::stream::iter(cached_batches.into_iter().map(Ok));
			return Ok(Box::pin(batch_stream));
		}

		// Get from database
		let mut aspect = self.get_aspect(*aspect_id).await?;
		let unprocessed_batches_db = aspect.unprocessed_batches().await?;
		let conn = unprocessed_batches_db.connect()?;

		// Query batches ordered by created_at (oldest first) for queue processing
		let query_sql = r"
                        SELECT id, aspect_id, database_id, size, resolution, batch_hash, 
                                created_at, updated_at, metadata_json, measurements_json
                        FROM batches 
                        WHERE aspect_id = ? AND database_id = ?
                        ORDER BY created_at ASC
                ";

		let mut rows = conn.query(query_sql, turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query unprocessed batches: {e}")))?;

		// Collect all batches first to avoid async issues in the stream
		let mut batches = Vec::new();

		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get batch row: {e}")))? {
			if let Ok(batch) = Self::parse_batch_row(row).await {
				batches.push(batch);
			} else {
				// Log error but continue processing other batches
				eprintln!("Warning: Failed to parse batch row");
			}
		} // Cache the results for future queries
		if !batches.is_empty() {
			self.cache.lock().await.store(&cache_key, batches.clone()).await;
		}

		// Convert to stream
		let batch_stream = futures::stream::iter(batches.into_iter().map(Ok));
		Ok(Box::pin(batch_stream))
	}

	/// Helper function to parse a batch row from the database
	async fn parse_batch_row(row: turso::Row) -> Result<Batch> {
		let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Batch ID is not text".to_string()))?.clone();
		let batch_id = BatchId::from_uuid(Uuid::parse_str(&id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for batch ID: {e}")))?);

		let metadata_json_str = row.get_value(8)?.as_text().ok_or_else(|| Error::DatabaseError("Metadata JSON is not text".to_string()))?.clone();
		let measurements_json_str = row.get_value(9)?.as_text().ok_or_else(|| Error::DatabaseError("Measurements JSON is not text".to_string()))?.clone();

		let metadata: BatchMetatdata = serde_json::from_str(&metadata_json_str).map_err(|e| Error::DatabaseError(format!("Failed to parse batch metadata JSON: {e}")))?;

		let measurements: Vec<BatchedMeasurement> = serde_json::from_str(&measurements_json_str).map_err(|e| Error::DatabaseError(format!("Failed to parse batch measurements JSON: {e}")))?;

		let batch_hash_str = row.get_value(5)?.as_text().cloned();

		Ok(Batch { metadata, measurements, batch_id, batch_hash: batch_hash_str })
	}
}
