use std::{collections::HashMap, path::PathBuf};

use anyhow::{bail, Result};

use super::creation;
use crate::{
	types::database::traits::{config::Config, database_structure::DatabaseStructure}, Aspect, Database, DatabaseId, Error, SubjectId, DATABASES
};

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
			result.insert(*db_id, db_info.name.clone());
		}
		result
	}

	/// Lists the databases stored in the data directory: every folder that holds a
	/// `metadata.db`, by name, sorted. Unlike [`list_databases`](Self::list_databases),
	/// which lists the databases opened in this process, this reads the disk.
	///
	/// It first removes the build directories that interrupted [`Database::new`] calls
	/// left behind (`.{name}.creating-*`, crash-consistency design S18), and it never lists
	/// a build directory, not even one a running `new` still owns. A missing data directory
	/// holds no databases.
	///
	/// # Errors
	/// - if the data directory exists but cannot be read
	pub async fn list_stored_databases() -> Result<Vec<String>> {
		let data_dir = PathBuf::from(<Self as Config>::get_data_dir());
		// Best-effort: a leftover build directory is skipped below either way.
		if let Err(e) = creation::sweep_stale_build_dirs(&data_dir).await {
			if e.kind() != std::io::ErrorKind::NotFound {
				tracing::warn!("Could not sweep stale database build directories in {}: {e}", data_dir.display());
			}
		}
		let entries = match std::fs::read_dir(&data_dir) {
			Ok(entries) => entries,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
			Err(e) => return Err(e.into()),
		};
		let mut names = Vec::new();
		for entry in entries {
			let entry = entry?;
			let Ok(name) = entry.file_name().into_string() else { continue };
			if !creation::is_build_dir_name(&name) && entry.path().join("metadata.db").exists() {
				names.push(name);
			}
		}
		names.sort();
		Ok(names)
	}

	/// Lists all subjects in a database - returns `HashMap`<`SubjectId`, String> for navigation
	/// # Errors
	/// - if database not found
	/// - if unable to read database subjects
	pub async fn list_subjects(&self) -> Result<HashMap<SubjectId, String>> {
		let mut result = HashMap::new();

		let db_info = match DATABASES.lock().await.get(&self.id()) {
			Some(info) => info.clone(),
			None => bail!(Error::DatabaseError("Database not found".to_string())),
		};

		for (subject_id, subject) in &db_info.subjects {
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

		let subject = db_info.subjects.get(subject_id).ok_or_else(|| Error::DatabaseError("Subject not found".to_string()))?;

		Ok(subject.aspects().values().cloned().collect())
	}

	/// Find databases by name pattern
	pub async fn find_databases_by_name(pattern: &str) -> Vec<(DatabaseId, String)> {
		let databases = DATABASES.lock().await;
		let results: Vec<_> = databases.iter().filter(|(_, db_info)| db_info.name.contains(pattern)).map(|(db_id, db_info)| (*db_id, db_info.name.clone())).collect();
		drop(databases);
		results
	}

	/// Get database by name
	pub async fn find_database_by_name(name: &str) -> Option<DatabaseId> {
		let databases = DATABASES.lock().await;

		databases.iter().find(|(_, db_info)| db_info.name == name).map(|(db_id, _)| *db_id)
	}

	/// Get all databases with their basic info
	pub async fn get_all_database_info() -> Vec<(DatabaseId, String, String)> {
		let databases = DATABASES.lock().await;
		let results: Vec<_> = databases.iter().map(|(db_id, db_info)| (*db_id, db_info.name.clone(), db_info.path.clone())).collect();
		drop(databases);
		results
	}
}
