use anyhow::Result;

use crate::cache::Connection;

/// Trait for database structure operations
/// This trait defines the operations related to managing the structure of the database.
///
/// Add a Subject for observation -> track various aspects of the subject
#[async_trait::async_trait]
pub trait Config {
	fn db_path(db_name: &str) -> String;

	fn metadata_db_path(db_name: &str) -> String;

	fn aspect_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String;

	fn aspect_measurements_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String;

	fn aspect_unprocessed_batches_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String;

	fn aspect_processed_batches_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String;

	fn aspect_patterns_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String;

	fn aspect_events_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String;

	fn aspect_correlations_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String;

	fn aspect_dictionaries_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String;

	fn dictionary_path(db_name: &str, subject_name: &str, aspect_name: &str, dictionary_name: &str) -> String;

	async fn db_name(metadata_conn: &Connection) -> Result<String>;

	async fn db_metadata_path(metadata_conn: &Connection) -> Result<String>;

	fn get_data_dir() -> String;
}
