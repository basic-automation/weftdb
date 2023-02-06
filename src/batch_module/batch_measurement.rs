use crate::batch_module::measurement::Measurement;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchMeasurement {
	pub batch_measurement_timestamp: i64,
	pub batch_measurement_location: i64,
	pub batch_measurement_value: f64,
	pub batch_measurement_amplitude: f64,
	pub batch_measurement_positive_distance: Option<f64>,
	pub batch_measurement_negative_distance: Option<f64>,
	pub batch_measurement_trend_vectors: Option<Vec<f64>>,
}

impl BatchMeasurement {
	pub fn new(measurement: Measurement) -> BatchMeasurement {
		let batch_measurement_timestamp = measurement.measurement_time;
		let batch_measurement_location = measurement.measurement_time;
		let batch_measurement_value = measurement.measurement_ratio;
		let batch_measurement_amplitude = measurement.measurement_ratio;
		let batch_measurement_positive_distance = None;
		let batch_measurement_negative_distance = None;
		let batch_measurement_trend_vectors = None;

		BatchMeasurement {
			batch_measurement_timestamp,
			batch_measurement_location,
			batch_measurement_value,
			batch_measurement_amplitude,
			batch_measurement_positive_distance,
			batch_measurement_negative_distance,
			batch_measurement_trend_vectors,
		}
	}
}
