use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Utc};
use splimes::Resolution;
use turso::Transaction;

use crate::{Aspect, AspectId, Subject, SubjectId, TxId};

/// Trait for database structure operations
/// This trait defines the operations related to managing the structure of the database.
///
/// Add a Subject for observation -> track various aspects of the subject
#[async_trait::async_trait]
pub trait AspectStructure {
	// Aspect

	async fn new(id: Option<AspectId>, name: String, subject_id: SubjectId, resolution: Resolution, database_metadata_db_path: String) -> Result<Aspect>;

	const fn id(&self) -> AspectId;

	const fn name(&self) -> &str;

	const fn subject_id(&self) -> SubjectId;

	const fn resolution(&self) -> Resolution;

	async fn database_metadata(&self) -> Result<turso::Database>;

	async fn get_database_metadata_path(turso_db_path: String) -> Result<String>;

	async fn get_subject_name(turso_db: turso::Database, subject_id: SubjectId) -> Result<String>;

	async fn get_aspect_path(turso_db_path: String, subject_id: SubjectId, aspect_name: String) -> Result<String>;

	/// get measurements database
	async fn measurements(&mut self) -> Result<TursoDatabase>;

	/// set measurements database
	fn set_measurements(&mut self, turso_db: turso::Database);

	/// Create Measurements tables
	async fn wireframe_measurements_tables(conn: &turso::Connection) -> Result<()>;

	async fn unprocessed_batches(&mut self) -> Result<turso::Database>;

	/// set unprocessed batches database
	fn set_unprocessed_batches(&mut self, turso_db: turso::Database);

	async fn processed_batches(&mut self) -> Result<turso::Database>;

	/// set processed batches database
	fn set_processed_batches(&mut self, turso_db: turso::Database);

	async fn wireframe_batches_tables(conn: &turso::Connection) -> Result<()>;

	/// get patterns database
	async fn patterns(&mut self) -> Result<turso::Database>;

	/// set patterns database
	fn set_patterns(&mut self, turso_db: turso::Database);

	async fn wireframe_patterns_tables(conn: &turso::Connection) -> Result<()>;

	/// get events database
	async fn events(&mut self) -> Result<turso::Database>;

	/// set events database
	fn set_events(&mut self, turso_db: turso::Database);

	async fn wireframe_events_tables(conn: &turso::Connection) -> Result<()>;

	/// get correlations database
	async fn correlations(&mut self) -> Result<turso::Database>;

	/// set correlations database
	fn set_correlations(&mut self, turso_db: turso::Database);

	async fn wireframe_correlations_tables(conn: &turso::Connection) -> Result<()>;
}
