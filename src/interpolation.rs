use crate::length::ToSeconds;
use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Copy, Eq)]
pub enum Interpolation {
	None,
	Second,
	Minute,
	Hour,
	Day,
}

pub trait ToInterpolation {
	fn to_interpolation(&self) -> Interpolation;
}

impl ToSeconds for Interpolation {
	fn to_seconds(&self) -> BigDecimal {
		match self {
			Interpolation::None => BigDecimal::from(0),
			Interpolation::Second => BigDecimal::from(1),
			Interpolation::Minute => BigDecimal::from(60),
			Interpolation::Hour => BigDecimal::from(3600),
			Interpolation::Day => BigDecimal::from(86400),
		}
	}
}
