use std::sync::LazyLock;

use anyhow::Result;
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use chrono::Datelike;
use database::{AspectId, Database, Resolution};
use futures::TryStreamExt;
use rayon::prelude::*;
use splimes::Spline;
use tokio::sync::Mutex;
pub use types::*;
use uuid::Uuid;

#[cfg(test)]
mod memory_test;
pub mod types;

pub const BATCH_SIZE: [usize; 1] = [100];

static UNPROCESSED_BATCHES_QUEUE: LazyLock<Mutex<Vec<Batch>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static PROCESSED_BATCHES_QUEUE: LazyLock<Mutex<Vec<Batch>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static PATTERNS_QUEUE: LazyLock<Mutex<Vec<Pattern>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static EVENTS_QUEUE: LazyLock<Mutex<Vec<Event>>> = LazyLock::new(|| Mutex::new(Vec::new()));

pub async fn build_unprocessed_queue(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, batch_size: usize) -> Result<()> {
	let start_time = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
	let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;

	// Use optimized bulk analysis without limit
	let mut point_stream = database.stream_analyze_range(*aspect, start_time, end_time, *resolution, *method);

	let mut batch_points = Vec::new();
	while let Some(point) = point_stream.try_next().await? {
		// add batch to batches up to BATCH_SIZE then process batch and clear
		batch_points.push(point);
		if batch_points.len() >= batch_size {
			let measurements = batch_points.iter().map(|p| BatchedMeasurement::new(p.clone())).collect();
			let database_info = database.get_database_info().await.expect("Database info should be available");
			let batch = Batch::new(batch_points.len(), measurements, *resolution, *aspect, database_info);

			let mut batches_lock = UNPROCESSED_BATCHES_QUEUE.lock().await;
			batches_lock.push(batch);
			drop(batches_lock);

			batch_points.clear();
		}
	}

	// Process any remaining points as a final batch
	if !batch_points.is_empty() {
		let measurements = batch_points.iter().map(|p| BatchedMeasurement::new(p.clone())).collect();
		let database_info = database.get_database_info().await.expect("Database info should be available");
		let batch = Batch::new(batch_points.len(), measurements, *resolution, *aspect, database_info);

		let mut batches_lock = UNPROCESSED_BATCHES_QUEUE.lock().await;
		batches_lock.push(batch);
		drop(batches_lock); // Explicitly drop the lock

		batch_points.clear();
	}

	Ok(())
}

pub async fn build_processed_batch_queue() -> Result<()> {
	// print UNPROCESSED_BATCHES_QUEUE length for verification
	let unprocessed_lock = UNPROCESSED_BATCHES_QUEUE.lock().await;
	println!("UNPROCESSED_BATCHES_QUEUE length: {}", unprocessed_lock.len());
	drop(unprocessed_lock);

	let mut processed_batches = UNPROCESSED_BATCHES_QUEUE.lock().await.clone();
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
	let processed_lock = PROCESSED_BATCHES_QUEUE.lock().await;
	let mut patterns_lock = PATTERNS_QUEUE.lock().await;

	for batch in processed_lock.iter() {
		// Generate a new pattern ID for this batch
		let pattern_id = PatternID::new();

		// Get the first and last timestamps from active measurements
		let active_measurements: Vec<_> = batch.measurements().iter().filter(|m| m.is_active()).collect();

		if active_measurements.is_empty() {
			continue; // Skip batches with no active measurements
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
	}

	Ok(())
}

/// Optimized dictionary loading with parallelism, SIMD, and smart batching
/// Single unified function that maintains specification compliance
pub async fn load_dictionary(dictionary: &mut Dictionary) -> Result<()> {
	let patterns_queue = PATTERNS_QUEUE.lock().await;
	let pattern_count = patterns_queue.len();

	// Clone patterns and release lock immediately to free memory
	let patterns: Vec<_> = patterns_queue.iter().cloned().collect();
	drop(patterns_queue);

	let start_time = std::time::Instant::now();
	println!("Loading {} patterns into dictionary (optimized)", pattern_count);

	// Adaptive batch sizing based on pattern count and CPU cores for optimal performance
	let cpu_count = num_cpus::get();
	let batch_size = match pattern_count {
		0..=500 => 10,
		501..=2000 => 25,
		2001..=5000 => 50,
		_ => 100,
	};
	let parallel_batch_size = (batch_size * cpu_count).min(200);

	let mut processed_count = 0;

	// Process patterns in optimized parallel batches
	for batch_patterns in patterns.chunks(parallel_batch_size) {
		// OPTIMIZATION: Parallel similarity checking within each batch
		// Each pattern still checks against ALL existing patterns (specification compliant)
		let batch_results: Vec<_> = batch_patterns
			.par_iter()
			.map(|pattern| {
				// Check similarity against ALL existing patterns using parallel iterator
				let similar_indices: Vec<usize> = dictionary
					.patterns
					.par_iter()
					.enumerate()
					.filter_map(|(idx, existing)| {
						// OPTIMIZATION: Use optimized similarity checking with SIMD where possible
						if dictionary.patterns_are_similar_optimized(pattern, existing).unwrap_or(false) {
							Some(idx)
						} else {
							None
						}
					})
					.collect();
				(pattern.clone(), similar_indices)
			})
			.collect();

		// Apply results sequentially to maintain data consistency
		for (pattern, similar_indices) in batch_results {
			if !similar_indices.is_empty() {
				// Specification requirement: merge with existing similar patterns
				for &idx in &similar_indices {
					if let Err(e) = dictionary.merge_pattern_occurrences_at_index(idx, pattern.clone()) {
						eprintln!("Failed to merge pattern occurrences: {}", e);
					}
				}
			} else {
				// Specification requirement: add as new pattern if no similar patterns found
				dictionary.patterns.push(pattern);
			}
			processed_count += 1;
		}

		// Optimized progress reporting - less frequent to reduce I/O overhead
		if processed_count % 1000 == 0 {
			let elapsed = start_time.elapsed();
			let patterns_per_sec = processed_count as f64 / elapsed.as_secs_f64();
			println!("Processed {} / {} patterns... ({:.2} patterns/sec)", processed_count, pattern_count, patterns_per_sec);
		}

		// Adaptive yielding based on batch size to reduce context switching overhead
		if processed_count % (batch_size * 2) == 0 {
			tokio::task::yield_now().await;
		}
	}

	let final_elapsed = start_time.elapsed();
	let final_rate = pattern_count as f64 / final_elapsed.as_secs_f64();
	println!("Dictionary loading completed: {} patterns processed in {:.2?} ({:.2} patterns/sec)", pattern_count, final_elapsed, final_rate);
	println!("Dictionary now contains {} unique patterns", dictionary.len());
	Ok(())
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
pub async fn build_events_5_percent_queue(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline) -> Result<()> {
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

	let mut events = Vec::new();
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
	for ((year, month), month_points) in months_data {
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

		// If increase is 5% or more, create an event
		if percentage_increase >= threshold_percentage {
			let database_info = database.get_database_info().await.expect("Database info should be available");

			let manifestation = Manifestation::new(
				database_info.id().as_uuid(),
				sorted_month_points[0].timestamp, // Start of the month
				end_timestamp,                    // End of the month
			);

			let event = Event::new(
				Uuid::new_v4(),
				format!("5% Monthly Price Increase - {}-{:02}", year, month),
				Some(format!("Price increased {:.2}% from {} to {} during {}-{:02} (from start to end of month, duration: {:.1} days)", percentage_increase.to_f64().unwrap_or(0.0) * 100.0, start_price.to_f64().unwrap_or(0.0), end_price.to_f64().unwrap_or(0.0), year, month, manifestation.duration_days())),
				vec![manifestation],
				vec![], // Empty correlations initially - these would be populated later during pattern correlation
			);

			events.push(event);
		}
	}

	// Add all detected events to the global queue
	let mut events_lock = EVENTS_QUEUE.lock().await;
	events_lock.extend(events);
	drop(events_lock);

	Ok(())
}

#[cfg(test)]
mod tests {
	use bigdecimal::{BigDecimal, FromPrimitive};
	use chrono::{TimeZone, Utc};
	use database::{Database, DatabaseInfo};
	use serde_json::json;
	use serial_test::serial;
	use splimes::{Point, Spline};

	use super::*;

	#[tokio::test(flavor = "multi_thread")]
	#[serial]
	async fn test_load_dictionary() -> Result<()> {
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
                                                value: BigDecimal::from(1) 
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

		build_events_5_percent_queue(&database, &aspect.id(), &resolution, &method).await?;

		println!("Time taken for build_events_5_percent_queue: {:?}", timer.elapsed());
		analyze_event_timing().await?;

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

		let dummy_database_info = DatabaseInfo::new("test".to_string(), "/tmp/test".to_string());
		let dummy_aspect = AspectId::new();
		Batch::new(measurements.len(), measurements, Resolution::Seconds, dummy_aspect, dummy_database_info)
	}

	fn output_denk_format_batch(batch: &Batch) {
		println!("----- DENK FORMAT OUTPUT BEGIN -----");
		let mut count = 0;
		for measurement in batch.clone().into_iter() {
			if (count == 0 || count == batch.size() - 1) || count % 1 == 0 {
				let vector = measurement.vector().unwrap();
				let amplitude = vector.amplitude().round(2);
				println!("{} {}", vector.location(), amplitude);
			}
			count += 1;
		}
		println!("----- DENK FORMAT OUTPUT END -----");
	}

	fn output_denk_format_pattern(pattern: &Pattern) {
		println!("----- DENK FORMAT OUTPUT BEGIN -----");
		println!("Pattern ID: {}", pattern.id());
		for relative in pattern.relatives() {
			println!("{} {}", relative.vector().location(), relative.vector().amplitude().round(2));
		}
		println!("----- DENK FORMAT OUTPUT END -----");
		println!("max_x: {}, max_y: {}", pattern.relatives().iter().map(|r| r.vector().location()).max().unwrap_or(&BigDecimal::from(0)), pattern.relatives().iter().map(|r| r.vector().amplitude()).max().unwrap_or(&BigDecimal::from(0)));
	}

	#[tokio::test]
	#[serial]
	async fn test_build_events_5_percent_queue() {
		use chrono::{TimeZone, Utc};
		use database::InputMeasurement;

		// Clear the events queue before test
		{
			let mut events_lock = EVENTS_QUEUE.lock().await;
			events_lock.clear();
		}

		// Create test database with unique name
		let test_db_name = format!("test_events_db_{}", uuid::Uuid::new_v4().to_string().replace('-', "_"));

		// Clean up any existing database first
		let _ = std::fs::remove_dir_all(format!("data/{}", test_db_name));

		let db = Database::new(&test_db_name).await.expect("Failed to create database");

		// Create test subject and aspect
		let subject = db.track_subject("test_subject").await.expect("Failed to create subject");
		let aspect = db.track_aspect(subject, "price", splimes::Resolution::Hours).await.expect("Failed to create aspect");

		// Create test measurements that show monthly price increases
		// Month 1: January 2024 - 6% increase from start to end (should trigger event)
		// Month 2: February 2024 - 3% increase from start to end (should NOT trigger event)
		// Month 3: March 2024 - 8% increase from start to end (should trigger event)
		let measurements = vec![
			// January 2024 - starts at 100, ends at 106 (6% increase)
			(Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(), BigDecimal::from_f64(100.0).unwrap()),
			(Utc.with_ymd_and_hms(2024, 1, 15, 12, 0, 0).unwrap(), BigDecimal::from_f64(103.0).unwrap()),
			(Utc.with_ymd_and_hms(2024, 1, 31, 23, 59, 59).unwrap(), BigDecimal::from_f64(106.0).unwrap()),
			// February 2024 - starts at 105, ends at 108.15 (3% increase)
			(Utc.with_ymd_and_hms(2024, 2, 1, 0, 0, 0).unwrap(), BigDecimal::from_f64(105.0).unwrap()),
			(Utc.with_ymd_and_hms(2024, 2, 15, 12, 0, 0).unwrap(), BigDecimal::from_f64(107.0).unwrap()),
			(Utc.with_ymd_and_hms(2024, 2, 29, 23, 59, 59).unwrap(), BigDecimal::from_f64(108.15).unwrap()),
			// March 2024 - starts at 110, ends at 118.8 (8% increase)
			(Utc.with_ymd_and_hms(2024, 3, 1, 0, 0, 0).unwrap(), BigDecimal::from_f64(110.0).unwrap()),
			(Utc.with_ymd_and_hms(2024, 3, 15, 12, 0, 0).unwrap(), BigDecimal::from_f64(115.0).unwrap()),
			(Utc.with_ymd_and_hms(2024, 3, 31, 23, 59, 59).unwrap(), BigDecimal::from_f64(118.8).unwrap()),
		];

		// Insert measurements into database
		for (timestamp, value) in measurements {
			let input_measurement = InputMeasurement::new(timestamp, value);
			db.observe_measurement(aspect.clone(), input_measurement).await.expect("Failed to insert measurement");
		}

		// Run the build_events_5_percent_queue function
		let result = build_events_5_percent_queue(&db, &aspect.id(), &splimes::Resolution::Hours, &Spline::Linear).await;
		assert!(result.is_ok(), "build_events_5_percent_queue should succeed");

		// Check that events were created
		let events_lock = EVENTS_QUEUE.lock().await;

		// We should have exactly 2 events: January (6%) and March (8%), but not February (3%)
		let events_count = events_lock.len();
		assert_eq!(events_count, 2, "Should have exactly 2 events for months with 5%+ increases, got {}", events_count);

		// Verify event structure
		let first_event = &events_lock[0];
		assert!(first_event.name.contains("5% Monthly Price Increase"), "Event name should indicate it's a monthly price increase");
		assert_eq!(first_event.manifestations.len(), 1, "Event should have exactly one manifestation");
		assert!(first_event.description.is_some(), "Event should have a description");

		// Verify manifestation has proper start and end timestamps
		let manifestation = &first_event.manifestations[0];
		assert!(manifestation.start < manifestation.end, "Manifestation start should be before end");
		assert!(manifestation.duration_days() > 0.0, "Manifestation should have positive duration");

		// The manifestation should span roughly a month (between 28-31 days)
		let duration_days = manifestation.duration_days();
		assert!(duration_days >= 28.0 && duration_days <= 31.0, "Duration should be roughly a month, got {:.1} days", duration_days);

		// Verify the events are for the correct months
		let event_names: Vec<&String> = events_lock.iter().map(|e| &e.name).collect();
		assert!(event_names.iter().any(|name| name.contains("2024-01")), "Should have event for January 2024");
		assert!(event_names.iter().any(|name| name.contains("2024-03")), "Should have event for March 2024");
		assert!(!event_names.iter().any(|name| name.contains("2024-02")), "Should NOT have event for February 2024");

		println!("Successfully detected {} monthly price increase events", events_count);
		for event in events_lock.iter() {
			println!("  Event: {}", event.name);
			if let Some(manifestation) = event.manifestations.first() {
				println!("    Duration: {:.1} days", manifestation.duration_days());
				println!("    Start: {}", manifestation.start.format("%Y-%m-%d %H:%M:%S"));
				println!("    End: {}", manifestation.end.format("%Y-%m-%d %H:%M:%S"));
			}
		}

		// Clean up
		drop(events_lock);
		let _ = std::fs::remove_dir_all(format!("data/{}", test_db_name));
	}

	#[tokio::test]
	#[serial]
	async fn test_analyze_event_timing() {
		use chrono::{TimeZone, Utc};
		use database::InputMeasurement;

		// Clear the events queue and populate with test data
		{
			let mut events_lock = EVENTS_QUEUE.lock().await;
			events_lock.clear();
		}

		// Create test database with unique name
		let test_db_name = format!("test_timing_db_{}", uuid::Uuid::new_v4().to_string().replace('-', "_"));
		let _ = std::fs::remove_dir_all(format!("data/{}", test_db_name));

		let db = Database::new(&test_db_name).await.expect("Failed to create database");
		let subject = db.track_subject("test_subject").await.expect("Failed to create subject");
		let aspect = db.track_aspect(subject, "price", splimes::Resolution::Hours).await.expect("Failed to create aspect");

		// Create one month with 6% increase
		let measurements = vec![(Utc.with_ymd_and_hms(2024, 4, 1, 0, 0, 0).unwrap(), BigDecimal::from_f64(100.0).unwrap()), (Utc.with_ymd_and_hms(2024, 4, 30, 23, 59, 59).unwrap(), BigDecimal::from_f64(106.0).unwrap())];

		for (timestamp, value) in measurements {
			let input_measurement = InputMeasurement::new(timestamp, value);
			db.observe_measurement(aspect.clone(), input_measurement).await.expect("Failed to insert measurement");
		}

		// Generate events
		build_events_5_percent_queue(&db, &aspect.id(), &splimes::Resolution::Hours, &Spline::Linear).await.expect("Failed to build events");

		// Test the timing analysis function
		let result = analyze_event_timing().await;
		assert!(result.is_ok(), "analyze_event_timing should succeed");

		// Verify we have events to analyze
		let events = get_events_queue().await;
		assert!(!events.is_empty(), "Should have events to analyze");

		// Clean up
		let _ = std::fs::remove_dir_all(format!("data/{}", test_db_name));
	}

	async fn analyze_event_timing() -> Result<()> {
		let events = get_events_queue().await;

		if events.is_empty() {
			println!("No events found in queue");
			return Ok(());
		}

		println!("=== Event Timing Analysis ===");

		for event in &events {
			println!("\nEvent: {}", event.name);

			for (i, manifestation) in event.manifestations.iter().enumerate() {
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
		let durations: Vec<f64> = events.iter().flat_map(|e| e.manifestations.iter()).map(|m| m.duration_days()).collect();

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
	async fn get_events_queue() -> Vec<Event> {
		let events_lock = EVENTS_QUEUE.lock().await;
		events_lock.clone()
	}

	/// Clears all events from the events queue
	///
	/// This function removes all events from the global EVENTS_QUEUE.
	/// Useful for testing or when starting fresh event detection.
	async fn clear_events_queue() {
		let mut events_lock = EVENTS_QUEUE.lock().await;
		events_lock.clear();
	}
}
