use crate::types::{Relative, Trend};

#[derive(Debug, Clone, PartialEq, Eq)]
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