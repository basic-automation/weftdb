use anyhow::{bail, Result};
use database::{AspectId, Database, Point, Resolution};
use splimes::Spline;
pub use types::*;

mod types;

/// Steps to build a batch
///
/// *parameters*
/// `aspect` - the aspect to analyze
/// `resolution` - the resolution to use for analysis
/// `method` - the spline method to use for interpolation
/// `batch_size` - the number of points to analyze in each batch
///
/// *variables*
/// `start_time` - find aspect start time via `database.get_earliest_measurement()`
/// `end_time` - find aspect end time via `database.get_latest_measurement()`
/// `min_resolution` - find aspect resolution via `database.get_aspect_resolution()`
/// (if resolution is less than `min_resolution` return error)
///
///
/// 1. get the points for the aspect from the database using `database.analyze_range()`
/// 2. create batches of points based on `batch_size`
///
async fn create_batches(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, batch_size: usize) -> Result<Vec<Batch>> {
	let start_time = database.get_earliest_measurement(aspect).await??;
	let end_time = database.get_latest_measurement(aspect).await??;
	let min_resolution = database.get_aspect_resolution(aspect).await??;

	if resolution < &min_resolution {
		return bail!("Resolution is less than minimum resolution for aspect");
	}

	let points: Vec<Point> = database.analyze_range(aspect, start_time, end_time, resolution).await?;

	let batch = points
		.chunks(batch_size)
		.map(|chunk| {
			let measurements = chunk.iter().map(|p| BatchedMeasurement::new(p.clone())).collect();
			Batch::new(chunk.len(), measurements, resolution.clone())
		})
		.collect();
}
