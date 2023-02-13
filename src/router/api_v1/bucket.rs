use crate::types::*;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::{extract::Path, extract::Query, response::Json, extract::FromRequest, http::Request, async_trait, extract::State};
use serde::{ser::SerializeStruct, Deserialize, Serialize};
use serde_json::{json, Value};
use sled::Db;
use std::collections::HashMap;
use std::convert::Infallible;
use axum::body::Bytes;

pub struct Qs<T>(T);

#[async_trait]
impl<S, B, T> FromRequest<S, B> for Qs<T> where T: serde::de::DeserializeOwned, B: Send + 'static, S: Send + Sync {
    type Rejection = Infallible;

    async fn from_request(req: Request<B>, _state: &S) -> Result<Self, Self::Rejection> {
        // TODO: error handling
        let query = req.uri().query().unwrap();
        Ok(Self(serde_qs::from_str(query).unwrap()))
    }
}

#[async_trait]
impl<S, T> FromRequestParts<S> for Qs<T> where T: serde::de::DeserializeOwned, S: Sized {
    type Rejection = Infallible;

    async fn from_request_parts(req: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let query = req.uri.query().unwrap();
        Ok(Self(serde_qs::from_str(query).unwrap()))
    }
}

#[derive(Deserialize)]
pub struct CreateBucketParams {
	pub name: String,
        #[serde(rename = "type")]
	pub bucket_type: String,
	pub debug: bool,
	pub tag: Option<Vec<String>>,
}

pub async fn process_tags(tags: Option<Vec<String>>) -> Option<HashMap<String, String>> {
        match tags {
                Some(tags) => {
                        let mut res = HashMap::new();
                        for tag in tags {
                                if tag.contains("=") == false {
                                        res.insert(tag, "".to_string());
                                } else {
                                        let tag: Vec<&str> = tag.split("=").collect();
                                        res.insert(tag[0].to_string(), tag[1].to_string());
                                }
                                
                        }
                        Some(res)
                }
                None => None,
        }
}

pub async fn create_bucket(State(db): State<Db>, Qs(params): Qs<CreateBucketParams>, _body: Bytes) -> Json<Value> {
	let bucket_name = params.name;
	let bucket_type = params.bucket_type;
	let tags = process_tags(params.tag).await;
	let debug = params.debug;

	match Bucket::new(&bucket_name, &bucket_type, tags, db.clone()).await {
		Ok(bucket) => {
			if debug {
				let debugdb = DebugDb { db: db.clone() };
				Json(json!({"bucket": bucket , "db": debugdb }))
			} else {
				Json(json!(bucket))
			}
		}
		Err(err) => {
			if debug {
				let debugdb = DebugDb { db: db.clone() };
				Json(json!({ "error": err, "db": debugdb }))
			} else {
				Json(json!({ "error": err }))
			}
		}
	}
}

pub async fn get_bucket(Path(bucket): Path<String>, Query(params): Query<HashMap<String, String>>, db: Db) -> Json<Value> {
	let debug: bool = params.get("debug").unwrap_or(&"false".to_string()).parse().unwrap();
	let bucket = Bucket::open(&bucket, db.clone()).await.unwrap();

	match bucket.type_ {
		BucketType::TimeSeries => match bucket.get(db.clone()).await {
			Ok(value) => {
				if debug {
					let debugdb = DebugDb { db: db.clone() };
					Json(json!({ "bucket": bucket, "value": value, "db": debugdb }))
				} else {
					Json(json!({ "bucket": bucket, "value": value }))
				}
			}
			Err(err) => {
				if debug {
					let debugdb = DebugDb { db: db.clone() };
					Json(json!({ "bucket": bucket, "error": err, "db": debugdb }))
				} else {
					Json(json!({ "bucket": bucket, "error": err }))
				}
			}
		},
		BucketType::Object => match bucket.get(db.clone()).await {
			Ok(value) => {
				if debug {
					let debugdb = DebugDb { db: db.clone() };
					Json(json!({ "bucket": bucket, "value": value, "db": debugdb }))
				} else {
					Json(json!({ "bucket": bucket, "value": value }))
				}
			}
			Err(err) => {
				if debug {
					let debugdb = DebugDb { db: db.clone() };
					Json(json!({ "bucket": bucket, "error": err, "db": debugdb }))
				} else {
					Json(json!({ "bucket": bucket, "error": err }))
				}
			}
		},
	}
}

pub async fn delete_bucket(Path(bucket): Path<String>, Query(params): Query<HashMap<String, String>>, db: Db) -> Json<Value> {
	let debug: bool = params.get("debug").unwrap_or(&"false".to_string()).parse().unwrap();

	let bucket = match Bucket::open(&bucket, db.clone()).await {
		Ok(bucket) => bucket,
		Err(err) => {
			if debug {
				let debugdb = DebugDb { db: db.clone() };
				return Json(json!({ "error": err, "db": debugdb }));
			} else {
				return Json(json!({ "error": err }));
			}
		}
	};

	match bucket.delete(db.clone()).await {
		Ok(value) => {
			if debug {
				let debugdb = DebugDb { db: db.clone() };
				Json(json!({ "bucket": bucket, "value": value, "db": debugdb }))
			} else {
				Json(json!({ "bucket": bucket, "value": value }))
			}
		}
		Err(err) => {
			if debug {
				let debugdb = DebugDb { db: db.clone() };
				Json(json!({ "bucket": bucket, "error": err, "db": debugdb }))
			} else {
				Json(json!({ "bucket": bucket, "error": err }))
			}
		}
	}
}

#[derive(Deserialize)]
pub struct AddToBucketParams {
	pub key: String,
	pub value: Option<String>,
	pub debug: Option<bool>,
	pub tag: Option<Vec<String>>,
}

#[axum::debug_handler]
pub async fn add_to_bucket(State(db): State<Db>, Qs(params): Qs<AddToBucketParams>, Path(bucket): Path<String>, Json(body): Json<Value>) -> Json<Value> {
	let debug = match params.debug {
                Some(debug) => debug,
                None => false,
        };
	let key = params.key;
	let tags = process_tags(params.tag).await;

	// value
	let value = body.clone();

	let bucket = Bucket::open(&bucket, db.clone()).await.unwrap();

	match bucket.type_ {
		BucketType::TimeSeries => {
			let key: i64 = match key.parse() {
				Ok(key) => key,
				Err(err) => {
					if debug {
						let debugdb = DebugDb { db: db.clone() };
						return Json(json!({ "error": err.to_string(), "db": debugdb }));
					} else {
						return Json(json!({ "error": err.to_string() }));
					}
				}
			};

			let value = params.value.unwrap_or_else(|| body.to_string());
			let bucket_value: BucketValue = BucketValue::TimeSeries(TimeSeriesMeasurement::new(&value, key, tags));
			match bucket.key_add(bucket_value, db.clone()).await {
				Ok(val) => {
					if debug {
						let debugdb = DebugDb { db: db.clone() };
						Json(json!({ "value": val, "db": debugdb }))
					} else {
						Json(json!(val))
					}
				}
				Err(err) => {
					if debug {
						let debugdb = DebugDb { db: db.clone() };
						Json(json!({ "error": err, "db": debugdb }))
					} else {
						Json(json!({ "error": err }))
					}
				}
			}
		}
		BucketType::Object => {
			let bucket_value: BucketValue = BucketValue::Object(ObjectValue::new(value.to_string(), &key.to_string()));
			match bucket.key_add(bucket_value, db.clone()).await {
				Ok(val) => {
					let val = match val {
						BucketValue::Object(val) => val,
						_ => panic!("Expected BucketValue::Object"),
					};
					if debug {
						let debugdb = DebugDb { db: db.clone() };
						Json(json!({ val.key: val.value, "db": debugdb }))
					} else {
						Json(json!({val.key: val.value}))
					}
				}
				Err(err) => {
					if debug {
						let debugdb = DebugDb { db: db.clone() };
						Json(json!({ "error": err, "db": debugdb }))
					} else {
						Json(json!({ "error": err }))
					}
				}
			}
		}
	}
}

pub async fn get_from_bucket(Path(path): Path<Vec<String>>, Query(params): Query<HashMap<String, String>>, db: Db) -> Json<Value> {
	let bucket = path[0].clone();
	let key = path[1].clone();
	let debug = match params.get("debug").map(|x| x.parse().unwrap()) {
		Some(debug) => debug,
		None => false,
	};

	match Bucket::open(&bucket, db.clone()).await {
		Ok(bucket) => match bucket.key_get(&key, db.clone()).await {
			Ok(val) => match val {
				BucketValue::TimeSeries(val) => {
					if debug {
						let debugdb = DebugDb { db: db.clone() };
						Json(json!(
						{
								"value": {
										"timestamp": val.timestamp,
										"value": val.value
								},
								"db": debugdb
						}))
					} else {
						Json(json!({
								"timestamp": val.timestamp,
								"value": val.value
						}))
					}
				}
				BucketValue::Object(val) => {
					if debug {
						let debugdb = DebugDb { db: db.clone() };
						Json(json!(
						{
								val.key: val.value,
								"db": debugdb
						}))
					} else {
						Json(json!({
								val.key: val.value
						}))
					}
				}
			},
			Err(err) => {
				if debug {
					let debugdb = DebugDb { db: db.clone() };
					Json(json!({ "error": err, "db": debugdb }))
				} else {
					Json(json!({ "error": err }))
				}
			}
		},
		Err(err) => {
			if debug {
				let debugdb = DebugDb { db: db.clone() };
				Json(json!({ "error": err, "db": debugdb }))
			} else {
				Json(json!({ "error": err }))
			}
		}
	}
}

pub async fn remove_from_bucket(Path(path): Path<Vec<String>>, Query(params): Query<HashMap<String, String>>, db: Db) -> Json<Value> {
	let bucket = path[0].clone();
	let key = path[1].clone();
	let debug = match params.get("debug").map(|x| x.parse().unwrap()) {
		Some(debug) => debug,
		None => false,
	};

	match Bucket::open(&bucket, db.clone()).await {
		Ok(bucket) => match bucket.key_remove(&key, db.clone()).await {
			Ok(val) => {
				if debug {
					let debugdb = DebugDb { db: db.clone() };
					Json(json!({ "value": val, "db": debugdb }))
				} else {
					Json(json!(val))
				}
			}
			Err(err) => {
				if debug {
					let debugdb = DebugDb { db: db.clone() };
					Json(json!({ "error": err, "db": debugdb }))
				} else {
					Json(json!({ "error": err }))
				}
			}
		},
		Err(err) => {
			if debug {
				let debugdb = DebugDb { db: db.clone() };
				Json(json!({ "error": err, "db": debugdb }))
			} else {
				Json(json!({ "error": err }))
			}
		}
	}
}
pub struct DebugDb {
	db: Db,
}

impl Serialize for DebugDb {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let mut state = serializer.serialize_struct("debug", 1)?;
		let tree_names = self.db.tree_names();
		let mut trees: Vec<Value> = Vec::new();
		for name in tree_names {
			let name = std::str::from_utf8(name.as_ref()).unwrap();
			let tree = self.db.open_tree(name).unwrap();
			let mut items: Vec<Value> = Vec::new();
			for item in tree.iter() {
				let item = item.unwrap();
				let key = std::str::from_utf8(item.0.as_ref()).unwrap();
				let value: Value = serde_json::from_str(std::str::from_utf8(item.1.as_ref()).unwrap()).unwrap();
				items.push(json!({ key: value }));
			}
			trees.push(json!({ name: items }));
		}
		state.serialize_field("db", &trees)?;
		state.end()
	}
}
