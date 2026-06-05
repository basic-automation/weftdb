use super::super::*;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::Json;

pub async fn get_bucket(Path(bucket): Path<String>, Qs(params): Qs<BucketParams>, State(db): State<Db>, _body: Result<Json<Value>, JsonRejection>) -> (StatusCode, Json<Value>) {
	let debug: bool = params.debug.unwrap_or(false);
	let bucket = match Bucket::open(&bucket, db.clone()).await {
		Ok(bucket) => bucket,
		Err(err) => return (err.0, err_debug(&err.1, db.clone(), debug).await),
	};

	match bucket.type_ {
		BucketType::TimeSeries => match bucket.get(Some(params), db.clone()).await {
			Ok(value) => {
				if debug {
					let debugdb = DebugDb { db: db.clone() };
					(StatusCode::OK, Json(json!({ "bucket": bucket, "value": value, "db": debugdb })))
				} else {
					(StatusCode::OK, Json(json!({ "bucket": bucket, "value": value })))
				}
			}
			Err(err) => (err.0, err_debug(&err.1, db.clone(), debug).await),
		},
		BucketType::Object => match bucket.get(Some(params), db.clone()).await {
			Ok(value) => {
				if debug {
					let debugdb = DebugDb { db: db.clone() };
					(StatusCode::OK, Json(json!({ "bucket": bucket, "value": value, "db": debugdb })))
				} else {
					(StatusCode::OK, Json(json!({ "bucket": bucket, "value": value })))
				}
			}
			Err(err) => (err.0, err_debug(&err.1, db.clone(), debug).await),
		},
	}
}
