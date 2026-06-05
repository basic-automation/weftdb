use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::prelude::*;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Log(Vec<IndexMap<String, Value>>);

impl Log {
	pub fn new(name: &str) -> Self {
		let mut log = IndexMap::new();
		log.insert("name".to_string(), json!(name));
		Self(vec![log])
	}

	pub fn log(&mut self, state: &str, log: &Value) {
		let timestamp = chrono::Utc::now().timestamp_micros();
		if let Some(map) = self.0.last_mut() {
			map.insert(format!("{} :: {}", timestamp, state), log.clone());
		}
	}

	pub async fn save(&mut self, folder: &str) -> Result<Self, String> {
		if let Some(map) = self.0.last_mut() {
			map.insert("end".to_string(), json!(chrono::Utc::now().timestamp_micros()));
		} else {
			return Err("Error: No logs to save".to_string());
		}
		let name = self.0[0].get("name").unwrap().as_str().unwrap();
		let folder_path = format!("./logs/{}/", folder);
		let file_path = format!("./logs/{}/{}.json", folder, name);

		// create folder if it doesn't exist
		fs::create_dir_all(folder_path).unwrap();

		match Path::new(&file_path).exists() {
			true => {
				// read file
				let mut file = File::open(&file_path).unwrap();
				let mut contents = String::new();
				file.read_to_string(&mut contents).unwrap();
				let old_log: Vec<IndexMap<String, Value>> = match serde_json::from_str(&contents) {
					Ok(json) => json,
					Err(e) => return Err(format!("Error: {}", e)),
				};
				// append to file
				let mut log = old_log;
				log.append(&mut self.0.clone());
				let log: String = match serde_json::to_string_pretty(&log) {
					Ok(json) => json,
					Err(e) => return Err(format!("Error: {}", e)),
				};
				let mut file = OpenOptions::new().write(true).truncate(true).open(&file_path).unwrap();

				file.write_all(log.as_bytes()).unwrap();
				self.0 = vec![];
				Ok(self.clone())
			}
			false => {
				let log: String = match serde_json::to_string_pretty(&self.0) {
					Ok(json) => json,
					Err(e) => return Err(format!("Error: {}", e)),
				};

				let mut file = File::create(&file_path).unwrap();
				file.write_all(log.as_bytes()).unwrap();
				self.0 = vec![];
				Ok(self.clone())
			}
		}
	}
}
