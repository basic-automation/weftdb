use std::{collections::HashMap, hash::Hash};

use serde::{Deserialize, Serialize};
use turso::Database as TursoDatabase;
use uuid::Uuid;

use crate::{Aspect, AspectId, DatabaseId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SubjectId(Uuid);

impl SubjectId {
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

impl Default for SubjectId {
	fn default() -> Self {
		Self::new()
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subject {
	id: SubjectId,
	database_id: DatabaseId,
	name: String,
	#[serde(skip)]
	turso_db: Option<TursoDatabase>,
	aspects: HashMap<AspectId, Aspect>,
}

impl Hash for Subject {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		self.id.hash(state);
		self.database_id.hash(state);
		self.name.hash(state);
	}
}

impl PartialEq for Subject {
	fn eq(&self, other: &Self) -> bool {
		self.id == other.id && self.database_id == other.database_id && self.name == other.name && self.aspects == other.aspects
		// Skip turso_db comparison since it doesn't implement PartialEq
	}
}

impl Eq for Subject {}

impl Subject {
	#[must_use]
	pub fn new(name: String, database_id: DatabaseId, turso_db: TursoDatabase) -> Self {
		let id = SubjectId::new();
		Self { id, name, turso_db: Some(turso_db), database_id, aspects: HashMap::new() }
	}

	#[must_use]
	pub fn new_with_id(id: SubjectId, name: String, database_id: DatabaseId, turso_db: TursoDatabase) -> Self {
		Self { id, name, turso_db: Some(turso_db), database_id, aspects: HashMap::new() }
	}

	#[must_use]
	pub const fn id(&self) -> SubjectId {
		self.id
	}

	#[must_use]
	pub const fn database_id(&self) -> DatabaseId {
		self.database_id
	}

	#[must_use]
	pub fn name(&self) -> &str {
		&self.name
	}

	#[must_use]
	pub const fn turso_db(&self) -> Option<&TursoDatabase> {
		self.turso_db.as_ref()
	}

	#[must_use]
	pub const fn aspects(&self) -> &HashMap<AspectId, Aspect> {
		&self.aspects
	}

	pub fn add_aspect(&mut self, aspect: Aspect) {
		self.aspects.insert(aspect.id(), aspect);
	}
}
