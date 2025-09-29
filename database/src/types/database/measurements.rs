use anyhow::{bail, Result};
use chrono::DateTime;

use crate::{Aspect, Database, Error, InputMeasurement, TxId, CACHE, DATABASES};

const CHUNK_SIZE: usize = 1000;

impl Database {
	/// Capture a measurement for an aspect
	///
	/// # Errors
	/// - if aspect not found
	/// - if unable to insert measurement into database
	pub async fn observe_measurement(&self, aspect: Aspect, measurement: InputMeasurement) -> Result<TxId> {
		let tx_id = TxId::new();

		// Get client and table name
		let f = DATABASES.lock().await.values().find_map(|db_info| db_info.subjects().values().find_map(|subject_info| subject_info.aspects().get(&aspect.id()).and_then(|aspect_info| subject_info.turso_db().map(|turso_db| (turso_db.clone(), aspect_info.table_name().to_string())))));
		let Some((turso_db, table_name)) = f else { bail!(Error::DatabaseError("Aspect not found".to_string())) };

		// Insert measurement
		let conn = turso_db.connect()?;
		let insert_sql = format!("INSERT INTO {table_name} (id, timestamp, value) VALUES (?, ?, ?)");
		conn.execute(&insert_sql, turso::params![tx_id.as_uuid().to_string(), measurement.timestamp().timestamp_millis().to_string(), measurement.value().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to insert measurement: {e}")))?;

		// Invalidate cache
		let cache_key = format!("aspect_measurements_{}", aspect.id().as_uuid());
		CACHE.invalidate_aspect_cache(&cache_key, aspect.id().as_uuid()).await;

		// Update earliest and latest in metadata
		let db_info = self.get_database_info().await.ok_or_else(|| Error::DatabaseError("Database not found".to_string()))?;
		let metadata_turso_db = db_info.metadata_turso_db().cloned().ok_or_else(|| Error::DatabaseError("Metadata turso_db not found".to_string()))?;

		let metadata_conn = metadata_turso_db.connect()?;
		let mut rows = metadata_conn.query("SELECT earliest_measurement, latest_measurement FROM aspects WHERE id = ?", turso::params![aspect.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query aspect metadata: {e}")))?;

		let (earliest, latest) = match rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get row: {e}")))? {
			Some(r) => (
				match r.get::<Option<String>>(0) {
					Ok(Some(s)) => s.parse::<i64>().ok().and_then(DateTime::from_timestamp_millis),
					_ => None,
				},
				match r.get::<Option<String>>(1) {
					Ok(Some(s)) => s.parse::<i64>().ok().and_then(DateTime::from_timestamp_millis),
					_ => None,
				},
			),
			None => bail!(Error::DatabaseError("Aspect metadata not found".to_string())),
		};

		let new_time = measurement.timestamp();
		let new_earliest = earliest.map_or(Some(new_time), |curr| Some(curr.min(new_time)));
		let new_latest = latest.map_or(Some(new_time), |curr| Some(curr.max(new_time)));

		metadata_conn.execute("UPDATE aspects SET earliest_measurement = ?, latest_measurement = ? WHERE id = ?", turso::params![new_earliest.map(|dt| dt.timestamp_millis().to_string()), new_latest.map(|dt| dt.timestamp_millis().to_string()), aspect.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to update aspect metadata: {e}")))?;

		Ok(tx_id)
	}

	/// Batch insert measurements for better performance
	///
	/// # Errors
	/// - if aspect not found
	/// - if unable to begin transaction
	/// - if unable to insert measurement
	/// - if unable to commit transaction
	///
	/// # Panics
	/// - if measurements vector is empty when computing min/max (this is already checked)
	pub async fn observe_measurements_batch(&self, aspect: Aspect, measurements: Vec<InputMeasurement>) -> Result<Vec<TxId>> {
		if measurements.is_empty() {
			return Ok(Vec::new());
		}

		// Pre-generate all TxIds
		let all_tx_ids: Vec<TxId> = (0..measurements.len()).map(|_| TxId::new()).collect();

		// Compute min/max upfront
		let min_new = measurements.iter().map(InputMeasurement::timestamp).min().unwrap();
		let max_new = measurements.iter().map(InputMeasurement::timestamp).max().unwrap();

		// Get client and table name
		let f = DATABASES.lock().await.values().find_map(|db_info| db_info.subjects().values().find_map(|subject_info| subject_info.aspects().get(&aspect.id()).and_then(|aspect_info| subject_info.turso_db().map(|turso_db| (turso_db.clone(), aspect_info.table_name().to_string())))));
		let Some((turso_db, table_name)) = f else { bail!(Error::DatabaseError("Aspect not found".to_string())) };

		// Begin transaction
		let mut conn = turso_db.connect()?;
		let tx = conn.transaction().await.map_err(|e| Error::DatabaseError(format!("Failed to begin transaction: {e}")))?;

		// Batch inserts in chunks
		let mut tx_id_offset = 0;
		for chunk in measurements.chunks(CHUNK_SIZE) {
			for (i, measurement) in chunk.iter().enumerate() {
				let tx_id = &all_tx_ids[tx_id_offset + i];
				let insert_sql = format!("INSERT INTO {table_name} (id, timestamp, value) VALUES (?, ?, ?)");
				tx.execute(&insert_sql, turso::params![tx_id.as_uuid().to_string(), measurement.timestamp().timestamp_millis().to_string(), measurement.value().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to insert batch measurement: {e}")))?;
			}
			tx_id_offset += chunk.len();
		}

		// Commit transaction
		tx.commit().await.map_err(|e| Error::DatabaseError(format!("Failed to commit transaction: {e}")))?;

		// Invalidate cache
		let cache_key = format!("aspect_measurements_{}", aspect.id().as_uuid());
		CACHE.invalidate_aspect_cache(&cache_key, aspect.id().as_uuid()).await;

		// Update earliest and latest in metadata
		let db_info = self.get_database_info().await.ok_or_else(|| Error::DatabaseError("Database not found".to_string()))?;
		let metadata_turso_db = db_info.metadata_turso_db().cloned().ok_or_else(|| Error::DatabaseError("Metadata turso_db not found".to_string()))?;

		let metadata_conn = metadata_turso_db.connect()?;
		let mut rows = metadata_conn.query("SELECT earliest_measurement, latest_measurement FROM aspects WHERE id = ?", turso::params![aspect.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to query aspect metadata: {e}")))?;

		let (earliest, latest) = match rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get row: {e}")))? {
			Some(r) => (
				match r.get::<Option<String>>(0) {
					Ok(Some(s)) => s.parse::<i64>().ok().and_then(DateTime::from_timestamp_millis),
					_ => None,
				},
				match r.get::<Option<String>>(1) {
					Ok(Some(s)) => s.parse::<i64>().ok().and_then(DateTime::from_timestamp_millis),
					_ => None,
				},
			),
			None => bail!(Error::DatabaseError("Aspect metadata not found".to_string())),
		};

		let new_earliest = earliest.map_or(Some(min_new), |curr| Some(curr.min(min_new)));
		let new_latest = latest.map_or(Some(max_new), |curr| Some(curr.max(max_new)));

		metadata_conn.execute("UPDATE aspects SET earliest_measurement = ?, latest_measurement = ? WHERE id = ?", turso::params![new_earliest.map(|dt| dt.timestamp_millis().to_string()), new_latest.map(|dt| dt.timestamp_millis().to_string()), aspect.id().as_uuid().to_string()]).await.map_err(|e| Error::DatabaseError(format!("Failed to update aspect metadata: {e}")))?;

		Ok(all_tx_ids)
	}
}
