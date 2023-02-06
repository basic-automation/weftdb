use crate::assets::Asset;
use crate::sources::Source;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MeasurementNewEvent {
	pub measurement_source: Source,
	pub measurement_asset_1: Asset,
	pub measurement_asset_2: Asset,
	#[serde(rename = "_value")]
	pub measurement_uuid: String,
	#[serde(rename = "_time", deserialize_with = "deserialize_datetime")]
	pub measurement_time: i64,
}

impl MeasurementNewEvent {
	pub fn name() -> String {
		"measurement_new".to_string()
	}
}

pub fn deserialize_datetime<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
	D: serde::Deserializer<'de>,
{
	let s = String::deserialize(deserializer)?;
	Ok(DateTime::parse_from_rfc3339(&s).unwrap().with_timezone(&Utc).timestamp())
}
