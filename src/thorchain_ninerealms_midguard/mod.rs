#![allow(dead_code)]
use bigdecimal::BigDecimal;
pub use depth_interval::*;
pub use pool_period::*;
pub use pool_status::*;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

mod depth_interval;
mod pool_period;
mod pool_status;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThorchainNinerealms {
	pub base_url: String,
}

impl ThorchainNinerealms {
	pub fn new() -> Self {
		Self { base_url: "https://midgard.ninerealms.com".to_string() }
	}

	pub async fn pools(&self, status: PoolStatus, period: PoolPeriod) -> Result<serde_json::Value, String> {
		let client = reqwest::Client::new();
		let url = format!("{}/v2/pools?status={}&period={}", self.base_url, status, period);
		let response = client.get(&url).send().await;
		match response {
			Ok(response) => {
				let response = response.json::<serde_json::Value>().await;
				match response {
					Ok(response) => Ok(response),
					Err(error) => Err(error.to_string()),
				}
			}
			Err(error) => Err(error.to_string()),
		}
	}

	pub async fn pool_is_active(&self, pool: &str) -> bool {
		let pools = self.pools(PoolStatus::Available, PoolPeriod::Hour).await;
		match pools {
			Ok(pools) => {
				let pools = match pools.as_array() {
					Some(pools) => pools,
					None => return false,
				};
				for p in pools {
					if p["asset"].as_str().unwrap() == pool {
						return true;
					}
				}
				false
			}
			Err(_) => false,
		}
	}

	pub async fn get_usd_quote(&self, pool: &str, interval: DepthInterval, count: u16, to: Option<i64>, from: Option<i64>) -> Vec<(i64, BigDecimal)> {
		let client = reqwest::Client::new();
		let url = format!("{}/v2/history/depths/{}?interval={}&count={}", self.base_url, pool, interval, count);
		let url = match to {
			Some(to) => format!("{}&to={}", url, to),
			None => url,
		};
		let url = match from {
			Some(from) => format!("{}&from={}", url, from),
			None => url,
		};

		let response = client.get(&url).send().await.unwrap().json::<serde_json::Value>().await.unwrap();

		let mut quotes: Vec<(i64, BigDecimal)> = Vec::new();
		for interval in response["intervals"].as_array().unwrap() {
			let price = BigDecimal::from_str(interval["assetPriceUSD"].as_str().unwrap()).unwrap();
			let time = i64::from_str(interval["endTime"].as_str().unwrap()).unwrap();
			quotes.push((time, price));
		}

		quotes
	}
}
