use anyhow::Result;

use crate::{types::database::traits::database_structure::DatabaseStructure, AspectId, Database, Pattern, PatternID, DATABASES};

const DICTIONARY_CHUNK_SIZE: usize = 100;
use crate::database::Config;
use crate::types::database::traits::connection::Connection;

impl Database {
	/// Get or create a dictionary patterns database and ensure the table structure exists
	///
	/// # Errors
	/// - if unable to create database
	/// - if unable to create table structure
	async fn get_or_create_dictionary_patterns_database(dictionary_name: &str, db_path: &str) -> Result<turso::Database> {
		let dictionary_dir = format!("{db_path}/dictionaries/{dictionary_name}");
		std::fs::create_dir_all(&dictionary_dir).map_err(|e| anyhow::anyhow!(format!("Failed to create dictionary directory: {e}")))?;

		let patterns_db_path = format!("{dictionary_dir}/patterns.db");
		let patterns_turso_db = match Self::get_turso_database(&patterns_db_path).await {
			Ok(db) => db,
			Err(_) => Self::create_turso_database(&patterns_db_path).await?,
		};

		// Ensure the patterns table exists
		let conn = patterns_turso_db.connect()?;
		conn.execute(
			"CREATE TABLE IF NOT EXISTS patterns (
				id TEXT PRIMARY KEY,
				aspect_id TEXT NOT NULL,
				database_id TEXT NOT NULL,
				dictionary_name TEXT NOT NULL,
				occurrences TEXT NOT NULL,
				relatives TEXT NOT NULL,
				created_at INTEGER NOT NULL
			)",
			turso::params![],
		)
		.await
		.map_err(|e| anyhow::anyhow!(format!("Failed to create patterns table: {e}")))?;

		// Create index for efficient querying
		conn.execute("CREATE INDEX IF NOT EXISTS idx_patterns_dictionary ON patterns(dictionary_name, aspect_id)", turso::params![]).await.map_err(|e| anyhow::anyhow!(format!("Failed to create patterns index: {e}")))?;

		Ok(patterns_turso_db)
	}

	/// Get or create a dictionary metadata database and ensure the table structure exists
	///
	/// # Errors
	/// - if unable to create database
	/// - if unable to create table structure
	async fn get_or_create_dictionary_metadata_database(db_name: String) -> Result<turso::Database> {
		let metadata_db_path = Self::metadata_db_path(&db_name);
		let metadata_db = match Self::get_turso_database(&metadata_db_path).await {
			Ok(db) => db,
			Err(_) => Self::create_turso_database(&metadata_db_path).await?,
		};

		// Ensure the dictionaries table exists
		let conn = Self::begin_concurrent(&metadata_db, &metadata_db_path).await?;

		let res = conn
			.as_ref()
			.execute(
				"CREATE TABLE IF NOT EXISTS dictionaries (
        			name TEXT PRIMARY KEY,
        			description TEXT NOT NULL,
        			constraints TEXT NOT NULL,
        			created_at INTEGER NOT NULL,
        			updated_at INTEGER NOT NULL
        	        )",
				turso::params![],
			)
			.await;
		match res {
			Ok(_) => println!("Dictionaries table ensured successfully"),
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!(format!("Failed to create dictionaries table: {e}")));
			}
		}

		let _ = Database::commit_concurrent(&conn).await;
		Ok(metadata_db)
	}

	/// Store a pattern in a specific dictionary
	///
	/// # Errors
	/// - if database not found
	/// - if unable to insert pattern
	pub async fn store_pattern_in_dictionary(&self, pattern: &Pattern, dictionary_name: &str) -> Result<()> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| anyhow::anyhow!("Database not found".to_string()))?
		};

		let patterns_turso_db = Self::get_or_create_dictionary_patterns_database(dictionary_name, &db_info.path).await?;

		// Serialize the pattern occurrences and relatives
		let occurrences_json = serde_json::to_string(&pattern.occurrences()).map_err(|e| anyhow::anyhow!(format!("Failed to serialize pattern occurrences: {e}")))?;
		let relatives_json = serde_json::to_string(&pattern.relatives()).map_err(|e| anyhow::anyhow!(format!("Failed to serialize pattern relatives: {e}")))?;

		let pattern_id = pattern.id().to_string();
		let conn = patterns_turso_db.connect()?;

		// Insert pattern
		conn.execute("INSERT INTO patterns (id, aspect_id, database_id, dictionary_name, occurrences, relatives, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)", turso::params![pattern_id, pattern.occurrences()[0].database_info().id().as_uuid().to_string(), self.id().as_uuid().to_string(), dictionary_name, occurrences_json, relatives_json, chrono::Utc::now().timestamp_millis()]).await.map_err(|e| anyhow::anyhow!(format!("Failed to insert pattern: {e}")))?;

		Ok(())
	}

	/// Store multiple patterns in a specific dictionary
	///
	/// # Errors
	/// - if database not found
	/// - if unable to insert patterns
	pub async fn store_patterns_in_dictionary(&self, patterns: &[Pattern], dictionary_name: &str, aspect_id: &AspectId) -> Result<()> {
		if patterns.is_empty() {
			return Ok(());
		}

		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| anyhow::anyhow!("Database not found".to_string()))?
		};

		let patterns_turso_db = Self::get_or_create_dictionary_patterns_database(dictionary_name, &db_info.path).await?;

		// Process in chunks for better performance
		for chunk in patterns.chunks(DICTIONARY_CHUNK_SIZE) {
			let mut conn = patterns_turso_db.connect()?;
			let tx = conn.transaction().await.map_err(|e| anyhow::anyhow!(format!("Failed to begin transaction: {e}")))?;

			let result = async {
				for pattern in chunk {
					// Serialize the pattern occurrences and relatives
					let occurrences_json = serde_json::to_string(&pattern.occurrences()).map_err(|e| anyhow::anyhow!(format!("Failed to serialize pattern occurrences: {e}")))?;
					let relatives_json = serde_json::to_string(&pattern.relatives()).map_err(|e| anyhow::anyhow!(format!("Failed to serialize pattern relatives: {e}")))?;

					let pattern_id = pattern.id().to_string();

					tx.execute("INSERT INTO patterns (id, aspect_id, database_id, dictionary_name, occurrences, relatives, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)", turso::params![pattern_id, aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string(), dictionary_name, occurrences_json, relatives_json, chrono::Utc::now().timestamp_millis()]).await.map_err(|e| anyhow::anyhow!(format!("Failed to insert pattern in transaction: {e}")))?;
				}
				Ok::<(), anyhow::Error>(())
			}
			.await;

			// Commit or rollback transaction based on result
			match result {
				Ok(()) => {
					tx.commit().await.map_err(|e| anyhow::anyhow!(format!("Failed to commit pattern transaction: {e}")))?;
				}
				Err(e) => {
					let _ = tx.rollback().await;
					return Err(e);
				}
			}
		}
		Ok(())
	}

	/// Get patterns from a specific dictionary
	///
	/// # Errors
	/// - if database not found
	/// - if unable to query patterns
	pub async fn get_patterns_from_dictionary(&self, dictionary_name: &str, aspect_id: &AspectId) -> Result<Vec<Pattern>> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| anyhow::anyhow!("Database not found".to_string()))?
		};

		let patterns_turso_db = Self::get_or_create_dictionary_patterns_database(dictionary_name, &db_info.path).await?;
		let conn = patterns_turso_db.connect()?;

		let mut rows = conn.query("SELECT id, occurrences, relatives FROM patterns WHERE dictionary_name = ? AND aspect_id = ? AND database_id = ? ORDER BY created_at", turso::params![dictionary_name, aspect_id.as_uuid().to_string(), self.id().as_uuid().to_string()]).await.map_err(|e| anyhow::anyhow!(format!("Failed to query patterns: {e}")))?;

		let mut patterns = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| anyhow::anyhow!(format!("Failed to get row: {e}")))? {
			let pattern_id_str = Self::value_to_string(&row.get_value(0)?, "Pattern ID").await?;
			let occurrences_json = Self::value_to_string(&row.get_value(1)?, "Occurrences").await?;
			let relatives_json = Self::value_to_string(&row.get_value(2)?, "Relatives").await?;

			let pattern_id = PatternID::from_string(&pattern_id_str).map_err(|e| anyhow::anyhow!(format!("Failed to parse pattern ID: {e}")))?;
			let occurrences: Vec<crate::Occurrence> = serde_json::from_str(&occurrences_json).map_err(|e| anyhow::anyhow!(format!("Failed to deserialize occurrences: {e}")))?;
			let relatives: Vec<crate::Relative> = serde_json::from_str(&relatives_json).map_err(|e| anyhow::anyhow!(format!("Failed to deserialize relatives: {e}")))?;

			let pattern = Pattern::new(pattern_id, occurrences, relatives);
			patterns.push(pattern);
		}

		Ok(patterns)
	}

	/// Store dictionary metadata
	///
	/// # Errors
	/// - if database not found
	/// - if unable to insert dictionary metadata
	pub async fn store_dictionary_metadata(&self, name: &str, description: &str, constraints: &serde_json::Value) -> Result<()> {
		let metadata_db = &self.metadata;
		let metadata_db_path = &self.metadata_path;
		let constraints_json = serde_json::to_string(constraints).map_err(|e| anyhow::anyhow!(format!("Failed to serialize constraints: {e}")))?;
		let conn = Self::begin_concurrent(metadata_db, metadata_db_path).await?;
		let now = chrono::Utc::now().timestamp_millis();

		let res = conn.as_ref().execute("INSERT INTO dictionaries (name, description, constraints, created_at, updated_at) VALUES (?, ?, ?, ?, ?)", turso::params![name, description, constraints_json.clone(), now, now]).await;
		let mut insert_ok = false;

		match res {
			Ok(_) => {
				println!("Dictionary metadata inserted successfully");
				insert_ok = true;
			}
			Err(_) => (),
		}

		if !insert_ok {
			let res = conn.as_ref().execute("UPDATE dictionaries SET description = ?, constraints = ?, updated_at = ? WHERE name = ?", turso::params![description, constraints_json.clone(), now, name]).await;
			match res {
				Ok(_) => println!("Dictionary metadata updated successfully"),
				Err(e) => {
					Self::rollback_concurrent(&conn).await?;
					return Err(anyhow::anyhow!(format!("Failed to update dictionary metadata: {e}")));
				}
			}
		}

		let _ = Database::commit_concurrent(&conn).await;
		Ok(())
	}

	/// Get dictionary metadata
	///
	/// # Errors
	/// - if database not found
	/// - if unable to query dictionary metadata
	pub async fn get_dictionary_metadata(&self, name: &str) -> Result<Option<(String, serde_json::Value)>> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| anyhow::anyhow!("Database not found".to_string()))?
		};

		let metadata_turso_db = Self::get_or_create_dictionary_metadata_database(db_info.path).await?;
		let conn = metadata_turso_db.connect()?;

		let mut rows = conn.query("SELECT description, constraints FROM dictionaries WHERE name = ?", turso::params![name]).await.map_err(|e| anyhow::anyhow!(format!("Failed to query dictionary metadata: {e}")))?;

		if let Some(row) = rows.next().await.map_err(|e| anyhow::anyhow!(format!("Failed to get row: {e}")))? {
			let description = Self::value_to_string(&row.get_value(0)?, "Description").await?;
			let constraints_json = Self::value_to_string(&row.get_value(1)?, "Constraints").await?;
			let constraints: serde_json::Value = serde_json::from_str(&constraints_json).map_err(|e| anyhow::anyhow!(format!("Failed to deserialize constraints: {e}")))?;

			Ok(Some((description, constraints)))
		} else {
			Ok(None)
		}
	}

	/// List all dictionaries in the database
	///
	/// # Errors
	/// - if database not found
	/// - if unable to query dictionaries
	pub async fn list_dictionaries(&self) -> Result<Vec<String>> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| anyhow::anyhow!("Database not found".to_string()))?
		};

		let metadata_turso_db = Self::get_or_create_dictionary_metadata_database(db_info.path).await?;
		let conn = metadata_turso_db.connect()?;

		let mut rows = conn.query("SELECT name FROM dictionaries ORDER BY name", turso::params![]).await.map_err(|e| anyhow::anyhow!(format!("Failed to query dictionaries: {e}")))?;

		let mut dictionaries = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| anyhow::anyhow!(format!("Failed to get row: {e}")))? {
			let name = Self::value_to_string(&row.get_value(0)?, "Dictionary name").await?;
			dictionaries.push(name);
		}

		Ok(dictionaries)
	}
}
