//! A [`StoreFs`] test double for the backup tests: the real filesystem, with every
//! directory-level operation recorded in order, and `remove_dir_all` optionally failing.
//!
//! The ordering is the point. A prune is crash-safe only because it renames a backup
//! away, and makes that rename durable, before it removes a single file; the end state
//! of a successful prune is the same either way, so only the recorded order can show a
//! regression to an in-place removal.

use std::{
	io, path::{Path, PathBuf}, sync::{Mutex, PoisonError}
};

use weftdb::durable::{FsEntry, FsMetadata, RealFs, StoreFs, SyncPolicy, WritePoints};

/// The error message [`RecordingFs::failing_remove_dir_all`] returns.
pub const INJECTED_CLEANUP_FAILURE: &str = "injected cleanup failure";

/// A directory-level operation a [`RecordingFs`] saw, with its paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsOp {
	CreateDir(PathBuf),
	Rename(PathBuf, PathBuf),
	SyncDir(PathBuf),
	RemoveFile(PathBuf),
	RemoveDirAll(PathBuf),
}

/// The real filesystem, recording each [`FsOp`] before performing it.
#[derive(Debug, Default)]
pub struct RecordingFs {
	ops: Mutex<Vec<FsOp>>,
	fail_remove_dir_all: bool,
}

impl RecordingFs {
	/// One whose `remove_dir_all` is recorded and then fails with
	/// [`INJECTED_CLEANUP_FAILURE`], removing nothing.
	pub fn failing_remove_dir_all() -> Self {
		Self { fail_remove_dir_all: true, ..Self::default() }
	}

	/// Every operation recorded so far, in order.
	pub fn ops(&self) -> Vec<FsOp> {
		self.ops.lock().unwrap_or_else(PoisonError::into_inner).clone()
	}

	fn record(&self, op: FsOp) {
		self.ops.lock().unwrap_or_else(PoisonError::into_inner).push(op);
	}
}

#[async_trait::async_trait]
impl StoreFs for RecordingFs {
	async fn create_new_write(&self, path: &Path, bytes: Vec<u8>, policy: SyncPolicy, points: WritePoints) -> io::Result<()> {
		RealFs.create_new_write(path, bytes, policy, points).await
	}

	async fn sync_file(&self, path: &Path) -> io::Result<()> {
		RealFs.sync_file(path).await
	}

	async fn sync_dir(&self, dir: &Path) -> io::Result<()> {
		self.record(FsOp::SyncDir(dir.to_path_buf()));
		RealFs.sync_dir(dir).await
	}

	async fn hard_link(&self, src: &Path, dst: &Path) -> io::Result<()> {
		RealFs.hard_link(src, dst).await
	}

	async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
		self.record(FsOp::Rename(from.to_path_buf(), to.to_path_buf()));
		RealFs.rename(from, to).await
	}

	async fn remove_file(&self, path: &Path) -> io::Result<()> {
		self.record(FsOp::RemoveFile(path.to_path_buf()));
		RealFs.remove_file(path).await
	}

	async fn create_dir(&self, path: &Path) -> io::Result<()> {
		self.record(FsOp::CreateDir(path.to_path_buf()));
		RealFs.create_dir(path).await
	}

	async fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
		self.record(FsOp::RemoveDirAll(path.to_path_buf()));
		if self.fail_remove_dir_all {
			return Err(io::Error::new(io::ErrorKind::PermissionDenied, INJECTED_CLEANUP_FAILURE));
		}
		RealFs.remove_dir_all(path).await
	}

	async fn copy_new(&self, src: &Path, dst: &Path, policy: SyncPolicy) -> io::Result<u64> {
		RealFs.copy_new(src, dst, policy).await
	}

	async fn read_dir(&self, dir: &Path) -> io::Result<Vec<FsEntry>> {
		RealFs.read_dir(dir).await
	}

	async fn metadata(&self, path: &Path) -> io::Result<FsMetadata> {
		RealFs.metadata(path).await
	}
}
