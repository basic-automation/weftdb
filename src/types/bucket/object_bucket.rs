use super::{BucketValue, IntoBucketValue};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::hash::Hash;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectValue {
	pub value: Value,
	pub key: String,
}

impl ObjectValue {
	pub fn new(value: String, key: &str) -> Self {
		let value = json!(value);
		Self { value, key: key.to_string() }
	}
}

impl IntoBucketValue for ObjectValue {
	fn into_bucket_value(self) -> BucketValue {
		BucketValue::Object(self)
	}
}

impl Hash for ObjectValue {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		self.key.hash(state);
	}
}

impl PartialOrd for ObjectValue {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
		self.key.partial_cmp(&other.key)
	}
}

impl Ord for ObjectValue {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		self.key.cmp(&other.key)
	}
}
