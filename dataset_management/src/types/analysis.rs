use crate::types::{Relative, Trend};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Analysis {
	trend: Option<Vec<Trend>>,
	relative: Option<Relative>,
}

impl Analysis {
	pub fn new(trend: Option<Vec<Trend>>, relative: Option<Relative>) -> Self {
		Self { trend, relative }
	}

	pub fn trend(&self) -> Option<&Vec<Trend>> {
		self.trend.as_ref()
	}

	pub fn get_trend(&self, index: usize) -> Option<&Trend> {
		self.trend.as_ref().and_then(|t| t.get(index))
	}

	pub fn get_slope(&self, index: usize) -> Option<&bigdecimal::BigDecimal> {
		self.get_trend(index).map(|t| t.slope())
	}

	pub fn set_trend(&mut self, trend: Option<Vec<Trend>>) {
		self.trend = trend;
	}

	pub fn relative(&self) -> Option<&Relative> {
		self.relative.as_ref()
	}

	pub fn set_relative(&mut self, relative: Option<Relative>) {
		self.relative = relative;
	}
}
