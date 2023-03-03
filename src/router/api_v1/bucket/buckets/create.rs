use super::super::*;
use axum::Json;

#[derive(Deserialize)]
pub struct CreateBucketParams {
	pub name: String,
	#[serde(rename = "type")]
	pub bucket_type: String,
	pub debug: Option<bool>,
	pub tag: Option<Vec<String>>,
}

pub async fn create_bucket(State(db): State<Db>, Qs(params): Qs<CreateBucketParams>, _body: Bytes) -> Json<Value> {
	let bucket_name = params.name;
	let bucket_type = params.bucket_type;
	let tags = process_tags(params.tag).await;
	let debug = params.debug.unwrap_or(false);

	let bucket = match Bucket::new(&bucket_name, &bucket_type, tags, db.clone()).await {
		Ok(bucket) => bucket,
		Err(err) => return err_debug(&err.to_string(), db, debug).await,
	};

	if debug {
		let debugdb = DebugDb { db: db.clone() };
		Json(json!({"bucket": bucket , "db": debugdb }))
	} else {
		Json(json!(bucket))
	}
}
