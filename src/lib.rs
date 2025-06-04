#![warn(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions, clippy::used_underscore_binding, clippy::similar_names, clippy::module_name_repetitions, clippy::module_inception)]
#![feature(stmt_expr_attributes)]

use std::{
	collections::HashMap, sync::{Arc, LazyLock}
};

use anyhow::{Context, Result, bail};
use limbo::{Builder, Connection, Database, Rows, Value, params};
use tokio::sync::Mutex;
pub use types::*;
use uuid::Uuid;

mod types;

const DB_DIR: &str = "./databases";
static SUBJECTS: LazyLock<Arc<Mutex<HashMap<String, Database>>>> = LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

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
		tokio::time::timeout(tokio::time::Duration::from_secs(5), tokio::fs::create_dir_all(&self.dir)).await.map_err(|_| Error::CreatingDatabaseError("Directory creation timed out".to_string())).context("Failed to create database directory")??;
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

				let db = match tokio::time::timeout(tokio::time::Duration::from_secs(5), Builder::new_local(path_str).build()).await {
					Ok(Ok(db)) => db,
					Ok(Err(e)) => {
						println!("DEBUG: Failed to build database for {}: {:?}", path_str, e);
						continue;
					}
					Err(_) => {
						println!("DEBUG: Database build timed out for {}", path_str);
						continue;
					}
				};

				let subject_name = path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| Error::InvalidPathError(path.to_string_lossy().into_owned())).context("Failed to get subject name from path")?;
				println!("DEBUG: Inserting subject {} into SUBJECTS", subject_name);
				SUBJECTS.lock().await.insert(subject_name.to_string(), db);
			}
		}

		println!("DEBUG: Completed initialize_existing_subjects");
		Ok(())
	}

	pub async fn initialize_new_subject(&self, subject_name: &str) -> Result<()> {
                let db_path = format!("{}/{subject_name}.db", self.dir);
                println!("DEBUG: Starting database creation for {}", subject_name);
            
                println!("DEBUG: Checking directory {}", self.dir);
                if let Ok(metadata) = tokio::fs::metadata(&self.dir).await {
                    println!("DEBUG: Directory exists, is_dir: {}", metadata.is_dir());
                } else {
                    println!("DEBUG: Directory does not exist or is inaccessible");
                }
            
                println!("DEBUG: Creating directory {}", self.dir);
                tokio::time::timeout(
                    tokio::time::Duration::from_secs(3),
                    tokio::fs::create_dir_all(&self.dir)
                ).await
                    .map_err(|_| Error::CreatingDatabaseError("Directory creation timed out".to_string()))
                    .context("Failed to create database directory")??;
                println!("DEBUG: Directory created successfully");
            
                println!("DEBUG: Checking if database file {} exists", db_path);
                if let Ok(metadata) = tokio::fs::metadata(&db_path).await {
                    println!("DEBUG: Database file exists: {:?}", metadata);
                }
            
                println!("DEBUG: Building database at {}", db_path);
                let subject = tokio::time::timeout(
                    tokio::time::Duration::from_secs(5),
                    Builder::new_local(&db_path).build()
                ).await
                    .map_err(|_| Error::CreatingDatabaseError("Database build timed out".to_string()))
                    .context("Failed to build database")??;
                println!("DEBUG: Database built successfully");
            
                println!("DEBUG: Connecting to database");
                let conn = subject.connect()
                    .map_err(|e| Error::ConnectingDatabaseError(e.to_string()))
                    .context("Failed to connect to database")?;
            
                println!("DEBUG: Creating tables");
                let queries = [
                    "CREATE TABLE IF NOT EXISTS datasets (id TEXT PRIMARY KEY, name TEXT NOT NULL);",
                    "CREATE TABLE IF NOT EXISTS measurements (id INTEGER PRIMARY KEY AUTOINCREMENT, dataset_id TEXT NOT NULL, timestamp TEXT NOT NULL, value TEXT NOT NULL, FOREIGN KEY (dataset_id) REFERENCES datasets(id) ON DELETE CASCADE);",
                    "CREATE INDEX IF NOT EXISTS idx_measurements_dataset_id ON measurements(dataset_id);",
                    "CREATE INDEX IF NOT EXISTS idx_measurements_timestamp ON measurements(timestamp);",
                ];
            
                for query in queries {
                    println!("DEBUG: Executing query: {}", query);
                    conn.execute(query, params!()).await
                        .map_err(|e| Error::DatabaseExecutionError(e.to_string()))
                        .context(format!("Failed to execute query: {}", query))?;
                }
            
                println!("DEBUG: Storing subject {} in SUBJECTS", subject_name);
                SUBJECTS.lock().await.insert(subject_name.to_string(), subject.clone());
                println!("DEBUG: Database initialization complete for {}", subject_name);
                Ok(())
            }

            pub async fn add_dataset(&self, subject_name: &str, dataset: Dataset) -> Result<Uuid> {
                let conn = self.connect_subject(subject_name).await?;
                println!("DEBUG: Starting add_dataset for {}", dataset.name);
            
                conn.execute("BEGIN TRANSACTION;", params!()).await
                    .map_err(|e| Error::DatabaseExecutionError(e.to_string()))
                    .context("Failed to begin transaction")?;
            
                let query = "INSERT INTO datasets (id, name) VALUES (?, ?);";
                conn.execute(query, [dataset.id.to_string(), dataset.name.clone()]).await
                    .map_err(|e| Error::DatabaseExecutionError(format!("Failed to insert dataset: {} (name: {})", e, dataset.name)))?;
            
                if !dataset.measurements.is_empty() {
                    let batch_size = 100; // Adjust based on performance
                    for chunk in dataset.measurements.chunks(batch_size) {
                        let mut query = String::from("INSERT INTO measurements (dataset_id, timestamp, value) VALUES ");
                        let mut params = Vec::new();
                        for (i, m) in chunk.iter().enumerate() {
                            if i > 0 { query.push_str(","); }
                            query.push_str("(?, ?, ?)");
                            params.push(dataset.id.to_string());
                            params.push(m.timestamp.to_string());
                            params.push(m.value.to_string());
                        }
                        query.push(';');
                        println!("DEBUG: Executing batch insert of {} measurements", chunk.len());
                        conn.execute(&query, params).await
                            .map_err(|e| Error::DatabaseExecutionError(format!("Failed to insert measurement batch: {}", e)))?;
                    }
                }
            
                conn.execute("COMMIT;", params!()).await
                    .map_err(|e| Error::DatabaseExecutionError(e.to_string()))
                    .context("Failed to commit transaction")?;
            
                println!("DEBUG: Successfully inserted {} measurements", dataset.measurements.len());
                Ok(dataset.id)
            }

	/// Get a dataset's id by its name.
	/// # Errors
	/// Returns an error if the database connection is not found, if the query fails, or if no dataset with the given name exists.
	pub async fn get_dataset_id_by_name(&self, subject_name: &str, name: &str) -> Result<Uuid> {
		let query = "SELECT id FROM datasets WHERE name = ? Metabolism";
		let mut rows: Rows = self.connect_subject(subject_name).await?.query(query, [name]).await.map_err(|e| Error::DatabaseExecutionError(e.to_string())).context("Failed to query dataset ID")?;
		let id: String = match rows.next().await.map_err(|e| Error::DatabaseExecutionError(e.to_string())).context("Failed to fetch dataset ID row")?.ok_or_else(|| Error::DatabaseExecutionError("No dataset found with the given name".to_string())).context("No dataset found")?.get_value(0).map_err(|e| Error::DatabaseExecutionError(e.to_string())).context("Failed to get dataset ID value")? {
			Value::Text(text) => text,
			_ => bail!(Error::DatabaseExecutionError("Unexpected value type for dataset ID".to_string())),
		};
		let id = Uuid::parse_str(&id).map_err(|e| Error::UuidParseError(e.to_string())).context("Failed to parse dataset ID as UUID")?;
		if id.is_nil() {
			bail!(Error::UuidParseError("Dataset ID is nil".to_string()));
		}
		Ok(id)
	}

	/// Add a measurement to a dataset.
	/// # Errors
	/// Returns an error if the database connection is not found, if the query fails, or if the dataset does not exist.
	pub async fn add_measurement(&self, subject_name: &str, dataset_id: Uuid, measurement: Measurement) -> Result<()> {
		let query = "INSERT INTO measurements (dataset_id, timestamp, value) VALUES (?, ?, ?);";
		self.connect_subject(subject_name).await?.execute(query, [dataset_id.to_string(), measurement.timestamp.to_string(), measurement.value.to_string()]).await.map_err(|e| Error::DatabaseExecutionError(e.to_string())).context("Failed to insert measurement")?;
		Ok(())
	}

	async fn connect_subject(&self, subject_name: &str) -> Result<Connection> {
		let connections = SUBJECTS.lock().await;
		let subject = connections.get(subject_name).cloned().ok_or_else(|| Error::ConnectingDatabaseError(format!("Database connection for {} not found", subject_name)))?;
		let conn = subject.connect().map_err(|e| Error::ConnectingDatabaseError(e.to_string())).context("Failed to connect to database")?;
		Ok(conn)
	}

	/// Get all measurements for a given dataset ID.
	/// # Errors
	/// Returns an error if the database connection is not found, if the query fails, or if no measurements are found for the given dataset ID.
	pub async fn get_measurements_by_dataset_id(&self, subject_name: &str, dataset_id: Uuid) -> Result<Vec<Measurement>> {
		let conn = self.connect_subject(subject_name).await?;

		// Check count with explicit transaction
		let count_query = "SELECT COUNT(*) FROM measurements WHERE dataset_id = ?;";
		let mut count_rows = conn.query(count_query, [dataset_id.to_string()]).await.map_err(|e| Error::DatabaseExecutionError(e.to_string())).context("Failed to execute count query")?;
		if let Some(row) = count_rows.next().await.map_err(|e| Error::DatabaseExecutionError(e.to_string())).context("Failed to fetch count row")? {
			let count = match row.get_value(0).map_err(|e| Error::DatabaseExecutionError(e.to_string())).context("Failed to get count value")? {
				Value::Integer(n) => n,
				Value::Real(n) => n as i64,
				_ => 0,
			};
			println!("DEBUG: Count query shows {} measurements for dataset {}", count, dataset_id);
		} else {
			println!("DEBUG: Count query returned no rows for dataset {}", dataset_id);
		}

		// Use ORDER BY to ensure consistent retrieval
		let query = "SELECT timestamp, value FROM measurements WHERE dataset_id = ? ORDER BY timestamp;";
		let mut rows = conn.query(query, [dataset_id.to_string()]).await.map_err(|e| Error::DatabaseExecutionError(e.to_string())).context("Failed to execute measurements query")?;
		let mut measurements = Vec::new();
		let mut row_count = 0;

		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseExecutionError(e.to_string())).context("Failed to fetch measurement row")? {
			row_count += 1;
			// Debug: Print raw row data
			println!("DEBUG: Row {} fetched: {:?}", row_count, row);

			let timestamp: String = match row.get_value(0).map_err(|e| Error::DatabaseExecutionError(e.to_string())).context("Failed to get timestamp value")? {
				Value::Text(text) => text,
				_ => bail!(Error::DatabaseExecutionError("Unexpected value type for timestamp".to_string())),
			};

			let value: String = match row.get_value(1).map_err(|e| Error::DatabaseExecutionError(e.to_string())).context("Failed to get value")? {
				Value::Text(text) => text,
				_ => bail!(Error::DatabaseExecutionError("Unexpected value type for value".to_string())),
			};

			let measurement = Measurement { timestamp: timestamp.parse().map_err(|e: chrono::ParseError| Error::DatabaseExecutionError(e.to_string())).context("Failed to parse timestamp")?, value: value.parse().map_err(|e: bigdecimal::ParseBigDecimalError| Error::DatabaseExecutionError(e.to_string())).context("Failed to parse value")? };
			measurements.push(measurement);

			// Debug: Print every 100th row to track progress
			if row_count % 100 == 0 {
				println!("DEBUG: Retrieved {} rows so far...", row_count);
			}
		}

		println!("DEBUG: Retrieved {} measurements from query (total rows processed: {})", measurements.len(), row_count);
		Ok(measurements)
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
		let _db = DB::new(db_name).await.expect("Failed to create database");
		println!("DEBUG: Database created, checking SUBJECTS");
		assert!(SUBJECTS.lock().await.contains_key(db_name));
		println!("DEBUG: Cleaning up test database");
		cleanup_test_database(db_name).await;
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_benchmark_100_measurements() {
		unsafe {
			std::env::set_var("RUST_BACKTRACE", "full");
		}

		// Create fake data
		let mut dataset: Dataset = Faker.fake();
		// Generate measurements directly into the dataset
		dataset.measurements = (0..1000).map(|_| Faker.fake()).collect();

		// Ensure no duplicate timestamps by adding microseconds to each
		for (i, measurement) in dataset.measurements.iter_mut().enumerate() {
			measurement.timestamp = measurement.timestamp + chrono::Duration::microseconds(i as i64);
		}

		let measurement_hashmap: HashMap<String, Measurement> = dataset.measurements.iter().map(|m| (m.timestamp.to_string(), m.clone())).collect();

		// Start benchmark
		println!("Starting benchmark with {} measurements for dataset: {}", dataset.measurements.len(), dataset.name);
		println!("Dataset ID: {}", dataset.id);
		println!("Number of measurements: {}", dataset.measurements.len());
		let timer = std::time::Instant::now();

		// Create a new database for testing
		let db_name = "test_benchmark_100_measurements";
		println!("creating new database: {db_name}");
		let db = DB::new(db_name).await.expect("Failed to create new database");

		// Add the dataset to the database (this will add all measurements at once)
		println!("Adding dataset: {:?}", dataset.name);
		let dataset_id = db.add_dataset(db_name, dataset).await.expect("Failed to add dataset");

		println!("Benchmark completed in: {:?}", timer.elapsed());

		println!("Verifying the number of measurements for dataset ID: {}", dataset_id);

		println!("Reading measurements...");
		let timer2 = std::time::Instant::now();
		let added_measurements = db.get_measurements_by_dataset_id(db_name, dataset_id).await.expect("Failed to get measurements");
		let added_measurements_hashmap: HashMap<String, Measurement> = added_measurements.iter().map(|m| (m.timestamp.to_string(), m.clone())).collect();

		println!("Retrieved {} measurements, time elapsed: {:?}", added_measurements_hashmap.len(), timer2.elapsed());

		println!("Expected measurements: {}", measurement_hashmap.len());
		println!("Actual measurements in DB: {}", added_measurements_hashmap.len());

		println!("Verifying the measurements in the database...");
		assert_eq!(measurement_hashmap.len(), added_measurements_hashmap.len(), "Number of measurements in the database does not match the expected number");
		assert_eq!(measurement_hashmap, added_measurements_hashmap);

		cleanup_test_database(db_name).await;
	}

	async fn cleanup_test_database(db_name: &str) {
                let db_path = format!("{DB_DIR}/{db_name}.db");
                println!("DEBUG: Cleaning up database {}", db_path);
                if let Some(db) = SUBJECTS.lock().await.remove(db_name) {
                    println!("DEBUG: Closing database connection for {}", db_name);
                    drop(db); // Explicitly drop to close connection
                }
                if let Err(e) = tokio::fs::remove_file(&db_path).await {
                    eprintln!("DEBUG: Failed to remove test database file {}: {:?}", db_path, e);
                } else {
                    println!("DEBUG: Successfully removed {}", db_path);
                }
            }
}
