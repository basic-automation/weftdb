#![feature(async_closure, ip_in_core)]
use bigdecimal::BigDecimal;
use bigdecimal::ToPrimitive;
use core::net::SocketAddr;
use dsm_batch::Batch;
use dsm_config::BatchLength;
use dsm_config::Configuration;
use dsm_measurement::{Measurement, MeasurementEvent};
use futures::future::join_all;
use rayon::iter::IntoParallelRefIterator;
use rayon::iter::ParallelIterator;
use router::*;
use serde_json::{json, Value};
use std::str::FromStr;
use std::sync::Mutex;
use std::thread;
use std::{collections::HashMap, sync::Arc};
use threadpool::ThreadPool;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

use std::env;

mod router;

#[tokio::main]
async fn main() {
	// set RUST_BACKTRACE=1
	env::set_var("RUST_BACKTRACE", "full");

	let server = thread::spawn(async move || {
		println!("server running...");

		let config = match Configuration::open("../dsm.toml").await {
			Ok(config) => config,
			Err(e) => panic!("Error: Failed to open configuration file: {}", e),
		};
		let app = router();

		let port = config.dsm.ports.batcher;
		let address = format!("0.0.0.0:{}", port);

		println!("Listening on address {}", address);
		let address: SocketAddr = match address.parse() {
			Ok(address) => address,
			Err(e) => panic!("Error: Failed to parse address: {}", e),
		};

		if let Err(e) = axum::Server::bind(&address).serve(app.into_make_service()).await {
			panic!("Error: Failed to bind server: {}", e);
		}
	});

	let app = thread::spawn(async move || {
		println!("app running...");
		'app: loop {
			let measurement_events = match get_measurement_events(None, None).await {
				Some(m) => m,
				None => {
					println!("no measurement events...");
					pause().await;
					continue 'app;
				}
			};

			println!("number of measurements: {}", measurement_events.len());

			let measurement_events = match get_batch_lengths_for_events(measurement_events).await {
				Ok(measurement_events) => measurement_events,
				Err(e) => panic!("Error: Failed to get batch lengths for events: {}", e),
			};
			let measurement_events_arc = Arc::new(Mutex::new(measurement_events.clone()));

			let pool = ThreadPool::with_name("process events thread".into(), 100);

			pool.execute(move || {
				let rt = match tokio::runtime::Runtime::new() {
					Ok(rt) => rt,
					Err(_) => panic!("Error: Faild to create new runtime."),
				};
				let i = Arc::new(Mutex::new(0));
				let measurement_events_arc = match measurement_events_arc.lock() {
					Ok(measurement_events_arc) => measurement_events_arc,
					Err(e) => panic!("Error: Failed to lock measurement events arc: {}", e),
				};
				measurement_events_arc.par_iter().for_each(|event| {
					// start timer
					let start = std::time::Instant::now();

					let (event_id, event, lengths) = event.clone();

					let event_id = Arc::new(Mutex::new(event_id));
					let lengths = lengths;
					let event = Arc::new(Mutex::new(event));

					let mut lgths_handles = Vec::new();
					for length in lengths {
						let event_id = event_id.clone();
						let event = event.clone();
						lgths_handles.push(thread::spawn(async move || {
							let length = length;
							let event = match event.lock() {
								Ok(event) => event.clone(),
								Err(e) => panic!("Error: Failed to lock event: {}", e),
							};
							let event_id = match event_id.lock() {
								Ok(id) => *id,
								Err(e) => panic!("Error: Faild to lock event_id: {}", e),
							};

							if let Err(e) = process_batch(length, event.clone()).await {
								println!("error: processing batch for measurement event {}: {}", event_id, e);
							}
						}));
					}

					let handles = lgths_handles
						.drain(..)
						.map(|h| match h.join() {
							Ok(h) => h,
							Err(e) => panic!("Faild to join thread: {:?}", e),
						})
						.collect::<Vec<_>>();

					rt.block_on(join_all(handles));

					let event_id = match event_id.lock() {
						Ok(id) => *id,
						Err(e) => panic!("Error: Faild to lock event_id: {}", e),
					};
					if let Err(e) = rt.block_on(delete_measurement_event(&event_id.to_string())) {
						println!("error: deleting measurement event {}: {}", event_id, e);
					}

					// stop timer
					let mut i = match i.lock() {
						Ok(i) => i,
						Err(e) => panic!("Error: Failed to lock i: {}", e),
					};
					let duration = start.elapsed();
					println!("Successfully processed measurement event {} in {}ms", i, duration.as_millis());

					*i += 1;
				});

				drop(rt);
			});

			pool.join();

			pause().await;
		}
	});

	tokio::join!(server.join().unwrap(), app.join().unwrap());

	std::future::pending::<()>().await;
}

/// get measurement events from bucket
/// returns a vector of tuples containing the key and the measurement event
/// returns only events of measurement_add type
pub async fn get_measurement_events(page: Option<usize>, count: Option<usize>) -> Option<Vec<(Uuid, MeasurementEvent)>> {
	let config = match Configuration::open("../dsm.toml").await {
		Ok(config) => config,
		Err(e) => panic!("Error: Failed to open configuration file: {}", e),
	};
	let client = reqwest::Client::new();

	let event_bucket = "measurement_event";
	let host = config.dsm.database.host;
	let port = config.dsm.database.port;

	let mut url = format!("http://{host}:{port}/bucket/{event_bucket}");

	if page.is_some() && count.is_some() {
		let page = page.unwrap_or(1);
		let count = count.unwrap_or(1);
		url = format!("http://{host}:{port}/bucket/{event_bucket}?page={page}&count={count}");
	}

	let res = client.get(url).send().await;

	let res = match res {
		Ok(res) => {
			if res.status() != 200 {
				return None;
			}
			res
		}
		Err(_) => return None,
	};

	let body = match res.json::<Value>().await {
		Ok(body) => body,
		Err(_) => return None,
	};

	let events: Vec<Value> = match serde_json::from_value(body["value"].clone()) {
		Ok(events) => events,
		Err(_) => return None,
	};

	let events: Vec<(Uuid, MeasurementEvent)> = events
		.iter()
		.map(|event| {
			let e = match serde_json::from_value(event["value"].clone()) {
				Ok(e) => e,
				Err(e) => panic!("Error: Failed to deserialize event: {}", e),
			};
			let u = match serde_json::from_value(event["key"].clone()) {
				Ok(u) => u,
				Err(e) => panic!("Error: Failed to deserialize event key: {}", e),
			};
			(u, e)
		})
		.collect();

	let mut add_events = Vec::new();
	events.iter().for_each(|event| {
		if event.1.event_type == "measurement_add" {
			add_events.push(event.clone());
		}
	});

	if add_events.is_empty() {
		return None;
	}

	Some(add_events)
}

async fn pause() {
	sleep(Duration::from_secs(1)).await;
}

async fn measurements_from_add_events(events: Vec<Value>) -> Result<Vec<Measurement>, String> {
	let mut measurements = Vec::new();
	for value in events.iter() {
		let uuid = match value["tags"]["uuid"].as_str() {
			Some(s) => match Uuid::parse_str(s) {
				Ok(u) => u,
				Err(_) => Uuid::new_v4(),
			},
			None => Uuid::new_v4(),
		};
		let timestamp = match value["timestamp"].as_str() {
			Some(s) => match BigDecimal::from_str(s) {
				Ok(b) => b,
				Err(e) => return Err(format!("no timestamp :: {}", e)),
			},
			None => return Err("no timestamp".to_string()),
		};
		let value = match value["value"].as_str() {
			Some(s) => match BigDecimal::from_str(s) {
				Ok(b) => b,
				Err(e) => return Err(format!("no value :: {}", e)),
			},
			None => return Err("no value".to_string()),
		};
		let measurement = Measurement::new(uuid, timestamp, value);
		measurements.push(measurement);
	}
	if measurements.is_empty() {
		return Err("no measurements".to_string());
	}
	Ok(measurements)
}

async fn get_measurements(bucket: &str, start: BigDecimal, end: BigDecimal, interpolation: &str, take: BigDecimal) -> Result<Vec<Measurement>, String> {
	let client = reqwest::Client::new();
	let url = format!("http://127.0.0.1:8515/bucket/{bucket}?range[1]={start}&range[2]={end}&interpolation={interpolation}&take={take}");
	//println!("batch url: {}", url);
	let res = client.get(&url).send().await;
	let measurements = match res {
		Ok(res) => {
			let body: Value = match res.json().await {
				Ok(b) => b,
				Err(e) => return Err(format!("error: getting measurements for batch: url: {} :: {}", url, e)),
			};
			let values: Vec<Value> = match serde_json::from_value(body["value"].clone()) {
				Ok(v) => v,
				Err(e) => return Err(format!("error: getting measurements for batch: url: {} :: {}", url, e)),
			};
			match measurements_from_add_events(values).await {
				Ok(m) => m,
				Err(e) => return Err(format!("error: getting measurements for batch: url: {} :: {}", url, e)),
			}
		}
		Err(e) => return Err(format!("error: getting measurements for batch: url: {} :: {}", url, e)),
	};
	Ok(measurements)
}

pub async fn delete_measurement_event(key: &str) -> Result<(), String> {
	let client = reqwest::Client::new();
	let bucket = "measurement_event";
	let url = format!("http://127.0.0.1:8515/bucket/{bucket}/{key}");
	let res = client.delete(url).send().await;
	match res {
		Ok(res) => {
			if res.status() != 200 {
				return Err(format!("Error deleting event: {}", res.status()));
			}
			Ok(())
		}
		Err(err) => Err(format!("Error: {}", err)),
	}
}

async fn save_batch(batch: &Batch) -> Result<(), String> {
	let client = reqwest::Client::new();
	let bucket = "batch";
	let uuid = batch.uuid;
	let start = match batch.start_timestamp().await {
		Some(s) => s,
		None => return Err(format!("error: getting start timestamp for batch: {uuid}")),
	};
	let end = match batch.end_timestamp().await {
		Some(e) => e,
		None => return Err(format!("error: getting end timestamp for batch: {uuid}")),
	};
	let locations = batch.relative_x_movements.clone().expect("no x movements");
	let amplitudes = batch.relative_y_movements.clone().expect("no y movements");
	let length = batch.length.clone().to_string();
	let dataset_name = batch.dataset_name.clone();

	let locations: HashMap<String, BigDecimal> = locations.0.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
	let amplitudes: HashMap<String, BigDecimal> = amplitudes.0.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();

	let body = json!({
			"start": start,
			"end": end,
			"locations": locations,
			"amplitudes": amplitudes,
			"length": length,
			"dataset_name": dataset_name,
	});

	// create bucket
	let url = format!("http://127.0.0.1:8515/bucket?name={bucket}&type=object");
	let res = client.post(url.clone()).send().await;
	match res {
		Ok(_) => {}
		Err(err) => return Err(format!("Error creating bucket: {err}")),
	}

	// add object to bucket
	let url = format!("http://127.0.0.1:8515/bucket/{bucket}?key={uuid}");
	let res = client.post(url).json(&body).send().await;

	match res {
		Ok(rea) => {
			if rea.status() != 200 {
				return Err(format!("Error adding object to bucket: {}", rea.status()));
			}
			Ok(())
		}
		Err(err) => Err(format!("Error adding object to bucket: {err}")),
	}
}

async fn get_batch_lengths_for_events(measurement_events: Vec<(Uuid, MeasurementEvent)>) -> Result<Vec<(Uuid, MeasurementEvent, Vec<dsm_config::BatchLength>)>, String> {
	let config = match Configuration::open("../dsm.toml").await {
		Ok(c) => c,
		Err(e) => return Err(format!("error: opening config: {}", e)),
	};
	let mut batch_lengths: Vec<(Uuid, MeasurementEvent, Vec<dsm_config::BatchLength>)> = Vec::new();
	for (uuid, event) in measurement_events {
		let dataset_name = match event.dataset_name.clone() {
			Some(d) => d,
			None => return Err(format!("error: getting dataset name for event: {uuid}")),
		};
		let dataset = match config.get_dataset(&dataset_name) {
			Ok(d) => d,
			Err(e) => return Err(format!("error: getting dataset: {e}")),
		};

		let min_length: usize = dataset.minimum_batch_length.into();
		let max_length: usize = dataset.maximum_batch_length.into();

                let mut lengths = dsm_config::BatchLength::range(min_length, max_length);
                lengths.push(dataset.minimum_batch_length);
                lengths.push(dataset.maximum_batch_length);

		batch_lengths.push((uuid, event, lengths));
	}
	Ok(batch_lengths)
}

async fn process_batch(length: BatchLength, event: MeasurementEvent) -> Result<(), String> {
        
	let dataset_name = match event.dataset_name {
		Some(d) => d,
		None => return Err(format!("error: getting dataset name for event.")),
	};
	let mut batch = Batch::new(dataset_name, length.to_owned(), false);
	let start = event.key.clone() - (batch.size().await / BigDecimal::from(2));
	let end = event.key.clone() + (batch.size().await / BigDecimal::from(2));
	let interpolation = "linear".to_string();
	let interpolation_steps = batch.interval().await;

	//println!("event: {:?}, start: {:?}, end: {:?}", event.key, start, end);
	//println!("batch size: {:?}, batch steps: {:?} :: interpolation_steps: {}", batch.length, batch.interpolation().await, interpolation_steps);

	// get measurements
	let measurements = match get_measurements(&batch.dataset_name, start, end, &interpolation, interpolation_steps.clone()).await {
		Ok(v) => v,
		Err(e) => return Err(format!("error: getting measurements for batch: url: {} :: {}", batch.dataset_name, e)),
	};

	let interpolation_steps_usize = match interpolation_steps.to_u64() {
		Some(v) => v as usize,
		None => return Err("error: interpolation_steps.to_u64()".to_string()),
	};

	if measurements.len() < interpolation_steps_usize {
		//println!("error: not enough measurements for batch");
		return Err("Not enough measurements for batch.".to_string());
	}

	// add measurements to batch
	batch.add_measurements(measurements).await;

	// process batch
	match batch.calculate_measurement_distances().await {
		Ok(_) => {}
		Err(e) => return Err(format!("error: calculating measurement distances: {}", e)),
	};

	match batch.vector_leveling().await {
		Ok(_) => {}
		Err(e) => return Err(format!("error: vector leveling: {}", e)),
	};

	match batch.origin_transformation().await {
		Ok(_) => {}
		Err(e) => return Err(format!("error: origin transformation: {}", e)),
	};

	match batch.simplify_transformation().await {
		Ok(_) => {}
		Err(e) => return Err(format!("error: simplify transformation: {}", e)),
	};

	match batch.calculate_relative_movements().await {
		Ok(_) => {}
		Err(e) => return Err(format!("error: calculating relative movements: {}", e)),
	};

	batch.finish().await;

	// add batch to bucket
	if let Err(e) = save_batch(&batch).await {
		return Err(format!("error: saving batch {}: {}", batch.uuid, e));
	}

	Ok(())
}
