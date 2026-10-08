//! Reclaiming the frames maintenance retires: reader pins, the backup hold and the
//! reaper (docs/design/crash-consistency.md sections 5.1 "Reclaim", 5.4 and 6, R6; slice
//! S8).
//!
//! A write-once swap never deletes a frame. It replaces its inputs' rows with its
//! outputs' in one transaction and journals each input's file name as `'retired'` in the
//! same transaction (`frame_journal`). A read that pruned the index before the swap may
//! still open a retired frame, so the frame stays on disk until no such read can be
//! running:
//!
//! - every read takes a [`ReadPin`] before it reads the index, which records the reclaim
//!   epoch it started in, and releases it when it is dropped: when the read returns, and
//!   also when its future is dropped half way (a timed-out or cancelled request);
//! - right after a swap commits, the store records its retired names under the current
//!   epoch and advances the epoch ([`ReclaimEpochs::retire`]). A read pinned in that
//!   epoch or an earlier one may have pruned the index before the swap; one pinned later
//!   pruned it after;
//! - a backup that links frames holds the reaper off entirely ([`GcHold`]), since its
//!   snapshot of the index may reference a frame retired after the snapshot.
//!
//! The reaper ([`Reaper::sweep`], driven by the store) then works through the journal:
//!
//! - **G1.** It takes the retired names no pin or hold protects any more.
//! - **G2.** It never unlinks a name a live row references. That cannot happen by
//!   construction; it is the guard that keeps a bug elsewhere from deleting data.
//! - **G3.** It unlinks the frame (a missing file counts as unlinked) and, for a legacy
//!   `{aspect}-{id}` frame, the id-named sidecar, but only when no live row has that id
//!   any more (until S15, sidecars are named by id, which a swap's output may keep).
//! - **G4.** The store fsyncs `segments/` once.
//! - **G5.** The store deletes the processed journal rows in one commit.
//!
//! An unlink the filesystem refuses for now (a Windows sharing violation: another
//! process has the file open without sharing deletion; or `EBUSY`) is not an error: the
//! row stays and a later pass retries it. Any other failure is logged and the row stays
//! too.
//!
//! The open replays the journal before the store serves (R6): a `'pending'` output that
//! no row references was never swapped in and is unlinked, one a row references
//! committed and only its row goes; a `'retired'` frame is unlinked unless a live row
//! references it; then `segments/` is fsynced and the rows are deleted.

use std::{
	collections::{BTreeMap, BTreeSet, HashMap, HashSet}, io, path::PathBuf, sync::{
		atomic::{AtomicU64, Ordering}, Arc, Mutex, MutexGuard, PoisonError
	}
};

use crate::{
	aspect_name, types::{
		durable::{DirSyncer, StoreFs}, index_txn::IndexRow, segment_store::{contained_file, legacy_file_id}
	}
};

/// The state of a `frame_journal` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JournalState {
	/// A maintenance output that is written, or about to be, and not yet swapped in.
	Pending,
	/// A frame a committed swap took out of the index, awaiting the reaper.
	Retired,
}

impl JournalState {
	/// The state a `frame_journal.state` value names.
	///
	/// # Errors
	///
	/// A value that is neither `'pending'` nor `'retired'`.
	pub(crate) fn parse(state: &str) -> anyhow::Result<Self> {
		match state {
			"pending" => Ok(Self::Pending),
			"retired" => Ok(Self::Retired),
			other => anyhow::bail!("unknown frame_journal state {other:?}"),
		}
	}
}

/// One `frame_journal` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JournalEntry {
	/// The frame's file name, directly under `segments/`.
	pub name: String,
	/// The aspect whose maintenance journaled it.
	pub aspect: String,
	pub state: JournalState,
	/// For a retired frame, the aspect epoch of the swap that retired it.
	pub retire_epoch: Option<u64>,
}

/// The reclaim epochs of one store: which reads are running since which epoch, which
/// retired frames they may still open, and whether a backup holds the reaper off.
#[derive(Debug, Default)]
pub(crate) struct ReclaimEpochs {
	state: Mutex<Epochs>,
	/// How many [`GcHold`]s are alive.
	gc_hold: AtomicU64,
}

#[derive(Debug, Default)]
struct Epochs {
	/// The epoch a read that pins now records.
	current: u64,
	/// How many live pins each epoch has.
	pins: BTreeMap<u64, usize>,
	/// The epoch each name was retired in, by this process, until the reaper is done
	/// with it. A pin of that epoch or an earlier one may still open the frame.
	retired: HashMap<String, u64>,
}

impl ReclaimEpochs {
	fn lock(&self) -> MutexGuard<'_, Epochs> {
		// Every update is a few map operations that leave the state consistent.
		self.state.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Pin the current epoch for a read, until the pin drops.
	pub(crate) fn pin(&self) -> ReadPin<'_> {
		let epoch = {
			let mut state = self.lock();
			let epoch = state.current;
			*state.pins.entry(epoch).or_insert(0) += 1;
			epoch
		};
		ReadPin { epochs: self, epoch }
	}

	/// Hold the reaper off retired frames until the hold drops.
	pub(crate) fn hold(&self) -> GcHold<'_> {
		self.gc_hold.fetch_add(1, Ordering::SeqCst);
		GcHold { epochs: self }
	}

	/// Whether a [`GcHold`] is alive.
	pub(crate) fn held(&self) -> bool {
		self.gc_hold.load(Ordering::SeqCst) > 0
	}

	/// Record `names` as retired by a swap that has just committed, under the current
	/// epoch, and advance the epoch: a read pinned from now on pruned the index after the
	/// swap and cannot open them.
	pub(crate) fn retire(&self, names: impl IntoIterator<Item = String>) {
		let mut state = self.lock();
		let epoch = state.current;
		state.current += 1;
		for name in names {
			state.retired.insert(name, epoch);
		}
	}

	/// Whether the retired frame `name` may be unlinked now: no backup holds the reaper
	/// off, and no read pinned in or before the epoch it was retired in is still running.
	/// A name this process did not retire (a journal row a previous process left) cannot
	/// be open in any read of this one.
	pub(crate) fn reclaimable(&self, name: &str) -> bool {
		if self.held() {
			return false;
		}
		let state = self.lock();
		state.retired.get(name).is_none_or(|&retired| state.pins.keys().next().is_none_or(|&oldest| oldest > retired))
	}

	/// Forget `names`, whose journal rows the reaper has deleted.
	pub(crate) fn forget(&self, names: &[String]) {
		let mut state = self.lock();
		for name in names {
			state.retired.remove(name);
		}
	}

	/// How many reads hold a pin now.
	#[cfg(test)]
	pub(crate) fn pins(&self) -> usize {
		self.lock().pins.values().sum()
	}
}

/// A read's pin of a reclaim epoch (see the module documentation). Released on drop, so
/// a read future dropped half way, by a timeout or a cancelled request, releases it too.
#[derive(Debug)]
#[must_use = "a pin protects the frames a read opens only while it is held"]
pub(crate) struct ReadPin<'a> {
	epochs: &'a ReclaimEpochs,
	epoch: u64,
}

impl Drop for ReadPin<'_> {
	fn drop(&mut self) {
		let mut state = self.epochs.lock();
		if let Some(count) = state.pins.get_mut(&self.epoch) {
			*count = count.saturating_sub(1);
			if *count == 0 {
				state.pins.remove(&self.epoch);
			}
		}
	}
}

/// A backup's hold on the reaper (design section 9, B1-B4): while one is alive, no
/// retired frame is unlinked. Released on drop.
#[derive(Debug)]
#[must_use = "the reaper is held off only while the hold is alive"]
pub(crate) struct GcHold<'a> {
	epochs: &'a ReclaimEpochs,
}

impl Drop for GcHold<'_> {
	fn drop(&mut self) {
		self.epochs.gc_hold.fetch_sub(1, Ordering::SeqCst);
	}
}

/// What an aspect's live rows reference: the file names of their frames and their ids.
#[derive(Debug, Default)]
pub(crate) struct LiveFrames {
	names: HashSet<String>,
	ids: HashSet<u64>,
}

impl LiveFrames {
	/// What `rows` reference.
	pub(crate) fn of(rows: &[IndexRow]) -> Self {
		Self { names: rows.iter().filter_map(|row| frame_file_name(&row.desc.path)).map(str::to_string).collect(), ids: rows.iter().map(|row| row.desc.id).collect() }
	}

	/// Whether a live row references the frame file `name`.
	pub(crate) fn references(&self, name: &str) -> bool {
		self.names.contains(name)
	}
}

/// The file name a stored frame path ends in (split on both `/` and `\`, as the readers
/// resolve it), or `None` for a path with no name.
pub(crate) fn frame_file_name(stored: &str) -> Option<&str> {
	stored.split(['/', '\\']).rfind(|part| !part.is_empty() && *part != ".")
}

/// What one [`Reaper::sweep`] did with the journal rows it was given.
#[derive(Debug, Default)]
pub(crate) struct Swept {
	/// The rows to delete: their frame is gone, or was kept on purpose (a live row
	/// references it). `segments/` must be synced before they are deleted when
	/// `unlinked` is not zero.
	pub processed: Vec<String>,
	/// How many frames were unlinked.
	pub unlinked: usize,
	/// Retired frames a running read or a backup hold may still need, left for later.
	pub waiting: usize,
	/// Unlinks the filesystem asked to retry later; the rows stay.
	pub deferred: Vec<String>,
	/// Unlinks that failed otherwise; the rows stay for another pass.
	pub failed: Vec<(String, io::Error)>,
	/// The aspects some processed row of which was retired: a swap of theirs committed.
	pub swapped: BTreeSet<String>,
}

/// The reaper of one store: the filesystem it unlinks through, the `segments/`
/// directory and its directory syncer, and the reclaim epochs that say what it may take.
#[derive(Debug)]
pub(crate) struct Reaper {
	fs: Arc<dyn StoreFs>,
	segments: PathBuf,
	syncer: DirSyncer,
	epochs: ReclaimEpochs,
}

impl Reaper {
	/// The reaper of the store whose frames live in `segments`, writing through `fs`.
	pub(crate) fn new(fs: Arc<dyn StoreFs>, segments: PathBuf) -> Self {
		let syncer = DirSyncer::new(fs.clone(), segments.clone());
		Self { fs, segments, syncer, epochs: ReclaimEpochs::default() }
	}

	/// The store's reclaim epochs.
	pub(crate) const fn epochs(&self) -> &ReclaimEpochs {
		&self.epochs
	}

	/// Make every entry created, renamed or removed in `segments/` before the call durable,
	/// sharing the fsync with concurrent callers ([`DirSyncer`]).
	///
	/// # Errors
	///
	/// The fsync's error; the syncer, and the caller's store, are poisoned by it.
	pub(crate) async fn sync(&self) -> io::Result<()> {
		self.syncer.sync().await
	}

	/// Work through `entries` (G1-G3, or R6 at the open): unlink what may go, keep what a
	/// live row references, and report which rows to delete. `live` holds each aspect's
	/// live rows; `respect_pins` is false only at the open, before any read can run.
	pub(crate) async fn sweep(&self, entries: &[JournalEntry], live: &HashMap<String, LiveFrames>, respect_pins: bool) -> Swept {
		let none = LiveFrames::default();
		let mut swept = Swept::default();
		for entry in entries {
			let retired = entry.state == JournalState::Retired;
			if retired && respect_pins && !self.epochs.reclaimable(&entry.name) {
				swept.waiting += 1;
				continue;
			}
			let live = live.get(&entry.aspect).unwrap_or(&none);
			if retired {
				swept.swapped.insert(entry.aspect.clone());
			}
			if live.references(&entry.name) {
				// A pending output a row references was swapped in: only its row goes. A
				// retired frame a row references is the guard (G2): something put the name
				// back into the index, so the frame is data and stays.
				if retired {
					tracing::error!(aspect = entry.aspect, frame = entry.name, "a retired frame is referenced by a live row; keeping it and dropping its journal row");
				}
				swept.processed.push(entry.name.clone());
				continue;
			}
			let path = match contained_file(&self.segments, &entry.name) {
				Ok(path) => path,
				Err(e) => {
					// No frame name leaves segments/; a journal row that does was not written
					// by a swap, and nothing is unlinked for it.
					tracing::error!(aspect = entry.aspect, frame = entry.name, error = %e, "dropping a journal row whose name is not a frame in segments/");
					swept.processed.push(entry.name.clone());
					continue;
				}
			};
			match self.fs.remove_file(&path).await {
				Ok(()) => {
					swept.unlinked += 1;
					swept.processed.push(entry.name.clone());
					if retired {
						self.remove_orphan_sidecar(entry, live).await;
					}
				}
				Err(e) if retry_later(&e) => {
					tracing::debug!(aspect = entry.aspect, frame = entry.name, error = %e, "a frame is busy; the reaper retries it later");
					swept.deferred.push(entry.name.clone());
				}
				Err(e) => {
					tracing::warn!(aspect = entry.aspect, frame = entry.name, error = %e, "could not unlink a frame; its journal row stays for the next pass");
					swept.failed.push((entry.name.clone(), e));
				}
			}
		}
		swept
	}

	/// G3's sidecar half: the id-named sidecar of a retired legacy `{aspect}-{id}` frame,
	/// once no live row of the aspect has that id (a swap that kept the id refreshes the
	/// sidecar for its output instead). Best effort: a sidecar is an accelerator.
	async fn remove_orphan_sidecar(&self, entry: &JournalEntry, live: &LiveFrames) {
		let Some((aspect, id)) = legacy_file_id(&entry.name) else { return };
		if aspect != entry.aspect || live.ids.contains(&id) || aspect_name::validate(aspect).is_err() {
			return;
		}
		let removed = match contained_file(&self.segments, &format!("{aspect}-{id}.weftpart")) {
			Ok(path) => self.fs.remove_file(&path).await.map_err(anyhow::Error::from),
			Err(e) => Err(e),
		};
		if let Err(e) = removed {
			tracing::warn!(aspect, id, error = %e, "could not remove the sidecar of a reaped segment");
		}
	}
}

/// Whether an unlink that failed with `e` should simply be retried later: a Windows
/// sharing or lock violation (another process has the file open without sharing
/// deletion, an antivirus scanner say) or a busy file (`EBUSY`). The release plan's D-S8
/// amendment: such a frame is not an error, it is reaped on a later pass.
pub(crate) fn retry_later(e: &io::Error) -> bool {
	unlink_busy(e, cfg!(windows))
}

/// [`retry_later`] with the platform as a parameter, so both rule sets are tested
/// everywhere: `ERROR_SHARING_VIOLATION` (32) and `ERROR_LOCK_VIOLATION` (33) mean busy
/// only on Windows, where those are the codes.
fn unlink_busy(e: &io::Error, windows: bool) -> bool {
	e.kind() == io::ErrorKind::ResourceBusy || (windows && matches!(e.raw_os_error(), Some(32 | 33)))
}

/// A [`StoreFs`] for the tests of the reaper's "retry later": the real filesystem, except
/// that unlinking a file whose name is marked busy fails as a busy file does
/// (`ErrorKind::ResourceBusy`, what a Windows sharing violation is to the reaper).
#[cfg(test)]
pub(crate) mod testing {
	use std::{
		collections::HashSet, io, path::Path, sync::{Mutex, PoisonError}
	};

	use async_trait::async_trait;

	use crate::types::durable::{FsEntry, FsMetadata, RealFs, StoreFs, SyncPolicy, WritePoints};

	#[derive(Debug, Default)]
	pub(crate) struct BusyFs {
		busy: Mutex<HashSet<String>>,
	}

	impl BusyFs {
		/// Make unlinking a file named `name` fail as busy, or stop doing so.
		pub(crate) fn set_busy(&self, name: &str, busy: bool) {
			let mut names = self.busy.lock().unwrap_or_else(PoisonError::into_inner);
			if busy {
				names.insert(name.to_string());
			} else {
				names.remove(name);
			}
		}
	}

	#[async_trait]
	impl StoreFs for BusyFs {
		async fn create_new_write(&self, path: &Path, bytes: Vec<u8>, policy: SyncPolicy, points: WritePoints) -> io::Result<()> {
			RealFs.create_new_write(path, bytes, policy, points).await
		}

		async fn sync_file(&self, path: &Path) -> io::Result<()> {
			RealFs.sync_file(path).await
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
			let busy = path.file_name().and_then(|name| name.to_str()).is_some_and(|name| self.busy.lock().unwrap_or_else(PoisonError::into_inner).contains(name));
			if busy {
				return Err(io::Error::new(io::ErrorKind::ResourceBusy, "the file is open in another process"));
			}
			RealFs.remove_file(path).await
		}

		async fn create_dir(&self, path: &Path) -> io::Result<()> {
			RealFs.create_dir(path).await
		}

		async fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
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
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use bigdecimal::BigDecimal;
	use weft_physical_type::{timestamp::TimeUnit, Segment, SegmentDescriptor};

	use super::*;

	/// A live row of `aspect`'s segment `id` whose frame is `segments/{name}`.
	fn live_row(segments: &std::path::Path, id: u64, name: &str) -> IndexRow {
		let segment = Segment::build(&[0, 10], &[BigDecimal::from(1), BigDecimal::from(2)], TimeUnit::Seconds, &BigDecimal::from(0)).expect("builds");
		IndexRow::legacy(SegmentDescriptor::of_segment(id, segments.join(name).to_string_lossy().into_owned(), 64, &segment))
	}

	fn entry(name: &str, state: JournalState) -> JournalEntry {
		JournalEntry { name: name.to_string(), aspect: "a".to_string(), state, retire_epoch: (state == JournalState::Retired).then_some(1) }
	}

	/// What the reaper does with each kind of journal row: an unreferenced pending output
	/// and an unreferenced retired frame are unlinked, a retired legacy frame's id-named
	/// sidecar with them unless a live row still has that id; a referenced pending output
	/// (swapped in) and a referenced retired frame (the guard) are kept and their rows
	/// processed; a frame the filesystem calls busy is deferred with its row; and a name
	/// that is not a plain file in `segments/` unlinks nothing. A missing file counts as
	/// unlinked, so a replay after a crash mid-pass finishes the job.
	#[tokio::test]
	async fn the_reaper_unlinks_only_what_no_live_row_references() {
		let dir = tempfile::TempDir::new().expect("tempdir");
		let segments = dir.path().join("segments");
		std::fs::create_dir(&segments).expect("creates segments/");
		for name in ["a~g1~p0.weftseg", "a~g2~p0.weftseg", "a-3.weftseg", "a-3.weftpart", "a-4.weftseg", "a-4.weftpart", "a-5.weftseg", "a~g6~p0.weftseg", "busy.weftseg", "outside.weftseg"] {
			std::fs::write(segments.join(name), b"frame").expect("writes");
		}
		let fs = Arc::new(testing::BusyFs::default());
		fs.set_busy("busy.weftseg", true);
		let reaper = Reaper::new(fs.clone(), segments.clone());
		// Segment 4 lives on in a write-once frame (a reconcile kept its id); segment 5's
		// legacy frame is live again (an in-place rewrite named it); gen 2 is swapped in.
		let live = HashMap::from([("a".to_string(), LiveFrames::of(&[live_row(&segments, 4, "a~g6~p0.weftseg"), live_row(&segments, 5, "a-5.weftseg"), live_row(&segments, 7, "a~g2~p0.weftseg")]))]);
		let entries = [entry("a~g1~p0.weftseg", JournalState::Pending), entry("a~g2~p0.weftseg", JournalState::Pending), entry("a-3.weftseg", JournalState::Retired), entry("a-4.weftseg", JournalState::Retired), entry("a-5.weftseg", JournalState::Retired), entry("gone.weftseg", JournalState::Retired), entry("busy.weftseg", JournalState::Retired), entry("../outside.weftseg", JournalState::Retired)];
		let swept = reaper.sweep(&entries, &live, true).await;
		let mut left: Vec<String> = std::fs::read_dir(&segments).expect("lists").map(|e| e.expect("entry").file_name().to_string_lossy().into_owned()).collect();
		left.sort();
		assert_eq!(left, vec!["a-4.weftpart", "a-5.weftseg", "a~g2~p0.weftseg", "a~g6~p0.weftseg", "busy.weftseg", "outside.weftseg"]);
		assert_eq!(swept.processed, vec!["a~g1~p0.weftseg", "a~g2~p0.weftseg", "a-3.weftseg", "a-4.weftseg", "a-5.weftseg", "gone.weftseg", "../outside.weftseg"]);
		assert_eq!((swept.unlinked, swept.waiting, swept.deferred.as_slice(), swept.failed.len()), (4, 0, ["busy.weftseg".to_string()].as_slice(), 0));
		assert_eq!(swept.swapped.into_iter().collect::<Vec<_>>(), vec!["a".to_string()]);

		// Once the file is no longer busy, the next pass takes it.
		fs.set_busy("busy.weftseg", false);
		let again = reaper.sweep(&[entry("busy.weftseg", JournalState::Retired)], &live, true).await;
		assert_eq!((again.unlinked, again.processed.as_slice()), (1, ["busy.weftseg".to_string()].as_slice()));
		assert!(!segments.join("busy.weftseg").exists());
	}

	/// The reaper leaves a retired frame a pin or a backup hold protects, and processes no
	/// row for it; a pending output is not a pin's business.
	#[tokio::test]
	async fn the_reaper_leaves_what_a_pin_or_a_hold_protects() {
		let dir = tempfile::TempDir::new().expect("tempdir");
		let segments = dir.path().join("segments");
		std::fs::create_dir(&segments).expect("creates segments/");
		for name in ["a-1.weftseg", "a~g1~p0.weftseg"] {
			std::fs::write(segments.join(name), b"frame").expect("writes");
		}
		let reaper = Reaper::new(Arc::new(crate::types::durable::RealFs), segments.clone());
		let pin = reaper.epochs().pin();
		reaper.epochs().retire(["a-1.weftseg".to_string()]);
		let entries = [entry("a-1.weftseg", JournalState::Retired), entry("a~g1~p0.weftseg", JournalState::Pending)];
		let pinned = reaper.sweep(&entries, &HashMap::new(), true).await;
		drop(pin);
		let hold = reaper.epochs().hold();
		let held = reaper.sweep(&entries[..1], &HashMap::new(), true).await;
		drop(hold);
		let free = reaper.sweep(&entries[..1], &HashMap::new(), true).await;
		assert_eq!((pinned.waiting, pinned.processed.as_slice()), (1, ["a~g1~p0.weftseg".to_string()].as_slice()), "the pending output goes, the pinned frame waits");
		assert_eq!((held.waiting, held.processed.len()), (1, 0), "a hold keeps it too");
		assert_eq!((free.unlinked, free.processed.as_slice()), (1, ["a-1.weftseg".to_string()].as_slice()));
	}

	/// A pin protects exactly the frames retired in its epoch or a later one it might have
	/// seen before the swap: a frame retired after the pin is taken is protected until the
	/// pin drops, one retired before it is not.
	#[test]
	fn a_pin_protects_frames_retired_while_it_is_held() {
		let epochs = ReclaimEpochs::default();
		epochs.retire(["before".to_string()]);
		let pin = epochs.pin();
		epochs.retire(["during".to_string()]);
		let later = epochs.pin();
		epochs.retire(["after".to_string()]);
		let while_pinned = ["before", "during", "after", "unknown"].map(|name| epochs.reclaimable(name));
		drop(pin);
		let after_first = ["during", "after"].map(|name| epochs.reclaimable(name));
		drop(later);
		let after_all = epochs.reclaimable("after");
		assert_eq!(while_pinned, [true, false, false, true], "a frame retired before the oldest pin, and one this process never retired, may go");
		assert_eq!(after_first, [true, false], "the later pin still protects what was retired while it was held");
		assert!(after_all);
		assert_eq!(epochs.pins(), 0);
	}

	/// The backup hold keeps every retired frame, whatever the pins say, until it drops.
	#[test]
	fn a_gc_hold_keeps_every_retired_frame_until_it_drops() {
		let epochs = ReclaimEpochs::default();
		epochs.retire(["old".to_string()]);
		let hold = epochs.hold();
		let second = epochs.hold();
		let held = (epochs.reclaimable("old"), epochs.reclaimable("unknown"));
		drop(hold);
		let still = epochs.reclaimable("old");
		drop(second);
		assert_eq!(held, (false, false));
		assert!(!still, "one hold is still alive");
		assert!(epochs.reclaimable("old"));
	}

	/// The release plan's D-S8 amendment: a pin is RAII, so a read future that a timeout
	/// drops half way releases its pin, and the frames it protected become reclaimable.
	#[tokio::test]
	async fn a_pin_is_released_when_a_timed_out_future_drops() {
		let epochs = ReclaimEpochs::default();
		let mut read = Box::pin(async {
			let _pin = epochs.pin();
			std::future::pending::<()>().await;
		});
		// Poll once so the read takes its pin, as a read does before it prunes the index.
		assert!(futures::poll!(read.as_mut()).is_pending());
		epochs.retire(["frame".to_string()]);
		let pinned = (epochs.pins(), epochs.reclaimable("frame"));
		// The timeout owns the future and drops it when it fires.
		let timed_out = tokio::time::timeout(Duration::from_millis(10), read).await.is_err();
		assert_eq!(pinned, (1, false), "the read in flight protects the frame");
		assert!(timed_out);
		assert_eq!((epochs.pins(), epochs.reclaimable("frame")), (0, true), "the dropped future released its pin");
	}

	#[test]
	fn a_busy_unlink_is_retried_later_and_only_a_busy_one() {
		let busy = io::Error::from(io::ErrorKind::ResourceBusy);
		assert!(unlink_busy(&busy, false) && unlink_busy(&busy, true));
		for code in [32, 33] {
			assert!(unlink_busy(&io::Error::from_raw_os_error(code), true), "{code} is a sharing or lock violation on Windows");
			assert!(!unlink_busy(&io::Error::from_raw_os_error(code), false), "{code} is EPIPE or EDOM elsewhere");
		}
		for other in [io::ErrorKind::PermissionDenied, io::ErrorKind::NotFound, io::ErrorKind::Other] {
			assert!(!unlink_busy(&io::Error::from(other), true), "{other:?}");
		}
		assert!(!unlink_busy(&io::Error::from_raw_os_error(5), true), "access denied is not busy");
	}

	#[test]
	fn a_frame_file_name_is_the_last_component_of_either_separator() {
		assert_eq!(frame_file_name("/root/segments/price-0.weftseg"), Some("price-0.weftseg"));
		assert_eq!(frame_file_name("C:\\store\\segments\\p~g1~p0.weftseg"), Some("p~g1~p0.weftseg"));
		assert_eq!(frame_file_name("segments/./x.weftseg/"), Some("x.weftseg"));
		assert_eq!(frame_file_name(""), None);
	}

	#[test]
	fn journal_states_parse() {
		assert_eq!(JournalState::parse("pending").unwrap(), JournalState::Pending);
		assert_eq!(JournalState::parse("retired").unwrap(), JournalState::Retired);
		assert!(JournalState::parse("held").is_err());
	}
}
