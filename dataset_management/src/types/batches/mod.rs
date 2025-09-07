use anyhow::{bail, Result};
pub use batch::Batch;
use database::{AspectId, Database};
use futures::stream::StreamExt;
use rayon::{prelude::*, ThreadPoolBuilder};
use serde::{Deserialize, Serialize};
use splimes::{Point, Resolution, Spline};

use crate::BatchedMeasurement;

mod batch;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Batches(Vec<Batch>);

impl Batches {
	/// Create batches from a vector of pre-constructed batches (useful for tests)
	pub fn new_from_vec(batches: Vec<Batch>) -> Self {
		Batches(batches)
	}

	/// Steps to build batches using sliding windows approach
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
	/// This method creates overlapping sliding window batches. For example, with batch_size=3:
	/// Points [A,B,C,D,E,F] become batches: [A,B,C], [B,C,D], [C,D,E], [D,E,F]
	///
	/// 1. get the points for the aspect from the database using `database.analyze_range()`
	/// 2. create sliding window batches of points based on `batch_size`
	///
	pub async fn new(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, batch_size: usize) -> Result<Batches> {
		let start_time = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
		let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;
		let min_resolution = database.get_aspect_resolution(aspect).await?.ok_or_else(|| anyhow::anyhow!("No aspect resolution found"))?;

		if resolution < &min_resolution {
			bail!("Resolution is less than minimum resolution for aspect");
		}

		let stream = database.stream_analyze_range(*aspect, start_time, end_time, *resolution, *method);
		let mut points: Vec<Point> = Vec::new();
		tokio::pin!(stream);
		while let Some(result) = stream.next().await {
			match result {
				Ok(point) => points.push(point),
				Err(e) => return Err(e),
			}
		}

		if batch_size == 0 || points.is_empty() || batch_size > points.len() {
			return Ok(Batches(Vec::new()));
		}

		ThreadPoolBuilder::new().num_threads(num_cpus::get()).build_global().unwrap();

		let batches = points
			.par_windows(batch_size)
			.map(|window| {
				let measurements = window.iter().map(|p| BatchedMeasurement::new(p.clone())).collect();
				Batch::new(window.len(), measurements, *resolution)
			})
			.collect();

		Ok(Batches(batches))
	}

	/// Test-optimized version of `new` that limits the amount of data used
	/// `max_points` - maximum number of points to use from the database
	pub async fn new_with_limited_data(database: &Database, aspect: &AspectId, resolution: &Resolution, method: &Spline, batch_size: usize, max_points: usize) -> Result<Batches> {
		let start_time = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
		let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;
		let min_resolution = database.get_aspect_resolution(aspect).await?.ok_or_else(|| anyhow::anyhow!("No aspect resolution found"))?;

		if resolution < &min_resolution {
			bail!("Resolution is less than minimum resolution for aspect");
		}

		let mut points: Vec<Point> = Database::analyze_range(*aspect, start_time, end_time, *resolution, *method).await?;

		// Limit the number of points for testing performance
		if points.len() > max_points {
			points.truncate(max_points);
		}

		if batch_size == 0 || points.is_empty() || batch_size > points.len() {
			return Ok(Batches(Vec::new()));
		}

		let batches = points
			.windows(batch_size)
			.map(|window| {
				let measurements = window.iter().map(|p| BatchedMeasurement::new(p.clone())).collect();
				Batch::new(window.len(), measurements, *resolution)
			})
			.collect();

		Ok(Batches(batches))
	}

	pub const fn existing(batches: Vec<Batch>) -> Self {
		Batches(batches)
	}

	pub fn is_empty(&self) -> bool {
		self.0.is_empty()
	}

	pub fn len(&self) -> usize {
		self.0.len()
	}

	pub fn first(&self) -> Option<&Batch> {
		self.0.first()
	}

	pub fn get(&self, index: usize) -> Option<&Batch> {
		self.0.get(index)
	}

	pub async fn process_batches(&mut self) -> Result<()> {
		let result: Result<Vec<()>> =
			self.0.par_iter_mut()
				.map(|batch| {
					batch.level_transform()?;
					batch.transpose_origin()?;
					batch.simplify_transformation()?;
					Ok(())
				})
				.collect();
		result?;
		Ok(())
	}
}

impl IntoIterator for Batches {
	type IntoIter = std::vec::IntoIter<Batch>;
	type Item = Batch;

	fn into_iter(self) -> Self::IntoIter {
		self.0.into_iter()
	}
}
