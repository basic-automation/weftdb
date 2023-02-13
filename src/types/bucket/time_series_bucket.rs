use super::{BucketValue, IntoBucketValue};
use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};
use std::hash::Hash;
use std::{collections::HashMap, str::FromStr};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeSeriesMeasurement {
	pub value: BigDecimal,
	pub timestamp: i64,
	pub tags: Option<HashMap<String, String>>,
}

impl TimeSeriesMeasurement {
	pub fn new(value: &str, timestamp: i64, tags: Option<HashMap<String, String>>) -> Self {
		Self { value: BigDecimal::from_str(value).unwrap(), timestamp, tags }
	}
}

impl IntoBucketValue for TimeSeriesMeasurement {
	fn into_bucket_value(self) -> BucketValue {
		super::BucketValue::TimeSeries(self)
	}
}

impl Ord for TimeSeriesMeasurement {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		self.timestamp.cmp(&other.timestamp)
	}
}

impl PartialOrd for TimeSeriesMeasurement {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
		Some(self.cmp(other))
	}
}

impl Hash for TimeSeriesMeasurement {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		self.timestamp.hash(state);
	}
}
