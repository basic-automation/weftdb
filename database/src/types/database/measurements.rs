use anyhow::{bail, Result};

use crate::{Aspect, Database, Error, InputMeasurement, TxId, CACHE, DATABASES};

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

		// Insert measurement into database
		let insert_sql = format!("INSERT INTO {table_name} (id, timestamp, value) VALUES (?, ?, ?)");

		match sqlx::query(&insert_sql).bind(tx_id.as_uuid().to_string()).bind(measurement.timestamp().timestamp_millis()).bind(measurement.value().to_string()).execute(&pool).await {
			Ok(_) => (),
			Err(e) => bail!(Error::DatabaseError(format!("Failed to insert measurement: {e}"))),
		}

		// Invalidate cache for this aspect
		let cache_key = format!("aspect_measurements_{}", aspect.id().as_uuid());
		CACHE.invalidate_aspect_cache(&cache_key, aspect.id().as_uuid()).await;

		Ok(tx_id)
	}

	/// Batch insert measurements for better performance
	///
	/// # Errors
	/// - if aspect not found
	/// - if unable to begin transaction
	/// - if unable to insert measurement
	/// - if unable to commit transaction
	pub async fn observe_measurements_batch(&self, aspect: Aspect, measurements: Vec<InputMeasurement>) -> Result<Vec<TxId>> {
		if measurements.is_empty() {
			return Ok(Vec::new());
		}

		let mut tx_ids = Vec::with_capacity(measurements.len());

		// Get pool and table name
		let f = DATABASES.lock().await.values().find_map(|db_info| db_info.subjects().values().find_map(|subject_info| subject_info.aspects().get(&aspect.id()).map(|aspect_info| (subject_info.pool(), aspect_info)))).map(|(pool, aspect_info)| (pool.clone(), aspect_info.table_name().to_string()));
		let Some((pool, table_name)) = f else { bail!(Error::DatabaseError("Aspect not found".to_string())) };

		// Begin transaction for batch insert
		let mut tx = pool.begin().await.map_err(|e| Error::DatabaseError(format!("Failed to begin transaction: {e}")))?;

		// Prepare the insert statement once
		let insert_sql = format!("INSERT INTO {table_name} (id, timestamp, value) VALUES (?, ?, ?)");

		for measurement in measurements {
			let tx_id = TxId::new();
			tx_ids.push(tx_id);

			sqlx::query(&insert_sql).bind(tx_id.as_uuid().to_string()).bind(measurement.timestamp().timestamp_millis()).bind(measurement.value().to_string()).execute(&mut *tx).await.map_err(|e| Error::DatabaseError(format!("Failed to insert measurement: {e}")))?;
		}

		// Commit transaction
		tx.commit().await.map_err(|e| Error::DatabaseError(format!("Failed to commit transaction: {e}")))?;

		// Invalidate cache once for the entire batch
		let cache_key = format!("aspect_measurements_{}", aspect.id().as_uuid());
		CACHE.invalidate_aspect_cache(&cache_key, aspect.id().as_uuid()).await;

		Ok(tx_ids)
	}
}
