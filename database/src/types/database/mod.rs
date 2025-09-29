use std::{
	collections::HashMap, path::Path, sync::{Arc, LazyLock}
};

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use turso::{Builder, Database as TursoDatabase};
use uuid::Uuid;

use crate::{AspectId, Subject, SubjectId, DEFAULT_DATA_DIR};

pub type DatabaseMap = Arc<Mutex<HashMap<DatabaseId, DatabaseInfo>>>;
pub static DATABASES: LazyLock<DatabaseMap> = LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

// Add connection manager for Turso
static CONNECTION_DATABASES: LazyLock<Arc<Mutex<HashMap<String, TursoDatabase>>>> = LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

// Add dedicated write connections for concurrency (Turso handles this internally)

mod analysis;
mod aspects;
mod helpers;
mod measurements;
mod navigation;
mod subjects;

#[derive(Debug, Clone)]
pub struct Database {
	id: DatabaseId,
	name: String,
	turso_db: TursoDatabase,
}

impl Database {
	/// Creates a new database instance. Creates folder /{`data_dir}/{name`}.
	/// Keeps count of instances of `DB::existing({name`}) and manages read / write access as necessary.
	///
	/// # Errors
	/// - if folder /data/{name} already exists.
	pub async fn new(name: &str) -> Result<Self> {
		let data_dir = DEFAULT_DATA_DIR;
		let db_path = format!("{data_dir}/{name}");

		// Check if folder already exists
		if Path::new(&db_path).exists() {
			bail!("Database folder already exists: {}", db_path);
		}

		// Create the directory
		std::fs::create_dir_all(&db_path)?;

		let db_id = DatabaseId::new();

		// Use shared connection database
		let metadata_db_path = format!("{db_path}/metadata.db");
		let metadata_turso_db = Self::get_or_create_turso_database(&metadata_db_path).await?;

		// Create metadata tables
		Self::create_metadata_tables(&metadata_turso_db).await?;

		// Insert database metadata
		let conn = metadata_turso_db.connect()?;
		let mut result = conn.query("INSERT INTO database_metadata (id, name, created_at) VALUES (?, ?, ?)", turso::params![db_id.as_uuid().to_string(), name.to_string(), chrono::Utc::now().timestamp_millis().to_string()]).await?;
		// Consume the result to ensure the insert completes
		while (result.next().await?).is_some() {}

		let mut db_info = DatabaseInfo::new(name.to_string(), db_path);
		db_info.set_metadata_turso_db(Some(metadata_turso_db.clone()));
		DATABASES.lock().await.insert(db_id, db_info);

		Ok(Self { id: db_id, name: name.to_string(), turso_db: metadata_turso_db })
	}

	async fn create_metadata_tables(turso_db: &TursoDatabase) -> Result<()> {
		let conn = turso_db.connect()?;

		// Create database metadata table
		conn.execute(
			r#"
			CREATE TABLE IF NOT EXISTS database_metadata (
				id TEXT PRIMARY KEY,
				name TEXT NOT NULL,
				created_at INTEGER NOT NULL
			)
			"#,
			turso::params![],
		)
		.await?;

		// Create subjects table (merged with subject_metadata)
		conn.execute(
			r#"
			CREATE TABLE IF NOT EXISTS subjects (
				id TEXT PRIMARY KEY,
				database_id TEXT NOT NULL,
				name TEXT NOT NULL,
				created_at INTEGER NOT NULL
			)
			"#,
			turso::params![],
		)
		.await?;

		// Create aspects table (merged with aspect_metadata)
		conn.execute(
			r#"
			CREATE TABLE IF NOT EXISTS aspects (
				id TEXT PRIMARY KEY,
				subject_id TEXT NOT NULL,
				database_id TEXT NOT NULL,
				name TEXT NOT NULL,
				table_name TEXT NOT NULL,
				resolution TEXT NOT NULL,
				created_at INTEGER NOT NULL,
				earliest_measurement TEXT,
				latest_measurement TEXT
			)
			"#,
			turso::params![],
		)
		.await?;

		Ok(())
	}

	/// Loads an instance of DB from /{`data_dir}/{name`}.
	/// Maps Subjects and loads cache.
	/// Keeps count of instances of `DB::existing({name`}) and manages read / write access as necessary.
	///
	/// # Errors
	/// - if folder /data/{name} does not exist
	/// - if database connection fails
	/// - if unable to query existing tables
	pub async fn existing(name: &str) -> Result<Self> {
		let data_dir = DEFAULT_DATA_DIR;
		let db_path = format!("{data_dir}/{name}");

		// Check if folder exists
		if !Path::new(&db_path).exists() {
			bail!("Database folder does not exist: {}", db_path);
		}

		// Use shared connection database
		let metadata_db_path = format!("{db_path}/metadata.db");
		let metadata_turso_db = Self::get_or_create_turso_database(&metadata_db_path).await?;

		// Query database ID from metadata
		let conn = metadata_turso_db.connect()?;
		let mut rows = conn.query("SELECT id FROM database_metadata WHERE name = ?", turso::params![name]).await?;
		let row = rows.next().await?.ok_or_else(|| anyhow::anyhow!("Database not found"))?;

		let db_id_str = value_to_string(row.get_value(0)?, "DB ID")?;
		let db_id = DatabaseId::from_uuid(Uuid::parse_str(&db_id_str)?);

		// Load subjects with database_id
		let conn = metadata_turso_db.connect()?;
		let mut subject_rows = conn.query("SELECT id, database_id, name, created_at FROM subjects", ()).await?;

		let mut db_info = DatabaseInfo::new(name.to_string(), db_path.clone());
		db_info.set_id(db_id);
		db_info.set_metadata_turso_db(Some(metadata_turso_db.clone()));

		// Manually construct subjects from rows
		while let Some(row) = subject_rows.next().await? {
			let subject_id_str = value_to_string(row.get_value(0)?, "Subject ID")?;
			let subject_name = value_to_string(row.get_value(2)?, "Subject name")?;
			// database_id is now in subjects table, but since we're loading per database, we can ignore it or verify

			let subject_id = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str)?);

			// Connect to the subject's individual database file
			let subject_db_path = format!("{}/{}.db", db_path, subject_name);
			let subject_turso_db = Self::get_or_create_turso_database(&subject_db_path).await?;

			let mut subject = Subject::new_with_id(subject_id, subject_name, db_id, subject_turso_db);

			// Load aspects for this subject
			let mut aspect_rows = conn.query("SELECT id, name, table_name, resolution FROM aspects WHERE subject_id = ?", turso::params![subject_id.as_uuid().to_string()]).await?;

			while let Some(aspect_row) = aspect_rows.next().await? {
				let aspect_id_str = value_to_string(aspect_row.get_value(0)?, "Aspect ID")?;
				let aspect_name = value_to_string(aspect_row.get_value(1)?, "Aspect name")?;
				let table_name = value_to_string(aspect_row.get_value(2)?, "Table name")?;
				let resolution_str = value_to_string(aspect_row.get_value(3)?, "Resolution")?;

				let aspect_id = AspectId::from_uuid(Uuid::parse_str(&aspect_id_str)?);

				// Parse resolution
				use splimes::Resolution;
				let resolution = match resolution_str.as_str() {
					"Nanoseconds" => Resolution::Nanoseconds,
					"Microseconds" => Resolution::Microseconds,
					"Milliseconds" => Resolution::Milliseconds,
					"Seconds" => Resolution::Seconds,
					"Minutes" => Resolution::Minutes,
					"Hours" => Resolution::Hours,
					"Days" => Resolution::Days,
					"Weeks" => Resolution::Weeks,
					"Months" => Resolution::Months,
					"Years" => Resolution::Years,
					_ => bail!("Invalid resolution value: {}", resolution_str),
				};

				let aspect = crate::Aspect::new_with_id(aspect_id, aspect_name, subject_id, table_name, resolution);
				subject.add_aspect(aspect);
			}

			db_info.add_subject(subject);
		}

		DATABASES.lock().await.insert(db_id, db_info);

		Ok(Self { id: db_id, name: name.to_string(), turso_db: metadata_turso_db })
	}

	pub async fn get_database_info(&self) -> Option<DatabaseInfo> {
		DATABASES.lock().await.get(&self.id).cloned()
	}

	/// Get or create a Turso database for reuse
	async fn get_or_create_turso_database(db_path: &str) -> Result<TursoDatabase> {
		let mut databases = CONNECTION_DATABASES.lock().await;

		if let Some(turso_db) = databases.get(db_path) {
			return Ok(turso_db.clone());
		}

		// Create Turso database
		let turso_db = Builder::new_local(db_path).build().await?;

		// Enable WAL mode for better concurrency
		let conn = turso_db.connect()?;
		let mut result = conn.query("PRAGMA journal_mode=WAL", turso::params![]).await?;
		// Consume any results
		while (result.next().await?).is_some() {}

		let mut result = conn.query("PRAGMA busy_timeout=30000", turso::params![]).await?;
		// Consume any results
		while (result.next().await?).is_some() {}

		let _ = std::collections::HashMap::insert(&mut *databases, db_path.to_string(), turso_db.clone());

		Ok(turso_db)
	}

	#[must_use]
	pub const fn id(&self) -> DatabaseId {
		self.id
	}

	#[must_use]
	pub fn name(&self) -> &str {
		&self.name
	}

	#[must_use]
	pub fn turso_db(&self) -> &TursoDatabase {
		&self.turso_db
	}

	/// Closes the database, releasing all resources and removing from global map
	///
	/// # Errors
	///
	/// Returns an error if there are issues closing connection pools or removing resources.
	pub async fn close(&self) -> Result<()> {
		let mut databases = DATABASES.lock().await;
		if let Some(_db_info) = databases.remove(&self.id) {
			// Turso databases don't need explicit closing like SQLx pools
			// The connections will be closed when dropped
		}

		// Also remove from connection databases
		let db_path = format!("{}/{}/metadata.db", DEFAULT_DATA_DIR, self.name);
		let mut connection_databases = CONNECTION_DATABASES.lock().await;
		connection_databases.remove(&db_path);

		// Release the mutex locks early
		drop(connection_databases);
		drop(databases);

		// Wait for database files to be actually released
		self.wait_for_database_release().await?;

		Ok(())
	}

	/// Wait for the database files to be released by checking if we can delete them
	async fn wait_for_database_release(&self) -> Result<()> {
		let db_dir = format!("{}/{}", DEFAULT_DATA_DIR, self.name);
		let metadata_db_path = format!("{}/metadata.db", db_dir);

		const MAX_ATTEMPTS: u32 = 50; // 5 seconds total
		const DELAY_MS: u64 = 100;

		for attempt in 0..MAX_ATTEMPTS {
			// Try to open the database file exclusively to check if it's still locked
			match std::fs::OpenOptions::new().write(true).truncate(false).open(&metadata_db_path) {
				Ok(_) => {
					// File is accessible, database is released
					return Ok(());
				}
				Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
					// File is still locked, wait and retry
					if attempt == MAX_ATTEMPTS - 1 {
						// Last attempt failed, but don't error - just log
						eprintln!("Warning: Database may still be locked after {} attempts: {}", MAX_ATTEMPTS, metadata_db_path);
						return Ok(());
					}
					tokio::time::sleep(std::time::Duration::from_millis(DELAY_MS)).await;
				}
				Err(_) => {
					// Other error (file doesn't exist, etc.) - consider it released
					return Ok(());
				}
			}
		}

		Ok(())
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DatabaseId(Uuid);

impl DatabaseId {
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

impl Default for DatabaseId {
	fn default() -> Self {
		Self::new()
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseInfo {
	id: DatabaseId,
	name: String,
	path: String,
	subjects: HashMap<SubjectId, Subject>,
	#[serde(skip)]
	metadata_turso_db: Option<TursoDatabase>,
}

impl PartialEq for DatabaseInfo {
	fn eq(&self, other: &Self) -> bool {
		self.id == other.id && self.name == other.name && self.path == other.path && self.subjects == other.subjects
		// Skip turso_db comparison since it doesn't implement PartialEq
	}
}

impl Eq for DatabaseInfo {}

impl DatabaseInfo {
	#[must_use]
	pub fn new(name: String, path: String) -> Self {
		Self { id: DatabaseId::default(), name, path, subjects: HashMap::new(), metadata_turso_db: None }
	}

	#[must_use]
	pub const fn id(&self) -> DatabaseId {
		self.id
	}

	pub const fn set_id(&mut self, id: DatabaseId) {
		self.id = id;
	}

	#[must_use]
	pub fn name(&self) -> &str {
		&self.name
	}

	#[must_use]
	pub fn path(&self) -> &str {
		&self.path
	}

	pub fn add_subject(&mut self, subject: Subject) {
		self.subjects.insert(subject.id(), subject);
	}

	#[must_use]
	pub const fn subjects(&self) -> &HashMap<SubjectId, Subject> {
		&self.subjects
	}

	#[must_use]
	pub const fn metadata_turso_db(&self) -> Option<&TursoDatabase> {
		self.metadata_turso_db.as_ref()
	}

	pub fn set_metadata_turso_db(&mut self, turso_db: Option<TursoDatabase>) {
		self.metadata_turso_db = turso_db;
	}

	pub fn subjects_mut(&mut self) -> &mut HashMap<SubjectId, Subject> {
		&mut self.subjects
	}

	/// Get subject by name
	pub fn get_subject_by_name(&self, name: &str) -> Option<&Subject> {
		self.subjects.values().find(|s| s.name() == name)
	}

	/// Get total number of aspects across all subjects
	pub fn total_aspects(&self) -> usize {
		self.subjects.values().map(|s| s.aspects().len()).sum()
	}

	/// Check if database is empty (no subjects)
	pub fn is_empty(&self) -> bool {
		self.subjects.is_empty()
	}

	/// Get creation timestamp if available
	pub async fn get_creation_time(&self) -> Result<Option<DateTime<Utc>>> {
		if let Some(turso_db) = &self.metadata_turso_db {
			let conn = turso_db.connect()?;
			let mut rows = conn.query("SELECT created_at FROM database_metadata WHERE name = ?", turso::params![self.name.clone()]).await?;

			if let Some(row) = rows.next().await? {
				let timestamp_millis_str = value_to_string(row.get_value(0)?, "Timestamp")?;
				let timestamp_millis: i64 = timestamp_millis_str.parse()?;
				return Ok(DateTime::from_timestamp_millis(timestamp_millis));
			}
		}
		Ok(None)
	}

	/// Get database size statistics
	pub async fn get_size_stats(&self) -> Result<DatabaseStats> {
		let mut stats = DatabaseStats::default();

		if let Some(turso_db) = &self.metadata_turso_db {
			let conn = turso_db.connect()?;

			// Count subjects
			let mut subject_rows = conn.query("SELECT COUNT(*) as count FROM subjects", ()).await?;
			if let Some(row) = subject_rows.next().await? {
				let count_str = value_to_string(row.get_value(0)?, "Count")?;
				stats.subject_count = count_str.parse::<usize>()?;
			}

			// Count aspects
			let mut aspect_rows = conn.query("SELECT COUNT(*) as count FROM aspects", ()).await?;
			if let Some(row) = aspect_rows.next().await? {
				let count_str = value_to_string(row.get_value(0)?, "Count")?;
				stats.aspect_count = count_str.parse::<usize>()?;
			}
		}

		Ok(stats)
	}
}

/// Helper function to convert Turso values to strings, handling different storage formats
fn value_to_string(value: turso::Value, field_name: &str) -> Result<String> {
	match value {
		turso::Value::Text(s) => Ok(s.to_string()),
		turso::Value::Blob(bytes) => {
			// Try to interpret as UUID bytes
			if bytes.len() == 16 {
				let uuid = uuid::Uuid::from_bytes(bytes.as_slice().try_into().map_err(|_| anyhow::anyhow!("{} blob is not 16 bytes", field_name))?);
				Ok(uuid.to_string())
			} else {
				String::from_utf8(bytes.to_vec()).map_err(|e| anyhow::anyhow!("{} blob is not valid UTF-8: {}", field_name, e))
			}
		}
		turso::Value::Integer(i) => Ok(i.to_string()),
		turso::Value::Real(r) => Ok(r.to_string()),
		turso::Value::Null => Err(anyhow::anyhow!("{} is null", field_name)),
	}
}

#[derive(Debug, Default, Clone)]
pub struct DatabaseStats {
	pub subject_count: usize,
	pub aspect_count: usize,
	pub total_measurements: usize,
	pub disk_size_bytes: u64,
}
