use anyhow::{bail, Result};
use bigdecimal::{BigDecimal, FromPrimitive, Zero};
use database::{AspectId, DatabaseInfo};
use serde::{Deserialize, Serialize};
use splimes::Resolution;

use crate::types::{Analysis, BatchedMeasurement, Distance, MeasurementVector, Relative, Trend};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchMetatdata {
	pub aspect: AspectId,
	pub resolution: Resolution,
	pub size: usize,
	pub database_info: DatabaseInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Batch {
	pub metadata: BatchMetatdata,
	pub measurements: Vec<BatchedMeasurement>,
}

impl Batch {
	pub fn new(size: usize, measurements: Vec<BatchedMeasurement>, resolution: Resolution, aspect: AspectId, database_info: DatabaseInfo) -> Self {
		let metadata = BatchMetatdata { resolution, size, aspect, database_info };
		let mut batch = Self { metadata, measurements };
		batch.initialize_measurement_vectors().expect("Failed to initialize measurement vectors");
		batch.initialize_distances().expect("Failed to initialize distances");
		batch
	}

	pub fn size(&self) -> usize {
		self.metadata.size
	}

	pub fn measurements(&self) -> &Vec<BatchedMeasurement> {
		&self.measurements
	}

	pub fn set_measurements(&mut self, measurements: Vec<BatchedMeasurement>) {
		self.measurements = measurements;
	}

	pub fn add_measurement(&mut self, measurement: BatchedMeasurement) {
		self.measurements.push(measurement);
	}

	pub fn get_vector_amplitudes(&self) -> Result<Vec<BigDecimal>> {
		let mut vectors: Vec<MeasurementVector> = Vec::with_capacity(self.measurements.len());
		for measurement in &self.measurements {
			let vector = match measurement.vector() {
				Some(v) => v,
				None => {
					let location = match BigDecimal::from_i64(measurement.point().timestamp.timestamp()) {
						Some(loc) => loc,
						None => bail!("Invalid timestamp for measurement"),
					};
					&MeasurementVector::new(location, measurement.point().value.clone())
				}
			};
			vectors.push(vector.clone());
		}
		Ok(vectors.into_iter().map(|v| v.amplitude().clone()).collect())
	}

	pub fn initialize_measurement_vectors(&mut self) -> Result<()> {
		if self.measurements.is_empty() {
			bail!("Batch is empty, cannot initialize measurement vectors");
		}

		for measurement in &mut self.measurements {
			if measurement.vector().is_none() {
				let location = match BigDecimal::from_i64(measurement.point().timestamp.timestamp()) {
					Some(loc) => loc,
					None => bail!("Invalid timestamp for measurement"),
				};
				let vector = MeasurementVector::new(location, measurement.point().value.clone());
				measurement.set_vector(vector);
			}
		}

		Ok(())
	}

	pub fn get_vector_locations(&self) -> Result<Vec<BigDecimal>> {
		let mut vectors: Vec<MeasurementVector> = Vec::with_capacity(self.measurements.len());
		for measurement in &self.measurements {
			let vector = match measurement.vector() {
				Some(v) => v,
				None => {
					let location = match BigDecimal::from_i64(measurement.point().timestamp.timestamp()) {
						Some(loc) => loc,
						None => bail!("Invalid timestamp for measurement"),
					};
					&MeasurementVector::new(location, measurement.point().value.clone())
				}
			};
			vectors.push(vector.clone());
		}
		Ok(vectors.into_iter().map(|v| v.location().clone()).collect())
	}

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
			let vector = match measurement.vector_mut() {
				Some(v) => v,
				None => bail!("Measurement vector is not set for all measurements in the batch"),
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
			let vector = match measurement.vector_mut() {
				Some(v) => v,
				None => bail!("Measurement vector is not set for all measurements in the batch"),
			};
			vector.set_amplitude(new_amplitude);
		}

		Ok(())
	}

	pub fn transpose_origin(&mut self) -> Result<()> {
		let locations = self.get_vector_locations()?;
		if locations.is_empty() {
			bail!("No locations found in the batch");
		}

		let location_0_movement = BigDecimal::from(0) - &locations[0];

		for (i, measurement) in self.measurements.iter_mut().enumerate() {
			let new_location = &locations[i] + &location_0_movement;

			// Get mutable reference to avoid cloning
			let vector = match measurement.vector_mut() {
				Some(v) => v,
				None => bail!("Measurement vector is not set for all measurements in the batch"),
			};
			vector.set_location(new_location);
		}

		Ok(())
	}

	fn trend_analysis(&mut self) -> Result<()> {
		if self.measurements.is_empty() {
			bail!("Batch is empty, cannot build trends");
		}

		let measurments = self.measurements.clone();

		for measurement in &mut self.measurements {
			let mut trends: Vec<Trend> = Vec::with_capacity(measurments.len());
			for destination_measurment in &measurments {
				let measurment_value = measurement.get_measurement_value().clone();
				let destination_value = destination_measurment.get_measurement_value().clone();
				let measurement_timestamp = *measurement.get_measurement_timestamp(); // Fixed: removed .clone()
				let destination_timestamp = *destination_measurment.get_measurement_timestamp(); // Fixed: removed .clone()
				let destination_measurement_timestamp_difference = match self.metadata.resolution {
					Resolution::Nanoseconds => (destination_timestamp - measurement_timestamp).num_nanoseconds().map(|n| n as f64).unwrap_or(0.0),
					Resolution::Microseconds => (destination_timestamp - measurement_timestamp).num_microseconds().map(|n| n as f64).unwrap_or(0.0),
					Resolution::Milliseconds => (destination_timestamp - measurement_timestamp).num_milliseconds() as f64,
					Resolution::Seconds => (destination_timestamp - measurement_timestamp).num_seconds() as f64,
					Resolution::Minutes => (destination_timestamp - measurement_timestamp).num_minutes() as f64,
					Resolution::Hours => (destination_timestamp - measurement_timestamp).num_hours() as f64,
					Resolution::Days => (destination_timestamp - measurement_timestamp).num_days() as f64,
					Resolution::Weeks => (destination_timestamp - measurement_timestamp).num_weeks() as f64,
					Resolution::Months => (destination_timestamp - measurement_timestamp).num_weeks() as f64 / 4.34524, // Approximation: 1 month = 4.34524 weeks
					Resolution::Years => (destination_timestamp - measurement_timestamp).num_weeks() as f64 / 52.1775,  // Approximation: 1 year = 52.1775 weeks
				};

				let destination_measurement_difference: BigDecimal = match BigDecimal::from_f64(destination_measurement_timestamp_difference) {
					Some(d) => d,
					None => bail!("Failed to convert destination measurement difference to BigDecimal"),
				};

				let measurement_destination_timestamp_difference = match self.metadata.resolution {
					Resolution::Nanoseconds => (measurement_timestamp - destination_timestamp).num_nanoseconds().map(|n| n as f64).unwrap_or(0.0),
					Resolution::Microseconds => (measurement_timestamp - destination_timestamp).num_microseconds().map(|n| n as f64).unwrap_or(0.0),
					Resolution::Milliseconds => (measurement_timestamp - destination_timestamp).num_milliseconds() as f64,
					Resolution::Seconds => (measurement_timestamp - destination_timestamp).num_seconds() as f64,
					Resolution::Minutes => (measurement_timestamp - destination_timestamp).num_minutes() as f64,
					Resolution::Hours => (measurement_timestamp - destination_timestamp).num_hours() as f64,
					Resolution::Days => (measurement_timestamp - destination_timestamp).num_days() as f64,
					Resolution::Weeks => (measurement_timestamp - destination_timestamp).num_weeks() as f64,
					Resolution::Months => (measurement_timestamp - destination_timestamp).num_weeks() as f64 / 4.34524, // Approximation: 1 month = 4.34524 weeks
					Resolution::Years => (measurement_timestamp - destination_timestamp).num_weeks() as f64 / 52.1775,  // Approximation: 1 year = 52.1775 weeks
				};

				let measurement_destination_timestamp_difference: BigDecimal = match BigDecimal::from_f64(measurement_destination_timestamp_difference) {
					Some(d) => d,
					None => bail!("Failed to convert measurement destination timestamp difference to BigDecimal"),
				};

				if measurement_timestamp < destination_timestamp {
					let slope = (destination_value - measurment_value) / (destination_measurement_difference);
					let trend = Trend::new(destination_measurment.clone().point().clone(), slope);
					trends.push(trend);
				} else if measurement_timestamp > destination_timestamp {
					let slope = (measurment_value - destination_value) / (measurement_destination_timestamp_difference);
					let trend = Trend::new(destination_measurment.clone().point().clone(), slope);
					trends.push(trend);
				} else {
					let slope = BigDecimal::from(0);
					let trend = Trend::new(destination_measurment.clone().point().clone(), slope);
					trends.push(trend);
				}
			}
			let mut analysis = match measurement.analysis() {
				Some(a) => a.clone(),
				None => Analysis::default(),
			};
			analysis.set_trend(Some(trends));
			measurement.set_analysis(analysis);
		}
		Ok(())
	}

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
			let prev_to_curr_slope = if curr_vector.location() != prev_vector.location() { (curr_vector.amplitude() - prev_vector.amplitude()) / (curr_vector.location() - prev_vector.location()) } else { BigDecimal::from(0) };

			// Calculate slope from current to next
			let curr_to_next_slope = if next_vector.location() != curr_vector.location() { (next_vector.amplitude() - curr_vector.amplitude()) / (next_vector.location() - curr_vector.location()) } else { BigDecimal::from(0) };

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

	pub fn relative_analysis(&mut self) -> Result<()> {
		if self.measurements.is_empty() {
			bail!("Batch is empty, cannot perform relative analysis");
		}

		let max_x = self
			.measurements
			.iter()
			.map(|m| m.get_vector_location()) // Fixed: removed .clone()
			.max()
			.ok_or_else(|| anyhow::anyhow!("No measurements found in the batch"))?
			.ok_or_else(|| anyhow::anyhow!("Failed to get max x value"))?
			.clone();

		let max_y = self
			.measurements
			.iter()
			.map(|m| m.get_vector_amplitude()) // Fixed: removed .clone()
			.max()
			.ok_or_else(|| anyhow::anyhow!("No measurements found in the batch"))?
			.ok_or_else(|| anyhow::anyhow!("Failed to get max y value"))?
			.clone();

		for measurement in &mut self.measurements {
			let Some(location) = measurement.get_vector_location() else {
				bail!("Location is not set for all measurements in the batch");
			};
			let Some(amplitude) = measurement.get_vector_amplitude() else {
				bail!("Amplitude is not set for all measurements in the batch");
			};
			let relative_location = location / &max_x;
			let relative_amplitude = amplitude / &max_y;

			let measurement_vector = MeasurementVector::new(relative_location, relative_amplitude);
			let relative = Relative::new(measurement_vector, max_x.clone(), max_y.clone());
			let mut analysis = match measurement.analysis() {
				Some(a) => a.clone(),
				None => Analysis::default(),
			};
			analysis.set_relative(Some(relative));
			measurement.set_analysis(analysis);
		}

		Ok(())
	}

	/// positive_distance  - property is the percentage distance that the measurement lies on the x-axis, between the first measurements in the batch and the last measurement in the batch.
	/// `positive_distance = (1/(location[location.len() -1] - locatiion[0])) * (location[this] - location[0])`
	///
	/// negative_distance  - property is the inverse of positive_distance.
	/// `negative_distance = 1 - positive_disatance`
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

			let distance = Distance::new(positive_distance, negative_distance);
			measurement.set_distance(distance);
		}

		Ok(())
	}

	pub fn process(&mut self) -> Result<()> {
		self.level_transform()?;
		self.transpose_origin()?;
		self.simplify_transformation()?;
		self.trend_analysis()?;
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
