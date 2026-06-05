use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum BucketType {
	TimeSeries,
	Object,
}

impl ToString for BucketType {
	fn to_string(&self) -> String {
		match self {
			BucketType::TimeSeries => "TimeSeries".to_string(),
			BucketType::Object => "Object".to_string(),
		}
	}
}

impl FromStr for BucketType {
	type Err = String;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		match s.to_lowercase().as_str() {
			"timeseries" => Ok(BucketType::TimeSeries),
			"object" => Ok(BucketType::Object),
			_ => Err("Invalid bucket type".to_string()),
		}
	}
}
