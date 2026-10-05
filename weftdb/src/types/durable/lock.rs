//! The store root's `LOCK` file (design sections 4 and 5.5).
//!
//! Turso already locks each database file exclusively, so a second process cannot
//! open the same DBs. It can still open `segments/` and race the first process's
//! frame names, reaper and recovery. [`RootLock`] closes that gap and turns the
//! failure into a message that names the process holding the root.

use std::{
	collections::HashSet, fmt, fs::{File, OpenOptions, TryLockError}, io, path::{Path, PathBuf}, sync::{LazyLock, Mutex, MutexGuard, PoisonError}, thread, time::{Duration, Instant}
};

/// The lock file's name under the store root.
pub const LOCK_FILE: &str = "LOCK";

/// The file beside [`LOCK_FILE`] in which the holder records its pid and session.
///
/// The record cannot live in `LOCK` itself: on Windows the lock is mandatory, so no
/// other handle can read a locked file, and a second opener could never say who holds
/// the root.
pub const HOLDER_FILE: &str = "LOCK.holder";

/// How long [`RootLock::acquire`] waits for a lock this process has released but a
/// child it is spawning still shares (see [`RootLock`]). Spawning takes microseconds
/// to milliseconds; the bound only matters if something else entirely holds the lock
/// and happens to have this process's pid in a stale record.
const INHERITED_LOCK_WAIT: Duration = Duration::from_secs(2);

/// The lock files held by live [`RootLock`]s in this process, canonicalised.
static HELD_HERE: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Mutex::default);

fn held_here() -> MutexGuard<'static, HashSet<PathBuf>> {
	HELD_HERE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Exclusive ownership of a store root, held for as long as the value lives.
///
/// It uses std's `File::try_lock` (an advisory `flock` on Unix, a mandatory
/// `LockFileEx` on Windows). The OS drops the lock when the process exits, however it
/// exits, so a crash never leaves a stale lock behind and the file is never deleted.
/// While held, [`HOLDER_FILE`] records the holder's pid and a per-open session id, so
/// a second opener can say who has the root.
///
/// A child process spawned by another thread while the lock is held inherits the
/// lock's file descriptor between its fork and its exec, which closes it (std opens
/// files close-on-exec). For that moment the lock outlives a drop of its `RootLock`.
/// [`acquire`](Self::acquire) therefore waits, briefly, when the lock is busy but the
/// record names this process and no `RootLock` in this process holds it: that is the
/// in-process reopen racing a spawn, which the crash tests do constantly (they drop
/// and reopen the store while other tests re-execute the test binary).
#[derive(Debug)]
pub struct RootLock {
	/// Keeps the lock: closing the file releases it.
	_file: File,
	path: PathBuf,
	/// `path`, canonicalised: this lock's entry in [`HELD_HERE`].
	key: PathBuf,
	session: String,
}

/// Who holds a root, as recorded in its [`HOLDER_FILE`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockHolder {
	/// The holder's process id.
	pub pid: u32,
	/// The holder's session id, unique per open.
	pub session: String,
}

/// Why [`RootLock::acquire`] failed.
#[derive(Debug)]
pub enum RootLockError {
	/// Another open holds the root. `holder` is `None` when the holder's record could
	/// not be read, for example because the holder has not written it yet.
	Held {
		/// The store root.
		root: PathBuf,
		/// The recorded holder, if it could be read.
		holder: Option<LockHolder>,
	},
	/// Opening or locking the lock file, or writing the holder record, failed.
	Io {
		/// The file concerned.
		path: PathBuf,
		/// The underlying error.
		source: io::Error,
	},
}

impl fmt::Display for RootLockError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Held { root, holder: Some(holder) } => write!(f, "store root {} is in use by pid {} (session {})", root.display(), holder.pid, holder.session),
			Self::Held { root, holder: None } => write!(f, "store root {} is in use by another process (its pid could not be read from {})", root.display(), root.join(HOLDER_FILE).display()),
			Self::Io { path, source } => write!(f, "could not lock {}: {source}", path.display()),
		}
	}
}

impl std::error::Error for RootLockError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Held { .. } => None,
			Self::Io { source, .. } => Some(source),
		}
	}
}

impl RootLock {
	/// Take the lock on `root`, creating `root/LOCK` if needed. It does not wait for
	/// another holder, only for a lock this process released but a child it is
	/// spawning still shares (see [`RootLock`]).
	///
	/// # Errors
	///
	/// [`RootLockError::Held`] if another open holds the root, naming its pid when the
	/// record is readable; [`RootLockError::Io`] if the lock file cannot be opened or
	/// locked, or the holder record cannot be written.
	pub fn acquire(root: &Path) -> Result<Self, RootLockError> {
		let path = root.join(LOCK_FILE);
		let io_err = |path: &Path, source| RootLockError::Io { path: path.to_path_buf(), source };
		let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path).map_err(|e| io_err(&path, e))?;
		let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
		let deadline = Instant::now() + INHERITED_LOCK_WAIT;
		loop {
			match file.try_lock() {
				Ok(()) => break,
				Err(TryLockError::WouldBlock) => {
					let holder = read_holder(root);
					let inherited = holder.as_ref().is_some_and(|holder| holder.pid == std::process::id()) && !held_here().contains(&key);
					if !inherited || Instant::now() >= deadline {
						return Err(RootLockError::Held { root: root.to_path_buf(), holder });
					}
					thread::sleep(Duration::from_millis(2));
				}
				Err(TryLockError::Error(e)) => return Err(io_err(&path, e)),
			}
		}
		held_here().insert(key.clone());
		let lock = Self { _file: file, path, key, session: uuid::Uuid::new_v4().to_string() };
		// The record is only for the error message, so it is not fsynced: a crash
		// releases the lock anyway, and the next holder rewrites it. A failure to write
		// it drops `lock`, which releases the root again.
		let holder_path = root.join(HOLDER_FILE);
		std::fs::write(&holder_path, format!("pid={}\nsession={}\n", std::process::id(), lock.session)).map_err(|e| io_err(&holder_path, e))?;
		Ok(lock)
	}

	/// The lock file.
	#[must_use]
	pub fn path(&self) -> &Path {
		&self.path
	}

	/// This open's session id, as recorded in the holder file.
	#[must_use]
	pub fn session(&self) -> &str {
		&self.session
	}
}

impl Drop for RootLock {
	fn drop(&mut self) {
		// Before the file closes (fields drop after this body), so an in-process
		// acquire that still sees the lock busy knows it is no longer held here.
		held_here().remove(&self.key);
	}
}

/// Parse the holder record a [`RootLock`] writes: `pid=<n>` and `session=<id>` lines.
fn read_holder(root: &Path) -> Option<LockHolder> {
	let text = std::fs::read_to_string(root.join(HOLDER_FILE)).ok()?;
	let field = |key: &str| text.lines().find_map(|line| line.strip_prefix(key)?.strip_prefix('='));
	Some(LockHolder { pid: field("pid")?.trim().parse().ok()?, session: field("session").unwrap_or("unknown").trim().to_owned() })
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_second_lock_on_the_same_root_names_the_holders_pid() {
		let root = tempfile::tempdir().unwrap();
		let held = RootLock::acquire(root.path()).unwrap();
		assert_eq!(held.path(), root.path().join(LOCK_FILE));

		let started = Instant::now();
		let err = RootLock::acquire(root.path()).unwrap_err();
		assert!(started.elapsed() < INHERITED_LOCK_WAIT, "a lock held in this process is refused at once, not waited for");
		let RootLockError::Held { holder, .. } = &err else { panic!("expected a held lock, got {err:?}") };
		let holder = holder.as_ref().expect("the holder's record is readable on every platform");
		assert_eq!(holder.pid, std::process::id());
		assert_eq!(holder.session, held.session());
		let message = err.to_string();
		assert!(message.contains(&format!("in use by pid {}", std::process::id())), "{message}");

		let session = held.session().to_owned();
		drop(held);
		let again = RootLock::acquire(root.path()).expect("dropping the lock releases it");
		assert_ne!(again.session(), session, "every open records a fresh session");
		let record = std::fs::read_to_string(root.path().join(HOLDER_FILE)).unwrap();
		assert_eq!(record, format!("pid={}\nsession={}\n", std::process::id(), again.session()), "the previous holder's record was replaced, not appended to");
	}

	#[test]
	fn a_held_lock_with_an_unreadable_record_still_refuses() {
		let root = tempfile::tempdir().unwrap();
		let held = RootLock::acquire(root.path()).unwrap();
		// Simulate a holder that has locked but not yet written its record.
		std::fs::write(root.path().join(HOLDER_FILE), b"").unwrap();
		let err = RootLock::acquire(root.path()).unwrap_err();
		assert!(matches!(err, RootLockError::Held { holder: None, .. }), "{err:?}");
		assert!(err.to_string().contains("in use by another process"), "{err}");
		drop(held);
	}

	/// A child spawned by another thread shares the lock's open file description until
	/// its exec closes it. A duplicated descriptor that outlives the `RootLock` is the
	/// same situation, without the timing.
	#[cfg(unix)]
	#[test]
	fn a_lock_still_shared_with_a_spawning_child_is_waited_out() {
		let root = tempfile::tempdir().unwrap();
		let held = RootLock::acquire(root.path()).unwrap();
		let inherited = held._file.try_clone().unwrap();
		drop(held);
		let child = thread::spawn(move || {
			thread::sleep(Duration::from_millis(100));
			drop(inherited);
		});
		RootLock::acquire(root.path()).expect("the lock is retaken once the child's copy closes");
		child.join().unwrap();
	}

	#[test]
	fn a_missing_root_is_an_io_error() {
		let root = tempfile::tempdir().unwrap();
		let err = RootLock::acquire(&root.path().join("missing")).unwrap_err();
		assert!(matches!(&err, RootLockError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound), "{err:?}");
	}
}
