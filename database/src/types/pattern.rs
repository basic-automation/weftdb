use std::fmt::Display;

use bigdecimal::{BigDecimal, FromPrimitive, Zero};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Occurrence, Relative};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PatternID(Uuid);

impl PatternID {
	#[must_use]
	pub fn new() -> Self {
		Self(Uuid::new_v4())
	}

	#[must_use]
	pub const fn from_uuid(uuid: Uuid) -> Self {
		Self(uuid)
	}

	#[must_use]
	pub const fn as_uuid(&self) -> Uuid {
		self.0
	}

	/// Create a `PatternID` from a string representation
	///
	/// # Errors
	/// Returns an error if the string is not a valid UUID
	pub fn from_string(s: &str) -> Result<Self, uuid::Error> {
		Uuid::parse_str(s).map(Self)
	}
}

impl Default for PatternID {
	fn default() -> Self {
		Self::new()
	}
}

impl Display for PatternID {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pattern {
	id: PatternID,
	occurrences: Vec<Occurrence>,
	relatives: Vec<Relative>,
	amplitudes: Vec<BigDecimal>,
	sum: BigDecimal,
	abs_sum: BigDecimal,
	max: BigDecimal,
	min: BigDecimal,
	abs_max: BigDecimal,
	avg: BigDecimal,
	abs_avg: BigDecimal,
}

impl Pattern {
	#[must_use]
	pub fn new(id: PatternID, occurrences: Vec<Occurrence>, relatives: Vec<Relative>) -> Self {
		let mut self_ = Self { id, occurrences, relatives, amplitudes: Vec::new(), sum: BigDecimal::zero(), abs_sum: BigDecimal::zero(), max: BigDecimal::zero(), min: BigDecimal::zero(), abs_max: BigDecimal::zero(), avg: BigDecimal::zero(), abs_avg: BigDecimal::zero() };
		self_.compute_precomputed();
		self_
	}

	fn compute_precomputed(&mut self) {
		self.amplitudes = self.relatives.iter().map(|r| r.vector().amplitude().clone()).collect();

		if self.amplitudes.is_empty() {
			return;
		}

		self.sum = self.amplitudes.iter().fold(BigDecimal::zero(), |acc, amp| acc + amp);
		self.abs_sum = self.amplitudes.iter().fold(BigDecimal::zero(), |acc, amp| acc + amp.abs());

		self.max = self.amplitudes.iter().cloned().max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)).unwrap_or_else(BigDecimal::zero);
		self.min = self.amplitudes.iter().cloned().min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)).unwrap_or_else(BigDecimal::zero);

		self.abs_max = self.amplitudes.iter().map(bigdecimal::BigDecimal::abs).max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)).unwrap_or_else(BigDecimal::zero);

		let len = BigDecimal::from_usize(self.amplitudes.len()).unwrap();
		self.avg = &self.sum / &len;
		self.abs_avg = &self.abs_sum / &len;
	}

	#[must_use]
	pub const fn id(&self) -> PatternID {
		self.id
	}

	#[must_use]
	pub const fn occurrences(&self) -> &Vec<Occurrence> {
		&self.occurrences
	}

	#[must_use]
	pub const fn relatives(&self) -> &Vec<Relative> {
		&self.relatives
	}

	pub fn add_occurrence(&mut self, occurrence: Occurrence) {
		self.occurrences.push(occurrence);
		// Note: adding occurrence doesn't affect precomputed, as they are based on relatives
	}

	// Add getters for precomputed if needed
	#[must_use]
	pub const fn amplitudes(&self) -> &Vec<BigDecimal> {
		&self.amplitudes
	}

	#[must_use]
	pub const fn sum(&self) -> &BigDecimal {
		&self.sum
	}

	#[must_use]
	pub const fn abs_sum(&self) -> &BigDecimal {
		&self.abs_sum
	}

	#[must_use]
	pub const fn max(&self) -> &BigDecimal {
		&self.max
	}

	#[must_use]
	pub const fn min(&self) -> &BigDecimal {
		&self.min
	}

	#[must_use]
	pub const fn abs_max(&self) -> &BigDecimal {
		&self.abs_max
	}

	#[must_use]
	pub const fn avg(&self) -> &BigDecimal {
		&self.avg
	}

	#[must_use]
	pub const fn abs_avg(&self) -> &BigDecimal {
		&self.abs_avg
	}
}
