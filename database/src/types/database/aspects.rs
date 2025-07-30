use anyhow::{bail, Result};

use crate::{Aspect, AspectId, Database, Error, Subject, DATABASES};

impl Database {
	/// Creates an instance of and initializes a new Aspect.
	/// Creates a new table in /{`data_dir}/{db_name}/{name}.db` file.
	/// Creates the necessary database structures.
	///
	/// # Errors
	/// - if subject not found
	/// - if unable to create database table or index
	pub async fn track_aspect(&self, subject: Subject, name: &str) -> Result<Aspect> {
		let aspect_id = AspectId::new();
		let table_name = Self::sanitize_table_name(name);

		// Get pool and metadata pool with proper scope management - extract immediately
		let db_id = self.id();
		let db_info = self.get_database_info().await.ok_or_else(|| Error::DatabaseError("Database not found".to_string()))?;
		let metadata_pool = db_info.metadata_pool().cloned().ok_or_else(|| Error::DatabaseError("Metadata pool not found".to_string()))?;
		let pool = subject.pool();

		// Create the aspect table
		let create_table_sql = format!("CREATE TABLE IF NOT EXISTS {table_name} (id TEXT PRIMARY KEY, timestamp INTEGER NOT NULL, value TEXT NOT NULL)");
		match sqlx::query(&create_table_sql).execute(pool).await {
			Ok(_) => (),
			Err(e) => bail!(Error::DatabaseError(format!("Failed to create aspect table: {e}"))),
		}

		// Create index for efficient time-based queries
		let create_index_sql = format!("CREATE INDEX IF NOT EXISTS idx_{table_name}_timestamp ON {table_name} (timestamp)");
		match sqlx::query(&create_index_sql).execute(pool).await {
			Ok(_) => (),
			Err(e) => bail!(Error::DatabaseError(format!("Failed to create index for aspect table: {e}"))),
		}

		// Insert aspect metadata - using UUID directly
		match sqlx::query("INSERT INTO aspect_metadata (id, subject_id, database_id, name, table_name, created_at) VALUES (?, ?, ?, ?, ?, ?)").bind(aspect_id.as_uuid()).bind(subject.id().as_uuid()).bind(db_id.as_uuid()).bind(name).bind(&table_name).bind(chrono::Utc::now().timestamp_millis()).execute(&metadata_pool).await {
			Ok(_) => (),
			Err(e) => bail!(Error::DatabaseError(format!("Failed to insert aspect metadata: {e}"))),
		}

		let aspect_info = Aspect::new_with_id(aspect_id, name.to_string(), subject.id(), table_name);

		// Reacquire lock only to insert the aspect
		{
			let mut databases = DATABASES.lock().await;
			match databases.values_mut().find_map(|db_info| {
				for subject_info in db_info.subjects_mut().values_mut() {
					if subject_info.id() == subject.id() {
						subject_info.add_aspect(aspect_info.clone());
						return Some(());
					}
				}
				None
			}) {
				Some(()) => (),
				None => bail!(Error::DatabaseError("Subject not found".to_string())),
			}
		}

		Ok(aspect_info)
	}

	pub async fn get_aspect(&self, aspect_id: &AspectId) -> Option<Aspect> {
		let db = self.get_database_info().await?;
		db.subjects().values().find_map(|subject_info| subject_info.aspects().get(aspect_id).cloned())
	}
}
