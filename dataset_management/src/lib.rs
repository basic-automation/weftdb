use std::sync::LazyLock;

use anyhow::Result;
use database::{AspectId, Database, Resolution};
use futures::TryStreamExt;
use rayon::prelude::*;
use splimes::Spline;
use tokio::sync::Mutex;
pub use types::*;

#[cfg(test)]
mod memory_test;
pub mod types;

pub const BATCH_SIZE: [usize; 1] = [100];

static UNPROCESSED_BATCHES_QUEUE: LazyLock<Mutex<Vec<Batch>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static PROCESSED_BATCHES_QUEUE: LazyLock<Mutex<Vec<Batch>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static PATTERNS_QUEUE: LazyLock<Mutex<Vec<Pattern>>> = LazyLock::new(|| Mutex::new(Vec::new()));

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
#[cfg(test)]
mod tests {
	use bigdecimal::{BigDecimal, FromPrimitive};
	use chrono::{TimeZone, Utc};
	use database::{Database, DatabaseInfo};
	use serde_json::json;
	use splimes::{Point, Spline};

	use super::*;

	#[tokio::test(flavor = "multi_thread")]
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
 

                // print the dictionary
                println!("Dictionary contents: {}", json!(&dictionary));

		Ok(())
	}

	#[tokio::test(flavor = "multi_thread")]
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
}
