use crate::router::api_v1::bucket::keys::get::GetFromBucket;
use axum::http::StatusCode;
use bigdecimal::BigDecimal;
use bigdecimal::FromPrimitive;
use bigdecimal::ToPrimitive;
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
	pub name:  String,
	pub uuid:  String,
	pub tags:  Option<HashMap<String, String>>,
}

impl Bucket {
	pub async fn new(name: &str, bucket_type: &str, tags: Option<HashMap<String, String>>, db: Db) -> Result<Self, (StatusCode, String)> {
		let bucket_tree = match db.open_tree("__buckets") {
			Ok(tree) => tree,
			Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
		};
		// check if a bucket with the same name exists
		for i in 0..bucket_tree.iter().count() {
			let item = match bucket_tree.iter().nth(i) {
				Some(item) => item,
				None => break,
			};
			let value = match item {
				Ok(item) => item.1,
				Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
			};
			let value = match String::from_utf8(value.to_vec()) {
				Ok(value) => value,
				Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
			};
			let value: Bucket = match serde_json::from_str(value.as_str()) {
				Ok(value) => value,
				Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
			};
			let bucket_type = match BucketType::from_str(bucket_type) {
				Ok(value) => value,
				Err(_) => return Err((StatusCode::BAD_REQUEST, "Invalid bucket type".to_string())),
			};
			if (value.type_ == bucket_type) && (value.name == *name) {
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
		match db.open_tree(uuid.clone()) {
			Ok(tree) => tree,
			Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
		};

		let res = Self { type_, name: name.to_string(), uuid, tags };
		let res_json = json!(res).to_string();

		// add bucket to the bucket tree
		match bucket_tree.insert(name, res_json.as_bytes()) {
			Ok(_) => (),
			Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
		};

		Ok(res)
	}

	pub async fn open(name: &str, db: Db) -> Result<Self, (StatusCode, String)> {
		let bucket_tree = match db.open_tree("__buckets") {
			Ok(tree) => tree,
			Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
		};
		// check if a bucket with the same name exists
		for i in 0..bucket_tree.iter().count() {
			let item = match bucket_tree.iter().nth(i) {
				Some(item) => item,
				None => break,
			};
			let value = match item {
				Ok(item) => item.1,
				Err(_) => break,
			};
			let value = match String::from_utf8(value.to_vec()) {
				Ok(value) => value,
				Err(_) => break,
			};
			let value: Bucket = match serde_json::from_str(value.as_str()) {
				Ok(value) => value,
				Err(_) => break,
			};
			if value.name == *name {
				return Ok(value);
			}
		}

		Err((StatusCode::NOT_FOUND, "Bucket not found".to_string()))
	}

	pub async fn get(&self, params: Option<BucketParams>, db: Db) -> Result<Vec<BucketValue>, (StatusCode, String)> {
		let tree = match db.open_tree(self.uuid.clone()) {
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
				if values.len() < start {
					return Err((StatusCode::BAD_REQUEST, "Page out of range".to_string()));
				}
				if values.len() < end {
					return Ok(values[start..].to_vec());
				}
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
				let key = &time_series_measurement.timestamp;

				// check if a key with the same name exists
				match tree.contains_key(key.to_f64().unwrap().to_be_bytes()) {
					Ok(value) => {
						if value {
							return Err((StatusCode::BAD_REQUEST, "Key already exists".to_string()));
						}
					}
					Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
				}
				let time_series_measurement = json!(time_series_measurement).to_string();
				match tree.insert(key.to_f64().unwrap().to_be_bytes(), time_series_measurement.as_bytes()) {
					Ok(_) => (),
					Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
				};
				Ok(value)
			}
		}
	}

	pub async fn key_get(&self, key: &str, params: GetFromBucket, db: Db) -> Result<BucketValue, (StatusCode, String)> {
		let tree = match db.open_tree(self.uuid.clone()) {
			Ok(tree) => tree,
			Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to get tree: {}", e))),
		};
		match self.type_.clone() {
			BucketType::TimeSeries => {
				let k = match key.parse::<f64>() {
					Ok(key) => key.to_be_bytes(),
					Err(e) => return Err((StatusCode::BAD_REQUEST, format!("Failed to parse key: {}", e))),
				};

				let value: Option<TimeSeriesMeasurement> = match tree.get(k) {
					Ok(value) => match value {
						Some(value) => match String::from_utf8(value.to_vec()) {
							Ok(value) => match serde_json::from_str(value.as_str()) {
								Ok(value) => value,
								Err(_) => None,
							},
							Err(_) => None,
						},
						None => None,
					},
					Err(_) => None,
				};

				if value.is_some() {
					return Ok(BucketValue::TimeSeries(value.unwrap()));
				}

				let interpolate = params.interpolate.unwrap_or(false);
				if interpolate {
					let mut next_value: Option<TimeSeriesMeasurement> = match tree.get_gt(k) {
						Ok(value) => match value {
							Some(value) => match String::from_utf8(value.1.to_vec()) {
								Ok(value) => match serde_json::from_str(value.as_str()) {
									Ok(value) => value,
									Err(_) => None,
								},
								Err(_) => None,
							},
							None => None,
						},
						Err(_) => None,
					};

					let mut pre_value: Option<TimeSeriesMeasurement> = match tree.get_lt(k) {
						Ok(value) => match value {
							Some(value) => match String::from_utf8(value.1.to_vec()) {
								Ok(value) => match serde_json::from_str(value.as_str()) {
									Ok(value) => value,
									Err(_) => None,
								},
								Err(_) => None,
							},
							None => None,
						},
						Err(_) => None,
					};

					let mut next_next_value: Option<TimeSeriesMeasurement> = None;
					let mut pre_pre_value: Option<TimeSeriesMeasurement> = None;

					// fix error in sleds handling of negative keys
					// get all keys and sort them
					// find the next_next_value and pre_pre_value
					// swap next_value and pre_value if the key is negative
					if key.parse::<f64>().unwrap() < 0.0 {
						let params = BucketParams::default();
						let values = match get::get_timeseries_bucket(tree.clone(), params).await {
							Ok(values) => values,
							Err(e) => return Err(e),
						};
						let mut values: Vec<TimeSeriesMeasurement> = values
							.iter()
							.map(|v| match v {
								BucketValue::TimeSeries(v) => v.clone(),
								_ => unreachable!(),
							})
							.collect();
						values.sort_by(|a, b| a.timestamp.partial_cmp(&b.timestamp).unwrap());
						let mut i = 0;
						while i < values.len() {
							if values[i].timestamp > BigDecimal::from_f64(key.parse::<f64>().unwrap()).unwrap() {
								if values.len() >= (i + 1) {
									next_value = Some(values[i].clone());
									next_next_value = Some(values[i + 1].clone());
								} else {
									next_value = None;
									next_next_value = None;
								}
								if i >= 2 {
									pre_value = Some(values[i - 2].clone());
									pre_pre_value = Some(values[i - 1].clone());
								} else {
									pre_value = None;
									pre_pre_value = None;
								}
								break;
							}
							i += 1;
						}
					}

					// if next and previous value are not none, interpolate
					if next_value.is_some() && pre_value.is_some() {
						let next_value = next_value.unwrap();
						let pre_value = pre_value.unwrap();
						let vals: [[BigDecimal; 2]; 2] = [[pre_value.timestamp, pre_value.value], [next_value.timestamp, next_value.value]];
						let new_value = get::linear_interp(vals, BigDecimal::from_str(key).unwrap()).await;
						let new_value = {
							let mut tags: HashMap<String, String> = HashMap::new();
							tags.insert("interpolated".to_owned(), "true".to_owned());
							TimeSeriesMeasurement { timestamp: new_value[0].clone(), value: new_value[1].clone(), tags: Some(tags.clone()) }
						};
						return Ok(BucketValue::TimeSeries(new_value));
					} else if next_value.is_none() && pre_value.is_none() {
						return Err((StatusCode::NOT_FOUND, "No keys found in tree.".to_string()));
					}

					// if next value is none, extrapolate
					if next_value.is_none() {
						if pre_value.is_some() {
							if pre_pre_value.is_none() {
								pre_pre_value = match tree.get_lt(pre_value.clone().unwrap().timestamp.to_f64().unwrap().to_be_bytes()) {
									Ok(value) => match value {
										Some(value) => match String::from_utf8(value.1.to_vec()) {
											Ok(value) => match serde_json::from_str(value.as_str()) {
												Ok(value) => value,
												Err(_) => None,
											},
											Err(_) => None,
										},
										None => None,
									},
									Err(_) => None,
								};
							}
							if pre_pre_value.is_some() {
								let pre_value = pre_value.unwrap();
								let pre_pre_value = pre_pre_value.unwrap();
								let vals: [[BigDecimal; 2]; 2] = [[pre_pre_value.timestamp, pre_pre_value.value], [pre_value.timestamp, pre_value.value]];
								let new_value = get::linear_interp(vals, BigDecimal::from_str(key).unwrap()).await;
								let new_value = {
									let mut tags: HashMap<String, String> = HashMap::new();
									tags.insert("interpolated".to_owned(), "true".to_owned());
									TimeSeriesMeasurement { timestamp: new_value[0].clone(), value: new_value[1].clone(), tags: Some(tags.clone()) }
								};
								return Ok(BucketValue::TimeSeries(new_value));
							} else {
								return Err((StatusCode::NOT_FOUND, "Key not found: no pre-previous value".to_string()));
							}
						} else {
							return Err((StatusCode::NOT_FOUND, "Key not found: no previous value.".to_string()));
						}
					}

					// if previous value is none, extrapolate
					if pre_value.is_none() {
						if next_value.is_some() {
							if next_next_value.is_none() {
								next_next_value = match tree.get_gt(next_value.clone().unwrap().timestamp.to_f64().unwrap().to_be_bytes()) {
									Ok(value) => match value {
										Some(value) => match String::from_utf8(value.1.to_vec()) {
											Ok(value) => match serde_json::from_str(value.as_str()) {
												Ok(value) => value,
												Err(_) => None,
											},
											Err(_) => None,
										},
										None => None,
									},
									Err(_) => None,
								};
							}

							if next_next_value.is_some() {
								let next_value = next_value.unwrap();
								let next_next_value = next_next_value.unwrap();
								let vals: [[BigDecimal; 2]; 2] = [[next_value.timestamp, next_value.value], [next_next_value.timestamp, next_next_value.value]];
								let new_value = get::linear_interp(vals, BigDecimal::from_str(key).unwrap()).await;
								let new_value = {
									let mut tags: HashMap<String, String> = HashMap::new();
									tags.insert("interpolated".to_owned(), "true".to_owned());
									TimeSeriesMeasurement { timestamp: new_value[0].clone(), value: new_value[1].clone(), tags: Some(tags.clone()) }
								};
								return Ok(BucketValue::TimeSeries(new_value));
							} else {
								return Err((StatusCode::NOT_FOUND, "Key not found: next next value".to_string()));
							}
						} else {
							return Err((StatusCode::NOT_FOUND, "Key not found: previous value".to_string()));
						}
					}
				}
				Err((StatusCode::NOT_FOUND, "Key not found. Try '?interpolation=true' to linearly interpolate value for any key.".to_string()))
			}
			BucketType::Object => {
				let mut value: ObjectValue = match tree.get(key) {
					Ok(value) => match value {
						Some(value) => match String::from_utf8(value.to_vec()) {
							Ok(value) => match serde_json::from_str(value.as_str()) {
								Ok(value) => value,
								Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse object value: {}", e))),
							},
							Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse object value: {}", e))),
						},
						None => return Err((StatusCode::NOT_FOUND, "Key not found".to_string())),
					},
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
					Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Faild to insert key: {}", e))),
				};
				let mut value = object_value;
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
			BucketValue::TimeSeries(time_series_measurement) => {
				let key = time_series_measurement.timestamp.to_string();
				let time_series_measurement = json!(time_series_measurement).to_string();
				match tree.insert(key, time_series_measurement.as_bytes()) {
					Ok(_) => (),
					Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Faild to insert key: {}", e))),
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
