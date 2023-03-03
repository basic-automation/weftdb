use super::super::*;
use axum::{extract::Path, extract::State, Json};
use serde_json::json;
use serde_json::Value;
use sled::Db;

pub async fn remove_from_bucket(Path(path): Path<Vec<String>>, Query(params): Query<HashMap<String, String>>, State(db): State<Db>) -> Json<Value> {
	let bucket = path[0].clone();
	let key = path[1].clone();
	let debug = params.get("debug").map(|x| x.parse().unwrap()).unwrap_or(false);

	let bucket = match Bucket::open(&bucket, db.clone()).await {
		Ok(bucket) => bucket,
		Err(err) => return err_debug(&err, db.clone(), debug).await,
	};

	let value = match bucket.key_remove(&key, db.clone()).await {
		Ok(val) => val,
		Err(err) => return err_debug(&err, db.clone(), debug).await,
	};

	if debug {
		let debugdb = DebugDb { db: db.clone() };
		Json(json!({ "value": value, "db": debugdb }))
	} else {
		Json(json!(value))
	}
}
