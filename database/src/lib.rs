//! # High-Performance Time-Series Database
//!
//! A high-performance time-series database with advanced interpolation capabilities,
//! optimized for real-time sensor data processing and analysis.
//!
//! ## Quick Start
//!
//! ```rust
//! use database::*;
//! use bigdecimal::BigDecimal;
//! use std::str::FromStr;
//! use chrono::TimeZone;
//!
//! # #[tokio::main]
//! # async fn main() -> anyhow::Result<()> {
//! # // Clean up any existing test data first
//! # std::fs::remove_dir_all("data/my_experiment").ok();
//! #
//! // Create a new database
//! let db_id = new("my_experiment").await?;
//!
//! // Add a subject
//! let subject_id = add_subject(db_id, "participant_001").await?;
//!
//! // Track an aspect (e.g., heart rate)
//! let aspect_id = track_aspect(subject_id, "heart_rate").await?;
//!
//! // Capture multiple measurements for interpolation
//! let measurements = vec![
//!     InputMeasurement::new(
//!         chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(),
//!         BigDecimal::from_str("70.0").unwrap()
//!     ),
//!     InputMeasurement::new(
//!         chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 1, 0).unwrap(),
//!         BigDecimal::from_str("72.5").unwrap()
//!     ),
//!     InputMeasurement::new(
//!         chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 2, 0).unwrap(),
//!         BigDecimal::from_str("75.0").unwrap()
//!     ),
//! ];
//!
//! for measurement in measurements {
//!     capture_measurement(aspect_id, measurement).await?;
//! }
//!
//! // Analyze data point (interpolate between existing measurements)
//! let time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 1, 30).unwrap();
//! let data_point = analyze_point(aspect_id, time, Resolution::Seconds, Spline::Linear).await?;
//!
//! println!("Interpolated value: {}", data_point.value);
//! # std::fs::remove_dir_all("data/my_experiment").ok();
//! # Ok(())
//! # }
//! ```

#![warn(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions, clippy::used_underscore_binding, clippy::similar_names, clippy::module_name_repetitions, clippy::module_inception)]

use std::{
	collections::HashMap, path::Path, str::FromStr, sync::{Arc, LazyLock}
};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
pub use splimes::{Point, Resolution, Spline};
use sqlx::{Pool, Row, Sqlite};
use tokio::sync::Mutex;
use uuid::Uuid;

pub mod cache;
pub mod types;

// Re-export commonly used types
pub use cache::DatabaseCache;
pub use types::{DatabaseId, Dataset, Error, InputMeasurement, Measurement, SubjectId};

// New ID types for the simplified API
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AspectId(Uuid);

impl AspectId {
	#[must_use]
	pub fn new() -> Self {
		Self(Uuid::new_v4())
	}
}

impl Default for AspectId {
	fn default() -> Self {
		Self::new()
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TxId(Uuid);

#[derive(Debug, Clone)]
pub struct DataPoint {
	pub timestamp: DateTime<Utc>,
	pub value: bigdecimal::BigDecimal,
}

#[allow(dead_code)]
const BATCH_SIZE: usize = 1000;
pub const DEFAULT_DATA_DIR: &str = "data";

// Internal storage structures
#[derive(Debug, Clone)]
struct DatabaseInfo {
	name: String,
	path: String,
	subjects: HashMap<SubjectId, SubjectInfo>,
}

#[derive(Debug, Clone)]
struct SubjectInfo {
	name: String,
	pool: Pool<Sqlite>,
	aspects: HashMap<AspectId, AspectInfo>,
}

#[derive(Debug, Clone)]
struct AspectInfo {
	name: String,
	#[allow(dead_code)]
	subject_id: SubjectId,
	table_name: String,
}

type DatabaseMap = Arc<Mutex<HashMap<DatabaseId, DatabaseInfo>>>;
static DATABASES: LazyLock<DatabaseMap> = LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));
static CACHE: LazyLock<DatabaseCache> = LazyLock::new(DatabaseCache::default);

/// Creates a new database instance. Creates folder /{`data_dir}/{name`}.
/// Keeps count of instances of `DB::existing({name`}) and manages read / write access as necessary.
///
/// # Errors
/// - if folder /data/{name} already exists.
pub async fn new(name: &str) -> Result<DatabaseId> {
	let data_dir = DEFAULT_DATA_DIR;
	let db_path = format!("{data_dir}/{name}");

	// Check if folder already exists
	if Path::new(&db_path).exists() {
		bail!("Database directory '{}' already exists", db_path);
	}

	// Create the directory
	std::fs::create_dir_all(&db_path).with_context(|| format!("Failed to create database directory '{db_path}'"))?;

	let db_id = DatabaseId(Uuid::new_v4());
	let db_info = DatabaseInfo { name: name.to_string(), path: db_path, subjects: HashMap::new() };

	DATABASES.lock().await.insert(db_id, db_info);

	Ok(db_id)
}

/// Loads an instance of DB from /{`data_dir}/{name`}.
/// Maps Subjects and loads cache.
/// Keeps count of instances of `DB::existing({name`}) and manages read / write access as necessary.
///
/// # Errors
/// - if folder /data/{name} does not exist
/// - if database connection fails
/// - if unable to query existing tables
pub async fn existing(name: &str) -> Result<DatabaseId> {
	let data_dir = DEFAULT_DATA_DIR;
	let db_path = format!("{data_dir}/{name}");

	// Check if folder exists
	if !Path::new(&db_path).exists() {
		bail!("Database directory '{}' does not exist", db_path);
	}

	let db_id = DatabaseId::new();
	let mut db_info = DatabaseInfo { name: name.to_string(), path: db_path.clone(), subjects: HashMap::new() };

	// Load existing subjects
	if let Ok(entries) = std::fs::read_dir(&db_path) {
		for entry in entries.flatten() {
			if let Some(file_name) = entry.file_name().to_str() {
				// Use case-insensitive extension check
				if std::path::Path::new(file_name).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("db")) {
					let subject_name = file_name.trim_end_matches(".db");
					// Create subject connection
					let subject_db_path = format!("{db_path}/{file_name}");
					let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(5).connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&subject_db_path).create_if_missing(false)).await.with_context(|| format!("Failed to connect to existing subject database for '{subject_name}'"))?;

					let subject_id = SubjectId::new();
					let subject_info = SubjectInfo { name: subject_name.to_string(), pool, aspects: HashMap::new() };
					db_info.subjects.insert(subject_id, subject_info);
				}
			}
		}
	}

	DATABASES.lock().await.insert(db_id, db_info);

	Ok(db_id)
}

/// Lists all databases in the system.
/// Returns a map of `DatabaseId` to database name.
/// # Errors
/// - if unable to read database directory
/// - if database directory is not set
///
pub async fn ls() -> Result<HashMap<DatabaseId, String>> {
	let mut result = HashMap::new();
	for (db_id, db_info) in DATABASES.lock().await.iter() {
		result.insert(*db_id, db_info.name.clone());
	}
	Ok(result)
}

/// Lists all subjects in a database.
/// # Errors
/// - if database not found
/// - if unable to read database subjects
///
pub async fn ls_subjects(db_id: DatabaseId) -> Result<HashMap<SubjectId, String>> {
	let mut result = HashMap::new();
	let db_info = DATABASES.lock().await.get(&db_id).context("Database not found")?.clone();
	for (subject_id, subject_info) in &db_info.subjects {
		result.insert(*subject_id, subject_info.name.clone());
	}
	Ok(result)
}

/// Lists all aspects of a subject.
/// # Errors
/// - if database not found
/// - if subject not found
///
pub async fn ls_aspects(db_id: DatabaseId, subject_id: SubjectId) -> Result<HashMap<AspectId, String>> {
	let db_info = DATABASES.lock().await.get(&db_id).context("Database not found")?.clone();
	let subject_info = db_info.subjects.get(&subject_id).context("Subject not found")?;
	let mut result = HashMap::new();
	for (aspect_id, aspect_info) in &subject_info.aspects {
		result.insert(*aspect_id, aspect_info.name.clone());
	}
	Ok(result)
}

/// Add a new subject to the database
///
/// # Errors
/// - if database not found
/// - if unable to create database connection
pub async fn add_subject(db_id: DatabaseId, name: &str) -> Result<SubjectId> {
	let subject_id = SubjectId::new();

	// Get database path with early drop
	let db_path = DATABASES.lock().await.get(&db_id).context("Database not found")?.path.clone();

	// Ensure the database directory exists
	std::fs::create_dir_all(&db_path).with_context(|| format!("Failed to create database directory '{db_path}'"))?;

	// Create subject database file
	let subject_db_path = format!("{db_path}/{name}.db");

	// Create the database file and connect
	let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(5).connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&subject_db_path).create_if_missing(true)).await.with_context(|| format!("Failed to create subject database for '{name}'"))?;

	let subject_info = SubjectInfo { name: name.to_string(), pool, aspects: HashMap::new() };

	// Add subject to database
	DATABASES.lock().await.get_mut(&db_id).context("Database not found")?.subjects.insert(subject_id, subject_info);

	Ok(subject_id)
}

/// Creates an instance of and initializes a new Aspect.
/// Creates a new table in /{`data_dir}/{db_name}/{name}.db` file.
/// Creates the necessary database structures.
///
/// # Errors
/// - if subject not found
/// - if unable to create database table or index
pub async fn track_aspect(subject: SubjectId, name: &str) -> Result<AspectId> {
	let aspect_id = AspectId(Uuid::new_v4());
	let table_name = sanitize_table_name(name);

	// Get pool with proper scope management - extract immediately
	let pool = DATABASES.lock().await.values().find_map(|db_info| db_info.subjects.get(&subject)).context("Subject not found")?.pool.clone();

	// Create the aspect table
	let create_table_sql = format!("CREATE TABLE IF NOT EXISTS {table_name} (
        id TEXT PRIMARY KEY,
        timestamp INTEGER NOT NULL,
        value TEXT NOT NULL
    )");

	sqlx::query(&create_table_sql).execute(&pool).await.context("Failed to create aspect table")?;

	// Create index for efficient time-based queries
	let create_index_sql = format!("CREATE INDEX IF NOT EXISTS idx_{table_name}_timestamp ON {table_name} (timestamp)");

	sqlx::query(&create_index_sql).execute(&pool).await.context("Failed to create timestamp index")?;

	let aspect_info = AspectInfo { name: name.to_string(), subject_id: subject, table_name };

	// Reacquire lock only to insert the aspect
	{
		let mut databases = DATABASES.lock().await;
		databases.values_mut().find_map(|db_info| db_info.subjects.get_mut(&subject)).context("Subject not found")?.aspects.insert(aspect_id, aspect_info);
	}

	Ok(aspect_id)
}

/// Capture a measurement for an aspect
///
/// # Errors
/// - if aspect not found
/// - if unable to insert measurement into database
pub async fn capture_measurement(aspect: AspectId, measurement: InputMeasurement) -> Result<TxId> {
	let tx_id = TxId(Uuid::new_v4());

	// Get pool and table name with proper scope management - extract immediately
	let (pool, table_name) = DATABASES.lock().await.values().find_map(|db_info| db_info.subjects.values().find_map(|subject_info| subject_info.aspects.get(&aspect).map(|aspect_info| (&subject_info.pool, aspect_info)))).map(|(pool, aspect_info)| (pool.clone(), aspect_info.table_name.clone())).context("Aspect not found")?;

	// Insert measurement into database
	let insert_sql = format!("INSERT INTO {table_name} (id, timestamp, value) VALUES (?, ?, ?)");

	sqlx::query(&insert_sql).bind(tx_id.0.to_string()).bind(measurement.timestamp().timestamp_millis()).bind(measurement.value().to_string()).execute(&pool).await.context("Failed to insert measurement")?;

	// Invalidate cache for this aspect
	let cache_key = format!("aspect_measurements_{}", aspect.0);
	CACHE.invalidate_aspect_cache(&cache_key, aspect.0).await;

	Ok(tx_id)
}

/// Analyze a single point in time for an aspect using interpolation
///
/// # Errors
/// - if aspect not found
/// - if unable to retrieve measurements
/// - if interpolation fails
///
/// # Panics
/// - if measurements collection is empty after validation
/// - if min/max time calculations fail on valid measurements
pub async fn analyze_point(aspect: AspectId, time: DateTime<Utc>, resolution: Resolution, method: Spline) -> Result<DataPoint> {
	// Check cache first for point analysis
	let cache_key = format!("point_{}_{}_{}_{:?}_{:?}", aspect.0, time.timestamp(), time.timestamp_subsec_nanos(), resolution, method);
	if let Some(cached_result) = CACHE.get_point_analysis(&cache_key).await {
		return Ok(DataPoint {
			timestamp: cached_result.timestamp, // Add missing timestamp
			value: cached_result.value,
		});
	}

	// Get measurements for this aspect
	let measurements = get_aspect_measurements(aspect).await?;

	if measurements.is_empty() {
		bail!("No measurements found for aspect");
	}

	// Find min and max times in the data
	let min_time = measurements.iter().map(|m| m.timestamp).min().unwrap();
	let max_time = measurements.iter().map(|m| m.timestamp).max().unwrap();

	// Create a window around the target time
	let window_duration = chrono::Duration::minutes(10); // 10-minute window
	let mut window_start = time - window_duration;
	let mut window_end = time + window_duration;

	// Ensure the window includes actual data
	if window_start > max_time {
		window_start = min_time;
		window_end = time + chrono::Duration::minutes(5);
	} else if window_end < min_time {
		window_start = time - chrono::Duration::minutes(5);
		window_end = max_time;
	} else {
		window_start = window_start.min(min_time);
		window_end = window_end.max(max_time);
	}

	// Final safety check to ensure start is before end
	if window_start >= window_end {
		window_start = min_time;
		window_end = max_time;
	}

	// Use the GPU-aware auto_interpolate function
	let interpolated = splimes::auto_interpolate(&mut measurements_to_points(&measurements), window_start, window_end, resolution, method).await?;

	// Find the measurement closest to the target time
	let closest = interpolated.iter().min_by_key(|m| (m.timestamp - time).num_milliseconds().abs()).context("No interpolated measurements found")?;
	let result = DataPoint { timestamp: closest.timestamp, value: closest.value.clone() };

	// Cache the result
	let analysis_result = cache::AnalysisResult { timestamp: time, value: result.value.clone(), method: format!("{method:?}"), resolution: format!("{resolution:?}") };
	CACHE.store_point_analysis(&cache_key, &analysis_result).await;

	Ok(result)
}

#[must_use]
pub fn measurements_to_points(measurements: &[types::Measurement]) -> Vec<Point> {
	measurements.iter().map(|m| Point { timestamp: m.timestamp, value: m.value.clone() }).collect()
}

/// Interpolates/extrapolates the `DataPoint`[] for a given time range, resolution, & spline type.
/// Uses intelligent measurement collection and caching with GPU acceleration when beneficial.
///
/// # Errors
/// - if interpolation fails
pub async fn analyze_range(aspect: AspectId, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, method: Spline) -> Result<Vec<DataPoint>> {
	// Get measurements for this aspect
	let measurements = get_aspect_measurements(aspect).await?;

	if measurements.is_empty() {
		bail!("No measurements found for aspect");
	}

	// Use the GPU-aware auto_interpolate function
	let interpolated = splimes::auto_interpolate(&mut measurements_to_points(&measurements), start, end, resolution, method).await?;

	// Convert to DataPoints
	let data_points = interpolated.into_iter().map(|m| DataPoint { timestamp: m.timestamp, value: m.value }).collect();

	Ok(data_points)
}

// Helper functions

fn sanitize_table_name(name: &str) -> String {
	name.chars().map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' }).collect()
}

async fn get_aspect_measurements(aspect: AspectId) -> Result<Vec<types::Measurement>> {
	// Check cache first
	let cache_key = format!("aspect_measurements_{}", aspect.0);
	if let Some(cached) = CACHE.get_aspect_measurements(&cache_key, aspect.0).await {
		return Ok(cached);
	}

	// Get from database with proper scope management - extract immediately
	let (pool, table_name, dataset_id) = DATABASES.lock().await.values().find_map(|db_info| db_info.subjects.values().find_map(|subject_info| subject_info.aspects.get(&aspect).map(|aspect_info| (&subject_info.pool, aspect_info)))).map(|(pool, aspect_info)| (pool.clone(), aspect_info.table_name.clone(), aspect.0)).context("Aspect not found")?;

	let query_sql = format!("SELECT id, timestamp, value FROM {table_name} ORDER BY timestamp");
	let rows = sqlx::query(&query_sql).fetch_all(&pool).await.context("Failed to query measurements")?;

	let measurements = rows
		.into_iter()
		.map(|row| {
			let id: String = row.get("id");
			let timestamp_millis: i64 = row.get("timestamp");
			let value_str: String = row.get("value");

			let timestamp = DateTime::from_timestamp_millis(timestamp_millis).unwrap_or_default();
			let value = bigdecimal::BigDecimal::from_str(&value_str).unwrap_or_default();

			types::Measurement::new(Uuid::parse_str(&id).unwrap_or_default(), dataset_id, timestamp, value)
		})
		.collect::<Vec<_>>();

	// Cache the results
	CACHE.store_aspect_measurements(&cache_key, &measurements, aspect.0).await;

	Ok(measurements)
}
