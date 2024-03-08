use bigdecimal::BigDecimal;
use dsm_config::{BatchLength, ToMilliseconds};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Copy, Eq)]
pub enum Interpolation {
	None,
	Millisecond,
	Second,
	Minute,
	Hour,
	Day,
	Week,
}

impl Interpolation {
	pub fn from_batch_length(length: BatchLength) -> Interpolation {
		match length {
			BatchLength::Millisecond => Interpolation::Millisecond,
			BatchLength::Second => Interpolation::Millisecond,
			BatchLength::ThirtySecond => Interpolation::Second,
			BatchLength::Minute => Interpolation::Second,
			BatchLength::FiveMinute => Interpolation::Second,
			BatchLength::TenMinute => Interpolation::Second,
			BatchLength::FifteenMinute => Interpolation::Minute,
			BatchLength::ThirtyMinute => Interpolation::Minute,
			BatchLength::Hour => Interpolation::Minute,
			BatchLength::ThreeHour => Interpolation::Minute,
			BatchLength::SixHour => Interpolation::Minute,
			BatchLength::TwelveHour => Interpolation::Hour,
			BatchLength::Day => Interpolation::Hour,
			BatchLength::ThreeDay => Interpolation::Hour,
			BatchLength::Week => Interpolation::Hour,
			BatchLength::TwoWeek => Interpolation::Day,
			BatchLength::Month => Interpolation::Day,
			BatchLength::TwoMonth => Interpolation::Day,
			BatchLength::ThreeMonth => Interpolation::Week,
			BatchLength::SixMonth => Interpolation::Week,
			BatchLength::Year => Interpolation::Week,
		}
	}
}

impl ToMilliseconds for Interpolation {
	fn to_milliseconds(&self) -> BigDecimal {
		match self {
			Interpolation::None => BigDecimal::from(0),
			Interpolation::Millisecond => BigDecimal::from(1),
			Interpolation::Second => BigDecimal::from(1000),
			Interpolation::Minute => BigDecimal::from(60000),
			Interpolation::Hour => BigDecimal::from(3600000),
			Interpolation::Day => BigDecimal::from(86400000),
			Interpolation::Week => BigDecimal::from(604800000),
		}
	}
}

pub trait ToInterpolation {
	fn to_interpolation(&self) -> Interpolation;
}
