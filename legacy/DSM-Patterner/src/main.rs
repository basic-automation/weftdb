#![feature(async_closure)]
use bigdecimal::BigDecimal;
use dsm_config::Configuration;
use dsm_pattern::*;
use num_bigint::BigUint;
use rand::Rng;
use reqwest::Client;
use reqwest::StatusCode;
use router::*;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::{str::FromStr, thread};
use threadpool::ThreadPool;
use tokio::sync::Mutex as TokMutex;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

mod router;

#[tokio::main]
async fn main() {
	env::set_var("RUST_BACKTRACE", "full");

	let client_pool = get_client_pool(400).await;

	let server = thread::spawn(async move || {
		let config = match Configuration::open("../dsm.toml").await {
			Ok(config) => config,
			Err(e) => panic!("Error: Failed to open configuration file: {}", e),
		};
		println!("server running...");
		let app = router();

		let port = config.dsm.ports.patterner;
		let address = format!("0.0.0.0:{}", port);
		println!("Listening on address {}", address);

		// run it with hyper on localhost:3000
		axum::Server::bind(&address.parse().unwrap()).serve(app.into_make_service()).await.unwrap();
	});

	let app = thread::spawn(async move || {
		println!("App is running...");

		loop {
			// create new pattern dictionary
			println!("Getting patterns...");
			let client_pool = Arc::clone(&client_pool);
			let patterns = match get_patterns(Arc::clone(&client_pool)).await {
				Ok(patterns) => patterns,
				Err(e) => {
					println!("Error getting patterns: {}", e);
					pause().await;
					continue;
				}
			};
			let arc_patterns = Arc::new(TokMutex::new(patterns.clone()));
			let dicts: Vec<String> = patterns.keys().cloned().collect();
			let arc_dicts = Arc::new(TokMutex::new(dicts.clone()));
			let length = dicts.len();
			if length < 1 {
				println!("No patterns found...");
				pause().await;
				continue;
			}

			let pool = ThreadPool::with_name("process events thread".into(), length * 2);
			for i in 0..length {
				let arc_dicts = Arc::clone(&arc_dicts);
				let client_pool = Arc::clone(&client_pool);
				let arc_patterns = Arc::clone(&arc_patterns);
				pool.execute(move || {
					let rt = tokio::runtime::Runtime::new().unwrap();
					let dicts = Arc::clone(&arc_dicts);

					let local_dicts = rt.block_on(dicts.lock());
					let name = local_dicts[i].clone();
					drop(local_dicts);

                                        let local_patterns = rt.block_on(arc_patterns.lock());
					let patterns = local_patterns.get(&name).unwrap().clone();
                                        drop(local_patterns);

                                        let dataset_name = patterns[0].occurrences[&BigUint::from(0_u8)].dataset_name.clone();

					let client_pool = Arc::clone(&client_pool);
					let mut dictionary = rt.block_on(open_dictionary(Arc::clone(&client_pool), &name, &dataset_name)).unwrap();

					println!("{name}: {}: adding {} patterns", name, patterns.len());

					// add patterns to dictionary

					// start timer
					let start = std::time::Instant::now();

					rt.block_on(dictionary.add_patterns(patterns.clone()));

					//stop timer
					let duration = start.elapsed();
					println!("{name}: {}: Adding patterns took: {:?}", name, duration);

					let patts = match dictionary.patterns.0.clone() {
						Some(patterns) => Arc::clone(&patterns),
						None => {
							println!("{}: No patterns found...", name);
							return;
						}
					};

					let local_patterns = rt.block_on(patts.lock());
					println!("{}: contains {} patterns", name, local_patterns.len());
					drop(local_patterns);

					// enforce variability
					// start timer
					let start = std::time::Instant::now();
					let local_patterns = rt.block_on(patts.lock());
					println!("{name}: {}: Enforcing dictionary variability: {} patterns in dictionary...", name, local_patterns.len());
					drop(local_patterns);
					rt.block_on(dictionary.enforce_variability());

					// stop timer
					let duration = start.elapsed();
					println!("{name}: {}: Enforcing variability took: {:?}", name, duration);

					// save logs
					rt.block_on(dictionary.finish());

					let local_patterns = rt.block_on(patts.lock());
					println!("{name}: {}: contains {} patterns", name, local_patterns.len());
					drop(local_patterns);

					//let dictionary_json = serde_json::to_string(&dictionary).unwrap();
					rt.block_on(delete_batches(Arc::clone(&client_pool), patterns));

					rt.block_on(save_dictionary(Arc::clone(&client_pool), &dictionary)).unwrap();
				});
			}

			pool.join();

			println!("--------------------------------------------------------");

			pause().await;
		}
	});

	tokio::join!(server.join().unwrap(), app.join().unwrap());
	std::future::pending::<()>().await;
}

async fn pause() {
	sleep(Duration::from_secs(1)).await;
}

async fn get_patterns(client_pool: Arc<TokMutex<Vec<Arc<TokMutex<Client>>>>>) -> Result<HashMap<String, Vec<Pattern>>, String> {
	let config = match Configuration::open("../dsm.toml").await {
		Ok(config) => config,
		Err(e) => panic!("Error: Failed to open configuration file: {}", e),
	};
	let client = get_client(client_pool).await;
	let throttle: u64 = config.dsm.throttles.patterner.unwrap_or(1000);
	let database_port = config.dsm.database.port;
	let url = format!("http://127.0.0.1:{database_port}/bucket/batch?count={throttle}&page=1");

	let local_client = client.lock().await;
	let response = local_client.get(url).send().await;
	drop(local_client);

	if let Err(e) = response {
		return Err(format!("Error: No response from database: {}", e));
	}

	let response = response.unwrap();
	let body: Value = response.json().await.unwrap();
	let batches: Vec<Value> = match serde_json::from_value(body["value"].clone()) {
		Ok(batches) => batches,
		Err(e) => return Err(format!("Error: {}", e)),
	};

	let patterns: Vec<Pattern> = batches
		.iter()
		.map(|batch| {
			let k: String = serde_json::from_value(batch["key"].clone()).unwrap();
			let o = Occurrence {
				source: serde_json::from_value(batch["value"]["source"].clone()).unwrap(),
				dataset_name: serde_json::from_value(batch["value"]["dataset_name"].clone()).unwrap(),
                                length: serde_json::from_value(batch["value"]["length"].clone()).unwrap(),
				start: serde_json::from_value(batch["value"]["start"].clone()).unwrap(),
				end: serde_json::from_value(batch["value"]["end"].clone()).unwrap(),
				id: Uuid::from_str(&k).unwrap(),
			};
			let mut occurrences = HashMap::new();
			occurrences.insert(BigUint::from(0_u8), o);

			let locations: HashMap<String, String> = serde_json::from_value(batch["value"]["locations"].clone()).unwrap();
			let locations: HashMap<BigUint, BigDecimal> = locations.iter().map(|(k, v)| (BigUint::from_str(k).unwrap(), BigDecimal::from_str(v).unwrap())).collect();
			let amplitudes: HashMap<String, String> = serde_json::from_value(batch["value"]["amplitudes"].clone()).unwrap();
			let amplitudes: HashMap<BigUint, BigDecimal> = amplitudes.iter().map(|(k, v)| (BigUint::from_str(k).unwrap(), BigDecimal::from_str(v).unwrap())).collect();

			Pattern { id: Uuid::new_v4(), occurrences, locations, amplitudes, /* merged_patterns: None */ }
		})
		.collect();

	// sort patterns by source
	let mut sorted_patterns: HashMap<String, Vec<Pattern>> = HashMap::new();
	for pattern in patterns {
                let first_pattern = pattern.occurrences.get(&BigUint::from(0_u8)).unwrap();
		let dictionary_name = first_pattern.dataset_name.clone() + "::" + &first_pattern.length.to_string();
		if sorted_patterns.contains_key(&dictionary_name) {
			let mut patts = sorted_patterns.get(&dictionary_name).unwrap().clone();
			patts.push(pattern);
			sorted_patterns.insert(dictionary_name, patts);
		} else {
			sorted_patterns.insert(dictionary_name, vec![pattern]);
		}
	}

	Ok(sorted_patterns)
}

async fn open_dictionary(client_pool: Arc<TokMutex<Vec<Arc<TokMutex<Client>>>>>, name: &str, dataset_name: &str) -> Result<Dictionary, String> {
	let config = match Configuration::open("../dsm.toml").await {
		Ok(config) => config,
		Err(e) => panic!("Error: Failed to open configuration file: {}", e),
	};
	let client = get_client(client_pool).await;
	let url = format!("http://127.0.0.1:8515/bucket/dictionary/{}", name);
	//println!("Getting dictionary: {}", url.clone());

	let local_client = client.lock().await;
	let response = local_client.get(url).send().await;
	drop(local_client);
	let response = match response {
		Ok(response) => response,
		Err(e) => return Err(format!("Error: No response from database: {}", e)),
	};

	match response.status() {
		StatusCode::NOT_FOUND => {
			println!("Creating new dictionary...");
			let dataset = config.get_dataset(dataset_name).unwrap();
			Ok(Dictionary::new(name, dataset, false))
		}
		StatusCode::OK => {
			let body: Value = response.json().await.unwrap();
			let mut dictionary: Dictionary = match serde_json::from_value::<Dictionary>(body[name].clone()) {
				Ok(dictionary) => {
					let patterns = match dictionary.patterns.0.clone() {
						Some(patterns) => Arc::clone(&patterns),
						None => return Err(format!("Error: No patterns found in dictionary")),
					};
					let local_patterns = patterns.lock().await;
					println!("{}: Opened dictionary with {} patterns...", name, local_patterns.len());
					drop(local_patterns);
					dictionary
				}
				Err(e) => return Err(format!("Error: {}", e)),
			};
			let dataset = config.get_dataset(dataset_name).unwrap();
			dictionary.configuration.0 = Arc::new(TokMutex::new(dataset));
			Ok(dictionary)
		}
		_ => Err(format!("Error: {}", response.status())),
	}
}

async fn save_dictionary(client_pool: Arc<TokMutex<Vec<Arc<TokMutex<Client>>>>>, dictionary: &Dictionary) -> Result<(), String> {
	let patterns = match dictionary.patterns.0.clone() {
		Some(patterns) => Arc::clone(&patterns),
		None => return Err(format!("Error: No patterns found in dictionary")),
	};
	let local_patterns = patterns.lock().await;
	println!("Saving Dictionary with {} patterns...", local_patterns.len());
	drop(local_patterns);

	// create dictionary bucket
	let client = get_client(client_pool).await;
	let url = format!("http://127.0.0.1:8515/bucket?name={}&type=object", "dictionary");

	let local_client = client.lock().await;
	match local_client.post(&url).send().await {
		Ok(_) => (),
		Err(e) => return Err(format!("Error: {}", e)),
	};
	drop(local_client);

	// add dictionary to bucket
	let url = format!("http://127.0.0.1:8515/bucket/dictionary/{}", dictionary.name);
	let body = json!(dictionary);
	let local_client = client.lock().await;
	match local_client.put(&url).json(&body).send().await {
		Ok(response) => {
			if response.status() != StatusCode::OK {
				return Err(format!("Error: {}", response.status()));
			}
		}
		Err(e) => return Err(format!("Error: {}", e)),
	};
	drop(local_client);

	Ok(())
}

async fn delete_batches(client_pool: Arc<TokMutex<Vec<Arc<TokMutex<Client>>>>>, patterns: Vec<Pattern>) {
	let client = get_client(client_pool).await;
	for pattern in patterns {
		let occurrences: Vec<Occurrence> = pattern.occurrences.values().cloned().collect();
		for occurrence in occurrences {
			let url = format!("http://127.0.0.1:8515/bucket/batch/{}", occurrence.id);
			let local_client = client.lock().await;
			local_client.delete(&url).send().await.unwrap();
			drop(local_client);
		}
	}
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
