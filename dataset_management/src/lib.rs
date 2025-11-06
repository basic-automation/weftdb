#![warn(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions, clippy::used_underscore_binding, clippy::similar_names, clippy::module_name_repetitions, clippy::module_inception, clippy::cast_precision_loss)]

use std::{collections::HashMap, sync::LazyLock};

use anyhow::Result;
use bigdecimal::{BigDecimal, FromPrimitive, Zero};
use chrono::Datelike;
use database::{
	database::traits::{AspectStructure, DatabaseStructure, Outputs}, AspectId, BatchId, Database, DictionaryId, Resolution
};

use futures::StreamExt;
use rayon::prelude::*;
use splimes::Spline;
use tokio::sync::Mutex;
pub use types::*;

pub mod batch_utils;
#[cfg(test)]
mod debug_batch_test;
#[cfg(test)]
mod memory_test;
pub mod types;

#[cfg(test)]
mod pattern_fix_test;

pub const BATCH_SIZE: [usize; 1] = [100];
pub static DEFAULT_ERROR_RATE: LazyLock<database::Distance> = LazyLock::new(|| {
	database::Distance::new(
		BigDecimal::zero(),
		splimes::Resolution::Seconds, // Use seconds as the canonical unit for error rates
	)
});

static SIGNALS_QUEUE: LazyLock<Mutex<Signals>> = LazyLock::new(|| Mutex::new(Signals::new()));

/// Builds a processed batch queue from unprocessed batches in the database.
///
/// This function retrieves unprocessed batches, processes them in parallel,
/// and marks them as processed in the database.
///
/// # Errors
///
/// Returns an error if:
/// - Database operations fail (getting batches, marking as processed)
/// - Batch processing fails
pub async fn build_processed_batch_queue(database: &Database, aspect_id: &database::AspectId) -> Result<()> {
	let unprocessed_batches = database.get_unprocessed_batches(aspect_id).await?;

	if unprocessed_batches.is_empty() {
		println!("No unprocessed batches found in database");
		return Ok(());
	}

	println!("Processing {} unprocessed batches from database", unprocessed_batches.len());

	// Store batch IDs before processing (since processing modifies the batch but we need original IDs)
	let batch_ids: Vec<BatchId> = unprocessed_batches.iter().map(|batch| *batch.batch_id()).collect();

	let batch_hashes: Vec<_> = unprocessed_batches.iter().filter_map(|batch| batch.batch_hash().cloned()).collect();

	let mut processed_batches = unprocessed_batches;
	let total_batches = processed_batches.len();

	// Process batches in chunks with progress reporting
	let chunk_size = 1000;
	let mut processed_count = 0;

	for chunk in processed_batches.chunks_mut(chunk_size) {
		chunk.par_iter_mut().for_each(|batch| {
			let _ = batch.process();
		});

		processed_count += chunk.len();
		println!("Processed {processed_count} / {total_batches} batches");
	}

	// Mark batches as processed using the original identifiers
	let has_batch_ids = !batch_ids.is_empty();

	println!("Marking batches as processed in database");
	let total_batch_ids = batch_ids.len();
	for (i, batch_id) in batch_ids.iter().enumerate() {
		database.mark_batch_processed_by_id(&batch_id.to_string(), aspect_id).await?;

		if (i + 1) % 1000 == 0 || (i + 1) == total_batch_ids {
			println!("Marked {} / {} batch IDs as processed", i + 1, total_batch_ids);
		}
	}

	if !has_batch_ids {
		// Only use hash if no IDs available
		let total_batch_hashes = batch_hashes.len();
		for (i, batch_hash) in batch_hashes.iter().enumerate() {
			database.mark_batch_processed_by_hash(batch_hash, aspect_id).await?;

			if (i + 1) % 1000 == 0 || (i + 1) == total_batch_hashes {
				println!("Marked {} / {} batch hashes as processed", i + 1, total_batch_hashes);
			}
		}
	}

	Ok(())
}

/// Builds a patterns queue from processed batches in the database.
///
/// This function extracts patterns from processed batches and imports them into the dictionary
/// with merge logic applied before storing to the database.
///
/// # Errors
///
/// Returns an error if:
/// - Database operations fail (getting batches, storing patterns, dequeuing batches)
/// - Pattern creation fails due to invalid batch data
/// - Dictionary import fails during pattern merging
///
/// # Panics
///
/// This function will panic if active measurements exist in a batch but have no timestamps,
/// which should not happen under normal circumstances.
pub async fn build_patterns_queue(database: &Database, aspect_id: &database::AspectId, dictionary: &mut Dictionary) -> Result<()> {
	// Get processed batches from database queue
	let batches = database.get_processed_batches_queue(aspect_id).await?;

	if batches.is_empty() {
		println!("No processed batches found in database queue");
		return Ok(());
	}

	println!("Processing {} batches from database queue into patterns", batches.len());

	let mut processed_batch_ids = Vec::new();
	let mut processed_count = 0;
	let total_batches = batches.len();

	for batch in &batches {
		processed_count += 1;

		// Progress reporting every 1000 batches
		if processed_count % 1000 == 0 || processed_count == total_batches {
			println!("Processed {processed_count} / {total_batches} batches into patterns");
		}
		// Generate a new pattern ID for this batch
		let pattern_id = PatternID::new();

		// Get the first and last timestamps from active measurements
		let active_measurements: Vec<_> = batch.measurements().iter().filter(|m| m.is_active()).collect();

		if active_measurements.is_empty() {
			continue; // Skip batches with no active measurements
		}

		let beginning = active_measurements.iter().map(|m| m.get_measurement_timestamp()).min().copied().unwrap();
		let end = active_measurements.iter().map(|m| m.get_measurement_timestamp()).max().copied().unwrap();

		// Create an occurrence for this pattern
		let occurrence = Occurrence::new(batch.metadata.aspect, batch.metadata.resolution, batch.metadata.size, batch.metadata.database_info.clone(), pattern_id, beginning, end);

		// Extract relatives from active measurements that have analysis, or generate from vectors
		let relatives: Vec<database::Relative> = active_measurements.iter().find_map(|measurement| measurement.analysis()?.relative()).map_or_else(
			|| {
				// If no analysis data, generate relatives from measurement vectors
				let vectors: Vec<_> = active_measurements.iter().filter_map(|measurement| measurement.vector()).collect();
				if vectors.is_empty() {
					Vec::new()
				} else {
					// Calculate max_x and max_y from all vectors
					let max_x = vectors.iter().map(|v| v.location()).max().cloned().unwrap_or_else(|| BigDecimal::from(0));
					let max_y = vectors.iter().map(|v| v.amplitude()).max().cloned().unwrap_or_else(|| BigDecimal::from(0));

					// Create relatives from vectors
					vectors.iter().map(|vector| database::Relative::new((*vector).clone(), max_x.clone(), max_y.clone())).collect()
				}
			},
			|_first_relative| active_measurements.iter().filter_map(|measurement| measurement.analysis()?.relative().cloned()).collect(),
		); // Only create pattern if we have relatives
		if !relatives.is_empty() {
			let pattern = Pattern::new(pattern_id, vec![occurrence], relatives);

			// Import pattern into dictionary (this applies merge logic)
			dictionary.import_pattern(pattern)?;

			// Track this batch for removal from database queue
			processed_batch_ids.push(batch.clone());
		}
	}

	// Get all patterns from dictionary after merging
	let merged_patterns: Vec<Pattern> = dictionary.patterns().to_vec(); // Store merged patterns in database
	if !merged_patterns.is_empty() {
		// Store patterns in the specified dictionary - handle case where dictionary schema doesn't exist
		match database.store_patterns_in_dictionary(&merged_patterns, dictionary.name(), aspect_id).await {
			Ok(()) => {
				println!("Stored {} patterns in database dictionary '{}' (after merging {} batches)", merged_patterns.len(), dictionary.name(), batches.len());
			}
			Err(e) => {
				println!("Warning: Failed to store patterns in database dictionary '{}': {}", dictionary.name(), e);
				println!("Patterns are still available in memory dictionary with {} patterns", merged_patterns.len());
			}
		}
	}

	// Remove processed batches from database queue
	for batch in processed_batch_ids {
		database.dequeue_processed_batch(&batch).await?;
	}

	Ok(())
}

/// Optimized dictionary loading with memory-aware parallelism using rayon
/// Dynamically calculates batch sizes based on available memory and removes patterns as processed
///
/// # Errors
///
/// Returns an error if:
/// - Database operations fail (getting patterns, dequeuing patterns)
/// - Pattern import fails during dictionary loading
/// - Memory calculation fails
pub async fn load_dictionary(database: &Database, aspect_id: &database::AspectId, dictionary: &mut Dictionary) -> Result<()> {
	// Check if dictionary metadata exists, create it if not
	match database.get_dictionary_metadata(dictionary.name()).await {
		Ok(Some(_)) => {
			// Dictionary metadata exists, proceed normally
		}
		Ok(None) => {
			// Dictionary metadata doesn't exist, create it
			let constraints_json = serde_json::json!({
				"steps": dictionary.constraints().steps().as_ref().map(|s| serde_json::json!({
					"count": s.count(),
					"interpolation": s.interpolation()
				})),
				"variabilities": dictionary.constraints().variabilities().as_ref().map(|v| serde_json::json!(v))
			});

			// Store dictionary metadata
			database.store_dictionary_metadata(dictionary.name(), dictionary.description(), &constraints_json).await?;
		}
		Err(e) => {
			// If dictionary metadata retrieval fails, try to create it anyway
			println!("Warning: Failed to check dictionary metadata ({e}), attempting to create");
			let constraints_json = serde_json::json!({
				"steps": dictionary.constraints().steps().as_ref().map(|s| serde_json::json!({
					"count": s.count(),
					"interpolation": s.interpolation()
				})),
				"variabilities": dictionary.constraints().variabilities().as_ref().map(|v| serde_json::json!(v))
			});

			// Try to store dictionary metadata - if this fails, the database might not support it yet
			if let Err(store_err) = database.store_dictionary_metadata(dictionary.name(), dictionary.description(), &constraints_json).await {
				println!("Warning: Failed to store dictionary metadata: {store_err}");
				println!("Continuing without dictionary metadata (dictionary will still function)");
			}
		}
	}

	// Get processed patterns from the specific dictionary - handle case where dictionary doesn't exist
	let patterns = match database.get_patterns_from_dictionary(dictionary.name(), aspect_id).await {
		Ok(patterns) => patterns,
		Err(e) => {
			println!("Warning: Failed to get patterns from dictionary '{}': {}", dictionary.name(), e);
			println!("Dictionary may not exist yet - returning empty pattern list");
			Vec::new()
		}
	};
	let pattern_count = patterns.len();

	let start_time = std::time::Instant::now();
	println!("Loading {pattern_count} patterns into dictionary with memory-aware batching");

	// For smaller pattern sets, process directly from the queue
	if pattern_count <= 1000 {
		for pattern in &patterns {
			dictionary.import_pattern(pattern.clone())?;
		}
	} else {
		// Get available memory information
		let available_memory_mb = get_available_memory_mb();
		println!("Available memory: {available_memory_mb} MB");

		// Calculate safe batch size based on available memory
		// Assume each pattern uses ~1MB when processed (conservative estimate)
		// Use only 25% of available memory for safety
		let safe_memory_mb = available_memory_mb / 4;
		let estimated_pattern_size_mb = 1; // Conservative estimate per pattern
		let memory_based_batch_size = (safe_memory_mb / estimated_pattern_size_mb).clamp(10, 200);

		let cpu_count = num_cpus::get();
		let optimal_batch_size = (memory_based_batch_size / cpu_count).max(5);

		println!("Using batch size: {optimal_batch_size} patterns per thread, {memory_based_batch_size} total per chunk");

		// Process in memory-aware chunks, removing from database queue as processed
		let mut processed_patterns = Vec::new();

		for chunk in patterns.chunks(memory_based_batch_size) {
			// Release the lock while processing
			let chunk_patterns: Vec<Dictionary> = chunk
				.chunks(optimal_batch_size)
				.collect::<Vec<_>>()
				.par_iter()
				.map(|chunk_patterns| {
					let mut chunk_dict = Dictionary::new(format!("Chunk Dictionary {}", uuid::Uuid::new_v4()), "Temporary dictionary for parallel processing".to_string(), dictionary.constraints().clone());
					for pattern in *chunk_patterns {
						if let Err(e) = chunk_dict.import_pattern(pattern.clone()) {
							eprintln!("Failed to import pattern in chunk: {e}");
						}
					}

					chunk_dict
				})
				.collect();

			// Merge chunk dictionaries
			for chunk_dict in chunk_patterns {
				dictionary.merge_dictionary(chunk_dict)?;
			}

			// Track processed patterns for removal from database queue
			processed_patterns.extend_from_slice(chunk);

			let remaining = pattern_count - processed_patterns.len();
			if remaining > 0 {
				// println!("Processed {} patterns, {} remaining", pattern_count - remaining, remaining);
			}

			// Yield to prevent blocking other tasks
			tokio::task::yield_now().await;
		}

		// Patterns are now stored in dictionary databases and should persist,
		// so we don't remove them after loading into memory
		// for pattern in processed_patterns {
		//     database.dequeue_processed_pattern(&pattern).await?;
		// }
	}

	let final_elapsed = start_time.elapsed();
	let final_rate = pattern_count as f64 / final_elapsed.as_secs_f64();
	println!("Dictionary loading completed. Processed {pattern_count} patterns in {final_elapsed:?} ({final_rate:.1} patterns/sec)");
	Ok(())
}

/// Get available system memory in MB
/// Returns a conservative estimate to prevent memory exhaustion
fn get_available_memory_mb() -> usize {
	#[cfg(target_os = "windows")]
	{
		use std::mem;

		#[repr(C)]
		struct MemoryStatusEx {
			dw_length: u32,
			dw_memory_load: u32,
			ull_total_phys: u64,
			ull_avail_phys: u64,
			ull_total_page_file: u64,
			ull_avail_page_file: u64,
			ull_total_virtual: u64,
			ull_avail_virtual: u64,
			ull_avail_extended_virtual: u64,
		}

		extern "system" {
			fn GlobalMemoryStatusEx(lpBuffer: *mut MemoryStatusEx) -> i32;
		}

		let mut mem_status = MemoryStatusEx { dw_length: u32::try_from(mem::size_of::<MemoryStatusEx>()).unwrap(), dw_memory_load: 0, ull_total_phys: 0, ull_avail_phys: 0, ull_total_page_file: 0, ull_avail_page_file: 0, ull_total_virtual: 0, ull_avail_virtual: 0, ull_avail_extended_virtual: 0 };

		unsafe {
			if GlobalMemoryStatusEx(&raw mut mem_status) != 0 {
				return (mem_status.ull_avail_phys / (1024 * 1024)) as usize;
			}
		}
	}

	#[cfg(target_os = "linux")]
	{
		if let Ok(contents) = std::fs::read_to_string("/proc/meminfo") {
			for line in contents.lines() {
				if line.starts_with("MemAvailable:") {
					if let Some(value) = line.split_whitespace().nth(1) {
						if let Ok(kb) = value.parse::<usize>() {
							return kb / 1024; // Convert KB to MB
						}
					}
				}
			}
		}
	}

	#[cfg(target_os = "macos")]
	{
		// Fallback for macOS - could implement using sysctl if needed
		return 4096; // 4GB conservative fallback
	}

	// Conservative fallback if we can't determine memory
	2048 // 2GB conservative fallback
}

/// Detects 5% price increases from month start to month end
///
/// This function analyzes price movements to detect when the price rises 5% or more
/// from the beginning of a calendar month to the end of that same month. Each detected
/// increase creates an event with a manifestation at the end of the month when the
/// 5% threshold is confirmed.
///
/// This is useful for determining if buying at the start of each month would be profitable.
///
/// # Parameters
/// - `database`: Database instance to query measurements from and store events
/// - `aspect`: The aspect ID to analyze for price movements
/// - `resolution`: The resolution for data analysis
/// - `method`: The spline interpolation method to use
///
/// # Algorithm
/// 1. Retrieve all measurements for the aspect within the date range
/// 2. Group measurements by calendar month
/// 3. For each month, find the first price (beginning) and last price (end)
/// 4. Calculate percentage increase from start to end of month
/// 5. If increase is 5% or higher, create an event at the end of the month
/// 6. Store events in the database
///
/// # Errors
///
/// Returns an error if:
/// - No earliest or latest measurements are found
/// - Database operations fail (streaming data, getting database info, storing events)
///
/// # Panics
///
/// This function will panic if the 5% threshold value (0.05) cannot be converted to `BigDecimal`,
/// which should not happen under normal circumstances.
pub async fn create_event_and_manifestations(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline) -> Result<()> {
	use std::collections::HashMap;
	let start_time: chrono::DateTime<chrono::Utc> = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
	let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;

	// Use optimized bulk analysis to get all points
	let mut point_stream = Outputs::analyze_range(database, *aspect, start_time, end_time, *resolution, *method).await?;

	let mut points = Vec::new();
	while let Some(result) = point_stream.next().await {
		let point = result?;
		points.push(point);
	}

	// Sort points by timestamp to ensure proper chronological order
	points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	let mut event = Event::new(format!("5% Monthly Price Increase - {aspect}"), Some(format!("Detects when price increases 5% or more from start to end of month for aspect {aspect}")));
	let threshold_percentage = BigDecimal::from_f64(0.05).unwrap(); // 5% threshold

	// Group points by month and analyze each month
	let mut months_data: HashMap<(i32, u32), Vec<&splimes::Point>> = HashMap::new();

	// Group all points by year and month
	for point in &points {
		let year = point.timestamp.year();
		let month = point.timestamp.month();
		months_data.entry((year, month)).or_default().push(point);
	}

	// Analyze each month for 5% increases
	for ((_year, _month), month_points) in months_data {
		if month_points.len() < 2 {
			continue; // Need at least 2 points to compare start and end
		}

		// Sort points within the month by timestamp
		let mut sorted_month_points = month_points;
		sorted_month_points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

		let start_price = &sorted_month_points[0].value;
		let end_price = &sorted_month_points[sorted_month_points.len() - 1].value;
		let end_timestamp = sorted_month_points[sorted_month_points.len() - 1].timestamp;

		// Calculate percentage increase from start to end of month
		// Formula: (end_price - start_price) / start_price
		let price_diff = end_price - start_price;
		let percentage_increase = &price_diff / start_price;

		// If increase is 5% or more, create a manifestation
		if percentage_increase >= threshold_percentage {
			let database_info = database.get_database_info().await.expect("Database info should be available");
			let manifestation = Manifestation::new(
				database_info.id().as_uuid(),
				sorted_month_points[0].timestamp, // Start of the month
				end_timestamp,                    // End of the month
			);

			event.add_manifestation(manifestation);
		}
	}

	// Store all detected events in the database
	if !event.manifestations().is_empty() {
		database.store_event(&event).await?;
		println!("Stored event '{}' with {} manifestations in database", event.name(), event.manifestations().len());
	}

	Ok(())
}

/// Creates correlations between events and patterns for signal generation.
///
/// This function correlates all events with all patterns in the dictionary,
/// creating the foundation for signal prediction.
///
/// # Errors
///
/// Returns an error if:
/// - Database operations fail (getting events, marking events processed)
/// - Correlation creation fails
pub async fn create_correlations_for_events(database: &Database, dictionary: &Dictionary, aspect_id: &AspectId) -> Result<()> {
	// Get patterns directly from database (not from dictionary object which may have filtered patterns)
	let patterns = database.get_patterns_from_dictionary(dictionary.name(), aspect_id).await?;
	let events = database.get_unprocessed_events().await?;
	let aspect = database.get_aspect(*aspect_id).await?;

	// Exit early if no patterns or events to correlate
	if patterns.is_empty() || events.is_empty() {
		println!("No patterns or events found to correlate");
		return Ok(());
	}

	println!("Correlating {} events with {} patterns", events.len(), patterns.len());

	// For each event, correlate with all patterns
	for event in &events {
		for pattern in &patterns {
			// Assign constant to local variable before borrowing to avoid clippy warning
			let error_rate = HashMap::new();

			let correlation = Correlation::new(None, DictionaryId::from_uuid(pattern.occurrences()[0].database_info().id().as_uuid()), aspect.subject_id(), aspect_id, pattern.id(), event.id().clone(), error_rate, pattern.occurrences().clone());
			database.store_correlation(&correlation).await?;
		}
	}

	println!("Pattern-Event correlation completed successfully");
	Ok(())
}

/// Creates prediction signals from correlations and events.
///
/// This function generates signals for each correlation-manifestation pair,
/// representing predictions of when events will occur.
///
/// # Errors
///
/// Returns an error if:
/// - Database operations fail (getting events)
/// - Time difference calculations fail
/// - Signal creation fails
pub async fn create_signals(database: &Database, aspect_id: &AspectId) -> Result<()> {
	// Get correlations and events from database
	let correlations = database.get_correlations(aspect_id).await?;
	let events = database.get_unprocessed_events().await?;

	// Exit early if no correlations or events to process
	if correlations.is_empty() || events.is_empty() {
		println!("No correlations or events found to create signals");
		return Ok(());
	}

	println!("Creating signals from {} correlations and {} events", correlations.len(), events.len());

	let mut signals_count = 0;

	// Process each correlation to create signals
	for correlation in &correlations {
		// Get the corresponding event
		if let Some(event) = events.iter().find(|e| e.id() == correlation.event_id()) {
			// For each manifestation of the event, create signals based on pattern occurrences
			for manifestation in event.manifestations().values() {
				// Create signals for each pattern occurrence in the correlation
				for occurrence in correlation.occurrences() {
					// Calculate the distance between the event manifestation and pattern occurrence
					let manifestation_start = *manifestation.start();
					let manifestation_midpoint = manifestation.midpoint();
					let manifestation_end = *manifestation.end();

					let occurrence_end = *occurrence.end(); // Calculate time difference using the pattern's resolution from the occurrence
					     // For predictive signals, we want the distance from when the pattern ENDS to when the event occurs
					     // difference(minuend, subtrahend) returns minuend - subtrahend
					     // So difference(manifestation, occurrence_end) gives us manifestation - occurrence_end (positive forward in time)
					let pattern_resolution = *occurrence.resolution();
					let time_diff_start = pattern_resolution.difference(&manifestation_start, &occurrence_end)?;
					let time_diff_midpoint = pattern_resolution.difference(&manifestation_midpoint, &occurrence_end)?;
					let time_diff_end = pattern_resolution.difference(&manifestation_end, &occurrence_end)?;

					// Debug: show first few signal distances
					if signals_count < 5 {
						println!("DEBUG Signal creation {}: occurrence.beginning={}, occurrence.end={}, manifestation.start={}, manifestation.end={}, time_diff_start={} minutes", signals_count, occurrence.beginning(), occurrence_end, manifestation_start, manifestation.end(), time_diff_start);
					}

					// Skip if pattern occurrence overlaps with or happens after the event manifestation
					// For predictive signals, the pattern must END before the event STARTS
					if occurrence_end >= manifestation_start {
						if signals_count < 5 {
							println!("  -> SKIPPING: Pattern ends at or after event starts (causality violation)");
						}
						continue;
					}

					let distance_start = database::Distance::new(BigDecimal::from(time_diff_start), pattern_resolution);
					let distance_midpoint = database::Distance::new(BigDecimal::from(time_diff_midpoint), pattern_resolution);
					let distance_end = database::Distance::new(BigDecimal::from(time_diff_end), pattern_resolution);

					// Create different types of signals based on the relationship
					// The manifestation_date for the signal should be when the pattern ENDS (the starting point of prediction)
					// The distance then points forward to when the event manifestation occurs
					let signal_types = vec![
						(SignalType::Custom("PredictStart".to_string()), distance_start),  // Pattern predicts event start
						(SignalType::Custom("PredictMid".to_string()), distance_midpoint), // Pattern predicts event midpoint
						(SignalType::Custom("PredictEnd".to_string()), distance_end),      // Pattern predicts event end
					];

					for (signal_type, distance) in signal_types {
						// Use the actual manifestation ID from the event
						let manifestation_id = manifestation.id().clone();

						// The signal's manifestation_date is when the pattern ends (the starting point for prediction)
						// From this point, the distance tells us how far in the future the event will occur
						let signal = Signal::new(correlation.id().clone(), manifestation_id, correlation.event_id().clone(), occurrence_end, signal_type, distance.clone());

						// Add signal to the global signals queue
						SIGNALS_QUEUE.lock().await.insert(signal);
						signals_count += 1;
					}
				}
			}
		}
	}

	println!("Created {signals_count} signals successfully");
	Ok(())
}

/// Filters out expired signals based on event resolution times.
///
/// Signals expire when their predicted event has already occurred.
/// This function removes expired signals and applies error correction.
///
/// # Errors
///
/// Returns an error if:
/// - Database operations fail (getting events, correlations, updating correlations)
/// - Signal removal or error correction fails
/// - Time calculations or conversions fail
pub async fn filter_expired_signals(database: &Database, aspect_id: &AspectId) -> Result<()> {
	filter_expired_signals_at_time(database, aspect_id, None).await
}

/// Filters out expired signals based on event resolution times at a specific time.
///
/// This is the implementation function for `filter_expired_signals` that allows
/// specifying a custom current time (useful for testing).
///
/// # Errors
///
/// Returns an error if:
/// - Database operations fail (getting events, correlations, updating correlations)
/// - Signal removal or error correction fails
/// - Time calculations or conversions fail
pub async fn filter_expired_signals_at_time(database: &Database, aspect_id: &AspectId, current_time: Option<chrono::DateTime<chrono::Utc>>) -> Result<()> {
	use chrono::Utc;

	let current_time = current_time.unwrap_or_else(Utc::now);
	let mut signals_lock = SIGNALS_QUEUE.lock().await;

	if signals_lock.is_empty() {
		println!("No signals found to filter");
		return Ok(());
	}

	let initial_count = signals_lock.len();
	println!("Starting filter_expired_signals at query_time={} UTC...", current_time.format("%Y-%m-%d %H:%M:%S"));
	println!("Filtering expired signals from {initial_count} total signals");

	// Debug: show sample signals before filtering
	let sample_signals: Vec<_> = signals_lock.values().take(5).collect();
	for (idx, sig) in sample_signals.iter().enumerate() {
		println!("DEBUG Sample signal {}: manifestation_date={}, distance={} {:?}, signal_type={:?}", idx, sig.manifestation_date(), sig.distance().value(), sig.distance().units(), sig.signal_type());
	}

	// Get events and correlations once to avoid database locks during processing
	let events_lock = database.get_unprocessed_events().await?;
	let correlations = database.get_correlations(aspect_id).await?;

	// Collect signals to remove
	let mut signals_to_remove = Vec::new();

	// Iterate through all signals to check for expiration
	for signal in signals_lock.values() {
		// Find the event this signal is predicting by matching correlation IDs
		let mut signal_event_id = None;

		// Look through correlations to find which event this signal belongs to
		for correlation in &correlations {
			if correlation.id() == signal.correlation_id() {
				signal_event_id = Some(correlation.event_id().clone());
				break;
			}
		}

		// If we found the event, check for expiration
		if let Some(event_id) = signal_event_id {
			if let Some(event) = events_lock.iter().find(|e| *e.id() == event_id) {
				// Find the manifestation this signal was created from (the baseline)
				if let Some(base_manifestation) = event.manifestations().values().find(|m| m.id() == signal.manifestation_id()) {
					// Find the next manifestation after the baseline that this signal is predicting
					let mut future_manifestations: Vec<_> = event.manifestations().values().filter(|manifestation| manifestation.start() > base_manifestation.end()).collect(); // Sort by start date to find the very next manifestation
					future_manifestations.sort_by(|a, b| a.start().cmp(b.start()));

					// If there's a next manifestation, check if we've passed the prediction point
					if let Some(next_manifestation) = future_manifestations.first() {
						// Determine the prediction point based on signal type
						let prediction_point = match signal.signal_type() {
							SignalType::Custom(ref type_name) if type_name == "PredictStart" => *next_manifestation.start(),
							SignalType::Custom(ref type_name) if type_name == "PredictMid" => next_manifestation.midpoint(),
							SignalType::Custom(ref type_name) if type_name == "PredictEnd" => *next_manifestation.end(),
							SignalType::Custom(_) => next_manifestation.midpoint(), // Default to midpoint for unknown types
						};

						// Signal expires when current_time reaches or passes the prediction point
						if current_time >= prediction_point {
							// Signal has expired - we've reached the time it was predicting
							signals_to_remove.push((signal.correlation_id().clone(), signal.manifestation_id().clone(), signal.signal_type().clone(), prediction_point));
						}
					}
					// If there are no future manifestations, the signal doesn't expire yet
				} else {
					// If we can't find the baseline manifestation this signal was created from, remove it as invalid
					// Use current time as fallback resolution time for invalid signals
					signals_to_remove.push((signal.correlation_id().clone(), signal.manifestation_id().clone(), signal.signal_type().clone(), current_time));
				}
			}
		}
	}

	// Correlations loaded from database, no lock to release

	println!("Removing {} expired or invalid signals", signals_to_remove.len());

	// Remove expired signals with error correction
	let mut removed_count = 0;
	let mut correlations_to_update = Vec::new();

	for (correlation_id, manifestation_id, signal_type, resolution_time) in signals_to_remove {
		// Find the correlation from our pre-loaded set
		if let Some(mut correlation) = correlations.iter().find(|c| c.id() == &correlation_id).cloned() {
			// Use error correction when removing the signal
			if let Ok(Some(_removed_signal)) = signals_lock.remove_with_error_correction(&mut correlation, &manifestation_id, &signal_type, resolution_time) {
				correlations_to_update.push(correlation);
				removed_count += 1;
			}
		}
	}

	// Batch update all modified correlations to avoid database lock conflicts
	for correlation in correlations_to_update {
		database.update_correlation(&correlation).await?;
	}

	let remaining_count = signals_lock.len();

	// Release locks
	drop(signals_lock);

	println!("Filtered {removed_count} expired signals. {remaining_count} signals remaining from {initial_count} initial signals");

	Ok(())
}

#[cfg(test)]
mod tests {

	use ::database::database::traits::{AspectStructure, Inputs};
	use anyhow::bail;
	use batch_utils::*;
	use bigdecimal::{BigDecimal, FromPrimitive};
	use chrono::{TimeZone, Utc};
	use database::{AspectId, Database, DatasetId, InputMeasurement, Resolution, DEFAULT_DATA_DIR};
	use serde_json::json;
	use serial_test::serial;
	use splimes::Spline;


	use super::*;

	#[tokio::test]
	#[serial]
	async fn test_api() -> Result<()> {
		// Skip this test if running in CI or if we want fast feedback
		if std::env::var("SKIP_SLOW_TESTS").is_ok() {
			println!("Skipping test_api due to SKIP_SLOW_TESTS environment variable");
			return Ok(());
		}

		// Try to get the Crypto database, skip test if it doesn't exist or doesn't have the required data
		let Ok(database) = Database::existing("Crypto").await else {
			println!("Skipping test_api - Crypto database not found (run database tests first)");
			return Ok(());
		};

		let subjects = database.list_subjects().await?;
		let Some(subject_id) = subjects.iter().find(|(_, name)| name.as_str() == "BTCUSD").map(|(id, _)| *id) else {
			println!("Skipping test_api - BTCUSD subject not found in Crypto database");
			return Ok(());
		};

		let aspects = database.get_subject_aspects(&subject_id).await?;
		let Some(aspect) = aspects.iter().find(|a| a.name() == "open") else {
			println!("Skipping test_api - 'open' aspect not found (available aspects: {:?})", aspects.iter().map(database::Aspect::name).collect::<Vec<_>>());
			return Ok(());
		};

		// Check if the aspect has any measurements before proceeding
		if database.get_earliest_measurement(&aspect.id()).await?.is_none() {
			println!("Skipping test_api - No measurements found for 'open' aspect in Crypto database");
			return Ok(());
		}

		let resolution = Resolution::Hours;
		let method = Spline::Linear;
		let batch_size = 24;

		println!("Aspect ID: {}", aspect.id());

		// start timer
		let timer = std::time::Instant::now();
		println!("Starting build_unprocessed_queue and build_processed_queue...");

		build_unprocessed_queue(&database, &aspect.id(), &resolution, &method, batch_size).await?;

		println!("Time taken for build_unprocessed_queue: {:?}", timer.elapsed());

		// start timer for processed queue
		let timer = std::time::Instant::now();
		println!("Starting build_processed_queue...");

		build_processed_batch_queue(&database, &aspect.id()).await?;

		println!("Time taken for build_processed_queue: {:?}", timer.elapsed());

		let timer = std::time::Instant::now();
		println!("Starting get_processed_batches_queue...");

		let processed_batches = database.get_processed_batches_queue(&aspect.id()).await?;

		println!("Time taken for get_processed_batches_queue: {:?}", timer.elapsed());

		// print random batch from processed queue for verification
		let length = processed_batches.len();
		if length > 0 {
			let random_index = rand::random::<usize>() % length;
			println!("Random processed batch: {}", json!(&processed_batches[random_index]));
			output_denk_format_batch(&processed_batches[random_index]);
		}

		#[rustfmt::skip]
		let contraints = DictionaryConstraints::new(
                        Some(Steps::new(
                                10,
                                Spline::Linear
                        )),
                        Some(
                                vec![
                                        VariablilityType::MaximumStatic(Variability::new(
                                                BigDecimal::from_f64(0.1).unwrap()
                                        ))
                                ]
                        )
                );

		let mut dictionary = Dictionary::new("TestDictionary".to_string(), "A dictionary for testing purposes".to_string(), contraints);
		let timer = std::time::Instant::now();
		println!("Starting load_dictionary...");
		load_dictionary(&database, &aspect.id(), &mut dictionary).await?;
		println!("Time taken for load_dictionary: {:?}", timer.elapsed());
		println!("Dictionary now contains {} patterns", dictionary.len());

		let _timer = std::time::Instant::now();
		println!("Starting build_processed_queue...");

		build_processed_batch_queue(&database, &aspect.id()).await?;

		println!("Finished building processed batch queue");

		let processed_batches = database.get_processed_batches_queue(&aspect.id()).await?;
		let length = processed_batches.len();
		println!("Processed batches count: {length}");
		if length > 0 {
			let random_index = rand::random::<usize>() % length;
			println!("Random processed batch: {}", json!(&processed_batches[random_index]));
			output_denk_format_batch(&processed_batches[random_index]);
		}

		build_patterns_queue(&database, &aspect.id(), &mut dictionary).await?;
		let patterns = database.get_patterns_from_dictionary("TestDictionary", &aspect.id()).await?;
		let length = patterns.len();
		println!("Patterns count: {length}");
		if length > 0 {
			let random_index = rand::random::<usize>() % length;
			println!("Random pattern: {}", json!(&patterns[random_index]));
			output_denk_format_pattern(&patterns[random_index]);
		}

		// Load the newly stored patterns into the dictionary
		load_dictionary(&database, &aspect.id(), &mut dictionary).await?;
		println!("Dictionary now contains {} patterns after loading from database", dictionary.len());

		let timer = std::time::Instant::now();
		println!("Starting build_events_5_percent_queue...");

		create_event_and_manifestations(&database, &aspect.id(), &resolution, &method).await?;

		println!("Time taken for build_events_5_percent_queue: {:?}", timer.elapsed());

		let timer = std::time::Instant::now();
		println!("Starting create_correlations_for_events...");
		create_correlations_for_events(&database, &dictionary, &aspect.id()).await?;
		println!("Time taken for create_correlations_for_events: {:?}", timer.elapsed());

		// print the number of correlations with more than 1 occurrence
		let correlations = database.get_correlations(&aspect.id()).await?;
		println!("Number of correlations with more than 1 occurrence: {}", correlations.iter().filter(|correlation| correlation.occurrences().len() > 1).count());

		// print the number of patterns with more than 1 occurrence
		println!("Number of patterns with more than 1 occurrence: {}", dictionary.patterns().iter().filter(|p| p.occurrences().len() > 1).count());

		let timer = std::time::Instant::now();
		println!("Starting create_signals...");
		create_signals(&database, &aspect.id()).await?;
		println!("Time taken for create_signals: {:?}", timer.elapsed());

		// print the number of signals created
		let signals_lock = SIGNALS_QUEUE.lock().await;
		println!("Number of signals created: {}", signals_lock.len());
		drop(signals_lock);

		let timer = std::time::Instant::now();
		println!("Starting filter_expired_signals...");
		filter_expired_signals(&database, &aspect.id()).await?;
		println!("Time taken for filter_expired_signals: {:?}", timer.elapsed());

		// print the number of signals after filtering
		let signals_lock = SIGNALS_QUEUE.lock().await;
		println!("Number of signals after filtering: {}", signals_lock.len());

		// print a random signal probability for verification

		println!("Preparing to calculate sample signal probability...");

		// Find a signal with non-zero error rates for more meaningful testing
		let mut random_signal = None;
		let correlation_to_modify = signals_lock.values().next().map(|signal| {
			random_signal = Some(signal.clone());
			signal.correlation_id().clone()
		});

		// If we found a signal, check and potentially modify its correlation's error rates
		if let (Some(signal), Some(correlation_id)) = (&random_signal, &correlation_to_modify) {
			if let Some(mut correlation) = database.get_correlation_by_id(correlation_id, &aspect.id()).await? {
				// Check if error rates are zero
				let has_nonzero = correlation.error_rate().values().any(|err| !err.value().is_zero());

				if !has_nonzero {
					println!("Setting test error rate for correlation {} to make testing more meaningful", correlation.id());
					correlation.set_error_rate(signal.signal_type().clone(), database::Distance::new(BigDecimal::from_f64(0.1).unwrap(), splimes::Resolution::Seconds));

					// Update the correlation in the database
					database.update_correlation(&correlation).await?;
				}
			}
		}

		// Use the signal we found
		let random_signal = random_signal.unwrap_or_else(|| panic!("No signals found"));

		// get the correlation id, manifestation id, event id, and signal type from the actual signal
		let random_correlation_id = random_signal.correlation_id().clone();
		let random_manifestation_id = random_signal.manifestation_id().clone();
		let random_event_id = random_signal.event_id().clone();
		let signal_type = random_signal.signal_type().clone();
		let random_correlation = database.get_correlation_by_id(&random_correlation_id, &aspect.id()).await?.ok_or_else(|| anyhow::anyhow!("No correlation found for signal"))?;
		let random_correlation_error_rate = random_correlation.error_rate().clone();

		// Get the error rate for the specific signal type we're using
		let error_rate = random_correlation_error_rate.get(&signal_type).or_else(|| random_correlation_error_rate.values().next()).cloned().unwrap_or_else(|| database::Distance::new(BigDecimal::from(0), splimes::Resolution::Seconds));

		let timer = std::time::Instant::now();
		println!("Calculating sample signal probability...");

		// get oct. 1st 2025 DateTime<Utc>
		let sample_datetime = Utc.with_ymd_and_hms(2025, 12, 15, 0, 0, 0).unwrap();

		let sig_sum_probability = signals_lock.probability_sum(&random_event_id, &signal_type, sample_datetime, &database, &aspect.id()).await?.unwrap_or_else(|| BigDecimal::from(0));

		println!("Time taken for sample signal probability calculation: {:?}", timer.elapsed());
		println!("Sample signal probability for correlation ID {random_correlation_id}, manifestation ID {random_manifestation_id}, signal type {signal_type:?}:");
		println!("  Sum-based Probability: {:.6} (using error rate: {})", sig_sum_probability, error_rate.value());

		drop(signals_lock);

		println!("Test completed successfully!");
		Ok(())
	}

	#[tokio::test]
	#[serial]
	async fn test_api_precise() -> Result<()> {
		// Skip this test if running in CI or if we want fast feedback
		if std::env::var("SKIP_SLOW_TESTS").is_ok() {
			println!("Skipping test_api_precise due to SKIP_SLOW_TESTS environment variable");
			return Ok(());
		}
		let db = fake_database().await;
		let subjects = db.list_subjects().await?;
		let subject = subjects.iter().find(|(_, name)| name.as_str() == "TestSubject").map(|(id, _)| *id).ok_or_else(|| anyhow::anyhow!("Subject 'TestSubject' not found"))?;
		let aspects = db.get_subject_aspects(&subject).await?;
		let aspect = aspects.iter().find(|a| a.name() == "TestAspect").ok_or_else(|| anyhow::anyhow!("Aspect 'TestAspect' not found"))?;
		let resolution = Resolution::Minutes;
		let method = Spline::Linear;
		let batch_size = 60;

		println!("Aspect ID: {}", aspect.id());

		build_unprocessed_queue(&db, &aspect.id(), &resolution, &method, batch_size).await?;
		build_processed_batch_queue(&db, &aspect.id()).await?;
		let processed_batches = db.get_processed_batches_queue(&aspect.id()).await?;
		let length = processed_batches.len();
		println!("Processed batches count: {length}");
		if length > 0 {
			let random_index = rand::random::<usize>() % length;
			println!("Random processed batch: {}", json!(&processed_batches[random_index]));
			output_denk_format_batch(&processed_batches[random_index]);
		}

		#[rustfmt::skip]
                let mut dictionary = Dictionary::new(
                        "TestDictionary".to_string(), 
                        "A dictionary for testing purposes".to_string(), 
                        DictionaryConstraints::new( 
                                Some(Steps::new( 
                                        10,
                                        Spline::Linear 
                                )), 
                                Some(vec![
                                        VariablilityType::AveragePercentile(Variability::new(BigDecimal::from_f64(0.000_000_001).unwrap())),
                                ]) ));

		load_dictionary(&db, &aspect.id(), &mut dictionary).await?;

		println!("Dictionary now contains {} patterns", dictionary.len());

		// Show the pattern after dictionary import (should have 10 steps)
		if !dictionary.is_empty() {
			let first_pattern = &dictionary.patterns()[0];
			println!("First pattern has {} relatives", first_pattern.relatives().len());
			println!("Pattern after dictionary import:");
			output_denk_format_pattern(first_pattern);
		}

		build_patterns_queue(&db, &aspect.id(), &mut dictionary).await?;
		let patterns = db.get_patterns_from_dictionary("TestDictionary", &aspect.id()).await?;
		let length = patterns.len();
		println!("Patterns count: {length}");
		if length > 0 {
			let random_index = rand::random::<usize>() % length;
			println!("Random pattern: {}", json!(&patterns[random_index]));
			output_denk_format_pattern(&patterns[random_index]);
		}

		// Peak detection event already created in fake_database()
		// No need to call create_event_and_manifestations here

		let timer = std::time::Instant::now();
		println!("Starting create_correlations_for_events...");
		create_correlations_for_events(&db, &dictionary, &aspect.id()).await?;
		println!("Time taken for create_correlations_for_events: {:?}", timer.elapsed());

		// print the number of correlations with more than 1 occurrence
		let correlations = db.get_correlations(&aspect.id()).await?;
		println!("Number of correlations with more than 1 occurrence: {}", correlations.iter().filter(|correlation| correlation.occurrences().len() > 1).count());

		// print the number of patterns with more than 1 occurrence
		println!("Number of patterns with more than 1 occurrence: {}", dictionary.patterns().iter().filter(|p| p.occurrences().len() > 1).count());

		let timer = std::time::Instant::now();
		println!("Starting create_signals...");
		create_signals(&db, &aspect.id()).await?;
		println!("Time taken for create_signals: {:?}", timer.elapsed());

		// print the number of signals created
		let signals_lock = SIGNALS_QUEUE.lock().await;
		println!("Number of signals created: {}", signals_lock.len());
		drop(signals_lock);

		let start_time = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
		let query_time = start_time + chrono::Duration::hours(61); // Hour 61 is our prediction target

		let timer = std::time::Instant::now();
		println!("Starting filter_expired_signals at query_time={query_time}...");
		filter_expired_signals_at_time(&db, &aspect.id(), Some(query_time)).await?;
		println!("Time taken for filter_expired_signals: {:?}", timer.elapsed());

		// print the number of signals after filtering
		let signals_lock = SIGNALS_QUEUE.lock().await;
		println!("Number of signals after filtering: {}", signals_lock.len());
		drop(signals_lock);

		// Calculate and print the probability for the Signals in the queue
		let events = get_events_queue(&db).await?;
		let event_id_option = events.first().map(|e| e.id().clone());
		drop(events);

		if let Some(event_id) = event_id_option {
			let signals_lock = SIGNALS_QUEUE.lock().await;
			// Use PredictStart since we're checking at the start of hour 61 (2025-01-03 13:00:00)
			let Some(sum_probability) = signals_lock.probability_sum(&event_id, &SignalType::Custom("PredictStart".to_string()), query_time, &db, &aspect.id()).await? else {
				bail!("No signals found for event ID {}", event_id);
			};

			let Some(avg_probability) = signals_lock.probability_average(&event_id, &SignalType::Custom("PredictStart".to_string()), query_time, &db, &aspect.id()).await? else {
				bail!("No signals found for event ID {}", event_id);
			};

			println!("Peak Event Probability - Sum: {sum_probability}");
			println!("Peak Event Probability - Average: {avg_probability}");
			drop(signals_lock);
		}
		Ok(())
	}

	async fn fake_database() -> Database {
		// Cleanup existing test database if it exists
		use std::fs::remove_dir_all;
		let db_path = format!("{DEFAULT_DATA_DIR}/TestDB");
		remove_dir_all(&db_path).ok();

		let db = Database::new("TestDB").await.unwrap();
		let test_subject = db.observe_subject("TestSubject").await.unwrap();
		let test_aspect = db.track_aspect(test_subject.id(), "TestAspect", Resolution::Seconds).await.unwrap();
		let start_time = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
		#[rustfmt::skip]
		let points = vec![
                        (0, BigDecimal::from(0)), // Added hour 0 to allow peak detection at hour 1
                        (1, BigDecimal::from(1)), 
                        (2, BigDecimal::from(0)), 
                        (3, BigDecimal::from(0)), 
                        (4, BigDecimal::from(0)), 
                        (5, BigDecimal::from(1)), 
                        (6, BigDecimal::from(0)), 
                        (7, BigDecimal::from(0)), 
                        (8, BigDecimal::from(0)), 
                        (9, BigDecimal::from(1)), 
                        (10, BigDecimal::from(0)),
                        (11, BigDecimal::from(0)), 
                        (12, BigDecimal::from(0)), 
                        (13, BigDecimal::from(1)), 
                        (14, BigDecimal::from(0)), 
                        (15, BigDecimal::from(0)), 
                        (16, BigDecimal::from(0)), 
                        (17, BigDecimal::from(1)), 
                        (18, BigDecimal::from(0)), 
                        (19, BigDecimal::from(0)), 
                        (20, BigDecimal::from(0)),
                        (21, BigDecimal::from(1)), 
                        (22, BigDecimal::from(0)), 
                        (23, BigDecimal::from(0)), 
                        (24, BigDecimal::from(0)), 
                        (25, BigDecimal::from(1)), 
                        (26, BigDecimal::from(0)), 
                        (27, BigDecimal::from(0)), 
                        (28, BigDecimal::from(0)), 
                        (29, BigDecimal::from(1)), 
                        (30, BigDecimal::from(0)),
                        (31, BigDecimal::from(0)), 
                        (32, BigDecimal::from(0)), 
                        (33, BigDecimal::from(1)), 
                        (34, BigDecimal::from(0)), 
                        (35, BigDecimal::from(0)), 
                        (36, BigDecimal::from(0)), 
                        (37, BigDecimal::from(1)), 
                        (38, BigDecimal::from(0)), 
                        (39, BigDecimal::from(0)), 
                        (40, BigDecimal::from(0)),
                        (41, BigDecimal::from(1)), 
                        (42, BigDecimal::from(0)), 
                        (43, BigDecimal::from(0)), 
                        (44, BigDecimal::from(0)), 
                        (45, BigDecimal::from(1)), 
                        (46, BigDecimal::from(0)), 
                        (47, BigDecimal::from(0)), 
                        (48, BigDecimal::from(0)), 
                        (49, BigDecimal::from(1)), 
                        (50, BigDecimal::from(0)),
                        (51, BigDecimal::from(0)), 
                        (52, BigDecimal::from(0)), 
                        (53, BigDecimal::from(1)), 
                        (54, BigDecimal::from(0)), 
                        (55, BigDecimal::from(0)), 
                        (56, BigDecimal::from(0)), 
                        (57, BigDecimal::from(1)), 
                        (58, BigDecimal::from(0)), 
                        (59, BigDecimal::from(0)), 
                        (60, BigDecimal::from(0)),
                        (61, BigDecimal::from(1)),  // Next peak - this is what we want to predict!
                        (62, BigDecimal::from(0)),
                        (63, BigDecimal::from(0)),
                        (64, BigDecimal::from(0))
                ];

		for (i, value) in points {
			let timestamp = start_time + chrono::Duration::hours(i64::from(i));
			let measurement = InputMeasurement::new(timestamp, value);
			db.capture_measurement(test_aspect.id(), DatasetId::new(), measurement).await.unwrap();
		}

		// create a peak detection event for testing
		create_peak_detection_events(&db, &test_aspect.id(), &Resolution::Hours, &Spline::Linear, "Peak Detection Test").await.unwrap();
		println!("Events in database after peak detection:");
		let events = get_events_queue(&db).await.unwrap();
		println!("Events in database: {}", events.len());
		drop(events);

		db
	}

	#[tokio::test]
	#[serial]
	async fn test_batch_processing() -> Result<()> {
		use std::fs::remove_dir_all;

		// Skip this test if running in CI or if we want fast feedback
		if std::env::var("SKIP_SLOW_TESTS").is_ok() {
			println!("Skipping test_batch_processing due to SKIP_SLOW_TESTS environment variable");
			return Ok(());
		}

		let db_path = format!("{DEFAULT_DATA_DIR}/test_bath_processing");
		remove_dir_all(&db_path).ok();

		let db = Database::new("test_bath_processing").await.unwrap();
		let test_subject = db.observe_subject("TestSubject").await.unwrap();
		let test_aspect = db.track_aspect(test_subject.id(), "TestAspect", Resolution::Seconds).await.unwrap();
		let start_time = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
		#[rustfmt::skip]
		let points = vec![
                        (1, BigDecimal::from(1)),
                        (2, BigDecimal::from(2)),
                        (3, BigDecimal::from(3)),
                        (4, BigDecimal::from(2)),
                        (5, BigDecimal::from(3)),
                        (6, BigDecimal::from(5)),
                        (7, BigDecimal::from(3)),
                        (8, BigDecimal::from(2)),
                        (9, BigDecimal::from(1)),
                        (10, BigDecimal::from(1)),
                        (11, BigDecimal::from(0)),
                        (12, BigDecimal::from(-1)),
                        (13, BigDecimal::from(0)),
                        (14, BigDecimal::from(1)),
                        (15, BigDecimal::from(0))
                ];

		for (i, value) in points {
			let timestamp = start_time + chrono::Duration::minutes(i64::from(i));
			let measurement = InputMeasurement::new(timestamp, value);
			db.capture_measurement(test_aspect.id(), DatasetId::new(), measurement).await.unwrap();
		}

		let aspect = test_aspect;
		//let resolution = Resolution::Minutes;
		//let method = Spline::Linear;
		//let batch_size = 10;

		// build_unprocessed_queue(&db, &aspect.id(), &resolution, &method, batch_size).await?;

		// Check how many batches were stored in the database
		let unprocessed_batches = db.get_unprocessed_batches(&aspect.id()).await?;
		println!("Unprocessed batches count: {}", unprocessed_batches.len());

		//build_processed_batch_queue(&db, &aspect.id()).await?;

		let processed_batches = db.get_processed_batches_queue(&aspect.id()).await?;
		println!("Processed batches count: {}", processed_batches.len());

		// With 15 points and batch size 10, sliding window creates: 15 - 10 + 1 = 6 overlapping batches
		assert_eq!(processed_batches.len(), 6);

		Ok(())
	}

	#[tokio::test]
	#[serial]
	async fn test_specific_process_batch() -> Result<()> {
		use std::fs::remove_dir_all;

		// Skip this test if running in CI or if we want fast feedback
		if std::env::var("SKIP_SLOW_TESTS").is_ok() {
			println!("Skipping test_specific_process_batch due to SKIP_SLOW_TESTS environment variable");
			return Ok(());
		}

		let db_path = format!("{DEFAULT_DATA_DIR}/test_specific_process_batch");
		remove_dir_all(&db_path).ok();

		let db = Database::new("test_specific_process_batch").await.unwrap();
		let test_subject = db.observe_subject("TestSubject").await.unwrap();
		let test_aspect = db.track_aspect(test_subject.id(), "TestAspect", Resolution::Seconds).await.unwrap();
		let start_time = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
		#[rustfmt::skip]
		let points = vec![
                        (1, BigDecimal::from(1)),
                        (2, BigDecimal::from(2)),
                        (3, BigDecimal::from(3)),
                        (4, BigDecimal::from(2)),
                        (5, BigDecimal::from(3)),
                        (6, BigDecimal::from(5)),
                        (7, BigDecimal::from(3)),
                        (8, BigDecimal::from(2)),
                        (9, BigDecimal::from(1)),
                        (10, BigDecimal::from(1)),
                        (11, BigDecimal::from(0)),
                        (12, BigDecimal::from(-1)),
                        (13, BigDecimal::from(0)),
                        (14, BigDecimal::from(1)),
                        (15, BigDecimal::from(0))
                ];

		for (i, value) in points {
			let timestamp = start_time + chrono::Duration::minutes(i64::from(i));
			let measurement = InputMeasurement::new(timestamp, value);
			db.capture_measurement(test_aspect.id(), DatasetId::new(), measurement).await.unwrap();
		}

		let aspect = test_aspect;
		// let resolution = Resolution::Minutes;
		// let method = Spline::Linear;
		// let batch_size = 10;

		// build_unprocessed_queue(&db, &aspect.id(), &resolution, &method, batch_size).await?;

		// Check how many batches were stored in the database
		let unprocessed_batches = db.get_unprocessed_batches(&aspect.id()).await?;
		println!("Unprocessed batches count: {}", unprocessed_batches.len());

		build_processed_batch_queue(&db, &aspect.id()).await?;

		let processed_batches = db.get_processed_batches_queue(&aspect.id()).await?;
		println!("Processed batches count: {}", processed_batches.len());

		// With 15 points and batch size 10, sliding window creates: 15 - 10 + 1 = 6 overlapping batches
		assert_eq!(processed_batches.len(), 6);

		Ok(())
	}

	async fn create_peak_detection_events(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, name: &str) -> Result<()> {
		let start_time = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
		let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;

		// Use optimized bulk analysis to get all points
		let mut point_stream = Outputs::analyze_range(database, *aspect, start_time, end_time, *resolution, *method).await?;

		let mut points = Vec::new();
		while let Some(result) = point_stream.next().await {
			let point = result?;
			points.push(point);
		}

		// Sort points by timestamp to ensure proper chronological order
		points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

		// Find the global maximum value
		let global_max = points.iter().map(|p| &p.value).max().unwrap();

		let mut event = Event::new(name.to_string(), Some(format!("Detects peak values (local maxima that are also global maximum) for aspect {aspect}")));

		// Check each point to see if it's a peak
		for i in 1..points.len() - 1 {
			let prev_value = &points[i - 1].value;
			let curr_value = &points[i].value;
			let next_value = &points[i + 1].value;

			// Check if current point is a local peak AND equals global maximum
			if curr_value == global_max && curr_value > prev_value && curr_value > next_value {
				let database_info = database.get_database_info().await.expect("Database info should be available");

				// Create manifestation spanning from the previous point (start of rise) to next point (start of decline)
				let manifestation = Manifestation::new(
					database_info.id().as_uuid(),
					points[i - 1].timestamp, // Start of rise to peak
					points[i + 1].timestamp, // Start of decline from peak
				);

				event.add_manifestation(manifestation);
			}
		}

		// Only add the event if we found at least one peak
		if event.manifestations().is_empty() {
			println!("No peaks detected in the dataset");
		} else {
			let manifestation_count = event.manifestations().len();
			database.store_event(&event).await?;
			println!("Added peak detection event with {manifestation_count} manifestation(s)");
		}

		Ok(())
	}

	#[allow(dead_code)]
	pub async fn get_events_queue(database: &Database) -> Result<Vec<Event>> {
		let events = database.get_unprocessed_events().await?;
		Ok(events)
	}

	fn output_denk_format_batch(batch: &Batch) {
		println!("----- DENK FORMAT OUTPUT BEGIN -----");
		for (count, measurement) in batch.clone().into_iter().enumerate() {
			if count == 0 || count == batch.size() - 1 {
				let vector = measurement.vector().unwrap();
				let amplitude = vector.amplitude().round(2);
				println!("{} {}", vector.location(), amplitude);
			}
		}
		println!("----- DENK FORMAT OUTPUT END -----");
	}

	fn output_denk_format_pattern(pattern: &Pattern) {
		println!("----- DENK FORMAT OUTPUT BEGIN -----");
		println!("Pattern ID: {}", pattern.id());
		for relative in pattern.relatives() {
			println!("{} {}", relative.vector().location().round(2), relative.vector().amplitude().round(2));
		}
		println!("----- DENK FORMAT OUTPUT END -----");
		let zero = BigDecimal::zero();
		println!("max_x: {}, max_y: {}", pattern.relatives().iter().map(|r| r.vector().location()).max().unwrap_or(&zero), pattern.relatives().iter().map(|r| r.vector().amplitude()).max().unwrap_or(&zero));
	}
}
