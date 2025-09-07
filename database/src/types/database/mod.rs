use std::{
	collections::HashMap, path::Path, sync::{Arc, LazyLock}
};

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use sqlx::{Pool, Row, Sqlite, SqlitePool};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{Subject, SubjectId, DEFAULT_DATA_DIR};

pub type DatabaseMap = Arc<Mutex<HashMap<DatabaseId, DatabaseInfo>>>;
pub static DATABASES: LazyLock<DatabaseMap> = LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

// Add connection pool manager
static CONNECTION_POOLS: LazyLock<Arc<Mutex<HashMap<String, SqlitePool>>>> = LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

// Add dedicated write pools for concurrency
static WRITE_POOLS: LazyLock<Arc<Mutex<HashMap<String, SqlitePool>>>> = LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

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
	pool: Pool<Sqlite>,
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

		// Use shared connection pool
		let metadata_db_path = format!("{db_path}/metadata.db");
		let metadata_pool = Self::get_or_create_pool(&metadata_db_path).await?;

		// Create metadata tables
		Self::create_metadata_tables(&metadata_pool).await?;

		// Insert database metadata - using UUID directly
		sqlx::query("INSERT INTO database_metadata (id, name, created_at) VALUES (?, ?, ?)").bind(db_id.as_uuid()).bind(name).bind(chrono::Utc::now().timestamp_millis()).execute(&metadata_pool).await?;

		let mut db_info = DatabaseInfo::new(name.to_string(), db_path);
		db_info.set_metadata_pool(Some(metadata_pool.clone()));
		DATABASES.lock().await.insert(db_id, db_info);

		Ok(Self { id: db_id, name: name.to_string(), pool: metadata_pool })
	}

	async fn create_metadata_tables(pool: &Pool<Sqlite>) -> Result<()> {
		// Create database metadata table - using BLOB for UUID storage
		sqlx::query(
			r#"
			CREATE TABLE IF NOT EXISTS database_metadata (
				id BLOB PRIMARY KEY,
				name TEXT NOT NULL,
				created_at INTEGER NOT NULL
			)
			"#,
		)
		.execute(pool)
		.await?;

		// Create subjects table (merged with subject_metadata)
		sqlx::query(
			r#"
			CREATE TABLE IF NOT EXISTS subjects (
				id BLOB PRIMARY KEY,
				database_id BLOB NOT NULL,
				name TEXT NOT NULL,
				created_at INTEGER NOT NULL
			)
			"#,
		)
		.execute(pool)
		.await?;

		// Removed subject_metadata table

		// Create aspects table (merged with aspect_metadata)
		sqlx::query(
			r#"
			CREATE TABLE IF NOT EXISTS aspects (
				id BLOB PRIMARY KEY,
				subject_id BLOB NOT NULL,
				database_id BLOB NOT NULL,
				name TEXT NOT NULL,
				table_name TEXT NOT NULL,
				resolution TEXT NOT NULL,
				created_at INTEGER NOT NULL,
				earliest_measurement INTEGER,
				latest_measurement INTEGER
			)
			"#,
		)
		.execute(pool)
		.await?;

		// Removed aspect_metadata table

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

		// Use shared connection pool
		let metadata_db_path = format!("{db_path}/metadata.db");
		let metadata_pool = Self::get_or_create_pool(&metadata_db_path).await?;

		// Query database ID from metadata
		let row = sqlx::query("SELECT id FROM database_metadata WHERE name = ?").bind(name).fetch_one(&metadata_pool).await?;

		let db_id_bytes: Vec<u8> = row.get("id");
		let db_id = DatabaseId::from_uuid(Uuid::from_slice(&db_id_bytes)?);

		// Load subjects with database_id
		let subject_rows = sqlx::query("SELECT id, database_id, name, created_at FROM subjects").fetch_all(&metadata_pool).await?;

		let mut db_info = DatabaseInfo::new(name.to_string(), db_path.clone());
		db_info.set_id(db_id);
		db_info.set_metadata_pool(Some(metadata_pool.clone()));

		// Manually construct subjects from rows
		for row in subject_rows {
			let subject_id_bytes: Vec<u8> = row.get("id");
			let subject_id = SubjectId::from_uuid(Uuid::from_slice(&subject_id_bytes)?);
			let subject_name: String = row.get("name");
			// database_id is now in subjects table, but since we're loading per database, we can ignore it or verify

			// Connect to the subject's individual database file
			let subject_db_path = format!("{}/{}.db", db_path, subject_name);
			let subject_pool = Self::get_or_create_pool(&subject_db_path).await?;

			let mut subject = Subject::new_with_id(subject_id, subject_name, db_id, subject_pool);

			// Load aspects for this subject
			let aspect_rows = sqlx::query("SELECT id, name, table_name, resolution FROM aspects WHERE subject_id = ?").bind(subject_id.as_uuid()).fetch_all(&metadata_pool).await?;

			for aspect_row in aspect_rows {
				let aspect_id_bytes: Vec<u8> = aspect_row.get("id");
				let aspect_id = crate::AspectId::from_uuid(Uuid::from_slice(&aspect_id_bytes)?);
				let aspect_name: String = aspect_row.get("name");
				let table_name: String = aspect_row.get("table_name");
				let resolution_str: String = aspect_row.get("resolution");

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

		Ok(Self { id: db_id, name: name.to_string(), pool: metadata_pool })
	}

	pub async fn get_database_info(&self) -> Option<DatabaseInfo> {
		DATABASES.lock().await.get(&self.id).cloned()
	}

	/// Get or create a connection pool for reuse
	async fn get_or_create_pool(db_path: &str) -> Result<SqlitePool> {
		let mut pools = CONNECTION_POOLS.lock().await;

		if let Some(pool) = pools.get(db_path) {
			return Ok(pool.clone());
		}

		let pool = SqlitePool::connect(&format!("sqlite://{db_path}?mode=rwc")).await?;

		// Enable WAL mode for better concurrency
		sqlx::query("PRAGMA journal_mode=WAL").execute(&pool).await?;

		// Set busy timeout for better concurrent access handling
		sqlx::query("PRAGMA busy_timeout=30000").execute(&pool).await?;

		pools.insert(db_path.to_string(), pool.clone());

		Ok(pool)
	}

	/// Get or create a dedicated write pool (single connection) for concurrency
	async fn get_or_create_write_pool(db_path: &str) -> Result<SqlitePool> {
		let mut pools = WRITE_POOLS.lock().await;

		if let Some(pool) = pools.get(db_path) {
			return Ok(pool.clone());
		}

		// Create pool with single connection for writes to avoid locking
		let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect(&format!("sqlite://{db_path}?mode=rwc")).await?;

		// Enable WAL mode for better concurrency
		sqlx::query("PRAGMA journal_mode=WAL").execute(&pool).await?;

		// Set busy timeout for better concurrent access handling
		sqlx::query("PRAGMA busy_timeout=30000").execute(&pool).await?;

		pools.insert(db_path.to_string(), pool.clone());

		Ok(pool)
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
	pub fn pool(&self) -> &Pool<Sqlite> {
		&self.pool
	}

	/// Closes the database, releasing all resources and removing from global map
	///
	/// # Errors
	///
	/// Returns an error if there are issues closing connection pools or removing resources.
	pub async fn close(&self) -> Result<()> {
		let mut databases = DATABASES.lock().await;
		if let Some(db_info) = databases.remove(&self.id) {
			if let Some(pool) = db_info.metadata_pool() {
				pool.close().await;
			}
		}

		// Also remove from connection pools
		let db_path = format!("{}/{}/metadata.db", DEFAULT_DATA_DIR, self.name);
		let mut pools = CONNECTION_POOLS.lock().await;
		if let Some(pool) = pools.remove(&db_path) {
			pool.close().await;
		}

		// Remove from write pools
		let mut write_pools = WRITE_POOLS.lock().await;
		if let Some(pool) = write_pools.remove(&db_path) {
			pool.close().await;
		}

		// Release the mutex locks early
		drop(write_pools);
		drop(pools);
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
		Self { id: DatabaseId::default(), name, path, subjects: HashMap::new(), metadata_pool: None }
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
		if let Some(pool) = &self.metadata_pool {
			let row = sqlx::query("SELECT created_at FROM database_metadata WHERE name = ?").bind(&self.name).fetch_optional(pool).await?;

			if let Some(row) = row {
				let timestamp_millis: i64 = row.get("created_at");
				return Ok(DateTime::from_timestamp_millis(timestamp_millis));
			}
		}
		Ok(None)
	}

	/// Get database size statistics
	pub async fn get_size_stats(&self) -> Result<DatabaseStats> {
		let mut stats = DatabaseStats::default();

		if let Some(pool) = &self.metadata_pool {
			// Count subjects
			let subject_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM subjects").fetch_one(pool).await?;
			stats.subject_count = subject_count as usize;

			// Count aspects
			let aspect_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM aspects").fetch_one(pool).await?;
			stats.aspect_count = aspect_count as usize;
		}

		Ok(stats)
	}
}

#[derive(Debug, Default, Clone)]
pub struct DatabaseStats {
	pub subject_count: usize,
	pub aspect_count: usize,
	pub total_measurements: usize,
	pub disk_size_bytes: u64,
}
