use anyhow::Result;
use weftdb::{
	database::traits::{DatabaseStructure, Inputs, Outputs}, AspectId, Database
};
use futures::StreamExt;
use splimes::{Resolution, Spline};
use tracing::{debug, info};

use super::calculate_affected_windows::{calculate_affected_windows, BatchWindow};
use crate::{Batch, BatchedMeasurement};

/// Checks if this is the first run for an aspect (no batches exist yet).
///
/// First run is detected when:
/// - There are no unbatched measurements in the queue, AND
/// - There are no existing unprocessed batches, AND
/// - There are no existing processed batches
///
/// This indicates a fresh start where all measurements should be processed.
///
/// # Errors
/// Returns an error if database queries fail.
pub async fn is_first_run(database: &Database, aspect_id: &AspectId) -> Result<bool> {
	let unbatched_count = database.count_unbatched_measurements(aspect_id).await?;
	let unprocessed_count = database.count_unprocessed_batches(aspect_id).await?;
	let processed_count = database.count_processed_batches(aspect_id).await?;

	Ok(unbatched_count == 0 && unprocessed_count == 0 && processed_count == 0)
}

/// Builds incremental sliding-window batches only for measurements that haven't been batched yet.
///
/// This function queries the unbatched measurements queue and creates batches only for the
/// windows affected by those new measurements. After batch creation, the measurements are
/// dequeued from the unbatched queue.
///
/// If this is the first run (no existing batches), it falls back to the full rebuild.
///
/// # Errors
/// Returns an error if database reads or writes fail while constructing or persisting batches.
pub async fn build_incremental_unprocessed_queue(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, batch_size: usize) -> Result<()> {
	// Check if this is the first run
	if is_first_run(database, aspect).await? {
		info!("First run detected, performing full batch creation");
		return build_unprocessed_queue(database, aspect, resolution, method, batch_size).await;
	}

	// Get unbatched measurements
	let unbatched_timestamps = database.get_unbatched_measurements(aspect).await?;

	if unbatched_timestamps.is_empty() {
		info!("No unbatched measurements, nothing to process");
		return Ok(());
	}

	info!(unbatched_count = unbatched_timestamps.len(), "Processing unbatched measurements");

	// Get the earliest measurement for window alignment
	let earliest_measurement = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
	let _latest_measurement = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;

	// Calculate which windows are affected
	let affected_windows: std::collections::HashSet<BatchWindow> = calculate_affected_windows(&unbatched_timestamps, resolution, batch_size, earliest_measurement);

	if affected_windows.is_empty() {
		info!("No affected windows found, clearing unbatched queue");
		database.dequeue_unbatched_measurements(aspect, &unbatched_timestamps).await?;
		return Ok(());
	}

	info!(affected_window_count = affected_windows.len(), "Creating batches for affected windows");

	let database_info = database.get_database_info().await.map_err(|_| anyhow::anyhow!("Database info not available"))?;

	let mut batches_created = 0;
	let mut timestamps_to_dequeue = Vec::new();

	// For each affected window, create a batch
	for window in affected_windows {
		// Fetch points within this window
		let mut point_stream = Outputs::analyze_range(database, aspect, window.start, window.end, *resolution, *method).await?;

		let mut window_points = Vec::new();
		while let Some(result) = point_stream.next().await {
			let point = result?;
			window_points.push(point);
		}

		// Only create a batch if we have enough points
		if window_points.len() >= batch_size {
			// Take exactly batch_size points
			let batch_points = &window_points[0..batch_size];
			let measurements: Vec<BatchedMeasurement> = batch_points.iter().map(|p| BatchedMeasurement::new(p.clone())).collect();

			let batch = Batch::new(batch_size, measurements, *resolution, *aspect, database_info.clone());

			database.insert_unprocessed_batch(aspect, &batch).await?;
			batches_created += 1;

			// Track which timestamps from unbatched queue are now covered by this batch
			for ts in &unbatched_timestamps {
				if window.contains(*ts) {
					timestamps_to_dequeue.push(*ts);
				}
			}
		} else {
			debug!(
				window_start = %window.start,
				window_end = %window.end,
				points = window_points.len(),
				required = batch_size,
				"Insufficient points for batch, skipping window"
			);
		}
	}

	// Dequeue the processed timestamps (deduplicated)
	timestamps_to_dequeue.sort();
	timestamps_to_dequeue.dedup();

	if !timestamps_to_dequeue.is_empty() {
		database.dequeue_unbatched_measurements(aspect, &timestamps_to_dequeue).await?;
		info!(dequeued = timestamps_to_dequeue.len(), "Dequeued processed timestamps");
	}

	info!(batches_created, "Incremental batch creation complete");

	Ok(())
}

/// Builds sliding-window batches from the provided aspect and stores them in the database.
///
/// # Errors
/// Returns an error if database reads or writes fail while constructing or persisting batches.
///
/// # Panics
/// Panics if the internally generated sliding window ever yields a batch with a size different
/// from the requested `batch_size`.
pub async fn build_unprocessed_queue(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, batch_size: usize) -> Result<()> {
	let start_time = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
	let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;

	// Collect all points first to create sliding window batches
	let mut point_stream = Outputs::analyze_range(database, aspect, start_time, end_time, *resolution, *method).await?;
	let mut all_points = Vec::new();
	while let Some(result) = point_stream.next().await {
		let point = result?;
		all_points.push(point);
	}

	info!(total_points = all_points.len(), batch_size, %start_time, %end_time, "Collected points for batch creation");

	// Create sliding window batches (overlapping)
	if batch_size > 0 && all_points.len() >= batch_size {
		let database_info = database.get_database_info().await.map_err(|_| anyhow::anyhow!("Database info not available"))?;

		let total_batches = all_points.len() - batch_size + 1;
		let report_interval = std::cmp::max(1, total_batches / 10); // Report every 10% of total batches (at least every batch)
		info!(total_batches, "Creating sliding window batches");

		// Collect all batches first, then store them in bulk for better performance
		let mut batches = Vec::with_capacity(total_batches);

		// Create overlapping sliding window batches - each batch has exactly batch_size measurements
		for i in 0..=(all_points.len() - batch_size) {
			let window = &all_points[i..i + batch_size];
			assert_eq!(window.len(), batch_size, "Window should always have exactly batch_size elements");

			let measurements = window.iter().map(|p| BatchedMeasurement::new(p.clone())).collect();
			let batch = Batch::new(batch_size, measurements, *resolution, *aspect, database_info.clone());
			batches.push(batch);

			// Progress indicator for batch creation
			if (i + 1) % report_interval == 0 || i + 1 == total_batches {
				debug!(created = i + 1, total = total_batches, "Batch creation progress");
			}
		}

		info!(batch_count = batches.len(), "Storing batches in database");
		// Store all batches at once using bulk insert
		database.batch_insert_unprocessed_batches(aspect, batches).await?;
	}

	// Note: No longer using global queue length since batches are stored in database
	info!("Batches stored in database successfully");

	Ok(())
}
