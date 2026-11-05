//! The fsynced audit log (1.0 plan **SEC-9**, §11 decision 8).
//!
//! Decision 8 fixes the depth: an **append-only JSONL file**, **fsynced**
//! per record, holding an **attempt record and an outcome record for
//! every admin or mutating operation**, **fail-closed** — plus
//! `weft_audit` *events* from the producers that are not request/response
//! pairs: committed ingests (D-S14), authentication denials (SEC-6/SEC-8)
//! and daemon supervision (ROB-3). The cheaper events-only log the plan
//! prices at ~2 h was not the recommendation, and the plan preface adopts
//! recommendations the owner did not override, so this module is the
//! fsynced, fail-closed shape.
//!
//! ## Which routes are audited
//!
//! The sealed registry already says so: SEC-3's [`RouteSpec`] carries an
//! [`Audit`] requirement per route, and every mutating route — ingest,
//! declare, maintenance, backup — registered
//! [`Audit::AttemptOutcome`]. This module's middleware
//! ([`audit_route`](crate::audit::audit_route)) reads that same registry table (never a second copy)
//! and, for an audited route:
//!
//! 1. writes the **attempt** record and fsyncs it *before* the handler
//!    runs. If the write fails, the operation does not run: the answer is
//!    `503 audit_unavailable`, the code X-ERR reserved for exactly this.
//!    That is what fail-closed means here — an operation the server
//!    cannot prove it recorded is an operation it refuses to perform;
//! 2. runs the handler inside a task drawn from this log's **task
//!    tracker** (plan C9 names the tracker in the shutdown drain, and C12
//!    names the boundary: an audited route catches
//!    `JoinError::is_panic` *inside its own task*, rather than leaving
//!    the panic to ROB-3's catch layer, so the outcome record can say
//!    `panic` — see [`audit_route`](crate::audit::audit_route));
//! 3. writes the **outcome** record — status, result, duration — and
//!    fsyncs it. If *that* write fails the trail is broken from here on,
//!    so the answer is again `503 audit_unavailable`: the operation may
//!    have completed, but the server will not vouch for an outcome it
//!    could not record, and the envelope claims nothing either way.
//!
//! Reads, probes and stateless compute carry [`Audit::None`] and are
//! never recorded: the decision's wording is "per admin or mutating
//! operation", and a log that records every read prices its fsyncs
//! against the read path for no forensic gain.
//!
//! ## Events
//!
//! [`AuditLog::event`] writes the third record kind. Events are
//! best-effort *by the caller's situation*, not by this module's choice:
//! a committed ingest cannot be un-committed when its event write fails,
//! an auth denial is itself the safe answer, and a daemon panic has
//! already happened. [`AuditLog::emit_event`] therefore logs a failed
//! event write through `tracing` and moves on — but the failure still
//! poisons the log (see below), so the next *audited operation* refuses
//! fail-closed rather than running unrecorded behind a dead trail.
//!
//! ## The file
//!
//! One JSON object per line, UTF-8, `\n`-terminated, appended only:
//!
//! ```json
//! {"v":1,"seq":41,"ts_ms":1762137600000,"kind":"attempt","operation":"storage_aspect_declare","method":"POST","path":"/api/v1/storage/aspects","principal":"operator","role":"admin"}
//! {"v":1,"seq":42,"ts_ms":1762137600123,"kind":"outcome","audit_id":41,"operation":"storage_aspect_declare","status":201,"result":"ok","duration_ms":7}
//! {"v":1,"seq":43,"ts_ms":1762137600456,"kind":"event","event":"ingest","aspect":"price","format":"json","rows":120,"segments":1,"commit_epoch":9,"series_id":0,"keyed":true}
//! ```
//!
//! - `seq` is a per-file monotonic counter. Opening an existing file
//!   counts its lines and continues the sequence, so a restart never
//!   reuses a number; `audit_id` on an attempt is its own `seq`, and the
//!   outcome names it back.
//! - `ts_ms` is wall-clock milliseconds since the Unix epoch — an audit
//!   trail is read against other systems' clocks, so it carries the
//!   clock an operator can correlate, not a monotonic instant.
//! - A record never carries a credential: the principal is the key's
//!   *id* and role (SEC-6: ids and roles are not secrets), and no request
//!   body, header value or key material is written.
//!
//! A write is `write_all` of the whole line followed by `sync_data`, so
//! a record this module reports as written survives a process crash and
//! an OS crash alike (on a filesystem that honours fsync — the same
//! durability class the store itself claims). The first failed write
//! poisons the log: the failure is latched and every later write fails
//! with the same message, because a file that tore once is not trusted
//! to have healed itself. Opening a file whose last line is not
//! `\n`-terminated — the torn tail of a crash mid-write — is refused
//! for the same reason: appending after torn bytes would corrupt the
//! one line a forensic reader most needs, so the operator truncates the
//! partial line (it was never fsynced, so it was never reported as
//! recorded) and restarts.
//!
//! ## Where the file lives
//!
//! [`AUDIT_FILE_ENV`] (`WEFT_AUDIT_FILE`) names it directly. Unset, it
//! lives beside the data it audits: `<store root>/audit.jsonl` when a
//! segment store is configured, else `./weft-audit.jsonl`. See
//! [`resolve_path`](crate::audit::resolve_path). The binary opens it at startup and refuses to start
//! when it cannot — a server that cannot audit must not come up
//! unaudited by accident (the keys file's rule, SEC-6, applied here).
//! A router state with no audit log attached ([`AppState::new`]'s
//! default, the shape the stateless test routers run) passes audited
//! routes through unrecorded, exactly as a state with no keys passes
//! requests unauthenticated: the *binary* always attaches one.
//!
//! ## Shutdown (plan C9)
//!
//! The drain order is serve → **SEC-9 TaskTracker** →
//! `store.shutdown()` → runtime `shutdown_timeout(5 s)`. The tracker is
//! [`AuditLog::spawn`]'s: [`audit_route`](crate::audit::audit_route) runs every audited handler
//! through it, and [`AuditLog::close_and_drain`] — called by startup
//! after serving ends and the daemons stop, before the store's shutdown
//! — closes it to new work and waits for what is in flight, so no
//! outcome record is lost to a handler still running when the store
//! closes under it.
//!
//! [`AppState::new`]: crate::state::AppState::new
//! [`RouteSpec`]: crate::routes::RouteSpec

use std::{
	fs::{File, OpenOptions}, io::{BufReader, Write}, path::{Path, PathBuf}, sync::{Arc, Mutex, OnceLock}, time::{Duration, Instant, SystemTime, UNIX_EPOCH}
};

use axum::{
	body::Body, extract::{MatchedPath, State}, http::{Request, StatusCode}, middleware::Next, response::{IntoResponse, Response}
};
use serde_json::{json, Value};
use tokio::{sync::Notify, task::JoinHandle};

use crate::{
	auth::Principal, error::{ApiError, Code}, panic_catch, routes::{Audit, RouteTable}, state::AppState
};

/// Environment variable naming the audit log file directly (SEC-9).
pub const AUDIT_FILE_ENV: &str = "WEFT_AUDIT_FILE";

/// The audit file's name inside a store root, when
/// [`AUDIT_FILE_ENV`] does not name a path.
pub const AUDIT_FILE_NAME: &str = "audit.jsonl";

/// The audit file's path when neither [`AUDIT_FILE_ENV`] nor a store
/// root names a directory: a stateless server still audits its (refused,
/// store-less) mutating routes, beside wherever it was started.
pub const DEFAULT_AUDIT_PATH: &str = "weft-audit.jsonl";

/// The path the audit configuration two environment values describe:
/// `WEFT_AUDIT_FILE` wins; else the store root's [`AUDIT_FILE_NAME`];
/// else [`DEFAULT_AUDIT_PATH`].
#[must_use]
pub fn resolve_path(explicit: Option<&str>, store_root: Option<&str>) -> PathBuf {
	if let Some(path) = explicit.map(str::trim).filter(|value| !value.is_empty()) {
		return PathBuf::from(path);
	}
	if let Some(root) = store_root.map(str::trim).filter(|value| !value.is_empty()) {
		return Path::new(root).join(AUDIT_FILE_NAME);
	}
	PathBuf::from(DEFAULT_AUDIT_PATH)
}

/// The record format's version, carried on every line as `v` so a later
/// shape change is a version bump a reader can branch on, never a guess.
const RECORD_VERSION: u64 = 1;

/// Wall-clock milliseconds since the Unix epoch, the trail's timestamp.
fn now_ms() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

/// The mutable writer half: the file, the next sequence number, and the
/// latched first failure (see the module docs on poisoning).
#[derive(Debug)]
struct Writer {
	file: File,
	next_seq: u64,
	failure: Option<String>,
}

/// The task tracker of plan C9's drain order: a count of in-flight
/// audited-handler tasks plus a closed flag. Deliberately minimal — a
/// spawn that registers, a close that refuses new registrations, and a
/// drain that waits for the count to reach zero.
#[derive(Debug, Default)]
struct TaskTracker {
	open: Mutex<usize>,
	closed: Mutex<bool>,
	emptied: Notify,
}

impl TaskTracker {
	/// Whether the tracker has been closed by a drain.
	fn is_closed(&self) -> bool {
		*self.closed.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	/// Register one task about to be spawned. `false` once closed: the
	/// caller runs the work untracked instead of refusing it (a request
	/// racing the drain is still answered; the drain's contract is about
	/// work registered *before* the close).
	fn register(&self) -> bool {
		if self.is_closed() {
			return false;
		}
		*self.open.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
		true
	}

	/// Mark one tracked task finished, waking a drain at zero.
	fn complete(&self) {
		let now_empty = {
			let mut open = self.open.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
			*open = open.saturating_sub(1);
			*open == 0
		};
		if now_empty {
			self.emptied.notify_waiters();
		}
	}

	/// Close to new registrations, then wait until every registered task
	/// has completed. Called once, by startup's drain.
	async fn close_and_drain(&self) {
		*self.closed.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
		loop {
			// Register for the notification before re-checking the
			// count, so a completion between the check and the wait is
			// not missed.
			let notified = self.emptied.notified();
			tokio::pin!(notified);
			notified.as_mut().enable();
			if *self.open.lock().unwrap_or_else(std::sync::PoisonError::into_inner) == 0 {
				return;
			}
			notified.await;
		}
	}
}

/// The audit log: one append-only fsynced JSONL file, behind a lock so
/// concurrent audited routes can never interleave a line. Cheap to
/// clone — every clone is the same file and the same sequence.
#[derive(Debug, Clone)]
pub struct AuditLog {
	writer: Arc<Mutex<Writer>>,
	tracker: Arc<TaskTracker>,
	path: PathBuf,
}

impl AuditLog {
	/// Open (creating when absent) the audit file at `path`, continuing
	/// its sequence past the records it already holds.
	///
	/// # Errors
	///
	/// A message naming the path and the problem: the parent directory
	/// cannot be created, the file cannot be opened, an existing file
	/// cannot be read back, or its last line is torn (not
	/// `\n`-terminated — see the module docs). Opening is a startup act:
	/// the binary refuses to start on any of these rather than serve
	/// unaudited.
	pub fn open(path: &Path) -> Result<Self, String> {
		if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
			std::fs::create_dir_all(parent).map_err(|err| format!("cannot create the audit log's directory {}: {err}", parent.display()))?;
		}
		// The trail must be a regular file: a device or pipe named here
		// would either swallow records silently (`/dev/null` accepts
		// every write and sync) or never terminate a read-back
		// (`/dev/full` reads as an endless stream). Refusing is the
		// fail-closed answer to a configuration that cannot audit.
		if let Ok(metadata) = std::fs::metadata(path) {
			if !metadata.is_file() {
				return Err(format!("the audit log path {} is not a regular file: an audit trail must live in a file that keeps its records", path.display()));
			}
		}
		let existed = path.exists();
		let file = OpenOptions::new().create(true).append(true).read(true).open(path).map_err(|err| format!("cannot open the audit log at {}: {err}", path.display()))?;
		// Count the records already there: the sequence continues after
		// them, and a torn tail (a final line with no terminator) is a
		// refusal, not a repair — see the module docs. Counted in
		// bounded chunks, so even a corrupt file is read in constant
		// memory.
		let mut next_seq = 0u64;
		{
			use std::io::Read as _;
			let mut reader = BufReader::new(&file);
			let mut chunk = vec![0u8; 64 * 1024];
			let mut last_byte: Option<u8> = None;
			loop {
				let read = reader.read(&mut chunk).map_err(|err| format!("cannot read back the audit log at {}: {err}", path.display()))?;
				if read == 0 {
					break;
				}
				next_seq += chunk[..read].iter().fold(0_u64, |lines, byte| lines + u64::from(*byte == b'\n'));
				last_byte = chunk[..read].last().copied();
			}
			if let Some(byte) = last_byte {
				if byte != b'\n' {
					return Err(format!("the audit log at {} ends in a torn record (its last line has no terminator): that record was never fsynced, so truncate the partial final line and restart", path.display()));
				}
			}
		}
		if !existed {
			// A file the operator has never seen should be durable as a
			// directory entry too, not only once its first record syncs.
			if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
				if let Ok(dir) = File::open(parent) {
					let _ = dir.sync_all();
				}
			}
		}
		Ok(Self { writer: Arc::new(Mutex::new(Writer { file, next_seq, failure: None })), tracker: Arc::new(TaskTracker::default()), path: path.to_path_buf() })
	}

	/// The file this log writes.
	#[must_use]
	pub fn path(&self) -> &Path {
		&self.path
	}

	/// The next record's sequence number — what a test (or a `/ready`
	/// reader, later) observes as the trail's length so far.
	#[must_use]
	pub fn next_seq(&self) -> u64 {
		self.writer.lock().unwrap_or_else(std::sync::PoisonError::into_inner).next_seq
	}

	/// Append one record: stamp its `seq`, write the whole line, fsync.
	/// This is the only way bytes reach the file, so no record kind can
	/// skip the sync. Returns the record's sequence number.
	///
	/// # Errors
	///
	/// The latched first failure if the log is poisoned, or this write's
	/// own failure (which poisons it): a message naming the path and the
	/// io error.
	fn append(&self, record: &mut Value) -> Result<u64, String> {
		let mut writer = self.writer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		if let Some(failure) = &writer.failure {
			return Err(failure.clone());
		}
		let seq = writer.next_seq;
		record["seq"] = json!(seq);
		let mut line = serde_json::to_vec(record).map_err(|err| format!("cannot serialise an audit record: {err}"))?;
		line.push(b'\n');
		if let Err(err) = writer.file.write_all(&line).and_then(|()| writer.file.sync_data()) {
			let failure = format!("the audit log at {} could not be written ({err}): the audit trail is broken, and audited operations are refused until the server restarts", self.path.display());
			writer.failure = Some(failure.clone());
			return Err(failure);
		}
		writer.next_seq += 1;
		drop(writer);
		Ok(seq)
	}

	/// Write an **attempt** record for `operation` and fsync it, before
	/// the operation runs. Returns the attempt's id (its own `seq`),
	/// which the outcome record names back.
	///
	/// # Errors
	///
	/// Whatever `append` reports: the caller
	/// ([`audit_route`]) turns it into `503 audit_unavailable` and does
	/// not run the operation — fail-closed.
	pub fn attempt(&self, operation: &str, method: &str, path: &str, principal: Option<&Principal>) -> Result<u64, String> {
		let mut record = json!({
			"v": RECORD_VERSION,
			"ts_ms": now_ms(),
			"kind": "attempt",
			"operation": operation,
			"method": method,
			"path": path,
			"principal": principal.map(|principal| principal.id.clone()),
			"role": principal.map(|principal| principal.role.as_str()),
		});
		// The attempt's id is its own sequence number: the outcome
		// record carries it as `audit_id`, and a reader joins the pair
		// on attempt.seq == outcome.audit_id.
		self.append(&mut record)
	}

	/// Write the **outcome** record for attempt `audit_id`: the status
	/// the caller was (or is about to be) given, the result class, and
	/// how long the operation took.
	///
	/// # Errors
	///
	/// Whatever `append` reports.
	pub fn outcome(&self, audit_id: u64, operation: &str, status: StatusCode, result: &str, duration: Duration) -> Result<(), String> {
		let mut record = json!({
			"v": RECORD_VERSION,
			"ts_ms": now_ms(),
			"kind": "outcome",
			"audit_id": audit_id,
			"operation": operation,
			"status": status.as_u16(),
			"result": result,
			"duration_ms": u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
		});
		self.append(&mut record)?;
		Ok(())
	}

	/// Write an **event** record: a `weft_audit` fact that is not a
	/// request's attempt or outcome (a committed ingest, an auth denial,
	/// a daemon panic). `detail`'s fields are merged into the record
	/// beside `event`; the module docs show the ingest event's shape.
	///
	/// # Errors
	///
	/// Whatever `append` reports. Callers that cannot
	/// act on a failure use [`emit_event`](Self::emit_event) instead.
	pub fn event(&self, event: &str, detail: Value) -> Result<(), String> {
		let mut record = json!({
			"v": RECORD_VERSION,
			"ts_ms": now_ms(),
			"kind": "event",
			"event": event,
		});
		if let (Value::Object(target), Value::Object(fields)) = (&mut record, detail) {
			for (key, value) in fields {
				target.insert(key, value);
			}
		}
		self.append(&mut record)?;
		Ok(())
	}

	/// [`event`](Self::event) for the producers that cannot fail closed
	/// (see the module docs): a failure is logged through `tracing` —
	/// and has already poisoned the log, so the next audited *operation*
	/// still refuses.
	pub fn emit_event(&self, event: &str, detail: Value) {
		if let Err(err) = self.event(event, detail) {
			tracing::error!(target: "weft_audit", event, error = %err, "an audit event could not be recorded");
		}
	}

	/// Spawn `future` as one of this log's tracked tasks (plan C9):
	/// audited handlers run through here so the shutdown drain can wait
	/// for them. A spawn after the tracker closed runs untracked rather
	/// than refused — the drain is already under way, and the request
	/// racing it is still owed an answer.
	pub fn spawn<Fut>(&self, future: Fut) -> JoinHandle<Fut::Output>
	where
		Fut: std::future::Future + Send + 'static,
		Fut::Output: Send + 'static,
	{
		let tracker = Arc::clone(&self.tracker);
		let registered = tracker.register();
		tokio::spawn(async move {
			let output = future.await;
			if registered {
				tracker.complete();
			}
			output
		})
	}

	/// Close the tracker and wait for every tracked task to finish:
	/// startup's C9 drain step, between serving and the store's
	/// shutdown.
	pub async fn close_and_drain(&self) {
		self.tracker.close_and_drain().await;
	}
}

/// The sealed registry's table, built once for the audit layer's spec
/// lookups — the same move [`crate::auth`] makes: the registry is
/// frozen, so the table this layer consults is the table the gate
/// sealed.
fn route_table() -> &'static RouteTable {
	static TABLE: OnceLock<Arc<RouteTable>> = OnceLock::new();
	let table: &'static Arc<RouteTable> = TABLE.get_or_init(|| crate::routes::build_registry().table());
	table
}

/// The `503 audit_unavailable` refusal: the envelope X-ERR's code table
/// pairs with the code whose sentence is "the server could not record
/// the request in its audit log".
fn audit_unavailable(detail: String) -> Response {
	ApiError::new(Code::AuditUnavailable, detail).into_response()
}

/// axum middleware: the audit pair around every audited route, as the
/// module docs describe.
///
/// Installed by [`crate::app_with_state`] between
/// the auth layer (outside it, so a denial is an *event*, never an
/// attempt, and the attempt can name the authenticated principal) and
/// the registry's gate (inside it). With no audit log in the state it
/// is a pass-through; the binary always attaches one.
pub async fn audit_route(State(state): State<AppState>, request: Request<Body>, next: Next) -> Response {
	let Some(log) = state.audit_log().cloned() else {
		return next.run(request).await;
	};
	let Some(matched) = request.extensions().get::<MatchedPath>().map(|matched| matched.as_str().to_string()) else {
		return next.run(request).await;
	};
	let Some(entry) = route_table().lookup(request.method(), &matched) else {
		return next.run(request).await;
	};
	if entry.spec.audit != Audit::AttemptOutcome {
		return next.run(request).await;
	}
	let operation = entry.spec.operation_id;
	let scope = entry.spec.scope;
	let principal = request.extensions().get::<Principal>().cloned();
	let method = request.method().as_str().to_string();
	// Fail-closed, step one: the attempt is durable before the handler
	// runs, or the handler does not run.
	let audit_id = match log.attempt(operation, &method, &matched, principal.as_ref()) {
		Ok(audit_id) => audit_id,
		Err(err) => return audit_unavailable(err),
	};
	let started = Instant::now();
	// C12: the handler runs in its own tracked task, so a panic comes
	// back as a JoinError this layer can record — not as an unwind
	// through ROB-3's catch layer, which could not write the outcome.
	let handle = log.spawn(next.run(request));
	let (response, result) = match handle.await {
		Ok(response) => {
			let result = if response.status().is_success() { "ok" } else { "error" };
			(response, result)
		}
		Err(join) if join.is_panic() => {
			let message = weftdb::exec::panic_message(&*join.into_panic());
			tracing::error!(operation, panic = %message, "audited handler panicked; recorded as its audit outcome (SEC-9, C12)");
			// The wire answer matches what ROB-3's catch layer would
			// have produced without this boundary: ingest panics map by
			// phase (with no phase readable here, D-S14's pre-commit
			// arm), everything else is the redacted `internal`.
			let response = if scope == crate::routes::Scope::Ingest { panic_catch::map_ingest_panic(None, &message).into_response() } else { ApiError::internal(format!("handler panicked: {message}")).into_response() };
			(response, "panic")
		}
		Err(join) => {
			tracing::error!(operation, error = %join, "audited handler task ended without an outcome; recorded as cancelled (SEC-9)");
			(ApiError::new(Code::OutcomeUnknown, "the audited operation's task ended without an outcome; whether it completed is unknown".to_string()).into_response(), "cancelled")
		}
	};
	// Fail-closed, step two: the outcome is durable, or the answer is
	// the audit refusal (which claims nothing about the operation).
	if let Err(err) = log.outcome(audit_id, operation, response.status(), result, started.elapsed()) {
		return audit_unavailable(err);
	}
	response
}

#[cfg(test)]
mod tests {
	use std::io::Read as _;

	use super::*;

	fn read_records(path: &Path) -> Vec<Value> {
		let mut text = String::new();
		File::open(path).expect("open audit file").read_to_string(&mut text).expect("read audit file");
		text.lines().map(|line| serde_json::from_str(line).expect("each line is a JSON record")).collect()
	}

	#[test]
	fn resolve_path_prefers_the_explicit_setting_then_the_store_root() {
		assert_eq!(resolve_path(Some("/var/log/a.jsonl"), Some("/store")), PathBuf::from("/var/log/a.jsonl"));
		assert_eq!(resolve_path(None, Some("/store")), PathBuf::from("/store/audit.jsonl"));
		assert_eq!(resolve_path(Some("  "), None), PathBuf::from(DEFAULT_AUDIT_PATH));
	}

	#[test]
	fn attempt_outcome_and_event_land_as_fsynced_jsonl_in_sequence() {
		let dir = tempfile::tempdir().expect("tempdir");
		let path = dir.path().join("nested/audit.jsonl");
		let log = AuditLog::open(&path).expect("open creates the parent and the file");
		let principal = Principal { id: "operator".to_string(), role: crate::routes::Role::Admin };
		let audit_id = log.attempt("storage_aspect_declare", "POST", "/api/v1/storage/aspects", Some(&principal)).expect("attempt");
		assert_eq!(audit_id, 0);
		log.outcome(audit_id, "storage_aspect_declare", StatusCode::CREATED, "ok", Duration::from_millis(3)).expect("outcome");
		log.event("ingest", json!({"aspect": "price", "rows": 2})).expect("event");
		let records = read_records(&path);
		assert_eq!(records.len(), 3);
		assert_eq!(records[0]["kind"], "attempt");
		assert_eq!(records[0]["seq"], 0);
		assert_eq!(records[0]["principal"], "operator");
		assert_eq!(records[0]["role"], "admin");
		assert_eq!(records[1]["kind"], "outcome");
		assert_eq!(records[1]["audit_id"], 0);
		assert_eq!(records[1]["status"], 201);
		assert_eq!(records[2]["kind"], "event");
		assert_eq!(records[2]["event"], "ingest");
		assert_eq!(records[2]["seq"], 2);
		assert_eq!(log.next_seq(), 3);
	}

	#[test]
	fn reopening_continues_the_sequence() {
		let dir = tempfile::tempdir().expect("tempdir");
		let path = dir.path().join("audit.jsonl");
		let log = AuditLog::open(&path).expect("open");
		log.event("daemon_panic", json!({"daemon": "backup"})).expect("event");
		drop(log);
		let log = AuditLog::open(&path).expect("reopen");
		assert_eq!(log.next_seq(), 1, "the sequence continues past the records on disk");
		let audit_id = log.attempt("storage_backup", "POST", "/api/v1/storage/backup", None).expect("attempt");
		assert_eq!(audit_id, 1);
	}

	#[test]
	fn a_torn_tail_is_refused_at_open() {
		let dir = tempfile::tempdir().expect("tempdir");
		let path = dir.path().join("audit.jsonl");
		std::fs::write(&path, "{\"v\":1}\n{\"v\":1,\"kind\":\"att").expect("write torn file");
		let err = AuditLog::open(&path).expect_err("a torn tail refuses the open");
		assert!(err.contains("torn record"), "{err}");
	}

	/// A non-regular file is refused at open: `/dev/null` would accept
	/// (and silently discard) every record, which is an audit trail in
	/// name only.
	#[cfg(unix)]
	#[test]
	fn a_device_is_not_an_audit_file() {
		let err = AuditLog::open(Path::new("/dev/null")).expect_err("a device refuses the open");
		assert!(err.contains("not a regular file"), "{err}");
	}

	/// A write that fails (here: the file-size rlimit set to zero in a
	/// child process, the stand-in for a disk that fills under a running
	/// server) poisons the log, and the failure sticks: every later
	/// write reports the one latched failure. Runs in a child because
	/// an rlimit is process-wide (the `WEFT_TEST_*` child pattern the
	/// crash matrices use).
	#[cfg(unix)]
	#[test]
	fn a_failed_write_poisons_the_log_and_sticks() {
		const CHILD_ENV: &str = "WEFT_TEST_AUDIT_FULL_CHILD";
		if std::env::var_os(CHILD_ENV).is_some() {
			// The child: refuse the SIGXFSZ the rlimit would kill us
			// with, cap file sizes at zero, and watch the writes fail.
			// Only the soft limit is capped (the hard limit stays, so
			// the cap can be restored): the harness writes this test's
			// own result line to stdout when the test returns, and a
			// cap still in force then — whenever stdout is a file
			// rather than a pipe — would eat that write and turn a
			// passing child into an unexplained exit 101.
			let mut saved = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
			unsafe {
				libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
				assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &raw mut saved), 0, "the rlimit is read");
				let limit = libc::rlimit { rlim_cur: 0, rlim_max: saved.rlim_max };
				assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &raw const limit), 0, "the rlimit is set");
			}
			let dir = tempfile::tempdir().expect("tempdir");
			let log = AuditLog::open(&dir.path().join("audit.jsonl")).expect("the file opens; only its writes are capped");
			let first = log.attempt("storage_backup", "POST", "/api/v1/storage/backup", None).expect_err("the write fails");
			assert!(first.contains("could not be written"), "{first}");
			let second = log.event("ingest", json!({})).expect_err("the failure is latched");
			assert_eq!(first, second, "a poisoned log reports the one latched failure");
			unsafe {
				assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &raw const saved), 0, "the rlimit is restored");
			}
			return;
		}
		let exe = std::env::current_exe().expect("the test binary's path");
		let status = std::process::Command::new(exe).args(["--exact", "audit::tests::a_failed_write_poisons_the_log_and_sticks", "--nocapture"]).env(CHILD_ENV, "1").status().expect("the child runs");
		assert!(status.success(), "the child verdict: {status}");
	}

	#[tokio::test]
	async fn the_tracker_drains_what_it_spawned() {
		let dir = tempfile::tempdir().expect("tempdir");
		let log = AuditLog::open(&dir.path().join("audit.jsonl")).expect("open");
		let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
		let handle = log.spawn(async move {
			let _ = release_rx.await;
			7
		});
		let drain = log.clone();
		let draining = tokio::spawn(async move { drain.close_and_drain().await });
		tokio::time::sleep(Duration::from_millis(20)).await;
		assert!(!draining.is_finished(), "the drain waits for the tracked task");
		release_tx.send(()).expect("release");
		assert_eq!(handle.await.expect("task"), 7);
		tokio::time::timeout(Duration::from_secs(5), draining).await.expect("the drain finishes").expect("drain task");
	}
}
