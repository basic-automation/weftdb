use std::str::FromStr;

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use splimes::Point;
use sqlx::Row;
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
		let p = v.values().find_map(|db_info| db_info.subjects().values().find_map(|subject_info| subject_info.aspects().get(&aspect).and_then(|aspect_info| subject_info.pool().map(|pool| (pool.clone(), aspect_info.table_name().to_string(), aspect.as_uuid())))));
		let Some((pool, table_name, dataset_id)) = p else { bail!(Error::DatabaseError("Aspect not found".to_string())) };

		let query_sql = format!("SELECT id, timestamp, value FROM {table_name} ORDER BY timestamp");
		let rows = match sqlx::query(&query_sql).fetch_all(&pool).await {
			Ok(rows) => rows,
			Err(e) => bail!(Error::DatabaseError(format!("Failed to query measurements: {e}"))),
		};

		let measurements = rows
			.into_iter()
			.map(|row| {
				let id_str: String = row.get("id");
				let timestamp_millis: i64 = row.get("timestamp");
				let value_str: String = row.get("value");

				// Parse the UUID from string (measurement IDs are stored as strings)
				let Ok(id) = Uuid::parse_str(&id_str) else {
					return Err(Error::InvalidIdError(format!("Invalid UUID format for measurement ID: {id_str}")));
				};

				let timestamp = DateTime::from_timestamp_millis(timestamp_millis).ok_or_else(|| Error::DatabaseError("Invalid timestamp".to_string()))?;
				let value = bigdecimal::BigDecimal::from_str(&value_str).map_err(|e| Error::DatabaseError(format!("Invalid value format: {e}")))?;

				// Use the correct method signature: new(id, dataset_id, timestamp, value)
				Ok(Measurement::new(id, dataset_id, timestamp, value))
			})
			.collect::<Result<Vec<_>, _>>()?;

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
		let p = v.values().find_map(|db_info| db_info.subjects().values().find_map(|subject_info| subject_info.aspects().get(&aspect).and_then(|aspect_info| subject_info.pool().map(|pool| (pool.clone(), aspect_info.table_name().to_string(), aspect.as_uuid())))));
		let Some((pool, table_name, dataset_id)) = p else { bail!(Error::DatabaseError("Aspect not found".to_string())) };

		let query_sql = if limit.is_some() { format!("SELECT id, timestamp, value FROM {table_name} WHERE timestamp BETWEEN ? AND ? ORDER BY timestamp LIMIT ?") } else { format!("SELECT id, timestamp, value FROM {table_name} WHERE timestamp BETWEEN ? AND ? ORDER BY timestamp") };

		let mut query = sqlx::query(&query_sql).bind(start.timestamp_millis()).bind(end.timestamp_millis());
		if let Some(limit) = limit {
			query = query.bind(limit as i64);
		}

		let rows = match query.fetch_all(&pool).await {
			Ok(rows) => rows,
			Err(e) => bail!(Error::DatabaseError(format!("Failed to query measurements: {e}"))),
		};

		let measurements = rows
			.into_iter()
			.map(|row| {
				let id_str: String = row.get("id");
				let timestamp_millis: i64 = row.get("timestamp");
				let value_str: String = row.get("value");

				let Ok(id) = Uuid::parse_str(&id_str) else {
					return Err(Error::InvalidIdError(format!("Invalid UUID format for measurement ID: {id_str}")));
				};

				let timestamp = DateTime::from_timestamp_millis(timestamp_millis).ok_or_else(|| Error::DatabaseError("Invalid timestamp".to_string()))?;
				let value = bigdecimal::BigDecimal::from_str(&value_str).map_err(|e| Error::DatabaseError(format!("Invalid value format: {e}")))?;

				Ok(Measurement::new(id, dataset_id, timestamp, value))
			})
			.collect::<Result<Vec<_>, _>>()?;

		Ok(measurements)
	}

	#[must_use]
	pub fn measurements_to_points(measurements: &[Measurement]) -> Vec<Point> {
		measurements.iter().map(|m| Point { timestamp: m.timestamp(), value: m.value().clone() }).collect()
	}
}
