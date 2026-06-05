use super::super::*;
use axum::extract::rejection::JsonRejection;
use axum::{http::StatusCode, Json};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct DeleteBucketParams {
	pub debug: Option<bool>,
}

pub async fn delete_bucket(Path(bucket): Path<String>, Qs(params): Qs<DeleteBucketParams>, State(db): State<Db>, _body: Result<Json<Value>, JsonRejection>) -> (StatusCode, Json<Value>) {
	let debug: bool = params.debug.unwrap_or(false);

	let bucket = match Bucket::open(&bucket, db.clone()).await {
		Ok(bucket) => bucket,
		Err(err) => return (err.0, err_debug(&err.1, db.clone(), debug).await),
	};

	let value = match bucket.delete(db.clone()).await {
		Ok(value) => value,
		Err(err) => return (err.0, err_debug(&err.1, db.clone(), debug).await),
	};

	if debug {
		let debugdb = DebugDb { db: db.clone() };
		(StatusCode::OK, Json(json!({ "bucket": bucket, "value": value, "db": debugdb })))
	} else {
		(StatusCode::OK, Json(json!({ "bucket": bucket, "value": value })))
	}
}
