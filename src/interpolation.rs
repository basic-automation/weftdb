use crate::length::ToSeconds;
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
	fn to_seconds(&self) -> u64 {
		match self {
			Interpolation::None => 0,
			Interpolation::Second => 1,
			Interpolation::Minute => 60,
			Interpolation::Hour => 3600,
			Interpolation::Day => 86400,
		}
	}
}
