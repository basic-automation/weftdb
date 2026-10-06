//! Open-time proof that a control-plane database commits the way the durability
//! promise assumes (docs/design/crash-consistency.md sections 1.3 and 5.5, OPEN step 2).
//!
//! Every control-plane write is a `BEGIN CONCURRENT` transaction, and a COMMIT counts
//! as durable because, under MVCC with `synchronous=FULL`, Turso fsyncs the logical
//! log before COMMIT returns. Neither is guaranteed by asking for it:
//!
//! - the switch to MVCC can fail, for example on a file another handle in this process
//!   holds in multiprocess-WAL mode. The stores used to discard that error with
//!   `.ok()` and carry on in WAL mode. (The switch always looked like a failure to
//!   `execute` anyway, because it answers with a row.)
//! - `synchronous` is per connection, and every write path opens a fresh one. Turso
//!   defaults a new connection to FULL today; nothing else makes it so.
//!
//! So each store's open switches to MVCC, then reads both settings back and refuses to
//! open on anything else.

use anyhow::{bail, Context, Result};
use turso::Value;

/// What `PRAGMA journal_mode` reports for Turso's MVCC journal (`turso_core`
/// `JournalMode::Mvcc`, displayed as `mvcc`; `experimental_mvcc` is the name it is
/// requested by).
const MVCC: &str = "mvcc";

/// What `PRAGMA synchronous` reports for FULL (`turso_core` `SyncMode::Full`).
const SYNCHRONOUS_FULL: i64 = 2;

/// Switch the database behind `conn` to MVCC, then prove that it is in MVCC and that a
/// fresh connection to `db`, like the ones every write opens, syncs FULL. `name` (the
/// database's path) is what the errors call it.
///
/// # Errors
///
/// If either pragma cannot be read, if the journal mode is not MVCC (the error carries
/// the switch's own error, when it had one), or if a fresh connection is not FULL.
pub async fn enable_mvcc_full(db: &turso::Database, conn: &turso::Connection, name: &str) -> Result<()> {
	// A query, not `execute`: the switch answers with the mode now in effect, and
	// `execute` fails on that row ("unexpected row during execution") even when the
	// switch worked. Its result is only kept to explain a failure; the read-back below
	// is what decides.
	let switched = pragma(conn, "PRAGMA journal_mode=experimental_mvcc").await;
	let mode = pragma(conn, "PRAGMA journal_mode").await.with_context(|| format!("{name}: reading PRAGMA journal_mode"))?;
	require_mvcc(name, &mode, switched.err())?;
	let fresh = db.connect().with_context(|| format!("{name}: connecting to read PRAGMA synchronous"))?;
	let synchronous = pragma(&fresh, "PRAGMA synchronous").await.with_context(|| format!("{name}: reading PRAGMA synchronous"))?;
	require_full_sync(name, &synchronous)
}

/// The single value a pragma query answers with.
async fn pragma(conn: &turso::Connection, sql: &str) -> Result<Value> {
	let mut rows = conn.query(sql, ()).await?;
	match rows.next().await? {
		Some(row) => Ok(row.get_value(0)?),
		None => bail!("{sql} returned no row"),
	}
}

/// Fail unless `mode` is MVCC. `switch_error` is why the switch to MVCC failed, if it
/// reported an error.
fn require_mvcc(name: &str, mode: &Value, switch_error: Option<anyhow::Error>) -> Result<()> {
	if matches!(mode, Value::Text(mode) if mode.eq_ignore_ascii_case(MVCC)) {
		return Ok(());
	}
	let cause = switch_error.map_or_else(String::new, |e| format!(" (switching to MVCC failed: {e:#})"));
	bail!("{name} is in journal mode {}, not MVCC{cause}. WeftDB's control plane commits with BEGIN CONCURRENT and counts a COMMIT as durable only because MVCC fsyncs its log first, so it will not open this database in any other mode", show(mode))
}

/// Fail unless `synchronous` is FULL.
fn require_full_sync(name: &str, synchronous: &Value) -> Result<()> {
	if matches!(synchronous, Value::Integer(SYNCHRONOUS_FULL)) {
		return Ok(());
	}
	bail!("a new connection to {name} reports PRAGMA synchronous={}, not FULL ({SYNCHRONOUS_FULL}). Without FULL a COMMIT can return before its log reaches the disk, so WeftDB will not open this database", show(synchronous))
}

/// A pragma value as SQL would print it.
fn show(value: &Value) -> String {
	match value {
		Value::Text(text) => text.clone(),
		Value::Integer(n) => n.to_string(),
		other => format!("{other:?}"),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn a_new_database_is_switched_to_mvcc_and_passes() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("probe.db");
		let path = path.to_string_lossy();
		let db = turso::Builder::new_local(&path).build().await.unwrap();
		let conn = db.connect().unwrap();
		assert_eq!(show(&pragma(&conn, "PRAGMA journal_mode").await.unwrap()), "wal", "Turso creates a database in WAL mode");
		enable_mvcc_full(&db, &conn, &path).await.expect("the switch takes and a fresh connection is FULL");
		assert_eq!(show(&pragma(&db.connect().unwrap(), "PRAGMA journal_mode").await.unwrap()), MVCC);

		let memory = turso::Builder::new_local(":memory:").build().await.unwrap();
		let memory_conn = memory.connect().unwrap();
		enable_mvcc_full(&memory, &memory_conn, ":memory:").await.expect("in-memory stores (tests) run MVCC too");
		drop(memory_conn);
		drop(memory);
	}

	#[test]
	fn only_mvcc_passes_the_journal_mode_check() {
		require_mvcc("x.db", &Value::Text("mvcc".into()), None).unwrap();
		require_mvcc("x.db", &Value::Text("MVCC".into()), None).unwrap();
		for mode in ["wal", "delete", "experimental_mvcc", ""] {
			let err = require_mvcc("x.db", &Value::Text(mode.into()), None).unwrap_err().to_string();
			assert!(err.starts_with(&format!("x.db is in journal mode {mode}, not MVCC.")), "{err}");
		}
		let err = require_mvcc("x.db", &Value::Text("wal".into()), Some(anyhow::anyhow!("database is readonly"))).unwrap_err().to_string();
		assert!(err.contains("not MVCC (switching to MVCC failed: database is readonly)"), "the switch's own error explains the mode: {err}");
		assert!(require_mvcc("x.db", &Value::Null, None).is_err());
	}

	#[test]
	fn only_full_passes_the_synchronous_check() {
		require_full_sync("x.db", &Value::Integer(2)).unwrap();
		for (value, shown) in [(Value::Integer(0), "0"), (Value::Integer(1), "1"), (Value::Integer(3), "3"), (Value::Text("2".into()), "2"), (Value::Null, "Null")] {
			let err = require_full_sync("x.db", &value).unwrap_err().to_string();
			assert!(err.starts_with(&format!("a new connection to x.db reports PRAGMA synchronous={shown}, not FULL (2).")), "{err}");
		}
	}
}
