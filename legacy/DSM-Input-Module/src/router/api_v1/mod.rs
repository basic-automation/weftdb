use axum::Json;
use dsm_config::SourceConfiguration;
use serde_json::Value;
use urlencoding::encode;

pub async fn add_source(Json(params): Json<SourceConfiguration>) -> Json<Value> {
	let client = reqwest::Client::new();
	client.post("http://127.0.0.1:8515/bucket?name=input_sources&type=object").send().await.unwrap();

	let url_encoded_key = encode(&params.url).to_string();
	let client = reqwest::Client::new();
	let res = client.put(format!("http://127.0.0.1:8515/bucket/input_sources/{}", url_encoded_key)).json(&params).send().await;

	Json(res.unwrap().json().await.unwrap())
}
