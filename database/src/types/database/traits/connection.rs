use anyhow::Result;
use crate::cache;


/// Trait for database structure operations
/// This trait defines the operations related to managing the structure of the database.
///
/// Add a Subject for observation -> track various aspects of the subject
#[async_trait::async_trait]
pub trait Connection {
        async fn begin_concurrent(turso_db: &turso::Database, cache_key: &str) -> Result<cache::Connection>;

        async fn commit_concurrent(conn: &cache::Connection) -> anyhow::Result<()>;

        async fn rollback_concurrent(conn: &cache::Connection) -> Result<()>;

        async fn configure_database_for_mvcc(turso_db: &turso::Database) -> Result<()>;
}