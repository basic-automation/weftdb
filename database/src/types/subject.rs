use std::{collections::HashMap, hash::Hash, path::Path};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
	types::database::traits::{aspect_structure::AspectStructure, DatabaseStructure}, Aspect, AspectId, Database, DatabaseId
};

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
	database_metadata_db_path: String,
	name: String,
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
	/// Creates a new Subject with the given parameters.
	///
	/// # Errors
	///
	/// Returns an error if the subject path cannot be determined or the directory cannot be created.
	pub async fn new(id: Option<SubjectId>, name: String, database_id: DatabaseId, database_metadata_db_path: String) -> Result<Self> {
		let id = id.unwrap_or_default();

		let subject_path = Self::get_subject_path(database_metadata_db_path.clone(), id).await?;

		// recursively create directory if it doesn't exist
		tokio::fs::create_dir_all(&subject_path).await?;

		Ok(Self { id, name, database_id, database_metadata_db_path, aspects: HashMap::new() })
	}

	/// Retrieves the database metadata path from the Turso database.
	///
	/// # Errors
	///
	/// Returns an error if the database connection fails or the metadata path cannot be retrieved.
	pub async fn get_database_metadata_path(turso_db_path: String) -> Result<String> {
		let turso_db = Database::get_turso_database(&turso_db_path).await?;

		// Query the database for the metadata_path field of the first item in the database table
		let conn = turso_db.connect()?;
		let mut rows = conn.query("SELECT metadata_path FROM database", turso::params![]).await?;
		let row = rows.next().await?.ok_or_else(|| anyhow::anyhow!("Database metadata not found"))?;
		let metadata_path: String = row.get(0)?;

		Ok(metadata_path)
	}

	async fn get_subject_name(turso_db: turso::Database, subject_id: SubjectId) -> Result<String> {
		// Query the database for the name field where id = subject_id in the subjects table
		let conn = turso_db.connect()?;
		let mut rows = conn.query("SELECT name FROM subjects WHERE id = ?", turso::params![subject_id.as_uuid().to_string()]).await?;
		let row = rows.next().await?.ok_or_else(|| anyhow::anyhow!("Subject not found"))?;
		let subject_name: String = row.get(0)?;

		Ok(subject_name)
	}

	async fn get_subject_path(database_metadata_db_path: String, subject_id: SubjectId) -> Result<String> {
		let database_metadata_path = Aspect::get_database_metadata_path(database_metadata_db_path.clone()).await?;
		let database_metadata_db = Database::get_turso_database(&database_metadata_path).await?;
		let subject_name = Self::get_subject_name(database_metadata_db, subject_id).await?;

		let subject_metadata_path = Path::new(&database_metadata_db_path).join(subject_name);

		if subject_metadata_path.exists() {
			Ok(subject_metadata_path.to_string_lossy().to_string())
		} else {
			bail!("Subject metadata path does not exist");
		}
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
	pub const fn aspects(&self) -> &HashMap<AspectId, Aspect> {
		&self.aspects
	}

	pub fn add_aspect(&mut self, aspect: Aspect) {
		self.aspects.insert(aspect.id(), aspect);
	}

	/// Get aspect by name
	#[must_use]
	pub fn get_aspect_by_name(&self, name: &str) -> Option<&Aspect> {
		self.aspects.values().find(|a| a.name() == name)
	}
}
