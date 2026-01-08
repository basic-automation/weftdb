use anyhow::Result;
use chrono::{DateTime, Utc};
use splimes::Resolution;

use crate::{cache::Connection, Aspect, AspectId, Database, DatabaseId, DatabaseInfo, Subject, SubjectId, Transaction, TxId};

/// Trait for database structure operations
/// This trait defines the operations related to managing the structure of the database.
///
/// Add a Subject for observation -> track various aspects of the subject
#[async_trait::async_trait]
pub trait DatabaseStructure {
	// Database

	/// Create a new Database
	#[allow(clippy::new_ret_no_self)]
	async fn new(name: &str) -> Result<Database>;

	/// Load an existing Database
	async fn existing(name: &str) -> Result<Database>;

	/// Get database `db_info`
	async fn get_database_info(&self) -> Result<DatabaseInfo>;

	/// get Turso database
	async fn get_turso_database(path: &str) -> Result<turso::Database>;

	/// create Turso database
	async fn create_turso_database(path: &str) -> Result<turso::Database>;

	/// get or create Turso database, ensuring proper caching and avoiding conflicts
	async fn get_or_create_turso_database(path: &str) -> Result<turso::Database>;

	/// get database id
	fn id(&self) -> DatabaseId;

	/// get database name
	fn name(&self) -> &str;

	/// Get Metadata Database
	fn metadata(&self) -> &turso::Database;

	fn metadata_path(&self) -> &str;

	/// Create the metadata database
	async fn wireframe_metadata_database(conn: &Connection) -> Result<Vec<Transaction>>;

	/// Create the transactions table for the metadata database
	async fn metadata_database_create_transactions_table(conn: &Connection) -> Result<Transaction>;

	/// Create the database table for the metadata database
	/// Holds general metadata about the database
	async fn metadata_database_create_database_table(conn: &Connection) -> Result<Transaction>;

	/// Create the subjects table for the metadata database
	/// Keeps track of all of the Subjects being observed by the database
	async fn metadata_database_create_subjects_table(conn: &Connection) -> Result<Transaction>;

	/// Create the aspects table for the metadata database
	/// Keeps track of all of the Aspects on each Subject in the database
	async fn metadata_database_create_aspects_table(conn: &Connection) -> Result<Transaction>;

	/// Value to String helpers
	async fn value_to_string(value: &turso::Value, field_name: &str) -> Result<String>;

	/// Close the database and release resources
	async fn close(&self) -> Result<()>;

	/// Wait for the database to be released
	async fn wait_for_database_release(&self) -> Result<()>;

	/// Helper to establish database connection with retry logic
	async fn connect_with_retry(turso_db: &turso::Database, attempts: i32, max_attempts: i32) -> Result<turso::Connection>;

	// Transations

	/// Create transaction
	async fn record_transaction(&self, message: &str) -> Result<TxId>;

	/// Log previously created transaction
	async fn log_transaction(&self, transaction: &Transaction) -> Result<()>;

	// Subjects

	/// Track a new subject in the database
	async fn observe_subject(&self, name: &str) -> Result<Subject>;

	/// Get a subject by its ID
	async fn get_subject(&self, id: &SubjectId) -> Result<Subject>;

	/// Get a Subject by its name
	async fn get_subject_by_name(&self, name: &str) -> Result<Subject>;

	/// Remove a subject from observation
	async fn remove_subject(&self, id: &SubjectId) -> Result<()>;

	/// List all subjects in the database
	async fn list_subjects(&self) -> Result<Vec<Subject>>;

	/// List all tracked aspects of a subject
	async fn list_aspects(&self, subject_id: &SubjectId) -> Result<Vec<Aspect>>;

	// Aspects

	/// Creates and initializes a new Aspect
	async fn track_aspect(&self, subject_id: &SubjectId, name: &str, resolution: &Resolution) -> Result<Aspect>;

	/// Get an Aspect by its ID
	async fn get_aspect(&self, id: &AspectId) -> Result<Aspect>;

	/// Get an Aspect by its name
	async fn get_aspect_by_name(&self, name: &str) -> Result<Aspect>;

	/// Get the earliest measurement timestamp for an aspect
	async fn get_earliest_measurement(&self, aspect_id: &AspectId) -> Result<Option<DateTime<Utc>>>;

	/// Get the latest measurement timestamp for an aspect
	async fn get_latest_measurement(&self, aspect_id: &AspectId) -> Result<Option<DateTime<Utc>>>;

	/// Get the resolution for an aspect
	async fn get_aspect_resolution(&self, aspect_id: &AspectId) -> Result<Resolution>;

	/// Helper to update aspect metadata timestamps
	async fn update_aspect_timestamps(&self, aspect_id: &AspectId, min_new: DateTime<Utc>, max_new: DateTime<Utc>) -> Result<()>;

	/// get measurement db by aspect id
	async fn get_measurement_db(&self, aspect_id: &AspectId) -> Result<turso::Database>;

	/// get measurement db path by aspect id
	async fn get_measurement_db_path(&self, aspect_id: &AspectId) -> Result<String>;

	/// get unprocessed batches db by aspect id
	async fn get_unprocessed_batches_db(&self, aspect_id: &AspectId) -> Result<turso::Database>;

	/// get unprocessed batches db path by aspect id
	async fn get_unprocessed_batches_db_path(&self, aspect_id: &AspectId) -> Result<String>;

	/// get processed batches db by aspect id
	async fn get_processed_batches_db(&self, aspect_id: &AspectId) -> Result<turso::Database>;

	/// get processed batches db path by aspect id
	async fn get_processed_batches_db_path(&self, aspect_id: &AspectId) -> Result<String>;

	/// get patterns db by aspect id
	async fn get_patterns_db(&self, aspect_id: &AspectId) -> Result<turso::Database>;

	/// get patterns db path by aspect id
	async fn get_patterns_db_path(&self, aspect_id: &AspectId) -> Result<String>;

	/// get events db by aspect id
	async fn get_events_db(&self, aspect_id: &AspectId) -> Result<turso::Database>;

	/// get events db path by aspect id
	async fn get_events_db_path(&self, aspect_id: &AspectId) -> Result<String>;

	/// get unprocessed events db by aspect id
	async fn get_unprocessed_events_db(&self, aspect_id: &AspectId) -> Result<turso::Database>;

	/// get unprocessed events db path by aspect id
	async fn get_unprocessed_events_db_path(&self, aspect_id: &AspectId) -> Result<String>;

	/// get processed events db by aspect id
	async fn get_processed_events_db(&self, aspect_id: &AspectId) -> Result<turso::Database>;

	/// get processed events db path by aspect id
	async fn get_processed_events_db_path(&self, aspect_id: &AspectId) -> Result<String>;

	/// get correlations db by aspect id
	async fn get_correlations_db(&self, aspect_id: &AspectId) -> Result<turso::Database>;

	/// get correlations db path by aspect id
	async fn get_correlations_db_path(&self, aspect_id: &AspectId) -> Result<String>;

	/// get dictionary db by aspect id and dictionary name
	async fn get_dictionary_db(&self, aspect_id: &AspectId, dictionary_name: &str) -> Result<turso::Database>;
}
