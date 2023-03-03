use super::super::*;
use axum::extract::rejection::JsonRejection;
use axum::Json;

pub async fn get_bucket(Path(bucket): Path<String>, Qs(params): Qs<BucketParams>, State(db): State<Db>, _body: Result<Json<Value>, JsonRejection>) -> Json<Value> {
	let debug: bool = params.debug.unwrap_or(false);
	let bucket = match Bucket::open(&bucket, db.clone()).await {
		Ok(bucket) => bucket,
		Err(err) => return err_debug(&err, db.clone(), debug).await,
	};

	match bucket.type_ {
		BucketType::TimeSeries => match bucket.get(Some(params), db.clone()).await {
			Ok(value) => {
				if debug {
					let debugdb = DebugDb { db: db.clone() };
					Json(json!({ "bucket": bucket, "value": value, "db": debugdb }))
				} else {
					Json(json!({ "bucket": bucket, "value": value }))
				}
			}
			Err(err) => err_debug(&err, db.clone(), debug).await,
		},
		BucketType::Object => match bucket.get(None, db.clone()).await {
			Ok(value) => {
				if debug {
					let debugdb = DebugDb { db: db.clone() };
					Json(json!({ "bucket": bucket, "value": value, "db": debugdb }))
				} else {
					Json(json!({ "bucket": bucket, "value": value }))
				}
			}
			Err(err) => err_debug(&err, db.clone(), debug).await,
		},
	}
}
