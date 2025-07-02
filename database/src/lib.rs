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
//! let data_point = analyze_point(aspect_id, time, Resolution::Seconds, SplineType::Linear).await?;
//!
//! println!("Interpolated value: {}", data_point.value);
//! # std::fs::remove_dir_all("data/my_experiment").ok();
//! # Ok(())
//! # }
//! ```

#![warn(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions, clippy::used_underscore_binding, clippy::similar_names, clippy::module_name_repetitions, clippy::module_inception)]

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
    path::Path,
};

use anyhow::{bail, Context, Result};
use sqlx::{Pool, Row, Sqlite};
use tokio::sync::Mutex;
use uuid::Uuid;
use chrono::{DateTime, Utc};

pub mod cache;
pub mod splines;
pub mod types;

// Re-export commonly used types
pub use cache::DatabaseCache;
pub use splines::{Resolution, SplineType, auto_interpolate};
pub use types::{Error, InputMeasurement, Measurement};

// For backward compatibility, re-export Dataset from old_db
pub use old_db::Dataset;

// New ID types for the simplified API
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DatabaseId(Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubjectId(Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AspectId(Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TxId(Uuid);

#[derive(Debug, Clone)]
pub struct DataPoint {
    pub timestamp: DateTime<Utc>,
    pub value: bigdecimal::BigDecimal,
}

#[allow(dead_code)]
const BATCH_SIZE: usize = 1000;
const DEFAULT_DATA_DIR: &str = "data";

// Internal storage structures
#[derive(Debug)]
struct DatabaseInfo {
    #[allow(dead_code)]
    name: String,
    path: String,
    subjects: HashMap<SubjectId, SubjectInfo>,
}

#[derive(Debug)]
struct SubjectInfo {
    #[allow(dead_code)]
    name: String,
    pool: Pool<Sqlite>,
    aspects: HashMap<AspectId, AspectInfo>,
}

#[derive(Debug, Clone)]
struct AspectInfo {
    #[allow(dead_code)]
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
        bail!("Database folder '{db_path}' already exists");
    }
    
    // Create the directory
    std::fs::create_dir_all(&db_path)
        .with_context(|| format!("Failed to create database directory '{db_path}'"))?;
    
    let db_id = DatabaseId(Uuid::new_v4());
    let db_info = DatabaseInfo {
        name: name.to_string(),
        path: db_path,
        subjects: HashMap::new(),
    };
    
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
        bail!("Database folder '{db_path}' does not exist");
    }
    
    let db_id = DatabaseId(Uuid::new_v4());
    let mut db_info = DatabaseInfo {
        name: name.to_string(),
        path: db_path.clone(),
        subjects: HashMap::new(),
    };
    
    // Load existing subjects
    if let Ok(entries) = std::fs::read_dir(&db_path) {
        for entry in entries.flatten() {
            if let Some(file_name) = entry.file_name().to_str() {
                // Use case-insensitive file extension check
                if std::path::Path::new(file_name)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("db")) {
                    let subject_name = file_name.trim_end_matches(".db");
                    let subject_id = SubjectId(Uuid::new_v4());
                    
                    // Connect to subject database
                    let database_url = format!("sqlite:{db_path}/{file_name}");
                    let pool = sqlx::sqlite::SqlitePoolOptions::new()
                        .max_connections(5)
                        .connect(&database_url)
                        .await
                        .with_context(|| format!("Failed to connect to subject database '{subject_name}'"))?;
                    
                    // Load aspects from database
                    let mut aspects = HashMap::new();
                    let rows = sqlx::query("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
                        .fetch_all(&pool)
                        .await
                        .context("Failed to query aspect tables")?;
                    
                    for row in rows {
                        let table_name: String = row.get("name");
                        let aspect_id = AspectId(Uuid::new_v4());
                        aspects.insert(aspect_id, AspectInfo {
                            name: table_name.clone(),
                            subject_id,
                            table_name,
                        });
                    }
                    
                    let subject_info = SubjectInfo {
                        name: subject_name.to_string(),
                        pool,
                        aspects,
                    };
                    
                    db_info.subjects.insert(subject_id, subject_info);
                }
            }
        }
    }
    
    DATABASES.lock().await.insert(db_id, db_info);
    
    Ok(db_id)
}

/// Creates an instance of and initializes a new Subject.
/// Creates a /{`data_dir}/{db_name}/{name}.db` file.
/// Creates the necessary tables and structure.
///
/// # Errors
/// - if database not found
/// - if unable to create directories or files
/// - if database connection fails
pub async fn add_subject(db: DatabaseId, name: &str) -> Result<SubjectId> {
    let subject_id = SubjectId(Uuid::new_v4());
    
    // Get database path and release lock immediately
    let db_path = DATABASES.lock().await
        .get(&db)
        .context("Database not found")?
        .path.clone();
    
    // Ensure the database directory exists
    std::fs::create_dir_all(&db_path)
        .with_context(|| format!("Failed to create database directory '{db_path}'"))?;
    
    let db_file_path = format!("{db_path}/{name}.db");
    
    // Ensure the file can be created by touching it first
    if let Some(parent) = std::path::Path::new(&db_file_path).parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create parent directory for '{db_file_path}'"))?;
    }
    
    // Try to create the file if it doesn't exist
    if !std::path::Path::new(&db_file_path).exists() {
        std::fs::File::create(&db_file_path)
            .with_context(|| format!("Failed to create database file '{db_file_path}'"))?;
    }
    
    let database_url = format!("sqlite:{db_file_path}");
    
    // Create the database connection
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .with_context(|| format!("Failed to create subject database connection for '{name}' at '{db_file_path}'"))?;
    
    let subject_info = SubjectInfo {
        name: name.to_string(),
        pool,
        aspects: HashMap::new(),
    };
    
    // Reacquire lock only to insert the subject
    DATABASES.lock().await
        .get_mut(&db)
        .context("Database not found")?
        .subjects.insert(subject_id, subject_info);
    
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
    
    // Get pool and release lock quickly
    let pool = {
        let databases = DATABASES.lock().await;
        databases
            .iter()
            .find_map(|(_, db_info)| {
                db_info.subjects.get(&subject).map(|s| s.pool.clone())
            })
            .context("Subject not found")?
    };
    
    // Create the aspect table
    let create_table_sql = format!(
        r"CREATE TABLE IF NOT EXISTS {table_name} (
            id TEXT PRIMARY KEY,
            timestamp TEXT NOT NULL,
            value TEXT NOT NULL
        )"
    );
    
    sqlx::query(&create_table_sql)
        .execute(&pool)
        .await
        .context("Failed to create aspect table")?;
    
    // Create index for efficient time-based queries
    let create_index_sql = format!(
        "CREATE INDEX IF NOT EXISTS idx_{table_name}_timestamp ON {table_name} (timestamp)"
    );
    
    sqlx::query(&create_index_sql)
        .execute(&pool)
        .await
        .context("Failed to create timestamp index")?;
    
    let aspect_info = AspectInfo {
        name: name.to_string(),
        subject_id: subject,
        table_name,
    };
    
    // Reacquire lock only to insert the aspect
    DATABASES.lock().await
        .iter_mut()
        .find_map(|(_, db_info)| {
            db_info.subjects.get_mut(&subject)
        })
        .context("Subject not found")?
        .aspects.insert(aspect_id, aspect_info);
    
    Ok(aspect_id)
}

/// Creates a new entry in the corresponding aspect's table.
/// Invalidates relevant cache entries.
///
/// # Errors
/// - if aspect not found
/// - if database insert operation fails
pub async fn capture_measurement(aspect: AspectId, input_measurement: InputMeasurement) -> Result<TxId> {
    let tx_id = TxId(Uuid::new_v4());
    
    // Get pool and table name, then release lock quickly
    let (pool, table_name) = {
        let databases = DATABASES.lock().await;
        
        // Find the subject and aspect
        databases
            .iter()
            .find_map(|(_, db_info)| {
                db_info.subjects.iter().find_map(|(_, subject_info)| {
                    subject_info.aspects.get(&aspect).map(|aspect_info| {
                        (subject_info.pool.clone(), aspect_info.table_name.clone())
                    })
                })
            })
            .context("Aspect not found")?
    };
    
    let insert_sql = format!(
        "INSERT INTO {table_name} (id, timestamp, value) VALUES (?, ?, ?)"
    );
    
    sqlx::query(&insert_sql)
        .bind(tx_id.0.to_string())
        .bind(input_measurement.timestamp().to_rfc3339())
        .bind(input_measurement.value().to_string())
        .execute(&pool)
        .await
        .context("Failed to insert measurement")?;
    
    // Invalidate cache for this aspect since we added new data
    let cache_key = format!("aspect_measurements_{}", aspect.0);
    CACHE.invalidate_aspect_cache(&cache_key, aspect.0).await;
    
    println!("Invalidated cache for aspect {} after new measurement", aspect.0);
    
    Ok(tx_id)
}

/// Interpolates/extrapolates the `DataPoint` for a given time, resolution, & spline type.
/// Uses intelligent measurement collection and caching.
///
/// # Errors
/// - if no measurements found for aspect
/// - if interpolation fails
/// - if no interpolated data point found
pub async fn analyze_point(
    aspect: AspectId,
    time: DateTime<Utc>,
    resolution: Resolution,
    method: SplineType,
) -> Result<DataPoint> {
    // Create cache key for this specific analysis
    let cache_key = format!(
        "point_{}_{}_{}_{:?}_{:?}",
        aspect.0,
        time.timestamp(),
        resolution as u8,
        method,
        time.format("%Y%m%d%H%M%S")
    );
    
    // Check cache first
    if let Some(cached_result) = CACHE.get_point_analysis(&cache_key).await {
        println!("Cache hit for point analysis at: {time}");
        return Ok(DataPoint {
            timestamp: cached_result.timestamp,
            value: cached_result.value,
        });
    }

    let measurements = get_aspect_measurements(aspect).await?;
    if measurements.is_empty() {
        bail!("No measurements found for aspect");
    }

    // Get measurements around the target time for interpolation
    let window_start = time - chrono::Duration::minutes(30);
    let window_end = time + chrono::Duration::minutes(30);

    let interpolated = splines::auto_interpolate(
        measurements,
        window_start,
        window_end,
        resolution,
        method,
    ).await?;

    // Find the measurement closest to our target time
    let result = interpolated
        .into_iter()
        .min_by_key(|m| {
            let diff = m.timestamp.signed_duration_since(time);
            diff.num_milliseconds().abs()
        })
        .map(|m| DataPoint {
            timestamp: m.timestamp,
            value: m.value,
        })
        .context("No interpolated data point found")?;

    // Cache the result
    let analysis_result = cache::AnalysisResult {
        timestamp: result.timestamp,
        value: result.value.clone(),
        method: format!("{method:?}"),
        resolution: format!("{resolution:?}"),
    };
    CACHE.store_point_analysis(&cache_key, &analysis_result).await;
    println!("Cached point analysis result for: {time}");

    Ok(result)
}

/// Interpolates/extrapolates the `DataPoint`[] for a given time range, resolution, & spline type.
/// Uses intelligent measurement collection and caching.
///
/// # Errors
/// - if interpolation fails
pub async fn analyze_range(
    aspect: AspectId,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    resolution: Resolution,
    method: SplineType,
) -> Result<Vec<DataPoint>> {
    // Create cache key for this specific range analysis
    let duration_secs = end.signed_duration_since(start).num_seconds();
    let cache_key = format!(
        "range_{}_{}_{}_{:?}_{:?}_{}",
        aspect.0,
        start.timestamp(),
        end.timestamp(),
        resolution,
        method,
        duration_secs
    );
    
    // Check cache first
    if let Some(cached_results) = CACHE.get_range_analysis(&cache_key).await {
        println!("Cache hit for range analysis: {start} to {end}");
        return Ok(cached_results.into_iter().map(|r| DataPoint {
            timestamp: r.timestamp,
            value: r.value,
        }).collect());
    }

    let measurements = get_aspect_measurements(aspect).await?;
    if measurements.is_empty() {
        return Ok(Vec::new());
    }

    let interpolated = splines::auto_interpolate(
        measurements,
        start,
        end,
        resolution,
        method,
    ).await?;

    let results: Vec<DataPoint> = interpolated
        .into_iter()
        .map(|m| DataPoint {
            timestamp: m.timestamp,
            value: m.value,
        })
        .collect();

    // Cache the results
    let cache_results: Vec<cache::AnalysisResult> = results.iter().map(|dp| cache::AnalysisResult {
        timestamp: dp.timestamp,
        value: dp.value.clone(),
        method: format!("{method:?}"),
        resolution: format!("{resolution:?}"),
    }).collect();
    
    CACHE.store_range_analysis(&cache_key, &cache_results).await;
    println!("Cached range analysis result: {} points for {} to {}", results.len(), start, end);

    Ok(results)
}

// Helper functions

fn sanitize_table_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

async fn get_aspect_measurements(aspect: AspectId) -> Result<Vec<types::Measurement>> {
    // Check cache first
    let cache_key = format!("aspect_measurements_{}", aspect.0);
    if let Some(cached_measurements) = CACHE.get_aspect_measurements(&cache_key, aspect.0).await {
        println!("Cache hit for aspect measurements: {}", aspect.0);
        return Ok(cached_measurements);
    }

    // Clone the necessary data to avoid lifetime issues
    let (pool, table_name) = {
        let databases = DATABASES.lock().await;
        
        // Find the subject and aspect
        databases
            .iter()
            .find_map(|(_, db_info)| {
                db_info.subjects.iter().find_map(|(_, subject_info)| {
                    subject_info.aspects.get(&aspect).map(|aspect_info| {
                        (subject_info.pool.clone(), aspect_info.table_name.clone())
                    })
                })
            })
            .context("Aspect not found")?
    }; // databases lock is released here
    
    let select_sql = format!(
        "SELECT id, timestamp, value FROM {table_name} ORDER BY timestamp"
    );
    
    let rows = sqlx::query(&select_sql)
        .fetch_all(&pool)
        .await
        .context("Failed to query measurements")?;
    
    let mut measurements = Vec::new();
    for row in rows {
        let id_str: String = row.get("id");
        let timestamp_str: String = row.get("timestamp");
        let value_str: String = row.get("value");
        
        let id = Uuid::parse_str(&id_str).context("Failed to parse measurement ID")?;
        let timestamp = chrono::DateTime::parse_from_rfc3339(&timestamp_str)
            .context("Failed to parse timestamp")?
            .with_timezone(&chrono::Utc);
        let value = bigdecimal::BigDecimal::parse_bytes(value_str.as_bytes(), 10)
            .context("Failed to parse value")?;
        
        measurements.push(types::Measurement {
            id,
            dataset_id: aspect.0, // Use aspect ID as dataset ID for compatibility
            timestamp,
            value,
        });
    }
    
    // Store in cache
    CACHE.store_aspect_measurements(&cache_key, &measurements, aspect.0).await;
    println!("Cached {} measurements for aspect: {}", measurements.len(), aspect.0);
    
    Ok(measurements)
}

// Keep the old DB struct for backward compatibility, but mark as deprecated
#[deprecated(note = "Use the new simplified API functions instead")]
pub use crate::old_db::DB;

#[allow(deprecated)]
mod old_db {
    use super::types::Measurement;
    use uuid::Uuid;
    
    #[derive(Debug, Clone, PartialEq, Eq, Copy)]
    pub struct DB;
    
    // Dataset struct for backward compatibility with cache
    #[derive(Debug, Clone)]
    pub struct Dataset {
        pub id: Uuid,
        pub name: String,
        measurements: Vec<Measurement>,
    }
    
    impl Dataset {
        #[must_use] pub const fn new(id: Uuid, name: String) -> Self {
            Self {
                id,
                name,
                measurements: Vec::new(),
            }
        }
        
        #[must_use] pub const fn id(&self) -> Uuid {
            self.id
        }
        
        #[must_use] pub fn name(&self) -> String {
            self.name.clone()
        }
        
        #[must_use] pub const fn measurements(&self) -> &Vec<Measurement> {
            &self.measurements
        }
        
        pub const fn measurements_mut(&mut self) -> &mut Vec<Measurement> {
            &mut self.measurements
        }
        
        pub fn add_measurement(&mut self, measurement: Measurement) {
            self.measurements.push(measurement);
        }
    }
}
