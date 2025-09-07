use anyhow::{bail, Result};
use chrono::DateTime;
use sqlx::Row;

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

		// Get pool and table name with proper scope management - extract immediately
		let f = DATABASES.lock().await.values().find_map(|db_info| db_info.subjects().values().find_map(|subject_info| subject_info.aspects().get(&aspect.id()).map(|aspect_info| (subject_info.pool(), aspect_info)))).map(|(pool, aspect_info)| (pool.clone(), aspect_info.table_name().to_string()));
		let Some((pool, table_name)) = f else { bail!(Error::DatabaseError("Aspect not found".to_string())) };

		// Use dedicated write pool for inserts to avoid concurrency issues
		let connect_options = pool.connect_options().clone();
		let filename = <sqlx::sqlite::SqliteConnectOptions as Clone>::clone(&connect_options).get_filename();
		let write_pool = Self::get_or_create_write_pool(&filename.to_string_lossy()).await?;

		// Insert measurement into database
		let insert_sql = format!("INSERT INTO {table_name} (id, timestamp, value) VALUES (?, ?, ?)");
		match sqlx::query(&insert_sql).bind(tx_id.as_uuid().to_string()).bind(measurement.timestamp().timestamp_millis()).bind(measurement.value().to_string()).execute(&write_pool).await {
			Ok(_) => (),
			Err(e) => bail!(Error::DatabaseError(format!("Failed to insert measurement: {e}"))),
		}

		// Invalidate cache for this aspect
		let cache_key = format!("aspect_measurements_{}", aspect.id().as_uuid());
		CACHE.invalidate_aspect_cache(&cache_key, aspect.id().as_uuid()).await;

		// Update earliest and latest in metadata
		let db_info = self.get_database_info().await.ok_or_else(|| Error::DatabaseError("Database not found".to_string()))?;
		let metadata_pool = db_info.metadata_pool().cloned().ok_or_else(|| Error::DatabaseError("Metadata pool not found".to_string()))?;

		let row = sqlx::query("SELECT earliest_measurement, latest_measurement FROM aspects WHERE id = ?").bind(aspect.id().as_uuid()).fetch_optional(&metadata_pool).await.map_err(|e| Error::DatabaseError(format!("Failed to query aspect metadata: {e}")))?;

		let (earliest, latest) = match row {
			Some(r) => (r.get::<Option<i64>, _>("earliest_measurement").and_then(DateTime::from_timestamp_millis), r.get::<Option<i64>, _>("latest_measurement").and_then(DateTime::from_timestamp_millis)),
			None => bail!(Error::DatabaseError("Aspect metadata not found".to_string())),
		};

		let new_time = measurement.timestamp();
		let new_earliest = earliest.map_or(Some(new_time), |curr| Some(curr.min(new_time)));
		let new_latest = latest.map_or(Some(new_time), |curr| Some(curr.max(new_time)));

		sqlx::query("UPDATE aspects SET earliest_measurement = ?, latest_measurement = ? WHERE id = ?").bind(new_earliest.map(|dt| dt.timestamp_millis())).bind(new_latest.map(|dt| dt.timestamp_millis())).bind(aspect.id().as_uuid()).execute(&metadata_pool).await.map_err(|e| Error::DatabaseError(format!("Failed to update aspect metadata: {e}")))?;

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

		// Pre-generate all TxIds to avoid allocation issues in closures
		let all_tx_ids: Vec<TxId> = (0..measurements.len()).map(|_| TxId::new()).collect();

		// Compute min/max upfront (outside transaction)
		let min_new = measurements.iter().map(InputMeasurement::timestamp).min().unwrap();
		let max_new = measurements.iter().map(InputMeasurement::timestamp).max().unwrap();

		// Get pool and table name
		let f = DATABASES.lock().await.values().find_map(|db_info| db_info.subjects().values().find_map(|subject_info| subject_info.aspects().get(&aspect.id()).map(|aspect_info| (subject_info.pool(), aspect_info)))).map(|(pool, aspect_info)| (pool.clone(), aspect_info.table_name().to_string()));
		let Some((pool, table_name)) = f else { bail!(Error::DatabaseError("Aspect not found".to_string())) };

		// Use dedicated write pool for batch inserts
		let connect_options = pool.connect_options().clone();
		let filename = <sqlx::sqlite::SqliteConnectOptions as Clone>::clone(&connect_options).get_filename();
		let write_pool = Self::get_or_create_write_pool(&filename.to_string_lossy()).await?;

		// Begin transaction
		let mut tx = write_pool.begin().await.map_err(|e| Error::DatabaseError(format!("Failed to begin transaction: {e}")))?;

		// Optional: Tune for max speed (WARNING: risks data loss on crash)
		// sqlx::query("PRAGMA synchronous = OFF").execute(&mut *tx).await?;

		// Batch inserts in chunks to avoid param limits
		let mut tx_id_offset = 0;
		for chunk in measurements.chunks(CHUNK_SIZE) {
			let mut query_builder: sqlx::QueryBuilder<sqlx::Sqlite> = sqlx::QueryBuilder::new(format!("INSERT INTO {table_name} (id, timestamp, value) "));

			// Build values manually to avoid closure capture
			query_builder.push("VALUES ");
			for (i, measurement) in chunk.iter().enumerate() {
				if i > 0 {
					query_builder.push(", ");
				}
				let tx_id = all_tx_ids[tx_id_offset + i];
				query_builder.push("(").push_bind(tx_id.as_uuid().to_string()).push(", ").push_bind(measurement.timestamp().timestamp_millis()).push(", ").push_bind(measurement.value().to_string()).push(")");
			}

			tx_id_offset += chunk.len();

			let query = query_builder.build();
			query.execute(&mut *tx).await.map_err(|e| Error::DatabaseError(format!("Failed to insert batch: {e}")))?;
		}

		// Commit transaction
		tx.commit().await.map_err(|e| Error::DatabaseError(format!("Failed to commit transaction: {e}")))?;

		// Invalidate cache once for the entire batch
		let cache_key = format!("aspect_measurements_{}", aspect.id().as_uuid());
		CACHE.invalidate_aspect_cache(&cache_key, aspect.id().as_uuid()).await;

		// Update earliest and latest in metadata
		let db_info = self.get_database_info().await.ok_or_else(|| Error::DatabaseError("Database not found".to_string()))?;
		let metadata_pool = db_info.metadata_pool().cloned().ok_or_else(|| Error::DatabaseError("Metadata pool not found".to_string()))?;

		let row = sqlx::query("SELECT earliest_measurement, latest_measurement FROM aspects WHERE id = ?").bind(aspect.id().as_uuid()).fetch_optional(&metadata_pool).await.map_err(|e| Error::DatabaseError(format!("Failed to query aspect metadata: {e}")))?;

		let (earliest, latest) = match row {
			Some(r) => (r.get::<Option<i64>, _>("earliest_measurement").and_then(DateTime::from_timestamp_millis), r.get::<Option<i64>, _>("latest_measurement").and_then(DateTime::from_timestamp_millis)),
			None => bail!(Error::DatabaseError("Aspect metadata not found".to_string())),
		};

		let new_earliest = earliest.map_or(Some(min_new), |curr| Some(curr.min(min_new)));
		let new_latest = latest.map_or(Some(max_new), |curr| Some(curr.max(max_new)));

		sqlx::query("UPDATE aspects SET earliest_measurement = ?, latest_measurement = ? WHERE id = ?").bind(new_earliest.map(|dt| dt.timestamp_millis())).bind(new_latest.map(|dt| dt.timestamp_millis())).bind(aspect.id().as_uuid()).execute(&metadata_pool).await.map_err(|e| Error::DatabaseError(format!("Failed to update aspect metadata: {e}")))?;

		Ok(all_tx_ids)
	}
}
