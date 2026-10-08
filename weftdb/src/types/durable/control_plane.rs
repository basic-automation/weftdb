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
//!
//! **The switch's header** (design section 1.3, closed by S6). `PRAGMA
//! journal_mode=experimental_mvcc` writes page 1 with the MVCC header straight into the
//! DB file and does not fsync it; COMMITs then fsync only the `-log`. A power cut before
//! anything else syncs the DB file can leave a header that still reads WAL beside a
//! `-log` of acknowledged commits, and Turso refuses to open that pair. Measured with a
//! recording I/O backend (`turso_probe`), Turso 0.8.1 does sync the DB file during the
//! switch of a database that never ran MVCC (its MVCC metadata bootstrap backfills a
//! new table through a checkpoint), but not the switch of one that already carries
//! Turso's MVCC metadata, such as a database switched back to WAL. So whenever an open
//! is the one that switched, [`enable_mvcc_full`] has Turso itself sync the DB file: it
//! commits one MVCC transaction (rewriting `user_version` with its own value) and runs a
//! TRUNCATE checkpoint, which backfills that commit through the WAL and fsyncs the DB
//! file, header included. WeftDB never fsyncs the file itself: Turso's lock is a
//! process-associated `fcntl` lock, which closing any other descriptor on the file
//! would release.

use anyhow::{bail, Context, Result};
use turso::Value;

/// What `PRAGMA journal_mode` reports for Turso's MVCC journal (`turso_core`
/// `JournalMode::Mvcc`, displayed as `mvcc`; `experimental_mvcc` is the name it is
/// requested by).
const MVCC: &str = "mvcc";

/// What `PRAGMA synchronous` reports for FULL (`turso_core` `SyncMode::Full`).
const SYNCHRONOUS_FULL: i64 = 2;

/// Switch the database behind `conn` to MVCC, then prove that it is in MVCC and that a
/// fresh connection to `db`, like the ones every write opens, syncs FULL. When this call
/// is the one that switched the database, it also makes the header the switch wrote
/// durable (see the module documentation). `name` (the database's path) is what the
/// errors call it.
///
/// # Errors
///
/// If a pragma cannot be read, if the journal mode is not MVCC (the error carries the
/// switch's own error, when it had one), if a fresh connection is not FULL, or if the
/// switched header cannot be synced.
pub async fn enable_mvcc_full(db: &turso::Database, conn: &turso::Connection, name: &str) -> Result<()> {
	let before = pragma(conn, "PRAGMA journal_mode").await.with_context(|| format!("{name}: reading PRAGMA journal_mode"))?;
	// A query, not `execute`: the switch answers with the mode now in effect, and
	// `execute` fails on that row ("unexpected row during execution") even when the
	// switch worked. Its result is only kept to explain a failure; the read-back below
	// is what decides.
	let switched = pragma(conn, "PRAGMA journal_mode=experimental_mvcc").await;
	let mode = pragma(conn, "PRAGMA journal_mode").await.with_context(|| format!("{name}: reading PRAGMA journal_mode"))?;
	require_mvcc(name, &mode, switched.err())?;
	let fresh = db.connect().with_context(|| format!("{name}: connecting to read PRAGMA synchronous"))?;
	#[cfg(test)]
	downgrade_for_test(&fresh, name).await?;
	let synchronous = pragma(&fresh, "PRAGMA synchronous").await.with_context(|| format!("{name}: reading PRAGMA synchronous"))?;
	require_full_sync(name, &synchronous)?;
	if is_mvcc(&before) {
		return Ok(());
	}
	// On the connection just proven FULL, so that the checkpoint does fsync.
	sync_switched_header(&fresh).await.with_context(|| format!("{name}: syncing the MVCC header this open wrote (a commit, then a TRUNCATE checkpoint)"))
}

/// Make the MVCC header the switch wrote durable, through Turso: commit one transaction
/// that changes nothing (`user_version` rewritten with its own value), then checkpoint
/// it with TRUNCATE. The checkpoint writes that commit through the WAL into the DB file
/// and fsyncs the file before it truncates the log, which syncs page 1 with it. A
/// checkpoint alone is not enough: with nothing committed since the switch it backfills
/// nothing and syncs nothing.
async fn sync_switched_header(conn: &turso::Connection) -> Result<()> {
	let user_version = match pragma(conn, "PRAGMA user_version").await.context("reading PRAGMA user_version")? {
		Value::Integer(version) => version,
		other => bail!("PRAGMA user_version answered {}, not an integer", show(&other)),
	};
	conn.execute(format!("PRAGMA user_version = {user_version}"), ()).await.context("rewriting PRAGMA user_version")?;
	pragma(conn, "PRAGMA wal_checkpoint(TRUNCATE)").await.context("PRAGMA wal_checkpoint(TRUNCATE)")?;
	Ok(())
}

#[cfg(test)]
tokio::task_local! {
	/// Test-only `(file, statement)`: before the probe reads `PRAGMA synchronous`, it runs
	/// `statement` (say `PRAGMA synchronous=OFF`) on the fresh connection of the database
	/// whose file name is `file`. That stands in for a Turso whose new connections are not
	/// FULL, which Turso 0.8 never produces, so without it nothing could show the real
	/// open refusing one. Task-local, so the opens of tests running alongside never see it.
	pub(crate) static NEW_CONNECTION_OVERRIDE: (&'static str, &'static str);
}

/// Apply [`NEW_CONNECTION_OVERRIDE`] to `fresh` when it names the database at `name`.
#[cfg(test)]
async fn downgrade_for_test(fresh: &turso::Connection, name: &str) -> Result<()> {
	let Ok(Some(statement)) = NEW_CONNECTION_OVERRIDE.try_with(|(file, statement)| (std::path::Path::new(name).file_name() == Some(std::ffi::OsStr::new(file))).then_some(*statement)) else { return Ok(()) };
	fresh.execute(statement, ()).await.with_context(|| format!("{name}: test override {statement}"))?;
	Ok(())
}

/// The single value a pragma query answers with.
async fn pragma(conn: &turso::Connection, sql: &str) -> Result<Value> {
	let mut rows = conn.query(sql, ()).await?;
	match rows.next().await? {
		Some(row) => Ok(row.get_value(0)?),
		None => bail!("{sql} returned no row"),
	}
}

/// Whether a `PRAGMA journal_mode` answer is MVCC.
fn is_mvcc(mode: &Value) -> bool {
	matches!(mode, Value::Text(mode) if mode.eq_ignore_ascii_case(MVCC))
}

/// Fail unless `mode` is MVCC. `switch_error` is why the switch to MVCC failed, if it
/// reported an error.
fn require_mvcc(name: &str, mode: &Value, switch_error: Option<anyhow::Error>) -> Result<()> {
	if is_mvcc(mode) {
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

	/// A database at `path` that ran MVCC, holds a committed row, and was switched back
	/// to WAL: it keeps Turso's MVCC metadata, so switching it to MVCC again is the case
	/// Turso's own bootstrap does not sync.
	async fn switched_back_to_wal(path: &str) {
		let db = turso::Builder::new_local(path).build().await.unwrap();
		let conn = db.connect().unwrap();
		enable_mvcc_full(&db, &conn, path).await.unwrap();
		conn.execute("CREATE TABLE t (x INTEGER)", ()).await.unwrap();
		conn.execute("INSERT INTO t VALUES (1)", ()).await.unwrap();
		assert_eq!(show(&pragma(&conn, "PRAGMA journal_mode=wal").await.unwrap()), "wal");
		drop(conn);
		drop(db);
	}

	/// Design section 1.3: the switch to MVCC writes page 1 and, on a database that
	/// already carries Turso's MVCC metadata, nothing in Turso fsyncs it; a COMMIT then
	/// syncs only the `-log`, and a power cut leaves a WAL header beside a log of
	/// acknowledged commits, which Turso refuses to open. The open that switches has
	/// Turso sync the DB file (a commit and a TRUNCATE checkpoint), so when it returns
	/// every write to the DB file, the header included, is synced. Without that step the
	/// recording backend shows the header write as the DB file's last, unsynced.
	#[tokio::test]
	async fn the_open_that_switches_to_mvcc_leaves_the_header_synced() {
		use crate::types::durable::turso_probe::ProbeIo;

		let dir = tempfile::tempdir().unwrap();
		for (case, back_to_wal) in [("a new database", false), ("a database switched back to WAL", true)] {
			let path = dir.path().join(format!("{back_to_wal}.db"));
			let path = path.to_string_lossy();
			let file = format!("{back_to_wal}.db");
			if back_to_wal {
				switched_back_to_wal(&path).await;
			}
			let io = ProbeIo::new().unwrap();
			let db = turso::Builder::new_local(&path).with_io_impl(io.clone()).build().await.unwrap();
			let conn = db.connect().unwrap();
			enable_mvcc_full(&db, &conn, &path).await.expect("switches");
			let header_written = io.writes(&file).iter().any(|&(pos, _)| pos == 0);
			let synced = io.synced_since_last_write(&file);
			// What the store's open does next: a commit, which syncs only the `-log`.
			conn.execute("CREATE TABLE IF NOT EXISTS u (x INTEGER)", ()).await.unwrap();
			conn.execute("INSERT INTO u VALUES (2)", ()).await.unwrap();
			let still_synced = io.synced_since_last_write(&file);
			let mode = show(&pragma(&conn, "PRAGMA journal_mode").await.unwrap());
			drop(conn);
			drop(db);
			assert!(header_written, "{case}: the switch wrote page 1: {:?}", io.events());
			assert!(synced, "{case}: every write to the DB file is synced when the switch returns: {:?}", io.events());
			assert!(still_synced, "{case}: and the commits after it do not write the DB file unsynced: {:?}", io.events());
			assert_eq!(mode, MVCC, "{case}");
		}
	}

	/// An open that finds the database in MVCC already switches nothing, so it writes
	/// nothing to the DB file and runs no checkpoint; the rows are all there.
	#[tokio::test]
	async fn an_open_of_an_mvcc_database_writes_nothing_to_it() {
		use crate::types::durable::turso_probe::ProbeIo;

		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("mvcc.db");
		let path = path.to_string_lossy();
		switched_back_to_wal(&path).await;
		let db = turso::Builder::new_local(&path).build().await.unwrap();
		let conn = db.connect().unwrap();
		enable_mvcc_full(&db, &conn, &path).await.unwrap();
		drop(conn);
		drop(db);

		let io = ProbeIo::new().unwrap();
		let db = turso::Builder::new_local(&path).with_io_impl(io.clone()).build().await.unwrap();
		let conn = db.connect().unwrap();
		enable_mvcc_full(&db, &conn, &path).await.expect("passes");
		let rows = pragma(&conn, "SELECT COUNT(*) FROM t").await.unwrap();
		drop(conn);
		drop(db);
		assert_eq!(rows, Value::Integer(1));
		assert_eq!(io.writes("mvcc.db"), Vec::new(), "{:?}", io.events());
	}

	/// The probe reads `synchronous` from a fresh connection and refuses the open on
	/// anything but FULL. Turso 0.8 always answers FULL, so the override plays a Turso
	/// that does not; without the probe (or with it reading some other connection) this
	/// open would pass.
	#[tokio::test]
	async fn a_fresh_connection_that_is_not_full_fails_the_probe() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("probe.db");
		let path = path.to_string_lossy();
		let db = turso::Builder::new_local(&path).build().await.unwrap();
		let conn = db.connect().unwrap();
		for (statement, shown) in [("PRAGMA synchronous=OFF", "0"), ("PRAGMA synchronous=NORMAL", "1")] {
			let err = NEW_CONNECTION_OVERRIDE.scope(("probe.db", statement), enable_mvcc_full(&db, &conn, &path)).await.expect_err(statement).to_string();
			assert_eq!(err, format!("a new connection to {path} reports PRAGMA synchronous={shown}, not FULL (2). Without FULL a COMMIT can return before its log reaches the disk, so WeftDB will not open this database"));
		}
		// The override names one file; any other database is left alone.
		NEW_CONNECTION_OVERRIDE.scope(("other.db", "PRAGMA synchronous=OFF"), enable_mvcc_full(&db, &conn, &path)).await.expect("only the named file is downgraded");
		enable_mvcc_full(&db, &conn, &path).await.expect("a fresh connection is FULL again");
		drop(conn);
		drop(db);
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
