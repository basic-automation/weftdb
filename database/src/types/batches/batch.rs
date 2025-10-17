use std::fmt::Display;

use anyhow::{bail, Result};
use bigdecimal::{BigDecimal, FromPrimitive, Zero};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
	types::{Analysis, BatchDistance, BatchedMeasurement, MeasurementVector, Relative}, AspectId, DatabaseInfo
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BatchId(Uuid);

impl BatchId {
	#[must_use]
	pub fn new() -> Self {
		Self(Uuid::new_v4())
	}

	#[must_use]
	pub const fn from_uuid(uuid: Uuid) -> Self {
		Self(uuid)
	}

	#[must_use]
	pub const fn as_uuid(&self) -> Uuid {
		self.0
	}
}

impl Default for BatchId {
	fn default() -> Self {
		Self::new()
	}
}

impl Display for BatchId {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchMetatdata {
	pub aspect: AspectId,
	pub resolution: splimes::Resolution,
	pub size: usize,
	pub database_info: DatabaseInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Batch {
	pub metadata: BatchMetatdata,
	pub measurements: Vec<BatchedMeasurement>,
	pub batch_id: BatchId,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub batch_hash: Option<String>,
}

impl Batch {
	/// Creates a new batch with the specified measurements and metadata.
	///
	/// # Panics
	///
	/// Panics if the number of measurements does not equal the expected size.
	#[must_use]
	pub fn new(size: usize, measurements: Vec<BatchedMeasurement>, resolution: splimes::Resolution, aspect: AspectId, database_info: DatabaseInfo) -> Self {
		// Validate that measurements vector has exactly the expected size
		assert_eq!(measurements.len(), size, "Batch measurements count ({}) must equal expected size ({})", measurements.len(), size);

		let metadata = BatchMetatdata { aspect, resolution, size, database_info };
		let mut batch = Self { metadata, measurements, batch_id: BatchId::new(), batch_hash: None };
		batch.initialize_measurement_vectors().expect("Failed to initialize measurement vectors");
		batch.initialize_distances().expect("Failed to initialize distances");
		batch
	}

	#[must_use]
	pub const fn size(&self) -> usize {
		self.metadata.size
	}

	#[must_use]
	pub const fn measurements(&self) -> &Vec<BatchedMeasurement> {
		&self.measurements
	}

	pub fn set_measurements(&mut self, measurements: Vec<BatchedMeasurement>) {
		self.measurements = measurements;
	}

	pub fn add_measurement(&mut self, measurement: BatchedMeasurement) {
		self.measurements.push(measurement);
	}

	// Updated: since batch_id is no longer optional, return &BatchId directly
	#[must_use]
	pub const fn batch_id(&self) -> &BatchId {
		&self.batch_id
	}

	// Updated: set the batch_id directly (not optional anymore)
	pub fn set_batch_id(&mut self, batch_id: BatchId) {
		self.batch_id = batch_id;
	}

	#[must_use]
	pub const fn batch_hash(&self) -> Option<&String> {
		self.batch_hash.as_ref()
	}

	pub fn set_batch_hash(&mut self, batch_hash: Option<String>) {
		self.batch_hash = batch_hash;
	}

	/// Returns the amplitudes of all measurement vectors in the batch.
	///
	/// # Errors
	///
	/// Returns an error if any measurement has an invalid timestamp that cannot be converted to `BigDecimal`.
	pub fn get_vector_amplitudes(&self) -> Result<Vec<BigDecimal>> {
		let mut vectors: Vec<MeasurementVector> = Vec::with_capacity(self.measurements.len());
		for measurement in &self.measurements {
			let vector = if let Some(v) = measurement.vector() {
				v
			} else {
				let Some(location) = BigDecimal::from_i64(measurement.point().timestamp.timestamp()) else {
					bail!("Invalid timestamp for measurement");
				};
				&MeasurementVector::new(location, measurement.point().value.clone())
			};
			vectors.push(vector.clone());
		}
		Ok(vectors.into_iter().map(|v| v.amplitude().clone()).collect())
	}

	/// Initializes measurement vectors for all measurements in the batch.
	///
	/// # Errors
	///
	/// Returns an error if the batch is empty or if any measurement has an invalid timestamp.
	pub fn initialize_measurement_vectors(&mut self) -> Result<()> {
		if self.measurements.is_empty() {
			bail!("Batch is empty, cannot initialize measurement vectors");
		}

		for measurement in &mut self.measurements {
			if measurement.vector().is_none() {
				let Some(location) = BigDecimal::from_i64(measurement.point().timestamp.timestamp()) else {
					bail!("Invalid timestamp for measurement");
				};
				let vector = MeasurementVector::new(location, measurement.point().value.clone());
				measurement.set_vector(vector);
			}
		}

		Ok(())
	}

	/// Returns the locations of all measurement vectors in the batch.
	///
	/// # Errors
	///
	/// Returns an error if any measurement has an invalid timestamp that cannot be converted to `BigDecimal`.
	pub fn get_vector_locations(&self) -> Result<Vec<BigDecimal>> {
		let mut vectors: Vec<MeasurementVector> = Vec::with_capacity(self.measurements.len());
		for measurement in &self.measurements {
			let vector = if let Some(v) = measurement.vector() {
				v
			} else {
				let Some(location) = BigDecimal::from_i64(measurement.point().timestamp.timestamp()) else {
					bail!("Invalid timestamp for measurement");
				};
				&MeasurementVector::new(location, measurement.point().value.clone())
			};
			vectors.push(vector.clone());
		}
		Ok(vectors.into_iter().map(|v| v.location().clone()).collect())
	}

	/// Applies level transformation to the batch measurements.
	///
	/// # Errors
	///
	/// Returns an error if the batch is empty or if any measurement lacks required vector or distance data.
	pub fn level_transform(&mut self) -> Result<()> {
		if self.measurements.is_empty() {
			bail!("Batch is empty, cannot perform level transform");
		}

		// Apply positive level transform first
		self.positive_level_transform()?;
		// Then apply negative level transform using updated amplitudes
		self.negative_level_transform()?;

		Ok(())
	}

	fn positive_level_transform(&mut self) -> Result<()> {
		let amplitudes = self.get_vector_amplitudes()?;
		if amplitudes.is_empty() {
			bail!("No amplitudes found in the batch");
		}

		let amplitude_0_movement = BigDecimal::from(0) - &amplitudes[0];

		for (i, measurement) in self.measurements.iter_mut().enumerate() {
			let negative_distance = match measurement.distance() {
				Some(d) => d.negative(),
				None => bail!("Distance is not set for all measurements in the batch"),
			};

			let new_amplitude = &amplitudes[i] + (&amplitude_0_movement * negative_distance);

			// Get mutable reference to avoid cloning
			let Some(vector) = measurement.vector_mut() else {
				bail!("Measurement vector is not set for all measurements in the batch");
			};
			vector.set_amplitude(new_amplitude);
		}

		Ok(())
	}

	fn negative_level_transform(&mut self) -> Result<()> {
		// Get the current amplitudes (after positive transform)
		let amplitudes = self.get_vector_amplitudes()?;
		if amplitudes.is_empty() {
			bail!("No amplitudes found in the batch");
		}

		let amplitude_last_movement = BigDecimal::from(0) - &amplitudes[amplitudes.len() - 1];

		for (i, measurement) in self.measurements.iter_mut().enumerate() {
			let positive_distance = match measurement.distance() {
				Some(d) => d.positive(),
				None => bail!("Distance is not set for all measurements in the batch"),
			};

			let new_amplitude = &amplitudes[i] + (&amplitude_last_movement * positive_distance);

			// Get mutable reference to avoid cloning
			let Some(vector) = measurement.vector_mut() else {
				bail!("Measurement vector is not set for all measurements in the batch");
			};
			vector.set_amplitude(new_amplitude);
		}

		Ok(())
	}

	/// Transposes the origin of measurement vectors to start from zero.
	///
	/// # Errors
	///
	/// Returns an error if no locations are found or if any measurement vector is not set.
	pub fn transpose_origin(&mut self) -> Result<()> {
		let locations = self.get_vector_locations()?;
		if locations.is_empty() {
			bail!("No locations found in the batch");
		}

		let location_0_movement = BigDecimal::from(0) - &locations[0];

		for (i, measurement) in self.measurements.iter_mut().enumerate() {
			let new_location = &locations[i] + &location_0_movement;

			// Get mutable reference to avoid cloning
			let Some(vector) = measurement.vector_mut() else {
				bail!("Measurement vector is not set for all measurements in the batch");
			};
			vector.set_location(new_location);
		}

		Ok(())
	}

	/// Simplifies the batch by removing measurements that don't represent significant trend changes.
	///
	/// # Errors
	///
	/// Returns an error if the batch is empty or if any measurement vector is not set.
	///
	/// # Panics
	///
	/// Panics if the last measurement cannot be accessed (should not happen with proper length checks).
	pub fn simplify_transformation(&mut self) -> Result<()> {
		// Skip the expensive O(n²) trend_analysis since we only need consecutive slopes
		// self.trend_analysis()?;

		if self.measurements.is_empty() {
			bail!("Batch is empty, cannot perform simplification");
		}

		if self.measurements.len() <= 2 {
			// Nothing to simplify with 2 or fewer points
			return Ok(());
		}

		// Optimized O(n) single-pass algorithm instead of O(n²) removal-in-place
		let mut kept_measurements = Vec::with_capacity(self.measurements.len());

		// Always keep the first measurement
		kept_measurements.push(self.measurements[0].clone());

		// Process middle measurements
		for i in 1..self.measurements.len() - 1 {
			let prev_vector = self.measurements[i - 1].vector().ok_or_else(|| anyhow::anyhow!("Previous measurement vector not set"))?;
			let curr_vector = self.measurements[i].vector().ok_or_else(|| anyhow::anyhow!("Current measurement vector not set"))?;
			let next_vector = self.measurements[i + 1].vector().ok_or_else(|| anyhow::anyhow!("Next measurement vector not set"))?;

			// Calculate slope from previous to current
			let prev_to_curr_slope = if curr_vector.location() == prev_vector.location() { BigDecimal::from(0) } else { (curr_vector.amplitude() - prev_vector.amplitude()) / (curr_vector.location() - prev_vector.location()) };

			// Calculate slope from current to next
			let curr_to_next_slope = if next_vector.location() == curr_vector.location() { BigDecimal::from(0) } else { (next_vector.amplitude() - curr_vector.amplitude()) / (next_vector.location() - curr_vector.location()) };

			// Keep the measurement if slopes have different signs (indicating a trend change)
			if prev_to_curr_slope.sign() != curr_to_next_slope.sign() {
				kept_measurements.push(self.measurements[i].clone());
			}
		}

		// Always keep the last measurement
		if self.measurements.len() > 1 {
			kept_measurements.push(self.measurements.last().unwrap().clone());
		}

		// Replace the original measurements with the simplified version
		self.measurements = kept_measurements;

		Ok(())
	}

	/// Performs relative analysis on the batch measurements.
	///
	/// # Errors
	///
	/// Returns an error if the batch is empty, no valid locations/amplitudes found, or if `max_x/max_y` is zero.
	pub fn relative_analysis(&mut self) -> Result<()> {
		if self.measurements.is_empty() {
			bail!("Batch is empty, cannot perform relative analysis");
		}

		// Get all locations and amplitudes
		let locations: Vec<BigDecimal> = self.measurements.iter().filter_map(|m| m.get_vector_location().cloned()).collect();

		let amplitudes: Vec<BigDecimal> = self.measurements.iter().filter_map(|m| m.get_vector_amplitude().cloned()).collect();

		if locations.is_empty() || amplitudes.is_empty() {
			bail!("No valid locations or amplitudes found in measurements");
		}

		// Calculate max_x as the difference between highest and lowest location values
		let min_location = locations.iter().min().ok_or_else(|| anyhow::anyhow!("Failed to find min location"))?.clone();
		let max_location = locations.iter().max().ok_or_else(|| anyhow::anyhow!("Failed to find max location"))?.clone();
		let max_x = max_location - min_location;

		// Calculate max_y as the difference between highest and lowest amplitude values
		let min_amplitude = amplitudes.iter().min().ok_or_else(|| anyhow::anyhow!("Failed to find min amplitude"))?.clone();
		let max_amplitude = amplitudes.iter().max().ok_or_else(|| anyhow::anyhow!("Failed to find max amplitude"))?.clone();
		let max_y = max_amplitude - min_amplitude;

		// Avoid division by zero
		if max_x.is_zero() || max_y.is_zero() {
			bail!("Cannot perform relative analysis: max_x or max_y is zero");
		}

		for measurement in &mut self.measurements {
			let Some(location) = measurement.get_vector_location() else {
				bail!("Location is not set for all measurements in the batch");
			};
			let Some(amplitude) = measurement.get_vector_amplitude() else {
				bail!("Amplitude is not set for all measurements in the batch");
			};

			// Relative.Location = Location / Max-X - percentage of movement relative to max movement
			let relative_location = location / &max_x;

			// Relative.Amplitude = Amplitude / Max-Y - percentage of movement relative to max movement
			let relative_amplitude = amplitude / &max_y;

			let measurement_vector = MeasurementVector::new(relative_location, relative_amplitude);
			let relative = Relative::new(measurement_vector, max_x.clone(), max_y.clone());
			let mut analysis = measurement.analysis().map_or_else(Analysis::default, Clone::clone);
			analysis.set_relative(Some(relative));
			measurement.set_analysis(analysis);
		}

		Ok(())
	}

	/// `positive_distance`  - property is the percentage distance that the measurement lies on the x-axis, between the first measurements in the batch and the last measurement in the batch.
	/// `positive_distance = (1/(location[location.len() -1] - locatiion[0])) * (location[this] - location[0])`
	///
	/// `negative_distance`  - property is the inverse of `positive_distance`.
	/// `negative_distance = 1 - positive_disatance`
	///
	/// # Errors
	///
	/// Returns an error if the batch is empty or if location range is zero.
	pub fn initialize_distances(&mut self) -> Result<()> {
		if self.measurements.is_empty() {
			bail!("Batch is empty, cannot initialize distances");
		}

		let first_location = self.measurements.first().and_then(|m| m.get_vector_location()).ok_or_else(|| anyhow::anyhow!("First measurement location is not set"))?.clone();
		let last_location = self.measurements.last().and_then(|m| m.get_vector_location()).ok_or_else(|| anyhow::anyhow!("Last measurement location is not set"))?.clone();

		let location_range = &last_location - &first_location;
		if location_range.is_zero() {
			bail!("Location range is zero, cannot initialize distances");
		}

		for measurement in &mut self.measurements {
			let location = measurement.get_vector_location().ok_or_else(|| anyhow::anyhow!("Measurement location is not set"))?;
			let positive_distance = (location.clone() - &first_location) / &location_range;
			let negative_distance = BigDecimal::from(1) - &positive_distance;

			let distance = BatchDistance::new(positive_distance, negative_distance);
			measurement.set_distance(distance);
		}

		Ok(())
	}

	/// Processes the batch by applying all transformations in sequence.
	///
	/// # Errors
	///
	/// Returns an error if any of the transformation steps fail.
	pub fn process(&mut self) -> Result<()> {
		self.level_transform()?;
		self.transpose_origin()?;
		self.simplify_transformation()?;
		self.relative_analysis()?;
		Ok(())
	}
}

impl IntoIterator for Batch {
	type IntoIter = std::vec::IntoIter<BatchedMeasurement>;
	type Item = BatchedMeasurement;

	fn into_iter(self) -> Self::IntoIter {
		self.measurements.into_iter()
	}
}
