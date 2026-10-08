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
//! - after it, `{name}` is complete, so `existing(name)` opens it. Nothing after the
//!   rename is undone: from that moment another task or process can open the database,
//!   so a failed parent fsync leaves it in place and only reports that the rename may not
//!   survive a power loss ([`PublishError::NotDurable`]).
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
	fs::{File, OpenOptions, TryLockError}, io, path::{Path, PathBuf}, sync::Arc
};

use crate::types::durable::{
	fault::{self, FaultPoint}, fs::sync_dir_blocking
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
/// Dropping a `Build` that was never [published](Self::publish) removes its build
/// directory, so every early return of `new` before the publish cleans up after itself; a
/// crash leaves the directory to the next sweep instead. Once `publish` has started, its
/// blocking task alone decides the directory's fate (see there).
#[derive(Debug)]
pub struct Build {
	dir: PathBuf,
	parent: PathBuf,
	target: PathBuf,
	/// Set when [`publish`](Self::publish) hands the build directory to its blocking task.
	/// [`Drop`] then leaves the directory alone: the task may be renaming it at that very
	/// moment (a `new` future dropped mid-publish, by a timeout or a `select!`, does not
	/// stop it), and removing the files then could publish a half-built database.
	handed_off: bool,
	/// Shared with the publish task, so that the lock is held until that task ends even if
	/// this `Build` is dropped first: a sweep must not take the directory from under the
	/// rename either. Otherwise it is dropped after [`Drop::drop`] has removed an
	/// unpublished directory, so no sweep can race that cleanup.
	lock: Arc<CreatorLock>,
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
		Ok(Self { dir, parent, target: target.to_path_buf(), handed_off: false, lock: Arc::new(lock) })
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
	/// All of it, and the cleanup after a failure, runs as one blocking task that holds
	/// the creator lock. If this future is dropped before the task ends, the task still
	/// runs to its end, and the `Build` leaves the directory to it: the database is then
	/// either published or, after a failure before the rename, removed by the task.
	///
	/// # Errors
	///
	/// - [`PublishError::NotPublished`]: syncing the build directory or the rename failed,
	///   with `AlreadyExists` if the target exists (another `new` of the same name won).
	///   Nothing is at the target, and the build directory is gone.
	/// - [`PublishError::NotDurable`]: the rename happened but the parent fsync (or the
	///   `L-new-renamed` fault point) failed. The complete database is at the target and
	///   stays there.
	pub async fn publish(&mut self) -> Result<(), PublishError> {
		let (dir, parent, target, lock) = (self.dir.clone(), self.parent.clone(), self.target.clone(), Arc::clone(&self.lock));
		self.handed_off = true;
		let published = blocking(move || {
			let _lock = lock;
			let published = publish_blocking(&dir, &parent, &target);
			if matches!(published, Err(PublishError::NotPublished(_))) {
				remove_build_dir(&dir);
			}
			Ok(published)
		})
		.await;
		// The task did not complete (the runtime is shutting down): reported as before the
		// rename, with an error that says the task did not complete.
		published.unwrap_or_else(|e| Err(PublishError::NotPublished(e)))
	}
}

/// How a [`Build::publish`] failed: before its rename, which is the commit point, or
/// after it.
#[derive(Debug)]
pub enum PublishError {
	/// The database was not published: nothing is at the target, and the build directory
	/// is removed. `AlreadyExists` when the target exists.
	NotPublished(io::Error),
	/// The database was published, complete, at the target, but the parent directory could
	/// not be fsynced afterwards, so the rename may not survive a power loss (after which
	/// the target would be gone and the build directory back, for a sweep). It is left in
	/// place rather than renamed back: from the rename on, another task or process can
	/// have opened it, and withdrawing it would delete files under that handle.
	NotDurable(io::Error),
}

/// [`Build::publish`]'s steps, on a blocking thread.
fn publish_blocking(dir: &Path, parent: &Path, target: &Path) -> Result<(), PublishError> {
	let taken = || io::Error::new(io::ErrorKind::AlreadyExists, format!("{} already exists", target.display()));
	sync_dir_blocking(dir).map_err(PublishError::NotPublished)?;
	if std::fs::metadata(target).is_ok() {
		return Err(PublishError::NotPublished(taken()));
	}
	// A concurrent `new` of the same name can publish between the check and the rename;
	// renaming onto its directory, which is not empty, then fails with `ENOTEMPTY` (or
	// `EEXIST`) rather than replacing it.
	std::fs::rename(dir, target).map_err(|e| PublishError::NotPublished(if matches!(e.kind(), io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty) { taken() } else { e }))?;
	// The commit point is passed: the database is visible at `target` from here on.
	fault::hit_blocking(FaultPoint::LNewRenamed).and_then(|()| sync_dir_blocking(parent)).map_err(PublishError::NotDurable)
}

/// Remove an unpublished build directory; a failure is logged and left to the next sweep.
fn remove_build_dir(dir: &Path) {
	match std::fs::remove_dir_all(dir) {
		Ok(()) => {}
		Err(e) if e.kind() == io::ErrorKind::NotFound => {}
		Err(e) => tracing::warn!(dir = %dir.display(), error = %e, "could not remove the build directory of a failed Database::new; the next sweep removes it"),
	}
}

impl Drop for Build {
	/// Removes the build directory unless [`publish`](Build::publish) took it over. This
	/// is the error path of `new` before the publish, on a directory holding a few small
	/// files, so it runs inline: deferring it to another thread would let `new` return its
	/// error while the directory still exists.
	fn drop(&mut self) {
		if !self.handed_off {
			remove_build_dir(&self.dir);
		}
	}
}

#[cfg(test)]
mod tests {
	use std::time::{Duration, Instant};

	use serial_test::serial;

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

	// Every test that publishes runs serially: one of them arms `L-new-renamed`, which is
	// process-global.
	#[tokio::test]
	#[serial(build_publish)]
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
	#[serial(build_publish)]
	async fn publish_refuses_an_existing_target_and_cleans_up() {
		let root = tempfile::tempdir().unwrap();
		let target = root.path().join("taken");
		std::fs::create_dir(&target).unwrap();
		let mut build = Build::begin(&target).await.unwrap();
		let dir = build.dir().to_path_buf();
		let err = build.publish().await.unwrap_err();
		assert!(matches!(&err, PublishError::NotPublished(e) if e.kind() == io::ErrorKind::AlreadyExists), "{err:?}");
		drop(build);
		assert!(!dir.exists());
		assert!(target.is_dir(), "the existing target is untouched");
	}

	/// A failure after the rename (the parent fsync; here the `L-new-renamed` fault point
	/// standing in for it) leaves the published database where it is and says so. The
	/// rename is the commit point: a concurrent `existing` can already have opened the
	/// database, and renaming it back and removing it, as this used to, deleted the files
	/// under that handle.
	#[tokio::test]
	#[serial(build_publish)]
	async fn a_failure_after_the_rename_leaves_the_database_published() {
		let root = tempfile::tempdir().unwrap();
		let target = root.path().join("db");
		let mut build = Build::begin(&target).await.unwrap();
		std::fs::write(build.dir().join("metadata.db"), b"x").unwrap();
		let dir = build.dir().to_path_buf();
		let published = {
			let _armed = fault::arm(FaultPoint::LNewRenamed, fault::FaultAction::ReturnErr);
			build.publish().await
		};
		drop(build);

		let err = published.unwrap_err();
		assert!(matches!(&err, PublishError::NotDurable(e) if e.to_string().contains("injected fault at L-new-renamed")), "{err:?}");
		assert_eq!(std::fs::read(target.join("metadata.db")).unwrap(), b"x", "the database stays published");
		assert!(!dir.exists(), "nothing is left under the build name");
		assert_eq!(sweep_stale_build_dirs(root.path()).await.unwrap(), 0);
		assert!(target.join("metadata.db").exists(), "a sweep leaves the published database alone");
	}

	/// Resumes a task paused at a fault point when dropped, so that a failed assertion
	/// while it is paused fails the test instead of leaving the runtime's shutdown waiting
	/// on that task forever (and the `serial` group behind it).
	struct ResumeOnDrop(Arc<tokio::sync::Notify>);

	impl Drop for ResumeOnDrop {
		fn drop(&mut self) {
			self.0.notify_one();
		}
	}

	/// A `publish` whose future is dropped part-way (a timeout or a `select!` dropping
	/// `Database::new`) leaves the build to its blocking task: dropping the `Build` removes
	/// nothing, no sweep runs until the task ends (it holds the creator lock), and the task
	/// completes the publish. Before, the `Build`'s drop removed the directory while its
	/// rename could be in flight, and released the lock to a sweep.
	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	#[serial(build_publish)]
	async fn a_dropped_publish_is_finished_by_its_task() {
		let root = tempfile::tempdir().unwrap();
		let target = root.path().join("db");
		let mut build = Build::begin(&target).await.unwrap();
		std::fs::write(build.dir().join("metadata.db"), b"x").unwrap();

		let resume = Arc::new(tokio::sync::Notify::new());
		let before = fault::hits(FaultPoint::LNewRenamed);
		let armed = fault::arm(FaultPoint::LNewRenamed, fault::FaultAction::Pause(resume.clone()));
		let paused = ResumeOnDrop(resume);
		{
			let publishing = build.publish();
			tokio::pin!(publishing);
			tokio::select! {
				_ = &mut publishing => panic!("the publish task is paused at L-new-renamed"),
				() = fault::reached(FaultPoint::LNewRenamed, before + 1) => {}
			}
		}
		drop(build);
		drop(armed);

		// Observe everything while the task is paused, then resume it before asserting.
		let published_while_paused = target.join("metadata.db").exists();
		// What a crashed creator leaves: no sweep may run while the publish task holds the lock.
		let stale = root.path().join(format!(".gone{BUILD_INFIX}{}", uuid::Uuid::new_v4().simple()));
		std::fs::create_dir(&stale).unwrap();
		let swept_while_paused = sweep_stale_build_dirs(root.path()).await;
		let stale_kept = stale.exists();
		drop(paused);

		assert!(published_while_paused, "dropping the Build mid-publish removed nothing");
		assert_eq!(swept_while_paused.unwrap(), 0, "the publish task still holds the creator lock");
		assert!(stale_kept);
		assert_eq!(sweep_once_unlocked(root.path()).await, 1, "the lock is released once the task ends");
		assert!(target.join("metadata.db").exists(), "the task completed the publish");
	}
}
