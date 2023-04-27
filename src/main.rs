#![feature(async_closure)]
use bigdecimal::ToPrimitive;
use dsm_config::{Configuration, SourceConfiguration};
use dsm_measurement::{Measurement, MeasurementEvent};
use rayon::prelude::IntoParallelRefIterator;
use rayon::prelude::ParallelIterator;
use reqwest::Client;
use router::router;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::thread;
use tokio::time::{sleep, Duration};

mod router;

#[tokio::main]
async fn main() {
	env::set_var("RUST_BACKTRACE", "1");

	let server = thread::spawn(async move || {
		println!("server running...");

		let app = router();
		let config = Configuration::open("../dsm.toml").await.unwrap();

		let port = config.dsm.ports.inputer;
		let address = format!("0.0.0.0:{}", port);
		println!("Listening on address {}", address);

		// run it with hyper on localhost:3000
		axum::Server::bind(&address.parse().unwrap()).serve(app.into_make_service()).await.unwrap();
	});

	let app = thread::spawn(async move || {
		println!("app running...");

		loop {
			let sources = match get_source().await {
				Ok(sources) => sources,
				Err(err) => {
					println!("Error: {}", err);
					pause(60000_u64).await;
					continue;
				}
			};

			let mut handles = Vec::new();

			sources.iter().for_each(|(name, source)| {
				let name = name.clone();
				let source = source.clone();
				let handle = thread::Builder::new()
					.name(name.clone())
					.spawn(move || loop {
						let rt = tokio::runtime::Runtime::new().unwrap();
						let measurements = rt.block_on(get_measurement(&source));

						rt.block_on(create_bucket(&name));

						measurements.par_iter().for_each(|measurement| {
							let rt2 = tokio::runtime::Runtime::new().unwrap();
							rt2.block_on(add_to_bucket(&name, measurement.clone()));
						});

						println!("{}: {} measurements added", name, measurements.len());

						rt.block_on(pause(source.interval.to_u64().unwrap()));
					})
					.unwrap();

				handles.push(handle);
			});

			for handle in handles {
				handle.join().unwrap();
			}
		}
	});

	tokio::join!(server.join().unwrap(), app.join().unwrap());

	std::future::pending::<()>().await;
}

pub async fn pause(length: u64) {
	sleep(Duration::from_millis(length)).await;
}

pub async fn get_source() -> Result<HashMap<String, SourceConfiguration>, String> {
	let config = Configuration::open("../dsm.toml").await.unwrap();
	let sources = config.sources;
	Ok(sources)
}

pub async fn get_measurement(source: &SourceConfiguration) -> Vec<Measurement> {
	let client = Client::new();
	let res = match client.get(&source.url).send().await {
		Ok(res) => res,
		Err(err) => {
			println!("Error retriving source url {}: {}", &source.url, err);
			return Vec::new();
		}
	};
	let value: Value = res.json::<Value>().await.unwrap()["measurements"].clone();
	let mut measurements: Vec<Measurement> = Vec::new();
	for measurement in value.as_array().unwrap() {
		let measurement: Measurement = serde_json::from_value(measurement.clone()).unwrap();
		measurements.push(measurement);
	}
	measurements
}

pub async fn create_bucket(source_name: &str) {
	let config = Configuration::open("../dsm.toml").await.unwrap();
	let client = Client::new();

	let host = config.dsm.database.host.clone();
	let port = config.dsm.database.port;

	let datasets = config.get_datasets_with_source(source_name);

	for (dataset_name, _) in datasets {
		let dataset_bucket_name = dataset_name.clone();
		let bucket_type = "timeseries".to_string();
		let tags = format!("tag[1]=source={}", source_name);

		let url = format!("http://{host}:{port}/bucket?name={dataset_bucket_name}&type={bucket_type}&{tags}");

		//create dataset bucket
		client.post(url).send().await.unwrap();
	}

	let event_name = "measurement_event".to_string();
	let event_type = "object".to_string();
	let url = format!("http://{host}:{port}/bucket?name={event_name}&type={event_type}");

	//create event bucket
	client.post(url).send().await.unwrap();
}

pub async fn add_to_bucket(source_name: &str, measurement: Measurement) {
	let config = Configuration::open("../dsm.toml").await.unwrap();
	let client = Client::new();

	let datasets = config.get_datasets_with_source(source_name);

	for (dataset_name, _) in datasets {
		let dataset_bucket_name = dataset_name.clone();
		let key = format!("{}", measurement.timestamp.clone());
		let value = measurement.ratio.to_string();
		let tags = format!("tag[1]=source={}&tag[2]=uuid={}", source_name, measurement.uuid);
		let host = config.dsm.database.host.clone();
		let port = config.dsm.database.port;

		let url = format!("http://{host}:{port}/bucket/{dataset_bucket_name}?key={key}&value={value}&{tags}");

		// add measurement to bucket
		client.post(url).send().await.unwrap();

		let event_bucket_name = "measurement_event".to_string();
		let key = uuid::Uuid::new_v4().to_string();
		let event = MeasurementEvent::new("measurement_add", measurement.timestamp.clone(), Some(dataset_bucket_name));
		let body = json!(event);

		let url = format!("http://{host}:{port}/bucket/{event_bucket_name}?key={key}");

		// add event to bucket
		if let Err(e) = client.post(url.clone()).json(&body.clone()).send().await {
                        println!("Error adding measurement event to bucket: {}", e);
                        println!("Error url: {}", url);
                        println!("Error body: {}", body);
                }
	}
}
