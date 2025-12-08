use anyhow::Result;
use splimes::Resolution;

use crate::{cache::Connection, AspectId, DictionaryConstraints, SubjectId};

/// Trait for database structure operations
/// This trait defines the operations related to managing the structure of the database.
///
/// Add a Subject for observation -> track various aspects of the subject
#[async_trait::async_trait]
pub trait AspectStructure {
	// Aspect

	async fn new(id: Option<AspectId>, name: &str, subject_id: &SubjectId, resolution: &Resolution, metadata_conn: &Connection) -> Result<Self>
	where
		Self: Sized;

	/// Lightweight constructor that creates an Aspect instance from metadata
	/// without opening or wireframing per-aspect databases. Useful for
	/// existence checks and listing operations where we want to avoid
	/// holding metadata DB locks.
	async fn from_metadata(id: Option<AspectId>, name: String, subject_id: &SubjectId, resolution: &Resolution, database_metadata_db_path: String, subject_name_opt: Option<String>) -> Result<Self>
	where
		Self: Sized;

	fn id(&self) -> AspectId;

	fn name(&self) -> &str;

	fn subject_id(&self) -> SubjectId;

	fn resolution(&self) -> Resolution;

	fn subject_name(&self) -> &str;

	fn aspect_path(&self) -> &str;

	async fn database_metadata(&self) -> Result<turso::Database>;

	async fn get_database_metadata_path(turso_db_path: String) -> Result<String>;

	async fn get_subject_name(conn: &Connection, subject_id: &SubjectId) -> Result<String>;

	async fn get_aspect_path(conn: &Connection, metadata_path: &str, subject_id: &SubjectId, aspect_name: &str) -> Result<String>;

	/// get measurements database
	async fn measurements(&mut self) -> Result<turso::Database>;

	/// set measurements database
	fn set_measurements(&mut self, turso_db: turso::Database);

	fn measurements_path(&self) -> String;

	async fn set_measurements_path(&mut self, path: String);

	/// Create Measurements tables
	async fn wireframe_measurements_tables(conn: &Connection) -> Result<()>;

	async fn unprocessed_batches(&mut self) -> Result<turso::Database>;

	/// set unprocessed batches database
	fn set_unprocessed_batches(&mut self, turso_db: turso::Database);

	fn unprocessed_batches_path(&self) -> String;

	async fn set_unprocessed_batches_path(&mut self, path: String);

	async fn processed_batches(&mut self) -> Result<turso::Database>;

	/// set processed batches database
	fn set_processed_batches(&mut self, turso_db: turso::Database);

	fn processed_batches_path(&self) -> String;

	async fn set_processed_batches_path(&mut self, path: String);

	async fn wireframe_batches_tables(conn: &Connection) -> Result<()>;

	/// get patterns database
	async fn patterns(&mut self) -> Result<turso::Database>;

	/// set patterns database
	fn set_patterns(&mut self, turso_db: turso::Database);

	fn patterns_path(&self) -> String;

	async fn set_patterns_path(&mut self, path: String);

	async fn wireframe_patterns_tables(conn: &Connection) -> Result<()>;

	/// get events database
	async fn events(&mut self) -> Result<turso::Database>;

	/// set events database
	fn set_events(&mut self, turso_db: turso::Database);

	fn events_path(&self) -> String;

	async fn set_events_path(&mut self, path: String);

	async fn wireframe_events_tables(conn: &Connection) -> Result<()>;

	/// get correlations database
	async fn correlations(&mut self) -> Result<turso::Database>;

	/// set correlations database
	fn set_correlations(&mut self, turso_db: turso::Database);

	fn correlations_path(&self) -> String;

	async fn set_correlations_path(&mut self, path: String);

	async fn wireframe_correlations_tables(conn: &Connection) -> Result<()>;

	async fn new_dictionary(&self, name: &str, description: &str, constraints: &DictionaryConstraints) -> Result<()>;

	async fn wireframe_dictionary_tables(&self, conn: &Connection) -> Result<()>;
}
