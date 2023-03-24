use super::Interpolation;
use crate::interpolation::ToInterpolation;
use crate::length::{BatchLength, ToSeconds};
use dsm_log::Log;
use bigdecimal::BigDecimal;
use dsm_measurement::Measurement;
use num_bigint::BigUint;
use rayon::prelude::*;
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize)]
pub struct Measurements(pub HashMap<BigUint, Measurement>);

impl Serialize for Measurements {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let mut map = serializer.serialize_map(Some(self.0.len()))?;
		for (k, v) in &self.0 {
			map.serialize_entry(&k.to_string(), &v)?;
		}
		map.end()
	}
}

#[derive(Debug, Clone, Deserialize)]
pub struct Movements(pub HashMap<BigUint, BigDecimal>);

impl Serialize for Movements {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let mut map = serializer.serialize_map(Some(self.0.len()))?;
		for (k, v) in &self.0 {
			map.serialize_entry(&k.to_string(), &v)?;
		}
		map.end()
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Batch {
	pub measurements: Option<Measurements>,
	pub length: BatchLength,
	pub measurement_bucket: String,
	pub uuid: Uuid,
	pub relative_x_movements: Option<Movements>,
	pub relative_y_movements: Option<Movements>,
	pub logs: Log,
        pub is_logging: bool,
}

impl Batch {
	pub fn new(measurement_bucket: String, length: BatchLength, log: bool) -> Self {
		let id = Uuid::new_v4();

		//log
		let mut logs = Log::new(&id.to_string());

                if log {
                    logs.log("1: Created", &json!("batch created"));
                }

		Self { measurements: None, length, measurement_bucket, uuid: id, relative_x_movements: None, relative_y_movements: None, logs, is_logging: log }
	}

	pub async fn numerator_asset(&self) -> String {
		self.measurement_bucket.split("::").collect::<Vec<&str>>()[0].to_string()
	}

	pub async fn denominator_asset(&self) -> String {
		self.measurement_bucket.split("::").collect::<Vec<&str>>()[1].to_string()
	}

	/// size = number of seconds in batch
	pub async fn size(&self) -> BigDecimal {
		self.length.to_seconds()
	}

	/// interpolation = unit of time between measurements
	pub async fn interpolation(&self) -> Interpolation {
		self.length.to_interpolation()
	}

	/// interpolation_steps = number of seconds between measurements
	pub async fn interpolation_steps(&self) -> BigDecimal {
		self.interpolation().await.to_seconds()
	}

	pub async fn interval(&self) -> BigDecimal {
		let size = self.size().await;
		let steps = self.interpolation_steps().await;

		if steps == BigDecimal::from(0) {
			BigDecimal::from(0)
		} else {
			size / steps
		}
	}

	/// start_timestamp = first measurement timestamp
	pub async fn start_timestamp(&self) -> Option<BigDecimal> {
		let measurements = match &self.measurements {
			Some(measurements) => measurements,
			None => return None,
		};

		measurements.0.get(&BigUint::from(0_u8)).map(|measurement| measurement.timestamp.clone())
	}

	/// end_timestamp = last measurement timestamp
	pub async fn end_timestamp(&self) -> Option<BigDecimal> {
		let measurements = match &self.measurements {
			Some(measurements) => measurements,
			None => return None,
		};

		let last_measurement_index = match self.last_measurement_index().await {
			Some(measurement) => measurement,
			None => return None,
		};

		let measurement = match measurements.0.get(&last_measurement_index) {
			Some(measurement) => measurement,
			None => return None,
		};

		Some(measurement.timestamp.clone())
	}

	/// max_x_movement = largest location - smallest location
	pub async fn max_x_movement(&self) -> Option<BigDecimal> {
		let measurements = match &self.measurements {
			Some(measurements) => measurements,
			None => return None,
		};

		let largest_location = match measurements.0.par_iter().max_by(|a, b| a.1.location.cmp(&b.1.location)) {
			Some(measurement) => match &measurement.1.location {
				Some(location) => location.clone(),
				None => return None,
			},
			None => return None,
		};

		let smallest_location = match measurements.0.par_iter().min_by(|a, b| a.1.location.cmp(&b.1.location)) {
			Some(measurement) => match &measurement.1.location {
				Some(location) => location.clone(),
				None => return None,
			},
			None => return None,
		};

		Some(largest_location - smallest_location)
	}

	/// max_y_movement = largest amplitude - smallest amplitude
	pub async fn max_y_movement(&self) -> Option<BigDecimal> {
		let measurements = match &self.measurements {
			Some(measurements) => measurements,
			None => return None,
		};

		let largest_amplitude = match measurements.0.par_iter().max_by(|a, b| a.1.amplitude.cmp(&b.1.amplitude)) {
			Some(measurement) => match &measurement.1.amplitude {
				Some(amplitude) => amplitude,
				None => return None,
			},
			None => return None,
		};

		let smallest_amplitude = match measurements.0.par_iter().min_by(|a, b| a.1.amplitude.cmp(&b.1.amplitude)) {
			Some(measurement) => match &measurement.1.amplitude {
				Some(amplitude) => amplitude,
				None => return None,
			},
			None => return None,
		};

		Some(largest_amplitude - smallest_amplitude)
	}

	/// relative_x_movement = measurement.location / max_x_movement
	pub async fn calculate_relative_x_movements(&mut self) -> Result<(), String> {
		let max_x_movement = match self.max_x_movement().await {
			Some(max_x_movement) => max_x_movement,
			None => return Ok(()),
		};

		let relative_x_movements: Arc<Mutex<HashMap<BigUint, BigDecimal>>> = Arc::new(Mutex::new(HashMap::new()));

		let measurements = match &self.measurements {
			Some(measurements) => measurements,
			None => return Err("No measurements".to_string()),
		};

                //log begin
                if self.is_logging {
                        let locations: Arc<Mutex<HashMap<String, BigDecimal>>> = Arc::new(Mutex::new(HashMap::new()));
                        self.measurements.as_ref().unwrap().0.par_iter().for_each(|measurement| {
                                if let Some(location) = &measurement.1.location {
                                        let mut locations = locations.lock().unwrap();
                                        locations.insert(measurement.0.clone().to_string(), location.clone());
                                }
                        });
                        self.logs.log(&format!("11: BATCH begin calculating relative x movments MAX-X: {:?}", max_x_movement.clone()), &json!(*locations.clone().lock().unwrap()));
                }
                //log end

		measurements.0.par_iter().for_each(|measurement| {
			let location = match &measurement.1.location {
				Some(location) => location,
				None => return,
			};

			match relative_x_movements.lock() {
				Ok(mut rxm) => if max_x_movement != BigDecimal::from(0) {
                                        rxm.insert(measurement.0.clone(), location / max_x_movement.clone())
                                } else {
                                        rxm.insert(measurement.0.clone(), BigDecimal::from(0))
                                }
				Err(_) => None,
			};
		});

		let rxm = match relative_x_movements.lock() {
			Ok(rxm) => rxm,
			Err(_) => return Err("Could not lock relative_x_movements".to_string()),
		};

		let relative_x_movements = rxm.clone();

		self.relative_x_movements = Some(Movements(relative_x_movements));

		// log begin
                if self.is_logging {
                        self.logs.log(&format!("12: BATCH end calculate_relative_x_movements"), &json!(self.relative_x_movements));
                }
                // log end


		Ok(())
	}

	/// relative_y_movement = measurement.amplitude / max_y_movement
	pub async fn calculate_relative_y_movements(&mut self) -> Result<(), String> {
		let max_y_movement = match self.max_y_movement().await {
			Some(max_y_movement) => max_y_movement,
			None => return Ok(()),
		};

		let relative_y_movements: Arc<Mutex<HashMap<BigUint, BigDecimal>>> = Arc::new(Mutex::new(HashMap::new()));

		let measurements = match &self.measurements {
			Some(measurements) => measurements,
			None => return Err("No measurements".to_string()),
		};

                // log begin
                if self.is_logging {
                        let amplitudes: Arc<Mutex<HashMap<String, BigDecimal>>> = Arc::new(Mutex::new(HashMap::new()));
                        self.measurements.as_ref().unwrap().0.par_iter().for_each(|measurement| {
                                if let Some(amplitude) = &measurement.1.amplitude {
                                        let mut amplitudes = amplitudes.lock().unwrap();
                                        amplitudes.insert(measurement.0.clone().to_string(), amplitude.clone());
                                }
                        });
                        self.logs.log(&format!("13: BATCH begin calculating relative y movments MAX-Y: {:?}", max_y_movement.clone()), &json!(*amplitudes.clone().lock().unwrap()));
                }
                // log end

		measurements.0.par_iter().for_each(|measurement| {
			let amplitude = match &measurement.1.amplitude {
				Some(amplitude) => amplitude.clone(),
				None => return,
			};

			match relative_y_movements.lock() {
				Ok(mut rym) => if max_y_movement.clone() != BigDecimal::from(0) {
                                        rym.insert(measurement.0.clone(), amplitude / max_y_movement.clone())
                                } else {
                                        rym.insert(measurement.0.clone(), BigDecimal::from(0))
                                },
				Err(_) => None,
			};
		});

		let relative_y_movements = match relative_y_movements.lock() {
			Ok(rym) => rym.clone(),
			Err(_) => return Err("Could not lock relative_y_movements".to_string()),
		};

		self.relative_y_movements = Some(Movements(relative_y_movements));

		// log begin
                if self.is_logging {
                        self.logs.log(&format!("14: BATCH end calculate_relative_y_movements"), &json!(self.relative_y_movements));
                }
                // log end

		Ok(())
	}

	pub async fn calculate_relative_movements(&mut self) -> Result<(), String> {
		match self.calculate_relative_x_movements().await {
                        Ok(_) => (),
                        Err(e) => return Err(e),
                };
		match self.calculate_relative_y_movements().await {
                        Ok(_) => (),
                        Err(e) => return Err(e),
                };

		Ok(())
	}

	pub async fn add_measurement(&mut self, mut measurement: Measurement) {
		measurement.location = Some(measurement.timestamp.clone());
		measurement.amplitude = Some(measurement.ratio.clone());
		let mut measurements = match &self.measurements {
			Some(measurements) => measurements.0.clone(),
			None => HashMap::new(),
		};
		let index = match measurements.len() {
			0 => BigUint::from(0_u8),
			_ => BigUint::from(measurements.len() as u8),
		};
		measurements.insert(index, measurement);

		self.measurements = Some(Measurements(measurements));
	}

	pub async fn add_measurements(&mut self, measurements: Vec<Measurement>) {
		for measurement in measurements {
			self.add_measurement(measurement).await;
		}
	}

	pub async fn last_measurement_index(&self) -> Option<BigUint> {
		let measurements = match self.measurements {
			Some(ref measurements) => measurements,
			None => return None,
		};

		let last_measurement: Arc<Mutex<BigUint>> = Arc::new(Mutex::new(BigUint::from(0_u8)));
		measurements.0.par_iter().for_each(|measurement| {
			if let Ok(mut last_measurement) = last_measurement.lock() {
				if measurement.0 > &last_measurement.clone() {
					last_measurement.clone_from(measurement.0);
				}
			}
		});

		let last_index = match last_measurement.lock() {
			Ok(last_index) => last_index.clone(),
			Err(_) => return None,
		};

		Some(last_index)
	}

	pub async fn calculate_measurement_distances(&mut self) -> Result<(), String> {
		let measurements = match &self.measurements {
			Some(measurements) => measurements,
			None => return Err("No measurements found".to_string()),
		};

		let first_location = match measurements.0.get(&BigUint::from(0_u8)) {
			Some(first_measurement) => match &first_measurement.location {
				Some(first_location) => first_location.clone(),
				None => return Err("No location found in first measurement".to_string()),
			},
			None => return Err("No measurements found".to_string()),
		};

		let last_measurement_index = match self.last_measurement_index().await {
			Some(last_measurement_index) => last_measurement_index,
			None => return Err("No measurements found".to_string()),
		};

		let last_location = match measurements.0.get(&last_measurement_index) {
			Some(last_measurement) => match &last_measurement.location {
				Some(last_location) => last_location.clone(),
				None => return Err("No location found in last measurement".to_string()),
			},
			None => return Err("No measurements found".to_string()),
		};

                // begin logs
                if self.is_logging {
                        let locations: Arc<Mutex<HashMap<String, BigDecimal>>> = Arc::new(Mutex::new(HashMap::new()));
                        self.measurements.as_ref().unwrap().0.par_iter().for_each(|measurement| {
                                if let Some(location) = &measurement.1.location {
                                        let mut locations = locations.lock().unwrap();
                                        locations.insert(measurement.0.clone().to_string(), location.clone());
                                }
                        });
                        self.logs.log(&format!("2: BATCH begin calculate distances locations:"), &json!(*locations.lock().unwrap()));
                }
                // end logs

		match self.measurements.as_mut() {
			Some(measurements) => {
				measurements.0.par_iter_mut().for_each(|measurement| {					
                                        let location = match &measurement.1.location {
						Some(location) => location,
						None => return,
					};

					let positive_distance = (BigDecimal::from(1) / (last_location.clone() - first_location.clone())) * (location - first_location.clone());
					measurement.1.positive_distance = Some(positive_distance.clone());
					measurement.1.negative_distance = Some(BigDecimal::from(1) - positive_distance);
				});
			}
			None => return Err("No measurements found".to_string()),
		}

                // begin logs
                if self.is_logging {
                        let postive_distances: Arc<Mutex<HashMap<String, BigDecimal>>> = Arc::new(Mutex::new(HashMap::new()));
                        self.measurements.as_ref().unwrap().0.par_iter().for_each(|measurement| {
                                if let Some(positive_distance) = &measurement.1.positive_distance {
                                        let mut postive_distances = postive_distances.lock().unwrap();
                                        postive_distances.insert(measurement.0.clone().to_string(), positive_distance.clone());
                                }
                        });

                        let negative_distances: Arc<Mutex<HashMap<String, BigDecimal>>> = Arc::new(Mutex::new(HashMap::new()));
                        self.measurements.as_ref().unwrap().0.par_iter().for_each(|measurement| {
                                if let Some(negative_distance) = &measurement.1.negative_distance {
                                        let mut negative_distances = negative_distances.lock().unwrap();
                                        negative_distances.insert(measurement.0.clone().to_string(), negative_distance.clone());
                                }
                        });

                        self.logs.log(&format!("3: BATCH end calculate distances postive_distances:"), &json!(*postive_distances.lock().unwrap()));
                        self.logs.log(&format!("4: BATCH end calculate distances negative_distances:"), &json!(*negative_distances.lock().unwrap()));
                }
                // end logs

		Ok(())
	}

	pub async fn vector_leveling(&mut self) -> Result<(), String> {
		let measurements = match &self.measurements {
			Some(measurements) => measurements,
			None => return Err("No measurements found".to_string()),
		};

		let first_amplitude = match measurements.0.get(&BigUint::from(0_u8)) {
			Some(first_measurement) => match &first_measurement.amplitude {
				Some(first_amplitude) => first_amplitude.clone(),
				None => return Err("No amplitude found in first measurement".to_string()),
			},
			None => return Err("No measurements found".to_string()),
		};

		let last_measurement_index = match self.last_measurement_index().await {
			Some(last_measurement_index) => last_measurement_index,
			None => return Err("No measurements found".to_string()),
		};

		let last_amplitude = match measurements.0.get(&last_measurement_index) {
			Some(last_measurement) => match &last_measurement.amplitude {
				Some(last_amplitude) => last_amplitude.clone(),
				None => return Err("No amplitude found in last measurement".to_string()),
			},
			None => return Err("No measurements found".to_string()),
		};

                // log begin
                if self.is_logging {
                        let amplitudes: Arc<Mutex<HashMap<String, BigDecimal>>> = Arc::new(Mutex::new(HashMap::new()));
                        self.measurements.as_ref().unwrap().0.par_iter().for_each(|measurement| {
                                if let Some(amplitude) = &measurement.1.amplitude {
                                        let mut amplitudes = amplitudes.lock().unwrap();
                                        amplitudes.insert(measurement.0.clone().to_string(), amplitude.clone());
                                }
                        });
                        self.logs.log(&format!("5: BATCH begin vector leveling amplitudes:"), &json!(*amplitudes.lock().unwrap()));
                }
                // log end

		match self.measurements.as_mut() {
			Some(measurements) => {
				measurements.0.par_iter_mut().for_each(|measurement| {
					let mut amplitude = match &measurement.1.amplitude {
						Some(amplitude) => amplitude.clone(),
						None => return,
					};
					let positive_distance = match measurement.1.positive_distance.clone() {
						Some(positive_distance) => positive_distance,
						None => return,
					};
					let negative_distance = match measurement.1.negative_distance.clone() {
						Some(negative_distance) => negative_distance,
						None => return,
					};

					// Positive vector leveling
					measurement.1.amplitude = Some(amplitude + ((BigDecimal::from(0) - first_amplitude.clone()) * negative_distance));

					amplitude = match &measurement.1.amplitude {
						Some(amplitude) => amplitude.clone(),
						None => return,
					};

					// Negative vector leveling
					measurement.1.amplitude = Some(amplitude + ((BigDecimal::from(0) - last_amplitude.clone()) * positive_distance));
				});
			}
			None => return Err("No measurements found".to_string()),
		}

                // log begin
                if self.is_logging {
                        let amplitudes: Arc<Mutex<HashMap<String, BigDecimal>>> = Arc::new(Mutex::new(HashMap::new()));
                        self.measurements.as_ref().unwrap().0.par_iter().for_each(|measurement| {
                                if let Some(amplitude) = &measurement.1.amplitude {
                                        let mut amplitudes = amplitudes.lock().unwrap();
                                        amplitudes.insert(measurement.0.clone().to_string(), amplitude.clone());
                                }
                        });
                        self.logs.log(&format!("6: BATCH end vector leveling amplitudes:"), &json!(*amplitudes.lock().unwrap()));
                }
                // log end

		Ok(())
	}

	pub async fn origin_transformation(&mut self) -> Result<(), String> {
		let measurements = match self.measurements.as_ref() {
			Some(measurements) => measurements,
			None => return Err("No measurements found".to_string()),
		};

		let first_location = match measurements.0.get(&BigUint::from(0_u8)) {
			Some(first_measurement) => match &first_measurement.location {
				Some(first_location) => first_location.clone(),
				None => return Err("No location found in first measurement".to_string()),
			},
			None => return Err("No first measurement found".to_string()),
		};

		let mut measurements = match self.measurements.as_ref() {
			Some(measurements) => measurements.clone(),
			None => return Err("No measurements found".to_string()),
		};

                // logs begin
                if self.is_logging {
                        let locations: Arc<Mutex<HashMap<String, BigDecimal>>> = Arc::new(Mutex::new(HashMap::new()));
                        self.measurements.as_ref().unwrap().0.par_iter().for_each(|measurement| {
                                if let Some(location) = measurement.1.location.clone() {
                                        let mut locations = locations.lock().unwrap();
                                        locations.insert(measurement.0.clone().to_string(), location);
                                }
                        });
                        self.logs.log(&format!("7: BATCH begin origin transformation locations:"), &json!(*locations.lock().unwrap()));
                }
                // logs end

		measurements.0.par_iter_mut().for_each(|measurement| {
			let location = match &measurement.1.location {
				Some(location) => location,
				None => return,
			};

			measurement.1.location = Some(location - first_location.clone());
		});

		self.measurements = Some(measurements.clone());

                // logs begin
                if self.is_logging {
                        let locations: Arc<Mutex<HashMap<String, BigDecimal>>> = Arc::new(Mutex::new(HashMap::new()));
                        self.measurements.as_ref().unwrap().0.par_iter().for_each(|measurement| {
                                if let Some(location) = measurement.1.location.clone() {
                                        let mut locations = locations.lock().unwrap();
                                        locations.insert(measurement.0.clone().to_string(), location);
                                }
                        });
                        self.logs.log(&format!("8: BATCH end origin transformation locations:"), &json!(*locations.lock().unwrap()));
                }
                // logs end

		Ok(())
	}

	pub async fn trend_vector_analysis(&mut self) -> Result<(), String> {
		let tvs: Arc<Mutex<HashMap<Uuid, Option<Vec<BigDecimal>>>>> = Arc::new(Mutex::new(HashMap::new()));
		let measurements = match &self.measurements {
			Some(measurements) => measurements,
			None => return Err("No measurements found".to_string()),
		};

		measurements.0.par_iter().for_each(|origin_measurement| {
			let trend_vectors: Arc<Mutex<Vec<BigDecimal>>> = Arc::new(Mutex::new(vec![]));
			let origin_location = match origin_measurement.1.location.clone() {
				Some(location) => location,
				None => return,
			};
			let origin_amplitude = match &origin_measurement.1.amplitude {
				Some(amplitude) => amplitude.clone(),
				None => return,
			};

			measurements.0.clone().par_iter().for_each(|end_measurement| {
				let mut t_vec = match trend_vectors.lock() {
					Ok(t_vec) => t_vec,
					Err(_) => return,
				};

				let end_location = match end_measurement.1.location.clone() {
					Some(location) => location,
					None => return,
				};

				if end_location == origin_location {
					t_vec.push(BigDecimal::from(0));
					return;
				}

				let end_amplitude = match &end_measurement.1.amplitude {
					Some(amplitude) => amplitude.clone(),
					None => return,
				};

				if end_amplitude.clone() - origin_amplitude.clone() == BigDecimal::from(0) {
					t_vec.push(BigDecimal::from(0));
					return;
				}

				t_vec.push((end_location - origin_location.clone()) / (end_amplitude - origin_amplitude.clone()));
			});

			let mut tvs = match tvs.lock() {
				Ok(tvs) => tvs,
				Err(_) => return,
			};

			let trend_vectors = match trend_vectors.lock() {
				Ok(trend_vectors) => trend_vectors,
				Err(_) => return,
			};
			tvs.insert(origin_measurement.1.uuid, Some(trend_vectors.clone()));
		});

		if let Some(measurements) = self.measurements.as_mut() {
			measurements.0.par_iter_mut().for_each(|measurement| {
				let tvs = match tvs.lock() {
					Ok(tvs) => tvs,
					Err(_) => return,
				};
				let trend_vectors = match tvs.get(&measurement.1.uuid) {
					Some(trend_vectors) => trend_vectors.clone(),
					None => return,
				};

				measurement.1.trend_vectors = trend_vectors;
			});
		}

		Ok(())
	}

	pub async fn simplify_transformation(&mut self) -> Result<(), String> {
		let mut simplified_measurements: HashMap<BigUint, Measurement> = HashMap::new();

                // logs begin
                if self.is_logging {
                        let locations_aplitudes: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
                        self.measurements.as_ref().unwrap().0.par_iter().for_each(|measurement| {
                                if let Some(location) = measurement.1.location.clone() {
                                        let mut locations_aplitudes = locations_aplitudes.lock().unwrap();
                                        locations_aplitudes.insert(measurement.0.clone().to_string(), format!("location: {}, Amplitude: {}", location, measurement.1.amplitude.clone().unwrap()));
                                }
                        });
                        self.logs.log(&format!("9: BATCH begin simplify transformation locations:"), &json!(*locations_aplitudes.lock().unwrap()));
                }
                // logs end

		let mut p = 0;
		let mut c = 1;
		let mut n = 2;

		let measurements = match &self.measurements {
			Some(measurements) => measurements,
			None => return Err("No measurements found".to_string()),
		};

		let first_measurement = match measurements.0.get(&BigUint::from(0_u8)) {
			Some(first_measurement) => first_measurement,
			None => return Err("No first measurement found".to_string()),
		};

		simplified_measurements.insert(BigUint::from(0_u8), first_measurement.clone());

		while c < self.measurements.clone().expect("no meaurements in batch").0.len() - 2 {
			let previous_amplitude = match measurements.0.get(&BigUint::from(p as u64)) {
				Some(previous_amplitude) => match &previous_amplitude.amplitude {
					Some(previous_amplitude) => previous_amplitude,
					None => return Err("No amplitude found in previous measurement".to_string()),
				},
				None => return Err("No amplitude found in previous measurement".to_string()),
			};

			let current_amplitude = match measurements.0.get(&BigUint::from(c as u64)) {
				Some(current_amplitude) => match &current_amplitude.amplitude {
					Some(current_amplitude) => current_amplitude,
					None => return Err("No amplitude found in current measurement".to_string()),
				},
				None => return Err("No amplitude found in current measurement".to_string()),
			};

			let next_amplitude = match measurements.0.get(&BigUint::from(n as u64)) {
				Some(next_amplitude) => match &next_amplitude.amplitude {
					Some(next_amplitude) => next_amplitude,
					None => return Err("No amplitude found in next measurement".to_string()),
				},
				None => return Err("No amplitude found in next measurement".to_string()),
			};

			let current_measurement = match measurements.0.get(&BigUint::from(c as u64)) {
				Some(current_measurement) => current_measurement,
				None => return Err("No current measurement found".to_string()),
			};

			// if both the previous and next amplitudes are greater than the current amplitude, then the current amplitude is a local maximum and should be kept
			if previous_amplitude > current_amplitude && next_amplitude > current_amplitude {
				simplified_measurements.insert(BigUint::from(c as u64), current_measurement.clone());
				p = c;
				c += 1;
				n += 1;
				continue;
			}

			// if both the previous and next amplitudes are less than the current amplitude, then the current amplitude is a local minimum and should be kept
			if previous_amplitude < current_amplitude && next_amplitude < current_amplitude {
				simplified_measurements.insert(BigUint::from(c as u64), current_measurement.clone());
				p = c;
				c += 1;
				n += 1;
				continue;
			}

			p = c;
			c += 1;
			n += 1;
		}

		let next_measurement = match measurements.0.get(&BigUint::from(n as u64)) {
			Some(next_measurement) => next_measurement,
			None => return Err("No next measurement found".to_string()),
		};

		simplified_measurements.insert(BigUint::from(n as u64), next_measurement.clone());

		self.measurements = Some(Measurements(simplified_measurements));

                // logs begin
                if self.is_logging {
                        let locations_aplitudes: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
                        self.measurements.as_ref().unwrap().0.par_iter().for_each(|measurement| {
                                if let Some(location) = measurement.1.location.clone() {
                                        let mut locations_aplitudes = locations_aplitudes.lock().unwrap();
                                        locations_aplitudes.insert(measurement.0.clone().to_string(), format!("Location: {}, Amplidute: {}", location, measurement.1.amplitude.clone().unwrap()));
                                }
                        });
                        self.logs.log(&format!("10: BATCH end simplify transformation locations:"), &json!(*locations_aplitudes.lock().unwrap()));
                }
                // logs end

		Ok(())
	}

	pub async fn finish(&mut self) {
		// log save log
                if self.is_logging {
		        self.logs.log("finish", &json!(""));
                }

		// save log
		let _ = self.logs.save("batch").await;
	}
}
