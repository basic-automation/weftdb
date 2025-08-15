use std::collections::HashMap;

use anyhow::{bail, Result};

use crate::{Aspect, Database, DatabaseId, Error, SubjectId, DATABASES};

impl Database {
	/// Lists all databases in the system.
	/// Returns a map of `DatabaseId` to database name.
	/// # Errors
	/// - if unable to read database directory
	/// - if database directory is not set
	#[must_use]
	pub async fn list_databases() -> HashMap<DatabaseId, String> {
		let mut result = HashMap::new();
		for (db_id, db_info) in DATABASES.lock().await.iter() {
			result.insert(*db_id, db_info.name().to_string());
		}
		result
	}

	/// Lists all subjects in a database - returns HashMap<SubjectId, String> for navigation
	/// # Errors
	/// - if database not found
	/// - if unable to read database subjects
	pub async fn list_subjects(&self) -> Result<HashMap<SubjectId, String>> {
		let mut result = HashMap::new();

		let db_info = match DATABASES.lock().await.get(&self.id()) {
			Some(info) => info.clone(),
			None => bail!(Error::DatabaseError("Database not found".to_string())),
		};

		for (subject_id, subject) in db_info.subjects() {
			result.insert(*subject_id, subject.name().to_string());
		}

		Ok(result)
	}

	/// Gets all aspects for a subject in this database.
	/// # Errors
	/// - if database not found
	pub async fn get_subject_aspects(&self, subject_id: &SubjectId) -> Result<Vec<Aspect>> {
		let db_info = match DATABASES.lock().await.get(&self.id()) {
			Some(info) => info.clone(),
			None => bail!(Error::DatabaseError("Database not found".to_string())),
		};

		let subject = match db_info.subjects().get(subject_id) {
			Some(s) => s,
			None => bail!(Error::DatabaseError("Subject not found".to_string())),
		};

		Ok(subject.aspects().values().cloned().collect())
	}

	/// Find databases by name pattern
	pub async fn find_databases_by_name(pattern: &str) -> Vec<(DatabaseId, String)> {
		let mut results = Vec::new();
		let databases = DATABASES.lock().await;

		for (db_id, db_info) in databases.iter() {
			if db_info.name().contains(pattern) {
				results.push((*db_id, db_info.name().to_string()));
			}
		}

		results
	}

	/// Get database by name
	pub async fn find_database_by_name(name: &str) -> Option<DatabaseId> {
		let databases = DATABASES.lock().await;

		databases.iter().find(|(_, db_info)| db_info.name() == name).map(|(db_id, _)| *db_id)
	}

	/// Get all databases with their basic info
	pub async fn get_all_database_info() -> Vec<(DatabaseId, String, String)> {
		let mut results = Vec::new();
		let databases = DATABASES.lock().await;

		for (db_id, db_info) in databases.iter() {
			results.push((*db_id, db_info.name().to_string(), db_info.path().to_string()));
		}

		results
	}
}
