use std::{collections::HashMap, fmt::Display, str::FromStr};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use splimes::Resolution;
use tracing::{debug, info, instrument, trace};
use turso::Database as TursoDatabase;
use uuid::Uuid;

use crate::{
	cache::Connection, types::{
		compression::CompressionConfig, database::{
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
	pipeline: Option<TursoDatabase>,
	pipeline_path: String,

	#[serde(skip)]
	dictionaries: Option<HashMap<String, TursoDatabase>>,
	dictionaries_paths: HashMap<String, String>,

	/// Compression configuration for this aspect (stored in metadata.db).
	compression_config: Option<CompressionConfig>,
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
		let (measurements, was_new) = Database::get_or_create_turso_database(&measurements_path).await?;
		trace!("Connected measurements DB: {}", measurements_path);
		// Use immediate transaction for DDL (table creation) - not compatible with BEGIN CONCURRENT
		// Always wireframe for new aspect creation
		if was_new {
			let schema_conn = Database::begin_immediate(&measurements).await?;
			trace!("Wireframing measurements tables for: {}", measurements_path);
			Self::wireframe_measurements_tables_direct(&schema_conn).await?;
			let _ = Database::commit_immediate(&schema_conn).await;
			drop(schema_conn);
			trace!("Wireframed measurements tables for: {}", measurements_path);
		}
		let measurements = Some(measurements);
		debug!("Aspect DB initialized: measurements");

		let unprocessed_batches_path = Database::aspect_unprocessed_batches_db_path(&db_name, &subject_name, name);
		trace!("Creating unprocessed_batches DB at: {}", unprocessed_batches_path);
		let (unprocessed_batches, was_new) = Database::get_or_create_turso_database(&unprocessed_batches_path).await?;
		trace!("Connected unprocessed_batches DB: {}", unprocessed_batches_path);
		if was_new {
			let schema_conn = Database::begin_immediate(&unprocessed_batches).await?;
			trace!("Wireframing batches tables for: {}", unprocessed_batches_path);
			Self::wireframe_batches_tables_direct(&schema_conn).await?;
			let _ = Database::commit_immediate(&schema_conn).await;
			drop(schema_conn);
			trace!("Wireframed batches tables for: {}", unprocessed_batches_path);
		}
		let unprocessed_batches = Some(unprocessed_batches);
		debug!("Aspect DB initialized: unprocessed_batches");

		let processed_batches_path = Database::aspect_processed_batches_db_path(&db_name, &subject_name, name);
		trace!("Creating processed_batches DB at: {}", processed_batches_path);
		let (processed_batches, was_new) = Database::get_or_create_turso_database(&processed_batches_path).await?;
		trace!("Connected processed_batches DB: {}", processed_batches_path);
		if was_new {
			let schema_conn = Database::begin_immediate(&processed_batches).await?;
			trace!("Wireframing batches tables for: {}", processed_batches_path);
			Self::wireframe_batches_tables_direct(&schema_conn).await?;
			let _ = Database::commit_immediate(&schema_conn).await;
			drop(schema_conn);
			trace!("Wireframed batches tables for: {}", processed_batches_path);
		}
		let processed_batches = Some(processed_batches);
		debug!("Aspect DB initialized: processed_batches");

		let patterns_path = Database::aspect_patterns_db_path(&db_name, &subject_name, name);
		trace!("Creating patterns DB at: {}", patterns_path);
		let (patterns, was_new) = Database::get_or_create_turso_database(&patterns_path).await?;
		trace!("Connected patterns DB: {}", patterns_path);
		if was_new {
			let schema_conn = Database::begin_immediate(&patterns).await?;
			trace!("Wireframing patterns tables for: {}", patterns_path);
			Self::wireframe_patterns_tables_direct(&schema_conn).await?;
			let _ = Database::commit_immediate(&schema_conn).await;
			drop(schema_conn);
			trace!("Wireframed patterns tables for: {}", patterns_path);
		}
		let patterns = Some(patterns);
		debug!("Aspect DB initialized: patterns");

		let unprocessed_events_path = Database::aspect_unprocessed_events_db_path(&db_name, &subject_name, name);
		trace!("Creating unprocessed_events DB at: {}", unprocessed_events_path);
		let (unprocessed_events, was_new) = Database::get_or_create_turso_database(&unprocessed_events_path).await?;
		trace!("Connected unprocessed_events DB: {}", unprocessed_events_path);
		if was_new {
			let schema_conn = Database::begin_immediate(&unprocessed_events).await?;
			trace!("Wireframing unprocessed_events tables for: {}", unprocessed_events_path);
			Self::wireframe_events_tables_direct(&schema_conn).await?;
			let _ = Database::commit_immediate(&schema_conn).await;
			drop(schema_conn);
			trace!("Wireframed unprocessed_events tables for: {}", unprocessed_events_path);
		}
		let unprocessed_events = Some(unprocessed_events);
		debug!("Aspect DB initialized: unprocessed_events");

		let processed_events_path = Database::aspect_processed_events_db_path(&db_name, &subject_name, name);
		trace!("Creating processed_events DB at: {}", processed_events_path);
		let (processed_events, was_new) = Database::get_or_create_turso_database(&processed_events_path).await?;
		trace!("Connected processed_events DB: {}", processed_events_path);
		if was_new {
			let schema_conn = Database::begin_immediate(&processed_events).await?;
			trace!("Wireframing processed_events tables for: {}", processed_events_path);
			Self::wireframe_events_tables_direct(&schema_conn).await?;
			let _ = Database::commit_immediate(&schema_conn).await;
			drop(schema_conn);
			trace!("Wireframed processed_events tables for: {}", processed_events_path);
		}
		let processed_events = Some(processed_events);
		debug!("Aspect DB initialized: processed_events");

		let correlations_path = Database::aspect_correlations_db_path(&db_name, &subject_name, name);
		trace!("Creating correlations DB at: {}", correlations_path);
		let (correlations, was_new) = Database::get_or_create_turso_database(&correlations_path).await?;
		trace!("Connected correlations DB: {}", correlations_path);
		if was_new {
			let schema_conn = Database::begin_immediate(&correlations).await?;
			trace!("Wireframing correlations tables for: {}", correlations_path);
			Self::wireframe_correlations_tables_direct(&schema_conn).await?;
			let _ = Database::commit_immediate(&schema_conn).await;
			drop(schema_conn);
			trace!("Wireframed correlations tables for: {}", correlations_path);
		}
		let correlations = Some(correlations);
		debug!("Aspect DB initialized: correlations");

		let pipeline_path = Database::aspect_pipeline_db_path(&db_name, &subject_name, name);
		trace!("Creating pipeline DB at: {}", pipeline_path);
		let (pipeline, was_new) = Database::get_or_create_turso_database(&pipeline_path).await?;
		trace!("Connected pipeline DB: {}", pipeline_path);
		if was_new {
			let schema_conn = Database::begin_immediate(&pipeline).await?;
			trace!("Wireframing pipeline tables for: {}", pipeline_path);
			Self::wireframe_pipeline_tables_direct(&schema_conn).await?;
			let _ = Database::commit_immediate(&schema_conn).await;
			drop(schema_conn);
			trace!("Wireframed pipeline tables for: {}", pipeline_path);
		}
		let pipeline = Some(pipeline);
		debug!("Aspect DB initialized: pipeline");

		let dictionaries_path = <Database as Config>::aspect_dictionaries_path(&db_name, &subject_name, name);
		trace!("Creating dictionaries directory at: {}", dictionaries_path);

		std::fs::create_dir_all(&dictionaries_path)?;
		debug!("Aspect DB initialized: dictionaries");

		info!(aspect_id = %id, "Aspect initialization complete");

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
			pipeline,
			pipeline_path,
			dictionaries: None,
			dictionaries_paths: HashMap::new(),
			compression_config: None, // Set via set_compression_config() after creation
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
		let pipeline_path = aspect_path_str.clone() + "/pipeline.db";
		let dictionaries_path = aspect_path_str.clone() + "/dictionaries";

		let mut dictionaries_paths = HashMap::new();
		// Find all dictionary database paths in the dictionaries directory
		if let Ok(entries) = tokio::fs::read_dir(&dictionaries_path).await {
			let mut dir_entries = entries;
			while let Ok(Some(entry)) = dir_entries.next_entry().await {
				let path = entry.path();
				if path.is_file() {
					if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
						if std::path::Path::new(file_name).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("db")) {
							let dict_name = file_name.trim_end_matches(".db").to_string();
							dictionaries_paths.insert(dict_name, path.to_string_lossy().to_string());
						}
					}
				}
			}
		}

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
			pipeline: None,
			pipeline_path,
			dictionaries: None,
			dictionaries_paths,
			compression_config: None, // Will be loaded from metadata.db separately
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
		let (measurements, was_new) = Database::get_or_create_turso_database(&self.measurements_path).await?;

		// Only wireframe if database was newly created (not from cache)
		if was_new {
			let schema_conn = Database::begin_immediate(&measurements).await?;
			Self::wireframe_measurements_tables_direct(&schema_conn).await?;
			let _ = Database::commit_immediate(&schema_conn).await;
			drop(schema_conn);
		}

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
		if let Err(e) = conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_measurements_timestamp ON measurements(timestamp)", turso::params![]).await {
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
		let (unprocessed_batches, was_new) = Database::get_or_create_turso_database(&self.unprocessed_batches_path).await?;

		// Only wireframe if database was newly created (not from cache)
		if was_new {
			let schema_conn = Database::begin_immediate(&unprocessed_batches).await?;
			Self::wireframe_batches_tables_direct(&schema_conn).await?;
			let _ = Database::commit_immediate(&schema_conn).await;
			drop(schema_conn);
		}

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
		let (processed_batches, was_new) = Database::get_or_create_turso_database(&self.processed_batches_path).await?;

		// Only wireframe if database was newly created (not from cache)
		if was_new {
			let schema_conn = Database::begin_immediate(&processed_batches).await?;
			Self::wireframe_batches_tables_direct(&schema_conn).await?;
			let _ = Database::commit_immediate(&schema_conn).await;
		}

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
		let (patterns, was_new) = Database::get_or_create_turso_database(&self.patterns_path).await?;

		// Only wireframe if database was newly created (not from cache)
		if was_new {
			let schema_conn = Database::begin_immediate(&patterns).await?;
			Self::wireframe_patterns_tables_direct(&schema_conn).await?;
			let _ = Database::commit_immediate(&schema_conn).await;
		}

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

		// Pattern occurrences table (INTEGER PRIMARY KEY is rowid alias, no index)
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS pattern_occurrences (
				id INTEGER PRIMARY KEY,
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

		// Pattern relatives table (INTEGER PRIMARY KEY is rowid alias, no index)
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS pattern_relatives (
				id INTEGER PRIMARY KEY,
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
		let (events, was_new) = Database::get_or_create_turso_database(&self.unprocessed_events_path).await?;

		// Only wireframe if database was newly created (not from cache)
		if was_new {
			let schema_conn = Database::begin_immediate(&events).await?;
			Self::wireframe_events_tables_direct(&schema_conn).await?;
			let _ = Database::commit_immediate(&schema_conn).await;
		}

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
		let (events, was_new) = Database::get_or_create_turso_database(&self.unprocessed_events_path).await?;

		// Only wireframe if database was newly created (not from cache)
		if was_new {
			let schema_conn = Database::begin_immediate(&events).await?;
			Self::wireframe_events_tables_direct(&schema_conn).await?;
			let _ = Database::commit_immediate(&schema_conn).await;
		}

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
		let (events, was_new) = Database::get_or_create_turso_database(&self.processed_events_path).await?;

		// Only wireframe if database was newly created (not from cache)
		if was_new {
			let schema_conn = Database::begin_immediate(&events).await?;
			Self::wireframe_events_tables_direct(&schema_conn).await?;
			let _ = Database::commit_immediate(&schema_conn).await;
		}

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
		let (correlations, was_new) = Database::get_or_create_turso_database(&self.correlations_path).await?;

		// Only wireframe if database was newly created (not from cache)
		if was_new {
			let schema_conn = Database::begin_immediate(&correlations).await?;
			Self::wireframe_correlations_tables_direct(&schema_conn).await?;
			let _ = Database::commit_immediate(&schema_conn).await;
			drop(schema_conn);
		}

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

		// Error rates table (INTEGER PRIMARY KEY is rowid alias, no index)
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS correlation_error_rates (
				id INTEGER PRIMARY KEY,
				correlation_id TEXT NOT NULL,
				signal_type TEXT NOT NULL,
				error_rate_value TEXT NOT NULL,
				error_rate_units TEXT NOT NULL
			)
			",
				turso::params![],
			)
			.await?;

		// Correlation occurrences table (INTEGER PRIMARY KEY is rowid alias, no index)
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS correlation_occurrences (
				id INTEGER PRIMARY KEY,
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
	async fn pipeline(&mut self) -> Result<turso::Database> {
		if let Some(turso_db) = &self.pipeline {
			return Ok(turso_db.clone());
		}

		let (turso_db, was_new) = Database::get_or_create_turso_database(&self.pipeline_path).await?;

		// Only wireframe if database was newly created (not from cache)
		if was_new {
			let schema_conn = Database::begin_immediate(&turso_db).await?;
			Self::wireframe_pipeline_tables_direct(&schema_conn).await?;
			let _ = Database::commit_immediate(&schema_conn).await;
		}

		self.pipeline = Some(turso_db.clone());
		Ok(turso_db)
	}

	fn set_pipeline(&mut self, turso_db: TursoDatabase) {
		self.pipeline = Some(turso_db);
	}

	fn pipeline_path(&self) -> String {
		self.pipeline_path.clone()
	}

	#[instrument]
	async fn set_pipeline_path(&mut self, path: String) {
		self.pipeline_path = path;
	}

	#[instrument]
	async fn wireframe_pipeline_tables(conn: &Connection) -> Result<()> {
		// Pipeline configuration table (singleton row with id=1, enforced in app code)
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS pipeline_config (
				id INTEGER PRIMARY KEY,
				spline_method TEXT NOT NULL,
				batch_size INTEGER NOT NULL,
				created_at INTEGER NOT NULL,
				updated_at INTEGER NOT NULL
			)
			",
				turso::params![],
			)
			.await?;

		// Pipeline state table (singleton row with id=1, enforced in app code)
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS pipeline_state (
				id INTEGER PRIMARY KEY,
				last_run INTEGER,
				run_count INTEGER NOT NULL DEFAULT 0,
				version INTEGER NOT NULL DEFAULT 1
			)
			",
				turso::params![],
			)
			.await?;

		// Pipeline dictionaries (names of dictionaries used by this pipeline)
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS pipeline_dictionaries (
				dictionary_name TEXT NOT NULL,
				added_at INTEGER NOT NULL
			)
			",
				turso::params![],
			)
			.await?;

		// Pipeline detector metadata (for re-registration hints)
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS pipeline_detectors (
				detector_id TEXT NOT NULL,
				detector_name TEXT NOT NULL,
				description TEXT,
				detector_type TEXT NOT NULL,
				config_json TEXT,
				added_at INTEGER NOT NULL
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

		// Dictionary constraints table (INTEGER PRIMARY KEY is rowid alias, no index)
		conn.as_ref()
			.execute(
				r"
                                        CREATE TABLE IF NOT EXISTS dictionary_constraints (
                                                id INTEGER PRIMARY KEY,
                                                dictionary_id TEXT NOT NULL,
                                                steps_count INTEGER,
                                                steps_interpolation TEXT
                                        )
                                ",
				turso::params![],
			)
			.await?;

		// Dictionary variabilities table (INTEGER PRIMARY KEY is rowid alias, no index)
		conn.as_ref()
			.execute(
				r"
                                        CREATE TABLE IF NOT EXISTS dictionary_variabilities (
                                                id INTEGER PRIMARY KEY,
                                                dictionary_id TEXT NOT NULL,
                                                variability_type TEXT NOT NULL,
                                                variability_value TEXT NOT NULL
                                        )
                                ",
				turso::params![],
			)
			.await?;

		// Dictionary patterns table (INTEGER PRIMARY KEY is rowid alias, no index)
		conn.as_ref()
			.execute(
				r"
                                        CREATE TABLE IF NOT EXISTS dictionary_patterns (
                                                id INTEGER PRIMARY KEY,
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
		// Extract db_name from path (path is like C:\Users\...\weft_data\{db_name}\{subject}\{aspect})
		// Use the parent's parent to get db_name from the aspect path
		let path = std::path::Path::new(&self.path);
		let subject_path = path.parent().ok_or_else(|| anyhow::anyhow!("Cannot get subject path from aspect path"))?;
		let db_path = subject_path.parent().ok_or_else(|| anyhow::anyhow!("Cannot get db path from subject path"))?;
		let db_name = db_path.file_name().ok_or_else(|| anyhow::anyhow!("Cannot extract db_name from path"))?.to_string_lossy().to_string();

		let dictionaries_db_path = Database::aspect_dictionaries_db_path(&db_name, &self.subject_name, &self.name, name);
		let (dictionaries_db, was_new) = Database::get_or_create_turso_database(&dictionaries_db_path).await?;

		// Use immediate transaction for DDL (not compatible with BEGIN CONCURRENT)
		// Always wireframe for new_dictionary since we're creating a new dictionary
		if was_new {
			let schema_conn = Database::begin_immediate(&dictionaries_db).await?;
			Self::wireframe_dictionary_tables_direct(&schema_conn).await?;
			let _ = Database::commit_immediate(&schema_conn).await;
			drop(schema_conn);
		}

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

		// Extract db_name from path (path is like C:\Users\...\weft_data\{db_name}\{subject}\{aspect})
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
		let (dictionary_db, was_new) = Database::get_or_create_turso_database(&dictionary_path).await?;

		// Only wireframe if database was newly created (not from cache)
		if was_new {
			let schema_conn = Database::begin_immediate(&dictionary_db).await?;
			Self::wireframe_dictionary_tables_direct(&schema_conn).await?;
			let _ = Database::commit_immediate(&schema_conn).await;
		}

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
		if let Err(e) = conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_measurements_timestamp ON measurements(timestamp)", turso::params![]).await {
			tracing::warn!("Could not create timestamp index (MVCC limitation): {e}");
		}

		// Create compression tracking tables
		Self::wireframe_compression_stats_tables_direct(conn).await?;

		Ok(())
	}

	/// Create tables for tracking compression history and dirty regions
	#[instrument]
	async fn wireframe_compression_stats_tables_direct(conn: &Connection) -> Result<()> {
		// Track every compression run
		conn.as_ref()
			.execute(
				"CREATE TABLE IF NOT EXISTS compression_history (
					id TEXT NOT NULL PRIMARY KEY,
					started_at INTEGER NOT NULL,
					completed_at INTEGER NOT NULL,
					duration_ms INTEGER NOT NULL,
					original_count INTEGER NOT NULL,
					compressed_count INTEGER NOT NULL,
					compression_ratio REAL NOT NULL,
					final_size_bytes INTEGER NOT NULL,
					time_based_tiers INTEGER NOT NULL,
					size_based_iterations INTEGER NOT NULL,
					config_json TEXT NOT NULL
				)",
				turso::params![],
			)
			.await?;

		// Per-tier/iteration details
		conn.as_ref()
			.execute(
				"CREATE TABLE IF NOT EXISTS compression_tier_results (
					id INTEGER PRIMARY KEY,
					history_id TEXT NOT NULL,
					tier_number INTEGER NOT NULL,
					phase TEXT NOT NULL,
					original_count INTEGER NOT NULL,
					compressed_count INTEGER NOT NULL,
					compression_ratio REAL NOT NULL,
					aggressiveness REAL NOT NULL,
					time_range_start INTEGER NOT NULL,
					time_range_end INTEGER NOT NULL,
					duration_ms INTEGER NOT NULL
				)",
				turso::params![],
			)
			.await?;

		// Regions needing recompression (e.g., new data inserted into compressed range)
		conn.as_ref()
			.execute(
				"CREATE TABLE IF NOT EXISTS dirty_regions (
					id INTEGER PRIMARY KEY,
					region_start INTEGER NOT NULL,
					region_end INTEGER NOT NULL,
					marked_at INTEGER NOT NULL,
					reason TEXT NOT NULL
				)",
				turso::params![],
			)
			.await?;

		// Current compression state for fast UI queries (singleton row, id always = 1)
		conn.as_ref()
			.execute(
				"CREATE TABLE IF NOT EXISTS compression_state (
					id INTEGER PRIMARY KEY,
					is_compressing INTEGER NOT NULL DEFAULT 0,
					current_phase TEXT,
					current_tier INTEGER,
					total_tiers INTEGER,
					last_compression_id TEXT,
					last_compression_at INTEGER,
					dirty_regions_count INTEGER NOT NULL DEFAULT 0
				)",
				turso::params![],
			)
			.await?;

		// Insert singleton if not exists
		conn.as_ref().execute("INSERT OR IGNORE INTO compression_state (id, is_compressing, dirty_regions_count) VALUES (1, 0, 0)", turso::params![]).await?;

		// Index for faster dirty region queries
		if let Err(e) = conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_dirty_regions_time ON dirty_regions(region_start, region_end)", turso::params![]).await {
			tracing::warn!("Could not create dirty_regions index (MVCC limitation): {e}");
		}

		// Index for history lookup by completion time
		if let Err(e) = conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_compression_history_time ON compression_history(completed_at)", turso::params![]).await {
			tracing::warn!("Could not create compression_history index (MVCC limitation): {e}");
		}

		Ok(())
	}

	#[instrument]
	async fn wireframe_batches_tables_direct(conn: &Connection) -> Result<()> {
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

		Ok(())
	}

	#[instrument]
	async fn wireframe_patterns_tables_direct(conn: &Connection) -> Result<()> {
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

		// Pattern occurrences table (INTEGER PRIMARY KEY is rowid alias, no index)
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS pattern_occurrences (
				id INTEGER PRIMARY KEY,
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

		// Pattern relatives table (INTEGER PRIMARY KEY is rowid alias, no index)
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS pattern_relatives (
				id INTEGER PRIMARY KEY,
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
	async fn wireframe_correlations_tables_direct(conn: &Connection) -> Result<()> {
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

		// Error rates table (INTEGER PRIMARY KEY is rowid alias, no index)
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS correlation_error_rates (
				id INTEGER PRIMARY KEY,
				correlation_id TEXT NOT NULL,
				signal_type TEXT NOT NULL,
				error_rate_value TEXT NOT NULL,
				error_rate_units TEXT NOT NULL
			)
			",
				turso::params![],
			)
			.await?;

		// Correlation occurrences table (INTEGER PRIMARY KEY is rowid alias, no index)
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS correlation_occurrences (
				id INTEGER PRIMARY KEY,
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
	async fn wireframe_pipeline_tables_direct(conn: &Connection) -> Result<()> {
		// Pipeline configuration table (singleton row with id=1, enforced in app code)
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS pipeline_config (
				id INTEGER PRIMARY KEY,
				spline_method TEXT NOT NULL,
				batch_size INTEGER NOT NULL,
				created_at INTEGER NOT NULL,
				updated_at INTEGER NOT NULL
			)
			",
				turso::params![],
			)
			.await?;

		// Pipeline state table (singleton row with id=1, enforced in app code)
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS pipeline_state (
				id INTEGER PRIMARY KEY,
				last_run INTEGER,
				run_count INTEGER NOT NULL DEFAULT 0,
				version INTEGER NOT NULL DEFAULT 1
			)
			",
				turso::params![],
			)
			.await?;

		// Pipeline dictionaries (names of dictionaries used by this pipeline)
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS pipeline_dictionaries (
				dictionary_name TEXT NOT NULL,
				added_at INTEGER NOT NULL
			)
			",
				turso::params![],
			)
			.await?;

		// Pipeline detector metadata (for re-registration hints)
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS pipeline_detectors (
				detector_id TEXT NOT NULL,
				detector_name TEXT NOT NULL,
				description TEXT,
				detector_type TEXT NOT NULL,
				config_json TEXT,
				added_at INTEGER NOT NULL
			)
			",
				turso::params![],
			)
			.await?;

		Ok(())
	}

	/// Create a dictionary database's **full** table set, in an exclusive transaction.
	///
	/// Turso rejects DDL inside a `BEGIN CONCURRENT` transaction
	/// (*"DDL statements require an exclusive transaction"*), so every schema-creating path
	/// must go through `begin_immediate` — the same shape as
	/// [`ensure_compression_tables`](Self::ensure_compression_tables).
	///
	/// This exists because [`set_dictionary_metadata`](crate::database::traits::Inputs::set_dictionary_metadata)
	/// needs the schema from outside `impl Aspect`, where
	/// `wireframe_dictionary_tables_direct` is not
	/// reachable. Routing through the wireframe rather than re-listing the statements is
	/// load-bearing: it creates all **seven** tables, including `patterns`,
	/// `pattern_occurrences` and `pattern_relatives`, which the pattern read/write paths need.
	///
	/// `CREATE TABLE IF NOT EXISTS` throughout, so calling it repeatedly is cheap and safe.
	///
	/// # Errors
	///
	/// Propagates any transaction or DDL failure.
	pub async fn ensure_dictionary_tables(db: &turso::Database) -> Result<()> {
		let schema_conn = Database::begin_immediate(db).await?;
		Self::wireframe_dictionary_tables_direct(&schema_conn).await?;
		let _ = Database::commit_immediate(&schema_conn).await;
		Ok(())
	}

	#[instrument]
	async fn wireframe_dictionary_tables_direct(conn: &Connection) -> Result<()> {
		// Note: No PRIMARY KEY on TEXT columns or indexes to support MVCC
		Self::create_dictionary_metadata_table(conn).await?;
		Self::create_dictionary_constraints_table(conn).await?;
		Self::create_dictionary_variabilities_table(conn).await?;
		Self::create_dictionary_patterns_table(conn).await?;
		Self::create_patterns_table(conn).await?;
		Self::create_pattern_occurrences_table(conn).await?;
		Self::create_pattern_relatives_table(conn).await?;
		Ok(())
	}

	async fn create_dictionary_metadata_table(conn: &Connection) -> Result<()> {
		conn.as_ref()
			.execute(
				r"CREATE TABLE IF NOT EXISTS dictionary_metadata (
				id TEXT NOT NULL, name TEXT NOT NULL, description TEXT, created_at INTEGER NOT NULL)",
				turso::params![],
			)
			.await?;
		Ok(())
	}

	async fn create_dictionary_constraints_table(conn: &Connection) -> Result<()> {
		conn.as_ref()
			.execute(
				r"CREATE TABLE IF NOT EXISTS dictionary_constraints (
				id INTEGER PRIMARY KEY, dictionary_id TEXT NOT NULL,
				steps_count INTEGER, steps_interpolation TEXT)",
				turso::params![],
			)
			.await?;
		Ok(())
	}

	async fn create_dictionary_variabilities_table(conn: &Connection) -> Result<()> {
		conn.as_ref()
			.execute(
				r"CREATE TABLE IF NOT EXISTS dictionary_variabilities (
				id INTEGER PRIMARY KEY, dictionary_id TEXT NOT NULL,
				variability_type TEXT NOT NULL, variability_value TEXT NOT NULL)",
				turso::params![],
			)
			.await?;
		Ok(())
	}

	async fn create_dictionary_patterns_table(conn: &Connection) -> Result<()> {
		conn.as_ref()
			.execute(
				r"CREATE TABLE IF NOT EXISTS dictionary_patterns (
				id INTEGER PRIMARY KEY, dictionary_id TEXT NOT NULL,
				pattern_id TEXT NOT NULL, added_at INTEGER NOT NULL)",
				turso::params![],
			)
			.await?;
		Ok(())
	}

	async fn create_patterns_table(conn: &Connection) -> Result<()> {
		conn.as_ref()
			.execute(
				r"CREATE TABLE IF NOT EXISTS patterns (
				id TEXT NOT NULL, sum_value TEXT NOT NULL, abs_sum_value TEXT NOT NULL,
				max_value TEXT NOT NULL, min_value TEXT NOT NULL, abs_max_value TEXT NOT NULL,
				avg_value TEXT NOT NULL, abs_avg_value TEXT NOT NULL)",
				turso::params![],
			)
			.await?;
		Ok(())
	}

	async fn create_pattern_occurrences_table(conn: &Connection) -> Result<()> {
		conn.as_ref()
			.execute(
				r"CREATE TABLE IF NOT EXISTS pattern_occurrences (
				id INTEGER PRIMARY KEY, pattern_id TEXT NOT NULL,
				occurrence_index INTEGER NOT NULL, aspect_id TEXT NOT NULL, resolution TEXT NOT NULL,
				size INTEGER NOT NULL, database_info TEXT NOT NULL,
				beginning_timestamp INTEGER NOT NULL, end_timestamp INTEGER NOT NULL)",
				turso::params![],
			)
			.await?;
		Ok(())
	}

	async fn create_pattern_relatives_table(conn: &Connection) -> Result<()> {
		conn.as_ref()
			.execute(
				r"CREATE TABLE IF NOT EXISTS pattern_relatives (
				id INTEGER PRIMARY KEY, pattern_id TEXT NOT NULL,
				relative_index INTEGER NOT NULL, relative_value TEXT NOT NULL)",
				turso::params![],
			)
			.await?;
		Ok(())
	}
}

// ===== Compression configuration methods =====
impl Aspect {
	/// Ensure compression stats tables exist (for databases created before this feature)
	/// This is safe to call multiple times - uses CREATE TABLE IF NOT EXISTS
	///
	/// # Errors
	/// Returns an error if database operations fail
	#[instrument(skip(self))]
	pub async fn ensure_compression_tables(&mut self) -> Result<()> {
		tracing::info!(">>> ensure_compression_tables: getting measurements db");
		let db = self.measurements().await?;
		tracing::info!(">>> ensure_compression_tables: beginning immediate transaction");
		let schema_conn = Database::begin_immediate(&db).await?;
		tracing::info!(">>> ensure_compression_tables: wireframing tables");
		Self::wireframe_compression_stats_tables_direct(&schema_conn).await?;
		tracing::info!(">>> ensure_compression_tables: committing");
		let _ = Database::commit_immediate(&schema_conn).await;
		tracing::info!(">>> ensure_compression_tables: done");
		Ok(())
	}

	/// Get the compression configuration for this aspect.
	#[must_use]
	pub const fn compression_config(&self) -> Option<&CompressionConfig> {
		self.compression_config.as_ref()
	}

	/// Set the compression configuration for this aspect.
	/// This updates the in-memory configuration. Call `save_compression_config`
	/// to persist to the database.
	pub fn set_compression_config_local(&mut self, config: Option<CompressionConfig>) {
		self.compression_config = config;
	}

	/// Set the compression configuration and persist it to metadata.db.
	///
	/// # Errors
	///
	/// Returns an error if database operations fail.
	#[instrument(skip(self))]
	pub async fn set_compression_config(&mut self, config: Option<CompressionConfig>) -> Result<()> {
		tracing::info!(">>> set_compression_config: cloning config");
		self.compression_config.clone_from(&config);

		tracing::info!(">>> set_compression_config: getting turso database");
		// Persist to metadata.db
		let turso_db = Database::get_turso_database(&self.database_metadata_db_path).await?;
		tracing::info!(">>> set_compression_config: beginning concurrent transaction");
		let conn = Database::begin_concurrent(&turso_db, &self.database_metadata_db_path, None).await?;

		tracing::info!(">>> set_compression_config: serializing config json");
		let config_json = config.map(|c| serde_json::to_string(&c)).transpose()?;

		tracing::info!(">>> set_compression_config: executing UPDATE");
		let id_str = self.id.as_uuid().to_string();
		conn.as_ref().execute("UPDATE aspects SET compression_config = ? WHERE id = ?", turso::params![config_json, id_str]).await?;

		tracing::info!(">>> set_compression_config: committing");
		let _ = Database::commit_concurrent(&conn).await;
		// NOTE: Removed checkpoint_wal_passive() call - other metadata.db operations don't do this
		// and it may cause timing issues with MVCC transactions after long compression runs

		tracing::info!(">>> set_compression_config: done");
		Ok(())
	}

	/// Compress measurements according to the aspect's compression config.
	///
	/// Returns `CompressionSummary` with results from both time-based and size-based modes.
	/// If no compression config is set, returns an error.
	///
	/// # Execution Order
	/// 1. Apply time-based compression first (if configured)
	/// 2. Check if size constraint is met
	/// 3. If size still exceeds target, apply size-based compression (takes precedence)
	///
	/// # Errors
	///
	/// Returns an error if:
	/// - No compression config is set for this aspect
	/// - Database operations fail
	/// - Compression algorithm fails
	#[instrument(skip(self, database))]
	pub async fn compress(&mut self, database: &Database) -> Result<crate::compression::CompressionSummary> {
		// Delegate to compress_with_progress without a callback
		let detailed = self.compress_with_progress(database, None).await?;
		Ok(detailed.summary)
	}

	/// Compress measurements with optional progress reporting.
	///
	/// Returns `DetailedCompressionSummary` with per-tier results and timing information.
	/// If a progress callback is provided, it will be called before each tier/iteration.
	///
	/// # Arguments
	/// * `database` - Database connection for reading/writing measurements
	/// * `progress_callback` - Optional callback for progress updates
	///
	/// # Errors
	///
	/// Returns an error if:
	/// - No compression config is set for this aspect
	/// - Database operations fail
	/// - Compression algorithm fails
	#[instrument(skip(self, database, progress_callback))]
	pub async fn compress_with_progress(&mut self, database: &Database, progress_callback: Option<crate::compression::ProgressCallback>) -> Result<crate::compression::DetailedCompressionSummary> {
		use crate::{
			compression::{size, CompressionPhase, CompressionProgress, CompressionSummary, DetailedCompressionSummary, TierCompressionResult}, database::traits::Outputs
		};

		let started_at = chrono::Utc::now();
		let config = self.compression_config().cloned().ok_or_else(|| anyhow::anyhow!("No compression config set for this aspect"))?;

		if !config.enabled {
			tracing::info!("Compression is disabled for this aspect");
			return Ok(DetailedCompressionSummary { summary: CompressionSummary::default(), tier_results: Vec::new(), started_at, completed_at: chrono::Utc::now(), total_duration_ms: 0 });
		}

		// Report initialization
		if let Some(ref cb) = progress_callback {
			cb(CompressionProgress { phase: CompressionPhase::Initializing, current_tier: 0, total_tiers: 0, aggressiveness: 0.0, time_range_start: chrono::Utc::now(), time_range_end: chrono::Utc::now() });
		}

		let mut summary = CompressionSummary::default();
		let mut tier_results: Vec<TierCompressionResult> = Vec::new();

		// 1. Apply time-based compression first (if configured)
		if let Some(ref time_config) = config.time_based {
			tracing::info!("Starting time-based compression");
			let (results, tier_details) = self.compress_time_based_with_progress(database, time_config, &config, progress_callback.clone()).await?;
			summary.time_based_results = results;
			tier_results.extend(tier_details);
		}

		// 2. Apply size-based compression (takes precedence if size still exceeded)
		if let Some(ref size_config) = config.size_based {
			// Check current size
			let measurement_count = database.get_measurements_count(&self.id).await?;
			let size_estimate = size::SizeEstimate::from_count(measurement_count);
			let current_size = size_estimate.best_estimate();

			if current_size > size_config.target_size_bytes {
				tracing::info!(current_size = current_size, target_size = size_config.target_size_bytes, "Size exceeds target, applying size-based compression");
				let (results, tier_details) = self.compress_size_based_with_progress(database, size_config, &config, progress_callback.clone()).await?;
				summary.size_based_results = results;
				tier_results.extend(tier_details);
			} else {
				tracing::info!(current_size = current_size, target_size = size_config.target_size_bytes, "Size is within target, skipping size-based compression");
			}
		}

		// Report finalizing
		if let Some(ref cb) = progress_callback {
			cb(CompressionProgress { phase: CompressionPhase::Finalizing, current_tier: 0, total_tiers: 0, aggressiveness: 0.0, time_range_start: chrono::Utc::now(), time_range_end: chrono::Utc::now() });
		}

		// Calculate totals
		summary.calculate_totals();

		// Get final size estimate
		let final_count = database.get_measurements_count(&self.id).await?;
		summary.final_size_bytes = size::SizeEstimate::from_count(final_count).best_estimate();

		let completed_at = chrono::Utc::now();
		let total_duration_ms = (completed_at - started_at).num_milliseconds().max(0) as u64;

		// Report complete
		tracing::info!(">>> About to send CompressionPhase::Complete callback");
		if let Some(ref cb) = progress_callback {
			cb(CompressionProgress { phase: CompressionPhase::Complete, current_tier: 0, total_tiers: 0, aggressiveness: 0.0, time_range_start: started_at, time_range_end: completed_at });
		}
		tracing::info!(">>> Callback sent, preparing return value");

		tracing::info!(
			original = summary.total_original_count,
			compressed = summary.total_compressed_count,
			ratio = %summary.overall_compression_ratio(),
			duration_ms = total_duration_ms,
			"Compression complete"
		);

		tracing::info!(">>> Building DetailedCompressionSummary");
		let result = DetailedCompressionSummary { summary, tier_results, started_at, completed_at, total_duration_ms };
		tracing::info!(">>> Returning from compress_with_progress");
		Ok(result)
	}

	/// Apply time-based compression to measurements older than the pure duration.
	#[allow(dead_code)] // Public API for time-based compression
	async fn compress_time_based(&self, database: &Database, time_config: &crate::compression::TimeBasedCompressionConfig, config: &CompressionConfig) -> Result<Vec<crate::compression::CompressionResult>> {
		let (results, _) = self.compress_time_based_with_progress(database, time_config, config, None).await?;
		Ok(results)
	}

	/// Apply time-based compression with progress reporting.
	async fn compress_time_based_with_progress(&self, database: &Database, time_config: &crate::compression::TimeBasedCompressionConfig, config: &CompressionConfig, progress_callback: Option<crate::compression::ProgressCallback>) -> Result<(Vec<crate::compression::CompressionResult>, Vec<crate::compression::TierCompressionResult>)> {
		use crate::compression::{calculate_tier_aggressiveness, CompressionPhase, CompressionProgress, TierCompressionResult};

		let now = chrono::Utc::now();
		let pure_cutoff = now - time_config.pure_duration;

		// Get the earliest measurement timestamp
		let earliest_opt = database.get_earliest_measurement(&self.id).await?;
		let Some(earliest) = earliest_opt else {
			tracing::info!("No measurements found, skipping time-based compression");
			return Ok((Vec::new(), Vec::new()));
		};

		if earliest >= pure_cutoff {
			tracing::info!("All data is within pure duration, skipping time-based compression");
			return Ok((Vec::new(), Vec::new()));
		}

		// Calculate total number of tiers upfront for progress reporting
		let total_duration = pure_cutoff - earliest;
		let tier_duration_millis = time_config.tier_duration.num_milliseconds().max(1);
		let total_duration_millis = total_duration.num_milliseconds().max(0);
		let total_tiers = ((total_duration_millis as f64 / tier_duration_millis as f64).ceil() as u32).min(time_config.max_tiers);

		let mut results = Vec::new();
		let mut tier_details = Vec::new();
		let mut tier_start = earliest;
		let mut current_tier = 1u32;

		while tier_start < pure_cutoff && current_tier <= time_config.max_tiers {
			let tier_end = (tier_start + time_config.tier_duration).min(pure_cutoff);

			// Calculate aggressiveness for this tier
			let data_age = now - tier_start;
			let aggressiveness = calculate_tier_aggressiveness(data_age, time_config.pure_duration, time_config.tier_duration, time_config.max_tiers, &time_config.scaling);

			// Report progress before each tier
			if let Some(ref cb) = progress_callback {
				cb(CompressionProgress { phase: CompressionPhase::TimeBased { tier: current_tier, of_tiers: total_tiers }, current_tier, total_tiers, aggressiveness, time_range_start: tier_start, time_range_end: tier_end });
			}

			if aggressiveness > 0.0 {
				tracing::debug!(
					tier = current_tier,
					aggressiveness = %aggressiveness,
					start = %tier_start,
					end = %tier_end,
					"Compressing tier"
				);

				let tier_start_time = std::time::Instant::now();
				let result = self.compress_time_range(database, tier_start, tier_end, aggressiveness, config).await?;
				let tier_duration_ms = tier_start_time.elapsed().as_millis() as u64;

				if result.original_count > 0 {
					tracing::info!(
						tier = current_tier,
						aggressiveness = %aggressiveness,
						original = result.original_count,
						compressed = result.compressed_count,
						"Tier compression complete"
					);

					// Record detailed tier result
					tier_details.push(TierCompressionResult { tier_number: current_tier, phase: CompressionPhase::TimeBased { tier: current_tier, of_tiers: total_tiers }, original_count: result.original_count, compressed_count: result.compressed_count, compression_ratio: result.compression_ratio, aggressiveness, time_range_start: result.time_range_start, time_range_end: result.time_range_end, duration_ms: tier_duration_ms });

					results.push(result);
				}
			}

			tier_start = tier_end;
			current_tier += 1;
		}

		Ok((results, tier_details))
	}

	/// Apply size-based compression, iteratively compressing oldest data until target size is reached.
	#[allow(dead_code)] // Public API for size-based compression
	async fn compress_size_based(&self, database: &Database, size_config: &crate::compression::SizeBasedCompressionConfig, config: &CompressionConfig) -> Result<Vec<crate::compression::CompressionResult>> {
		let (results, _) = self.compress_size_based_with_progress(database, size_config, config, None).await?;
		Ok(results)
	}

	/// Apply size-based compression with progress reporting.
	async fn compress_size_based_with_progress(&self, database: &Database, size_config: &crate::compression::SizeBasedCompressionConfig, config: &CompressionConfig, progress_callback: Option<crate::compression::ProgressCallback>) -> Result<(Vec<crate::compression::CompressionResult>, Vec<crate::compression::TierCompressionResult>)> {
		use crate::{
			compression::{size, CompressionPhase, CompressionProgress, TierCompressionResult}, database::traits::Outputs
		};

		let mut results = Vec::new();
		let mut tier_details = Vec::new();
		let mut iteration = 0u32;

		while iteration < size_config.max_iterations {
			let measurement_count = database.get_measurements_count(&self.id).await?;
			let current_size = size::SizeEstimate::from_count(measurement_count).best_estimate();

			if current_size <= size_config.target_size_bytes {
				tracing::info!(current_size = current_size, target_size = size_config.target_size_bytes, iteration = iteration, "Target size achieved");
				break;
			}

			// Calculate required compression
			let requirement = size::calculate_required_compression(current_size, size_config.target_size_bytes, measurement_count);

			// Map required reduction to aggressiveness
			let aggressiveness = requirement.required_reduction_ratio.mul_add(size_config.max_aggressiveness - size_config.min_aggressiveness, size_config.min_aggressiveness).clamp(size_config.min_aggressiveness, size_config.max_aggressiveness);

			// Compress oldest 25% of data
			let earliest = database.get_earliest_measurement(&self.id).await?;
			let latest = database.get_latest_measurement(&self.id).await?;

			match (earliest, latest) {
				(Some(start), Some(end)) => {
					let total_duration = end - start;
					let chunk_duration = total_duration / 4; // 25% of data
					let chunk_end = start + chunk_duration;

					// Report progress before each iteration
					if let Some(ref cb) = progress_callback {
						cb(CompressionProgress { phase: CompressionPhase::SizeBased { iteration: iteration + 1 }, current_tier: iteration + 1, total_tiers: size_config.max_iterations, aggressiveness, time_range_start: start, time_range_end: chunk_end });
					}

					tracing::debug!(
						iteration = iteration,
						aggressiveness = %aggressiveness,
						start = %start,
						end = %chunk_end,
						"Size-based compression iteration"
					);

					let iter_start_time = std::time::Instant::now();
					let result = self.compress_time_range(database, start, chunk_end, aggressiveness, config).await?;
					let iter_duration_ms = iter_start_time.elapsed().as_millis() as u64;

					if result.original_count > 0 {
						tracing::info!(
							iteration = iteration,
							aggressiveness = %aggressiveness,
							original = result.original_count,
							compressed = result.compressed_count,
							"Size-based iteration complete"
						);

						// Record detailed tier result
						tier_details.push(TierCompressionResult { tier_number: iteration + 1, phase: CompressionPhase::SizeBased { iteration: iteration + 1 }, original_count: result.original_count, compressed_count: result.compressed_count, compression_ratio: result.compression_ratio, aggressiveness, time_range_start: result.time_range_start, time_range_end: result.time_range_end, duration_ms: iter_duration_ms });

						results.push(result);
					} else {
						// No more data to compress
						break;
					}
				}
				_ => break,
			}

			iteration += 1;
		}

		Ok((results, tier_details))
	}

	/// Compress a specific time range with the given aggressiveness.
	async fn compress_time_range(&self, database: &Database, start: chrono::DateTime<chrono::Utc>, end: chrono::DateTime<chrono::Utc>, aggressiveness: f64, config: &CompressionConfig) -> Result<crate::compression::CompressionResult> {
		use futures::StreamExt;

		use crate::{
			compression::{algorithm, CompressionResult}, database::traits::{Inputs, Outputs}
		};

		// Get measurements in range
		let mut stream = database.fetch_measurements_for_range(&self.id, start, end).await?;
		let mut measurements: Vec<crate::Measurement> = vec![];
		while let Some(m) = stream.next().await {
			measurements.push(m?);
		}

		if measurements.len() <= 2 {
			return Ok(CompressionResult::new(measurements.len(), measurements.len(), start, end));
		}

		let original_count = measurements.len();

		// Get dataset_id from first measurement
		let dataset_id = measurements[0].dataset_id();

		// Apply simplification
		let compressed_points = algorithm::simplify_with_aggressiveness(&measurements, aggressiveness, config.base_resolution, self.resolution, config.interpolation_method).await?;

		// Convert back to InputMeasurement
		let new_measurements: Vec<crate::InputMeasurement> = compressed_points.iter().map(|p| crate::InputMeasurement::new(p.timestamp, p.value.clone())).collect();

		let compressed_count = new_measurements.len();

		// Replace in database
		database.replace_measurements_in_range(&self.id, &dataset_id, start, end, new_measurements).await?;

		Ok(CompressionResult::new(original_count, compressed_count, start, end))
	}
}

// ===== Compression history and dirty region tracking =====
impl Aspect {
	/// Save compression run to history and update compression state
	///
	/// # Arguments
	/// * `summary` - The detailed compression summary to save
	/// * `config` - The compression config used for this run
	///
	/// # Returns
	/// The unique ID for this compression run
	///
	/// # Errors
	/// Returns an error if database operations fail
	#[instrument(skip(self, summary, config))]
	pub async fn save_compression_history(&mut self, summary: &crate::compression::DetailedCompressionSummary, config: &crate::compression::CompressionConfig) -> Result<String> {
		tracing::info!(">>> save_compression_history: getting measurements db");
		let db = self.measurements().await?;
		tracing::info!(">>> save_compression_history: beginning concurrent transaction");
		let conn = Database::begin_concurrent(&db, &self.measurements_path, None).await?;
		tracing::info!(">>> save_compression_history: generating id and config json");

		let id = uuid::Uuid::new_v4().to_string();
		let config_json = serde_json::to_string(config)?;

		// Insert into compression_history
		conn.as_ref().execute("INSERT INTO compression_history (id, started_at, completed_at, duration_ms, original_count, compressed_count, compression_ratio, final_size_bytes, time_based_tiers, size_based_iterations, config_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)", turso::params![id.clone(), summary.started_at.timestamp_millis(), summary.completed_at.timestamp_millis(), summary.total_duration_ms as i64, summary.summary.total_original_count as i64, summary.summary.total_compressed_count as i64, summary.summary.overall_compression_ratio(), summary.summary.final_size_bytes as i64, summary.summary.time_based_results.len() as i64, summary.summary.size_based_results.len() as i64, config_json,]).await?;

		// Insert tier results
		for tier_result in &summary.tier_results {
			let phase_str = format!("{}", tier_result.phase);
			conn.as_ref().execute("INSERT INTO compression_tier_results (history_id, tier_number, phase, original_count, compressed_count, compression_ratio, aggressiveness, time_range_start, time_range_end, duration_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)", turso::params![id.clone(), i64::from(tier_result.tier_number), phase_str, tier_result.original_count as i64, tier_result.compressed_count as i64, tier_result.compression_ratio, tier_result.aggressiveness, tier_result.time_range_start.timestamp_millis(), tier_result.time_range_end.timestamp_millis(), tier_result.duration_ms as i64,]).await?;
		}

		// Update compression_state singleton
		conn.as_ref().execute("UPDATE compression_state SET is_compressing = 0, current_phase = NULL, current_tier = NULL, total_tiers = NULL, last_compression_id = ?, last_compression_at = ? WHERE id = 1", turso::params![id.clone(), summary.completed_at.timestamp_millis()]).await?;

		let _ = Database::commit_concurrent(&conn).await;
		// NOTE: Removed checkpoint_wal_passive() call for consistency with other db operations

		tracing::info!(compression_id = %id, "Saved compression history");
		Ok(id)
	}

	/// Get the last compression info for UI display
	///
	/// # Returns
	/// The last compression info, or None if no compression has been run
	///
	/// # Errors
	/// Returns an error if database operations fail
	#[instrument(skip(self))]
	pub async fn get_last_compression(&mut self) -> Result<Option<crate::compression::LastCompressionInfo>> {
		let db = self.measurements().await?;
		let conn = Database::begin_concurrent(&db, &self.measurements_path, None).await?;

		let mut rows = conn.as_ref().query("SELECT id, completed_at, original_count, compressed_count, compression_ratio, final_size_bytes, time_based_tiers, size_based_iterations, duration_ms FROM compression_history ORDER BY completed_at DESC LIMIT 1", turso::params![]).await?;

		let result = if let Some(row) = rows.next().await? {
			let id = row.get_value(0)?.as_text().cloned().unwrap_or_default();
			let completed_at_millis = *row.get_value(1)?.as_integer().unwrap_or(&0);
			let original_count = *row.get_value(2)?.as_integer().unwrap_or(&0) as usize;
			let compressed_count = *row.get_value(3)?.as_integer().unwrap_or(&0) as usize;
			let compression_ratio = *row.get_value(4)?.as_real().unwrap_or(&0.0);
			let final_size_bytes = *row.get_value(5)?.as_integer().unwrap_or(&0) as u64;
			let time_based_tiers = *row.get_value(6)?.as_integer().unwrap_or(&0) as usize;
			let size_based_iterations = *row.get_value(7)?.as_integer().unwrap_or(&0) as usize;
			let duration_ms = *row.get_value(8)?.as_integer().unwrap_or(&0) as u64;

			Some(crate::compression::LastCompressionInfo { id, completed_at: chrono::DateTime::from_timestamp_millis(completed_at_millis).unwrap_or_default(), original_count, compressed_count, compression_ratio, final_size_bytes, time_based_tiers, size_based_iterations, duration_ms })
		} else {
			None
		};

		let _ = Database::commit_concurrent(&conn).await;
		Ok(result)
	}

	/// Get the count of dirty regions that need recompression
	///
	/// # Errors
	/// Returns an error if database operations fail
	#[instrument(skip(self))]
	pub async fn get_dirty_regions_count(&mut self) -> Result<usize> {
		let db = self.measurements().await?;
		let conn = Database::begin_concurrent(&db, &self.measurements_path, None).await?;

		let mut rows = conn.as_ref().query("SELECT COUNT(*) FROM dirty_regions", turso::params![]).await?;

		let count = if let Some(row) = rows.next().await? { *row.get_value(0)?.as_integer().unwrap_or(&0) as usize } else { 0 };

		let _ = Database::commit_concurrent(&conn).await;
		Ok(count)
	}

	/// Get list of dirty regions that need recompression
	///
	/// # Errors
	/// Returns an error if database operations fail
	#[instrument(skip(self))]
	pub async fn get_dirty_regions(&mut self) -> Result<Vec<crate::compression::DirtyRegion>> {
		let db = self.measurements().await?;
		let conn = Database::begin_concurrent(&db, &self.measurements_path, None).await?;

		let mut rows = conn.as_ref().query("SELECT id, region_start, region_end, marked_at, reason FROM dirty_regions ORDER BY region_start ASC", turso::params![]).await?;

		let mut regions = Vec::new();
		while let Some(row) = rows.next().await? {
			let id = *row.get_value(0)?.as_integer().unwrap_or(&0);
			let region_start_millis = *row.get_value(1)?.as_integer().unwrap_or(&0);
			let region_end_millis = *row.get_value(2)?.as_integer().unwrap_or(&0);
			let marked_at_millis = *row.get_value(3)?.as_integer().unwrap_or(&0);
			let reason = row.get_value(4)?.as_text().cloned().unwrap_or_default();

			regions.push(crate::compression::DirtyRegion { id, region_start: chrono::DateTime::from_timestamp_millis(region_start_millis).unwrap_or_default(), region_end: chrono::DateTime::from_timestamp_millis(region_end_millis).unwrap_or_default(), marked_at: chrono::DateTime::from_timestamp_millis(marked_at_millis).unwrap_or_default(), reason });
		}

		let _ = Database::commit_concurrent(&conn).await;
		Ok(regions)
	}

	/// Mark a region as dirty (needs recompression)
	///
	/// This is called when new measurements are inserted into a time range that has
	/// already been compressed, indicating that recompression may be needed.
	///
	/// # Arguments
	/// * `region_start` - Start of the dirty region
	/// * `region_end` - End of the dirty region
	/// * `reason` - Why the region was marked dirty
	///
	/// # Errors
	/// Returns an error if database operations fail
	#[instrument(skip(self))]
	pub async fn mark_dirty_region(&mut self, region_start: chrono::DateTime<chrono::Utc>, region_end: chrono::DateTime<chrono::Utc>, reason: &str) -> Result<()> {
		let db = self.measurements().await?;
		let conn = Database::begin_concurrent(&db, &self.measurements_path, None).await?;

		let now = chrono::Utc::now();

		// Insert the dirty region
		conn.as_ref().execute("INSERT INTO dirty_regions (region_start, region_end, marked_at, reason) VALUES (?, ?, ?, ?)", turso::params![region_start.timestamp_millis(), region_end.timestamp_millis(), now.timestamp_millis(), reason.to_string(),]).await?;

		// Update dirty_regions_count in compression_state
		conn.as_ref().execute("UPDATE compression_state SET dirty_regions_count = (SELECT COUNT(*) FROM dirty_regions) WHERE id = 1", turso::params![]).await?;

		let _ = Database::commit_concurrent(&conn).await;
		Database::checkpoint_wal_passive(&db).await?;

		tracing::debug!(
			region_start = %region_start,
			region_end = %region_end,
			reason = %reason,
			"Marked dirty region"
		);

		Ok(())
	}

	/// Clear dirty regions that were marked before a given time
	///
	/// Called after successful recompression to clear the dirty flags.
	///
	/// # Arguments
	/// * `before` - Clear regions marked before this time
	///
	/// # Errors
	/// Returns an error if database operations fail
	#[instrument(skip(self))]
	pub async fn clear_dirty_regions(&mut self, before: chrono::DateTime<chrono::Utc>) -> Result<()> {
		let db = self.measurements().await?;
		let conn = Database::begin_concurrent(&db, &self.measurements_path, None).await?;

		conn.as_ref().execute("DELETE FROM dirty_regions WHERE marked_at < ?", turso::params![before.timestamp_millis()]).await?;

		// Update dirty_regions_count in compression_state
		conn.as_ref().execute("UPDATE compression_state SET dirty_regions_count = (SELECT COUNT(*) FROM dirty_regions) WHERE id = 1", turso::params![]).await?;

		let _ = Database::commit_concurrent(&conn).await;
		Database::checkpoint_wal_passive(&db).await?;

		tracing::debug!(before = %before, "Cleared dirty regions");
		Ok(())
	}

	/// Check if a timestamp falls within a previously compressed time range
	///
	/// # Arguments
	/// * `timestamp` - The timestamp to check
	///
	/// # Returns
	/// Some((start, end)) if the timestamp is in a compressed range, None otherwise
	///
	/// # Errors
	/// Returns an error if database operations fail
	#[instrument(skip(self))]
	pub async fn get_compressed_range_containing(&mut self, timestamp: chrono::DateTime<chrono::Utc>) -> Result<Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>> {
		let db = self.measurements().await?;
		let conn = Database::begin_concurrent(&db, &self.measurements_path, None).await?;

		// Query tier results to find if timestamp falls within any compressed range
		let mut rows = conn.as_ref().query("SELECT time_range_start, time_range_end FROM compression_tier_results WHERE time_range_start <= ? AND time_range_end >= ? LIMIT 1", turso::params![timestamp.timestamp_millis(), timestamp.timestamp_millis()]).await?;

		let result = if let Some(row) = rows.next().await? {
			let start_millis = *row.get_value(0)?.as_integer().unwrap_or(&0);
			let end_millis = *row.get_value(1)?.as_integer().unwrap_or(&0);
			Some((chrono::DateTime::from_timestamp_millis(start_millis).unwrap_or_default(), chrono::DateTime::from_timestamp_millis(end_millis).unwrap_or_default()))
		} else {
			None
		};

		let _ = Database::commit_concurrent(&conn).await;
		Ok(result)
	}
}

impl PartialEq for Aspect {
	fn eq(&self, other: &Self) -> bool {
		self.id == other.id && self.name == other.name && self.subject_id == other.subject_id && self.resolution == other.resolution
		// Skip measurements_turso_db comparison since it doesn't implement PartialEq
	}
}
