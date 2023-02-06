use crate::assets::Asset;
use crate::sources::Source;
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Measurement {
	#[serde(rename = "_time", deserialize_with = "deserialize_timestamp")]
	pub measurement_time: i64,
	#[serde(rename = "_value")]
	pub measurement_ratio: f64,
	#[serde(deserialize_with = "deserialize_source")]
	pub measurement_source: Source,
	#[serde(deserialize_with = "deserialize_asset")]
	pub measurement_asset_1: Asset,
	#[serde(deserialize_with = "deserialize_asset")]
	pub measurement_asset_2: Asset,
}

fn deserialize_source<'de, D>(deserializer: D) -> Result<Source, D::Error>
where
	D: serde::Deserializer<'de>,
{
	let s = String::deserialize(deserializer)?;
	match Source::from_str(&s) {
		Ok(source) => Ok(source),
		Err(_) => Err(serde::de::Error::custom("invalid source")),
	}
}

fn deserialize_asset<'de, D>(deserializer: D) -> Result<Asset, D::Error>
where
	D: serde::Deserializer<'de>,
{
	let s = String::deserialize(deserializer)?;
	match Asset::from_str(&s) {
		Ok(asset) => Ok(asset),
		Err(_) => Err(serde::de::Error::custom("invalid asset")),
	}
}

fn deserialize_timestamp<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
	D: serde::Deserializer<'de>,
{
	let s = String::deserialize(deserializer)?;
	match DateTime::parse_from_rfc3339(&s) {
		Ok(datetime) => Ok(datetime.timestamp()),
		Err(_) => Err(serde::de::Error::custom("invalid timestamp")),
	}
}
