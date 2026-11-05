use anyhow::Result;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use splimes::{Resolution, Spline};
use tracing::{debug, info};
use weftdb::{
	database::traits::{DatabaseStructure, Inputs, Outputs}, AspectId, Database, UnbatchedEntry
};

use super::calculate_affected_windows::{calculate_affected_windows, calculate_affected_windows_for_range, BatchWindow};
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
/// For a seal-backed aspect the queue is always empty (slice D-S20: its
/// ingest feeds the seal store's change log instead), so the pending-work
/// term is the change log's: no pending changes, and no batches either
/// way, is a first run.
///
/// # Errors
/// Returns an error if database queries fail.
pub async fn is_first_run(database: &Database, aspect_id: &AspectId) -> Result<bool> {
	let unbatched_count = if database.is_seal_backed(aspect_id).await? { u64::try_from(database.seal_changes(aspect_id).await?.len()).unwrap_or(u64::MAX) } else { database.count_unbatched_measurements(aspect_id).await? };
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
	// Seal-backed aspects feed the change log, not the queue (D-S20).
	if database.is_seal_backed(aspect).await? {
		return build_incremental_from_change_log(database, aspect, resolution, method, batch_size).await;
	}

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

	let affected_window_count = affected_windows.len();
	info!(affected_window_count, "Creating batches for affected windows");

	// The latest stored measurement, read after the entries (see the docs above): a window
	// that ends after it is skipped without interpolating it.
	let Some(latest_measurement) = database.get_latest_measurement(aspect).await? else {
		info!(unbatched_count = unbatched_timestamps.len(), "Queued timestamps but no stored measurements yet, nothing to batch");
		return Ok(());
	};

	// It cannot reach `batch_size` points yet; its entries stay queued.
	let mut windows: Vec<BatchWindow> = affected_windows.into_iter().filter(|window| window.end <= latest_measurement).collect();
	let windows_past_the_data = affected_window_count - windows.len();
	windows.sort_by_key(|window| window.start);

	let built = build_batches_for_windows(database, aspect, resolution, method, batch_size, &windows, earliest_measurement).await?;

	// Which of the entries read above a stored batch covers.
	let mut covered = vec![false; unbatched.len()];
	for (window, _) in &built {
		for (entry, covered) in unbatched.iter().zip(covered.iter_mut()) {
			if window.contains(entry.data_timestamp) {
				*covered = true;
			}
		}
	}
	let batches_created = built.len();
	// One insert for every batch, committed a chunk at a time; each is skipped (still `Ok`)
	// when it is already queued, processed or extracted.
	if !built.is_empty() {
		database.batch_insert_unprocessed_batches(aspect, built.into_iter().map(|(_, batch)| batch).collect()).await?;
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

/// Interpolate `windows` (sorted by start, every one ending at or before
/// the stored data's end) and cut a batch out of each that reaches
/// `batch_size` points: the shared build step of the queue consumer and
/// the change-log consumer.
///
/// Interpolate each run of overlapping windows once and cut its windows
/// out of the result (#505). One `analyze_range` and one insert per
/// window cost ~25 ms and ~13 ms apiece, so a first run over 10k rows
/// spent ~300 s here on ~7.5k windows of 60 points each. A run starts a
/// window's length early, so a window at its head is interpolated from
/// knots on both sides, as the full rebuild's one pass over the whole
/// range interpolates it, and not extrapolated from the knots inside the
/// window alone.
async fn build_batches_for_windows(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, batch_size: usize, windows: &[BatchWindow], earliest_measurement: DateTime<Utc>) -> Result<Vec<(BatchWindow, Batch)>> {
	let database_info = database.get_database_info().await.map_err(|_| anyhow::anyhow!("Database info not available"))?;
	let mut built: Vec<(BatchWindow, Batch)> = Vec::new();
	let mut run_start = 0;
	while run_start < windows.len() {
		let mut run_end = run_start;
		let mut reach = windows[run_start].end;
		while run_end + 1 < windows.len() && windows[run_end + 1].start <= reach {
			run_end += 1;
			reach = reach.max(windows[run_end].end);
		}
		let run = &windows[run_start..=run_end];
		let lead = (run[0].end - run[0].start).max(resolution.step());
		let from = run[0].start.checked_sub_signed(lead).map_or(earliest_measurement, |t| t.max(earliest_measurement));

		let mut point_stream = Outputs::analyze_range(database, aspect, from, reach, *resolution, *method).await?;
		let mut points = Vec::new();
		while let Some(result) = point_stream.next().await {
			points.push(result?);
		}

		for window in run {
			let first = points.partition_point(|p| p.timestamp < window.start);
			let window_points: Vec<_> = points[first..].iter().take_while(|p| p.timestamp <= window.end).collect();
			if window_points.len() < batch_size {
				debug!(
					window_start = %window.start,
					window_end = %window.end,
					points = window_points.len(),
					required = batch_size,
					"Insufficient points for batch, skipping window"
				);
				continue;
			}
			// Take exactly batch_size points
			let measurements: Vec<BatchedMeasurement> = window_points[..batch_size].iter().map(|p| BatchedMeasurement::new((*p).clone())).collect();
			built.push((window.clone(), Batch::new(batch_size, measurements, *resolution, *aspect, database_info.clone())));
		}
		run_start = run_end + 1;
	}
	Ok(built)
}

/// The change-log consumer for a seal-backed aspect (slice D-S20): the
/// counterpart of [`build_incremental_unprocessed_queue`] that reads the
/// seal store's `segment_changes` log instead of the per-timestamp
/// unbatched queue.
///
/// Each log row is one ingest commit (epoch, time span, row count). The
/// windows its span touches are rebuilt exactly as the queue consumer
/// rebuilds a queued timestamp's windows — a span's windows are a
/// superset of its rows' windows, and a window rebuilt over unchanged
/// rows is the same batch, which storing skips — and a row is
/// acknowledged (deleted) only once every window its span touches has
/// been built or has no window at all. A row whose windows are not all
/// built yet (a window past the stored data, or short of `batch_size`
/// points) stays in the log for the next run, as a queued timestamp
/// stays queued. A crash between storing the batches and acknowledging
/// rebuilds the same windows next time, and the dedupe skips them.
///
/// A first run (no batches yet) falls back to the full rebuild, then
/// acknowledges every row it read: the rebuild batched every stored row,
/// which is everything the log names.
///
/// # Errors
/// Returns an error if database reads or writes fail while constructing
/// or persisting batches.
pub async fn build_incremental_from_change_log(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, batch_size: usize) -> Result<()> {
	let changes = database.seal_changes(aspect).await?;
	// No batches yet is a first run however the aspect's rows arrived —
	// including rows sealed before D-S20, which have no change rows: the
	// full rebuild batches every stored row, which is everything the log
	// names, so the rows it read are acknowledged behind it.
	let no_batches_yet = database.count_unprocessed_batches(aspect).await? == 0 && database.count_processed_batches(aspect).await? == 0;
	if changes.is_empty() && !no_batches_yet {
		info!("No seal changes, nothing to process");
		return Ok(());
	}
	info!(change_count = changes.len(), "Processing seal change log");

	if no_batches_yet {
		info!("First run detected, performing full batch creation");
		build_unprocessed_queue(database, aspect, resolution, method, batch_size).await?;
		if let Some(last) = changes.last() {
			database.ack_seal_changes(aspect, last.epoch).await?;
		}
		return Ok(());
	}

	// Window alignment is on the earliest stored measurement, read
	// uncached, as in the queue consumer: the log names spans of stored
	// rows only (a change row commits with its rows), so an aspect with
	// changes always has an earliest row.
	let Some(earliest_measurement) = database.get_earliest_measurement_uncached(aspect).await? else {
		info!(change_count = changes.len(), "Changes but no stored measurements yet, nothing to batch");
		return Ok(());
	};
	let Some(latest_measurement) = database.get_latest_measurement(aspect).await? else {
		info!(change_count = changes.len(), "Changes but no stored measurements yet, nothing to batch");
		return Ok(());
	};

	// The windows each change's span touches, and their union.
	let mut per_change_windows: Vec<std::collections::HashSet<BatchWindow>> = Vec::with_capacity(changes.len());
	let mut affected_windows: std::collections::HashSet<BatchWindow> = std::collections::HashSet::new();
	for change in &changes {
		let (Some(min_ts), Some(max_ts)) = (change.min_ts.and_then(DateTime::from_timestamp_millis), change.max_ts.and_then(DateTime::from_timestamp_millis)) else {
			// A change row always carries its commit's span; a spanless
			// row names no window and is acknowledged with the rest.
			per_change_windows.push(std::collections::HashSet::new());
			continue;
		};
		let windows = calculate_affected_windows_for_range(min_ts, max_ts, resolution, batch_size, earliest_measurement);
		affected_windows.extend(windows.iter().cloned());
		per_change_windows.push(windows);
	}

	let affected_window_count = affected_windows.len();
	let mut windows: Vec<BatchWindow> = affected_windows.into_iter().filter(|window| window.end <= latest_measurement).collect();
	let windows_past_the_data = affected_window_count - windows.len();
	windows.sort_by_key(|window| window.start);

	let built = build_batches_for_windows(database, aspect, resolution, method, batch_size, &windows, earliest_measurement).await?;
	let batches_created = built.len();
	// Acknowledge the longest prefix of changes whose windows were all
	// built (or had no window): acknowledgement deletes through an
	// epoch, so a change that must stay keeps every later one too, even
	// one this run did batch — it is rebuilt and skipped next run. A
	// window past the stored data is never built, so a change touching
	// one stays, as a queued timestamp past the data stays queued.
	let built_windows: std::collections::HashSet<&BatchWindow> = built.iter().map(|(window, _)| window).collect();
	let mut ack_through: Option<u64> = None;
	for (change, change_windows) in changes.iter().zip(&per_change_windows) {
		if change_windows.iter().all(|window| built_windows.contains(window)) {
			ack_through = Some(change.epoch);
		} else {
			break;
		}
	}
	if !built.is_empty() {
		database.batch_insert_unprocessed_batches(aspect, built.into_iter().map(|(_, batch)| batch).collect()).await?;
	}

	// The batches are committed and the changes are still logged. A crash
	// here makes the next run rebuild the same windows; the store skips
	// each batch already queued, processed or extracted.
	weftdb::durable::fault::hit(weftdb::durable::FaultPoint::LConsumerBatches).await?;
	if let Some(epoch) = ack_through {
		database.ack_seal_changes(aspect, epoch).await?;
		info!(acked_through = epoch, "Acknowledged seal changes");
	}

	info!(batches_created, windows_past_the_data, "Change-log batch creation complete");
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
