use std::sync::LazyLock;

use anyhow::Result;
use database::{AspectId, Database, Resolution};
use futures::TryStreamExt;
use rayon::prelude::*;
use splimes::Spline;
use tokio::sync::Mutex;
pub use types::*;

mod types;

pub const BATCH_SIZE: [usize; 1] = [100];

static UNPROCESSED_BATCHES_QUEUE: LazyLock<Mutex<Vec<Batch>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static PROCESSED_BATCHES_QUEUE: LazyLock<Mutex<Vec<Batch>>> = LazyLock::new(|| Mutex::new(Vec::new()));

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

#[cfg(test)]
mod tests {
	use bigdecimal::{BigDecimal, FromPrimitive};
	use chrono::{TimeZone, Utc};
	use database::{Database, DatabaseInfo};
	use serde_json::json;
	use splimes::{Point, Spline};

	use super::*;

	#[tokio::test(flavor = "multi_thread")]
	async fn test_build_processed_queue() -> Result<()> {
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
		let length = processed_lock.len();
		if length > 0 {
			let random_index = rand::random::<usize>() % length;
			println!("Random processed batch: {}", json!(&processed_lock[random_index]));
			output_denk_format(&processed_lock[random_index]);
		}

		Ok(())
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_specific_process_batch() -> Result<()> {
		let test_batch = generate_specific_test_batch();

		println!("Generated test batch with {} measurements", test_batch.measurements.len());

		output_denk_format(&test_batch);

		Ok(())
	}

	fn generate_specific_test_batch() -> Batch {
		let measurements = vec![BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(0, 0).unwrap(), BigDecimal::from(0)), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(0), BigDecimal::from(0))), analysis: None }, BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(600, 0).unwrap(), BigDecimal::from_f64(4.03).unwrap()), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(600), BigDecimal::from_f64(4.03).unwrap())), analysis: None }, BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(1200, 0).unwrap(), BigDecimal::from_f64(8.06).unwrap()), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(1200), BigDecimal::from_f64(8.06).unwrap())), analysis: None }, BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(1800, 0).unwrap(), BigDecimal::from_f64(10.88).unwrap()), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(1800), BigDecimal::from_f64(10.88).unwrap())), analysis: None }, BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(2400, 0).unwrap(), BigDecimal::from_f64(11.48).unwrap()), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(2400), BigDecimal::from_f64(11.48).unwrap())), analysis: None }, BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(3000, 0).unwrap(), BigDecimal::from_f64(10.06).unwrap()), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(3000), BigDecimal::from_f64(10.06).unwrap())), analysis: None }, BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(3600, 0).unwrap(), BigDecimal::from_f64(12.57).unwrap()), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(3600), BigDecimal::from_f64(12.57).unwrap())), analysis: None }, BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(4200, 0).unwrap(), BigDecimal::from_f64(6.18).unwrap()), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(4200), BigDecimal::from_f64(6.18).unwrap())), analysis: None }, BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(4800, 0).unwrap(), BigDecimal::from_f64(5.37).unwrap()), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(4800), BigDecimal::from_f64(5.37).unwrap())), analysis: None }, BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(5400, 0).unwrap(), BigDecimal::from_f64(-1.09).unwrap()), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(5400), BigDecimal::from_f64(-1.09).unwrap())), analysis: None }, BatchedMeasurement { active: true, point: Point::new(Utc.timestamp_opt(5940, 0).unwrap(), BigDecimal::from(0)), distance: None, vector: Some(MeasurementVector::new(BigDecimal::from(5940), BigDecimal::from(0))), analysis: None }];

		let dummy_database_info = DatabaseInfo::new("test".to_string(), "/tmp/test".to_string());
		let dummy_aspect = AspectId::new();
		Batch::new(measurements.len(), measurements, Resolution::Seconds, dummy_aspect, dummy_database_info)
	}

	fn output_denk_format(batch: &Batch) {
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
}
