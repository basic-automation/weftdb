#![allow(dead_code)]
use crate::influxdb2::*;
use core::fmt::Display;
use core::str::FromStr;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub enum BatchLength {
	#[default]
	Minute,
	FiveMinute,
	TenMinute,
	FifteenMinute,
	ThirtyMinute,
	Hour,
	ThreeHour,
	SixHour,
	TwelveHour,
	Day,
	ThreeDay,
	Week,
	TwoWeek,
	Month,
	TwoMonth,
	ThreeMonth,
	SixMonth,
	Year,
}

impl Display for BatchLength {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			BatchLength::Minute => write!(f, "60"),
			BatchLength::FiveMinute => write!(f, "300"),
			BatchLength::TenMinute => write!(f, "600"),
			BatchLength::FifteenMinute => write!(f, "900"),
			BatchLength::ThirtyMinute => write!(f, "1800"),
			BatchLength::Hour => write!(f, "3600"),
			BatchLength::ThreeHour => write!(f, "10800"),
			BatchLength::SixHour => write!(f, "21600"),
			BatchLength::TwelveHour => write!(f, "43200"),
			BatchLength::Day => write!(f, "86400"),
			BatchLength::ThreeDay => write!(f, "259200"),
			BatchLength::Week => write!(f, "604800"),
			BatchLength::TwoWeek => write!(f, "1209600"),
			BatchLength::Month => write!(f, "2419200"),
			BatchLength::TwoMonth => write!(f, "4838400"),
			BatchLength::ThreeMonth => write!(f, "7257600"),
			BatchLength::SixMonth => write!(f, "14515200"),
			BatchLength::Year => write!(f, "29030400"),
		}
	}
}

impl FromStr for BatchLength {
	type Err = ();

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		match s {
			"1m" => Ok(BatchLength::Minute),
			"5m" => Ok(BatchLength::FiveMinute),
			"10m" => Ok(BatchLength::TenMinute),
			"15m" => Ok(BatchLength::FifteenMinute),
			"30m" => Ok(BatchLength::ThirtyMinute),
			"1h" => Ok(BatchLength::Hour),
			"3h" => Ok(BatchLength::ThreeHour),
			"6h" => Ok(BatchLength::SixHour),
			"12h" => Ok(BatchLength::TwelveHour),
			"1d" => Ok(BatchLength::Day),
			"3d" => Ok(BatchLength::ThreeDay),
			"1w" => Ok(BatchLength::Week),
			"2w" => Ok(BatchLength::TwoWeek),
			"1M" => Ok(BatchLength::Month),
			"2M" => Ok(BatchLength::TwoMonth),
			"3M" => Ok(BatchLength::ThreeMonth),
			"6M" => Ok(BatchLength::SixMonth),
			"1y" => Ok(BatchLength::Year),
			_ => Err(()),
		}
	}
}

impl ToSeconds for BatchLength {
	fn to_seconds(&self) -> u64 {
		match self {
			BatchLength::Minute => 60,
			BatchLength::FiveMinute => 300,
			BatchLength::TenMinute => 600,
			BatchLength::FifteenMinute => 900,
			BatchLength::ThirtyMinute => 1800,
			BatchLength::Hour => 3600,
			BatchLength::ThreeHour => 10800,
			BatchLength::SixHour => 21600,
			BatchLength::TwelveHour => 43200,
			BatchLength::Day => 86400,
			BatchLength::ThreeDay => 259200,
			BatchLength::Week => 604800,
			BatchLength::TwoWeek => 1209600,
			BatchLength::Month => 2419200,
			BatchLength::TwoMonth => 4838400,
			BatchLength::ThreeMonth => 7257600,
			BatchLength::SixMonth => 15724800,
			BatchLength::Year => 31449600,
		}
	}
}

pub trait ToSeconds {
	fn to_seconds(&self) -> u64;
}

impl ToFluxInterpolation for BatchLength {
	fn to_interpolation(&self) -> FluxInterpolation {
		match self {
			BatchLength::Minute => FluxInterpolation::Second,
			BatchLength::FiveMinute => FluxInterpolation::Second,
			BatchLength::TenMinute => FluxInterpolation::Second,
			BatchLength::FifteenMinute => FluxInterpolation::Second,
			BatchLength::ThirtyMinute => FluxInterpolation::Second,
			BatchLength::Hour => FluxInterpolation::Second,
			BatchLength::ThreeHour => FluxInterpolation::Minute,
			BatchLength::SixHour => FluxInterpolation::Minute,
			BatchLength::TwelveHour => FluxInterpolation::Minute,
			BatchLength::Day => FluxInterpolation::Minute,
			BatchLength::ThreeDay => FluxInterpolation::Hour,
			BatchLength::Week => FluxInterpolation::Hour,
			BatchLength::TwoWeek => FluxInterpolation::Hour,
			BatchLength::Month => FluxInterpolation::Hour,
			BatchLength::TwoMonth => FluxInterpolation::Day,
			BatchLength::ThreeMonth => FluxInterpolation::Day,
			BatchLength::SixMonth => FluxInterpolation::Day,
			BatchLength::Year => FluxInterpolation::Day,
		}
	}
}
