use axum::http::StatusCode;
pub use bucket_type::*;
pub use bucket_value::*;
pub use get::*;
pub use object_bucket::*;
pub use params::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sled::Db;
use std::{collections::HashMap, hash::Hash, str::FromStr};
pub use time_series_bucket::*;
use uuid::Uuid;

mod bucket_type;
mod bucket_value;
mod get;
mod object_bucket;
mod params;
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
	pub async fn new(name: &str, bucket_type: &str, tags: Option<HashMap<String, String>>, db: Db) -> Result<Self, (StatusCode, String)> {
		let bucket_tree = db.open_tree("__buckets").unwrap();
		// check if a bucket with the same name exists
		for i in 0..bucket_tree.iter().count() {
			let item = match bucket_tree.iter().nth(i) {
				Some(item) => item,
				None => break,
			};
			let (_, value) = item.unwrap();
			let value: Bucket = serde_json::from_str(String::from_utf8(value.to_vec()).unwrap().as_str()).unwrap();
			if (value.type_ == BucketType::from_str(bucket_type).unwrap()) && (value.name == *name) {
				return Err((StatusCode::CONFLICT, "Bucket already exists".to_string()));
			}
		}

		// generate new id
		let uuid = Uuid::new_v4().to_string();

		// define a bucket type
		let type_ = match BucketType::from_str(bucket_type) {
			Ok(value) => value,
			Err(_) => return Err((StatusCode::BAD_REQUEST, "Invalid bucket type".to_string())),
		};

		// create a new tree
		db.open_tree(uuid.clone()).unwrap();

		let res = Self { type_, name: name.to_string(), uuid, tags };
		let res_json = json!(res).to_string();

		// add bucket to the bucket tree
		bucket_tree.insert(name, res_json.as_bytes()).unwrap();

		Ok(res)
	}

	pub async fn open(name: &str, db: Db) -> Result<Self, (StatusCode, String)> {
		let bucket_tree = db.open_tree("__buckets").unwrap();
		// check if a bucket with the same name exists
		for i in 0..bucket_tree.iter().count() {
			let item = match bucket_tree.iter().nth(i) {
				Some(item) => item,
				None => break,
			};
			let (_, value) = item.unwrap();
			let value: Bucket = serde_json::from_str(String::from_utf8(value.to_vec()).unwrap().as_str()).unwrap();
			if value.name == *name {
				return Ok(value);
			}
		}

		Err((StatusCode::NOT_FOUND, "Bucket not found".to_string()))
	}

	pub async fn get(&self, params: Option<BucketParams>, db: Db) -> Result<Vec<BucketValue>, (StatusCode, String)> {
		let tree =  match db.open_tree(self.uuid.clone()) {
                        Ok(tree) => tree,
                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
                };
		let params = match params {
			Some(params) => params,
			None => BucketParams::default(),
		};

		let values = match self.type_ {
			BucketType::TimeSeries => match get_timeseries_bucket(tree, params.clone()).await {
                                Ok(values) => values,
                                Err(e) => return Err(e),
                        },
			BucketType::Object => match get_object_bucket(tree).await {
                                Ok(values) => values,
                                Err(e) => return Err(e),
                        },
		};

		// paginate response
		match params.page {
			Some(page) => {
				let page = page - 1;
				let start = page * params.count.unwrap_or(100);
				let end = start + params.count.unwrap_or(100);
				Ok(values[start..end].to_vec())
			}
			None => match params.count {
				Some(count) => {
					let start = 0;
					let end = start + count;
					Ok(values[start..end].to_vec())
				}
				None => Ok(values),
			},
		}
	}

	pub async fn delete(&self, db: Db) -> Result<Self, (StatusCode, String)> {
		let bucket_tree = match db.open_tree("__buckets") {
			Ok(tree) => tree,
			Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
		};

		match bucket_tree.remove(self.name.clone()) {
			Ok(_) => (),
			Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
		};

		match db.drop_tree(self.uuid.clone()) {
			Ok(_) => (),
			Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
		};

		Ok(self.clone())
	}

	pub async fn key_add(&self, value: BucketValue, db: Db) -> Result<BucketValue, (StatusCode, String)> {
		let tree = match db.open_tree(self.uuid.clone()) {
                        Ok(tree) => tree,
                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
                };
		match value.clone() {
			BucketValue::Object(object_value) => {
				let key = object_value.key.clone();

				match tree.contains_key(object_value.key.clone()) {
					Ok(value) => {
						if value {
							return Err((StatusCode::BAD_REQUEST, "Key already exists".to_string()));
						}
					}
					Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
				}
				let object_value_json = json!(object_value).to_string();
				match tree.insert(key, object_value_json.as_bytes()) {
                                        Ok(_) => (),
                                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
                                };
				let mut value = object_value;
                                let val = match value.value.as_str() {
                                        Some(value) => value,
                                        None => return Err((StatusCode::INTERNAL_SERVER_ERROR, "Invalid value".to_string())),
                                };
				value.value = match serde_json::from_str(val) {
                                        Ok(value) => value,
                                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
                                };
				Ok(BucketValue::Object(value))
			}
			BucketValue::TimeSeries(time_series_measurement) => {
				let key = time_series_measurement.timestamp.to_string();

				// check if a key with the same name exists
				match tree.contains_key(key.clone()) {
					Ok(value) => {
						if value {
							return Err((StatusCode::BAD_REQUEST, "Key already exists".to_string()));
						}
					}
					Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
				}
				let time_series_measurement = json!(time_series_measurement).to_string();
				match tree.insert(key, time_series_measurement.as_bytes()) {
                                        Ok(_) => (),
                                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
                                };
				Ok(value)
			}
		}
	}

	pub async fn key_get(&self, key: &str, db: Db) -> Result<BucketValue, (StatusCode, String)> {
		let tree = match db.open_tree(self.uuid.clone()) {
                        Ok(tree) => tree,
                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to get tree: {}", e))),
                };
		let value = match tree.get(key) {
			Ok(value) => value,
			Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Faild to get key: {}", e))),
		};
		let value = match value {
			Some(value) => value,
			None => return Err((StatusCode::INTERNAL_SERVER_ERROR, "Key not found".to_string())),
		};
		let value = match String::from_utf8(value.to_vec()) {
                        Ok(value) => value,
                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse value: {}", e))),
                };
		match self.type_.clone() {
			BucketType::Object => {
				let mut value: ObjectValue = match serde_json::from_str(value.as_str()) {
                                        Ok(value) => value,
                                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse object value: {}", e))),
                                };
                                let val = match value.value.as_str() {
                                        Some(value) => value,
                                        None => return Err((StatusCode::INTERNAL_SERVER_ERROR, "Invalid value".to_string())),
                                };
				value.value = match serde_json::from_str(val) {
                                        Ok(value) => value,
                                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse object value: {}", e))),
                                };
				Ok(BucketValue::Object(value))
			}
			BucketType::TimeSeries => {
				let value: TimeSeriesMeasurement = match serde_json::from_str(value.as_str()) {
                                        Ok(value) => value,
                                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse time series value: {}", e))),
                                };
				Ok(BucketValue::TimeSeries(value))
			}
		}
	}

	pub async fn key_remove(&self, key: &str, db: Db) -> Result<BucketValue, (StatusCode, String)> {
		let tree = match db.open_tree(self.uuid.clone()) {
                        Ok(tree) => tree,
                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Faild to get tree: {}", e))),
                };
		match tree.remove(key) {
			Ok(value) => {
				let value = match value {
					Some(value) => value,
					None => return Err((StatusCode::INTERNAL_SERVER_ERROR, "Key not found".to_string())),
				};
				let value = match String::from_utf8(value.to_vec()) {
                                        Ok(value) => value,
                                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse value: {}", e))),
                                };
				match self.type_.clone() {
					BucketType::Object => {
						let mut value: ObjectValue = match serde_json::from_str(value.as_str()) {
                                                        Ok(value) => value,
                                                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse object value: {}", e))),
                                                };
                                                let val = match value.value.as_str() {
                                                        Some(value) => value,
                                                        None => return Err((StatusCode::INTERNAL_SERVER_ERROR, "Invalid value".to_string())),
                                                };
						value.value = match serde_json::from_str(val) {
                                                        Ok(value) => value,
                                                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse object value: {}", e))),
                                                };
						Ok(BucketValue::Object(value))
					}
					BucketType::TimeSeries => {
						let value: TimeSeriesMeasurement = match serde_json::from_str(value.as_str()) {
                                                        Ok(value) => value,
                                                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse time series value: {}", e))),
                                                };
						Ok(BucketValue::TimeSeries(value))
					}
				}
			}
			Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Faild to remove key: {}", e))),
		}
	}

	pub async fn key_update(&self, value: BucketValue, db: Db) -> Result<BucketValue, (StatusCode, String)> {
		let tree = match db.open_tree(self.uuid.clone()) {
                        Ok(tree) => tree,
                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Faild to get tree: {}", e))),
                };
		match value.clone() {
			BucketValue::Object(object_value) => {
				let key = object_value.key.clone();
				let object_value_json = json!(object_value).to_string();
				match tree.insert(key, object_value_json.as_bytes()) {
                                        Ok(_) => (),
                                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Faild to insert key: {}", e.to_string()))),
                                };
				let mut value = object_value;
                                let val = match value.value.as_str() {
                                        Some(value) => value,
                                        None => return Err((StatusCode::INTERNAL_SERVER_ERROR, "Invalid value".to_string())),
                                };
				value.value = match serde_json::from_str(val) {
                                        Ok(value) => value,
                                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse object value: {}", e.to_string()))),
                                };
				Ok(BucketValue::Object(value))
			}
			BucketValue::TimeSeries(time_series_measurement) => {
				let key = time_series_measurement.timestamp.to_string();
				let time_series_measurement = json!(time_series_measurement).to_string();
				match tree.insert(key, time_series_measurement.as_bytes()) {
                                        Ok(_) => (),
                                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Faild to insert key: {}", e.to_string()))),
                                };
				Ok(value)
			}
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
