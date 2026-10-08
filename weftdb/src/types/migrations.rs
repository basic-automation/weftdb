//! The control-plane migration registry (freeze design §4.3, slice FRE-12a).
//!
//! Every schema change of the four control-plane databases is a registered
//! [`Migration`], identified by an ordinal id (`0001_baseline`, `0002_s6_s7`, …) and
//! applied in that order. No database's open function runs DDL: a
//! [`SegmentStore`](crate::SegmentStore) opens its databases (pragmas and probes only),
//! then runs the migrations its `store_migrations` table does not list, each recording
//! itself there once it has applied. So a development store created half way through a
//! release still receives every later migration, and running the registry again is a
//! no-op.
//!
//! Each migration is idempotent (a crash between its DDL and its record re-runs it) and
//! runs its DDL outside `BEGIN CONCURRENT`, where Turso refuses DDL (ROADMAP.md,
//! "DDL inside BEGIN CONCURRENT"). A column it adds is added only when `PRAGMA
//! table_info` does not list it, and a racing duplicate-column error is matched exactly
//! ([`is_duplicate_column`]); any other error fails the open instead of leaving a store
//! half migrated behind a successful one.
//!
//! A migration also says what it needs from the store's floors: once it has applied, a
//! WeftDB must know `min_read_after` to read the store and `min_write_after` to write it
//! (both its layout by default). The open raises the floors in `STORE_FORMAT` *before* it
//! runs a migration (see `store_format`). A migration that leaves older writers able to
//! write (`older_writers_safe`) must ship a `rederive` hook, which a newer WeftDB runs
//! when it finds that an older one wrote the store since.
//!
//! The registry:
//!
//! - **`0001_baseline`** (layout 1): every table a pre-1.0 store has, exactly as the
//!   pre-1.0 opens created them, plus `store_meta` and `store_migrations`, which the
//!   registry itself needs.
//! - **`0002_s6_s7`** (layout 2): the write-once columns and tables of the
//!   crash-consistency design (S6, S7), `segment_index.series_id` (tags A7) and
//!   `aspect_seq.next_series_id`. It refuses a store whose index records a frame outside
//!   `segments/` ([`StoreError::UnsafeLegacyPath`]) before it changes anything.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use turso::Value;

use crate::{
	types::{
		durable::control_plane::connect, index_txn::{IndexOp, IndexTxn}
	}, StoreError
};

/// A migration step over the databases it is given; see [`ControlPlane`].
pub(crate) type MigrationFn = for<'a> fn(&'a ControlPlane<'a>) -> BoxFuture<'a, Result<()>>;

/// One control-plane database a migration runs against: the database and the name its
/// errors use (its path).
#[derive(Clone, Copy)]
pub(crate) struct Db<'a> {
	pub db: &'a turso::Database,
	pub name: &'a str,
}

/// The control-plane databases a migration runs against. A store passes all four; a
/// database opened on its own passes just itself, and each migration runs the part of
/// itself that belongs to the databases present.
#[derive(Clone, Copy, Default)]
pub(crate) struct ControlPlane<'a> {
	/// `segment_index.db`, which also holds `store_meta` and `store_migrations`.
	pub index: Option<Db<'a>>,
	/// `metadata.db`.
	pub metadata: Option<Db<'a>>,
	/// `aspect_catalog.db`.
	pub aspect_catalog: Option<Db<'a>>,
	/// `catalog.db`.
	pub catalog: Option<Db<'a>>,
}

/// One registered migration; see the module documentation.
pub(crate) struct Migration {
	/// The ordinal id, `NNNN_name`; the registry is in id order.
	pub id: &'static str,
	/// The release layout the migration belongs to.
	pub layout: u32,
	/// The read floor once it has applied.
	pub min_read_after: u32,
	/// The write floor once it has applied.
	pub min_write_after: u32,
	/// Whether a WeftDB of an older layout may still write the store once it has applied
	/// (then `min_write_after` stays below `layout` and `rederive` is set).
	pub older_writers_safe: bool,
	/// A check that runs before `apply` and refuses the store, changing nothing.
	pub precheck: Option<MigrationFn>,
	/// The idempotent DDL.
	pub apply: MigrationFn,
	/// Re-derive what the migration maintains, after an older WeftDB wrote the store.
	pub rederive: Option<MigrationFn>,
}

/// What a set of migrations needs of the store's marker: the newest layout among them,
/// and the floors once they have all applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Floors {
	pub layout: u32,
	pub min_read: u32,
	pub min_write: u32,
}

/// Every registered migration, in id order.
pub(crate) static REGISTRY: &[Migration] = &[Migration { id: "0001_baseline", layout: 1, min_read_after: 1, min_write_after: 1, older_writers_safe: false, precheck: None, apply: baseline, rederive: None }, Migration { id: "0002_s6_s7", layout: 2, min_read_after: 2, min_write_after: 2, older_writers_safe: false, precheck: Some(refuse_unsafe_legacy_paths), apply: s6_s7, rederive: None }];

/// The ordinal of a migration id: its leading digits (`0002_s6_s7` → 2). An id without
/// them sorts first.
fn ordinal(id: &str) -> u64 {
	id.split('_').next().and_then(|digits| digits.parse().ok()).unwrap_or(0)
}

/// The migrations of `registry` that `applied` does not list, in order.
pub(crate) fn pending<'r>(registry: &'r [Migration], applied: &BTreeSet<String>) -> Vec<&'r Migration> {
	registry.iter().filter(|migration| !applied.contains(migration.id)).collect()
}

/// The migrations of `registry` after `applied_through` (all of them for `None`): what a
/// store's marker says is still to run, before its databases are open to say exactly.
pub(crate) fn after<'r>(registry: &'r [Migration], applied_through: Option<&str>) -> Vec<&'r Migration> {
	let through = applied_through.map_or(0, ordinal);
	registry.iter().filter(|migration| ordinal(migration.id) > through).collect()
}

/// What `migrations` need of the marker, or `None` for no migration.
pub(crate) fn floors<'r>(migrations: impl IntoIterator<Item = &'r Migration>) -> Option<Floors> {
	migrations.into_iter().fold(None, |acc: Option<Floors>, migration| {
		let own = Floors { layout: migration.layout, min_read: migration.min_read_after, min_write: migration.min_write_after };
		Some(acc.map_or(own, |acc| Floors { layout: acc.layout.max(own.layout), min_read: acc.min_read.max(own.min_read), min_write: acc.min_write.max(own.min_write) }))
	})
}

/// The ids `store_migrations` lists in `index`: empty when the table does not exist yet
/// (a store no registry has run on).
///
/// # Errors
///
/// A failed read.
pub(crate) async fn applied(index: Db<'_>) -> Result<BTreeSet<String>> {
	let conn = connect(index.db).await.with_context(|| format!("{}: connecting", index.name))?;
	if !table_exists(&conn, "store_migrations").await.with_context(|| format!("{}: looking for store_migrations", index.name))? {
		return Ok(BTreeSet::new());
	}
	let mut rows = conn.query("SELECT id FROM store_migrations", ()).await.with_context(|| format!("{}: reading store_migrations", index.name))?;
	let mut ids = BTreeSet::new();
	while let Some(row) = rows.next().await? {
		if let Value::Text(id) = row.get_value(0)? {
			ids.insert(id);
		}
	}
	Ok(ids)
}

/// Run `migrations` in order against `cp`, which must hold all four databases: each one's
/// precheck, then its DDL, then its record in `store_migrations`. Returns the ids
/// applied.
///
/// # Errors
///
/// A precheck's refusal (nothing of that migration ran), or a failed DDL statement or
/// record (the migration re-runs at the next open).
pub(crate) async fn run(cp: &ControlPlane<'_>, migrations: &[&Migration]) -> Result<Vec<&'static str>> {
	let index = cp.index.context("running the store migrations needs segment_index.db")?;
	let mut ran = Vec::with_capacity(migrations.len());
	for migration in migrations {
		if let Some(precheck) = migration.precheck {
			precheck(cp).await.with_context(|| format!("checking the store before migration {}", migration.id))?;
		}
		(migration.apply)(cp).await.with_context(|| format!("applying migration {}", migration.id))?;
		let applied_ms = i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis()).unwrap_or(i64::MAX);
		IndexTxn::new(vec![IndexOp::RecordMigration { id: migration.id.to_string(), layout: migration.layout, applied_ms }]).run(index.db).await.with_context(|| format!("{}: recording migration {}", index.name, migration.id))?;
		ran.push(migration.id);
	}
	Ok(ran)
}

/// Run the re-derive hook of every migration in `registry` that declared older writers
/// safe and belongs to a layout newer than `last_written`, the layout of the WeftDB that
/// last wrote the store. Returns the ids re-derived.
///
/// # Errors
///
/// A failed hook.
pub(crate) async fn rederive(cp: &ControlPlane<'_>, registry: &[Migration], applied: &BTreeSet<String>, last_written: u32) -> Result<Vec<&'static str>> {
	let mut ran = Vec::new();
	for migration in registry.iter().filter(|migration| migration.older_writers_safe && migration.layout > last_written && applied.contains(migration.id)) {
		if let Some(hook) = migration.rederive {
			hook(cp).await.with_context(|| format!("re-deriving migration {} after an older WeftDB wrote the store", migration.id))?;
			ran.push(migration.id);
		}
	}
	Ok(ran)
}

/// Apply every registered migration's DDL to the databases `cp` holds, recording
/// nothing: what a control-plane database opened on its own (outside a store, so with
/// no marker and no floors) needs to be usable. Idempotent.
///
/// # Errors
///
/// A failed DDL statement.
pub(crate) async fn apply_standalone(cp: &ControlPlane<'_>) -> Result<()> {
	for migration in REGISTRY {
		(migration.apply)(cp).await.with_context(|| format!("applying migration {}", migration.id))?;
	}
	Ok(())
}

/// `segment_index` as every layout-1 store created it.
const SEGMENT_INDEX_TABLE: &str = "CREATE TABLE IF NOT EXISTS segment_index (
	aspect TEXT NOT NULL,
	id INTEGER NOT NULL,
	path TEXT NOT NULL,
	format_version INTEGER NOT NULL,
	physical_type TEXT,
	time_unit TEXT,
	row_count INTEGER NOT NULL,
	null_count INTEGER NOT NULL,
	time_sorted INTEGER NOT NULL,
	min_ts INTEGER,
	max_ts INTEGER,
	min_value TEXT,
	max_value TEXT,
	byte_len INTEGER NOT NULL,
	PRIMARY KEY (aspect, id)
)";

/// The covering range index over the time span that turns a time prune into an index
/// scan. Best effort, as it always was: a store without it prunes by a scan.
const SEGMENT_INDEX_TIME_INDEX: &str = "CREATE INDEX IF NOT EXISTS idx_segment_index_time ON segment_index(aspect, min_ts, max_ts)";

/// The transactional copy of the store's marker, and the keys later slices add.
const STORE_META_TABLE: &str = "CREATE TABLE IF NOT EXISTS store_meta (key TEXT NOT NULL PRIMARY KEY, value TEXT NOT NULL)";

/// The applied set of the registry.
const STORE_MIGRATIONS_TABLE: &str = "CREATE TABLE IF NOT EXISTS store_migrations (id TEXT NOT NULL PRIMARY KEY, layout INTEGER NOT NULL, applied_ms INTEGER NOT NULL)";

/// `metadata.db`'s rollup table as layout 1 created it, `unsorted_segments` included.
const ASPECT_METADATA_TABLE: &str = "CREATE TABLE IF NOT EXISTS aspect_metadata (
	aspect TEXT NOT NULL,
	segment_count INTEGER NOT NULL,
	total_rows INTEGER NOT NULL,
	total_nulls INTEGER NOT NULL,
	total_bytes INTEGER NOT NULL,
	unsorted_segments INTEGER NOT NULL DEFAULT 0,
	min_ts INTEGER,
	max_ts INTEGER,
	min_value TEXT,
	max_value TEXT,
	PRIMARY KEY (aspect)
)";

/// The order-health column a `metadata.db` created before it lacks.
const UNSORTED_SEGMENTS: (&str, &str, &str) = ("aspect_metadata", "unsorted_segments", "INTEGER NOT NULL DEFAULT 0");

/// `aspect_catalog.db`'s declarations.
const ASPECT_SCHEMA_TABLE: &str = "CREATE TABLE IF NOT EXISTS aspect_schema (
	database TEXT NOT NULL,
	subject TEXT NOT NULL,
	aspect TEXT NOT NULL,
	physical_type TEXT NOT NULL,
	value_tolerance TEXT NOT NULL,
	timestamp_unit TEXT NOT NULL,
	PRIMARY KEY (database, subject, aspect)
)";

/// `catalog.db`'s databases.
const DATABASES_TABLE: &str = "CREATE TABLE IF NOT EXISTS databases (
	name TEXT NOT NULL,
	PRIMARY KEY (name)
)";

/// `catalog.db`'s subjects.
const SUBJECTS_TABLE: &str = "CREATE TABLE IF NOT EXISTS subjects (
	database TEXT NOT NULL,
	subject TEXT NOT NULL,
	PRIMARY KEY (database, subject)
)";

/// The columns layout 2 adds to `segment_index`, in order, with their declarations.
/// `ALTER TABLE ADD COLUMN` needs a constant default for a NOT NULL column: 0 is the
/// legacy generation, and series 0 is the empty tag set (tags A7).
const V2_INDEX_COLUMNS: [(&str, &str); 5] = [("gen", "INTEGER NOT NULL DEFAULT 0"), ("prec", "INTEGER"), ("frame_crc", "INTEGER"), ("commit_epoch", "INTEGER"), ("series_id", "INTEGER NOT NULL DEFAULT 0")];

/// The tables layout 2 adds to `segment_index.db` (crash-consistency design section 4).
/// Each is filled by a later slice; they exist from the first layout-2 open so that no
/// later open has to run DDL on a live store.
const V2_INDEX_TABLES: [&str; 6] = [
	// One row per aspect, so commits on different aspects never touch the same row.
	// Series ids start at 1: series 0 is the empty tag set, implicit (tags A7).
	"CREATE TABLE IF NOT EXISTS aspect_seq (aspect TEXT NOT NULL PRIMARY KEY, next_id INTEGER NOT NULL, next_gen INTEGER NOT NULL, epoch INTEGER NOT NULL DEFAULT 0, synced_epoch INTEGER NOT NULL DEFAULT 0, next_series_id INTEGER NOT NULL DEFAULT 1)",
	// state is 'pending' (an output not yet swapped in) or 'retired' (awaiting the reaper).
	"CREATE TABLE IF NOT EXISTS frame_journal (name TEXT NOT NULL PRIMARY KEY, aspect TEXT NOT NULL, state TEXT NOT NULL, retire_epoch INTEGER, created_ms INTEGER NOT NULL)",
	"CREATE TABLE IF NOT EXISTS ingest_ledger (aspect TEXT NOT NULL, key TEXT NOT NULL, fingerprint INTEGER NOT NULL, row_count INTEGER NOT NULL, min_ts INTEGER, max_ts INTEGER, id_lo INTEGER, id_hi INTEGER, receipt_json TEXT NOT NULL, commit_epoch INTEGER NOT NULL, created_ms INTEGER NOT NULL, PRIMARY KEY (aspect, key))",
	"CREATE TABLE IF NOT EXISTS segment_quarantine (aspect TEXT NOT NULL, id INTEGER NOT NULL, gen INTEGER NOT NULL, name TEXT NOT NULL, reason TEXT NOT NULL, descriptor_json TEXT, quarantined_ms INTEGER NOT NULL, PRIMARY KEY (aspect, id, gen))",
	// The rollup, with metadata.db's columns, folded inside the seal transaction from S11.
	"CREATE TABLE IF NOT EXISTS aspect_metadata (aspect TEXT NOT NULL, segment_count INTEGER NOT NULL, total_rows INTEGER NOT NULL, total_nulls INTEGER NOT NULL, total_bytes INTEGER NOT NULL, unsorted_segments INTEGER NOT NULL DEFAULT 0, min_ts INTEGER, max_ts INTEGER, min_value TEXT, max_value TEXT, PRIMARY KEY (aspect))",
	"CREATE TABLE IF NOT EXISTS segment_changes (aspect TEXT NOT NULL, epoch INTEGER NOT NULL, min_ts INTEGER, max_ts INTEGER, row_count INTEGER NOT NULL, PRIMARY KEY (aspect, epoch))",
];

/// The column an `aspect_seq` created by S6 or S7, before tags A7, lacks.
const NEXT_SERIES_ID: (&str, &str, &str) = ("aspect_seq", "next_series_id", "INTEGER NOT NULL DEFAULT 1");

/// `0001_baseline`: every table of a pre-1.0 store, plus the registry's own tables.
fn baseline<'a>(cp: &'a ControlPlane<'a>) -> BoxFuture<'a, Result<()>> {
	Box::pin(async move {
		if let Some(index) = cp.index {
			let conn = connect(index.db).await.with_context(|| format!("{}: connecting", index.name))?;
			ddl(&conn, index.name, SEGMENT_INDEX_TABLE).await?;
			if let Err(e) = conn.execute(SEGMENT_INDEX_TIME_INDEX, ()).await {
				tracing::warn!(database = index.name, error = %e, "could not create the segment_index time index; time prunes scan the aspect's rows instead");
			}
			ddl(&conn, index.name, STORE_META_TABLE).await?;
			ddl(&conn, index.name, STORE_MIGRATIONS_TABLE).await?;
		}
		if let Some(metadata) = cp.metadata {
			let conn = connect(metadata.db).await.with_context(|| format!("{}: connecting", metadata.name))?;
			ddl(&conn, metadata.name, ASPECT_METADATA_TABLE).await?;
			let (table, column, declaration) = UNSORTED_SEGMENTS;
			add_column(&conn, metadata.name, table, column, declaration).await?;
		}
		if let Some(catalog) = cp.aspect_catalog {
			let conn = connect(catalog.db).await.with_context(|| format!("{}: connecting", catalog.name))?;
			ddl(&conn, catalog.name, ASPECT_SCHEMA_TABLE).await?;
		}
		if let Some(registry) = cp.catalog {
			let conn = connect(registry.db).await.with_context(|| format!("{}: connecting", registry.name))?;
			ddl(&conn, registry.name, DATABASES_TABLE).await?;
			ddl(&conn, registry.name, SUBJECTS_TABLE).await?;
		}
		Ok(())
	})
}

/// `0002_s6_s7`: layout 2's columns and tables in `segment_index.db`.
fn s6_s7<'a>(cp: &'a ControlPlane<'a>) -> BoxFuture<'a, Result<()>> {
	Box::pin(async move {
		let Some(index) = cp.index else { return Ok(()) };
		let conn = connect(index.db).await.with_context(|| format!("{}: connecting", index.name))?;
		for (column, declaration) in V2_INDEX_COLUMNS {
			add_column(&conn, index.name, "segment_index", column, declaration).await?;
		}
		for table in V2_INDEX_TABLES {
			ddl(&conn, index.name, table).await?;
		}
		let (table, column, declaration) = NEXT_SERIES_ID;
		add_column(&conn, index.name, table, column, declaration).await
	})
}

/// `0002_s6_s7`'s precheck: refuse a store whose index records a legacy frame outside
/// `segments/` (see [`legacy_path_is_contained`]), naming its aspects. The frames are
/// left where they are; nothing is quarantined.
fn refuse_unsafe_legacy_paths<'a>(cp: &'a ControlPlane<'a>) -> BoxFuture<'a, Result<()>> {
	Box::pin(async move {
		let Some(index) = cp.index else { return Ok(()) };
		let conn = connect(index.db).await.with_context(|| format!("{}: connecting", index.name))?;
		// Before layout 2 every row is a legacy (generation 0) row; once `gen` exists,
		// only those are named after their aspect and id.
		let legacy_only = column_names(&conn, "segment_index").await.with_context(|| format!("{}: listing the segment_index columns", index.name))?.iter().any(|name| name == "gen");
		let sql = if legacy_only { "SELECT aspect, id, path FROM segment_index WHERE gen = 0" } else { "SELECT aspect, id, path FROM segment_index" };
		let mut rows = conn.query(sql, ()).await.with_context(|| format!("{}: reading the segment index", index.name))?;
		let mut unsafe_aspects = BTreeSet::new();
		while let Some(row) = rows.next().await? {
			let (Value::Text(aspect), Value::Integer(id), Value::Text(path)) = (row.get_value(0)?, row.get_value(1)?, row.get_value(2)?) else { continue };
			let contained = u64::try_from(id).is_ok_and(|id| legacy_path_is_contained(&aspect, id, &path));
			if !contained {
				unsafe_aspects.insert(aspect);
			}
		}
		if unsafe_aspects.is_empty() {
			return Ok(());
		}
		Err(StoreError::UnsafeLegacyPath { aspects: unsafe_aspects.into_iter().collect() }.into())
	})
}

/// Whether the path a layout-1 store recorded for `aspect`'s segment `id` names the frame
/// directly inside a `segments/` directory: its last two components, split on both `/`
/// and `\` (a store written on Windows records `\`), are `segments` and
/// `{aspect}-{id}.weftseg`.
///
/// Anything else escaped `segments/`: the aspect name held a separator or a `..`, which
/// the pre-1.0 store joined into the path unchecked. Such a frame cannot be resolved
/// against a moved root, and reading it as recorded could read outside the store.
pub(crate) fn legacy_path_is_contained(aspect: &str, id: u64, stored: &str) -> bool {
	let parts: Vec<&str> = stored.split(['/', '\\']).filter(|part| !part.is_empty() && *part != ".").collect();
	match parts.as_slice() {
		[.., parent, name] => *parent == "segments" && *name == format!("{aspect}-{id}.weftseg"),
		_ => false,
	}
}

/// Run one DDL statement outside any transaction; `name` names the database in errors.
async fn ddl(conn: &turso::Connection, name: &str, sql: &str) -> Result<()> {
	let first_line = sql.lines().next().unwrap_or(sql);
	conn.execute(sql, ()).await.with_context(|| format!("{name}: {first_line}"))?;
	Ok(())
}

/// Add `column` to `table` unless `PRAGMA table_info` lists it already. A duplicate-column
/// error (another opener added it meanwhile) is matched exactly and ignored; any other
/// error fails.
async fn add_column(conn: &turso::Connection, name: &str, table: &str, column: &str, declaration: &str) -> Result<()> {
	let present = column_names(conn, table).await.with_context(|| format!("{name}: listing the {table} columns"))?;
	if present.iter().any(|existing| existing == column) {
		return Ok(());
	}
	match conn.execute(format!("ALTER TABLE {table} ADD COLUMN {column} {declaration}"), ()).await {
		Ok(_) => Ok(()),
		Err(e) if is_duplicate_column(&e, column) => Ok(()),
		Err(e) => Err(e).with_context(|| format!("{name}: adding column {table}.{column}")),
	}
}

/// Whether `e` is exactly Turso's refusal to add `column` because the table has it
/// already, and not some other error that happens to mention a duplicate.
pub(crate) fn is_duplicate_column(e: &turso::Error, column: &str) -> bool {
	let expected = format!("duplicate column name: {column}");
	match e {
		turso::Error::Error(message) => message == &expected || message.strip_prefix("Parse error: ") == Some(expected.as_str()),
		_ => false,
	}
}

/// Whether the database behind `conn` has a table named `table`.
pub(crate) async fn table_exists(conn: &turso::Connection, table: &str) -> Result<bool> {
	let mut rows = conn.query("SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?", [Value::Text(table.to_string())]).await?;
	Ok(rows.next().await?.is_some())
}

/// The column names of `table`, from `PRAGMA table_info`.
pub(crate) async fn column_names(conn: &turso::Connection, table: &str) -> Result<Vec<String>> {
	let mut rows = conn.query(format!("PRAGMA table_info({table})"), ()).await?;
	let mut names = Vec::new();
	while let Some(row) = rows.next().await? {
		if let Value::Text(name) = row.get_value(1)? {
			names.push(name);
		}
	}
	Ok(names)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_registry_is_in_ordinal_order_with_unique_ids_and_sane_floors() {
		let ordinals: Vec<u64> = REGISTRY.iter().map(|migration| ordinal(migration.id)).collect();
		assert!(ordinals.windows(2).all(|pair| pair[0] < pair[1]), "{ordinals:?}");
		for migration in REGISTRY {
			assert!(migration.min_read_after <= migration.layout && migration.min_write_after <= migration.layout, "{}: a floor above its own layout", migration.id);
			assert!(migration.layout <= crate::SUPPORTED_LAYOUT, "{}: a layout this build does not support", migration.id);
			if migration.min_write_after < migration.layout {
				assert!(migration.older_writers_safe && migration.rederive.is_some(), "{}: leaving older writers in needs a declaration and a re-derive hook", migration.id);
			}
		}
		assert_eq!(floors(REGISTRY.iter()).map(|f| f.layout), Some(crate::SUPPORTED_LAYOUT), "the registry reaches this build's layout");
	}

	#[test]
	fn pending_and_after_select_by_applied_set_and_by_ordinal() {
		let applied: BTreeSet<String> = ["0001_baseline".to_string()].into();
		assert_eq!(pending(REGISTRY, &applied).iter().map(|m| m.id).collect::<Vec<_>>(), vec!["0002_s6_s7"]);
		assert_eq!(pending(REGISTRY, &BTreeSet::new()).len(), REGISTRY.len());
		assert_eq!(after(REGISTRY, None).len(), REGISTRY.len());
		assert_eq!(after(REGISTRY, Some("0001_baseline")).iter().map(|m| m.id).collect::<Vec<_>>(), vec!["0002_s6_s7"]);
		assert!(after(REGISTRY, Some("0002_s6_s7")).is_empty());
		assert!(after(REGISTRY, Some("0009_future")).is_empty(), "a newer marker's ordinal is past everything here");
		assert_eq!(floors(pending(REGISTRY, &applied)), Some(Floors { layout: 2, min_read: 2, min_write: 2 }));
		assert_eq!(floors(Vec::<&Migration>::new()), None);
	}

	#[test]
	fn a_legacy_path_is_contained_only_directly_inside_segments_under_its_own_name() {
		let contained = [("price", 0, "/var/tmp/weft-pre-v2-fixture/root/segments/price-0.weftseg"), ("price", 12, "C:\\data\\store\\segments\\price-12.weftseg"), ("Room-A.temp", 3, "relative/segments/Room-A.temp-3.weftseg"), ("price", 0, "/root/./segments/./price-0.weftseg"), ("price", 0, "/data/segments/segments/price-0.weftseg"), ("job:rate", 1, "/r/segments/job:rate-1.weftseg")];
		for (aspect, id, path) in contained {
			assert!(legacy_path_is_contained(aspect, id, path), "{aspect} {id} {path}");
		}
		let escaped = [("../evil", 0, "/root/segments/../evil-0.weftseg"), ("a/segments/b", 0, "/root/segments/a/segments/b-0.weftseg"), ("x\\y", 0, "/root/segments/x\\y-0.weftseg"), ("price", 0, "/root/frames/price-0.weftseg"), ("price", 1, "/root/segments/price-0.weftseg"), ("price", 0, "price-0.weftseg"), ("price", 0, "")];
		for (aspect, id, path) in escaped {
			assert!(!legacy_path_is_contained(aspect, id, path), "{aspect} {id} {path}");
		}
	}

	/// Turso's refusal to add an existing column is matched exactly; another error, or the
	/// same refusal for another column, is not.
	#[tokio::test]
	async fn a_duplicate_column_is_recognised_exactly() {
		let db = turso::Builder::new_local(":memory:").build().await.unwrap();
		let conn = db.connect().unwrap();
		conn.execute("CREATE TABLE t (a INTEGER)", ()).await.unwrap();
		let err = conn.execute("ALTER TABLE t ADD COLUMN a INTEGER", ()).await.expect_err("a duplicate column");
		assert!(is_duplicate_column(&err, "a"), "{err:?}");
		assert!(!is_duplicate_column(&err, "b"), "{err:?}");
		let other = conn.execute("ALTER TABLE missing ADD COLUMN a INTEGER", ()).await.expect_err("no such table");
		assert!(!is_duplicate_column(&other, "a"), "{other:?}");
		// add_column skips a present column and adds a missing one.
		add_column(&conn, "t.db", "t", "a", "INTEGER").await.unwrap();
		add_column(&conn, "t.db", "t", "b", "INTEGER NOT NULL DEFAULT 0").await.unwrap();
		assert_eq!(column_names(&conn, "t").await.unwrap(), vec!["a", "b"]);
		let failed = add_column(&conn, "t.db", "missing", "a", "INTEGER").await.expect_err("any other error fails");
		assert!(format!("{failed:#}").contains("t.db: adding column missing.a"), "{failed:#}");
	}
}
