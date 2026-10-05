//! The store root's `LOCK` file (design sections 4 and 5.5).
//!
//! Turso already locks each database file exclusively, so a second process cannot
//! open the same DBs. It can still open `segments/` and race the first process's
//! frame names, reaper and recovery. [`RootLock`] closes that gap and turns the
//! failure into a message that names the process holding the root.

use std::{
	fmt, fs::{File, OpenOptions, TryLockError}, io::{self, Seek, SeekFrom, Write}, path::{Path, PathBuf}
};

/// The lock file's name under the store root.
pub const LOCK_FILE: &str = "LOCK";

/// Exclusive ownership of a store root, held for as long as the value lives.
///
/// It uses std's `File::try_lock` (an advisory `flock` on Unix, a mandatory
/// `LockFileEx` on Windows). The OS drops the lock when the process exits, however it
/// exits, so a crash never leaves a stale lock behind and the file is never deleted.
/// While held, the file records the holder's pid and a per-open session id, so a
/// second opener can say who has the root.
#[derive(Debug)]
pub struct RootLock {
	/// Keeps the lock: closing the file releases it.
	_file: File,
	path: PathBuf,
	session: String,
}

/// Who holds a root, as recorded in its `LOCK` file.
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
	/// not be read: it may not have written it yet, and on Windows the lock also bars
	/// other handles from reading the file.
	Held {
		/// The store root.
		root: PathBuf,
		/// The recorded holder, if it could be read.
		holder: Option<LockHolder>,
	},
	/// Opening, locking or writing the lock file failed.
	Io {
		/// The lock file.
		path: PathBuf,
		/// The underlying error.
		source: io::Error,
	},
}

impl fmt::Display for RootLockError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Held { root, holder: Some(holder) } => write!(f, "store root {} is in use by pid {} (session {})", root.display(), holder.pid, holder.session),
			Self::Held { root, holder: None } => write!(f, "store root {} is in use by another process (its pid could not be read from {})", root.display(), root.join(LOCK_FILE).display()),
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
	/// Take the lock on `root`, creating `root/LOCK` if needed, without waiting.
	///
	/// # Errors
	///
	/// [`RootLockError::Held`] if another open holds the root, naming its pid when the
	/// record is readable; [`RootLockError::Io`] if the lock file cannot be opened,
	/// locked or written.
	pub fn acquire(root: &Path) -> Result<Self, RootLockError> {
		let path = root.join(LOCK_FILE);
		let io_err = |source| RootLockError::Io { path: path.clone(), source };
		// Never truncate on open: until the lock is ours, the contents are the
		// holder's record.
		let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path).map_err(io_err)?;
		match file.try_lock() {
			Ok(()) => {}
			Err(TryLockError::WouldBlock) => return Err(RootLockError::Held { root: root.to_path_buf(), holder: read_holder(&path) }),
			Err(TryLockError::Error(e)) => return Err(io_err(e)),
		}
		let session = uuid::Uuid::new_v4().to_string();
		// The record is only for the error message, so it is not fsynced: a crash
		// releases the lock anyway, and the next holder rewrites it.
		let record = format!("pid={}\nsession={session}\n", std::process::id());
		file.set_len(0).and_then(|()| file.seek(SeekFrom::Start(0))).and_then(|_| file.write_all(record.as_bytes())).map_err(io_err)?;
		Ok(Self { _file: file, path, session })
	}

	/// The lock file.
	#[must_use]
	pub fn path(&self) -> &Path {
		&self.path
	}

	/// This open's session id, as recorded in the lock file.
	#[must_use]
	pub fn session(&self) -> &str {
		&self.session
	}
}

/// Parse the holder record a [`RootLock`] writes: `pid=<n>` and `session=<id>` lines.
fn read_holder(path: &Path) -> Option<LockHolder> {
	let text = std::fs::read_to_string(path).ok()?;
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

		let err = RootLock::acquire(root.path()).unwrap_err();
		let RootLockError::Held { holder, .. } = &err else { panic!("expected a held lock, got {err:?}") };
		// Windows locks are mandatory, so no other handle can read the record there.
		if cfg!(unix) {
			let holder = holder.as_ref().expect("the holder's record is readable");
			assert_eq!(holder.pid, std::process::id());
			assert_eq!(holder.session, held.session());
			let message = err.to_string();
			assert!(message.contains(&format!("in use by pid {}", std::process::id())), "{message}");
		}

		let session = held.session().to_owned();
		drop(held);
		let again = RootLock::acquire(root.path()).expect("dropping the lock releases it");
		assert_ne!(again.session(), session, "every open records a fresh session");
		if cfg!(unix) {
			let record = std::fs::read_to_string(root.path().join(LOCK_FILE)).unwrap();
			assert_eq!(record, format!("pid={}\nsession={}\n", std::process::id(), again.session()), "the previous holder's record was replaced, not appended to");
		}
	}

	#[cfg(unix)]
	#[test]
	fn a_held_lock_with_an_unreadable_record_still_refuses() {
		let root = tempfile::tempdir().unwrap();
		let held = RootLock::acquire(root.path()).unwrap();
		// Simulate a holder that has locked but not yet written its record.
		std::fs::write(root.path().join(LOCK_FILE), b"").unwrap();
		let err = RootLock::acquire(root.path()).unwrap_err();
		assert!(matches!(err, RootLockError::Held { holder: None, .. }), "{err:?}");
		assert!(err.to_string().contains("in use by another process"), "{err}");
		drop(held);
	}

	#[test]
	fn a_missing_root_is_an_io_error() {
		let root = tempfile::tempdir().unwrap();
		let err = RootLock::acquire(&root.path().join("missing")).unwrap_err();
		assert!(matches!(&err, RootLockError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound), "{err:?}");
	}
}
