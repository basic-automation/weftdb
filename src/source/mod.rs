use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Source {
	pub name: String,
	pub url: String,
	pub interval: u64,
	pub numerator: String,
	pub denominator: String,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct SourceCollection(pub Vec<Source>);

impl SourceCollection {
	pub fn new() -> Self {
		Self(Vec::new())
	}

	pub fn add(&mut self, source: Source) {
		self.0.push(source);
	}
}
