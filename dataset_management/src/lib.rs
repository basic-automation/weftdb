use std::sync::LazyLock;

use anyhow::Result;
use bigdecimal::{BigDecimal, FromPrimitive, Zero};
use chrono::Datelike;
use database::{AspectId, Database, Resolution};
use futures::TryStreamExt;
use rayon::prelude::*;
use splimes::Spline;
use tokio::sync::Mutex;
pub use types::*;
pub use batch_utils::{UNPROCESSED_BATCHES_QUEUE, build_unprocessed_queue};

#[cfg(test)]
mod memory_test;
pub mod types;
mod batch_utils;

pub const BATCH_SIZE: [usize; 1] = [100];
pub static DEFAULT_ERROR_RATE: LazyLock<signal::Distance> = LazyLock::new(|| signal::Distance { 
	value: BigDecimal::zero(), 
	units: splimes::Resolution::Seconds  // Use seconds as the canonical unit for error rates
});

static PROCESSED_BATCHES_QUEUE: LazyLock<Mutex<Vec<Batch>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static PATTERNS_QUEUE: LazyLock<Mutex<Vec<Pattern>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static EVENTS_QUEUE: LazyLock<Mutex<Events>> = LazyLock::new(|| Mutex::new(Events::new()));
static CORRELATIONS_QUEUE: LazyLock<Mutex<Correlations>> = LazyLock::new(|| Mutex::new(Correlations::new()));
static SIGNALS_QUEUE: LazyLock<Mutex<Signals>> = LazyLock::new(|| Mutex::new(Signals::new()));



pub async fn build_processed_batch_queue() -> Result<()> {
	// print UNPROCESSED_BATCHES_QUEUE length for verification
	let unprocessed_lock = UNPROCESSED_BATCHES_QUEUE.lock().await;
	println!("UNPROCESSED_BATCHES_QUEUE length: {}", unprocessed_lock.len());
	drop(unprocessed_lock);

	let mut processed_batches = UNPROCESSED_BATCHES_QUEUE.lock().await.clone();
	UNPROCESSED_BATCHES_QUEUE.lock().await.clear();
	processed_batches
		.par_iter_mut()
		.map(|batch| {
			let _ = batch.process();
		})
		.collect::<()>();

	let mut processed_lock = PROCESSED_BATCHES_QUEUE.lock().await;
	processed_lock.append(&mut processed_batches);
	Ok(())
}

pub async fn build_patterns_queue() -> Result<()> {
	let mut processed_lock = PROCESSED_BATCHES_QUEUE.lock().await;
	let mut patterns_lock = PATTERNS_QUEUE.lock().await;

	// Take all batches out, process them, and don't put back the ones we've processed
	let batches = std::mem::take(&mut *processed_lock);
	let mut remaining_batches = Vec::new();

	for batch in batches.into_iter() {
		// Generate a new pattern ID for this batch
		let pattern_id = PatternID::new();

		// Get the first and last timestamps from active measurements
		let active_measurements: Vec<_> = batch.measurements().iter().filter(|m| m.is_active()).collect();

		if active_measurements.is_empty() {
			remaining_batches.push(batch); // Keep batches with no active measurements
			continue;
		}

		let beginning = active_measurements.iter().map(|m| m.get_measurement_timestamp()).min().cloned().unwrap();
		let end = active_measurements.iter().map(|m| m.get_measurement_timestamp()).max().cloned().unwrap();

		// Create an occurrence for this pattern
		let occurrence = Occurrence::new(batch.metadata.aspect, batch.metadata.resolution, batch.metadata.size, batch.metadata.database_info.clone(), pattern_id, beginning, end);

		// Extract relatives from active measurements that have analysis
		let relatives: Vec<Relative> = active_measurements.iter().filter_map(|measurement| measurement.analysis()?.relative().cloned()).collect();

		// Create the pattern
		let pattern = Pattern::new(pattern_id, vec![occurrence], relatives);
		patterns_lock.push(pattern);
		// Don't add this batch back to remaining_batches (effectively removing it)
	}

	// Put back any batches that weren't processed
	*processed_lock = remaining_batches;

	Ok(())
}

/// Optimized dictionary loading with memory-aware parallelism using rayon
/// Dynamically calculates batch sizes based on available memory and removes patterns as processed
pub async fn load_dictionary(dictionary: &mut Dictionary) -> Result<()> {
	let mut patterns_queue = PATTERNS_QUEUE.lock().await;
	let pattern_count = patterns_queue.len();

	let start_time = std::time::Instant::now();
	println!("Loading {} patterns into dictionary with memory-aware batching", pattern_count);

	// For smaller pattern sets, process directly from the queue
	if pattern_count <= 1000 {
		while let Some(pattern) = patterns_queue.pop() {
			dictionary.import_pattern(pattern).await?;
		}
	} else {
		// Get available memory information
		let available_memory_mb = get_available_memory_mb();
		println!("Available memory: {} MB", available_memory_mb);

		// Calculate safe batch size based on available memory
		// Assume each pattern uses ~1MB when processed (conservative estimate)
		// Use only 25% of available memory for safety
		let safe_memory_mb = (available_memory_mb as f64 * 0.25) as usize;
		let estimated_pattern_size_mb = 1; // Conservative estimate per pattern
		let memory_based_batch_size = (safe_memory_mb / estimated_pattern_size_mb).clamp(10, 200);

		let cpu_count = num_cpus::get();
		let optimal_batch_size = (memory_based_batch_size / cpu_count).max(5);

		println!("Using batch size: {} patterns per thread, {} total per chunk", optimal_batch_size, memory_based_batch_size);

		// Process in memory-aware chunks, draining from the queue
		while !patterns_queue.is_empty() {
			let queue_len = patterns_queue.len();
			let chunk_size = memory_based_batch_size.min(queue_len);
			let chunk: Vec<Pattern> = patterns_queue.drain(queue_len - chunk_size..).collect();

			// Release the lock while processing
			drop(patterns_queue);

			// Process chunk in parallel with rayon
			let chunk_dictionaries: Vec<Dictionary> = chunk
				.chunks(optimal_batch_size)
				.collect::<Vec<_>>()
				.par_iter()
				.map(|chunk_patterns| {
					let mut chunk_dict = Dictionary::new(format!("Chunk Dictionary {}", uuid::Uuid::new_v4()), "Temporary dictionary for parallel processing".to_string(), dictionary.constraints.clone());

					for pattern in chunk_patterns.iter() {
						if let Err(e) = futures::executor::block_on(chunk_dict.import_pattern(pattern.clone())) {
							eprintln!("Failed to import pattern in chunk: {}", e);
						}
					}

					chunk_dict
				})
				.collect();

			// Merge chunk dictionaries
			for chunk_dict in chunk_dictionaries {
				dictionary.merge_dictionary(chunk_dict).await?;
			}

			// Re-acquire lock for next iteration
			patterns_queue = PATTERNS_QUEUE.lock().await;

			let remaining = patterns_queue.len();
			if remaining > 0 {
				// println!("Processed {} patterns, {} remaining", pattern_count - remaining, remaining);
			}

			// Yield to prevent blocking other tasks
			tokio::task::yield_now().await;
		}
	}

	let final_elapsed = start_time.elapsed();
	let final_rate = pattern_count as f64 / final_elapsed.as_secs_f64();
	println!("Dictionary loading completed. Processed {} patterns in {:?} ({:.1} patterns/sec)", pattern_count, final_elapsed, final_rate);
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

		let mut mem_status = MemoryStatusEx { dw_length: mem::size_of::<MemoryStatusEx>() as u32, dw_memory_load: 0, ull_total_phys: 0, ull_avail_phys: 0, ull_total_page_file: 0, ull_avail_page_file: 0, ull_total_virtual: 0, ull_avail_virtual: 0, ull_avail_extended_virtual: 0 };

		unsafe {
			if GlobalMemoryStatusEx(&mut mem_status) != 0 {
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
/// - `database`: Database instance to query measurements from
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
/// 6. Add events to the global EVENTS_QUEUE
pub async fn create_event_and_manifestations(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline) -> Result<()> {
	let start_time = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
	let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;

	// Use optimized bulk analysis to get all points
	let mut point_stream = database.stream_analyze_range(*aspect, start_time, end_time, *resolution, *method);

	let mut points = Vec::new();
	while let Some(point) = point_stream.try_next().await? {
		points.push(point);
	}

	// Sort points by timestamp to ensure proper chronological order
	points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	let mut event = Event::new(format!("5% Monthly Price Increase - {}", aspect), Some(format!("Detects when price increases 5% or more from start to end of month for aspect {}", aspect)));
	let threshold_percentage = BigDecimal::from_f64(0.05).unwrap(); // 5% threshold

	// Group points by month and analyze each month
	use std::collections::HashMap;
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

	// Add all detected events to the global queue
	let mut events_lock = EVENTS_QUEUE.lock().await;
	events_lock.insert(event.id.clone(), event);
	drop(events_lock);

	Ok(())
}

pub async fn create_correlations_for_events(dictionary: &Dictionary) -> Result<()> {
	// Get patterns from dictionary (merged patterns with multiple occurrences) and events from global queue
	let patterns = &dictionary.patterns;
	let events = EVENTS_QUEUE.lock().await.clone();

	// Exit early if no patterns or events to correlate
	if patterns.is_empty() || events.is_empty() {
		println!("No patterns or events found to correlate");
		return Ok(());
	}

	println!("Correlating {} events with {} patterns", events.len(), patterns.len());

	// For each event, correlate with all patterns
	for event_id in events.keys() {
		for pattern in patterns {
			// Assign constant to local variable before borrowing to avoid clippy warning
			let error_rate = DEFAULT_ERROR_RATE.clone();

			let correlation = Correlation::new(pattern.occurrences()[0].database_info.id().as_uuid(), pattern.id(), event_id.clone(), error_rate, pattern.occurrences().clone());
			CORRELATIONS_QUEUE.lock().await.insert(event_id.clone(), pattern.id(), correlation);
		}
	}

	// Update the global events queue with correlations
	let mut events_lock = EVENTS_QUEUE.lock().await;
	*events_lock = events;
	drop(events_lock);

	println!("Pattern-Event correlation completed successfully");
	Ok(())
}

pub async fn create_signals(_dictionary: &Dictionary) -> Result<()> {
	// Get correlations and events from global queues
	let correlations = CORRELATIONS_QUEUE.lock().await.clone();
	let events = EVENTS_QUEUE.lock().await.clone();

	// Exit early if no correlations or events to process
	if correlations.is_empty() || events.is_empty() {
		println!("No correlations or events found to create signals");
		return Ok(());
	}

	println!("Creating signals from {} correlations and {} events", correlations.len(), events.len());

	let mut signals_count = 0;

	// Process each correlation to create signals
	for ((_event_id, _pattern_id), correlation) in correlations.iter() {
		// Get the corresponding event
		if let Some(event) = events.get(&correlation.event_id) {
			// For each manifestation of the event, create signals based on pattern occurrences
			for manifestation in event.manifestations.values() {
				// Create signals for each pattern occurrence in the correlation
				for occurrence in correlation.occurrences() {
					// Calculate the distance between the event manifestation and pattern occurrence
					let manifestation_start = manifestation.start;
					let manifestation_midpoint = manifestation.midpoint();
					let manifestation_end = manifestation.end;

					let occurrence_beginning = occurrence.beginning;
					let occurrence_midpoint = occurrence.beginning + (occurrence.end - occurrence.beginning) / 2;
					let occurrence_end = occurrence.end;

					// Calculate time difference using the pattern's resolution from the occurrence
					let pattern_resolution = occurrence.resolution;
					let time_diff_start = pattern_resolution.difference(&manifestation_start, &occurrence_beginning)?;
					let time_diff_midpoint = pattern_resolution.difference(&manifestation_midpoint, &occurrence_midpoint)?;
					let time_diff_end = pattern_resolution.difference(&manifestation_end, &occurrence_end)?;
					
					let distance_start = signal::Distance { value: BigDecimal::from(time_diff_start), units: pattern_resolution };
					let distance_midpoint = signal::Distance { value: BigDecimal::from(time_diff_midpoint), units: pattern_resolution };
					let distance_end = signal::Distance { value: BigDecimal::from(time_diff_end), units: pattern_resolution };

					// Create different types of signals based on the relationship
					let signal_types = vec![
						(SignalType::Custom("PredictStart".to_string()), distance_start),  // Pattern predicts event start
						(SignalType::Custom("PredictMid".to_string()), distance_midpoint), // Pattern predicts event midpoint
						(SignalType::Custom("PredictEnd".to_string()), distance_end),      // Pattern predicts event end
					];

					for (signal_type, distance) in signal_types {
						// Use the actual manifestation ID from the event
						let manifestation_id = manifestation.id.clone();

						let signal = Signal::new(correlation.id.clone(), manifestation_id, correlation.event_id.clone(), manifestation_midpoint, signal_type, distance.clone());

						// Add signal to the global signals queue
						SIGNALS_QUEUE.lock().await.insert(signal);
						signals_count += 1;
					}
				}
			}
		}
	}

	println!("Created {} signals successfully", signals_count);
	Ok(())
}

pub async fn filter_expired_signals() -> Result<()> {
	use chrono::Utc;

	let current_time = Utc::now();
	let mut signals_lock = SIGNALS_QUEUE.lock().await;
	let events_lock = EVENTS_QUEUE.lock().await;
	let correlations_lock = CORRELATIONS_QUEUE.lock().await;

	if signals_lock.is_empty() {
		println!("No signals found to filter");
		return Ok(());
	}

	let initial_count = signals_lock.len();
	println!("Filtering expired signals from {} total signals", initial_count);

	// Collect signals to remove
	let mut signals_to_remove = Vec::new();

	// Iterate through all signals to check for expiration
	for signal in signals_lock.values() {
		// Find the event this signal is predicting by matching correlation IDs
		let mut signal_event_id = None;

		// Look through correlations to find which event this signal belongs to
		for ((event_id, _pattern_id), correlation) in correlations_lock.iter() {
			if correlation.id == signal.correlation_id {
				signal_event_id = Some(event_id.clone());
				break;
			}
		}

		// If we found the event, check for expiration
		if let Some(event_id) = signal_event_id {
			if let Some(event) = events_lock.get(&event_id) {
				// Find the manifestation this signal was created from (the baseline)
				if let Some(base_manifestation) = event.manifestations.values().find(|m| m.id == signal.manifestation_id) {
					// Find the next manifestation after the baseline that this signal is predicting
					let mut future_manifestations: Vec<_> = event.manifestations.values().filter(|manifestation| manifestation.start > base_manifestation.end).collect();

					// Sort by start date to find the very next manifestation
					future_manifestations.sort_by(|a, b| a.start.cmp(&b.start));

					// If there's a next manifestation and it has completely ended, signal expires
					if let Some(next_manifestation) = future_manifestations.first() {
						if current_time >= next_manifestation.end {
							// Signal has expired - the predicted manifestation has ended
							// Use the MIDPOINT of the next manifestation as the resolution time (when event was supposed to happen)
							let resolution_time = next_manifestation.midpoint();
							signals_to_remove.push((signal.correlation_id.clone(), signal.manifestation_id.clone(), signal.signal_type.clone(), resolution_time));
						}
					}
					// If there are no future manifestations, the signal doesn't expire yet
				} else {
					// If we can't find the baseline manifestation this signal was created from, remove it as invalid
					// Use current time as fallback resolution time for invalid signals
					signals_to_remove.push((signal.correlation_id.clone(), signal.manifestation_id.clone(), signal.signal_type.clone(), current_time));
				}
			}
		}
	}

	// Release the correlations lock before modifying signals
	drop(correlations_lock);

        println!("Removing {} expired or invalid signals", signals_to_remove.len());

	// Remove expired signals with error correction
	let mut removed_count = 0;
	for (correlation_id, manifestation_id, signal_type, resolution_time) in signals_to_remove {
		// Find the correlation for this signal and apply error correction
		let mut correlations_lock = CORRELATIONS_QUEUE.lock().await;

		// Find the correlation first without holding the iterator
		let correlation_clone = correlations_lock.iter().find(|((_, _), corr)| corr.id == correlation_id).map(|(_, v)| v.clone());

		if let Some(mut correlation) = correlation_clone {
			// Use error correction when removing the signal
			if let Ok(Some(_removed_signal)) = signals_lock.remove_with_error_correction(&mut correlation, &manifestation_id, &signal_type, resolution_time).await {
				// Find the key for this correlation to update it
				let key_to_update = correlations_lock.iter().find(|((_, _), c)| c.id == correlation_id).map(|((event_id, pattern_id), _)| (event_id.clone(), *pattern_id));

				if let Some((event_id, pattern_id)) = key_to_update {
					correlations_lock.remove(&event_id, &pattern_id);
					correlations_lock.insert(event_id, pattern_id, correlation);
				}
				removed_count += 1;
			}
		}

		drop(correlations_lock);
	}

	let remaining_count = signals_lock.len();

	// Release locks
	drop(events_lock);
	drop(signals_lock);

	println!("Filtered {} expired signals. {} signals remaining from {} initial signals", removed_count, remaining_count, initial_count);

	Ok(())
}

#[cfg(test)]
mod tests {

	use anyhow::bail;
	use bigdecimal::{BigDecimal, FromPrimitive};
	use chrono::{TimeZone, Utc};
	use database::{Database, DatabaseInfo, InputMeasurement};
	use serde_json::json;
	use serial_test::serial;
	use splimes::{Point, Spline};
        use batch_utils::build_unprocessed_queue;

	use super::*;

	#[tokio::test(flavor = "multi_thread")]
	#[serial]
	async fn test_api() -> Result<()> {
		let database = Database::existing("Crypto").await?;
		let subjects = database.list_subjects().await?;
		let subject_id = subjects.iter().find(|(_, name)| name.as_str() == "BTCUSD").map(|(id, _)| *id).ok_or_else(|| anyhow::anyhow!("Subject 'BTCUSD' not found"))?;
		let aspects = database.get_subject_aspects(&subject_id).await?;
		let aspect = aspects.iter().find(|a| a.name() == "open").ok_or_else(|| anyhow::anyhow!("Aspect 'open' not found"))?;
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

		build_processed_batch_queue().await?;

		println!("Time taken for build_processed_queue: {:?}", timer.elapsed());

		let processed_lock = PROCESSED_BATCHES_QUEUE.lock().await; 

		// print random batch from processed queue for verification
		let mut random_index = 0;
		let length = processed_lock.len();
		if length > 0 {
			random_index = rand::random::<usize>() % length;
			println!("Random processed batch: {}", json!(&processed_lock[random_index]));
			output_denk_format_batch(&processed_lock[random_index]);
		}

		drop(processed_lock);

		let timer = std::time::Instant::now();
		println!("Starting build_patterns_queue...");

		build_patterns_queue().await?;

		println!("Time taken for build_patterns_queue: {:?}", timer.elapsed());

		let patterns_lock = PATTERNS_QUEUE.lock().await;

		// print random pattern from patterns queue for verification
		let length = patterns_lock.len();
		if length > 0 {
			println!("Random pattern: {}", json!(&patterns_lock[random_index]));
			output_denk_format_pattern(&patterns_lock[random_index]);
		}

		drop(patterns_lock);

		#[rustfmt::skip]
		let contraints = DictionaryConstraints {
                        steps: Some(Steps {
                                count: 10,
                                interpolation: Spline::Linear
                        }),
                        variabilities: Some(
                                vec![
                                        VariablilityType::MaximumStatic(Variability {
                                                value: BigDecimal::from_f64(0.1).unwrap()
                                        })
                                ]
                        )
                };

		let mut dictionary = Dictionary::new("Test Dictionary".to_string(), "A dictionary for testing purposes".to_string(), contraints);
		let timer = std::time::Instant::now();
		println!("Starting load_dictionary...");
		load_dictionary(&mut dictionary).await?;
		println!("Time taken for load_dictionary: {:?}", timer.elapsed());
		println!("Dictionary now contains {} patterns", dictionary.len());

		let timer = std::time::Instant::now();
		println!("Starting build_events_5_percent_queue...");

		create_event_and_manifestations(&database, &aspect.id(), &resolution, &method).await?;

		println!("Time taken for build_events_5_percent_queue: {:?}", timer.elapsed());

		let timer = std::time::Instant::now();
		println!("Starting create_correlations_for_events...");
		create_correlations_for_events(&dictionary).await?;
		println!("Time taken for create_correlations_for_events: {:?}", timer.elapsed());

		// print the number of correlations with more than 1 occurrence
		let correlations_lock = CORRELATIONS_QUEUE.lock().await;
		let c = correlations_lock.iter().filter(|(_, correlation)| correlation.occurrences().len() > 1).collect::<Vec<_>>();

		println!("Number of correlations with more than 1 occurrence: {}", c.len());
		drop(correlations_lock);

		// print the number of patterns with more than 1 occurrence
		println!("Number of patterns with more than 1 occurrence: {}", dictionary.patterns.iter().filter(|p| p.occurrences().len() > 1).count());

		let timer = std::time::Instant::now();
		println!("Starting create_signals...");
		create_signals(&dictionary).await?;
		println!("Time taken for create_signals: {:?}", timer.elapsed());

		// print the number of signals created
		let signals_lock = SIGNALS_QUEUE.lock().await;
		println!("Number of signals created: {}", signals_lock.len());
		drop(signals_lock);

		let timer = std::time::Instant::now();
		println!("Starting filter_expired_signals...");
		filter_expired_signals().await?;
		println!("Time taken for filter_expired_signals: {:?}", timer.elapsed());

		// print the number of signals after filtering
		let signals_lock = SIGNALS_QUEUE.lock().await;
		println!("Number of signals after filtering: {}", signals_lock.len());

		// print a random signal probability for verification

		println!("Preparing to calculate sample signal probability...");

		// Find a signal with non-zero error rates for more meaningful testing
		let mut random_signal = None;
		let mut correlation_to_modify = None;

		// First, find a signal and identify which correlation needs modification
		if let Some(signal) = signals_lock.values().next() {
			random_signal = Some(signal.clone());
			correlation_to_modify = Some(signal.correlation_id.clone());
			// Just take the first signal for now
		}

		// If we found a signal, check and potentially modify its correlation's error rates
		if let (Some(signal), Some(correlation_id)) = (&random_signal, &correlation_to_modify) {
			let mut correlations_lock = CORRELATIONS_QUEUE.lock().await;

			// Find and clone the correlation first
			let correlation_data = correlations_lock.iter().find(|((_, _), corr)| corr.id == *correlation_id).map(|((event_id, pattern_id), v)| ((event_id.clone(), *pattern_id), v.clone()));

			if let Some((key, correlation)) = correlation_data {
				// Check if error rates are zero
				let has_nonzero = correlation.error_rate().values().any(|err| !err.value.is_zero());

				if !has_nonzero {
					// Clone the correlation, modify it, and put it back
					let mut modified_correlation = correlation;
					println!("Setting test error rate for correlation {} to make testing more meaningful", modified_correlation.id);
					modified_correlation.set_error_rate(signal.signal_type.clone(), 
						signal::Distance { value: BigDecimal::from_f64(0.1).unwrap(), units: splimes::Resolution::Seconds }
					);

					// Replace the correlation in the map
					correlations_lock.remove(&key.0, &key.1);
					correlations_lock.insert(key.0, key.1, modified_correlation);
				}
			}

			drop(correlations_lock);
		}

		// Use the signal we found
		let random_signal = random_signal.unwrap_or_else(|| panic!("No signals found"));

		// get the correlation id, manifestation id, event id, and signal type from the actual signal
		let random_correlation_id = random_signal.correlation_id.clone();
		let random_manifestation_id = random_signal.manifestation_id.clone();
		let random_event_id = random_signal.event_id.clone();
		let signal_type = random_signal.signal_type.clone();

		let random_correlation = CORRELATIONS_QUEUE.lock().await.iter().find(|((_, _), correlation)| correlation.id == random_correlation_id).map(|(_, correlation)| correlation.clone()).ok_or_else(|| anyhow::anyhow!("No correlation found for signal"))?;
		let random_correlation_error_rate = random_correlation.error_rate.clone();

		// Get the error rate for the specific signal type we're using
		let error_rate = random_correlation_error_rate.get(&signal_type).or_else(|| random_correlation_error_rate.values().next()).cloned().unwrap_or_else(|| 
		signal::Distance { value: BigDecimal::from(0), units: splimes::Resolution::Seconds }
	);

		let timer = std::time::Instant::now();
		println!("Calculating sample signal probability...");

                // get oct. 1st 2025 DateTime<Utc>
                let sample_datetime = Utc.with_ymd_and_hms(2025, 12, 15, 0, 0, 0).unwrap();

		let sig_sum_probability = signals_lock.probability_sum(&random_event_id, &signal_type, sample_datetime).await?.unwrap_or(BigDecimal::from(0));

		println!("Time taken for sample signal probability calculation: {:?}", timer.elapsed());
		println!("Sample signal probability for correlation ID {}, manifestation ID {}, signal type {:?}:", random_correlation_id, random_manifestation_id, signal_type);
		println!("  Sum-based Probability: {:.6} (using error rate: {})", sig_sum_probability, error_rate.value);

		drop(signals_lock);

		println!("Test completed successfully!");
		Ok(())
	}

	#[tokio::test(flavor = "multi_thread")]
	#[serial]
	async fn test_specific_process_batch() -> Result<()> {
		let test_batch = generate_specific_test_batch();

		println!("Generated test batch with {} measurements", test_batch.measurements.len());

		output_denk_format_batch(&test_batch);

		Ok(())
	}

	fn generate_specific_test_batch() -> Batch {
		let measurements = vec![BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(0, 0).unwrap(), BigDecimal::from(0)), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(0), BigDecimal::from(0))), analysis: None }, BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(600, 0).unwrap(), BigDecimal::from_f64(4.03).unwrap()), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(600), BigDecimal::from_f64(4.03).unwrap())), analysis: None }, BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(1200, 0).unwrap(), BigDecimal::from_f64(8.06).unwrap()), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(1200), BigDecimal::from_f64(8.06).unwrap())), analysis: None }, BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(1800, 0).unwrap(), BigDecimal::from_f64(10.88).unwrap()), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(1800), BigDecimal::from_f64(10.88).unwrap())), analysis: None }, BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(2400, 0).unwrap(), BigDecimal::from_f64(11.48).unwrap()), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(2400), BigDecimal::from_f64(11.48).unwrap())), analysis: None }, BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(3000, 0).unwrap(), BigDecimal::from_f64(10.06).unwrap()), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(3000), BigDecimal::from_f64(10.06).unwrap())), analysis: None }, BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(3600, 0).unwrap(), BigDecimal::from_f64(12.57).unwrap()), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(3600), BigDecimal::from_f64(12.57).unwrap())), analysis: None }, BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(4200, 0).unwrap(), BigDecimal::from_f64(6.18).unwrap()), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(4200), BigDecimal::from_f64(6.18).unwrap())), analysis: None }, BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(4800, 0).unwrap(), BigDecimal::from_f64(5.37).unwrap()), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(4800), BigDecimal::from_f64(5.37).unwrap())), analysis: None }, BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(5400, 0).unwrap(), BigDecimal::from_f64(-1.09).unwrap()), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(5400), BigDecimal::from_f64(-1.09).unwrap())), analysis: None }, BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(5940, 0).unwrap(), BigDecimal::from(0)), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(5940), BigDecimal::from(0))), analysis: None }];

		// This test batch has exactly 11 measurements as intended
		let expected_size = 11;
		assert_eq!(measurements.len(), expected_size, "Test batch should have exactly {} measurements", expected_size);

		let dummy_database_info = DatabaseInfo::new("test".to_string(), "/tmp/test".to_string());
		let dummy_aspect = AspectId::new();
		Batch::new(expected_size, measurements, Resolution::Seconds, dummy_aspect, dummy_database_info)
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
		println!("max_x: {}, max_y: {}", pattern.relatives().iter().map(|r| r.vector().location()).max().unwrap_or(&BigDecimal::from(0)), pattern.relatives().iter().map(|r| r.vector().amplitude()).max().unwrap_or(&BigDecimal::from(0)));
	}

	#[allow(dead_code)]
	pub async fn analyze_event_timing() -> Result<()> {
		let events = get_events_queue().await;

		if events.is_empty() {
			println!("No events found in queue");
			return Ok(());
		}

		println!("=== Event Timing Analysis ===");

		for event in &events {
			println!("\nEvent: {}", event.name);

			for (i, (_timing_key, manifestation)) in event.manifestations.iter().enumerate() {
				println!("  Manifestation {}:", i + 1);
				println!("    Start: {}", manifestation.start.format("%Y-%m-%d %H:%M:%S UTC"));
				println!("    End: {}", manifestation.end.format("%Y-%m-%d %H:%M:%S UTC"));
				println!("    Duration: {:.1} days ({:.1} hours)", manifestation.duration_days(), manifestation.duration_hours());
				println!("    Midpoint: {}", manifestation.midpoint().format("%Y-%m-%d %H:%M:%S UTC"));

				// Example: Check if manifestation contains specific dates
				let month_15th = manifestation.start.with_day(15);
				if let Some(mid_month) = month_15th {
					if manifestation.contains(mid_month) {
						println!("    Contains mid-month (15th): Yes");
					}
				}
			}
		}

		// Duration statistics
		let durations: Vec<f64> = events.iter().flat_map(|e| e.manifestations.values()).map(|m| m.duration_days()).collect();

		if !durations.is_empty() {
			let avg_duration = durations.iter().sum::<f64>() / durations.len() as f64;
			let min_duration = durations.iter().fold(f64::INFINITY, |a, &b| a.min(b));
			let max_duration = durations.iter().fold(f64::NEG_INFINITY, |a, &b| a.max(b));

			println!("\n=== Duration Statistics ===");
			println!("Average duration: {:.1} days", avg_duration);
			println!("Minimum duration: {:.1} days", min_duration);
			println!("Maximum duration: {:.1} days", max_duration);
		}

		Ok(())
	}

	/// Retrieves all events from the events queue
	///
	/// This function provides access to all events that have been detected and added
	/// to the global EVENTS_QUEUE. It returns a clone of all events to avoid
	/// blocking the queue for extended periods.
	///
	/// # Returns
	/// A vector containing all events currently in the queue
	#[allow(dead_code)]
	pub async fn get_events_queue() -> Vec<Event> {
		let events_lock = EVENTS_QUEUE.lock().await;
		events_lock.values().cloned().collect()
	}

	/// Clears all events from the events queue
	///
	/// # Returns
	/// Nothing
	#[allow(dead_code)]
	pub async fn clear_events_queue() {
		let mut events_lock = EVENTS_QUEUE.lock().await;
		events_lock.clear();
	}

	/// Detects peak values in the dataset
	///
	/// This function analyzes measurements to detect when the value reaches a local peak
	/// (higher than both previous and next values) AND is the global maximum in the dataset.
	/// Each detected peak creates an event with a manifestation spanning from the rise to the decline.
	///
	/// For the test data (1,2,3,2,3,5,3,2,1,1), this will detect exactly one peak at value 5.
	///
	/// # Parameters
	/// - `database`: Database instance to query measurements from
	/// - `aspect`: The aspect ID to analyze for peak detection
	/// - `resolution`: The resolution for data analysis
	/// - `method`: The spline interpolation method to use
	///
	/// # Algorithm
	/// 1. Retrieve all measurements for the aspect within the date range
	/// 2. Find the global maximum value in the dataset
	/// 3. For each point that equals the global maximum, check if it's a local peak
	/// 4. If it's a local peak, create an event spanning from rise to decline
	/// 5. Add events to the global EVENTS_QUEUE
	async fn create_peak_detection_events(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, name: &str) -> Result<()> {
		let start_time = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
		let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;

		// Use optimized bulk analysis to get all points
		let mut point_stream = database.stream_analyze_range(*aspect, start_time, end_time, *resolution, *method);

		let mut points = Vec::new();
		while let Some(point) = point_stream.try_next().await? {
			points.push(point);
		}

		// Sort points by timestamp to ensure proper chronological order
		points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

		if points.len() < 3 {
			println!("Not enough points for peak detection (need at least 3)");
			return Ok(());
		}

		// Find the global maximum value
		let global_max = points.iter().map(|p| &p.value).max().unwrap();

		let mut event = Event::new(name.to_string(), Some(format!("Detects peak values (local maxima that are also global maximum) for aspect {}", aspect)));

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
		if !event.manifestations.is_empty() {
			let manifestation_count = event.manifestations.len();
			let mut events_lock = EVENTS_QUEUE.lock().await;
			events_lock.insert(event.id.clone(), event);
			drop(events_lock);
			println!("Added peak detection event with {} manifestation(s)", manifestation_count);
		} else {
			println!("No peaks detected in the dataset");
		}

		Ok(())
	}

	#[tokio::test(flavor = "multi_thread")]
	#[serial]
	async fn test_api_precise() -> Result<()> {
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
		build_processed_batch_queue().await?;
		let processed_lock = PROCESSED_BATCHES_QUEUE.lock().await;
		let length = processed_lock.len();
		println!("Processed batches count: {}", length);
		if length > 0 {
			let random_index = rand::random::<usize>() % length;
			println!("Random processed batch: {}", json!(&processed_lock[random_index]));
			output_denk_format_batch(&processed_lock[random_index]);
		}
		drop(processed_lock);

		build_patterns_queue().await?;
		let patterns_lock = PATTERNS_QUEUE.lock().await;
		let length = patterns_lock.len();
		println!("Patterns count: {}", length);
		if length > 0 {
			let random_index = rand::random::<usize>() % length;
			println!("Random pattern: {}", json!(&patterns_lock[random_index]));
			output_denk_format_pattern(&patterns_lock[random_index]);
		}
		drop(patterns_lock);

		#[rustfmt::skip]
                let mut dictionary = Dictionary::new(
                        "Test Dictionary".to_string(), 
                        "A dictionary for testing purposes".to_string(), 
                        DictionaryConstraints { 
                                steps: Some(Steps { 
                                        count: 10,
                                        interpolation: Spline::Linear 
                                }), 
                                variabilities: Some(vec![
                                        VariablilityType::AbsoluteAveragePercentile(Variability { value: BigDecimal::from_f64(10.0).unwrap() }),
                                ]) });

		load_dictionary(&mut dictionary).await?;

		println!("Dictionary now contains {} patterns", dictionary.patterns.len());

		// Show the pattern after dictionary import (should have 10 steps)
		if !dictionary.patterns.is_empty() {
			let first_pattern = &dictionary.patterns[0];
			println!("First pattern has {} relatives", first_pattern.relatives().len());
			println!("Pattern after dictionary import:");
			output_denk_format_pattern(first_pattern);
		}

		// create a peak detection event for testing
		let event_name = "peaks";
		create_peak_detection_events(&db, &aspect.id(), &Resolution::Hours, &Spline::Linear, event_name).await?;

		create_correlations_for_events(&dictionary).await?;
		let correlations_lock = CORRELATIONS_QUEUE.lock().await;
		println!("Number of correlations created: {}", correlations_lock.len());
		drop(correlations_lock);
		create_signals(&dictionary).await?;
		let signals_lock = SIGNALS_QUEUE.lock().await;
		println!("Number of signals created: {}", signals_lock.len());
		drop(signals_lock);
		filter_expired_signals().await?;
		let signals_lock = SIGNALS_QUEUE.lock().await;
		println!("Number of signals after filtering: {}", signals_lock.len());
		drop(signals_lock);

		// Calculate and print the probability for the Signals in the queue
		let events_lock = EVENTS_QUEUE.lock().await;
		let event_id_option = events_lock.keys().next().cloned();
		drop(events_lock);

		let start_time = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();

		if let Some(event_id) = event_id_option {
			let signals_lock = SIGNALS_QUEUE.lock().await;
			let Some(sum_probability) = signals_lock.probability_sum(&event_id, &SignalType::Custom("PredictMid".to_string()), start_time + chrono::Duration::hours(61)).await? else {
				bail!("No signals found for event ID {}", event_id);
			};

			println!("Peak Event Probability - Sum: {}", sum_probability);
			drop(signals_lock);
		}

		Ok(())
	}

	async fn fake_database() -> Database {
		// Cleanup existing test database if it exists
		use std::fs::remove_dir_all;
		let db_path = format!("{}/TestDB", database::DEFAULT_DATA_DIR);
		remove_dir_all(&db_path).ok();

		let db = Database::new("TestDB").await.unwrap();
		let test_subject = db.track_subject("TestSubject").await.unwrap();
		let test_aspect = db.track_aspect(test_subject, "TestAspect", Resolution::Seconds).await.unwrap();
		let start_time = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
		#[rustfmt::skip]
		let points = vec![
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
                        (60, BigDecimal::from(0))
                ];

		for (i, value) in points {
			let timestamp = start_time + chrono::Duration::hours(i as i64);
			let measurement = InputMeasurement::new(timestamp, value);
			db.observe_measurement(test_aspect.clone(), measurement).await.unwrap();
		}

		// create a peak detection event for testing
		create_peak_detection_events(&db, &test_aspect.id(), &Resolution::Hours, &Spline::Linear, "Peak Detection Test").await.unwrap();
		println!("Events in queue after peak detection:");
		let events_lock = EVENTS_QUEUE.lock().await;
		println!("Events in queue: {}", events_lock.len());
		drop(events_lock);

		db
	}

	#[tokio::test(flavor = "multi_thread")]
	#[serial]
	async fn test_batch_processing() -> Result<()> {
		use std::fs::remove_dir_all;
		let db_path = format!("{}/test_bath_processing", database::DEFAULT_DATA_DIR);
		remove_dir_all(&db_path).ok();

		let db = Database::new("test_bath_processing").await.unwrap();
		let test_subject = db.track_subject("TestSubject").await.unwrap();
		let test_aspect = db.track_aspect(test_subject, "TestAspect", Resolution::Seconds).await.unwrap();
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
                        (15, BigDecimal::from(0)), 
                        (16, BigDecimal::from(-1)), 
                        (17, BigDecimal::from(-2)), 
                        (18, BigDecimal::from(-1)), 
                        (19, BigDecimal::from(0)), 
                        (20, BigDecimal::from(1)),
                        (21, BigDecimal::from(2)), 
                        (22, BigDecimal::from(3)), 
                        (23, BigDecimal::from(4)), 
                        (24, BigDecimal::from(5)), 
                        (25, BigDecimal::from(6)), 
                        (26, BigDecimal::from(5)), 
                        (27, BigDecimal::from(4)), 
                        (28, BigDecimal::from(3)), 
                        (29, BigDecimal::from(2)), 
                        (30, BigDecimal::from(1)),
                        (31, BigDecimal::from(0)), 
                        (32, BigDecimal::from(-1)), 
                        (33, BigDecimal::from(-2)), 
                        (34, BigDecimal::from(-3)), 
                        (35, BigDecimal::from(-4)), 
                        (36, BigDecimal::from(-5)), 
                        (37, BigDecimal::from(-4)), 
                        (38, BigDecimal::from(-3)), 
                        (39, BigDecimal::from(-2)), 
                        (40, BigDecimal::from(-1)),
                        (41, BigDecimal::from(0)), 
                        (42, BigDecimal::from(1)), 
                        (43, BigDecimal::from(2)), 
                        (44, BigDecimal::from(3)), 
                        (45, BigDecimal::from(4)), 
                        (46, BigDecimal::from(5)), 
                        (47, BigDecimal::from(6)), 
                        (48, BigDecimal::from(7)), 
                        (49, BigDecimal::from(8)), 
                        (50, BigDecimal::from(9)),
                        (51, BigDecimal::from(10)), 
                        (52, BigDecimal::from(9)), 
                        (53, BigDecimal::from(8)), 
                        (54, BigDecimal::from(7)), 
                        (55, BigDecimal::from(6)), 
                        (56, BigDecimal::from(5)), 
                        (57, BigDecimal::from(4)), 
                        (58, BigDecimal::from(3)), 
                        (59, BigDecimal::from(2)), 
                        (60, BigDecimal::from(1))
                ];

		for (i, value) in points {
			let timestamp = start_time + chrono::Duration::minutes(i as i64);
			let measurement = InputMeasurement::new(timestamp, value);
			db.observe_measurement(test_aspect.clone(), measurement).await.unwrap();
		}

		let aspect = test_aspect;
		let resolution = Resolution::Minutes;
		let method = Spline::Linear;
		let batch_size = 10;

		build_unprocessed_queue(&db, &aspect.id(), &resolution, &method, batch_size).await?;

		println!("Unprocessed batches count: {}", UNPROCESSED_BATCHES_QUEUE.lock().await.len());

		build_processed_batch_queue().await?;

		println!("Processed batches count: {}", PROCESSED_BATCHES_QUEUE.lock().await.len());

		// With 60 points and batch size 10, sliding window creates: 60 - 10 + 1 = 51 overlapping batches
		assert_eq!(PROCESSED_BATCHES_QUEUE.lock().await.len(), 51);

		Ok(())
	}
}
