use super::super::*;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::{extract::Path, extract::State, Json};
use bigdecimal::BigDecimal;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use sled::Db;

#[derive(Deserialize)]
pub struct AddToBucketParams {
	pub key: String,
	pub value: Option<String>,
	pub debug: Option<bool>,
	pub tag: Option<Vec<String>>,
}

pub async fn add_key_to_bucket(State(db): State<Db>, Qs(params): Qs<AddToBucketParams>, Path(bucket): Path<String>, body: Result<Json<Value>, JsonRejection>) -> (StatusCode, Json<Value>) {
	let debug = params.debug.unwrap_or(false);
	let key = params.key;
	let tags = process_tags(params.tag).await;

	let body = body.map(|body| body.0).unwrap_or_else(|_| Value::Null);

	// value
	let value = body.clone();

	let bucket = match Bucket::open(&bucket, db.clone()).await {
		Ok(bucket) => bucket,
		Err(err) => return (err.0, err_debug(&err.1.to_string(), db, debug).await),
	};

	match bucket.type_ {
		BucketType::TimeSeries => {
			let key: BigDecimal = match key.parse() {
				Ok(key) => key,
				Err(err) => return (StatusCode::BAD_REQUEST, err_debug(&format!("Invaild Key: {}", err), db, debug).await),
			};

			let value = params.value.unwrap_or_else(|| body.to_string());
			let bucket_value: BucketValue = BucketValue::TimeSeries(TimeSeriesMeasurement::new(&value, key, tags));
			match bucket.key_add(bucket_value, db.clone()).await {
				Ok(val) => {
					if debug {
						let debugdb = DebugDb { db: db.clone() };
						(StatusCode::OK, Json(json!({ "value": val, "db": debugdb })))
					} else {
						(StatusCode::OK, Json(json!(val)))
					}
				}
				Err(err) => (err.0, err_debug(&err.1.to_string(), db, debug).await),
			}
		}
		BucketType::Object => {
			let bucket_value: BucketValue = BucketValue::Object(ObjectValue::new(value.to_string(), &key.to_string()));
			match bucket.key_add(bucket_value, db.clone()).await {
				Ok(val) => {
					let val = match val {
						BucketValue::Object(val) => val,
						_ => return (StatusCode::BAD_REQUEST, err_debug("Value must be an object value.", db, debug).await),
					};
					if debug {
						let debugdb = DebugDb { db: db.clone() };
						(StatusCode::OK, Json(json!({ val.key: val.value, "db": debugdb })))
					} else {
						(StatusCode::OK, Json(json!({val.key: val.value})))
					}
				}
				Err(err) => (err.0, err_debug(&err.1.to_string(), db, debug).await),
			}
		}
	}
}
