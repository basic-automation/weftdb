use std::{collections::HashMap, pin::Pin, str::FromStr, sync::Arc};

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
	}, AnalysisResult, AspectId, Batch, BatchId, BatchMetatdata, BatchedMeasurement, Correlation, CorrelationID, Database, DatabaseInfo, DatasetId, DictionaryConstraints, DictionaryId, DictionaryMetadata, Error, Event, EventID, Manifestation, ManifestationId, Measurement, MeasurementId, Occurrence, Pattern, PatternID, Relative, SignalType, Steps, SubjectId, UnbatchedEntry, Variability, VariablilityType
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
#[async_trait]
impl crate::types::database::traits::outputs::Outputs for Database {
	//
	// Unbatched Measurements Queue
	//

	/// Get all unbatched measurement timestamps for an aspect
	async fn get_unbatched_measurements(&self, aspect_id: &AspectId) -> Result<Vec<DateTime<Utc>>> {
		crate::types::database::inputs::mark_queue_consumer(aspect_id).await;
		let db_path = self.metadata_path();
		let conn = Self::begin_concurrent(self.metadata(), db_path, Some(self.cache.clone())).await?;

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

	/// Get every unbatched queue entry for an aspect, with its `queued_at`
	async fn get_unbatched_entries(&self, aspect_id: &AspectId) -> Result<Vec<UnbatchedEntry>> {
		crate::types::database::inputs::mark_queue_consumer(aspect_id).await;
		let db_path = self.metadata_path();
		let conn = Self::begin_concurrent(self.metadata(), db_path, Some(self.cache.clone())).await?;

		let sql = r"
			SELECT data_timestamp, queued_at FROM unbatched_measurements
			WHERE aspect_id = ?
			ORDER BY queued_at ASC
		";

		let mut rows = conn.as_ref().query(sql, turso::params![aspect_id.as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query unbatched measurements: {e}")))?;

		let mut entries = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get unbatched row: {e}")))? {
			let ts_millis: i64 = row.get(0).map_err(|e| Error::DatabaseError(format!("Failed to get timestamp: {e}")))?;
			let queued_at: i64 = row.get(1).map_err(|e| Error::DatabaseError(format!("Failed to get queued_at: {e}")))?;
			let data_timestamp = DateTime::from_timestamp_millis(ts_millis).ok_or_else(|| Error::DatabaseError("Invalid timestamp".to_string()))?;
			entries.push(UnbatchedEntry { data_timestamp, queued_at });
		}

		Self::commit_concurrent(&conn).await?;
		Ok(entries)
	}

	/// Count unbatched measurements for an aspect
	async fn count_unbatched_measurements(&self, aspect_id: &AspectId) -> Result<u64> {
		crate::types::database::inputs::mark_queue_consumer(aspect_id).await;
		let db_path = self.metadata_path();
		let conn = Self::begin_concurrent(self.metadata(), db_path, Some(self.cache.clone())).await?;

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
		drop(db);

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
		drop(db);

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
		// Seal-backed aspects (slice D-S19): the boundary rows are the seal
		// store's earliest and latest rows.
		if self.is_seal_backed(aspect_id).await? {
			let store = self.seal_store().await?;
			let all = crate::legacy_seal::read_measurements(&store, aspect_id, None, None).await?;
			drop(store);
			let mut boundary: Vec<Measurement> = all.iter().take(2).cloned().collect();
			for latest in all.iter().rev().take(2).rev() {
				if !boundary.iter().any(|m| m.id() == latest.id()) {
					boundary.push(latest.clone());
				}
			}
			boundary.sort_by_key(|m: &Measurement| m.timestamp());
			return Ok(Box::pin(futures::stream::iter(boundary.into_iter().map(Ok))));
		}
		let db = self.get_measurement_db(aspect_id).await?;
		let db_path = self.get_measurement_db_path(aspect_id).await?;

		let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		drop(db);

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

	/// Get the total count of measurements for an aspect (useful for pagination).
	///
	/// # Invariant (seal-backed)
	///
	/// On a seal-backed aspect this equals the number of rows a full-range
	/// [`get_raw_measurements`](crate::Outputs::get_raw_measurements) would return: the
	/// segment index is kept overlap-free at write time by
	/// [`ingest_batch_with_gate`](crate::legacy_seal::ingest_batch_with_gate),
	/// which calls [`reconcile_overlaps`](crate::SegmentStore::reconcile_overlaps) after every ingest so overlapping
	/// timestamp ranges are folded before the count is published.
	async fn get_measurements_count(&self, aspect_id: &AspectId) -> Result<usize> {
		// Seal-backed aspects (slice D-S19): the count is the seal store's
		// rollup total.
		if self.is_seal_backed(aspect_id).await? {
			let store = self.seal_store().await?;
			let stats = store.aspect_stats(&crate::legacy_seal::aspect_key(aspect_id)).await?;
			drop(store);
			return usize::try_from(stats.total_rows).map_err(|_| Error::DatabaseError("Seal-backed measurement count does not fit usize".to_string()).into());
		}
		let measurement_db = self.get_measurement_db(aspect_id).await?;
		let db_path = self.get_measurement_db_path(aspect_id).await?;

		let count_sql = "SELECT COUNT(*) FROM measurements";
		let conn: cache::Connection = Self::begin_concurrent(&measurement_db, &db_path, Some(self.cache.clone())).await?;
		drop(measurement_db);
		let mut rows: turso::Rows = conn.as_ref().query(count_sql, turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to count measurements: {e}")))?;

		Self::commit_concurrent(&conn).await?;

		if let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get count row: {e}")))? {
			let count_value = row.get_value(0)?;
			let count = if let Some(int_val) = count_value.as_integer() {
				crate::types::num::i64_to_usize(*int_val)
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
		// The knots a chunk needs past each edge, however far away they are (#506).
		let bracket_knots = overlap_points.max(1);

		// Target ~50,000 output points per chunk for responsive streaming
		let chunk_size = 50_000i64;
		let chunk_count = ((expected_points + chunk_size - 1) / chunk_size).max(1);
		// Each chunk spans a whole number of steps. The interpolation grid is anchored at the
		// chunk's start, so a chunk that ended between two steps started the next chunk's grid
		// off the range's: past the first chunk every point was resampled between the steps it
		// should have landed on, and an extreme sitting on a step was never output.
		let steps_per_chunk = ((expected_points - 1 + chunk_count - 1) / chunk_count).max(1);
		let chunk_duration = chrono::Duration::nanoseconds(step_ns.saturating_mul(steps_per_chunk));

		tracing::info!(expected_points = expected_points, chunk_count = chunk_count, chunk_duration_ms = chunk_duration.num_milliseconds(), "[analyze_range] Chunked streaming setup");

		// Clone self for use in async stream
		let db_handle = self.clone();
		let aspect_id = *aspect_id;

		// Create streaming iterator that fetches and interpolates per chunk
		let stream = futures::stream::unfold((start, None::<std::vec::IntoIter<Point>>, false), move |(current_chunk_start, mut current_iter, done)| {
			let db = db_handle.clone();
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
				let measurements = match db.fetch_measurements_for_chunk(&aspect_id, fetch_start, fetch_end).await {
					Ok(m) => m,
					Err(e) => return Some((Err(e), (chunk_end, None, true))),
				};
				let measurements = match Self::widen_to_brackets(&db, &aspect_id, measurements, BracketWindows { chunk: (current_chunk_start, chunk_end), fetch: (fetch_start, fetch_end), range: (range_start, end_time) }, bracket_knots, chunk_dur).await {
					Ok(m) => m,
					Err(e) => return Some((Err(e), (chunk_end, None, true))),
				};
				drop(db);

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
						new_iter.next().map_or_else(|| if is_last_chunk { None } else { Some((Ok(Point { timestamp: current_chunk_start, value: BigDecimal::from(0) }), (chunk_end, None, false))) }, |first_point| Some((Ok(first_point), (chunk_end, Some(new_iter), is_last_chunk))))
					}
					Err(e) => Some((Err(e.into()), (chunk_end, None, true))),
				}
			}
		});

		// The grid is inclusive at both ends, so a chunk's last point is the next chunk's first.
		// Emitting it twice made a value on a chunk boundary its own neighbour, which no strict
		// extreme survives.
		let mut last_emitted = None;
		let stream = stream.filter(move |item| {
			let keep = item.as_ref().map_or(true, |point| last_emitted.is_none_or(|last| point.timestamp > last));
			if let (true, Ok(point)) = (keep, item) {
				last_emitted = Some(point.timestamp);
			}
			futures::future::ready(keep)
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
			let value = cached_batches.iter().find(|b| *b.batch_id() == *batch_id);
			if let Some(batch) = value {
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

		let next_row = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get batch row: {e}")))?;
		if let Some(row) = next_row {
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
		// Read from the database every time, never from the cache: the queue changes on
		// every pipeline run and its writers do not invalidate a cached listing, so a cached
		// copy (kept for the cache's 10-minute wall-clock TTL) handed the same batches to
		// every run in that window and hid the batches queued meanwhile, which then all
		// arrived at once when it expired (the fanctl soak's re-extraction and backlog).
		// The listing is read once per run and consumed, so a cache saves nothing.
		let db = self.get_unprocessed_batches_db(aspect_id).await?;
		let db_path = self.get_unprocessed_batches_db_path(aspect_id).await?;
		let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		drop(db);

		// Query matches schema: id, aspect_id, database_id, size, resolution, measurements, batch_hash
		let query_sql = r"
			SELECT id, aspect_id, database_id, size, resolution, measurements, batch_hash
			FROM batches 
			WHERE aspect_id = ? AND database_id = ?
			ORDER BY created_at ASC
		";

		let mut rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query unprocessed batches: {e}")))?;

		// Collect all batches first to avoid async issues in the stream
		let batches = Self::collect_batch_rows(&mut rows, self.name(), &db_path).await?;

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
			let value = cached_batches.into_iter().find(|b| *b.batch_id() == *batch_id);
			if let Some(batch) = value {
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

		let next_row = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get batch row: {e}")))?;
		if let Some(row) = next_row {
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
		// Read from the database every time, never from the cache: the queue changes on
		// every pipeline run and its writers do not invalidate a cached listing, so a cached
		// copy (kept for the cache's 10-minute wall-clock TTL) handed the same batches to
		// every run in that window and hid the batches queued meanwhile, which then all
		// arrived at once when it expired (the fanctl soak's re-extraction and backlog).
		// The listing is read once per run and consumed, so a cache saves nothing.
		let db = self.get_processed_batches_db(aspect_id).await?;
		let db_path = self.get_processed_batches_db_path(aspect_id).await?;
		let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		drop(db);

		// Query matches schema: id, aspect_id, database_id, size, resolution, measurements, batch_hash
		let query_sql = r"
			SELECT id, aspect_id, database_id, size, resolution, measurements, batch_hash
			FROM batches 
			WHERE aspect_id = ? AND database_id = ?
			ORDER BY created_at ASC
		";

		let mut rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query processed batches: {e}")))?;

		// Collect all batches first to avoid async issues in the stream
		let batches = Self::collect_batch_rows(&mut rows, self.name(), &db_path).await?;

		Self::commit_concurrent(&conn).await?;

		// Convert to stream
		let batch_stream = futures::stream::iter(batches.into_iter().map(Ok));
		Ok(Box::pin(batch_stream))
	}

	//
	// dictionaries
	//

	async fn get_dictionary_metadata(&self, aspect_id: &AspectId, dictionary_name: &str) -> Result<Option<DictionaryMetadata>> {
		self.dictionary_registration(aspect_id, dictionary_name).await?.transpose()
	}

	async fn list_dictionaries(&self, aspect_id: &AspectId) -> Result<Vec<DictionaryMetadata>> {
		// Each dictionary is its own `<aspect>/dictionaries/<name>.db`, so the aspect's
		// dictionaries are the `.db` files there. (This read one dictionary's database, the
		// one named "default", and failed for an aspect without it.) Not cached: a
		// dictionary created through `Aspect::new_dictionary` could not invalidate it, and
		// each dictionary's metadata is cached by `get_dictionary_metadata`.
		let names = self.dictionary_file_names(aspect_id).await?;

		// A file with no complete registration (see `read_dictionary_registration`) is not
		// listed. Nor is one whose stored registration does not parse, such as a step method
		// splimes rejects: it is logged, and the readable dictionaries are still listed
		// (`get_dictionary_metadata` reports its error). A read that fails (a query, I/O, an
		// MVCC conflict) fails the listing, so that a shorter list never just means absent.
		let mut dictionaries = Vec::with_capacity(names.len());
		for name in names {
			match self.dictionary_registration(aspect_id, &name).await? {
				Some(Ok(metadata)) => dictionaries.push(metadata),
				Some(Err(e)) => tracing::warn!(error = %e, dictionary = %name, aspect = %aspect_id, "Skipping a dictionary whose stored registration does not parse"),
				None => {}
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
		let pattern = self.load_dictionary_pattern(aspect_id, dictionary_name, pattern_id).await?;
		self.cache.lock().await.store(&cache_key, pattern.clone()).await;
		Ok(pattern)
	}

	async fn get_dictionary_patterns(&self, aspect_id: &AspectId, dictionary_name: &str) -> Result<Pin<Box<dyn Stream<Item = Result<Pattern>> + Send + 'static>>> {
		// Each reader clones a pattern only as the stream yields it; the list itself is
		// the one shared copy the cache holds.
		Ok(Box::pin(shared_pattern_stream(self.dictionary_patterns_shared(aspect_id, dictionary_name).await?)))
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
		drop(aspect);
		let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		drop(db);
		let query_sql = r"
						SELECT id, dictionary_id, subject_id, aspect_id, pattern_id, event_id, average_distance_value, average_distance_units
						FROM correlations 
						WHERE id = ?
				";

		let mut rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![correlation_id.to_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query correlation: {e}")))?;
		let next_row = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get correlation row: {e}")))?;
		if let Some(row) = next_row {
			// Parse the row using shared helper
			let (id, dictionary_id, subject_id, aspect_id_parsed, pattern_id, event_id, average_distance) = Self::parse_correlation_row(&row)?;

			let error_rate: HashMap<SignalType, ErrorRate> = Self::load_correlation_error_rates(&conn, &id).await?;
			let correlation = Correlation::new(Some(id), crate::CorrelationKeys { dictionary_id, subject_id, aspect_id: aspect_id_parsed, pattern_id, event_id }, error_rate, average_distance);

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
		let mut aspect = self.get_aspect(aspect_id).await?;
		let db = aspect.correlations().await?;
		let db_path = aspect.correlations_path();
		drop(aspect);
		let cache = self.cache.clone();
		let aspect_id = *aspect_id;

		// One query per table, assembled in memory: paging ids with OFFSET and fetching each
		// correlation by id scanned the tables once per correlation (fanctl soak stall).
		let stream = futures::stream::once(async move { Self::load_all_correlations(&db, &db_path, &cache, &aspect_id).await.unwrap_or_else(|e| vec![Err(e)]) }).flat_map(futures::stream::iter);

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
		drop(db);

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
		let cache_key = Self::unprocessed_events_cache_key(aspect_id);
		// Taken before the read, so a write committed during it keeps this read uncached.
		let generation = self.cache.lock().await.generation(&cache_key).await;
		if let Some(cached_events) = self.cache.lock().await.get::<Vec<Event>>(&cache_key).await {
			// Convert cached events to stream
			let event_stream = futures::stream::iter(cached_events.into_iter().map(Ok));
			return Ok(Box::pin(event_stream));
		}

		// Get from database
		let db = self.get_unprocessed_events_db(aspect_id).await?;
		let db_path = self.get_unprocessed_events_db_path(aspect_id).await?;
		let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		drop(db);
		// DISTINCT: two writers that stored one new event concurrently can leave its row twice.
		let query_sql = r"
                        SELECT DISTINCT id
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
		// Cache the events unless a writer changed the table since `generation`.
		self.cache.lock().await.store_if_generation(&cache_key, generation, events.clone()).await;

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
		drop(db);

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
		drop(db);
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
		// Seal-backed aspects (slice D-S19): raw rows come from the seal
		// store, paged in memory exactly as the SQL LIMIT/OFFSET paged
		// them (the range is the caller's page window already).
		if self.is_seal_backed(aspect_id).await? {
			let store = self.seal_store().await?;
			let all = crate::legacy_seal::read_measurements(&store, aspect_id, start.map(|t| t.timestamp_millis()), end.map(|t| t.timestamp_millis())).await?;
			drop(store);
			let offset = page.saturating_mul(max_per_page);

			return Ok(Box::pin(futures::stream::iter(all.into_iter().skip(offset).take(max_per_page).map(Ok))));
		}
		let db = self.get_measurement_db(aspect_id).await?;
		let db_path = self.get_measurement_db_path(aspect_id).await?;
		let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		drop(db);

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
/// The time windows [`Database::widen_to_brackets`] works within: the chunk being
/// served, what has been fetched for it so far, and the whole requested range.
#[derive(Clone, Copy)]
struct BracketWindows {
	chunk: (DateTime<Utc>, DateTime<Utc>),
	fetch: (DateTime<Utc>, DateTime<Utc>),
	range: (DateTime<Utc>, DateTime<Utc>),
}

/// Each correlation's error rates, or why one did not parse, keyed by the stored
/// `correlation_id` text.
type ErrorRatesByCorrelation = HashMap<String, std::result::Result<HashMap<SignalType, ErrorRate>, String>>;

impl Database {
	/// The names of the aspect's dictionary files (`<aspect>/dictionaries/<name>.db`, regular
	/// files with a valid dictionary name), sorted, whether or not each is registered.
	pub(crate) async fn dictionary_file_names(&self, aspect_id: &AspectId) -> Result<Vec<String>> {
		let aspect = self.get_aspect(aspect_id).await?;
		let dictionaries_path = Self::aspect_dictionaries_path(&self.name, aspect.subject_name(), aspect.name());
		drop(aspect);
		let mut entries = match tokio::fs::read_dir(&dictionaries_path).await {
			Ok(entries) => entries,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
			Err(e) => return Err(Error::DatabaseError(format!("Failed to read dictionaries directory '{dictionaries_path}': {e}")).into()),
		};
		let mut names = Vec::new();
		while let Some(entry) = entries.next_entry().await.map_err(|e| Error::DatabaseError(format!("Failed to read dictionaries directory '{dictionaries_path}': {e}")))? {
			// `<name>.db` only, not Turso's `<name>.db-wal` and `<name>.db-log` beside it.
			let path = entry.path();
			if path.extension().is_none_or(|ext| ext != "db") {
				continue;
			}
			// A regular file only. Reading a dictionary opens its file, which sets its journal
			// mode and can create its tables, so a symlink (to anywhere), a directory or a
			// device named `*.db` is skipped: `symlink_metadata` does not follow the link.
			match tokio::fs::symlink_metadata(&path).await {
				Ok(metadata) if metadata.file_type().is_file() => {}
				Ok(_) => {
					tracing::warn!(path = %path.display(), "Skipping an entry in the dictionaries directory that is not a regular file");
					continue;
				}
				Err(e) => {
					tracing::warn!(error = %e, path = %path.display(), "Skipping an entry in the dictionaries directory that cannot be inspected");
					continue;
				}
			}
			if let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) {
				// No dictionary operation accepts a name that fails validation, so such a
				// file (`.backup.db`, `CON.db`) is not one of the aspect's dictionaries.
				match crate::dictionary_name::validate(name) {
					Ok(()) => names.push(name.to_string()),
					Err(e) => tracing::warn!(error = %e, path = %path.display(), "Skipping a file in the dictionaries directory whose name is not a dictionary name"),
				}
			}
		}
		names.sort_unstable();
		Ok(names)
	}
}

impl Database {
	/// Every batch row `rows` holds, parsed; a row that does not parse is logged and skipped.
	async fn collect_batch_rows(rows: &mut turso::Rows, db_name: &str, db_path: &str) -> Result<Vec<Batch>> {
		let mut batches = Vec::new();
		loop {
			let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get batch row: {e}")))? else {
				break;
			};
			if let Ok(batch) = Self::parse_batch_row_helper(&row, db_name, db_path) {
				batches.push(batch);
			} else {
				// Log error but continue processing other batches
				tracing::warn!("Failed to parse batch row");
			}
		}
		Ok(batches)
	}

	/// Reads one pattern (occurrences and relatives) from its dictionary file, without
	/// touching the cache.
	async fn load_dictionary_pattern(&self, aspect_id: &AspectId, dictionary_name: &str, pattern_id: &PatternID) -> Result<Pattern> {
		let db = self.get_dictionary_db(aspect_id, dictionary_name).await?;
		let conn: cache::Connection = Self::begin_concurrent(&db, dictionary_name, Some(self.cache.clone())).await?;
		let query_sql = r"
                        SELECT id, sum_value, abs_sum_value, max_value, min_value, abs_max_value, avg_value, abs_avg_value
                        FROM patterns 
                        WHERE id = ?
                ";

		let mut rows: turso::Rows = conn.as_ref().query(query_sql, turso::params![pattern_id.as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query pattern: {e}")))?;
		Self::commit_concurrent(&conn).await?;

		let next_row = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get pattern row: {e}")))?;
		if let Some(row) = next_row {
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
			loop {
				let Some(occ_row) = occ_rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get pattern occurrence row: {e}")))? else {
					break;
				};
				occurrences.push(Self::parse_pattern_occurrence_row(&occ_row)?);
			}

			// Then the compact lists (see `occurrence_codec`), in the order they were saved.
			// A dictionary file from before the codec has no such table; that is no error.
			if let Ok(mut blob_rows) = conn.as_ref().query(r"SELECT codec, payload FROM pattern_occurrence_blobs WHERE pattern_id = ? ORDER BY id ASC", turso::params![pattern_id.as_uuid().to_string()]).await {
				loop {
					let next = blob_rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get pattern occurrence list: {e}")))?;
					let Some(blob_row) = next else { break };
					let codec = *blob_row.get_value(0)?.as_integer().ok_or_else(|| Error::DatabaseError("Occurrence codec is not integer".to_string()))?;
					if codec != crate::types::occurrence_codec::CODEC_V1 {
						return Err(Error::DatabaseError(format!("Unknown occurrence codec {codec} for pattern {pattern_id}")).into());
					}
					let payload = blob_row.get_value(1)?.as_blob().ok_or_else(|| Error::DatabaseError("Occurrence payload is not a blob".to_string()))?.clone();
					occurrences.extend(crate::types::occurrence_codec::decode(&payload).map_err(|e| Error::DatabaseError(format!("Corrupt occurrence list for pattern {pattern_id}: {e:#}")))?);
				}
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
			loop {
				let Some(rel_row) = rel_rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get pattern relative row: {e}")))? else {
					break;
				};
				let _rel_pattern_id_str = rel_row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Relative Pattern ID is not text".to_string()))?.clone();
				let _rel_relative_index: usize = usize::try_from(*rel_row.get_value(1)?.as_integer().ok_or_else(|| Error::DatabaseError("Relative Index is not integer".to_string()))?).map_err(|e| Error::DatabaseError(format!("Relative index out of range: {e}")))?;
				let rel_value_json = rel_row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Relative Value is not text".to_string()))?.clone();

				let relative: Relative = serde_json::from_str(&rel_value_json).map_err(|e| Error::DatabaseError(format!("Failed to parse relative JSON: {e}")))?;
				relatives.push(relative);
			}

			Ok(Pattern::new(id, occurrences, relatives))
		} else {
			Err(anyhow::anyhow!("Pattern with ID {pattern_id} not found"))
		}
	}

	/// Every pattern of a dictionary as one shared list. The list is cached once (until the
	/// dictionary is saved); callers borrow it instead of holding their own copy.
	pub(crate) async fn dictionary_patterns_shared(&self, aspect_id: &AspectId, dictionary_name: &str) -> Result<Arc<[Pattern]>> {
		let cache_key = Self::dictionary_patterns_cache_key(aspect_id, dictionary_name);
		// Taken before the read, so a replace that commits meanwhile keeps this read's
		// (possibly older) list out of the cache; see `DatabaseCache::generation`.
		let generation = {
			let cache = self.cache.lock().await;
			if let Some(cached) = cache.get::<Arc<[Pattern]>>(&cache_key).await {
				return Ok(cached);
			}
			cache.generation(&cache_key).await
		};
		let db = self.get_dictionary_db(aspect_id, dictionary_name).await?;
		let conn: cache::Connection = Self::begin_concurrent(&db, dictionary_name, Some(self.cache.clone())).await?;
		drop(db);
		let patterns = Self::load_all_dictionary_patterns(&conn).await;
		Self::commit_concurrent(&conn).await?;
		let patterns = patterns?;
		let patterns: Arc<[Pattern]> = patterns.into();
		// An empty list is not cached, as before: the dictionary may not be saved yet.
		if !patterns.is_empty() {
			self.cache.lock().await.store_if_generation(&cache_key, generation, Arc::clone(&patterns)).await;
		}
		Ok(patterns)
	}

	/// Every pattern of a dictionary file, read with one scan of each pattern table.
	///
	/// The pattern tables have no index on `pattern_id` (MVCC files keep none), so reading
	/// pattern by pattern scanned every table once per pattern: the reads grew with the
	/// pattern count times the file size, and once the file outgrew the connection's page
	/// cache every one of those scans went back to disk. Patterns come out in the order of
	/// their first `patterns` row, each built exactly as
	/// [`Self::load_dictionary_pattern`] builds it; a pattern that does not load is logged
	/// and skipped.
	async fn load_all_dictionary_patterns(conn: &cache::Connection) -> Result<Vec<Pattern>> {
		let db_err = |what: &str, e: turso::Error| Self::db_error(what, &e);
		// Each id once, in the order of its first `patterns` row (rowid order: a plain scan
		// of the table, which no index can serve), and that row's values, as `WHERE id = ?`
		// returns it.
		let mut ids: Vec<String> = Vec::new();
		let mut values: HashMap<String, [String; 7]> = HashMap::new();
		let mut rows = conn.as_ref().query("SELECT id, sum_value, abs_sum_value, max_value, min_value, abs_max_value, avg_value, abs_avg_value FROM patterns", turso::params![]).await.map_err(|e| db_err("query patterns", e))?;
		loop {
			let Some(row) = rows.next().await.map_err(|e| db_err("get pattern row", e))? else { break };
			let text = |i: usize| -> Result<Option<String>> { Ok(row.get_value(i)?.as_text().cloned()) };
			let id = text(0)?.ok_or_else(|| Error::DatabaseError("Pattern ID is not text".to_string()))?;
			if values.contains_key(&id) {
				continue;
			}
			ids.push(id.clone());
			let mut v: [String; 7] = Default::default();
			let mut ok = true;
			for (k, slot) in v.iter_mut().enumerate() {
				let value = text(k + 1)?;
				if let Some(t) = value {
					*slot = t;
				} else {
					ok = false;
				}
			}
			// A non-text value makes the per-pattern load fail; an empty entry marks it.
			values.insert(id, if ok { v } else { Default::default() });
		}
		drop(rows);

		let mut occurrences: HashMap<String, Result<Vec<Occurrence>>> = HashMap::new();
		let mut rows = conn.as_ref().query("SELECT pattern_id, aspect_id, resolution, size, database_info, beginning_timestamp, end_timestamp FROM pattern_occurrences", turso::params![]).await.map_err(|e| db_err("query pattern occurrences", e))?;
		loop {
			let Some(row) = rows.next().await.map_err(|e| db_err("get pattern occurrence row", e))? else { break };
			let Some(pid) = row.get_value(0)?.as_text().cloned() else { continue };
			let entry = occurrences.entry(pid).or_insert_with(|| Ok(Vec::new()));
			if let Ok(list) = entry {
				match Self::parse_pattern_occurrence_row(&row) {
					Ok(o) => list.push(o),
					Err(e) => *entry = Err(e),
				}
			}
		}
		drop(rows);

		// Compact lists, in saved (`id`) order per pattern. A file from before the codec
		// has no such table; that is no error.
		let mut blobs: HashMap<String, Vec<(i64, i64, Vec<u8>)>> = HashMap::new();
		if let Ok(mut rows) = conn.as_ref().query("SELECT pattern_id, id, codec, payload FROM pattern_occurrence_blobs", turso::params![]).await {
			loop {
				let Some(row) = rows.next().await.map_err(|e| db_err("get pattern occurrence list", e))? else { break };
				let Some(pid) = row.get_value(0)?.as_text().cloned() else { continue };
				let id = *row.get_value(1)?.as_integer().ok_or_else(|| Error::DatabaseError("Occurrence list id is not integer".to_string()))?;
				let codec = *row.get_value(2)?.as_integer().ok_or_else(|| Error::DatabaseError("Occurrence codec is not integer".to_string()))?;
				let payload = row.get_value(3)?.as_blob().ok_or_else(|| Error::DatabaseError("Occurrence payload is not a blob".to_string()))?.clone();
				blobs.entry(pid).or_default().push((id, codec, payload));
			}
		}

		let mut relatives = Self::scan_pattern_relatives(conn).await?;

		let mut patterns = Vec::with_capacity(ids.len());
		for id_str in ids {
			let built = (|| -> Result<Pattern> {
				let v = values.remove(&id_str).ok_or_else(|| anyhow::anyhow!("Pattern with ID {id_str} not found"))?;
				let id = PatternID::from_uuid(Uuid::parse_str(&id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for pattern ID: {e}")))?);
				for value in &v {
					BigDecimal::from_str(value).map_err(|e| Error::DatabaseError(format!("Invalid pattern value format: {e}")))?;
				}
				let mut list = occurrences.remove(&id_str).unwrap_or_else(|| Ok(Vec::new()))?;
				let mut lists = blobs.remove(&id_str).unwrap_or_default();
				lists.sort_by_key(|(id, _, _)| *id);
				for (_, codec, payload) in lists {
					if codec != crate::types::occurrence_codec::CODEC_V1 {
						return Err(Error::DatabaseError(format!("Unknown occurrence codec {codec} for pattern {id}")).into());
					}
					list.extend(crate::types::occurrence_codec::decode(&payload).map_err(|e| Error::DatabaseError(format!("Corrupt occurrence list for pattern {id}: {e:#}")))?);
				}
				let mut rels = relatives.remove(&id_str).unwrap_or_else(|| Ok(Vec::new()))?;
				rels.sort_by_key(|(index, _)| *index);
				Ok(Pattern::new(id, list, rels.into_iter().map(|(_, r)| r).collect()))
			})();
			match built {
				Ok(p) => patterns.push(p),
				Err(e) => tracing::warn!("Failed to parse pattern row for ID {id_str}: {e:#}"),
			}
		}
		Ok(patterns)
	}

	/// Every `pattern_relatives` row, grouped by pattern in `relative_index` order once
	/// sorted; a pattern with a row that does not parse maps to that error.
	async fn scan_pattern_relatives(conn: &cache::Connection) -> Result<HashMap<String, Result<Vec<(i64, Relative)>>>> {
		let mut relatives: HashMap<String, Result<Vec<(i64, Relative)>>> = HashMap::new();
		let mut rows = conn.as_ref().query("SELECT pattern_id, relative_index, relative_value FROM pattern_relatives", turso::params![]).await.map_err(|e| Self::db_error("query pattern relatives", &e))?;
		loop {
			let Some(row) = rows.next().await.map_err(|e| Self::db_error("get pattern relative row", &e))? else { break };
			let Some(pid) = row.get_value(0)?.as_text().cloned() else { continue };
			let entry = relatives.entry(pid).or_insert_with(|| Ok(Vec::new()));
			if let Ok(list) = entry {
				let parsed = (|| -> Result<(i64, Relative)> {
					let index = *row.get_value(1)?.as_integer().ok_or_else(|| Error::DatabaseError("Relative Index is not integer".to_string()))?;
					usize::try_from(index).map_err(|e| Error::DatabaseError(format!("Relative index out of range: {e}")))?;
					let json = row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Relative Value is not text".to_string()))?.clone();
					Ok((index, serde_json::from_str(&json).map_err(|e| Error::DatabaseError(format!("Failed to parse relative JSON: {e}")))?))
				})();
				match parsed {
					Ok(r) => list.push(r),
					Err(e) => *entry = Err(e),
				}
			}
		}
		drop(rows);
		Ok(relatives)
	}

	fn db_error(what: &str, e: &turso::Error) -> Error {
		Error::DatabaseError(format!("Failed to {what}: {e}"))
	}

	/// One `pattern_occurrences` row (`pattern_id, aspect_id, resolution, size,
	/// database_info, beginning_timestamp, end_timestamp`) as an [`Occurrence`].
	fn parse_pattern_occurrence_row(occ_row: &turso::Row) -> Result<Occurrence> {
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
		Ok(Occurrence::new(occ_aspect_id, occ_resolution, occ_size, occ_database_info, occ_pattern_id, occ_beginning_timestamp, occ_end_timestamp))
	}

	/// Inner implementation of `fetch_measurements_for_range` without retry
	async fn fetch_measurements_for_range_inner_impl(&self, aspect_id: &AspectId, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Vec<Measurement>> {
		// Seal-backed aspects (slice D-S19): the range comes from the seal store.
		if self.is_seal_backed(aspect_id).await? {
			let store = self.seal_store().await?;
			return crate::legacy_seal::read_measurements(&store, aspect_id, Some(start.timestamp_millis()), Some(end.timestamp_millis())).await;
		}
		let db = self.get_measurement_db(aspect_id).await?;
		let db_path = self.get_measurement_db_path(aspect_id).await?;
		let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		drop(db);

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
		// Seal-backed aspects (slice D-S19): the chunk comes from the seal store.
		if self.is_seal_backed(aspect_id).await? {
			let store = self.seal_store().await?;
			return crate::legacy_seal::read_measurements(&store, aspect_id, Some(chunk_start.timestamp_millis()), Some(chunk_end.timestamp_millis())).await;
		}
		let db = self.get_measurement_db(aspect_id).await?;
		let db_path = self.get_measurement_db_path(aspect_id).await?;
		let conn: cache::Connection = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		drop(db);

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
	/// Extends one `analyze_range` chunk's measurements until `knots` lie past each edge of the
	/// chunk, or the requested range runs out (#506).
	///
	/// The chunk fetch reaches only `degree` steps past the chunk, which is enough on a series
	/// sampled every step. Across a gap longer than that it is not: a chunk inside the gap read
	/// nothing and came out as one `0`, and a chunk at its edge was interpolated without the knot
	/// that closes it. So each side is probed outward through the same chunk fetch, doubling the
	/// window from one chunk's span, until the knots are found or the range's own bound is
	/// reached. The bound is kept because `analyze_range` does not extrapolate past the requested
	/// range. On dense data the first fetch already holds the knots and nothing more is read.
	async fn widen_to_brackets(db: &Self, aspect_id: &AspectId, mut measurements: Vec<Measurement>, windows: BracketWindows, knots: usize, span: chrono::Duration) -> Result<Vec<Measurement>> {
		let BracketWindows { chunk: (chunk_start, chunk_end), fetch: (fetch_start, fetch_end), range: (range_start, range_end) } = windows;
		let span = span.max(chrono::Duration::milliseconds(1));

		let mut after = measurements.iter().filter(|m| m.timestamp() > chunk_end).count();
		let (mut low, mut width) = (fetch_end, span);
		while after < knots && low < range_end {
			let high = low.checked_add_signed(width).map_or(range_end, |h| h.min(range_end));
			// The fetch is inclusive at both ends, and `low` was the previous window's `high`.
			let more: Vec<Measurement> = <Self as crate::types::database::traits::outputs::Outputs>::fetch_measurements_for_chunk(db, aspect_id, low, high).await?.into_iter().filter(|m| m.timestamp() > low).collect();
			after += more.len();
			measurements.extend(more);
			low = high;
			width = width + width;
		}

		let mut before = measurements.iter().filter(|m| m.timestamp() < chunk_start).count();
		let (mut high, mut width) = (fetch_start, span);
		let mut earlier: Vec<Measurement> = Vec::new();
		while before < knots && high > range_start {
			let low = high.checked_sub_signed(width).map_or(range_start, |l| l.max(range_start));
			let mut more: Vec<Measurement> = <Self as crate::types::database::traits::outputs::Outputs>::fetch_measurements_for_chunk(db, aspect_id, low, high).await?.into_iter().filter(|m| m.timestamp() < high).collect();
			before += more.len();
			// Each window is older than everything gathered so far.
			more.append(&mut earlier);
			earlier = more;
			high = low;
			width = width + width;
		}
		if !earlier.is_empty() {
			earlier.append(&mut measurements);
			measurements = earlier;
		}
		Ok(measurements)
	}

	/// Fetch a correlation by ID without requiring &self: the per-id reference the bulk read
	/// is checked against.
	#[cfg(test)]
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

		let error_rate: HashMap<SignalType, ErrorRate> = Self::load_correlation_error_rates(&conn, &id).await?;
		let correlation = Correlation::new(Some(id), crate::CorrelationKeys { dictionary_id, subject_id, aspect_id: aspect_id_parsed, pattern_id, event_id }, error_rate, average_distance);

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

	/// Every correlation in `db`, in the order the old id paging returned them, each
	/// assembled from one scan of `correlations` and `correlation_error_rates` (a
	/// correlation holds no occurrences, so `correlation_occurrences` is not read). A
	/// correlation already in `cache` is served from it, and one read here is cached, as
	/// [`fetch_correlation_by_id`](Self::fetch_correlation_by_id) does.
	async fn load_all_correlations(db: &turso::Database, db_path: &str, cache: &std::sync::Arc<tokio::sync::Mutex<cache::DatabaseCache>>, aspect_id: &AspectId) -> Result<Vec<Result<Correlation>>> {
		let conn: cache::Connection = Self::begin_concurrent(db, db_path, Some(cache.clone())).await?;
		let loaded = Self::scan_correlation_tables(&conn).await;
		if loaded.is_err() {
			let _ = Self::rollback_concurrent(&conn).await;
		}
		let (order, mut rows_by_id, mut error_rates) = loaded?;
		Self::commit_concurrent(&conn).await?;

		// Index in `out` of each id already assembled, so a duplicate id reuses it.
		let mut built: HashMap<String, usize> = HashMap::new();
		let mut out: Vec<Result<Correlation>> = Vec::with_capacity(order.len());
		for correlation_id in order {
			let key = correlation_id.to_uuid().to_string();
			let cache_key = format!("correlation_{}_{key}", aspect_id.as_uuid());
			if let Some(cached) = cache.lock().await.get::<Correlation>(&cache_key).await {
				out.push(Ok(cached));
				continue;
			}
			if let Some(&slot) = built.get(&key) {
				let again = match &out[slot] {
					Ok(c) => Ok(Correlation::clone(c)),
					Err(e) => Err(anyhow::anyhow!(e.to_string())),
				};
				out.push(again);
				continue;
			}
			let result = match rows_by_id.remove(&key) {
				None => Err(format!("Correlation with ID {correlation_id} not found")),
				Some(Err(e)) => Err(e),
				Some(Ok((id, dictionary_id, subject_id, aspect_id_parsed, pattern_id, event_id, average_distance))) => {
					let id_key = id.to_uuid().to_string();
					match error_rates.remove(&id_key).unwrap_or_else(|| Ok(HashMap::new())) {
						Err(e) => Err(e),
						Ok(error_rate) => {
							let correlation = Correlation::new(Some(id), crate::CorrelationKeys { dictionary_id, subject_id, aspect_id: aspect_id_parsed, pattern_id, event_id }, error_rate, average_distance);
							cache.lock().await.store(&cache_key, correlation.clone()).await;
							Ok(correlation)
						}
					}
				}
			};
			if let Err(e) = &result {
				tracing::warn!("Failed to fetch correlation {correlation_id}: {e}");
			}
			built.insert(key, out.len());
			out.push(result.map_err(|e| anyhow::anyhow!(e)));
		}
		Ok(out)
	}

	/// One scan of each correlation table: the correlation ids in table order, the first
	/// row of each id (as `WHERE id = ?` returns it), and each id's error rates. The error
	/// rates are read in rowid order, the order a per-id lookup returns them in, so a
	/// repeated signal type resolves the same way.
	async fn scan_correlation_tables(conn: &cache::Connection) -> Result<(Vec<CorrelationID>, HashMap<String, std::result::Result<CorrelationRowData, String>>, ErrorRatesByCorrelation)> {
		let mut order = Vec::new();
		let mut rows_by_id: HashMap<String, std::result::Result<CorrelationRowData, String>> = HashMap::new();
		let mut rows = conn.as_ref().query("SELECT id, dictionary_id, subject_id, aspect_id, pattern_id, event_id, average_distance_value, average_distance_units FROM correlations", turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to query correlations: {e}")))?;
		loop {
			let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get correlation row: {e}")))? else { break };
			let Some(id_text) = row.get_value(0).ok().and_then(|v| v.as_text().cloned()) else { continue };
			let Ok(uuid) = Uuid::parse_str(&id_text) else { continue };
			order.push(CorrelationID::from_uuid(uuid));
			rows_by_id.entry(id_text).or_insert_with(|| Self::parse_correlation_row(&row).map_err(|e| e.to_string()));
		}
		drop(rows);

		let mut error_rates: ErrorRatesByCorrelation = HashMap::new();
		let mut rows = conn.as_ref().query("SELECT correlation_id, signal_type, error_rate_value, error_rate_units FROM correlation_error_rates", turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to query correlation error rates: {e}")))?;
		loop {
			let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get correlation error rate row: {e}")))? else { break };
			let Some(correlation_id) = row.get_value(0).ok().and_then(|v| v.as_text().cloned()) else { continue };
			let parsed = Self::parse_error_rate_row(&row);
			let entry = error_rates.entry(correlation_id).or_insert_with(|| Ok(HashMap::new()));
			if let Ok(map) = entry {
				match parsed {
					Ok((signal_type, rate)) => {
						map.insert(signal_type, rate);
					}
					Err(e) => *entry = Err(e.to_string()),
				}
			}
		}
		drop(rows);

		Ok((order, rows_by_id, error_rates))
	}

	/// An error rate row: `correlation_id(0)`, `signal_type(1)`, `error_rate_value(2)`,
	/// `error_rate_units(3)`.
	fn parse_error_rate_row(row: &turso::Row) -> Result<(SignalType, ErrorRate)> {
		let signal_type_str = row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Error rate signal type is not text".to_string()))?.clone();
		let error_rate_value: BigDecimal = {
			let value_str = row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Error rate value is not text".to_string()))?.clone();
			BigDecimal::from_str(&value_str).map_err(|e| Error::DatabaseError(format!("Invalid error rate value format: {e}")))?
		};
		let error_rate_units_str = row.get_value(3)?.as_text().ok_or_else(|| Error::DatabaseError("Error rate units is not text".to_string()))?.clone();

		let units: Resolution = Resolution::from_str(&error_rate_units_str).map_err(|e| Error::DatabaseError(format!("Invalid error rate units format: {e}")))?;
		let err_rate = ErrorRate::new(error_rate_value, units);

		let signal_type = SignalType::from_str(&signal_type_str).map_err(|e| Error::DatabaseError(format!("Invalid error type format: {e}")))?;
		Ok((signal_type, err_rate))
	}

	/// An occurrence row: `correlation_id(0)`, `occurrence_index(1)`, then the occurrence.
	pub(crate) fn parse_occurrence_row(row: &turso::Row) -> Result<(usize, Occurrence)> {
		let occ_index: usize = usize::try_from(*row.get_value(1)?.as_integer().ok_or_else(|| Error::DatabaseError("Occurrence Index is not integer".to_string()))?).map_err(|e| Error::DatabaseError(format!("Occurrence index out of range: {e}")))?;
		let occ_aspect_id_str = row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Occurrence Aspect ID is not text".to_string()))?.clone();
		let occ_resolution_str = row.get_value(3)?.as_text().ok_or_else(|| Error::DatabaseError("Occurrence Resolution is not text".to_string()))?.clone();
		let occ_size: usize = usize::try_from(*row.get_value(4)?.as_integer().ok_or_else(|| Error::DatabaseError("Occurrence Size is not integer".to_string()))?).map_err(|e| Error::DatabaseError(format!("Occurrence size out of range: {e}")))?;
		let occ_database_info_str = row.get_value(5)?.as_text().ok_or_else(|| Error::DatabaseError("Occurrence Database Info is not text".to_string()))?.clone();
		let occ_pattern_id_str = row.get_value(6)?.as_text().ok_or_else(|| Error::DatabaseError("Occurrence Pattern ID is not text".to_string()))?.clone();
		let occ_beginning_timestamp_millis: i64 = *row.get_value(7)?.as_integer().ok_or_else(|| Error::DatabaseError("Occurrence Beginning Timestamp is not integer".to_string()))?;
		let occ_end_timestamp_millis: i64 = *row.get_value(8)?.as_integer().ok_or_else(|| Error::DatabaseError("Occurrence End Timestamp is not integer".to_string()))?;
		let occ_aspect_id = AspectId::from_str(&occ_aspect_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for occurrence Aspect ID: {e}")))?;
		let occ_resolution = Resolution::from_str(&occ_resolution_str).map_err(|e| Error::DatabaseError(format!("Invalid resolution format: {e}")))?;
		let occ_pattern_id = PatternID::from_uuid(Uuid::parse_str(&occ_pattern_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for occurrence Pattern ID: {e}")))?);
		let occ_beginning_timestamp = DateTime::from_timestamp_millis(occ_beginning_timestamp_millis).ok_or_else(|| Error::DatabaseError("Invalid beginning timestamp".to_string()))?;
		let occ_end_timestamp = DateTime::from_timestamp_millis(occ_end_timestamp_millis).ok_or_else(|| Error::DatabaseError("Invalid end timestamp".to_string()))?;
		let occ_database_info: DatabaseInfo = serde_json::from_str(&occ_database_info_str).map_err(|e| Error::DatabaseError(format!("Failed to parse occurrence database info JSON: {e}")))?;
		Ok((occ_index, Occurrence::new(occ_aspect_id, occ_resolution, occ_size, occ_database_info, occ_pattern_id, occ_beginning_timestamp, occ_end_timestamp)))
	}

	async fn load_correlation_error_rates(conn: &cache::Connection, correlation_id: &CorrelationID) -> Result<HashMap<SignalType, ErrorRate>> {
		let error_rate_query_sql = r"
	                                SELECT correlation_id, signal_type, error_rate_value, error_rate_units
	                                FROM correlation_error_rates
	                                WHERE correlation_id = ?
	                        ";
		let mut rows: turso::Rows = conn.as_ref().query(error_rate_query_sql, turso::params![correlation_id.to_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query correlation error rates: {e}")))?;
		let mut error_rate: HashMap<SignalType, ErrorRate> = HashMap::new();
		loop {
			let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get correlation error rate row: {e}")))? else { break };
			let (signal_type, rate) = Self::parse_error_rate_row(&row)?;
			error_rate.insert(signal_type, rate);
		}
		Ok(error_rate)
	}

	/// Parse a measurement row without requiring &self (for use in streaming closures)
	/// Helper function to parse a batch row from the database with aspect context
	///
	/// Expected columns: `id(0)`, `aspect_id(1)`, `database_id(2)`, `size(3)`, `resolution(4)`, `measurements(5)`, `batch_hash(6)`
	fn parse_batch_row_helper(row: &turso::Row, db_name: &str, db_path: &str) -> Result<Batch> {
		let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("Batch ID is not text".to_string()))?.clone();
		let batch_id = BatchId::from_uuid(Uuid::parse_str(&id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for batch ID: {e}")))?);

		let aspect_id_str = row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Aspect ID is not text".to_string()))?.clone();
		let aspect_id = AspectId::from_uuid(Uuid::parse_str(&aspect_id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for aspect ID: {e}")))?);

		let size = crate::types::num::i64_to_usize(*row.get_value(3)?.as_integer().ok_or_else(|| Error::DatabaseError("Size is not an integer".to_string()))?);

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
	/// The cache key of an aspect's unprocessed event list. Every write to the unprocessed
	/// events table invalidates it with `invalidate_generation`.
	pub(crate) fn unprocessed_events_cache_key(aspect_id: &AspectId) -> String {
		format!("unprocessed_events_{}", aspect_id.as_uuid())
	}

	pub(crate) fn dictionary_metadata_cache_key(aspect_id: &AspectId, dictionary_name: &str) -> String {
		format!("dictionary_metadata_{}_{dictionary_name}", aspect_id.as_uuid())
	}

	/// [`get_dictionary_metadata`](crate::database::traits::Outputs::get_dictionary_metadata),
	/// with a registration whose stored values do not parse as `Some(Err(…))` instead of an
	/// error: [`list_dictionaries`](crate::database::traits::Outputs::list_dictionaries)
	/// skips such a dictionary, but fails on a read that failed.
	///
	/// A registration that was read is cached (see `get_dictionary_metadata`), and only if
	/// no write on this `Database` invalidated its entry since the read began: the cache
	/// generation is taken before the read's snapshot. Only `DatabaseCache`'s
	/// `store_if_generation` is tested for that; the interleaving inside this method, a
	/// replacing commit between the snapshot and the store, has no deterministic test.
	///
	/// # Errors
	///
	/// An [`InvalidDictionaryName`](crate::InvalidDictionaryName), an unknown aspect, or a
	/// failed read: I/O, a query, or an MVCC conflict.
	async fn dictionary_registration(&self, aspect_id: &AspectId, dictionary_name: &str) -> Result<Option<StoredRegistration>> {
		// Check cache first. Keyed by aspect too: dictionary names are per aspect.
		let cache_key = Self::dictionary_metadata_cache_key(aspect_id, dictionary_name);
		if let Some(metadata) = self.cache.lock().await.get::<DictionaryMetadata>(&cache_key).await {
			return Ok(Some(Ok(metadata)));
		}

		// Each dictionary is its own database file. Without one nothing is registered under
		// this name, so the answer is `None` (on which `load_dictionary` registers the
		// dictionary), not the error `get_dictionary_db` gives for a missing file.
		let aspect = self.get_aspect(aspect_id).await?;
		let db_path = Self::aspect_dictionaries_db_path(&self.name, aspect.subject_name(), aspect.name(), dictionary_name)?;
		drop(aspect);
		if !tokio::fs::try_exists(&db_path).await? {
			return Ok(None);
		}

		// Get from database. The cache generation is taken before the read's snapshot, so a
		// registration a writer replaces meanwhile is not cached over its invalidation.
		let generation = self.cache.lock().await.generation(&cache_key).await;
		let db = self.get_dictionary_db(aspect_id, dictionary_name).await?;
		let conn: cache::Connection = Self::begin_concurrent(&db, dictionary_name, Some(self.cache.clone())).await?;
		drop(db);
		let registration = match Self::read_dictionary_registration(&conn, dictionary_name).await {
			Ok(registration) => registration,
			Err(e) => {
				// Report why the read failed, not a failed rollback.
				Self::rollback_after_error(&conn).await;
				return Err(e);
			}
		};
		Self::commit_concurrent(&conn).await?;

		if let Some(Ok(metadata)) = &registration {
			self.cache.lock().await.store_if_generation(&cache_key, generation, metadata.clone()).await;
		}
		Ok(registration)
	}

	/// Read `dictionary_name`'s registration on `conn`, a transaction on its database.
	///
	/// A registration is a `dictionary_metadata` row with a `dictionary_constraints` row;
	/// its `dictionary_variabilities` rows, in the order they were written, are its
	/// variabilities (none is `None`). The metadata table has no unique constraint, and
	/// databases written before `set_dictionary_metadata` replaced registrations can hold
	/// several rows for a name, some without constraints (it wrote none): the newest
	/// complete one, by `created_at` and then id, is read, and a name with none is `None`.
	///
	/// A dictionary file can exist without its tables: one that
	/// `insert_pattern_into_dictionary` opened first (it creates none), or one this process
	/// opened while another was still creating them. It cannot hold a registration, so it
	/// is `None`, on which `load_dictionary` registers the dictionary and so creates them.
	///
	/// A registration whose stored values do not parse is `Some(Err(…))`: see
	/// [`parse_stored_steps`](Self::parse_stored_steps) and
	/// [`parse_stored_variability`](Self::parse_stored_variability).
	///
	/// # Errors
	///
	/// A failed query.
	async fn read_dictionary_registration(conn: &cache::Connection, dictionary_name: &str) -> Result<Option<StoredRegistration>> {
		let tables = Self::registration_tables(conn).await?;
		if !(tables.metadata && tables.constraints) {
			return Ok(None);
		}
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
		let columns = [row.get_value(0)?, row.get_value(1)?, row.get_value(2)?, row.get_value(3)?, row.get_value(4)?];
		drop(rows);
		let [id, name, description, steps_count, steps_interpolation] = &columns;
		let parsed = (|| -> Result<_> {
			let id_str = id.as_text().ok_or_else(|| Error::DatabaseError("Dictionary ID is not text".to_string()))?;
			let name = name.as_text().ok_or_else(|| Error::DatabaseError("Dictionary name is not text".to_string()))?.clone();
			let description = description.as_text().ok_or_else(|| Error::DatabaseError("Dictionary description is not text".to_string()))?.clone();
			let steps = Self::parse_stored_steps(steps_count, steps_interpolation)?;
			let id = DictionaryId::from_uuid(Uuid::parse_str(id_str).map_err(|e| Error::InvalidIdError(format!("Invalid UUID format for dictionary ID: {e}")))?);
			Ok((id, id_str.clone(), name, description, steps))
		})();
		let (id, id_str, name, description, steps) = match parsed {
			Ok(parsed) => parsed,
			Err(e) => return Ok(Some(Err(e))),
		};

		let variability_query_sql = r"
                        SELECT variability_type, variability_value
                        FROM dictionary_variabilities
                        WHERE dictionary_id = ?
                        ORDER BY id
                ";
		let mut stored_variabilities = Vec::new();
		if tables.variabilities {
			let mut rows: turso::Rows = conn.as_ref().query(variability_query_sql, turso::params![id_str]).await.map_err(|e| Error::DatabaseError(format!("Failed to query dictionary variabilities: {e}")))?;
			while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get variability row: {e}")))? {
				stored_variabilities.push((row.get_value(0)?, row.get_value(1)?));
			}
		}
		let variabilities = match stored_variabilities.iter().map(|(kind, value)| Self::parse_stored_variability(kind, value)).collect::<Result<Vec<_>>>() {
			Ok(variabilities) => (!variabilities.is_empty()).then_some(variabilities),
			Err(e) => return Ok(Some(Err(e))),
		};

		Ok(Some(Ok(DictionaryMetadata { id, name, description, constraints: DictionaryConstraints::new(steps, variabilities) })))
	}

	/// Which of a registration's tables exist in the dictionary's database, on `conn`.
	///
	/// # Errors
	///
	/// A failed query of `sqlite_master`.
	async fn registration_tables(conn: &cache::Connection) -> Result<RegistrationTables> {
		let mut tables = RegistrationTables::default();
		let mut rows = conn.as_ref().query("SELECT name FROM sqlite_master WHERE type = 'table' AND name IN ('dictionary_metadata', 'dictionary_constraints', 'dictionary_variabilities')", turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to list the dictionary's tables: {e}")))?;
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to list the dictionary's tables: {e}")))? {
			match row.get_value(0)?.as_text().map(String::as_str) {
				Some("dictionary_metadata") => tables.metadata = true,
				Some("dictionary_constraints") => tables.constraints = true,
				Some("dictionary_variabilities") => tables.variabilities = true,
				_ => {}
			}
		}
		Ok(tables)
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

/// A dictionary's registration as stored: its metadata, or why its stored values do not
/// parse (see `Database::read_dictionary_registration`).
type StoredRegistration = Result<DictionaryMetadata>;

/// Which of the tables a dictionary's registration is stored in exist in its database.
#[derive(Debug, Default, Clone, Copy)]
struct RegistrationTables {
	/// `dictionary_metadata`.
	metadata: bool,
	/// `dictionary_constraints`.
	constraints: bool,
	/// `dictionary_variabilities`.
	variabilities: bool,
}

/// A stream that clones each pattern of `patterns` only as it is yielded.
fn shared_pattern_stream(patterns: Arc<[Pattern]>) -> impl Stream<Item = Result<Pattern>> + Send + 'static {
	let len = patterns.len();
	futures::stream::iter((0..len).map(move |i| Ok(patterns[i].clone())))
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

	mod dictionary_reads {
		use futures::TryStreamExt;

		use crate::{
			database::traits::{AspectStructure, DatabaseStructure, Outputs}, Database, DatabaseInfo, DictionaryConstraints, Occurrence, Pattern, PatternID
		};

		fn pattern(aspect_id: crate::AspectId) -> Pattern {
			let id = PatternID::from_uuid(uuid::Uuid::new_v4());
			let now = chrono::Utc::now();
			Pattern::new(id, vec![Occurrence::new(aspect_id, splimes::Resolution::Seconds, 3, DatabaseInfo::new("t".to_string(), "t".to_string()), id, now, now)], Vec::new())
		}

		/// A dictionary's patterns are cached once, as one shared list: reading them does
		/// not also cache each pattern, every reader shares the same list, and a save
		/// drops it so the next read sees the new patterns.
		#[tokio::test]
		#[serial_test::serial]
		async fn a_dictionary_is_cached_as_one_shared_list() -> anyhow::Result<()> {
			crate::types::database::config::use_temp_data_dir();
			let name = format!("dictionary_reads_{}", uuid::Uuid::new_v4().simple());
			let db = Database::new(&name).await?;
			let subject = db.observe_subject("s").await?;
			let aspect = db.track_aspect(&subject.id(), "a", &splimes::Resolution::Seconds, None).await?;
			let aspect_id = aspect.id();
			aspect.new_dictionary("d", "", &DictionaryConstraints::default()).await?;
			drop(aspect);
			let saved: Vec<Pattern> = (0..3).map(|_| pattern(aspect_id)).collect();
			db.replace_dictionary_patterns(&aspect_id, "d", &saved).await?;

			let first = db.dictionary_patterns_shared(&aspect_id, "d").await?;
			let second = db.dictionary_patterns_shared(&aspect_id, "d").await?;
			assert!(std::sync::Arc::ptr_eq(&first, &second), "each read built its own list");
			assert_eq!(first.len(), saved.len());
			for p in &saved {
				assert!(db.cache.lock().await.get::<Pattern>(&format!("pattern_{}", p.id().as_uuid())).await.is_none(), "the list read cached a second copy of a pattern");
			}
			let streamed: Vec<Pattern> = db.get_dictionary_patterns(&aspect_id, "d").await?.try_collect().await?;
			assert_eq!(streamed.as_slice(), &*first);

			let grown: Vec<Pattern> = saved.iter().cloned().chain(std::iter::once(pattern(aspect_id))).collect();
			db.replace_dictionary_patterns(&aspect_id, "d", &grown).await?;
			let after = db.dictionary_patterns_shared(&aspect_id, "d").await?;
			assert!(!std::sync::Arc::ptr_eq(&first, &after), "the save left the old list cached");
			assert_eq!(after.len(), grown.len());
			drop((first, second, after));
			db.close().await?;
			drop(db);
			Ok(())
		}

		/// A save leaves the list a fresh read would build in the cache, so the pipeline's
		/// next read (every run saves, then reads) does not scan the whole file again. Saved
		/// patterns replace their stored copies and move after the patterns kept.
		#[tokio::test]
		#[serial_test::serial]
		async fn a_save_caches_the_list_a_fresh_read_builds() -> anyhow::Result<()> {
			crate::types::database::config::use_temp_data_dir();
			let name = format!("dictionary_write_through_{}", uuid::Uuid::new_v4().simple());
			let db = Database::new(&name).await?;
			let subject = db.observe_subject("s").await?;
			let aspect = db.track_aspect(&subject.id(), "a", &splimes::Resolution::Seconds, None).await?;
			let aspect_id = aspect.id();
			aspect.new_dictionary("d", "", &DictionaryConstraints::default()).await?;
			drop(aspect);
			let key = Database::dictionary_patterns_cache_key(&aspect_id, "d");
			let stored: Vec<Pattern> = (0..4).map(|_| pattern(aspect_id)).collect();
			db.replace_dictionary_patterns(&aspect_id, "d", &stored).await?;
			assert!(db.cache.lock().await.get::<std::sync::Arc<[Pattern]>>(&key).await.is_none(), "a save with no list cached cannot vouch for the file");
			drop(db.dictionary_patterns_shared(&aspect_id, "d").await?);

			// The second pattern grows an occurrence; a new one joins; the rest are kept.
			let id = stored[1].id();
			let now = chrono::Utc::now();
			let occurrences = stored[1].occurrences().iter().cloned().chain(std::iter::once(Occurrence::new(aspect_id, splimes::Resolution::Seconds, 3, DatabaseInfo::new("t".to_string(), "t".to_string()), id, now, now))).collect();
			let saved = vec![Pattern::new(id, occurrences, Vec::new()), pattern(aspect_id)];
			drop(stored);
			db.replace_dictionary_patterns(&aspect_id, "d", &saved).await?;
			let cached = db.cache.lock().await.get::<std::sync::Arc<[Pattern]>>(&key).await.expect("the save dropped the list instead of replacing it");

			*db.cache.lock().await = crate::DatabaseCache::new();
			let fresh = db.dictionary_patterns_shared(&aspect_id, "d").await?;
			assert_eq!(&*cached, &*fresh, "the cached list differs from what the file holds");
			assert_eq!(fresh.len(), 5);

			// Saving the whole list again with one pattern changed rewrites only that one:
			// the others keep their rows, so it alone moves to the end of the file.
			let changed = Pattern::new(fresh[0].id(), fresh[0].occurrences().iter().cloned().chain(std::iter::once(Occurrence::new(aspect_id, splimes::Resolution::Seconds, 3, DatabaseInfo::new("t".to_string(), "t".to_string()), fresh[0].id(), now, now))).collect(), Vec::new());
			let resaved: Vec<Pattern> = std::iter::once(changed.clone()).chain(fresh[1..].iter().cloned()).collect();
			db.replace_dictionary_patterns(&aspect_id, "d", &resaved).await?;
			let cached = db.cache.lock().await.get::<std::sync::Arc<[Pattern]>>(&key).await.expect("the save dropped the list");
			*db.cache.lock().await = crate::DatabaseCache::new();
			let reread = db.dictionary_patterns_shared(&aspect_id, "d").await?;
			let expected: Vec<Pattern> = fresh[1..].iter().cloned().chain(std::iter::once(changed)).collect();
			assert_eq!(&*reread, expected.as_slice(), "an unchanged pattern was written again");
			assert_eq!(&*cached, &*reread);

			// Each pattern table is indexed on its pattern id, so that a save's deletes and a
			// one-pattern read do not scan the file.
			let file = db.get_dictionary_db(&aspect_id, "d").await?;
			let conn = file.connect()?;
			let mut rows = conn.query("SELECT name FROM sqlite_schema WHERE type = 'index'", ()).await?;
			let mut indexes = Vec::new();
			while let Some(row) = rows.next().await? {
				indexes.push(row.get_value(0)?.as_text().cloned().unwrap_or_default());
			}
			drop((rows, conn, file));
			for name in ["idx_patterns_id", "idx_pattern_occurrences_pattern", "idx_pattern_occurrence_blobs_pattern", "idx_pattern_relatives_pattern"] {
				assert!(indexes.iter().any(|i| i == name), "{name} missing from {indexes:?}");
			}

			// Saving one id twice is left to a read to rebuild.
			let twice = vec![saved[1].clone(), saved[1].clone()];
			db.replace_dictionary_patterns(&aspect_id, "d", &twice).await?;
			assert!(db.cache.lock().await.get::<std::sync::Arc<[Pattern]>>(&key).await.is_none());
			drop((cached, fresh, reread));
			db.close().await?;
			drop(db);
			Ok(())
		}

		/// A save of more patterns than an old 256-id chunk clears every stored copy in its
		/// one statement per table: saving the same patterns again leaves one copy of each.
		#[tokio::test]
		#[serial_test::serial]
		async fn a_large_save_replaces_every_stored_copy() -> anyhow::Result<()> {
			crate::types::database::config::use_temp_data_dir();
			let name = format!("dictionary_large_save_{}", uuid::Uuid::new_v4().simple());
			let db = Database::new(&name).await?;
			let subject = db.observe_subject("s").await?;
			let aspect = db.track_aspect(&subject.id(), "a", &splimes::Resolution::Seconds, None).await?;
			let aspect_id = aspect.id();
			aspect.new_dictionary("d", "", &DictionaryConstraints::default()).await?;
			drop(aspect);
			let saved: Vec<Pattern> = (0..600).map(|_| pattern(aspect_id)).collect();
			db.replace_dictionary_patterns(&aspect_id, "d", &saved).await?;
			db.replace_dictionary_patterns(&aspect_id, "d", &saved).await?;
			*db.cache.lock().await = crate::DatabaseCache::new();
			let read = db.dictionary_patterns_shared(&aspect_id, "d").await?;
			assert_eq!(&*read, saved.as_slice(), "a pattern was stored twice or lost");
			drop(read);
			db.close().await?;
			drop(db);
			Ok(())
		}

		/// The whole-dictionary read (one scan per pattern table) builds every pattern exactly
		/// as the one-pattern read does: occurrences, relatives in index order, and order.
		#[tokio::test]
		#[serial_test::serial]
		async fn the_whole_dictionary_read_matches_the_per_pattern_read() -> anyhow::Result<()> {
			use bigdecimal::BigDecimal;
			crate::types::database::config::use_temp_data_dir();
			let name = format!("dictionary_bulk_{}", uuid::Uuid::new_v4().simple());
			let db = Database::new(&name).await?;
			let subject = db.observe_subject("s").await?;
			let aspect = db.track_aspect(&subject.id(), "a", &splimes::Resolution::Seconds, None).await?;
			let aspect_id = aspect.id();
			aspect.new_dictionary("d", "", &DictionaryConstraints::default()).await?;
			drop(aspect);
			let saved: Vec<Pattern> = (0..5_i64)
				.map(|n| {
					let id = PatternID::from_uuid(uuid::Uuid::new_v4());
					let at = |s: i64| chrono::DateTime::from_timestamp(1_700_000_000 + s, 0).expect("valid time");
					let occurrences = (0..=n).map(|k| Occurrence::new(aspect_id, splimes::Resolution::Seconds, 3, DatabaseInfo::new("t".to_string(), "t".to_string()), id, at(k), at(k + 3))).collect();
					let relatives = (0..3_i64).map(|k| crate::Relative::new(crate::MeasurementVector::new(BigDecimal::from(k), BigDecimal::from(n * 10 + k)), BigDecimal::from(2), BigDecimal::from(n))).collect();
					Pattern::new(id, occurrences, relatives)
				})
				.collect();
			db.replace_dictionary_patterns(&aspect_id, "d", &saved).await?;

			let bulk = db.dictionary_patterns_shared(&aspect_id, "d").await?;
			assert_eq!(bulk.len(), saved.len());
			for p in bulk.iter() {
				let one = db.load_dictionary_pattern(&aspect_id, "d", &p.id()).await?;
				assert_eq!(p, &one, "the bulk read built a pattern differently");
				let original = saved.iter().find(|s| s.id() == p.id()).expect("a saved pattern");
				assert_eq!(p.occurrences().len(), original.occurrences().len());
				assert_eq!(p.relatives(), original.relatives());
			}
			drop(bulk);
			db.close().await?;
			drop(db);
			Ok(())
		}

		/// A dictionary heavier than the cache's byte budget is not kept resident: every
		/// read loads it again from disk, and the patterns are the same as a cached read's.
		#[tokio::test]
		#[serial_test::serial]
		async fn a_dictionary_over_the_cache_budget_is_reread_unchanged() -> anyhow::Result<()> {
			crate::types::database::config::use_temp_data_dir();
			let name = format!("dictionary_budget_{}", uuid::Uuid::new_v4().simple());
			let db = Database::new(&name).await?;
			let subject = db.observe_subject("s").await?;
			let aspect = db.track_aspect(&subject.id(), "a", &splimes::Resolution::Seconds, None).await?;
			let aspect_id = aspect.id();
			aspect.new_dictionary("d", "", &DictionaryConstraints::default()).await?;
			drop(aspect);
			let saved: Vec<Pattern> = (0..3).map(|_| pattern(aspect_id)).collect();
			db.replace_dictionary_patterns(&aspect_id, "d", &saved).await?;
			let cached = db.dictionary_patterns_shared(&aspect_id, "d").await?;

			let list_weight = crate::Cacheable::weight(&cached);
			*db.cache.lock().await = crate::DatabaseCache::with_max_bytes(list_weight - 1);
			let first = db.dictionary_patterns_shared(&aspect_id, "d").await?;
			let second = db.dictionary_patterns_shared(&aspect_id, "d").await?;
			assert!(!std::sync::Arc::ptr_eq(&first, &second), "the list was cached over budget");
			assert!(db.cache.lock().await.bytes().await < list_weight, "the list is not resident");
			assert_eq!(&*first, &*cached);
			assert_eq!(&*second, &*cached);
			drop((cached, first, second));
			db.close().await?;
			drop(db);
			Ok(())
		}
	}

	mod correlation_reads {
		use futures::StreamExt;

		use crate::{
			cache, correlation::ErrorRate, database::traits::{AspectStructure, DatabaseStructure, Inputs, Outputs}, types::signal::Distance, Correlation, CorrelationID, CorrelationKeys, Database, DictionaryId, Event, EventID, PatternID, SignalType, SubjectId
		};

		fn correlation(aspect_id: crate::AspectId, subject_id: SubjectId, n: i64) -> Correlation {
			let pattern_id = PatternID::from_uuid(uuid::Uuid::new_v4());
			let mut error_rate = std::collections::HashMap::new();
			error_rate.insert(SignalType::PredictStart, ErrorRate::new(bigdecimal::BigDecimal::from(n), splimes::Resolution::Seconds));
			if n % 2 == 0 {
				error_rate.insert(SignalType::PredictEnd, ErrorRate::new(bigdecimal::BigDecimal::from(n * 3), splimes::Resolution::Milliseconds));
			}
			let average_distance = (n % 3 != 0).then(|| Distance::new(bigdecimal::BigDecimal::from(n), splimes::Resolution::Seconds));
			Correlation::new(Some(CorrelationID::from_uuid(uuid::Uuid::new_v4())), CorrelationKeys { dictionary_id: DictionaryId::from_uuid(uuid::Uuid::new_v4()), subject_id, aspect_id, pattern_id, event_id: EventID::from_uuid(uuid::Uuid::new_v4()) }, error_rate, average_distance)
		}

		/// The bulk read returns what the per-id read returns, in the table's order, and the
		/// correlation tables are indexed by correlation id.
		#[tokio::test]
		#[serial_test::serial]
		async fn the_bulk_read_matches_the_per_id_read() -> anyhow::Result<()> {
			crate::types::database::config::use_temp_data_dir();
			let name = format!("correlation_reads_{}", uuid::Uuid::new_v4().simple());
			let db = Database::new(&name).await?;
			let subject = db.observe_subject("s").await?;
			let aspect_id = db.track_aspect(&subject.id(), "a", &splimes::Resolution::Seconds, None).await?.id();
			let inserted: Vec<Correlation> = (1..=6).map(|n| correlation(aspect_id, subject.id(), n)).collect();
			for c in &inserted {
				db.insert_correlation(&aspect_id, c).await?;
			}

			let streamed: Vec<Correlation> = db.get_correlations(&aspect_id).await?.map(|r| r.expect("every correlation reads")).collect().await;
			assert_eq!(streamed.iter().map(Correlation::id).collect::<Vec<_>>(), inserted.iter().map(Correlation::id).collect::<Vec<_>>(), "not in insertion (table) order");

			let mut handle = db.get_aspect(&aspect_id).await?;
			let corr_db = handle.correlations().await?;
			let corr_path = handle.correlations_path();
			drop(handle);
			// Fresh caches, so both paths read the tables.
			let bulk = Database::load_all_correlations(&corr_db, &corr_path, &std::sync::Arc::new(tokio::sync::Mutex::new(cache::DatabaseCache::new())), &aspect_id).await?;
			assert_eq!(bulk.len(), inserted.len());
			let per_id_cache = std::sync::Arc::new(tokio::sync::Mutex::new(cache::DatabaseCache::new()));
			for (bulk, expected) in bulk.into_iter().zip(&inserted) {
				let bulk = bulk?;
				let per_id = Database::fetch_correlation_by_id(&corr_db, &corr_path, &per_id_cache, &aspect_id, expected.id()).await?;
				assert_eq!(bulk, per_id, "the bulk read differs from the per-id read");
				assert_eq!(&bulk, expected, "the read differs from what was inserted");
			}

			let conn = <Database as crate::database::traits::connection::Connection>::begin_concurrent(&corr_db, &corr_path, None).await?;
			for (sql, index) in [("EXPLAIN QUERY PLAN SELECT id FROM correlations WHERE id = '00000000-0000-0000-0000-000000000000'", "idx_correlations_id"), ("EXPLAIN QUERY PLAN SELECT signal_type FROM correlation_error_rates WHERE correlation_id = 'x'", "idx_correlation_error_rates_correlation"), ("EXPLAIN QUERY PLAN SELECT size FROM correlation_occurrences WHERE correlation_id = 'x'", "idx_correlation_occurrences_correlation")] {
				let mut rows = conn.as_ref().query(sql, turso::params![]).await?;
				let mut plan = String::new();
				while let Some(row) = turso::Rows::next(&mut rows).await? {
					for i in 0..row.column_count() {
						if let Ok(turso::Value::Text(t)) = row.get_value(i) {
							plan.push_str(&t);
							plan.push(' ');
						}
					}
				}
				drop(rows);
				assert!(plan.contains(index), "{sql} does not use {index}: {plan}");
			}
			<Database as crate::database::traits::connection::Connection>::commit_concurrent(&conn).await?;
			drop(conn);
			drop(corr_db);
			db.close().await?;
			drop(db);
			Ok(())
		}

		/// The correlation read never opens `correlation_occurrences`: a row there whose
		/// occurrence JSON does not parse (which failed the old read of that correlation)
		/// leaves every correlation readable, and no correlation write adds a row.
		#[tokio::test]
		#[serial_test::serial]
		async fn get_correlations_does_not_parse_occurrence_json() -> anyhow::Result<()> {
			crate::types::database::config::use_temp_data_dir();
			let name = format!("correlation_no_occurrences_{}", uuid::Uuid::new_v4().simple());
			let db = Database::new(&name).await?;
			let subject = db.observe_subject("s").await?;
			let aspect_id = db.track_aspect(&subject.id(), "a", &splimes::Resolution::Seconds, None).await?.id();
			let inserted: Vec<Correlation> = (1..=3).map(|n| correlation(aspect_id, subject.id(), n)).collect();
			for c in &inserted {
				db.insert_correlation(&aspect_id, c).await?;
			}
			db.update_correlation(&aspect_id, &inserted[0]).await?;
			db.update_correlations(&aspect_id, &inserted).await?;

			let mut handle = db.get_aspect(&aspect_id).await?;
			let corr_db = handle.correlations().await?;
			let corr_path = handle.correlations_path();
			drop(handle);
			let conn = <Database as crate::database::traits::connection::Connection>::begin_concurrent(&corr_db, &corr_path, None).await?;
			let mut rows = conn.as_ref().query("SELECT COUNT(*) FROM correlation_occurrences", turso::params![]).await?;
			let count = turso::Rows::next(&mut rows).await?.and_then(|row| row.get_value(0).ok()).and_then(|v| v.as_integer().copied());
			drop(rows);
			assert_eq!(count, Some(0), "a correlation write copied occurrences");
			for c in &inserted {
				conn.as_ref().execute("INSERT INTO correlation_occurrences (correlation_id, occurrence_index, aspect_id, resolution, size, database_info, pattern_id, beginning_timestamp, end_timestamp) VALUES (?, 0, ?, 'Seconds', 1, '{not json', ?, 0, 1)", turso::params![c.id().to_string(), aspect_id.to_string(), c.pattern_id().to_string()]).await?;
			}
			<Database as crate::database::traits::connection::Connection>::commit_concurrent(&conn).await?;
			drop(conn);

			let bulk = Database::load_all_correlations(&corr_db, &corr_path, &std::sync::Arc::new(tokio::sync::Mutex::new(cache::DatabaseCache::new())), &aspect_id).await?;
			let read: Vec<Correlation> = bulk.into_iter().collect::<anyhow::Result<_>>()?;
			assert_eq!(read, inserted);
			let fresh = Database::fetch_correlation_by_id(&corr_db, &corr_path, &std::sync::Arc::new(tokio::sync::Mutex::new(cache::DatabaseCache::new())), &aspect_id, inserted[0].id()).await?;
			assert_eq!(fresh, inserted[0]);
			drop(corr_db);
			db.close().await?;
			drop(db);
			Ok(())
		}

		/// A correlation serialized by an older release, with its `occurrences`, still reads.
		#[test]
		fn old_json_with_occurrences_deserializes() -> anyhow::Result<()> {
			let current = correlation(crate::AspectId::from_uuid(uuid::Uuid::new_v4()), SubjectId::from_uuid(uuid::Uuid::new_v4()), 2);
			let mut json: serde_json::Value = serde_json::to_value(&current)?;
			json.as_object_mut().expect("an object").insert("occurrences".to_string(), serde_json::json!([]));
			let read: Correlation = serde_json::from_value(json)?;
			assert_eq!(read, current);
			Ok(())
		}

		/// An unprocessed event is listed as soon as it is written, and gone as soon as it
		/// is removed, though the list was read (and cached) before.
		#[tokio::test]
		#[serial_test::serial]
		async fn unprocessed_event_writes_show_at_once() -> anyhow::Result<()> {
			crate::types::database::config::use_temp_data_dir();
			let name = format!("unprocessed_events_{}", uuid::Uuid::new_v4().simple());
			let db = Database::new(&name).await?;
			let subject = db.observe_subject("s").await?;
			let aspect_id = db.track_aspect(&subject.id(), "a", &splimes::Resolution::Seconds, None).await?.id();
			let ids = |db: Database| async move { db.get_unprocessed_events(&aspect_id).await.expect("listed").map(|e| e.expect("event").id().clone()).collect::<Vec<_>>().await };

			let first = Event::new(None, "first".to_string(), None, None);
			db.insert_unprocessed_event(&aspect_id, &first).await?;
			assert_eq!(ids(db.clone()).await, vec![first.id().clone()]);

			let second = Event::new(None, "second".to_string(), None, None);
			db.insert_unprocessed_event(&aspect_id, &second).await?;
			assert_eq!(ids(db.clone()).await, vec![first.id().clone(), second.id().clone()], "the cached list hid the new event");

			db.remove_unprocessed_event(&aspect_id, first.id()).await?;
			assert_eq!(ids(db.clone()).await, vec![second.id().clone()], "the cached list kept the removed event");

			db.clear_unprocessed_events(&aspect_id).await?;
			assert!(ids(db.clone()).await.is_empty(), "the cached list survived the clear");
			db.close().await?;
			drop(db);
			Ok(())
		}
	}
}
