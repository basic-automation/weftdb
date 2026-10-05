use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use super::Database;
pub use crate::types::database::traits::config::Config;
use crate::{cache, database::traits::Connection, DatabaseCache};

/// `COMMIT` the connection's open transaction; on failure, roll it back before returning
/// the error.
///
/// A failed commit (an MVCC write-write conflict, an aborted commit dependency, or a
/// transaction poisoned by an abandoned write statement) means **nothing was written**, so
/// the error must reach the caller. The best-effort `ROLLBACK` leaves the connection
/// without a dangling transaction; its own error is ignored because the commit error is
/// the one that matters.
async fn commit_or_rollback(conn: &cache::Connection) -> Result<()> {
	match conn.as_ref().execute("COMMIT", turso::params![]).await {
		Ok(_) => Ok(()),
		Err(e) => {
			let _ = conn.as_ref().execute("ROLLBACK", turso::params![]).await;
			Err(anyhow::anyhow!("Commit failed; the transaction was rolled back: {e}"))
		}
	}
}

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
						tracing::error!("[begin_concurrent] Failed after {attempts} attempts: {e}");
						return Err(anyhow::anyhow!("Failed to begin concurrent transaction after {attempts} attempts: {e}"));
					}
					// Log retries to diagnose blocking
					if attempts == 1 || attempts % 10 == 0 {
						tracing::debug!("[begin_concurrent] Attempt {} failed: {}, retrying...", attempts, e);
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
		commit_or_rollback(conn).await
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

		// No `PRAGMA synchronous` here: it is per connection, so setting it on this throwaway
		// connection would change nothing. Every Turso connection starts in `SyncMode::Full`
		// (turso_core-0.8.1 database.rs:2591), which fsyncs the MVCC log on every COMMIT, and
		// WeftDB relies on that for its durability.

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
		commit_or_rollback(conn).await
	}

	/// Non-blocking WAL checkpoint using PASSIVE mode.
	/// This checkpoints as much as possible without blocking readers/writers.
	/// Use this during imports to avoid contention with concurrent operations.
	async fn checkpoint_wal_passive(turso_db: &turso::Database) -> Result<()> {
		let start = std::time::Instant::now();
		tracing::debug!("[checkpoint_wal_passive] Starting PASSIVE checkpoint...");
		let conn = turso_db.connect()?;

		// PASSIVE mode: checkpoint without blocking
		// Does not wait for readers/writers to finish
		match conn.query("PRAGMA wal_checkpoint(PASSIVE)", turso::params![]).await {
			Ok(mut rows) => {
				// Consume the result set
				while let Ok(Some(_)) = rows.next().await {}
				tracing::debug!("[checkpoint_wal_passive] PASSIVE checkpoint completed in {:?}", start.elapsed());
				Ok(())
			}
			Err(e) => {
				// Expected under MVCC: Turso rejects PASSIVE unless
				// `experimental_mvcc_passive_checkpoint` is set (turso_core-0.8.1
				// translate/pragma.rs:943-948). Nothing is lost; every committed transaction
				// is already fsynced in the `-log`, which Turso replays at open.
				tracing::warn!("[checkpoint_wal_passive] PASSIVE checkpoint not run ({e}): Turso rejects PASSIVE under MVCC; committed data stays safe in the -log");
				Ok(())
			}
		}
	}
}
