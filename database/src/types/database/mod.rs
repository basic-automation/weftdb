use std::{
	collections::HashMap, path::Path, sync::{Arc, LazyLock}
};

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use splimes::Resolution;
use tokio::sync::Mutex;
use traits::DatabaseStructure;
use turso::Builder;
use uuid::Uuid;

use crate::{
	subject, types::{
		database::traits::{aspect_structure::AspectStructure, database_structure::DatabaseStructure}, Transaction, TxId
	}, AspectId, Error, Subject, SubjectId, DEFAULT_DATA_DIR
};

pub type DatabaseMap = Arc<Mutex<HashMap<DatabaseId, DatabaseInfo>>>;
pub static DATABASES: LazyLock<DatabaseMap> = LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

// Add connection manager for Turso
static CONNECTION_DATABASES: LazyLock<Arc<Mutex<HashMap<String, turso::Database>>>> = LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

// Add dedicated write connections for concurrency (Turso handles this internally)

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

#[async_trait::async_trait]
impl DatabaseStructure for Database {
	/// Create a Turso database for reuse with concurrent writes enabled
	async fn create_turso_database(db_path: &str) -> Result<turso::Database> {
		// if the db is already in cache return error.
		if let Some(_) = CONNECTION_DATABASES.lock().await.get(db_path) {
			bail!("Database already exists.");
		}

		// if db_path file exists, return error
		if Path::new(db_path).exists() {
			bail!("Database file already exists.");
		}

		// Create Turso database
		let turso_db = Builder::new_local(db_path).build().await?;

		// Configure database for concurrent writes
		{
			let conn = turso_db.connect()?;

			// Enable WAL mode - required for concurrent writes
			conn.execute("PRAGMA journal_mode=WAL", turso::params![]).await?;

			// Set busy timeout for handling transient locks
			conn.execute("PRAGMA busy_timeout = 30000", turso::params![]).await?;

			// Optimize for concurrent access
			conn.execute("PRAGMA synchronous = NORMAL", turso::params![]).await?;
		}

		CONNECTION_DATABASES.lock().await.insert(db_path.to_string(), turso_db.clone());

		Ok(turso_db)
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
				conn.execute("PRAGMA journal_mode=WAL", turso::params![]).await?;

				// Set busy timeout for handling transient locks
				conn.execute("PRAGMA busy_timeout = 30000", turso::params![]).await?;

				// Optimize for concurrent access
				conn.execute("PRAGMA synchronous = NORMAL", turso::params![]).await?;
			}

			CONNECTION_DATABASES.lock().await.insert(db_path.to_string(), turso_db.clone());
			return Ok(turso_db);
		}

		bail!("Database not found")
	}

	async fn record_transaction(&self, message: &str) -> Result<TxId> {
		let tx = Transaction::new(None, message.to_string());
		let tx_id = tx.id();

		// Use INSERT ... ON CONFLICT for concurrent writes support
		// This handles the case where a transaction with the same ID already exists (very rare but possible)
		let conn = self.metadata.connect()?;
		conn.execute("INSERT INTO transactions (id, message, created_at) VALUES (?, ?, ?) ON CONFLICT(id) DO NOTHING", turso::params![tx_id.as_uuid().to_string(), tx.message(), tx.created_at().timestamp_millis().to_string()]).await?;

		Ok(tx_id)
	}

	async fn log_transaction(&self, transaction: &Transaction) -> Result<()> {
		// Use INSERT ... ON CONFLICT for concurrent writes support
		let conn = self.metadata.connect()?;
		conn.execute("INSERT INTO transactions (id, message, created_at) VALUES (?, ?, ?) ON CONFLICT(id) DO NOTHING", turso::params![transaction.id().as_uuid().to_string(), transaction.message(), transaction.created_at().timestamp_millis().to_string()]).await?;

		Ok(())
	}

	async fn metadata_database_create_transactions_table(conn: &turso::Connection) -> Result<Transaction> {
		// Use IF NOT EXISTS for concurrent writes safety
		conn.execute(
			r"
                                CREATE TABLE IF NOT EXISTS transactions (
                                        id TEXT PRIMARY KEY,
                                        message TEXT NOT NULL,
                                        created_at INTEGER NOT NULL
                                )
                        ",
			turso::params![],
		)
		.await?;

		Ok(Transaction::new(None, "Create transactions table".to_string()))
	}

	async fn metadata_database_create_database_table(conn: &turso::Connection) -> Result<Transaction> {
		conn.execute(
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
		.await?;
		Ok(Transaction::new(None, "Create database table".to_string()))
	}

	async fn metadata_database_create_subjects_table(conn: &turso::Connection) -> Result<Transaction> {
		conn.execute(
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
		.await?;
		Ok(Transaction::new(None, "Create subjects table".to_string()))
	}

	async fn metadata_database_create_aspects_table(conn: &turso::Connection) -> Result<Transaction> {
		conn.execute(
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
		.await?;
		Ok(Transaction::new(None, "Create aspects table".to_string()))
	}

	async fn wireframe_metadata_database(turso_db: &turso::Database) -> Result<Vec<Transaction>> {
		let conn = turso_db.connect()?;
		let create_transactions_table_transaction = Self::metadata_database_create_transactions_table(&conn).await?;
		let create_database_table_transaction = Self::metadata_database_create_database_table(&conn).await?;
		let create_subjects_table_transaction = Self::metadata_database_create_subjects_table(&conn).await?;
		let create_aspects_table_transaction = Self::metadata_database_create_aspects_table(&conn).await?;

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
		let metadata_turso_db = Self::create_turso_database(&metadata_db_path).await?;

		// Create metadata tables
		let mut transactions = Self::wireframe_metadata_database(&metadata_turso_db).await?;

		// Insert database metadata
		let conn = metadata_turso_db.connect()?;
		let mut result = conn.query("INSERT INTO database_metadata (id, name, created_at, metadata_path) VALUES (?, ?, ?, ?)", turso::params![db_id.as_uuid().to_string(), name.to_string(), chrono::Utc::now().timestamp_millis().to_string(), metadata_db_path]).await?;

		// Consume the result to ensure the insert completes
		while (result.next().await?).is_some() {}

		transactions.push(Transaction::new(None, "Insert database metadata".to_string()));

		let mut db_info = DatabaseInfo::new(name.to_string(), db_path);
		db_info.set_metadata(Some(metadata_turso_db.clone()));
		DATABASES.lock().await.insert(db_id, db_info);

		let db = Self { id: db_id, name: name.to_string(), metadata: metadata_turso_db, metadata_path: metadata_db_path };

		for transaction in transactions {
			self.log_transaction(&transaction).await?;
		}

		Ok(db)
	}

	/// Helper function to convert Turso values to strings, handling different storage formats
	async fn value_to_string(value: &turso::Value, field_name: &str) -> Result<String> {
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

	const fn id(&self) -> DatabaseId {
		self.id
	}

	fn name(&self) -> &str {
		&self.name
	}

	const fn metadata(&self) -> &turso::Database {
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
		let data_dir = DEFAULT_DATA_DIR;
		let db_path = format!("{data_dir}/{name}");

		// Check if folder exists
		if !Path::new(&db_path).exists() {
			bail!("Database folder does not exist: {}", db_path);
		}

		// Use shared connection database
		let metadata_db_path = &format!("{db_path}/metadata.db");
		let metadata_turso_db = Self::get_turso_database(metadata_db_path).await?;

		// Query database ID from metadata
		let conn = metadata_turso_db.connect()?;
		let mut rows = conn.query("SELECT id FROM database WHERE name = ?", turso::params![name]).await?;
		let row = rows.next().await?.ok_or_else(|| anyhow::anyhow!("Database not found"))?;

		let db_id_str = Self::value_to_string(&row.get_value(0)?, "DB ID").await?;
		let db_id = DatabaseId::from_uuid(Uuid::parse_str(&db_id_str)?);

		// Load subjects with database_id
		let conn = metadata_turso_db.connect()?;
		let mut subject_rows = conn.query("SELECT id, database_id, name, created_at FROM subjects", ()).await?;

		let mut db_info = DatabaseInfo::new(name.to_string(), db_path.clone());
		db_info.set_id(db_id);
		db_info.set_metadata(Some(metadata_turso_db.clone()));

		// Manually construct subjects from rows
		while let Some(row) = subject_rows.next().await? {
			let subject_id_str = Self::value_to_string(&row.get_value(0)?, "Subject ID").await?;
			let subject_name = Self::value_to_string(&row.get_value(2)?, "Subject name").await?;
			let subject_id = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str)?);
			let mut subject = Subject::new(Some(subject_id), subject_name.clone(), db_id, metadata_db_path.clone());

			// Load aspects for this subject
			let mut aspect_rows = conn.query("SELECT id, name, table_name, resolution FROM aspects WHERE subject_id = ?", turso::params![subject_id.as_uuid().to_string()]).await?;

			while let Some(aspect_row) = aspect_rows.next().await? {
				let aspect_id_str = Self::value_to_string(&aspect_row.get_value(0)?, "Aspect ID").await?;
				let aspect_name = Self::value_to_string(&aspect_row.get_value(1)?, "Aspect name").await?;
				let resolution_str = Self::value_to_string(&aspect_row.get_value(3)?, "Resolution").await?;
				let aspect_id = AspectId::from_uuid(Uuid::parse_str(&aspect_id_str)?);
				let resolution: Resolution = serde_json::from_str(&resolution_str)?;

				let aspect = crate::Aspect::new(Some(aspect_id), aspect_name, subject_id, resolution, metadata_db_path.clone()).await?;
				subject.add_aspect(aspect);
			}

			db_info.add_subject(subject);
		}

		DATABASES.lock().await.insert(db_id, db_info);

		Ok(Self { id: db_id, name: name.to_string(), metadata: metadata_turso_db, metadata_path: metadata_db_path.clone() })
	}

	pub async fn get_database_info(&self) -> Option<DatabaseInfo> {
		DATABASES.lock().await.get(&self.id).cloned()
	}

	/// Closes the database, releasing all resources and removing from global map
	///
	/// # Errors
	///
	/// Returns an error if there are issues closing connection pools or removing resources.
	pub async fn close(&self) -> Result<()> {
		{
			let mut databases = DATABASES.lock().await;
			if let Some(_db_info) = databases.remove(&self.id) {
				// Turso databases don't need explicit closing like SQLx pools
				// The connections will be closed when dropped
			}
		}

		// Also remove from connection databases
		let db_path = format!("{}/{}/metadata.db", DEFAULT_DATA_DIR, self.name);
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

		let db_dir = format!("{}/{}", DEFAULT_DATA_DIR, self.name);
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
		// Check if subject already exists
		if self.get_subject_by_name(name).await.is_ok() {
			return Err(anyhow::anyhow!("Subject '{}' already exists", name));
		}

		// Create subject folder
		let subject_path = format!("{}/{}/{}", DEFAULT_DATA_DIR, self.name, name);
		tokio::fs::create_dir_all(&subject_path).await?;

		// Add subject to database metadata
		let metadata_db = self.metadata.connect()?;

		// create subject
		let subject = Subject::new(None, name.to_string(), self.id, self.metadata_path.clone());

		// Add subject to metadata database
		metadata_db.execute("INSERT INTO subjects (name, database_id) VALUES (?, ?)", turso::params![name, self.id.as_uuid().to_string()]).await?;

		// Log the transaction
		self.record_transaction(&format!("Added subject '{}'", name)).await?;

		Ok(subject)
	}

	/// Get a subject by its ID
	async fn get_subject(&self, id: SubjectId) -> Result<Subject> {
		// Retrieve subject from metadata database
		let conn = self.metadata.connect()?;
		let mut rows = conn.query("SELECT id, name, database_id FROM subjects WHERE id = ?", turso::params![id.as_uuid().to_string()]).await?;

		if let Some(row) = rows.next().await? {
			let subject_id_str = Self::value_to_string(&row.get_value(0)?, "Subject ID").await?;
			let name = Self::value_to_string(&row.get_value(1)?, "Subject name").await?;
			let database_id_str = Self::value_to_string(&row.get_value(2)?, "Database ID").await?;

			let subject_id = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str)?);
			let database_id = DatabaseId::from_uuid(Uuid::parse_str(&database_id_str)?);

			Ok(Subject::new(Some(subject_id), name, database_id, self.metadata_path.clone()))
		} else {
			Err(anyhow::anyhow!("Subject not found"))
		}
	}

	/// Get a subject by its name
	async fn get_subject_by_name(&self, name: &str) -> Result<Subject> {
		let conn = self.metadata.connect()?;
		let mut rows = conn.query("SELECT id, name, database_id FROM subjects WHERE name = ?", turso::params![name]).await?;

		if let Some(row) = rows.next().await? {
			let subject_id_str = Self::value_to_string(&row.get_value(0)?, "Subject ID").await?;
			let name = Self::value_to_string(&row.get_value(1)?, "Subject name").await?;
			let database_id_str = Self::value_to_string(&row.get_value(2)?, "Database ID").await?;

			let subject_id = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str)?);
			let database_id = DatabaseId::from_uuid(Uuid::parse_str(&database_id_str)?);

			Ok(Subject::new(Some(subject_id), name, database_id, self.metadata_path.clone()))
		} else {
			Err(anyhow::anyhow!("Subject not found"))
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

		// Delete the subject folder and all child files and folders
		let subject_path = format!("{}/{}/{}", DEFAULT_DATA_DIR, self.name, subject.name());
		tokio::fs::remove_dir_all(&subject_path).await?;

		// Remove subject from database metadata
		let metadata_db = self.metadata().connect()?;
		metadata_db.execute("DELETE FROM subjects WHERE id = ?", turso::params![id.as_uuid().to_string()]).await?;

		// Log the transaction
		self.record_transaction(&format!("Removed subject '{}'", subject.name())).await?;

		Ok(())
	}

	/// List all subjects in the database
	async fn list_subjects(&self) -> Result<Vec<Subject>> {
		let conn = self.metadata.connect()?;

		let mut rows = conn.query("SELECT id, name, database_id FROM subjects WHERE database_id = ?", turso::params![self.id.as_uuid().to_string()]).await?;
		let mut subjects = Vec::new();

		while let Some(row) = rows.next().await? {
			let subject_id_str = Self::value_to_string(&row.get_value(0)?, "Subject ID").await?;
			let name = Self::value_to_string(&row.get_value(1)?, "Subject name").await?;
			let database_id_str = Self::value_to_string(&row.get_value(2)?, "Database ID").await?;

			let subject_id = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str)?);
			let database_id = DatabaseId::from_uuid(Uuid::parse_str(&database_id_str)?);

			subjects.push(Subject::new(Some(subject_id), name, database_id, self.metadata_path.clone()));
		}

		Ok(subjects)
	}

	/// List all tracked aspects of a subject
	async fn list_aspects(&self, subject_id: SubjectId) -> Result<Vec<Aspect>> {
		let conn = self.metadata.connect()?;

		let mut rows = conn.query("SELECT id, name, subject_id, resolution FROM aspects WHERE subject_id = ?", turso::params![subject_id.as_uuid().to_string()]).await?;
		let mut aspects = Vec::new();

		while let Some(row) = rows.next().await? {
			let aspect_id_str = Self::value_to_string(&row.get_value(0)?, "Aspect ID").await?;
			let name = Self::value_to_string(&row.get_value(1)?, "Aspect name").await?;
			let subject_id_str = Self::value_to_string(&row.get_value(2)?, "Subject ID").await?;
			let resolution_str = Self::value_to_string(&row.get_value(3)?, "Resolution").await?;

			let aspect_id = AspectId::from_uuid(Uuid::parse_str(&aspect_id_str)?);
			let subject_id = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str)?);
			let resolution: Resolution = serde_json::from_str(&resolution_str)?;

			aspects.push(Aspect::new(Some(aspect_id), name, subject_id, resolution, self.metadata_path.clone()).await?);
		}

		Ok(aspects)
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
		// Check if subject exists
		let subject = self.get_subject(subject_id).await?;

		// Check if aspect already exists for the subject
		let existing_aspects = self.list_aspects(subject_id).await?;
		if existing_aspects.iter().any(|a| a.name() == name) {
			return Err(anyhow::anyhow!("Aspect '{}' already exists for subject '{}'", name, subject.name()));
		}

		// Create a new aspect folder
		let aspect_path = format!("{}/{}/{}/{}", DEFAULT_DATA_DIR, self.name, subject.name(), name);
		tokio::fs::create_dir_all(&aspect_path).await?;

		// Add aspect to database metadata
		let aspect_id = AspectId::new();
		let metadata_db = self.metadata().connect()?;
		metadata_db.execute("INSERT INTO aspects (id, name, subject_id, resolution) VALUES (?, ?, ?, ?)", turso::params![aspect_id.as_uuid().to_string(), name, subject_id.as_uuid().to_string(), serde_json::to_string(&resolution)?]).await?;

		// Initialize and wireframe aspect data stores
		let aspect = Aspect::new(Some(aspect_id), name.to_string(), subject_id, resolution, self.metadata_path.clone()).await?;

		// Log the transaction
		self.record_transaction(&format!("Tracking new aspect '{}' for subject '{}'", name, subject.name())).await?;

		Ok(aspect)
	}

	async fn get_aspect(&self, id: AspectId) -> Result<Aspect> {
		let conn = self.metadata.connect()?;
		let mut rows = conn.query("SELECT id, name, subject_id, resolution FROM aspects WHERE id = ?", turso::params![id.as_uuid().to_string()]).await?;

		if let Some(row) = rows.next().await? {
			let aspect_id_str = Self::value_to_string(&row.get_value(0)?, "Aspect ID").await?;
			let name = Self::value_to_string(&row.get_value(1)?, "Aspect name").await?;
			let subject_id_str = Self::value_to_string(&row.get_value(2)?, "Subject ID").await?;
			let resolution_str = Self::value_to_string(&row.get_value(3)?, "Resolution").await?;

			let aspect_id = AspectId::from_uuid(Uuid::parse_str(&aspect_id_str)?);
			let subject_id = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str)?);
			let resolution: Resolution = serde_json::from_str(&resolution_str)?;

			return Ok(Aspect::new(Some(aspect_id), name, subject_id, resolution, self.metadata_path.clone()).await?);
		}

		bail!("Aspect not found")
	}

	async fn get_aspect_by_name(&self, name: &str) -> Result<Aspect> {
		let conn = self.metadata.connect()?;
		let mut rows = conn.query("SELECT id, name, subject_id, resolution FROM aspects WHERE name = ?", turso::params![name]).await?;

		if let Some(row) = rows.next().await? {
			let aspect_id_str = Self::value_to_string(&row.get_value(0)?, "Aspect ID").await?;
			let name = Self::value_to_string(&row.get_value(1)?, "Aspect name").await?;
			let subject_id_str = Self::value_to_string(&row.get_value(2)?, "Subject ID").await?;
			let resolution_str = Self::value_to_string(&row.get_value(3)?, "Resolution").await?;

			let aspect_id = AspectId::from_uuid(Uuid::parse_str(&aspect_id_str)?);
			let subject_id = SubjectId::from_uuid(Uuid::parse_str(&subject_id_str)?);
			let resolution: Resolution = serde_json::from_str(&resolution_str)?;

			return Ok(Aspect::new(Some(aspect_id), name, subject_id, resolution, self.metadata_path.clone()).await?);
		}

		bail!("Aspect not found")
	}

	async fn get_earliest_measurement(&self, aspect_id: &AspectId) -> Result<Option<DateTime<Utc>>> {
		let conn = self.metadata.connect()?;
		let mut rows = conn.query("SELECT MIN(timestamp) FROM measurements WHERE aspect_id = ?", turso::params![aspect_id.as_uuid().to_string()]).await?;

		if let Some(row) = rows.next().await? {
			let timestamp_str = Self::value_to_string(&row.get_value(0)?, "Earliest Measurement Timestamp").await?;
			if timestamp_str.is_empty() {
				return Ok(None);
			}
			let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)?.with_timezone(&Utc);
			return Ok(Some(timestamp));
		}

		Ok(None)
	}

	async fn get_latest_measurement(&self, aspect_id: &AspectId) -> Result<Option<DateTime<Utc>>> {
		let conn = self.metadata.connect()?;
		let mut rows = conn.query("SELECT MAX(timestamp) FROM measurements WHERE aspect_id = ?", turso::params![aspect_id.as_uuid().to_string()]).await?;

		if let Some(row) = rows.next().await? {
			let timestamp_str = Self::value_to_string(&row.get_value(0)?, "Latest Measurement Timestamp").await?;
			if timestamp_str.is_empty() {
				return Ok(None);
			}
			let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)?.with_timezone(&Utc);
			return Ok(Some(timestamp));
		}

		Ok(None)
	}

	async fn get_aspect_resolution(&self, aspect_id: &AspectId) -> Result<Option<Resolution>> {
		let conn = self.metadata.connect()?;
		let mut rows = conn.query("SELECT resolution FROM aspects WHERE id = ?", turso::params![aspect_id.as_uuid().to_string()]).await?;

		if let Some(row) = rows.next().await? {
			let resolution_str = Self::value_to_string(&row.get_value(0)?, "Aspect Resolution").await?;
			if resolution_str.is_empty() {
				return Ok(None);
			}
			let resolution: Resolution = serde_json::from_str(&resolution_str)?;
			return Ok(Some(resolution));
		}

		Ok(None)
	}

	async fn update_aspect_timestamps(&self, aspect: &Aspect, min_new: DateTime<Utc>, max_new: DateTime<Utc>) -> Result<()> {
		let conn = self.metadata.connect()?;
		conn.execute("UPDATE aspects SET min_timestamp = ?, max_timestamp = ? WHERE id = ?", turso::params![min_new.to_rfc3339(), max_new.to_rfc3339(), aspect.id().as_uuid().to_string()]).await?;
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
	metadata: Option<turso::Database>,
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
		Self { id: DatabaseId::default(), name, path, subjects: HashMap::new(), metadata: None }
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
			let conn = turso_db.connect()?;
			let mut rows = conn.query("SELECT created_at FROM database_metadata WHERE name = ?", turso::params![self.name.clone()]).await?;

			if let Some(row) = rows.next().await? {
				let timestamp_millis_str = Database::value_to_string(&row.get_value(0)?, "Timestamp").await?;
				let timestamp_millis: i64 = timestamp_millis_str.parse()?;
				return Ok(DateTime::from_timestamp_millis(timestamp_millis));
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
		let mut stats = DatabaseStats::default();

		if let Some(turso_db) = &self.metadata {
			let conn = turso_db.connect()?;

			// Count subjects
			let mut subject_rows = conn.query("SELECT COUNT(*) as count FROM subjects", ()).await?;
			if let Some(row) = subject_rows.next().await? {
				let count_str = Database::value_to_string(&row.get_value(0)?, "Count").await?;
				stats.subject_count = count_str.parse::<usize>()?;
			}

			// Count aspects
			let mut aspect_rows = conn.query("SELECT COUNT(*) as count FROM aspects", ()).await?;
			if let Some(row) = aspect_rows.next().await? {
				let count_str = Database::value_to_string(&row.get_value(0)?, "Count").await?;
				stats.aspect_count = count_str.parse::<usize>()?;
			}
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
