#![warn(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions, clippy::used_underscore_binding, clippy::similar_names, clippy::module_name_repetitions, clippy::module_inception)]

use std::{
	collections::HashMap, sync::{Arc, LazyLock}
};

use anyhow::{Context, Result};
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use sqlx::{Pool, Row, Sqlite, SqlitePool};
use tokio::sync::Mutex;
pub use types::*;
use uuid::Uuid;

mod types;

const DB_DIR: &str = "./databases";
static SUBJECTS: LazyLock<Arc<Mutex<HashMap<String, Pool<Sqlite>>>>> = LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

#[derive(Debug, Clone)]
pub struct DB {
	dir: String,
}

impl DB {
	/// Create a new database.
	/// # Errors
	/// Returns an error if the database initialization fails.
	pub async fn new(name: &str) -> Result<Self> {
		let db = Self { dir: DB_DIR.to_string() };

		db.initialize_existing_subjects().await?;

		// Ensure connection exists
		if !SUBJECTS.lock().await.contains_key(name) {
			println!("DEBUG: Initializing new connection for {}", name);
			db.initialize_new_subject(name).await?;
		}

		Ok(db)
	}

	pub async fn existing() -> Self {
		let db = Self { dir: DB_DIR.to_string() };
		if let Err(e) = db.initialize_existing_subjects().await {
			println!("DEBUG: Failed to initialize existing connections: {:?}", e);
		}
		db
	}

	async fn initialize_existing_subjects(&self) -> Result<()> {
		println!("DEBUG: Starting initialize_existing_subjects for {}", self.dir);
		tokio::fs::create_dir_all(&self.dir).await.context("Failed to create database directory")?;
		println!("DEBUG: Directory {} created or exists", self.dir);

		let mut entries = tokio::fs::read_dir(self.dir.clone()).await.map_err(|e| Error::ReadingDirectoryError(e.to_string()))?;

		while let Ok(entry) = entries.next_entry().await {
			let Some(entry) = entry else { break };
			let path = entry.path();
			println!("DEBUG: Processing path {:?}", path);

			if path.extension().is_some_and(|e| e == "db") {
				let path_str = path.to_str().ok_or_else(|| Error::InvalidPathError(path.to_string_lossy().into_owned())).context("Failed to convert path to string")?;
				println!("DEBUG: Attempting to initialize connection for {}", path_str);

				// Check file accessibility
				if let Err(e) = tokio::fs::metadata(&path).await {
					println!("DEBUG: Skipping inaccessible file {}: {:?}", path_str, e);
					continue;
				}

				// Use the path directly - normalize separators for consistency
				let normalized_path = path_str.replace('/', "\\");
				let database_url = format!("sqlite:{}", normalized_path);
				println!("DEBUG: Connecting to database: {}", database_url);

				let pool = match SqlitePool::connect(&database_url).await {
					Ok(pool) => pool,
					Err(e) => {
						println!("DEBUG: Failed to connect to database {}: {:?}", database_url, e);
						continue;
					}
				};

				let subject_name = path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| Error::InvalidPathError(path.to_string_lossy().into_owned())).context("Failed to get subject name from path")?;
				println!("DEBUG: Successfully connected to existing database for subject {}", subject_name);
				SUBJECTS.lock().await.insert(subject_name.to_string(), pool);
			}
		}

		println!("DEBUG: Completed initialize_existing_subjects");
		Ok(())
	}

	/// Get a dataset's id by its name.
	/// # Errors
	/// Returns an error if the database connection is not found, if the query fails, or if no dataset with the given name exists.
	pub async fn get_dataset_id_by_name(&self, subject_name: &str, name: &str) -> Result<Uuid> {
		let pool = self.get_pool(subject_name).await?;
		let row = sqlx::query("SELECT id FROM datasets WHERE name = ?").bind(name).fetch_one(&pool).await.context("Failed to query dataset ID")?;

		let id_str: String = row.get("id");
		let id = Uuid::parse_str(&id_str).context("Failed to parse dataset ID as UUID")?;
		Ok(id)
	}

	/// Add a measurement to a dataset.
	/// # Errors
	/// Returns an error if the database connection is not found, if the query fails, or if the dataset does not exist.
	pub async fn add_measurement(&self, subject_name: &str, dataset_id: Uuid, measurement: Measurement) -> Result<()> {
		let pool = self.get_pool(subject_name).await?;
		sqlx::query("INSERT INTO measurements (dataset_id, timestamp, value) VALUES (?, ?, ?)").bind(dataset_id.to_string()).bind(measurement.timestamp.to_rfc3339()).bind(measurement.value.to_string()).execute(&pool).await.context("Failed to insert measurement")?;
		Ok(())
	}

	/// Get all measurements for a specific dataset.
	/// # Errors
	/// Returns an error if the database connection is not found or if the query fails.
	pub async fn get_measurements_by_dataset_id(&self, subject_name: &str, dataset_id: Uuid) -> Result<Vec<Measurement>> {
		let pool = self.get_pool(subject_name).await?;
		println!("DEBUG: Using SQLite for reading measurements");

		let rows = sqlx::query("SELECT timestamp, value FROM measurements WHERE dataset_id = ? ORDER BY timestamp").bind(dataset_id.to_string()).fetch_all(&pool).await.context("Failed to query measurements")?;

		let mut measurements = Vec::new();
		for (index, row) in rows.iter().enumerate() {
			let timestamp_str: String = row.get("timestamp");
			let value_str: String = row.get("value");

			let timestamp = match DateTime::parse_from_rfc3339(&timestamp_str) {
				Ok(ts) => ts.with_timezone(&Utc),
				Err(e) => {
					println!("DEBUG: Failed to parse timestamp '{}' in row {}: {}", timestamp_str, index, e);
					continue;
				}
			};

			let value = match value_str.parse::<BigDecimal>() {
				Ok(v) => v,
				Err(e) => {
					println!("DEBUG: Failed to parse value '{}' in row {}: {}", value_str, index, e);
					continue;
				}
			};

			measurements.push(Measurement { timestamp, value });
		}

		println!("DEBUG: Successfully retrieved {} measurements for dataset {}", measurements.len(), dataset_id);
		Ok(measurements)
	}

	async fn get_pool(&self, subject_name: &str) -> Result<Pool<Sqlite>> {
		let connections = SUBJECTS.lock().await;
		let pool = connections.get(subject_name).cloned().ok_or_else(|| Error::ConnectingDatabaseError(format!("Database connection for {} not found", subject_name)))?;
		Ok(pool)
	}

	pub async fn initialize_new_subject(&self, subject_name: &str) -> Result<()> {
		let db_path = format!("{}/{subject_name}.db", self.dir);
		println!("DEBUG: Starting SQLite database creation for {}", subject_name);

		// Ensure directory exists first
		tokio::fs::create_dir_all(&self.dir).await.context("Failed to create database directory")?;
		println!("DEBUG: Directory {} created successfully", self.dir);

		// Create the database file if it doesn't exist
		if !tokio::fs::try_exists(&db_path).await.unwrap_or(false) {
			println!("DEBUG: Database file {} does not exist, creating it", db_path);
			tokio::fs::File::create(&db_path).await.context("Failed to create database file")?;
			println!("DEBUG: Database file {} created successfully", db_path);
		}

		// Use the relative path directly for SQLite - no need for canonicalization
		// Convert forward slashes to backslashes for Windows
		let normalized_path = db_path.replace('/', "\\");
		let database_url = format!("sqlite:{}", normalized_path);
		println!("DEBUG: Connecting to SQLite database at: {}", database_url);

		let pool = SqlitePool::connect(&database_url).await.context("Failed to connect to SQLite database")?;
		println!("DEBUG: Successfully connected to SQLite database");

		println!("DEBUG: Creating tables");
		sqlx::query(
			"CREATE TABLE IF NOT EXISTS datasets (
				id TEXT PRIMARY KEY, 
				name TEXT NOT NULL
			)",
		)
		.execute(&pool)
		.await
		.context("Failed to create datasets table")?;

		sqlx::query(
			"CREATE TABLE IF NOT EXISTS measurements (
				id INTEGER PRIMARY KEY AUTOINCREMENT, 
				dataset_id TEXT NOT NULL, 
				timestamp TEXT NOT NULL, 
				value TEXT NOT NULL, 
				FOREIGN KEY (dataset_id) REFERENCES datasets(id) ON DELETE CASCADE
			)",
		)
		.execute(&pool)
		.await
		.context("Failed to create measurements table")?;

		sqlx::query("CREATE INDEX IF NOT EXISTS idx_measurements_dataset_id ON measurements(dataset_id)").execute(&pool).await.context("Failed to create dataset_id index")?;

		sqlx::query("CREATE INDEX IF NOT EXISTS idx_measurements_timestamp ON measurements(timestamp)").execute(&pool).await.context("Failed to create timestamp index")?;

		println!("DEBUG: Storing subject {} in SUBJECTS", subject_name);
		SUBJECTS.lock().await.insert(subject_name.to_string(), pool);
		println!("DEBUG: SQLite database initialization complete for {}", subject_name);
		Ok(())
	}

	/// Add a dataset with all measurements at once using a transaction
	pub async fn add_dataset(&self, subject_name: &str, dataset: Dataset) -> Result<Uuid> {
		let pool = self.get_pool(subject_name).await?;
		println!("DEBUG: Starting add_dataset for {} with dataset ID {}", dataset.name, dataset.id);

		let mut tx = pool.begin().await.context("Failed to start transaction")?;

		// Insert the dataset first
		sqlx::query("INSERT INTO datasets (id, name) VALUES (?, ?)").bind(dataset.id.to_string()).bind(&dataset.name).execute(&mut *tx).await.context("Failed to insert dataset")?;

		// Insert all measurements
		if !dataset.measurements.is_empty() {
			let total_measurements = dataset.measurements.len();

			for (index, measurement) in dataset.measurements.iter().enumerate() {
				sqlx::query("INSERT INTO measurements (dataset_id, timestamp, value) VALUES (?, ?, ?)").bind(dataset.id.to_string()).bind(measurement.timestamp.to_rfc3339()).bind(measurement.value.to_string()).execute(&mut *tx).await.with_context(|| format!("Failed to insert measurement {} of {}", index + 1, total_measurements))?;

				if index % 100 == 0 {
					println!("DEBUG: Inserted measurement {} of {}", index + 1, total_measurements);
				}
			}

			println!("DEBUG: Successfully inserted {} measurements for dataset ID {}", total_measurements, dataset.id);
		}

		tx.commit().await.context("Failed to commit transaction")?;
		println!("DEBUG: Transaction committed successfully");

		// Verify the data
		let count_row = sqlx::query("SELECT COUNT(*) as count FROM measurements WHERE dataset_id = ?").bind(dataset.id.to_string()).fetch_one(&pool).await.context("Failed to verify measurement count")?;

		let count: i64 = count_row.get("count");
		println!("DEBUG: Verification shows {} measurements for dataset {}", count, dataset.id);

		Ok(dataset.id)
	}
}

#[cfg(test)]
mod test {
	use fake::{Fake, Faker};

	use super::*;

	#[tokio::test]
	async fn test_db_creation() {
		let db_name = "test_db_creation";
		println!("DEBUG: Starting test_db_creation for {}", db_name);
		cleanup_test_database(db_name).await;
		let _db = DB::new(db_name).await.expect("Failed to create database");
		println!("DEBUG: Database created, checking SUBJECTS");
		assert!(SUBJECTS.lock().await.contains_key(db_name));
		println!("DEBUG: Cleaning up test database");
		cleanup_test_database(db_name).await;
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_benchmark_100_measurements() {
		let db_name = "test_benchmark_100_measurements";

		// Clean up any existing test database
		cleanup_test_database(db_name).await;

		// Generate test data
		let dataset_name: String = Faker.fake();
		let dataset_id = Uuid::new_v4();
		println!("Starting benchmark with 1000 measurements for dataset: {}", dataset_name);
		println!("Dataset ID: {}", dataset_id);

		let mut measurements = Vec::new();
		for _i in 0..1000 {
			measurements.push(Faker.fake::<Measurement>());
		}

		println!("Number of measurements: {}", measurements.len());

		let dataset = Dataset { id: dataset_id, name: dataset_name.clone(), measurements };

		println!("creating new database: {}", db_name);
		let db = DB::new(db_name).await.expect("Failed to create database");

		println!("Adding dataset: {:?}", dataset.name);
		let start_time = std::time::Instant::now();

		let returned_id = db.add_dataset(db_name, dataset).await.expect("Failed to add dataset");

		let elapsed = start_time.elapsed();
		println!("Benchmark completed in: {:?}", elapsed);

		// Verify by reading back
		let measurements = db.get_measurements_by_dataset_id(db_name, returned_id).await.expect("Failed to get measurements");

		println!("Expected measurements: {}", 1000);
		println!("Actual measurements in DB: {}", measurements.len());

		assert_eq!(1000, measurements.len(), "Number of measurements in the database does not match the expected number");

		println!("Test completed successfully!");
	}

	async fn cleanup_test_database(db_name: &str) {
		let db_path = format!("{DB_DIR}/{db_name}.db");
		println!("DEBUG: Cleaning up database {}", db_path);

		// Close the connection first and ensure it's properly dropped
		if let Some(pool) = SUBJECTS.lock().await.remove(db_name) {
			println!("DEBUG: Closing database connection for {}", db_name);
			pool.close().await;
			// Give it a moment to fully close
			tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
		}

		// Remove main database file
		match tokio::fs::remove_file(&db_path).await {
			Ok(_) => println!("DEBUG: Successfully removed {}", db_path),
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
				println!("DEBUG: Database file {} already removed", db_path);
			}
			Err(e) => println!("DEBUG: Failed to remove file {}: {:?}", db_path, e),
		}

		// Remove SQLite journal file if it exists
		let journal_path = format!("{}-journal", db_path);
		if tokio::fs::remove_file(&journal_path).await.is_ok() {
			println!("DEBUG: Successfully removed journal file {}", journal_path);
		}

		// Remove SQLite WAL file if it exists
		let wal_path = format!("{}-wal", db_path);
		if tokio::fs::remove_file(&wal_path).await.is_ok() {
			println!("DEBUG: Successfully removed WAL file {}", wal_path);
		}

		// Remove SQLite SHM file if it exists
		let shm_path = format!("{}-shm", db_path);
		if tokio::fs::remove_file(&shm_path).await.is_ok() {
			println!("DEBUG: Successfully removed SHM file {}", shm_path);
		}
	}
}
