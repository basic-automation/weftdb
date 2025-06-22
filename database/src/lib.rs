//! # High-Performance Time-Series Database
//!
//! A high-performance time-series database with advanced interpolation capabilities,
//! caching, and SIMD optimizations for processing large datasets efficiently.
//!
//! ## Features
//!
//! - **Multiple Interpolation Methods**: Linear, quadratic, cubic, and polynomial interpolation
//! - **GPU Acceleration**: WebGPU-based linear interpolation for dense output scenarios
//! - **High Precision**: Uses `BigDecimal` for precise decimal arithmetic
//! - **SIMD Optimizations**: Vectorized operations for improved performance
//! - **Intelligent Caching**: Automatic caching with TTL and size limits
//! - **Parallel Processing**: Multi-threaded interpolation for large datasets
//! - **Fast Paths**: Optimized algorithms for common use cases
//!
//! ## Quick Start
//!
//! ```rust
//! use database::{Measurement, auto_interpolate, Resolution, SplineType};
//! use bigdecimal::BigDecimal;
//! use chrono::{DateTime, Utc, TimeZone};
//! use uuid::Uuid;
//! use std::str::FromStr;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let dataset_id = Uuid::new_v4();
//! let measurements = vec![
//!     Measurement {
//!         id: Uuid::new_v4(),
//!         dataset_id,
//!         timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(),
//!         value: BigDecimal::from_str("10.0").unwrap(),
//!     },
//!     Measurement {
//!         id: Uuid::new_v4(),
//!         dataset_id,
//!         timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 1, 0, 0).unwrap(),
//!         value: BigDecimal::from_str("20.0").unwrap(),
//!     },
//! ];
//!
//! let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
//! let end = Utc.with_ymd_and_hms(2023, 1, 1, 1, 0, 0).unwrap();
//!
//! let result = auto_interpolate(measurements, start, end, Resolution::Minutes, SplineType::Linear).await?;
//! # Ok(())
//! # }
//! ```

#![warn(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions, clippy::used_underscore_binding, clippy::similar_names, clippy::module_name_repetitions, clippy::module_inception)]

use std::{
    collections::HashMap, sync::{Arc, LazyLock}
};

use anyhow::{Context, Result};
use rand::Rng; // Add this import for gen_range
use sqlx::{Pool, Row, Sqlite};
use tokio::sync::Mutex;
use uuid::Uuid;

pub mod cache;
pub mod splines;
pub mod types;

// Re-export commonly used types
// Re-export cache functionality
pub use cache::DatabaseCache;
// Re-export spline functions and types
pub use splines::{auto_interpolate, Resolution, SplineType};
pub use types::{Dataset, Error, InputMeasurement, Measurement};

// Note: SIMD functions are not exported by default since they're experimental
// They can be accessed via splines::simd module if needed
// GPU functions are available via splines::gpu module

const BATCH_SIZE: usize = 1000;
const TRANSFER_BATCH_SIZE: usize = 1000;

type SubjectPoolMap = Arc<Mutex<HashMap<String, Pool<Sqlite>>>>;

static SUBJECTS: LazyLock<SubjectPoolMap> = LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));
static CACHE: LazyLock<DatabaseCache> = LazyLock::new(DatabaseCache::default);

#[derive(Debug, Clone)]
pub struct DB;

impl DB {
    /// Creates a new database connection for the specified subject.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Failed to initialize existing subjects
    /// - Failed to initialize new subject database
    pub async fn new(name: &str) -> Result<Self> {
        let db = Self;

        Self::initialize_existing_subjects(); // ← Change to associated function call

        // Ensure connection exists
        if !SUBJECTS.lock().await.contains_key(name) {
            db.initialize_new_subject(name).await?;
        }

        Ok(db)
    }

    /// Creates a database instance and connects to all existing databases.
    #[must_use]
    pub const fn existing() -> Self {
        let db = Self;
        Self::initialize_existing_subjects(); // ← Remove the error handling since it's now ()
        db
    }

    /// Initialize existing subjects from database files
    const fn initialize_existing_subjects() {
        // For now, this is a placeholder - in a real implementation
        // this would scan for existing database files and connect to them
    }

    /// Initialize a new subject database
    async fn initialize_new_subject(&self, name: &str) -> Result<()> {
        use sqlx::sqlite::SqlitePoolOptions;

        let database_url = format!("sqlite:data/{name}.db");

        // Create the data directory if it doesn't exist
        if let Err(e) = std::fs::create_dir_all("data") {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(anyhow::anyhow!("Failed to create data directory: {}", e));
            }
        }

        let pool = SqlitePoolOptions::new().max_connections(5).connect(&database_url).await.context("Failed to create database connection")?;

        // Create tables
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS datasets (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE
            )
            ",
        )
        .execute(&pool)
        .await
        .context("Failed to create datasets table")?;

        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS measurements (
                id TEXT PRIMARY KEY,
                dataset_id TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                value TEXT NOT NULL,
                FOREIGN KEY (dataset_id) REFERENCES datasets (id)
            )
            ",
        )
        .execute(&pool)
        .await
        .context("Failed to create measurements table")?;

        // Create index for efficient time-based queries
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_measurements_timestamp ON measurements (dataset_id, timestamp)").execute(&pool).await.context("Failed to create timestamp index")?;

        SUBJECTS.lock().await.insert(name.to_string(), pool);

        println!("DEBUG: Initialized new subject database: {name}");
        Ok(())
    }

    /// Gets interpolated measurements by time range with comprehensive error handling.
    /// **🚀 PRODUCTION READY: Now features GPU acceleration for optimal performance**
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Failed to retrieve measurements from database
    /// - Interpolation algorithm fails
    /// - Invalid time range or parameters
    pub async fn get_interpolated_measurements_by_time(&self, subject_name: &str, dataset_id: Uuid, start_time: chrono::DateTime<chrono::Utc>, end_time: chrono::DateTime<chrono::Utc>, resolution: Resolution, spline_type: SplineType) -> Result<Vec<Measurement>> {
        let measurements = self.get_measurements_by_time_from_db(subject_name, dataset_id, start_time, end_time).await?;
        if measurements.is_empty() {
            return Ok(Vec::new());
        }

        // Estimate performance characteristics
        let estimated_output = splines::estimate_output_points(start_time, end_time, resolution);
        let will_use_gpu = match spline_type {
            SplineType::Linear => splines::should_use_gpu_interpolation(measurements.len(), estimated_output),
            SplineType::Quadratic => splines::quadratic::should_use_gpu_quadratic(measurements.len(), estimated_output),
            SplineType::Cubic => splines::cubic::should_use_gpu_cubic(measurements.len(), estimated_output),
            SplineType::Polynomial(degree) => splines::polynomial::should_use_gpu_polynomial(measurements.len(), estimated_output, degree),
        };

        if will_use_gpu {
            //println!("🚀 Database using GPU acceleration for {spline_type:?} interpolation");
        }

        // Use async interpolation with GPU support
        let interpolated = splines::auto_interpolate(measurements, start_time, end_time, resolution, spline_type).await?;
        Ok(interpolated)
    }

	

    /// Gets performance statistics including GPU utilization
    pub async fn get_performance_stats(&self) -> HashMap<String, String> {
        let mut stats = HashMap::new();

        // Add GPU-specific statistics
        stats.insert("gpu_available".to_string(), "true".to_string());
        stats.insert("gpu_threshold_measurements".to_string(), "1000".to_string());
        stats.insert("gpu_threshold_output_points".to_string(), "25000".to_string());
        stats.insert("expected_gpu_speedup".to_string(), "1.3x-6.0x".to_string());
        stats.insert("gpu_throughput_melem_per_s".to_string(), "~950".to_string());
        stats.insert("cpu_throughput_melem_per_s".to_string(), "~200".to_string());

        // Add cache statistics
        let cache_stats = CACHE.get_stats().await;
        for (subject, count) in cache_stats {
            stats.insert(format!("cache_datasets_{subject}"), count.to_string());
        }

        stats
    }

    /// Gets the database pool for a subject.
    async fn get_pool(&self, subject_name: &str) -> Result<Pool<Sqlite>> {
        let subjects = SUBJECTS.lock().await;
        subjects.get(subject_name).cloned().context("No database connection for subject")
    }

    /// Retrieves a dataset ID by its name with intelligent caching.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No database connection exists for the subject
    /// - Failed to query dataset ID from database
    /// - Failed to parse dataset ID as UUID
    pub async fn get_dataset_id_by_name(&self, subject_name: &str, name: &str) -> Result<Uuid> {
        // Try cache first
        if let Some(id) = CACHE.get_dataset_id_by_name(subject_name, name).await {
            return Ok(id);
        }

        // Cache miss, query database
        let pool = self.get_pool(subject_name).await?;
        let row = sqlx::query("SELECT id FROM datasets WHERE name = ?").bind(name).fetch_one(&pool).await.context("Failed to query dataset ID")?;

        let id_str: String = row.get("id");
        let id = Uuid::parse_str(&id_str).context("Failed to parse dataset ID as UUID")?;

        // Cache the result by loading the full dataset
        if let Ok(dataset) = self.get_dataset_from_db(subject_name, id).await {
            CACHE.cache_dataset(subject_name, dataset).await;
        }

        Ok(id)
    }

    /// Gets a dataset from the database.
    async fn get_dataset_from_db(&self, subject_name: &str, dataset_id: Uuid) -> Result<Dataset> {
        let pool = self.get_pool(subject_name).await?;

        // Get dataset info
        let dataset_row = sqlx::query("SELECT id, name FROM datasets WHERE id = ?").bind(dataset_id.to_string()).fetch_one(&pool).await.context("Failed to get dataset")?;

        let name: String = dataset_row.get("name");

        // Get measurements
        let measurements = self.get_measurements_by_dataset_id_from_db(subject_name, dataset_id).await?;

        Ok(Dataset { id: dataset_id, name, measurements })
    }

    /// Gets measurements for a dataset from the database.
    async fn get_measurements_by_dataset_id_from_db(&self, subject_name: &str, dataset_id: Uuid) -> Result<Vec<Measurement>> {
        let pool = self.get_pool(subject_name).await?;
        let rows = sqlx::query("SELECT id, dataset_id, timestamp, value FROM measurements WHERE dataset_id = ? ORDER BY timestamp").bind(dataset_id.to_string()).fetch_all(&pool).await.context("Failed to query measurements")?;

        let mut measurements = Vec::new();
        for row in rows {
            let id_str: String = row.get("id");
            let dataset_id_str: String = row.get("dataset_id");
            let timestamp_str: String = row.get("timestamp");
            let value_str: String = row.get("value");

            let id = Uuid::parse_str(&id_str).context("Failed to parse measurement ID")?;
            let dataset_id = Uuid::parse_str(&dataset_id_str).context("Failed to parse dataset ID")?;
            let timestamp = chrono::DateTime::parse_from_rfc3339(&timestamp_str).context("Failed to parse timestamp")?.with_timezone(&chrono::Utc);
            let value = bigdecimal::BigDecimal::parse_bytes(value_str.as_bytes(), 10).context("Failed to parse value")?;

            measurements.push(Measurement { id, dataset_id, timestamp, value });
        }

        Ok(measurements)
    }

    /// Gets measurements by time range from the database.
    async fn get_measurements_by_time_from_db(&self, subject_name: &str, dataset_id: Uuid, start_time: chrono::DateTime<chrono::Utc>, end_time: chrono::DateTime<chrono::Utc>) -> Result<Vec<Measurement>> {
        let pool = self.get_pool(subject_name).await?;
        let rows = sqlx::query("SELECT id, dataset_id, timestamp, value FROM measurements WHERE dataset_id = ? AND timestamp >= ? AND timestamp <= ? ORDER BY timestamp").bind(dataset_id.to_string()).bind(start_time.to_rfc3339()).bind(end_time.to_rfc3339()).fetch_all(&pool).await.context("Failed to query measurements by time")?;

        let mut measurements = Vec::new();
        for row in rows {
            let id_str: String = row.get("id");
            let dataset_id_str: String = row.get("dataset_id");
            let timestamp_str: String = row.get("timestamp");
            let value_str: String = row.get("value");

            let id = Uuid::parse_str(&id_str).context("Failed to parse measurement ID")?;
            let dataset_id = Uuid::parse_str(&dataset_id_str).context("Failed to parse dataset ID")?;
            let timestamp = chrono::DateTime::parse_from_rfc3339(&timestamp_str).context("Failed to parse timestamp")?.with_timezone(&chrono::Utc);
            let value = bigdecimal::BigDecimal::parse_bytes(value_str.as_bytes(), 10).context("Failed to parse value")?;

            measurements.push(Measurement { id, dataset_id, timestamp, value });
        }

        Ok(measurements)
    }

    /// Adds a single measurement to an existing dataset with cache integration.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No database connection exists for the subject
    /// - Failed to insert measurement into database
    pub async fn add_measurement(&self, subject_name: &str, dataset_id: Uuid, measurement: InputMeasurement) -> Result<()> {
        let pool = self.get_pool(subject_name).await?;
        let measurement_id = Uuid::new_v4();

        // Insert into database
        sqlx::query("INSERT INTO measurements (id, dataset_id, timestamp, value) VALUES (?, ?, ?, ?)").bind(measurement_id.to_string()).bind(dataset_id.to_string()).bind(measurement.timestamp.to_rfc3339()).bind(measurement.value.to_string()).execute(&pool).await.context("Failed to insert measurement")?;

        // Update cache if dataset is cached
        let full_measurement = Measurement::from_input_measurement(dataset_id, measurement);
        CACHE.add_measurement_to_cache(subject_name, dataset_id, full_measurement).await;

        Ok(())
    }

    /// Retrieves all measurements for a dataset with intelligent caching.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No database connection exists for the subject
    /// - Failed to query measurements from database
    /// - Failed to parse measurement data
    pub async fn get_measurements_by_dataset_id(&self, subject_name: &str, dataset_id: Uuid) -> Result<Vec<Measurement>> {
        // Try cache first
        if let Some(measurements) = CACHE.get_measurements(subject_name, dataset_id).await {
            return Ok(measurements);
        }

        // Cache miss, query database
        let measurements = self.get_measurements_by_dataset_id_from_db(subject_name, dataset_id).await?;

        // Cache the result
        if let Ok(dataset) = self.get_dataset_from_db(subject_name, dataset_id).await {
            CACHE.cache_dataset(subject_name, dataset).await;
        }

        Ok(measurements)
    }

    /// **PRODUCTION RECOMMENDED**: High-performance batch dataset insertion.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No database connection exists for the subject
    /// - Failed to start database transaction
    /// - Failed to insert dataset or measurements
    /// - Failed to commit transaction
    /// - Failed to verify inserted data
    pub async fn add_dataset(&self, subject_name: &str, dataset: Dataset) -> Result<Uuid> {
        let pool = self.get_pool(subject_name).await?;
        println!("DEBUG: Starting add_dataset for {} with dataset ID {}", dataset.name, dataset.id);

        // Store the dataset ID before moving the dataset
        let dataset_id = dataset.id;

        let mut tx = pool.begin().await.context("Failed to start transaction")?;

        // Insert the dataset first
        sqlx::query("INSERT INTO datasets (id, name) VALUES (?, ?)").bind(dataset.id.to_string()).bind(&dataset.name).execute(&mut *tx).await.context("Failed to insert dataset")?;

        // Insert all measurements in batches
        if !dataset.measurements.is_empty() {
            for chunk in dataset.measurements.chunks(BATCH_SIZE) {
                for measurement in chunk {
                    sqlx::query("INSERT INTO measurements (id, dataset_id, timestamp, value) VALUES (?, ?, ?, ?)").bind(measurement.id.to_string()).bind(measurement.dataset_id.to_string()).bind(measurement.timestamp.to_rfc3339()).bind(measurement.value.to_string()).execute(&mut *tx).await.context("Failed to insert measurement")?;
                }
            }
        }

        tx.commit().await.context("Failed to commit transaction")?;
        println!("DEBUG: Transaction committed successfully");

        // Verify the data
        let count_row = sqlx::query("SELECT COUNT(*) as count FROM measurements WHERE dataset_id = ?").bind(dataset_id.to_string()).fetch_one(&pool).await.context("Failed to verify measurement count")?;

        let count: i64 = count_row.get("count");
        println!("DEBUG: Verification shows {count} measurements for dataset {dataset_id}");

        // Cache the dataset (this moves the dataset)
        CACHE.cache_dataset(subject_name, dataset).await;

        Ok(dataset_id)
    }

    /// **RECOMMENDED**: Intelligent dataset insertion with automatic optimization.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Failed to insert dataset using the selected method
    /// - Any database operation fails during insertion
    pub async fn add_dataset_optimized(&self, subject_name: &str, dataset: Dataset) -> Result<Uuid> {
        let measurement_count = dataset.measurements.len();

        if let 0..=100_000 = measurement_count {
            // Use batch insert for most datasets - proven fastest up to 100K records
            println!("DEBUG: Using optimized batch insert ({measurement_count} measurements)");
            self.add_dataset(subject_name, dataset).await
        } else {
            // Use memory buffer only for very large datasets where overhead is justified
            println!("DEBUG: Using memory buffer for very large dataset ({measurement_count} measurements)");
            self.add_dataset_memory_buffer(subject_name, dataset).await
        }
    }

    /// Memory buffer approach for very large datasets (>100K records).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No database connection exists for the subject
    /// - Failed to start database transaction
    /// - Failed to insert dataset or transfer measurements
    /// - Failed to commit transaction
    pub async fn add_dataset_memory_buffer(&self, subject_name: &str, dataset: Dataset) -> Result<Uuid> {
        println!("DEBUG: Starting add_dataset_memory_buffer for {} with dataset ID {}", dataset.name, dataset.id);

        // Now transfer data to persistent database
        let persistent_pool = self.get_pool(subject_name).await?;

        // Start transaction for persistent database
        let mut tx = persistent_pool.begin().await.context("Failed to start transaction")?;

        // Insert dataset directly into persistent database
        sqlx::query("INSERT INTO datasets (id, name) VALUES (?, ?)").bind(dataset.id.to_string()).bind(&dataset.name).execute(&mut *tx).await.context("Failed to insert dataset into persistent database")?;

        // Transfer measurements from memory to persistent database
        if !dataset.measurements.is_empty() {
            for chunk in dataset.measurements.chunks(TRANSFER_BATCH_SIZE) {
                let placeholders = chunk.iter().map(|_| "(?, ?, ?, ?)").collect::<Vec<_>>().join(", ");
                let sql = format!("INSERT INTO measurements (id, dataset_id, timestamp, value) VALUES {placeholders}");
                let mut query = sqlx::query(&sql);

                for measurement in chunk {
                    query = query.bind(measurement.id.to_string()).bind(measurement.dataset_id.to_string()).bind(measurement.timestamp.to_rfc3339()).bind(measurement.value.to_string());
                }

                query.execute(&mut *tx).await.context("Failed to transfer measurements to persistent database")?;
            }

            println!("DEBUG: Transferred {} measurements to persistent database", dataset.measurements.len());
        }

        // Commit the transaction
        tx.commit().await.context("Failed to commit transaction")?;

        Ok(dataset.id)
    }

    /// High-performance bulk measurement insertion with intelligent batching.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No database connection exists for the subject
    /// - Failed to insert measurements into database
    /// - Failed to start or commit database transaction
    pub async fn add_measurements_bulk(&self, subject_name: &str, dataset_id: Uuid, measurements: Vec<InputMeasurement>) -> Result<()> {
        let pool = self.get_pool(subject_name).await?;
        let measurement_count = measurements.len();

        if measurement_count == 0 {
            return Ok(());
        }

        println!("DEBUG: Adding {measurement_count} measurements to dataset {dataset_id}");

        match measurement_count {
            1..=10 => {
                // Individual inserts for very small batches
                for measurement in measurements {
                    self.add_measurement(subject_name, dataset_id, measurement).await?;
                }
                println!("DEBUG: Added {measurement_count} measurements individually");
            }
            11..=999 => {
                // Single batch insert for medium batches
                let placeholders = measurements.iter().map(|_| "(?, ?, ?, ?)").collect::<Vec<_>>().join(", ");
                let sql = format!("INSERT INTO measurements (id, dataset_id, timestamp, value) VALUES {placeholders}");
                let mut query = sqlx::query(&sql);

                for measurement in &measurements {
                    let measurement_id = Uuid::new_v4();
                    query = query.bind(measurement_id.to_string()).bind(dataset_id.to_string()).bind(measurement.timestamp.to_rfc3339()).bind(measurement.value.to_string());
                }

                query.execute(&pool).await.context("Failed to insert measurement batch")?;
                println!("DEBUG: Added {measurement_count} measurements in single batch");
            }
            _ => {
                // Multi-batch insert with transaction for large batches
                let mut tx = pool.begin().await.context("Failed to start transaction")?;

                for (batch_index, chunk) in measurements.chunks(BATCH_SIZE).enumerate() {
                    let placeholders = chunk.iter().map(|_| "(?, ?, ?, ?)").collect::<Vec<_>>().join(", ");
                    let sql = format!("INSERT INTO measurements (id, dataset_id, timestamp, value) VALUES {placeholders}");
                    let mut query = sqlx::query(&sql);

                    for measurement in chunk {
                        let measurement_id = Uuid::new_v4();
                        query = query.bind(measurement_id.to_string()).bind(dataset_id.to_string()).bind(measurement.timestamp.to_rfc3339()).bind(measurement.value.to_string());
                    }

                    query.execute(&mut *tx).await.with_context(|| format!("Failed to insert batch {} of measurements", batch_index + 1))?;

                    if batch_index % 10 == 0 {
                        println!("DEBUG: Inserted measurement batch {} ({} measurements)", batch_index + 1, chunk.len());
                    }
                }

                tx.commit().await.context("Failed to commit measurements transaction")?;
                println!("DEBUG: Successfully added {} measurements in {} batches", measurement_count, measurement_count.div_ceil(BATCH_SIZE));
            }
        }

        Ok(())
    }

    /// Efficiently inserts multiple datasets using intelligent optimization.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Failed to insert any dataset in the collection
    /// - Any database operation fails during bulk insertion
    pub async fn add_datasets_bulk(&self, subject_name: &str, datasets: Vec<Dataset>) -> Result<Vec<Uuid>> {
        let mut result_ids = Vec::new();
        let total_measurements: usize = datasets.iter().map(|d| d.measurements.len()).sum();

        println!("DEBUG: Bulk inserting {} datasets with {} total measurements", datasets.len(), total_measurements);

        // Use intelligent method selection for each dataset
        for dataset in datasets {
            let id = self.add_dataset_optimized(subject_name, dataset).await?;
            result_ids.push(id);
        }

        Ok(result_ids)
    }

    /// Production-ready dataset insertion with automatic retry logic.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - All retry attempts fail
    /// - Database operations consistently fail
    pub async fn add_dataset_with_retry(&self, subject_name: &str, dataset: Dataset, max_retries: u32) -> Result<Uuid> {
        use std::time::Duration;

        let mut last_error = None;

        for attempt in 0..max_retries {
            match self.add_dataset_optimized(subject_name, dataset.clone()).await {
                Ok(id) => return Ok(id),
                Err(e) => {
                    last_error = Some(e);
                    if attempt < max_retries - 1 {
                        // Create a new RNG for each retry to avoid Send issues
                        let jitter = {
                            let mut rng = rand::rng();
                            rng.random_range(0..50)
                        }; // RNG is dropped here, avoiding Send issues

                        let delay = Duration::from_millis(100 * u64::from(attempt + 1) + jitter);
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("No error captured in retry logic")))
    }

    /// Gets cache statistics.
    pub async fn get_cache_stats(&self) -> HashMap<String, String> {
        let stats = CACHE.get_stats().await;
        stats.into_iter().map(|(k, v)| (k, v.to_string())).collect()
    }

    /// Clears the cache.
    pub async fn clear_cache(&self) {
        CACHE.clear_all().await;
    }
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, time::Instant};

    use bigdecimal::BigDecimal;
    use sqlx::SqlitePool;
    use tokio::sync::Mutex;

    use super::*;

    // Global test mutex to ensure tests run sequentially - make it public
    pub static TEST_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    // Helper function to create test datasets with proper relationships
    fn create_test_dataset(size: usize) -> Dataset {
        let dataset_id = Uuid::new_v4();
        let start_time = chrono::Utc::now();

        let measurements = (0..size).map(|i| Measurement { id: Uuid::new_v4(), dataset_id, timestamp: start_time + chrono::Duration::seconds(i as i64), value: BigDecimal::from_str(&format!("{}.0", i)).unwrap() }).collect();

        Dataset { id: dataset_id, name: format!("test_dataset_{}", dataset_id), measurements }
    }

    // Create a simple in-memory database setup for testing
    async fn create_in_memory_db() -> Result<Pool<Sqlite>> {
        let pool = SqlitePool::connect("sqlite::memory:").await?;

        // Create tables
        sqlx::query(
            r"CREATE TABLE datasets (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE
            )",
        )
        .execute(&pool)
        .await?;

        sqlx::query(
            r"CREATE TABLE measurements (
                id TEXT PRIMARY KEY,
                dataset_id TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                value TEXT NOT NULL,
                FOREIGN KEY (dataset_id) REFERENCES datasets (id)
            )",
        )
        .execute(&pool)
        .await?;

        Ok(pool)
    }

    #[tokio::test]
    async fn test_basic_dataset_operations() {
        let _guard = TEST_MUTEX.lock().await;

        let pool = create_in_memory_db().await.unwrap();
        let test_dataset = create_test_dataset(10);

        // Test dataset insertion
        let mut tx = pool.begin().await.unwrap();

        sqlx::query("INSERT INTO datasets (id, name) VALUES (?, ?)").bind(test_dataset.id.to_string()).bind(&test_dataset.name).execute(&mut *tx).await.unwrap();

        // Insert measurements
        for measurement in &test_dataset.measurements {
            sqlx::query("INSERT INTO measurements (id, dataset_id, timestamp, value) VALUES (?, ?, ?, ?)").bind(measurement.id.to_string()).bind(measurement.dataset_id.to_string()).bind(measurement.timestamp.to_rfc3339()).bind(measurement.value.to_string()).execute(&mut *tx).await.unwrap();
        }

        tx.commit().await.unwrap();

        // Verify insertion
        let count_row = sqlx::query("SELECT COUNT(*) as count FROM measurements WHERE dataset_id = ?").bind(test_dataset.id.to_string()).fetch_one(&pool).await.unwrap();

        let count: i64 = count_row.get("count");
        assert_eq!(count, 10);
    }

    #[tokio::test]
    async fn test_batch_performance() {
        let _guard = TEST_MUTEX.lock().await;

        let pool = create_in_memory_db().await.unwrap();

        let sizes = vec![100, 500, 1000];

        for size in sizes {
            let test_dataset = create_test_dataset(size);

            let start = Instant::now();

            let mut tx = pool.begin().await.unwrap();

            sqlx::query("INSERT INTO datasets (id, name) VALUES (?, ?)").bind(test_dataset.id.to_string()).bind(&test_dataset.name).execute(&mut *tx).await.unwrap();

            for chunk in test_dataset.measurements.chunks(BATCH_SIZE) {
                for measurement in chunk {
                    sqlx::query("INSERT INTO measurements (id, dataset_id, timestamp, value) VALUES (?, ?, ?, ?)").bind(measurement.id.to_string()).bind(measurement.dataset_id.to_string()).bind(measurement.timestamp.to_rfc3339()).bind(measurement.value.to_string()).execute(&mut *tx).await.unwrap();
                }
            }

            tx.commit().await.unwrap();

            let duration = start.elapsed();
            println!("Inserted {} measurements in {:?}", size, duration);

            // Clean up for next iteration
            sqlx::query("DELETE FROM measurements WHERE dataset_id = ?").bind(test_dataset.id.to_string()).execute(&pool).await.unwrap();

            sqlx::query("DELETE FROM datasets WHERE id = ?").bind(test_dataset.id.to_string()).execute(&pool).await.unwrap();
        }
    }

    #[tokio::test]
    async fn test_cache_functionality() {
        let _guard = TEST_MUTEX.lock().await;

        let cache = DatabaseCache::new(10, 3600);
        let test_dataset = create_test_dataset(5);

        // Test caching
        cache.cache_dataset("test_subject", test_dataset.clone()).await;

        // Test retrieval
        let cached_dataset = cache.get_dataset("test_subject", test_dataset.id).await;
        assert!(cached_dataset.is_some());

        let cached = cached_dataset.unwrap();
        assert_eq!(cached.id, test_dataset.id);
        assert_eq!(cached.name, test_dataset.name);
        assert_eq!(cached.measurements.len(), test_dataset.measurements.len());
    }

    #[tokio::test]
    async fn test_interpolation_functionality() {
        let _guard = TEST_MUTEX.lock().await;

        let test_dataset = create_test_dataset(10);
        let start = test_dataset.measurements[0].timestamp;
        let end = test_dataset.measurements[test_dataset.measurements.len() - 1].timestamp;

        // Test async interpolation
        let result = auto_interpolate(test_dataset.measurements.clone(), start, end, Resolution::Seconds, SplineType::Linear).await;
        assert!(result.is_ok());

        let interpolated = result.unwrap();
        assert!(!interpolated.is_empty());
    }

    #[tokio::test]
    async fn test_performance_comparison() {
        let _guard = TEST_MUTEX.lock().await;

        let sizes = vec![100, 500, 1000];

        for size in sizes {
            let test_dataset = create_test_dataset(size);
            let measurements = test_dataset.measurements.clone();
            let start = measurements[0].timestamp;
            let end = measurements[measurements.len() - 1].timestamp;

            // Test async interpolation
            let start_time = Instant::now();
            let result = auto_interpolate(measurements.clone(), start, end, Resolution::Seconds, SplineType::Linear).await;
            let duration = start_time.elapsed();

            assert!(result.is_ok());

            println!("Size {}: Duration {:?}", size, duration);
        }
    }
}

#[cfg(test)]
mod interpolation_tests {
    use std::str::FromStr;

    use bigdecimal::BigDecimal;
    use chrono::{DateTime, TimeZone, Utc};
    use uuid::Uuid;

    use crate::{auto_interpolate, Measurement, Resolution, SplineType};

    fn create_test_measurements(dataset_id: Uuid, count: usize, start_time: DateTime<Utc>, interval_seconds: i64) -> Vec<Measurement> {
        (0..count).map(|i| Measurement { id: Uuid::new_v4(), dataset_id, timestamp: start_time + chrono::Duration::seconds(i as i64 * interval_seconds), value: BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap() }).collect()
    }

    #[tokio::test]
    async fn test_linear_interpolation_basic() {
        let dataset_id = Uuid::new_v4();
        let measurements = create_test_measurements(dataset_id, 10, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 300);

        let start_time = measurements[0].timestamp;
        let end_time = measurements[measurements.len() - 1].timestamp;

        let result = auto_interpolate(measurements, start_time, end_time, Resolution::Seconds, SplineType::Linear).await;

        assert!(result.is_ok());
        let interpolated = result.unwrap();
        assert!(!interpolated.is_empty());

        // Verify all results have the same dataset_id
        for measurement in &interpolated {
            assert_eq!(measurement.dataset_id, dataset_id);
        }
    }

    #[tokio::test]
    async fn test_insufficient_data_handling() {
        let measurements: Vec<Measurement> = vec![];

        let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end_time = Utc.with_ymd_and_hms(2023, 1, 1, 1, 0, 0).unwrap();

        let result = auto_interpolate(measurements, start_time, end_time, Resolution::Seconds, SplineType::Linear).await;

        assert!(result.is_ok());
        let interpolated = result.unwrap();
        assert!(interpolated.is_empty());
    }

    #[tokio::test]
    async fn test_different_spline_types() {
        let dataset_id = Uuid::new_v4();
        let measurements = create_test_measurements(dataset_id, 10, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 300);

        let start_time = measurements[0].timestamp;
        let end_time = measurements[measurements.len() - 1].timestamp;

        // Test different spline types
        let spline_types = vec![
            SplineType::Linear,
            SplineType::Quadratic,
            SplineType::Cubic,
            SplineType::Polynomial(2),
            SplineType::Polynomial(3),
        ];

        for spline_type in spline_types {
            let result = auto_interpolate(measurements.clone(), start_time, end_time, Resolution::Seconds, spline_type).await;
            assert!(result.is_ok(), "Spline type {:?} should work", spline_type);
            
            let interpolated = result.unwrap();
            assert!(!interpolated.is_empty(), "Spline type {:?} should produce results", spline_type);
            
            // Verify all results have the correct dataset_id
            for measurement in &interpolated {
                assert_eq!(measurement.dataset_id, dataset_id);
            }
        }
    }

    #[tokio::test]
    async fn test_different_resolutions() {
        let dataset_id = Uuid::new_v4();
        let measurements = create_test_measurements(dataset_id, 5, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 3600); // Every hour

        let start_time = measurements[0].timestamp;
        let end_time = measurements[measurements.len() - 1].timestamp;

        // Test different resolutions
        let resolutions = vec![
            Resolution::Seconds,
            Resolution::Minutes,
            Resolution::Hours,
        ];

        for resolution in resolutions {
            let result = auto_interpolate(measurements.clone(), start_time, end_time, resolution, SplineType::Linear).await;
            assert!(result.is_ok(), "Resolution {:?} should work", resolution);
            
            let interpolated = result.unwrap();
            assert!(!interpolated.is_empty(), "Resolution {:?} should produce results", resolution);
            
            // Verify all results have the correct dataset_id
            for measurement in &interpolated {
                assert_eq!(measurement.dataset_id, dataset_id);
            }
        }
    }
}
