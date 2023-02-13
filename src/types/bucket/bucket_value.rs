use super::ObjectValue;
use super::TimeSeriesMeasurement;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BucketValue {
	TimeSeries(TimeSeriesMeasurement),
	Object(ObjectValue),
}

pub trait IntoBucketValue {
	fn into_bucket_value(self) -> BucketValue;
}
