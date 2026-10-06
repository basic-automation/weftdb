//! Named crash points for the crash-consistency test matrix (design section 11).
//!
//! The store marks each step of a commit protocol with `fault::hit(point).await?`.
//! In a normal build that call is a `const fn` returning a zero-sized future that is
//! always ready with `Ok(())`: it reads no environment variable, takes no lock and
//! compiles to nothing, so a stray `WEFT_FAULT` can never abort a production server.
//! Under `cfg(test)` or the `fault-injection` feature, a point can be armed with a
//! `FaultAction`:
//!
//! - **`ReturnErr`**: `hit` returns an injected error. The caller unwinds as on a real
//!   I/O error, and the test then drops and reopens the store, which is exactly a
//!   process crash at that point.
//! - **`Abort`**: `std::process::abort()`. Tests re-exec their own binary with
//!   `WEFT_FAULT=<point>:abort` so the parent survives to inspect the store.
//! - **`Pause`**: park the hitting task on a `Notify`, to force an interleaving.
//!
//! Code that is already on a blocking thread (the steps `RealFs` runs on one file
//! handle inside `spawn_blocking`) uses [`hit_blocking`] instead, which acts the same
//! way and parks the thread for a `Pause`.

use std::{fmt, str::FromStr};

#[cfg(any(test, feature = "fault-injection"))]
pub use self::active::{arm, disarm, hit, hit_blocking, hits, injected_point, reached, Armed, FaultAction, InjectedFault, FAULT_ENV};
#[cfg(not(any(test, feature = "fault-injection")))]
pub use self::inert::{hit, hit_blocking, Inert};

/// Whether fault points can do anything in this build.
pub const ENABLED: bool = cfg!(any(test, feature = "fault-injection"));

macro_rules! fault_points {
	($($(#[doc = $doc:literal])* $point:ident = $name:literal,)*) => {
		/// A step of a commit protocol where the crash tests can inject a fault.
		///
		/// The names are the ones design section 11 uses; [`Display`](fmt::Display) and
		/// [`FromStr`] convert to and from them (`WEFT_FAULT` uses them). Points that
		/// repeat within one operation carry the repetition: `B-vacuum(2)` is the third
		/// database a backup vacuums.
		#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
		#[non_exhaustive]
		pub enum FaultPoint {
			$($(#[doc = $doc])* $point,)*
			/// `R<n>`: recovery step `R<n>` (R1-R13, design section 6) is about to run.
			Recovery(u8),
			/// `B-vacuum(k)`: the backup has vacuumed its `k`-th database.
			BVacuum(u32),
			/// `restore-copied(k)`: the restore has copied its `k`-th file.
			RestoreCopied(u32),
			/// `L-chunk(k)`: legacy ingest has committed its `k`-th chunk.
			LChunk(u32),
		}

		impl FaultPoint {
			/// Every point without a parameter, with its name.
			const NAMED: &'static [(Self, &'static str)] = &[$((Self::$point, $name),)*];
		}
	};
}

fault_points! {
	/// `S-encoded`: a seal has encoded its frames; nothing is on disk yet.
	SEncoded = "S-encoded",
	/// `S-frame-created`: a seal frame's name exists, its bytes are not written.
	SFrameCreated = "S-frame-created",
	/// `S-frame-written`: a seal frame is written but not fsynced.
	SFrameWritten = "S-frame-written",
	/// `S-frame-synced`: a seal frame is fsynced; its directory entry may not be.
	SFrameSynced = "S-frame-synced",
	/// `S-dir-synced`: the seal's directory entries are durable; nothing is committed.
	SDirSynced = "S-dir-synced",
	/// `S-txn-begun`: the seal's index transaction has begun.
	STxnBegun = "S-txn-begun",
	/// `S-rows-inserted`: the seal's rows are inserted, not yet committed.
	SRowsInserted = "S-rows-inserted",
	/// `S-commit-phantom`: COMMIT executed but the caller sees an error (ambiguous).
	SCommitPhantom = "S-commit-phantom",
	/// `S-committed-unacked`: the seal committed and the client was not answered.
	SCommittedUnacked = "S-committed-unacked",
	/// `S-mid-sidecar`: a post-ack sidecar is half materialised.
	SMidSidecar = "S-mid-sidecar",
	/// `S-between-frames`: a multi-frame ingest has written some of its frames.
	SBetweenFrames = "S-between-frames",
	/// `M-planned`: a maintenance operation has planned its outputs.
	MPlanned = "M-planned",
	/// `M-pending-committed`: the outputs' pending journal rows are committed.
	MPendingCommitted = "M-pending-committed",
	/// `M-output-written`: an output frame is written but not fsynced.
	MOutputWritten = "M-output-written",
	/// `M-output-synced`: an output frame is fsynced.
	MOutputSynced = "M-output-synced",
	/// `M-dir-synced`: the outputs' directory entries are durable.
	MDirSynced = "M-dir-synced",
	/// `M-swap-begun`: the swap transaction has begun.
	MSwapBegun = "M-swap-begun",
	/// `M-swap-phantom`: the swap COMMIT executed but the caller sees an error.
	MSwapPhantom = "M-swap-phantom",
	/// `M-swapped`: the swap is committed; nothing has been reaped.
	MSwapped = "M-swapped",
	/// `G-unlinked`: the reaper has unlinked retired frames.
	GUnlinked = "G-unlinked",
	/// `G-dir-synced`: the reaper's unlinks are durable.
	GDirSynced = "G-dir-synced",
	/// `G-journal-deleted`: the reaper has deleted the processed journal rows.
	GJournalDeleted = "G-journal-deleted",
	/// `B-partial-created`: the backup's `.partial-*` directory exists.
	BPartialCreated = "B-partial-created",
	/// `B-links`: the backup has hard-linked its frames.
	BLinks = "B-links",
	/// `B-manifest`: the backup's manifest is written.
	BManifest = "B-manifest",
	/// `B-renamed`: the backup directory has its final name.
	BRenamed = "B-renamed",
	/// `prune-renamed`: a pruned backup was renamed to `.deleting-*`.
	PruneRenamed = "prune-renamed",
	/// `prune-removed`: a pruned backup's files are removed.
	PruneRemoved = "prune-removed",
	/// `restore-renamed`: the restored files have their final names.
	RestoreRenamed = "restore-renamed",
	/// `L-enqueued`: legacy ingest has written its write-ahead queue entry.
	LEnqueued = "L-enqueued",
	/// `L-sealed`: a seal-backed legacy ingest has committed.
	LSealed = "L-sealed",
	/// `L-consumer-batches`: the queue consumer has inserted batches, not dequeued.
	LConsumerBatches = "L-consumer-batches",
	/// `L-new-created`: `Database::new` has built its `.creating-*` directory.
	LNewCreated = "L-new-created",
	/// `L-new-renamed`: `Database::new` has renamed the database into place.
	LNewRenamed = "L-new-renamed",
	/// `O-scope-database-inserted`: an open's scope registration has inserted the
	/// database row, not yet the subject row.
	OScopeDatabaseInserted = "O-scope-database-inserted",
}

impl FaultPoint {
	/// The recovery steps an `R<n>` point can name (design section 6).
	pub const RECOVERY_STEPS: std::ops::RangeInclusive<u8> = 1..=13;
}

impl fmt::Display for FaultPoint {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Recovery(n) => write!(f, "R{n}"),
			Self::BVacuum(k) => write!(f, "B-vacuum({k})"),
			Self::RestoreCopied(k) => write!(f, "restore-copied({k})"),
			Self::LChunk(k) => write!(f, "L-chunk({k})"),
			named => f.write_str(Self::NAMED.iter().find(|(point, _)| point == named).map_or("unnamed-fault-point", |(_, name)| name)),
		}
	}
}

/// A string that names no [`FaultPoint`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownFaultPoint(pub String);

impl fmt::Display for UnknownFaultPoint {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "unknown fault point {:?}", self.0)
	}
}

impl std::error::Error for UnknownFaultPoint {}

impl FromStr for FaultPoint {
	type Err = UnknownFaultPoint;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		if let Some((point, _)) = Self::NAMED.iter().find(|(_, name)| *name == s) {
			return Ok(*point);
		}
		let unknown = || UnknownFaultPoint(s.to_owned());
		let indexed = |prefix: &str| s.strip_prefix(prefix)?.strip_suffix(')')?.parse::<u32>().ok();
		let point = if let Some(n) = s.strip_prefix('R').and_then(|n| n.parse::<u8>().ok()) {
			if !Self::RECOVERY_STEPS.contains(&n) {
				return Err(unknown());
			}
			Self::Recovery(n)
		} else if let Some(k) = indexed("B-vacuum(") {
			Self::BVacuum(k)
		} else if let Some(k) = indexed("restore-copied(") {
			Self::RestoreCopied(k)
		} else if let Some(k) = indexed("L-chunk(") {
			Self::LChunk(k)
		} else {
			return Err(unknown());
		};
		// Only the canonical spelling: integer parsing also takes `R04`, `R+4` and
		// `B-vacuum(+1)`, which would arm a point under a name nothing displays. That is
		// the silent miss a malformed `WEFT_FAULT` must never be.
		if point.to_string() != s {
			return Err(unknown());
		}
		Ok(point)
	}
}

/// Keep a test child's deliberate abort from dumping core. Every test that re-executes
/// itself to abort at a fault point calls this in the child first: under
/// systemd-coredump each run would otherwise store a core of this large binary and
/// raise a desktop "process crashed" notice.
#[cfg(test)]
pub(crate) fn suppress_core_dump() {
	#[cfg(unix)]
	{
		let none = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
		// SAFETY: setrlimit only reads the struct, for the duration of the call.
		unsafe { libc::setrlimit(libc::RLIMIT_CORE, &raw const none) };
	}
	// A pipe `core_pattern` (systemd-coredump) ignores RLIMIT_CORE, but the kernel
	// never dumps a process that is not dumpable.
	#[cfg(target_os = "linux")]
	{
		let not_dumpable: libc::c_ulong = 0;
		// SAFETY: PR_SET_DUMPABLE takes one integer and changes only this process's
		// dumpable flag.
		unsafe { libc::prctl(libc::PR_SET_DUMPABLE, not_dumpable) };
	}
}

/// The production build: every fault point is inert.
#[cfg(not(any(test, feature = "fault-injection")))]
mod inert {
	use std::{
		future::Future, io, pin::Pin, task::{Context, Poll}
	};

	use super::FaultPoint;

	/// The future [`hit`] returns when fault injection is compiled out: zero-sized and
	/// ready with `Ok(())` on its first poll.
	#[derive(Debug, Clone, Copy, Default)]
	#[must_use = "a fault point is a step only when awaited"]
	pub struct Inert;

	impl Future for Inert {
		type Output = io::Result<()>;

		fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
			Poll::Ready(Ok(()))
		}
	}

	/// Mark `point` in a commit protocol. Fault injection is compiled out of this
	/// build, so this is a `const fn` that ignores `point` and returns a zero-sized
	/// future that is already complete.
	#[inline]
	pub const fn hit(point: FaultPoint) -> Inert {
		let _ = point;
		Inert
	}

	/// Mark `point` from a blocking thread. Fault injection is compiled out of this
	/// build, so this is a `const fn` that ignores `point` and returns `Ok(())`.
	///
	/// # Errors
	///
	/// Never, in this build.
	#[inline]
	pub const fn hit_blocking(point: FaultPoint) -> io::Result<()> {
		let _ = point;
		Ok(())
	}
}

/// The test harness build (`cfg(test)` or `--features fault-injection`).
#[cfg(any(test, feature = "fault-injection"))]
mod active {
	use std::{
		collections::HashMap, ffi::OsString, fmt, io, sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock, PoisonError}
	};

	use tokio::sync::Notify;

	use super::FaultPoint;

	/// The environment variable a re-executed test process reads its faults from.
	///
	/// It holds a comma-separated list of `<point>:abort` or `<point>:err`, for
	/// example `WEFT_FAULT=S-frame-synced:abort`, and is read once, at the first hit.
	pub const FAULT_ENV: &str = "WEFT_FAULT";

	/// What an armed fault point does when it is hit.
	#[derive(Debug, Clone)]
	pub enum FaultAction {
		/// Return an [`InjectedFault`] error from [`hit`].
		ReturnErr,
		/// Abort the process on the spot: a process crash, after which the OS still
		/// holds every write, synced or not.
		Abort,
		/// Wait until the test calls `notify_one` on the `Notify`. A permit stored
		/// before the hit lets the task straight through, so a test that must know the
		/// task is parked waits for [`reached`] first.
		Pause(Arc<Notify>),
	}

	/// The error [`FaultAction::ReturnErr`] makes [`hit`] return, wrapped in an
	/// `io::Error` of kind `Other`; see [`injected_point`].
	#[derive(Debug, Clone, Copy, PartialEq, Eq)]
	pub struct InjectedFault(pub FaultPoint);

	impl fmt::Display for InjectedFault {
		fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
			write!(f, "injected fault at {}", self.0)
		}
	}

	impl std::error::Error for InjectedFault {}

	/// The point an error was injected at, if `err` came from [`hit`].
	#[must_use]
	pub fn injected_point(err: &io::Error) -> Option<FaultPoint> {
		err.get_ref()?.downcast_ref::<InjectedFault>().map(|fault| fault.0)
	}

	#[derive(Default)]
	struct Registry {
		armed: HashMap<FaultPoint, FaultAction>,
		hits: HashMap<FaultPoint, u64>,
	}

	/// The armed points are process-global: a fault must reach code running in any
	/// task, including detached ones. Tests that arm a point use one no other
	/// concurrently running test passes, or run serially.
	static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(Mutex::default);
	/// Woken on every hit, for [`reached`].
	static HIT: LazyLock<Notify> = LazyLock::new(Notify::new);
	static FROM_ENV: OnceLock<HashMap<FaultPoint, FaultAction>> = OnceLock::new();

	fn registry() -> MutexGuard<'static, Registry> {
		REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Arm `point` with `action` until the returned guard drops (or [`disarm`]).
	#[must_use = "the point is disarmed when the guard drops"]
	pub fn arm(point: FaultPoint, action: FaultAction) -> Armed {
		registry().armed.insert(point, action);
		Armed(point)
	}

	/// Disarm `point`.
	pub fn disarm(point: FaultPoint) {
		registry().armed.remove(&point);
	}

	/// Disarms its point on drop.
	#[derive(Debug)]
	pub struct Armed(FaultPoint);

	impl Drop for Armed {
		fn drop(&mut self) {
			disarm(self.0);
		}
	}

	/// How many times `point` has been hit in this process, armed or not.
	#[must_use]
	pub fn hits(point: FaultPoint) -> u64 {
		registry().hits.get(&point).copied().unwrap_or(0)
	}

	/// Wait until `point` has been hit at least `n` times.
	pub async fn reached(point: FaultPoint, n: u64) {
		loop {
			let hit = HIT.notified();
			tokio::pin!(hit);
			hit.as_mut().enable();
			if hits(point) >= n {
				return;
			}
			hit.await;
		}
	}

	/// Mark `point` in a commit protocol and perform its armed action, if any. A point
	/// armed in-process takes precedence over one named in `WEFT_FAULT`.
	///
	/// # Errors
	///
	/// An [`InjectedFault`] when the point is armed with [`FaultAction::ReturnErr`].
	///
	/// # Panics
	///
	/// If `WEFT_FAULT` is malformed or not UTF-8: a crash test that silently injected
	/// nothing would pass for the wrong reason.
	pub async fn hit(point: FaultPoint) -> io::Result<()> {
		match record(point) {
			None => Ok(()),
			Some(FaultAction::ReturnErr) => Err(io::Error::other(InjectedFault(point))),
			Some(FaultAction::Abort) => abort_at(point),
			Some(FaultAction::Pause(resume)) => {
				resume.notified().await;
				Ok(())
			}
		}
	}

	/// [`hit`] for code already running on a blocking thread, such as the steps
	/// `RealFs` performs on one file handle inside `spawn_blocking`. A `Pause` parks
	/// the thread, so never call this from an async task.
	///
	/// # Errors
	///
	/// An [`InjectedFault`] when the point is armed with [`FaultAction::ReturnErr`].
	///
	/// # Panics
	///
	/// If `WEFT_FAULT` is malformed or not UTF-8, as for [`hit`].
	pub fn hit_blocking(point: FaultPoint) -> io::Result<()> {
		match record(point) {
			None => Ok(()),
			Some(FaultAction::ReturnErr) => Err(io::Error::other(InjectedFault(point))),
			Some(FaultAction::Abort) => abort_at(point),
			Some(FaultAction::Pause(resume)) => {
				// `Notify` is executor-agnostic, so a thread can wait on it directly.
				futures::executor::block_on(resume.notified());
				Ok(())
			}
		}
	}

	/// Count a hit on `point`, wake [`reached`], and return the action armed for it.
	fn record(point: FaultPoint) -> Option<FaultAction> {
		let armed = {
			let mut registry = registry();
			*registry.hits.entry(point).or_insert(0) += 1;
			registry.armed.get(&point).cloned()
		};
		HIT.notify_waiters();
		armed.or_else(|| from_env().get(&point).cloned())
	}

	fn abort_at(point: FaultPoint) -> ! {
		eprintln!("{FAULT_ENV}: aborting at fault point {point}");
		std::process::abort()
	}

	fn from_env() -> &'static HashMap<FaultPoint, FaultAction> {
		FROM_ENV.get_or_init(|| faults_from_var(std::env::var_os(FAULT_ENV)))
	}

	/// The faults a `WEFT_FAULT` value arms. Unset arms nothing. A value that is not
	/// UTF-8, or does not parse, panics instead of being ignored.
	// The escaped (Debug) form shows exactly which bytes are not UTF-8.
	#[allow(clippy::unnecessary_debug_formatting)]
	pub(super) fn faults_from_var(value: Option<OsString>) -> HashMap<FaultPoint, FaultAction> {
		let Some(value) = value else { return HashMap::new() };
		let spec = value.into_string().unwrap_or_else(|raw| panic!("{FAULT_ENV}={raw:?} is not valid UTF-8"));
		parse_env(&spec).unwrap_or_else(|e| panic!("{FAULT_ENV}={spec:?}: {e}"))
	}

	pub(super) fn parse_env(spec: &str) -> Result<HashMap<FaultPoint, FaultAction>, String> {
		let mut faults = HashMap::new();
		for item in spec.split(',').map(str::trim).filter(|item| !item.is_empty()) {
			let (point, action) = item.rsplit_once(':').ok_or_else(|| format!("{item:?} is not <point>:<abort|err>"))?;
			let point = point.parse::<FaultPoint>().map_err(|e| e.to_string())?;
			let action = match action {
				"abort" => FaultAction::Abort,
				"err" => FaultAction::ReturnErr,
				other => return Err(format!("unknown action {other:?} for {point} (expected abort or err)")),
			};
			faults.insert(point, action);
		}
		Ok(faults)
	}
}

#[cfg(test)]
mod tests {
	use std::{
		collections::HashSet, io, sync::{
			atomic::{AtomicBool, Ordering}, Arc
		}, time::Duration
	};

	use tokio::sync::Notify;

	use super::*;

	/// Set only in the child process `abort_is_driven_by_the_env_var` spawns.
	const CHILD_ENV: &str = "WEFT_FAULT_TEST_CHILD";

	#[test]
	fn names_round_trip_and_are_unique() {
		let mut names = HashSet::new();
		let parameterized = [FaultPoint::Recovery(1), FaultPoint::Recovery(13), FaultPoint::BVacuum(0), FaultPoint::RestoreCopied(7), FaultPoint::LChunk(42)];
		for point in FaultPoint::NAMED.iter().map(|(point, _)| *point).chain(parameterized) {
			let name = point.to_string();
			assert_eq!(name.parse::<FaultPoint>(), Ok(point), "{name}");
			assert!(names.insert(name.clone()), "{name} is used twice");
		}
		assert_eq!(FaultPoint::SFrameSynced.to_string(), "S-frame-synced");
		assert_eq!(FaultPoint::BVacuum(2).to_string(), "B-vacuum(2)");
		assert_eq!(FaultPoint::Recovery(4).to_string(), "R4");
		for bad in ["", "S-frame", "R", "Rx", "B-vacuum()", "B-vacuum(2", "L-chunk(-1)"] {
			assert!(bad.parse::<FaultPoint>().is_err(), "{bad:?} must not parse");
		}
		// Only R1-R13 exist, and only canonical spellings parse: anything else would arm
		// a point that is never hit.
		for bad in ["R0", "R14", "R255", "R256", "R+4", "R04", "R 4", "B-vacuum(+1)", "B-vacuum(01)", "restore-copied(+0)", "L-chunk(007)"] {
			assert!(bad.parse::<FaultPoint>().is_err(), "{bad:?} must not parse");
		}
	}

	#[test]
	fn the_env_spec_parses_and_rejects_typos() {
		let parsed = active::parse_env("S-frame-synced:abort, B-vacuum(1):err").unwrap();
		assert!(matches!(parsed.get(&FaultPoint::SFrameSynced), Some(FaultAction::Abort)));
		assert!(matches!(parsed.get(&FaultPoint::BVacuum(1)), Some(FaultAction::ReturnErr)));
		assert!(active::parse_env("S-frame-synced").is_err());
		assert!(active::parse_env("S-frame-syncd:abort").is_err());
		assert!(active::parse_env("S-frame-synced:explode").is_err());
		assert!(active::parse_env("R04:abort").is_err(), "a non-canonical point name is a typo");
		assert!(active::faults_from_var(None).is_empty(), "an unset variable arms nothing");
	}

	#[cfg(unix)]
	#[test]
	#[should_panic(expected = "is not valid UTF-8")]
	fn a_non_utf8_env_value_panics_instead_of_being_ignored() {
		use std::{ffi::OsString, os::unix::ffi::OsStringExt};

		let _ = active::faults_from_var(Some(OsString::from_vec(b"S-frame-synced:abort\xff".to_vec())));
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn hit_blocking_acts_on_a_blocking_thread() {
		let point = FaultPoint::GUnlinked;
		let before = hits(point);
		tokio::task::spawn_blocking(move || hit_blocking(point)).await.unwrap().expect("an unarmed point does nothing");

		let armed = arm(point, FaultAction::ReturnErr);
		let err = tokio::task::spawn_blocking(move || hit_blocking(point)).await.unwrap().unwrap_err();
		assert_eq!(injected_point(&err), Some(point));
		drop(armed);

		let resume = Arc::new(Notify::new());
		let _armed = arm(point, FaultAction::Pause(resume.clone()));
		let passed = Arc::new(AtomicBool::new(false));
		let task = tokio::task::spawn_blocking({
			let passed = passed.clone();
			move || {
				hit_blocking(point).unwrap();
				passed.store(true, Ordering::SeqCst);
			}
		});
		tokio::time::timeout(Duration::from_secs(10), reached(point, before + 3)).await.expect("the thread reaches the point");
		tokio::time::sleep(Duration::from_millis(20)).await;
		assert!(!passed.load(Ordering::SeqCst), "the thread is parked at the point");
		resume.notify_one();
		task.await.unwrap();
		assert!(passed.load(Ordering::SeqCst));
	}

	#[tokio::test]
	async fn return_err_is_injected_until_disarmed() {
		let point = FaultPoint::SRowsInserted;
		let before = hits(point);
		assert!(hit(point).await.is_ok(), "an unarmed point does nothing");

		let armed = arm(point, FaultAction::ReturnErr);
		let err = hit(point).await.unwrap_err();
		assert_eq!(injected_point(&err), Some(point));
		assert_eq!(err.to_string(), "injected fault at S-rows-inserted");
		assert!(injected_point(&io::Error::other("a real error")).is_none());

		drop(armed);
		assert!(hit(point).await.is_ok(), "dropping the guard disarms the point");
		assert_eq!(hits(point), before + 3, "every hit is counted, armed or not");
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn pause_parks_the_task_until_notified() {
		let point = FaultPoint::MSwapBegun;
		let resume = Arc::new(Notify::new());
		let _armed = arm(point, FaultAction::Pause(resume.clone()));
		let before = hits(point);
		let passed = Arc::new(AtomicBool::new(false));

		let task = tokio::spawn({
			let passed = passed.clone();
			async move {
				hit(point).await.unwrap();
				passed.store(true, Ordering::SeqCst);
			}
		});
		tokio::time::timeout(Duration::from_secs(10), reached(point, before + 1)).await.expect("the task reaches the point");
		tokio::time::sleep(Duration::from_millis(20)).await;
		assert!(!passed.load(Ordering::SeqCst), "the task is parked at the point");

		resume.notify_one();
		task.await.unwrap();
		assert!(passed.load(Ordering::SeqCst));
	}

	/// The body the re-executed child runs. In a normal test run the variable is unset
	/// and this passes without doing anything.
	#[tokio::test]
	async fn abort_child() {
		if std::env::var_os(CHILD_ENV).is_none() {
			return;
		}
		suppress_core_dump();
		let result = hit(FaultPoint::SMidSidecar).await;
		// Reached only when the env var asks for `err` rather than `abort`.
		assert_eq!(injected_point(&result.unwrap_err()), Some(FaultPoint::SMidSidecar));
	}

	fn run_child(spec: &str) -> std::process::Output {
		std::process::Command::new(std::env::current_exe().unwrap()).args(["types::durable::fault::tests::abort_child", "--exact", "--nocapture", "--test-threads=1"]).env(CHILD_ENV, "1").env(FAULT_ENV, spec).output().unwrap()
	}

	#[test]
	fn abort_is_driven_by_the_env_var() {
		let aborted = run_child("S-mid-sidecar:abort");
		assert!(!aborted.status.success(), "the child aborted: {aborted:?}");
		#[cfg(unix)]
		{
			use std::os::unix::process::ExitStatusExt;
			assert_eq!(aborted.status.signal(), Some(6), "killed by SIGABRT: {aborted:?}");
		}
		assert!(String::from_utf8_lossy(&aborted.stderr).contains("aborting at fault point S-mid-sidecar"), "{aborted:?}");

		let errored = run_child("S-mid-sidecar:err");
		assert!(errored.status.success(), "the same point armed with err returns an injected error instead: {errored:?}");
		assert!(String::from_utf8_lossy(&errored.stdout).contains("1 passed"), "the child really ran the test body: {errored:?}");
	}
}
