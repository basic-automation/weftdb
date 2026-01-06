use std::{
	collections::HashMap, pin::Pin, str::FromStr
};

use anyhow::{bail, Result};
use bigdecimal::{BigDecimal, Zero};
use chrono::{DateTime, Utc};
use futures::{Stream, StreamExt, stream};
use splimes::{Point, Resolution, Spline};
use uuid::Uuid;

use crate::{
	cache, correlation::ErrorRate, database::traits::{AspectStructure, DatabaseStructure, Outputs}, types::database::traits::connection::Connection, AnalysisResult, AspectId, Batch, BatchId, BatchMetatdata, BatchedMeasurement, Correlation, CorrelationID, Database, DatabaseInfo, DatasetId, DictionaryConstraints, DictionaryId, DictionaryMetadata, Error, Event, EventID, Manifestation, ManifestationId, Measurement, MeasurementId, Occurrence, Pattern, PatternID, Relative, SignalType, Steps, SubjectId, VariablilityType
};

#[async_trait::async_trait]
impl Outputs for Database {
	/// Streams measurements using cursor-based pagination for O(n) performance.
	/// Uses WHERE timestamp > last_timestamp instead of OFFSET for efficient large dataset handling.
	async fn get_raw_measurements(&self, aspect_id: &AspectId, start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>, max_per_page: usize, _page: usize) -> Result<Pin<Box<dyn Stream<Item = Result<Measurement>> + Send + 'static>>> {
		tracing::debug!(aspect_id = %aspect_id.as_uuid(), "[get_raw_measurements] Starting");
		
		// For small page sizes with caching, check cache first (only for full range queries)
		if max_per_page <= 10_000 && start.is_none() && end.is_none() {
			let cache_key = format!("aspect_measurements_{}_{}", aspect_id.as_uuid(), max_per_page);
			if let Some(cached) = self.cache.lock().await.get::<Vec<Measurement>>(&cache_key).await {
				tracing::debug!("[get_raw_measurements] Returning cached measurements");
				return Ok(Box::pin(stream::iter(cached.into_iter().map(Ok))));
			}
		}

		// Get database connection info upfront for the streaming closure
		tracing::debug!("[get_raw_measurements] Getting measurement db...");
		let db = self.get_measurement_db(aspect_id).await?;
		tracing::debug!("[get_raw_measurements] Got measurement db");
		let db_path = self.get_measurement_db_path(aspect_id).await?;
		tracing::debug!(db_path = %db_path, "[get_raw_measurements] Got db path");
		let cache = self.cache.clone();
		let start_owned = start;
		let end_owned = end;

		// Use very large chunk size for efficiency - fewer transactions
		const CHUNK_SIZE: usize = 1_000_000;

		// State: (last_timestamp_cursor, current_chunk_buffer, db, db_path, cache, start, end, is_first_query)
		// last_timestamp_cursor: None means we haven't started, Some(ts) means fetch records > ts
		let stream = futures::stream::unfold(
			(None::<i64>, Vec::<Measurement>::new(), db, db_path, cache, start_owned, end_owned),
			move |(cursor, mut buffer, db, db_path, cache, start, end)| {
				async move {
					// If we have measurements in the buffer, return one
					if let Some(measurement) = buffer.pop() {
						return Some((Ok(measurement), (cursor, buffer, db, db_path, cache, start, end)));
					}

					tracing::debug!(cursor = ?cursor, "[stream] Buffer empty, fetching next chunk");

					// Need to fetch next chunk using cursor-based pagination
					tracing::debug!("[stream] About to begin_concurrent...");
					let conn = match Self::begin_concurrent(&db, &db_path, Some(cache.clone())).await {
						Ok(c) => {
							tracing::debug!("[stream] begin_concurrent succeeded");
							c
						},
						Err(e) => {
							tracing::error!(error = %e, "[stream] begin_concurrent FAILED");
							return Some((Err(e), (cursor, buffer, db, db_path, cache, start, end)));
						}
					};

					let limit_i64 = CHUNK_SIZE as i64;

					// Build query using cursor-based pagination (WHERE timestamp > cursor)
					// This is O(1) per page instead of O(n) with OFFSET
					let (query_sql, params): (String, Vec<turso::Value>) = match (cursor, start, end) {
						// First query: use start time as lower bound
						(None, None, None) => (
							"SELECT id, dataset_id, timestamp, value FROM measurements ORDER BY timestamp ASC LIMIT ?".to_string(),
							vec![turso::Value::from(limit_i64)],
						),
						(None, Some(start_time), None) => (
							"SELECT id, dataset_id, timestamp, value FROM measurements WHERE timestamp >= ? ORDER BY timestamp ASC LIMIT ?".to_string(),
							vec![turso::Value::from(start_time.timestamp_millis()), turso::Value::from(limit_i64)],
						),
						(None, None, Some(end_time)) => (
							"SELECT id, dataset_id, timestamp, value FROM measurements WHERE timestamp <= ? ORDER BY timestamp ASC LIMIT ?".to_string(),
							vec![turso::Value::from(end_time.timestamp_millis()), turso::Value::from(limit_i64)],
						),
						(None, Some(start_time), Some(end_time)) => (
							"SELECT id, dataset_id, timestamp, value FROM measurements WHERE timestamp >= ? AND timestamp <= ? ORDER BY timestamp ASC LIMIT ?".to_string(),
							vec![turso::Value::from(start_time.timestamp_millis()), turso::Value::from(end_time.timestamp_millis()), turso::Value::from(limit_i64)],
						),
						// Subsequent queries: use cursor (last timestamp) as lower bound
						(Some(last_ts), _, None) => (
							"SELECT id, dataset_id, timestamp, value FROM measurements WHERE timestamp > ? ORDER BY timestamp ASC LIMIT ?".to_string(),
							vec![turso::Value::from(last_ts), turso::Value::from(limit_i64)],
						),
						(Some(last_ts), _, Some(end_time)) => (
							"SELECT id, dataset_id, timestamp, value FROM measurements WHERE timestamp > ? AND timestamp <= ? ORDER BY timestamp ASC LIMIT ?".to_string(),
							vec![turso::Value::from(last_ts), turso::Value::from(end_time.timestamp_millis()), turso::Value::from(limit_i64)],
						),
					};

					tracing::debug!("[stream] About to execute query...");
					let rows_result = conn.as_ref().query(&query_sql, params).await;
					tracing::debug!("[stream] Query executed!");
					let mut rows = match rows_result {
						Ok(r) => {
							tracing::debug!("[stream] Query succeeded");
							r
						},
						Err(e) => {
							tracing::error!(error = %e, "[stream] Query FAILED");
							let _ = Self::commit_concurrent(&conn).await;
							return Some((Err(Error::DatabaseError(format!("Failed to query measurements: {e}")).into()), (cursor, buffer, db, db_path, cache, start, end)));
						}
					};

					// Collect measurements from this chunk and track the last timestamp
					let mut new_measurements = Vec::with_capacity(CHUNK_SIZE);
					let mut new_cursor: Option<i64> = cursor;

					loop {
						match rows.next().await {
							Ok(Some(row)) => {
								match Self::parse_measurement_row_static(row).await {
									Ok(measurement) => {
										// Update cursor to the timestamp of this measurement
										new_cursor = Some(measurement.timestamp().timestamp_millis());
										new_measurements.push(measurement);
									}
									Err(e) => {
										let _ = Self::commit_concurrent(&conn).await;
										return Some((Err(e), (new_cursor, buffer, db, db_path, cache, start, end)));
									}
								}
							}
							Ok(None) => break,
							Err(e) => {
								let _ = Self::commit_concurrent(&conn).await;
								return Some((Err(Error::DatabaseError(format!("Failed to read measurement row: {e}")).into()), (new_cursor, buffer, db, db_path, cache, start, end)));
							}
						}
					}

					let _ = Self::commit_concurrent(&conn).await;

					if new_measurements.is_empty() {
						// No more data, end the stream
						return None;
					}

					// Reverse so we can pop from the end efficiently (LIFO for FIFO order)
					new_measurements.reverse();

					// Pop first measurement and return it
					if let Some(measurement) = new_measurements.pop() {
						Some((Ok(measurement), (new_cursor, new_measurements, db, db_path, cache, start, end)))
					} else {
						None
					}
				}
			},
		);

		Ok(Box::pin(stream))
	}

	/// Helper function to get boundary measurements (earliest 2 and latest 2 points)
	/// Used when requested range is outside of available data
	async fn get_boundary_measurements(&self, aspect_id: &AspectId) -> Result<Pin<Box<dyn Stream<Item = Result<Measurement>> + Send + 'static>>> {
		let db = self.get_measurement_db(aspect_id).await?;
		let db_path = self.get_measurement_db_path(aspect_id).await?;

		let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let mut all_measurements: Vec<Measurement> = Vec::new();

		// Get earliest 2 measurements
		let earliest_sql = r"
                        SELECT id, dataset_id, timestamp, value
                                FROM measurements
                                ORDER BY timestamp ASC
                                LIMIT 2
                ";

		let mut rows: turso::Rows = conn.as_ref().query(earliest_sql, turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to query earliest measurements: {e}")))?;

		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get earliest row: {e}")))? {
			let measurement: Measurement = Self::parse_measurement_row_static(row).await?;
			all_measurements.push(measurement);
		}

		// Get latest 2 measurements (avoid duplicates if we have <= 2 total measurements)
		let latest_sql = r"
                        SELECT id, dataset_id, timestamp, value
                                FROM measurements
                                ORDER BY timestamp DESC
                                LIMIT 2
                ";

		let mut rows: turso::Rows = conn.as_ref().query(latest_sql, turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to query latest measurements: {e}")))?;

		let mut latest_measurements: Vec<Measurement> = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get latest row: {e}")))? {
			let measurement: Measurement = Self::parse_measurement_row_static(row).await?;
			latest_measurements.push(measurement);
		}

		// Add latest measurements, avoiding duplicates
		for latest in latest_measurements.into_iter().rev() {
			// Reverse to maintain chronological order
			if !all_measurements.iter().any(|m: &Measurement| m.id() == latest.id()) {
				all_measurements.push(latest);
			}
		}

		let _ = Self::commit_concurrent(&conn).await;

		// Sort by timestamp to maintain chronological order
		all_measurements.sort_by_key(|m: &Measurement| m.timestamp());

		// Convert to stream
		let measurement_stream = futures::stream::iter(all_measurements.into_iter().map(Ok));
		Ok(Box::pin(measurement_stream))
	}

	/// Helper function to parse a measurement row
	async fn parse_measurement_row(&self, row: turso::Row) -> Result<Measurement> {
		let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("ID is not text".to_string()))?.clone();
		let dataset_id_str = row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Dataset ID is not text".to_string()))?.clone();
		let timestamp_millis: i64 = *row.get_value(2)?.as_integer().ok_or_else(|| Error::DatabaseError("Timestamp is not integer".to_string()))?;
		let value_str = row.get_value(3)?.as_text().ok_or_else(|| Error::DatabaseError("Value is not text".to_string()))?.clone();

		let id = MeasurementId::from_string(&id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for measurement ID: {e}")))?;
		let dataset_id = DatasetId::from_str(&dataset_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for dataset ID: {e}")))?;
		let timestamp = DateTime::from_timestamp_millis(timestamp_millis).ok_or_else(|| Error::DatabaseError("Invalid timestamp".to_string()))?;
		let value = BigDecimal::from_str(&value_str).map_err(|e| Error::DatabaseError(format!("Invalid value format: {e}")))?;

		Ok(Measurement::new(id, dataset_id, timestamp, value))
	}

	/// Get the total count of measurements for an aspect (useful for pagination)
	async fn get_measurements_count(&self, aspect_id: &AspectId) -> Result<usize> {
		let measurement_db = self.get_measurement_db(aspect_id).await?;
		let db_path = self.get_measurement_db_path(aspect_id).await?;

		let count_sql = "SELECT COUNT(*) FROM measurements";
		let conn: cache::Connection = Self::begin_concurrent(&measurement_db, &db_path, Some(self.cache.clone())).await?;
		let mut rows: turso::Rows = conn.as_ref().query(count_sql, turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to count measurements: {e}")))?;

		let _ = Self::commit_concurrent(&conn).await;

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
	async fn analyze_point(&self, aspect_id: &AspectId, time: DateTime<Utc>, resolution: &Resolution, method: &Spline) -> Result<Point> {
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
			let measurements: Vec<Measurement> = self.get_raw_measurements(aspect_id, Some(range_start), Some(range_end), initial_page_size, page).await?.collect::<Vec<_>>().await.into_iter().collect::<Result<Vec<_>>>()?;

			if measurements.is_empty() {
				// No data in the time window, try getting boundary measurements
				if page == 0 {
						let _boundary_measurements: Vec<Measurement> = self.get_boundary_measurements(aspect_id).await?.collect::<Vec<_>>().await.into_iter().collect::<Result<Vec<_>>>()?;
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
		let interpolated = splimes::auto_interpolate(&mut points, time, end_time, *resolution, *method).await?;

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
	async fn analyze_range(&self, aspect_id: &AspectId, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, method: Spline) -> Result<Pin<Box<dyn Stream<Item = Result<Point>> + Send + 'static>>> {
		// Pre-fetch all measurements for the range to avoid async issues in the stream
		// For very large ranges, this could be optimized further with lazy loading
		tracing::info!(start = %start, end = %end, "[analyze_range] Starting measurement collection");
		tracing::debug!("[analyze_range] About to call fetch_measurements_for_range...");
		
		let mut measurement_stream = self.fetch_measurements_for_range(aspect_id, start, end).await?;
		tracing::debug!("[analyze_range] fetch_measurements_for_range returned, stream created");
		tracing::debug!("[analyze_range] About to call stream.next() for first item...");
		
		let mut all_measurements: Vec<Measurement> = Vec::new();
		let mut count = 0usize;
		let progress_interval = 1_000_000; // Report every 1M measurements
		
		while let Some(result) = measurement_stream.next().await {
			if count == 0 {
				tracing::debug!("[analyze_range] Received first measurement from stream!");
			}
			let measurement = result?;
			all_measurements.push(measurement);
			count += 1;
			if count % progress_interval == 0 {
				tracing::info!(count = count, "[analyze_range] Loaded measurements...");
			}
		}
		tracing::info!(total = count, "[analyze_range] Finished loading measurements");

		if all_measurements.is_empty() {
			// Try boundary measurements if no data in range
			let boundary_measurements: Vec<Measurement> = self.get_boundary_measurements(aspect_id).await?.collect::<Vec<_>>().await.into_iter().collect::<Result<Vec<_>>>()?;
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
		tracing::debug!(count = all_measurements.len(), "[analyze_range] Converting measurements to points...");
		let mut points: Vec<Point> = all_measurements.iter().map(|m| Point { timestamp: m.timestamp(), value: m.value().clone() }).collect();

		// For large datasets, we'll process in chunks to avoid memory issues
		let total_duration = end - start;
		let step_duration = resolution.to_step();
		let total_ns = total_duration.num_nanoseconds().unwrap_or(i64::MAX);
		let step_ns = step_duration.num_nanoseconds().unwrap_or(1);
		let expected_points = if step_ns > 0 { (total_ns / step_ns) + 1 } else { 1 };
		tracing::info!(expected_points = expected_points, step = ?step_duration, "[analyze_range] Calculated output");

		// If expected output is reasonable, process all at once
		if expected_points < 1_000_000 {
			// Small enough to process all at once
			tracing::debug!(expected_points = expected_points, "[analyze_range] Processing all at once (< 1M threshold)");
			let interpolated = splimes::auto_interpolate(&mut points, start, end, resolution, method).await?;
			tracing::info!(output_points = interpolated.len(), "[analyze_range] Interpolation complete");
			let point_stream = futures::stream::iter(interpolated.into_iter().map(Ok));
			return Ok(Box::pin(point_stream));
		}

		// For very large outputs, process in time-based chunks
		tracing::info!(expected_points = expected_points, "[analyze_range] Using chunked processing");
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
	async fn fetch_measurements_for_range(&self, aspect_id: &AspectId, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Pin<Box<dyn Stream<Item = Result<Measurement>> + Send + 'static>>> {
		// Use get_raw_measurements directly to get a streaming result
		// Set max_per_page to a reasonable size and page to 0 to start
		// The stream will handle pagination internally
		let page_size = 10_000;
		let measurements_stream = self.get_raw_measurements(aspect_id, Some(start), Some(end), page_size, 0).await?;

		Ok(measurements_stream)
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
		let db = self.get_unprocessed_batches_db(aspect_id).await?;
		let db_path = self.get_unprocessed_batches_db_path(aspect_id).await?;
		let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// Query matches schema: id, aspect_id, database_id, size, resolution, measurements, batch_hash, status, created_at, processed_at, updated_at
		let query_sql = r"
			SELECT id, aspect_id, database_id, size, resolution, measurements, batch_hash
			FROM batches 
			WHERE id = ?
		";

		let mut rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![batch_id.as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query batch: {e}")))?;

		let _ = Self::commit_concurrent(&conn).await;

		if let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get batch row: {e}")))? {
			let batch = parse_batch_row_helper(&row, self.name(), &db_path)?;

			// Cache the single batch (as a vec with one element)
			self.cache.lock().await.store(&cache_key, vec![batch.clone()]).await;

			Ok(batch)
		} else {
			Err(anyhow::anyhow!("Batch with ID {batch_id} not found"))
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
		let db = self.get_unprocessed_batches_db(aspect_id).await?;
		let db_path = self.get_unprocessed_batches_db_path(aspect_id).await?;
		let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// Query matches schema: id, aspect_id, database_id, size, resolution, measurements, batch_hash
		let query_sql = r"
			SELECT id, aspect_id, database_id, size, resolution, measurements, batch_hash
			FROM batches 
			WHERE aspect_id = ? AND database_id = ?
			ORDER BY created_at ASC
		";

		let mut rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query unprocessed batches: {e}")))?;

		// Collect all batches first to avoid async issues in the stream
		let mut batches = Vec::new();
		let db_name = self.name();
		let db_path_clone = db_path.clone();

		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get batch row: {e}")))? {
			if let Ok(batch) = parse_batch_row_helper(&row, db_name, &db_path_clone) {
				batches.push(batch);
			} else {
				// Log error but continue processing other batches
				tracing::warn!("Failed to parse batch row");
			}
		}
		
		// Cache the results for future queries
		if !batches.is_empty() {
			self.cache.lock().await.store(&cache_key, batches.clone()).await;
		}

		// Convert to stream
		let batch_stream = futures::stream::iter(batches.into_iter().map(Ok));

		let _ = Self::commit_concurrent(&conn).await;
		Ok(Box::pin(batch_stream))
	}

	// Processed batches

	async fn get_processed_batch(&self, aspect_id: &AspectId, batch_id: &BatchId) -> Result<Batch> {
		// Check cache first using aspect_id and batch_id as cache key
		let cache_key = format!("processed_batch_{}_{}", aspect_id.as_uuid(), batch_id.as_uuid());

		// Try to get from cache first
		if let Some(cached_batches) = self.cache.lock().await.get::<Vec<Batch>>(&cache_key).await {
			// Since we're looking for a specific batch, find it in the cached results
			if let Some(batch) = cached_batches.into_iter().find(|b| *b.batch_id() == *batch_id) {
				return Ok(batch);
			}
			// If not found in cache, fall through to database query
		}

		// Get from database
		let db = self.get_processed_batches_db(aspect_id).await?;
		let db_path = self.get_processed_batches_db_path(aspect_id).await?;
		let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// Query matches schema: id, aspect_id, database_id, size, resolution, measurements, batch_hash
		let query_sql = r"
			SELECT id, aspect_id, database_id, size, resolution, measurements, batch_hash
			FROM batches 
			WHERE id = ?
		";

		let mut rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![batch_id.as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query batch: {e}")))?;

		if let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get batch row: {e}")))? {
			let batch = parse_batch_row_helper(&row, self.name(), &db_path)?;

			// Cache the single batch (as a vec with one element)
			self.cache.lock().await.store(&cache_key, vec![batch.clone()]).await;

			let _ = Self::commit_concurrent(&conn).await;

			Ok(batch)
		} else {
			let _ = Self::commit_concurrent(&conn).await;
			Err(anyhow::anyhow!("Batch with ID {batch_id} not found"))
		}
	}

	async fn get_processed_batches(&self, aspect_id: &AspectId) -> Result<Pin<Box<dyn Stream<Item = Result<Batch>> + Send + 'static>>> {
		// Check cache first
		let cache_key = format!("processed_batches_{}", aspect_id.as_uuid());
		if let Some(cached_batches) = self.cache.lock().await.get::<Vec<Batch>>(&cache_key).await {
			// Convert cached batches to stream
			let batch_stream = futures::stream::iter(cached_batches.into_iter().map(Ok));
			return Ok(Box::pin(batch_stream));
		}

		// Get from database
		let db = self.get_processed_batches_db(aspect_id).await?;
		let db_path = self.get_processed_batches_db_path(aspect_id).await?;
		let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// Query matches schema: id, aspect_id, database_id, size, resolution, measurements, batch_hash
		let query_sql = r"
			SELECT id, aspect_id, database_id, size, resolution, measurements, batch_hash
			FROM batches 
			WHERE aspect_id = ? AND database_id = ?
			ORDER BY created_at ASC
		";

		let mut rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query processed batches: {e}")))?;

		// Collect all batches first to avoid async issues in the stream
		let mut batches = Vec::new();
		let db_name = self.name();
		let db_path_clone = db_path.clone();

		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get batch row: {e}")))? {
			if let Ok(batch) = parse_batch_row_helper(&row, db_name, &db_path_clone) {
				batches.push(batch);
			} else {
				// Log error but continue processing other batches
				tracing::warn!("Failed to parse batch row");
			}
		}

		let _ = Self::commit_concurrent(&conn).await;

		// Cache the results for future queries
		if !batches.is_empty() {
			self.cache.lock().await.store(&cache_key, batches.clone()).await;
		}

		// Convert to stream
		let batch_stream = futures::stream::iter(batches.into_iter().map(Ok));
		Ok(Box::pin(batch_stream))
	}

	//
	// dictionaries
	//

	async fn get_dictionary_metadata(&self, aspect_id: &AspectId, dictionary_name: &str) -> Result<Option<DictionaryMetadata>> {
		// Check cache first
		let cache_key = format!("dictionary_metadata_{dictionary_name}");
		if let Some(metadata) = self.cache.lock().await.get::<DictionaryMetadata>(&cache_key).await {
			return Ok(Some(metadata));
		}

		// Get from database
		let db = self.get_dictionary_db(aspect_id, dictionary_name).await?;
		let conn = Self::begin_concurrent(&db, dictionary_name, Some(self.cache.clone())).await?;
		let query_sql = r"
                        SELECT id, name, description, created_at, updated_at
                        FROM dictionary_metadata 
                        WHERE name = ?
                ";

		let mut metadata_rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![dictionary_name]).await.map_err(|e| Error::DatabaseError(format!("Failed to query dictionary metadata: {e}")))?;
		if let Some(row) = metadata_rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get metadata row: {e}")))? {
			let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Dictionary ID is not text".to_string()))?.clone();
			let name_str = row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Dictionary name is not text".to_string()))?.clone();
			let description_str = row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Dictionary description is not text".to_string()))?.clone();
			let created_at_millis: i64 = *row.get_value(3)?.as_integer().ok_or_else(|| Error::DatabaseError("Created at is not integer".to_string()))?;
			let updated_at_millis: i64 = *row.get_value(4)?.as_integer().ok_or_else(|| Error::DatabaseError("Updated at is not integer".to_string()))?;

			let id = DictionaryId::from_uuid(Uuid::parse_str(&id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for dictionary ID: {e}")))?);
			let _created_at = DateTime::from_timestamp_millis(created_at_millis).ok_or_else(|| Error::DatabaseError("Invalid created at timestamp".to_string()))?;
			let _updated_at = DateTime::from_timestamp_millis(updated_at_millis).ok_or_else(|| Error::DatabaseError("Invalid updated at timestamp".to_string()))?;

			let constraint_query_sql = r"
                                SELECT steps_count, steps_interpolation
                                FROM dictionary_constraints
                                WHERE dictionary_id = ?
                        "
			.to_string();

			let mut constraint_rows: turso::Rows = conn.as_ref().query(&constraint_query_sql, turso::params![id.as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query dictionary constraints: {e}")))?;
			let result = if let Some(constraint_row) = constraint_rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get constraint row: {e}")))? {
				let steps_count_str = constraint_row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Steps count is not text".to_string()))?.clone();
				let steps_interpolation_str = constraint_row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Steps interpolation is not text".to_string()))?.clone();

				// Parse steps configuration
				let steps: Option<Steps> = if steps_count_str.is_empty() || steps_count_str == "null" {
					None
				} else {
					let count: usize = steps_count_str.parse().map_err(|e| Error::DatabaseError(format!("Invalid steps count format: {e}")))?;
					let interpolation: Spline = steps_interpolation_str.parse().map_err(|e| Error::DatabaseError(format!("Invalid interpolation format: {e}")))?;
					Some(Steps::new(count, interpolation))
				};
				
				// Parse variabilities - try to get from a separate query or use None
				let variabilities: Option<Vec<VariablilityType>> = None;

				let constraints = DictionaryConstraints::new(steps, variabilities);

				let metadata = DictionaryMetadata { id, name: name_str, description: description_str, constraints };

				// Cache the metadata
				self.cache.lock().await.store(&cache_key, metadata.clone()).await;

				Some(metadata)
			} else {
				None
			};
			let _ = Self::commit_concurrent(&conn).await;
			Ok(result)
		} else {
			let _ = Self::commit_concurrent(&conn).await;
			Ok(None)
		}
	}

	async fn list_dictionaries(&self, aspect_id: &AspectId) -> Result<Vec<DictionaryMetadata>> {
		// Check cache first
		let cache_key = format!("dictionaries_list_{}", aspect_id.as_uuid());
		if let Some(dictionaries) = self.cache.lock().await.get::<Vec<DictionaryMetadata>>(&cache_key).await {
			return Ok(dictionaries);
		}

		// Get from database
		let db = self.get_dictionary_db(aspect_id, "default").await?;
		let conn = Self::begin_concurrent(&db, "default", Some(self.cache.clone())).await?;
		let query_sql = r"
                        SELECT id, name, description, created_at, updated_at
                        FROM dictionary_metadata
                ";

		let mut rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to query dictionaries: {e}")))?;
		let mut dictionaries: Vec<DictionaryMetadata> = Vec::new();

		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get dictionary row: {e}")))? {
			let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Dictionary ID is not text".to_string()))?.clone();
			let name_str = row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Dictionary name is not text".to_string()))?.clone();
			let description_str = row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Dictionary description is not text".to_string()))?.clone();
			let created_at_millis: i64 = *row.get_value(3)?.as_integer().ok_or_else(|| Error::DatabaseError("Created at is not integer".to_string()))?;
			let updated_at_millis: i64 = *row.get_value(4)?.as_integer().ok_or_else(|| Error::DatabaseError("Updated at is not integer".to_string()))?;

			let id = DictionaryId::from_uuid(Uuid::parse_str(&id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for dictionary ID: {e}")))?);
			let _created_at = DateTime::from_timestamp_millis(created_at_millis).ok_or_else(|| Error::DatabaseError("Invalid created at timestamp".to_string()))?;
			let _updated_at = DateTime::from_timestamp_millis(updated_at_millis).ok_or_else(|| Error::DatabaseError("Invalid updated at timestamp".to_string()))?;

			let metadata = DictionaryMetadata { id, name: name_str, description: description_str, constraints: DictionaryConstraints::default() };

			dictionaries.push(metadata);
		}
		let _ = Self::commit_concurrent(&conn).await;
		self.cache.lock().await.store(&cache_key, dictionaries.clone()).await;
		Ok(dictionaries)
	}

	async fn get_dictionary_pattern(&self, aspect_id: &AspectId, dictionary_name: &str, pattern_id: &PatternID) -> Result<Pattern> {
		// Check cache first using aspect_id and batch_id as cache key
		let cache_key = format!("pattern_{}", pattern_id.as_uuid());

		// Try to get from cache first
		if let Some(pattern) = self.cache.lock().await.get::<Pattern>(&cache_key).await {
			return Ok(pattern);
		}

		// Get from database
		let db = self.get_dictionary_db(aspect_id, dictionary_name).await?;
		let conn = Self::begin_concurrent(&db, dictionary_name, Some(self.cache.clone())).await?;
		let query_sql = r"
                        SELECT id, sum_value, abs_sum_value, max_value, min_value, abs_max_value, avg_value, abs_avg_value
                        FROM patterns 
                        WHERE id = ?
                ";

		let mut rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![pattern_id.as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query pattern: {e}")))?;
		let _ = Self::commit_concurrent(&conn).await;

		if let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get pattern row: {e}")))? {
			let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Pattern ID is not text".to_string()))?.clone();
			let sum_value_str = row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Sum value is not text".to_string()))?.clone();
			let abs_sum_value_str = row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Abs sum value is not text".to_string()))?.clone();
			let max_value_str = row.get_value(3)?.as_text().ok_or_else(|| Error::DatabaseError("Max value is not text".to_string()))?.clone();
			let min_value_str = row.get_value(4)?.as_text().ok_or_else(|| Error::DatabaseError("Min value is not text".to_string()))?.clone();
			let abs_max_value_str = row.get_value(5)?.as_text().ok_or_else(|| Error::DatabaseError("Abs max value is not text".to_string()))?.clone();
			let avg_value_str = row.get_value(6)?.as_text().ok_or_else(|| Error::DatabaseError("Avg value is not text".to_string()))?.clone();
			let abs_avg_value_str = row.get_value(7)?.as_text().ok_or_else(|| Error::DatabaseError("Abs avg value is not text".to_string()))?.clone();

			let id = PatternID::from_uuid(Uuid::parse_str(&id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for pattern ID: {e}")))?);
			let _sum_value = BigDecimal::from_str(&sum_value_str).map_err(|e| Error::DatabaseError(format!("Invalid sum value format: {e}")))?;
			let _abs_sum_value = BigDecimal::from_str(&abs_sum_value_str).map_err(|e| Error::DatabaseError(format!("Invalid abs sum value format: {e}")))?;
			let _max_value = BigDecimal::from_str(&max_value_str).map_err(|e| Error::DatabaseError(format!("Invalid max value format: {e}")))?;
			let _min_value = BigDecimal::from_str(&min_value_str).map_err(|e| Error::DatabaseError(format!("Invalid min value format: {e}")))?;
			let _abs_max_value = BigDecimal::from_str(&abs_max_value_str).map_err(|e| Error::DatabaseError(format!("Invalid abs max value format: {e}")))?;
			let _avg_value = BigDecimal::from_str(&avg_value_str).map_err(|e| Error::DatabaseError(format!("Invalid avg value format: {e}")))?;
			let _abs_avg_value = BigDecimal::from_str(&abs_avg_value_str).map_err(|e| Error::DatabaseError(format!("Invalid abs avg value format: {e}")))?;

			// Get occurrences from pattern_occurrences table
			let occurrences_query_sql = r"
                                SELECT pattern_id, aspect_id, resolution, size, database_info, beginning_timestamp, end_timestamp
                                FROM pattern_occurrences
                                WHERE pattern_id = ?
                        ";
			let mut occ_rows: turso::Rows = conn.as_ref().query(occurrences_query_sql, turso::params![pattern_id.as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query pattern occurrences: {e}")))?;
			let mut occurrences: Vec<Occurrence> = Vec::new();
			while let Some(occ_row) = occ_rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get pattern occurrence row: {e}")))? {
				// Column indices: 0=pattern_id, 1=aspect_id, 2=resolution, 3=size, 4=database_info, 5=beginning_timestamp, 6=end_timestamp
				let occ_pattern_id_str = occ_row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Occurrence Pattern ID is not text".to_string()))?.clone();
				let occ_aspect_id_str = occ_row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Occurrence Aspect ID is not text".to_string()))?.clone();
				let occ_resolution_str = occ_row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Occurrence Resolution is not text".to_string()))?.clone();
				let occ_size: usize = usize::try_from(*occ_row.get_value(3)?.as_integer().ok_or_else(|| Error::DatabaseError("Occurrence Size is not integer".to_string()))?).map_err(|e| Error::DatabaseError(format!("Occurrence size out of range: {e}")))?;
				let occ_database_info_str = occ_row.get_value(4)?.as_text().ok_or_else(|| Error::DatabaseError("Occurrence Database Info is not text".to_string()))?.clone();
				let occ_beginning_timestamp_millis: i64 = *occ_row.get_value(5)?.as_integer().ok_or_else(|| Error::DatabaseError("Occurrence Beginning Timestamp is not integer".to_string()))?;
				let occ_end_timestamp_millis: i64 = *occ_row.get_value(6)?.as_integer().ok_or_else(|| Error::DatabaseError("Occurrence End Timestamp is not integer".to_string()))?;

				let occ_pattern_id = PatternID::from_uuid(Uuid::parse_str(&occ_pattern_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for occurrence Pattern ID: {e}")))?);
				let occ_aspect_id = AspectId::from_str(&occ_aspect_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for occurrence Aspect ID: {e}")))?;
				let occ_resolution = Resolution::from_str(&occ_resolution_str).map_err(|e| Error::DatabaseError(format!("Invalid resolution format: {e}")))?;
				let occ_database_info: DatabaseInfo = serde_json::from_str(&occ_database_info_str).map_err(|e| Error::DatabaseError(format!("Failed to parse occurrence database info JSON: {e}")))?;
				let occ_beginning_timestamp = DateTime::from_timestamp_millis(occ_beginning_timestamp_millis).ok_or_else(|| Error::DatabaseError("Invalid beginning timestamp".to_string()))?;
				let occ_end_timestamp = DateTime::from_timestamp_millis(occ_end_timestamp_millis).ok_or_else(|| Error::DatabaseError("Invalid end timestamp".to_string()))?;

				let occurrence = Occurrence::new(occ_aspect_id, occ_resolution, occ_size, occ_database_info, occ_pattern_id, occ_beginning_timestamp, occ_end_timestamp);
				occurrences.push(occurrence);
			}

			// Get relatives from pattern_relatives table
			let relatives_query_sql = r"
                                SELECT pattern_id, relative_index, relative_value
                                FROM pattern_relatives
                                WHERE pattern_id = ?
                                ORDER BY relative_index ASC
                        ";

			let mut rel_rows: turso::Rows = conn.as_ref().query(relatives_query_sql, turso::params![pattern_id.as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query pattern relatives: {e}")))?;
			let mut relatives: Vec<Relative> = Vec::new();
			while let Some(rel_row) = rel_rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get pattern relative row: {e}")))? {
				let _rel_pattern_id_str = rel_row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Relative Pattern ID is not text".to_string()))?.clone();
				let _rel_relative_index: usize = usize::try_from(*rel_row.get_value(1)?.as_integer().ok_or_else(|| Error::DatabaseError("Relative Index is not integer".to_string()))?).map_err(|e| Error::DatabaseError(format!("Relative index out of range: {e}")))?;
				let rel_value_json = rel_row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Relative Value is not text".to_string()))?.clone();

				let relative: Relative = serde_json::from_str(&rel_value_json).map_err(|e| Error::DatabaseError(format!("Failed to parse relative JSON: {e}")))?;
				relatives.push(relative);
			}

			let pattern = Pattern::new(id, occurrences, relatives);

			// Cache the pattern
			self.cache.lock().await.store(&cache_key, pattern.clone()).await;

			Ok(pattern)
		} else {
			Err(anyhow::anyhow!("Pattern with ID {pattern_id} not found"))
		}
	}

	async fn get_dictionary_patterns(&self, aspect_id: &AspectId, dictionary_name: &str) -> Result<Pin<Box<dyn Stream<Item = Result<Pattern>> + Send + 'static>>> {
		// Check cache first
		let cache_key = format!("patterns_{dictionary_name}");
		if let Some(cached_patterns) = self.cache.lock().await.get::<Vec<Pattern>>(&cache_key).await {
			// Convert cached patterns to stream
			let pattern_stream = futures::stream::iter(cached_patterns.into_iter().map(Ok));
			return Ok(Box::pin(pattern_stream));
		}

		// Get from database
		let db = self.get_dictionary_db(aspect_id, dictionary_name).await?;
		let conn = Self::begin_concurrent(&db, dictionary_name, Some(self.cache.clone())).await?;
		let query_sql = r"
                        SELECT id, sum_value, abs_sum_value, max_value, min_value, abs_max_value, avg_value, abs_avg_value
                        FROM patterns 
                ";
		let mut rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to query patterns: {e}")))?;
		// Collect all patterns first to avoid async issues in the stream
		let mut patterns = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get pattern row: {e}")))? {
			let pattern_id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Pattern ID is not text".to_string()))?.clone();
			let pattern_id = PatternID::from_uuid(Uuid::parse_str(&pattern_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for pattern ID: {e}")))?);
			// Use get_dictionary_pattern to get the full pattern with occurrences and relatives
			if let Ok(pattern) = self.get_dictionary_pattern(aspect_id, dictionary_name, &pattern_id).await {
				patterns.push(pattern);
			} else {
				// Log error but continue processing other patterns
				tracing::warn!("Failed to parse pattern row for ID {pattern_id_str}");
			}
		}
		let _ = Self::commit_concurrent(&conn).await;
		// Cache the results for future queries
		if !patterns.is_empty() {
			self.cache.lock().await.store(&cache_key, patterns.clone()).await;
		}
		// Convert to stream
		let pattern_stream = futures::stream::iter(patterns.into_iter().map(Ok));
		Ok(Box::pin(pattern_stream))
	}

	//
	// Correlations
	//

	async fn get_correlation(&self, aspect_id: &AspectId, correlation_id: &CorrelationID) -> Result<Correlation> {
		// Check cache first using aspect_id and correlation_id as cache key
		let cache_key = format!("correlation_{}_{}", aspect_id.as_uuid(), correlation_id.to_uuid());

		// Try to get from cache first
		if let Some(cached_correlation) = self.cache.lock().await.get::<Correlation>(&cache_key).await {
			return Ok(cached_correlation);
		}

		// Get from database
		let mut aspect = self.get_aspect(aspect_id).await?;
		let db = aspect.correlations().await?;
		let db_path = aspect.correlations_path();
		let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		let query_sql = r"
                        SELECT id, dictionary_id, subject_id, aspect_id, pattern_id, event_id, average_distance_value, average_distance_units
                        FROM correlations 
                        WHERE id = ?
                ";

		let mut rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![correlation_id.to_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query correlation: {e}")))?;
		if let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get correlation row: {e}")))? {
			let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Correlation ID is not text".to_string()))?.clone();
			let dictionary_id_str = row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Dictionary ID is not text".to_string()))?.clone();
			let subject_id_str = row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Subject ID is not text".to_string()))?.clone();
			let aspect_id_str = row.get_value(3)?.as_text().ok_or_else(|| Error::DatabaseError("Aspect ID is not text".to_string()))?.clone();
			let pattern_id_str = row.get_value(4)?.as_text().ok_or_else(|| Error::DatabaseError("Pattern ID is not text".to_string()))?.clone();
			let event_id_str = row.get_value(5)?.as_text().ok_or_else(|| Error::DatabaseError("Event ID is not text".to_string()))?.clone();
			
			// Parse average_distance (nullable columns)
			let average_distance = {
				let avg_dist_value_opt = row.get_value(6).ok().and_then(|v| v.as_text().cloned());
				let avg_dist_units_opt = row.get_value(7).ok().and_then(|v| v.as_text().cloned());
				
				match (avg_dist_value_opt, avg_dist_units_opt) {
					(Some(value_str), Some(units_str)) => {
						let value = BigDecimal::from_str(&value_str).ok();
						let units = Resolution::from_str(&units_str).ok();
						match (value, units) {
							(Some(v), Some(u)) => Some(crate::types::signal::Distance::new(v, u)),
							_ => None,
						}
					}
					_ => None,
				}
			};

			let id = CorrelationID::from_uuid(Uuid::parse_str(&id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for correlation ID: {e}")))?);
			let dictionary_id = DictionaryId::from_uuid(Uuid::parse_str(&dictionary_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for dictionary ID: {e}")))?);
			let subject_id = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for subject ID: {e}")))?);
			let aspect_id = AspectId::from_uuid(Uuid::parse_str(&aspect_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for aspect ID: {e}")))?);
			let pattern_id = PatternID::from_uuid(Uuid::parse_str(&pattern_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for pattern ID: {e}")))?);
			let event_id = EventID::from_uuid(Uuid::parse_str(&event_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for event ID: {e}")))?);

			// get error rates
			let error_rate_query_sql = r"
                                SELECT signal_type, error_rate_value, error_rate_units
                                FROM correlation_error_rates
                                WHERE correlation_id = ?
                        ";
			let mut error_rate_rows: turso::Rows = conn.as_ref().query(error_rate_query_sql, turso::params![correlation_id.to_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query correlation error rates: {e}")))?;
			let mut error_rate: HashMap<SignalType, ErrorRate> = HashMap::new();
			while let Some(error_rate_row) = error_rate_rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get correlation error rate row: {e}")))? {
				let signal_type_str = error_rate_row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Error rate signal type is not text".to_string()))?.clone();
				let error_rate_value: BigDecimal = {
					let value_str = error_rate_row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Error rate value is not text".to_string()))?.clone();
					BigDecimal::from_str(&value_str).map_err(|e| Error::DatabaseError(format!("Invalid error rate value format: {e}")))?
				};
				let error_rate_units_str = error_rate_row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Error rate units is not text".to_string()))?.clone();

				let units: Resolution = Resolution::from_str(&error_rate_units_str).map_err(|e| Error::DatabaseError(format!("Invalid error rate units format: {e}")))?;
				let err_rate = ErrorRate::new(error_rate_value, units);

				let signal_type = SignalType::from_str(&signal_type_str).map_err(|e| Error::DatabaseError(format!("Invalid error type format: {e}")))?;
				// Currently not used, but could be stored in Correlation if needed

				error_rate.insert(signal_type, err_rate);
			}

			// get occurrences
			let occurrences_query_sql = r"
                                SELECT correlation_id, occurrence_index, aspect_id, resolution, size, database_info, pattern_id, beginning_timestamp, end_timestamp
                                FROM correlation_occurrences
                                WHERE correlation_id = ?
                        ";
			let mut occ_rows: turso::Rows = conn.as_ref().query(occurrences_query_sql, turso::params![correlation_id.to_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query correlation occurrences: {e}")))?;
			let mut occurrences: Vec<Occurrence> = Vec::new();
			// load occerrences in order of occurrence_index
			let mut occ_map: HashMap<usize, Occurrence> = HashMap::new();
			while let Some(occ_row) = occ_rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get correlation occurrence row: {e}")))? {
				let occ_index: usize = usize::try_from(*occ_row.get_value(1)?.as_integer().ok_or_else(|| Error::DatabaseError("Occurrence Index is not integer".to_string()))?).map_err(|e| Error::DatabaseError(format!("Occurrence index out of range: {e}")))?;
				let occ_aspect_id_str = occ_row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Occurrence Aspect ID is not text".to_string()))?.clone();
				let occ_resolution_str = occ_row.get_value(3)?.as_text().ok_or_else(|| Error::DatabaseError("Occurrence Resolution is not text".to_string()))?.clone();
				let occ_size: usize = usize::try_from(*occ_row.get_value(4)?.as_integer().ok_or_else(|| Error::DatabaseError("Occurrence Size is not integer".to_string()))?).map_err(|e| Error::DatabaseError(format!("Occurrence size out of range: {e}")))?;
				let occ_database_info_str = occ_row.get_value(5)?.as_text().ok_or_else(|| Error::DatabaseError("Occurrence Database Info is not text".to_string()))?.clone();
				let occ_pattern_id_str = occ_row.get_value(6)?.as_text().ok_or_else(|| Error::DatabaseError("Occurrence Pattern ID is not text".to_string()))?.clone();
				let occ_beginning_timestamp_millis: i64 = *occ_row.get_value(7)?.as_integer().ok_or_else(|| Error::DatabaseError("Occurrence Beginning Timestamp is not integer".to_string()))?;
				let occ_end_timestamp_millis: i64 = *occ_row.get_value(8)?.as_integer().ok_or_else(|| Error::DatabaseError("Occurrence End Timestamp is not integer".to_string()))?;
				let occ_aspect_id = AspectId::from_str(&occ_aspect_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for occurrence Aspect ID: {e}")))?;
				let occ_resolution = Resolution::from_str(&occ_resolution_str).map_err(|e| Error::DatabaseError(format!("Invalid resolution format: {e}")))?;
				let occ_database_info: DatabaseInfo = serde_json::from_str(&occ_database_info_str).map_err(|e| Error::DatabaseError(format!("Failed to parse occurrence database info JSON: {e}")))?;
				let occ_pattern_id = PatternID::from_uuid(Uuid::parse_str(&occ_pattern_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for occurrence Pattern ID: {e}")))?);
				let occ_beginning_timestamp = DateTime::from_timestamp_millis(occ_beginning_timestamp_millis).ok_or_else(|| Error::DatabaseError("Invalid beginning timestamp".to_string()))?;
				let occ_end_timestamp = DateTime::from_timestamp_millis(occ_end_timestamp_millis).ok_or_else(|| Error::DatabaseError("Invalid end timestamp".to_string()))?;
				let occurrence = Occurrence::new(occ_aspect_id, occ_resolution, occ_size, occ_database_info, occ_pattern_id, occ_beginning_timestamp, occ_end_timestamp);
				occ_map.insert(occ_index, occurrence);
			}
			// Sort occurrences by index
			let mut occ_indices: Vec<usize> = occ_map.keys().copied().collect();
			occ_indices.sort_unstable();
			for index in occ_indices {
				if let Some(occurrence) = occ_map.get(&index) {
					occurrences.push(occurrence.clone());
				}
			}

			let correlation = Correlation::new(Some(id), dictionary_id, subject_id, &aspect_id, pattern_id, event_id, error_rate, occurrences, average_distance);

			// Cache the correlation
			self.cache.lock().await.store(&cache_key, correlation.clone()).await;
			let _ = Self::commit_concurrent(&conn).await;
			Ok(correlation)
		} else {
			let _ = Self::commit_concurrent(&conn).await;
			Err(anyhow::anyhow!("Correlation with ID {correlation_id} not found"))
		}
	}

	async fn get_correlations(&self, aspect_id: &AspectId) -> Result<Pin<Box<dyn Stream<Item = Result<Correlation>> + Send + 'static>>> {
		// Get database connection info upfront for the streaming closure
		let mut aspect = self.get_aspect(aspect_id).await?;
		let db = aspect.correlations().await?;
		let db_path = aspect.correlations_path();
		let cache = self.cache.clone();
		let aspect_id_owned = *aspect_id;
		
		// Page size for chunked loading
		const PAGE_SIZE: i64 = 100;
		
		// Use unfold to create a true streaming iterator that fetches in chunks
		let stream = futures::stream::unfold(
			(0i64, Vec::<CorrelationID>::new(), db, db_path, cache, aspect_id_owned),
			move |(offset, mut current_ids, db, db_path, cache, aspect_id)| {
				async move {
					// If we have IDs in the current batch, pop one and fetch it
					if let Some(correlation_id) = current_ids.pop() {
						// Fetch the full correlation for this ID
						match Self::fetch_correlation_by_id(&db, &db_path, &cache, &aspect_id, &correlation_id).await {
							Ok(correlation) => Some((Ok(correlation), (offset, current_ids, db, db_path, cache, aspect_id))),
							Err(e) => {
								tracing::warn!("Failed to fetch correlation {correlation_id}: {e}");
								// Continue with next ID
								Some((Err(e), (offset, current_ids, db, db_path, cache, aspect_id)))
							}
						}
					} else {
						// Need to fetch next page of IDs
						let conn = match Self::begin_concurrent(&db, &db_path, Some(cache.clone())).await {
							Ok(c) => c,
							Err(e) => return Some((Err(e), (offset, current_ids, db, db_path, cache, aspect_id))),
						};
						
						let query_sql = "SELECT id FROM correlations LIMIT ? OFFSET ?";
						let rows_result = conn.as_ref().query(query_sql, turso::params![PAGE_SIZE, offset]).await;
						
						let mut rows = match rows_result {
							Ok(r) => r,
							Err(e) => {
								let _ = Self::rollback_concurrent(&conn).await;
								return Some((Err(Error::DatabaseError(format!("Failed to query correlations: {e}")).into()), (offset, current_ids, db, db_path, cache, aspect_id)));
							}
						};
						
						// Collect IDs from this page
						let mut new_ids = Vec::new();
						loop {
							match rows.next().await {
								Ok(Some(row)) => {
									if let Ok(id_val) = row.get_value(0) {
										if let Some(id_str) = id_val.as_text() {
											if let Ok(uuid) = Uuid::parse_str(id_str) {
												new_ids.push(CorrelationID::from_uuid(uuid));
											}
										}
									}
								}
								Ok(None) => break,
								Err(e) => {
											tracing::warn!("Error reading correlation ID row: {e}");
											break;
								}
							}
						}
						
						let _ = Self::commit_concurrent(&conn).await;
						
						if new_ids.is_empty() {
							// No more data, end the stream
							return None;
						}
						
						// Update offset for next page
						let new_offset = offset + i64::try_from(new_ids.len()).unwrap_or(PAGE_SIZE);
						
						// Reverse so we can pop from the end efficiently
						new_ids.reverse();
						
						// Pop first ID and fetch it
						if let Some(correlation_id) = new_ids.pop() {
							match Self::fetch_correlation_by_id(&db, &db_path, &cache, &aspect_id, &correlation_id).await {
								Ok(correlation) => Some((Ok(correlation), (new_offset, new_ids, db, db_path, cache, aspect_id))),
								Err(e) => {
									tracing::warn!("Failed to fetch correlation {correlation_id}: {e}");
									Some((Err(e), (new_offset, new_ids, db, db_path, cache, aspect_id)))
								}
							}
						} else {
							None
						}
					}
				}
			},
		);
		
		Ok(Box::pin(stream))
	}

	//
	// Unprocessed Events
	//

	async fn get_unprocessed_event(&self, aspect_id: &AspectId, event_id: &EventID) -> Result<Event> {
		// Check cache first using aspect_id and event_id as cache key
		let cache_key = format!("unprocessed_event_{}_{}", aspect_id.as_uuid(), event_id.to_uuid());

		// Try to get from cache first
		if let Some(cached_event) = self.cache.lock().await.get::<Event>(&cache_key).await {
			return Ok(cached_event);
		}

		// Get from database
		let db = self.get_unprocessed_events_db(aspect_id).await?;
		let db_path = self.get_unprocessed_events_db_path(aspect_id).await?;
		let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let query_sql = r"
                        SELECT id, name, description
                        FROM events 
                        WHERE id = ?
                ";

		let mut rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![event_id.to_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query unprocessed event: {e}")))?;
		if let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get unprocessed event row: {e}")))? {
			let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Event ID is not text".to_string()))?.clone();
			let name_str = row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Event name is not text".to_string()))?.clone();
			let description_str = row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Event description is not text".to_string()))?.clone();
			let id = EventID::from_uuid(Uuid::parse_str(&id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for event ID: {e}")))?);

			// get manifestations
			let manifestations_query_sql = r"
                                SELECT id, event_id, dataset_id, start_timestamp, end_timestamp
                                FROM event_manifestations
                                WHERE event_id = ?
                        ";
			let mut manif_rows: turso::Rows = conn.as_ref().query(manifestations_query_sql, turso::params![event_id.to_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query event manifestations: {e}")))?;
			let mut manifestations: HashMap<ManifestationId, Manifestation> = HashMap::new();

			while let Some(manif_row) = manif_rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get event manifestation row: {e}")))? {
				let manif_id_str = manif_row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Manifestation ID is not text".to_string()))?.clone();
				let manif_event_id_str = manif_row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Manifestation Event ID is not text".to_string()))?.clone();
				let manif_dataset_id_str = manif_row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Manifestation Dataset ID is not text".to_string()))?.clone();
				let manif_start_timestamp_millis: i64 = *manif_row.get_value(3)?.as_integer().ok_or_else(|| Error::DatabaseError("Manifestation Start Timestamp is not integer".to_string()))?;
				let manif_end_timestamp_millis: i64 = *manif_row.get_value(4)?.as_integer().ok_or_else(|| Error::DatabaseError("Manifestation End Timestamp is not integer".to_string()))?;

				let manif_id = ManifestationId::from_uuid(Uuid::parse_str(&manif_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for manifestation ID: {e}")))?);
				let _manif_event_id = EventID::from_uuid(Uuid::parse_str(&manif_event_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for manifestation Event ID: {e}")))?);
				let manif_dataset_id = DatasetId::from_uuid(Uuid::parse_str(&manif_dataset_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for manifestation Dataset ID: {e}")))?);
				let manif_start_timestamp = DateTime::from_timestamp_millis(manif_start_timestamp_millis).ok_or_else(|| Error::DatabaseError("Invalid manifestation start timestamp".to_string()))?;
				let manif_end_timestamp = DateTime::from_timestamp_millis(manif_end_timestamp_millis).ok_or_else(|| Error::DatabaseError("Invalid manifestation end timestamp".to_string()))?;

				let manifestation = Manifestation::with_id(manif_id.clone(), manif_dataset_id.as_uuid(), manif_start_timestamp, manif_end_timestamp);
				manifestations.insert(manif_id, manifestation);
			}

			let event = Event::new(Some(id), name_str, Some(description_str), Some(manifestations));

			// Cache the event
			self.cache.lock().await.store(&cache_key, event.clone()).await;

			let _ = Self::commit_concurrent(&conn).await;

			Ok(event)
		} else {
			let _ = Self::commit_concurrent(&conn).await;
			Err(anyhow::anyhow!("Unprocessed Event with ID {event_id} not found"))
		}
	}

	async fn get_unprocessed_events(&self, aspect_id: &AspectId) -> Result<Pin<Box<dyn Stream<Item = Result<Event>> + Send + 'static>>> {
		// Check cache first
		let cache_key = format!("unprocessed_events_{}", aspect_id.as_uuid());
		if let Some(cached_events) = self.cache.lock().await.get::<Vec<Event>>(&cache_key).await {
			// Convert cached events to stream
			let event_stream = futures::stream::iter(cached_events.into_iter().map(Ok));
			return Ok(Box::pin(event_stream));
		}

		// Get from database
		let db = self.get_unprocessed_events_db(aspect_id).await?;
		let db_path = self.get_unprocessed_events_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		let query_sql = r"
                        SELECT id
                        FROM events 
                ";
		let mut rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to query unprocessed events: {e}")))?;
		// Collect all events first to avoid async issues in the stream
		let mut events = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get unprocessed event row: {e}")))? {
			let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Unprocessed Event ID is not text".to_string()))?.clone();
			let event_id = EventID::from_uuid(Uuid::parse_str(&id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for unprocessed event ID: {e}")))?);
			if let Ok(event) = self.get_unprocessed_event(aspect_id, &event_id).await {
				events.push(event);
			} else {
				// Log error but continue processing other events
				tracing::warn!("Failed to parse unprocessed event with ID {event_id}");
			}
		}
		// Cache the events
		self.cache.lock().await.store(&cache_key, events.clone()).await;

		// Convert events to stream
		let event_stream = futures::stream::iter(events.into_iter().map(Ok));
		Ok(Box::pin(event_stream))
	}

        //
        // Processed Events
        //

        async fn get_processed_event(&self, aspect_id: &AspectId, event_id: &EventID) -> Result<Event> {
                // Check cache first using aspect_id and event_id as cache key
                let cache_key = format!("processed_event_{}_{}", aspect_id.as_uuid(), event_id.to_uuid());

                // Try to get from cache first
                if let Some(cached_event) = self.cache.lock().await.get::<Event>(&cache_key).await {
                        return Ok(cached_event);
                }

                // Get from database
                let db = self.get_processed_events_db(aspect_id).await?;
                let db_path = self.get_processed_events_db_path(aspect_id).await?;
                let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

                let query_sql = r"
                                SELECT id, name, description
                                FROM processed_events 
                                WHERE id = ?
                        ";

                let mut rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![event_id.to_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query processed event: {e}")))?;
                if let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get processed event row: {e}")))? {
                        let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Event ID is not text".to_string()))?.clone();
                        let name_str = row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Event name is not text".to_string()))?.clone();
                        let description_str = row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Event description is not text".to_string()))?.clone();
                        let id = EventID::from_uuid(Uuid::parse_str(&id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for event ID: {e}")))?);

                        let event = Event::new(Some(id), name_str, Some(description_str), None);

                        // Cache the event
                        self.cache.lock().await.store(&cache_key, event.clone()).await;

                        let _ = Self::commit_concurrent(&conn).await;

                        Ok(event)
                } else {
                        let _ = Self::commit_concurrent(&conn).await;
                        Err(anyhow::anyhow!("Processed Event with ID {event_id} not found"))
                }
        }

        async fn get_processed_events(&self, aspect_id: &AspectId) -> Result<Pin<Box<dyn Stream<Item = Result<Event>> + Send + 'static>>> {
                // Check cache first
                let cache_key = format!("processed_events_{}", aspect_id.as_uuid());
                if let Some(cached_events) = self.cache.lock().await.get::<Vec<Event>>(&cache_key).await {
                        // Convert cached events to stream
                        let event_stream = futures::stream::iter(cached_events.into_iter().map(Ok));
                        return Ok(Box::pin(event_stream));
                }

                // Get from database
                let db = self.get_processed_events_db(aspect_id).await?;
                let db_path = self.get_processed_events_db_path(aspect_id).await?;
                let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
                let query_sql = r"
                                SELECT id
                                FROM processed_events 
                        ";
                let mut rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to query processed events: {e}")))?;
                // Collect all events first to avoid async issues in the stream
                let mut events = Vec::new();
                while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get processed event row: {e}")))? {
                        let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Processed Event ID is not text".to_string()))?.clone();
                        let event_id = EventID::from_uuid(Uuid::parse_str(&id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for processed event ID: {e}")))?);
                        if let Ok(event) = self.get_processed_event(aspect_id, &event_id).await {
                                events.push(event);
                        } else {
                                // Log error but continue processing other events
                                tracing::warn!("Failed to parse processed event with ID {event_id}");
                        }
                }
                // Cache the events
                self.cache.lock().await.store(&cache_key, events.clone()).await;

                // Convert events to stream
                let event_stream = futures::stream::iter(events.into_iter().map(Ok));
                Ok(Box::pin(event_stream))
        }
}

/// Helper function to parse a batch row from the database with aspect context
/// 
/// Expected columns: `id(0)`, `aspect_id(1)`, `database_id(2)`, `size(3)`, `resolution(4)`, `measurements(5)`, `batch_hash(6)`
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn parse_batch_row_helper(row: &turso::Row, db_name: &str, db_path: &str) -> Result<Batch> {
	let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Batch ID is not text".to_string()))?.clone();
	let batch_id = BatchId::from_uuid(Uuid::parse_str(&id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for batch ID: {e}")))?);

	let aspect_id_str = row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Aspect ID is not text".to_string()))?.clone();
	let aspect_id = AspectId::from_uuid(Uuid::parse_str(&aspect_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for aspect ID: {e}")))?);

	let size = *row.get_value(3)?.as_integer().ok_or_else(|| Error::DatabaseError("Size is not an integer".to_string()))? as usize;
	
	let resolution_str = row.get_value(4)?.as_text().ok_or_else(|| Error::DatabaseError("Resolution is not text".to_string()))?.clone();
	let resolution = Resolution::from_str(&resolution_str).map_err(|e| Error::DatabaseError(format!("Invalid resolution format: {e}")))?;

	let measurements_json_str = row.get_value(5)?.as_text().ok_or_else(|| Error::DatabaseError("Measurements is not text".to_string()))?.clone();
	let measurements: Vec<BatchedMeasurement> = serde_json::from_str(&measurements_json_str).map_err(|e| Error::DatabaseError(format!("Failed to parse batch measurements JSON: {e}")))?;

	let batch_hash_str = row.get_value(6)?.as_text().cloned();

	// Reconstruct DatabaseInfo with minimal required fields
	let database_info = DatabaseInfo::new(db_name.to_string(), db_path.to_string());
	let metadata = BatchMetatdata {
		aspect: aspect_id,
		resolution,
		size,
		database_info,
	};

	Ok(Batch { metadata, measurements, batch_id, batch_hash: batch_hash_str })
}

/// Helper methods for streaming operations that don't require &self
impl Database {
	/// Fetch a correlation by ID without requiring &self (for use in streaming closures)
	async fn fetch_correlation_by_id(
		db: &turso::Database,
		db_path: &str,
		cache: &std::sync::Arc<tokio::sync::Mutex<cache::DatabaseCache>>,
		aspect_id: &AspectId,
		correlation_id: &CorrelationID,
	) -> Result<Correlation> {
		// Check cache first
		let cache_key = format!("correlation_{}_{}", aspect_id.as_uuid(), correlation_id.to_uuid());
		if let Some(cached_correlation) = cache.lock().await.get::<Correlation>(&cache_key).await {
			return Ok(cached_correlation);
		}

		let conn = Self::begin_concurrent(db, db_path, Some(cache.clone())).await?;
		
		let query_sql = r"
			SELECT id, dictionary_id, subject_id, aspect_id, pattern_id, event_id, average_distance_value, average_distance_units
			FROM correlations 
			WHERE id = ?
		";

		let mut rows = conn.as_ref().query(query_sql, turso::params![correlation_id.to_uuid().to_string()]).await
			.map_err(|e| Error::DatabaseError(format!("Failed to query correlation: {e}")))?;
		
		let row = rows.next().await
			.map_err(|e| Error::DatabaseError(format!("Failed to get correlation row: {e}")))?
			.ok_or_else(|| anyhow::anyhow!("Correlation with ID {correlation_id} not found"))?;

		let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Correlation ID is not text".to_string()))?.clone();
		let dictionary_id_str = row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Dictionary ID is not text".to_string()))?.clone();
		let subject_id_str = row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Subject ID is not text".to_string()))?.clone();
		let aspect_id_str = row.get_value(3)?.as_text().ok_or_else(|| Error::DatabaseError("Aspect ID is not text".to_string()))?.clone();
		let pattern_id_str = row.get_value(4)?.as_text().ok_or_else(|| Error::DatabaseError("Pattern ID is not text".to_string()))?.clone();
		let event_id_str = row.get_value(5)?.as_text().ok_or_else(|| Error::DatabaseError("Event ID is not text".to_string()))?.clone();
		
		// Parse average_distance (nullable columns)
		let average_distance = {
			let avg_dist_value_opt = row.get_value(6).ok().and_then(|v| v.as_text().cloned());
			let avg_dist_units_opt = row.get_value(7).ok().and_then(|v| v.as_text().cloned());
			
			match (avg_dist_value_opt, avg_dist_units_opt) {
				(Some(value_str), Some(units_str)) => {
					let value = BigDecimal::from_str(&value_str).ok();
					let units = Resolution::from_str(&units_str).ok();
					match (value, units) {
						(Some(v), Some(u)) => Some(crate::types::signal::Distance::new(v, u)),
						_ => None,
					}
				}
				_ => None,
			}
		};

		let id = CorrelationID::from_uuid(Uuid::parse_str(&id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for correlation ID: {e}")))?);
		let dictionary_id = DictionaryId::from_uuid(Uuid::parse_str(&dictionary_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for dictionary ID: {e}")))?);
		let subject_id = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for subject ID: {e}")))?);
		let aspect_id_parsed = AspectId::from_uuid(Uuid::parse_str(&aspect_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for aspect ID: {e}")))?);
		let pattern_id = PatternID::from_uuid(Uuid::parse_str(&pattern_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for pattern ID: {e}")))?);
		let event_id = EventID::from_uuid(Uuid::parse_str(&event_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for event ID: {e}")))?);

		// Get error rates
		let error_rate_query_sql = r"
			SELECT signal_type, error_rate_value, error_rate_units
			FROM correlation_error_rates
			WHERE correlation_id = ?
		";
		let mut error_rate_rows = conn.as_ref().query(error_rate_query_sql, turso::params![correlation_id.to_uuid().to_string()]).await
			.map_err(|e| Error::DatabaseError(format!("Failed to query correlation error rates: {e}")))?;
		
		let mut error_rate: HashMap<SignalType, ErrorRate> = HashMap::new();
		while let Some(error_rate_row) = error_rate_rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get correlation error rate row: {e}")))? {
			let signal_type_str = error_rate_row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Error rate signal type is not text".to_string()))?.clone();
			let error_rate_value: BigDecimal = {
				let value_str = error_rate_row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Error rate value is not text".to_string()))?.clone();
				BigDecimal::from_str(&value_str).map_err(|e| Error::DatabaseError(format!("Invalid error rate value format: {e}")))?
			};
			let error_rate_units_str = error_rate_row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Error rate units is not text".to_string()))?.clone();

			let units: Resolution = Resolution::from_str(&error_rate_units_str).map_err(|e| Error::DatabaseError(format!("Invalid error rate units format: {e}")))?;
			let err_rate = ErrorRate::new(error_rate_value, units);
			let signal_type = SignalType::from_str(&signal_type_str).map_err(|e| Error::DatabaseError(format!("Invalid error type format: {e}")))?;
			error_rate.insert(signal_type, err_rate);
		}

		// Get occurrences
		let occurrences_query_sql = r"
			SELECT correlation_id, occurrence_index, aspect_id, resolution, size, database_info, pattern_id, beginning_timestamp, end_timestamp
			FROM correlation_occurrences
			WHERE correlation_id = ?
		";
		let mut occ_rows = conn.as_ref().query(occurrences_query_sql, turso::params![correlation_id.to_uuid().to_string()]).await
			.map_err(|e| Error::DatabaseError(format!("Failed to query correlation occurrences: {e}")))?;
		
		let mut occ_map: HashMap<usize, Occurrence> = HashMap::new();
		while let Some(occ_row) = occ_rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get correlation occurrence row: {e}")))? {
			let occ_index: usize = usize::try_from(*occ_row.get_value(1)?.as_integer().ok_or_else(|| Error::DatabaseError("Occurrence Index is not integer".to_string()))?).map_err(|e| Error::DatabaseError(format!("Occurrence index out of range: {e}")))?;
			let occ_aspect_id_str = occ_row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Occurrence Aspect ID is not text".to_string()))?.clone();
			let occ_resolution_str = occ_row.get_value(3)?.as_text().ok_or_else(|| Error::DatabaseError("Occurrence Resolution is not text".to_string()))?.clone();
			let occ_size: usize = usize::try_from(*occ_row.get_value(4)?.as_integer().ok_or_else(|| Error::DatabaseError("Occurrence Size is not integer".to_string()))?).map_err(|e| Error::DatabaseError(format!("Occurrence size out of range: {e}")))?;
			let occ_database_info_str = occ_row.get_value(5)?.as_text().ok_or_else(|| Error::DatabaseError("Occurrence Database Info is not text".to_string()))?.clone();
			let occ_pattern_id_str = occ_row.get_value(6)?.as_text().ok_or_else(|| Error::DatabaseError("Occurrence Pattern ID is not text".to_string()))?.clone();
			let occ_beginning_timestamp_millis: i64 = *occ_row.get_value(7)?.as_integer().ok_or_else(|| Error::DatabaseError("Occurrence Beginning Timestamp is not integer".to_string()))?;
			let occ_end_timestamp_millis: i64 = *occ_row.get_value(8)?.as_integer().ok_or_else(|| Error::DatabaseError("Occurrence End Timestamp is not integer".to_string()))?;
			
			let occ_aspect_id = AspectId::from_str(&occ_aspect_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for occurrence Aspect ID: {e}")))?;
			let occ_resolution = Resolution::from_str(&occ_resolution_str).map_err(|e| Error::DatabaseError(format!("Invalid resolution format: {e}")))?;
			let occ_database_info: DatabaseInfo = serde_json::from_str(&occ_database_info_str).map_err(|e| Error::DatabaseError(format!("Failed to parse occurrence database info JSON: {e}")))?;
			let occ_pattern_id = PatternID::from_uuid(Uuid::parse_str(&occ_pattern_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for occurrence Pattern ID: {e}")))?);
			let occ_beginning_timestamp = DateTime::from_timestamp_millis(occ_beginning_timestamp_millis).ok_or_else(|| Error::DatabaseError("Invalid beginning timestamp".to_string()))?;
			let occ_end_timestamp = DateTime::from_timestamp_millis(occ_end_timestamp_millis).ok_or_else(|| Error::DatabaseError("Invalid end timestamp".to_string()))?;
			
			let occurrence = Occurrence::new(occ_aspect_id, occ_resolution, occ_size, occ_database_info, occ_pattern_id, occ_beginning_timestamp, occ_end_timestamp);
			occ_map.insert(occ_index, occurrence);
		}
		
		// Sort occurrences by index
		let mut occurrences: Vec<Occurrence> = Vec::new();
		let mut occ_indices: Vec<usize> = occ_map.keys().copied().collect();
		occ_indices.sort_unstable();
		for index in occ_indices {
			if let Some(occurrence) = occ_map.get(&index) {
				occurrences.push(occurrence.clone());
			}
		}

		let correlation = Correlation::new(Some(id), dictionary_id, subject_id, &aspect_id_parsed, pattern_id, event_id, error_rate, occurrences, average_distance);

		// Cache the correlation
		cache.lock().await.store(&cache_key, correlation.clone()).await;
		let _ = Self::commit_concurrent(&conn).await;
		
		Ok(correlation)
	}

	/// Parse a measurement row without requiring &self (for use in streaming closures)
	async fn parse_measurement_row_static(row: turso::Row) -> Result<Measurement> {
		let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("ID is not text".to_string()))?.clone();
		let dataset_id_str = row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Dataset ID is not text".to_string()))?.clone();
		let timestamp_millis: i64 = *row.get_value(2)?.as_integer().ok_or_else(|| Error::DatabaseError("Timestamp is not integer".to_string()))?;
		let value_str = row.get_value(3)?.as_text().ok_or_else(|| Error::DatabaseError("Value is not text".to_string()))?.clone();

		let id = MeasurementId::from_string(&id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for measurement ID: {e}")))?;
		let dataset_id = DatasetId::from_str(&dataset_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for dataset ID: {e}")))?;
		let timestamp = DateTime::from_timestamp_millis(timestamp_millis).ok_or_else(|| Error::DatabaseError("Invalid timestamp".to_string()))?;
		let value = BigDecimal::from_str(&value_str).map_err(|e| Error::DatabaseError(format!("Invalid value format: {e}")))?;

		Ok(Measurement::new(id, dataset_id, timestamp, value))
	}
}
