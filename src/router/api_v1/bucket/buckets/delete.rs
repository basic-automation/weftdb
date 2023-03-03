use super::super::*;
use axum::Json;

pub async fn delete_bucket(Path(bucket): Path<String>, Query(params): Query<HashMap<String, String>>, State(db): State<Db>) -> Json<Value> {
	let debug: bool = params.get("debug").unwrap_or(&"false".to_string()).parse().unwrap();

	let bucket = match Bucket::open(&bucket, db.clone()).await {
		Ok(bucket) => bucket,
		Err(err) => return err_debug(&err, db.clone(), debug).await,
	};

	let value = match bucket.delete(db.clone()).await {
		Ok(value) => value,
		Err(err) => return err_debug(&err, db.clone(), debug).await,
	};

	if debug {
		let debugdb = DebugDb { db: db.clone() };
		Json(json!({ "bucket": bucket, "value": value, "db": debugdb }))
	} else {
		Json(json!({ "bucket": bucket, "value": value }))
	}
}
