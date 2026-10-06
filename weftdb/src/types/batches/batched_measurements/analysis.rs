use serde::{Deserialize, Serialize};

use crate::types::{Relative, Trend};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Analysis {
	trend: Option<Vec<Trend>>,
	relative: Option<Relative>,
}

impl Analysis {
	#[must_use]
	pub const fn new(trend: Option<Vec<Trend>>, relative: Option<Relative>) -> Self {
		Self { trend, relative }
	}

	#[must_use]
	pub const fn trend(&self) -> Option<&Vec<Trend>> {
		self.trend.as_ref()
	}

	#[must_use]
	pub fn get_trend(&self, index: usize) -> Option<&Trend> {
		self.trend.as_ref().and_then(|t| t.get(index))
	}

	#[must_use]
	pub fn get_slope(&self, index: usize) -> Option<&bigdecimal::BigDecimal> {
		self.get_trend(index).map(crate::types::Trend::slope)
	}

	pub fn set_trend(&mut self, trend: Option<Vec<Trend>>) {
		self.trend = trend;
	}

	#[must_use]
	pub const fn relative(&self) -> Option<&Relative> {
		self.relative.as_ref()
	}

	pub fn set_relative(&mut self, relative: Option<Relative>) {
		self.relative = relative;
	}

	pub fn remove_index(&mut self, index: usize) {
		if let Some(trend) = &mut self.trend {
			if index < trend.len() {
				trend.remove(index);
			}
		}
	}
}
