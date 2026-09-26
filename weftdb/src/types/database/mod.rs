use std::{
	collections::HashMap, fmt, fmt::{Display, Formatter}, path::Path, sync::{Arc, LazyLock}
};

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use splimes::Resolution;
use tokio::sync::Mutex;
use turso::Builder;
use uuid::Uuid;

pub use crate::types::database::traits::config::Config;
use crate::{
	cache, database::traits::Connection, types::{
		database::traits::{aspect_structure::AspectStructure, database_structure::DatabaseStructure}, Transaction, TxId
	}, Aspect, AspectId, Error, Subject, SubjectId
};

pub type DatabaseMap = Arc<Mutex<HashMap<DatabaseId, DatabaseInfo>>>;
pub static DATABASES: LazyLock<DatabaseMap> = LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

// Add connection manager for Turso with connection pools
static CONNECTION_DATABASES: LazyLock<Arc<Mutex<HashMap<String, turso::Database>>>> = LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

pub use config::{data_dir, default_data_dir};

/// Clear all cached connections that contain the given name in their path.
/// This is useful for test cleanup to ensure database connections are released
/// before deleting the database files.
pub async fn clear_connection_cache_by_name(name: &str) {
	let mut cache = CONNECTION_DATABASES.lock().await;
	let keys_to_remove: Vec<_> = cache.keys().filter(|path| path.contains(name)).cloned().collect();
	for key in keys_to_remove {
		cache.remove(&key);
	}
}

pub mod config;
pub mod connection;
pub mod helpers;
pub mod inputs;
pub mod navigation;
pub mod outputs;
pub mod pipeline;
pub mod traits;

#[derive(Debug, Clone)]
pub struct Database {
	id: DatabaseId,
	name: String,
	metadata: turso::Database,
	metadata_path: String,
	cache: Arc<Mutex<cache::DatabaseCache>>,
}

#[async_trait::async_trait]
impl DatabaseStructure for Database {
	/// Create a Turso database for reuse with concurrent writes enabled
	async fn create_turso_database(db_path: &str) -> Result<turso::Database> {
		// Use get_or_create_turso_database for consistency and proper race condition handling
		let (db, _was_new) = Self::get_or_create_turso_database(db_path).await?;
		Ok(db)
	}

	/// Get a Turso database for reuse with concurrent writes enabled
	async fn get_turso_database(db_path: &str) -> Result<turso::Database> {
		// Check if the database is already in the cache, if so return it
		if let Some(turso_db) = CONNECTION_DATABASES.lock().await.get(db_path) {
			return Ok(turso_db.clone());
		}

		// If not in cache check if the database file exists on disk
		if Path::new(db_path).exists() {
			// Check if MVCC log files exist - if so, try to clean them up first
			// The turso MVCC mode can leave behind log files that cause permission errors on Windows
			let log_path = format!("{db_path}-log");
			let wal_path = format!("{db_path}-wal");

			// Remove stale MVCC log files if they exist (they cause permission errors on reopening)
			if Path::new(&log_path).exists() {
				match std::fs::remove_file(&log_path) {
					Ok(()) => tracing::debug!("Removed stale MVCC log file: {}", log_path),
					Err(e) => tracing::warn!("Could not remove MVCC log file {}: {}", log_path, e),
				}
			}
			// Only remove WAL files if they're empty (indicating incomplete transactions)
			if Path::new(&wal_path).exists() && std::fs::metadata(&wal_path).map(|m| m.len() == 0).unwrap_or(false) {
				match std::fs::remove_file(&wal_path) {
					Ok(()) => tracing::debug!("Removed empty WAL file: {}", wal_path),
					Err(e) => tracing::warn!("Could not remove WAL file {}: {}", wal_path, e),
				}
			}

			// Open database (MVCC is now enabled via PRAGMA journal_mode=experimental_mvcc in 0.4.0)
			let turso_db = Builder::new_local(db_path).build().await?;

			// Configure database for MVCC concurrent writes
			{
				let conn = turso_db.connect()?;

				// Enable MVCC mode - required for BEGIN CONCURRENT transactions (Turso 0.4.0+)
				conn.execute("PRAGMA journal_mode=experimental_mvcc", turso::params![]).await.ok();

				// Set busy timeout for handling transient locks
				conn.execute("PRAGMA busy_timeout = 30000", turso::params![]).await.ok();

				// Optimize for concurrent access
				conn.execute("PRAGMA synchronous = NORMAL", turso::params![]).await.ok();

				// Explicitly drop connection to ensure it's closed
				drop(conn);
			}

			CONNECTION_DATABASES.lock().await.insert(db_path.to_string(), turso_db.clone());
			return Ok(turso_db);
		}

		bail!("Database not found")
	}

	/// Get or create a Turso database with proper MVCC configuration and connection pooling
	/// Returns (database, `was_newly_created`) - `was_newly_created` is true if the database
	/// was just created, false if it was retrieved from cache
	async fn get_or_create_turso_database(db_path: &str) -> Result<(turso::Database, bool)> {
		// Use a critical section to prevent race conditions during database creation
		let mut cache = CONNECTION_DATABASES.lock().await;

		// Check if the database is already in the cache, if so return it
		if let Some(turso_db) = cache.get(db_path) {
			return Ok((turso_db.clone(), false)); // From cache, not newly created
		}

		// Create the database (file will be created if it doesn't exist)
		// MVCC is now enabled via PRAGMA journal_mode in Turso 0.4.0+
		let turso_db = Builder::new_local(db_path).build().await?;

		// Configure database for MVCC concurrent writes (enables MVCC via PRAGMA)
		Self::configure_database_for_mvcc(&turso_db).await?;

		// Allow connection to fully close before returning
		tokio::task::yield_now().await;

		cache.insert(db_path.to_string(), turso_db.clone());
		drop(cache);
		Ok((turso_db, true)) // Newly created
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

		let conn = Self::begin_concurrent(&self.metadata.clone(), self.metadata_path(), Some(self.cache.clone())).await?;

		// Simple INSERT - duplicates are unlikely with UUID ids
		let res = conn.as_ref().execute("INSERT INTO transactions (id, message, created_at) VALUES (?, ?, ?)", turso::params![id_str.clone(), msg.clone(), created_at]).await;

		match res {
			Ok(_) => tracing::trace!("Background logged transaction id={id_str}"),
			Err(e) => {
				tracing::trace!("Background transaction logging attempt failed: {e}");
				let _ = Self::rollback_concurrent(&conn).await;
				return Err(anyhow::anyhow!("Failed to log transaction {id_str}: {e}"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;

		// Return immediately without waiting for the logging to complete
		Ok(tx_id)
	}

	async fn log_transaction(&self, transaction: &Transaction) -> Result<()> {
		// Since transaction logging is just for auditing, make it non-blocking
		let tx_clone = transaction.clone();
		let id_str = tx_clone.id().as_uuid().to_string();
		let msg = tx_clone.message().to_string();
		let created_at = tx_clone.created_at().timestamp_millis();

		let conn = Self::begin_concurrent(&self.metadata, self.metadata_path(), Some(self.cache.clone())).await?;

		// Create a fresh connection for each attempt

		// Simple INSERT - duplicates are unlikely with UUID ids
		let res = conn.as_ref().execute("INSERT INTO transactions (id, message, created_at) VALUES (?, ?, ?)", turso::params![id_str.clone(), msg.clone(), created_at]).await;

		match res {
			Ok(_) => tracing::trace!("Logged transaction id={id_str}"),
			Err(e) => {
				tracing::trace!("Transaction logging attempt failed: {e}");
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("Failed to log transaction {id_str}: {e}"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;

		// Return immediately without waiting for the logging to complete
		Ok(())
	}

	async fn metadata_database_create_transactions_table(conn: &cache::Connection) -> Result<Transaction> {
		let res = conn
			.as_ref()
			.execute(
				r"
                        CREATE TABLE IF NOT EXISTS transactions (
        			id TEXT NOT NULL,
        			message TEXT NOT NULL,
        			created_at INTEGER NOT NULL
        		)",
				turso::params![],
			)
			.await;

		match res {
			Ok(_) => tracing::debug!("Transactions table created or already exists"),
			Err(e) => {
				tracing::debug!("Failed to create transactions table: {e}");
				return Err(anyhow::anyhow!("SQL execution failure 5: `{e}`"));
			}
		}

		Ok(Transaction::new(None, "Create transactions table".to_string()))
	}

	async fn metadata_database_create_database_table(conn: &cache::Connection) -> Result<Transaction> {
		let res = conn
			.as_ref()
			.execute(
				r"
        		        CREATE TABLE IF NOT EXISTS database (
        				id TEXT NOT NULL,
        				name TEXT NOT NULL,
        				created_at INTEGER NOT NULL,
        				metadata_path TEXT NOT NULL
        		        )
        		",
				turso::params![],
			)
			.await;

		match res {
			Ok(_) => tracing::debug!("Database table created or already exists"),
			Err(e) => {
				return Err(anyhow::anyhow!("SQL execution failure 6: `{e}`"));
			}
		}

		Ok(Transaction::new(None, "Create database table".to_string()))
	}

	async fn metadata_database_create_subjects_table(conn: &cache::Connection) -> Result<Transaction> {
		let res = conn
			.as_ref()
			.execute(
				r"
        			CREATE TABLE IF NOT EXISTS subjects (
        				id TEXT NOT NULL,
        				database_id TEXT NOT NULL,
        				name TEXT NOT NULL,
        				created_at INTEGER NOT NULL
        			)
			",
				turso::params![],
			)
			.await;

		match res {
			Ok(_) => tracing::debug!("Subjects table created or already exists"),
			Err(e) => {
				tracing::debug!("Failed to create subjects table: {e}");
				return Err(anyhow::anyhow!("SQL execution failure 7: `{e}`"));
			}
		}

		Ok(Transaction::new(None, "Create subjects table".to_string()))
	}

	async fn metadata_database_create_aspects_table(conn: &cache::Connection) -> Result<Transaction> {
		let res = conn
			.as_ref()
			.execute(
				r"
                	CREATE TABLE IF NOT EXISTS aspects (
                        		id TEXT NOT NULL,
                        		subject_id TEXT NOT NULL,
                        		database_id TEXT NOT NULL,
                        		name TEXT NOT NULL,
                        		table_name TEXT NOT NULL,
                        		resolution TEXT NOT NULL,
                        		created_at INTEGER NOT NULL,
                        		earliest_measurement TEXT,
                        		latest_measurement TEXT,
                        		compression_config TEXT
                        	)
                	",
				turso::params![],
			)
			.await;

		match res {
			Ok(_) => tracing::debug!("Aspects table created or already exists"),
			Err(e) => {
				tracing::debug!("Failed to create aspects table: {e}");
				return Err(anyhow::anyhow!("SQL execution failure 8: `{e}`"));
			}
		}

		Ok(Transaction::new(None, "Create aspects table".to_string()))
	}

	async fn wireframe_metadata_database(conn: &cache::Connection) -> Result<Vec<Transaction>> {
		tracing::debug!("Connecting to database for table creation...");
		tracing::debug!("Creating transactions table...");
		let create_transactions_table_transaction = Self::metadata_database_create_transactions_table(conn).await?;
		tracing::debug!("Creating database table...");
		let create_database_table_transaction = Self::metadata_database_create_database_table(conn).await?;
		tracing::debug!("Creating subjects table...");
		let create_subjects_table_transaction = Self::metadata_database_create_subjects_table(conn).await?;
		tracing::debug!("Creating aspects table...");
		let create_aspects_table_transaction = Self::metadata_database_create_aspects_table(conn).await?;
		tracing::debug!("All tables created successfully");

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
		tracing::debug!("Creating new database: {name}");
		let data_dir = Self::get_data_dir();
		let db_path = format!("{data_dir}/{name}");
		tracing::debug!("Database path: {db_path}");

		// Check if folder already exists
		if Path::new(&db_path).exists() {
			bail!("Database folder already exists: {db_path}");
		}

		// Create the directory
		std::fs::create_dir_all(&db_path)?;
		tracing::debug!("Created directory: {db_path}");

		let db_id = DatabaseId::new();
		tracing::debug!("Generated database ID: {}", db_id.as_uuid());

		// Use shared connection database
		let metadata_db_path = format!("{db_path}/metadata.db");
		tracing::debug!("Creating Turso database at: {metadata_db_path}");
		let metadata_turso_db = Self::create_turso_database(&metadata_db_path).await?;
		tracing::debug!("Turso database created successfully");

		// For DDL operations (CREATE TABLE), use a direct connection without BEGIN CONCURRENT
		// DDL operations may not be compatible with MVCC concurrent transactions
		tracing::debug!("Creating metadata tables...");
		let schema_conn = metadata_turso_db.connect()?;
		let transactions = Self::wireframe_metadata_database_direct(&schema_conn).await;
		let mut transactions = match transactions {
			Ok(txs) => txs,
			Err(e) => {
				tracing::debug!("Failed to create metadata tables: {e}");
				return Err(anyhow::anyhow!("Failed to create metadata tables: {e}"));
			}
		};
		drop(schema_conn);
		tracing::debug!("Metadata tables created, {} transactions logged", transactions.len());

		// Now use BEGIN CONCURRENT for data operations (INSERT)
		let conn = Self::begin_concurrent(&metadata_turso_db, &metadata_db_path, None).await?;

		// Insert database metadata using concurrent-safe transaction pattern
		tracing::debug!("Inserting database metadata...");

		let exec_res = conn.as_ref().execute("INSERT INTO database (id, name, created_at, metadata_path) VALUES (?, ?, ?, ?)", turso::params![db_id.as_uuid().to_string(), name.to_string(), chrono::Utc::now().timestamp_millis(), metadata_db_path.clone()]).await;

		match exec_res {
			Ok(_) => tracing::debug!("Database metadata inserted successfully"),
			Err(e) => {
				tracing::debug!("Failed to insert database metadata: {e}");
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure 9: `{e}`"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;

		drop(conn);

		transactions.push(Transaction::new(None, "Insert database metadata".to_string()));

		let mut db_info = DatabaseInfo::new(name.to_string(), db_path);
		db_info.set_metadata(Some(metadata_turso_db.clone()));
		DATABASES.lock().await.insert(db_id, db_info);

		let db = Self { id: db_id, name: name.to_string(), metadata: metadata_turso_db, metadata_path: metadata_db_path, cache: Arc::new(Mutex::new(cache::DatabaseCache::new())) };

		for transaction in transactions {
			let () = db.log_transaction(&transaction).await?;
		}

		// Checkpoint metadata WAL to ensure schema and initial data are persisted
		Self::checkpoint_wal_passive(&db.metadata).await?;

		Ok(db)
	}

	/// Helper function to convert Turso values to strings, handling different storage formats
	async fn value_to_string(value: &turso::Value, field_name: &str) -> Result<String> {
		match value {
			turso::Value::Text(s) => Ok(s.clone()),
			turso::Value::Blob(bytes) => {
				// Try to interpret as UUID bytes
				if bytes.len() == 16 {
					let uuid = uuid::Uuid::from_bytes(bytes.as_slice().try_into().map_err(|_| anyhow::anyhow!("{field_name} blob is not 16 bytes"))?);
					Ok(uuid.to_string())
				} else {
					String::from_utf8(bytes.clone()).map_err(|e| anyhow::anyhow!("{field_name} blob is not valid UTF-8: {e}"))
				}
			}
			turso::Value::Integer(i) => Ok(i.to_string()),
			turso::Value::Real(r) => Ok(r.to_string()),
			turso::Value::Null => Err(anyhow::anyhow!("{field_name} is null")),
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
		let data_dir = Self::get_data_dir();
		let db_path = format!("{data_dir}/{name}");

		// Check if folder exists
		if !Path::new(&db_path).exists() {
			bail!("Database folder does not exist: {db_path}");
		}

		// Use shared connection database
		let metadata_db_path = &format!("{db_path}/metadata.db");
		let metadata_turso_db = Self::get_turso_database(metadata_db_path).await?;

		// Query database ID from metadata
		let conn = Self::begin_concurrent(&metadata_turso_db, metadata_db_path, None).await?;

		let mut rows = conn.as_ref().query("SELECT id, name FROM database LIMIT 1", ()).await?;
		let row = rows.next().await?.ok_or_else(|| anyhow::anyhow!("No database record found in metadata.db"))?;

		let db_id_str = Self::value_to_string(&row.get_value(0)?, "DB ID").await?;
		let _stored_name = Self::value_to_string(&row.get_value(1)?, "DB Name").await?;
		let db_id = DatabaseId::from_uuid(Uuid::parse_str(&db_id_str)?);

		let mut subject_rows = conn.as_ref().query("SELECT id, database_id, name, created_at FROM subjects", ()).await?;

		// Use the folder name for the DatabaseInfo, but the stored name is available if needed
		let mut db_info = DatabaseInfo::new(name.to_string(), db_path.clone());
		db_info.set_id(db_id);
		db_info.set_metadata(Some(metadata_turso_db.clone()));
		let () = db_info.set_metadata_path(Some(metadata_db_path.clone()));

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

				let aspect = Aspect::new(Some(aspect_id), &aspect_name, &subject_id, &resolution, &conn).await?;
				subject.add_aspect(aspect);
			}

			db_info.add_subject(subject);
		}

		let _ = Self::commit_concurrent(&conn).await;

		DATABASES.lock().await.insert(db_id, db_info);

		Ok(Self { id: db_id, name: name.to_string(), metadata: metadata_turso_db, metadata_path: metadata_db_path.clone(), cache: Arc::new(Mutex::new(cache::DatabaseCache::new())) })
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
		// Checkpoint metadata WAL before closing to ensure all data is persisted
		Self::checkpoint_wal_passive(&self.metadata).await.ok();

		{
			let mut databases = DATABASES.lock().await;
			if let Some(_db_info) = databases.remove(&self.id) {
				// Turso databases don't need explicit closing like SQLx pools
				// The connections will be closed when dropped
			}
		}

		// Also remove from connection databases
		let db_path = format!("{}/{}/metadata.db", Self::get_data_dir(), self.name);
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

		let db_dir = format!("{}/{}", <Self as Config>::get_data_dir(), self.name);
		let metadata_db_path = format!("{db_dir}/metadata.db");

		for attempt in 0..MAX_ATTEMPTS {
			// Try to open the database file exclusively to check if it's still locked
			if let Err(e) = std::fs::OpenOptions::new().write(true).truncate(false).open(&metadata_db_path) {
				if e.kind() == std::io::ErrorKind::PermissionDenied {
					// File is still locked, wait and retry
					if attempt == MAX_ATTEMPTS - 1 {
						// Last attempt failed, but don't error - just log
						tracing::warn!("Database may still be locked after {MAX_ATTEMPTS} attempts: {metadata_db_path}");
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
						tracing::warn!("Database appears to be locked by another application (like DB Browser). Retrying...");
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
					tracing::warn!("Database locked after {max_attempts} attempts. Close DB Browser and try again.");
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
		tracing::debug!("Observing subject: {name}");
		// Check if subject already exists
		match self.get_subject_by_name(name).await {
			Ok(_) => {
				tracing::warn!("Subject '{name}' already exists");
				return Err(anyhow::anyhow!("Subject '{name}' already exists"));
			}
			Err(e) => {
				tracing::debug!("Subject check (expected not found): {}", e);
			}
		}

		// Create subject folder
		let subject_path = format!("{}/{}/{}", Self::get_data_dir(), self.name, name);
		tracing::debug!("Creating subject folder: {subject_path}");
		match tokio::fs::create_dir_all(&subject_path).await {
			Ok(()) => tracing::debug!("Subject folder created successfully"),
			Err(e) => {
				tracing::error!("Failed to create subject folder: {}", e);
				return Err(e.into());
			}
		}

		// Add subject to database metadata
		let metadata_db = &self.metadata;
		let metadata_db_path = self.metadata_path.clone();

		// create subject
		tracing::debug!("Creating Subject instance...");
		let subject = Subject::new(None, name.to_string(), self.id, self.metadata_path.clone()).await?;
		tracing::debug!("Subject created with ID: {}", subject.id().as_uuid());

		// Add subject to metadata database
		tracing::debug!("Inserting subject into database...");
		let id_str = subject.id().as_uuid().to_string();
		let name_str = name.to_string();
		let db_id_str = self.id.as_uuid().to_string();
		let created_at = chrono::Utc::now().timestamp_millis();

		let conn = Self::begin_concurrent(metadata_db, &metadata_db_path, Some(self.cache.clone())).await?;
		let res = conn.as_ref().execute("INSERT INTO subjects (id, name, database_id, created_at) VALUES (?, ?, ?, ?)", turso::params![id_str.clone(), name_str.clone(), db_id_str.clone(), created_at]).await;
		match res {
			Ok(_) => tracing::debug!("Subject inserted successfully"),
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure 10: `{e}`"));
			}
		}

		let _ = Self::commit_concurrent(&conn).await;
		tracing::debug!("Subject inserted successfully");

		// Update in-memory cache so subsequent operations don't need metadata DB
		{
			let mut dbs = DATABASES.lock().await;
			if let Some(info) = dbs.get_mut(&self.id) {
				info.add_subject(subject.clone());
			}
		}

		// Log the transaction
		self.record_transaction(&format!("Added subject '{name}'")).await?;

		// Checkpoint metadata WAL to ensure subject is persisted
		Self::checkpoint_wal_passive(&self.metadata).await?;

		Ok(subject)
	}

	/// Get a subject by its ID
	async fn get_subject(&self, id: &SubjectId) -> Result<Subject> {
		// Read-only subject lookup with busy_timeout and retry; avoid transactions to reduce lock contention
		tracing::debug!("Getting subject by ID: {}", id.as_uuid());

		let conn = Self::begin_concurrent(&self.metadata, &self.metadata_path, Some(self.cache.clone())).await?;
		tracing::debug!("Connected to metadata DB, querying for subject ID {}", id.as_uuid());
		let res = conn.as_ref().query("SELECT id, name, database_id FROM subjects WHERE id = ?", turso::params![id.as_uuid().to_string()]).await;

		let subject = match res {
			Ok(mut rows) => {
				if let Some(row) = rows.next().await? {
					tracing::debug!("Found subject row for ID {}", id.as_uuid());
					let subject_id_str = Self::value_to_string(&row.get_value(0)?, "Subject ID").await?;
					let name = Self::value_to_string(&row.get_value(1)?, "Subject name").await?;
					let database_id_str = Self::value_to_string(&row.get_value(2)?, "Database ID").await?;

					let subject_id = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str)?);
					let database_id = DatabaseId::from_uuid(Uuid::parse_str(&database_id_str)?);
					Subject::new(Some(subject_id), name, database_id, self.metadata_path.clone()).await?
				} else {
					tracing::debug!("Subject ID {} not found in database", id.as_uuid());
					return Err(anyhow::anyhow!("Subject not found"));
				}
			}
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure 12: in get_subject: `{e}`"));
			}
		};

		let _ = Self::commit_concurrent(&conn).await;
		Ok(subject)
	}

	/// Get a subject by its name
	async fn get_subject_by_name(&self, name: &str) -> Result<Subject> {
		// Read-only subject lookup with busy_timeout and retry; avoid transactions to reduce lock contention
		let conn = Self::begin_concurrent(&self.metadata, &self.metadata_path, Some(self.cache.clone())).await?;
		let mut rows = conn.as_ref().query("SELECT id, name, database_id FROM subjects WHERE name = ?", turso::params![name]).await?;
		if let Some(row) = rows.next().await? {
			let subject_id_str = Self::value_to_string(&row.get_value(0)?, "Subject ID").await?;
			let name = Self::value_to_string(&row.get_value(1)?, "Subject name").await?;
			let database_id_str = Self::value_to_string(&row.get_value(2)?, "Database ID").await?;

			let subject_id = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str)?);
			let database_id = DatabaseId::from_uuid(Uuid::parse_str(&database_id_str)?);

			Self::commit_concurrent(&conn).await?;

			return Subject::new(Some(subject_id), name, database_id, self.metadata_path.clone()).await;
		}
		Self::rollback_concurrent(&conn).await?;
		return Err(anyhow::anyhow!("Subject not found"));
	}

	/// Remove a subject from observation
	///
	/// 1. Delete the subject folder and all child files and folders.
	/// 2. Remove subject from database metadata.
	///
	/// # Errors
	/// Returns an error if the subject does not exist
	async fn remove_subject(&self, id: &SubjectId) -> Result<()> {
		// Retrieve subject from metadata database
		let subject = self.get_subject(id).await?;
		let metadata_db = &self.metadata;
		let metadata_db_path = self.metadata_path.clone();

		// Delete the subject folder and all child files and folders
		let subject_path = format!("{}/{}/{}", Self::get_data_dir(), self.name, subject.name());
		tokio::fs::remove_dir_all(&subject_path).await?;

		// Remove subject from database metadata
		let id_str = id.as_uuid().to_string();

		let conn = Self::begin_concurrent(metadata_db, &metadata_db_path, Some(self.cache.clone())).await?;
		let res = conn.as_ref().execute("DELETE FROM subjects WHERE id = ?", turso::params![id_str.clone()]).await;
		match res {
			Ok(_) => tracing::debug!("Deleted subject with ID {}", id.as_uuid()),
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure 13: `{e}`"));
			}
		}
		let _ = Self::commit_concurrent(&conn).await;

		// Log the transaction
		self.record_transaction(&format!("Removed subject '{}'", subject.name())).await?;

		// Checkpoint metadata WAL to ensure deletion is persisted
		Self::checkpoint_wal_passive(&self.metadata).await?;

		Ok(())
	}

	/// List all subjects in the database
	async fn list_subjects(&self) -> Result<Vec<Subject>> {
		let conn = Self::begin_concurrent(&self.metadata, &self.metadata_path, Some(self.cache.clone())).await?;
		let res = conn.as_ref().query("SELECT id, name, database_id FROM subjects WHERE database_id = ?", turso::params![self.id.as_uuid().to_string()]).await;
		let mut rows = match res {
			Ok(rows) => rows,
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure 14: `{e}`"));
			}
		};

		let _ = Self::commit_concurrent(&conn).await;

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
	async fn list_aspects(&self, subject_id: &SubjectId) -> Result<Vec<Aspect>> {
		let conn = Self::begin_concurrent(&self.metadata, &self.metadata_path, Some(self.cache.clone())).await?;
		let result = conn.as_ref().query("SELECT id, name, subject_id, resolution FROM aspects WHERE subject_id = ?", turso::params![subject_id.as_uuid().to_string()]).await;

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
					aspects.push(Aspect::from_metadata(Some(aspect_id), name, &subject_id_parsed, &resolution, self.metadata_path.clone(), None).await?);
				}

				let _ = Self::commit_concurrent(&conn).await;

				return Ok(aspects);
			}
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure 15: in list_aspects: `{e}`"));
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
	async fn track_aspect(&self, subject_id: &SubjectId, name: &str, resolution: &Resolution, compression_config: Option<crate::CompressionConfig>) -> Result<Aspect> {
		tracing::debug!("Tracking aspect '{}' for subject {}", name, subject_id.as_uuid());
		// Check if subject exists
		tracing::debug!("Retrieving subject with ID: {}", subject_id.as_uuid());
		let subject = self.get_subject(subject_id).await?;

		// Check if aspect already exists for the subject
		tracing::debug!("Checking existing aspects for subject '{}'", subject.name());
		let existing_aspects = self.list_aspects(subject_id).await?;
		if existing_aspects.iter().any(|a| a.name() == name) {
			return Err(anyhow::anyhow!("Aspect '{}' already exists for subject '{}'", name, subject.name()));
		}

		// Create a new aspect folder
		let aspect_path = format!("{}/{}/{}/{}", Self::get_data_dir(), self.name, subject.name(), name);
		tracing::debug!("Creating aspect folder: {aspect_path}");
		tokio::fs::create_dir_all(&aspect_path).await?;

		// Add aspect to database metadata
		let aspect_id = AspectId::new();
		tracing::debug!("Generated aspect ID: {}", aspect_id.as_uuid());

		let table_name = format!("aspect_{}_{}", subject_id.as_uuid().to_string().replace('-', "_"), name.replace([' ', '-'], "_"));
		tracing::debug!("Inserting aspect into database with table_name: {table_name}");

		let aspect_id_str = aspect_id.as_uuid().to_string();
		let name_str = name.to_string();
		let subj_id_str = subject_id.as_uuid().to_string();
		let db_id_str = self.id.as_uuid().to_string();
		let resolution_json = serde_json::to_string(&resolution)?;
		let compression_config_json = compression_config.as_ref().map(serde_json::to_string).transpose()?;
		let created_at = chrono::Utc::now().timestamp_millis();

		// Establish a fresh connection for this attempt while holding the mutex
		let metadata_db_conn = self.metadata();
		let metadata_db_path = self.metadata_path.clone();

		// Begin transaction on this connection
		let conn = Self::begin_concurrent(metadata_db_conn, &metadata_db_path, Some(self.cache.clone())).await?;
		tracing::trace!("About to execute INSERT INTO aspects (aspect_id={aspect_id_str} table_name={table_name})");

		// Execute the INSERT while holding the mutex
		conn.as_ref().execute("INSERT INTO aspects (id, name, subject_id, database_id, table_name, resolution, created_at, compression_config) VALUES (?, ?, ?, ?, ?, ?, ?, ?)", turso::params![aspect_id_str.clone(), name_str.clone(), subj_id_str.clone(), db_id_str.clone(), table_name.clone(), resolution_json.clone(), created_at, compression_config_json]).await?;

		tracing::debug!("Aspect inserted successfully");

		// At this point the metadata INSERT succeeded and we've dropped the metadata connection.
		// Release the aspect creation mutex (it was locked above) implicitly by letting the
		// earlier guard drop (we only held it across the insertion). Now perform the
		// heavier wireframing without holding the metadata lock.
		let mut aspect = Aspect::new(Some(aspect_id), name, subject_id, resolution, &conn).await?;

		// Set compression config on the created aspect
		aspect.set_compression_config_local(compression_config);

		let _ = Self::commit_concurrent(&conn).await;

		// Update in-memory cache with the newly created aspect to avoid future metadata reads
		{
			let mut dbs = DATABASES.lock().await;
			if let Some(info) = dbs.get_mut(&self.id) {
				// Ensure subject exists in cache; use the subject we already loaded above
				if !info.subjects().contains_key(subject_id) {
					info.add_subject(subject.clone());
				}
				if let Some(subj) = info.subjects_mut().get_mut(subject_id) {
					subj.add_aspect(aspect.clone());
				}
			}
		}

		tracing::debug!("Aspect created and wireframed successfully: {}", aspect.name());

		// Log the transaction
		self.record_transaction(&format!("Tracking new aspect '{}' for subject '{}'", name, subject.name())).await?;

		// Checkpoint metadata WAL to ensure aspect is persisted
		Self::checkpoint_wal_passive(&self.metadata).await?;

		tracing::debug!("Transaction recorded successfully for aspect creation.");

		Ok(aspect)
	}

	async fn get_aspect(&self, id: &AspectId) -> Result<Aspect> {
		let cache_key = format!("aspect_{}", id.as_uuid());
		// Check cache first (cleanup is done periodically, not on every call)
		if let Some(cached) = self.cache.lock().await.get(&cache_key).await {
			return Ok(cached);
		}

		let conn = Self::begin_concurrent(&self.metadata, &self.metadata_path, Some(self.cache.clone())).await?;

		let res = conn
			.as_ref()
			.query(
				"SELECT a.id, a.name, a.subject_id, a.resolution, s.name AS subject_name, a.compression_config \
			        FROM aspects a JOIN subjects s ON s.id = a.subject_id \
			        WHERE a.id = ?",
				turso::params![id.as_uuid().to_string()],
			)
			.await;

		let aspect = match res {
			Ok(mut rows) => {
				if let Some(row) = rows.next().await? {
					let aspect_id_str = Self::value_to_string(&row.get_value(0)?, "Aspect ID").await?;
					let name = Self::value_to_string(&row.get_value(1)?, "Aspect name").await?;
					let subject_id_str = Self::value_to_string(&row.get_value(2)?, "Subject ID").await?;
					let resolution_str = Self::value_to_string(&row.get_value(3)?, "Resolution").await?;
					let compression_config_json: Option<String> = row.get(5).ok();
					let aspect_id = AspectId::from_uuid(Uuid::parse_str(&aspect_id_str)?);
					let subject_id = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str)?);
					let resolution: Resolution = serde_json::from_str(&resolution_str)?;
					let compression_config: Option<crate::CompressionConfig> = compression_config_json.filter(|s| !s.is_empty()).map(|s| serde_json::from_str(&s)).transpose()?;

					let mut aspect = Aspect::from_metadata(Some(aspect_id), name, &subject_id, &resolution, self.metadata_path.clone(), None).await?;
					aspect.set_compression_config_local(compression_config);
					aspect
				} else {
					bail!("Aspect not found");
				}
			}
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure 16: in get_aspect: `{e}`"));
			}
		};
		let _ = Self::commit_concurrent(&conn).await;
		self.cache.lock().await.store(&cache_key, aspect.clone()).await;
		Ok(aspect)
	}

	async fn get_aspect_by_name(&self, name: &str) -> Result<Aspect> {
		let conn = Self::begin_concurrent(&self.metadata, &self.metadata_path, Some(self.cache.clone())).await?;

		let query_res = conn
			.as_ref()
			.query(
				"SELECT a.id, a.name, a.subject_id, a.resolution, s.name AS subject_name, a.compression_config \
				FROM aspects a JOIN subjects s ON s.id = a.subject_id \
				WHERE a.name = ?",
				turso::params![name],
			)
			.await;

		let aspect = match query_res {
			Ok(mut rows) => {
				if let Some(row) = rows.next().await? {
					let aspect_id_str = Self::value_to_string(&row.get_value(0)?, "Aspect ID").await?;
					let name = Self::value_to_string(&row.get_value(1)?, "Aspect name").await?;
					let subject_id_str = Self::value_to_string(&row.get_value(2)?, "Subject ID").await?;
					let resolution_str = Self::value_to_string(&row.get_value(3)?, "Resolution").await?;
					let compression_config_json: Option<String> = row.get(5).ok();

					let aspect_id = AspectId::from_uuid(Uuid::parse_str(&aspect_id_str)?);
					let subject_id = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str)?);
					let resolution: Resolution = serde_json::from_str(&resolution_str)?;
					let compression_config: Option<crate::CompressionConfig> = compression_config_json.filter(|s| !s.is_empty()).map(|s| serde_json::from_str(&s)).transpose()?;

					let mut aspect = Aspect::from_metadata(Some(aspect_id), name, &subject_id, &resolution, self.metadata_path.clone(), None).await?;
					aspect.set_compression_config_local(compression_config);
					aspect
				} else {
					bail!("Aspect not found");
				}
			}
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!("SQL execution failure in 17: get_aspect_by_name: `{e}`"));
			}
		};
		let _ = Self::commit_concurrent(&conn).await;
		Ok(aspect)
	}

	async fn get_earliest_measurement(&self, aspect_id: &AspectId) -> Result<Option<DateTime<Utc>>> {
		let cache_key = format!("metadata_earliest_measurement_{}", aspect_id.as_uuid());
		self.cache.lock().await.cleanup_expired().await;
		if let Some(cached) = self.cache.lock().await.get(&cache_key).await {
			return Ok(Some(cached));
		}

		let db = self.get_measurement_db(aspect_id).await?;
		let db_path = self.get_measurement_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		let res = conn.as_ref().query("SELECT MIN(timestamp) FROM measurements", turso::params![]).await;

		let timestamp = match res {
			Ok(mut rows) => {
				if let Some(row) = rows.next().await? {
					// Check if the value is null (empty table)
					let value = row.get_value(0)?;
					if matches!(value, turso::Value::Null) {
						None
					} else {
						let timestamp_str = Self::value_to_string(&value, "Earliest Measurement Timestamp").await?;
						if timestamp_str.is_empty() {
							return Ok(None);
						}
						// Parse as milliseconds (i64) instead of RFC3339
						let timestamp_millis: i64 = timestamp_str.parse().map_err(|e| anyhow::anyhow!("Invalid timestamp format: {e}"))?;
						let timestamp = DateTime::from_timestamp_millis(timestamp_millis).ok_or_else(|| anyhow::anyhow!("Invalid timestamp"))?;
						Some(timestamp)
					}
				} else {
					None
				}
			}
			Err(e) => {
				let _ = Self::rollback_concurrent(&conn).await;
				return Err(anyhow::anyhow!("SQL execution failure 18: in get_earliest_measurement: `{e}`"));
			}
		};

		let _ = Self::commit_concurrent(&conn).await;

		if let Some(ts) = timestamp {
			self.cache.lock().await.store(&cache_key, ts).await;
			Ok(Some(ts))
		} else {
			Ok(None)
		}
	}

	async fn get_latest_measurement(&self, aspect_id: &AspectId) -> Result<Option<DateTime<Utc>>> {
		let db = self.get_measurement_db(aspect_id).await?;
		let db_path = self.get_measurement_db_path(aspect_id).await?;
		let _cache_key = format!("measurement_db_{aspect_id}");
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		let res = conn.as_ref().query("SELECT MAX(timestamp) FROM measurements", turso::params![]).await;
		match res {
			Ok(mut rows) => {
				if let Some(row) = rows.next().await? {
					let _ = Self::commit_concurrent(&conn).await;
					// Check if the value is null (empty table)
					let value = row.get_value(0)?;
					if matches!(value, turso::Value::Null) {
						return Ok(None);
					}
					let timestamp_str = Self::value_to_string(&value, "Latest Measurement Timestamp").await?;
					if timestamp_str.is_empty() {
						return Ok(None);
					}
					// Parse as milliseconds (i64) instead of RFC3339
					let timestamp_millis: i64 = timestamp_str.parse().map_err(|e| anyhow::anyhow!("Invalid timestamp format: {e}"))?;
					let timestamp = DateTime::from_timestamp_millis(timestamp_millis).ok_or_else(|| anyhow::anyhow!("Invalid timestamp"))?;
					Ok(Some(timestamp))
				} else {
					Ok(None)
				}
			}
			Err(e) => {
				let _ = Self::rollback_concurrent(&conn).await;
				return Err(anyhow::anyhow!("SQL execution failure 19: in get_latest_measurement: `{e}`"));
			}
		}
	}

	async fn get_aspect_resolution(&self, aspect_id: &AspectId) -> Result<Resolution> {
		let aspect = self.get_aspect(aspect_id).await?;
		let resolution = aspect.resolution();
		Ok(resolution)
	}

	async fn update_aspect_timestamps(&self, aspect_id: &AspectId, min_new: DateTime<Utc>, max_new: DateTime<Utc>) -> Result<()> {
		let metadata_db = &self.metadata;
		let metadata_db_path = self.metadata_path.clone();

		let conn = Self::begin_concurrent(metadata_db, &metadata_db_path, Some(self.cache.clone())).await?;
		let res = conn.as_ref().execute("UPDATE aspects SET earliest_measurement = ?, latest_measurement = ? WHERE id = ?", turso::params![min_new.to_rfc3339(), max_new.to_rfc3339(), aspect_id.as_uuid().to_string()]).await;
		match res {
			Ok(_) => tracing::debug!("Updated aspect timestamps for aspect {aspect_id}"),
			Err(e) => {
				let _ = Self::rollback_concurrent(&conn).await;
				return Err(anyhow::anyhow!("SQL execution failure 20: `{e}`"));
			}
		}
		let _ = Self::commit_concurrent(&conn).await;
		Ok(())
	}

	async fn get_measurement_db(&self, aspect_id: &AspectId) -> Result<turso::Database> {
		// Build the measurements path directly from aspect_id
		let cache_key = format!("measurements_db_{aspect_id}");

		// Try cache first
		if let Some(db) = self.cache.lock().await.get::<turso::Database>(&cache_key).await {
			return Ok(db);
		}

		// Get aspect metadata (lightweight - no wireframing)
		let mut aspect = self.get_aspect(aspect_id).await?;
		let db = aspect.measurements().await?;

		// Cache it
		self.cache.lock().await.store(&cache_key, db.clone()).await;

		Ok(db)
	}

	async fn get_measurement_db_path(&self, aspect_id: &AspectId) -> Result<String> {
		let cache_key = format!("measurements_db_path_{aspect_id}");
		if let Some(path) = self.cache.lock().await.get::<String>(&cache_key).await {
			return Ok(path);
		}
		let aspect = self.get_aspect(aspect_id).await?;
		let measurements_path = aspect.measurements_path();
		self.cache.lock().await.store(&cache_key, measurements_path.clone()).await;
		Ok(measurements_path)
	}

	async fn get_unprocessed_batches_db(&self, aspect_id: &AspectId) -> Result<turso::Database> {
		let cache_key = format!("unprocessed_batches_db_{aspect_id}");
		if let Some(db) = self.cache.lock().await.get::<turso::Database>(&cache_key).await {
			return Ok(db);
		}
		let mut aspect = self.get_aspect(aspect_id).await?;
		let db = aspect.unprocessed_batches().await?;
		self.cache.lock().await.store(&cache_key, db.clone()).await;
		Ok(db)
	}

	async fn get_unprocessed_batches_db_path(&self, aspect_id: &AspectId) -> Result<String> {
		let cache_key = format!("unprocessed_batches_db_path_{aspect_id}");
		if let Some(path) = self.cache.lock().await.get::<String>(&cache_key).await {
			return Ok(path);
		}
		let aspect = self.get_aspect(aspect_id).await?;
		let unprocessed_batches_path = aspect.unprocessed_batches_path();
		self.cache.lock().await.store(&cache_key, unprocessed_batches_path.clone()).await;
		Ok(unprocessed_batches_path)
	}

	async fn get_processed_batches_db(&self, aspect_id: &AspectId) -> Result<turso::Database> {
		let cache_key = format!("processed_batches_db_{aspect_id}");
		if let Some(db) = self.cache.lock().await.get::<turso::Database>(&cache_key).await {
			return Ok(db);
		}
		let mut aspect = self.get_aspect(aspect_id).await?;
		let db = aspect.processed_batches().await?;
		self.cache.lock().await.store(&cache_key, db.clone()).await;
		Ok(db)
	}

	async fn get_processed_batches_db_path(&self, aspect_id: &AspectId) -> Result<String> {
		let cache_key = format!("processed_batches_db_path_{aspect_id}");
		if let Some(path) = self.cache.lock().await.get::<String>(&cache_key).await {
			return Ok(path);
		}
		let aspect = self.get_aspect(aspect_id).await?;
		let processed_batches_path = aspect.processed_batches_path();
		self.cache.lock().await.store(&cache_key, processed_batches_path.clone()).await;
		Ok(processed_batches_path)
	}

	async fn get_patterns_db(&self, aspect_id: &AspectId) -> Result<turso::Database> {
		let cache_key = format!("patterns_db_{aspect_id}");
		if let Some(db) = self.cache.lock().await.get::<turso::Database>(&cache_key).await {
			return Ok(db);
		}
		let mut aspect = self.get_aspect(aspect_id).await?;
		let db = aspect.patterns().await?;
		self.cache.lock().await.store(&cache_key, db.clone()).await;

		Ok(db)
	}

	async fn get_patterns_db_path(&self, aspect_id: &AspectId) -> Result<String> {
		let cache_key = format!("patterns_db_path_{aspect_id}");
		if let Some(path) = self.cache.lock().await.get::<String>(&cache_key).await {
			return Ok(path);
		}
		let aspect = self.get_aspect(aspect_id).await?;
		let patterns_path = aspect.patterns_path();
		self.cache.lock().await.store(&cache_key, patterns_path.clone()).await;
		Ok(patterns_path)
	}

	async fn get_events_db(&self, aspect_id: &AspectId) -> Result<turso::Database> {
		let cache_key = format!("events_db_{aspect_id}");
		if let Some(db) = self.cache.lock().await.get::<turso::Database>(&cache_key).await {
			return Ok(db);
		}
		let mut aspect = self.get_aspect(aspect_id).await?;
		let events_db = aspect.events().await?;
		self.cache.lock().await.store(&cache_key, events_db.clone()).await;
		Ok(events_db)
	}

	async fn get_events_db_path(&self, aspect_id: &AspectId) -> Result<String> {
		let cache_key = format!("events_db_path_{aspect_id}");
		if let Some(path) = self.cache.lock().await.get::<String>(&cache_key).await {
			return Ok(path);
		}
		let aspect = self.get_aspect(aspect_id).await?;
		let events_db_path = aspect.events_path();
		self.cache.lock().await.store(&cache_key, events_db_path.clone()).await;
		Ok(events_db_path)
	}

	async fn get_unprocessed_events_db(&self, aspect_id: &AspectId) -> Result<turso::Database> {
		let cache_key = format!("unprocessed_events_db_{aspect_id}");
		if let Some(db) = self.cache.lock().await.get::<turso::Database>(&cache_key).await {
			return Ok(db);
		}
		let mut aspect = self.get_aspect(aspect_id).await?;
		let events_db = aspect.unprocessed_events().await?;
		self.cache.lock().await.store(&cache_key, events_db.clone()).await;
		Ok(events_db)
	}

	async fn get_unprocessed_events_db_path(&self, aspect_id: &AspectId) -> Result<String> {
		let cache_key = format!("unprocessed_events_db_path_{aspect_id}");
		if let Some(path) = self.cache.lock().await.get::<String>(&cache_key).await {
			return Ok(path);
		}
		let aspect = self.get_aspect(aspect_id).await?;
		let events_db_path = aspect.unprocessed_events_path();
		self.cache.lock().await.store(&cache_key, events_db_path.clone()).await;
		Ok(events_db_path)
	}

	async fn get_processed_events_db(&self, aspect_id: &AspectId) -> Result<turso::Database> {
		let cache_key = format!("processed_events_db_{aspect_id}");
		if let Some(db) = self.cache.lock().await.get::<turso::Database>(&cache_key).await {
			return Ok(db);
		}
		let mut aspect = self.get_aspect(aspect_id).await?;
		let events_db = aspect.processed_events().await?;
		self.cache.lock().await.store(&cache_key, events_db.clone()).await;
		Ok(events_db)
	}

	async fn get_processed_events_db_path(&self, aspect_id: &AspectId) -> Result<String> {
		let cache_key = format!("processed_events_db_path_{aspect_id}");
		if let Some(path) = self.cache.lock().await.get::<String>(&cache_key).await {
			return Ok(path);
		}
		let aspect = self.get_aspect(aspect_id).await?;
		let events_db_path = aspect.processed_events_path();
		self.cache.lock().await.store(&cache_key, events_db_path.clone()).await;
		Ok(events_db_path)
	}

	async fn get_correlations_db(&self, aspect_id: &AspectId) -> Result<turso::Database> {
		let cache_key = format!("correlations_db_{aspect_id}");
		if let Some(db) = self.cache.lock().await.get::<turso::Database>(&cache_key).await {
			return Ok(db);
		}
		let mut aspect = self.get_aspect(aspect_id).await?;
		let db = aspect.correlations().await?;
		self.cache.lock().await.store(&cache_key, db.clone()).await;
		Ok(db)
	}

	async fn get_correlations_db_path(&self, aspect_id: &AspectId) -> Result<String> {
		let cache_key = format!("correlations_db_path_{aspect_id}");
		if let Some(path) = self.cache.lock().await.get::<String>(&cache_key).await {
			return Ok(path);
		}
		let aspect = self.get_aspect(aspect_id).await?;
		let correlations_path = aspect.correlations_path();
		self.cache.lock().await.store(&cache_key, correlations_path.clone()).await;
		Ok(correlations_path)
	}

	async fn get_dictionary_db(&self, aspect_id: &AspectId, dictionary_name: &str) -> Result<turso::Database> {
		let cache_key = format!("dictionary_db_{aspect_id}_{dictionary_name}");
		if let Some(db) = self.cache.lock().await.get::<turso::Database>(&cache_key).await {
			return Ok(db);
		}
		let mut aspect = self.get_aspect(aspect_id).await?;
		let db = aspect.dictionary(dictionary_name).await?;
		self.cache.lock().await.store(&cache_key, db.clone()).await;
		Ok(db)
	}

	/// Check if database is currently locked
	/// Returns true if locked, false if available
	/// Uses a minimal timeout (100ms) to avoid blocking
	async fn is_locked(&self) -> bool {
		// Try to get a connection and begin a transaction with minimal timeout
		match self.metadata.connect() {
			Ok(conn) => {
				// Set minimal busy timeout for this check
				let _ = conn.execute("PRAGMA busy_timeout=100", turso::params![]).await;

				// Try to begin a concurrent transaction
				match conn.execute("BEGIN CONCURRENT", turso::params![]).await {
					Ok(_) => {
						// Successfully got lock, clean up and return false (not locked)
						let _ = conn.execute("ROLLBACK", turso::params![]).await;
						drop(conn);
						false
					}
					Err(e) => {
						// Check if error is due to lock
						let err_msg = e.to_string().to_lowercase();
						drop(conn);
						err_msg.contains("locked") || err_msg.contains("busy")
					}
				}
			}
			Err(_) => {
				// Connection failed, treat as locked
				true
			}
		}
	}

	/// Check if a specific measurement database is locked
	/// This is more accurate for plot updates since `analyze_range` queries the measurement DB
	/// CRITICAL: This must NOT load/initialize the aspect database as that's expensive!
	/// Instead, we check the database file directly.
	async fn is_measurement_db_locked(&self, aspect_id: &AspectId) -> bool {
		// Build the measurement database path directly from aspect_id (without loading metadata)
		let _data_dir = Self::get_data_dir();

		// Get aspect details from cache to find the database path
		// If not cached, we can't check it, so return false to allow the import to proceed
		let measurement_db_path = match self.get_measurement_db_path(aspect_id).await {
			Ok(path) => path,
			Err(_) => {
				// Can't determine path, assume not locked to avoid blocking
				return false;
			}
		};

		// Try to create a fresh connection to just this measurement database
		match turso::Builder::new_local(&measurement_db_path).build().await {
			Ok(measurement_db) => {
				// Try to get a connection with minimal timeout
				match measurement_db.connect() {
					Ok(conn) => {
						// Set minimal busy timeout for this check (100ms)
						let _ = conn.execute("PRAGMA busy_timeout=100", turso::params![]).await;

						// Try to begin a concurrent transaction
						match conn.execute("BEGIN CONCURRENT", turso::params![]).await {
							Ok(_) => {
								// Successfully got lock, clean up and return false (not locked)
								let _ = conn.execute("ROLLBACK", turso::params![]).await;
								drop(conn);
								false
							}
							Err(e) => {
								// Check if error is due to lock
								let err_msg = e.to_string().to_lowercase();
								drop(conn);
								err_msg.contains("locked") || err_msg.contains("busy")
							}
						}
					}
					Err(_) => {
						// Connection failed, treat as locked
						true
					}
				}
			}
			Err(_) => {
				// Couldn't build database, assume not locked to avoid blocking
				false
			}
		}
	}
}

impl Database {
	/// DDL-safe version that uses a direct `turso::Connection` for schema creation
	/// DDL operations (CREATE TABLE) may not work well with BEGIN CONCURRENT transactions
	/// Note: Tables have no indexes to support MVCC (turso MVCC doesn't support indexes yet)
	async fn wireframe_metadata_database_direct(conn: &turso::Connection) -> Result<Vec<Transaction>> {
		tracing::debug!("Connecting to database for table creation...");

		tracing::debug!("Creating transactions table...");
		conn.execute(
			r"CREATE TABLE IF NOT EXISTS transactions (
				id TEXT NOT NULL,
				message TEXT NOT NULL,
				created_at INTEGER NOT NULL
			)",
			turso::params![],
		)
		.await
		.map_err(|e| anyhow::anyhow!("Failed to create transactions table: {e}"))?;
		tracing::debug!("Transactions table created");

		tracing::debug!("Creating database table...");
		conn.execute(
			r"CREATE TABLE IF NOT EXISTS database (
				id TEXT NOT NULL,
				name TEXT NOT NULL,
				created_at INTEGER NOT NULL,
				metadata_path TEXT NOT NULL
			)",
			turso::params![],
		)
		.await
		.map_err(|e| anyhow::anyhow!("Failed to create database table: {e}"))?;
		tracing::debug!("Database table created");

		tracing::debug!("Creating subjects table...");
		conn.execute(
			r"CREATE TABLE IF NOT EXISTS subjects (
				id TEXT NOT NULL,
				database_id TEXT NOT NULL,
				name TEXT NOT NULL,
				created_at INTEGER NOT NULL
			)",
			turso::params![],
		)
		.await
		.map_err(|e| anyhow::anyhow!("Failed to create subjects table: {e}"))?;
		tracing::debug!("Subjects table created");

		tracing::debug!("Creating aspects table...");
		conn.execute(
			r"CREATE TABLE IF NOT EXISTS aspects (
				id TEXT NOT NULL,
				subject_id TEXT NOT NULL,
				database_id TEXT NOT NULL,
				name TEXT NOT NULL,
				table_name TEXT NOT NULL,
				resolution TEXT NOT NULL,
				created_at INTEGER NOT NULL,
				earliest_measurement TEXT,
				latest_measurement TEXT,
				compression_config TEXT
			)",
			turso::params![],
		)
		.await
		.map_err(|e| anyhow::anyhow!("Failed to create aspects table: {e}"))?;
		tracing::debug!("Aspects table created");

		// Add compression_config column for existing databases (migration)
		conn.execute("ALTER TABLE aspects ADD COLUMN compression_config TEXT", turso::params![]).await.ok(); // Ignore error if column already exists

		tracing::debug!("Creating unbatched_measurements table...");
		conn.execute(
			r"CREATE TABLE IF NOT EXISTS unbatched_measurements (
				aspect_id TEXT NOT NULL,
				data_timestamp INTEGER NOT NULL,
				queued_at INTEGER NOT NULL,
				UNIQUE(aspect_id, data_timestamp)
			)",
			turso::params![],
		)
		.await
		.map_err(|e| anyhow::anyhow!("Failed to create unbatched_measurements table: {e}"))?;
		tracing::debug!("Unbatched measurements table created");

		tracing::debug!("All tables created successfully");

		Ok(vec![Transaction::new(None, "Create transactions table".to_string()), Transaction::new(None, "Create database table".to_string()), Transaction::new(None, "Create subjects table".to_string()), Transaction::new(None, "Create aspects table".to_string()), Transaction::new(None, "Create unbatched_measurements table".to_string())])
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
	pub const fn metadata(&self) -> Option<&turso::Database> {
		self.metadata.as_ref()
	}

	pub fn set_metadata(&mut self, turso_db: Option<turso::Database>) {
		self.metadata = turso_db;
	}

	#[must_use]
	pub const fn metadata_path(&self) -> &Option<String> {
		&self.metadata_path
	}

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
				let conn = Database::begin_concurrent(turso_db, cache_key, None).await?;
				let res = conn.as_ref().query("SELECT created_at FROM database WHERE name = ?", turso::params![self.name.clone()]).await;
				let timestamp = match res {
					Ok(mut rows) => {
						tracing::debug!("Querying creation time for database '{}'", self.name);
						if let Some(row) = rows.next().await? {
							let timestamp_str = Database::value_to_string(&row.get_value(0)?, "Creation Timestamp").await?;
							if timestamp_str.is_empty() {
								let _ = Database::commit_concurrent(&conn).await;
								return Ok(None);
							}
							DateTime::parse_from_rfc3339(&timestamp_str)?.with_timezone(&Utc)
						} else {
							let _ = Database::commit_concurrent(&conn).await;
							return Ok(None);
						}
					}
					Err(e) => {
						Database::rollback_concurrent(&conn).await?;
						return Err(anyhow::anyhow!("SQL execution failure 21: in get_creation_time: `{e}`"));
					}
				};

				let _ = Database::commit_concurrent(&conn).await;
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

		let conn = Database::begin_concurrent(metadata_db, metadata_db_path, None).await?;

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
				return Err(anyhow::anyhow!("SQL execution failure 22: in get_size_stats: `{e}`"));
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
				return Err(anyhow::anyhow!(" 13: in get_size_stats: `{e}`"));
			}
		}

		let _ = Database::commit_concurrent(&conn).await;
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
