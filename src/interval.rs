use dsm_source::*;
use core::fmt::{Display, Formatter};

#[derive(Debug, Clone)]
pub enum Interval {
	FiveMinute,
	Hour,
	Day,
	Week,
	Month,
	Quarter,
	Year,
}

impl Display for Interval {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		match self {
			Interval::FiveMinute => write!(f, "5min"),
			Interval::Hour => write!(f, "hour"),
			Interval::Day => write!(f, "day"),
			Interval::Week => write!(f, "week"),
			Interval::Month => write!(f, "month"),
			Interval::Quarter => write!(f, "quarter"),
			Interval::Year => write!(f, "year"),
		}
	}
}

impl ToDepthInterval for Interval {
	fn to_depth_interval(&self) -> DepthInterval {
		match self {
			Interval::FiveMinute => DepthInterval::FiveMinute,
			Interval::Hour => DepthInterval::Hour,
			Interval::Day => DepthInterval::Day,
			Interval::Week => DepthInterval::Week,
			Interval::Month => DepthInterval::Month,
			Interval::Quarter => DepthInterval::Quarter,
			Interval::Year => DepthInterval::Year,
		}
	}
}
