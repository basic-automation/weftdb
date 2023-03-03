use crate::source::Source;
use axum::Json;
use serde_json::Value;
use urlencoding::encode;

pub async fn add_source(Json(params): Json<Source>) -> Json<Value> {
	// create input_sources bucket if it doesn't exist
	// e.g. http://127.0.0.1:8515/bucket?name=input_sources&type=object
	let client = reqwest::Client::new();
	client.post("http://127.0.0.1:8515/bucket?name=input_sources&type=object").send().await.unwrap();

	// add source to input_sources bucket
	// e.g. http://127.0.0.1:8515/bucket/{bucket_name}/{key}
	// with body: { "name": "name", "interval": 60, "numerator": "numerator", "denominator": "denominator" }
	let url_encoded_key = encode(&params.url).to_string();
	let client = reqwest::Client::new();
	let res = client.put(format!("http://127.0.0.1:8515/bucket/input_sources/{}", url_encoded_key)).json(&params).send().await;

	Json(res.unwrap().json().await.unwrap())
}
