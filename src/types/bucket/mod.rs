pub use bucket_type::*;
pub use bucket_value::*;
pub use object_bucket::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sled::Db;
use std::{collections::HashMap, hash::Hash, str::FromStr};
pub use time_series_bucket::*;
use uuid::Uuid;

mod bucket_type;
mod bucket_value;
mod object_bucket;
mod time_series_bucket;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
	#[serde(rename = "type")]
	pub type_: BucketType,
	pub name: String,
	pub uuid: String,
	pub tags: Option<HashMap<String, String>>,
}

impl Bucket {
	pub async fn new(name: &str, bucket_type: &str, tags: Option<HashMap<String, String>>, db: Db) -> Result<Self, String> {
		let bucket_tree = db.open_tree("__buckets").unwrap();
		// check if a bucket with the same name exists
		for i in 0..bucket_tree.iter().count() {
			let item = match bucket_tree.iter().nth(i) {
				Some(item) => item,
				None => break,
			};
			let (_, value) = item.unwrap();
			let value: Bucket = serde_json::from_str(String::from_utf8(value.to_vec()).unwrap().as_str()).unwrap();
			if (value.type_ == BucketType::from_str(bucket_type).unwrap()) && (value.name == name.to_string()) {
				return Err("Bucket already exists".to_string());
			}
		}

		// generate new id
		let uuid = Uuid::new_v4().to_string();

		// define a bucket type
		let type_ = match BucketType::from_str(bucket_type) {
			Ok(value) => value,
			Err(e) => return Err(e),
		};

		// create a new tree
		db.open_tree(uuid.clone()).unwrap();

		let res = Self { type_, name: name.to_string(), uuid: uuid.clone(), tags };
		let res_json = json!(res).to_string();

		// add bucket to the bucket tree
		bucket_tree.insert(name.to_string(), res_json.as_bytes()).unwrap();

		Ok(res)
	}

	pub async fn open(name: &str, db: Db) -> Result<Self, String> {
		let bucket_tree = db.open_tree("__buckets").unwrap();
		// check if a bucket with the same name exists
		for i in 0..bucket_tree.iter().count() {
			let item = match bucket_tree.iter().nth(i) {
				Some(item) => item,
				None => break,
			};
			let (_, value) = item.unwrap();
			let value: Bucket = serde_json::from_str(String::from_utf8(value.to_vec()).unwrap().as_str()).unwrap();
			if value.name == name.to_string() {
				return Ok(value);
			}
		}

		Err("Bucket not found".to_string())
	}

	pub async fn get(&self, db: Db) -> Result<Vec<BucketValue>, String> {
		let tree = db.open_tree(self.uuid.clone()).unwrap();
		let mut res = Vec::new();
		for i in 0..tree.iter().count() {
			let item = match tree.iter().nth(i) {
				Some(item) => item,
				None => break,
			};
			let (_, value) = item.unwrap();
			let value: BucketValue = serde_json::from_str(String::from_utf8(value.to_vec()).unwrap().as_str()).unwrap();
			match value.clone() {
				BucketValue::Object(object_value) => {
					let mut value = object_value.clone();
					value.value = serde_json::from_str(value.value.as_str().unwrap()).unwrap();
					res.push(BucketValue::Object(value));
				}
				BucketValue::TimeSeries(_) => {
					res.push(value);
				}
			}
		}
		Ok(res)
	}

	pub async fn delete(&self, db: Db) -> Result<Self, String> {
		let bucket_tree = db.open_tree("__buckets").unwrap();
		bucket_tree.remove(self.name.clone()).unwrap();
		db.drop_tree(self.uuid.clone()).unwrap();
		Ok(self.clone())
	}

	pub async fn key_add(&self, value: BucketValue, db: Db) -> Result<BucketValue, String> {
		let tree = db.open_tree(self.uuid.clone()).unwrap();
		match value.clone() {
			BucketValue::Object(object_value) => {
				let key = object_value.key.clone();
				let object_value_json = json!(object_value).to_string();
				tree.insert(key, object_value_json.as_bytes()).unwrap();
				let mut value = object_value.clone();
				value.value = serde_json::from_str(value.value.as_str().unwrap()).unwrap();
				Ok(BucketValue::Object(value))
			}
			BucketValue::TimeSeries(time_series_measurement) => {
				let key = time_series_measurement.timestamp.to_string();
				let time_series_measurement = json!(time_series_measurement).to_string();
				tree.insert(key, time_series_measurement.as_bytes()).unwrap();
				Ok(value)
			}
		}
	}

	pub async fn key_get(&self, key: &str, db: Db) -> Result<BucketValue, String> {
		let tree = db.open_tree(self.uuid.clone()).unwrap();
		let value = match tree.get(key) {
			Ok(value) => value,
			Err(e) => return Err(e.to_string()),
		};
		let value = match value {
			Some(value) => value,
			None => return Err("Key not found".to_string()),
		};
		let value = String::from_utf8(value.to_vec()).unwrap();
		match self.type_.clone() {
			BucketType::Object => {
				let mut value: ObjectValue = serde_json::from_str(value.as_str()).unwrap();
				value.value = serde_json::from_str(value.value.as_str().unwrap()).unwrap();
				Ok(BucketValue::Object(value))
			}
			BucketType::TimeSeries => {
				let value: TimeSeriesMeasurement = serde_json::from_str(value.as_str()).unwrap();
				Ok(BucketValue::TimeSeries(value))
			}
		}
	}

	pub async fn key_remove(&self, key: &str, db: Db) -> Result<BucketValue, String> {
		let tree = db.open_tree(self.uuid.clone()).unwrap();
		match tree.remove(key) {
			Ok(value) => {
				let value = match value {
					Some(value) => value,
					None => return Err("Key not found".to_string()),
				};
				let value = String::from_utf8(value.to_vec()).unwrap();
				match self.type_.clone() {
					BucketType::Object => {
						let mut value: ObjectValue = serde_json::from_str(value.as_str()).unwrap();
						value.value = serde_json::from_str(value.value.as_str().unwrap()).unwrap();
						Ok(BucketValue::Object(value))
					}
					BucketType::TimeSeries => {
						let value: TimeSeriesMeasurement = serde_json::from_str(value.as_str()).unwrap();
						Ok(BucketValue::TimeSeries(value))
					}
				}
			}
			Err(e) => Err(e.to_string()),
		}
	}
}

impl PartialOrd for Bucket {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
		Some(self.name.cmp(&other.name))
	}
}

impl Ord for Bucket {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		self.name.cmp(&other.name)
	}
}

impl Hash for Bucket {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		self.name.hash(state);
	}
}

pub trait AddToBucket {
	fn add(&mut self, value: BucketValue) -> Result<BucketValue, String>;
}
