#![feature(async_closure)]
use bigdecimal::BigDecimal;
use dsm_batch::{Batch, BatchLength};
use dsm_measurement::{Measurement, MeasurementEvent};
use num_bigint::BigUint;
use router::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::str::FromStr;
use std::{env, thread};
use tokio::time::{sleep, Duration};
use uuid::Uuid;
use bigdecimal::ToPrimitive;

mod router;

#[tokio::main]
async fn main() {
	//env::set_var("RUST_BACKTRACE", "full");

	let server = thread::spawn(async move || {
		println!("server running...");
		let app = router();

		let port = env::var("PORT").unwrap_or("8517".to_string());
		let address = format!("0.0.0.0:{}", port);
		println!("Listening on address {}", address);

		// run it with hyper on localhost:3000
		axum::Server::bind(&address.parse().unwrap()).serve(app.into_make_service()).await.unwrap();
	});

	let app = thread::spawn(async move || {
		println!("app running...");
		'app: loop {
			let measurement_events = match get_measurement_events().await {
				Some(m) => m,
				None => {
					println!("no measurement events");
					pause().await;
					continue 'app;
				}
			};
			println!("number of measurements: {}", measurement_events.len());

			// start timer
			let start = std::time::Instant::now();
			println!("processing batches...");

			'events: for event in measurement_events.iter() {
				// set defaults for batch
				let length = BatchLength::TenSeconds;

				// create batch
				let key = event.0;
				let event = &event.1;
				let mut batch = Batch::new(event.bucket.clone(), length, false);
				let start = event.key.clone() - (batch.size().await / BigDecimal::from(2));
				let end = event.key.clone() + (batch.size().await / BigDecimal::from(2));
				let interpolation = "linear".to_string();
				let interpolation_steps = batch.size().await;

				// get measurements
				let measurements = match get_measurements(&batch.measurement_bucket, start, end, &interpolation, interpolation_steps.clone(), &batch).await {
					Ok(m) => m,
					Err(e) => {
						println!("error: getting measurements for batch: {}", e);
						continue 'events;
					}
				};

                                if measurements.len() < interpolation_steps.to_u64().unwrap() as usize {
                                        println!("error: not enough measurements for batch");
                                        continue 'events;
                                }

				// add measurements to batch
				batch.add_measurements(measurements).await;

				// process batch
				batch.calculate_measurement_distances().await.unwrap();
				batch.vector_leveling().await.unwrap();
				batch.origin_transformation().await.unwrap();
				batch.simplify_transformation().await.unwrap();
				batch.calculate_relative_movements().await.unwrap();
				batch.finish().await;

				//println!("batch: {}", batch.uuid);

				// delete measurement event
				if let Err(e) = delete_measurement_event(&key.to_string()).await {
					println!("error: deleting measurement event {}: {}", key, e);
					pause().await;
					continue 'events;
				}

				// add batch to bucket
				if let Err(e) = save_batch(&batch).await {
					println!("error: saving batch {}: {}", batch.uuid, e);
					pause().await;
					continue 'events;
				}
			}

			let duration = start.elapsed();
			println!("Batch processing: {}s", duration.as_secs_f64());

			sleep(Duration::from_secs(1)).await;
		}
	});

	tokio::join!(server.join().unwrap(), app.join().unwrap());

	std::future::pending::<()>().await;
}

/// get measurement events from bucket
/// returns a vector of tuples containing the key and the measurement event
/// returns only events of measurement_add type
pub async fn get_measurement_events() -> Option<Vec<(Uuid, MeasurementEvent)>> {
	let client = reqwest::Client::new();
	let url = "http://127.0.0.1:8515/bucket/measurement_event";
	let res = client.get(url).send().await;

	match res {
		Ok(res) => {
			if res.status() != 200 {
				println!("Error retrieving measurement events: {}", res.status());
				return None;
			}
			match res.json::<Value>().await {
				Ok(body) => {
					let events: Vec<Value> = match serde_json::from_value(body["value"].clone()) {
						Ok(events) => events,
						Err(_) => {
							println!("No events found..");
							return None;
						}
					};
					let events: Vec<(Uuid, MeasurementEvent)> = events
						.iter()
						.map(|event| {
							let e = serde_json::from_value(event["value"].clone()).unwrap();
							let u = serde_json::from_value(event["key"].clone()).unwrap();
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
				Err(err) => {
					println!("Error parsing response body: {}", err);
					None
				}
			}
		}
		Err(err) => {
			println!("Error: {}", err);
			None
		}
	}
}

async fn pause() {
	sleep(Duration::from_secs(1)).await;
}

async fn measurements_from_add_events(events: Vec<Value>, batch: &Batch) -> Result<Vec<Measurement>, String> {
	let mut measurements = Vec::new();
	for value in events.iter() {
		let source = match value["tags"]["source"].as_str() {
			Some(s) => s,
			None => batch.measurement_bucket.as_str(),
		};
		let numerator_asset = batch.numerator_asset().await;
		let denominator_asset = batch.denominator_asset().await;
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
		let measurement = Measurement::new(source, &numerator_asset, &denominator_asset, uuid, timestamp, value);
		measurements.push(measurement);
	}
	if measurements.is_empty() {
		return Err("no measurements".to_string());
	}
	Ok(measurements)
}

async fn get_measurements(bucket: &str, start: BigDecimal, end: BigDecimal, interpolation: &str, take: BigDecimal, batch: &Batch) -> Result<Vec<Measurement>, String> {
	let client = reqwest::Client::new();
	let url = format!("http://127.0.0.1:8515/bucket/{}?range[1]={}&range[2]={}&interpolation={}&take={}", bucket, start, end, interpolation, take);
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
			match measurements_from_add_events(values, batch).await {
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
	let source = batch.measurements.clone().expect("No measurements found.").0.get(&BigUint::from(0_u8)).unwrap().source.clone();
	let uuid = batch.uuid;
	let start = batch.start_timestamp().await.unwrap();
	let end = batch.end_timestamp().await.unwrap();
	let locations = batch.relative_x_movements.clone().expect("no x movements");
	let amplitudes = batch.relative_y_movements.clone().expect("no y movements");
	let length = batch.length.clone().to_string();
	let measurement_bucket = batch.measurement_bucket.clone();

	let locations: HashMap<String, BigDecimal> = locations.0.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
	let amplitudes: HashMap<String, BigDecimal> = amplitudes.0.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();

	let body = json!({
			"source": source,
			"start": start,
			"end": end,
			"locations": locations,
			"amplitudes": amplitudes,
			"length": length,
			"measurement_bucket": measurement_bucket,
	});

	// create bucket
	let url = format!("http://127.0.0.1:8515/bucket?name={}&type=object", bucket);
	let res = client.post(url).send().await;
	match res {
		Ok(_) => {}
		Err(err) => return Err(format!("Error creating bucket: {}", err)),
	}

	// add object to bucket
	let url = format!("http://127.0.0.1:8515/bucket/{}?key={}", bucket, uuid);
	let res = client.post(url).json(&body).send().await;

	match res {
		Ok(rea) => {
			if rea.status() != 200 {
				return Err(format!("Error adding object to bucket: {}", rea.status()));
			}
			Ok(())
		}
		Err(err) => Err(format!("Error adding object to bucket: {}", err)),
	}
}
