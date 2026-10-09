use anyhow::Result;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use splimes::{Resolution, Spline};
use tracing::{debug, info};
use weftdb::{
	database::traits::{DatabaseStructure, Inputs, Outputs}, AspectId, Database, UnbatchedEntry
};

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
/// # Crash consistency (crash-consistency design, S18)
///
/// - **Crash before the dequeue.** The batches are stored before their timestamps are
///   dequeued, so a run that dies in between rebuilds the same windows next time; storing
///   skips every batch already queued, processed or extracted, so the re-run adds no
///   duplicate batches or pattern occurrences, whatever ran in between (an extracted
///   batch is recognised while the extracted-batch record keeps it, 48 hours by default;
///   see `Inputs::remove_extracted_batches`).
/// - **Ingest running at the same time.** Ingest queues a timestamp before its row is
///   inserted and again once it is committed, so this run may read a timestamp whose row
///   is not stored yet and build its windows without it. It therefore dequeues only the
///   queue entries it read, matched on their `queued_at`
///   ([`dequeue_unbatched_entries`](Inputs::dequeue_unbatched_entries)): an entry queued
///   again after the read survives, and the next run batches the landed row. The row is
///   missed only if that ingest also died, or failed to queue it again within its
///   retries, after its commit. A window this run batched from committed rows is rebuilt
///   by the next run, after the second enqueue, into the same batch, which storing skips;
///   only a window spanning committed rows and rows of the same ingest still to come is
///   batched twice, differently (see `batch_capture_measurements`).
/// - **Window alignment.** The windows are aligned on the earliest stored measurement,
///   read from the rows on every run: a value cached before a backfill (by this or another
///   `Database` instance) would put the backfill before the base, where it has no window,
///   and the run would clear it from the queue.
/// - **Timestamps past the stored range.** A window that ends after the latest stored
///   measurement cannot have its last point (`analyze_range` does not extrapolate past
///   the data), so it is skipped without being interpolated, and its entries stay queued
///   until rows reach it. Such timestamps are an append in progress, or the write-ahead
///   entries of one that failed or was abandoned before its rows landed (up to a whole
///   call's worth); without the skip every run interpolated each of their windows only to
///   find it short. The latest measurement is read (uncached) after the entries, so a row
///   committed after that read is queued again after the read too, its entry survives
///   this run's dequeue, and the next run builds its windows.
///
/// # Errors
/// Returns an error if database reads or writes fail while constructing or persisting batches.
pub async fn build_incremental_unprocessed_queue(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, batch_size: usize) -> Result<()> {
	// Check if this is the first run
	if is_first_run(database, aspect).await? {
		info!("First run detected, performing full batch creation");
		return build_unprocessed_queue(database, aspect, resolution, method, batch_size).await;
	}

	// Get the unbatched queue entries; only these are dequeued below
	let unbatched = database.get_unbatched_entries(aspect).await?;
	let unbatched_timestamps: Vec<DateTime<Utc>> = unbatched.iter().map(|entry| entry.data_timestamp).collect();

	if unbatched_timestamps.is_empty() {
		info!("No unbatched measurements, nothing to process");
		return Ok(());
	}

	info!(unbatched_count = unbatched_timestamps.len(), "Processing unbatched measurements");

	// Get the earliest measurement for window alignment, uncached (see above). Ingest
	// queues timestamps before it inserts their rows (write-ahead enqueue), so the queue
	// can hold timestamps whose rows have not landed (or never will), here every one of
	// them: there is nothing to batch yet, and they stay queued.
	let Some(earliest_measurement) = database.get_earliest_measurement_uncached(aspect).await? else {
		info!(unbatched_count = unbatched_timestamps.len(), "Queued timestamps but no stored measurements yet, nothing to batch");
		return Ok(());
	};

	// Calculate which windows are affected
	let affected_windows: std::collections::HashSet<BatchWindow> = calculate_affected_windows(&unbatched_timestamps, resolution, batch_size, earliest_measurement);

	// Only timestamps before the earliest stored row (whose rows have not landed) have no
	// window. Clearing only the entries read above leaves one queued again since then,
	// such as by a backfill whose rows committed meanwhile, for the next run.
	if affected_windows.is_empty() {
		info!("No affected windows found, clearing unbatched queue");
		database.dequeue_unbatched_entries(aspect, &unbatched).await?;
		return Ok(());
	}

	info!(affected_window_count = affected_windows.len(), "Creating batches for affected windows");

	// The latest stored measurement, read after the entries (see the docs above): a window
	// that ends after it is skipped without interpolating it.
	let Some(latest_measurement) = database.get_latest_measurement(aspect).await? else {
		info!(unbatched_count = unbatched_timestamps.len(), "Queued timestamps but no stored measurements yet, nothing to batch");
		return Ok(());
	};

	let database_info = database.get_database_info().await.map_err(|_| anyhow::anyhow!("Database info not available"))?;

	let mut batches_created = 0;
	let mut windows_past_the_data = 0usize;
	// Which of the entries read above a stored batch covers.
	let mut covered = vec![false; unbatched.len()];

	// For each affected window, create a batch
	for window in affected_windows {
		// It cannot reach `batch_size` points yet; its entries stay queued.
		if window.end > latest_measurement {
			windows_past_the_data += 1;
			continue;
		}

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

			// Skipped (still `Ok`) when the batch is already queued, processed or extracted.
			database.insert_unprocessed_batch(aspect, &batch).await?;
			batches_created += 1;

			// Track which entries from unbatched queue are now covered by this batch
			for (entry, covered) in unbatched.iter().zip(covered.iter_mut()) {
				if window.contains(entry.data_timestamp) {
					*covered = true;
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

	// The batches are committed and their timestamps are still queued. A crash here makes
	// the next run rebuild the same windows; the store skips each batch whose hash is
	// already queued, processed or extracted, so that re-run adds no duplicates.
	weftdb::durable::fault::hit(weftdb::durable::FaultPoint::LConsumerBatches).await?;

	// Dequeue the covered entries, each only if it was not queued again since it was read
	let entries_to_dequeue: Vec<UnbatchedEntry> = unbatched.iter().zip(covered).filter_map(|(entry, covered)| covered.then_some(*entry)).collect();

	if !entries_to_dequeue.is_empty() {
		database.dequeue_unbatched_entries(aspect, &entries_to_dequeue).await?;
		info!(dequeued = entries_to_dequeue.len(), "Dequeued processed timestamps");
	}

	info!(batches_created, windows_past_the_data, "Incremental batch creation complete");

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
