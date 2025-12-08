use std::{collections::HashMap, fmt::Display};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use splimes::Resolution;
use turso::Database as TursoDatabase;
use uuid::Uuid;

use crate::{
	Database, DatabaseStructure, DictionaryConstraints, DictionaryId, SubjectId, cache::Connection, database::dictionaries, types::{database::{
		Config, traits::{aspect_structure::AspectStructure, connection::Connection as ConnectionTrait}
	}, dictionary::VariablilityType}
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
	events: Option<TursoDatabase>,
	events_path: String,

	#[serde(skip)]
	correlations: Option<TursoDatabase>,
	correlations_path: String,

        #[serde(skip)]
        dictionaries: Option<HashMap<String, TursoDatabase>>,
        dictionaries_paths: HashMap<String, String>,
}

#[async_trait::async_trait]
impl AspectStructure for Aspect {
	async fn new(id: Option<AspectId>, name: &str, subject_id: &SubjectId, resolution: &Resolution, metadata_conn: &Connection) -> Result<Self> {
		let id = id.unwrap_or_default();
		let subject_name = Self::get_subject_name(metadata_conn, subject_id).await?;
		let db_name = <Database as Config>::db_name(metadata_conn).await?;
		let database_metadata_db_path = Database::db_metadata_path(metadata_conn).await?;
		let aspect_path = Database::aspect_path(&db_name, &subject_name, name);

		// recursively create directory if it doesn't exist
		tokio::fs::create_dir_all(&aspect_path).await?;

		// Create databases sequentially to avoid lock contention during concurrent aspect creation
		// The databases themselves will use MVCC for internal concurrency
		let measurements_path = Database::aspect_measurements_db_path(&db_name, &subject_name, name);
		println!("[TRACE] Creating measurements DB at: {measurements_path}");
		let measurements = Database::get_or_create_turso_database(&measurements_path).await?;
		println!("[TRACE] Connected measurements DB: {measurements_path}");
		let conn = Database::begin_concurrent(&measurements, &measurements_path, None).await?;
		println!("[TRACE] Wireframing measurements tables for: {measurements_path}");
		Self::wireframe_measurements_tables(&conn).await?;
		let _ = Database::commit_concurrent(&conn).await;
		println!("[TRACE] Wireframed measurements tables for: {measurements_path}");
		let measurements = Some(measurements);

		let unprocessed_batches_path = Database::aspect_unprocessed_batches_db_path(&db_name, &subject_name, name);
		println!("[TRACE] Creating unprocessed_batches DB at: {unprocessed_batches_path}");
		let unprocessed_batches = Database::get_or_create_turso_database(&unprocessed_batches_path).await?;
		println!("[TRACE] Connected unprocessed_batches DB: {unprocessed_batches_path}");
		let conn = Database::begin_concurrent(&unprocessed_batches, &unprocessed_batches_path, None).await?;
		println!("[TRACE] Wireframing batches tables for: {unprocessed_batches_path}");
		Self::wireframe_batches_tables(&conn).await?;
		println!("[TRACE] Wireframed batches tables for: {unprocessed_batches_path}");
		let unprocessed_batches = Some(unprocessed_batches);
		let _ = Database::commit_concurrent(&conn).await;

		let processed_batches_path = Database::aspect_processed_batches_db_path(&db_name, &subject_name, name);
		println!("[TRACE] Creating processed_batches DB at: {processed_batches_path}");
		let processed_batches = Database::get_or_create_turso_database(&processed_batches_path).await?;
		println!("[TRACE] Connected processed_batches DB: {processed_batches_path}");
		let conn = Database::begin_concurrent(&processed_batches, &processed_batches_path, None).await?;
		println!("[TRACE] Wireframing batches tables for: {processed_batches_path}");
		Self::wireframe_batches_tables(&conn).await?;
		println!("[TRACE] Wireframed batches tables for: {processed_batches_path}");
		let processed_batches = Some(processed_batches);
		let _ = Database::commit_concurrent(&conn).await;

		let patterns_path = Database::aspect_patterns_db_path(&db_name, &subject_name, name);
		println!("[TRACE] Creating patterns DB at: {patterns_path}");
		let patterns = Database::get_or_create_turso_database(&patterns_path).await?;
		println!("[TRACE] Connected patterns DB: {patterns_path}");
		let conn = Database::begin_concurrent(&patterns, &patterns_path, None).await?;
		println!("[TRACE] Wireframing patterns tables for: {patterns_path}");
		Self::wireframe_patterns_tables(&conn).await?;
		println!("[TRACE] Wireframed patterns tables for: {patterns_path}");
		let patterns = Some(patterns);
		let _ = Database::commit_concurrent(&conn).await;

		let events_path = Database::aspect_events_db_path(&db_name, &subject_name, name);
		println!("[TRACE] Creating events DB at: {events_path}");
		let events = Database::get_or_create_turso_database(&events_path).await?;
		println!("[TRACE] Connected events DB: {events_path}");
		let conn = Database::begin_concurrent(&events, &events_path, None).await?;
		println!("[TRACE] Wireframing events tables for: {events_path}");
		Self::wireframe_events_tables(&conn).await?;
		println!("[TRACE] Wireframed events tables for: {events_path}");
		let events = Some(events);
		let _ = Database::commit_concurrent(&conn).await;

		let correlations_path = Database::aspect_correlations_db_path(&db_name, &subject_name, name);
		println!("[TRACE] Creating correlations DB at: {correlations_path}");
		let correlations = Database::get_or_create_turso_database(&correlations_path).await?;
		println!("[TRACE] Connected correlations DB: {correlations_path}");
		let conn = Database::begin_concurrent(&correlations, &correlations_path, None).await?;
		println!("[TRACE] Wireframing correlations tables for: {correlations_path}");
		Self::wireframe_correlations_tables(&conn).await?;
		println!("[TRACE] Wireframed correlations tables for: {correlations_path}");
		let correlations = Some(correlations);
		let _ = Database::commit_concurrent(&conn).await;

		let dictionaries_path = <Database as Config>::aspect_dictionaries_path(&db_name, &subject_name, name);
		println!("[TRACE] Creating dictionaries directory at: {dictionaries_path}");
		std::fs::create_dir_all(&dictionaries_path)?;

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
			events,
			events_path,
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
		let events_path = aspect_path_str.clone() + "/events.db";
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
                                                if file_name.ends_with(".db") {
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
                        events: None,
                        correlations: None,
                        measurements_path,
                        unprocessed_batches_path,
                        processed_batches_path,
                        patterns_path,
                        events_path,
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

	async fn database_metadata(&self) -> Result<TursoDatabase> {
		let turso_db = Database::get_turso_database(&self.database_metadata_db_path).await.unwrap();
		Ok(turso_db)
	}

	async fn get_database_metadata_path(turso_db_path: String) -> Result<String> {
		let turso_db = Database::get_turso_database(&turso_db_path).await?;

		// Query the database for the metadata_path field of the first item in the database table
		let conn = turso_db.connect()?;
		let mut rows = conn.query("SELECT metadata_path FROM database", turso::params![]).await?;
		let row = rows.next().await?.ok_or_else(|| anyhow::anyhow!("Database metadata not found"))?;
		let metadata_path: String = row.get(0)?;

		Ok(metadata_path)
	}

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

	async fn get_aspect_path(conn: &Connection, metadata_path: &str, subject_id: &SubjectId, aspect_name: &str) -> Result<String> {
		let subject_name = Self::get_subject_name(conn, subject_id).await?;
		let aspect_metadata_path = std::path::Path::new(&metadata_path).parent().ok_or_else(|| anyhow::anyhow!("Cannot determine database directory"))?.join(subject_name).join(aspect_name);
		if aspect_metadata_path.exists() {
			Ok(aspect_metadata_path.to_string_lossy().to_string())
		} else {
			Err(anyhow::anyhow!("Aspect metadata path does not exist"))
		}
	}

	async fn measurements(&mut self) -> Result<TursoDatabase> {
		if let Some(ref db) = self.measurements {
			return Ok(db.clone());
		}

		// Lazily initialize the measurements database
		let measurements = Database::get_or_create_turso_database(&self.measurements_path).await?;
		let conn = Database::begin_concurrent(&measurements, &self.measurements_path, None).await?;
		Self::wireframe_measurements_tables(&conn).await?;
		let _ = Database::commit_concurrent(&conn).await;

		self.measurements = Some(measurements.clone());
		Ok(measurements)
	}

	fn set_measurements(&mut self, turso_db: TursoDatabase) {
		self.measurements = Some(turso_db);
	}

	fn measurements_path(&self) -> String {
		self.measurements_path.clone()
	}

	async fn set_measurements_path(&mut self, path: String) {
		self.measurements_path = path;
	}

	async fn wireframe_measurements_tables(conn: &Connection) -> Result<()> {
		conn.as_ref()
			.execute(
				"CREATE TABLE IF NOT EXISTS measurements (
				id TEXT PRIMARY KEY,
				dataset_id TEXT NOT NULL,
				timestamp INTEGER NOT NULL UNIQUE,
				value TEXT NOT NULL
			)",
				turso::params![],
			)
			.await?;

		// Add index for common queries
		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_measurements_dataset_timestamp ON measurements(dataset_id, timestamp)", turso::params![]).await?;

		Ok(())
	}

	async fn unprocessed_batches(&mut self) -> Result<TursoDatabase> {
		if let Some(ref db) = self.unprocessed_batches {
			return Ok(db.clone());
		}

		// Lazily initialize the unprocessed_batches database
		let unprocessed_batches = Database::get_or_create_turso_database(&self.unprocessed_batches_path).await?;
		let conn = Database::begin_concurrent(&unprocessed_batches, &self.unprocessed_batches_path, None).await?;
		Self::wireframe_batches_tables(&conn).await?;
		let _ = Database::commit_concurrent(&conn).await;

		self.unprocessed_batches = Some(unprocessed_batches.clone());
		Ok(unprocessed_batches)
	}

	fn set_unprocessed_batches(&mut self, turso_db: TursoDatabase) {
		self.unprocessed_batches = Some(turso_db);
	}

	fn unprocessed_batches_path(&self) -> String {
		self.unprocessed_batches_path.clone()
	}

	async fn wireframe_batches_tables(conn: &Connection) -> Result<()> {
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS batches (
				id TEXT PRIMARY KEY,
				aspect_id TEXT NOT NULL,
				database_id TEXT NOT NULL,
				size INTEGER NOT NULL,
				resolution TEXT NOT NULL,
				batch_hash TEXT,
				created_at INTEGER NOT NULL,
				updated_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000),

				metadata_json TEXT NOT NULL,
				measurements_json TEXT NOT NULL,

				FOREIGN KEY (aspect_id) REFERENCES aspects(id)
			)",
				turso::params![],
			)
			.await?;

		// Add unique constraint for batch_hash to prevent duplicate batches
		conn.as_ref().execute("CREATE UNIQUE INDEX IF NOT EXISTS idx_batches_unique_hash ON batches(batch_hash) WHERE batch_hash IS NOT NULL", turso::params![]).await?;

		// Enhanced indexes for concurrent write scenarios and queue processing
		// OPTIMIZATION: Composite index for the exact query pattern used in get_unprocessed_batches
		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_batches_aspect_database_created ON batches(aspect_id, database_id, created_at)", turso::params![]).await?;

		// Keep individual indexes for other query patterns
		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_batches_created_at ON batches(created_at)", turso::params![]).await?;
		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_batches_hash ON batches(batch_hash) WHERE batch_hash IS NOT NULL", turso::params![]).await?;

		let _ = Database::commit_concurrent(conn).await;

		Ok(())
	}

	async fn set_unprocessed_batches_path(&mut self, path: String) {
		self.unprocessed_batches_path = path;
	}

	async fn processed_batches(&mut self) -> Result<TursoDatabase> {
		if let Some(ref db) = self.processed_batches {
			return Ok(db.clone());
		}

		// Lazily initialize the processed_batches database
		let processed_batches = Database::get_or_create_turso_database(&self.processed_batches_path).await?;
		let conn = Database::begin_concurrent(&processed_batches, &self.processed_batches_path, None).await?;
		Self::wireframe_batches_tables(&conn).await?;
		let _ = Database::commit_concurrent(&conn).await;

		self.processed_batches = Some(processed_batches.clone());
		Ok(processed_batches)
	}

	fn set_processed_batches(&mut self, turso_db: TursoDatabase) {
		self.processed_batches = Some(turso_db);
	}

	fn processed_batches_path(&self) -> String {
		self.processed_batches_path.clone()
	}

	async fn set_processed_batches_path(&mut self, path: String) {
		self.processed_batches_path = path;
	}

	async fn patterns(&mut self) -> Result<TursoDatabase> {
		if let Some(ref db) = self.patterns {
			return Ok(db.clone());
		}

		// Lazily initialize the patterns database
		let patterns = Database::get_or_create_turso_database(&self.patterns_path).await?;
		let conn = Database::begin_concurrent(&patterns, &self.patterns_path, None).await?;
		Self::wireframe_patterns_tables(&conn).await?;
		let _ = Database::commit_concurrent(&conn).await;

		self.patterns = Some(patterns.clone());
		Ok(patterns)
	}

	fn set_patterns(&mut self, turso_db: TursoDatabase) {
		self.patterns = Some(turso_db);
	}

	fn patterns_path(&self) -> String {
		self.patterns_path.clone()
	}

	async fn set_patterns_path(&mut self, path: String) {
		self.patterns_path = path;
	}

	async fn wireframe_patterns_tables(conn: &Connection) -> Result<()> {
		// Main patterns table with precomputed statistics
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS patterns (
				id TEXT PRIMARY KEY,

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

		// Pattern occurrences table
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS pattern_occurrences (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				pattern_id TEXT NOT NULL,
				aspect_id TEXT NOT NULL,
				resolution TEXT NOT NULL,
				size INTEGER NOT NULL,
				database_info TEXT NOT NULL, -- JSON blob
				beginning_timestamp INTEGER NOT NULL,
				end_timestamp INTEGER NOT NULL,

				FOREIGN KEY (pattern_id) REFERENCES patterns(id) ON DELETE CASCADE
			)
			",
				turso::params![],
			)
			.await?;

		// Pattern relatives table
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
				max_y TEXT NOT NULL,

				FOREIGN KEY (pattern_id) REFERENCES patterns(id) ON DELETE CASCADE,
				UNIQUE(pattern_id, relative_index)
			)
			",
				turso::params![],
			)
			.await?;

		// Indexes for performance
		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_pattern_occurrences_pattern_id ON pattern_occurrences(pattern_id)", turso::params![]).await?;
		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_pattern_relatives_pattern_id ON pattern_relatives(pattern_id)", turso::params![]).await?;

		Ok(())
	}

	async fn events(&mut self) -> Result<TursoDatabase> {
		if let Some(ref db) = self.events {
			return Ok(db.clone());
		}

		// Lazily initialize the events database
		let events = Database::get_or_create_turso_database(&self.events_path).await?;
		let conn = Database::begin_concurrent(&events, &self.events_path, None).await?;
		Self::wireframe_events_tables(&conn).await?;
		let _ = Database::commit_concurrent(&conn).await;

		self.events = Some(events.clone());
		Ok(events)
	}

	fn set_events(&mut self, turso_db: TursoDatabase) {
		self.events = Some(turso_db);
	}

	fn events_path(&self) -> String {
		self.events_path.clone()
	}

	async fn set_events_path(&mut self, path: String) {
		self.events_path = path;
	}

	async fn wireframe_events_tables(conn: &Connection) -> Result<()> {
		// Main events table
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS events (
				id TEXT PRIMARY KEY,
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
				id TEXT PRIMARY KEY,
				event_id TEXT NOT NULL,
				dataset_id TEXT NOT NULL,
				start_timestamp INTEGER NOT NULL,
				end_timestamp INTEGER NOT NULL,

				FOREIGN KEY (event_id) REFERENCES events(id) ON DELETE CASCADE
			)
			",
				turso::params![],
			)
			.await?;

		// Indexes for performance
		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_event_manifestations_event_id ON event_manifestations(event_id)", turso::params![]).await?;
		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_event_manifestations_dataset ON event_manifestations(dataset_id)", turso::params![]).await?;
		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_event_manifestations_time_range ON event_manifestations(start_timestamp, end_timestamp)", turso::params![]).await?;
		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_events_name ON events(name)", turso::params![]).await?;

		Ok(())
	}

	async fn correlations(&mut self) -> Result<TursoDatabase> {
		if let Some(ref db) = self.correlations {
			return Ok(db.clone());
		}

		// Lazily initialize the correlations database
		let correlations = Database::get_or_create_turso_database(&self.correlations_path).await?;
		let conn = Database::begin_concurrent(&correlations, &self.correlations_path, None).await?;
		Self::wireframe_correlations_tables(&conn).await?;
		let _ = Database::commit_concurrent(&conn).await;

		self.correlations = Some(correlations.clone());
		Ok(correlations)
	}

	fn set_correlations(&mut self, turso_db: TursoDatabase) {
		self.correlations = Some(turso_db);
	}

	fn correlations_path(&self) -> String {
		self.correlations_path.clone()
	}

	async fn set_correlations_path(&mut self, path: String) {
		self.correlations_path = path;
	}

	async fn wireframe_correlations_tables(conn: &Connection) -> Result<()> {
		// Main correlations table
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS correlations (
				id TEXT PRIMARY KEY,
				dictionary_id TEXT NOT NULL,
				subject_id TEXT NOT NULL,
				aspect_id TEXT NOT NULL,
				pattern_id TEXT NOT NULL,
				event_id TEXT NOT NULL,
				created_at INTEGER NOT NULL,
				updated_at INTEGER NOT NULL,

				FOREIGN KEY (pattern_id) REFERENCES patterns(id),
				FOREIGN KEY (event_id) REFERENCES events(id)
			)
			",
				turso::params![],
			)
			.await?;

		// Add columns if they don't exist (for existing tables)
		conn.as_ref().execute("ALTER TABLE correlations ADD COLUMN subject_id TEXT", turso::params![]).await.ok();
		conn.as_ref().execute("ALTER TABLE correlations ADD COLUMN aspect_id TEXT", turso::params![]).await.ok();

		// Error rates table for the HashMap<SignalType, ErrorRate>
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS correlation_error_rates (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				correlation_id TEXT NOT NULL,
				signal_type TEXT NOT NULL,
				error_rate_value TEXT NOT NULL, -- BigDecimal as TEXT
				error_rate_units TEXT NOT NULL, -- Resolution as JSON

				FOREIGN KEY (correlation_id) REFERENCES correlations(id) ON DELETE CASCADE,
				UNIQUE(correlation_id, signal_type)
			)
			",
				turso::params![],
			)
			.await?;

		// Correlation occurrences table
		conn.as_ref()
			.execute(
				r"
			CREATE TABLE IF NOT EXISTS correlation_occurrences (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				correlation_id TEXT NOT NULL,
				occurrence_index INTEGER NOT NULL,
				aspect_id TEXT NOT NULL,
				resolution TEXT NOT NULL, -- Resolution as JSON
				size INTEGER NOT NULL,
				database_info TEXT NOT NULL, -- DatabaseInfo as JSON blob
				pattern_id TEXT NOT NULL,
				beginning_timestamp INTEGER NOT NULL,
				end_timestamp INTEGER NOT NULL,

				FOREIGN KEY (correlation_id) REFERENCES correlations(id) ON DELETE CASCADE,
				UNIQUE(correlation_id, occurrence_index)
			)
			",
				turso::params![],
			)
			.await?;

		// Indexes for performance
		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_correlations_dictionary ON correlations(dictionary_id)", turso::params![]).await?;
		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_correlations_subject ON correlations(subject_id)", turso::params![]).await?;
		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_correlations_aspect ON correlations(aspect_id)", turso::params![]).await?;
		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_correlations_pattern ON correlations(pattern_id)", turso::params![]).await?;
		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_correlations_event ON correlations(event_id)", turso::params![]).await?;
		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_correlation_error_rates_correlation_id ON correlation_error_rates(correlation_id)", turso::params![]).await?;
		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_correlation_occurrences_correlation_id ON correlation_occurrences(correlation_id)", turso::params![]).await?;
		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_correlation_occurrences_time_range ON correlation_occurrences(beginning_timestamp, end_timestamp)", turso::params![]).await?;

		Ok(())
	}

	async fn wireframe_dictionary_tables(&self, conn: &Connection) -> Result<()> {
		// Main dictionary metadata table
		conn.as_ref()
			.execute(
				r"
                                        CREATE TABLE IF NOT EXISTS dictionary_metadata (
                                                id TEXT PRIMARY KEY,
                                                name TEXT NOT NULL UNIQUE,
                                                description TEXT,
                                                created_at INTEGER NOT NULL
                                        )
                                ",
				turso::params![],
			)
			.await?;

		// Dictionary constraints table - stores steps configuration
		conn.as_ref()
			.execute(
				r"
                                        CREATE TABLE IF NOT EXISTS dictionary_constraints (
                                                id INTEGER PRIMARY KEY AUTOINCREMENT,
                                                dictionary_id TEXT NOT NULL,
                                                steps_count INTEGER,
                                                steps_interpolation TEXT,

                                                FOREIGN KEY (dictionary_id) REFERENCES dictionary_metadata(id) ON DELETE CASCADE
                                        )
                                ",
				turso::params![],
			)
			.await?;

		// Dictionary variabilities table - stores variability constraints
		conn.as_ref()
			.execute(
				r"
                                        CREATE TABLE IF NOT EXISTS dictionary_variabilities (
                                                id INTEGER PRIMARY KEY AUTOINCREMENT,
                                                dictionary_id TEXT NOT NULL,
                                                variability_type TEXT NOT NULL,
                                                variability_value TEXT NOT NULL,

                                                FOREIGN KEY (dictionary_id) REFERENCES dictionary_metadata(id) ON DELETE CASCADE
                                        )
                                ",
				turso::params![],
			)
			.await?;

		// Dictionary patterns table - links patterns to dictionaries
		conn.as_ref()
			.execute(
				r"
                                        CREATE TABLE IF NOT EXISTS dictionary_patterns (
                                                id INTEGER PRIMARY KEY AUTOINCREMENT,
                                                dictionary_id TEXT NOT NULL,
                                                pattern_id TEXT NOT NULL,
                                                added_at INTEGER NOT NULL,

                                                FOREIGN KEY (dictionary_id) REFERENCES dictionary_metadata(id) ON DELETE CASCADE,
                                                FOREIGN KEY (pattern_id) REFERENCES patterns(id) ON DELETE CASCADE,
                                                UNIQUE(dictionary_id, pattern_id)
                                        )
                                ",
				turso::params![],
			)
			.await?;

		// Indexes for performance
		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_dictionary_name ON dictionary_metadata(name)", turso::params![]).await?;

		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_dictionary_constraints_dict_id ON dictionary_constraints(dictionary_id)", turso::params![]).await?;

		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_dictionary_variabilities_dict_id ON dictionary_variabilities(dictionary_id)", turso::params![]).await?;

		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_dictionary_patterns_dict_id ON dictionary_patterns(dictionary_id)", turso::params![]).await?;

		conn.as_ref().execute("CREATE INDEX IF NOT EXISTS idx_dictionary_patterns_pattern_id ON dictionary_patterns(pattern_id)", turso::params![]).await?;

		Ok(())
	}

	async fn new_dictionary(&self, name: &str, description: &str, constraints: &DictionaryConstraints) -> Result<()> {
		let dictionaries_db_path = Database::aspect_dictionaries_db_path(&self.database_metadata_db_path, &self.subject_name, &self.name, name);
		let dictionaries_db = Database::get_or_create_turso_database(&dictionaries_db_path).await?;
		let conn = Database::begin_concurrent(&dictionaries_db, &dictionaries_db_path, None).await?;
		self.wireframe_dictionary_tables(&conn).await?;
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
}

impl PartialEq for Aspect {
	fn eq(&self, other: &Self) -> bool {
		self.id == other.id && self.name == other.name && self.subject_id == other.subject_id && self.resolution == other.resolution
		// Skip measurements_turso_db comparison since it doesn't implement PartialEq
	}
}
