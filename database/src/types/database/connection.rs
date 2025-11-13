use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use super::Database;
pub use crate::types::database::traits::config::Config;
use crate::{cache, database::traits::Connection, DatabaseCache};

#[async_trait::async_trait]
impl Connection for Database {
	/// Get a cached database connection with MVCC concurrent transaction support
	/// Returns a cached connection if available, otherwise creates a new one
	async fn begin_concurrent(turso_db: &turso::Database, cache_key: &str, cache: Option<Arc<Mutex<DatabaseCache>>>) -> Result<cache::Connection> {
		let cache = cache.unwrap_or_else(|| Arc::new(Mutex::new(DatabaseCache::default())));

		// Try to get cached connection first
		if let Some(cached_conn) = cache.lock().await.get::<cache::Connection>(cache_key).await {
			return Ok(cached_conn);
		}

		// Create new connection if not cached
		let conn = turso_db.connect()?;

		// Try BEGIN CONCURRENT first for MVCC support
		match conn.execute("BEGIN CONCURRENT", turso::params![]).await {
			Ok(_) => {}
			Err(_) => {
				// Fallback to BEGIN IMMEDIATE for compatibility
				match conn.execute("BEGIN IMMEDIATE", turso::params![]).await {
					Ok(_) => {}
					Err(_) => {
						// Final fallback to regular BEGIN
						conn.execute("BEGIN", turso::params![]).await.map_err(|e| anyhow::anyhow!("Failed to begin transaction: {}", e))?;
					}
				}
			}
		}

		// Wrap in our Connection type and cache it
		let cached_conn = cache::Connection::new(conn);
		cache.lock().await.store::<cache::Connection>(cache_key, cached_conn.clone()).await;

		Ok(cached_conn)
	}

	async fn commit_concurrent(conn: &cache::Connection) -> anyhow::Result<()> {
		conn.as_ref().execute("COMMIT", turso::params![]).await.map_err(Into::into).map(|_| ())
	}

	async fn rollback_concurrent(conn: &cache::Connection) -> Result<()> {
		match conn.as_ref().execute("ROLLBACK", turso::params![]).await {
			Ok(_) => Ok(()),
			Err(e) => Err(anyhow::anyhow!("Rollback failed: {}", e)),
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

		// Test BEGIN CONCURRENT support
		if conn.execute("BEGIN CONCURRENT", turso::params![]).await.is_ok() {
			conn.execute("ROLLBACK", turso::params![]).await.ok();
		}

		Ok(())
	}
}
