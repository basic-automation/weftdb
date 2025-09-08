use chrono::{DateTime, Utc};
use database::{AspectId, DatabaseInfo};
use serde::{Deserialize, Serialize};
use splimes::Resolution;

use crate::types::pattern::PatternID;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occurrence {
	pub aspect: AspectId,
	pub resolution: Resolution,
	pub size: usize,
	pub database_info: DatabaseInfo,
	pub pattern_id: PatternID,
	pub beginning: DateTime<Utc>,
	pub end: DateTime<Utc>,
}

impl Occurrence {
	pub fn new(aspect: AspectId, resolution: Resolution, size: usize, database_info: DatabaseInfo, pattern_id: PatternID, beginning: DateTime<Utc>, end: DateTime<Utc>) -> Self {
		Self { aspect, resolution, size, database_info, pattern_id, beginning, end }
	}
}
