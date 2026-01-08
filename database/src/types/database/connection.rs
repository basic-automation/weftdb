use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use super::Database;
pub use crate::types::database::traits::config::Config;
use crate::{cache, database::traits::Connection, DatabaseCache};

#[async_trait::async_trait]
impl Connection for Database {
	/// Create a new database connection with MVCC concurrent transaction support
	/// Each call creates a fresh connection with its own transaction - no caching at transaction level
	/// to avoid transaction conflicts when multiple tasks use the same cached connection
	/// 
	/// Uses BEGIN CONCURRENT which requires MVCC to be enabled on the database (via `PRAGMA journal_mode=experimental_mvcc`)
	async fn begin_concurrent(turso_db: &turso::Database, _cache_key: &str, _cache: Option<Arc<Mutex<DatabaseCache>>>) -> Result<cache::Connection> {
		// Always create a new connection for each transaction to avoid conflicts
		// Connection caching at the transaction level causes issues with concurrent writes
		let conn = turso_db.connect()?;

		// BEGIN CONCURRENT allows multiple concurrent write transactions with MVCC
		// Retry with exponential backoff on transient errors
		let mut attempts = 0;
		let max_attempts = 100; // ~30 seconds total with backoff
		
		loop {
			match conn.execute("BEGIN CONCURRENT", turso::params![]).await {
				Ok(_) => break,
				Err(e) => {
					attempts += 1;
					if attempts >= max_attempts {
						return Err(anyhow::anyhow!("Failed to begin concurrent transaction after {attempts} attempts: {e}"));
					}
					// Exponential backoff with jitter: 10-20ms, 20-40ms, ... capped at 500ms
					let base_delay = std::cmp::min(10 * (1 << attempts.min(6)), 500);
					let jitter = fastrand::u64(0..base_delay / 2);
					tokio::time::sleep(std::time::Duration::from_millis(base_delay + jitter)).await;
				}
			}
		}

		// Wrap in our Connection type (no caching - each transaction gets its own connection)
		let cached_conn = cache::Connection::new(conn);

		Ok(cached_conn)
	}

	async fn commit_concurrent(conn: &cache::Connection) -> anyhow::Result<()> {
		conn.as_ref().execute("COMMIT", turso::params![]).await.map_err(Into::into).map(|_| ())
	}

	async fn rollback_concurrent(conn: &cache::Connection) -> Result<()> {
		match conn.as_ref().execute("ROLLBACK", turso::params![]).await {
			Ok(_) => Ok(()),
			Err(e) => Err(anyhow::anyhow!("Rollback failed: {e}")),
		}
	}

	/// Configure database for MVCC concurrent writes
	async fn configure_database_for_mvcc(turso_db: &turso::Database) -> Result<()> {
		let conn = turso_db.connect()?;

		// Enable MVCC mode - required for BEGIN CONCURRENT (Turso 0.4.0+)
		conn.execute("PRAGMA journal_mode=experimental_mvcc", turso::params![]).await.ok();
		conn.execute("PRAGMA busy_timeout=600000", turso::params![]).await.ok(); // 10 minutes for large concurrent operations
		conn.execute("PRAGMA synchronous=NORMAL", turso::params![]).await.ok();
		conn.execute("PRAGMA temp_store=memory", turso::params![]).await.ok();
		conn.execute("PRAGMA wal_autocheckpoint=1000", turso::params![]).await.ok(); // Better WAL handling for large writes
		conn.execute("PRAGMA cache_size=-256000", turso::params![]).await.ok(); // 256MB cache for large batch operations

		// Explicitly drop connection to ensure it's closed before returning
		drop(conn);

		Ok(())
	}

	/// Begin an immediate transaction for DDL operations (CREATE TABLE, etc.)
	/// DDL operations are not compatible with BEGIN CONCURRENT in MVCC mode.
	/// This uses BEGIN IMMEDIATE which acquires a write lock but allows schema changes.
	async fn begin_immediate(turso_db: &turso::Database) -> Result<cache::Connection> {
		let conn = turso_db.connect()?;

		// BEGIN IMMEDIATE acquires a write lock immediately, suitable for DDL
		// Retry with exponential backoff on transient errors
		let mut attempts = 0;
		let max_attempts = 100;
		
		loop {
			match conn.execute("BEGIN IMMEDIATE", turso::params![]).await {
				Ok(_) => break,
				Err(e) => {
					attempts += 1;
					if attempts >= max_attempts {
						return Err(anyhow::anyhow!("Failed to begin immediate transaction after {attempts} attempts: {e}"));
					}
					let base_delay = std::cmp::min(10 * (1 << attempts.min(6)), 500);
					let jitter = fastrand::u64(0..base_delay / 2);
					tokio::time::sleep(std::time::Duration::from_millis(base_delay + jitter)).await;
				}
			}
		}

		let cached_conn = cache::Connection::new(conn);
		Ok(cached_conn)
	}

	/// Commit an immediate transaction
	async fn commit_immediate(conn: &cache::Connection) -> Result<()> {
		conn.as_ref().execute("COMMIT", turso::params![]).await.map_err(Into::into).map(|_| ())
	}

	/// Checkpoint the WAL (Write-Ahead Log) to flush pending writes to the main database file.
	/// This should be called after large batch operations to ensure data is persisted.
	/// Uses PRAGMA `wal_checkpoint(TRUNCATE)` to checkpoint and truncate the WAL file.
	async fn checkpoint_wal(turso_db: &turso::Database) -> Result<()> {
		let conn = turso_db.connect()?;
		
		// TRUNCATE mode: checkpoint and truncate the WAL file
		// This ensures all data is written to the main database file
		// Use query() instead of execute() because PRAGMA wal_checkpoint returns rows
		match conn.query("PRAGMA wal_checkpoint(TRUNCATE)", turso::params![]).await {
			Ok(mut rows) => {
				// Consume the result set (contains busy, log, checkpointed columns)
				while let Ok(Some(_)) = rows.next().await {}
				tracing::debug!("WAL checkpoint completed successfully");
				Ok(())
			}
			Err(e) => {
				tracing::warn!("WAL checkpoint failed: {e}");
				// Try PASSIVE checkpoint as fallback (doesn't block)
				if let Ok(mut rows) = conn.query("PRAGMA wal_checkpoint(PASSIVE)", turso::params![]).await {
					while let Ok(Some(_)) = rows.next().await {}
				}
				Ok(())
			}
		}
	}
}
