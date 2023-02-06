#![allow(dead_code)]
use crate::assets::*;
use core::fmt::{Display, Formatter};
use core::str::FromStr;
use serde::{Deserialize, Serialize};
pub use thorchain_ninerealms_midguard::*;

mod thorchain_ninerealms_midguard;

/// The Measurement bucket where the data is placed in the database.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Source {
	#[serde(rename = "AgrogateBtcBtcAndUsaUsd")]
	AgrogateBtcBtcAndUsaUsd,
	#[serde(rename = "ThorchainBtcBtcAndUsaUsd", deserialize_with = "thorchain_deserializer")]
	ThorchainBtcBtcAndUsaUsd(ThorchainNinerealms),
}

fn thorchain_deserializer<'de, D>(_deserializer: D) -> Result<ThorchainNinerealms, D::Error>
where
	D: serde::Deserializer<'de>,
{
	Ok(ThorchainNinerealms::new())
}

impl Source {
	pub async fn aggrogate_source(&self) -> Source {
		match self {
			Source::AgrogateBtcBtcAndUsaUsd => Source::AgrogateBtcBtcAndUsaUsd,
			Source::ThorchainBtcBtcAndUsaUsd(_) => Source::AgrogateBtcBtcAndUsaUsd,
		}
	}
}

impl FromStr for Source {
	type Err = ();

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		match s.to_lowercase().as_str() {
			"agrogatebtcbtcandusausd" => Ok(Source::AgrogateBtcBtcAndUsaUsd),
			"thorchainbtcbtcandusausd" => Ok(Source::ThorchainBtcBtcAndUsaUsd(ThorchainNinerealms::new())),
			_ => Err(()),
		}
	}
}

impl Display for Source {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		match self {
			Source::AgrogateBtcBtcAndUsaUsd => write!(f, "AgrogateBtcBtcAndUsaUsd"),
			Source::ThorchainBtcBtcAndUsaUsd(_) => write!(f, "ThorchainBtcBtcAndUsaUsd"),
		}
	}
}

impl ToAssets for Source {
	fn to_assets(&self) -> (Asset, Asset) {
		match self {
			Source::AgrogateBtcBtcAndUsaUsd => (Asset::BtcBtc, Asset::UsaUsd),
			Source::ThorchainBtcBtcAndUsaUsd(_) => (Asset::BtcBtc, Asset::UsaUsd),
		}
	}
}
