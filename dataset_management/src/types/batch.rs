use anyhow::{Result, bail};
use bigdecimal::{BigDecimal, FromPrimitive};
use database::{AspectId, DataPoint, Resolution};
use chrono::{DateTime, Utc};
use std::collections::HashMap;

use crate::types::{Analysis, BatchedMeasurement, MeasurementVector, Relative, Trend};

/// A batch of data points organized by aspect and time window
#[derive(Debug, Clone)]
pub struct Batch {
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub data: HashMap<AspectId, Vec<DataPoint>>,
    pub created_at: DateTime<Utc>,
}

impl Batch {
    /// Create a new batch with the specified time window and data
    pub fn new(
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
        data: HashMap<AspectId, Vec<DataPoint>>,
    ) -> Self {
        Self {
            start_time,
            end_time,
            data,
            created_at: Utc::now(),
        }
    }

    /// Get the duration of this batch in milliseconds
    pub fn duration_ms(&self) -> i64 {
        self.end_time.signed_duration_since(self.start_time).num_milliseconds()
    }

    /// Get the number of aspects in this batch
    pub fn aspect_count(&self) -> usize {
        self.data.len()
    }

    /// Get the total number of data points across all aspects
    pub fn total_points(&self) -> usize {
        self.data.values().map(|points| points.len()).sum()
    }

    /// Get data points for a specific aspect
    pub fn get_aspect_data(&self, aspect_id: AspectId) -> Option<&Vec<DataPoint>> {
        self.data.get(&aspect_id)
    }

    /// Get all aspect IDs in this batch
    pub fn aspect_ids(&self) -> Vec<AspectId> {
        self.data.keys().copied().collect()
    }

    /// Check if this batch contains data for the specified aspect
    pub fn contains_aspect(&self, aspect_id: AspectId) -> bool {
        self.data.contains_key(&aspect_id)
    }

    /// Get the average number of points per aspect
    pub fn avg_points_per_aspect(&self) -> f64 {
        if self.data.is_empty() {
            0.0
        } else {
            self.total_points() as f64 / self.data.len() as f64
        }
    }

    /// Check if this batch is empty (no data points)
    pub fn is_empty(&self) -> bool {
        self.data.is_empty() || self.total_points() == 0
    }

    /// Get a summary string of this batch
    pub fn summary(&self) -> String {
        format!(
            "Batch [{} to {}]: {} aspects, {} total points, avg {:.1} points/aspect",
            self.start_time.format("%Y-%m-%d %H:%M:%S"),
            self.end_time.format("%Y-%m-%d %H:%M:%S"),
            self.aspect_count(),
            self.total_points(),
            self.avg_points_per_aspect()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use database::AspectId;
    use uuid::Uuid;
    use bigdecimal::BigDecimal;
    use std::str::FromStr;
    use chrono::TimeZone;

    #[test]
    fn test_batch_creation() {
        let start = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 1, 0, 0).unwrap();
        
        let aspect1 = AspectId(Uuid::new_v4());
        let aspect2 = AspectId(Uuid::new_v4());
        
        let mut data = HashMap::new();
        data.insert(aspect1, vec![
            DataPoint {
                timestamp: start,
                value: BigDecimal::from_str("1.0").unwrap(),
            },
            DataPoint {
                timestamp: start + chrono::Duration::minutes(30),
                value: BigDecimal::from_str("2.0").unwrap(),
            },
        ]);
        data.insert(aspect2, vec![
            DataPoint {
                timestamp: start + chrono::Duration::minutes(15),
                value: BigDecimal::from_str("3.0").unwrap(),
            },
        ]);
        
        let batch = Batch::new(start, end, data);
        
        assert_eq!(batch.aspect_count(), 2);
        assert_eq!(batch.total_points(), 3);
        assert_eq!(batch.duration_ms(), 3600000); // 1 hour in ms
        assert!(!batch.is_empty());
        assert!(batch.contains_aspect(aspect1));
        assert!(batch.contains_aspect(aspect2));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyBatch {
	size: usize,
	measurements: Vec<BatchedMeasurement>,
	resolution: Resolution,
}

impl LegacyBatch {
	pub fn new(size: usize, measurements: Vec<BatchedMeasurement>, resolution: Resolution) -> Self {
		Self { size, measurements, resolution }
	}

	pub fn size(&self) -> usize {
		self.size
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
				None => bail!("Measurement vector is not set for all measurements in the batch"),
			};
			vectors.push(vector.clone());
		}
		Ok(vectors.into_iter().map(|v| v.amplitude().clone()).collect())
	}

	pub fn get_vector_locations(&self) -> Result<Vec<BigDecimal>> {
		let mut vectors: Vec<MeasurementVector> = Vec::with_capacity(self.measurements.len());
		for measurement in &self.measurements {
			let vector = match measurement.vector() {
				Some(v) => v,
				None => bail!("Measurement vector is not set for all measurements in the batch"),
			};
			vectors.push(vector.clone());
		}
		Ok(vectors.into_iter().map(|v| v.location().clone()).collect())
	}

	fn positive_level_transform(&mut self) -> Result<()> {
		let amplitudes = self.get_vector_amplitudes().unwrap_or_default();
		if amplitudes.is_empty() {
			bail!("No amplitudes found in the batch");
		}
		let amplitude_0 = amplitudes[0].clone();
		let amplitude_0_movement = BigDecimal::from(0) - &amplitude_0;
		for (i, measurement) in self.measurements.iter_mut().enumerate() {
			let negative_distance = match measurement.distance() {
				Some(d) => d.negative().clone(),
				None => bail!("Distance is not set for all measurements in the batch"),
			};
			let new_amplitude = &amplitudes[i] + (&amplitude_0_movement * &negative_distance);
			let mut vector = match measurement.vector() {
				Some(v) => v.clone(),
				None => bail!("Measurement vector is not set for all measurements in the batch"),
			};
			vector.set_amplitude(new_amplitude);
			measurement.set_vector(vector);
		}

		Ok(())
	}

	fn negative_level_transform(&mut self) -> Result<()> {
		let amplitudes = self.get_vector_amplitudes().unwrap_or_default();
		if amplitudes.is_empty() {
			bail!("No amplitudes found in the batch");
		}
		let amplitude_last = amplitudes[amplitudes.len() - 1].clone();
		let amplitude_last_movement = BigDecimal::from(0) - &amplitude_last;
		for (i, measurement) in self.measurements.iter_mut().enumerate() {
			let positive_distance = match measurement.distance() {
				Some(d) => d.positive().clone(),
				None => bail!("Distance is not set for all measurements in the batch"),
			};
			let new_amplitude = &amplitudes[i] + (&amplitude_last_movement * &positive_distance);
			let mut vector = match measurement.vector() {
				Some(v) => v.clone(),
				None => bail!("Measurement vector is not set for all measurements in the batch"),
			};
			vector.set_amplitude(new_amplitude);
			measurement.set_vector(vector);
		}

		Ok(())
	}

	pub fn level_transform(&mut self) -> Result<()> {
		if self.measurements.is_empty() {
			bail!("Batch is empty, cannot perform level transform");
		}

		self.positive_level_transform()?;
		self.negative_level_transform()?;

		Ok(())
	}

	pub fn transpose_origin(&mut self) -> Result<()> {
		let locations = self.get_vector_locations().unwrap_or_default();
		if locations.is_empty() {
			bail!("No locations found in the batch");
		}

		let location_0 = locations[0].clone();
		let location_0_movement = BigDecimal::from(0) - &location_0;
		for (i, measurement) in self.measurements.iter_mut().enumerate() {
			let new_location = &locations[i] + &location_0_movement;
			let mut vector = match measurement.vector() {
				Some(v) => v.clone(),
				None => bail!("Measurement vector is not set for all measurements in the batch"),
			};
			vector.set_location(new_location);
			measurement.set_vector(vector);
		}

		Ok(())
	}

	pub fn trend_analysis(&mut self) -> Result<()> {
		if self.measurements.is_empty() {
			bail!("Batch is empty, cannot build trends");
		}

		let measurments = self.measurements.clone();

		for measurement in &mut self.measurements {
			let mut trends: Vec<Trend> = Vec::with_capacity(measurments.len());
			for destination_measurment in &measurments {
				let measurment_value = measurement.get_measurement_value().clone();
				let destination_value = destination_measurment.get_measurement_value().clone();
				let measurement_timestamp = measurement.get_measurement_timestamp().clone();
				let destination_timestamp = destination_measurment.get_measurement_timestamp().clone();
				let destination_measurement_timestamp_difference = match self.resolution {
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

				let measurement_destination_timestamp_difference = match self.resolution {
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
					let trend = Trend::new(destination_measurment.clone().measurement().clone(), slope);
					trends.push(trend);
				} else if measurement_timestamp > destination_timestamp {
					let slope = (measurment_value - destination_value) / (measurement_destination_timestamp_difference);
					let trend = Trend::new(destination_measurment.clone().measurement().clone(), slope);
					trends.push(trend);
				} else {
					let slope = BigDecimal::from(0);
					let trend = Trend::new(destination_measurment.clone().measurement().clone(), slope);
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
		if self.measurements.is_empty() {
			bail!("Batch is empty, cannot perform simplification");
		}

		let mut i = 1;
		while i < self.measurements.len() - 1 {
			let measurement = &self.measurements[i];

			let Some(previous_trend_slope) = measurement.analysis().and_then(|a| a.get_slope(i - 1)) else {
				bail!("Previous trend slope is not set for measurement at index {}", i);
			};
			let Some(next_trend_slope) = measurement.analysis().and_then(|a| a.get_slope(i + 1)) else {
				bail!("Next trend slope is not set for measurement at index {}", i);
			};

			if previous_trend_slope.sign() == next_trend_slope.sign() {
				// Simplify the trend by removing the current measurement
				self.measurements.remove(i);
				// Don't increment i since we removed an element
			} else {
				i += 1;
			}
		}
		Ok(())
	}

	pub fn relative_analysis(&mut self) -> Result<()> {
		if self.measurements.is_empty() {
			bail!("Batch is empty, cannot perform relative analysis");
		}

		let max_x = self.measurements.iter().map(|m| m.get_vector_location().clone()).max().ok_or_else(|| anyhow::anyhow!("No measurements found in the batch"))?.ok_or_else(|| anyhow::anyhow!("Failed to get max x value"))?.clone();
		let max_y = self.measurements.iter().map(|m| m.get_vector_amplitude().clone()).max().ok_or_else(|| anyhow::anyhow!("No measurements found in the batch"))?.ok_or_else(|| anyhow::anyhow!("Failed to get max y value"))?.clone();

		for measurement in &mut self.measurements {
			let Some(location) = measurement.get_vector_location() else {
				bail!("Measurement vector location is not set for all measurements in the batch");
			};
			let Some(amplitude) = measurement.get_vector_amplitude() else {
				bail!("Measurement vector amplitude is not set for all measurements in the batch");
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
