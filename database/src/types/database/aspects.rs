use std::{collections::HashMap, sync::Mutex};

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use splimes::Resolution;

use crate::{Aspect, AspectId, Database, Error, Subject, DATABASES};

impl Database {
	/// Creates an instance of and initializes a new Aspect.
	/// Creates a new table in /{`data_dir}/{db_name}/{name}.db` file.
	/// Creates the necessary database structures.
	///
	/// # Errors
	/// - if subject not found
	/// - if unable to create database table or index
	pub async fn track_aspect(&self, subject: Subject, name: &str, resolution: Resolution) -> Result<Aspect> {
		let aspect_id = AspectId::new();
		let table_name = Self::sanitize_table_name(name);

		// Get client and metadata client with proper scope management - extract immediately
		let db_id = self.id();
		let db_info = self.get_database_info().await.ok_or_else(|| Error::DatabaseError("Database not found".to_string()))?;
		let metadata_turso_db = db_info.metadata_turso_db().cloned().ok_or_else(|| Error::DatabaseError("Metadata turso_db not found".to_string()))?;
		let subject_turso_db = subject.turso_db().cloned().ok_or_else(|| Error::DatabaseError("Subject turso_db not found".to_string()))?;

		// Create the aspect table
		let conn = subject_turso_db.connect()?;
		let create_table_sql = format!("CREATE TABLE IF NOT EXISTS {table_name} (id TEXT PRIMARY KEY, timestamp TEXT NOT NULL, value TEXT NOT NULL)");
		conn.execute(&create_table_sql, turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to create aspect table: {e}")))?;

		// Create index for efficient time-based queries
		let create_index_sql = format!("CREATE INDEX IF NOT EXISTS idx_{table_name}_timestamp ON {table_name} (timestamp)");
		conn.execute(&create_index_sql, turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to create index for aspect table: {e}")))?;

		// Insert aspect metadata into merged aspects table
		let metadata_conn = metadata_turso_db.connect()?;
		metadata_conn.execute("INSERT INTO aspects (id, subject_id, database_id, name, table_name, resolution, created_at, earliest_measurement, latest_measurement) VALUES (?, ?, ?, ?, ?, ?, ?, NULL, NULL)", turso::params![aspect_id.as_uuid().to_string(), subject.id().as_uuid().to_string(), db_id.as_uuid().to_string(), name.to_string(), table_name.clone(), format!("{resolution:?}"), chrono::Utc::now().timestamp_millis().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to insert aspect metadata: {e}")))?;

		let aspect_info = Aspect::new_with_id(aspect_id, name.to_string(), subject.id(), table_name, resolution);

		// Reacquire lock only to insert the aspect
		{
			let mut databases = DATABASES.lock().await;
			match databases.values_mut().find_map(|db_info| {
				for subject_info in db_info.subjects_mut().values_mut() {
					if subject_info.id() == subject.id() {
						subject_info.add_aspect(aspect_info.clone());
						return Some(());
					}
				}
				None
			}) {
				Some(()) => (),
				None => bail!(Error::DatabaseError("Subject not found".to_string())),
			}
		}

		Ok(aspect_info)
	}

	pub async fn get_aspect(&self, aspect_id: &AspectId) -> Option<Aspect> {
		let db = self.get_database_info().await?;
		db.subjects().values().find_map(|subject_info| subject_info.aspects().get(aspect_id).cloned())
	}

	/// Get the earliest measurement timestamp for an aspect
	///
	/// # Errors
	/// - if database not found
	/// - if metadata pool not found
	/// - if unable to query aspect metadata
	pub async fn get_earliest_measurement(&self, aspect_id: &AspectId) -> Result<Option<DateTime<Utc>>> {
		static CACHE: Lazy<Mutex<HashMap<AspectId, Option<DateTime<Utc>>>>> = Lazy::new(|| Mutex::new(HashMap::new()));
		{
			let guard = CACHE.lock().unwrap();
			if let Some(cached) = guard.get(aspect_id) {
				return Ok(*cached);
			}
		}
		let db_info = self.get_database_info().await.ok_or_else(|| Error::DatabaseError("Database not found".to_string()))?;
		let metadata_turso_db = db_info.metadata_turso_db().cloned().ok_or_else(|| Error::DatabaseError("Metadata turso_db not found".to_string()))?;

		let conn = metadata_turso_db.connect()?;
		let mut rows = conn.query("SELECT earliest_measurement FROM aspects WHERE id = ?", turso::params![aspect_id.as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query earliest measurement: {e}")))?;

		let result = if let Some(r) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get row: {e}")))? {
			match r.get_value(0) {
				Ok(value) => {
					if let Some(text) = value.as_text() {
						text.parse::<i64>().ok().and_then(DateTime::from_timestamp_millis)
					} else {
						None
					}
				}
				Err(_) => None,
			}
		} else {
			None
		};
		{
			let mut guard = CACHE.lock().unwrap();
			guard.insert(*aspect_id, result);
		}
		Ok(result)
	}

	/// Get the latest measurement timestamp for an aspect
	///
	/// # Errors
	/// - if database not found
	/// - if metadata pool not found
	/// - if unable to query aspect metadata
	pub async fn get_latest_measurement(&self, aspect_id: &AspectId) -> Result<Option<DateTime<Utc>>> {
		static CACHE: Lazy<Mutex<HashMap<AspectId, Option<DateTime<Utc>>>>> = Lazy::new(|| Mutex::new(HashMap::new()));
		{
			let guard = CACHE.lock().unwrap();
			if let Some(cached) = guard.get(aspect_id) {
				return Ok(*cached);
			}
		}
		let db_info = self.get_database_info().await.ok_or_else(|| Error::DatabaseError("Database not found".to_string()))?;
		let metadata_turso_db = db_info.metadata_turso_db().cloned().ok_or_else(|| Error::DatabaseError("Metadata turso_db not found".to_string()))?;

		let conn = metadata_turso_db.connect()?;
		let mut rows = conn.query("SELECT latest_measurement FROM aspects WHERE id = ?", turso::params![aspect_id.as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query latest measurement: {e}")))?;

		let result = if let Some(r) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get row: {e}")))? {
			match r.get_value(0) {
				Ok(value) => {
					if let Some(text) = value.as_text() {
						text.parse::<i64>().ok().and_then(DateTime::from_timestamp_millis)
					} else {
						None
					}
				}
				Err(_) => None,
			}
		} else {
			None
		};
		{
			let mut guard = CACHE.lock().unwrap();
			guard.insert(*aspect_id, result);
		}
		Ok(result)
	}

	/// Get the resolution for an aspect
	///
	/// # Errors
	/// - if database not found
	/// - if metadata pool not found
	/// - if unable to query aspect metadata
	/// - if unable to parse resolution
	pub async fn get_aspect_resolution(&self, aspect_id: &AspectId) -> Result<Option<Resolution>> {
		static CACHE: Lazy<Mutex<HashMap<AspectId, Option<Resolution>>>> = Lazy::new(|| Mutex::new(HashMap::new()));
		{
			let guard = CACHE.lock().unwrap();
			if let Some(cached) = guard.get(aspect_id) {
				return Ok(*cached);
			}
		}
		let db_info = self.get_database_info().await.ok_or_else(|| Error::DatabaseError("Database not found".to_string()))?;
		let metadata_turso_db = db_info.metadata_turso_db().cloned().ok_or_else(|| Error::DatabaseError("Metadata turso_db not found".to_string()))?;

		let conn = metadata_turso_db.connect()?;
		let mut rows = conn.query("SELECT resolution FROM aspects WHERE id = ?", turso::params![aspect_id.as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query resolution: {e}")))?;

		let result = if let Some(r) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get row: {e}")))? {
			match r.get_value(0) {
				Ok(value) => {
					if let Some(res_str) = value.as_text() {
						Some(match res_str.as_str() {
							"Nanoseconds" => Resolution::Nanoseconds,
							"Microseconds" => Resolution::Microseconds,
							"Milliseconds" => Resolution::Milliseconds,
							"Seconds" => Resolution::Seconds,
							"Minutes" => Resolution::Minutes,
							"Hours" => Resolution::Hours,
							"Days" => Resolution::Days,
							"Weeks" => Resolution::Weeks,
							"Months" => Resolution::Months,
							"Years" => Resolution::Years,
							_ => return Err(Error::DatabaseError(format!("Invalid resolution value: {}", res_str)).into()),
						})
					} else {
						None
					}
				}
				Err(_) => None,
			}
		} else {
			None
		};
		{
			let mut guard = CACHE.lock().unwrap();
			guard.insert(*aspect_id, result);
		}
		Ok(result)
	}
}
