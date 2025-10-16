use anyhow::{bail, Result};
pub use batch::Batch;
use futures::stream::StreamExt;
use num_cpus::get as get_num_cpus;
use rayon::{prelude::*, ThreadPoolBuilder};
use serde::{Deserialize, Serialize};

use crate::{
	database::traits::DatabaseStructure, types::{database::traits::ouputs::Outputs, BatchedMeasurement}, AspectId, Database
};

mod batch;
pub mod batched_measurements;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::unsafe_derive_deserialize)]
pub struct Batches(Vec<Batch>);

impl Batches {
	/// Create batches from a vector of pre-constructed batches (useful for tests)
	#[must_use]
	pub const fn new_from_vec(batches: Vec<Batch>) -> Self {
		Self(batches)
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
	/// This method creates overlapping sliding window batches. For example, with `batch_size=3`:
	/// Points [A,B,C,D,E,F] become batches: [A,B,C], [B,C,D], [C,D,E], [D,E,F]
	///
	/// 1. get the points for the aspect from the database using `database.analyze_range()`
	/// 2. create sliding window batches of points based on `batch_size`
	///
	/// # Errors
	///
	/// Returns an error if no measurements are found, resolution is invalid, or database operations fail.
	///
	/// # Panics
	///
	/// Panics if the global thread pool cannot be built or if database info is not available.
	pub async fn new(database: &Database, aspect: &AspectId, resolution: &splimes::Resolution, method: &splimes::Spline, batch_size: usize) -> Result<Self> {
		let start_time = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
		let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;
		let min_resolution = database.get_aspect_resolution(aspect).await?.ok_or_else(|| anyhow::anyhow!("No aspect resolution found"))?;

		if resolution < &min_resolution {
			bail!("Resolution is less than minimum resolution for aspect");
		}

		let mut stream = Outputs::analyze_range(database, *aspect, start_time, end_time, *resolution, *method).await?;
		let mut points: Vec<splimes::Point> = Vec::new();
		while let Some(result) = stream.next().await {
			match result {
				Ok(point) => points.push(point),
				Err(e) => return Err(e),
			}
		}

		if batch_size == 0 || points.is_empty() || batch_size > points.len() {
			return Ok(Self(Vec::new()));
		}

		ThreadPoolBuilder::new().num_threads(get_num_cpus()).build_global().unwrap();

		let database_info = database.get_database_info().await.expect("Database info should be available");

		let batches = points
			.par_windows(batch_size)
			.map(|window| {
				assert_eq!(window.len(), batch_size, "Sliding window should always have exactly batch_size elements");
				let measurements = window.iter().map(|p| BatchedMeasurement::new(p.clone())).collect();
				Batch::new(batch_size, measurements, *resolution, *aspect, database_info.clone())
			})
			.collect();

		Ok(Self(batches))
	}

	/// Test-optimized version of `new` that limits the amount of data used
	/// `max_points` - maximum number of points to use from the database
	///
	/// # Errors
	///
	/// Returns an error if no measurements are found, resolution is invalid, or database operations fail.
	///
	/// # Panics
	///
	/// Panics if database info is not available.
	pub async fn new_with_limited_data(database: &Database, aspect: &AspectId, resolution: &splimes::Resolution, method: &splimes::Spline, batch_size: usize, max_points: usize) -> Result<Self> {
		let start_time = database.get_earliest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No earliest measurement found"))?;
		let end_time = database.get_latest_measurement(aspect).await?.ok_or_else(|| anyhow::anyhow!("No latest measurement found"))?;
		let min_resolution = database.get_aspect_resolution(aspect).await?.ok_or_else(|| anyhow::anyhow!("No aspect resolution found"))?;

		if resolution < &min_resolution {
			bail!("Resolution is less than minimum resolution for aspect");
		}

		let mut stream = Outputs::analyze_range(database, *aspect, start_time, end_time, *resolution, *method).await?;
		let mut points: Vec<splimes::Point> = Vec::new();
		while let Some(result) = stream.next().await {
			match result {
				Ok(point) => {
					points.push(point);
					if points.len() >= max_points {
						break;
					}
				}
				Err(e) => return Err(e),
			}
		}

		// Ensure we obey the max_points limit even if stream produced more points before break condition
		if points.len() > max_points {
			points.truncate(max_points);
		}

		if batch_size == 0 || points.is_empty() || batch_size > points.len() {
			return Ok(Self(Vec::new()));
		}

		let database_info = database.get_database_info().await.expect("Database info should be available");

		let batches = points
			.windows(batch_size)
			.map(|window| {
				assert_eq!(window.len(), batch_size, "Sliding window should always have exactly batch_size elements");
				let measurements = window.iter().map(|p| BatchedMeasurement::new(p.clone())).collect();
				Batch::new(batch_size, measurements, *resolution, *aspect, database_info.clone())
			})
			.collect();

		Ok(Self(batches))
	}

	#[must_use]
	pub const fn existing(batches: Vec<Batch>) -> Self {
		Self(batches)
	}

	#[must_use]
	pub const fn is_empty(&self) -> bool {
		self.0.is_empty()
	}

	#[must_use]
	pub const fn len(&self) -> usize {
		self.0.len()
	}

	#[must_use]
	pub fn first(&self) -> Option<&Batch> {
		self.0.first()
	}

	#[must_use]
	pub fn get(&self, index: usize) -> Option<&Batch> {
		self.0.get(index)
	}

	/// Processes all batches by applying transformations.
	///
	/// # Errors
	///
	/// Returns an error if any batch transformation fails.
	pub fn process_batches(&mut self) -> Result<()> {
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
