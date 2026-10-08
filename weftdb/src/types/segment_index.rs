//! libSQL-backed `segment_index` control plane (roadmap **Phase 4.3**).
//!
//! Phase 4.3 sealed measurement data into typed columnar `.weftseg` segments and
//! gave each a [`SegmentDescriptor`](weft_physical_type::SegmentDescriptor) — the
//! resident index row (min/max ts/value, row/null counts, byte length, path) that
//! lets a query prune segments without opening them. This module makes that index
//! **durable**: it persists descriptors into a libSQL `segment_index` table and
//! answers a time-range query with a SQL `WHERE` over the integer `min_ts`/`max_ts`
//! columns, returning only the descriptors a query must open.
//!
//! This is squarely a **control-plane** component (hard constraint #3): libSQL owns
//! catalog/metadata; the measurement hot path stays in the `.weftseg` segments. The
//! store holds *metadata about* segments — never the measurements themselves. The
//! `BigDecimal` value bounds round-trip through their plain-text form (hard
//! constraint #4 — no silent float downcast, even in the catalog), and the
//! `PhysicalType`/`TimeUnit` metadata round-trip as JSON so a `ScaledI64 { scale }`
//! reconstructs faithfully.
//!
//! A single `segment_index.db` can index many aspects: every row is scoped by an
//! `aspect` key, so [`SegmentIndexStore::prune_by_time`] and the accessors all take
//! the aspect they operate on. Every write commits through one `IndexTxn` (a
//! `BEGIN CONCURRENT` transaction that classifies its failures).
//!
//! **Schema.** The open runs no DDL of its own: the tables are the migration registry's
//! (`migrations`), which a [`SegmentStore`](crate::SegmentStore) runs once all four of its
//! databases are open and its `STORE_FORMAT` gate has passed. Layout 2
//! (docs/design/crash-consistency.md section 4) adds, additively and idempotently,
//! `gen`, `prec`, `frame_crc`, `commit_epoch` and `series_id` to `segment_index` and the
//! tables the write-once protocols commit into (`aspect_seq`, `frame_journal`,
//! `ingest_ledger`, `segment_quarantine`, `aspect_metadata`, `segment_changes`). No row is
//! rewritten: a legacy row reads back as generation 0, series 0, with the other three
//! unset. `store_meta` holds the transactional copy of the store's marker.

use std::str::FromStr;

use anyhow::{bail, Context, Result};
use bigdecimal::BigDecimal;
use turso::{Builder, Value};
use weft_physical_type::{SegmentDescriptor, SegmentIndex};

use crate::{
	types::{
		durable::control_plane::connect, index_txn::{IndexOp, IndexRow, IndexTxn, IndexTxnError, TxnApplied}, migrations
	}, StoreError, SUPPORTED_LAYOUT
};

/// Every `segment_index` column a read decodes, in the order
/// [`SegmentIndexStore::decode_row`] reads them.
const SELECT_COLUMNS: &str = "id, path, format_version, physical_type, time_unit, row_count, null_count, time_sorted, min_ts, max_ts, min_value, max_value, byte_len, gen, prec, frame_crc, commit_epoch, series_id";

/// A durable, libSQL-backed index of sealed segments, scoped by aspect.
///
/// Open one with [`SegmentIndexStore::open`] (a file path) or
/// [`SegmentIndexStore::open_in_memory`] (tests); record a freshly sealed segment
/// with [`insert`](SegmentIndexStore::insert); prune a query with
/// [`prune_by_time`](SegmentIndexStore::prune_by_time).
pub struct SegmentIndexStore {
	db: turso::Database,
}

impl SegmentIndexStore {
	/// The tables every `segment_index.db` holds once opened. A snapshot missing any of them is
	/// not a backup of this database (an empty file passes every other check), so
	/// backup and restore verification require them.
	pub const TABLES: &'static [&'static str] = &["segment_index"];

	/// Open (creating if absent) the `segment_index.db` at `path` on its own, outside a
	/// store: enable MVCC, prove it took, and bring the schema up to this build's layout
	/// with the migration registry's DDL (recording nothing, since a database on its own
	/// has no `STORE_FORMAT`).
	///
	/// A [`SegmentStore`](crate::SegmentStore) does not use this: it opens the database
	/// without DDL and runs the registry itself, after its marker gate.
	///
	/// # Errors
	///
	/// Fails if the database does not end up in MVCC journal mode or a new connection
	/// does not sync FULL, or if its `store_meta` records a write floor newer than
	/// [`SUPPORTED_LAYOUT`] ([`StoreError::IncompatibleLayout`]), and propagates any libSQL
	/// connection or DDL failure.
	pub async fn open(path: &str) -> Result<Self> {
		let store = Self::open_unmigrated(path).await?;
		migrations::apply_standalone(&migrations::ControlPlane { index: Some(migrations::Db { db: &store.db, name: path }), ..migrations::ControlPlane::default() }).await?;
		Ok(store)
	}

	/// Open the `segment_index.db` at `path` without running any DDL: refuse a newer write
	/// floor, enable MVCC, prove it and sync its header, nothing else. What a
	/// [`SegmentStore`](crate::SegmentStore) opens before it runs the migrations.
	///
	/// # Errors
	///
	/// As [`open`](Self::open), less the DDL.
	pub(crate) async fn open_unmigrated(path: &str) -> Result<Self> {
		Self::open_with(Builder::new_local(path), path).await
	}

	/// [`open`](Self::open) on Turso I/O backend `io`, for the tests that record what the
	/// open does to the files.
	#[cfg(test)]
	pub(crate) async fn open_with_io(path: &str, io: std::sync::Arc<dyn turso::core::IO>) -> Result<Self> {
		let store = Self::open_with(Builder::new_local(path).with_io_impl(io), path).await?;
		migrations::apply_standalone(&migrations::ControlPlane { index: Some(migrations::Db { db: &store.db, name: path }), ..migrations::ControlPlane::default() }).await?;
		Ok(store)
	}

	/// Build the database `builder` describes and configure it as
	/// [`open_unmigrated`](Self::open_unmigrated) does.
	async fn open_with(builder: Builder, path: &str) -> Result<Self> {
		let db = builder.build().await?;
		Self::configure(&db, path).await?;
		Ok(Self { db })
	}

	/// Open an ephemeral in-memory store — the path `:memory:` — for tests.
	///
	/// # Errors
	///
	/// Propagates any libSQL connection or DDL failure.
	pub async fn open_in_memory() -> Result<Self> {
		Self::open(":memory:").await
	}

	/// Snapshot this `segment_index.db` to `dest` (a fresh file) via Turso's
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
		let conn = connect(&self.db).await?;
		crate::types::backup::snapshot_with_verify(&conn, dest, mode, Self::TABLES).await
	}

	/// Refuse a newer write floor, enable MVCC, prove it took and that commits sync FULL.
	/// `path` names the database in errors. No DDL: the schema is the registry's.
	async fn configure(db: &turso::Database, path: &str) -> Result<()> {
		let conn = connect(db).await?;
		// Before anything writes to the database (the switch to MVCC, its header sync): a
		// store whose floor is newer than this build is refused untouched. Turso replayed
		// the MVCC log when it built the database, so `store_meta` reads the same before
		// the switch. The store's `STORE_FORMAT` gate has normally refused it already; this
		// holds for a store that lost its marker, and for a database opened on its own.
		refuse_newer_floor(&conn, path).await?;
		// The control plane's MVCC write path; open fails rather than run without it.
		crate::types::durable::control_plane::enable_mvcc_full(db, &conn, path).await?;
		conn.execute("PRAGMA busy_timeout=600000", turso::params![]).await.ok();
		Ok(())
	}

	/// Record a sealed segment's descriptor under `aspect`, returning the number of
	/// control-plane rows the write actually affected.
	///
	/// Replaces any existing row with the same `(aspect, id)` (a re-seal of the same
	/// segment id overwrites), so the call is idempotent on segment identity.
	///
	/// The returned count is libSQL's `Statement::n_change()` (roadmap Phase 3 /
	/// Turso-0.6 adoption: affected-row accounting for idempotent batch ingest +
	/// instrumentation spans). It is the *write's own* report of what it changed, so a
	/// caller can distinguish a real catalog mutation from a no-op — the accounting an
	/// idempotency ledger needs and a seal span should carry. It is also emitted as a
	/// `rows_changed` field on a `control_plane.index.insert` debug span.
	///
	/// The row is a legacy one (generation 0, nothing bound). A
	/// [`SegmentStore`](crate::SegmentStore) writes through its own poison-aware commit
	/// path instead of this method, so that an ambiguous COMMIT stops its writes.
	///
	/// **This method knows nothing of a store's write poison.** Called on the index of a
	/// [`SegmentStore`](crate::SegmentStore) (through
	/// [`SegmentStore::index`](crate::SegmentStore::index)), it writes even while that
	/// store is poisoned, and an ambiguous COMMIT here does not poison the store. Write
	/// through the store's own entry points instead.
	///
	/// # Errors
	///
	/// Propagates any libSQL write failure, or a metadata-serialization failure.
	pub async fn insert(&self, aspect: &str, descriptor: &SegmentDescriptor) -> Result<u64> {
		let span = tracing::debug_span!("control_plane.index.insert", aspect, id = descriptor.id, rows_changed = tracing::field::Empty);
		let _guard = span.enter();
		let txn = IndexTxn::new(vec![IndexOp::Upsert { aspect: aspect.to_string(), row: IndexRow::legacy(descriptor.clone()) }]);
		let applied = self.apply(&txn).await.map_err(|e| anyhow::anyhow!("segment_index insert failed: {e}"))?;
		let changed = applied.changes.first().copied().unwrap_or(0);
		span.record("rows_changed", changed);
		Ok(changed)
	}

	/// Commit `txn` against this index: all of its ops or none of them.
	///
	/// # Errors
	///
	/// The transaction's [`IndexTxnError`], whose kind says whether it may have
	/// committed anyway.
	pub(crate) async fn apply(&self, txn: &IndexTxn) -> std::result::Result<TxnApplied, IndexTxnError> {
		txn.run(&self.db).await
	}

	/// [`apply`](Self::apply), asking `may_retry` before each retry
	/// ([`IndexTxn::run_while`]).
	///
	/// # Errors
	///
	/// As [`apply`](Self::apply).
	pub(crate) async fn apply_while(&self, txn: &IndexTxn, may_retry: impl Fn() -> bool) -> std::result::Result<TxnApplied, IndexTxnError> {
		txn.run_while(&self.db, may_retry).await
	}

	/// The database behind this index: what the migration registry runs against, and what
	/// tests race a transaction of their own on.
	pub(crate) const fn database(&self) -> &turso::Database {
		&self.db
	}

	/// Remove the descriptor for segment `id` under `aspect` from the index, returning
	/// `true` when a row was deleted and `false` when none matched.
	///
	/// The control-plane half of dropping a segment (roadmap Phase 4.6 cross-segment
	/// merge): the caller removes the `.weftseg` file; this removes its catalog row so a
	/// pruned read never opens the now-absent file. A [`SegmentStore`](crate::SegmentStore)
	/// hands ids out from a persisted per-aspect allocator, so it never reissues the id of
	/// a deleted row, not even the largest one.
	///
	/// Like [`insert`](Self::insert), this knows nothing of a
	/// [`SegmentStore`](crate::SegmentStore)'s write poison: it deletes even while the
	/// store is poisoned, and an ambiguous COMMIT here does not poison the store.
	///
	/// # Errors
	///
	/// Propagates any libSQL write failure.
	pub async fn delete(&self, aspect: &str, id: u64) -> Result<bool> {
		let span = tracing::debug_span!("control_plane.index.delete", aspect, id, rows_changed = tracing::field::Empty);
		let _guard = span.enter();
		let txn = IndexTxn::new(vec![IndexOp::Delete { aspect: aspect.to_string(), id }]);
		let applied = self.apply(&txn).await.map_err(|e| anyhow::anyhow!("segment_index delete failed: {e}"))?;
		let changed = applied.changes.first().copied().unwrap_or(0);
		span.record("rows_changed", changed);
		Ok(changed > 0)
	}

	/// **Data skipping at the control plane** (roadmap Phase 4.4): the descriptors
	/// for `aspect` whose segments may hold a row in the inclusive time range
	/// `[start, end]` — the ones a query must open. The pruning runs as a SQL
	/// `WHERE min_ts <= end AND start <= max_ts` over the indexed integer columns, so
	/// disjoint segments are skipped without their rows ever leaving libSQL. Ordered
	/// by segment `id` (seal order).
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure, or a row that cannot be decoded back into
	/// a [`SegmentDescriptor`].
	pub async fn prune_by_time(&self, aspect: &str, start: i64, end: i64) -> Result<Vec<SegmentDescriptor>> {
		Ok(self.prune_rows_by_time(aspect, start, end).await?.into_iter().map(|row| row.desc).collect())
	}

	/// [`prune_by_time`](Self::prune_by_time), returning whole [`IndexRow`]s.
	pub(crate) async fn prune_rows_by_time(&self, aspect: &str, start: i64, end: i64) -> Result<Vec<IndexRow>> {
		let conn = connect(&self.db).await?;
		// A NULL-spanned (empty) segment can never overlap, and the inequalities reject it.
		let rows = conn
			.query(
				format!("SELECT {SELECT_COLUMNS}
					FROM segment_index
					WHERE aspect = ? AND min_ts IS NOT NULL AND max_ts IS NOT NULL AND min_ts <= ? AND ? <= max_ts
					ORDER BY id"),
				turso::params![aspect.to_string(), end, start],
			)
			.await?;
		Self::collect(rows).await
	}

	/// Every descriptor recorded under `aspect`, in seal (`id`) order.
	///
	/// Use this to rebuild the resident [`SegmentIndex`] for finer in-memory pruning
	/// (value/quality), via [`load_index`](SegmentIndexStore::load_index).
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure, or an undecodable row.
	pub async fn all(&self, aspect: &str) -> Result<Vec<SegmentDescriptor>> {
		Ok(self.rows(aspect).await?.into_iter().map(|row| row.desc).collect())
	}

	/// [`all`](Self::all), returning whole [`IndexRow`]s.
	pub(crate) async fn rows(&self, aspect: &str) -> Result<Vec<IndexRow>> {
		let conn = connect(&self.db).await?;
		let rows = conn.query(format!("SELECT {SELECT_COLUMNS} FROM segment_index WHERE aspect = ? ORDER BY id"), turso::params![aspect.to_string()]).await?;
		Self::collect(rows).await
	}

	/// Load every descriptor for `aspect` into a resident
	/// [`SegmentIndex`](weft_physical_type::SegmentIndex) — the in-memory model whose
	/// value/quality pruning complements this store's SQL time pruning.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure, or an undecodable row.
	pub async fn load_index(&self, aspect: &str) -> Result<SegmentIndex> {
		let mut index = SegmentIndex::new();
		for descriptor in self.all(aspect).await? {
			index.push(descriptor);
		}
		Ok(index)
	}

	/// Every aspect with at least one indexed segment, in name order — the
	/// authoritative list of what the index actually holds (e.g. for rebuilding the
	/// per-aspect `metadata.db` rollups from the durable index).
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn list_aspects(&self) -> Result<Vec<String>> {
		let conn = connect(&self.db).await?;
		let mut rows = conn.query("SELECT DISTINCT aspect FROM segment_index ORDER BY aspect", turso::params![]).await?;
		let mut out = Vec::new();
		while let Some(row) = rows.next().await? {
			if let Value::Text(s) = row.get_value(0)? {
				out.push(s);
			}
		}
		Ok(out)
	}

	/// Number of segments indexed under `aspect`.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn count(&self, aspect: &str) -> Result<usize> {
		let conn = connect(&self.db).await?;
		let mut rows = conn.query("SELECT COUNT(*) FROM segment_index WHERE aspect = ?", turso::params![aspect.to_string()]).await?;
		let row = rows.next().await?.ok_or_else(|| anyhow::anyhow!("COUNT returned no row"))?;
		let n = *row.get_value(0)?.as_integer().unwrap_or(&0);
		Ok(usize::try_from(n).unwrap_or(0))
	}

	/// One past the largest segment `id` indexed under `aspect`, or `0` when the aspect
	/// has no segments.
	///
	/// This is not an id to seal under. It hands out again the id of a deleted segment
	/// that had the largest one, and of a frame a seal wrote but crashed before indexing,
	/// and two callers that read it before either commits get the same id. A
	/// [`SegmentStore`](crate::SegmentStore) takes its ids from a persisted per-aspect
	/// allocator instead (crash-consistency design, S7). Kept for tests.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	#[deprecated(note = "MAX(id) + 1 reissues deleted and crashed ids and races concurrent callers; SegmentStore allocates ids from its persisted per-aspect allocator. Kept for tests only.")]
	pub async fn next_id(&self, aspect: &str) -> Result<u64> {
		Ok(self.max_id(aspect).await?.map_or(0, |max| max.saturating_add(1)))
	}

	/// The largest segment `id` indexed under `aspect`, if it has any.
	async fn max_id(&self, aspect: &str) -> Result<Option<u64>> {
		let conn = connect(&self.db).await?;
		let mut rows = conn.query("SELECT MAX(id) FROM segment_index WHERE aspect = ?", turso::params![aspect.to_string()]).await?;
		let row = rows.next().await?.ok_or_else(|| anyhow::anyhow!("MAX returned no row"))?;
		// MAX over no rows is SQL NULL.
		match row.get_value(0)? {
			Value::Integer(max) => Ok(Some(u64::try_from(max).unwrap_or(0))),
			_ => Ok(None),
		}
	}

	/// What `aspect`'s id allocator is seeded from in the index: its persisted
	/// `aspect_seq` row, if it has one, and the largest id among its rows, if it has any.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub(crate) async fn allocator_seed(&self, aspect: &str) -> Result<AllocatorSeed> {
		let conn = connect(&self.db).await?;
		let mut rows = conn.query("SELECT next_id, epoch FROM aspect_seq WHERE aspect = ?", turso::params![aspect.to_string()]).await?;
		let persisted = match rows.next().await? {
			Some(row) => {
				let unsigned = |idx: usize| Self::opt_integer(&row, idx).and_then(|n| u64::try_from(n).ok()).unwrap_or(0);
				Some((unsigned(0), unsigned(1)))
			}
			None => None,
		};
		drop(rows);
		Ok(AllocatorSeed { next_id: persisted.map(|(next_id, _)| next_id), epoch: persisted.map_or(0, |(_, epoch)| epoch), max_id: self.max_id(aspect).await? })
	}

	/// Decode a result set ([`SELECT_COLUMNS`], in that order) into [`IndexRow`]s.
	async fn collect(mut rows: turso::Rows) -> Result<Vec<IndexRow>> {
		let mut out = Vec::new();
		while let Some(row) = rows.next().await? {
			out.push(Self::decode_row(&row)?);
		}
		Ok(out)
	}

	/// Decode one row ([`SELECT_COLUMNS`], in that order) back into an [`IndexRow`].
	fn decode_row(row: &turso::Row) -> Result<IndexRow> {
		let id = u64::try_from(*row.get_value(0)?.as_integer().unwrap_or(&0)).unwrap_or(0);
		let path = row.get_value(1)?.as_text().cloned().unwrap_or_default();
		let format_version = u16::try_from(*row.get_value(2)?.as_integer().unwrap_or(&0)).unwrap_or(0);
		let physical_type = match row.get_value(3)? {
			Value::Text(s) => Some(serde_json::from_str(&s)?),
			_ => None,
		};
		let time_unit = match row.get_value(4)? {
			Value::Text(s) => Some(serde_json::from_str(&s)?),
			_ => None,
		};
		let row_count = usize::try_from(*row.get_value(5)?.as_integer().unwrap_or(&0)).unwrap_or(0);
		let null_count = usize::try_from(*row.get_value(6)?.as_integer().unwrap_or(&0)).unwrap_or(0);
		let time_sorted = *row.get_value(7)?.as_integer().unwrap_or(&1) != 0;
		let min_ts = Self::opt_integer(row, 8);
		let max_ts = Self::opt_integer(row, 9);
		let min_value = Self::opt_decimal(row, 10)?;
		let max_value = Self::opt_decimal(row, 11)?;
		let byte_len = u64::try_from(*row.get_value(12)?.as_integer().unwrap_or(&0)).unwrap_or(0);
		let desc = SegmentDescriptor { id, path, format_version, physical_type, time_unit, row_count, null_count, time_sorted, min_ts, max_ts, min_value, max_value, byte_len };
		let gen = u64::try_from(*row.get_value(13)?.as_integer().unwrap_or(&0)).unwrap_or(0);
		let unsigned = |idx: usize| Self::opt_integer(row, idx).and_then(|n| u64::try_from(n).ok());
		let frame_crc = Self::opt_integer(row, 15).and_then(|n| u32::try_from(n).ok());
		Ok(IndexRow { desc, gen, prec: unsigned(14), frame_crc, commit_epoch: unsigned(16), series_id: unsigned(17).unwrap_or(0) })
	}

	/// Read a nullable integer column, distinguishing SQL `NULL` from `0`.
	fn opt_integer(row: &turso::Row, idx: usize) -> Option<i64> {
		match row.get_value(idx) {
			Ok(Value::Integer(n)) => Some(n),
			_ => None,
		}
	}

	/// Read a nullable `BigDecimal` column stored as its plain-text form.
	fn opt_decimal(row: &turso::Row, idx: usize) -> Result<Option<BigDecimal>> {
		match row.get_value(idx) {
			Ok(Value::Text(s)) => Ok(Some(BigDecimal::from_str(&s)?)),
			_ => Ok(None),
		}
	}

	/// The `store_meta` value under `key`, if one is recorded.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub(crate) async fn meta(&self, key: &str) -> Result<Option<String>> {
		let conn = connect(&self.db).await?;
		if !migrations::table_exists(&conn, "store_meta").await? {
			return Ok(None);
		}
		meta_value(&conn, key).await
	}
}

/// What an aspect's id allocator is seeded from in `segment_index.db`
/// ([`SegmentIndexStore::allocator_seed`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AllocatorSeed {
	/// `aspect_seq.next_id`, when the aspect has a row there.
	pub next_id: Option<u64>,
	/// `aspect_seq.epoch`, or 0 without a row.
	pub epoch: u64,
	/// The largest id among the aspect's `segment_index` rows, when it has any.
	pub max_id: Option<u64>,
}

/// Refuse the database behind `conn` when its `store_meta` records a write floor newer
/// than [`SUPPORTED_LAYOUT`]: `min_write_layout`, or, for a store written before floors
/// were recorded (layout 2's first development stores), `layout_version`. A store
/// without `store_meta` has no floor here.
///
/// The open reads it before it writes anything to the database (the switch to MVCC, the
/// header sync): running this build's statements first would write to a store the
/// refusal then says this build does not write to.
///
/// # Errors
///
/// A failed read, [`StoreError::IncompatibleLayout`] for a newer floor, or a recorded
/// floor that is not a number.
async fn refuse_newer_floor(conn: &turso::Connection, path: &str) -> Result<()> {
	if !migrations::table_exists(conn, "store_meta").await.with_context(|| format!("{path}: looking for store_meta"))? {
		return Ok(());
	}
	let recorded = match meta_value(conn, "min_write_layout").await.with_context(|| format!("{path}: reading the store's write floor"))? {
		Some(floor) => Some(floor),
		None => meta_value(conn, "layout_version").await.with_context(|| format!("{path}: reading the store layout"))?,
	};
	let Some(recorded) = recorded else { return Ok(()) };
	let min_write: u32 = recorded.trim().parse().with_context(|| format!("{path}: store_meta records the write floor {recorded:?}, which is not a layout number"))?;
	if min_write > SUPPORTED_LAYOUT {
		return Err(anyhow::Error::new(StoreError::IncompatibleLayout { min_write, supported: SUPPORTED_LAYOUT }).context(format!("{path}: refusing a store a newer WeftDB wrote")));
	}
	Ok(())
}

/// The `store_meta` value under `key`, if one is recorded.
async fn meta_value(conn: &turso::Connection, key: &str) -> Result<Option<String>> {
	let mut rows = conn.query("SELECT value FROM store_meta WHERE key = ?", [Value::Text(key.to_string())]).await?;
	match rows.next().await? {
		Some(row) => match row.get_value(0)? {
			Value::Text(value) => Ok(Some(value)),
			other => bail!("store_meta {key} holds {other:?}, not text"),
		},
		None => Ok(None),
	}
}

#[cfg(test)]
mod tests {
	use weft_physical_type::{timestamp::TimeUnit, Segment};

	use super::*;

	fn bd(s: &str) -> BigDecimal {
		BigDecimal::from_str(s).expect("parses")
	}

	/// A sealed single-block segment over `[base, base+90]`, values 0..10.
	fn sealed(base: i64) -> (Segment, u64) {
		let ts: Vec<i64> = (0..10).map(|i| base + i * 10).collect();
		let vs: Vec<BigDecimal> = (0..10).map(BigDecimal::from).collect();
		let seg = Segment::build(&ts, &vs, TimeUnit::Seconds, &bd("0")).expect("builds");
		let len = seg.write_to().len() as u64;
		(seg, len)
	}

	#[tokio::test]
	async fn insert_then_read_round_trips_a_descriptor() {
		let store = SegmentIndexStore::open_in_memory().await.expect("opens");
		let (seg, len) = sealed(100);
		let d = SegmentDescriptor::of_segment(1, "segments/1.weftseg", len, &seg);
		store.insert("temp", &d).await.expect("inserts");
		let all = store.all("temp").await.expect("reads");
		let count = store.count("temp").await.expect("counts");
		drop(store);
		assert_eq!(all.len(), 1);
		// The descriptor round-trips field-for-field through libSQL.
		assert_eq!(all[0], d);
		assert_eq!(count, 1);
	}

	/// `insert` and `delete` report libSQL's affected-row count (`Statement::n_change()`)
	/// so a seal can record what the catalog write actually changed rather than assuming
	/// it changed something. The distinguishing case is the miss: deleting an absent id
	/// affects zero rows, which is exactly the signal an idempotency ledger needs.
	#[tokio::test]
	async fn control_plane_writes_report_their_affected_row_counts() {
		let store = SegmentIndexStore::open_in_memory().await.expect("opens");
		let (seg, len) = sealed(100);
		let d = SegmentDescriptor::of_segment(1, "segments/1.weftseg", len, &seg);
		let inserted = store.insert("temp", &d).await.expect("inserts");
		// A re-seal of the same (aspect, id) REPLACEs — still one row affected.
		let reinserted = store.insert("temp", &d).await.expect("re-inserts");
		let count_after = store.count("temp").await.expect("counts");
		let deleted_missing = store.delete("temp", 999).await.expect("deletes a miss");
		let deleted_hit = store.delete("temp", 1).await.expect("deletes a hit");
		drop(store);
		assert_eq!(inserted, 1, "a fresh descriptor write affects one row");
		assert_eq!(reinserted, 1, "an idempotent re-seal replaces rather than appends");
		assert_eq!(count_after, 1, "the re-seal did not add a second row");
		assert!(!deleted_missing, "deleting an absent id affects no rows");
		assert!(deleted_hit);
	}

	#[tokio::test]
	async fn prune_by_time_skips_disjoint_segments_in_sql() {
		let store = SegmentIndexStore::open_in_memory().await.expect("opens");
		for (i, base) in [0_i64, 100, 200].into_iter().enumerate() {
			let (seg, len) = sealed(base);
			let d = SegmentDescriptor::of_segment(i as u64, format!("s{i}.weftseg"), len, &seg);
			store.insert("aspect", &d).await.expect("inserts");
		}
		let ids = |ds: &[SegmentDescriptor]| ds.iter().map(|d| d.id).collect::<Vec<_>>();
		// Each sealed() spans [base, base+90].
		let mid = store.prune_by_time("aspect", 120, 150).await.expect("prunes");
		let straddle = store.prune_by_time("aspect", 50, 150).await.expect("prunes");
		let all = store.prune_by_time("aspect", 0, 290).await.expect("prunes");
		let gap = store.prune_by_time("aspect", 91, 99).await.expect("prunes");
		drop(store);
		assert_eq!(ids(&mid), vec![1]);
		assert_eq!(ids(&straddle), vec![0, 1]);
		assert_eq!(ids(&all), vec![0, 1, 2]);
		assert!(gap.is_empty());
	}

	#[tokio::test]
	async fn aspects_are_isolated() {
		let store = SegmentIndexStore::open_in_memory().await.expect("opens");
		let (seg, len) = sealed(0);
		store.insert("a", &SegmentDescriptor::of_segment(0, "a0.weftseg", len, &seg)).await.expect("inserts");
		store.insert("b", &SegmentDescriptor::of_segment(0, "b0.weftseg", len, &seg)).await.expect("inserts");
		store.insert("b", &SegmentDescriptor::of_segment(1, "b1.weftseg", len, &seg)).await.expect("inserts");
		let count_a = store.count("a").await.expect("counts");
		let count_b = store.count("b").await.expect("counts");
		// Pruning never crosses aspects.
		let prune_a = store.prune_by_time("a", 0, 90).await.expect("prunes").len();
		drop(store);
		assert_eq!(count_a, 1);
		assert_eq!(count_b, 2);
		assert_eq!(prune_a, 1);
	}

	#[tokio::test]
	async fn reinsert_same_id_replaces() {
		let store = SegmentIndexStore::open_in_memory().await.expect("opens");
		let (seg, len) = sealed(0);
		store.insert("a", &SegmentDescriptor::of_segment(0, "old.weftseg", len, &seg)).await.expect("inserts");
		store.insert("a", &SegmentDescriptor::of_segment(0, "new.weftseg", len, &seg)).await.expect("re-inserts");
		let all = store.all("a").await.expect("reads");
		drop(store);
		assert_eq!(all.len(), 1, "same (aspect, id) replaces, not duplicates");
		assert_eq!(all[0].path, "new.weftseg");
	}

	#[tokio::test]
	#[allow(deprecated)]
	async fn delete_removes_a_row_and_reports_whether_it_matched() {
		let store = SegmentIndexStore::open_in_memory().await.expect("opens");
		let (seg, len) = sealed(0);
		store.insert("a", &SegmentDescriptor::of_segment(0, "s0.weftseg", len, &seg)).await.expect("inserts 0");
		store.insert("a", &SegmentDescriptor::of_segment(1, "s1.weftseg", len, &seg)).await.expect("inserts 1");
		// Deleting an existing id removes exactly that row and reports true.
		assert!(store.delete("a", 0).await.expect("deletes"), "an existing id is deleted");
		let remaining = store.all("a").await.expect("reads");
		// Deleting an absent id is a false no-op.
		let missed = store.delete("a", 7).await.expect("no-op delete");
		// After deleting the max id, next_id still hands out a free id past the new max.
		store.insert("a", &SegmentDescriptor::of_segment(1, "s1.weftseg", len, &seg)).await.expect("keeps 1");
		let next = store.next_id("a").await.expect("next id");
		drop(store);
		assert_eq!(remaining.len(), 1);
		assert_eq!(remaining[0].id, 1, "only segment 1 survives");
		assert!(!missed, "deleting an absent id reports false");
		assert_eq!(next, 2, "next_id is one past the surviving max");
	}

	#[tokio::test]
	async fn nullable_and_metadata_round_trip() {
		let store = SegmentIndexStore::open_in_memory().await.expect("opens");
		// A nullable segment with two nulls and a scaled encoding to exercise the
		// JSON metadata + plain-text decimal round trip.
		let ts = vec![10_i64, 20, 30, 40];
		let vs = vec![Some(bd("1.25")), None, Some(bd("3.75")), None];
		let seg = Segment::build_nullable(&ts, &vs, TimeUnit::Micros, &bd("0")).expect("builds");
		let d = SegmentDescriptor::of_segment(5, "n.weftseg", seg.write_to().len() as u64, &seg);
		store.insert("a", &d).await.expect("inserts");
		let back = store.all("a").await.expect("reads").into_iter().next().expect("one row");
		drop(store);
		assert_eq!(back, d);
		assert_eq!(back.null_count, 2);
		assert_eq!(back.physical_type, d.physical_type);
		assert_eq!(back.time_unit, Some(TimeUnit::Micros));
		assert_eq!(back.min_value, Some(bd("1.25")));
	}

	#[tokio::test]
	async fn load_index_rebuilds_resident_pruning() {
		let store = SegmentIndexStore::open_in_memory().await.expect("opens");
		// Values offset by base ⇒ disjoint value spans [0,9], [100,109], [200,209].
		for (i, base) in [0_i64, 100, 200].into_iter().enumerate() {
			let ts: Vec<i64> = (0..10).map(|v| base + v * 10).collect();
			let vs: Vec<BigDecimal> = (0..10).map(|v| BigDecimal::from(base + v)).collect();
			let seg = Segment::build(&ts, &vs, TimeUnit::Seconds, &bd("0")).expect("builds");
			store.insert("a", &SegmentDescriptor::of_segment(i as u64, format!("s{i}.weftseg"), seg.write_to().len() as u64, &seg)).await.expect("inserts");
		}
		let index = store.load_index("a").await.expect("loads");
		drop(store);
		assert_eq!(index.len(), 3);
		assert_eq!(index.total_rows(), 30);
		// The resident index answers value pruning the SQL store does not.
		let hit = index.prune_by_value(&bd("102"), &bd("108"));
		assert_eq!(hit.len(), 1, "values 102..108 only in the second segment");
		assert_eq!(hit[0].id, 1);
	}

	#[tokio::test]
	async fn empty_segment_never_overlaps() {
		let store = SegmentIndexStore::open_in_memory().await.expect("opens");
		let empty = Segment::build(&[], &[], TimeUnit::Seconds, &bd("0")).expect("builds");
		let d = SegmentDescriptor::of_segment(0, "empty.weftseg", empty.write_to().len() as u64, &empty);
		store.insert("a", &d).await.expect("inserts");
		// Stored, but its NULL span is excluded from every time prune.
		let count = store.count("a").await.expect("counts");
		let pruned = store.prune_by_time("a", i64::MIN, i64::MAX).await.expect("prunes");
		drop(store);
		assert_eq!(count, 1);
		assert!(pruned.is_empty());
	}

	#[tokio::test]
	async fn list_aspects_enumerates_distinct_aspects_in_order() {
		let store = SegmentIndexStore::open_in_memory().await.expect("opens");
		let (seg, len) = sealed(0);
		// Two aspects, one with multiple segments — list_aspects deduplicates.
		store.insert("temp", &SegmentDescriptor::of_segment(0, "t0.weftseg", len, &seg)).await.expect("inserts");
		store.insert("temp", &SegmentDescriptor::of_segment(1, "t1.weftseg", len, &seg)).await.expect("inserts");
		store.insert("humidity", &SegmentDescriptor::of_segment(0, "h0.weftseg", len, &seg)).await.expect("inserts");
		let aspects = store.list_aspects().await.expect("lists");
		// An empty index lists nothing.
		let empty = SegmentIndexStore::open_in_memory().await.expect("opens");
		let none = empty.list_aspects().await.expect("lists");
		drop(store);
		drop(empty);
		assert_eq!(aspects, vec!["humidity".to_string(), "temp".to_string()]);
		assert!(none.is_empty());
	}

	/// Every file in `dir`, by name, with its bytes.
	fn files_in(dir: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
		std::fs::read_dir(dir).expect("lists the directory").map(|entry| entry.expect("an entry").path()).filter(|path| path.is_file()).map(|path| (path.file_name().expect("a name").to_string_lossy().into_owned(), std::fs::read(&path).expect("reads the file"))).collect()
	}

	/// The journal mode of the database at `path`, read on a raw connection.
	async fn journal_mode(path: &str) -> Value {
		let db = turso::Builder::new_local(path).build().await.expect("opens raw");
		let mut rows = db.connect().expect("connects").query("PRAGMA journal_mode", ()).await.expect("reads the journal mode");
		let mode = rows.next().await.expect("answers").expect("a row").get_value(0).expect("the mode");
		drop(rows);
		drop(db);
		mode
	}

	/// The open refuses a database whose `store_meta` records a write floor newer than
	/// this build knows before it writes anything to it: no DDL, no switch to MVCC, no
	/// header sync. The refused files are byte for byte what they were, for a newer store
	/// in MVCC and for one in WAL mode, which the refused open leaves in WAL. The floor is
	/// `min_write_layout`; a store that records only `layout_version` (layout 2's first
	/// development stores) is judged by that, and a newer layout whose write floor this
	/// build meets opens.
	#[tokio::test]
	async fn a_newer_write_floor_is_refused_before_anything_is_written() {
		let dir = tempfile::TempDir::new().expect("tempdir");
		let path = dir.path().join("segment_index.db");
		let path = path.to_string_lossy();
		let store = SegmentIndexStore::open(&path).await.expect("opens");
		let conn = store.db.connect().expect("connects");
		// A newer WeftDB that dropped one of layout 2's tables and the time index, and
		// recorded only its layout.
		conn.execute("INSERT INTO store_meta (key, value) VALUES ('layout_version', '3')", ()).await.expect("plays a newer WeftDB");
		conn.execute("DROP TABLE segment_changes", ()).await.expect("drops a v2 table");
		conn.execute("DROP INDEX idx_segment_index_time", ()).await.expect("drops the time index");
		drop(conn);
		drop(store);
		let before = files_in(dir.path());
		let refused = SegmentIndexStore::open(&path).await.err().expect("a newer layout is refused");
		let after = files_in(dir.path());
		let db = turso::Builder::new_local(&path).build().await.expect("opens the refused store raw");
		let conn = db.connect().expect("connects");
		let mut rows = conn.query("SELECT name FROM sqlite_schema WHERE name IN ('segment_changes', 'idx_segment_index_time')", ()).await.expect("reads the schema");
		let recreated = rows.next().await.expect("reads").map(|row| row.get_value(0).expect("a name"));
		drop(rows);
		// The same store in WAL mode: the refused open must not switch it to MVCC.
		let mut rows = conn.query("PRAGMA journal_mode=wal", ()).await.expect("switches back to WAL");
		assert_eq!(rows.next().await.expect("answers").expect("a row").get_value(0).expect("the mode"), Value::Text("wal".into()));
		drop(rows);
		drop(conn);
		drop(db);
		let wal_before = files_in(dir.path());
		let wal_refused = SegmentIndexStore::open(&path).await.err().expect("a newer layout in WAL mode is refused");
		let wal_after = files_in(dir.path());
		let wal_mode = journal_mode(&path).await;
		assert_eq!(refused.downcast_ref::<StoreError>(), Some(&StoreError::IncompatibleLayout { min_write: 3, supported: SUPPORTED_LAYOUT }), "{refused:#}");
		assert_eq!(recreated, None, "the refused open ran none of its DDL on the newer store");
		assert!(before == after, "the refused open wrote nothing to the database files: {:?} became {:?}", before.keys(), after.keys());
		assert_eq!(wal_refused.downcast_ref::<StoreError>(), Some(&StoreError::IncompatibleLayout { min_write: 3, supported: SUPPORTED_LAYOUT }), "{wal_refused:#}");
		assert!(wal_before == wal_after, "nor to a newer store in WAL mode: {:?} became {:?}", wal_before.keys(), wal_after.keys());
		assert_eq!(wal_mode, Value::Text("wal".into()), "which it did not switch to MVCC");

		// A newer layout whose write floor this build meets is not refused.
		let db = turso::Builder::new_local(&path).build().await.expect("opens raw");
		let conn = db.connect().expect("connects");
		conn.execute("INSERT INTO store_meta (key, value) VALUES ('min_write_layout', '2')", ()).await.expect("records a write floor");
		drop(conn);
		drop(db);
		let store = SegmentIndexStore::open(&path).await.expect("a newer layout this build may write opens");
		let floor = store.meta("min_write_layout").await.expect("reads");
		drop(store);
		assert_eq!(floor.as_deref(), Some("2"));
	}

	/// Design section 1.3, through the real open: when it is the open that switches
	/// `segment_index.db` to MVCC, every write it made to the DB file (the switched header
	/// first among them) is synced by the time it returns, for a new store and for a v2
	/// store that was switched back to WAL, whose switch Turso does not sync by itself.
	/// The rows are all there, and an open of a store already in MVCC syncs the DB file
	/// as well (see `control_plane`: it cannot tell whether that header was ever synced).
	#[tokio::test]
	async fn the_open_leaves_the_switched_header_synced() {
		use crate::types::durable::turso_probe::{FileEvent, ProbeIo};

		const FILE: &str = "segment_index.db";
		let dir = tempfile::TempDir::new().expect("tempdir");
		let path = dir.path().join(FILE);
		let path = path.to_string_lossy();
		let (seg, len) = sealed(0);
		let opened = |io: &std::sync::Arc<ProbeIo>| SegmentIndexStore::open_with_io(&path, io.clone());

		let io = ProbeIo::new().expect("probe");
		let store = opened(&io).await.expect("creates the store");
		let synced = io.synced_since_last_write(FILE);
		store.insert("a", &SegmentDescriptor::of_segment(0, "a0.weftseg", len, &seg)).await.expect("inserts");
		drop(store);
		assert!(synced, "a new store: {:?}", io.events());

		let db = turso::Builder::new_local(&path).build().await.expect("opens raw");
		let conn = db.connect().expect("connects");
		let mut rows = conn.query("PRAGMA journal_mode=wal", ()).await.expect("switches back to WAL");
		assert_eq!(rows.next().await.expect("answers").expect("a row").get_value(0).expect("the mode"), Value::Text("wal".into()));
		drop(rows);
		drop(conn);
		drop(db);

		let io = ProbeIo::new().expect("probe");
		let store = opened(&io).await.expect("switches the store back to MVCC");
		let header_written = io.writes(FILE).iter().any(|&(pos, _)| pos == 0);
		let synced = io.synced_since_last_write(FILE);
		let all = store.all("a").await.expect("reads");
		drop(store);
		assert!(header_written, "the switch wrote the header: {:?}", io.events());
		assert!(synced, "a store switched back to WAL: {:?}", io.events());
		assert_eq!(all.len(), 1);

		let io = ProbeIo::new().expect("probe");
		drop(opened(&io).await.expect("reopens"));
		assert!(io.events().contains(&FileEvent::Sync { file: FILE.to_string() }) && io.synced_since_last_write(FILE), "an open of a store already in MVCC syncs the DB file too, since it cannot tell whether its header ever was: {:?}", io.events());
	}

	#[tokio::test]
	#[allow(deprecated)]
	async fn next_id_is_monotonic_per_aspect() {
		let store = SegmentIndexStore::open_in_memory().await.expect("opens");
		let (seg, len) = sealed(0);
		// An empty aspect starts at 0.
		let first = store.next_id("a").await.expect("next_id");
		store.insert("a", &SegmentDescriptor::of_segment(first, "a0.weftseg", len, &seg)).await.expect("inserts");
		// After inserting id 0, the next is 1; a different aspect is independent.
		let second = store.next_id("a").await.expect("next_id");
		let other = store.next_id("b").await.expect("next_id");
		store.insert("a", &SegmentDescriptor::of_segment(second, "a1.weftseg", len, &seg)).await.expect("inserts");
		let third = store.next_id("a").await.expect("next_id");
		drop(store);
		assert_eq!(first, 0);
		assert_eq!(second, 1);
		assert_eq!(third, 2);
		assert_eq!(other, 0, "a fresh aspect starts at 0");
	}
}
