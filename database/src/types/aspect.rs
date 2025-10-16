use std::{
	fmt::Display, path::{self, Path}
};

use anyhow::{bail, Result};
use fake::locales::Data;
use serde::{Deserialize, Serialize};
use splimes::Resolution;
use turso::Database as TursoDatabase;
use uuid::Uuid;

use crate::{
	aspect, batches, database::{self, traits::AspectStructure}, event, transaction::Transaction, types::database::traits::aspect_structure::AspectStructure, Database, DatabaseStructure, SubjectId, TxId
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

	#[serde(skip)]
	measurements: Option<TursoDatabase>,
	#[serde(skip)]
	unprocessed_batches: Option<TursoDatabase>,
	#[serde(skip)]
	processed_batches: Option<TursoDatabase>,
	#[serde(skip)]
	patterns: Option<TursoDatabase>,
	#[serde(skip)]
	events: Option<TursoDatabase>,
	#[serde(skip)]
	correlations: Option<TursoDatabase>,
}

#[async_trait::async_trait]
impl AspectStructure for Aspect {
	async fn new(id: Option<AspectId>, name: String, subject_id: SubjectId, resolution: Resolution, database_metadata_db_path: String) -> Result<Self> {
		let id = id.unwrap_or_else(AspectId::new);

		let aspect_path = Self::get_aspect_path(database_metadata_db_path.clone(), subject_id, name.clone()).await?;

		// recursively create directory if it doesn't exist
		tokio::fs::create_dir_all(&aspect_path).await?;

		let measurements_db_path = aspect_path.clone() + "/measurements.db";
		let measurements = match Database::get_turso_database(&measurements_db_path).await {
			Ok(db) => Some(db),
			Err(_) => Some(Database::create_turso_database(&measurements_db_path).await?),
		};

		match measurements {
			Some(ref db) => {
				let conn = db.connect()?;
				Aspect::wireframe_measurements_tables(&conn).await?
			}
			None => bail!("Failed to get or create measurements database"),
		};

		let unprocessed_batches_db_path = aspect_path.clone() + "/unprocessed_batches.db";
		let unprocessed_batches = match Database::get_turso_database(&unprocessed_batches_db_path).await {
			Ok(db) => Some(db),
			Err(_) => Some(Database::create_turso_database(&unprocessed_batches_db_path).await?),
		};

		match unprocessed_batches {
			Some(ref db) => {
				let conn = db.connect()?;
				Aspect::wireframe_batches_tables(&conn).await?;
			}
			None => bail!("Failed to get or create unprocessed batches database"),
		}

		let processed_batches_db_path = aspect_path.clone() + "/processed_batches.db";
		let processed_batches = match Database::get_turso_database(&processed_batches_db_path).await {
			Ok(db) => Some(db),
			Err(_) => Some(Database::create_turso_database(&processed_batches_db_path).await?),
		};

		match processed_batches {
			Some(ref db) => {
				let conn = db.connect()?;
				Aspect::wireframe_batches_tables(&conn).await?;
			}
			None => bail!("Failed to get or create processed batches database"),
		}

		let patterns_db_path = aspect_path.clone() + "/patterns.db";
		let patterns = match Database::get_turso_database(&patterns_db_path).await {
			Ok(db) => Some(db),
			Err(_) => Some(Database::create_turso_database(&patterns_db_path).await?),
		};

		match patterns {
			Some(ref db) => {
				let conn = db.connect()?;
				Aspect::wireframe_patterns_tables(&conn).await?
			}
			None => bail!("Failed to get or create patterns database"),
		}

		let events_db_path = aspect_path.clone() + "/events.db";
		let events = match Database::get_turso_database(&events_db_path).await {
			Ok(db) => Some(db),
			Err(_) => Some(Database::create_turso_database(&events_db_path).await?),
		};

		match events {
			Some(ref db) => {
				let conn = db.connect()?;
				Aspect::wireframe_events_tables(&conn).await?
			}
			None => bail!("Failed to get or create events database"),
		}

		let correlations_db_path = aspect_path + "/correlations.db";
		let correlations = match Database::get_turso_database(&correlations_db_path).await {
			Ok(db) => Some(db),
			Err(_) => Some(Database::create_turso_database(&correlations_db_path).await?),
		};

		match correlations {
			Some(ref db) => {
				let conn = db.connect()?;
				Aspect::wireframe_correlations_tables(&conn).await?
			}
			None => bail!("Failed to get or create correlations database"),
		};

		#[rustfmt::skip]
		Ok(Self {
                        id,
                        name,
                        subject_id,
                        resolution,
                        database_metadata_db_path,
                        measurements,
                        unprocessed_batches,
                        processed_batches,
                        patterns,
                        events,
                        correlations,
                })
	}

	const fn id(&self) -> AspectId {
		self.id
	}

	fn name(&self) -> &str {
		&self.name
	}

	const fn subject_id(&self) -> SubjectId {
		self.subject_id
	}

	const fn resolution(&self) -> Resolution {
		self.resolution
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

	async fn get_subject_name(turso_db: TursoDatabase, subject_id: SubjectId) -> Result<String> {
		// Query the database for the name field where id = subject_id in the subjects table
		let conn = turso_db.connect()?;
		let mut rows = conn.query("SELECT name FROM subjects WHERE id = ?", turso::params![subject_id.as_uuid().to_string()]).await?;
		let row = rows.next().await?.ok_or_else(|| anyhow::anyhow!("Subject not found"))?;
		let subject_name: String = row.get(0)?;

		Ok(subject_name)
	}

	async fn get_aspect_path(turso_db_path: String, subject_id: SubjectId, aspect_name: String) -> Result<String> {
		let database_metadata_path = Aspect::get_database_metadata_path(self.database_metadata_db_path.clone()).await?;
		let database_metadata_db = Database::get_turso_database(&database_metadata_path).await?;
		let subject_name = Self::get_subject_name(database_metadata_db, subject_id).await?;

		let aspect_metadata_path = Path::new(&database_metadata_path).join(subject_name).join(aspect_name);

		if aspect_metadata_path.exists() {
			Ok(aspect_metadata_path.to_string_lossy().to_string())
		} else {
			bail!("Aspect metadata path does not exist");
		}
	}

	async fn measurements(&mut self) -> Result<TursoDatabase> {
		if self.measurements.is_none() {
			let database_metadata_path = Aspect::get_database_metadata_path(self.database_metadata_db_path.clone()).await?;
			let database_metadata_db = Database::get_turso_database(&database_metadata_path).await?;
			let _subject_name = Aspect::get_subject_name(database_metadata_db, self.subject_id).await?;

			let aspect_path = Aspect::get_aspect_path(database_metadata_path, self.subject_id, self.name.clone()).await?;
			let aspect_measurements_path = Path::new(&aspect_path).with_file_name("measurements.db").to_string_lossy().to_string();

			// If the measurements_path exists, open the TursoDatabase
			let measurements_db = Database::get_turso_database(&aspect_measurements_path).await?;
			self.measurements = Some(measurements_db);
		}

		Ok(match self.measurements {
			Some(ref db) => db.clone(),
			None => bail!("Measurements database is not initialized"),
		})
	}

	fn set_measurements(&mut self, turso_db: TursoDatabase) {
		self.measurements = Some(turso_db)
	}

	async fn wireframe_measurements_tables(conn: &turso::Connection) -> Result<()> {
		conn.execute(
			"CREATE TABLE IF NOT EXISTS measurements (
                                id TEXT PRIMARY KEY,
                                dataset_id TEXT NOT NULL,
                                timestamp INTEGER NOT NULL UNIQUE,
                                value TEXT NOT NULL,
                                FOREIGN KEY (dataset_id) REFERENCES datasets(id)
                        )",
			turso::params![],
		)
		.await?;

		// Add index for common queries
		conn.execute("CREATE INDEX IF NOT EXISTS idx_measurements_dataset_timestamp ON measurements(dataset_id, timestamp)", turso::params![]).await?;

		Ok(())
	}

	async fn unprocessed_batches(&mut self) -> Result<TursoDatabase> {
		if self.unprocessed_batches.is_none() {
			let database_metadata_path = Aspect::get_database_metadata_path(self.database_metadata_db_path.clone()).await?;
			let database_metadata_db = Database::get_turso_database(&database_metadata_path).await?;
			let _subject_name = Aspect::get_subject_name(database_metadata_db, self.subject_id).await?;

			let aspect_path = Aspect::get_aspect_path(database_metadata_path, self.subject_id, self.name.clone()).await?;
			let aspect_batches_path = Path::new(&aspect_path).with_file_name("unprocessed_batches.db").to_string_lossy().to_string();

			// If the batches_path exists, open the TursoDatabase
			let batches_db = Database::get_turso_database(&aspect_batches_path).await?;
			self.unprocessed_batches = Some(batches_db);
		}

		Ok(match self.unprocessed_batches {
			Some(ref db) => db.clone(),
			None => bail!("Unprocessed batches database is not initialized"),
		})
	}

	fn set_unprocessed_batches(&mut self, turso_db: TursoDatabase) {
		self.unprocessed_batches = Some(turso_db);
	}

	async fn wireframe_batches_tables(conn: &turso::Connection) -> Result<()> {
		conn.execute(
			r"
                        CREATE TABLE IF NOT EXISTS batches (
                                id TEXT PRIMARY KEY,
                                aspect_id TEXT NOT NULL,
                                database_id TEXT NOT NULL,
                                size INTEGER NOT NULL,
                                resolution TEXT NOT NULL,
                                batch_hash TEXT,
                                created_at INTEGER NOT NULL,
                                processed_at INTEGER,
                                updated_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000),

                                metadata_json TEXT NOT NULL,
                                measurements_json TEXT NOT NULL,

                                FOREIGN KEY (aspect_id) REFERENCES aspects(id),

                                -- Add unique constraint for batch_hash to prevent duplicate batches
                                UNIQUE(batch_hash) WHERE batch_hash IS NOT NULL
                )",
			turso::params![],
		)
		.await?;

		// Enhanced indexes for concurrent write scenarios (removed status reference)
		conn.execute("CREATE INDEX IF NOT EXISTS idx_batches_aspect_processed ON batches(aspect_id, processed_at)", turso::params![]).await?;
		conn.execute("CREATE INDEX IF NOT EXISTS idx_batches_created_at ON batches(created_at)", turso::params![]).await?;
		conn.execute("CREATE INDEX IF NOT EXISTS idx_batches_hash ON batches(batch_hash) WHERE batch_hash IS NOT NULL", turso::params![]).await?;

		Ok(())
	}

	async fn processed_batches(&mut self) -> Result<TursoDatabase> {
		if self.processed_batches.is_none() {
			let database_metadata_path = Aspect::get_database_metadata_path(self.database_metadata_db_path.clone()).await?;
			let database_metadata_db = Database::get_turso_database(&database_metadata_path).await?;
			let _subject_name = Aspect::get_subject_name(database_metadata_db, self.subject_id).await?;

			let aspect_path = Aspect::get_aspect_path(database_metadata_path, self.subject_id, self.name.clone()).await?;
			let aspect_batches_path = Path::new(&aspect_path).with_file_name("processed_batches.db").to_string_lossy().to_string();

			// If the batches_path exists, open the TursoDatabase
			let batches_db = Database::get_turso_database(&aspect_batches_path).await?;
			self.processed_batches = Some(batches_db);
		}

		Ok(match self.processed_batches {
			Some(ref db) => db.clone(),
			None => bail!("Processed batches database is not initialized"),
		})
	}

	fn set_processed_batches(&mut self, turso_db: TursoDatabase) {
		self.processed_batches = Some(turso_db);
	}

	async fn patterns(&mut self) -> Result<TursoDatabase> {
		if self.patterns.is_none() {
			let database_metadata_path = Aspect::get_database_metadata_path(self.database_metadata_db_path.clone()).await?;
			let database_metadata_db = Database::get_turso_database(&database_metadata_path).await?;
			let _subject_name = Aspect::get_subject_name(database_metadata_db, self.subject_id).await?;

			let aspect_path = Aspect::get_aspect_path(database_metadata_path, self.subject_id, self.name.clone()).await?;
			let aspect_patterns_path = Path::new(&aspect_path).with_file_name("patterns.db").to_string_lossy().to_string();

			// If the patterns_path exists, open the TursoDatabase
			let patterns_db = Database::get_turso_database(&aspect_patterns_path).await?;
			self.patterns = Some(patterns_db);
		}

		Ok(match self.patterns {
			Some(ref db) => db.clone(),
			None => bail!("Patterns database is not initialized"),
		})
	}

	fn set_patterns(&mut self, turso_db: TursoDatabase) {
		self.patterns = Some(turso_db);
	}

	async fn wireframe_patterns_tables(conn: &turso::Connection) -> Result<()> {
		// Main patterns table with precomputed statistics
		conn.execute(
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
		conn.execute(
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
		conn.execute(
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
		conn.execute("CREATE INDEX IF NOT EXISTS idx_pattern_occurrences_pattern_id ON pattern_occurrences(pattern_id)", turso::params![]).await?;
		conn.execute("CREATE INDEX IF NOT EXISTS idx_pattern_relatives_pattern_id ON pattern_relatives(pattern_id)", turso::params![]).await?;

		Ok(())
	}

	async fn events(&mut self) -> Result<TursoDatabase> {
		if self.events.is_none() {
			let database_metadata_path = Aspect::get_database_metadata_path(self.database_metadata_db_path.clone()).await?;
			let database_metadata_db = Database::get_turso_database(&database_metadata_path).await?;
			let _subject_name = Aspect::get_subject_name(database_metadata_db, self.subject_id).await?;

			let aspect_path = Aspect::get_aspect_path(database_metadata_path.to_string(), self.subject_id, self.name.clone()).await?;
			let aspect_events_path = Path::new(&aspect_path).with_file_name("events.db").to_string_lossy().to_string();

			// If the events_path exists, open the TursoDatabase
			let events_db = Database::get_turso_database(&aspect_events_path).await?;
			self.events = Some(events_db);
		}

		Ok(match self.events {
			Some(ref db) => db.clone(),
			None => bail!("Events database is not initialized"),
		})
	}

	fn set_events(&mut self, turso_db: TursoDatabase) {
		self.events = Some(turso_db);
	}

	async fn wireframe_events_tables(conn: &turso::Connection) -> Result<()> {
		// Main events table
		conn.execute(
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
		conn.execute(
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
		conn.execute("CREATE INDEX IF NOT EXISTS idx_event_manifestations_event_id ON event_manifestations(event_id)", turso::params![]).await?;
		conn.execute("CREATE INDEX IF NOT EXISTS idx_event_manifestations_dataset ON event_manifestations(dataset_id)", turso::params![]).await?;
		conn.execute("CREATE INDEX IF NOT EXISTS idx_event_manifestations_time_range ON event_manifestations(start_timestamp, end_timestamp)", turso::params![]).await?;
		conn.execute("CREATE INDEX IF NOT EXISTS idx_events_name ON events(name)", turso::params![]).await?;

		Ok(())
	}

	async fn correlations(&mut self) -> Result<TursoDatabase> {
		if self.correlations.is_none() {
			let database_metadata_path = Aspect::get_database_metadata_path(self.database_metadata_db_path.clone()).await?;
			let database_metadata_db = Database::get_turso_database(&database_metadata_path).await?;
			let _subject_name = Aspect::get_subject_name(database_metadata_db, self.subject_id).await?;

			let aspect_path = Aspect::get_aspect_path(database_metadata_path, self.subject_id, self.name.clone()).await?;
			let aspect_correlations_path = Path::new(&aspect_path).with_file_name("correlations.db").to_string_lossy().to_string();

			// If the correlations_path exists, open the TursoDatabase
			let correlations_db = Database::get_turso_database(&aspect_correlations_path).await?;
			self.correlations = Some(correlations_db);
		}

		Ok(match self.correlations {
			Some(ref db) => db.clone(),
			None => bail!("Correlations database is not initialized"),
		})
	}

	fn set_correlations(&mut self, turso_db: TursoDatabase) {
		self.correlations = Some(turso_db);
	}

	async fn wireframe_correlations_tables(conn: &turso::Connection) -> Result<()> {
		// Main correlations table
		conn.execute(
			r"
                                CREATE TABLE IF NOT EXISTS correlations (
                                        id TEXT PRIMARY KEY,
                                        dictionary_id TEXT NOT NULL,
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

		// Error rates table for the HashMap<SignalType, ErrorRate>
		conn.execute(
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
		conn.execute(
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
		conn.execute("CREATE INDEX IF NOT EXISTS idx_correlations_dictionary ON correlations(dictionary_id)", turso::params![]).await?;
		conn.execute("CREATE INDEX IF NOT EXISTS idx_correlations_pattern ON correlations(pattern_id)", turso::params![]).await?;
		conn.execute("CREATE INDEX IF NOT EXISTS idx_correlations_event ON correlations(event_id)", turso::params![]).await?;
		conn.execute("CREATE INDEX IF NOT EXISTS idx_correlations_event ON correlations(event_id)", turso::params![]).await?;
		conn.execute("CREATE INDEX IF NOT EXISTS idx_correlation_error_rates_correlation_id ON correlation_error_rates(correlation_id)", turso::params![]).await?;
		conn.execute("CREATE INDEX IF NOT EXISTS idx_correlation_occurrences_correlation_id ON correlation_occurrences(correlation_id)", turso::params![]).await?;
		conn.execute("CREATE INDEX IF NOT EXISTS idx_correlation_occurrences_time_range ON correlation_occurrences(beginning_timestamp, end_timestamp)", turso::params![]).await?;

		Ok(())
	}
}

impl PartialEq for Aspect {
	fn eq(&self, other: &Self) -> bool {
		self.id == other.id && self.name == other.name && self.subject_id == other.subject_id && self.resolution == other.resolution
		// Skip measurements_turso_db comparison since it doesn't implement PartialEq
	}
}
