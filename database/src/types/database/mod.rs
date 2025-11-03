use std::{
	collections::HashMap, fmt, fmt::{Formatter}, path::{Path, PathBuf}, sync::{Arc, LazyLock}
};

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use splimes::Resolution;
use tokio::sync::Mutex;
use turso::Builder;
use uuid::Uuid;
use std::fmt::Display;

use crate::{
	get_data_dir, types::{
		cache::Connection as CachedConnection, database::traits::{aspect_structure::AspectStructure, database_structure::DatabaseStructure}, Transaction, TxId
	}, Aspect, AspectId, Error, Subject, SubjectId, CACHE
};


pub type DatabaseMap = Arc<Mutex<HashMap<DatabaseId, DatabaseInfo>>>;
pub static DATABASES: LazyLock<DatabaseMap> = LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

// Add connection manager for Turso with connection pools
static CONNECTION_DATABASES: LazyLock<Arc<Mutex<HashMap<String, turso::Database>>>> = LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));
static METADATA_DB_FILENAME: &str = "metadata.db";
static MEASUREMENT_DB_FILENAME: &str = "measurements.db";
static UNPROCESSED_BATCHES_DB_FILENAME: &str = "unprocessed_batches.db";
static PROCESSED_BATCHES_DB_FILENAME: &str = "processed_batches.db";
static PATTERNS_DB_FILENAME: &str = "patterns.db";
static EVENTS_DB_FILENAME: &str = "events.db";
static CORRELATIONS_DB_FILENAME: &str = "correlations.db";
static DICTIONARIES_DB_FOLDERNAME: &str = "dictionaries";

pub fn db_path(db_name: &str) -> String {
	let mut path_buf: PathBuf = get_data_dir().into();
	path_buf.push(db_name);
	path_buf.to_string_lossy().to_string()
}

pub fn metadata_db_path(db_name: &str) -> String {
	let mut db_path: PathBuf = db_path(db_name).into();
	db_path.push(METADATA_DB_FILENAME);
	db_path.to_string_lossy().to_string()
}

pub fn aspect_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
	let mut db_path: PathBuf = db_path(db_name).into();
	db_path.push(subject_name);
	db_path.push(aspect_name);
	db_path.to_string_lossy().to_string()
}

pub fn aspect_measurements_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
	let mut db_path: PathBuf = aspect_path(db_name, subject_name, aspect_name).into();
	db_path.push(MEASUREMENT_DB_FILENAME);
	db_path.to_string_lossy().to_string()
}

pub fn aspect_unprocessed_batches_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
	let mut db_path: PathBuf = aspect_path(db_name, subject_name, aspect_name).into();
	db_path.push(UNPROCESSED_BATCHES_DB_FILENAME);
	db_path.to_string_lossy().to_string()
}

pub fn aspect_processed_batches_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
	let mut db_path: PathBuf = aspect_path(db_name, subject_name, aspect_name).into();
	db_path.push(PROCESSED_BATCHES_DB_FILENAME);
	db_path.to_string_lossy().to_string()
}

pub fn aspect_patterns_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
	let mut db_path: PathBuf = aspect_path(db_name, subject_name, aspect_name).into();
	db_path.push(PATTERNS_DB_FILENAME);
	db_path.to_string_lossy().to_string()
}

pub fn aspect_events_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
	let mut db_path: PathBuf = aspect_path(db_name, subject_name, aspect_name).into();
	db_path.push(EVENTS_DB_FILENAME);
	db_path.to_string_lossy().to_string()
}

pub fn aspect_correlations_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
	let mut db_path: PathBuf = aspect_path(db_name, subject_name, aspect_name).into();
	db_path.push(CORRELATIONS_DB_FILENAME);
	db_path.to_string_lossy().to_string()
}

pub fn aspect_dictionaries_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
	let mut db_path: PathBuf = aspect_path(db_name, subject_name, aspect_name).into();
	db_path.push(DICTIONARIES_DB_FOLDERNAME);
	db_path.to_string_lossy().to_string()
}

pub fn dictionary_path(db_name: &str, subject_name: &str, aspect_name: &str, dictionary_name: &str) -> String {
	let mut db_path: PathBuf = aspect_dictionaries_path(db_name, subject_name, aspect_name).into();
	db_path.push(format!("{}.db", dictionary_name));
	db_path.to_string_lossy().to_string()
}

pub mod batches;
pub mod correlations;
pub mod dictionaries;
pub mod events;
pub mod helpers;
pub mod inputs;
pub mod navigation;
pub mod outputs;
pub mod patterns;
pub mod traits;

#[derive(Debug, Clone)]
pub struct Database {
	id: DatabaseId,
	name: String,
	metadata: turso::Database,
	metadata_path: String,
}

impl Database {
	/// Get a cached database connection with MVCC concurrent transaction support
	/// Returns a cached connection if available, otherwise creates a new one
	pub async fn begin_concurrent(turso_db: &turso::Database, cache_key: &str) -> Result<CachedConnection> {
		// Try to get cached connection first
		if let Some(cached_conn) = CACHE.get_connection(cache_key).await {
			return Ok(cached_conn);
		}

		// Create new connection if not cached
		let conn = turso_db.connect()?;

		// Try BEGIN CONCURRENT first for MVCC support
		match conn.execute("BEGIN CONCURRENT", turso::params![]).await {
			Ok(_) => {}
			Err(_) => {
				// Fallback to BEGIN IMMEDIATE for compatibility
				match conn.execute("BEGIN IMMEDIATE", turso::params![]).await {
					Ok(_) => {}
					Err(_) => {
						// Final fallback to regular BEGIN
						conn.execute("BEGIN", turso::params![]).await.map_err(|e| anyhow::anyhow!("Failed to begin transaction: {}", e))?;
					}
				}
			}
		}

		// Wrap in our Connection type and cache it
		let cached_conn = CachedConnection::new(conn);
		CACHE.store_connection(cache_key, cached_conn.clone()).await;

		Ok(cached_conn)
	}

	pub async fn commit_concurrent(conn: &CachedConnection) -> anyhow::Result<()> {
		conn.as_ref().execute("COMMIT", turso::params![]).await.map_err(Into::into).map(|_| ())
	}

	pub async fn rollback_concurrent(conn: &CachedConnection) -> Result<()> {
		match conn.as_ref().execute("ROLLBACK", turso::params![]).await {
			Ok(_) => Ok(()),
			Err(e) => Err(anyhow::anyhow!("Rollback failed: {}", e)),
		}
	}

	/// Configure database for MVCC concurrent writes
	pub async fn configure_database_for_mvcc(turso_db: &turso::Database) -> Result<()> {
		let conn = turso_db.connect()?;

		// Apply basic concurrent write configuration
		conn.execute("PRAGMA journal_mode=WAL", turso::params![]).await.ok();
		conn.execute("PRAGMA busy_timeout=600000", turso::params![]).await.ok(); // 10 minutes for large concurrent operations
		conn.execute("PRAGMA synchronous=NORMAL", turso::params![]).await.ok();
		conn.execute("PRAGMA temp_store=memory", turso::params![]).await.ok();
		conn.execute("PRAGMA wal_autocheckpoint=1000", turso::params![]).await.ok(); // Better WAL handling for large writes
		conn.execute("PRAGMA cache_size=-64000", turso::params![]).await.ok(); // 64MB cache for performance

		// Test BEGIN CONCURRENT support
		if conn.execute("BEGIN CONCURRENT", turso::params![]).await.is_ok() {
			conn.execute("ROLLBACK", turso::params![]).await.ok();
		}

		Ok(())
	}
}

#[async_trait::async_trait]
impl DatabaseStructure for Database {
	/// Create a Turso database for reuse with concurrent writes enabled
	async fn create_turso_database(db_path: &str) -> Result<turso::Database> {
		// Use get_or_create_turso_database for consistency and proper race condition handling
		Self::get_or_create_turso_database(db_path).await
	}

	/// Get a Turso database for reuse with concurrent writes enabled
	async fn get_turso_database(db_path: &str) -> Result<turso::Database> {
		// Check if the database is already in the cache, if so return it
		if let Some(turso_db) = CONNECTION_DATABASES.lock().await.get(db_path) {
			return Ok(turso_db.clone());
		}

		// If not in cache check if the database file exists on disk
		if Path::new(db_path).exists() {
			let turso_db = Builder::new_local(db_path).build().await?;

			// Configure database for concurrent writes
			{
				let conn = turso_db.connect()?;

				// Enable WAL mode - required for concurrent writes
				conn.execute("PRAGMA journal_mode=WAL", turso::params![]).await.ok();

				// Set busy timeout for handling transient locks
				conn.execute("PRAGMA busy_timeout = 30000", turso::params![]).await.ok();

				// Optimize for concurrent access
				conn.execute("PRAGMA synchronous = NORMAL", turso::params![]).await.ok();
			}

			CONNECTION_DATABASES.lock().await.insert(db_path.to_string(), turso_db.clone());
			return Ok(turso_db);
		}

		bail!("Database not found")
	}

	/// Get or create a Turso database with proper MVCC configuration and connection pooling
	async fn get_or_create_turso_database(db_path: &str) -> Result<turso::Database> {
		// Use a critical section to prevent race conditions during database creation
		let mut cache = CONNECTION_DATABASES.lock().await;

		// Check if the database is already in the cache, if so return it
		if let Some(turso_db) = cache.get(db_path) {
			return Ok(turso_db.clone());
		}

		// Create the database (file will be created if it doesn't exist)
		let turso_db = Builder::new_local(db_path).build().await?;

		// Configure database for MVCC concurrent writes
		Self::configure_database_for_mvcc(&turso_db).await?;

		cache.insert(db_path.to_string(), turso_db.clone());
		drop(cache);
		Ok(turso_db)
	}

	async fn record_transaction(&self, message: &str) -> Result<TxId> {
		let tx = Transaction::new(None, message.to_string());
		let tx_id = tx.id();

		// Since transaction logging is just for auditing and not critical to the operation,
		// spawn it as a background task to avoid blocking the main thread
		let tx_clone = tx.clone();

		// Use a more aggressive timeout and fewer retries since this is background logging
		let id_str = tx_clone.id().as_uuid().to_string();
		let msg = tx_clone.message().to_string();
		let created_at = tx_clone.created_at().timestamp_millis();

		let conn = Database::begin_concurrent(&self.metadata.clone(), self.metadata_path()).await?;

		// Simple INSERT without explicit transaction wrapper - SQLite handles this atomically
		let res = conn.as_ref().execute("INSERT INTO transactions (id, message, created_at) VALUES (?, ?, ?) ON CONFLICT(id) DO NOTHING", turso::params![id_str.clone(), msg.clone(), created_at]).await;

		match res {
			Ok(_) => println!("[TRACE] Background logged transaction id={}", id_str),
			Err(e) => {
				println!("[TRACE] Background transaction logging attempt failed: {}", e);
				Database::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to log transaction {}: {}", id_str, e));
			}
		}

		Database::commit_concurrent(&conn).await?;

		// Return immediately without waiting for the logging to complete
		Ok(tx_id)
	}

	async fn log_transaction(&self, transaction: &Transaction) -> Result<()> {
		// Since transaction logging is just for auditing, make it non-blocking
		let tx_clone = transaction.clone();
		let id_str = tx_clone.id().as_uuid().to_string();
		let msg = tx_clone.message().to_string();
		let created_at = tx_clone.created_at().timestamp_millis();

		let conn = Database::begin_concurrent(&self.metadata, self.metadata_path()).await?;

		// Create a fresh connection for each attempt

		// Simple INSERT without explicit transaction wrapper
		let res = conn.as_ref().execute("INSERT INTO transactions (id, message, created_at) VALUES (?, ?, ?) ON CONFLICT(id) DO NOTHING", turso::params![id_str.clone(), msg.clone(), created_at]).await;

		match res {
			Ok(_) => println!("[TRACE] Logged transaction id={}", id_str),
			Err(e) => {
				println!("[TRACE] Transaction logging attempt failed: {}", e);
				Database::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to log transaction {}: {}", id_str, e));
			}
		}

		Database::commit_concurrent(&conn).await?;

		// Return immediately without waiting for the logging to complete
		Ok(())
	}

	async fn metadata_database_create_transactions_table(db: &turso::Database, db_path: &str) -> Result<Transaction> {
		let conn = Self::begin_concurrent(db, db_path).await?;
		let res = conn
			.as_ref()
			.execute(
				r"CREATE TABLE IF NOT EXISTS transactions (
        			id TEXT PRIMARY KEY,
        			message TEXT NOT NULL,
        			created_at INTEGER NOT NULL
			)",
				turso::params![],
			)
			.await;

		match res {
			Ok(_) => println!("[DEBUG] Transactions table created or already exists"),
			Err(e) => {
				println!("[DEBUG] Failed to create transactions table: {}", e);
				Database::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure: `{}`", e));
			}
		}

		Database::commit_concurrent(&conn).await?;

		Ok(Transaction::new(None, "Create transactions table".to_string()))
	}

	async fn metadata_database_create_database_table(db: &turso::Database, db_path: &str) -> Result<Transaction> {
		let conn = Self::begin_concurrent(db, db_path).await?;
		let res = conn
			.as_ref()
			.execute(
				r"
			        CREATE TABLE IF NOT EXISTS database (
        				id TEXT PRIMARY KEY,
        				name TEXT NOT NULL,
        				created_at INTEGER NOT NULL,
					metadata_path TEXT NOT NULL
			        )
			",
				turso::params![],
			)
			.await;

		match res {
			Ok(_) => println!("[DEBUG] Database table created or already exists"),
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure: `{}`", e));
			}
		}
		Database::commit_concurrent(&conn).await?;

		Ok(Transaction::new(None, "Create database table".to_string()))
	}

	async fn metadata_database_create_subjects_table(db: &turso::Database, db_path: &str) -> Result<Transaction> {
		let conn = Self::begin_concurrent(db, db_path).await?;
		let res = conn
			.as_ref()
			.execute(
				r"
        			CREATE TABLE IF NOT EXISTS subjects (
        				id TEXT PRIMARY KEY,
        				database_id TEXT NOT NULL,
        				name TEXT NOT NULL,
        				created_at INTEGER NOT NULL
        			)
			",
				turso::params![],
			)
			.await;

		match res {
			Ok(_) => println!("[DEBUG] Subjects table created or already exists"),
			Err(e) => {
				println!("[DEBUG] Failed to create subjects table: {}", e);
				Database::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure: `{}`", e));
			}
		}

		Database::commit_concurrent(&conn).await?;

		Ok(Transaction::new(None, "Create subjects table".to_string()))
	}

	async fn metadata_database_create_aspects_table(db: &turso::Database, db_path: &str) -> Result<Transaction> {
		let conn = Database::begin_concurrent(db, db_path).await?;
		let res = conn
			.as_ref()
			.execute(
				r"
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
                	",
				turso::params![],
			)
			.await;

		match res {
			Ok(_) => println!("[DEBUG] Aspects table created or already exists"),
			Err(e) => {
				println!("[DEBUG] Failed to create aspects table: {}", e);
				Database::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure: `{}`", e));
			}
		}

		Database::commit_concurrent(&conn).await?;

		Ok(Transaction::new(None, "Create aspects table".to_string()))
	}

	async fn wireframe_metadata_database(db: &turso::Database, db_path: &str) -> Result<Vec<Transaction>> {
		println!("[DEBUG] Connecting to database for table creation...");
		println!("[DEBUG] Creating transactions table...");
		let create_transactions_table_transaction = Self::metadata_database_create_transactions_table(db, db_path).await?;
		println!("[DEBUG] Creating database table...");
		let create_database_table_transaction = Self::metadata_database_create_database_table(db, db_path).await?;
		println!("[DEBUG] Creating subjects table...");
		let create_subjects_table_transaction = Self::metadata_database_create_subjects_table(db, db_path).await?;
		println!("[DEBUG] Creating aspects table...");
		let create_aspects_table_transaction = Self::metadata_database_create_aspects_table(db, db_path).await?;
		println!("[DEBUG] All tables created successfully");

		#[rustfmt::skip]
		Ok(vec![
			create_transactions_table_transaction,
			create_database_table_transaction,
			create_subjects_table_transaction,
			create_aspects_table_transaction
		])
	}

	/// Creates a new database instance. Creates folder /{`data_dir}/{name`}.
	/// Keeps count of instances of `DB::existing({name`}) and manages read / write access as necessary.
	///
	/// # Errors
	/// - if folder /data/{name} already exists.
	async fn new(name: &str) -> Result<Self> {
		println!("[DEBUG] Creating new database: {name}");
		let data_dir = get_data_dir();
		let db_path = format!("{data_dir}/{name}");
		println!("[DEBUG] Database path: {db_path}");

		// Check if folder already exists
		if Path::new(&db_path).exists() {
			bail!("Database folder already exists: {}", db_path);
		}

		// Create the directory
		std::fs::create_dir_all(&db_path)?;
		println!("[DEBUG] Created directory: {db_path}");

		let db_id = DatabaseId::new();
		println!("[DEBUG] Generated database ID: {}", db_id.as_uuid());

		// Use shared connection database
		let metadata_db_path = format!("{db_path}/metadata.db");
		println!("[DEBUG] Creating Turso database at: {metadata_db_path}");
		let metadata_turso_db = Self::create_turso_database(&metadata_db_path).await?;
		println!("[DEBUG] Turso database created successfully");

		// Create metadata tables
		println!("[DEBUG] Creating metadata tables...");
		let mut transactions = Self::wireframe_metadata_database(&metadata_turso_db, &metadata_db_path).await?;
		println!("[DEBUG] Metadata tables created, {} transactions logged", transactions.len());

		// Insert database metadata using concurrent-safe transaction pattern
		println!("[DEBUG] Inserting database metadata...");

		let conn = Self::begin_concurrent(&metadata_turso_db, &metadata_db_path).await?;
		let exec_res = conn.as_ref().execute("INSERT INTO database (id, name, created_at, metadata_path) VALUES (?, ?, ?, ?)", turso::params![db_id.as_uuid().to_string(), name.to_string(), chrono::Utc::now().timestamp_millis(), metadata_db_path.clone()]).await;

		match exec_res {
			Ok(_) => println!("[DEBUG] Database metadata inserted successfully"),
			Err(e) => {
				println!("[DEBUG] Failed to insert database metadata: {}", e);
				Database::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure: `{}`", e));
			}
		}

		Database::commit_concurrent(&conn).await?;

		drop(conn);

		transactions.push(Transaction::new(None, "Insert database metadata".to_string()));

		let mut db_info = DatabaseInfo::new(name.to_string(), db_path);
		db_info.set_metadata(Some(metadata_turso_db.clone()));
		DATABASES.lock().await.insert(db_id, db_info);

		let db = Self { id: db_id, name: name.to_string(), metadata: metadata_turso_db, metadata_path: metadata_db_path };

		for transaction in transactions {
			db.log_transaction(&transaction).await?;
		}

		Ok(db)
	}

	/// Helper function to convert Turso values to strings, handling different storage formats
	async fn value_to_string(value: &turso::Value, field_name: &str) -> Result<String> {
		match value {
			turso::Value::Text(s) => Ok(s.clone()),
			turso::Value::Blob(bytes) => {
				// Try to interpret as UUID bytes
				if bytes.len() == 16 {
					let uuid = uuid::Uuid::from_bytes(bytes.as_slice().try_into().map_err(|_| anyhow::anyhow!("{} blob is not 16 bytes", field_name))?);
					Ok(uuid.to_string())
				} else {
					String::from_utf8(bytes.clone()).map_err(|e| anyhow::anyhow!("{} blob is not valid UTF-8: {}", field_name, e))
				}
			}
			turso::Value::Integer(i) => Ok(i.to_string()),
			turso::Value::Real(r) => Ok(r.to_string()),
			turso::Value::Null => Err(anyhow::anyhow!("{} is null", field_name)),
		}
	}

	fn id(&self) -> DatabaseId {
		self.id
	}

	fn name(&self) -> &str {
		&self.name
	}

	fn metadata(&self) -> &turso::Database {
		&self.metadata
	}

	fn metadata_path(&self) -> &str {
		&self.metadata_path
	}

	/// Loads an instance of DB from /{`data_dir}/{name`}.
	/// Maps Subjects and loads cache.
	/// Keeps count of instances of `DB::existing({name`}) and manages read / write access as necessary.
	///
	/// # Errors
	/// - if folder /data/{name} does not exist
	/// - if database connection fails
	/// - if unable to query existing tables
	async fn existing(name: &str) -> Result<Self> {
		let data_dir = get_data_dir();
		let db_path = format!("{data_dir}/{name}");

		// Check if folder exists
		if !Path::new(&db_path).exists() {
			bail!("Database folder does not exist: {}", db_path);
		}

		// Use shared connection database
		let metadata_db_path = &format!("{db_path}/metadata.db");
		let metadata_turso_db = Self::get_turso_database(metadata_db_path).await?;

		// Query database ID from metadata
		let conn = Self::begin_concurrent(&metadata_turso_db, &metadata_db_path).await?;
		let mut rows = conn.as_ref().query("SELECT id FROM database WHERE name = ?", turso::params![name]).await?;
		let row = rows.next().await?.ok_or_else(|| anyhow::anyhow!("Database not found"))?;

		let db_id_str = Self::value_to_string(&row.get_value(0)?, "DB ID").await?;
		let db_id = DatabaseId::from_uuid(Uuid::parse_str(&db_id_str)?);

		let mut subject_rows = conn.as_ref().query("SELECT id, database_id, name, created_at FROM subjects", ()).await?;

		let mut db_info = DatabaseInfo::new(name.to_string(), db_path.clone());
		db_info.set_id(db_id);
		db_info.set_metadata(Some(metadata_turso_db.clone()));
		let _ = db_info.set_metadata_path(Some(metadata_db_path.clone()));

		// Manually construct subjects from rows
		while let Some(row) = subject_rows.next().await? {
			let subject_id_str = Self::value_to_string(&row.get_value(0)?, "Subject ID").await?;
			let subject_name = Self::value_to_string(&row.get_value(2)?, "Subject name").await?;
			let subject_id = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str)?);
			let mut subject = Subject::new(Some(subject_id), subject_name.clone(), db_id, metadata_db_path.clone()).await?;

			// Load aspects for this subject
			let mut aspect_rows = conn.as_ref().query("SELECT id, name, table_name, resolution FROM aspects WHERE subject_id = ?", turso::params![subject_id.as_uuid().to_string()]).await?;

			while let Some(aspect_row) = aspect_rows.next().await? {
				let aspect_id_str = Self::value_to_string(&aspect_row.get_value(0)?, "Aspect ID").await?;
				let aspect_name = Self::value_to_string(&aspect_row.get_value(1)?, "Aspect name").await?;
				let resolution_str = Self::value_to_string(&aspect_row.get_value(3)?, "Resolution").await?;
				let aspect_id = AspectId::from_uuid(Uuid::parse_str(&aspect_id_str)?);
				let resolution: Resolution = serde_json::from_str(&resolution_str)?;
                                

				let aspect = Aspect::new(Some(aspect_id), aspect_name, subject_id, db_info.name().to_string(), resolution, metadata_db_path.to_string()).await?;
				subject.add_aspect(aspect);
			}

			db_info.add_subject(subject);
		}

		Self::commit_concurrent(&conn).await?;

		DATABASES.lock().await.insert(db_id, db_info);

		Ok(Self { id: db_id, name: name.to_string(), metadata: metadata_turso_db, metadata_path: db_path.clone() })
	}

	async fn get_database_info(&self) -> Result<DatabaseInfo> {
		DATABASES.lock().await.get(&self.id).cloned().ok_or_else(|| anyhow::anyhow!("Database info not found for ID: {}", self.id.as_uuid()))
	}

	/// Closes the database, releasing all resources and removing from global map
	///
	/// # Errors
	///
	/// Returns an error if there are issues closing connection pools or removing resources.
	async fn close(&self) -> Result<()> {
		{
			let mut databases = DATABASES.lock().await;
			if let Some(_db_info) = databases.remove(&self.id) {
				// Turso databases don't need explicit closing like SQLx pools
				// The connections will be closed when dropped
			}
		}

		// Also remove from connection databases
		let db_path = format!("{}/{}/metadata.db", get_data_dir(), self.name);
		{
			let mut connection_databases = CONNECTION_DATABASES.lock().await;
			connection_databases.remove(&db_path);
		}

		// Wait for database files to be actually released
		self.wait_for_database_release().await?;

		Ok(())
	}

	/// Wait for the database files to be released by checking if we can delete them
	async fn wait_for_database_release(&self) -> Result<()> {
		const MAX_ATTEMPTS: u32 = 50; // 5 seconds total
		const DELAY_MS: u64 = 100;

		let db_dir = format!("{}/{}", get_data_dir(), self.name);
		let metadata_db_path = format!("{db_dir}/metadata.db");

		for attempt in 0..MAX_ATTEMPTS {
			// Try to open the database file exclusively to check if it's still locked
			if let Err(e) = std::fs::OpenOptions::new().write(true).truncate(false).open(&metadata_db_path) {
				if e.kind() == std::io::ErrorKind::PermissionDenied {
					// File is still locked, wait and retry
					if attempt == MAX_ATTEMPTS - 1 {
						// Last attempt failed, but don't error - just log
						eprintln!("Warning: Database may still be locked after {MAX_ATTEMPTS} attempts: {metadata_db_path}");
						return Ok(());
					}
					tokio::time::sleep(std::time::Duration::from_millis(DELAY_MS)).await;
				} else {
					// Other error (file doesn't exist, etc.) - consider it released
					return Ok(());
				}
			} else {
				// File is accessible, database is released
				return Ok(());
			}
		}

		Ok(())
	}

	/// Helper to establish database connection with retry logic
	async fn connect_with_retry(turso_db: &turso::Database, attempts: i32, max_attempts: i32) -> Result<turso::Connection> {
		match turso_db.connect() {
			Ok(c) => Ok(c),
			Err(e) if attempts < max_attempts => {
				let error_msg = e.to_string().to_lowercase();
				if error_msg.contains("i/o error") || error_msg.contains("unexpected end of file") {
					if attempts == 1 {
						eprintln!("⚠️  Database appears to be locked by another application (like DB Browser). Retrying...");
					}
					tokio::time::sleep(std::time::Duration::from_secs(2)).await;
				} else {
					tokio::time::sleep(std::time::Duration::from_millis(200)).await;
				}
				Err(anyhow::anyhow!("Retry needed"))
			}
			Err(e) => {
				let error_msg = e.to_string().to_lowercase();
				if error_msg.contains("i/o error") || error_msg.contains("unexpected end of file") {
					eprintln!("\n❌ Database locked after {max_attempts} attempts. Close DB Browser and try again.\n");
				}
				Err(Error::DatabaseError(format!("Failed to connect after {max_attempts} attempts: {e}")).into())
			}
		}
	}

	/// Observe a subject by name, creating if it doesn't exist
	///
	/// 1. Create subject folder.
	/// 2. Add subject to database metadata.
	///
	/// # Errors
	/// 1. Returns error if the subject already exists
	async fn observe_subject(&self, name: &str) -> Result<Subject> {
		println!("[DEBUG] Observing subject: {name}");
		// Check if subject already exists
		if self.get_subject_by_name(name).await.is_ok() {
			return Err(anyhow::anyhow!("Subject '{}' already exists", name));
		}

		// Create subject folder
		let subject_path = format!("{}/{}/{}", get_data_dir(), self.name, name);
		println!("[DEBUG] Creating subject folder: {subject_path}");
		tokio::fs::create_dir_all(&subject_path).await?;

		// Add subject to database metadata
		let metadata_db = &self.metadata;
		let metadata_db_path = self.metadata_path.clone();

		// create subject
		println!("[DEBUG] Creating Subject instance...");
		let subject = Subject::new(None, name.to_string(), self.id, self.metadata_path.clone()).await?;
		println!("[DEBUG] Subject created with ID: {}", subject.id().as_uuid());

		// Add subject to metadata database
		println!("[DEBUG] Inserting subject into database...");
		let id_str = subject.id().as_uuid().to_string();
		let name_str = name.to_string();
		let db_id_str = self.id.as_uuid().to_string();
		let created_at = chrono::Utc::now().timestamp_millis();

		let conn = Self::begin_concurrent(&metadata_db, &metadata_db_path).await?;
		let res = conn.as_ref().execute("INSERT INTO subjects (id, name, database_id, created_at) VALUES (?, ?, ?, ?)", turso::params![id_str.clone(), name_str.clone(), db_id_str.clone(), created_at]).await;
		match res {
			Ok(_) => println!("[DEBUG] Subject inserted successfully"),
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure: `{}`", e));
			}
		}

		Self::commit_concurrent(&conn).await?;
		println!("[DEBUG] Subject inserted successfully");

		// Update in-memory cache so subsequent operations don't need metadata DB
		{
			let mut dbs = DATABASES.lock().await;
			if let Some(info) = dbs.get_mut(&self.id) {
				info.add_subject(subject.clone());
			}
		}

		// Log the transaction
		self.record_transaction(&format!("Added subject '{name}'")).await?;

		Ok(subject)
	}

	/// Get a subject by its ID
	async fn get_subject(&self, id: SubjectId) -> Result<Subject> {
		// Read-only subject lookup with busy_timeout and retry; avoid transactions to reduce lock contention
		println!("[DEBUG] Getting subject by ID: {}", id.as_uuid());

		let mut attempts = 0u32;
		const MAX_ATTEMPTS: u32 = 40;

		loop {
			attempts += 1;
			let conn = match self.metadata.connect() {
				Ok(c) => c,
				Err(e) => {
					let em = e.to_string().to_lowercase();
					if attempts < MAX_ATTEMPTS && (em.contains("locked") || em.contains("busy")) {
						tokio::time::sleep(std::time::Duration::from_millis(25 * attempts as u64)).await;
						continue;
					}
					return Err(anyhow::anyhow!("Failed to connect to metadata DB in get_subject: {e}"));
				}
			};

			println!("[DEBUG] Connected to metadata DB, querying for subject ID {}", id.as_uuid());
			let _ = conn.execute("PRAGMA busy_timeout=600000", turso::params![]).await;

			let query_res = conn.query("SELECT id, name, database_id FROM subjects WHERE id = ?", turso::params![id.as_uuid().to_string()]).await;

			match query_res {
				Ok(mut rows) => match rows.next().await? {
					Some(row) => {
						println!("[DEBUG] Found subject row for ID {}", id.as_uuid());
						let subject_id_str = Self::value_to_string(&row.get_value(0)?, "Subject ID").await?;
						let name = Self::value_to_string(&row.get_value(1)?, "Subject name").await?;
						let database_id_str = Self::value_to_string(&row.get_value(2)?, "Database ID").await?;

						let subject_id = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str)?);
						let database_id = DatabaseId::from_uuid(Uuid::parse_str(&database_id_str)?);
						return Subject::new(Some(subject_id), name, database_id, self.metadata_path.clone()).await;
					}
					None => {
						println!("[DEBUG] Subject ID {} not found in database", id.as_uuid());
						return Err(anyhow::anyhow!("Subject not found"));
					}
				},
				Err(e) => {
					let em = e.to_string().to_lowercase();
					if attempts < MAX_ATTEMPTS && (em.contains("locked") || em.contains("busy") || em.contains("conflict")) {
						let delay = 25u64.saturating_mul(1u64 << (attempts.min(7)));
						tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
						continue;
					}
					println!("[ERROR] SQL execution failure in get_subject for ID {}: {}", id.as_uuid(), e);
					return Err(anyhow::anyhow!("SQL execution failure in get_subject: `{}`", e));
				}
			}
		}
	}

	/// Get a subject by its name
	async fn get_subject_by_name(&self, name: &str) -> Result<Subject> {
		// Read-only subject lookup with busy_timeout and retry; avoid transactions to reduce lock contention
		let mut attempts = 0u32;
		const MAX_ATTEMPTS: u32 = 40;
		loop {
			attempts += 1;
			let conn = match self.metadata.connect() {
				Ok(c) => c,
				Err(e) => {
					let em = e.to_string().to_lowercase();
					if attempts < MAX_ATTEMPTS && (em.contains("locked") || em.contains("busy") || em.contains("conflict")) {
						let delay = 25u64.saturating_mul(1u64 << (attempts.min(7)));
						tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
						continue;
					}
					return Err(anyhow::anyhow!("Failed to connect to metadata DB in get_subject_by_name: {e}"));
				}
			};

			let _ = conn.execute("PRAGMA busy_timeout=600000", turso::params![]).await;
			let mut rows = conn.query("SELECT id, name, database_id FROM subjects WHERE name = ?", turso::params![name]).await?;
			if let Some(row) = rows.next().await? {
				let subject_id_str = Self::value_to_string(&row.get_value(0)?, "Subject ID").await?;
				let name = Self::value_to_string(&row.get_value(1)?, "Subject name").await?;
				let database_id_str = Self::value_to_string(&row.get_value(2)?, "Database ID").await?;

				let subject_id = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str)?);
				let database_id = DatabaseId::from_uuid(Uuid::parse_str(&database_id_str)?);
				return Ok(Subject::new(Some(subject_id), name, database_id, self.metadata_path.clone()).await?);
			} else {
				return Err(anyhow::anyhow!("Subject not found"));
			}
		}
	}

	/// Remove a subject from observation
	///
	/// 1. Delete the subject folder and all child files and folders.
	/// 2. Remove subject from database metadata.
	///
	/// # Errors
	/// Returns an error if the subject does not exist
	async fn remove_subject(&self, id: SubjectId) -> Result<()> {
		// Retrieve subject from metadata database
		let subject = self.get_subject(id).await?;
		let metadata_db = &self.metadata;
		let metadata_db_path = self.metadata_path.clone();

		// Delete the subject folder and all child files and folders
		let subject_path = format!("{}/{}/{}", get_data_dir(), self.name, subject.name());
		tokio::fs::remove_dir_all(&subject_path).await?;

		// Remove subject from database metadata
		let id_str = id.as_uuid().to_string();

		let conn = Self::begin_concurrent(metadata_db, &metadata_db_path).await?;
		let res = conn.as_ref().execute("DELETE FROM subjects WHERE id = ?", turso::params![id_str.clone()]).await;
		match res {
			Ok(_) => println!("Deleted subject with ID {}", id.as_uuid()),
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure: `{}`", e));
			}
		}
		Self::commit_concurrent(&conn).await?;

		// Log the transaction
		self.record_transaction(&format!("Removed subject '{}'", subject.name())).await?;

		Ok(())
	}

	/// List all subjects in the database
	async fn list_subjects(&self) -> Result<Vec<Subject>> {
		let conn = Self::begin_concurrent(&self.metadata, &self.metadata_path).await?;
		let res = conn.as_ref().query("SELECT id, name, database_id FROM subjects WHERE database_id = ?", turso::params![self.id.as_uuid().to_string()]).await;
		let mut rows = match res {
			Ok(rows) => rows,
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure: `{}`", e));
			}
		};

		Self::commit_concurrent(&conn).await?;

		let mut raw_rows: Vec<(String, String, String)> = Vec::new();
		while let Some(row) = rows.next().await? {
			let subject_id_str = Self::value_to_string(&row.get_value(0)?, "Subject ID").await?;
			let name = Self::value_to_string(&row.get_value(1)?, "Subject name").await?;
			let database_id_str = Self::value_to_string(&row.get_value(2)?, "Database ID").await?;
			raw_rows.push((subject_id_str, name, database_id_str));
		}

		let mut subjects = Vec::new();
		for (subject_id_str, name, database_id_str) in raw_rows {
			let subject_id = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str)?);
			let database_id = DatabaseId::from_uuid(Uuid::parse_str(&database_id_str)?);
			subjects.push(Subject::new(Some(subject_id), name, database_id, self.metadata_path.clone()).await?);
		}

		Ok(subjects)
	}

	/// List all tracked aspects of a subject
	async fn list_aspects(&self, subject_id: SubjectId) -> Result<Vec<Aspect>> {
		let mut attempts = 0u32;
		const MAX_ATTEMPTS: u32 = 40;
		loop {
			attempts += 1;
			let conn = match self.metadata.connect() {
				Ok(c) => c,
				Err(e) => {
					if attempts < MAX_ATTEMPTS && e.to_string().to_lowercase().contains("locked") {
						tokio::time::sleep(std::time::Duration::from_millis(25 * attempts as u64)).await;
						continue;
					}
					return Err(anyhow::anyhow!("Failed to connect to metadata DB in list_aspects: {e}"));
				}
			};

			let _ = conn.execute("PRAGMA busy_timeout=600000", turso::params![]).await;
			let result = conn.query("SELECT id, name, subject_id, resolution FROM aspects WHERE subject_id = ?", turso::params![subject_id.as_uuid().to_string()]).await;

			match result {
				Ok(mut rows) => {
					let mut raw_rows: Vec<(String, String, String, String)> = Vec::new();
					while let Some(row) = rows.next().await? {
						let aspect_id_str = Self::value_to_string(&row.get_value(0)?, "Aspect ID").await?;
						let name = Self::value_to_string(&row.get_value(1)?, "Aspect name").await?;
						let subject_id_str = Self::value_to_string(&row.get_value(2)?, "Subject ID").await?;
						let resolution_str = Self::value_to_string(&row.get_value(3)?, "Resolution").await?;
						raw_rows.push((aspect_id_str, name, subject_id_str, resolution_str));
					}

					let mut aspects = Vec::new();
					for (aspect_id_str, name, subject_id_str, resolution_str) in raw_rows {
						let aspect_id = AspectId::from_uuid(Uuid::parse_str(&aspect_id_str)?);
						let subject_id_parsed = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str)?);
						let resolution: Resolution = serde_json::from_str(&resolution_str)?;
						aspects.push(Aspect::from_metadata(Some(aspect_id), name, subject_id_parsed, resolution, self.metadata_path.clone(), None).await?);
					}
					return Ok(aspects);
				}
				Err(e) => {
					let em = e.to_string().to_lowercase();
					if attempts < MAX_ATTEMPTS && (em.contains("locked") || em.contains("busy") || em.contains("conflict")) {
						let delay = 25u64.saturating_mul(1u64 << (attempts.min(7)));
						tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
						continue;
					}
					return Err(anyhow::anyhow!("SQL execution failure in list_aspects: `{}`", e));
				}
			}
		}
	}

	/// Track a new aspect for a subject
	///
	/// 1. Create a new aspect folder.
	/// 2. Add aspect to database metadata.
	/// 3. Initialize and wireframe aspect data stores.
	///
	/// # Errors
	/// Returns an error if the subject does not exist or aspect creation fails or the aspect already exists.
	async fn track_aspect(&self, subject_id: SubjectId, name: &str, resolution: Resolution) -> Result<Aspect> {
		println!("[DEBUG] Tracking aspect '{}' for subject {}", name, subject_id.as_uuid());
		// Check if subject exists
		println!("[DEBUG] Retrieving subject with ID: {}", subject_id.as_uuid());
		let subject = self.get_subject(subject_id).await?;

		// Check if aspect already exists for the subject
		println!("[DEBUG] Checking existing aspects for subject '{}'", subject.name());
		let existing_aspects = self.list_aspects(subject_id).await?;
		if existing_aspects.iter().any(|a| a.name() == name) {
			return Err(anyhow::anyhow!("Aspect '{}' already exists for subject '{}'", name, subject.name()));
		}

		// Create a new aspect folder
		let aspect_path = format!("{}/{}/{}/{}", get_data_dir(), self.name, subject.name(), name);
		println!("[DEBUG] Creating aspect folder: {aspect_path}");
		tokio::fs::create_dir_all(&aspect_path).await?;

		// Add aspect to database metadata
		let aspect_id = AspectId::new();
		println!("[DEBUG] Generated aspect ID: {}", aspect_id.as_uuid());

		let table_name = format!("aspect_{}_{}", subject_id.as_uuid().to_string().replace('-', "_"), name.replace([' ', '-'], "_"));
		println!("[DEBUG] Inserting aspect into database with table_name: {table_name}");

		let aspect_id_str = aspect_id.as_uuid().to_string();
		let name_str = name.to_string();
		let subj_id_str = subject_id.as_uuid().to_string();
		let db_id_str = self.id.as_uuid().to_string();
		let resolution_json = serde_json::to_string(&resolution)?;
		let created_at = chrono::Utc::now().timestamp_millis();

		// Establish a fresh connection for this attempt while holding the mutex
		let metadata_db_conn = self.metadata();
		let metadata_db_path = self.metadata_path.clone();

		// Begin transaction on this connection
		let conn = Self::begin_concurrent(metadata_db_conn, &metadata_db_path).await?;
		println!("[TRACE] About to execute INSERT INTO aspects (aspect_id={aspect_id_str} table_name={table_name})");

		// Execute the INSERT while holding the mutex
		conn.as_ref().execute("INSERT INTO aspects (id, name, subject_id, database_id, table_name, resolution, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)", turso::params![aspect_id_str.clone(), name_str.clone(), subj_id_str.clone(), db_id_str.clone(), table_name.clone(), resolution_json.clone(), created_at]).await?;

		Self::commit_concurrent(&conn).await?;

		drop(conn); // Explicitly drop the connection to release locks

		println!("[DEBUG] Aspect inserted successfully");

		// At this point the metadata INSERT succeeded and we've dropped the metadata connection.
		// Release the aspect creation mutex (it was locked above) implicitly by letting the
		// earlier guard drop (we only held it across the insertion). Now perform the
		// heavier wireframing without holding the metadata lock.
		let aspect = Aspect::new(Some(aspect_id), name.to_string(), subject_id, self.name.clone(), resolution, self.metadata_path.clone()).await?;

		// Update in-memory cache with the newly created aspect to avoid future metadata reads
		{
			let mut dbs = DATABASES.lock().await;
			if let Some(info) = dbs.get_mut(&self.id) {
				// Ensure subject exists in cache; use the subject we already loaded above
				if !info.subjects().contains_key(&subject_id) {
					info.add_subject(subject.clone());
				}
				if let Some(subj) = info.subjects_mut().get_mut(&subject_id) {
					subj.add_aspect(aspect.clone());
				}
			}
		}

		println!("Aspect created and wireframed successfully: {}", aspect.name());

		// Log the transaction
		self.record_transaction(&format!("Tracking new aspect '{}' for subject '{}'", name, subject.name())).await?;

		println!("Transaction recorded successfully for aspect creation.");

		Ok(aspect)
	}

	async fn get_aspect(&self, id: AspectId) -> Result<Aspect> {
		// Fast path: try in-memory cache first to avoid touching metadata DB during heavy writes
		if let Some(cached) = {
			let dbs = DATABASES.lock().await;
			dbs.get(&self.id).and_then(|info| info.subjects().values().find_map(|s| s.aspects().get(&id).cloned()))
		} {
			return Ok(cached);
		}

		// Use a simple, short-lived connection for metadata reads to avoid shared-connection contention
		let mut attempts = 0u32;
		const MAX_ATTEMPTS: u32 = 40; // allow ample time while large batch setup finishes

		loop {
			attempts += 1;
			let conn = match self.metadata.connect() {
				Ok(c) => c,
				Err(e) => {
					if attempts < MAX_ATTEMPTS && e.to_string().to_lowercase().contains("locked") {
						tokio::time::sleep(std::time::Duration::from_millis(25 * attempts as u64)).await;
						continue;
					}
					return Err(anyhow::anyhow!("Failed to connect to metadata DB: {e}"));
				}
			};

			// Ensure this connection waits on locks instead of failing immediately
			let _ = conn.execute("PRAGMA busy_timeout=600000", turso::params![]).await; // 10 minutes

			// No explicit transaction for a read-only, single-row query; add retry on transient locks
			let query_res = conn
				.query(
					"SELECT a.id, a.name, a.subject_id, a.resolution, s.name AS subject_name \
					 FROM aspects a JOIN subjects s ON s.id = a.subject_id \
					 WHERE a.id = ?",
					turso::params![id.as_uuid().to_string()],
				)
				.await;

			match query_res {
				Ok(mut rows) => {
					if let Some(row) = rows.next().await? {
						let aspect_id_str = Self::value_to_string(&row.get_value(0)?, "Aspect ID").await?;
						let name = Self::value_to_string(&row.get_value(1)?, "Aspect name").await?;
						let subject_id_str = Self::value_to_string(&row.get_value(2)?, "Subject ID").await?;
						let resolution_str = Self::value_to_string(&row.get_value(3)?, "Resolution").await?;
						let aspect_id = AspectId::from_uuid(Uuid::parse_str(&aspect_id_str)?);
						let subject_id = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str)?);
						let resolution: Resolution = serde_json::from_str(&resolution_str)?;

						return Aspect::new(Some(aspect_id), name, subject_id, self.name.clone(), resolution, self.metadata_path.clone()).await;
					} else {
						bail!("Aspect not found");
					}
				}
				Err(e) => {
					let em = e.to_string().to_lowercase();
					if attempts < MAX_ATTEMPTS && (em.contains("locked") || em.contains("busy") || em.contains("conflict")) {
						// exponential backoff up to ~2s
						let delay = 25u64.saturating_mul(1u64 << (attempts.min(7)));
						tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
						continue;
					}
					return Err(anyhow::anyhow!("SQL execution failure in get_aspect: `{}`", e));
				}
			}
		}
	}

	async fn get_aspect_by_name(&self, name: &str) -> Result<Aspect> {
		// Use a simple, short-lived connection for metadata reads to avoid shared-connection contention
		let mut attempts = 0u32;
		const MAX_ATTEMPTS: u32 = 40; // allow ample time while large batch setup finishes

		loop {
			attempts += 1;
			let conn = Self::begin_concurrent(&self.metadata, &self.metadata_path).await?;

			// Ensure this connection waits on locks instead of failing immediately
			let _ = conn.as_ref().execute("PRAGMA busy_timeout=600000", turso::params![]).await;


			let query_res = conn.as_ref().query(
					"SELECT a.id, a.name, a.subject_id, a.resolution, s.name AS subject_name \
					 FROM aspects a JOIN subjects s ON s.id = a.subject_id \
					 WHERE a.name = ?",
					turso::params![name],
				)
				.await;

			match query_res {
				Ok(mut rows) => {
					if let Some(row) = rows.next().await? {
						let aspect_id_str = Self::value_to_string(&row.get_value(0)?, "Aspect ID").await?;
						let name = Self::value_to_string(&row.get_value(1)?, "Aspect name").await?;
						let subject_id_str = Self::value_to_string(&row.get_value(2)?, "Subject ID").await?;
						let resolution_str = Self::value_to_string(&row.get_value(3)?, "Resolution").await?;

						let aspect_id = AspectId::from_uuid(Uuid::parse_str(&aspect_id_str)?);
						let subject_id = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str)?);
						let resolution: Resolution = serde_json::from_str(&resolution_str)?;

						return Aspect::new(Some(aspect_id), name, subject_id, self.name.clone(), resolution, self.metadata_path.clone()).await;
					} else {
						bail!("Aspect not found");
					}
				}
				Err(e) => {
					let em = e.to_string().to_lowercase();
					if attempts < MAX_ATTEMPTS && (em.contains("locked") || em.contains("busy") || em.contains("conflict")) {
						let delay = 25u64.saturating_mul(1u64 << (attempts.min(7)));
						tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
						continue;
					}
					return Err(anyhow::anyhow!("SQL execution failure in get_aspect_by_name: `{}`", e));
				}
			}
		}
	}

	async fn get_earliest_measurement(&self, aspect_id: &AspectId) -> Result<Option<DateTime<Utc>>> {
		let cache_key = format!("metadata_earliest_measurement_{}", aspect_id.as_uuid());
		let conn = Self::begin_concurrent(&self.metadata, &cache_key).await?;

		let res = conn.as_ref().query("SELECT MIN(timestamp) FROM measurements WHERE aspect_id = ?", turso::params![aspect_id.as_uuid().to_string()]).await;

		let timestamp = match res {
			Ok(mut rows) => {
				if let Some(row) = rows.next().await? {
					let timestamp_str = Self::value_to_string(&row.get_value(0)?, "Earliest Measurement Timestamp").await?;
					if timestamp_str.is_empty() {
						return Ok(None);
					}
					let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)?.with_timezone(&Utc);
					timestamp
				} else {
					return Ok(None);
				}
			}
			Err(e) => {
				let _ = Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure in get_earliest_measurement: `{}`", e));
			}
		};

		Self::commit_concurrent(&conn).await?;
		Ok(Some(timestamp))
	}

	async fn get_latest_measurement(&self, aspect_id: &AspectId) -> Result<Option<DateTime<Utc>>> {
		let metadata_db = &self.metadata;
		let metadata_db_path = self.metadata_path.clone();
		let conn = Self::begin_concurrent(&metadata_db, &metadata_db_path).await?;
		let res = conn.as_ref().query("SELECT MAX(timestamp) FROM measurements WHERE aspect_id = ?", turso::params![aspect_id.as_uuid().to_string()]).await;
		match res {
			Ok(mut rows) => {
				if let Some(row) = rows.next().await? {
					Self::commit_concurrent(&conn).await?;
					let timestamp_str = Self::value_to_string(&row.get_value(0)?, "Latest Measurement Timestamp").await?;
					if timestamp_str.is_empty() {
						return Ok(None);
					}
					let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)?.with_timezone(&Utc);
					return Ok(Some(timestamp));
				}
				return Ok(None);
			}
			Err(e) => {
				let _ = Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure in get_latest_measurement: `{}`", e));
			}
		}
	}

	async fn get_aspect_resolution(&self, aspect_id: &AspectId) -> Result<Resolution> {
		let aspect = self.get_aspect(*aspect_id).await?;
		let resolution = aspect.resolution();
		Ok(resolution)
	}

	async fn update_aspect_timestamps(&self, aspect_id: &AspectId, min_new: DateTime<Utc>, max_new: DateTime<Utc>) -> Result<()> {
		let metadata_db = &self.metadata;
		let metadata_db_path = self.metadata_path.clone();

		let conn = Self::begin_concurrent(&metadata_db, &metadata_db_path).await?;
		let res = conn.as_ref().execute("UPDATE aspects SET earliest_measurement = ?, latest_measurement = ? WHERE id = ?", turso::params![min_new.to_rfc3339(), max_new.to_rfc3339(), aspect_id.as_uuid().to_string()]).await;
		match res {
			Ok(_) => println!("Updated aspect timestamps for aspect {}", aspect_id),
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure: `{}`", e));
			}
		}
		Self::commit_concurrent(&conn).await?;
		Ok(())
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DatabaseId(Uuid);

impl DatabaseId {
	pub fn new() -> Self {
		Self(Uuid::new_v4())
	}

	pub fn from_uuid(uuid: Uuid) -> Self {
		Self(uuid)
	}

	pub fn as_uuid(&self) -> Uuid {
		self.0
	}
}

impl Display for DatabaseId {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		write!(f, "{}", self.0)
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
	metadata: Option<turso::Database>,
	metadata_path: Option<String>,
}

impl PartialEq for DatabaseInfo {
	fn eq(&self, other: &Self) -> bool {
		self.id == other.id && self.name == other.name && self.path == other.path && self.subjects == other.subjects
		// Skip metadata comparison since it doesn't implement PartialEq
	}
}

impl Eq for DatabaseInfo {}

#[derive(Debug, Clone, Default)]
pub struct DatabaseStats {
	pub subject_count: usize,
	pub aspect_count: usize,
}

impl DatabaseInfo {
	#[must_use]
	pub fn new(name: String, path: String) -> Self {
		Self { id: DatabaseId::default(), name, path, subjects: HashMap::new(), metadata: None, metadata_path: None }
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
	pub fn metadata(&self) -> Option<&turso::Database> {
		self.metadata.as_ref()
	}

	pub fn set_metadata(&mut self, turso_db: Option<turso::Database>) {
		self.metadata = turso_db;
	}

	#[must_use]
	pub const fn metadata_path(&self) -> &Option<String> {
		&self.metadata_path
	}

	#[must_use]
	pub fn set_metadata_path(&mut self, path: Option<String>) {
		self.metadata_path = path;
	}

	pub const fn subjects_mut(&mut self) -> &mut HashMap<SubjectId, Subject> {
		&mut self.subjects
	}

	/// Get subject by name
	#[must_use]
	pub fn get_subject_by_name(&self, name: &str) -> Option<&Subject> {
		self.subjects.values().find(|s| s.name() == name)
	}

	/// Get total number of aspects across all subjects
	#[must_use]
	pub fn total_aspects(&self) -> usize {
		self.subjects.values().map(|s| s.aspects().len()).sum()
	}

	/// Check if database is empty (no subjects)
	#[must_use]
	pub fn is_empty(&self) -> bool {
		self.subjects.is_empty()
	}

	/// Get creation timestamp if available
	///
	/// # Errors
	/// - If database connection fails
	/// - If timestamp parsing fails
	pub async fn get_creation_time(&self) -> Result<Option<DateTime<Utc>>> {
		if let Some(turso_db) = &self.metadata {
			if let Some(cache_key) = &self.metadata_path {
				let conn = Database::begin_concurrent(turso_db, cache_key).await?;
				let res = conn.as_ref().query("SELECT created_at FROM database WHERE name = ?", turso::params![self.name.clone()]).await;
				let timestamp = match res {
					Ok(mut rows) => {
						println!("[DEBUG] Querying creation time for database '{}'", self.name);
						if let Some(row) = rows.next().await? {
							let timestamp_str = Database::value_to_string(&row.get_value(0)?, "Creation Timestamp").await?;
							if timestamp_str.is_empty() {
								Database::commit_concurrent(&conn).await?;
								return Ok(None);
							}
							DateTime::parse_from_rfc3339(&timestamp_str)?.with_timezone(&Utc)
						} else {
							Database::commit_concurrent(&conn).await?;
							return Ok(None);
						}
					}
					Err(e) => {
						Database::rollback_concurrent(&conn).await?;
						return Err(anyhow::anyhow!("SQL execution failure in get_creation_time: `{}`", e));
					}
				};

				Database::commit_concurrent(&conn).await?;
				return Ok(Some(timestamp));
			}
		}
		Ok(None)
	}

	/// Get database size statistics
	///
	/// # Errors
	/// - If database connection fails
	/// - If count parsing fails
	pub async fn get_size_stats(&self) -> Result<DatabaseStats> {
		let Some(metadata_db) = &self.metadata else {
			bail!("Metadata database connection not available");
		};

		let Some(metadata_db_path) = &self.metadata_path else {
			bail!("Metadata database path not found");
		};

		let mut stats = DatabaseStats::default();

		let conn = Database::begin_concurrent(metadata_db, metadata_db_path).await?;

		let mut subject_rows = conn.as_ref().query("SELECT COUNT(*) as count FROM subjects", ()).await;
		match subject_rows {
			Ok(ref mut rows) => {
				if let Some(row) = rows.next().await? {
					let count_str = Database::value_to_string(&row.get_value(0)?, "Count").await?;
					stats.subject_count = count_str.parse::<usize>()?;
				}
			}
			Err(e) => {
				Database::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure in get_size_stats: `{}`", e));
			}
		}

		// Count aspects
		let mut aspect_rows = conn.as_ref().query("SELECT COUNT(*) as count FROM aspects", ()).await;
		match aspect_rows {
			Ok(ref mut rows) => {
				if let Some(row) = rows.next().await? {
					let count_str = Database::value_to_string(&row.get_value(0)?, "Count").await?;
					stats.aspect_count = count_str.parse::<usize>()?;
				}
			}
			Err(e) => {
				Database::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure in get_size_stats: `{}`", e));
			}
		}

		Database::commit_concurrent(&conn).await?;
		Ok(stats)
	}

	/// Converts the database to a JSON string representation.
	///
	/// # Errors
	///
	/// Returns an error if the serialization fails.
	pub fn to_json_str(&self) -> Result<String> {
		Ok(serde_json::to_string_pretty(self)?)
	}
}
