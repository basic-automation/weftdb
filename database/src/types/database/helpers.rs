use std::str::FromStr;

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use splimes::Point;
use uuid::Uuid;

use crate::{AspectId, Database, Error, Measurement, CACHE, DATABASES};

impl Database {
	pub(crate) fn sanitize_table_name(name: &str) -> String {
		// Replace special characters with underscores
		name.chars().map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' }).collect()
	}

	pub(crate) async fn get_aspect_measurements(aspect: AspectId) -> Result<Vec<Measurement>> {
		// Check cache first
		let cache_key = format!("aspect_measurements_{}", aspect.as_uuid());
		if let Some(cached) = CACHE.get_aspect_measurements(&cache_key, aspect.as_uuid()).await {
			return Ok(cached);
		}

		// Get from database with proper scope management - extract immediately
		let v = DATABASES.lock().await.clone();
		let p = v.values().find_map(|db_info| db_info.subjects().values().find_map(|subject_info| subject_info.aspects().get(&aspect).and_then(|aspect_info| subject_info.turso_db().map(|turso_db| (turso_db.clone(), aspect_info.table_name().to_string(), aspect.as_uuid())))));
		let Some((turso_db, table_name, dataset_id)) = p else { bail!(Error::DatabaseError("Aspect not found".to_string())) };

		let query_sql = format!("SELECT id, timestamp, value FROM {table_name} ORDER BY timestamp");
		let conn = turso_db.connect()?;
		let mut rows = conn.query(&query_sql, ()).await.map_err(|e| Error::DatabaseError(format!("Failed to query measurements: {e}")))?;

		let mut measurements = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get row: {e}")))? {
			let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("ID is not text".to_string()))?.to_string();
			let timestamp_millis_str = row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Timestamp is not text".to_string()))?.to_string();
			let value_str = row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Value is not text".to_string()))?.to_string();

			// Parse the UUID from string (measurement IDs are stored as strings)
			let Ok(id) = Uuid::parse_str(&id_str) else {
				return Err(Error::InvalidIdError(format!("Invalid UUID format for measurement ID: {id_str}")).into());
			};

			let timestamp_millis: i64 = timestamp_millis_str.parse().map_err(|e| Error::DatabaseError(format!("Invalid timestamp format: {e}")))?;
			let timestamp = DateTime::from_timestamp_millis(timestamp_millis).ok_or_else(|| Error::DatabaseError("Invalid timestamp".to_string()))?;
			let value = bigdecimal::BigDecimal::from_str(&value_str).map_err(|e| Error::DatabaseError(format!("Invalid value format: {e}")))?;

			measurements.push(Measurement::new(id, dataset_id, timestamp, value));
		}

		// Cache the results
		CACHE.store_aspect_measurements(&cache_key, &measurements, aspect.as_uuid()).await;

		Ok(measurements)
	}

	pub(crate) async fn get_aspect_measurements_range(aspect: AspectId, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Vec<Measurement>> {
		Self::get_aspect_measurements_range_limited(aspect, start, end, None).await
	}

	async fn get_aspect_measurements_range_limited(aspect: AspectId, start: DateTime<Utc>, end: DateTime<Utc>, limit: Option<usize>) -> Result<Vec<Measurement>> {
		// No cache for ranged queries to avoid complexity
		let v = DATABASES.lock().await.clone();
		let p = v.values().find_map(|db_info| db_info.subjects().values().find_map(|subject_info| subject_info.aspects().get(&aspect).and_then(|aspect_info| subject_info.turso_db().map(|turso_db| (turso_db.clone(), aspect_info.table_name().to_string(), aspect.as_uuid())))));
		let Some((turso_db, table_name, dataset_id)) = p else { bail!(Error::DatabaseError("Aspect not found".to_string())) };

		let conn = turso_db.connect()?;
		let mut rows = if let Some(limit) = limit {
			let sql = format!("SELECT id, timestamp, value FROM {table_name} WHERE timestamp BETWEEN ? AND ? ORDER BY timestamp LIMIT ?");
			conn.query(&sql, turso::params![start.timestamp_millis().to_string(), end.timestamp_millis().to_string(), limit.to_string()]).await
		} else {
			let sql = format!("SELECT id, timestamp, value FROM {table_name} WHERE timestamp BETWEEN ? AND ? ORDER BY timestamp");
			conn.query(&sql, turso::params![start.timestamp_millis().to_string(), end.timestamp_millis().to_string()]).await
		}
		.map_err(|e| Error::DatabaseError(format!("Failed to query measurements: {e}")))?;

		let mut measurements = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get row: {e}")))? {
			let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("ID is not text".to_string()))?.to_string();
			let timestamp_millis_str = row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Timestamp is not text".to_string()))?.to_string();
			let value_str = row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Value is not text".to_string()))?.to_string();

			let Ok(id) = Uuid::parse_str(&id_str) else {
				return Err(Error::InvalidIdError(format!("Invalid UUID format for measurement ID: {id_str}")).into());
			};

			let timestamp_millis: i64 = timestamp_millis_str.parse().map_err(|e| Error::DatabaseError(format!("Invalid timestamp format: {e}")))?;
			let timestamp = DateTime::from_timestamp_millis(timestamp_millis).ok_or_else(|| Error::DatabaseError("Invalid timestamp".to_string()))?;
			let value = bigdecimal::BigDecimal::from_str(&value_str).map_err(|e| Error::DatabaseError(format!("Invalid value format: {e}")))?;

			measurements.push(Measurement::new(id, dataset_id, timestamp, value));
		}

		Ok(measurements)
	}

	#[must_use]
	pub fn measurements_to_points(measurements: &[Measurement]) -> Vec<Point> {
		measurements.iter().map(|m| Point { timestamp: m.timestamp(), value: m.value().clone() }).collect()
	}
}
