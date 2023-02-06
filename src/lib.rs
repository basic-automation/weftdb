use dsm_asset::*;
use core::fmt::{Display, Formatter};
use core::str::FromStr;
use serde::{Deserialize, Serialize};
pub use thorchain_ninerealms_midguard::*;

mod thorchain_ninerealms_midguard;

/// The Measurement bucket where the data is placed in the database.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Source {
	AgrogateBtcBtcAndUsaUsd,
	ThorchainBtcBtcAndUsaUsd(ThorchainNinerealms),
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

impl ToAsset for Source {
	fn to_asset(&self) -> Asset {
		match self {
			Source::AgrogateBtcBtcAndUsaUsd => Asset::BtcBtc,
			Source::ThorchainBtcBtcAndUsaUsd(_) => Asset::BtcBtc,
		}
	}
}

