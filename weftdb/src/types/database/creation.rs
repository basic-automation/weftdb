//! Atomic `Database::new`: build under a hidden name, then publish with one rename
//! (crash-consistency design, S18: legacy-database-new-half-created).
//!
//! `Database::new(name)` used to `mkdir {data_dir}/{name}` first and run the DDL and the
//! `database` row commit afterwards, so a crash or error in between left a folder that
//! `new` refused ("already exists") and `existing` could not open (no `database` row).
//! Now the whole database is built in `{parent}/.{name}.creating-{nonce}/` and becomes
//! `{name}` only through a rename, after its own directory entries are fsynced; the
//! parent directory is fsynced after the rename. The rename is the commit point:
//!
//! - before it, `{name}` does not exist, so a retry of `new(name)` starts over, and the
//!   build directory is crash litter that the next `new` or database listing sweeps;
//! - after it, `{name}` is complete, so `existing(name)` opens it.
//!
//! **Which build directories are stale.** A sweep must never delete the directory of a
//! `new` that is still running, in this process or another one. Every creator holds a
//! *shared* lock on [`CREATION_LOCK_FILE`] in the parent directory from before it
//! creates its build directory until after the rename (or its own cleanup); a sweep
//! runs only while it holds the *exclusive* lock, which it merely tries for and skips
//! the sweep when busy. The OS drops a crashed creator's lock, so its directory becomes
//! sweepable, and a live one's never is. The lock is std's `File::lock_shared` /
//! `File::try_lock` (`flock` on Unix, `LockFileEx` on Windows), which conflicts between
//! two handles of the same process too, so concurrent in-process creators are covered.
//!
//! **Not covered here: power loss.** The durable-I/O simulator (`durable::sim::SimFs`)
//! does not model directories yet (design section 11), so the `L-new-*` points are
//! exercised for process crashes only.

use std::{
	fs::{File, OpenOptions, TryLockError}, io, path::{Path, PathBuf}
};

use crate::types::durable::{
	fault::{self, FaultPoint}, RealFs, StoreFs
};

/// The lock file creators share and sweepers take exclusively, in the directory that
/// holds the databases (the data directory). It is created by the first `new` and never
/// removed: deleting a lock file while someone may be about to lock it would let two
/// processes lock two different files of the same name.
const CREATION_LOCK_FILE: &str = ".weft-creating.lock";

/// The infix of a build directory's name: `.{name}.creating-{nonce}`.
const BUILD_INFIX: &str = ".creating-";

/// Whether `file_name` is a build directory's name: `.{name}.creating-{32 hex digits}`.
///
/// The nonce must be exactly what [`Build::begin`] writes, so that a sweep never takes a
/// directory a user happened to name `.x.creating-y` for a build directory.
pub fn is_build_dir_name(file_name: &str) -> bool {
	let Some(rest) = file_name.strip_prefix('.') else { return false };
	let Some((name, nonce)) = rest.rsplit_once(BUILD_INFIX) else { return false };
	!name.is_empty() && nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Run one blocking filesystem job off the async workers.
async fn blocking<T, F>(job: F) -> io::Result<T>
where
	T: Send + 'static,
	F: FnOnce() -> io::Result<T> + Send + 'static,
{
	match tokio::task::spawn_blocking(job).await {
		Ok(result) => result,
		Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
		Err(e) => Err(io::Error::other(format!("blocking filesystem task did not complete: {e}"))),
	}
}

/// Remove every stale build directory in `dir` and return how many were removed.
///
/// A directory is stale only when no creator holds the creation lock, which this checks
/// by trying the exclusive lock: when it is busy (a `new` is running somewhere) the sweep
/// is skipped, and the next `new` or listing does it. A `dir` with no lock file has
/// never had a build directory in it, so there is nothing to sweep. Removals are
/// fsynced (on platforms that can fsync a directory) before the lock is released.
///
/// The lock can also look busy for a moment after its creator released it: a child
/// process that another thread is spawning shares the creator's descriptor between its
/// fork and its exec (std opens files close-on-exec). That only defers the sweep, which
/// errs on the side of keeping a directory, never of removing a live one.
///
/// # Errors
///
/// Any error opening the lock file (other than its absence), reading `dir`, removing a
/// stale directory or syncing `dir`. Callers treat the sweep as best-effort.
pub async fn sweep_stale_build_dirs(dir: &Path) -> io::Result<usize> {
	let dir = dir.to_path_buf();
	let removed = blocking(move || sweep_blocking(&dir)).await?;
	Ok(removed)
}

fn sweep_blocking(dir: &Path) -> io::Result<usize> {
	let lock = match OpenOptions::new().read(true).write(true).open(dir.join(CREATION_LOCK_FILE)) {
		Ok(lock) => lock,
		Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
		Err(e) => return Err(e),
	};
	match lock.try_lock() {
		Ok(()) => {}
		Err(TryLockError::WouldBlock) => {
			tracing::debug!(dir = %dir.display(), "a database is being created; skipping the stale build-directory sweep");
			return Ok(0);
		}
		// Without the lock nothing proves a build directory is stale, so leave them all.
		Err(TryLockError::Error(e)) => return Err(e),
	}
	let mut removed = 0;
	for entry in std::fs::read_dir(dir)? {
		let entry = entry?;
		let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else { continue };
		if !is_build_dir_name(&file_name) || !entry.file_type()?.is_dir() {
			continue;
		}
		match std::fs::remove_dir_all(entry.path()) {
			Ok(()) => {
				tracing::info!(dir = %entry.path().display(), "removed a stale database build directory left by an interrupted Database::new");
				removed += 1;
			}
			Err(e) if e.kind() == io::ErrorKind::NotFound => {}
			Err(e) => return Err(e),
		}
	}
	if removed > 0 {
		crate::types::durable::fs::sync_dir_blocking(dir)?;
	}
	drop(lock);
	Ok(removed)
}

/// A creator's shared hold on the creation lock: no sweep runs while it lives.
#[derive(Debug)]
struct CreatorLock {
	_file: Option<File>,
}

impl CreatorLock {
	/// Take the shared lock in `dir`, creating the lock file if needed. A sweep holds the
	/// exclusive lock only briefly, so this waits for it.
	///
	/// A filesystem that cannot lock (`Unsupported`, or `ENOLCK` on some network
	/// filesystems) still lets the database be created, as it did before the lock existed:
	/// the creator goes ahead unlocked, which is safe because a sweep there cannot take
	/// the exclusive lock either, and never sweeps without it.
	///
	/// # Errors
	///
	/// Any error opening or creating the lock file.
	async fn take(dir: &Path) -> io::Result<Self> {
		let path = dir.join(CREATION_LOCK_FILE);
		blocking(move || {
			let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?;
			loop {
				match file.lock_shared() {
					Ok(()) => return Ok(Self { _file: Some(file) }),
					// A signal cut the wait short; that says nothing about the filesystem.
					Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
					Err(e) => {
						tracing::warn!(lock = %path.display(), error = %e, "could not lock; creating the database without the build-directory lock (no sweep can run here without it either)");
						return Ok(Self { _file: None });
					}
				}
			}
		})
		.await
	}
}

/// One `Database::new` in progress: its build directory and its creator lock.
///
/// Dropping a `Build` that was not [published](Self::publish) removes its build
/// directory, so every early return of `new` cleans up after itself; a crash leaves the
/// directory to the next sweep instead.
#[derive(Debug)]
pub struct Build {
	dir: PathBuf,
	parent: PathBuf,
	target: PathBuf,
	published: bool,
	/// Dropped after [`Drop::drop`] has removed an unpublished directory, so no sweep can
	/// race that cleanup.
	_lock: CreatorLock,
}

impl Build {
	/// Take the creator lock in `target`'s parent directory and create a fresh build
	/// directory beside `target`. The parent is created if it does not exist.
	///
	/// # Errors
	///
	/// `InvalidInput` if `target` has no parent or no UTF-8 file name; any error
	/// creating the parent, taking the lock or creating the build directory.
	pub async fn begin(target: &Path) -> io::Result<Self> {
		let invalid = || io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not a database path", target.display()));
		let parent = target.parent().ok_or_else(invalid)?.to_path_buf();
		let leaf = target.file_name().and_then(|leaf| leaf.to_str()).ok_or_else(invalid)?;
		let dir = parent.join(format!(".{leaf}{BUILD_INFIX}{}", uuid::Uuid::new_v4().simple()));
		let create_parent = parent.clone();
		blocking(move || std::fs::create_dir_all(&create_parent)).await?;
		let lock = CreatorLock::take(&parent).await?;
		let create = dir.clone();
		blocking(move || std::fs::create_dir(&create)).await?;
		Ok(Self { dir, parent, target: target.to_path_buf(), published: false, _lock: lock })
	}

	/// The build directory, where the database files are written.
	pub fn dir(&self) -> &Path {
		&self.dir
	}

	/// Publish the build: fsync the build directory (so the database files' entries are
	/// durable), rename it to the target, then fsync the parent (so the rename is).
	///
	/// Call it only after every handle to the files inside is closed: Windows cannot
	/// rename a directory with open files, and a database opened afterwards must be
	/// opened at its final path.
	///
	/// # Errors
	///
	/// `AlreadyExists` if the target exists (another `new` of the same name won). Any
	/// other error syncing or renaming. A failure after the rename (the parent fsync, or
	/// the `L-new-renamed` fault point) renames the build back, so an error always means
	/// the database was not created; if that rename back fails too, the complete database
	/// stays at the target and the error says so.
	pub async fn publish(&mut self) -> io::Result<()> {
		let taken = || io::Error::new(io::ErrorKind::AlreadyExists, format!("{} already exists", self.target.display()));
		RealFs.sync_dir(&self.dir).await?;
		if RealFs.metadata(&self.target).await.is_ok() {
			return Err(taken());
		}
		// A concurrent `new` of the same name can publish between the check and the rename;
		// renaming onto its directory, which is not empty, then fails with `ENOTEMPTY` (or
		// `EEXIST`) rather than replacing it.
		RealFs.rename(&self.dir, &self.target).await.map_err(|e| if matches!(e.kind(), io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty) { taken() } else { e })?;
		let committed = match fault::hit(FaultPoint::LNewRenamed).await {
			Ok(()) => RealFs.sync_dir(&self.parent).await,
			Err(e) => Err(e),
		};
		if let Err(e) = committed {
			if let Err(back) = RealFs.rename(&self.target, &self.dir).await {
				self.published = true;
				return Err(io::Error::new(e.kind(), format!("{e}; the database at {} is complete but could not be withdrawn ({back})", self.target.display())));
			}
			return Err(e);
		}
		self.published = true;
		Ok(())
	}
}

impl Drop for Build {
	fn drop(&mut self) {
		if self.published {
			return;
		}
		match std::fs::remove_dir_all(&self.dir) {
			Ok(()) => {}
			Err(e) if e.kind() == io::ErrorKind::NotFound => {}
			Err(e) => tracing::warn!(dir = %self.dir.display(), error = %e, "could not remove the build directory of a failed Database::new; the next sweep removes it"),
		}
	}
}

#[cfg(test)]
mod tests {
	use std::time::{Duration, Instant};

	use super::*;

	/// Sweep `dir` until a sweep removes something, for at most a few seconds, and return
	/// what it removed. Other tests in this binary re-execute it, and a child they are
	/// spawning shares a just-released lock's descriptor until its exec, which makes the
	/// lock look busy for that moment (see [`sweep_stale_build_dirs`]).
	async fn sweep_once_unlocked(dir: &Path) -> usize {
		let deadline = Instant::now() + Duration::from_secs(5);
		loop {
			let removed = sweep_stale_build_dirs(dir).await.unwrap();
			if removed > 0 || Instant::now() >= deadline {
				return removed;
			}
			tokio::time::sleep(Duration::from_millis(5)).await;
		}
	}

	#[test]
	fn only_names_build_writes_are_build_dirs() {
		let nonce = uuid::Uuid::new_v4().simple().to_string();
		assert!(is_build_dir_name(&format!(".sensors.creating-{nonce}")));
		assert!(is_build_dir_name(&format!(".a.creating-b.creating-{nonce}")), "the nonce is the last infix's");
		for name in ["sensors", ".sensors", ".sensors.creating-", ".creating-0123456789abcdef0123456789abcdef", ".sensors.creating-xyz", ".sensors.creating-0123456789ABCDEF0123456789ABCDEF", ".sensors.creating-0123456789abcdef0123456789abcde", "sensors.creating-0123456789abcdef0123456789abcdef"] {
			assert!(!is_build_dir_name(name), "{name:?} is not a build directory");
		}
	}

	#[tokio::test]
	async fn a_live_build_is_not_swept_and_a_dropped_one_cleans_up() {
		let root = tempfile::tempdir().unwrap();
		let target = root.path().join("db");
		let build = Build::begin(&target).await.unwrap();
		std::fs::write(build.dir().join("metadata.db"), b"x").unwrap();
		assert_eq!(sweep_stale_build_dirs(root.path()).await.unwrap(), 0, "a live build holds the lock");
		assert!(build.dir().exists());

		let dir = build.dir().to_path_buf();
		drop(build);
		assert!(!dir.exists(), "an unpublished build removes its directory");
	}

	#[tokio::test]
	async fn a_stale_build_is_swept_and_other_entries_are_kept() {
		let root = tempfile::tempdir().unwrap();
		assert_eq!(sweep_stale_build_dirs(root.path()).await.unwrap(), 0, "no lock file: nothing was ever built here");
		let mut build = Build::begin(&root.path().join("kept")).await.unwrap();
		build.publish().await.unwrap();
		drop(build);
		// A creator that crashed: its directory is there and nobody holds the lock.
		let stale = root.path().join(format!(".gone{BUILD_INFIX}{}", uuid::Uuid::new_v4().simple()));
		std::fs::create_dir_all(stale.join("nested")).unwrap();
		std::fs::write(stale.join("metadata.db"), b"x").unwrap();
		let lookalike = root.path().join(".mine.creating-not-a-nonce");
		std::fs::create_dir(&lookalike).unwrap();

		assert_eq!(sweep_once_unlocked(root.path()).await, 1);
		assert!(!stale.exists());
		assert!(lookalike.exists(), "a name the build never writes is not swept");
		assert!(root.path().join("kept").is_dir(), "a published database is not swept");
	}

	#[tokio::test]
	async fn publish_refuses_an_existing_target_and_cleans_up() {
		let root = tempfile::tempdir().unwrap();
		let target = root.path().join("taken");
		std::fs::create_dir(&target).unwrap();
		let mut build = Build::begin(&target).await.unwrap();
		let dir = build.dir().to_path_buf();
		let err = build.publish().await.unwrap_err();
		assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
		drop(build);
		assert!(!dir.exists());
		assert!(target.is_dir(), "the existing target is untouched");
	}
}
