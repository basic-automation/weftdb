use std::{path::Path, str::FromStr};

use anyhow::{bail, Result};
use chrono::DateTime;
use splimes::Point;
use sqlx::{Pool, Row, Sqlite};
use uuid::Uuid;

use crate::{Aspect, AspectId, Database, DatabaseInfo, Error, Measurement, Subject, SubjectId, CACHE, DATABASES};

impl Database {
	pub(crate) fn sanitize_table_name(name: &str) -> String {
		name.chars().map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' }).collect()
	}

	pub(crate) async fn load_subjects_from_metadata(db_info: &mut DatabaseInfo, db_path: &str) -> Result<()> {
		// Early return if no metadata pool
		let metadata_pool = match db_info.metadata_pool() {
			Some(pool) => pool.clone(),
			None => return Err(anyhow::anyhow!("No metadata pool available for database '{}'", db_info.name()))?,
		};

		// Load subjects from metadata - using UUID directly
		let subject_rows = match sqlx::query("SELECT id, name FROM subject_metadata ORDER BY created_at").fetch_all(&metadata_pool).await {
			Ok(rows) => rows,
			Err(e) => bail!(Error::DatabaseError(format!("Failed to query subject metadata: {e}"))),
		};

		for subject_row in subject_rows {
			let subject_uuid: Uuid = subject_row.get("id");
			let subject_name: String = subject_row.get("name");

			let subject_id = SubjectId::from_uuid(subject_uuid);

			// Create subject connection
			let subject_db_path = format!("{db_path}/{subject_name}.db");
			if Path::new(&subject_db_path).exists() {
				let pool = match sqlx::sqlite::SqlitePoolOptions::new().max_connections(5).connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&subject_db_path).create_if_missing(false)).await {
					Ok(pool) => pool,
					Err(e) => bail!(Error::DatabaseError(format!("Failed to connect to subject database: {e}"))),
				};

				let mut subject_info = Subject::new_with_id(subject_id, subject_name, db_info.id(), pool);

				// Load aspects for this subject
				Self::load_aspects_from_metadata(&mut subject_info, &metadata_pool, subject_id).await?;

				db_info.add_subject(subject_info);
			}
		}

		Ok(())
	}

	pub(crate) async fn load_aspects_from_metadata(subject_info: &mut Subject, metadata_pool: &Pool<Sqlite>, subject_id: SubjectId) -> Result<()> {
		// Load aspects from metadata - using UUID directly
		let aspect_rows = match sqlx::query("SELECT id, name, table_name FROM aspect_metadata WHERE subject_id = ? ORDER BY created_at").bind(subject_id.as_uuid()).fetch_all(metadata_pool).await {
			Ok(rows) => rows,
			Err(e) => bail!(Error::DatabaseError(format!("Failed to query aspect metadata: {e}"))),
		};

		for aspect_row in aspect_rows {
			let aspect_uuid: Uuid = aspect_row.get("id");
			let aspect_name: String = aspect_row.get("name");
			let table_name: String = aspect_row.get("table_name");

			let aspect_id = AspectId::from_uuid(aspect_uuid);
			let aspect = Aspect::new_with_id(aspect_id, aspect_name, subject_id, table_name);
			subject_info.add_aspect(aspect);
		}

		Ok(())
	}

	pub(crate) async fn get_aspect_measurements(aspect: AspectId) -> Result<Vec<Measurement>> {
		// Check cache first
		let cache_key = format!("aspect_measurements_{}", aspect.as_uuid());
		if let Some(cached) = CACHE.get_aspect_measurements(&cache_key, aspect.as_uuid()).await {
			return Ok(cached);
		}

		// Get from database with proper scope management - extract immediately
		let v = DATABASES.lock().await.clone();
		let p = v.values().find_map(|db_info| db_info.subjects().values().find_map(|subject_info| subject_info.aspects().get(&aspect).map(|aspect_info| (subject_info.pool(), aspect_info)))).map(|(pool, aspect_info)| (pool.clone(), aspect_info.table_name().to_string(), aspect.as_uuid()));
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
				let Ok(id) = Uuid::parse_str(&id_str) else { return Err(Error::InvalidIdError(format!("Invalid UUID format for measurement ID: {id_str}"))) };

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

	#[must_use]
	pub fn measurements_to_points(measurements: &[Measurement]) -> Vec<Point> {
		measurements.iter().map(|m| Point { timestamp: m.timestamp, value: m.value.clone() }).collect()
	}
}
