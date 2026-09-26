//! Pipeline persistence implementation for Database.

use anyhow::Result;
use chrono::Utc;

use crate::{
	database::traits::{
		aspect_structure::AspectStructure, connection::Connection as ConnectionTrait, database_structure::DatabaseStructure, pipeline::{PipelineInputs, PipelineOutputs}
	}, types::{
		aspect::Aspect, pipeline::{DetectorMetadata, DetectorType, PipelineConfig, PipelineState}
	}, AspectId, Database, Error
};

#[async_trait::async_trait]
impl PipelineInputs for Database {
	async fn save_pipeline_config(&self, aspect_id: &AspectId, config: &PipelineConfig) -> Result<()> {
		let db = self.get_pipeline_db(aspect_id).await?;
		let db_path = self.get_pipeline_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let now = Utc::now().timestamp_millis();
		let spline_str = serde_json::to_string(&config.spline_method).map_err(|e| Error::DatabaseError(format!("Failed to serialize spline method: {e}")))?;
		let batch_size_i64 = i64::try_from(config.batch_size).map_err(|e| Error::DatabaseError(format!("Batch size too large: {e}")))?;

		// Upsert: try update first, then insert if no rows affected
		let update_sql = r"
			UPDATE pipeline_config SET spline_method = ?, batch_size = ?, updated_at = ? WHERE id = 1
		";
		let rows = conn.as_ref().execute(update_sql, turso::params![spline_str.clone(), batch_size_i64, now]).await.map_err(|e| Error::DatabaseError(format!("Failed to update pipeline config: {e}")))?;

		if rows == 0 {
			let insert_sql = r"
				INSERT INTO pipeline_config (id, spline_method, batch_size, created_at, updated_at) VALUES (1, ?, ?, ?, ?)
			";
			conn.as_ref().execute(insert_sql, turso::params![spline_str, batch_size_i64, now, now]).await.map_err(|e| Error::DatabaseError(format!("Failed to insert pipeline config: {e}")))?;
		}

		let _ = Self::commit_concurrent(&conn).await;
		Ok(())
	}

	async fn save_pipeline_state(&self, aspect_id: &AspectId, state: &PipelineState) -> Result<()> {
		let db = self.get_pipeline_db(aspect_id).await?;
		let db_path = self.get_pipeline_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let last_run = state.last_run.map(|dt| dt.timestamp_millis());
		let run_count_i64 = i64::try_from(state.run_count).map_err(|e| Error::DatabaseError(format!("Run count too large: {e}")))?;
		let version_i64 = i64::from(state.version);

		// Upsert: try update first, then insert if no rows affected
		let update_sql = r"
			UPDATE pipeline_state SET last_run = ?, run_count = ?, version = ? WHERE id = 1
		";
		let rows = conn.as_ref().execute(update_sql, turso::params![last_run, run_count_i64, version_i64]).await.map_err(|e| Error::DatabaseError(format!("Failed to update pipeline state: {e}")))?;

		if rows == 0 {
			let insert_sql = r"
				INSERT INTO pipeline_state (id, last_run, run_count, version) VALUES (1, ?, ?, ?)
			";
			conn.as_ref().execute(insert_sql, turso::params![last_run, run_count_i64, version_i64]).await.map_err(|e| Error::DatabaseError(format!("Failed to insert pipeline state: {e}")))?;
		}

		let _ = Self::commit_concurrent(&conn).await;
		Ok(())
	}

	async fn delete_pipeline(&self, aspect_id: &AspectId) -> Result<()> {
		let db = self.get_pipeline_db(aspect_id).await?;
		let db_path = self.get_pipeline_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		// Delete all pipeline data
		conn.as_ref().execute("DELETE FROM pipeline_config WHERE id = 1", turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to delete pipeline config: {e}")))?;
		conn.as_ref().execute("DELETE FROM pipeline_state WHERE id = 1", turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to delete pipeline state: {e}")))?;
		conn.as_ref().execute("DELETE FROM pipeline_dictionaries", turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to delete pipeline dictionaries: {e}")))?;
		conn.as_ref().execute("DELETE FROM pipeline_detectors", turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to delete pipeline detectors: {e}")))?;

		let _ = Self::commit_concurrent(&conn).await;
		Ok(())
	}

	async fn add_pipeline_dictionary(&self, aspect_id: &AspectId, dictionary_name: &str) -> Result<()> {
		let db = self.get_pipeline_db(aspect_id).await?;
		let db_path = self.get_pipeline_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let now = Utc::now().timestamp_millis();

		// Check if already exists
		let check_sql = "SELECT 1 FROM pipeline_dictionaries WHERE dictionary_name = ? LIMIT 1";
		let mut rows = conn.as_ref().query(check_sql, turso::params![dictionary_name]).await.map_err(|e| Error::DatabaseError(format!("Failed to check dictionary: {e}")))?;

		if rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to read row: {e}")))?.is_none() {
			let insert_sql = "INSERT INTO pipeline_dictionaries (dictionary_name, added_at) VALUES (?, ?)";
			conn.as_ref().execute(insert_sql, turso::params![dictionary_name, now]).await.map_err(|e| Error::DatabaseError(format!("Failed to add dictionary: {e}")))?;
		}

		let _ = Self::commit_concurrent(&conn).await;
		Ok(())
	}

	async fn remove_pipeline_dictionary(&self, aspect_id: &AspectId, dictionary_name: &str) -> Result<()> {
		let db = self.get_pipeline_db(aspect_id).await?;
		let db_path = self.get_pipeline_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let sql = "DELETE FROM pipeline_dictionaries WHERE dictionary_name = ?";
		conn.as_ref().execute(sql, turso::params![dictionary_name]).await.map_err(|e| Error::DatabaseError(format!("Failed to remove dictionary: {e}")))?;

		let _ = Self::commit_concurrent(&conn).await;
		Ok(())
	}

	async fn clear_pipeline_dictionaries(&self, aspect_id: &AspectId) -> Result<()> {
		let db = self.get_pipeline_db(aspect_id).await?;
		let db_path = self.get_pipeline_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		conn.as_ref().execute("DELETE FROM pipeline_dictionaries", turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to clear dictionaries: {e}")))?;

		let _ = Self::commit_concurrent(&conn).await;
		Ok(())
	}

	async fn add_pipeline_detector(&self, aspect_id: &AspectId, metadata: &DetectorMetadata) -> Result<()> {
		let db = self.get_pipeline_db(aspect_id).await?;
		let db_path = self.get_pipeline_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let now = Utc::now().timestamp_millis();
		let detector_type_str = serde_json::to_string(metadata.detector_type()).map_err(|e| Error::DatabaseError(format!("Failed to serialize detector type: {e}")))?;

		// Delete existing then insert (simpler than upsert for text primary key in MVCC)
		conn.as_ref().execute("DELETE FROM pipeline_detectors WHERE detector_id = ?", turso::params![metadata.detector_id()]).await.map_err(|e| Error::DatabaseError(format!("Failed to delete detector: {e}")))?;

		let insert_sql = r"
			INSERT INTO pipeline_detectors (detector_id, detector_name, description, detector_type, config_json, added_at)
			VALUES (?, ?, ?, ?, ?, ?)
		";
		conn.as_ref().execute(insert_sql, turso::params![metadata.detector_id(), metadata.name(), metadata.description(), detector_type_str, metadata.config_json(), now]).await.map_err(|e| Error::DatabaseError(format!("Failed to insert detector: {e}")))?;

		let _ = Self::commit_concurrent(&conn).await;
		Ok(())
	}

	async fn remove_pipeline_detector(&self, aspect_id: &AspectId, detector_id: &str) -> Result<()> {
		let db = self.get_pipeline_db(aspect_id).await?;
		let db_path = self.get_pipeline_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		conn.as_ref().execute("DELETE FROM pipeline_detectors WHERE detector_id = ?", turso::params![detector_id]).await.map_err(|e| Error::DatabaseError(format!("Failed to remove detector: {e}")))?;

		let _ = Self::commit_concurrent(&conn).await;
		Ok(())
	}

	async fn clear_pipeline_detectors(&self, aspect_id: &AspectId) -> Result<()> {
		let db = self.get_pipeline_db(aspect_id).await?;
		let db_path = self.get_pipeline_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		conn.as_ref().execute("DELETE FROM pipeline_detectors", turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to clear detectors: {e}")))?;

		let _ = Self::commit_concurrent(&conn).await;
		Ok(())
	}
}

#[async_trait::async_trait]
impl PipelineOutputs for Database {
	async fn load_pipeline_config(&self, aspect_id: &AspectId) -> Result<Option<PipelineConfig>> {
		let db = self.get_pipeline_db(aspect_id).await?;
		let db_path = self.get_pipeline_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let sql = "SELECT spline_method, batch_size FROM pipeline_config WHERE id = 1";
		let mut rows = conn.as_ref().query(sql, turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to query pipeline config: {e}")))?;

		let result = if let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to read row: {e}")))? {
			let spline_str: String = row.get(0).map_err(|e| Error::DatabaseError(format!("Failed to get spline: {e}")))?;
			let batch_size: i64 = row.get(1).map_err(|e| Error::DatabaseError(format!("Failed to get batch_size: {e}")))?;

			let spline_method = serde_json::from_str(&spline_str).map_err(|e| Error::DatabaseError(format!("Failed to deserialize spline: {e}")))?;
			let batch_size = usize::try_from(batch_size).map_err(|e| Error::DatabaseError(format!("Invalid batch size: {e}")))?;

			Some(PipelineConfig::new(spline_method, batch_size))
		} else {
			None
		};

		let _ = Self::commit_concurrent(&conn).await;
		Ok(result)
	}

	async fn load_pipeline_state(&self, aspect_id: &AspectId) -> Result<Option<PipelineState>> {
		let db = self.get_pipeline_db(aspect_id).await?;
		let db_path = self.get_pipeline_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let sql = "SELECT last_run, run_count, version FROM pipeline_state WHERE id = 1";
		let mut rows = conn.as_ref().query(sql, turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to query pipeline state: {e}")))?;

		let result = if let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to read row: {e}")))? {
			let last_run_millis: Option<i64> = row.get(0).ok();
			let run_count: i64 = row.get(1).map_err(|e| Error::DatabaseError(format!("Failed to get run_count: {e}")))?;
			let version: i64 = row.get(2).map_err(|e| Error::DatabaseError(format!("Failed to get version: {e}")))?;

			let last_run = last_run_millis.and_then(chrono::DateTime::from_timestamp_millis);
			let run_count = u64::try_from(run_count).unwrap_or(0);
			let version = u32::try_from(version).unwrap_or(PipelineState::CURRENT_VERSION);

			Some(PipelineState { last_run, run_count, version })
		} else {
			None
		};

		let _ = Self::commit_concurrent(&conn).await;
		Ok(result)
	}

	async fn list_pipeline_dictionaries(&self, aspect_id: &AspectId) -> Result<Vec<String>> {
		let db = self.get_pipeline_db(aspect_id).await?;
		let db_path = self.get_pipeline_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let sql = "SELECT dictionary_name FROM pipeline_dictionaries ORDER BY added_at ASC";
		let mut rows = conn.as_ref().query(sql, turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to query dictionaries: {e}")))?;

		let mut names = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to read row: {e}")))? {
			let name: String = row.get(0).map_err(|e| Error::DatabaseError(format!("Failed to get name: {e}")))?;
			names.push(name);
		}

		let _ = Self::commit_concurrent(&conn).await;
		Ok(names)
	}

	async fn list_pipeline_detectors(&self, aspect_id: &AspectId) -> Result<Vec<DetectorMetadata>> {
		let db = self.get_pipeline_db(aspect_id).await?;
		let db_path = self.get_pipeline_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let sql = "SELECT detector_id, detector_name, description, detector_type, config_json FROM pipeline_detectors ORDER BY added_at ASC";
		let mut rows = conn.as_ref().query(sql, turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to query detectors: {e}")))?;

		let mut detectors = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to read row: {e}")))? {
			let id: String = row.get(0).map_err(|e| Error::DatabaseError(format!("Failed to get id: {e}")))?;
			let name: String = row.get(1).map_err(|e| Error::DatabaseError(format!("Failed to get name: {e}")))?;
			let description: Option<String> = row.get(2).ok();
			let detector_type_str: String = row.get(3).map_err(|e| Error::DatabaseError(format!("Failed to get type: {e}")))?;
			let config_json: Option<String> = row.get(4).ok();

			let detector_type: DetectorType = serde_json::from_str(&detector_type_str).unwrap_or(DetectorType::Custom);

			detectors.push(DetectorMetadata::new(&id, &name, description, detector_type, config_json));
		}

		let _ = Self::commit_concurrent(&conn).await;
		Ok(detectors)
	}

	async fn get_pipeline_detector(&self, aspect_id: &AspectId, detector_id: &str) -> Result<Option<DetectorMetadata>> {
		let db = self.get_pipeline_db(aspect_id).await?;
		let db_path = self.get_pipeline_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let sql = "SELECT detector_id, detector_name, description, detector_type, config_json FROM pipeline_detectors WHERE detector_id = ?";
		let mut rows = conn.as_ref().query(sql, turso::params![detector_id]).await.map_err(|e| Error::DatabaseError(format!("Failed to query detector: {e}")))?;

		let result = if let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to read row: {e}")))? {
			let id: String = row.get(0).map_err(|e| Error::DatabaseError(format!("Failed to get id: {e}")))?;
			let name: String = row.get(1).map_err(|e| Error::DatabaseError(format!("Failed to get name: {e}")))?;
			let description: Option<String> = row.get(2).ok();
			let detector_type_str: String = row.get(3).map_err(|e| Error::DatabaseError(format!("Failed to get type: {e}")))?;
			let config_json: Option<String> = row.get(4).ok();

			let detector_type: DetectorType = serde_json::from_str(&detector_type_str).unwrap_or(DetectorType::Custom);

			Some(DetectorMetadata::new(&id, &name, description, detector_type, config_json))
		} else {
			None
		};

		let _ = Self::commit_concurrent(&conn).await;
		Ok(result)
	}

	async fn is_pipeline_configured(&self, aspect_id: &AspectId) -> Result<bool> {
		let db = self.get_pipeline_db(aspect_id).await?;
		let db_path = self.get_pipeline_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;

		let sql = "SELECT 1 FROM pipeline_config WHERE id = 1 LIMIT 1";
		let mut rows = conn.as_ref().query(sql, turso::params![]).await.map_err(|e| Error::DatabaseError(format!("Failed to check config: {e}")))?;

		let exists = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to read row: {e}")))?.is_some();

		let _ = Self::commit_concurrent(&conn).await;
		Ok(exists)
	}
}

// Helper methods for Database
impl Database {
	/// Get the pipeline database for a specific aspect.
	///
	/// # Errors
	/// Returns an error if the aspect cannot be found or the pipeline database cannot be accessed.
	pub async fn get_pipeline_db(&self, aspect_id: &AspectId) -> Result<turso::Database> {
		let mut aspect: Aspect = self.get_aspect(aspect_id).await?;
		aspect.pipeline().await
	}

	/// Get the pipeline database path for a specific aspect.
	///
	/// # Errors
	/// Returns an error if the aspect cannot be found.
	pub async fn get_pipeline_db_path(&self, aspect_id: &AspectId) -> Result<String> {
		let aspect: Aspect = self.get_aspect(aspect_id).await?;
		Ok(aspect.pipeline_path())
	}
}
