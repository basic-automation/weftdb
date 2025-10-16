use anyhow::Result;
use database::{
	database::traits::{DatabaseStructure, Outputs}, AspectId, Database
};
use futures::StreamExt;
use splimes::{Resolution, Spline};

use crate::{Batch, BatchedMeasurement};

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
	let mut point_stream = Outputs::analyze_range(database, *aspect, start_time, end_time, *resolution, *method).await?;
	let mut all_points = Vec::new();
	while let Some(result) = point_stream.next().await {
		let point = result?;
		all_points.push(point);
	}

	println!("Total points streamed: {}", all_points.len());
	println!("Batch size: {batch_size}");
	println!("Start time: {start_time}, End time: {end_time}");

	// Create sliding window batches (overlapping)
	if batch_size > 0 && all_points.len() >= batch_size {
		let database_info = database.get_database_info().await.ok_or_else(|| anyhow::anyhow!("Database info not available"))?;

		let total_batches = all_points.len() - batch_size + 1;
		println!("Creating {total_batches} sliding window batches...");

		// Collect all batches first, then store them in bulk for better performance
		let mut batches = Vec::with_capacity(total_batches);

		// Create overlapping sliding window batches - each batch has exactly batch_size measurements
		for i in 0..=(all_points.len() - batch_size) {
			let window: &[splimes::Point] = &all_points[i..i + batch_size];
			assert_eq!(window.len(), batch_size, "Window should always have exactly batch_size elements");

			let measurements = window.iter().map(|p| BatchedMeasurement::new(p.clone())).collect();
			let batch = Batch::new(batch_size, measurements, *resolution, *aspect, database_info.clone());
			batches.push(batch);

			// Progress indicator for batch creation
			if total_batches > 1000 && (i + 1) % 1000 == 0 {
				println!("Created {} / {} batches", i + 1, total_batches);
			}
		}

		println!("Storing {} batches in database...", batches.len());
		// Store all batches at once using bulk insert
		database.store_batches(&batches).await?;
	}

	// Note: No longer using global queue length since batches are stored in database
	println!("Batches stored in database successfully");

	Ok(())
}
