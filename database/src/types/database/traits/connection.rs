use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use crate::{cache, DatabaseCache};

/// Trait for database structure operations
/// This trait defines the operations related to managing the structure of the database.
///
/// Add a Subject for observation -> track various aspects of the subject
#[async_trait::async_trait]
pub trait Connection {
	async fn begin_concurrent(turso_db: &turso::Database, cache_key: &str, cache: Option<Arc<Mutex<DatabaseCache>>>) -> Result<cache::Connection>;

	async fn commit_concurrent(conn: &cache::Connection) -> anyhow::Result<()>;

	async fn rollback_concurrent(conn: &cache::Connection) -> Result<()>;

	async fn configure_database_for_mvcc(turso_db: &turso::Database) -> Result<()>;

	/// Begin an immediate transaction for DDL operations (CREATE TABLE, etc.)
	/// DDL operations are not compatible with BEGIN CONCURRENT in MVCC mode.
	/// This uses BEGIN IMMEDIATE which acquires a write lock but allows schema changes.
	async fn begin_immediate(turso_db: &turso::Database) -> Result<cache::Connection>;

	/// Commit an immediate transaction
	async fn commit_immediate(conn: &cache::Connection) -> Result<()>;

	/// Checkpoint the WAL (Write-Ahead Log) to flush pending writes to the main database file.
	/// This should be called after large batch operations to ensure data is persisted.
	/// Uses PRAGMA `wal_checkpoint(TRUNCATE)` to checkpoint and truncate the WAL file.
	async fn checkpoint_wal(turso_db: &turso::Database) -> Result<()>;
}
