use anyhow::{bail, Result};

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
		// Get database path and metadata pool with early drop
		let (db_path, metadata_pool) = {
			let databases = DATABASES.lock().await.clone();
			let Some(db_info) = databases.get(&self.id()) else { bail!(Error::DatabaseError("Database not found".to_string())) };
			(db_info.path().to_string(), db_info.metadata_pool().cloned())
		};

		// Ensure the database directory exists
		match std::fs::create_dir_all(&db_path) {
			Ok(()) => (),
			Err(e) => bail!(Error::DatabaseError(format!("Failed to create database directory: {e}"))),
		}

		// Create subject database file
		let subject_db_path = format!("{db_path}/{name}.db");

		// Create the database file and connect
		let pool = match sqlx::sqlite::SqlitePoolOptions::new().max_connections(5).connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&subject_db_path).create_if_missing(true)).await {
			Ok(pool) => pool,
			Err(e) => bail!(Error::DatabaseError(format!("Failed to connect to subject database: {e}"))),
		};

		let subject_id = SubjectId::new();

		// Insert subject metadata if metadata pool exists - using UUID directly
		if let Some(ref metadata_pool) = metadata_pool {
			match sqlx::query("INSERT INTO subject_metadata (id, database_id, name, created_at) VALUES (?, ?, ?, ?)").bind(subject_id.as_uuid()).bind(self.id().as_uuid()).bind(name).bind(chrono::Utc::now().timestamp_millis()).execute(metadata_pool).await {
				Ok(_) => (),
				Err(e) => bail!(Error::DatabaseError(format!("Failed to insert subject metadata: {e}"))),
			}
		}

		let subject = Subject::new_with_id(subject_id, name.to_string(), self.id(), pool);

		// Add subject to database
		match DATABASES.lock().await.get_mut(&self.id()) {
			Some(db_info) => {
				db_info.add_subject(subject.clone());
			}
			None => bail!(Error::DatabaseError("Database not found".to_string())),
		}

		Ok(subject)
	}

	pub async fn get_subject(&self, subject_id: &SubjectId) -> Option<Subject> {
		let databases = DATABASES.lock().await;
		databases.get(&self.id()).and_then(|db_info| db_info.subjects().get(subject_id).cloned())
	}
}
