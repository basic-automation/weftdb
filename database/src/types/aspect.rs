use std::{collections::HashMap, fmt::Display, str::FromStr};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use splimes::Resolution;
use turso::Database as TursoDatabase;
use uuid::Uuid;
use tracing::{debug, info, instrument, trace};

use crate::{
	cache::Connection, types::{
		database::{
			traits::{aspect_structure::AspectStructure, connection::Connection as ConnectionTrait}, Config
		}, dictionary::VariablilityType
	}, Database, DatabaseStructure, DictionaryConstraints, DictionaryId, SubjectId
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AspectId(Uuid);

impl AspectId {
	#[must_use]
	pub fn new() -> Self {
		Self(Uuid::new_v4())
	}

	#[must_use]
	pub const fn from_uuid(uuid: Uuid) -> Self {
		Self(uuid)
	}

	#[must_use]
	pub const fn as_uuid(&self) -> Uuid {
		self.0
	}
}

impl Default for AspectId {
	fn default() -> Self {
		Self::new()
	}
}

impl Display for AspectId {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

impl FromStr for AspectId {
	type Err = uuid::Error;

	fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
		Uuid::parse_str(s).map(Self)
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Aspect {
	id: AspectId,
	name: String,
	subject_id: SubjectId,
	resolution: Resolution,
	database_metadata_db_path: String,
	subject_name: String,
	path: String,

	#[serde(skip)]
	measurements: Option<TursoDatabase>,
	measurements_path: String,

	#[serde(skip)]
	unprocessed_batches: Option<TursoDatabase>,
	unprocessed_batches_path: String,

	#[serde(skip)]
	processed_batches: Option<TursoDatabase>,
	processed_batches_path: String,

	#[serde(skip)]
	patterns: Option<TursoDatabase>,
	patterns_path: String,

	#[serde(skip)]
	unprocessed_events: Option<TursoDatabase>,
	unprocessed_events_path: String,

	#[serde(skip)]
	processed_events: Option<TursoDatabase>,
	processed_events_path: String,

	#[serde(skip)]
	correlations: Option<TursoDatabase>,
	correlations_path: String,

	#[serde(skip)]
	dictionaries: Option<HashMap<String, TursoDatabase>>,
	dictionaries_paths: HashMap<String, String>,
}

#[async_trait::async_trait]
impl AspectStructure for Aspect {
	#[allow(clippy::too_many_lines)]
	#[instrument(skip(metadata_conn), fields(aspect_name = %name, subject_id = %subject_id))]
	async fn new(id: Option<AspectId>, name: &str, subject_id: &SubjectId, resolution: &Resolution, metadata_conn: &Connection) -> Result<Self> {
		let id = id.unwrap_or_default();
		let subject_name = Self::get_subject_name(metadata_conn, subject_id).await?;
		let db_name = <Database as Config>::db_name(metadata_conn).await?;
		let database_metadata_db_path = Database::db_metadata_path(metadata_conn).await?;
		let aspect_path = Database::aspect_path(&db_name, &subject_name, name);

		// recursively create directory if it doesn't exist
		tokio::fs::create_dir_all(&aspect_path).await?;

		info!(aspect_id = %id, path = %aspect_path, "Initializing aspect databases");

		// Create databases sequentially to avoid lock contention during concurrent aspect creation
		// For DDL operations (CREATE TABLE), use direct connection without BEGIN CONCURRENT
		// DDL operations are not compatible with MVCC concurrent transactions
		let measurements_path = Database::aspect_measurements_db_path(&db_name, &subject_name, name);
		trace!("Creating measurements DB at: {}", measurements_path);
		let measurements = Database::get_or_create_turso_database(&measurements_path).await?;
		trace!("Connected measurements DB: {}", measurements_path);
		// Use immediate transaction for DDL (table creation) - not compatible with BEGIN CONCURRENT
		let schema_conn = Database::begin_immediate(&measurements).await?;
		trace!("Wireframing measurements tables for: {}", measurements_path);
		Self::wireframe_measurements_tables_direct(&schema_conn).await?;
                let _ = Database::commit_immediate(&schema_conn).await;
		drop(schema_conn);
		trace!("Wireframed measurements tables for: {}", measurements_path);
		let measurements = Some(measurements);
		debug!("Aspect DB initialized: measurements");

		let unprocessed_batches_path = Database::aspect_unprocessed_batches_db_path(&db_name, &subject_name, name);
		trace!("Creating unprocessed_batches DB at: {}", unprocessed_batches_path);
		let unprocessed_batches = Database::get_or_create_turso_database(&unprocessed_batches_path).await?;

		trace!("Connected unprocessed_batches DB: {}", unprocessed_batches_path);
		let schema_conn = Database::begin_immediate(&unprocessed_batches).await?;
		trace!("Wireframing batches tables for: {}", unprocessed_batches_path);
		Self::wireframe_batches_tables_direct(&schema_conn).await?;
                let _ = Database::commit_immediate(&schema_conn).await;
		drop(schema_conn);
		trace!("Wireframed batches tables for: {}", unprocessed_batches_path);
		let unprocessed_batches = Some(unprocessed_batches);
		debug!("Aspect DB initialized: unprocessed_batches");

		let processed_batches_path = Database::aspect_processed_batches_db_path(&db_name, &subject_name, name);
		trace!("Creating processed_batches DB at: {}", processed_batches_path);
		let processed_batches = Database::get_or_create_turso_database(&processed_batches_path).await?;
		trace!("Connected processed_batches DB: {}", processed_batches_path);
		let schema_conn = Database::begin_immediate(&processed_batches).await?;
		trace!("Wireframing batches tables for: {}", processed_batches_path);
		Self::wireframe_batches_tables_direct(&schema_conn).await?;
                let _ = Database::commit_immediate(&schema_conn).await;
		drop(schema_conn);
		trace!("Wireframed batches tables for: {}", processed_batches_path);
		let processed_batches = Some(processed_batches);
		debug!("Aspect DB initialized: processed_batches");

		let patterns_path = Database::aspect_patterns_db_path(&db_name, &subject_name, name);
		trace!("Creating patterns DB at: {}", patterns_path);
		let patterns = Database::get_or_create_turso_database(&patterns_path).await?;
		trace!("Connected patterns DB: {}", patterns_path);
		let schema_conn = Database::begin_immediate(&patterns).await?;
		trace!("Wireframing patterns tables for: {}", patterns_path);
		Self::wireframe_patterns_tables_direct(&schema_conn).await?;
                let _ = Database::commit_immediate(&schema_conn).await;
		drop(schema_conn);
		trace!("Wireframed patterns tables for: {}", patterns_path);
		let patterns = Some(patterns);
		debug!("Aspect DB initialized: patterns");

		let unprocessed_events_path = Database::aspect_unprocessed_events_db_path(&db_name, &subject_name, name);
		trace!("Creating unprocessed_events DB at: {}", unprocessed_events_path);
		let unprocessed_events = Database::get_or_create_turso_database(&unprocessed_events_path).await?;
		trace!("Connected unprocessed_events DB: {}", unprocessed_events_path);
		let schema_conn = Database::begin_immediate(&unprocessed_events).await?;
		trace!("Wireframing unprocessed_events tables for: {}", unprocessed_events_path);
		Self::wireframe_events_tables_direct(&schema_conn).await?;
                let _ = Database::commit_immediate(&schema_conn).await;
		drop(schema_conn);
		trace!("Wireframed unprocessed_events tables for: {}", unprocessed_events_path);
		let unprocessed_events = Some(unprocessed_events);
		debug!("Aspect DB initialized: unprocessed_events");

		let processed_events_path = Database::aspect_processed_events_db_path(&db_name, &subject_name, name);
		trace!("Creating processed_events DB at: {}", processed_events_path);
		let processed_events = Database::get_or_create_turso_database(&processed_events_path).await?;
		trace!("Connected processed_events DB: {}", processed_events_path);
		let schema_conn = Database::begin_immediate(&processed_events).await?;
		trace!("Wireframing processed_events tables for: {}", processed_events_path);
		Self::wireframe_events_tables_direct(&schema_conn).await?;
                let _ = Database::commit_immediate(&schema_conn).await;
		drop(schema_conn);
		trace!("Wireframed processed_events tables for: {}", processed_events_path);
		let processed_events = Some(processed_events);
		debug!("Aspect DB initialized: processed_events");

		let correlations_path = Database::aspect_correlations_db_path(&db_name, &subject_name, name);
		trace!("Creating correlations DB at: {}", correlations_path);
		let correlations = Database::get_or_create_turso_database(&correlations_path).await?;
		trace!("Connected correlations DB: {}", correlations_path);
		let schema_conn = Database::begin_immediate(&correlations).await?;
		trace!("Wireframing correlations tables for: {}", correlations_path);
		Self::wireframe_correlations_tables_direct(&schema_conn).await?;
                let _ = Database::commit_immediate(&schema_conn).await;
		drop(schema_conn);
		trace!("Wireframed correlations tables for: {}", correlations_path);
		let correlations = Some(correlations);
		debug!("Aspect DB initialized: correlations");

		let dictionaries_path = <Database as Config>::aspect_dictionaries_path(&db_name, &subject_name, name);
		trace!("Creating dictionaries directory at: {}", dictionaries_path);

		std::fs::create_dir_all(&dictionaries_path)?;
		debug!("Aspect DB initialized: dictionaries");

		info!(aspect_id = %id, "Aspect initialization complete");

		#[rustfmt::skip]
		Ok(Self {
			id,
			name: name.to_string(),
			subject_id: *subject_id,
			resolution: *resolution,
			database_metadata_db_path,
			subject_name,
			path: aspect_path,
			measurements,
			measurements_path,
			unprocessed_batches,
			unprocessed_batches_path,
			processed_batches,
			processed_batches_path,
			patterns,
			patterns_path,
			unprocessed_events,
			unprocessed_events_path,
			processed_events,
			processed_events_path,
			correlations,
			correlations_path,
                        dictionaries: None,
                        dictionaries_paths: HashMap::new(),
		})
	}

	/// Lightweight constructor used when we only need the Aspect metadata
	/// without opening or wireframing per-aspect databases. This avoids
	/// holding metadata DB locks during expensive IO when simply listing
	/// aspects or performing existence checks.
	#[instrument]
	async fn from_metadata(id: Option<AspectId>, name: String, subject_id: &SubjectId, resolution: &Resolution, database_metadata_db_path: String, subject_name_opt: Option<String>) -> Result<Self> {
		let provided_subject_name = if let Some(sn) = subject_name_opt {
			sn
		} else {
			let database_metadata_db = Database::get_turso_database(&database_metadata_db_path).await?;
			let conn = Database::begin_concurrent(&database_metadata_db, &database_metadata_db_path, None).await?;
			let s = Self::get_subject_name(&conn, subject_id).await?;
			let _ = Database::commit_concurrent(&conn).await;
			s
		};

		let database_dir = std::path::Path::new(&database_metadata_db_path).parent().ok_or_else(|| anyhow::anyhow!("Cannot determine database directory"))?;

		let aspect_path = database_dir.join(&provided_subject_name).join(&name);
		let aspect_path_str = aspect_path.to_string_lossy().to_string();

		let measurements_path = aspect_path_str.clone() + "/measurements.db";
		let unprocessed_batches_path = aspect_path_str.clone() + "/unprocessed_batches.db";
		let processed_batches_path = aspect_path_str.clone() + "/processed_batches.db";
		let patterns_path = aspect_path_str.clone() + "/patterns.db";
		let unprocessed_events_path = aspect_path_str.clone() + "/unprocessed_events.db";
		let processed_events_path = aspect_path_str.clone() + "/processed_events.db";
		let correlations_path = aspect_path_str.clone() + "/correlations.db";
		let dictionaries_path = aspect_path_str.clone() + "/dictionaries";

		let mut dictionaries_paths = HashMap::new();
		// Find all dictionary database paths in the dictionaries directory
		if let Ok(entries) = tokio::fs::read_dir(&dictionaries_path).await {
			let mut dir_entries = entries;
			while let Ok(Some(entry)) = dir_entries.next_entry().await {
				let path = entry.path();
				if path.is_file() {
					if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
						if std::path::Path::new(file_name)
							.extension()
							.is_some_and(|ext| ext.eq_ignore_ascii_case("db"))
						{
							let dict_name = file_name.trim_end_matches(".db").to_string();
							dictionaries_paths.insert(dict_name, path.to_string_lossy().to_string());
						}
					}
				}
			}
		}

		#[rustfmt::skip]
		Ok(Self {
                        id: id.unwrap_or_default(),
                        name: name.clone(),
                        subject_id: *subject_id,
                        resolution: *resolution,
                        database_metadata_db_path: database_metadata_db_path.clone(),
                        subject_name: provided_subject_name,
                        path: aspect_path_str,
                        measurements: None,
                        unprocessed_batches: None,
                        processed_batches: None,
                        patterns: None,
                        unprocessed_events: None,
                        processed_events: None,
                        correlations: None,
                        measurements_path,
                        unprocessed_batches_path,
                        processed_batches_path,
                        patterns_path,
                        unprocessed_events_path,
                        processed_events_path,
                        correlations_path,
                        dictionaries: None,
                        dictionaries_paths,
                })
	}

	fn id(&self) -> AspectId {
		self.id
	}

	fn name(&self) -> &str {
		&self.name
	}

	fn subject_id(&self) -> SubjectId {
		self.subject_id
	}

	fn resolution(&self) -> Resolution {
		self.resolution
	}

	fn subject_name(&self) -> &str {
		&self.subject_name
	}

	fn aspect_path(&self) -> &str {
		&self.path
	}

	#[instrument]
	async fn database_metadata(&self) -> Result<TursoDatabase> {
		let turso_db = Database::get_turso_database(&self.database_metadata_db_path).await.unwrap();
		Ok(turso_db)
	}

	#[instrument]
	async fn get_database_metadata_path(turso_db_path: String) -> Result<String> {
		let turso_db = Database::get_turso_database(&turso_db_path).await?;

		// Query the database for the metadata_path field of the first item in the database table
		let conn = Database::begin_concurrent(&turso_db, &turso_db_path, None).await?;
		let mut rows = conn.as_ref().query("SELECT metadata_path FROM database", turso::params![]).await?;
		let row = rows.next().await?.ok_or_else(|| anyhow::anyhow!("Database metadata not found"))?;
		let metadata_path: String = row.get(0)?;
		let _ = Database::commit_concurrent(&conn).await;

		Ok(metadata_path)
	}

	#[instrument]
	async fn get_subject_name(conn: &Connection, subject_id: &SubjectId) -> Result<String> {
		// Read-only query with simple retry/backoff; no explicit transaction to avoid writer locks
		let res = conn.as_ref().query("SELECT name FROM subjects WHERE id = ?", turso::params![subject_id.as_uuid().to_string()]).await;
		let subject_name = match res {
			Ok(mut rows) => {
				let row = rows.next().await?.ok_or_else(|| anyhow::anyhow!("Subject not found"))?;
				let subject_name: String = row.get(0)?;
				subject_name
			}
			Err(e) => bail!("SQL execution failure 3: in get_subject_name: `{e}`"),
		};
		Ok(subject_name)
	}

	#[instrument]
	async fn get_aspect_path(conn: &Connection, metadata_path: &str, subject_id: &SubjectId, aspect_name: &str) -> Result<String> {
		let subject_name = Self::get_subject_name(conn, subject_id).await?;
		let aspect_metadata_path = std::path::Path::new(&metadata_path).parent().ok_or_else(|| anyhow::anyhow!("Cannot determine database directory"))?.join(subject_name).join(aspect_name);
		if aspect_metadata_path.exists() {
			Ok(aspect_metadata_path.to_string_lossy().to_string())
		} else {
			Err(anyhow::anyhow!("Aspect metadata path does not exist"))
		}
	}

	#[instrument]
	async fn measurements(&mut self) -> Result<TursoDatabase> {
		if let Some(ref db) = self.measurements {
			return Ok(db.clone());
		}

		// Lazily initialize the measurements database
		// Use immediate transaction for DDL (not compatible with BEGIN CONCURRENT)
		let measurements = Database::get_or_create_turso_database(&self.measurements_path).await?;
		let schema_conn = Database::begin_immediate(&measurements).await?;
		Self::wireframe_measurements_tables_direct(&schema_conn).await?;
                let _ = Database::commit_immediate(&schema_conn).await;
		drop(schema_conn);

		self.measurements = Some(measurements.clone());
		Ok(measurements)
	}

	fn set_measurements(&mut self, turso_db: TursoDatabase) {
		self.measurements = Some(turso_db);
	}

	fn measurements_path(&self) -> String {
		self.measurements_path.clone()
	}

	#[instrument]
	async fn set_measurements_path(&mut self, path: String) {
		self.measurements_path = path;
	}

	#[instrument]
	async fn wireframe_measurements_tables(conn: &Connection) -> Result<()> {
		// Create measurements table
		conn.as_ref()
			.execute(
				"CREATE TABLE IF NOT EXISTS measurements (
				id TEXT NOT NULL,
				dataset_id TEXT NOT NULL,
				timestamp INTEGER NOT NULL,
				value TEXT NOT NULL
			)",
				turso::params![],
			)
			.await?;

		// Try to create index on timestamp for efficient range queries
		// This may fail on older turso versions with MVCC, but cursor-based pagination works without it
		if let Err(e) = conn.as_ref()
			.execute(
				"CREATE INDEX IF NOT EXISTS idx_measurements_timestamp ON measurements(timestamp)",
				turso::params![],
			)
			.await
		{
			tracing::warn!("Could not create timestamp index (MVCC limitation): {e}");
		}

		Ok(())
	}

	#[instrument]
	async fn unprocessed_batches(&mut self) -> Result<TursoDatabase> {
		if let Some(ref db) = self.unprocessed_batches {
			return Ok(db.clone());
		}

		// Lazily initialize the unprocessed_batches database
		// Use immediate transaction for DDL (not compatible with BEGIN CONCURRENT)
		let unprocessed_batches = Database::get_or_create_turso_database(&self.unprocessed_batches_path).await?;
		let schema_conn = Database::begin_immediate(&unprocessed_batches).await?;
		Self::wireframe_batches_tables_direct(&schema_conn).await?;
                let _ = Database::commit_immediate(&schema_conn).await;
		drop(schema_conn);

		self.unprocessed_batches = Some(unprocessed_batches.clone());
		Ok(unprocessed_batches)
	}

	fn set_unprocessed_batches(&mut self, turso_db: TursoDatabase) {
		self.unprocessed_batches = Some(turso_db);
	}

	fn unprocessed_batches_path(&self) -> String {
		self.unprocessed_batches_path.clone()
	}

	#[instrument]
	async fn wireframe_batches_tables(conn: &Connection) -> Result<()> {
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS batches (
				id TEXT NOT NULL,
				aspect_id TEXT NOT NULL,
				database_id TEXT NOT NULL,
				size INTEGER NOT NULL,
				resolution TEXT NOT NULL,
				measurements TEXT NOT NULL,
				batch_hash TEXT,
				status TEXT NOT NULL DEFAULT 'pending',
				created_at INTEGER NOT NULL,
				updated_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000)
			)",
				turso::params![],
			)
			.await?;

		// Note: No indexes to support MVCC (turso MVCC doesn't support indexes yet)
		// Duplicate batch detection must be handled at application level

		let _ = Database::commit_concurrent(conn).await;

		Ok(())
	}

	#[instrument]
	async fn set_unprocessed_batches_path(&mut self, path: String) {
		self.unprocessed_batches_path = path;
	}

	#[instrument]
	async fn processed_batches(&mut self) -> Result<TursoDatabase> {
		if let Some(ref db) = self.processed_batches {
			return Ok(db.clone());
		}

		// Lazily initialize the processed_batches database
		// Use immediate transaction for DDL (not compatible with BEGIN CONCURRENT)
		let processed_batches = Database::get_or_create_turso_database(&self.processed_batches_path).await?;
		let schema_conn = Database::begin_immediate(&processed_batches).await?;
		Self::wireframe_batches_tables_direct(&schema_conn).await?;
                let _ = Database::commit_immediate(&schema_conn).await;

		self.processed_batches = Some(processed_batches.clone());
		Ok(processed_batches)
	}

	fn set_processed_batches(&mut self, turso_db: TursoDatabase) {
		self.processed_batches = Some(turso_db);
	}

	fn processed_batches_path(&self) -> String {
		self.processed_batches_path.clone()
	}

	#[instrument]
	async fn set_processed_batches_path(&mut self, path: String) {
		self.processed_batches_path = path;
	}

	#[instrument]
	async fn patterns(&mut self) -> Result<TursoDatabase> {
		if let Some(ref db) = self.patterns {
			return Ok(db.clone());
		}

		// Lazily initialize the patterns database
		// Use immediate transaction for DDL (not compatible with BEGIN CONCURRENT)
		let patterns = Database::get_or_create_turso_database(&self.patterns_path).await?;
		let schema_conn = Database::begin_immediate(&patterns).await?;
		Self::wireframe_patterns_tables_direct(&schema_conn).await?;
                let _ = Database::commit_immediate(&schema_conn).await;

		self.patterns = Some(patterns.clone());
		Ok(patterns)
	}

	fn set_patterns(&mut self, turso_db: TursoDatabase) {
		self.patterns = Some(turso_db);
	}

	fn patterns_path(&self) -> String {
		self.patterns_path.clone()
	}

	#[instrument]
	async fn set_patterns_path(&mut self, path: String) {
		self.patterns_path = path;
	}

	#[instrument]
	async fn wireframe_patterns_tables(conn: &Connection) -> Result<()> {
		// Note: No PRIMARY KEY on TEXT columns or indexes to support MVCC
		// Main patterns table with precomputed statistics
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS patterns (
				id TEXT NOT NULL,

				sum_value TEXT NOT NULL,
				abs_sum_value TEXT NOT NULL,
				max_value TEXT NOT NULL,
				min_value TEXT NOT NULL,
				abs_max_value TEXT NOT NULL,
				avg_value TEXT NOT NULL,
				abs_avg_value TEXT NOT NULL
			)
			",
				turso::params![],
			)
			.await?;

		// Pattern occurrences table (INTEGER PRIMARY KEY AUTOINCREMENT is rowid alias, no index)
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS pattern_occurrences (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				pattern_id TEXT NOT NULL,
				aspect_id TEXT NOT NULL,
				resolution TEXT NOT NULL,
				size INTEGER NOT NULL,
				database_info TEXT NOT NULL,
				beginning_timestamp INTEGER NOT NULL,
				end_timestamp INTEGER NOT NULL
			)
			",
				turso::params![],
			)
			.await?;

		// Pattern relatives table (INTEGER PRIMARY KEY AUTOINCREMENT is rowid alias, no index)
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS pattern_relatives (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				pattern_id TEXT NOT NULL,
				relative_index INTEGER NOT NULL,

				-- MeasurementVector data
				vector_location TEXT NOT NULL,
				vector_amplitude TEXT NOT NULL,

				-- Relative-specific data
				max_x TEXT NOT NULL,
				max_y TEXT NOT NULL
			)
			",
				turso::params![],
			)
			.await?;

		Ok(())
	}

	#[instrument]
	async fn events(&mut self) -> Result<TursoDatabase> {
		if let Some(ref db) = self.unprocessed_events {
			return Ok(db.clone());
		}

		// Lazily initialize the events database
		// Use immediate transaction for DDL (not compatible with BEGIN CONCURRENT)
		let events = Database::get_or_create_turso_database(&self.unprocessed_events_path).await?;
		let schema_conn = Database::begin_immediate(&events).await?;
		Self::wireframe_events_tables_direct(&schema_conn).await?;
                let _ = Database::commit_immediate(&schema_conn).await;

		self.unprocessed_events = Some(events.clone());
		Ok(events)
	}

	fn set_events(&mut self, turso_db: TursoDatabase) {
		self.unprocessed_events = Some(turso_db);
	}

	fn events_path(&self) -> String {
		self.unprocessed_events_path.clone()
	}

	#[instrument]
	async fn set_events_path(&mut self, path: String) {
		self.unprocessed_events_path = path;
	}

	#[instrument]
	async fn unprocessed_events(&mut self) -> Result<TursoDatabase> {
		if let Some(ref db) = self.unprocessed_events {
			return Ok(db.clone());
		}

		// Lazily initialize the unprocessed events database
		// Use immediate transaction for DDL (not compatible with BEGIN CONCURRENT)
		let events = Database::get_or_create_turso_database(&self.unprocessed_events_path).await?;
		let schema_conn = Database::begin_immediate(&events).await?;
		Self::wireframe_events_tables_direct(&schema_conn).await?;
                let _ = Database::commit_immediate(&schema_conn).await;

		self.unprocessed_events = Some(events.clone());
		Ok(events)
	}

	fn set_unprocessed_events(&mut self, turso_db: TursoDatabase) {
		self.unprocessed_events = Some(turso_db);
	}

	fn unprocessed_events_path(&self) -> String {
		self.unprocessed_events_path.clone()
	}

	#[instrument]
	async fn set_unprocessed_events_path(&mut self, path: String) {
		self.unprocessed_events_path = path;
	}

	#[instrument]
	async fn processed_events(&mut self) -> Result<TursoDatabase> {
		if let Some(ref db) = self.processed_events {
			return Ok(db.clone());
		}

		// Lazily initialize the processed events database
		// Use immediate transaction for DDL (not compatible with BEGIN CONCURRENT)
		let events = Database::get_or_create_turso_database(&self.processed_events_path).await?;
		let schema_conn = Database::begin_immediate(&events).await?;
		Self::wireframe_events_tables_direct(&schema_conn).await?;
                let _ = Database::commit_immediate(&schema_conn).await;

		self.processed_events = Some(events.clone());
		Ok(events)
	}

	fn set_processed_events(&mut self, turso_db: TursoDatabase) {
		self.processed_events = Some(turso_db);
	}

	fn processed_events_path(&self) -> String {
		self.processed_events_path.clone()
	}

	#[instrument]
	async fn set_processed_events_path(&mut self, path: String) {
		self.processed_events_path = path;
	}

	#[instrument]
	async fn wireframe_events_tables(conn: &Connection) -> Result<()> {
		// Note: No PRIMARY KEY on TEXT columns or indexes to support MVCC
		// Main events table
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS events (
				id TEXT NOT NULL,
				name TEXT NOT NULL,
				description TEXT,
				created_at INTEGER NOT NULL
			)
			",
				turso::params![],
			)
			.await?;

		// Event manifestations table
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS event_manifestations (
				id TEXT NOT NULL,
				event_id TEXT NOT NULL,
				dataset_id TEXT NOT NULL,
				start_timestamp INTEGER NOT NULL,
				end_timestamp INTEGER NOT NULL
			)
			",
				turso::params![],
			)
			.await?;

		Ok(())
	}

	#[instrument]
	async fn correlations(&mut self) -> Result<TursoDatabase> {
		if let Some(ref db) = self.correlations {
			return Ok(db.clone());
		}

		// Lazily initialize the correlations database
		// Use immediate transaction for DDL (not compatible with BEGIN CONCURRENT)
		let correlations = Database::get_or_create_turso_database(&self.correlations_path).await?;
		let schema_conn = Database::begin_immediate(&correlations).await?;
		Self::wireframe_correlations_tables_direct(&schema_conn).await?;
                let _ = Database::commit_immediate(&schema_conn).await;
		drop(schema_conn);

		self.correlations = Some(correlations.clone());
		Ok(correlations)
	}

	fn set_correlations(&mut self, turso_db: TursoDatabase) {
		self.correlations = Some(turso_db);
	}

	fn correlations_path(&self) -> String {
		self.correlations_path.clone()
	}

	#[instrument]
	async fn set_correlations_path(&mut self, path: String) {
		self.correlations_path = path;
	}

	#[instrument]
	async fn wireframe_correlations_tables(conn: &Connection) -> Result<()> {
		// Note: No PRIMARY KEY on TEXT columns or indexes to support MVCC
		// Main correlations table
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS correlations (
				id TEXT NOT NULL,
				dictionary_id TEXT NOT NULL,
				subject_id TEXT NOT NULL,
				aspect_id TEXT NOT NULL,
				pattern_id TEXT NOT NULL,
				event_id TEXT NOT NULL,
				created_at INTEGER NOT NULL,
				updated_at INTEGER NOT NULL
			)
			",
				turso::params![],
			)
			.await?;

		// Add columns if they don't exist (for existing tables)
		conn.as_ref().execute("ALTER TABLE correlations ADD COLUMN subject_id TEXT", turso::params![]).await.ok();
		conn.as_ref().execute("ALTER TABLE correlations ADD COLUMN aspect_id TEXT", turso::params![]).await.ok();
		conn.as_ref().execute("ALTER TABLE correlations ADD COLUMN average_distance_value TEXT", turso::params![]).await.ok();
		conn.as_ref().execute("ALTER TABLE correlations ADD COLUMN average_distance_units TEXT", turso::params![]).await.ok();

		// Error rates table (INTEGER PRIMARY KEY AUTOINCREMENT is rowid alias, no index)
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS correlation_error_rates (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				correlation_id TEXT NOT NULL,
				signal_type TEXT NOT NULL,
				error_rate_value TEXT NOT NULL,
				error_rate_units TEXT NOT NULL
			)
			",
				turso::params![],
			)
			.await?;

		// Correlation occurrences table (INTEGER PRIMARY KEY AUTOINCREMENT is rowid alias, no index)
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS correlation_occurrences (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				correlation_id TEXT NOT NULL,
				occurrence_index INTEGER NOT NULL,
				aspect_id TEXT NOT NULL,
				resolution TEXT NOT NULL,
				size INTEGER NOT NULL,
				database_info TEXT NOT NULL,
				pattern_id TEXT NOT NULL,
				beginning_timestamp INTEGER NOT NULL,
				end_timestamp INTEGER NOT NULL
			)
			",
				turso::params![],
			)
			.await?;

		Ok(())
	}

	#[instrument]
	async fn wireframe_dictionary_tables(&self, conn: &Connection) -> Result<()> {
		// Note: No PRIMARY KEY on TEXT columns or indexes to support MVCC
		// Main dictionary metadata table
		conn.as_ref()
			.execute(
				r"
                                        CREATE TABLE IF NOT EXISTS dictionary_metadata (
                                                id TEXT NOT NULL,
                                                name TEXT NOT NULL,
                                                description TEXT,
                                                created_at INTEGER NOT NULL
                                        )
                                ",
				turso::params![],
			)
			.await?;

		// Dictionary constraints table (INTEGER PRIMARY KEY AUTOINCREMENT is rowid alias, no index)
		conn.as_ref()
			.execute(
				r"
                                        CREATE TABLE IF NOT EXISTS dictionary_constraints (
                                                id INTEGER PRIMARY KEY AUTOINCREMENT,
                                                dictionary_id TEXT NOT NULL,
                                                steps_count INTEGER,
                                                steps_interpolation TEXT
                                        )
                                ",
				turso::params![],
			)
			.await?;

		// Dictionary variabilities table (INTEGER PRIMARY KEY AUTOINCREMENT is rowid alias, no index)
		conn.as_ref()
			.execute(
				r"
                                        CREATE TABLE IF NOT EXISTS dictionary_variabilities (
                                                id INTEGER PRIMARY KEY AUTOINCREMENT,
                                                dictionary_id TEXT NOT NULL,
                                                variability_type TEXT NOT NULL,
                                                variability_value TEXT NOT NULL
                                        )
                                ",
				turso::params![],
			)
			.await?;

		// Dictionary patterns table (INTEGER PRIMARY KEY AUTOINCREMENT is rowid alias, no index)
		conn.as_ref()
			.execute(
				r"
                                        CREATE TABLE IF NOT EXISTS dictionary_patterns (
                                                id INTEGER PRIMARY KEY AUTOINCREMENT,
                                                dictionary_id TEXT NOT NULL,
                                                pattern_id TEXT NOT NULL,
                                                added_at INTEGER NOT NULL
                                        )
                                ",
				turso::params![],
			)
			.await?;

		Ok(())
	}

	#[instrument]
	async fn new_dictionary(&self, name: &str, description: &str, constraints: &DictionaryConstraints) -> Result<()> {
		// Extract db_name from path (path is like C:\Users\...\dsp_data\{db_name}\{subject}\{aspect})
		// Use the parent's parent to get db_name from the aspect path
		let path = std::path::Path::new(&self.path);
		let subject_path = path.parent().ok_or_else(|| anyhow::anyhow!("Cannot get subject path from aspect path"))?;
		let db_path = subject_path.parent().ok_or_else(|| anyhow::anyhow!("Cannot get db path from subject path"))?;
		let db_name = db_path.file_name().ok_or_else(|| anyhow::anyhow!("Cannot extract db_name from path"))?.to_string_lossy().to_string();
		
		let dictionaries_db_path = Database::aspect_dictionaries_db_path(&db_name, &self.subject_name, &self.name, name);
		let dictionaries_db = Database::get_or_create_turso_database(&dictionaries_db_path).await?;
		
		// Use immediate transaction for DDL (not compatible with BEGIN CONCURRENT)
		let schema_conn = Database::begin_immediate(&dictionaries_db).await?;
		Self::wireframe_dictionary_tables_direct(&schema_conn).await?;
                let _ = Database::commit_immediate(&schema_conn).await;
                drop(schema_conn);

		// Now use BEGIN CONCURRENT for data operations
		let conn = Database::begin_concurrent(&dictionaries_db, &dictionaries_db_path, None).await?;
		let id = DictionaryId::new();

		// Insert metadata
		conn.as_ref().execute("INSERT INTO dictionary_metadata (id, name, description, created_at) VALUES (?, ?, ?, ?)", turso::params![id.as_uuid().to_string(), name, description, chrono::Utc::now().timestamp_millis()]).await?;

		// Insert constraints
		let steps_count = constraints.steps().as_ref().map(|s| s.count().to_string());
		let steps_interpolation = constraints.steps().as_ref().map(|s| s.interpolation().to_string());
		conn.as_ref().execute("INSERT INTO dictionary_constraints (dictionary_id, steps_count, steps_interpolation) VALUES (?, ?, ?)", turso::params![id.as_uuid().to_string(), steps_count, steps_interpolation]).await?;

		// Insert variabilities
		if let Some(variabilities) = constraints.variabilities() {
			for variability in variabilities {
				let (var_type, var_value) = match variability {
					VariablilityType::MaximumStatic(v) => ("MaximumStatic", v.value().to_string()),
					VariablilityType::AverageStatic(v) => ("AverageStatic", v.value().to_string()),
					VariablilityType::AbsoluteMaximumStatic(v) => ("AbsoluteMaximumStatic", v.value().to_string()),
					VariablilityType::AbsoluteAverageStatic(v) => ("AbsoluteAverageStatic", v.value().to_string()),
					VariablilityType::MaximumPercentile(v) => ("MaximumPercentile", v.value().to_string()),
					VariablilityType::AveragePercentile(v) => ("AveragePercentile", v.value().to_string()),
					VariablilityType::AbsoluteMaximumPercentile(v) => ("AbsoluteMaximumPercentile", v.value().to_string()),
					VariablilityType::AbsoluteAveragePercentile(v) => ("AbsoluteAveragePercentile", v.value().to_string()),
					VariablilityType::SumStatic(v) => ("SumStatic", v.value().to_string()),
					VariablilityType::SumPercentile(v) => ("SumPercentile", v.value().to_string()),
					VariablilityType::AbsoluteSumStatic(v) => ("AbsoluteSumStatic", v.value().to_string()),
					VariablilityType::AbsoluteSumPercentile(v) => ("AbsoluteSumPercentile", v.value().to_string()),
				};
				conn.as_ref().execute("INSERT INTO dictionary_variabilities (dictionary_id, variability_type, variability_value) VALUES (?, ?, ?)", turso::params![id.as_uuid().to_string(), var_type, var_value]).await?;
			}
		}

		let _ = Database::commit_concurrent(&conn).await;
		Ok(())
	}

	#[instrument]
	async fn dictionary(&mut self, name: &str) -> Result<TursoDatabase> {
		// Check if we already have this dictionary cached
		if let Some(ref dictionaries) = self.dictionaries {
			if let Some(db) = dictionaries.get(name) {
				return Ok(db.clone());
			}
		}

		// Extract db_name from path (path is like C:\Users\...\dsp_data\{db_name}\{subject}\{aspect})
		let path = std::path::Path::new(&self.path);
		let subject_path = path.parent().ok_or_else(|| anyhow::anyhow!("Cannot get subject path from aspect path"))?;
		let db_path_parent = subject_path.parent().ok_or_else(|| anyhow::anyhow!("Cannot get db path from subject path"))?;
		let db_name = db_path_parent.file_name().ok_or_else(|| anyhow::anyhow!("Cannot extract db_name from path"))?.to_string_lossy().to_string();

		// Get the path for this dictionary
		let dictionary_path = Database::aspect_dictionaries_db_path(&db_name, &self.subject_name, &self.name, name);
		
		// Check if path exists on disk (dictionary may have been created but not cached in paths map)
		if !std::path::Path::new(&dictionary_path).exists() && !self.dictionaries_paths.contains_key(name) {
			anyhow::bail!("Dictionary '{}' does not exist for aspect '{}'", name, self.name);
		}

		// Get or create the database
		let dictionary_db = Database::get_or_create_turso_database(&dictionary_path).await?;
		
		// Ensure all tables exist (DDL uses immediate transaction, not BEGIN CONCURRENT)
		// This handles the case where the dictionary was created before new tables were added
		let schema_conn = Database::begin_immediate(&dictionary_db).await?;
		Self::wireframe_dictionary_tables_direct(&schema_conn).await?;
                let _ = Database::commit_immediate(&schema_conn).await;
		
		// Cache it
		if self.dictionaries.is_none() {
			self.dictionaries = Some(HashMap::new());
		}
		if let Some(ref mut dictionaries) = self.dictionaries {
			dictionaries.insert(name.to_string(), dictionary_db.clone());
		}
		// Also cache the path if not already there
		if !self.dictionaries_paths.contains_key(name) {
			self.dictionaries_paths.insert(name.to_string(), dictionary_path);
		}

		Ok(dictionary_db)
	}
}

// ===== Direct wireframe functions for DDL (CREATE TABLE) operations =====
// These use turso::Connection directly instead of Connection wrapper
// Required because DDL is not compatible with MVCC concurrent transactions
impl Aspect {
	#[instrument]
	async fn wireframe_measurements_tables_direct(conn: &Connection) -> Result<()> {
		// Create measurements table
		conn.as_ref().execute(
			"CREATE TABLE IF NOT EXISTS measurements (
				id TEXT NOT NULL,
				dataset_id TEXT NOT NULL,
				timestamp INTEGER NOT NULL,
				value TEXT NOT NULL
			)",
			turso::params![],
		)
		.await?;

		// Try to create index on timestamp for efficient range queries
		// This may fail on older turso versions with MVCC, but cursor-based pagination works without it
		if let Err(e) = conn.as_ref()
			.execute(
				"CREATE INDEX IF NOT EXISTS idx_measurements_timestamp ON measurements(timestamp)",
				turso::params![],
			)
			.await
		{
			tracing::warn!("Could not create timestamp index (MVCC limitation): {e}");
		}

		Ok(())
	}

	#[instrument]
	async fn wireframe_batches_tables_direct(conn: &Connection) -> Result<()> {
		conn.as_ref().execute(
			r"
			CREATE TABLE IF NOT EXISTS batches (
				id TEXT NOT NULL,
				aspect_id TEXT NOT NULL,
				database_id TEXT NOT NULL,
				size INTEGER NOT NULL,
				resolution TEXT NOT NULL,
				measurements TEXT NOT NULL,
				batch_hash TEXT,
				status TEXT NOT NULL DEFAULT 'pending',
				created_at INTEGER NOT NULL,
				updated_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000)
			)",
			turso::params![],
		)
		.await?;

		Ok(())
	}

	#[instrument]
	async fn wireframe_patterns_tables_direct(conn: &Connection) -> Result<()> {
		// Note: No PRIMARY KEY on TEXT columns or indexes to support MVCC
		// Main patterns table with precomputed statistics
		conn.as_ref().execute(
			r"
			CREATE TABLE IF NOT EXISTS patterns (
				id TEXT NOT NULL,

				sum_value TEXT NOT NULL,
				abs_sum_value TEXT NOT NULL,
				max_value TEXT NOT NULL,
				min_value TEXT NOT NULL,
				abs_max_value TEXT NOT NULL,
				avg_value TEXT NOT NULL,
				abs_avg_value TEXT NOT NULL
			)
			",
			turso::params![],
		)
		.await?;

		// Pattern occurrences table (INTEGER PRIMARY KEY AUTOINCREMENT is rowid alias, no index)
		conn.as_ref().execute(
			r"
			CREATE TABLE IF NOT EXISTS pattern_occurrences (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				pattern_id TEXT NOT NULL,
				occurrence_index INTEGER NOT NULL,
				aspect_id TEXT NOT NULL,
				resolution TEXT NOT NULL,
				size INTEGER NOT NULL,
				database_info TEXT NOT NULL,
				beginning_timestamp INTEGER NOT NULL,
				end_timestamp INTEGER NOT NULL
			)
			",
			turso::params![],
		)
		.await?;

		// Pattern relatives table (INTEGER PRIMARY KEY AUTOINCREMENT is rowid alias, no index)
		conn.as_ref().execute(
			r"
			CREATE TABLE IF NOT EXISTS pattern_relatives (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				pattern_id TEXT NOT NULL,
				relative_index INTEGER NOT NULL,
				relative_value TEXT NOT NULL
			)
			",
			turso::params![],
		)
		.await?;

		Ok(())
	}

	#[instrument]
	async fn wireframe_events_tables_direct(conn: &Connection) -> Result<()> {
		// Note: No PRIMARY KEY on TEXT columns or indexes to support MVCC
		// Main events table
		conn.as_ref().execute(
			r"
			CREATE TABLE IF NOT EXISTS events (
				id TEXT NOT NULL,
				name TEXT NOT NULL,
				description TEXT,
				created_at INTEGER NOT NULL
			)
			",
			turso::params![],
		)
		.await?;

		// Event manifestations table
		conn.as_ref().execute(
			r"
			CREATE TABLE IF NOT EXISTS event_manifestations (
				id TEXT NOT NULL,
				event_id TEXT NOT NULL,
				dataset_id TEXT NOT NULL,
				start_timestamp INTEGER NOT NULL,
				end_timestamp INTEGER NOT NULL
			)
			",
			turso::params![],
		)
		.await?;

		Ok(())
	}

	#[instrument]
	async fn wireframe_correlations_tables_direct(conn: &Connection) -> Result<()> {
		// Note: No PRIMARY KEY on TEXT columns or indexes to support MVCC
		// Main correlations table
		conn.as_ref().execute(
			r"
			CREATE TABLE IF NOT EXISTS correlations (
				id TEXT NOT NULL,
				dictionary_id TEXT NOT NULL,
				subject_id TEXT NOT NULL,
				aspect_id TEXT NOT NULL,
				pattern_id TEXT NOT NULL,
				event_id TEXT NOT NULL,
				created_at INTEGER NOT NULL,
				updated_at INTEGER NOT NULL
			)
			",
			turso::params![],
		)
		.await?;

		// Add columns if they don't exist (for existing tables)
		conn.as_ref().execute("ALTER TABLE correlations ADD COLUMN subject_id TEXT", turso::params![]).await.ok();
		conn.as_ref().execute("ALTER TABLE correlations ADD COLUMN aspect_id TEXT", turso::params![]).await.ok();
		conn.as_ref().execute("ALTER TABLE correlations ADD COLUMN average_distance_value TEXT", turso::params![]).await.ok();
		conn.as_ref().execute("ALTER TABLE correlations ADD COLUMN average_distance_units TEXT", turso::params![]).await.ok();

		// Error rates table (INTEGER PRIMARY KEY AUTOINCREMENT is rowid alias, no index)
		conn.as_ref().execute(
			r"
			CREATE TABLE IF NOT EXISTS correlation_error_rates (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				correlation_id TEXT NOT NULL,
				signal_type TEXT NOT NULL,
				error_rate_value TEXT NOT NULL,
				error_rate_units TEXT NOT NULL
			)
			",
			turso::params![],
		)
		.await?;

		// Correlation occurrences table (INTEGER PRIMARY KEY AUTOINCREMENT is rowid alias, no index)
		conn.as_ref().execute(
			r"
			CREATE TABLE IF NOT EXISTS correlation_occurrences (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				correlation_id TEXT NOT NULL,
				occurrence_index INTEGER NOT NULL,
				aspect_id TEXT NOT NULL,
				resolution TEXT NOT NULL,
				size INTEGER NOT NULL,
				database_info TEXT NOT NULL,
				pattern_id TEXT NOT NULL,
				beginning_timestamp INTEGER NOT NULL,
				end_timestamp INTEGER NOT NULL
			)
			",
			turso::params![],
		)
		.await?;

		Ok(())
	}

	#[instrument]
	async fn wireframe_dictionary_tables_direct(conn: &Connection) -> Result<()> {
		// Note: No PRIMARY KEY on TEXT columns or indexes to support MVCC
		// Main dictionary metadata table
		conn.as_ref().execute(
			r"
			CREATE TABLE IF NOT EXISTS dictionary_metadata (
				id TEXT NOT NULL,
				name TEXT NOT NULL,
				description TEXT,
				created_at INTEGER NOT NULL
			)
			",
			turso::params![],
		)
		.await?;

		// Dictionary constraints table (INTEGER PRIMARY KEY AUTOINCREMENT is rowid alias, no index)
		conn.as_ref().execute(
			r"
			CREATE TABLE IF NOT EXISTS dictionary_constraints (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				dictionary_id TEXT NOT NULL,
				steps_count INTEGER,
				steps_interpolation TEXT
			)
			",
			turso::params![],
		)
		.await?;

		// Dictionary variabilities table (INTEGER PRIMARY KEY AUTOINCREMENT is rowid alias, no index)
		conn.as_ref().execute(
			r"
			CREATE TABLE IF NOT EXISTS dictionary_variabilities (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				dictionary_id TEXT NOT NULL,
				variability_type TEXT NOT NULL,
				variability_value TEXT NOT NULL
			)
			",
			turso::params![],
		)
		.await?;

		// Dictionary patterns linking table (INTEGER PRIMARY KEY AUTOINCREMENT is rowid alias, no index)
		conn.as_ref().execute(
			r"
			CREATE TABLE IF NOT EXISTS dictionary_patterns (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				dictionary_id TEXT NOT NULL,
				pattern_id TEXT NOT NULL,
				added_at INTEGER NOT NULL
			)
			",
			turso::params![],
		)
		.await?;

		// Main patterns table with precomputed statistics (same schema as aspect-level patterns)
		conn.as_ref().execute(
			r"
			CREATE TABLE IF NOT EXISTS patterns (
				id TEXT NOT NULL,
				sum_value TEXT NOT NULL,
				abs_sum_value TEXT NOT NULL,
				max_value TEXT NOT NULL,
				min_value TEXT NOT NULL,
				abs_max_value TEXT NOT NULL,
				avg_value TEXT NOT NULL,
				abs_avg_value TEXT NOT NULL
			)
			",
			turso::params![],
		)
		.await?;

		// Pattern occurrences table (INTEGER PRIMARY KEY AUTOINCREMENT is rowid alias, no index)
		conn.as_ref().execute(
			r"
			CREATE TABLE IF NOT EXISTS pattern_occurrences (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				pattern_id TEXT NOT NULL,
				occurrence_index INTEGER NOT NULL,
				aspect_id TEXT NOT NULL,
				resolution TEXT NOT NULL,
				size INTEGER NOT NULL,
				database_info TEXT NOT NULL,
				beginning_timestamp INTEGER NOT NULL,
				end_timestamp INTEGER NOT NULL
			)
			",
			turso::params![],
		)
		.await?;

		// Pattern relatives table (INTEGER PRIMARY KEY AUTOINCREMENT is rowid alias, no index)
		conn.as_ref().execute(
			r"
			CREATE TABLE IF NOT EXISTS pattern_relatives (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				pattern_id TEXT NOT NULL,
				relative_index INTEGER NOT NULL,
				relative_value TEXT NOT NULL
			)
			",
			turso::params![],
		)
		.await?;

		Ok(())
	}
}

impl PartialEq for Aspect {
	fn eq(&self, other: &Self) -> bool {
		self.id == other.id && self.name == other.name && self.subject_id == other.subject_id && self.resolution == other.resolution
		// Skip measurements_turso_db comparison since it doesn't implement PartialEq
	}
}
