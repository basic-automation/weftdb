use std::sync::LazyLock;
use tokio::sync::Mutex;
use anyhow::Result;
use crate::Batch;
use database::Database;
use futures::TryStreamExt;
use splimes::Spline;
use splimes::Resolution;
use database::AspectId;
use crate::BatchedMeasurement;


pub static UNPROCESSED_BATCHES_QUEUE: LazyLock<Mutex<Vec<Batch>>> = LazyLock::new(|| Mutex::new(Vec::new()));


pub async fn build_unprocessed_queue(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, batch_size: usize) -> Result<()> {
	let start_time = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
	let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;

	// Collect all points first to create sliding window batches
	let mut point_stream = database.stream_analyze_range(*aspect, start_time, end_time, *resolution, *method);
	let mut all_points = Vec::new();
	while let Some(point) = point_stream.try_next().await? {
		all_points.push(point);
	}

	println!("Total points streamed: {}", all_points.len());
	println!("Batch size: {}", batch_size);
	println!("Start time: {}, End time: {}", start_time, end_time);

	// Create sliding window batches (overlapping)
	if batch_size > 0 && all_points.len() >= batch_size {
		let database_info = database.get_database_info().await.expect("Database info should be available");

		// Create overlapping sliding window batches - each batch has exactly batch_size measurements
		for i in 0..=(all_points.len() - batch_size) {
			let window = &all_points[i..i + batch_size];
			assert_eq!(window.len(), batch_size, "Window should always have exactly batch_size elements");

			let measurements = window.iter().map(|p| BatchedMeasurement::new(p.clone())).collect();
			let batch = Batch::new(batch_size, measurements, *resolution, *aspect, database_info.clone());

			let mut batches_lock = UNPROCESSED_BATCHES_QUEUE.lock().await;
			batches_lock.push(batch);
			drop(batches_lock);
		}
	}

	let final_lock = UNPROCESSED_BATCHES_QUEUE.lock().await;
	println!("Final UNPROCESSED_BATCHES_QUEUE length: {}", final_lock.len());
	drop(final_lock);

	Ok(())
}