use axum::{routing::get, Json, Router};
use dsm_measurement::*;
use serde::Deserialize;
use serde_json::{json, Value};
use thorchain_ninerealms_midguard::*;
use uuid::Uuid;

mod thorchain_ninerealms_midguard;

#[tokio::main]
async fn main() {
	let app = Router::new().route("/btcbtcusausd", get(btcbtc_usausd));
        println!("Listening on http://0.0.0.0:8519/");
	axum::Server::bind(&"0.0.0.0:8519".parse().unwrap()).serve(app.into_make_service()).await.unwrap();
}

#[derive(Deserialize)]
pub struct BtcBtcUsaUsdParams {
	pub interval: DepthInterval,
	pub count: u16,
	pub to: Option<i64>,
	pub from: Option<i64>,
}

#[axum::debug_handler]
pub async fn btcbtc_usausd() -> Json<Value> {
	let params = BtcBtcUsaUsdParams { interval: DepthInterval::FiveMinute, count: 400, to: None, from: None };

	let thorchain = ThorchainNinerealms::new();
	if !thorchain.pool_is_active("BTC.BTC").await {
		return Json(json!({
				"error": "Pool is not active"
		}));
	}

	let quotes = thorchain.get_usd_quote("BTC.BTC", params.interval, params.count, params.to, params.from).await;

	let measurements: Vec<Measurement> = quotes
		.iter()
        		.map(|(time, price)| {
                                Measurement::new("thorchain", "BtcBtc", "UsaUsd", Uuid::new_v4(), time.clone(), price.clone())
                        })
		.collect();

	Json(json!({ "measurements": measurements }))
}






