use bigdecimal::{BigDecimal, FromPrimitive, Zero};
pub use occurrence::Occurrence;
pub use pattern_id::PatternID;
use serde::{Deserialize, Serialize};

use crate::Relative;

mod occurrence;
mod pattern_id;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pattern {
	id: PatternID,
	occurrences: Vec<Occurrence>,
	relatives: Vec<Relative>,
	// Precomputed for optimization
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

		self.abs_max = self.amplitudes.iter().map(|amp| amp.abs()).max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)).unwrap_or_else(BigDecimal::zero);

		let len = BigDecimal::from_usize(self.amplitudes.len()).unwrap();
		self.avg = &self.sum / &len;
		self.abs_avg = &self.abs_sum / &len;
	}

	pub const fn id(&self) -> PatternID {
		self.id
	}

	pub fn occurrences(&self) -> &Vec<Occurrence> {
		&self.occurrences
	}

	pub fn relatives(&self) -> &Vec<Relative> {
		&self.relatives
	}

	pub fn add_occurrence(&mut self, occurrence: Occurrence) {
		self.occurrences.push(occurrence);
		// Note: adding occurrence doesn't affect precomputed, as they are based on relatives
	}

	// Add getters for precomputed if needed
	pub fn amplitudes(&self) -> &Vec<BigDecimal> {
		&self.amplitudes
	}

	pub fn sum(&self) -> &BigDecimal {
		&self.sum
	}

	pub fn abs_sum(&self) -> &BigDecimal {
		&self.abs_sum
	}

	pub fn max(&self) -> &BigDecimal {
		&self.max
	}

	pub fn min(&self) -> &BigDecimal {
		&self.min
	}

	pub fn abs_max(&self) -> &BigDecimal {
		&self.abs_max
	}

	pub fn avg(&self) -> &BigDecimal {
		&self.avg
	}

	pub fn abs_avg(&self) -> &BigDecimal {
		&self.abs_avg
	}
}
