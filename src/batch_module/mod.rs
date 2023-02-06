#![allow(dead_code)]
use crate::assets::Asset;
use crate::assets::ToAssets;
pub use crate::influxdb2::*;
use crate::sources::Source;
pub use batch_new_event::*;
use batch_queue::*;
pub use length::*;
use measurement::*;
use measurement_new_event::*;
pub use parameters::*;
use std::collections::HashMap;
use std::str::FromStr;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use batch::*;
use rand::Rng;
use sled::Batch as SledBatch;

mod batch_measurement;
mod batch_new_event;
mod batch_queue;
mod length;
mod measurement;
mod measurement_new_event;
mod parameters;
mod batch;
mod batch_nature;
mod pattern;

#[derive(Debug, Clone)]
pub struct BatchModule {
	batch_module_parameters: BatchModuleParameters,
	batch_new_events: Option<Vec<BatchNewEvent>>,
	measurement_parameters: Option<MeasurementParameters>,
	measurement_new_events: Option<Vec<MeasurementNewEvent>>,
	measurements: Option<HashMap<String, Vec<Measurement>>>,
	batch_queues: Option<Vec<BatchQueue>>,
        batches: Option<Vec<Batch>>,
}

impl BatchModule {
	pub fn new(batch_parameters: BatchModuleParameters) -> BatchModule {
                println!("Batch Module Initialized...");
		BatchModule { batch_module_parameters: batch_parameters, batch_new_events: None, measurement_parameters: None, measurement_new_events: None, measurements: None, batch_queues: None, batches: None }
	}

	async fn get_batch_new_events(&mut self) -> Self {
		let mut influx = Influxdb2::new();
		let mut batch_new_events = Vec::new();
		influx.connection("http://localhost:8086", "***REMOVED-CREDENTIAL***==", "Mim", "dsm_events").await;
		influx.new_query().await;

		let mut filters: HashMap<String, InfluxField> = HashMap::new();
		filters.insert("_measurement".to_string(), InfluxField::String(BatchNewEvent::name()));
		filters.insert("_field".to_string(), InfluxField::String("batch_measurement_event_uuid".to_string()));
		filters.insert("batch_size".to_string(), InfluxField::String(self.batch_module_parameters.size().await.to_string()));
		filters.insert("batch_source".to_string(), InfluxField::String(self.batch_module_parameters.batch_source.to_string()));
		filters.insert("batch_asset_1".to_string(), InfluxField::String(self.batch_module_parameters.batch_source.to_assets().0.to_string()));
		filters.insert("batch_asset_2".to_string(), InfluxField::String(self.batch_module_parameters.batch_source.to_assets().1.to_string()));
		filters.insert("batch_interval".to_string(), InfluxField::String(self.batch_module_parameters.interval().await.to_string()));
		filters.insert("batch_interpolation".to_string(), InfluxField::String(self.batch_module_parameters.interpolation().await.to_string()));
		influx.select(0, None, FluxInterpolation::None, filters).await;

		let batch_new_events_res = influx.query().await;

		let mut batch_new_events_rdr = csv::Reader::from_reader(batch_new_events_res.as_bytes());
		for result in batch_new_events_rdr.records() {
			let record = result.unwrap();
			let batch_new_event = BatchNewEvent {
				batch_start: record[0].parse::<i64>().unwrap(),
				batch_end: record[1].parse::<i64>().unwrap(),
				batch_size: record[2].parse::<usize>().unwrap(),
				batch_source: Source::from_str(&record[3]).unwrap(),
				batch_asset_1: Asset::from_str(&record[4]).unwrap(),
				batch_asset_2: Asset::from_str(&record[5]).unwrap(),
				batch_interval: record[6].parse::<u64>().unwrap(),
				batch_interpolation: FluxInterpolation::from_str(&record[7]).unwrap(),
				batch_measurement_event_uuid: record[8].to_string(),
			};
			batch_new_events.push(batch_new_event);
		}

		if batch_new_events.is_empty() {
			self.batch_new_events = None;
		} else {
			self.batch_new_events = Some(batch_new_events.clone());
		}

                println!("Received {} batch_new events...", batch_new_events.len());
		self.clone()
	}

	async fn define_measurmement_parameters(&mut self) -> Self {
		if self.batch_new_events.is_none() {
			self.get_batch_new_events().await;
		}

		let mut batch_measurement_event_uuids = Vec::new();
		if self.batch_new_events.is_some() {
			// collect all batch_new_event.batch_measurement_event_uuids
			for batch_new_event in self.batch_new_events.as_ref().unwrap() {
				batch_measurement_event_uuids.push(batch_new_event.batch_measurement_event_uuid.clone());
			}
		}

		self.measurement_parameters = Some(MeasurementParameters::new(self.batch_module_parameters.clone(), batch_measurement_event_uuids).await);
		
                //write measurement_parameters to file
                //File::create("measurement_parameters.json").await.unwrap().write_all(serde_json::to_string_pretty(&self.measurement_parameters.as_ref().unwrap()).unwrap().as_bytes()).await.unwrap();

                println!("Defined measurement parameters...");
                self.clone()
	}

	pub async fn get_measurement_new_events(&mut self) -> Self {
		if self.measurement_parameters.is_none() {
			self.define_measurmement_parameters().await;
		}

		if self.measurement_parameters.is_some() {
			let mut influx = Influxdb2::new();
			influx.connection("http://localhost:8086", "***REMOVED-CREDENTIAL***==", "Mim", "dsm_events").await;
			influx.new_query().await;

			let mut filters: HashMap<String, InfluxField> = HashMap::new();
			filters.insert("_measurement".to_string(), InfluxField::String(MeasurementNewEvent::name()));
			filters.insert("_field".to_string(), InfluxField::String("measurement_uuid".to_string()));
			filters.insert("measurement_source".to_string(), InfluxField::String(self.measurement_parameters.as_ref().unwrap().measurement_parameter_source.to_string()));
			filters.insert("measurement_asset_1".to_string(), InfluxField::String(self.measurement_parameters.as_ref().unwrap().measurement_parameter_source.to_assets().0.to_string()));
			filters.insert("measurement_asset_2".to_string(), InfluxField::String(self.measurement_parameters.as_ref().unwrap().measurement_parameter_source.to_assets().1.to_string()));
			influx.select(0, None, FluxInterpolation::None, filters.clone()).await;

			let measurement_new_events_res = influx.query().await;

			let mut measurement_new_events_rdr = csv::Reader::from_reader(measurement_new_events_res.as_bytes());

			let mut measurement_new_events = Vec::new();
			for (i, result) in measurement_new_events_rdr.deserialize().enumerate() {
				let measurement_new_event: MeasurementNewEvent = match result {
					Ok(measurement_new_event) => measurement_new_event,
					Err(e) => {
						panic!("error deserializing measurement_new_event {}: {}", i, e);
					}
				};
				measurement_new_events.push(measurement_new_event);
			}

			// filter measurement_new_events where measurement_event_uuid is not in measurement_parameters.batch_measurement_event_uuids
			let mut measurement_new_events_filtered = Vec::new();
			for measurement_new_event in measurement_new_events {
				if !self.measurement_parameters.as_ref().unwrap().measurement_parameter_used_measurement_event_uuids.contains(&measurement_new_event.measurement_uuid) {
					measurement_new_events_filtered.push(measurement_new_event);
				}
			}

			if measurement_new_events_filtered.is_empty() {
				self.measurement_new_events = None;
			} else {
				self.measurement_new_events = Some(measurement_new_events_filtered.clone());
			}
		} else {
			panic!("Failed to get measurement_parameters");
		}

                println!("Received {} measurement_new events...", self.measurement_new_events.as_ref().unwrap().len());

		self.clone()
	}

	pub async fn get_measurements(&mut self) -> Self {
		if self.measurement_new_events.is_none() {
			self.get_measurement_new_events().await;
		}

		if self.measurement_new_events.is_some() {
			let mut measure = HashMap::new();

			let measurement_name = self.batch_module_parameters.batch_source.to_string();

			for event in self.measurement_new_events.as_ref().unwrap() {
				let mut influx = Influxdb2::new();
				influx.connection("http://localhost:8086", "***REMOVED-CREDENTIAL***==", "Mim", "dsm_measurements").await;
				influx.new_query().await;

				let mut filters: HashMap<String, InfluxField> = HashMap::new();
				filters.insert("_measurement".to_string(), InfluxField::String(measurement_name.to_string()));
				filters.insert("_field".to_string(), InfluxField::String("measurement_ratio".to_string()));
				filters.insert("measurement_asset_1".to_string(), InfluxField::String(self.measurement_parameters.as_ref().unwrap().measurement_parameter_source.to_assets().0.to_string()));
				filters.insert("measurement_asset_2".to_string(), InfluxField::String(self.measurement_parameters.as_ref().unwrap().measurement_parameter_source.to_assets().1.to_string()));
				let start = event.measurement_time - self.batch_module_parameters.size().await as i64;
				let end = event.measurement_time + self.batch_module_parameters.size().await as i64;
				let interpolation = self.batch_module_parameters.interpolation().await;
				influx.select(start, Some(end), interpolation, filters.clone()).await;

				let measurement_res = influx.query().await;

				let mut measurement_rdr = csv::Reader::from_reader(measurement_res.as_bytes());

				let mut measurements = Vec::new();
				for (i, result) in measurement_rdr.deserialize().enumerate() {
					let measurement: Measurement = match result {
						Ok(measurement) => measurement,
						Err(e) => {
							panic!("error deserializing measurement {}: {}", i, e);
						}
					};
					measurements.push(measurement);
				}

				measure.insert(event.measurement_uuid.clone(), measurements);
			}

                        println!("Received {} measurements...", measure.len());

			self.measurements = Some(measure);
		}

		self.clone()
	}

	pub async fn get_batch_queues(&mut self) -> Self {
		if self.measurements.is_none() {
			self.get_measurements().await;
		}

		if self.measurements.is_some() {
			let mut batches = Vec::new();
			for (measurement_uuid, measurements) in self.measurements.as_ref().unwrap() {
				let batch = BatchQueue::new(measurement_uuid.clone(), measurements.clone(), self.measurement_parameters.as_ref().unwrap().clone()).await;
				batches.push(batch);
			}

			self.batch_queues = Some(batches);
		}

		// write batches to file
		//File::create("batches.json").await.unwrap().write_all(serde_json::to_string_pretty(&self.batch_queues).unwrap().as_bytes()).await.unwrap();

                println!("Number of measurements in batch queues: {}", self.batch_queues.as_ref().unwrap()[0].batch_measurements.len());

                println!("Received {} batch queues...", self.batch_queues.as_ref().unwrap().len());

		self.clone()
	}

        pub async fn get_batches(&mut self) -> Self {
                if self.batch_queues.is_none() {
                        self.get_batch_queues().await;
                }

                if self.batch_queues.is_some() {
                        let mut batches = Vec::new();
                        for batch_queue in self.batch_queues.as_mut().unwrap() {
                                let number_of_measurements = batch_queue.batch_measurements.len();
                                let interval = batch_queue.batch_interval as usize;
                                let number_of_batches = number_of_measurements - (number_of_measurements % interval);

                                for i in 0..number_of_batches {
                                        if i + interval > number_of_measurements {
                                                break;
                                        }

                                        let measurements = batch_queue.batch_measurements[i..(i + interval)].to_vec();
                                        let batch = batch_queue.to_batch(measurements);
                                        batches.push(batch);
                                }
                        }

                        self.batches = Some(batches);
                }

                // write random batch to file
                let index = rand::thread_rng().gen_range(0..self.batches.as_ref().unwrap().len());
                File::create("batches.json").await.unwrap().write_all(serde_json::to_string_pretty(&self.batches.as_ref().unwrap()[index]).unwrap().as_bytes()).await.unwrap();

                println!("Received {} batches...", self.batches.as_ref().unwrap().len());
                self.clone()
        }
  
        pub async fn process_batches(&mut self) -> Self {
                if self.batches.is_none() {
                        self.get_batches().await;
                }

                if self.batches.is_some() {
                        for batch in self.batches.as_mut().unwrap() {
                                batch.measurement_distance_calculation().await;
                                batch.measurement_vector_leveling().await;
                                batch.measurement_origin_transformation().await;
                                batch.measurement_trend_vector_analysis().await;
                                batch.measurement_simplify_transformation().await;
                                batch.compute_nature().await;
                        }
                }

                // write random processed batch to file
                let index = rand::thread_rng().gen_range(0..self.batches.as_ref().unwrap().len());
                File::create("processed_batches.json").await.unwrap().write_all(serde_json::to_string_pretty(&self.batches.as_ref().unwrap()[index]).unwrap().as_bytes()).await.unwrap();

                println!("Processed {} batches...", self.batches.as_ref().unwrap().len());
                self.clone()
        }

        pub async fn send(&self) {
                
                if self.batches.is_some() {
                        println!("Sending {} patterns to database.", self.batches.as_ref().unwrap().len());

                        let db: sled::Db = sled::open("patterns.db").unwrap();
                        let pattern_queue_tree = db.open_tree("patterns_queue").unwrap();
                        

                        let mut sled_batch = SledBatch::default();
                        for batch in self.batches.as_ref().unwrap() {
                                let (u, p) = batch.gather().await;
                                sled_batch.insert(u.as_bytes(), p);
                        }

                        pattern_queue_tree.apply_batch(sled_batch).unwrap();
                        db.flush_async().await.unwrap();

                        drop(db);
                }
        }
}
