use anyhow::Result;
use database::{
	database::traits::{DatabaseStructure, Inputs, Outputs}, AspectId, Database
};
use futures::StreamExt;
use splimes::{Resolution, Spline};
use tracing::{debug, info};

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
	let mut point_stream = Outputs::analyze_range(database, aspect, start_time, end_time, *resolution, *method).await?;
	let mut all_points = Vec::new();
	while let Some(result) = point_stream.next().await {
		let point = result?;
		all_points.push(point);
	}

	info!(total_points = all_points.len(), batch_size, %start_time, %end_time, "Points streamed for batch creation");

	// Create sliding window batches (overlapping)
	if batch_size > 0 && all_points.len() >= batch_size {
		let database_info = database.get_database_info().await.map_err(|_| anyhow::anyhow!("Database info not available"))?;

		let total_batches = all_points.len() - batch_size + 1;
		let report_interval = std::cmp::max(1000, total_batches / 10); // Report every 1000 or 10% (whichever is larger)
		info!(total_batches, "Creating sliding window batches");

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
