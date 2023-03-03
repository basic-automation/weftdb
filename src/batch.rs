use super::Interpolation;
use crate::interpolation::ToInterpolation;
use crate::length::{BatchLength, ToSeconds};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use dsm_measurement::Measurement;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use std::collections::HashMap;
use std::sync::{Mutex, Arc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Batch {
	pub measurements: Option<Vec<Measurement>>,
	pub length: BatchLength,
	pub measurement_bucket: String,
	pub uuid: Uuid,
        pub relative_x_movements: Option<Vec<BigDecimal>>,
        pub relative_y_movements: Option<Vec<BigDecimal>>,
}

impl Batch {
	pub fn new(measurement_bucket: String, length: BatchLength) -> Self {
		Self { measurements: None, length, measurement_bucket, uuid: Uuid::new_v4(), relative_x_movements: None, relative_y_movements: None }
	}

	pub async fn numerator_asset(&self) -> String {
		self.measurement_bucket.split("::").collect::<Vec<&str>>()[0].to_string()
	}

	pub async fn denominator_asset(&self) -> String {
		self.measurement_bucket.split("::").collect::<Vec<&str>>()[1].to_string()
	}

	/// size = number of seconds in batch
	pub async fn size(&self) -> usize {
		self.length.to_seconds() as usize
	}

	/// interpolation = unit of time between measurements
	pub async fn interpolation(&self) -> Interpolation {
		self.length.to_interpolation()
	}

	/// interpolation_steps = number of seconds between measurements
	pub async fn interpolation_steps(&self) -> usize {
		self.interpolation().await.to_seconds() as usize
	}

	pub async fn interval(&self) -> u64 {
		let size = self.size().await;
		let unit = self.interpolation().await.to_seconds();

		if unit == 0 {
			0
		} else {
			size as u64 / unit
		}
	}

	/// start_timestamp = first measurement timestamp
	pub async fn start_timestamp(&self) -> Option<i64> {
		if self.measurements.is_some() {
			Some(self.measurements.as_ref().unwrap()[0].timestamp)
		} else {
			None
		}
	}

	/// end_timestamp = last measurement timestamp
	pub async fn end_timestamp(&self) -> Option<i64> {
		if self.measurements.is_some() {
			Some(self.measurements.as_ref().unwrap()[self.measurements.as_ref().unwrap().len() - 1].timestamp)
		} else {
			None
		}
	}

	/// max_x_movement = largest location - smallest location
	pub async fn max_x_movement(&self) -> Option<BigDecimal> {
		if self.measurements.is_some() {
			let largest_location = match self.measurements.as_ref().unwrap().par_iter().max_by(|a, b| a.location.cmp(&b.location)).unwrap().location {
				Some(location) => BigDecimal::from_i64(location).unwrap(),
				None => return None,
			};
			let smallest_location = match self.measurements.as_ref().unwrap().par_iter().min_by(|a, b| a.location.cmp(&b.location)).unwrap().location {
				Some(location) => BigDecimal::from_i64(location).unwrap(),
				None => return None,
			};

			Some(largest_location - smallest_location)
		} else {
			None
		}
	}

	/// max_y_movement = largest amplitude - smallest amplitude
	pub async fn max_y_movement(&self) -> Option<BigDecimal> {
		if self.measurements.is_some() {
			let largest_amplitude = match &self.measurements.as_ref().unwrap().par_iter().max_by(|a, b| a.amplitude.cmp(&b.amplitude)).unwrap().amplitude {
				Some(amplitude) => amplitude.clone(),
				None => return None,
			};
			let smallest_amplitude = match &self.measurements.as_ref().unwrap().par_iter().min_by(|a, b| a.amplitude.cmp(&b.amplitude)).unwrap().amplitude {
				Some(amplitude) => amplitude.clone(),
				None => return None,
			};

			Some(largest_amplitude - smallest_amplitude)
		} else {
			None
		}
	}

	/// relative_x_movement = measurement.location / max_x_movement
	pub async fn calculate_relative_x_movements(&mut self) -> Result<(), String> {
		let max_x_movement = match self.max_x_movement().await {
			Some(max_x_movement) => max_x_movement,
			None => return Ok(()),
		};

		let relative_x_movements: Arc<Mutex<Vec<BigDecimal>>> = Arc::new(Mutex::new(vec![]));

                self.measurements.clone().unwrap().par_iter().for_each(|measurement| {
                        let location = match measurement.location {
                                Some(location) => BigDecimal::from_i64(location).unwrap(),
                                None => return,
                        };

                        relative_x_movements.lock().unwrap().push(location / max_x_movement.clone());
                });

                let relative_x_movements = relative_x_movements.lock().unwrap().clone();

		self.relative_x_movements = Some(relative_x_movements);

                Ok(())
	}

	/// relative_y_movement = measurement.amplitude / max_y_movement
	pub async fn calculate_relative_y_movements(&mut self) -> Result<(), String> {
		let max_y_movement = match self.max_y_movement().await {
			Some(max_y_movement) => max_y_movement,
			None => return Ok(()),
		};

		let relative_y_movements: Arc<Mutex<Vec<BigDecimal>>> = Arc::new(Mutex::new(vec![]));

                self.measurements.clone().unwrap().par_iter().for_each(|measurement| {
                        let amplitude = match &measurement.amplitude {
                                Some(amplitude) => amplitude.clone(),
                                None => return,
                        };

                        relative_y_movements.lock().unwrap().push(amplitude / max_y_movement.clone());
                });

                let relative_y_movements = relative_y_movements.lock().unwrap().clone();

		self.relative_y_movements = Some(relative_y_movements);

                Ok(())
	}

        pub async fn calculate_relative_movements(&mut self) -> Result<(), String> {
                self.calculate_relative_x_movements().await?;
                self.calculate_relative_y_movements().await?;

                Ok(())
        }

	pub async fn add_measurement(&mut self, mut measurement: Measurement) {
		measurement.location = Some(measurement.timestamp);
		measurement.amplitude = Some(measurement.ratio.clone());

		if self.measurements.is_none() {
			self.measurements = Some(vec![measurement]);
		} else {
			self.measurements.as_mut().unwrap().push(measurement);
		}
	}

	pub async fn sort(&mut self) {
		self.measurements.as_mut().unwrap().sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
	}

	pub async fn calculate_measurement_distances(&mut self) -> Result<(), String> {
		self.sort().await;

		let first_location = match self.measurements.as_ref().unwrap().first().unwrap().location {
			Some(first_location) => BigDecimal::from_i64(first_location).unwrap(),
			None => return Err("No location found in first measurement".to_string()),
		};
		let last_location = match self.measurements.as_ref().unwrap().last().unwrap().location {
			Some(last_location) => BigDecimal::from_i64(last_location).unwrap(),
			None => return Err("No location found in last measurement".to_string()),
		};

		self.measurements.as_mut().unwrap().par_iter_mut().for_each(|measurement| {
			let location = match measurement.location {
				Some(location) => BigDecimal::from_i64(location).unwrap(),
				None => return,
			};

			let positive_distance = (BigDecimal::from(1) / (last_location.clone() - first_location.clone())) * (location - first_location.clone());
			measurement.positive_distance = Some(positive_distance.clone());
			measurement.negative_distance = Some(BigDecimal::from(1) - positive_distance);
		});

		Ok(())
	}

	pub async fn vector_leveling(&mut self) -> Result<(), String> {
		self.sort().await;

		let first_amplitude = match &self.measurements.as_ref().unwrap().first().unwrap().amplitude {
			Some(first_amplitude) => first_amplitude.clone(),
			None => return Err("No amplitude found in first measurement".to_string()),
		};
		let last_amplitude = match &self.measurements.as_ref().unwrap().last().unwrap().amplitude {
			Some(last_amplitude) => last_amplitude.clone(),
			None => return Err("No amplitude found in last measurement".to_string()),
		};

		self.measurements.as_mut().unwrap().par_iter_mut().for_each(|measurement| {
			let mut amplitude = match &measurement.amplitude {
				Some(amplitude) => amplitude.clone(),
				None => return,
			};
			let positive_distance = match measurement.positive_distance.clone() {
				Some(positive_distance) => positive_distance,
				None => return,
			};
			let negative_distance = match measurement.negative_distance.clone() {
				Some(negative_distance) => negative_distance,
				None => return,
			};

			// Positive vector leveling
			measurement.amplitude = Some(amplitude + ((BigDecimal::from(0) - first_amplitude.clone()) * negative_distance));

			amplitude = match &measurement.amplitude {
				Some(amplitude) => amplitude.clone(),
				None => return,
			};

			// Negative vector leveling
			measurement.amplitude = Some(amplitude + ((BigDecimal::from(0) - last_amplitude.clone()) * positive_distance));
		});

		Ok(())
	}

        pub async fn origin_transformation(&mut self) -> Result<(), String> {
                self.sort().await;

                let first_location = match self.measurements.as_ref().unwrap().first().unwrap().location {
                        Some(first_location) => BigDecimal::from_i64(first_location).unwrap(),
                        None => return Err("No location found in first measurement".to_string()),
                };

                self.measurements.as_mut().unwrap().par_iter_mut().for_each(|measurement| {
                        let location = match measurement.location {
                                Some(location) => BigDecimal::from_i64(location).unwrap(),
                                None => return,
                        };

                        measurement.location = Some((location - first_location.clone()).to_i64().unwrap());
                });

                Ok(())
        }

        pub async fn trend_vector_analysis(&mut self) -> Result<(), String> {
                self.sort().await;

                let tvs: Arc<Mutex<HashMap<Uuid, Option<Vec<BigDecimal>>>>> = Arc::new(Mutex::new(HashMap::new()));

                self.measurements.as_ref().unwrap().par_iter().for_each(|origin_measurement| {
                        let trend_vectors: Arc<Mutex<Vec<BigDecimal>>> = Arc::new(Mutex::new(vec![]));
                        let origin_location = match origin_measurement.location {
                                Some(location) => BigDecimal::from_i64(location).unwrap(),
                                None => return,
                        };
                        let origin_amplitude = match &origin_measurement.amplitude {
                                Some(amplitude) => amplitude.clone(),
                                None => return,
                        };

                        self.measurements.clone().as_ref().unwrap().par_iter().for_each(|end_measurement| {
                                let mut t_vec = trend_vectors.lock().unwrap();
                                
                                if end_measurement.location.unwrap() == origin_measurement.location.unwrap() {
                                        t_vec.push(BigDecimal::from(0));
                                        return;
                                }

                                let end_location = match end_measurement.location {
                                        Some(location) => BigDecimal::from_i64(location).unwrap(),
                                        None => return,
                                };
                                let end_amplitude = match &end_measurement.amplitude {
                                        Some(amplitude) => amplitude.clone(),
                                        None => return,
                                };

                                if end_amplitude.clone() - origin_amplitude.clone() == BigDecimal::from(0) {
                                        t_vec.push(BigDecimal::from(0));
                                        return;
                                }

                                t_vec.push((end_location - origin_location.clone()) / (end_amplitude - origin_amplitude.clone()));
                        });

                        let mut tvs = tvs.lock().unwrap();
                        tvs.insert(origin_measurement.uuid, Some(trend_vectors.lock().unwrap().clone()));
                });

                self.measurements.as_mut().unwrap().par_iter_mut().for_each(|measurement| {
                        let tvs = tvs.lock().unwrap();
                        let trend_vectors = tvs.get(&measurement.uuid).unwrap().clone();

                        measurement.trend_vectors = trend_vectors;
                });

                Ok(())
        }

        pub async fn simplify_transformation(&mut self) -> Result<(), String> {
                self.sort().await;

                let mut simplified_measurements: Vec<Measurement> = vec![];

                let mut p = 0;
                let mut c = 1;
                let mut n = 2;
                
                while c < self.measurements.clone().expect("no meaurements in batch").len() - 1 {
                        let previous_amplitude = match &self.measurements.as_ref().unwrap()[p].amplitude {
                                Some(amplitude) => amplitude.clone(),
                                None => return Err("No amplitude found in previous measurement".to_string()),
                        };
                        let current_amplitude = match &self.measurements.as_ref().unwrap()[c].amplitude {
                                Some(amplitude) => amplitude.clone(),
                                None => return Err("No amplitude found in current measurement".to_string()),
                        };
                        let next_amplitude = match &self.measurements.as_ref().unwrap()[n].amplitude {
                                Some(amplitude) => amplitude.clone(),
                                None => return Err("No amplitude found in next measurement".to_string()),
                        };

                        if previous_amplitude < current_amplitude && next_amplitude > current_amplitude {
                                simplified_measurements.push(self.measurements.as_ref().unwrap()[c].clone());
                                c += 1;
                                n += 1;
                                continue;
                        }

                        if previous_amplitude > current_amplitude && next_amplitude < current_amplitude {
                                simplified_measurements.push(self.measurements.as_ref().unwrap()[c].clone());
                                c += 1;
                                n += 1;
                                continue;
                        }

                        p = c;
                        c += 1;
                        n += 1;
                }

                self.measurements = Some(simplified_measurements);

                Ok(())
        }

}
