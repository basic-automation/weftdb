use crate::types::Interpolation;
use serde::{Deserialize, Serialize};
use bigdecimal::BigDecimal;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub struct BucketParams {
	pub debug: Option<bool>,
	pub range: Option<Vec<String>>,

	/// The interpolation method to use when querying a timeseries bucket.
	pub interpolation: Option<Interpolation>,

	/// The number of steps to use when interpolating a timeseries bucket.
	pub steps: Option<BigDecimal>,

	/// The maximum number of items to return when querying a bucket.
	pub count: Option<usize>,

	// page number for paginated results
	pub page: Option<usize>,
}
