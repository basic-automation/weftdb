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
static UNPROCESSED_EVENTS_DB_FILENAME: &str = "unprocessed_events.db";
static PROCESSED_EVENTS_DB_FILENAME: &str = "processed_events.db";
static CORRELATIONS_DB_FILENAME: &str = "correlations.db";
static PIPELINE_DB_FILENAME: &str = "pipeline.db";
static DICTIONARIES_DB_FOLDERNAME: &str = "dictionaries";

/// Returns the portable, per-user default data directory for WeftDB databases.
///
/// No hard-coded paths: this resolves to a writable, machine-independent location
/// for the current user. Resolution order:
/// 1. `~/.weftdb/data` — consistent with the `weft-tui` home directory (`~/.weftdb`).
/// 2. The platform data directory + `weftdb` (e.g. `%APPDATA%\weftdb`,
///    `~/Library/Application Support/weftdb`, `~/.local/share/weftdb`) when the home
///    directory cannot be determined.
/// 3. A relative `weftdb_data` directory as a last resort.
#[must_use]
pub fn default_data_dir() -> String {
	dirs::home_dir().map(|home| home.join(".weftdb").join("data")).or_else(|| dirs::data_dir().map(|data| data.join("weftdb"))).unwrap_or_else(|| PathBuf::from("weftdb_data")).to_string_lossy().into_owned()
}

/// Returns the active data directory for WeftDB databases.
///
/// Resolution order:
/// 1. `TEST_DATA_DIR` — explicit override (used by the test suite).
/// 2. `WEFT_DATA_DIR` — shared override, also honored by `weft-tui` for its logs.
/// 3. [`default_data_dir`] — the portable per-user default.
#[must_use]
pub fn data_dir() -> String {
	std::env::var("TEST_DATA_DIR").or_else(|_| std::env::var("WEFT_DATA_DIR")).unwrap_or_else(|_| default_data_dir())
}

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

	fn aspect_unprocessed_events_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
		let mut db_path: PathBuf = Self::aspect_path(db_name, subject_name, aspect_name).into();
		db_path.push(UNPROCESSED_EVENTS_DB_FILENAME);
		db_path.to_string_lossy().to_string()
	}

	fn aspect_processed_events_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
		let mut db_path: PathBuf = Self::aspect_path(db_name, subject_name, aspect_name).into();
		db_path.push(PROCESSED_EVENTS_DB_FILENAME);
		db_path.to_string_lossy().to_string()
	}

	fn aspect_correlations_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
		let mut db_path: PathBuf = Self::aspect_path(db_name, subject_name, aspect_name).into();
		db_path.push(CORRELATIONS_DB_FILENAME);
		db_path.to_string_lossy().to_string()
	}

	fn aspect_pipeline_db_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
		let mut db_path: PathBuf = Self::aspect_path(db_name, subject_name, aspect_name).into();
		db_path.push(PIPELINE_DB_FILENAME);
		db_path.to_string_lossy().to_string()
	}

	fn aspect_dictionaries_path(db_name: &str, subject_name: &str, aspect_name: &str) -> String {
		let mut db_path: PathBuf = Self::aspect_path(db_name, subject_name, aspect_name).into();
		db_path.push(DICTIONARIES_DB_FOLDERNAME);
		db_path.to_string_lossy().to_string()
	}

	fn aspect_dictionaries_db_path(db_name: &str, subject_name: &str, aspect_name: &str, dictionary_name: &str) -> String {
		let mut db_path: PathBuf = Self::aspect_dictionaries_path(db_name, subject_name, aspect_name).into();
		db_path.push(format!("{dictionary_name}.db"));
		db_path.to_string_lossy().to_string()
	}

	fn dictionary_path(db_name: &str, subject_name: &str, aspect_name: &str, dictionary_name: &str) -> String {
		let mut db_path: PathBuf = Self::aspect_dictionaries_path(db_name, subject_name, aspect_name).into();
		db_path.push(format!("{dictionary_name}.db"));
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
		data_dir()
	}
}
