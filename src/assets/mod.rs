use core::fmt::{Display, Formatter};
use core::str::FromStr;
use serde::{Deserialize, Serialize};

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Asset {
	#[serde(rename = "USA.USD")]
	UsaUsd,
	#[serde(rename = "BTC.BTC")]
	BtcBtc,
}

impl FromStr for Asset {
	type Err = ();

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		match s.to_lowercase().as_str() {
			"usausd" => Ok(Asset::UsaUsd),
			"usa.usd" => Ok(Asset::UsaUsd),
			"btcbtc" => Ok(Asset::BtcBtc),
			"btc.btc" => Ok(Asset::BtcBtc),
			_ => Err(()),
		}
	}
}

impl Display for Asset {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		match self {
			Asset::UsaUsd => write!(f, "USA.USD"),
			Asset::BtcBtc => write!(f, "BTC.BTC"),
		}
	}
}

pub trait ToAssets {
	fn to_assets(&self) -> (Asset, Asset);
}
