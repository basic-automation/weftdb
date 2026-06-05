#![feature(async_closure)]
use bigdecimal::ToPrimitive;
use dsm_config::{Configuration, SourceConfiguration};
use dsm_measurement::{Measurement, MeasurementEvent};
use rand::Rng;
use rayon::prelude::IntoParallelRefIterator;
use rayon::prelude::ParallelIterator;
use reqwest::Client;
use router::router;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::fmt::Display;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::{thread, time};
use tokio::sync::Mutex as TokMutex;

mod router;

#[tokio::main]
async fn main() {
	env::set_var("RUST_BACKTRACE", "full");
	let client_pool = get_client_pool(100).await;

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
			let unproccessed_measurements = Arc::new(Mutex::new(Vec::new()));
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
				let unproccessed_measurements = Arc::clone(&unproccessed_measurements);
				let name = name.clone();
				let source = source.clone();
				let client = Arc::clone(&client_pool);
				let handle: JoinHandle<_> = thread::Builder::new()
					.name(name.clone())
					.spawn(move || loop {
						let rt = tokio::runtime::Runtime::new().unwrap();
						let mut measurements = match rt.block_on(get_measurement(Arc::clone(&client), &source)) {
							Ok(measurements) => measurements,
							Err(err) => {
								println!("Error: {}", err);
								rt.block_on(pause(source.interval.to_u64().unwrap_or(60000_u64)));
								continue;
							}
						};

						let mut u_m = unproccessed_measurements.lock().unwrap();
						measurements.append(&mut u_m.clone());
						*u_m = Vec::new();

						match rt.block_on(create_bucket(Arc::clone(&client), &name)) {
							Ok(_) => (),
							Err(err) => {
								u_m.extend(measurements);

								println!("Error: {}", err);
								rt.block_on(pause(source.interval.to_u64().unwrap_or(60000_u64)));
								continue;
							}
						}
						drop(u_m);

						measurements.par_iter().for_each(|measurement| {
							let u_m = Arc::clone(&unproccessed_measurements);
							let rt2 = tokio::runtime::Runtime::new().unwrap();
							match rt2.block_on(add_to_bucket(Arc::clone(&client), &name, measurement.clone())) {
								Ok(_) => {}
								Err(err) => {
									let mut u_m = u_m.lock().unwrap();
									u_m.push(measurement.clone());
									drop(u_m);
									println!("Error: {}", err);
								}
							};
						});

						println!("{}: {} measurements added", name, measurements.len());

						rt.block_on(pause(source.interval.to_u64().unwrap_or(60000_u64)));
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

pub async fn get_client_pool(size: usize) -> Arc<TokMutex<Vec<Arc<TokMutex<Client>>>>> {
	let mut pool = Vec::new();
	for _ in 0..size {
		let client = Arc::new(TokMutex::new(Client::new()));
		pool.push(client);
	}
	Arc::new(TokMutex::new(pool))
}

pub async fn get_client(client_pool: Arc<TokMutex<Vec<Arc<TokMutex<Client>>>>>) -> Arc<TokMutex<Client>> {
	// select random client from pool
	let pool = client_pool.lock().await;
	let mut rng = rand::thread_rng();
	let index = rng.gen_range(0..pool.len());
	let client = pool.get(index).unwrap().clone();
	drop(pool);
	client
}

pub async fn pause(length: u64) {
	tokio::time::sleep(time::Duration::from_millis(length)).await;
}

pub async fn get_source() -> Result<HashMap<String, SourceConfiguration>, Error> {
	let config = match Configuration::open("../dsm.toml").await {
		Ok(config) => config,
		Err(err) => return Err(Error::ConfigurationError(err)),
	};
	let sources = config.sources;
	Ok(sources)
}

pub async fn get_measurement(client_pool: Arc<TokMutex<Vec<Arc<TokMutex<Client>>>>>, source: &SourceConfiguration) -> Result<Vec<Measurement>, Error> {
	let client = get_client(client_pool).await;
	let client = client.lock().await;
	let res = match client.get(&source.url).send().await {
		Ok(res) => res,
		Err(err) => {
			if err.is_request() {
				return Err(Error::DatabaseConnectionError(err.to_string()));
			} else {
				return Err(Error::UnknownError(err.to_string()));
			}
		}
	};

	let value: Value = match res.json::<Value>().await {
		Ok(value) => value["measurements"].clone(),
		Err(err) => return Err(Error::UnknownError(err.to_string())),
	};

	let value = match value.as_array() {
		Some(value) => value,
		None => return Err(Error::UnknownError("Error parsing json".to_string())),
	};

	let mut measurements: Vec<Measurement> = Vec::new();

	for measurement in value {
		let measurement: Measurement = match serde_json::from_value(measurement.clone()) {
			Ok(measurement) => measurement,
			Err(err) => return Err(Error::UnknownError(err.to_string())),
		};
		measurements.push(measurement);
	}

	Ok(measurements)
}

pub async fn create_bucket(client_pool: Arc<TokMutex<Vec<Arc<TokMutex<Client>>>>>, source_name: &str) -> Result<(), Error> {
	let client = get_client(client_pool).await;
	let config = match Configuration::open("../dsm.toml").await {
		Ok(config) => config,
		Err(err) => return Err(Error::ConfigurationError(err)),
	};
	let host = config.dsm.database.host.clone();
	let port = config.dsm.database.port;

	let datasets = config.get_datasets_with_source(source_name);

	for (dataset_name, _) in datasets {
		let dataset_bucket_name = dataset_name.clone();
		let bucket_type = "timeseries".to_string();
		let tags = format!("tag[1]=source={}", source_name);

		let url = format!("http://{host}:{port}/bucket?name={dataset_bucket_name}&type={bucket_type}&{tags}");

		let local_client = client.lock().await;
		//create dataset bucket
		match local_client.post(url).send().await {
			Ok(_) => (),
			Err(err) => {
				if err.is_request() {
					return Err(Error::DatabaseConnectionError(err.to_string()));
				} else {
					continue;
				}
			}
		}
		drop(local_client);
	}

	let event_name = "measurement_event".to_string();
	let event_type = "object".to_string();
	let url = format!("http://{host}:{port}/bucket?name={event_name}&type={event_type}");

	//create event bucket
	let local_client = client.lock().await;
	match local_client.post(url).send().await {
		Ok(_) => {}
		Err(err) => {
			if err.is_request() {
				return Err(Error::DatabaseConnectionError(err.to_string()));
			} else {
				return Err(Error::UnknownError(err.to_string()));
			}
		}
	};
	drop(local_client);
	Ok(())
}

pub async fn add_to_bucket(client_pool: Arc<TokMutex<Vec<Arc<TokMutex<Client>>>>>, source_name: &str, measurement: Measurement) -> Result<(), Error> {
	let client = get_client(client_pool).await;
	let config = match Configuration::open("../dsm.toml").await {
		Ok(config) => config,
		Err(err) => return Err(Error::ConfigurationError(err)),
	};
	let datasets = config.get_datasets_with_source(source_name);
	let host = config.dsm.database.host.clone();
	let port = config.dsm.database.port;

	for (dataset_name, _) in datasets {
		let dataset_bucket_name = dataset_name.clone();
		let key = format!("{}", measurement.timestamp.clone());
		let value = measurement.ratio.to_string();
		let tags = format!("tag[1]=source={}&tag[2]=uuid={}", source_name, measurement.uuid);

		let url = format!("http://{host}:{port}/bucket/{dataset_bucket_name}?key={key}&value={value}&{tags}");

		// add measurement to bucket
		let local_client = client.lock().await;
		match local_client.post(url).send().await {
			Ok(_) => {}
			Err(err) => {
				if err.is_request() {
					return Err(Error::DatabaseConnectionError(err.to_string()));
				} else {
					return Err(Error::UnknownError(err.to_string()));
				}
			}
		}
		drop(local_client);
	}

	let event_bucket_name = "measurement_event".to_string();
	let key = uuid::Uuid::new_v4().to_string();
	let event = MeasurementEvent::new("measurement_add", measurement.timestamp.clone(), Some(source_name.to_owned()));
	let body = json!(event);

	let url = format!("http://{host}:{port}/bucket/{event_bucket_name}?key={key}");

	// add event to bucket
	let local_client = client.lock().await;
	match local_client.post(url.clone()).json(&body.clone()).send().await {
		Ok(_) => {}
		Err(err) => {
			if err.is_request() {
				return Err(Error::DatabaseConnectionError(err.to_string()));
			} else {
				return Err(Error::UnknownError(err.to_string()));
			}
		}
	}
	drop(local_client);

	Ok(())
}

pub enum Error {
	ConfigurationError(String),
	DatabaseConnectionError(String),
	UnknownError(String),
}

impl Display for Error {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Error::ConfigurationError(err) => write!(f, "ConfigurationError: {}", err),
			Error::DatabaseConnectionError(err) => write!(f, "DatabaseConnectionError: {}", err),
			Error::UnknownError(err) => write!(f, "UnknownError: {}", err),
		}
	}
}
