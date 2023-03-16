use super::super::*;
use axum::http::StatusCode;
use axum::{extract::Path, extract::State, Json};
use serde_json::json;
use serde_json::Value;
use sled::Db;
use serde::Deserialize;
use axum::extract::rejection::JsonRejection;

#[derive(Deserialize)]
pub struct GetFromBucket {
        pub debug: Option<bool>,
}

pub async fn get_from_bucket(Path(path): Path<Vec<String>>, Qs(params): Qs<GetFromBucket>, State(db): State<Db>, _body: Result<Json<Value>, JsonRejection>) -> (StatusCode, Json<Value>) {
	let bucket = path[0].clone();
	let key = path[1].clone();
	let debug = params.debug.unwrap_or(false);

	let bucket = match Bucket::open(&bucket, db.clone()).await {
		Ok(bucket) => bucket,
		Err(err) => return (err.0, err_debug(&err.1, db.clone(), debug).await),
	};

	let value = match bucket.key_get(&key, db.clone()).await {
		Ok(val) => val,
		Err(err) => return (err.0, err_debug(&err.1, db.clone(), debug).await),
	};

	match value {
		BucketValue::TimeSeries(val) => {
			if debug {
				let debugdb = DebugDb { db: db.clone() };
				(StatusCode::OK, Json(json!({"value": {"timestamp": val.timestamp, "value": val.value}, "db": debugdb})))
			} else {
				(StatusCode::OK, Json(json!({"timestamp": val.timestamp,"value": val.value})))
			}
		}
		BucketValue::Object(val) => {
			if debug {
				let debugdb = DebugDb { db: db.clone() };
				(StatusCode::OK, Json(json!({val.key: val.value, "db": debugdb})))
			} else {
				(StatusCode::OK, Json(json!({val.key: val.value})))
			}
		}
	}
}
