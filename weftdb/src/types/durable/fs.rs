//! The filesystem seam every durable store write goes through.
//!
//! [`StoreFs`] names exactly the operations the crash-consistency protocols need
//! (docs/design/crash-consistency.md section 5): create a never-used name and write it
//! once, fsync a file, fsync a directory, hard-link, rename, remove, list and stat.
//! The store holds an `Arc<dyn StoreFs>`, so production runs on [`RealFs`] while the
//! crash tests swap in `SimFs` (`fault-injection` feature), which records which bytes and
//! directory entries are durable and can materialise a legal post-power-loss image.
//!
//! Reads of frame bytes deliberately stay outside this trait: a crash cannot change
//! what a read returns before the crash, and `SimFs` passes every write through to the
//! real directory, so the existing read paths see the live state unchanged.

use std::{
	ffi::OsString, fmt, io::{self, Write}, path::Path
};

use async_trait::async_trait;

/// Whether [`write_new_durable`] makes the file's bytes durable before it returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPolicy {
	/// `sync_all` the file before returning: strict-mode seals and every maintenance
	/// and backup write (design section 2, SEAL-4 and M4).
	Full,
	/// Leave the bytes to the page cache. Only relaxed-mode seals (S17) use this; a
	/// background syncer later makes them durable and advances `synced_epoch`.
	None,
}

/// What [`StoreFs::metadata`] reports about a path (symlinks are followed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsMetadata {
	/// The length in bytes (meaningless for a directory).
	pub len: u64,
	/// Whether the path is a directory.
	pub is_dir: bool,
}

/// One entry of [`StoreFs::read_dir`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FsEntry {
	/// The entry's file name (not a path). Frame names are ASCII, but recovery must
	/// still see, and quarantine, litter whose name is not UTF-8.
	pub name: OsString,
	/// Whether the entry is a directory (for example `segments/quarantine/`).
	pub is_dir: bool,
}

/// The filesystem operations the durability protocols are written against.
///
/// Every method is async so that [`RealFs`] can run the blocking syscall on
/// `spawn_blocking` instead of stalling a tokio worker for the length of an fsync,
/// which on a loaded HDD array is tens of milliseconds.
#[async_trait]
pub trait StoreFs: Send + Sync + fmt::Debug {
	/// Create `path`, which must not exist (`O_CREAT | O_EXCL`), and write `bytes` to
	/// it. Nothing is synced.
	///
	/// # Errors
	///
	/// `AlreadyExists` if `path` exists; the existing file is left untouched. On any
	/// other error no file is left at `path`: a file this call created but could not
	/// fully write is removed again, because a short frame under a final name is
	/// exactly the torn-frame window the design closes.
	async fn create_new_write(&self, path: &Path, bytes: Vec<u8>) -> io::Result<()>;

	/// Make the contents of the existing file at `path` durable (`fsync`; on Apple
	/// targets std issues `F_FULLFSYNC`). It does not make the file's directory entry
	/// durable: that is [`sync_dir`](Self::sync_dir)'s job.
	///
	/// # Errors
	///
	/// Any error opening or syncing the file. A sync error must be treated as fatal
	/// for the bytes concerned (fsyncgate): retrying can report success for pages the
	/// kernel already dropped.
	async fn sync_file(&self, path: &Path) -> io::Result<()>;

	/// Make the entries of directory `dir` durable: every create, link, rename and
	/// remove in it that completed before the call. On Unix this opens the directory
	/// and fsyncs it. On Windows std cannot open a directory for flushing, so this is
	/// a no-op that warns once: strict mode there survives a process crash only, and
	/// `/ready` reports that durability class (design section 2).
	///
	/// # Errors
	///
	/// Any error opening or syncing the directory.
	async fn sync_dir(&self, dir: &Path) -> io::Result<()>;

	/// Create `dst` as a second name for the file at `src`. Backups link frames
	/// instead of copying them (design section 9).
	///
	/// # Errors
	///
	/// `AlreadyExists` if `dst` exists, or any other link error.
	async fn hard_link(&self, src: &Path, dst: &Path) -> io::Result<()>;

	/// Atomically rename `from` to `to`, replacing `to` if it is a file.
	///
	/// # Errors
	///
	/// Any rename error.
	async fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

	/// Remove the file at `path`. A missing file counts as success, so that the
	/// reaper and recovery can replay a removal that may already have happened before
	/// a crash.
	///
	/// # Errors
	///
	/// Any removal error other than `NotFound`.
	async fn remove_file(&self, path: &Path) -> io::Result<()>;

	/// List the entries of `dir`, sorted by name so that recovery is deterministic.
	///
	/// # Errors
	///
	/// Any error reading the directory.
	async fn read_dir(&self, dir: &Path) -> io::Result<Vec<FsEntry>>;

	/// Stat `path`.
	///
	/// # Errors
	///
	/// Any stat error, including `NotFound`.
	async fn metadata(&self, path: &Path) -> io::Result<FsMetadata>;
}

/// The production [`StoreFs`]: std filesystem calls, each run on `spawn_blocking`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealFs;

/// Run one blocking filesystem call off the async workers.
async fn blocking<T, F>(op: F) -> io::Result<T>
where
	T: Send + 'static,
	F: FnOnce() -> io::Result<T> + Send + 'static,
{
	match tokio::task::spawn_blocking(op).await {
		Ok(result) => result,
		// A panic inside a std filesystem call is a bug; keep it a panic rather than
		// turning it into an I/O error a caller might retry.
		Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
		Err(e) => Err(io::Error::other(format!("blocking filesystem task did not complete: {e}"))),
	}
}

#[async_trait]
impl StoreFs for RealFs {
	async fn create_new_write(&self, path: &Path, bytes: Vec<u8>) -> io::Result<()> {
		let path = path.to_path_buf();
		blocking(move || create_new_write_blocking(&path, &bytes)).await
	}

	async fn sync_file(&self, path: &Path) -> io::Result<()> {
		let path = path.to_path_buf();
		blocking(move || sync_file_blocking(&path)).await
	}

	async fn sync_dir(&self, dir: &Path) -> io::Result<()> {
		let dir = dir.to_path_buf();
		blocking(move || sync_dir_blocking(&dir)).await
	}

	async fn hard_link(&self, src: &Path, dst: &Path) -> io::Result<()> {
		let (src, dst) = (src.to_path_buf(), dst.to_path_buf());
		blocking(move || std::fs::hard_link(&src, &dst)).await
	}

	async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
		let (from, to) = (from.to_path_buf(), to.to_path_buf());
		blocking(move || std::fs::rename(&from, &to)).await
	}

	async fn remove_file(&self, path: &Path) -> io::Result<()> {
		let path = path.to_path_buf();
		blocking(move || remove_file_blocking(&path)).await
	}

	async fn read_dir(&self, dir: &Path) -> io::Result<Vec<FsEntry>> {
		let dir = dir.to_path_buf();
		blocking(move || read_dir_blocking(&dir)).await
	}

	async fn metadata(&self, path: &Path) -> io::Result<FsMetadata> {
		let path = path.to_path_buf();
		blocking(move || metadata_blocking(&path)).await
	}
}

/// `create_new` + `write_all`, removing the file again if the write fails part way.
/// Shared with `SimFs`, which performs the real operation before recording it.
pub(super) fn create_new_write_blocking(path: &Path, bytes: &[u8]) -> io::Result<()> {
	let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
	if let Err(e) = file.write_all(bytes) {
		// Close first: Windows refuses to remove a file that is still open.
		drop(file);
		discard_own_file(path, &e);
		return Err(e);
	}
	Ok(())
}

/// Remove a file this call created and is abandoning after `cause`. A failure to
/// remove it is logged, not returned: the caller's error is the one that matters, and
/// the leftover file is crash litter that recovery quarantines (S12).
fn discard_own_file(path: &Path, cause: &io::Error) {
	if let Err(e) = remove_file_blocking(path) {
		tracing::warn!(path = %path.display(), cause = %cause, error = %e, "could not remove a partially written file; recovery will quarantine it");
	}
}

fn sync_file_blocking(path: &Path) -> io::Result<()> {
	let mut options = std::fs::OpenOptions::new();
	// fsync(2) needs no write access, but FlushFileBuffers on Windows does.
	if cfg!(windows) {
		options.write(true);
	} else {
		options.read(true);
	}
	options.open(path)?.sync_all()
}

#[cfg(unix)]
fn sync_dir_blocking(dir: &Path) -> io::Result<()> {
	std::fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir_blocking(dir: &Path) -> io::Result<()> {
	use std::sync::atomic::{AtomicBool, Ordering};

	static WARNED: AtomicBool = AtomicBool::new(false);
	if !WARNED.swap(true, Ordering::Relaxed) {
		tracing::warn!(dir = %dir.display(), "directory fsync is unavailable on this platform: new directory entries survive a process crash but not power loss");
	}
	Ok(())
}

pub(super) fn remove_file_blocking(path: &Path) -> io::Result<()> {
	match std::fs::remove_file(path) {
		Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
		other => other,
	}
}

pub(super) fn read_dir_blocking(dir: &Path) -> io::Result<Vec<FsEntry>> {
	let mut entries = Vec::new();
	for entry in std::fs::read_dir(dir)? {
		let entry = entry?;
		entries.push(FsEntry { name: entry.file_name(), is_dir: entry.file_type()?.is_dir() });
	}
	entries.sort();
	Ok(entries)
}

pub(super) fn metadata_blocking(path: &Path) -> io::Result<FsMetadata> {
	let meta = std::fs::metadata(path)?;
	Ok(FsMetadata { len: meta.len(), is_dir: meta.is_dir() })
}

/// Write a complete frame under a new final name and return its CRC trailer.
///
/// This is `create_new` at `dir/name`, `write_all`, and `sync_all` when `policy` is
/// [`SyncPolicy::Full`]. The returned value is the frame's trailing CRC-32, which the
/// index records as `frame_crc`.
///
/// There is no temporary name and no rename. The name is never reused (design
/// section 4), so `create_new` both proves nobody else owns it and makes a torn write
/// impossible to mistake for another writer's frame. The file is not reachable from
/// the index until a later commit names it, so a crash at any point here leaves at
/// most an unreferenced file for recovery.
///
/// This does not sync the directory entry. Callers batch that through
/// [`DirSyncer`](super::DirSyncer) so that concurrent writers share one directory
/// fsync (SEAL-5, M4).
///
/// The CRC is read from the trailer the encoder wrote rather than recomputed: the
/// encoder has just computed it over the same bytes, and readers verify it on decode.
///
/// # Errors
///
/// - `InvalidInput`, before anything is created, if `name` is not a single plain path
///   component or `bytes` is too short to end in a 4-byte CRC trailer;
/// - `AlreadyExists` if `dir/name` exists, which is left untouched;
/// - any create, write or sync error. On every error except `AlreadyExists`, no file
///   is left at `dir/name`.
pub async fn write_new_durable<F>(fs: &F, dir: &Path, name: &str, bytes: Vec<u8>, policy: SyncPolicy) -> io::Result<u32>
where
	F: StoreFs + ?Sized,
{
	check_file_name(name)?;
	let crc = trailer_crc(&bytes)?;
	let path = dir.join(name);
	fs.create_new_write(&path, bytes).await?;
	if policy == SyncPolicy::Full {
		if let Err(e) = fs.sync_file(&path).await {
			// The file is ours (create_new succeeded), so removing it cannot touch anyone
			// else's data. Its contents were never acknowledged as durable.
			if let Err(remove_err) = fs.remove_file(&path).await {
				tracing::warn!(path = %path.display(), cause = %e, error = %remove_err, "could not remove a frame whose fsync failed; recovery will quarantine it");
			}
			return Err(e);
		}
	}
	Ok(crc)
}

/// The CRC-32 a `.weftseg` frame stores in its last four bytes, little-endian
/// (weft-physical-type `write_segment`).
fn trailer_crc(bytes: &[u8]) -> io::Result<u32> {
	match bytes.len().checked_sub(4).map(|at| &bytes[at..]) {
		Some(&[a, b, c, d]) => Ok(u32::from_le_bytes([a, b, c, d])),
		_ => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("a frame ends in a 4-byte CRC-32 trailer, but only {} bytes were given", bytes.len()))),
	}
}

/// Accept only a single, plain path component, so a name can never escape `dir`.
/// Frame names are `enc`-escaped (design section 4) and contain no separator; both
/// separators are rejected on every platform so a name valid on Unix is also safe on
/// Windows.
fn check_file_name(name: &str) -> io::Result<()> {
	if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
		return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{name:?} is not a plain file name")));
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use std::sync::{
		atomic::{AtomicUsize, Ordering}, Arc
	};

	use weft_physical_type::crc32;

	use super::*;

	/// A frame-shaped byte string: a body followed by its CRC-32, little-endian.
	fn frame(body: &[u8]) -> Vec<u8> {
		let mut bytes = body.to_vec();
		bytes.extend_from_slice(&crc32(body).to_le_bytes());
		bytes
	}

	/// [`RealFs`], except that `sync_file` always fails, as an fsync EIO would.
	#[derive(Debug, Default)]
	struct SyncFailsFs {
		removes: AtomicUsize,
	}

	#[async_trait]
	impl StoreFs for SyncFailsFs {
		async fn create_new_write(&self, path: &Path, bytes: Vec<u8>) -> io::Result<()> {
			RealFs.create_new_write(path, bytes).await
		}

		async fn sync_file(&self, _path: &Path) -> io::Result<()> {
			Err(io::Error::other("injected fsync failure"))
		}

		async fn sync_dir(&self, dir: &Path) -> io::Result<()> {
			RealFs.sync_dir(dir).await
		}

		async fn hard_link(&self, src: &Path, dst: &Path) -> io::Result<()> {
			RealFs.hard_link(src, dst).await
		}

		async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
			RealFs.rename(from, to).await
		}

		async fn remove_file(&self, path: &Path) -> io::Result<()> {
			self.removes.fetch_add(1, Ordering::SeqCst);
			RealFs.remove_file(path).await
		}

		async fn read_dir(&self, dir: &Path) -> io::Result<Vec<FsEntry>> {
			RealFs.read_dir(dir).await
		}

		async fn metadata(&self, path: &Path) -> io::Result<FsMetadata> {
			RealFs.metadata(path).await
		}
	}

	#[tokio::test]
	async fn write_new_durable_writes_the_bytes_and_returns_the_trailer_crc() {
		let dir = tempfile::tempdir().unwrap();
		let bytes = frame(b"WEFTSEG body");
		for policy in [SyncPolicy::Full, SyncPolicy::None] {
			let name = format!("a~g{}~p1.weftseg", u8::from(policy == SyncPolicy::Full));
			let crc = write_new_durable(&RealFs, dir.path(), &name, bytes.clone(), policy).await.unwrap();
			assert_eq!(crc, crc32(b"WEFTSEG body"), "the returned CRC is the frame's trailer");
			assert_eq!(std::fs::read(dir.path().join(&name)).unwrap(), bytes);
		}
	}

	#[tokio::test]
	async fn write_new_durable_refuses_an_existing_name_and_leaves_it_untouched() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("a~g1~p1.weftseg");
		std::fs::write(&path, b"someone else's frame").unwrap();

		let fs: Arc<dyn StoreFs> = Arc::new(RealFs);
		let err = write_new_durable(fs.as_ref(), dir.path(), "a~g1~p1.weftseg", frame(b"mine"), SyncPolicy::Full).await.unwrap_err();
		assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
		assert_eq!(std::fs::read(&path).unwrap(), b"someone else's frame", "create_new never truncates or removes another writer's file");
	}

	#[tokio::test]
	async fn write_new_durable_leaves_no_file_when_the_fsync_fails() {
		let dir = tempfile::tempdir().unwrap();
		let fs = SyncFailsFs::default();
		let err = write_new_durable(&fs, dir.path(), "a~g1~p1.weftseg", frame(b"body"), SyncPolicy::Full).await.unwrap_err();
		assert_eq!(err.to_string(), "injected fsync failure", "the fsync error is the one returned");
		assert_eq!(fs.removes.load(Ordering::SeqCst), 1, "the half-made frame was removed");
		assert!(!dir.path().join("a~g1~p1.weftseg").exists(), "no file is left behind on error");

		// Under SyncPolicy::None the fsync never runs, so the same filesystem succeeds.
		write_new_durable(&fs, dir.path(), "a~g2~p2.weftseg", frame(b"body"), SyncPolicy::None).await.unwrap();
		assert!(dir.path().join("a~g2~p2.weftseg").exists());
	}

	#[tokio::test]
	async fn write_new_durable_rejects_bad_input_before_creating_anything() {
		let dir = tempfile::tempdir().unwrap();
		for name in ["", ".", "..", "../escape", "a/b", "a\\b", "nul\0byte"] {
			let err = write_new_durable(&RealFs, dir.path(), name, frame(b"body"), SyncPolicy::Full).await.unwrap_err();
			assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{name:?} must be rejected");
		}
		let err = write_new_durable(&RealFs, dir.path(), "short.weftseg", vec![1, 2, 3], SyncPolicy::Full).await.unwrap_err();
		assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "three bytes cannot hold a CRC trailer");
		assert!(RealFs.read_dir(dir.path()).await.unwrap().is_empty(), "nothing was created");
		assert!(!dir.path().parent().unwrap().join("escape").exists());

		let missing = dir.path().join("no-such-dir");
		let err = write_new_durable(&RealFs, &missing, "a.weftseg", frame(b"body"), SyncPolicy::Full).await.unwrap_err();
		assert_eq!(err.kind(), io::ErrorKind::NotFound);
	}

	#[tokio::test]
	async fn real_fs_operations_behave_as_documented() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		std::fs::create_dir(root.join("quarantine")).unwrap();

		RealFs.create_new_write(&root.join("b"), b"bee".to_vec()).await.unwrap();
		assert_eq!(RealFs.create_new_write(&root.join("b"), b"again".to_vec()).await.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
		RealFs.sync_file(&root.join("b")).await.unwrap();
		RealFs.sync_dir(root).await.unwrap();
		assert_eq!(RealFs.metadata(&root.join("b")).await.unwrap(), FsMetadata { len: 3, is_dir: false });
		assert!(RealFs.metadata(&root.join("quarantine")).await.unwrap().is_dir);
		assert_eq!(RealFs.metadata(&root.join("nope")).await.unwrap_err().kind(), io::ErrorKind::NotFound);

		RealFs.hard_link(&root.join("b"), &root.join("a")).await.unwrap();
		RealFs.rename(&root.join("b"), &root.join("c")).await.unwrap();
		let names: Vec<_> = RealFs.read_dir(root).await.unwrap().into_iter().map(|e| (e.name.into_string().unwrap(), e.is_dir)).collect();
		assert_eq!(names, vec![("a".into(), false), ("c".into(), false), ("quarantine".into(), true)], "sorted, with directories flagged");
		assert_eq!(std::fs::read(root.join("a")).unwrap(), b"bee");

		RealFs.remove_file(&root.join("c")).await.unwrap();
		RealFs.remove_file(&root.join("c")).await.unwrap();
		assert!(!root.join("c").exists(), "removing a missing file is a success, so a removal can be replayed");
	}
}
