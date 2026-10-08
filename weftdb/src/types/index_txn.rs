//! Atomic multi-op `segment_index.db` transactions (docs/design/crash-consistency.md
//! section 5.1, `IndexTxn`; slice S6).
//!
//! Every state change of the write-once design commits exactly one `segment_index.db`
//! transaction: a seal inserts its rows together with the allocator and ledger rows, a
//! maintenance swap replaces and deletes its members together with its journal rows.
//! [`IndexTxn`] is that commit point. It applies a list of [`IndexOp`]s in one
//! `BEGIN CONCURRENT … COMMIT`, so a crash, an error or a failed precondition between
//! any two of them leaves none of them behind.
//!
//! It also says what a failure means, because the caller's next step depends on it
//! ([`TxnErrorKind`]):
//!
//! - **Retryable**: an MVCC conflict (`Busy`, `BusySnapshot`, a write-write conflict)
//!   before COMMIT. Nothing was written. Whether the whole transaction is retried on a
//!   fresh snapshot (up to [`MAX_RETRIES`] times, with backoff) depends on its ops; see
//!   below.
//! - **Conflict**: a precondition failed. [`IndexOp::ReplaceExpected`] and
//!   [`IndexOp::DeleteExpected`] must each change exactly the one row whose
//!   `(aspect, id, gen, frame_crc)` the caller read, and [`IndexOp::InsertNew`] must not
//!   find its key taken. Another writer got there first; nothing was written.
//! - **Definite**: any other error before COMMIT. Nothing was written.
//! - **Ambiguous**: COMMIT itself returned an error. Turso fsyncs the MVCC log inside
//!   COMMIT and returns a sync error with `?`, so the transaction may be durable even
//!   though the caller saw an error, and only the log replay at the next open knows.
//!   The store write-poisons itself on this (`SegmentStore`'s poison), because any
//!   further write could build on a state that does or does not exist.
//!
//! One kind of COMMIT error is not ambiguous: the conflicts Turso raises while it
//! validates the commit (a write-write conflict with a transaction that committed first,
//! a stale snapshot or an aborted commit dependency, the last two as `BusySnapshot`).
//! Turso checks those before it writes the log record and rolls the transaction back,
//! so they are certain not to have committed and are classified like the same conflicts
//! raised by a statement. Calling them ambiguous would poison a store over an ordinary
//! race between two writers.
//!
//! **Which conflicts are retried.** A retry re-runs every op on a snapshot that now holds
//! the commit of the writer that won the conflict, so it is only safe for an op that
//! re-checks, on that snapshot, what its caller decided from: [`IndexOp::InsertNew`],
//! [`IndexOp::ReplaceExpected`] and [`IndexOp::DeleteExpected`] are guarded, and a race
//! they lost turns into a [`TxnErrorKind::Conflict`] on the retry. The legacy
//! [`IndexOp::Upsert`] and [`IndexOp::Delete`] are not: their callers chose the id from an
//! earlier read (a seal's `MAX(id) + 1`, a reconcile's or a squash's member list), and a
//! retry would replace or delete the row the winner just committed, then report success
//! to both callers. So a transaction holding either of them is retried only when its
//! conflict came before any op ran (a `Busy` at `BEGIN`, which used no snapshot), and is
//! otherwise reported at its first conflict, as every segment-index write was before
//! this type existed. The write-once protocols that replace those ops (S7-S9) get the
//! retries.

use std::{fmt, time::Duration};

use turso::Value;
use weft_physical_type::SegmentDescriptor;

use crate::types::durable::fault::{self, FaultPoint};

/// How many times a [`TxnErrorKind::Retryable`] failure is retried before it is
/// returned (design section 5.1).
pub const MAX_RETRIES: u32 = 5;

/// The `segment_index` columns every row write sets, in the order [`row_values`] binds
/// them.
const ROW_COLUMNS: &str = "aspect, id, path, format_version, physical_type, time_unit, row_count, null_count, time_sorted, min_ts, max_ts, min_value, max_value, byte_len, gen, prec, frame_crc, commit_epoch";

/// One placeholder per [`ROW_COLUMNS`] entry.
const ROW_PLACEHOLDERS: &str = "?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?";

/// One `segment_index` row: the segment's descriptor plus the write-once columns layout
/// v2 adds beside it.
///
/// [`SegmentDescriptor`] belongs to `weft-physical-type` and describes a frame wherever
/// it is stored, so the columns that only mean something to this store's commit
/// protocols live here instead of on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRow {
	/// The frame's descriptor; `desc.path` is the root-joined path it was written under.
	pub desc: SegmentDescriptor,
	/// The frame's generation: 0 for a legacy `{aspect}-{id}` frame, and from a
	/// per-aspect counter that is never reused for a write-once frame (S7, S10).
	pub gen: u64,
	/// The adoption-order key: `gen` for a seal, the largest member `prec` for a
	/// maintenance output. `None` on legacy rows, which count as 0.
	pub prec: Option<u64>,
	/// The frame's CRC-32 trailer. `None` until something binds it (legacy rows, until
	/// a backup or scrub fills it in).
	pub frame_crc: Option<u32>,
	/// The aspect epoch the row committed in. `None` on legacy rows.
	pub commit_epoch: Option<u64>,
}

impl IndexRow {
	/// A legacy row: generation 0, nothing bound. What every seal and in-place rewrite
	/// writes until the write-once protocols take over (S7-S10).
	pub const fn legacy(desc: SegmentDescriptor) -> Self {
		Self { desc, gen: 0, prec: None, frame_crc: None, commit_epoch: None }
	}

	/// The identity a precondition on this row names.
	#[cfg_attr(not(test), expect(dead_code, reason = "the swap protocols (S8, S9) take their preconditions from the rows they read"))]
	pub const fn version(&self) -> RowVersion {
		RowVersion { id: self.desc.id, gen: self.gen, frame_crc: self.frame_crc }
	}
}

/// The version of a row a precondition expects to find: the row of its aspect with this
/// `id` and `gen` whose `frame_crc` is this one (`IS`, so `None` matches only NULL).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowVersion {
	pub id: u64,
	pub gen: u64,
	pub frame_crc: Option<u32>,
}

/// One change an [`IndexTxn`] applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexOp {
	/// Insert a row whose `(aspect, id)` must be free: a plain `INSERT`, never
	/// `OR REPLACE`. A taken key is a [`TxnErrorKind::Conflict`].
	#[cfg_attr(not(test), expect(dead_code, reason = "seals switch to plain INSERT with the persistent allocator (S7)"))]
	InsertNew { aspect: String, row: IndexRow },
	/// Replace the row matching `expected` with `row`. Exactly one row must match.
	#[cfg_attr(not(test), expect(dead_code, reason = "the maintenance swaps (S8, S9) replace their members with this"))]
	ReplaceExpected { aspect: String, expected: RowVersion, row: IndexRow },
	/// Delete the row matching `expected`. Exactly one row must match.
	#[cfg_attr(not(test), expect(dead_code, reason = "the maintenance swaps (S8, S9) delete their members with this"))]
	DeleteExpected { aspect: String, expected: RowVersion },
	/// Insert `row`, replacing any row with its `(aspect, id)` (`INSERT OR REPLACE`).
	/// Today's seal and in-place rewrite semantics, kept until S7-S9 replace them with
	/// [`InsertNew`](Self::InsertNew) and [`ReplaceExpected`](Self::ReplaceExpected).
	Upsert { aspect: String, row: IndexRow },
	/// Delete the row with `(aspect, id)`, if any. Today's merge and squash semantics,
	/// kept until S9 replaces them with [`DeleteExpected`](Self::DeleteExpected).
	Delete { aspect: String, id: u64 },
}

impl IndexOp {
	/// Whether the op re-checks, when it runs, the state its caller decided from, so
	/// that running it again on a later snapshot cannot overwrite or delete a row another
	/// writer committed in between (see the module documentation).
	const fn is_guarded(&self) -> bool {
		match self {
			Self::InsertNew { .. } | Self::ReplaceExpected { .. } | Self::DeleteExpected { .. } => true,
			Self::Upsert { .. } | Self::Delete { .. } => false,
		}
	}
}

/// The fault points one [`IndexTxn`] hits, so the crash tests can stop it between its
/// steps. The protocol that builds a transaction passes its own (a seal's `S-*`, a
/// swap's `M-*`, from S8 and S10 on). Every transaction S6 commits passes
/// [`NONE`](Self::NONE): a point that every seal hit could not be armed in-process
/// while other tests seal alongside.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TxnPoints {
	/// Hit right after `BEGIN CONCURRENT`: `S-txn-begun`, `M-swap-begun`.
	pub begun: Option<FaultPoint>,
	/// Hit right after the op with this index has been applied, before the next one or
	/// COMMIT: `S-rows-inserted` after a seal's last row.
	pub after_op: Option<(usize, FaultPoint)>,
	/// Hit after COMMIT returned success. An error injected here is the phantom commit
	/// the design tests poison with: committed, but the caller sees a failure.
	/// `S-commit-phantom`, `M-swap-phantom`.
	pub phantom: Option<FaultPoint>,
}

impl TxnPoints {
	/// No fault points.
	pub const NONE: Self = Self { begun: None, after_op: None, phantom: None };
}

/// A list of [`IndexOp`]s that commit together or not at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexTxn {
	ops: Vec<IndexOp>,
	points: TxnPoints,
}

/// What a committed [`IndexTxn`] changed: the rows each op affected, in op order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxnApplied {
	pub changes: Vec<u64>,
	/// How many attempts it took: 1, plus one per retry.
	pub attempts: u32,
}

/// Why an [`IndexTxn`] failed, which decides what its caller may do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnErrorKind {
	/// An MVCC conflict before COMMIT, and either the transaction may not be retried
	/// (an unguarded op had run; see the module documentation) or the conflict outlasted
	/// every retry. Nothing was written; the caller may try again later, from a fresh
	/// read.
	Retryable,
	/// A precondition failed. Nothing was written; the caller's view of the rows is stale.
	Conflict,
	/// Any other failure before COMMIT. Nothing was written.
	Definite,
	/// COMMIT returned an error other than a conflict found while validating the commit
	/// (or a phantom fault fired after it): the transaction may or may not be durable.
	/// The store must stop writing until a restart recovers.
	Ambiguous,
}

impl fmt::Display for TxnErrorKind {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::Retryable => "an MVCC conflict",
			Self::Conflict => "a precondition that no longer holds",
			Self::Definite => "an error before COMMIT",
			Self::Ambiguous => "an ambiguous COMMIT",
		})
	}
}

/// A failed [`IndexTxn`]: its [`TxnErrorKind`], the op it failed at (when it failed at
/// one), what went wrong, and how many attempts were made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexTxnError {
	pub kind: TxnErrorKind,
	pub op: Option<usize>,
	pub message: String,
	/// How many attempts were made: 1, plus one per retry. The error is the last one's.
	pub attempts: u32,
}

impl IndexTxnError {
	const fn new(kind: TxnErrorKind, op: Option<usize>, message: String) -> Self {
		Self { kind, op, message, attempts: 1 }
	}

	/// Whether the transaction may have committed although it reported an error.
	pub fn is_ambiguous(&self) -> bool {
		self.kind == TxnErrorKind::Ambiguous
	}
}

impl fmt::Display for IndexTxnError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "segment_index transaction failed with {}", self.kind)?;
		if let Some(op) = self.op {
			write!(f, " at op {op}")?;
		}
		if self.attempts > 1 {
			write!(f, " (attempt {})", self.attempts)?;
		}
		write!(f, ": {}", self.message)
	}
}

/// A failed attempt of an [`IndexTxn`], and whether any of its ops had run, which
/// decides whether an unguarded transaction may be retried.
struct AttemptFailure {
	error: IndexTxnError,
	ops_ran: bool,
}

impl AttemptFailure {
	/// A failure before the first op ran: connecting, or `BEGIN`.
	const fn before_ops(error: IndexTxnError) -> Self {
		Self { error, ops_ran: false }
	}

	/// A failure once an op had run, or could have: from the ops themselves, COMMIT, or
	/// after it.
	const fn after_ops(error: IndexTxnError) -> Self {
		Self { error, ops_ran: true }
	}
}

impl std::error::Error for IndexTxnError {}

impl IndexTxn {
	/// A transaction of `ops`, hitting no fault points.
	pub const fn new(ops: Vec<IndexOp>) -> Self {
		Self { ops, points: TxnPoints::NONE }
	}

	/// The same transaction, hitting `points`.
	#[must_use]
	#[cfg_attr(not(test), expect(dead_code, reason = "the seal and swap protocols (S8, S10) pass their fault points; until then only the tests do"))]
	pub const fn with_points(mut self, points: TxnPoints) -> Self {
		self.points = points;
		self
	}

	/// Apply every op in one `BEGIN CONCURRENT … COMMIT` on a fresh connection to `db`,
	/// retrying the whole transaction on a [`TxnErrorKind::Retryable`] failure when that
	/// is safe (see the module documentation).
	///
	/// # Errors
	///
	/// An [`IndexTxnError`] whose kind says whether the transaction is certain not to
	/// have committed (every kind but [`TxnErrorKind::Ambiguous`]).
	pub async fn run(&self, db: &turso::Database) -> Result<TxnApplied, IndexTxnError> {
		self.run_while(db, || true).await
	}

	/// [`run`](Self::run), asking `may_retry` before each retry, after its backoff: when it
	/// answers `false`, the conflict the retry would have resolved is returned instead. A
	/// store passes "not poisoned", so that a transaction waiting out a conflict does not
	/// commit after another writer poisoned the store.
	///
	/// # Errors
	///
	/// As [`run`](Self::run).
	pub async fn run_while(&self, db: &turso::Database, may_retry: impl Fn() -> bool) -> Result<TxnApplied, IndexTxnError> {
		let mut attempts = 1;
		loop {
			let failure = match self.attempt(db).await {
				Ok(changes) => return Ok(TxnApplied { changes, attempts }),
				Err(failure) => failure,
			};
			let error = IndexTxnError { attempts, ..failure.error };
			let retry_safe = !failure.ops_ran || self.ops.iter().all(IndexOp::is_guarded);
			if error.kind != TxnErrorKind::Retryable || !retry_safe || attempts > MAX_RETRIES {
				return Err(error);
			}
			tracing::debug!(retry = attempts, of = MAX_RETRIES, %error, "segment_index transaction lost an MVCC conflict; retrying it on a fresh snapshot");
			tokio::time::sleep(backoff(attempts)).await;
			if !may_retry() {
				return Err(error);
			}
			attempts += 1;
		}
	}

	/// One attempt of [`run_while`](Self::run_while): the rows each op changed, or why it
	/// failed.
	async fn attempt(&self, db: &turso::Database) -> Result<Vec<u64>, AttemptFailure> {
		let conn = db.connect().map_err(|e| AttemptFailure::before_ops(IndexTxnError::new(TxnErrorKind::Definite, None, format!("connecting: {e}"))))?;
		conn.execute("BEGIN CONCURRENT", ()).await.map_err(|e| AttemptFailure::before_ops(IndexTxnError::new(statement_error_kind(&e), None, format!("BEGIN CONCURRENT: {e}"))))?;
		let changes = match self.apply_ops(&conn).await {
			Ok(changes) => changes,
			Err(e) => {
				rollback(&conn).await;
				return Err(AttemptFailure::after_ops(e));
			}
		};
		if let Err(e) = conn.execute("COMMIT", ()).await {
			// A COMMIT that failed validation rolled itself back; one that failed later
			// may have left the connection inside the transaction. Either way the
			// connection is dropped next, and only recovery can say what is durable.
			rollback(&conn).await;
			return Err(AttemptFailure::after_ops(IndexTxnError::new(commit_error_kind(&e), None, format!("COMMIT: {e}"))));
		}
		if let Some(point) = self.points.phantom {
			fault::hit(point).await.map_err(|e| AttemptFailure::after_ops(IndexTxnError::new(TxnErrorKind::Ambiguous, None, format!("after COMMIT: {e}"))))?;
		}
		Ok(changes)
	}

	/// Apply each op inside the open transaction, checking its precondition and hitting
	/// the fault points between them.
	async fn apply_ops(&self, conn: &turso::Connection) -> Result<Vec<u64>, IndexTxnError> {
		if let Some(point) = self.points.begun {
			fault::hit(point).await.map_err(|e| IndexTxnError::new(TxnErrorKind::Definite, None, e.to_string()))?;
		}
		let mut changes = Vec::with_capacity(self.ops.len());
		for (index, op) in self.ops.iter().enumerate() {
			changes.push(apply(conn, op).await.map_err(|(kind, message)| IndexTxnError::new(kind, Some(index), message))?);
			if let Some((after, point)) = self.points.after_op {
				if after == index {
					fault::hit(point).await.map_err(|e| IndexTxnError::new(TxnErrorKind::Definite, Some(index), e.to_string()))?;
				}
			}
		}
		Ok(changes)
	}
}

/// Apply one op and check its precondition, returning the rows it changed.
async fn apply(conn: &turso::Connection, op: &IndexOp) -> Result<u64, (TxnErrorKind, String)> {
	let statement_error = |what: &str, e: &turso::Error| (statement_error_kind(e), format!("{what}: {e}"));
	match op {
		IndexOp::InsertNew { aspect, row } => {
			let sql = format!("INSERT INTO segment_index ({ROW_COLUMNS}) VALUES ({ROW_PLACEHOLDERS})");
			conn.execute(sql, row_values(aspect, row)?).await.map_err(|e| statement_error(&format!("inserting {aspect:?} segment {}", row.desc.id), &e))
		}
		IndexOp::Upsert { aspect, row } => {
			let sql = format!("INSERT OR REPLACE INTO segment_index ({ROW_COLUMNS}) VALUES ({ROW_PLACEHOLDERS})");
			conn.execute(sql, row_values(aspect, row)?).await.map_err(|e| statement_error(&format!("upserting {aspect:?} segment {}", row.desc.id), &e))
		}
		IndexOp::ReplaceExpected { aspect, expected, row } => {
			let assignments = ROW_COLUMNS.split(", ").map(|column| format!("{column} = ?")).collect::<Vec<_>>().join(", ");
			let sql = format!("UPDATE segment_index SET {assignments} WHERE aspect = ? AND id = ? AND gen = ? AND frame_crc IS ?");
			let mut params = row_values(aspect, row)?;
			params.extend(version_values(aspect, *expected));
			let changed = conn.execute(sql, params).await.map_err(|e| statement_error(&format!("replacing {aspect:?} segment {}", expected.id), &e))?;
			expect_one(changed, "replace", aspect, *expected)
		}
		IndexOp::DeleteExpected { aspect, expected } => {
			let sql = "DELETE FROM segment_index WHERE aspect = ? AND id = ? AND gen = ? AND frame_crc IS ?";
			let changed = conn.execute(sql, version_values(aspect, *expected)).await.map_err(|e| statement_error(&format!("deleting {aspect:?} segment {}", expected.id), &e))?;
			expect_one(changed, "delete", aspect, *expected)
		}
		IndexOp::Delete { aspect, id } => conn.execute("DELETE FROM segment_index WHERE aspect = ? AND id = ?", vec![Value::Text(aspect.clone()), integer(*id)]).await.map_err(|e| statement_error(&format!("deleting {aspect:?} segment {id}"), &e)),
	}
}

/// A precondition holds when the guarded statement changed exactly one row.
fn expect_one(changed: u64, what: &str, aspect: &str, expected: RowVersion) -> Result<u64, (TxnErrorKind, String)> {
	if changed == 1 {
		return Ok(changed);
	}
	let crc = expected.frame_crc.map_or_else(|| "NULL".to_string(), |crc| format!("{crc:#010x}"));
	Err((TxnErrorKind::Conflict, format!("{what} expected one {aspect:?} row with id {}, gen {} and frame_crc {crc}, and matched {changed}", expected.id, expected.gen)))
}

/// The values [`ROW_COLUMNS`] binds for `row` under `aspect`.
fn row_values(aspect: &str, row: &IndexRow) -> Result<Vec<Value>, (TxnErrorKind, String)> {
	let desc = &row.desc;
	let json = |what: &str, value: Result<String, serde_json::Error>| value.map(Value::Text).map_err(|e| (TxnErrorKind::Definite, format!("encoding the {what} of {aspect:?} segment {}: {e}", desc.id)));
	let physical_type = match &desc.physical_type {
		Some(pt) => json("physical type", serde_json::to_string(pt))?,
		None => Value::Null,
	};
	let time_unit = match &desc.time_unit {
		Some(tu) => json("time unit", serde_json::to_string(tu))?,
		None => Value::Null,
	};
	let decimal = |value: Option<&bigdecimal::BigDecimal>| value.map_or(Value::Null, |v| Value::Text(v.to_plain_string()));
	Ok(vec![Value::Text(aspect.to_string()), integer(desc.id), Value::Text(desc.path.clone()), Value::Integer(i64::from(desc.format_version)), physical_type, time_unit, integer(desc.row_count as u64), integer(desc.null_count as u64), Value::Integer(i64::from(desc.time_sorted)), desc.min_ts.map_or(Value::Null, Value::Integer), desc.max_ts.map_or(Value::Null, Value::Integer), decimal(desc.min_value.as_ref()), decimal(desc.max_value.as_ref()), integer(desc.byte_len), integer(row.gen), row.prec.map_or(Value::Null, integer), row.frame_crc.map_or(Value::Null, |crc| Value::Integer(i64::from(crc))), row.commit_epoch.map_or(Value::Null, integer)])
}

/// The `aspect = ? AND id = ? AND gen = ? AND frame_crc IS ?` values of `expected`.
fn version_values(aspect: &str, expected: RowVersion) -> [Value; 4] {
	[Value::Text(aspect.to_string()), integer(expected.id), integer(expected.gen), expected.frame_crc.map_or(Value::Null, |crc| Value::Integer(i64::from(crc)))]
}

/// A `u64` as SQL stores it (saturating, as the rest of the index does).
fn integer(n: u64) -> Value {
	Value::Integer(i64::try_from(n).unwrap_or(i64::MAX))
}

/// Roll back what is left of a failed attempt. Its error is not interesting: Turso has
/// already rolled back a transaction that hit a conflict, and the connection is
/// dropped right after.
async fn rollback(conn: &turso::Connection) {
	if let Err(e) = conn.execute("ROLLBACK", ()).await {
		tracing::trace!(error = %e, "ROLLBACK after a failed segment_index transaction");
	}
}

/// Whether `e` is an MVCC conflict that retrying the whole transaction can resolve: a
/// busy database, a stale snapshot or aborted commit dependency (`BusySnapshot`), or a
/// write-write conflict (Turso 0.8 reports the last only as a generic error, by its
/// message; `database/inputs.rs` matches it the same way).
fn is_mvcc_conflict(e: &turso::Error) -> bool {
	match e {
		turso::Error::Busy(_) | turso::Error::BusySnapshot(_) => true,
		turso::Error::Error(message) => message.contains("Write-write conflict"),
		_ => false,
	}
}

/// The kind of an error from `BEGIN` or an op's statement, all before COMMIT.
fn statement_error_kind(e: &turso::Error) -> TxnErrorKind {
	if is_mvcc_conflict(e) {
		TxnErrorKind::Retryable
	} else if matches!(e, turso::Error::Constraint(_)) {
		// A plain INSERT onto a taken key: the "must not exist" precondition failed.
		TxnErrorKind::Conflict
	} else {
		TxnErrorKind::Definite
	}
}

/// The kind of an error COMMIT returned: ambiguous, except for the conflicts Turso
/// detects while validating the commit, before it writes the log record (see the module
/// documentation). A plain `Busy` stays ambiguous: nothing pins down where in the
/// commit it can arise.
fn commit_error_kind(e: &turso::Error) -> TxnErrorKind {
	match e {
		turso::Error::BusySnapshot(_) => TxnErrorKind::Retryable,
		turso::Error::Error(message) if message.contains("Write-write conflict") => TxnErrorKind::Retryable,
		_ => TxnErrorKind::Ambiguous,
	}
}

/// The wait before retry `retry` (from 1): doubling from 20 ms to at most 320 ms, plus up
/// to half again of jitter, so two writers that conflicted do not retry in step.
fn backoff(retry: u32) -> Duration {
	let base = 10_u64 << retry.min(5);
	Duration::from_millis(base + fastrand::u64(0..=base / 2))
}

#[cfg(test)]
mod tests {
	use bigdecimal::BigDecimal;
	use weft_physical_type::{timestamp::TimeUnit, Segment};

	use super::*;
	use crate::SegmentIndexStore;

	#[test]
	fn statement_errors_classify_by_what_a_retry_can_fix() {
		assert_eq!(statement_error_kind(&turso::Error::Busy("database is locked".into())), TxnErrorKind::Retryable);
		assert_eq!(statement_error_kind(&turso::Error::BusySnapshot("stale".into())), TxnErrorKind::Retryable);
		assert_eq!(statement_error_kind(&turso::Error::Error("Write-write conflict".into())), TxnErrorKind::Retryable);
		assert_eq!(statement_error_kind(&turso::Error::Constraint("UNIQUE constraint failed: segment_index.aspect, segment_index.id".into())), TxnErrorKind::Conflict);
		assert_eq!(statement_error_kind(&turso::Error::Error("no such table: segment_index".into())), TxnErrorKind::Definite);
		assert_eq!(statement_error_kind(&turso::Error::IoError(std::io::ErrorKind::Other, "write")), TxnErrorKind::Definite);
	}

	/// Any COMMIT error may have left the transaction durable, except the validation
	/// conflicts Turso raises before it writes the log record.
	#[test]
	fn commit_errors_are_ambiguous_unless_validation_refused_the_commit() {
		assert_eq!(commit_error_kind(&turso::Error::IoError(std::io::ErrorKind::Other, "sync")), TxnErrorKind::Ambiguous);
		assert_eq!(commit_error_kind(&turso::Error::Error("I/O error: fsync failed".into())), TxnErrorKind::Ambiguous);
		assert_eq!(commit_error_kind(&turso::Error::Busy("database is locked".into())), TxnErrorKind::Ambiguous);
		assert_eq!(commit_error_kind(&turso::Error::Constraint("x".into())), TxnErrorKind::Ambiguous);
		assert_eq!(commit_error_kind(&turso::Error::BusySnapshot("Commit dependency aborted, rollback and retry the whole transaction".into())), TxnErrorKind::Retryable);
		assert_eq!(commit_error_kind(&turso::Error::Error("Write-write conflict".into())), TxnErrorKind::Retryable);
	}

	#[test]
	fn the_backoff_grows_and_is_bounded() {
		for retry in 1..=MAX_RETRIES {
			let base = 10_u64 << retry;
			let wait = backoff(retry).as_millis();
			assert!((u128::from(base)..=u128::from(base + base / 2)).contains(&wait), "retry {retry}: {wait} ms");
		}
		assert!(backoff(64) <= Duration::from_millis(480));
	}

	/// A write-once row for segment `id` of generation `gen` with frame CRC `crc`.
	fn row(id: u64, gen: u64, crc: Option<u32>) -> IndexRow {
		let base = i64::try_from(id).expect("small id") * 100;
		let ts: Vec<i64> = (0..4).map(|i| base + i * 10).collect();
		let vs: Vec<BigDecimal> = (0..4).map(BigDecimal::from).collect();
		let segment = Segment::build(&ts, &vs, TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		let desc = SegmentDescriptor::of_segment(id, format!("/root/segments/a~g{gen}~p{gen}.weftseg"), 64 + id, &segment);
		IndexRow { desc, gen, prec: Some(gen), frame_crc: crc, commit_epoch: Some(gen + 1) }
	}

	/// `aspect`'s rows, in id order.
	async fn rows_of(index: &SegmentIndexStore, aspect: &str) -> Vec<IndexRow> {
		index.rows(aspect).await.expect("reads the rows")
	}

	/// The aspect every test transaction writes.
	const ASPECT: &str = "a";

	fn insert(row: IndexRow) -> IndexOp {
		IndexOp::InsertNew { aspect: ASPECT.to_string(), row }
	}

	fn replace(expected: RowVersion, row: IndexRow) -> IndexOp {
		IndexOp::ReplaceExpected { aspect: ASPECT.to_string(), expected, row }
	}

	fn delete(expected: RowVersion) -> IndexOp {
		IndexOp::DeleteExpected { aspect: ASPECT.to_string(), expected }
	}

	/// The rows [`the_txn_under_test`] expects to find: `C` (id 2, gen 1, a CRC) and `D`
	/// (id 3, gen 1, no CRC yet), committed on their own first.
	fn base_rows() -> [IndexRow; 2] {
		[row(2, 1, Some(0xC0FF_EE00)), row(3, 1, None)]
	}

	/// Two inserts (`A`, `B`), then a replace of `C` by its next generation and a delete
	/// of `D`, each guarded by the version [`base_rows`] committed.
	fn the_txn_under_test() -> IndexTxn {
		let [c, d] = base_rows();
		IndexTxn::new(vec![insert(row(0, 1, Some(1))), insert(row(1, 1, Some(2))), replace(c.version(), row(2, 2, Some(0xC0FF_EE01))), delete(d.version())])
	}

	/// Risk 1(b) of the design, pinned: Turso's MVCC conflicts are per row, so concurrent
	/// transactions on different rows (64 of them, over four aspects) all commit on their
	/// first attempt, with no retry to hide a conflict. Writers of one row do conflict,
	/// and Turso reports many of those from COMMIT itself ("Write-write conflict", raised
	/// while it validates the commit, before it writes the log record). An `IndexTxn`
	/// never calls one ambiguous, so a race between two writers of a row cannot poison a
	/// store. Nor does it replay a legacy upsert that lost: each of the 16 upserters
	/// either committed on its first attempt or failed `Retryable` there, and the row
	/// holds the version of one that committed.
	#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
	async fn only_writers_of_one_row_conflict_and_never_ambiguously() {
		let dir = tempfile::TempDir::new().expect("tempdir");
		let path = dir.path().join("segment_index.db").to_string_lossy().into_owned();
		drop(SegmentIndexStore::open(&path).await.expect("creates the index"));
		let db = std::sync::Arc::new(turso::Builder::new_local(&path).build().await.expect("opens the database"));
		let upsert = |aspect: String, id: u64, version: usize| {
			let mut desc = row(id, 0, None).desc;
			desc.path = format!("/root/segments/{aspect}-{id}.v{version}.weftseg");
			IndexTxn::new(vec![IndexOp::Upsert { aspect, row: IndexRow::legacy(desc) }])
		};

		let distinct: Vec<_> = (0..64_u64)
			.map(|i| {
				let (db, txn) = (db.clone(), upsert(format!("a{}", i % 4), i, 0));
				tokio::spawn(async move { txn.run(&db).await })
			})
			.collect();
		for (i, task) in distinct.into_iter().enumerate() {
			let applied = task.await.expect("joins").unwrap_or_else(|e| panic!("transaction {i}, alone on its row, conflicted: {e}"));
			assert_eq!((applied.changes, applied.attempts), (vec![1], 1), "transaction {i}");
		}

		let one_row: Vec<_> = (0..16)
			.map(|version| {
				let (db, txn) = (db.clone(), upsert("a0".to_string(), 1000, version));
				tokio::spawn(async move { (version, txn.run(&db).await) })
			})
			.collect();
		let mut committed = Vec::new();
		for task in one_row {
			match task.await.expect("joins") {
				(version, Ok(applied)) => {
					assert_eq!(applied.attempts, 1, "upserter {version} committed on its first attempt, not by replaying itself over a winner");
					committed.push(format!("/root/segments/a0-1000.v{version}.weftseg"));
				}
				(version, Err(e)) => assert_eq!((e.kind, e.attempts), (TxnErrorKind::Retryable, 1), "upserter {version}: a conflict over one row is never ambiguous, and a legacy upsert is not retried: {e}"),
			}
		}
		let mut kept = db.connect().expect("connects").query("SELECT path FROM segment_index WHERE aspect = 'a0' AND id = 1000", ()).await.expect("reads");
		let kept = kept.next().await.expect("reads").expect("the contended row").get_value(0).expect("its path").as_text().cloned().expect("text");
		drop(db);
		assert!(!committed.is_empty(), "the writers of one row are serialised, not all refused");
		assert!(committed.contains(&kept), "the row holds the version of an upserter that reported success, {kept:?}");
	}

	/// The hazard the retry rule exists for, made deterministic. A winner holds an
	/// uncommitted change to row 5 while a loser's transaction runs, so the loser's first
	/// attempt conflicts; `may_retry` then commits the winner before letting a retry go
	/// ahead, which is exactly the window a retry on a fresh snapshot runs in.
	///
	/// - A legacy `Upsert` or `Delete` of the row is not retried: it fails `Retryable` on
	///   its first attempt, without asking, and the winner's row stands. (Retried, the
	///   upsert would have replaced the winner's row and the delete removed it, both
	///   reporting success.)
	/// - A guarded `ReplaceExpected` of the version the loser read is retried, and the
	///   retry, on a snapshot holding the winner's commit, fails its precondition: a
	///   `Conflict`, and the winner's row still stands.
	#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
	async fn a_retry_never_replays_an_unguarded_op_over_the_winners_commit() {
		let base = row(5, 1, Some(10));
		let winner = row(5, 2, Some(20));
		let losers = [("upsert", IndexOp::Upsert { aspect: ASPECT.to_string(), row: IndexRow::legacy(row(5, 0, None).desc) }, TxnErrorKind::Retryable, 1, 0), ("delete", IndexOp::Delete { aspect: ASPECT.to_string(), id: 5 }, TxnErrorKind::Retryable, 1, 0), ("guarded replace", replace(base.version(), row(5, 3, Some(30))), TxnErrorKind::Conflict, 2, 1)];
		for (what, op, kind, attempts, asked) in losers {
			let dir = tempfile::TempDir::new().expect("tempdir");
			let path = dir.path().join("segment_index.db").to_string_lossy().into_owned();
			let index = SegmentIndexStore::open(&path).await.expect("opens");
			index.apply(&IndexTxn::new(vec![insert(base.clone())])).await.expect("commits the base row");

			let winning = index.database().connect().expect("connects");
			winning.execute("BEGIN CONCURRENT", ()).await.expect("begins");
			apply(&winning, &replace(base.version(), winner.clone())).await.expect("the winner takes the row first");
			let (go, go_rx) = std::sync::mpsc::channel::<()>();
			let (done_tx, done) = std::sync::mpsc::channel::<()>();
			let committer = tokio::spawn(async move {
				tokio::task::spawn_blocking(move || go_rx.recv()).await.expect("joins").expect("told to commit");
				winning.execute("COMMIT", ()).await.expect("the winner commits");
				done_tx.send(()).expect("reports the commit");
			});
			let asks = std::sync::atomic::AtomicU32::new(0);
			let commit_the_winner = || {
				if asks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
					go.send(()).expect("starts the commit");
					done.recv().expect("waits for it");
				}
				true
			};
			let err = index.apply_while(&IndexTxn::new(vec![op]), commit_the_winner).await.expect_err(what);
			if asks.load(std::sync::atomic::Ordering::SeqCst) == 0 {
				go.send(()).expect("starts the commit");
			}
			committer.await.expect("the winner committed");
			let rows = rows_of(&index, ASPECT).await;
			drop(index);
			assert_eq!((err.kind, err.attempts, asks.into_inner()), (kind, attempts, asked), "{what}: {err}");
			assert_eq!(rows, vec![winner.clone()], "{what}: the winner's row stands");
		}
	}

	/// Every column of every op round-trips: the write-once columns of a row written with
	/// them, and the legacy shape (generation 0, nothing bound) of an upsert.
	#[tokio::test]
	async fn rows_round_trip_their_write_once_columns() {
		let index = SegmentIndexStore::open_in_memory().await.expect("opens");
		let fresh = row(0, 7, Some(u32::MAX));
		let legacy = IndexRow::legacy(row(1, 0, None).desc);
		let applied = index.apply(&IndexTxn::new(vec![insert(fresh.clone()), IndexOp::Upsert { aspect: ASPECT.to_string(), row: legacy.clone() }])).await.expect("commits");
		let rows = rows_of(&index, ASPECT).await;
		let pruned = index.prune_rows_by_time(ASPECT, 0, 30).await.expect("prunes");
		drop(index);
		assert_eq!(applied.changes, vec![1, 1]);
		assert_eq!(rows, vec![fresh.clone(), legacy.clone()]);
		assert_eq!((legacy.gen, legacy.prec, legacy.frame_crc, legacy.commit_epoch), (0, None, None, None));
		assert_eq!(pruned, vec![fresh], "a time prune returns whole rows too");
	}

	/// A precondition that no longer holds aborts the transaction with `Conflict`, and
	/// nothing it did before that op stays: not the insert ahead of it, not a change to
	/// the guarded row.
	#[tokio::test]
	async fn a_precondition_mismatch_rolls_back_every_op() {
		let index = SegmentIndexStore::open_in_memory().await.expect("opens");
		let [c, d] = base_rows();
		index.apply(&IndexTxn::new(vec![insert(c.clone()), insert(d.clone())])).await.expect("commits the base rows");
		let a = row(0, 1, Some(1));
		let stale = [("a CRC that changed", replace(RowVersion { frame_crc: Some(0xBAD), ..c.version() }, row(2, 2, None))), ("a generation that changed", replace(RowVersion { gen: 2, ..c.version() }, row(2, 2, None))), ("a CRC where the row has none", delete(RowVersion { frame_crc: Some(0), ..d.version() })), ("no CRC where the row has one", delete(RowVersion { frame_crc: None, ..c.version() })), ("a row that does not exist", delete(row(9, 1, None).version())), ("a key that is taken", insert(row(3, 5, None)))];
		for (what, op) in stale {
			let err = index.apply(&IndexTxn::new(vec![insert(a.clone()), op])).await.expect_err(what);
			assert_eq!((err.kind, err.op), (TxnErrorKind::Conflict, Some(1)), "{what}: {err}");
			assert_eq!(rows_of(&index, ASPECT).await, vec![c.clone(), d.clone()], "{what}: the insert ahead of the failed precondition rolled back and the base rows are untouched");
		}
		// With the versions the rows really have, the same ops commit.
		let applied = index.apply(&IndexTxn::new(vec![insert(a.clone()), replace(c.version(), row(2, 2, None)), delete(d.version())])).await.expect("commits");
		let rows = rows_of(&index, ASPECT).await;
		drop(index);
		assert_eq!(applied.changes, vec![1, 1, 1]);
		assert_eq!(rows, vec![a, row(2, 2, None)]);
	}

	/// Set only in the child process [`run_txn_child`] starts: the index database to open.
	const TXN_CHILD_INDEX: &str = "WEFT_TEST_TXN_CHILD_INDEX";

	/// The body the re-executed child of
	/// `an_index_txn_is_all_or_nothing_across_a_fault_between_its_ops` runs: commit
	/// [`the_txn_under_test`] against the index it is given, stopping after its second op
	/// at `S-rows-inserted`, where the parent's `WEFT_FAULT` aborts the process or
	/// injects an error. In a normal test run the variable is unset and this does
	/// nothing.
	#[tokio::test]
	async fn txn_child() {
		let Some(path) = std::env::var_os(TXN_CHILD_INDEX) else { return };
		crate::types::durable::fault::suppress_core_dump();
		let path = path.to_string_lossy().into_owned();
		let index = SegmentIndexStore::open(&path).await.expect("opens");
		let txn = the_txn_under_test().with_points(TxnPoints { after_op: Some((1, FaultPoint::SRowsInserted)), ..TxnPoints::NONE });
		// Returns only when the point is armed with `err` rather than `abort`.
		let err = index.apply(&txn).await.expect_err("the transaction stops at the armed fault");
		assert_eq!((err.kind, err.op), (TxnErrorKind::Definite, Some(1)), "{err}");
		assert!(err.message.contains("injected fault at S-rows-inserted"), "{err}");
		// The error is what a crash at this point looks like to the next open: drop the
		// store and reopen it.
		drop(index);
		let index = SegmentIndexStore::open(&path).await.expect("reopens");
		assert_eq!(rows_of(&index, ASPECT).await, base_rows().to_vec(), "the reopened index holds none of the transaction's ops");
	}

	/// Run [`txn_child`] against the index at `path` in a fresh process with
	/// `WEFT_FAULT=<spec>`. A child, because fault points are process-global: arming
	/// `S-rows-inserted` here would fail the fault module's own tests running alongside,
	/// and an abort must not take the test runner with it.
	async fn run_txn_child(path: &std::path::Path, spec: &str) -> std::process::Output {
		let exe = std::env::current_exe().expect("finds the test binary");
		tokio::process::Command::new(exe).args(["types::index_txn::tests::txn_child", "--exact", "--nocapture", "--test-threads=1"]).env(TXN_CHILD_INDEX, path).env(fault::FAULT_ENV, spec).output().await.expect("runs the child")
	}

	/// An [`IndexTxn`] commits all of its ops or none of them: a fault between two of its
	/// ops, as an error (after which the store is dropped and reopened) or as a process
	/// abort, leaves the index exactly as it was before the transaction began. The same
	/// transaction without the fault then applies all four ops.
	#[tokio::test]
	async fn an_index_txn_is_all_or_nothing_across_a_fault_between_its_ops() {
		for spec in ["S-rows-inserted:err", "S-rows-inserted:abort"] {
			let dir = tempfile::TempDir::new().expect("tempdir");
			let path = dir.path().join("segment_index.db");
			let index = SegmentIndexStore::open(&path.to_string_lossy()).await.expect("opens");
			index.apply(&IndexTxn::new(base_rows().into_iter().map(insert).collect())).await.expect("commits the base rows");
			drop(index);

			let out = run_txn_child(&path, spec).await;
			let stderr = String::from_utf8_lossy(&out.stderr);
			if spec.ends_with(":abort") {
				assert!(!out.status.success(), "{spec}: the child aborted: {out:?}");
				#[cfg(unix)]
				{
					use std::os::unix::process::ExitStatusExt;
					assert_eq!(out.status.signal(), Some(6), "{spec}: killed by SIGABRT: {out:?}");
				}
				assert!(stderr.contains("aborting at fault point S-rows-inserted"), "{spec}: {stderr}");
			} else {
				assert!(out.status.success(), "{spec}: the child saw the injected error and a clean reopen: {out:?}");
				assert!(String::from_utf8_lossy(&out.stdout).contains("1 passed"), "{spec}: the child really ran the transaction: {out:?}");
			}

			let index = SegmentIndexStore::open(&path.to_string_lossy()).await.expect("reopens");
			assert_eq!(rows_of(&index, ASPECT).await, base_rows().to_vec(), "{spec}: none of the transaction's ops survived the crash");
			let applied = index.apply(&the_txn_under_test()).await.expect("the same transaction commits without the fault");
			let rows = rows_of(&index, ASPECT).await;
			drop(index);
			assert_eq!(applied.changes, vec![1, 1, 1, 1], "{spec}");
			assert_eq!(rows, vec![row(0, 1, Some(1)), row(1, 1, Some(2)), row(2, 2, Some(0xC0FF_EE01))], "{spec}: all four ops applied together");
		}
	}
}
