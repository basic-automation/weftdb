use std::{collections::HashMap, pin::Pin, str::FromStr};

use anyhow::{bail, Result};
use async_trait::async_trait;
use bigdecimal::{BigDecimal, Zero};
use chrono::{DateTime, Utc};
use futures::{Stream, StreamExt};
use splimes::{Interpolator, Point, Resolution, Spline};
use uuid::Uuid;

use crate::{
	cache::{self}, correlation::ErrorRate, types::{
		database::traits::{aspect_structure::AspectStructure, config::Config, connection::Connection as _ConnectionTrait, database_structure::DatabaseStructure}, error::is_transient_mvcc_error
	}, AnalysisResult, AspectId, Batch, BatchId, BatchMetatdata, BatchedMeasurement, Correlation, CorrelationID, Database, DatabaseInfo, DatasetId, DictionaryConstraints, DictionaryId, DictionaryMetadata, Error, Event, EventID, Manifestation, ManifestationId, Measurement, MeasurementId, Occurrence, Pattern, PatternID, Relative, SignalType, Steps, SubjectId, Variability, VariablilityType
};

/// Maximum retries for transient MVCC errors during concurrent compression
const MAX_MVCC_RETRIES: u32 = 3;

/// Base delay between retries (exponential backoff)
const MVCC_RETRY_BASE_DELAY_MS: u64 = 50;

/// Parsed correlation row data from database query.
/// Contains (`correlation_id`, `dictionary_id`, `subject_id`, `aspect_id`, `pattern_id`, `event_id`, `average_distance`).
type CorrelationRowData = (CorrelationID, DictionaryId, SubjectId, AspectId, PatternID, EventID, Option<crate::types::signal::Distance>);

/// Maximum number of pages to fetch when performing point analysis.
/// This limit prevents infinite loops when searching for data around a target time
/// and ensures bounded memory usage during interpolation data collection.
const MAX_PAGES_FOR_POINT_ANALYSIS: usize = 10;

// Avoid triggering pedantic 'too_many_lines' on this large impl
#[allow(clippy::too_many_lines)]
#[async_trait]
impl crate::types::database::traits::outputs::Outputs for Database {
	//
	// Unbatched Measurements Queue
	//

	/// Get all unbatched measurement timestamps for an aspect
	async fn get_unbatched_measurements(&self, aspect_id: &AspectId) -> Result<Vec<DateTime<Utc>>> {
		let db = self.metadata();
		let db_path = self.metadata_path();
		let conn = Self::begin_concurrent(db, db_path, Some(self.cache.clone())).await?;

		let sql = r"
			SELECT data_timestamp FROM unbatched_measurements
			WHERE aspect_id = ?
			ORDER BY queued_at ASC
		";

		let mut rows = conn.as_ref().query(sql, turso::params![aspect_id.as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query unbatched measurements: {e}")))?;

		let mut timestamps = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get unbatched row: {e}")))? {
			let ts_millis: i64 = row.get(0).map_err(|e| Error::DatabaseError(format!("Failed to get timestamp: {e}")))?;
			let ts = DateTime::from_timestamp_millis(ts_millis).ok_or_else(|| Error::DatabaseError("Invalid timestamp".to_string()))?;
			timestamps.push(ts);
		}

		Self::commit_concurrent(&conn).await?;
		Ok(timestamps)
	}

	/// Count unbatched measurements for an aspect
	async fn count_unbatched_measurements(&self, aspect_id: &AspectId) -> Result<u64> {
		let db = self.metadata();
		let db_path = self.metadata_path();
		let conn = Self::begin_concurrent(db, db_path, Some(self.cache.clone())).await?;

		let sql = "SELECT COUNT(*) FROM unbatched_measurements WHERE aspect_id = ?";
		let mut rows = conn.as_ref().query(sql, turso::params![aspect_id.as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to count unbatched measurements: {e}")))?;

		let count: u64 = if let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get count row: {e}")))? {
			let c: i64 = row.get(0).map_err(|e| Error::DatabaseError(format!("Failed to get count: {e}")))?;
			u64::try_from(c).unwrap_or(0)
		} else {
			0
		};

		Self::commit_concurrent(&conn).await?;
		Ok(count)
	}

	/// Count unprocessed batches for an aspect
	async fn count_unprocessed_batches(&self, aspect_id: &AspectId) -> Result<u64> {
		let db = self.get_unprocessed_batches_db(aspect_id).await?;
		let db_path = self.get_unprocessed_batches_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let sql = "SELECT COUNT(*) FROM batches";
		let mut rows = conn.as_ref().query(sql, turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to count unprocessed batches: {e}")))?;

		let count: u64 = if let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get count row: {e}")))? {
			let c: i64 = row.get(0).map_err(|e| Error::DatabaseError(format!("Failed to get count: {e}")))?;
			u64::try_from(c).unwrap_or(0)
		} else {
			0
		};

		Self::commit_concurrent(&conn).await?;
		Ok(count)
	}

	/// Count processed batches for an aspect
	async fn count_processed_batches(&self, aspect_id: &AspectId) -> Result<u64> {
		let db = self.get_processed_batches_db(aspect_id).await?;
		let db_path = self.get_processed_batches_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let sql = "SELECT COUNT(*) FROM batches";
		let mut rows = conn.as_ref().query(sql, turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to count processed batches: {e}")))?;

		let count: u64 = if let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get count row: {e}")))? {
			let c: i64 = row.get(0).map_err(|e| Error::DatabaseError(format!("Failed to get count: {e}")))?;
			u64::try_from(c).unwrap_or(0)
		} else {
			0
		};

		Self::commit_concurrent(&conn).await?;
		Ok(count)
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
			let measurement: Measurement = Self::parse_measurement_row_static(&row)?;
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
			let measurement: Measurement = Self::parse_measurement_row_static(&row)?;
			latest_measurements.push(measurement);
		}

		// Add latest measurements, avoiding duplicates
		for latest in latest_measurements.into_iter().rev() {
			// Reverse to maintain chronological order
			if !all_measurements.iter().any(|m: &Measurement| m.id() == latest.id()) {
				all_measurements.push(latest);
			}
		}

		Self::commit_concurrent(&conn).await?;

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

		Self::commit_concurrent(&conn).await?;

		if let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get count row: {e}")))? {
			let count_value = row.get_value(0)?;
			let count = if let Some(int_val) = count_value.as_integer() {
				*int_val as usize
			} else if let Some(text_val) = count_value.as_text() {
				text_val.parse().map_err(|e| Error::DatabaseError(format!("Invalid count format: {e}")))?
			} else {
				return Err(Error::DatabaseError("Count value is neither integer nor text".to_string()).into());
			};
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
		/// Maximum number of times to double the search window when looking for measurements
		const MAX_WINDOW_EXPANSIONS: i32 = 10;

		// Check cache first for point analysis
		let cache_key = format!("point_{}_{}_{}_{:?}_{:?}", aspect_id.as_uuid(), time.timestamp(), time.timestamp_subsec_nanos(), resolution, method);
		if let Some(cached_result) = self.cache.lock().await.get::<AnalysisResult>(&cache_key).await {
			return Ok(Point { timestamp: cached_result.timestamp(), value: cached_result.value().clone() });
		}

		// Determine minimum points needed for this interpolation method (its degree + 1)
		let min_points_needed = method.min_points();

		// Get measurements efficiently using pagination with time range optimization
		// For point analysis, fetch data around the target time for better efficiency
		// Start with a reasonable window and expand if needed
		let base_window = resolution.step() * 100;
		let initial_page_size = 10_000;
		let mut all_measurements = Vec::new();
		let mut found_target_range = false;
		let mut window_multiplier = 1i32;

		// Try progressively larger windows until we have enough data
		'window_loop: loop {
			let window = base_window * window_multiplier;
			let range_start = time - window;
			let range_end = time + window;
			let mut page = 0;

			// Fetch measurements until we have enough data around our target time
			loop {
				let measurements: Vec<Measurement> = self.get_raw_measurements(aspect_id, Some(range_start), Some(range_end), initial_page_size, page).await?.collect::<Vec<_>>().await.into_iter().collect::<Result<Vec<_>>>()?;

				if measurements.is_empty() {
					// No data in the time window, try getting boundary measurements
					if page == 0 && all_measurements.is_empty() {
						let boundary_measurements: Vec<Measurement> = self.get_boundary_measurements(aspect_id).await?.collect::<Vec<_>>().await.into_iter().collect::<Result<Vec<_>>>()?;
						all_measurements.extend(boundary_measurements);
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
				if found_target_range && all_measurements.len() >= min_points_needed {
					break 'window_loop;
				}

				// If we haven't found the target range yet, or need more points, continue
				page += 1;

				// Safety check to prevent infinite loops
				if page > MAX_PAGES_FOR_POINT_ANALYSIS {
					break;
				}
			}

			// If we have enough points, exit the window expansion loop
			if all_measurements.len() >= min_points_needed {
				break;
			}

			// Check if we've exhausted our window expansion attempts
			// Only clear and retry if we haven't reached the max AND we have no data yet
			// If we have some data but not enough for the method, keep it for fallback
			if window_multiplier >= MAX_WINDOW_EXPANSIONS {
				break;
			}

			// Expand the window and try again - only clear if we have no data
			// (otherwise we might lose valid boundary measurements)
			window_multiplier *= 2;
			if all_measurements.is_empty() {
				// No data found yet, keep trying with larger window
				found_target_range = false;
			} else {
				// We have some data but not enough - keep what we have and try for more
				// Don't clear, just try a larger window
			}
		}

		if all_measurements.is_empty() {
			bail!("No measurements found for aspect");
		}

		// Sort measurements by timestamp to ensure proper ordering - fix type annotation
		all_measurements.sort_by_key(|m: &Measurement| m.timestamp());

		// Convert to Points for splimes
		let points: Vec<Point> = all_measurements.iter().map(|m| Point { timestamp: m.timestamp(), value: m.value().clone() }).collect();

		// splimes handles both interpolation and extrapolation. We need a single point, and a
		// grid with `start == end` is exactly that one instant. splimes is synchronous and
		// CPU-bound, so it runs on tokio's blocking pool rather than this async task.
		let interpolated = Interpolator::new(*method, *resolution).run_async(points, time, time).await?;

		// Get the interpolated point (the grid's only point)
		let point = interpolated.into_points().into_iter().next().unwrap_or_else(|| Point { timestamp: time, value: BigDecimal::zero() });

		// Cache the result
		let method_description = if all_measurements.len() == 1 {
			format!("{method:?} (constant extrapolation from single point)")
		} else if all_measurements.len() < 2 {
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
	/// Uses true chunked streaming: fetches, interpolates, and yields per chunk for responsive UI.
	///
	/// # Errors
	/// - if interpolation fails
	async fn analyze_range(&self, aspect_id: &AspectId, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, method: Spline) -> Result<Pin<Box<dyn Stream<Item = Result<Point>> + Send + 'static>>> {
		tracing::info!(start = %start, end = %end, "[analyze_range] Starting chunked streaming");

		// Calculate chunk parameters
		let total_duration = end - start;
		let step_duration = resolution.step();
		let total_ns = total_duration.num_nanoseconds().unwrap_or(i64::MAX);
		let step_ns = step_duration.num_nanoseconds().unwrap_or(1);
		let expected_points = if step_ns > 0 { (total_ns / step_ns) + 1 } else { 1 };

		// Calculate overlap needed for interpolation method (its degree)
		let overlap_points = method.degree();
		let overlap_duration = step_duration * i32::try_from(overlap_points).unwrap_or(3);

		// Target ~50,000 output points per chunk for responsive streaming
		let chunk_size = 50_000i64;
		let chunk_count = ((expected_points + chunk_size - 1) / chunk_size).max(1);
		let chunk_duration_ns = total_ns / chunk_count;
		let chunk_duration = chrono::Duration::nanoseconds(chunk_duration_ns);

		tracing::info!(expected_points = expected_points, chunk_count = chunk_count, chunk_duration_ms = chunk_duration.num_milliseconds(), "[analyze_range] Chunked streaming setup");

		// Clone self for use in async stream
		let db_name = self.name().to_string();
		let aspect_id = *aspect_id;

		// Create streaming iterator that fetches and interpolates per chunk
		let stream = futures::stream::unfold((start, None::<std::vec::IntoIter<Point>>, false), move |(current_chunk_start, mut current_iter, done)| {
			let db_name = db_name.clone();
			let aspect_id = aspect_id;
			let end_time = end;
			let resolution_val = resolution;
			let method_val = method;
			let overlap = overlap_duration;
			let chunk_dur = chunk_duration;
			let range_start = start;

			async move {
				// Return next point from current chunk if available
				if let Some(ref mut iter) = current_iter {
					if let Some(point) = iter.next() {
						return Some((Ok(point), (current_chunk_start, current_iter, done)));
					}
				}

				// If we're done, return None
				if done || current_chunk_start >= end_time {
					return None;
				}

				// Calculate this chunk's boundaries
				let chunk_end = (current_chunk_start + chunk_dur).min(end_time);
				let fetch_start = (current_chunk_start - overlap).max(range_start);
				let fetch_end = (chunk_end + overlap).min(end_time);

				tracing::debug!(
					chunk_start = %current_chunk_start,
					chunk_end = %chunk_end,
					fetch_start = %fetch_start,
					fetch_end = %fetch_end,
					"[analyze_range] Processing chunk"
				);

				// Fetch measurements for this chunk (with overlap)
				let db = match Self::existing(&db_name).await {
					Ok(db) => db,
					Err(e) => return Some((Err(e), (chunk_end, None, true))),
				};

				let measurements = match db.fetch_measurements_for_chunk(&aspect_id, fetch_start, fetch_end).await {
					Ok(m) => m,
					Err(e) => return Some((Err(e), (chunk_end, None, true))),
				};

				if measurements.is_empty() {
					// No data for this chunk, move to next
					if chunk_end >= end_time {
						return None;
					}
					return Some((Ok(Point { timestamp: current_chunk_start, value: BigDecimal::from(0) }), (chunk_end, None, false)));
				}

				// Convert to points
				let points: Vec<Point> = measurements.iter().map(|m| Point { timestamp: m.timestamp(), value: m.value().clone() }).collect();

				// Adjust effective end to not extrapolate beyond actual data
				let actual_data_end = points.iter().map(|p| p.timestamp).max().unwrap_or(chunk_end);
				let effective_chunk_end = chunk_end.min(actual_data_end);

				// That clamp can COLLAPSE the range. The chunk fetch is inclusive at both
				// ends, so a chunk whose only visible measurement sits exactly at (or before)
				// `current_chunk_start` clamps the end back onto (or behind) the start — and
				// splimes rejects `start > end` with `Error::InvalidTimeRange`, failing the
				// whole stream. The `measurements.is_empty()` guard above does not catch this,
				// because the chunk is not empty; its data is simply all at or behind the start.
				//
				// A zero-width window has exactly one sensible answer — the last known value
				// at the start instant — so emit that and advance, mirroring the empty-chunk
				// arm above rather than aborting.
				if effective_chunk_end <= current_chunk_start {
					// `points` is ordered by timestamp ASC (the fetch query sorts), so the
					// last element is the newest value at or before the start.
					let value = points.last().map_or_else(|| BigDecimal::from(0), |p| p.value.clone());
					let is_last_chunk = chunk_end >= end_time;
					return Some((Ok(Point { timestamp: current_chunk_start, value }), (chunk_end, None, is_last_chunk)));
				}

				// Interpolate this chunk, on tokio's blocking pool: splimes is synchronous and
				// CPU-bound, and a chunk can be tens of thousands of grid points.
				match Interpolator::new(method_val, resolution_val).run_async(points, current_chunk_start, effective_chunk_end).await {
					Ok(interpolated) => {
						let is_last_chunk = chunk_end >= end_time;
						let mut new_iter = interpolated.into_points().into_iter();

						// Return first point and set up iterator for the rest
						match new_iter.next() {
							Some(first_point) => Some((Ok(first_point), (chunk_end, Some(new_iter), is_last_chunk))),
							None => {
								if is_last_chunk {
									None
								} else {
									Some((Ok(Point { timestamp: current_chunk_start, value: BigDecimal::from(0) }), (chunk_end, None, false)))
								}
							}
						}
					}
					Err(e) => Some((Err(e.into()), (chunk_end, None, true))),
				}
			}
		});

		Ok(Box::pin(stream))
	}

	/// Helper function to fetch all measurements for a time range
	/// Loads all measurements within the specified time range without pagination
	/// Includes retry logic for transient MVCC errors during concurrent compression
	async fn fetch_measurements_for_range(&self, aspect_id: &AspectId, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Pin<Box<dyn Stream<Item = Result<Measurement>> + Send + 'static>>> {
		// Retry loop for transient MVCC errors
		for attempt in 0..MAX_MVCC_RETRIES {
			match Self::fetch_measurements_for_range_inner_impl(self, aspect_id, start, end).await {
				Ok(measurements) => {
					let stream = futures::stream::iter(measurements.into_iter().map(Ok));
					return Ok(Box::pin(stream));
				}
				Err(e) if is_transient_mvcc_error(&e) && attempt < MAX_MVCC_RETRIES - 1 => {
					let delay = MVCC_RETRY_BASE_DELAY_MS * 2u64.pow(attempt);
					tracing::debug!(attempt = attempt + 1, delay_ms = delay, "Transient MVCC error during fetch_measurements_for_range, retrying...");
					tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
					continue;
				}
				Err(e) => return Err(e),
			}
		}
		// Should not reach here, but just in case
		Self::fetch_measurements_for_range_inner_impl(self, aspect_id, start, end).await.map(|m| {
			let stream = futures::stream::iter(m.into_iter().map(Ok));
			Box::pin(stream) as Pin<Box<dyn Stream<Item = Result<Measurement>> + Send + 'static>>
		})
	}

	/// Fetch measurements for a specific time chunk with overlap for interpolation
	/// Includes retry logic for transient MVCC errors during concurrent compression
	async fn fetch_measurements_for_chunk(&self, aspect_id: &AspectId, chunk_start: DateTime<Utc>, chunk_end: DateTime<Utc>) -> Result<Vec<Measurement>> {
		// Retry loop for transient MVCC errors
		for attempt in 0..MAX_MVCC_RETRIES {
			match Self::fetch_measurements_for_chunk_inner_impl(self, aspect_id, chunk_start, chunk_end).await {
				Ok(measurements) => return Ok(measurements),
				Err(e) if is_transient_mvcc_error(&e) && attempt < MAX_MVCC_RETRIES - 1 => {
					let delay = MVCC_RETRY_BASE_DELAY_MS * 2u64.pow(attempt);
					tracing::debug!(attempt = attempt + 1, delay_ms = delay, "Transient MVCC error during fetch_measurements_for_chunk, retrying...");
					tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
					continue;
				}
				Err(e) => return Err(e),
			}
		}
		// Should not reach here, but just in case
		Self::fetch_measurements_for_chunk_inner_impl(self, aspect_id, chunk_start, chunk_end).await
	}

	async fn get_unprocessed_batch(&self, aspect_id: &AspectId, batch_id: &BatchId) -> Result<Batch> {
		// Check cache first using aspect_id and batch_id as cache key
		let cache_key = format!("unprocessed_batch_{}_{}", aspect_id.as_uuid(), batch_id.as_uuid());

		// Try to get from cache first
		if let Some(cached_batches) = self.cache.lock().await.get::<Vec<Batch>>(&cache_key).await {
			if let Some(batch) = cached_batches.iter().find(|b| *b.batch_id() == *batch_id) {
				return Ok(batch.clone());
			}
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

		Self::commit_concurrent(&conn).await?;

		if let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get batch row: {e}")))? {
			let batch = Self::parse_batch_row_helper(&row, self.name(), &db_path)?;

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
			if let Ok(batch) = Self::parse_batch_row_helper(&row, db_name, &db_path_clone) {
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

		Self::commit_concurrent(&conn).await?;
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
			let batch = Self::parse_batch_row_helper(&row, self.name(), &db_path)?;

			// Cache the single batch (as a vec with one element)
			self.cache.lock().await.store(&cache_key, vec![batch.clone()]).await;

			Self::commit_concurrent(&conn).await?;

			Ok(batch)
		} else {
			Self::commit_concurrent(&conn).await?;
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
			if let Ok(batch) = Self::parse_batch_row_helper(&row, db_name, &db_path_clone) {
				batches.push(batch);
			} else {
				// Log error but continue processing other batches
				tracing::warn!("Failed to parse batch row");
			}
		}

		Self::commit_concurrent(&conn).await?;

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
		// Check cache first. Keyed by aspect too: dictionary names are per aspect.
		let cache_key = Self::dictionary_metadata_cache_key(aspect_id, dictionary_name);
		if let Some(metadata) = self.cache.lock().await.get::<DictionaryMetadata>(&cache_key).await {
			return Ok(Some(metadata));
		}

		// Each dictionary is its own database file. Without one nothing is registered under
		// this name, so the answer is `None` (on which `load_dictionary` registers the
		// dictionary), not the error `get_dictionary_db` gives for a missing file.
		let aspect = self.get_aspect(aspect_id).await?;
		let db_path = Self::aspect_dictionaries_db_path(&self.name, aspect.subject_name(), aspect.name(), dictionary_name);
		if !tokio::fs::try_exists(&db_path).await? {
			return Ok(None);
		}

		// Get from database
		let db = self.get_dictionary_db(aspect_id, dictionary_name).await?;
		let conn: cache::Connection = Self::begin_concurrent(&db, dictionary_name, Some(self.cache.clone())).await?;
		let metadata = match Self::read_dictionary_registration(&conn, dictionary_name).await {
			Ok(metadata) => metadata,
			Err(e) => {
				// Report why the read failed, not a failed rollback.
				if let Err(rollback) = Self::rollback_concurrent(&conn).await {
					tracing::warn!(error = %rollback, dictionary = dictionary_name, "Failed to roll back a dictionary metadata read");
				}
				return Err(e);
			}
		};
		Self::commit_concurrent(&conn).await?;

		if let Some(metadata) = &metadata {
			self.cache.lock().await.store(&cache_key, metadata.clone()).await;
		}
		Ok(metadata)
	}

	async fn list_dictionaries(&self, aspect_id: &AspectId) -> Result<Vec<DictionaryMetadata>> {
		// Each dictionary is its own `<aspect>/dictionaries/<name>.db`, so the aspect's
		// dictionaries are the `.db` files there. (This read one dictionary's database, the
		// one named "default", and failed for an aspect without it.) Not cached: a
		// dictionary created through `Aspect::new_dictionary` could not invalidate it, and
		// each dictionary's metadata is cached by `get_dictionary_metadata`.
		let aspect = self.get_aspect(aspect_id).await?;
		let dictionaries_path = Self::aspect_dictionaries_path(&self.name, aspect.subject_name(), aspect.name());
		let mut entries = match tokio::fs::read_dir(&dictionaries_path).await {
			Ok(entries) => entries,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
			Err(e) => return Err(Error::DatabaseError(format!("Failed to read dictionaries directory '{dictionaries_path}': {e}")).into()),
		};
		let mut names = Vec::new();
		while let Some(entry) = entries.next_entry().await.map_err(|e| Error::DatabaseError(format!("Failed to read dictionaries directory '{dictionaries_path}': {e}")))? {
			// `<name>.db` only, not Turso's `<name>.db-wal` and `<name>.db-log` beside it.
			let path = entry.path();
			if path.extension().is_some_and(|ext| ext == "db") && tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_file()) {
				if let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) {
					names.push(name.to_string());
				}
			}
		}
		names.sort_unstable();

		// A file with no complete registration (see `read_dictionary_registration`) is not
		// listed; a registration that cannot be read is an error, as it is for
		// `get_dictionary_metadata`.
		let mut dictionaries = Vec::with_capacity(names.len());
		for name in names {
			if let Some(metadata) = self.get_dictionary_metadata(aspect_id, &name).await? {
				dictionaries.push(metadata);
			}
		}
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
		let conn: cache::Connection = Self::begin_concurrent(&db, dictionary_name, Some(self.cache.clone())).await?;
		let query_sql = r"
                        SELECT id, sum_value, abs_sum_value, max_value, min_value, abs_max_value, avg_value, abs_avg_value
                        FROM patterns 
                        WHERE id = ?
                ";

		let mut rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![pattern_id.as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query pattern: {e}")))?;
		Self::commit_concurrent(&conn).await?;

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
		let conn: cache::Connection = Self::begin_concurrent(&db, dictionary_name, Some(self.cache.clone())).await?;
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
		Self::commit_concurrent(&conn).await?;
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
			// Parse the row using shared helper
			let (id, dictionary_id, subject_id, aspect_id_parsed, pattern_id, event_id, average_distance) = Self::parse_correlation_row(&row)?;

			// Load error rates and occurrences using helpers
			let error_rate: HashMap<SignalType, ErrorRate> = Self::load_correlation_error_rates(&conn, &id).await?;
			let occurrences: Vec<Occurrence> = Self::load_correlation_occurrences(&conn, &id).await?;

			let correlation = Correlation::new(Some(id), dictionary_id, subject_id, &aspect_id_parsed, pattern_id, event_id, error_rate, occurrences, average_distance);

			// Cache the correlation
			self.cache.lock().await.store(&cache_key, correlation.clone()).await;
			Self::commit_concurrent(&conn).await?;
			Ok(correlation)
		} else {
			Self::commit_concurrent(&conn).await?;
			Err(anyhow::anyhow!("Correlation with ID {correlation_id} not found"))
		}
	}

	async fn get_correlations(&self, aspect_id: &AspectId) -> Result<Pin<Box<dyn Stream<Item = Result<Correlation>> + Send + 'static>>> {
		// Page size for chunked loading
		const PAGE_SIZE: i64 = 100;

		// Get database connection info upfront for the streaming closure
		let mut aspect = self.get_aspect(aspect_id).await?;
		let db = aspect.correlations().await?;
		let db_path = aspect.correlations_path();
		let cache = self.cache.clone();
		let aspect_id_owned = *aspect_id;

		// Use unfold to create a true streaming iterator that fetches in chunks
		let initial_state: (i64, Vec<CorrelationID>, turso::Database, String, std::sync::Arc<tokio::sync::Mutex<cache::DatabaseCache>>, AspectId) = (0i64, Vec::<CorrelationID>::new(), db, db_path, cache, aspect_id_owned);
		let stream = futures::stream::unfold(initial_state, move |(offset, mut current_ids, db, db_path, cache, aspect_id)| {
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
					let conn: cache::Connection = match Self::begin_concurrent(&db, &db_path, Some(cache.clone())).await {
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

					drop(rows);
					if let Err(e) = Self::commit_concurrent(&conn).await {
						return Some((Err(e), (offset, current_ids, db, db_path, cache, aspect_id)));
					}

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
		});

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

			Self::commit_concurrent(&conn).await?;

			Ok(event)
		} else {
			Self::commit_concurrent(&conn).await?;
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
		let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
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

			Self::commit_concurrent(&conn).await?;

			Ok(event)
		} else {
			Self::commit_concurrent(&conn).await?;
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
		let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
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

	async fn get_raw_measurements(&self, aspect_id: &AspectId, start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>, max_per_page: usize, page: usize) -> Result<Pin<Box<dyn Stream<Item = Result<Measurement>> + Send + 'static>>> {
		let db = self.get_measurement_db(aspect_id).await?;
		let db_path = self.get_measurement_db_path(aspect_id).await?;
		let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// Safely convert page and max_per_page to i64 to avoid potential wrapping on 64-bit targets
		let page_i64 = i64::try_from(page).map_err(|_| Error::DatabaseError("Page value too large".to_string()))?;
		let per_page_i64 = i64::try_from(max_per_page).map_err(|_| Error::DatabaseError("max_per_page value too large".to_string()))?;
		let offset = page_i64.checked_mul(per_page_i64).ok_or_else(|| Error::DatabaseError("Offset multiplication overflow".to_string()))?;

		let mut rows: turso::Rows = if let (Some(s), Some(e)) = (start, end) {
			let query_sql = "SELECT id, dataset_id, timestamp, value FROM measurements WHERE timestamp >= ? AND timestamp <= ? ORDER BY timestamp ASC LIMIT ? OFFSET ?";
			conn.as_ref().query(query_sql, turso::params![s.timestamp_millis(), e.timestamp_millis(), per_page_i64, offset]).await.map_err(|e| Error::DatabaseError(format!("Failed to query measurements: {e}")))?
		} else if let Some(s) = start {
			let query_sql = "SELECT id, dataset_id, timestamp, value FROM measurements WHERE timestamp >= ? ORDER BY timestamp ASC LIMIT ? OFFSET ?";
			conn.as_ref().query(query_sql, turso::params![s.timestamp_millis(), per_page_i64, offset]).await.map_err(|e| Error::DatabaseError(format!("Failed to query measurements: {e}")))?
		} else if let Some(e) = end {
			let query_sql = "SELECT id, dataset_id, timestamp, value FROM measurements WHERE timestamp <= ? ORDER BY timestamp ASC LIMIT ? OFFSET ?";
			conn.as_ref().query(query_sql, turso::params![e.timestamp_millis(), per_page_i64, offset]).await.map_err(|e| Error::DatabaseError(format!("Failed to query measurements: {e}")))?
		} else {
			let query_sql = "SELECT id, dataset_id, timestamp, value FROM measurements ORDER BY timestamp ASC LIMIT ? OFFSET ?";
			conn.as_ref().query(query_sql, turso::params![per_page_i64, offset]).await.map_err(|e| Error::DatabaseError(format!("Failed to query measurements: {e}")))?
		};

		let mut measurements: Vec<Measurement> = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get measurement row: {e}")))? {
			let m = Self::parse_measurement_row_static(&row)?;
			measurements.push(m);
		}

		Self::commit_concurrent(&conn).await?;

		let stream = futures::stream::iter(measurements.into_iter().map(Ok));
		Ok(Box::pin(stream))
	}
}

/// Helper methods for retry-with-MVCC operations
impl Database {
	/// Inner implementation of `fetch_measurements_for_range` without retry
	async fn fetch_measurements_for_range_inner_impl(&self, aspect_id: &AspectId, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Vec<Measurement>> {
		let db = self.get_measurement_db(aspect_id).await?;
		let db_path = self.get_measurement_db_path(aspect_id).await?;
		let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// Query all measurements in range without LIMIT - we need all data for accurate interpolation
		let query_sql = "SELECT id, dataset_id, timestamp, value FROM measurements WHERE timestamp >= ? AND timestamp <= ? ORDER BY timestamp ASC";
		let mut rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![start.timestamp_millis(), end.timestamp_millis()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query measurements: {e}")))?;

		let mut measurements: Vec<Measurement> = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get measurement row: {e}")))? {
			let m = Self::parse_measurement_row_static(&row)?;
			measurements.push(m);
		}

		Self::commit_concurrent(&conn).await?;

		Ok(measurements)
	}

	/// Inner implementation of `fetch_measurements_for_chunk` without retry
	async fn fetch_measurements_for_chunk_inner_impl(&self, aspect_id: &AspectId, chunk_start: DateTime<Utc>, chunk_end: DateTime<Utc>) -> Result<Vec<Measurement>> {
		let db = self.get_measurement_db(aspect_id).await?;
		let db_path = self.get_measurement_db_path(aspect_id).await?;
		let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let query_sql = "SELECT id, dataset_id, timestamp, value FROM measurements WHERE timestamp >= ? AND timestamp <= ? ORDER BY timestamp ASC";
		let mut rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![chunk_start.timestamp_millis(), chunk_end.timestamp_millis()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query measurements: {e}")))?;

		let mut measurements: Vec<Measurement> = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get measurement row: {e}")))? {
			let m = Self::parse_measurement_row_static(&row)?;
			measurements.push(m);
		}

		Self::commit_concurrent(&conn).await?;
		Ok(measurements)
	}
}

/// Helper methods for streaming operations that don't require &self
impl Database {
	/// Fetch a correlation by ID without requiring &self (for use in streaming closures)
	async fn fetch_correlation_by_id(db: &turso::Database, db_path: &str, cache: &std::sync::Arc<tokio::sync::Mutex<cache::DatabaseCache>>, aspect_id: &AspectId, correlation_id: &CorrelationID) -> Result<Correlation> {
		// Check cache first
		let cache_key = format!("correlation_{}_{}", aspect_id.as_uuid(), correlation_id.to_uuid());
		if let Some(cached_correlation) = cache.lock().await.get::<Correlation>(&cache_key).await {
			return Ok(cached_correlation);
		}

		let conn: cache::Connection = Self::begin_concurrent(db, db_path, Some(cache.clone())).await?;

		let query_sql = r"
			SELECT id, dictionary_id, subject_id, aspect_id, pattern_id, event_id, average_distance_value, average_distance_units
			FROM correlations 
			WHERE id = ?
		";

		let mut rows = conn.as_ref().query(query_sql, turso::params![correlation_id.to_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query correlation: {e}")))?;

		let row = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get correlation row: {e}")))?.ok_or_else(|| anyhow::anyhow!("Correlation with ID {correlation_id} not found"))?;

		// Parse the row using shared helper
		let (id, dictionary_id, subject_id, aspect_id_parsed, pattern_id, event_id, average_distance) = Self::parse_correlation_row(&row)?;

		// Load error rates and occurrences using helpers to keep this function concise
		let error_rate: HashMap<SignalType, ErrorRate> = Self::load_correlation_error_rates(&conn, &id).await?;
		let occurrences: Vec<Occurrence> = Self::load_correlation_occurrences(&conn, &id).await?;

		let correlation = Correlation::new(Some(id), dictionary_id, subject_id, &aspect_id_parsed, pattern_id, event_id, error_rate, occurrences, average_distance);

		// Cache the correlation
		cache.lock().await.store(&cache_key, correlation.clone()).await;
		Self::commit_concurrent(&conn).await?;

		Ok(correlation)
	}

	/// Parse a correlation row from the database
	/// Returns tuple of (id, `dictionary_id`, `subject_id`, `aspect_id`, `pattern_id`, `event_id`, `average_distance`)
	fn parse_correlation_row(row: &turso::Row) -> Result<CorrelationRowData> {
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

		Ok((id, dictionary_id, subject_id, aspect_id, pattern_id, event_id, average_distance))
	}

	async fn load_correlation_error_rates(conn: &cache::Connection, correlation_id: &CorrelationID) -> Result<HashMap<SignalType, ErrorRate>> {
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

			error_rate.insert(signal_type, err_rate);
		}
		Ok(error_rate)
	}

	async fn load_correlation_occurrences(conn: &cache::Connection, correlation_id: &CorrelationID) -> Result<Vec<Occurrence>> {
		let occurrences_query_sql = r"
	                                SELECT correlation_id, occurrence_index, aspect_id, resolution, size, database_info, pattern_id, beginning_timestamp, end_timestamp
	                                FROM correlation_occurrences
	                                WHERE correlation_id = ?
	                        ";
		let mut occ_rows: turso::Rows = conn.as_ref().query(occurrences_query_sql, turso::params![correlation_id.to_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query correlation occurrences: {e}")))?;
		let mut occurrences: Vec<Occurrence> = Vec::new();
		// load occurrences in order of occurrence_index
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
		Ok(occurrences)
	}

	/// Parse a measurement row without requiring &self (for use in streaming closures)
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
		let metadata = BatchMetatdata { aspect: aspect_id, resolution, size, database_info };

		Ok(Batch { metadata, measurements, batch_id, batch_hash: batch_hash_str })
	}

	fn parse_measurement_row_static(row: &turso::Row) -> Result<Measurement> {
		// During concurrent compression (DELETE + INSERT), MVCC reads can observe partial states
		// where type casting fails. Return TransientMvccError for these cases to allow retries.
		let id_val = row.get_value(0)?;
		let id_str = match id_val.as_text() {
			Some(s) => s.clone(),
			None => {
				return Err(Error::TransientMvccError("ID field type mismatch - concurrent compression in progress".to_string()).into());
			}
		};

		let dataset_id_val = row.get_value(1)?;
		let dataset_id_str = match dataset_id_val.as_text() {
			Some(s) => s.clone(),
			None => {
				return Err(Error::TransientMvccError("Dataset ID field type mismatch - concurrent compression in progress".to_string()).into());
			}
		};

		let timestamp_val = row.get_value(2)?;
		let timestamp_millis: i64 = match timestamp_val.as_integer() {
			Some(i) => *i,
			None => {
				return Err(Error::TransientMvccError("Timestamp field type mismatch - concurrent compression in progress".to_string()).into());
			}
		};

		let value_val = row.get_value(3)?;
		let value_str = match value_val.as_text() {
			Some(s) => s.clone(),
			None => {
				return Err(Error::TransientMvccError("Value field type mismatch - concurrent compression in progress".to_string()).into());
			}
		};

		let id = MeasurementId::from_string(&id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for measurement ID: {e}")))?;
		let dataset_id = DatasetId::from_str(&dataset_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for dataset ID: {e}")))?;
		let timestamp = DateTime::from_timestamp_millis(timestamp_millis).ok_or_else(|| Error::DatabaseError("Invalid timestamp".to_string()))?;
		let value = BigDecimal::from_str(&value_str).map_err(|e| Error::DatabaseError(format!("Invalid value format: {e}")))?;

		Ok(Measurement::new(id, dataset_id, timestamp, value))
	}

	/// The key `get_dictionary_metadata` caches a dictionary's metadata under, which
	/// `set_dictionary_metadata` invalidates. Dictionary names are per aspect, so it has both.
	pub(crate) fn dictionary_metadata_cache_key(aspect_id: &AspectId, dictionary_name: &str) -> String {
		format!("dictionary_metadata_{}_{dictionary_name}", aspect_id.as_uuid())
	}

	/// Read `dictionary_name`'s registration on `conn`, a transaction on its database.
	///
	/// A registration is a `dictionary_metadata` row with a `dictionary_constraints` row;
	/// its `dictionary_variabilities` rows, in the order they were written, are its
	/// variabilities (none is `None`). The metadata table has no unique constraint, and
	/// databases written before `set_dictionary_metadata` replaced registrations can hold
	/// several rows for a name, some without constraints (it wrote none): the newest
	/// complete one is read, and a name with none is `None`.
	///
	/// # Errors
	///
	/// A failed query, or a stored value that does not parse: see
	/// [`parse_stored_steps`](Self::parse_stored_steps) and
	/// [`parse_stored_variability`](Self::parse_stored_variability).
	async fn read_dictionary_registration(conn: &cache::Connection, dictionary_name: &str) -> Result<Option<DictionaryMetadata>> {
		let query_sql = r"
                        SELECT m.id, m.name, m.description, c.steps_count, c.steps_interpolation
                        FROM dictionary_metadata m
                        JOIN dictionary_constraints c ON c.dictionary_id = m.id
                        WHERE m.name = ?
                        ORDER BY m.created_at DESC, m.id DESC
                        LIMIT 1
                ";
		let mut rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![dictionary_name]).await.map_err(|e| Error::DatabaseError(format!("Failed to query dictionary metadata: {e}")))?;
		let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get metadata row: {e}")))? else {
			return Ok(None);
		};
		let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Dictionary ID is not text".to_string()))?.clone();
		let name = row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Dictionary name is not text".to_string()))?.clone();
		let description = row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Dictionary description is not text".to_string()))?.clone();
		let steps = Self::parse_stored_steps(&row.get_value(3)?, &row.get_value(4)?)?;
		drop(rows);
		let id = DictionaryId::from_uuid(Uuid::parse_str(&id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for dictionary ID: {e}")))?);

		let variability_query_sql = r"
                        SELECT variability_type, variability_value
                        FROM dictionary_variabilities
                        WHERE dictionary_id = ?
                        ORDER BY id
                ";
		let mut rows: turso::Rows = conn.as_ref().query(variability_query_sql, turso::params![id_str]).await.map_err(|e| Error::DatabaseError(format!("Failed to query dictionary variabilities: {e}")))?;
		let mut variabilities = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get variability row: {e}")))? {
			variabilities.push(Self::parse_stored_variability(&row.get_value(0)?, &row.get_value(1)?)?);
		}
		let variabilities = (!variabilities.is_empty()).then_some(variabilities);

		Ok(Some(DictionaryMetadata { id, name, description, constraints: DictionaryConstraints::new(steps, variabilities) }))
	}

	/// Parse one of a dictionary's stored variabilities: the `variability_type` and
	/// `variability_value` columns of a `dictionary_variabilities` row, which hold the
	/// variant's name ([`VariablilityType::kind`]) and its value as decimal text.
	///
	/// # Errors
	///
	/// A `DatabaseError` for a column that is not text, a value that is not a decimal, or a
	/// name that is no variant, e.g. `Invalid variability: Unknown VariabilityType: Median`.
	fn parse_stored_variability(variability_type: &turso::Value, variability_value: &turso::Value) -> Result<VariablilityType> {
		let kind = variability_type.as_text().ok_or_else(|| Error::DatabaseError("Variability type is not text".to_string()))?;
		let value = variability_value.as_text().ok_or_else(|| Error::DatabaseError("Variability value is not text".to_string()))?;
		let value = BigDecimal::from_str(value).map_err(|e| Error::DatabaseError(format!("Invalid variability value '{value}': {e}")))?;
		Ok(VariablilityType::from_kind(kind, Variability::new(value)).map_err(|e| Error::DatabaseError(format!("Invalid variability: {e}")))?)
	}

	/// Parse a dictionary's stored step configuration: the `steps_count` and
	/// `steps_interpolation` columns of its `dictionary_constraints` row.
	///
	/// `steps_count` is an `INTEGER` column, so the count `new_dictionary` binds as text is
	/// stored as an integer; text is accepted too. A `NULL` count (or an empty or `"null"`
	/// text one) means the dictionary has no steps, and its interpolation is not read.
	/// The interpolation is the [`Spline`]'s text, which splimes validates as it parses.
	///
	/// # Errors
	///
	/// A `DatabaseError` for a count that is not a non-negative integer, or an
	/// interpolation that is not text or not a valid [`Spline`], e.g.
	/// `Invalid interpolation format: invalid polynomial degree 9: must be between 1 and 8`.
	fn parse_stored_steps(steps_count: &turso::Value, steps_interpolation: &turso::Value) -> Result<Option<Steps>> {
		let count: usize = match steps_count {
			turso::Value::Null => return Ok(None),
			turso::Value::Text(text) if text.is_empty() || text == "null" => return Ok(None),
			turso::Value::Integer(count) => usize::try_from(*count).map_err(|e| Error::DatabaseError(format!("Invalid steps count {count}: {e}")))?,
			turso::Value::Text(text) => text.parse().map_err(|e| Error::DatabaseError(format!("Invalid steps count format: {e}")))?,
			turso::Value::Real(_) | turso::Value::Blob(_) => bail!(Error::DatabaseError("Steps count is neither an integer nor text".to_string())),
		};
		let interpolation = steps_interpolation.as_text().ok_or_else(|| Error::DatabaseError("Steps interpolation is not text".to_string()))?;
		let interpolation: Spline = interpolation.parse().map_err(|e| Error::DatabaseError(format!("Invalid interpolation format: {e}")))?;
		Ok(Some(Steps::new(count, interpolation)))
	}
}

#[cfg(test)]
mod tests {
	use turso::Value;

	use super::*;

	fn text(s: &str) -> Value {
		Value::Text(s.to_string())
	}

	#[test]
	fn stored_steps_read_an_integer_or_text_count() {
		// The column's INTEGER affinity stores the count `new_dictionary` binds as text as
		// an integer; the old reader required text, so no stored dictionary ever loaded.
		let steps = Database::parse_stored_steps(&Value::Integer(10), &text("Cubic")).expect("integer count").expect("steps");
		assert_eq!((steps.count(), *steps.interpolation()), (10, Spline::Cubic));
		let steps = Database::parse_stored_steps(&text("4"), &text("Polynomial(degree: 8, bounds_factor: None)")).expect("text count").expect("steps");
		assert_eq!((steps.count(), *steps.interpolation()), (4, Spline::Polynomial(8, None)));
	}

	#[test]
	fn stored_steps_without_a_count_are_none() {
		// `new_dictionary` writes NULL for both columns when the dictionary has no steps.
		for count in [Value::Null, text(""), text("null")] {
			assert!(Database::parse_stored_steps(&count, &Value::Null).expect("no steps").is_none());
		}
	}

	#[test]
	fn stored_variabilities_read_their_kind_and_value() {
		let variability = Database::parse_stored_variability(&text("AveragePercentile"), &text("0.000000001")).expect("a stored variability");
		assert_eq!(variability.kind(), "AveragePercentile");
		assert_eq!(variability.variability().value(), &BigDecimal::from_str("0.000000001").unwrap());
		let err = Database::parse_stored_variability(&text("Median"), &text("1")).expect_err("no such variant");
		assert_eq!(err.to_string(), "Database error: Invalid variability: Unknown VariabilityType: Median");
		assert!(Database::parse_stored_variability(&text("SumStatic"), &text("lots")).is_err());
		assert!(Database::parse_stored_variability(&Value::Null, &text("1")).is_err());
	}

	#[test]
	fn stored_steps_reject_what_splimes_rejects() {
		let err = Database::parse_stored_steps(&Value::Integer(10), &text("Polynomial(degree: 9, bounds_factor: None)")).expect_err("degree 9");
		assert_eq!(err.to_string(), "Database error: Invalid interpolation format: invalid polynomial degree 9: must be between 1 and 8");
		let err = Database::parse_stored_steps(&Value::Integer(10), &text("Polynomial(degree: 3, bounds_factor: -1)")).expect_err("negative bounds");
		assert_eq!(err.to_string(), "Database error: Invalid interpolation format: invalid bounds factor -1: must be finite and not negative");
		assert!(Database::parse_stored_steps(&Value::Integer(-1), &text("Cubic")).is_err());
		assert!(Database::parse_stored_steps(&Value::Integer(10), &Value::Null).is_err());
	}
}
