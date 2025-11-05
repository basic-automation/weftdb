use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::{
	cache::Connection, types::database::{traits::config::Config, Database}
};

static METADATA_DB_FILENAME: &str = "metadata.db";
static MEASUREMENT_DB_FILENAME: &str = "measurements.db";
static UNPROCESSED_BATCHES_DB_FILENAME: &str = "unprocessed_batches.db";
static PROCESSED_BATCHES_DB_FILENAME: &str = "processed_batches.db";
static PATTERNS_DB_FILENAME: &str = "patterns.db";
static EVENTS_DB_FILENAME: &str = "events.db";
static CORRELATIONS_DB_FILENAME: &str = "correlations.db";
static DICTIONARIES_DB_FOLDERNAME: &str = "dictionaries";

// Default data directory - can be overridden with environment variable
pub const DEFAULT_DATA_DIR: &str = "C:\\Users\\physi\\Desktop\\dsp_data";

// Version information
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PKG_NAME: &str = env!("CARGO_PKG_NAME");

#[async_trait::async_trait]
impl Config for Database {
	fn db_path(db_name: &str) -> String {
		let mut path_buf: PathBuf = Self::get_data_dir().into();
		path_buf.push(db_name);
		path_buf.to_string_lossy().to_string()
	}

	fn metadata_db_path(db_name: &str) -> String {
		let mut db_path: PathBuf = Self::db_path(db_name).into();
		db_path.push(METADATA_DB_FILENAME);
		db_path.to_string_lossy().to_string()
	}

	fn aspect_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
		let mut db_path: PathBuf = Self::db_path(db_name).into();
		db_path.push(subject_name);
		db_path.push(aspect_name);
		db_path.to_string_lossy().to_string()
	}

	fn aspect_measurements_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
		let mut db_path: PathBuf = Self::aspect_path(db_name, subject_name, aspect_name).into();
		db_path.push(MEASUREMENT_DB_FILENAME);
		db_path.to_string_lossy().to_string()
	}

	fn aspect_unprocessed_batches_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
		let mut db_path: PathBuf = Self::aspect_path(db_name, subject_name, aspect_name).into();
		db_path.push(UNPROCESSED_BATCHES_DB_FILENAME);
		db_path.to_string_lossy().to_string()
	}

	fn aspect_processed_batches_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
		let mut db_path: PathBuf = Self::aspect_path(db_name, subject_name, aspect_name).into();
		db_path.push(PROCESSED_BATCHES_DB_FILENAME);
		db_path.to_string_lossy().to_string()
	}

	fn aspect_patterns_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
		let mut db_path: PathBuf = Self::aspect_path(db_name, subject_name, aspect_name).into();
		db_path.push(PATTERNS_DB_FILENAME);
		db_path.to_string_lossy().to_string()
	}

	fn aspect_events_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
		let mut db_path: PathBuf = Self::aspect_path(db_name, subject_name, aspect_name).into();
		db_path.push(EVENTS_DB_FILENAME);
		db_path.to_string_lossy().to_string()
	}

	fn aspect_correlations_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
		let mut db_path: PathBuf = Self::aspect_path(db_name, subject_name, aspect_name).into();
		db_path.push(CORRELATIONS_DB_FILENAME);
		db_path.to_string_lossy().to_string()
	}

	fn aspect_dictionaries_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
		let mut db_path: PathBuf = Self::aspect_path(db_name, subject_name, aspect_name).into();
		db_path.push(DICTIONARIES_DB_FOLDERNAME);
		db_path.to_string_lossy().to_string()
	}

	fn dictionary_path(db_name: &str, subject_name: &str, aspect_name: &str, dictionary_name: &str) -> String {
		let mut db_path: PathBuf = Self::aspect_dictionaries_path(db_name, subject_name, aspect_name).into();
		db_path.push(format!("{}.db", dictionary_name));
		db_path.to_string_lossy().to_string()
	}

	async fn db_name(metadata_conn: &Connection) -> Result<String> {
		let mut stmt = metadata_conn.as_ref().prepare("SELECT name FROM database LIMIT 1").await?;
		let mut rows = stmt.query(turso::params![]).await?;
		if let Some(row) = rows.next().await? {
			let name: String = row.get(0)?;
			Ok(name)
		} else {
			bail!("Database name not found in metadata")
		}
	}

	async fn db_metadata_path(metadata_conn: &Connection) -> Result<String> {
		let mut stmt = metadata_conn.as_ref().prepare("SELECT metadata_path FROM database LIMIT 1").await?;
		let mut rows = stmt.query(turso::params![]).await?;
		if let Some(row) = rows.next().await? {
			let path: String = row.get(0)?;
			Ok(path)
		} else {
			bail!("Database metadata path not found in metadata")
		}
	}

	fn get_data_dir() -> String {
		std::env::var("TEST_DATA_DIR").unwrap_or_else(|_| DEFAULT_DATA_DIR.to_string())
	}
}
