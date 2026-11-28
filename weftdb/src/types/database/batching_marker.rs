//! The durable "this aspect has been batched" marker.
//!
//! The batch consumer (`weft-orchestration`'s `build_incremental_unprocessed_queue`) used to
//! call a run a first run whenever the batch tables happened to be empty, and a first run
//! rebuilds every sliding window over the aspect's whole history. Pattern extraction drains
//! both batch tables, so on an aspect whose extraction keeps up every run looked like a
//! first run: each one re-read the whole history and built one batch per point, only for
//! the extracted-batch dedupe to discard nearly all of them. That work (and its transient
//! heap) grew with the history on every run, and once a window aged past the dedupe's
//! retention (`WEFT_EXTRACTED_BATCH_RETENTION_SECS`, 48 hours) it was queued and extracted
//! again as a duplicate occurrence.
//!
//! The marker is one row in the aspect's unprocessed batches DB, written after the first
//! batches are stored and kept until [`Inputs::clear_unprocessed_batches`] resets the
//! aspect. The consumer treats an aspect with the marker as never being on its first run.
//! The table is created on first use (`CREATE TABLE IF NOT EXISTS`), so stores written
//! before it existed need no migration: their first batch insert after the upgrade writes
//! it, and until then the old empty-tables rule applies.
//!
//! [`Inputs::clear_unprocessed_batches`]: crate::database::traits::Inputs::clear_unprocessed_batches

use anyhow::Result;

use super::{traits::Connection as _, Database};
use crate::{database::traits::DatabaseStructure as _, AspectId};

const CREATE: &str = "CREATE TABLE IF NOT EXISTS batching_marker (id INTEGER PRIMARY KEY, first_batched_at INTEGER NOT NULL)";

impl Database {
	/// Whether `aspect_id` has had batches stored since it was created or last cleared (see
	/// the module docs). A store whose unprocessed batches DB predates the marker table
	/// answers false.
	///
	/// # Errors
	/// Returns an error if the aspect's unprocessed batches DB cannot be opened or read.
	pub async fn batching_started(&self, aspect_id: &AspectId) -> Result<bool> {
		let db = self.get_unprocessed_batches_db(aspect_id).await?;
		let db_path = self.get_unprocessed_batches_db_path(aspect_id).await?;
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		drop(db);
		let found = match conn.as_ref().query("SELECT 1 FROM batching_marker LIMIT 1", turso::params![]).await {
			Ok(mut rows) => rows.next().await.map(|row| row.is_some()),
			Err(e) if e.to_string().contains("no such table") => Ok(false),
			Err(e) => Err(e),
		};
		match found {
			Ok(found) => {
				Self::commit_concurrent(&conn).await?;
				Ok(found)
			}
			Err(e) => {
				Self::rollback_after_error(&conn).await;
				Err(anyhow::anyhow!("Failed to read the batching marker for aspect {aspect_id}: {e}"))
			}
		}
	}

	/// Record that `aspect_id` has had batches stored. Idempotent; the first write's time is
	/// kept.
	pub(crate) async fn mark_batching_started(&self, aspect_id: &AspectId) -> Result<()> {
		if self.batching_started(aspect_id).await? {
			return Ok(());
		}
		let now = chrono::Utc::now().timestamp_millis();
		self.write_marker(aspect_id, &[CREATE], Some(now)).await
	}

	/// Forget that `aspect_id` was batched, so its next consumer run is a first run again.
	pub(crate) async fn reset_batching_started(&self, aspect_id: &AspectId) -> Result<()> {
		self.write_marker(aspect_id, &[CREATE, "DELETE FROM batching_marker"], None).await
	}

	/// Run `statements` and, with `insert_at`, insert the marker row, in one immediate
	/// transaction (the `CREATE` is DDL, which `BEGIN CONCURRENT` does not allow).
	async fn write_marker(&self, aspect_id: &AspectId, statements: &[&str], insert_at: Option<i64>) -> Result<()> {
		let db = self.get_unprocessed_batches_db(aspect_id).await?;
		let conn = Self::begin_immediate(&db).await?;
		drop(db);
		let run = async {
			for sql in statements {
				conn.as_ref().execute(sql, turso::params![]).await?;
			}
			if let Some(at) = insert_at {
				conn.as_ref().execute("INSERT INTO batching_marker (id, first_batched_at) SELECT 1, ? WHERE NOT EXISTS (SELECT 1 FROM batching_marker)", turso::params![at]).await?;
			}
			turso::Result::Ok(())
		};
		if let Err(e) = run.await {
			Self::rollback_after_error(&conn).await;
			return Err(anyhow::anyhow!("Failed to write the batching marker for aspect {aspect_id}: {e}"));
		}
		Self::commit_immediate(&conn).await
	}
}
