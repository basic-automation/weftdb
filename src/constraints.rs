use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter, Result};
use std::str::FromStr;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Constraints {
	pub steps: Steps,
	pub variability: Variability,
	pub occurrences: Occurrences,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(untagged)]
pub enum Interpolation {
	#[default]
	Linear,
}

impl Display for Interpolation {
	fn fmt(&self, f: &mut Formatter<'_>) -> Result {
		match self {
			Interpolation::Linear => write!(f, "Linear"),
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Steps {
	pub is_enforced: bool,
	pub interpolation: Interpolation,
	pub count: u64,
}

impl Default for Steps {
	fn default() -> Self {
		Steps { is_enforced: true, interpolation: Interpolation::Linear, count: 10 }
	}
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Variability {
	pub static_variability: StaticVariability,
	pub absolute_static_variability: AbsoluteStaticVariability,
	pub percentage_variability: PercentageVariability,
	pub absolute_percentage_variability: AbsolutePercentageVariability,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum VariabilityType {
	#[default]
	MaxVariability,
	AverageVariability,
	SumVariability,
}

/// When comparing two `Patterns` the `Amplitudes` will be compared individually.
/// * If `Max-Vairability` type is set, the two patterns will be deemed the same if the difference between any of the congruent `Amplitudes` is less than the `StaticVariability.Value`.
/// * If `Average-Variability` type is set, the two patterns will be deemed the same if the average of all of the differences between the congruent `Amplitudes` is less than `StaticVariability.Value`.
/// * If `Sum-Variability` type is set, the two patterns will be deemed the same if their sums vary less than the `StaicSumVariability.Value`.'
/// enforced: Is `Static Variability` enforced on the dictionary?
/// value: The maximum allowable differential.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticVariability {
	pub enforced: bool,
	#[serde(rename = "type")]
	pub type_: VariabilityType,
	pub value: BigDecimal,
}

impl Default for StaticVariability {
	fn default() -> Self {
		StaticVariability { enforced: false, type_: VariabilityType::SumVariability, value: BigDecimal::from(4) }
	}
}

/// The same as `Static Variability` but uses the absolute value of the `Amplitudes` when making comparisons.
/// enforced: Is `Absolute Static Variability` enforced on the dictionary?
/// value: The maximum allowable differential.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbsoluteStaticVariability {
	pub enforced: bool,
	#[serde(rename = "type")]
	pub type_: VariabilityType,
	pub value: BigDecimal,
}

impl Default for AbsoluteStaticVariability {
	fn default() -> Self {
		AbsoluteStaticVariability { enforced: false, type_: VariabilityType::SumVariability, value: BigDecimal::from(2) }
	}
}

/// Similar to `Static Variability` but the `Value` property is treated as a decimal percentage instead of a static value.
/// enforced: Is `Percentage Variability` enforced on the dictionary?
/// value: The maximum allowable differential. Must be between 0 and 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PercentageVariability {
	pub enforced: bool,
	#[serde(rename = "type")]
	pub type_: VariabilityType,
	pub value: BigDecimal,
}

impl Default for PercentageVariability {
	fn default() -> Self {
		PercentageVariability { enforced: true, type_: VariabilityType::AverageVariability, value: BigDecimal::from_str(".5").unwrap() }
	}
}

/// Similar to `Absolute Static Variability` but the `Value` property is treated as a decimal percentage instead of a static value.
/// enforced: Is `Absolute Percentage Variability` enforced on the dictionary?
/// value: The maximum allowable differential. Must be between 0 and 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbsolutePercentageVariability {
	pub enforced: bool,
	#[serde(rename = "type")]
	pub type_: VariabilityType,
	pub value: BigDecimal,
}

impl Default for AbsolutePercentageVariability {
	fn default() -> Self {
		AbsolutePercentageVariability { enforced: false, type_: VariabilityType::MaxVariability, value: BigDecimal::from_str("1.9").unwrap() }
	}
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Occurrences {
	pub distance: OccurrenceDistance,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OccurrenceDistance {
	pub enforced: bool,
	pub value: BigDecimal,
}

impl Default for OccurrenceDistance {
	fn default() -> Self {
		OccurrenceDistance { enforced: true, value: BigDecimal::from(1) }
	}
}
