//! libSQL aspect-schema catalog (roadmap **Phase 4.3**, control-plane registry).
//!
//! The segment-index control plane ([`SegmentIndexStore`](crate::SegmentIndexStore))
//! answers *where* an aspect's sealed segments are; this is the other half the
//! roadmap names — the catalog that records *how* an aspect is encoded: its declared
//! [`AspectSchema`] (physical value [`PhysicalType`], the per-value error bound, and
//! the timestamp [`TimeUnit`]). With it the store no longer needs the schema passed
//! in on every read — it looks the aspect's declaration up in the control plane.
//!
//! Keyed by `(database, subject, aspect)` — the catalog hierarchy the rest of DSP
//! uses (a database holds subjects; a subject holds aspects). This is squarely a
//! **control-plane** component (hard constraint #3): it holds schema *metadata*, no
//! measurements. The declaration round-trips faithfully — the `PhysicalType` and
//! `TimeUnit` as JSON (so a `ScaledI64 { scale }` is preserved) and the
//! `BigDecimal` tolerance as plain text (no silent float downcast, hard
//! constraint #4).

use std::str::FromStr;

use anyhow::{bail, Result};
use bigdecimal::BigDecimal;
use dsp_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use turso::{Builder, Value};

/// A durable, libSQL-backed registry of aspect [`AspectSchema`] declarations.
///
/// Open with [`AspectCatalog::open`] (a file path) or
/// [`AspectCatalog::open_in_memory`] (tests); declare an aspect's schema with
/// [`declare`](AspectCatalog::declare); look it up with [`get`](AspectCatalog::get).
pub struct AspectCatalog {
	db: turso::Database,
}

impl AspectCatalog {
	/// Open (creating if absent) the catalog DB at `path`, enabling MVCC and ensuring
	/// the `aspect_schema` table exists.
	///
	/// # Errors
	///
	/// Propagates any libSQL connection or DDL failure.
	pub async fn open(path: &str) -> Result<Self> {
		let db = Builder::new_local(path).build().await?;
		let conn = db.connect()?;
		conn.execute("PRAGMA journal_mode=experimental_mvcc", turso::params![]).await.ok();
		conn.execute("PRAGMA busy_timeout=600000", turso::params![]).await.ok();
		conn.execute(
			"CREATE TABLE IF NOT EXISTS aspect_schema (
				database TEXT NOT NULL,
				subject TEXT NOT NULL,
				aspect TEXT NOT NULL,
				physical_type TEXT NOT NULL,
				value_tolerance TEXT NOT NULL,
				timestamp_unit TEXT NOT NULL,
				PRIMARY KEY (database, subject, aspect)
			)",
			turso::params![],
		)
		.await?;
		Ok(Self { db })
	}

	/// Open an ephemeral in-memory catalog (`:memory:`) for tests.
	///
	/// # Errors
	///
	/// Propagates any libSQL connection or DDL failure.
	pub async fn open_in_memory() -> Result<Self> {
		Self::open(":memory:").await
	}

	/// Snapshot this `aspect_catalog.db` to `dest` (a fresh file) via Turso's
	/// `VACUUM INTO`, verifying the copy opens and its rows match. The online, consistent
	/// control-plane backup primitive (roadmap Phase 7.4) — see
	/// [`snapshot_and_verify`](crate::snapshot_and_verify) for the consistency scope.
	///
	/// # Errors
	///
	/// Propagates a connection failure or any backup/verify failure.
	pub async fn backup_to(&self, dest: &std::path::Path) -> Result<crate::SnapshotReport> {
		self.backup_to_with(dest, crate::VerifyMode::default()).await
	}

	/// Snapshot this database to `dest` under an explicit [`VerifyMode`](crate::VerifyMode).
	///
	/// [`VerifyMode::SnapshotOnly`](crate::VerifyMode::SnapshotOnly) verifies the copy
	/// without re-reading the source, so it is the mode an **online** backup (the backup
	/// daemon) must use while writers are still committing.
	///
	/// # Errors
	///
	/// Propagates a connection failure or any backup/verify failure.
	pub async fn backup_to_with(&self, dest: &std::path::Path, mode: crate::VerifyMode) -> Result<crate::SnapshotReport> {
		let conn = self.db.connect()?;
		crate::types::backup::snapshot_with_verify(&conn, dest, mode).await
	}

	/// Declare (or re-declare) the schema for `(database, subject, aspect)`.
	///
	/// `INSERT OR REPLACE` makes a re-declaration overwrite, so the call is
	/// idempotent on the aspect key. The `PhysicalType`/`TimeUnit` store as JSON and
	/// the tolerance as plain text, round-tripping faithfully.
	///
	/// # Errors
	///
	/// Propagates any libSQL write failure or a metadata-serialization failure.
	pub async fn declare(&self, database: &str, subject: &str, aspect: &str, schema: &AspectSchema) -> Result<()> {
		let physical_type = serde_json::to_string(&schema.value)?;
		let timestamp_unit = serde_json::to_string(&schema.timestamp_unit)?;
		let conn = self.db.connect()?;
		conn.execute("BEGIN CONCURRENT", turso::params![]).await?;
		let res = conn.execute("INSERT OR REPLACE INTO aspect_schema (database, subject, aspect, physical_type, value_tolerance, timestamp_unit) VALUES (?, ?, ?, ?, ?, ?)", turso::params![database.to_string(), subject.to_string(), aspect.to_string(), physical_type, schema.value_tolerance.to_plain_string(), timestamp_unit]).await;
		match res {
			Ok(_) => {
				conn.execute("COMMIT", turso::params![]).await?;
				Ok(())
			}
			Err(e) => {
				conn.execute("ROLLBACK", turso::params![]).await.ok();
				bail!("aspect_schema declare failed: {e}")
			}
		}
	}

	/// The declared schema for `(database, subject, aspect)`, or [`None`] if the
	/// aspect has no declaration.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure, or a row whose stored declaration cannot
	/// be decoded.
	pub async fn get(&self, database: &str, subject: &str, aspect: &str) -> Result<Option<AspectSchema>> {
		let conn = self.db.connect()?;
		let mut rows = conn.query("SELECT physical_type, value_tolerance, timestamp_unit FROM aspect_schema WHERE database = ? AND subject = ? AND aspect = ?", turso::params![database.to_string(), subject.to_string(), aspect.to_string()]).await?;
		match rows.next().await? {
			Some(row) => {
				let physical_type: PhysicalType = serde_json::from_str(&row.get_value(0)?.as_text().cloned().unwrap_or_default())?;
				let value_tolerance = BigDecimal::from_str(&row.get_value(1)?.as_text().cloned().unwrap_or_default())?;
				let timestamp_unit: TimeUnit = serde_json::from_str(&row.get_value(2)?.as_text().cloned().unwrap_or_default())?;
				Ok(Some(AspectSchema::new(physical_type, value_tolerance, timestamp_unit)))
			}
			None => Ok(None),
		}
	}

	/// The aspect names declared under `(database, subject)`, in name order.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn list_aspects(&self, database: &str, subject: &str) -> Result<Vec<String>> {
		let conn = self.db.connect()?;
		let mut rows = conn.query("SELECT aspect FROM aspect_schema WHERE database = ? AND subject = ? ORDER BY aspect", turso::params![database.to_string(), subject.to_string()]).await?;
		let mut out = Vec::new();
		while let Some(row) = rows.next().await? {
			if let Value::Text(s) = row.get_value(0)? {
				out.push(s);
			}
		}
		Ok(out)
	}

	/// Every declared `(database, subject, aspect)` triple in the catalog, ordered by
	/// database, then subject, then aspect — a flat enumeration of the whole catalog
	/// for control-plane introspection or registry recovery.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn list_all(&self) -> Result<Vec<(String, String, String)>> {
		let conn = self.db.connect()?;
		let mut rows = conn.query("SELECT database, subject, aspect FROM aspect_schema ORDER BY database, subject, aspect", turso::params![]).await?;
		let mut out = Vec::new();
		while let Some(row) = rows.next().await? {
			let (Value::Text(database), Value::Text(subject), Value::Text(aspect)) = (row.get_value(0)?, row.get_value(1)?, row.get_value(2)?) else {
				continue;
			};
			out.push((database, subject, aspect));
		}
		Ok(out)
	}
}

#[cfg(test)]
mod tests {
	use dsp_physical_type::TimeUnit;

	use super::*;

	fn bd(s: &str) -> BigDecimal {
		BigDecimal::from_str(s).expect("parses")
	}

	#[tokio::test]
	async fn declare_then_get_round_trips_a_schema() {
		let catalog = AspectCatalog::open_in_memory().await.expect("opens");
		let schema = AspectSchema::new(PhysicalType::ScaledI64 { scale: 4 }, bd("0.00005"), TimeUnit::Micros);
		catalog.declare("market", "BTCUSD", "price", &schema).await.expect("declares");
		let got = catalog.get("market", "BTCUSD", "price").await.expect("reads");
		drop(catalog);
		// The declaration round-trips field-for-field, scale and tolerance included.
		assert_eq!(got, Some(schema));
	}

	#[tokio::test]
	async fn get_unknown_aspect_is_none() {
		let catalog = AspectCatalog::open_in_memory().await.expect("opens");
		let got = catalog.get("market", "BTCUSD", "missing").await.expect("reads");
		drop(catalog);
		assert_eq!(got, None);
	}

	#[tokio::test]
	async fn redeclare_replaces_the_schema() {
		let catalog = AspectCatalog::open_in_memory().await.expect("opens");
		catalog.declare("d", "s", "a", &AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)).await.expect("declares");
		catalog.declare("d", "s", "a", &AspectSchema::new(PhysicalType::F32, bd("0.01"), TimeUnit::Millis)).await.expect("re-declares");
		let got = catalog.get("d", "s", "a").await.expect("reads");
		drop(catalog);
		assert_eq!(got, Some(AspectSchema::new(PhysicalType::F32, bd("0.01"), TimeUnit::Millis)));
	}

	#[tokio::test]
	async fn list_all_enumerates_every_declaration_ordered() {
		let catalog = AspectCatalog::open_in_memory().await.expect("opens");
		let f64 = AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds);
		// Declared out of order across two databases and subjects.
		catalog.declare("market", "ETHUSD", "price", &f64).await.expect("declares");
		catalog.declare("iot", "sensor-7", "temp", &f64).await.expect("declares");
		catalog.declare("market", "BTCUSD", "volume", &f64).await.expect("declares");
		catalog.declare("market", "BTCUSD", "price", &f64).await.expect("declares");
		let all = catalog.list_all().await.expect("lists");
		drop(catalog);
		assert_eq!(all, vec![("iot".to_string(), "sensor-7".to_string(), "temp".to_string()), ("market".to_string(), "BTCUSD".to_string(), "price".to_string()), ("market".to_string(), "BTCUSD".to_string(), "volume".to_string()), ("market".to_string(), "ETHUSD".to_string(), "price".to_string()),]);
	}

	#[tokio::test]
	async fn list_all_is_empty_for_a_fresh_catalog() {
		let catalog = AspectCatalog::open_in_memory().await.expect("opens");
		let all = catalog.list_all().await.expect("lists");
		drop(catalog);
		assert!(all.is_empty());
	}

	#[tokio::test]
	async fn list_aspects_scopes_to_subject_in_order() {
		let catalog = AspectCatalog::open_in_memory().await.expect("opens");
		let f64 = AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds);
		catalog.declare("d", "s", "temp", &f64).await.expect("declares");
		catalog.declare("d", "s", "humidity", &f64).await.expect("declares");
		// A different subject and database are isolated.
		catalog.declare("d", "other", "pressure", &f64).await.expect("declares");
		catalog.declare("other_db", "s", "wind", &f64).await.expect("declares");
		let aspects = catalog.list_aspects("d", "s").await.expect("lists");
		drop(catalog);
		assert_eq!(aspects, vec!["humidity".to_string(), "temp".to_string()]);
	}
}
