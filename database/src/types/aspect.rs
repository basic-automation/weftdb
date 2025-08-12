use splimes::Resolution;
use uuid::Uuid;

use crate::SubjectId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AspectId(Uuid);

impl AspectId {
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
}

impl Default for AspectId {
	fn default() -> Self {
		Self::new()
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Aspect {
	id: AspectId,
	name: String,
	subject_id: SubjectId,
	table_name: String,
	resolution: Resolution,
}

impl Aspect {
	#[must_use]
	pub fn new(name: String, subject_id: SubjectId, table_name: String, resolution: Resolution) -> Self {
		let id = AspectId::new();
		Self { id, name, subject_id, table_name, resolution }
	}

	#[must_use]
	pub const fn new_with_id(id: AspectId, name: String, subject_id: SubjectId, table_name: String, resolution: Resolution) -> Self {
		Self { id, name, subject_id, table_name, resolution }
	}

	#[must_use]
	pub const fn id(&self) -> AspectId {
		self.id
	}

	#[must_use]
	pub fn name(&self) -> &str {
		&self.name
	}

	#[must_use]
	pub const fn subject_id(&self) -> SubjectId {
		self.subject_id
	}

	#[must_use]
	pub fn table_name(&self) -> &str {
		&self.table_name
	}

	#[must_use]
	pub const fn resolution(&self) -> Resolution {
		self.resolution
	}
}
