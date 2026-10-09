//! Coalesced directory fsyncs (design section 5.1, `DirSyncer`).
//!
//! A new frame is durable only once its directory entry is, and fsyncing `segments/`
//! after every frame would serialise every writer on one slow syscall. [`DirSyncer`]
//! keeps a single fsync in flight and lets every caller that arrives while it runs
//! share the next one, so N concurrent writers cost at most N directory fsyncs and
//! usually two.

use std::{
	fmt, io, path::{Path, PathBuf}, sync::{Arc, Mutex, MutexGuard, PoisonError}
};

use tokio::sync::Notify;

use super::fs::StoreFs;

/// Shares directory fsyncs between concurrent writers.
///
/// Correctness rests on two generation counters. Each [`sync`](Self::sync) call takes
/// the next *requested* generation. An fsync started when the requested counter reads
/// `g` covers every call up to `g`, because it began after each of them had registered,
/// and so after each caller's own file operations had finished. A call returns only
/// once the *completed* counter reaches its generation, so an fsync that was already
/// in flight when the call arrived never satisfies it.
///
/// The fsync runs in its own task, so a caller that is cancelled while it waits
/// cannot strand the others. An fsync error poisons the syncer for good: after a
/// failed fsync the kernel may have dropped the dirty entries, so a later success
/// would not prove they reached the disk (fsyncgate). Callers treat the error as
/// store-poisoning (S6).
#[derive(Clone)]
pub struct DirSyncer {
	inner: Arc<Inner>,
}

struct Inner {
	fs: Arc<dyn StoreFs>,
	dir: PathBuf,
	state: Mutex<State>,
	/// Woken whenever an fsync finishes, successfully or not.
	finished: Notify,
}

#[derive(Default)]
struct State {
	/// The generation handed to the most recent caller.
	requested: u64,
	/// Every caller with a generation at or below this is covered by a finished fsync.
	completed: u64,
	in_flight: bool,
	poisoned: Option<Poison>,
}

/// The first fsync failure, replayed to every later caller.
#[derive(Clone)]
struct Poison {
	kind: io::ErrorKind,
	message: String,
}

impl DirSyncer {
	/// A syncer for directory `dir` on `fs`. One per directory: two syncers on the same
	/// directory would each keep their own fsync in flight.
	pub fn new(fs: Arc<dyn StoreFs>, dir: impl Into<PathBuf>) -> Self {
		Self { inner: Arc::new(Inner { fs, dir: dir.into(), state: Mutex::new(State::default()), finished: Notify::new() }) }
	}

	/// The directory this syncer fsyncs.
	#[must_use]
	pub fn dir(&self) -> &Path {
		&self.inner.dir
	}

	/// Whether an fsync has failed, so that every call now errors.
	#[must_use]
	pub fn is_poisoned(&self) -> bool {
		self.inner.lock().poisoned.is_some()
	}

	/// Return once an fsync of the directory that *began after this call* has
	/// completed, so every entry the caller created, renamed or removed before calling
	/// is durable.
	///
	/// # Errors
	///
	/// The fsync's error if the covering fsync failed, or the first failure's kind and
	/// message if the syncer was already poisoned.
	pub async fn sync(&self) -> io::Result<()> {
		let generation = {
			let mut state = self.inner.lock();
			if let Some(poison) = &state.poisoned {
				return Err(poison.to_error(&self.inner.dir));
			}
			state.requested += 1;
			state.requested
		};
		loop {
			// Register for the wake-up before inspecting the state, so an fsync that
			// finishes between the check and the await cannot be missed.
			let finished = self.inner.finished.notified();
			tokio::pin!(finished);
			finished.as_mut().enable();
			let flight = {
				let mut state = self.inner.lock();
				// A covering fsync that succeeded counts even if a later one failed.
				if state.completed >= generation {
					return Ok(());
				}
				if let Some(poison) = &state.poisoned {
					return Err(poison.to_error(&self.inner.dir));
				}
				(!state.in_flight).then(|| Flight::begin(&self.inner, &mut state))
			};
			// Spawned outside the lock: a task refused by a shutting-down runtime is
			// dropped on the spot, and the flight's drop takes the lock.
			if let Some(flight) = flight {
				tokio::spawn(flight.run());
			}
			finished.await;
		}
	}

	/// The most recently requested generation (tests use it to see who has arrived).
	#[cfg(test)]
	fn requested(&self) -> u64 {
		self.inner.lock().requested
	}
}

impl fmt::Debug for DirSyncer {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let state = self.inner.lock();
		f.debug_struct("DirSyncer").field("dir", &self.inner.dir).field("requested", &state.requested).field("completed", &state.completed).field("in_flight", &state.in_flight).field("poisoned", &state.poisoned.as_ref().map(|p| &p.message)).finish_non_exhaustive()
	}
}

impl Inner {
	fn lock(&self) -> MutexGuard<'_, State> {
		// The state is a handful of counters updated in single statements, so it is
		// consistent even if a holder panicked.
		self.state.lock().unwrap_or_else(PoisonError::into_inner)
	}
}

/// One in-flight fsync. Its result is published when it drops, so that a panic in
/// the filesystem or a runtime shutdown still clears `in_flight` and wakes the
/// waiters (as a failure) instead of leaving them parked forever.
struct Flight {
	inner: Arc<Inner>,
	covers: u64,
	outcome: Option<io::Result<()>>,
}

impl Flight {
	/// Claim the single in-flight slot for an fsync covering every generation
	/// requested so far.
	fn begin(inner: &Arc<Inner>, state: &mut State) -> Self {
		state.in_flight = true;
		Self { inner: Arc::clone(inner), covers: state.requested, outcome: None }
	}

	async fn run(mut self) {
		self.outcome = Some(self.inner.fs.sync_dir(&self.inner.dir).await);
	}
}

impl Drop for Flight {
	fn drop(&mut self) {
		let outcome = self.outcome.take().unwrap_or_else(|| Err(io::Error::other("the directory fsync task ended without a result")));
		{
			let mut state = self.inner.lock();
			state.in_flight = false;
			match outcome {
				Ok(()) => state.completed = state.completed.max(self.covers),
				Err(e) => {
					if state.poisoned.is_none() {
						tracing::error!(dir = %self.inner.dir.display(), error = %e, "directory fsync failed; refusing further durable writes until restart");
						state.poisoned = Some(Poison { kind: e.kind(), message: e.to_string() });
					}
				}
			}
		}
		self.inner.finished.notify_waiters();
	}
}

impl Poison {
	fn to_error(&self, dir: &Path) -> io::Error {
		io::Error::new(self.kind, format!("directory fsync of {} failed ({}); the directory syncer is poisoned until restart", dir.display(), self.message))
	}
}

#[cfg(test)]
mod tests {
	use std::{
		sync::atomic::{AtomicU64, AtomicUsize, Ordering}, time::Duration
	};

	use async_trait::async_trait;
	use tokio::sync::Barrier;

	use super::*;
	use crate::types::durable::fs::{FsEntry, FsMetadata, RealFs, SyncPolicy, WritePoints};

	/// A [`StoreFs`] whose `sync_dir` only records itself: when each fsync began and
	/// ended on a shared logical clock, after an optional delay or gate, and optionally
	/// fails. Every other method is unused by the syncer.
	#[derive(Debug, Default)]
	struct ProbeFs {
		clock: AtomicU64,
		/// `(began, ended)` clock readings of every fsync.
		fsyncs: Mutex<Vec<(u64, u64)>>,
		started: AtomicUsize,
		delay: Option<Duration>,
		/// When set, the first fsync waits here until the test releases it.
		gate: Option<Arc<Notify>>,
		fail: bool,
	}

	impl ProbeFs {
		fn tick(&self) -> u64 {
			self.clock.fetch_add(1, Ordering::SeqCst)
		}

		fn fsync_count(&self) -> usize {
			self.fsyncs.lock().unwrap().len()
		}
	}

	#[async_trait]
	impl StoreFs for ProbeFs {
		async fn create_new_write(&self, _: &Path, _: Vec<u8>, _: SyncPolicy, _: WritePoints) -> io::Result<()> {
			unreachable!()
		}

		async fn sync_file(&self, _: &Path) -> io::Result<()> {
			unreachable!()
		}

		async fn sync_dir(&self, _: &Path) -> io::Result<()> {
			let began = self.tick();
			let nth = self.started.fetch_add(1, Ordering::SeqCst);
			if let (0, Some(gate)) = (nth, &self.gate) {
				gate.notified().await;
			}
			if let Some(delay) = self.delay {
				tokio::time::sleep(delay).await;
			}
			let ended = self.tick();
			self.fsyncs.lock().unwrap().push((began, ended));
			if self.fail {
				return Err(io::Error::new(io::ErrorKind::StorageFull, "injected directory fsync failure"));
			}
			Ok(())
		}

		async fn hard_link(&self, _: &Path, _: &Path) -> io::Result<()> {
			unreachable!()
		}

		async fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
			unreachable!()
		}

		async fn remove_file(&self, _: &Path) -> io::Result<()> {
			unreachable!()
		}

		async fn create_dir(&self, _: &Path) -> io::Result<()> {
			unreachable!()
		}

		async fn remove_dir_all(&self, _: &Path) -> io::Result<()> {
			unreachable!()
		}

		async fn copy_new(&self, _: &Path, _: &Path, _: SyncPolicy) -> io::Result<u64> {
			unreachable!()
		}

		async fn read_dir(&self, _: &Path) -> io::Result<Vec<FsEntry>> {
			unreachable!()
		}

		async fn metadata(&self, _: &Path) -> io::Result<FsMetadata> {
			unreachable!()
		}
	}

	async fn wait_until(mut condition: impl FnMut() -> bool) {
		tokio::time::timeout(Duration::from_secs(10), async {
			while !condition() {
				tokio::time::sleep(Duration::from_millis(1)).await;
			}
		})
		.await
		.expect("condition not reached within 10 s");
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
	async fn concurrent_callers_each_return_after_a_covering_fsync() {
		const CALLERS: usize = 32;
		let fs = Arc::new(ProbeFs { delay: Some(Duration::from_millis(5)), ..ProbeFs::default() });
		let syncer = DirSyncer::new(fs.clone(), "segments");
		let barrier = Arc::new(Barrier::new(CALLERS));

		let mut tasks = Vec::new();
		for _ in 0..CALLERS {
			let (fs, syncer, barrier) = (fs.clone(), syncer.clone(), barrier.clone());
			tasks.push(tokio::spawn(async move {
				barrier.wait().await;
				let called = fs.tick();
				syncer.sync().await.unwrap();
				(called, fs.tick())
			}));
		}
		let mut calls = Vec::new();
		for task in tasks {
			calls.push(task.await.unwrap());
		}

		let fsyncs = fs.fsyncs.lock().unwrap().clone();
		assert!(!fsyncs.is_empty() && fsyncs.len() <= CALLERS, "{CALLERS} callers triggered {} fsyncs", fsyncs.len());
		for (called, returned) in calls {
			assert!(fsyncs.iter().any(|&(began, ended)| called < began && ended < returned), "a caller (called at {called}, returned at {returned}) returned without an fsync that began after its call: {fsyncs:?}");
		}
		let mut ordered = fsyncs;
		ordered.sort_unstable();
		assert!(ordered.windows(2).all(|w| w[0].1 < w[1].0), "fsyncs never overlap: {ordered:?}");
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
	async fn callers_behind_an_in_flight_fsync_share_the_next_one() {
		let gate = Arc::new(Notify::new());
		let fs = Arc::new(ProbeFs { gate: Some(gate.clone()), ..ProbeFs::default() });
		let syncer = DirSyncer::new(fs.clone(), "segments");

		let first = tokio::spawn({
			let syncer = syncer.clone();
			async move { syncer.sync().await }
		});
		wait_until(|| fs.started.load(Ordering::SeqCst) == 1).await;

		// 31 callers arrive while fsync #1 is parked. It began before they registered,
		// so it must not release any of them.
		let late: Vec<_> = (0..31)
			.map(|_| {
				let syncer = syncer.clone();
				tokio::spawn(async move { syncer.sync().await })
			})
			.collect();
		wait_until(|| syncer.requested() == 32).await;
		tokio::time::sleep(Duration::from_millis(20)).await;
		assert!(late.iter().all(|t| !t.is_finished()), "no late caller returns on an fsync that began before it");

		gate.notify_one();
		first.await.unwrap().unwrap();
		for task in late {
			task.await.unwrap().unwrap();
		}
		assert_eq!(fs.fsync_count(), 2, "the 31 late callers share a single fsync");
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
	async fn an_fsync_error_poisons_every_later_call() {
		let fs = Arc::new(ProbeFs { fail: true, delay: Some(Duration::from_millis(5)), ..ProbeFs::default() });
		let syncer = DirSyncer::new(fs.clone(), "segments");

		let waiters: Vec<_> = (0..4)
			.map(|_| {
				let syncer = syncer.clone();
				tokio::spawn(async move { syncer.sync().await })
			})
			.collect();
		for waiter in waiters {
			let err = waiter.await.unwrap().unwrap_err();
			assert_eq!(err.kind(), io::ErrorKind::StorageFull, "the fsync's error kind reaches every waiter: {err}");
		}
		assert!(syncer.is_poisoned());
		let fsyncs_before = fs.fsync_count();

		let err = syncer.sync().await.unwrap_err();
		assert_eq!(err.kind(), io::ErrorKind::StorageFull);
		assert!(err.to_string().contains("injected directory fsync failure") && err.to_string().contains("poisoned"), "{err}");
		assert_eq!(fs.fsync_count(), fsyncs_before, "a poisoned syncer does not fsync again: a retry could falsely succeed");
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
	async fn a_cancelled_caller_does_not_strand_the_others() {
		let gate = Arc::new(Notify::new());
		let fs = Arc::new(ProbeFs { gate: Some(gate.clone()), ..ProbeFs::default() });
		let syncer = DirSyncer::new(fs.clone(), "segments");

		// The caller that starts fsync #1 is cancelled while it is parked.
		let leader = tokio::spawn({
			let syncer = syncer.clone();
			async move { syncer.sync().await }
		});
		wait_until(|| fs.started.load(Ordering::SeqCst) == 1).await;
		leader.abort();
		assert!(leader.await.unwrap_err().is_cancelled());

		let follower = tokio::spawn({
			let syncer = syncer.clone();
			async move { syncer.sync().await }
		});
		gate.notify_one();
		tokio::time::timeout(Duration::from_secs(10), follower).await.expect("the follower is not stranded").unwrap().unwrap();
		assert_eq!(fs.fsync_count(), 2, "the cancelled caller's fsync still ran, and the follower got its own");
	}

	#[tokio::test]
	async fn syncs_a_real_directory() {
		let dir = tempfile::tempdir().unwrap();
		let syncer = DirSyncer::new(Arc::new(RealFs), dir.path());
		std::fs::write(dir.path().join("frame"), b"bytes").unwrap();
		syncer.sync().await.unwrap();
		syncer.sync().await.unwrap();
		assert_eq!(syncer.dir(), dir.path());
		assert!(!syncer.is_poisoned());

		// Windows has no directory fsync, so only Unix can fail one.
		if cfg!(unix) {
			let missing = DirSyncer::new(Arc::new(RealFs), dir.path().join("missing"));
			assert_eq!(missing.sync().await.unwrap_err().kind(), io::ErrorKind::NotFound);
			assert!(missing.is_poisoned(), "any fsync failure poisons, even a missing directory");
		}
	}
}
