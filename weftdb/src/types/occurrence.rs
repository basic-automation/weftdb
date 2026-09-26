use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use splimes::Resolution;

use crate::{AspectId, DatabaseInfo, PatternID};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occurrence {
	aspect: AspectId,
	resolution: Resolution,
	size: usize,
	database_info: DatabaseInfo,
	pattern_id: PatternID,
	beginning: DateTime<Utc>,
	end: DateTime<Utc>,
}

impl Occurrence {
	#[must_use]
	pub const fn new(aspect: AspectId, resolution: Resolution, size: usize, database_info: DatabaseInfo, pattern_id: PatternID, beginning: DateTime<Utc>, end: DateTime<Utc>) -> Self {
		Self { aspect, resolution, size, database_info, pattern_id, beginning, end }
	}

	/// Get the aspect ID
	#[must_use]
	pub const fn aspect(&self) -> &AspectId {
		&self.aspect
	}

	/// Set the aspect ID
	pub const fn set_aspect(&mut self, aspect: AspectId) {
		self.aspect = aspect;
	}

	/// Get the resolution
	#[must_use]
	pub const fn resolution(&self) -> &Resolution {
		&self.resolution
	}

	/// Set the resolution
	pub const fn set_resolution(&mut self, resolution: Resolution) {
		self.resolution = resolution;
	}

	/// Get the size
	#[must_use]
	pub const fn size(&self) -> usize {
		self.size
	}

	/// Set the size
	pub const fn set_size(&mut self, size: usize) {
		self.size = size;
	}

	/// Get the database info
	#[must_use]
	pub const fn database_info(&self) -> &DatabaseInfo {
		&self.database_info
	}

	/// Set the database info
	pub fn set_database_info(&mut self, database_info: DatabaseInfo) {
		self.database_info = database_info;
	}

	/// Get the pattern ID
	#[must_use]
	pub const fn pattern_id(&self) -> &PatternID {
		&self.pattern_id
	}

	/// Set the pattern ID
	pub const fn set_pattern_id(&mut self, pattern_id: PatternID) {
		self.pattern_id = pattern_id;
	}

	/// Get the beginning timestamp
	#[must_use]
	pub const fn beginning(&self) -> &DateTime<Utc> {
		&self.beginning
	}

	/// Set the beginning timestamp
	pub const fn set_beginning(&mut self, beginning: DateTime<Utc>) {
		self.beginning = beginning;
	}

	/// Get the end timestamp
	#[must_use]
	pub const fn end(&self) -> &DateTime<Utc> {
		&self.end
	}

	/// Set the end timestamp
	pub const fn set_end(&mut self, end: DateTime<Utc>) {
		self.end = end;
	}
}
