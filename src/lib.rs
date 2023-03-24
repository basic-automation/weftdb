use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::prelude::*;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Log(IndexMap<String, Value>);

impl Log {
	pub fn new(name: &str) -> Self {
		let mut log = IndexMap::new();
		log.insert("name".to_string(), json!(name));
		Self(log)
	}

	pub fn log(&mut self, state: &str, log: &Value) {
		let timestamp = chrono::Utc::now().timestamp_micros();
		self.0.insert(format!("{} :: {}", timestamp, state), log.clone());
	}

	pub async fn save(&self, folder: &str) -> Result<(), String> {
		let name = self.0.get("name").unwrap().as_str().unwrap();
		let folder_path = format!("./logs/{}/", folder);
		let file_path = format!("./logs/{}/{}.json", folder, name);

		// create folder if it doesn't exist
		fs::create_dir_all(folder_path).unwrap();

		let log: String = match serde_json::to_string_pretty(&self.0) {
			Ok(json) => json,
			Err(e) => return Err(format!("Error: {}", e)),
		};

		match Path::new(&file_path).exists() {
			true => {
				let mut file = OpenOptions::new().write(true).append(true).open(&file_path).unwrap();
				file.write_all(log.as_bytes()).unwrap();
				Ok(())
			}
			false => {
				let mut file = File::create(&file_path).unwrap();
				file.write_all(log.as_bytes()).unwrap();
				Ok(())
			}
		}
	}
}
