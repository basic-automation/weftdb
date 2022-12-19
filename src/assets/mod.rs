use core::fmt::{Display, Formatter};
use core::str::FromStr;
use serde::{Deserialize, Serialize};

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Asset {
	UsaUsd,
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

pub trait ToAsset {
	fn to_asset(&self) -> Asset;
}
