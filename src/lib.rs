#![warn(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions, clippy::used_underscore_binding, clippy::similar_names, clippy::module_name_repetitions, clippy::module_inception)]
#![feature(stmt_expr_attributes)]

use std::{
	collections::HashMap, sync::{Arc, LazyLock}
};

use anyhow::{Result, bail};
use limbo::{Builder, Connection, Rows, Value, params};
use tokio::sync::Mutex;
pub use types::*;
use uuid::Uuid;

mod types;

const DB_DIR: &str = "./databases";
static DATABASE_CONNECTIONS: LazyLock<Arc<Mutex<HashMap<String, Connection>>>> = LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

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

		#[rustfmt::skip]
		if let Err(e) = db.initialize_existing_connections().await && let Ok(Error::InitializingExistingConnectionsError(inner)) = e.downcast::<Error>() && let Error::ReadingDirectoryError(_) = *inner { db.initialize_new_connection(name).await? }
		if !DATABASE_CONNECTIONS.lock().await.contains_key(name) {
			db.initialize_new_connection(name).await?;
		}
		Ok(db)
	}

	pub async fn existing() -> Self {
		let db = Self { dir: DB_DIR.to_string() };
		let _ = db.initialize_existing_connections().await;
		db
	}

	/// Loop over *.db files in the `DB_DIR` and initialize connections for each file.
	///
	/// # Errors
	/// If the directory cannot be read or a database connection fails, an error is returned.
	async fn initialize_existing_connections(&self) -> Result<()> {
		let mut entries = tokio::fs::read_dir(self.dir.clone()).await.map_err(|e| Error::InitializingExistingConnectionsError(Box::new(Error::ReadingDirectoryError(e.to_string()))))?;

		while let Ok(entry) = entries.next_entry().await {
			let Some(entry) = entry else { return Ok(()) };
			let path = entry.path();

			if path.extension().is_some_and(|e| e == "db") {
				let path_str = path.to_str().ok_or_else(|| Error::InitializingExistingConnectionsError(Box::new(Error::InvalidPathError(path.to_string_lossy().into_owned()))))?;
				let db = Builder::new_local(path_str).build().await.map_err(|e| Error::InitializingExistingConnectionsError(Box::new(Error::CreatingDatabaseError(e.to_string()))))?;
				let conn = db.connect().map_err(|e| Error::InitializingExistingConnectionsError(Box::new(Error::ConnectingDatabaseError(e.to_string()))))?;
				let stem = path.file_stem().ok_or_else(|| Error::InitializingExistingConnectionsError(Box::new(Error::InvalidPathError(path.to_string_lossy().into_owned()))))?.to_str().ok_or_else(|| Error::InitializingExistingConnectionsError(Box::new(Error::InvalidPathError(path.to_string_lossy().into_owned()))))?;
				DATABASE_CONNECTIONS.lock().await.insert(stem.to_string(), conn);
			}
		}

		Ok(())
	}

	/// Initialize a new database and store the connection.
	/// # Errors
	/// Returns an error if the database connection fails or if the table creation queries fail.
	pub async fn initialize_new_connection(&self, db_name: &str) -> Result<()> {
		let db_path = format!("{}/{db_name}.db", self.dir);

		// recursively create the directory if it does not exist
		tokio::fs::create_dir_all(&self.dir).await.map_err(|e| Error::CreatingDatabaseError(e.to_string()))?;

		let db = Builder::new_local(&db_path).build().await.map_err(|e| Error::CreatingDatabaseError(e.to_string()))?;
		let conn = db.connect().map_err(|e| Error::ConnectingDatabaseError(e.to_string()))?;

		let create_datasets_table = "CREATE TABLE datasets (id TEXT PRIMARY KEY, name TEXT NOT NULL);";
		let create_measurements_table = "CREATE TABLE measurements (id INTEGER PRIMARY KEY AUTOINCREMENT, dataset_id TEXT NOT NULL, timestamp TEXT NOT NULL, value TEXT NOT NULL, FOREIGN KEY (dataset_id) REFERENCES datasets(id) ON DELETE CASCADE);";
		let create_index_measurments = "CREATE INDEX idx_measurements_dataset_id ON measurements(dataset_id);";
		let create_index_measurments_timestamp = "CREATE INDEX idx_measurements_timestamp ON measurements(timestamp);";

		conn.execute(create_datasets_table, params!()).await.map_err(|e| Error::DatabaseExecutionError(e.to_string()))?;
		conn.execute(create_measurements_table, params!()).await.map_err(|e| Error::DatabaseExecutionError(e.to_string()))?;
		conn.execute(create_index_measurments, params!()).await.map_err(|e| Error::DatabaseExecutionError(e.to_string()))?;
		conn.execute(create_index_measurments_timestamp, params!()).await.map_err(|e| Error::DatabaseExecutionError(e.to_string()))?;

		DATABASE_CONNECTIONS.lock().await.insert(db_name.to_string(), conn);
		Ok(())
	}

	/// Add dataset to the database.
	/// # Errors
	/// Returns an error if the database connection is not found, if the query fails, or if the dataset already exists.
	pub async fn add_dataset(&self, db_name: &str, dataset: Dataset) -> Result<Uuid, String> {
		let connections = DATABASE_CONNECTIONS.lock().await;
		let conn = connections.get(db_name).ok_or("Database connection not found")?;

		// Start transaction 
		conn.execute("BEGIN TRANSACTION;", params!()).await.map_err(|e| e.to_string())?;

		// Insert dataset
		let query = "INSERT INTO datasets (id, name) VALUES (?, ?);";
		if let Err(e) = conn.execute(query, [dataset.id.to_string(), dataset.name]).await {
			let _ = conn.execute("ROLLBACK;", params!()).await;
			return Err(e.to_string());
		}

		// Insert measurements using individual execute calls instead of prepared statement
		let mut inserted_count = 0;
		for (i, measurement) in dataset.measurments.iter().enumerate() {
			let insert_measurement = "INSERT INTO measurements (dataset_id, timestamp, value) VALUES (?, ?, ?);";
			match conn.execute(insert_measurement, [dataset.id.to_string(), measurement.timestamp.to_string(), measurement.value.to_string()]).await {
				Ok(_) => {
					inserted_count += 1;
				}
				Err(e) => {
					let _ = conn.execute("ROLLBACK;", params!()).await;
					return Err(format!("Failed to insert measurement #{}: {} (timestamp: {}, value: {})", i, e, measurement.timestamp, measurement.value));
				}
			}
		}

		// Commit transaction
		conn.execute("COMMIT;", params!()).await.map_err(|e| e.to_string())?;

		// Small delay to ensure data is persisted
		tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

		println!("DEBUG: Successfully inserted {}/{} measurements", inserted_count, dataset.measurments.len());

		Ok(dataset.id)
	}

	/// Get a dataset's id by its name.
	/// # Errors
	/// Returns an error if the database connection is not found, if the query fails, or if no dataset with the given name exists.
	pub async fn get_dataset_id_by_name(&self, db_name: &str, name: &str) -> Result<Uuid> {
		let query = "SELECT id FROM datasets WHERE name = ?;";
		let mut rows: Rows = DATABASE_CONNECTIONS.lock().await.get(db_name).ok_or_else(|| Error::ConnectingDatabaseError("Database connection not found".to_string()))?.query(query, [name]).await.map_err(|e| Error::DatabaseExecutionError(e.to_string()))?;
		let id: String = match rows.next().await.map_err(|e| Error::DatabaseExecutionError(e.to_string()))?.ok_or_else(|| Error::DatabaseExecutionError("No dataset found with the given name".to_string()))?.get_value(0).map_err(|e| Error::DatabaseExecutionError(e.to_string()))? {
			Value::Text(text) => text,
			_ => bail!(Error::DatabaseExecutionError("Unexpected value type for dataset ID".to_string())),
		};
		let id = Uuid::parse_str(&id).map_err(|e| Error::UuidParseError(e.to_string()))?;
		if id.is_nil() {
			bail!(Error::UuidParseError("Dataset ID is nil".to_string()));
		}
		Ok(id)
	}

	/// Add a measurement to a dataset.
	/// # Errors
	/// Returns an error if the database connection is not found, if the query fails, or if the dataset does not exist.
	pub async fn add_measurement(&self, db_name: &str, dataset_id: Uuid, measurement: Measurement) -> Result<()> {
		let query = "INSERT INTO measurements (dataset_id, timestamp, value) VALUES (?, ?, ?);";
		DATABASE_CONNECTIONS.lock().await.get(db_name).ok_or_else(|| Error::ConnectingDatabaseError("Database connection not found".to_string()))?.execute(query, [dataset_id.to_string(), measurement.timestamp.to_string(), measurement.value.to_string()]).await.map_err(|e| Error::DatabaseExecutionError(e.to_string()))?;
		Ok(())
	}

	/// Get all measurements for a given dataset ID.
	/// # Errors
	/// Returns an error if the database connection is not found, if the query fails, or if no measurements are found for the given dataset ID.
	pub async fn get_measurements_by_dataset_id(&self, db_name: &str, dataset_id: Uuid) -> Result<Vec<Measurement>> {
		let connections = DATABASE_CONNECTIONS.lock().await;
		let conn = connections.get(db_name).ok_or_else(|| Error::ConnectingDatabaseError("Database connection not found".to_string()))?;

		// First check count with explicit transaction
		let count_query = "SELECT COUNT(*) FROM measurements WHERE dataset_id = ?;";
		let mut count_rows = conn.query(count_query, [dataset_id.to_string()]).await.map_err(|e| Error::DatabaseExecutionError(e.to_string()))?;
		if let Some(row) = count_rows.next().await.map_err(|e| Error::DatabaseExecutionError(e.to_string()))? {
			let count = match row.get_value(0).map_err(|e| Error::DatabaseExecutionError(e.to_string()))? {
				Value::Integer(n) => n,
				Value::Real(n) => n as i64,
				_ => 0,
			};
			println!("Count query shows {} measurements for dataset {}", count, dataset_id);
		}

		// Use ORDER BY to ensure consistent retrieval
		let query = "SELECT timestamp, value FROM measurements WHERE dataset_id = ? ORDER BY timestamp;";
		let mut rows = conn.query(query, [dataset_id.to_string()]).await.map_err(|e| Error::DatabaseExecutionError(e.to_string()))?;
		let mut measurements = Vec::new();
		let mut row_count = 0;

		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseExecutionError(e.to_string()))? {
			row_count += 1;

			let timestamp: String = match row.get_value(0).map_err(|e| Error::DatabaseExecutionError(e.to_string()))? {
				Value::Text(text) => text,
				_ => bail!(Error::DatabaseExecutionError("Unexpected value type for timestamp".to_string())),
			};

			let value: String = match row.get_value(1).map_err(|e| Error::DatabaseExecutionError(e.to_string()))? {
				Value::Text(text) => text,
				_ => bail!(Error::DatabaseExecutionError("Unexpected value type for value".to_string())),
			};

			let measurement = Measurement { timestamp: timestamp.parse().map_err(|e: chrono::ParseError| Error::DatabaseExecutionError(e.to_string()))?, value: value.parse().map_err(|e: bigdecimal::ParseBigDecimalError| Error::DatabaseExecutionError(e.to_string()))? };
			measurements.push(measurement);

			// Debug: Print every 100th row to track progress
			if row_count % 100 == 0 {
				println!("Retrieved {} rows so far...", row_count);
			}
		}

		println!("Retrieved {} measurements from query (total rows processed: {})", measurements.len(), row_count);
		Ok(measurements)
	}
}

#[cfg(test)]
mod test {
	use fake::{Fake, Faker};

	use super::*;

	#[tokio::test(flavor = "multi_thread")]
	async fn test_benchmark_100_measurements() {
		unsafe {
			std::env::set_var("RUST_BACKTRACE", "full");
		}

		// Create fake data
		let mut dataset: Dataset = Faker.fake();
		// Generate measurements directly into the dataset
		dataset.measurments = (0..1000).map(|_| Faker.fake()).collect();

		// Ensure no duplicate timestamps by adding microseconds to each
		for (i, measurement) in dataset.measurments.iter_mut().enumerate() {
			measurement.timestamp = measurement.timestamp + chrono::Duration::microseconds(i as i64);
		}

		let measement_hashmap: HashMap<String, Measurement> = dataset.measurments.iter().map(|m| (m.timestamp.to_string(), m.clone())).collect();

		// Start benchmark
		println!("Starting benchmark with {} measurements for dataset: {}", dataset.measurments.len(), dataset.name);
		println!("Dataset ID: {}", dataset.id);
		println!("Number of measurements: {}", dataset.measurments.len());
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
                tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await; // Ensure the database has time to persist the data
		let timer2 = std::time::Instant::now();
		let added_measurements = db.get_measurements_by_dataset_id(db_name, dataset_id).await.expect("Failed to get measurements");
		let added_measurements_hashmap: HashMap<String, Measurement> = added_measurements.iter().map(|m| (m.timestamp.to_string(), m.clone())).collect();

		println!("Retrieved {} measurements, time elapsed: {:?}", added_measurements_hashmap.len(), timer2.elapsed());

		println!("Expected measurements: {}", measement_hashmap.len());
		println!("Actual measurements in DB: {}", added_measurements_hashmap.len());

		println!("Verifying the measurements in the database...");
		assert_eq!(measement_hashmap.len(), added_measurements_hashmap.len(), "Number of measurements in the database does not match the expected number");
		assert_eq!(measement_hashmap, added_measurements_hashmap);

		cleanup_test_database(db_name).await;
	}

	async fn cleanup_test_database(db_name: &str) {
		let db_path = format!("{DB_DIR}/{db_name}.db");
		if tokio::fs::remove_file(&db_path).await.is_err() {
			eprintln!("Failed to remove test database file: {db_path}");
		}
	}
}
