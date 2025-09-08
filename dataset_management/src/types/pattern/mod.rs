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
}

impl Pattern {
	pub fn new(id: PatternID, occurrences: Vec<Occurrence>, relatives: Vec<Relative>) -> Self {
		Self { id, occurrences, relatives }
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
	}
}
