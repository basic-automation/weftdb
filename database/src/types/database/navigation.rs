use std::collections::HashSet;

use anyhow::{bail, Result};

use crate::{Aspect, Database, Error, Subject, SubjectId, DATABASES};

impl Database {
	/// Lists all databases in the system.
	/// Returns a map of `DatabaseId` to database name.
	/// # Errors
	/// - if unable to read database directory
	/// - if database directory is not set
	///
	pub async fn ls() -> HashSet<Self> {
		let mut result = HashSet::new();
		for (db_id, db_info) in DATABASES.lock().await.iter() {
			result.insert(Self { id: *db_id, name: db_info.name().to_string() });
		}
		result
	}

	/// Lists all subjects in a database.
	/// # Errors
	/// - if database not found
	/// - if unable to read database subjects
	///
	#[allow(clippy::mutable_key_type)]
	pub async fn ls_subjects(&self) -> Result<HashSet<Subject>> {
		let mut result = HashSet::new();
		let db_info = match DATABASES.lock().await.get(&self.id()) {
			Some(info) => info.clone(),
			None => bail!(Error::DatabaseError("Database not found".to_string())),
		};
		for (subject_id, subject_info) in db_info.subjects() {
			result.insert(Subject::new_with_id(*subject_id, subject_info.name().to_string(), self.id(), subject_info.pool().clone()));
		}
		Ok(result)
	}

	/// Lists all aspects of a subject.
	/// # Errors
	/// - if database not found
	/// - if subject not found
	///
	pub async fn ls_aspects(&self, subject_id: SubjectId) -> Result<HashSet<Aspect>> {
		let db_info = match DATABASES.lock().await.get(&self.id()) {
			Some(info) => info.clone(),
			None => bail!(Error::DatabaseError("Database not found".to_string())),
		};
		let subject_info = match db_info.subjects().get(&subject_id) {
			Some(info) => info.clone(),
			None => bail!(Error::DatabaseError("Subject not found".to_string())),
		};
		let mut result = HashSet::new();
		for aspect_info in subject_info.aspects().values() {
			result.insert(aspect_info.clone());
		}
		Ok(result)
	}
}
