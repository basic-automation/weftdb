#![feature(async_closure)]
use dsm_measurement::{MeasurementEvent, Measurement};
use router::*;
use std::{env, thread};
use tokio::time::{sleep, Duration};
use serde_json::{Value, json};
use dsm_batch::{Batch, BatchLength};
use uuid::Uuid;
use bigdecimal::BigDecimal;
use std::str::FromStr;

mod router;

#[tokio::main]
async fn main() {
	env::set_var("RUST_BACKTRACE", "1");

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
		loop {
                        let measurement_events = get_measurement_events().await;
                        println!("number of measurements: {}", measurement_events.len());

                        // start timer
                        let start = std::time::Instant::now();
                        println!("processing batches...");

                        for event in measurement_events.iter() {
                                let length = BatchLength::ThreeHour;
                                let key = event.0;
                                let event = &event.1;
                                let mut batch = Batch::new(event.bucket.clone(), length);

                                let start = event.key - (batch.size().await / 2) as i64;
                                let end = event.key + (batch.size().await / 2) as i64;
                                let interpolation = "linear".to_string();
                                let interpolation_steps = batch.interpolation_steps().await;

                                let client = reqwest::Client::new();
                                let url = format!("http://127.0.0.1:8515/bucket/{}?range[1]={}&range[2]={}&interpolation={}&steps={}", batch.measurement_bucket, start, end, interpolation, interpolation_steps);
                                //println!("url: {}", url);
                                let res = client.get(&url).send().await;
                                match res {
                                        Ok(res) => {
                                                let body: Value = res.json().await.unwrap();
                                                let values: Vec<Value> = serde_json::from_value(body["value"].clone()).unwrap();
                                                for value in values.iter() {
                                                        let measurement = Measurement::new(value["tags"]["source"].as_str().unwrap(), batch.numerator_asset().await.as_str(), batch.denominator_asset().await.as_str(), Uuid::parse_str(value["tags"]["uuid"].as_str().unwrap()).unwrap(), value["timestamp"].as_i64().unwrap(), BigDecimal::from_str(value["value"].as_str().unwrap()).unwrap());
                                                        batch.add_measurement(measurement).await;
                                                }

                                                match batch.calculate_measurement_distances().await {
                                                        Ok(_) => {
                                                                // stop timer
                                                                /* let duration = start.elapsed();
                                                                println!("Calculated measurement distances: {}s", duration.as_secs_f64()); */
                                                        },
                                                        Err(e) => println!("error: {}", e),
                                                };

                                                match batch.vector_leveling().await {
                                                        Ok(_) => {
                                                                // stop timer
                                                                /* let duration = start.elapsed();
                                                                println!("Vector leveling: {}s", duration.as_secs_f64()); */
                                                        },
                                                        Err(e) => println!("error: {}", e),
                                                };

                                                match batch.origin_transformation().await {
                                                        Ok(_) => {
                                                                // stop timer
                                                                /* let duration = start.elapsed();
                                                                println!("Origin transformation: {}s", duration.as_secs_f64());
                                                                println!("batch measurements: {}", batch.measurements.clone().expect("no measurements").len()); */
                                                        },
                                                        Err(e) => println!("error: {}", e),
                                                };

                                                match batch.simplify_transformation().await {
                                                        Ok(_) => {
                                                                // stop timer
                                                                /* let duration = start.elapsed();
                                                                println!("Simplify transformation: {}s", duration.as_secs_f64());
                                                                println!("batch measurements: {}", batch.measurements.clone().expect("no measurements").len()); */
                                                        },
                                                        Err(e) => println!("error: {}", e),
                                                };

                                                match batch.calculate_relative_movements().await {
                                                        Ok(_) => {
                                                                // stop timer
                                                                /* let duration = start.elapsed();
                                                                println!("Calculate relative movements: {}s", duration.as_secs_f64()); */
                                                        },
                                                        Err(e) => println!("error: {}", e),
                                                };

                                                // stop timer
                                                
                                        }
                                        Err(err) => {
                                                println!("Error: {}", err);
                                        }
                                }

                                // delete measurement event
                                let client = reqwest::Client::new();
                                let bucket = "measurement_event";
                                let url = format!("http://127.0.0.1:8515/bucket/{bucket}/{}", key.to_string());
                                let res = client.delete(url).send().await;
                                match res {
                                        Ok(_) => {},
                                        Err(err) => panic!("Error deleting event: {}", err),
                                }

                                

                                // add batch to bucket
                                let client = reqwest::Client::new();
                                let bucket = "batch";
                                let source = batch.measurements.clone().expect("no measurements")[0].source.clone();
                                let uuid = batch.uuid.clone();
                                let start = batch.start_timestamp().await.unwrap();
                                let end = batch.end_timestamp().await.unwrap();
                                let locations = batch.relative_x_movements.clone().expect("no x movements");
                                let amplitudes = batch.relative_y_movements.clone().expect("no y movements");
                                let length = batch.length.clone().to_string();
                                let measurement_bucket = batch.measurement_bucket.clone();

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
                                        Ok(_) => {},
                                        Err(err) => panic!("Error creating bucket: {}", err),
                                }

                                // add object to bucket
                                let url = format!("http://127.0.0.1:8515/bucket/{}?key={}", bucket, uuid.to_string());
                                let res = client.post(url).json(&body).send().await;

                                match res {
                                        Ok(_) => {},
                                        Err(err) => panic!("Error adding object to bucket: {}", err),
                                }
                        }

                        let duration = start.elapsed();
                        println!("Batch processing: {}s", duration.as_secs_f64());

			sleep(Duration::from_secs(60)).await;
		}
	});

	tokio::join!(server.join().unwrap(), app.join().unwrap());

	std::future::pending::<()>().await;
}

pub async fn get_measurement_events() -> Vec<(Uuid, MeasurementEvent)> {
	let client = reqwest::Client::new();
	let url = "http://127.0.0.1:8515/bucket/measurement_event";

	let res = client.get(url).send().await;

	match res {
		Ok(res) => {
			let body: Value = res.json().await.unwrap();
			let events: Vec<Value> = serde_json::from_value(body["value"].clone()).unwrap();
                        let events: Vec<(Uuid, MeasurementEvent)> = events.iter().map(|event| {
                                let e = serde_json::from_value(event["value"].clone()).unwrap();
                                let u = serde_json::from_value(event["key"].clone()).unwrap();
                                (u, e)
                        }).collect();
                        
                        // filter events with event type measurement_add
                        let mut add_events: Vec<(Uuid,MeasurementEvent)> = Vec::new();
                        events.iter().for_each(|event| {
                                if event.1.event_type == "measurement_add" {
                                        add_events.push(event.clone());
                                }
                        });
			add_events
		}
		Err(err) => {
			println!("Error: {}", err);
			Vec::new()
		}
	}
}
