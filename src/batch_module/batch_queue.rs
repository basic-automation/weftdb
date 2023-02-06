use super::{batch_measurement::*, Measurement, MeasurementParameters, ToBatch, Batch};
use crate::assets::Asset;
use crate::influxdb2::FluxInterpolation;
use crate::sources::Source;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchQueue {
	pub batch_measurements: Vec<BatchMeasurement>,
	pub batch_start_timestamp: i64,
	pub batch_end_timestamp: i64,
	pub batch_timestamp: i64,
	pub batch_interval: u64,
	pub batch_size: usize,
	pub batch_source: Source,
	pub batch_asset_1: Asset,
	pub batch_asset_2: Asset,
	pub batch_interpolation: FluxInterpolation,
	pub batch_measurement_event_uuid: String,
}

impl BatchQueue {
	pub async fn new(id: String, measurements: Vec<Measurement>, parameters: MeasurementParameters) -> BatchQueue {
		let mut batch_measurements: Vec<BatchMeasurement> = Vec::new();
		for measurement in measurements {
			let batch_measurement = BatchMeasurement::new(measurement);
			batch_measurements.push(batch_measurement);
		}
		let batch_start_timestamp = batch_measurements[0].batch_measurement_timestamp;
		let batch_end_timestamp = batch_measurements[batch_measurements.len() - 1].batch_measurement_timestamp;
		let batch_timestamp = batch_start_timestamp;
		let batch_interval = parameters.measurement_parameter_interval;
		let batch_size = parameters.measurement_parameter_size;
		let batch_source = parameters.measurement_parameter_source;
		let batch_asset_1 = parameters.measurement_parameter_asset_1;
		let batch_asset_2 = parameters.measurement_parameter_asset_2;
		let batch_interpolation = parameters.measurement_parameter_interpolation;
		let batch_measurement_event_uuid = id;

		BatchQueue {
			batch_measurements,
			batch_start_timestamp,
			batch_end_timestamp,
			batch_timestamp,
			batch_interval,
			batch_size,
			batch_source,
			batch_asset_1,
			batch_asset_2,
			batch_interpolation,
			batch_measurement_event_uuid,
		}
	}
}


impl ToBatch for BatchQueue {
        fn to_batch(&self, batch_measurements: Vec<BatchMeasurement>) -> Batch {
                let batch_start_timestamp = batch_measurements[0].batch_measurement_timestamp;
                let batch_end_timestamp = batch_measurements[batch_measurements.len() - 1].batch_measurement_timestamp;
                let batch_timestamp = batch_start_timestamp;
                let batch_interval = self.batch_interval;
                let batch_size = self.batch_size;
                let batch_source = self.batch_source.clone();
                let batch_asset_1 = self.batch_asset_1;
                let batch_asset_2 = self.batch_asset_2;
                let batch_interpolation = self.batch_interpolation;
                let batch_measurement_event_uuid = self.batch_measurement_event_uuid.clone();
                let batch_uuid = Uuid::new_v4().to_string();

                Batch {
                        batch_measurements,
                        batch_start_timestamp,
                        batch_end_timestamp,
                        batch_timestamp,
                        batch_interval,
                        batch_size,
                        batch_source,
                        batch_asset_1,
                        batch_asset_2,
                        batch_interpolation,
                        batch_measurement_event_uuid,
                        batch_uuid,
                        batch_nature: None,
                }
        }
}



