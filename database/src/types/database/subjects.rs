use anyhow::{bail, Result};
use turso::{params, Builder};

use crate::{Database, Error, Subject, SubjectId, DATABASES};

impl Database {
	/// Track a new subject in the database.
	/// # Errors
	/// - if database not found
	/// - if unable to create subject database
	/// - if unable to insert subject metadata
	/// # Returns
	/// A `Subject` instance representing the newly created subject.
	///
	pub async fn track_subject(&self, name: &str) -> Result<Subject> {
		// Get database path and metadata turso_db with early drop
		let (db_path, metadata_turso_db) = {
			let databases = DATABASES.lock().await.clone();
			let Some(db_info) = databases.get(&self.id()) else { bail!(Error::DatabaseError("Database not found".to_string())) };
			(db_info.path().to_string(), db_info.metadata_turso_db().cloned())
		};

		// Ensure the database directory exists
		match std::fs::create_dir_all(&db_path) {
			Ok(()) => (),
			Err(e) => bail!(Error::DatabaseError(format!("Failed to create database directory: {e}"))),
		}

		// Create subject database file
		let subject_db_path = format!("{db_path}/{name}.db");

		// Create the Turso database
		let subject_turso_db = Builder::new_local(&subject_db_path).build().await.map_err(|e| Error::DatabaseError(format!("Failed to create subject database: {e}")))?;

		let subject_id = SubjectId::new();

		// Insert into merged subjects table
		if let Some(ref metadata_turso_db) = metadata_turso_db {
			let conn = metadata_turso_db.connect()?;
			conn.execute("INSERT INTO subjects (id, database_id, name, created_at) VALUES (?, ?, ?, ?)", params![subject_id.as_uuid().to_string(), self.id().as_uuid().to_string(), name.to_string(), chrono::Utc::now().timestamp_millis().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to insert subject: {e}")))?;
		}

		let subject = Subject::new_with_id(subject_id, name.to_string(), self.id(), subject_turso_db);

		// Add subject to database
		match DATABASES.lock().await.get_mut(&self.id()) {
			Some(db_info) => {
				db_info.add_subject(subject.clone());
			}
			None => bail!(Error::DatabaseError("Database not found".to_string())),
		}

		Ok(subject)
	}

	// Removed duplicate get_subject method - it's now in mod.rs as get_subject
	// The mod.rs version returns Result<Option<Subject>> for proper error handling
}
