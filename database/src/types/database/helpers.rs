use std::str::FromStr;

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use splimes::Point;
use uuid::Uuid;

use crate::{types::database::traits::aspect_structure::AspectStructure, AspectId, Database, Error, Measurement, CACHE, DATABASES};

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
		let p = v.values().find_map(|db_info| db_info.subjects().values().find_map(|subject_info| subject_info.aspects().get(&aspect).and_then(|aspect_info| aspect_info.measurements().await.map(|turso_db| (turso_db.clone(), aspect_info.name().to_string(), aspect.as_uuid())))));
		let Some((turso_db, table_name, dataset_id)) = p else { bail!(Error::DatabaseError("Aspect not found".to_string())) };

		let query_sql = format!("SELECT id, timestamp, value FROM {table_name} ORDER BY timestamp");
		let conn = turso_db.connect()?;
		let mut rows = conn.query(&query_sql, ()).await.map_err(|e| Error::DatabaseError(format!("Failed to query measurements: {e}")))?;

		let mut measurements = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get row: {e}")))? {
			let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("ID is not text".to_string()))?.clone();
			let timestamp_millis_str = row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Timestamp is not text".to_string()))?.clone();
			let value_str = row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Value is not text".to_string()))?.clone();

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
		let p = v.values().find_map(|db_info| db_info.subjects().values().find_map(|subject_info| subject_info.aspects().get(&aspect).and_then(|aspect_info| aspect_info.measurements().await.map(|turso_db| (turso_db.clone(), aspect.as_uuid())))));
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
			let id_str = row.get_value(0)?.as_text().ok_or_else(|| Error::DatabaseError("ID is not text".to_string()))?.clone();
			let timestamp_millis_str = row.get_value(1)?.as_text().ok_or_else(|| Error::DatabaseError("Timestamp is not text".to_string()))?.clone();
			let value_str = row.get_value(2)?.as_text().ok_or_else(|| Error::DatabaseError("Value is not text".to_string()))?.clone();

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

pub(crate) fn safe_usize_to_f64(value: usize) -> std::result::Result<f64, Error> {
	let value_u64 = u64::try_from(value).map_err(|_| Error::NumericConversionError(format!("Value {value} does not fit into u64")))?;
	let max_exact = 1u64 << f64::MANTISSA_DIGITS;
	if value_u64 > max_exact {
		return Err(Error::NumericConversionError(format!("Value {value} exceeds f64 precision limit (max exact integer is {max_exact})")));
	}
	#[allow(clippy::cast_precision_loss)]
	Ok(value_u64 as f64)
}

pub(crate) fn safe_i64_to_f64(value: i64) -> std::result::Result<f64, Error> {
	let abs_value = value.checked_abs().ok_or_else(|| Error::NumericConversionError(format!("Value {value} cannot be safely negated")))?;
	let abs_u64 = u64::try_from(abs_value).map_err(|_| Error::NumericConversionError(format!("Value {abs_value} cannot be represented as u64")))?;
	let max_exact = 1u64 << f64::MANTISSA_DIGITS;
	if abs_u64 > max_exact {
		return Err(Error::NumericConversionError(format!("Value {value} exceeds f64 precision limit (max exact integer is {max_exact})")));
	}
	#[allow(clippy::cast_precision_loss)]
	Ok(value as f64)
}

pub(crate) fn safe_ratio(numerator: usize, denominator: usize) -> std::result::Result<f64, Error> {
	if denominator == 0 {
		return Ok(0.0);
	}
	let numerator_f64 = safe_usize_to_f64(numerator)?;
	let denominator_f64 = safe_usize_to_f64(denominator)?;
	Ok(numerator_f64 / denominator_f64)
}
