#![feature(async_closure)]
use dsm_measurement::{MeasurementEvent, Measurement};
use reqwest::Client;
use router::router;
use serde_json::json;
use serde_json::Value;
use source::*;
use std::env;
use std::thread;
use tokio::time::{sleep, Duration};

mod router;
mod source;

#[tokio::main]
async fn main() {
	env::set_var("RUST_BACKTRACE", "1");

	let server = thread::spawn(async move || {
		println!("server running...");
		let app = router();

		let port = env::var("PORT").unwrap_or("8516".to_string());
		let address = format!("0.0.0.0:{}", port);
		println!("Listening on address {}", address);

		// run it with hyper on localhost:3000
		axum::Server::bind(&address.parse().unwrap()).serve(app.into_make_service()).await.unwrap();
	});

	let app = thread::spawn(async move || {
		println!("app running...");

		loop {
			let client = Client::new();
			let res = client.get("http://127.0.0.1:8515/bucket/input_sources").send().await.unwrap();
			let value: Value = res.json::<Value>().await.unwrap()["value"].clone().into();
			let mut sources: SourceCollection = SourceCollection::new();
			if value.as_array().is_none() {
				println!("No sources found...");
				sleep(Duration::from_secs(60)).await;
				continue;
			}
			for source in value.as_array().unwrap() {
				let source: Source = serde_json::from_value(source["value"].clone()).unwrap();
				sources.add(source)
			}

			for source in sources.0.iter() {
				let measurements = get_measurement(&source).await;

				// add measurement to bucket
				for measurement in measurements {
					create_bucket(measurement.clone()).await;
					add_to_bucket(measurement.clone()).await;
					println!("{}:{}::{} time: {}, ratio: {}", measurement.source, measurement.numerator_asset, measurement.denominator_asset, measurement.timestamp, measurement.ratio);
				}
			}
			sleep(Duration::from_secs(60)).await;
		}
	});

	tokio::join!(server.join().unwrap(), app.join().unwrap(),);

	std::future::pending::<()>().await;
}

pub async fn get_measurement(source: &Source) -> Vec<Measurement> {
	let client = Client::new();
	let res = client.get(&source.url).send().await.unwrap();
	let value: Value = res.json::<Value>().await.unwrap()["measurements"].clone().into();
	let mut measurements: Vec<Measurement> = Vec::new();
	for measurement in value.as_array().unwrap() {
		let measurement: Measurement = serde_json::from_value(measurement.clone()).unwrap();
		measurements.push(measurement);
	}
	measurements
}

pub async fn create_bucket(measurement: Measurement) {
	let client = Client::new();

	let measurement_name = format!("{}::{}", measurement.numerator_asset, measurement.denominator_asset);
	let measurement_type = "timeseries".to_string();
	let measurement_tags = format!("&tag[1]=source={}", measurement.source);

	//create measurement bucket
	client.post(format!("http://127.0.0.1:8515/bucket?name={measurement_name}&type={measurement_type}{measurement_tags}")).send().await.unwrap();

	//create event bucket
	let event_name = "measurement_event".to_string();
	let event_type = "object".to_string();
	client.post(format!("http://127.0.0.1:8515/bucket?name={event_name}&type={event_type}")).send().await.unwrap();
}

pub async fn add_to_bucket(measurement: Measurement) {
	let client = Client::new();

	let measurement_bucket_name = format!("{}::{}", measurement.numerator_asset, measurement.denominator_asset);
	let measurement_key = format!("{}", measurement.timestamp);
	let measurement_value = measurement.ratio.to_string();
	let measurement_tags = format!("&tag[1]=source={}&tag[2]=uuid={}", measurement.source, measurement.uuid);

	client.post(format!("http://127.0.0.1:8515/bucket/{}?key={}&value={}&{}", measurement_bucket_name, measurement_key, measurement_value, measurement_tags)).send().await.unwrap();

	let event_bucket_name = "measurement_event".to_string();
	let event_key = uuid::Uuid::new_v4().to_string();
	let event = MeasurementEvent::new("measurement_add", measurement.timestamp, &measurement_bucket_name);
	let event_body = json!(event);

	client.post(format!("http://127.0.0.1:8515/bucket/{}?key={}", event_bucket_name, event_key)).json(&event_body).send().await.unwrap();
}
