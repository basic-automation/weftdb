#![feature(async_closure)]
use bigdecimal::BigDecimal;
use dsm_pattern::*;
use num_bigint::BigUint;
use router::*;
use serde_json::Value;
use std::collections::HashMap;
use std::{env, str::FromStr, thread};
use tokio::time::{sleep, Duration};
use uuid::Uuid;
use serde_json::json;
//use futures::future::join_all;
use reqwest::StatusCode;

mod router;

#[tokio::main]
async fn main() {
	let server = thread::spawn(async move || {
		println!("server running...");
		let app = router();

		let port = env::var("PORT").unwrap_or("8518".to_string());
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
                        let patterns = get_patterns().await;
                        println!("Number of patterns: {}", patterns.len());

                        let dicts: Vec<String> = patterns.keys().cloned().collect();
                        let length = dicts.len();
                        println!("Number of dictionaries: {}", length);
                        
                        //let mut threads = Vec::new();
                        let mut i = 0_usize;
                        loop {
                                if i == length {
                                        break;
                                }
                                let name = dicts[i].clone();
                                let mut dictionary = get_dictionary(&name).await.unwrap();

                                //let thread = std::thread::spawn(async move || {
                                        println!("Processing Dictionary: {}", dictionary.name.clone());

                                        // get batches
                                        let patterns = get_patterns().await;
                                        let patterns = patterns.get(&name).unwrap().clone();

                                        // add patterns to dictionary

                                        // start timer
                                        let start = std::time::Instant::now();
                                        println!("Adding {} patterns to dictionary...", patterns.len());
                                        dictionary.add_patterns(patterns.clone()).await;

                                        //stop timer
                                        let duration = start.elapsed();
                                        println!("Adding patterns took: {:?}", duration);

                                        println!("Dictionary size: {}", dictionary.patterns.clone().unwrap().len());

                                        // enforce variability
                                        // start timer
                                        let start = std::time::Instant::now();
                                        let patts = dictionary.patterns.clone().unwrap();
                                        println!("Enforcing dictionary variability: {} patterns in dictionary...", patts.len());
                                        dictionary.enforce_variability().await;

                                        // stop timer
                                        let duration = start.elapsed();
                                        println!("Enforcing variability took: {:?}", duration);

                                        println!("Dictionary size: {}", dictionary.patterns.clone().unwrap().len());

                                        //let dictionary_json = serde_json::to_string(&dictionary).unwrap();

                                        delete_batches(patterns).await;

                                        save_dictionary(&dictionary).await.unwrap();
                                //});

                                //threads.push(thread);
                                i += 1;
                        }

                        /* let mut joined = Vec::new();
                        for thread in threads {
                                joined.push(thread.join().unwrap());
                        }

                        join_all(joined).await; */
			sleep(Duration::from_secs(1)).await;
		}
	});

	tokio::join!(server.join().unwrap(), app.join().unwrap());
	std::future::pending::<()>().await;
}

async fn get_patterns() -> HashMap<String, Vec<Pattern>> {
	let client = reqwest::Client::new();
	let url = "http://127.0.0.1:8515/bucket/batch?count=1000&page=1";

	let response = client.get(url).send().await;
	match response {
		Ok(response) => {
			let body: Value = response.json().await.unwrap();
			let batches: Vec<Value> = match serde_json::from_value(body["value"].clone()) {
				Ok(batches) => batches,
				Err(_) => {
					println!("No batches found...");
					return HashMap::new();
				}
			};
			let patterns: Vec<Pattern> = batches
				.iter()
				.map(|batch| {
					let k: String = serde_json::from_value(batch["key"].clone()).unwrap();
					let o = Occurrence {
						measurement_bucket: serde_json::from_value(batch["value"]["measurement_bucket"].clone()).unwrap(),
						source: serde_json::from_value(batch["value"]["source"].clone()).unwrap(),
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

					Pattern { id: Uuid::new_v4(), occurrences, locations, amplitudes, merged_patterns: None }
				})
				.collect();

                        // sort patterns by source
                        let mut sorted_patterns: HashMap<String, Vec<Pattern>> = HashMap::new();
                        for pattern in patterns {
                                let source = pattern.occurrences.get(&BigUint::from(0_u8)).unwrap().measurement_bucket.clone();
                                if sorted_patterns.contains_key(&source) {
                                        let mut patts = sorted_patterns.get(&source).unwrap().clone();
                                        patts.push(pattern);
                                        sorted_patterns.insert(source, patts);
                                } else {
                                        sorted_patterns.insert(source, vec![pattern]);
                                }
                        }

                        sorted_patterns
		}
		Err(e) => {
			println!("Error: {}", e);
			HashMap::new()
		}
	}
}

async fn get_dictionary(name: &str) -> Result<Dictionary, String> {
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:8515/bucket/dictionary/{}", name);
        println!("Getting dictionary: {}", url.clone());
        let response = client.get(url).send().await;
        let response = match response {
                Ok(response) => response,
                Err(e) => return Err(format!("Error: No response from database: {}", e)),
        };


        match response.status() {
                StatusCode::NOT_FOUND => {
                        println!("Creating new dictionary...");
                        Ok(Dictionary::new(name, Constraints::default()))
                },
                StatusCode::OK => {
                        let body: Value = response.json().await.unwrap();
                        let dictionary: Dictionary = match serde_json::from_value::<Dictionary>(body[name].clone()) {
                                Ok(dictionary) => {
                                        println!("Opened dictionary with {} patterns...", dictionary.patterns.clone().expect("").len());
                                        dictionary
                                },
                                Err(e) => return Err(format!("Error: {}", e)),
                        };
                        Ok(dictionary)
                },
                _ => Err(format!("Error: {}", response.status())),
        }
}

async fn save_dictionary(dictionary: &Dictionary) -> Result<(), String> {
        println!("Saving Dictionary with {} patterns...", dictionary.patterns.clone().expect("").len());

        // create dictionary bucket
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:8515/bucket?name={}&type=object", "dictionary");
        match client.post(&url).send().await {
                Ok(_) => (),
                Err(e) => return Err(format!("Error: {}", e)),
        };

        // add dictionary to bucket
        let url = format!("http://127.0.0.1:8515/bucket/dictionary/{}", dictionary.name);
        let body = json!(dictionary);
        match client.put(&url).json(&body).send().await {
                Ok(response) => {
                        if response.status() != StatusCode::OK {
                                return Err(format!("Error: {}", response.status()));
                        }
                },
                Err(e) => return Err(format!("Error: {}", e)),
        };

        Ok(())
 }


async fn delete_batches(patterns: Vec<Pattern>) {
        let client = reqwest::Client::new();
        for pattern in patterns {
                let occurrences: Vec<Occurrence> = pattern.occurrences.values().cloned().collect();
                for occurrence in occurrences {
                        let url = format!("http://127.0.0.1:8515/bucket/batch/{}", occurrence.id);
                        client.delete(&url).send().await.unwrap();
                }
        }
}
