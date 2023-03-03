use crate::types::Interpolation;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub struct BucketParams {
	pub debug: Option<bool>,
	pub range: Option<Vec<i64>>,
	pub interpolation: Option<Interpolation>,
	pub steps: Option<u64>,
}
