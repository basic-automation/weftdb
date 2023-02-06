use super::{batch_measurement::*, batch_nature::*};
use crate::influxdb2::FluxInterpolation;
use serde::{Deserialize, Serialize};
use crate::assets::Asset;
use crate::sources::Source;
use super::pattern::*;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Batch {
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
        pub batch_uuid: String,
        pub batch_nature: Option<BatchNature>,
}

pub trait ToBatch {
        fn to_batch(&self, batch_measurements: Vec<BatchMeasurement>) -> Batch;
}

impl Batch {
        pub async fn measurement_distance_calculation(&mut self) -> Self {
                let last_location = self.batch_measurements.last().unwrap().batch_measurement_location as f64;
                let first_location = self.batch_measurements.first().unwrap().batch_measurement_location as f64;

                for i in 0..self.batch_measurements.len() {
                        let this_location = self.batch_measurements[i].batch_measurement_location as f64;
                        let positive_distance = (1.0 / (last_location - first_location)) * (this_location - first_location);
                        self.batch_measurements[i].batch_measurement_positive_distance = Some(positive_distance);
                        self.batch_measurements[i].batch_measurement_negative_distance = Some(1.0 - positive_distance);
                }

                self.clone()
        }

        pub async fn measurement_vector_leveling(&mut self) -> Self {
                let first_amplitude = self.batch_measurements.first().unwrap().batch_measurement_amplitude;
                let last_amplitude = self.batch_measurements.last().unwrap().batch_measurement_amplitude;

                self.batch_measurements.iter_mut().for_each(|measurement| {
                        let mut amplitude = measurement.batch_measurement_amplitude;
                        let negative_distance = match measurement.batch_measurement_negative_distance {
                                Some(negative_distance) => negative_distance,
                                None => panic!("Cannot perform positive vector leveling if negative distance is not set. Measurement: {:?}", measurement),
                        };
                        let positive_distance = match measurement.batch_measurement_positive_distance {
                                Some(positive_distance) => positive_distance,
                                None => panic!("Cannot perform negative vector leveling if positive distance is not set. Measurement: {:?}", measurement),
                        };

                        // positive vector leveling
                        measurement.batch_measurement_amplitude = amplitude + ((0.0 - first_amplitude) * negative_distance);

                        amplitude = measurement.batch_measurement_amplitude;

                        // negative vector leveling
                        measurement.batch_measurement_amplitude = amplitude + ((0.0 - last_amplitude) * positive_distance);
                });

                self.clone()
        }

        pub async fn measurement_origin_transformation(&mut self) -> Self {
                let first_location = self.batch_measurements.first().unwrap().batch_measurement_location;
                self.batch_measurements.iter_mut().for_each(|measurement| {
                        let location = measurement.batch_measurement_location;
                        measurement.batch_measurement_location = location - first_location;
                });

                self.clone()
        }

        pub async fn measurement_trend_vector_analysis(&mut self) -> Self {
                for i in 0..self.batch_measurements.len() {
                        let mut trend_vectors: Vec<f64> = Vec::new();
                        let i_location: f64 = self.batch_measurements[i].batch_measurement_location as f64;
                        let i_amplitude: f64 = self.batch_measurements[i].batch_measurement_amplitude as f64;

                        for j in 0..self.batch_measurements.len() {
                                if i == j {
                                        trend_vectors.push(0.0);
                                        continue;
                                }

                                let j_location: f64 = self.batch_measurements[j].batch_measurement_location as f64;
                                let j_amplitude: f64 = self.batch_measurements[j].batch_measurement_amplitude as f64;

                                //divide by zero check
                                if j_amplitude - i_amplitude == 0.0 {
                                        trend_vectors.push(0.0);
                                        continue;
                                }

                                trend_vectors.push((j_location - i_location) / (j_amplitude - i_amplitude) as f64);
                        }
                        self.batch_measurements[i].batch_measurement_trend_vectors = Some(trend_vectors);
                }

                self.clone()
        }

        pub async fn measurement_simplify_transformation(&mut self) -> Self {
                let mut i = 1;

                while i < self.batch_measurements.len() - 1 {
                        let previous_vector = self.batch_measurements[i].batch_measurement_trend_vectors.as_ref().unwrap()[i - 1];
                        let next_vector = self.batch_measurements[i].batch_measurement_trend_vectors.as_ref().unwrap()[i + 1];

                        if previous_vector.signum() == next_vector.signum() {
                                self.batch_measurements.remove(i);
                                self.batch_measurements.iter_mut().for_each(|measurement| {
                                        measurement.batch_measurement_trend_vectors.as_mut().expect("Cannot perform simplify transformation if trend vectors are not set.").remove(i);                                });
                                i -= 1;
                        }

                        i += 1;
                }

                self.clone()
        }

        pub async fn compute_nature(&mut self) -> Self {
                // get largetst and smallest batch measurement location value
                let largest_location = self.batch_measurements.last().unwrap().batch_measurement_location as f64;
                let smallest_location = self.batch_measurements.first().unwrap().batch_measurement_location as f64;

                // get largest and smallest batch measurement amplitude value
                let largest_amplitude = self.batch_measurements.iter().max_by(|a, b| a.batch_measurement_amplitude.partial_cmp(&b.batch_measurement_amplitude).unwrap()).unwrap().batch_measurement_amplitude;
                let smallest_amplitude = self.batch_measurements.iter().min_by(|a, b| a.batch_measurement_amplitude.partial_cmp(&b.batch_measurement_amplitude).unwrap()).unwrap().batch_measurement_amplitude;

                let max_x_movement = largest_location - smallest_location;
                let max_y_movement = largest_amplitude - smallest_amplitude;
                
                let mut relative_locations: Vec<f64> = Vec::new();
                let mut relative_amplitudes: Vec<f64> = Vec::new();

                for measurement in self.batch_measurements.iter() {
                        relative_locations.push(measurement.batch_measurement_location as f64 / max_x_movement);
                        relative_amplitudes.push(measurement.batch_measurement_amplitude / max_y_movement);
                }

                let nature: BatchNature = BatchNature { 
                        max_x_movement,
                        max_y_movement,
                        relative_x_movements: relative_locations,
                        relative_y_movements: relative_amplitudes,
                };

                self.batch_nature = Some(nature);

                self.clone()
        }

        pub async fn gather(&self) -> (String, Vec<u8>) {
                let pattern = self.to_pattern();
                let pattern_bytes = rmp_serde::to_vec(&pattern).unwrap();

                (pattern.uuid, pattern_bytes)
        }
}


impl ToPattern for Batch {
        fn to_pattern(&self) -> Pattern {
                let uuid = Uuid::new_v4().to_string();
                let size = self.batch_size;
                let start_timestamp = self.batch_start_timestamp;
                let end_timestamp = self.batch_end_timestamp;
                let source = self.batch_source.clone();
                let x_coordinates = self.batch_nature.as_ref().unwrap().relative_x_movements.clone();
                let y_coordinates = self.batch_nature.as_ref().unwrap().relative_y_movements.clone();


                Pattern {
                    uuid,
                    size,
                    start_timestamp,
                    end_timestamp,
                    source,
                    x_coordinates,
                    y_coordinates,
                }
        }
}

