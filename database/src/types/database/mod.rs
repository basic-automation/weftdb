use std::{
	collections::HashMap, path::Path, sync::{Arc, LazyLock}
};

use anyhow::{bail, Result};
use sqlx::{Pool, Row, Sqlite};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{Error, Subject, SubjectId, DEFAULT_DATA_DIR};

pub type DatabaseMap = Arc<Mutex<HashMap<DatabaseId, DatabaseInfo>>>;
pub static DATABASES: LazyLock<DatabaseMap> = LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

mod analysis;
mod aspects;
mod helpers;
mod measurements;
mod navigation;
mod subjects;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Database {
	id: DatabaseId,
	name: String,
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
			bail!(Error::DatabaseError(format!("Database '{name}' already exists")));
		}

		// Create the directory
		match std::fs::create_dir_all(&db_path) {
			Ok(()) => (),
			Err(e) => bail!(Error::DatabaseError(format!("Failed to create database directory '{db_path}': {e}"))),
		}
		let db_id = DatabaseId::new();

		// Create metadata database
		let metadata_db_path = format!("{db_path}/metadata.db");
		let metadata_pool = match sqlx::sqlite::SqlitePoolOptions::new().max_connections(5).connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&metadata_db_path).create_if_missing(true)).await {
			Ok(pool) => pool,
			Err(e) => bail!(Error::DatabaseError(format!("Failed to connect to metadata database '{metadata_db_path}': {e}"))),
		};

		// Create metadata tables
		Self::create_metadata_tables(&metadata_pool).await?;

		// Insert database metadata - using UUID directly
		match sqlx::query("INSERT INTO database_metadata (id, name, created_at) VALUES (?, ?, ?)").bind(db_id.as_uuid()).bind(name).bind(chrono::Utc::now().timestamp_millis()).execute(&metadata_pool).await {
			Ok(_) => (),
			Err(e) => bail!(Error::DatabaseError(format!("Failed to insert database metadata: {e}"))),
		}
		let mut db_info = DatabaseInfo::new(name.to_string(), db_path);
		db_info.set_metadata_pool(Some(metadata_pool));
		DATABASES.lock().await.insert(db_id, db_info);

		Ok(Self { id: db_id, name: name.to_string() })
	}

	async fn create_metadata_tables(pool: &Pool<Sqlite>) -> Result<()> {
		// Create database metadata table - using BLOB for UUID storage
		match sqlx::query("CREATE TABLE IF NOT EXISTS database_metadata (id BLOB PRIMARY KEY, name TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL)").execute(pool).await {
			Ok(_) => (),
			Err(e) => bail!(Error::DatabaseError(format!("Failed to create database metadata table: {e}"))),
		}

		// Create subject metadata table
		match sqlx::query("CREATE TABLE IF NOT EXISTS subject_metadata (id BLOB PRIMARY KEY, database_id BLOB NOT NULL, name TEXT NOT NULL, created_at INTEGER NOT NULL, FOREIGN KEY (database_id) REFERENCES database_metadata(id))").execute(pool).await {
			Ok(_) => (),
			Err(e) => bail!(Error::DatabaseError(format!("Failed to create subject metadata table: {e}"))),
		}

		// Create aspect metadata table
		match sqlx::query("CREATE TABLE IF NOT EXISTS aspect_metadata (id BLOB PRIMARY KEY, subject_id BLOB NOT NULL, database_id BLOB NOT NULL, name TEXT NOT NULL, table_name TEXT NOT NULL, created_at INTEGER NOT NULL, FOREIGN KEY (subject_id) REFERENCES subject_metadata(id), FOREIGN KEY (database_id) REFERENCES database_metadata(id))").execute(pool).await {
			Ok(_) => (),
			Err(e) => bail!(Error::DatabaseError(format!("Failed to create aspect metadata table: {e}"))),
		}

		// Create indexes for efficient queries
		match sqlx::query("CREATE INDEX IF NOT EXISTS idx_subject_metadata_database_id ON subject_metadata(database_id)").execute(pool).await {
			Ok(_) => (),
			Err(e) => bail!(Error::DatabaseError(format!("Failed to create subject_metadata index: {e}"))),
		}

		match sqlx::query("CREATE INDEX IF NOT EXISTS idx_aspect_metadata_subject_id ON aspect_metadata(subject_id)").execute(pool).await {
			Ok(_) => (),
			Err(e) => bail!(Error::DatabaseError(format!("Failed to create aspect_metadata index: {e}"))),
		}

		match sqlx::query("CREATE INDEX IF NOT EXISTS idx_aspect_metadata_database_id ON aspect_metadata(database_id)").execute(pool).await {
			Ok(_) => (),
			Err(e) => bail!(Error::DatabaseError(format!("Failed to create aspect_metadata index: {e}"))),
		}

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
			bail!("Database directory '{}' does not exist", db_path);
		}

		// Connect to metadata database
		let metadata_db_path = format!("{db_path}/metadata.db");
		let metadata_pool = if Path::new(&metadata_db_path).exists() {
			Some(match sqlx::sqlite::SqlitePoolOptions::new().max_connections(5).connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&metadata_db_path).create_if_missing(false)).await {
				Ok(pool) => pool,
				Err(e) => bail!(Error::DatabaseError(format!("Failed to connect to metadata database '{metadata_db_path}': {e}"))),
			})
		} else {
			None
		};

		// Load database metadata
		let db_id = if let Some(ref pool) = metadata_pool {
			// Load existing database ID from metadata - using UUID directly
			let row = match sqlx::query("SELECT id FROM database_metadata WHERE name = ?").bind(name).fetch_optional(pool).await {
				Ok(row) => row,
				Err(e) => bail!(Error::DatabaseError(format!("Failed to query database metadata: {e}"))),
			};

			row.map_or_else(DatabaseId::new, |row| {
				let uuid: Uuid = row.get("id");
				DatabaseId::from_uuid(uuid)
			})
		} else {
			DatabaseId::new()
		};

		let mut db_info = DatabaseInfo::new(name.to_string(), db_path.clone());
		db_info.set_metadata_pool(metadata_pool);
		db_info.set_id(db_id); // Set the loaded ID

		// Load existing subjects from metadata if available
		Self::load_subjects_from_metadata(&mut db_info, &db_path).await?;

		DATABASES.lock().await.insert(db_id, db_info);

		Ok(Self { id: db_id, name: name.to_string() })
	}

	pub async fn get_database_info(&self) -> Option<DatabaseInfo> {
		let databases = DATABASES.lock().await;
		databases.get(&self.id).cloned()
	}

	#[must_use]
	pub const fn id(&self) -> DatabaseId {
		self.id
	}

	#[must_use]
	pub fn name(&self) -> &str {
		&self.name
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

#[derive(Debug, Clone)]
pub struct DatabaseInfo {
	id: DatabaseId,
	name: String,
	path: String,
	subjects: HashMap<SubjectId, Subject>,
	metadata_pool: Option<Pool<Sqlite>>,
}

impl DatabaseInfo {
	#[must_use]
	pub fn new(name: String, path: String) -> Self {
		let id = DatabaseId::new();
		Self { id, name, path, subjects: HashMap::new(), metadata_pool: None }
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
	pub const fn metadata_pool(&self) -> Option<&Pool<Sqlite>> {
		self.metadata_pool.as_ref()
	}

	pub fn set_metadata_pool(&mut self, pool: Option<Pool<Sqlite>>) {
		self.metadata_pool = pool;
	}

	pub const fn subjects_mut(&mut self) -> &mut HashMap<SubjectId, Subject> {
		&mut self.subjects
	}
}
