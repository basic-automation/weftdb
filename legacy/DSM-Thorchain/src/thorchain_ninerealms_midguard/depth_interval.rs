use core::fmt::{Display, Formatter};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DepthInterval {
	FiveMinute,
	Hour,
	Day,
	Week,
	Month,
	Quarter,
	Year,
}

impl Display for DepthInterval {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		match self {
			DepthInterval::FiveMinute => write!(f, "5min"),
			DepthInterval::Hour => write!(f, "hour"),
			DepthInterval::Day => write!(f, "day"),
			DepthInterval::Week => write!(f, "week"),
			DepthInterval::Month => write!(f, "month"),
			DepthInterval::Quarter => write!(f, "quarter"),
			DepthInterval::Year => write!(f, "year"),
		}
	}
}

pub trait ToDepthInterval {
	fn to_depth_interval(&self) -> DepthInterval;
}
