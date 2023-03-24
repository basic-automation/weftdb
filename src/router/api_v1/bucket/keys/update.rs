use super::super::*;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::{extract::Path, extract::State, Json};
use bigdecimal::BigDecimal;
use serde_json::json;
use serde_json::Value;
use sled::Db;

#[derive(Deserialize)]
pub struct UpdateBucketParams {
	debug: Option<bool>,
	tag:   Option<Vec<String>>,
	value: Option<String>,
}

pub async fn update_key(State(db): State<Db>, Qs(params): Qs<UpdateBucketParams>, Path(path): Path<Vec<String>>, body: Result<Json<Value>, JsonRejection>) -> (StatusCode, Json<Value>) {
	let debug = params.debug.unwrap_or(false);
	let bucket = match Bucket::open(&path[0].clone(), db.clone()).await {
		Ok(bucket) => bucket,
		Err(err) => return (err.0, err_debug(&err.1, db.clone(), debug).await),
	};
	let key = path[1].clone();
	let tags = process_tags(params.tag).await;
	let body = body.map(|body| body.0).unwrap_or_else(|_| Value::Null);

	match bucket.type_ {
		BucketType::TimeSeries => {
			let key: BigDecimal = match key.parse() {
				Ok(key) => key,
				Err(err) => return (StatusCode::BAD_REQUEST, err_debug(&err.to_string(), db.clone(), debug).await),
			};

			let value = params.value.unwrap_or_else(|| body.to_string());
			let bucket_value: BucketValue = BucketValue::TimeSeries(TimeSeriesMeasurement::new(&value, key, tags));
			let value = match bucket.key_update(bucket_value, db.clone()).await {
				Ok(val) => val,
				Err(err) => return (err.0, err_debug(&err.1.to_string(), db.clone(), debug).await),
			};

			if debug {
				let debugdb = DebugDb { db: db.clone() };
				(StatusCode::OK, Json(json!({ "value": value, "db": debugdb })))
			} else {
				(StatusCode::OK, Json(json!(value)))
			}
		}
		BucketType::Object => {
			let value = body.clone();
			let bucket_value: BucketValue = BucketValue::Object(ObjectValue::new(value.to_string(), &key.to_string()));
			let value = match bucket.key_update(bucket_value, db.clone()).await {
				Ok(val) => val,
				Err(err) => return (err.0, err_debug(&err.1.to_string(), db.clone(), debug).await),
			};

			let val = match value {
				BucketValue::Object(val) => val,
				_ => return (StatusCode::BAD_REQUEST, err_debug("Value must be an object value.", db.clone(), debug).await),
			};

			if debug {
				let debugdb = DebugDb { db: db.clone() };
				(StatusCode::OK, Json(json!({ val.key: val.value, "db": debugdb })))
			} else {
				(StatusCode::OK, Json(json!({val.key: val.value})))
			}
		}
	}
}
