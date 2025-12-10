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
	/// Uses BEGIN CONCURRENT which requires MVCC to be enabled on the database (via `Builder::with_mvcc(true)`)
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

		// Apply basic concurrent write configuration
		conn.execute("PRAGMA journal_mode=WAL", turso::params![]).await.ok();
		conn.execute("PRAGMA busy_timeout=600000", turso::params![]).await.ok(); // 10 minutes for large concurrent operations
		conn.execute("PRAGMA synchronous=NORMAL", turso::params![]).await.ok();
		conn.execute("PRAGMA temp_store=memory", turso::params![]).await.ok();
		conn.execute("PRAGMA wal_autocheckpoint=1000", turso::params![]).await.ok(); // Better WAL handling for large writes
		conn.execute("PRAGMA cache_size=-64000", turso::params![]).await.ok(); // 64MB cache for performance

		// Explicitly drop connection to ensure it's closed before returning
		drop(conn);

		Ok(())
	}
}
