//! A Turso I/O backend for tests that records what Turso does to each file: which
//! bytes it writes and when it fsyncs. `SimFs` cannot see this, because Turso does its
//! own I/O and never goes through [`StoreFs`](super::StoreFs). The open's MVCC header
//! tests (`control_plane`, `segment_index`) read it.
//!
//! [`ProbeIo`] wraps the platform backend Turso would use anyway and delegates every
//! call to it unchanged, so a database opened on it behaves exactly as in production;
//! it only appends a [`FileEvent`] for each write, sync and truncate it passes on. A test
//! can also hold one sync ([`ProbeIo::hold_next_sync`]) to stop Turso inside a step,
//! with whatever locks that step holds still held.

use std::{
	path::Path, ptr::NonNull, sync::{mpsc, Arc, Mutex}, time::Duration
};

use turso::core::{
	io::{FileId, FileSyncType, SharedWalLockKind, SharedWalMappedRegion}, Buffer, Clock, Completion, File, MemoryIO, MonotonicInstant, OpenFlags, PlatformIO, WallClockInstant, IO
};

/// One thing Turso did to a file, named by its file name (`segment_index.db`,
/// `segment_index.db-log`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileEvent {
	/// `len` bytes written at `pos`.
	Write { file: String, pos: u64, len: usize },
	/// An fsync issued.
	Sync { file: String },
	/// A truncate to `len`.
	Truncate { file: String, len: u64 },
}

impl FileEvent {
	/// The file the event happened to.
	pub fn file(&self) -> &str {
		match self {
			Self::Write { file, .. } | Self::Sync { file } | Self::Truncate { file, .. } => file,
		}
	}
}

/// A [`PlatformIO`] that records a [`FileEvent`] for every write, sync and truncate.
pub struct ProbeIo {
	inner: Arc<dyn IO>,
	events: Arc<Mutex<Vec<FileEvent>>>,
	held: Arc<Mutex<Option<HeldSync>>>,
}

/// The sync [`ProbeIo::hold_next_sync`] armed: the file, and the two ends of the hold.
struct HeldSync {
	file: String,
	reached: mpsc::Sender<()>,
	release: mpsc::Receiver<()>,
}

/// A sync held by [`ProbeIo::hold_next_sync`]. Dropping it releases the sync.
pub struct SyncHold {
	reached: mpsc::Receiver<()>,
	release: mpsc::Sender<()>,
}

impl SyncHold {
	/// Wait, for at most `timeout`, until a thread is held in the sync. Whether one is.
	pub fn reached(&self, timeout: Duration) -> bool {
		self.reached.recv_timeout(timeout).is_ok()
	}

	/// Let the held sync go ahead (now, or as soon as it is reached).
	pub fn release(&self) {
		let _ = self.release.send(());
	}
}

impl ProbeIo {
	/// The platform backend, recording.
	pub fn new() -> turso::core::Result<Arc<Self>> {
		Ok(Arc::new(Self { inner: Arc::new(PlatformIO::new()?), events: Arc::default(), held: Arc::default() }))
	}

	/// Hold the next sync of `file`: the thread that issues it blocks, before the sync
	/// reaches the platform, until [`SyncHold::release`] (or the hold drops). Turso stays
	/// inside the step that synced, so a test can act while that step's locks are held:
	/// a TRUNCATE checkpoint, say, which holds MVCC's stop-the-world gate.
	pub fn hold_next_sync(&self, file: &str) -> SyncHold {
		let (reached_tx, reached) = mpsc::channel();
		let (release, release_rx) = mpsc::channel();
		if let Ok(mut held) = self.held.lock() {
			*held = Some(HeldSync { file: file.to_string(), reached: reached_tx, release: release_rx });
		}
		SyncHold { reached, release }
	}

	/// Every event so far, in the order Turso issued them.
	pub fn events(&self) -> Vec<FileEvent> {
		self.events.lock().map(|events| events.clone()).unwrap_or_default()
	}

	/// The writes to `file` so far, as `(pos, len)`, in order.
	pub fn writes(&self, file: &str) -> Vec<(u64, usize)> {
		self.events()
			.into_iter()
			.filter_map(|event| match event {
				FileEvent::Write { file: written, pos, len } if written == file => Some((pos, len)),
				_ => None,
			})
			.collect()
	}

	/// Whether Turso has fsynced `file` since its last write to it: what a power cut now
	/// would keep of the file is everything it wrote. True for a file it never wrote.
	pub fn synced_since_last_write(&self, file: &str) -> bool {
		let mine: Vec<FileEvent> = self.events().into_iter().filter(|event| event.file() == file).collect();
		mine.iter().rposition(|event| matches!(event, FileEvent::Write { .. })).is_none_or(|last| mine[last..].iter().any(|event| matches!(event, FileEvent::Sync { .. })))
	}

	fn wrap(&self, path: &str, file: Arc<dyn File>) -> Arc<dyn File> {
		let name = Path::new(path).file_name().map_or_else(|| path.to_string(), |name| name.to_string_lossy().into_owned());
		Arc::new(ProbeFile { inner: file, name, events: self.events.clone(), held: self.held.clone() })
	}
}

impl Clock for ProbeIo {
	fn current_time_monotonic(&self) -> MonotonicInstant {
		self.inner.current_time_monotonic()
	}

	fn current_time_wall_clock(&self) -> WallClockInstant {
		self.inner.current_time_wall_clock()
	}
}

impl IO for ProbeIo {
	fn open_file(&self, path: &str, flags: OpenFlags, direct: bool) -> turso::core::Result<Arc<dyn File>> {
		Ok(self.wrap(path, self.inner.open_file(path, flags, direct)?))
	}

	fn open_shared_wal_file(&self, path: &str) -> turso::core::Result<Arc<dyn File>> {
		Ok(self.wrap(path, self.inner.open_shared_wal_file(path)?))
	}

	fn remove_file(&self, path: &str) -> turso::core::Result<()> {
		self.inner.remove_file(path)
	}

	fn supports_shared_wal_coordination(&self) -> bool {
		self.inner.supports_shared_wal_coordination()
	}

	fn step(&self) -> turso::core::Result<()> {
		self.inner.step()
	}

	fn cancel(&self, c: &[Completion]) -> turso::core::Result<()> {
		self.inner.cancel(c)
	}

	fn drain_completions(&self, completions: &[Completion]) -> turso::core::Result<()> {
		self.inner.drain_completions(completions)
	}

	fn wait_for_completion(&self, c: Completion) -> turso::core::Result<()> {
		self.inner.wait_for_completion(c)
	}

	fn generate_random_number(&self) -> i64 {
		self.inner.generate_random_number()
	}

	fn fill_bytes(&self, dest: &mut [u8]) {
		self.inner.fill_bytes(dest);
	}

	fn get_memory_io(&self) -> Arc<MemoryIO> {
		self.inner.get_memory_io()
	}

	fn register_fixed_buffer(&self, ptr: NonNull<u8>, len: usize) -> turso::core::Result<u32> {
		self.inner.register_fixed_buffer(ptr, len)
	}

	fn yield_now(&self) {
		self.inner.yield_now();
	}

	fn sleep(&self, duration: std::time::Duration) {
		self.inner.sleep(duration);
	}

	fn file_id(&self, path: &str) -> turso::core::Result<FileId> {
		self.inner.file_id(path)
	}
}

/// A file of a [`ProbeIo`]: the platform file, recording.
struct ProbeFile {
	inner: Arc<dyn File>,
	name: String,
	events: Arc<Mutex<Vec<FileEvent>>>,
	held: Arc<Mutex<Option<HeldSync>>>,
}

impl ProbeFile {
	fn record(&self, event: FileEvent) {
		if let Ok(mut events) = self.events.lock() {
			events.push(event);
		}
	}

	/// If [`ProbeIo::hold_next_sync`] armed a hold of this file, take it (it holds one
	/// sync) and block until it is released.
	fn wait_if_held(&self) {
		let held = self.held.lock().ok().and_then(|mut held| if held.as_ref().is_some_and(|held| held.file == self.name) { held.take() } else { None });
		if let Some(held) = held {
			let _ = held.reached.send(());
			let _ = held.release.recv();
		}
	}
}

impl File for ProbeFile {
	fn lock_file(&self, exclusive: bool) -> turso::core::Result<()> {
		self.inner.lock_file(exclusive)
	}

	fn unlock_file(&self) -> turso::core::Result<()> {
		self.inner.unlock_file()
	}

	fn pread(&self, pos: u64, c: Completion) -> turso::core::Result<Completion> {
		self.inner.pread(pos, c)
	}

	fn pwrite(&self, pos: u64, buffer: Arc<Buffer>, c: Completion) -> turso::core::Result<Completion> {
		self.record(FileEvent::Write { file: self.name.clone(), pos, len: buffer.len() });
		self.inner.pwrite(pos, buffer, c)
	}

	fn sync(&self, c: Completion, sync_type: FileSyncType) -> turso::core::Result<Completion> {
		self.wait_if_held();
		self.record(FileEvent::Sync { file: self.name.clone() });
		self.inner.sync(c, sync_type)
	}

	fn pwritev(&self, pos: u64, buffers: Vec<Arc<Buffer>>, c: Completion) -> turso::core::Result<Completion> {
		self.record(FileEvent::Write { file: self.name.clone(), pos, len: buffers.iter().map(|buffer| buffer.len()).sum() });
		self.inner.pwritev(pos, buffers, c)
	}

	fn size(&self) -> turso::core::Result<u64> {
		self.inner.size()
	}

	fn truncate(&self, len: u64, c: Completion) -> turso::core::Result<Completion> {
		self.record(FileEvent::Truncate { file: self.name.clone(), len });
		self.inner.truncate(len, c)
	}

	fn has_hole(&self, pos: usize, len: usize) -> turso::core::Result<bool> {
		self.inner.has_hole(pos, len)
	}

	fn punch_hole(&self, pos: usize, len: usize) -> turso::core::Result<()> {
		self.inner.punch_hole(pos, len)
	}

	fn shared_wal_lock_byte(&self, offset: u64, exclusive: bool, kind: SharedWalLockKind) -> turso::core::Result<()> {
		self.inner.shared_wal_lock_byte(offset, exclusive, kind)
	}

	fn shared_wal_try_lock_byte(&self, offset: u64, exclusive: bool, kind: SharedWalLockKind) -> turso::core::Result<bool> {
		self.inner.shared_wal_try_lock_byte(offset, exclusive, kind)
	}

	fn shared_wal_probe_exclusive_byte(&self, offset: u64, kind: SharedWalLockKind) -> turso::core::Result<bool> {
		self.inner.shared_wal_probe_exclusive_byte(offset, kind)
	}

	fn shared_wal_probe_exclusive_while_shared_byte(&self, offset: u64, kind: SharedWalLockKind) -> turso::core::Result<bool> {
		self.inner.shared_wal_probe_exclusive_while_shared_byte(offset, kind)
	}

	fn shared_wal_unlock_byte(&self, offset: u64, kind: SharedWalLockKind) -> turso::core::Result<()> {
		self.inner.shared_wal_unlock_byte(offset, kind)
	}

	fn shared_wal_set_len(&self, len: u64) -> turso::core::Result<()> {
		self.inner.shared_wal_set_len(len)
	}

	fn shared_wal_map(&self, offset: u64, len: usize) -> turso::core::Result<Box<dyn SharedWalMappedRegion>> {
		self.inner.shared_wal_map(offset, len)
	}
}
