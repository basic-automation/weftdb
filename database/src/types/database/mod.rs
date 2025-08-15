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

		// Create subjects table
		sqlx::query(
			r#"
            CREATE TABLE IF NOT EXISTS subjects (
                id BLOB PRIMARY KEY,
                name TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )
            "#,
		)
		.execute(pool)
		.await?;

		// Create subject_metadata table for track_subject compatibility
		sqlx::query(
			r#"
            CREATE TABLE IF NOT EXISTS subject_metadata (
                id BLOB PRIMARY KEY,
                database_id BLOB NOT NULL,
                name TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                FOREIGN KEY (database_id) REFERENCES database_metadata(id)
            )
            "#,
		)
		.execute(pool)
		.await?;

		// Create aspects table
		sqlx::query(
			r#"
            CREATE TABLE IF NOT EXISTS aspects (
                id BLOB PRIMARY KEY,
                subject_id BLOB NOT NULL,
                name TEXT NOT NULL,
                resolution TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                FOREIGN KEY (subject_id) REFERENCES subjects(id)
            )
            "#,
		)
		.execute(pool)
		.await?;

		// Create aspect metadata table for tracking earliest/latest measurements
		sqlx::query(
			r#"
            CREATE TABLE IF NOT EXISTS aspect_metadata (
                id BLOB PRIMARY KEY,
                subject_id BLOB NOT NULL,
                database_id BLOB NOT NULL,
                name TEXT NOT NULL,
                table_name TEXT NOT NULL,
                resolution TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                earliest_measurement INTEGER,
                latest_measurement INTEGER,
                FOREIGN KEY (subject_id) REFERENCES subjects(id),
                FOREIGN KEY (database_id) REFERENCES database_metadata(id)
            )
            "#,
		)
		.execute(pool)
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

		// Use shared connection pool
		let metadata_db_path = format!("{db_path}/metadata.db");
		let metadata_pool = Self::get_or_create_pool(&metadata_db_path).await?;

		// Query database ID from metadata
		let row = sqlx::query("SELECT id FROM database_metadata WHERE name = ?").bind(name).fetch_one(&metadata_pool).await?;

		let db_id_bytes: Vec<u8> = row.get("id");
		let db_id = DatabaseId::from_uuid(Uuid::from_slice(&db_id_bytes)?);

		// Load subjects manually instead of using query_as
		let subject_rows = sqlx::query("SELECT id, name, created_at FROM subjects").fetch_all(&metadata_pool).await?;

		let mut db_info = DatabaseInfo::new(name.to_string(), db_path);
		db_info.set_id(db_id);
		db_info.set_metadata_pool(Some(metadata_pool.clone()));

		// Manually construct subjects from rows
		for row in subject_rows {
			let subject_id_bytes: Vec<u8> = row.get("id");
			let subject_id = SubjectId::from_uuid(Uuid::from_slice(&subject_id_bytes)?);
			let subject_name: String = row.get("name");

			let subject = Subject::new_with_id(subject_id, subject_name, db_id, metadata_pool.clone());

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

		Ok(())
	}

	/// Clean up unused connection pools periodically
	pub async fn cleanup_unused_pools() -> Result<()> {
		let mut pools = CONNECTION_POOLS.lock().await;
		let databases = DATABASES.lock().await;

		// Find pools that are no longer referenced by any database
		let active_paths: std::collections::HashSet<String> = databases.values().filter_map(|db_info| db_info.metadata_pool().map(|_| format!("{}/metadata.db", db_info.path()))).collect();

		let unused_paths: Vec<String> = pools.keys().filter(|path| !active_paths.contains(*path)).cloned().collect();

		for path in unused_paths {
			if let Some(pool) = pools.remove(&path) {
				pool.close().await;
			}
		}

		// Clean up write pools similarly
		let mut write_pools = WRITE_POOLS.lock().await;
		let unused_write_paths: Vec<String> = write_pools.keys().filter(|path| !active_paths.contains(*path)).cloned().collect();

		for path in unused_write_paths {
			if let Some(pool) = write_pools.remove(&path) {
				pool.close().await;
			}
		}

		Ok(())
	}

	/// Get connection pool statistics for monitoring
	pub async fn get_pool_stats() -> HashMap<String, (usize, usize)> {
		let pools = CONNECTION_POOLS.lock().await;
		pools.iter().map(|(path, pool)| (path.clone(), (pool.size() as usize, pool.num_idle()))).collect()
	}

	/// Create a new subject in this database
	///
	/// # Errors
	/// - if unable to insert subject into metadata
	/// - if database not found
	pub async fn create_subject(&self, name: &str) -> Result<Subject> {
		let subject_id = SubjectId::new();

		// Insert into metadata table
		sqlx::query("INSERT INTO subjects (id, name, created_at) VALUES (?, ?, ?)").bind(subject_id.as_uuid()).bind(name).bind(chrono::Utc::now().timestamp_millis()).execute(&self.pool).await?;

		let subject = Subject::new_with_id(subject_id, name.to_string(), self.id, self.pool.clone());

		// Add to database info
		if let Some(db_info) = DATABASES.lock().await.get_mut(&self.id) {
			db_info.add_subject(subject.clone());
		}

		Ok(subject)
	}

	/// Get a subject by ID
	///
	/// # Errors
	/// - if database not found
	pub async fn get_subject(&self, subject_id: &SubjectId) -> Result<Option<Subject>> {
		let db_info = self.get_database_info().await.ok_or_else(|| anyhow::anyhow!("Database not found"))?;

		Ok(db_info.subjects().get(subject_id).cloned())
	}

	/// List all subjects in this database - returns Vec<Subject> instead of HashMap
	///
	/// # Errors
	/// - if database not found
	pub async fn get_all_subjects(&self) -> Result<Vec<Subject>> {
		let db_info = self.get_database_info().await.ok_or_else(|| anyhow::anyhow!("Database not found"))?;

		Ok(db_info.subjects().values().cloned().collect())
	}

	/// Remove a subject and all its aspects
	///
	/// # Errors
	/// - if unable to delete from metadata
	/// - if database not found
	pub async fn remove_subject(&self, subject_id: &SubjectId) -> Result<()> {
		// Delete from metadata table (cascading should handle aspects)
		sqlx::query("DELETE FROM subjects WHERE id = ?").bind(subject_id.as_uuid()).execute(&self.pool).await?;

		// Remove from database info
		if let Some(db_info) = DATABASES.lock().await.get_mut(&self.id) {
			db_info.subjects_mut().remove(subject_id);
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
