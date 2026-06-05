use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Occurrence {
	pub source: String,
	pub dataset_name: String,
        pub length: String,
	pub start: BigDecimal,
	pub end: BigDecimal,
	pub id: Uuid,
}

impl Occurrence {
	pub fn new(source: &str, dataset_name: String, length: String, start: BigDecimal, end: BigDecimal) -> Self {
		Self { source: source.to_string(), dataset_name, length, start, end, id: Uuid::new_v4() }
	}
}
