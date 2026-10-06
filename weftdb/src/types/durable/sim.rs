//! A power-cut simulating [`StoreFs`] for the crash tests (design section 11).
//!
//! [`SimFs`] performs every operation on a real directory, so the store, Turso and
//! the read paths see an ordinary live filesystem. Alongside, it keeps a model of
//! what a power cut would preserve: per file, the bytes made durable by a sync
//! versus the bytes merely written; per directory entry, the state made durable by
//! `sync_dir` versus the operations still pending. [`SimFs::power_cut`] then writes
//! one legal post-crash image of the root into a fresh directory, chosen by a seed:
//!
//! - an unsynced file comes back absent (its entry was lost), empty, as a prefix of
//!   what was written, as a prefix followed by zero fill (the size reached the disk
//!   but the data blocks did not), or complete. A file rewritten in place can also
//!   come back with its old content;
//! - each pending directory operation is either applied or not. An operation is
//!   applied only together with the earlier pending operations on the same name, so
//!   the image never holds a state no filesystem could produce, such as a rename's
//!   target without its source ever having existed, or a new file that took a name
//!   whose previous file thereby lost every name;
//! - writes that bypass `SimFs` are not trusted. [`SimFs::new`] records every file
//!   under the root as the durable baseline. A file created, rewritten or removed
//!   since then by anything other than `SimFs` (`tokio::fs::write`, say), including
//!   one `SimFs` tracks, gets the outcomes of an unsynced one, so a write path that
//!   bypasses [`StoreFs`] shows up in the crash tests as torn or lost data instead of
//!   passing unexamined. Only the paths the caller exempts are copied as they are:
//!   [`SimFs::turso_file`] exempts Turso's databases and logs, which is sound because
//!   every COMMIT that returned was FULL fsynced (asserted at open in S3).
//!
//! The model is deliberately harsher than ext4 or XFS in ordered mode, which commit
//! metadata as a journal prefix: a directory fsync here makes durable only the
//! operations in that directory (plus their dependencies), which is all POSIX
//! promises. Changes made behind `SimFs`'s back carry no order at all, so a test that
//! needs one must route the operation through [`StoreFs`].
//!
//! Directories themselves are not modelled: a directory is assumed durable as soon as
//! it exists, and renaming one through `SimFs` is refused rather than simulated wrongly.
//! The power-loss points that create or rename directories (`B-partial-created`,
//! `B-renamed`, `prune-renamed`, `restore-renamed`, `L-new-renamed`) therefore cannot
//! be simulated yet: the slices that own them (S5, S16, S18) extend the model first
//! (design section 11).

use std::{
	collections::{BTreeMap, BTreeSet}, ffi::{OsStr, OsString}, fmt, fs::File, io::{self, Write}, path::{Component, Path, PathBuf}, sync::{Mutex, MutexGuard, PoisonError}
};

use async_trait::async_trait;

use super::{
	fault, fs::{metadata_blocking, read_dir_blocking, remove_file_blocking, FsEntry, FsMetadata, StoreFs, SyncPolicy, WritePoints}
};

/// A directory entry: (directory relative to the root, file name).
type Slot = (PathBuf, OsString);
/// An index into [`Model::inodes`]; hard links share one.
type Ino = usize;

/// A file's content: what a power cut is guaranteed to keep, and what is written.
#[derive(Debug)]
struct Inode {
	durable: Vec<u8>,
	current: Vec<u8>,
}

/// The content a file whose durable bytes are `durable` and whose written bytes are
/// `current` has after a power cut, drawn from `rng`.
fn after_cut(durable: &[u8], current: &[u8], rng: &mut fastrand::Rng) -> Vec<u8> {
	if durable == current {
		return current.to_vec();
	}
	// The unsynced bytes either extend the durable ones (a new file, or an append) or
	// replace them (a rewrite in place: truncate, then write). Only an extension keeps
	// a durable prefix.
	let kept = if current.starts_with(durable) { durable.len() } else { 0 };
	match rng.u8(0..5) {
		// Part of the new bytes reached the disk.
		1 if current.len() - kept >= 2 => current[..rng.usize(kept + 1..current.len())].to_vec(),
		// The new size reached the disk but not all of the data blocks.
		2 if current.len() > kept => {
			let mut torn = current[..rng.usize(kept..current.len())].to_vec();
			torn.resize(current.len(), 0);
			torn
		}
		3 => current.to_vec(),
		// The truncation reached the disk but none of the new bytes, so a rewritten file
		// comes back empty (and an extended one as it was).
		4 => current[..kept].to_vec(),
		// None of the change did. (A one-byte tail has no partial prefix, so that draw
		// lands here too.)
		_ => durable.to_vec(),
	}
}

/// A directory operation that has happened but is not yet known to be durable.
#[derive(Debug, Clone)]
enum DirOp {
	/// `slot` now names `ino`: a create, or a hard link.
	Add { slot: Slot, ino: Ino },
	/// `slot` no longer names anything.
	Remove { slot: Slot },
	/// `from`'s file moved to `to`, replacing whatever `to` named.
	Rename { from: Slot, to: Slot, ino: Ino },
}

impl DirOp {
	fn slots(&self) -> impl Iterator<Item = &Slot> {
		let (first, second) = match self {
			Self::Add { slot, .. } | Self::Remove { slot } => (slot, None),
			Self::Rename { from, to, .. } => (from, Some(to)),
		};
		std::iter::once(first).chain(second)
	}

	fn touches_dir(&self, dir: &Path) -> bool {
		self.slots().any(|(slot_dir, _)| slot_dir == dir)
	}

	/// Whether this operation is only meaningful once `earlier` has been applied:
	/// whether they name a common entry. Applying two operations on one name out of
	/// order gives an image no filesystem could produce (a rename's target without
	/// its source, a create in a name the old file still holds).
	///
	/// Sharing a file is deliberately not a dependency. A hard link names the inode,
	/// not its source entry, so an image may keep the link and lose the original
	/// name, as POSIX allows when only the link's directory was synced; and syncing
	/// one directory must not make durable an operation in another that merely
	/// involves the same file.
	fn depends_on(&self, earlier: &Self) -> bool {
		self.slots().any(|slot| earlier.slots().any(|e| e == slot))
	}

	fn apply(&self, entries: &mut BTreeMap<Slot, Ino>) {
		match self {
			Self::Add { slot, ino } => {
				entries.insert(slot.clone(), *ino);
			}
			Self::Remove { slot } => {
				entries.remove(slot);
			}
			Self::Rename { from, to, ino } => {
				entries.remove(from);
				entries.insert(to.clone(), *ino);
			}
		}
	}
}

/// The indices of `ops` named by `roots`, plus every earlier operation they depend
/// on, transitively.
fn with_dependencies(ops: &[DirOp], roots: impl IntoIterator<Item = usize>) -> BTreeSet<usize> {
	let mut chosen = BTreeSet::new();
	let mut stack: Vec<usize> = roots.into_iter().collect();
	while let Some(i) = stack.pop() {
		if chosen.insert(i) {
			stack.extend((0..i).filter(|&j| ops[i].depends_on(&ops[j])));
		}
	}
	chosen
}

#[derive(Debug, Default)]
struct Model {
	inodes: Vec<Inode>,
	/// Every entry the model tracks: those `SimFs` has operated on. Untracked entries
	/// are judged against the baseline in an image.
	known: BTreeSet<Slot>,
	/// The known entries that survive any power cut.
	durable: BTreeMap<Slot, Ino>,
	/// The known entries as they are now.
	live: BTreeMap<Slot, Ino>,
	/// Directory operations since the last `sync_dir` covering them, in order.
	pending: Vec<DirOp>,
	file_syncs: u64,
	dir_syncs: u64,
}

impl Model {
	fn new_inode(&mut self, durable: Vec<u8>, current: Vec<u8>) -> Ino {
		self.inodes.push(Inode { durable, current });
		self.inodes.len() - 1
	}

	/// Make the operations at `indices` durable, in their original order.
	fn commit(&mut self, indices: &BTreeSet<usize>) {
		for (i, op) in std::mem::take(&mut self.pending).into_iter().enumerate() {
			if indices.contains(&i) {
				op.apply(&mut self.durable);
			} else {
				self.pending.push(op);
			}
		}
	}

	fn live_ino(&self, slot: &Slot, path: &Path) -> io::Result<Ino> {
		self.live.get(slot).copied().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("SimFs has no file at {}", path.display())))
	}
}

/// A [`StoreFs`] over a real directory that can also produce the image a power cut
/// at this instant could leave behind. See the module docs for the crash model.
pub struct SimFs {
	root: PathBuf,
	/// Paths (relative to the root) trusted as durable whatever happens to them.
	exempt: fn(&Path) -> bool,
	/// Every other file under the root when the simulation began, with its content:
	/// what a power cut keeps of a file nothing has synced through `SimFs`.
	baseline: BTreeMap<Slot, Vec<u8>>,
	model: Mutex<Model>,
}

impl fmt::Debug for SimFs {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("SimFs").field("root", &self.root).field("baseline_files", &self.baseline.len()).finish_non_exhaustive()
	}
}

impl SimFs {
	/// Simulate over the existing directory `root`, taking every file under it now
	/// as durable and every later change not made through `SimFs` as unsynced. Every
	/// path passed to the [`StoreFs`] methods must lie under the root.
	///
	/// # Errors
	///
	/// Any error reading the root.
	pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
		Self::exempting(root, |_| false)
	}

	/// Like [`new`](Self::new), but a file whose path relative to the root satisfies
	/// `exempt` is trusted as durable whatever happens to it, and is copied into every
	/// image as it is. Pass [`SimFs::turso_file`] when the root holds Turso databases.
	///
	/// # Errors
	///
	/// Any error reading the root.
	pub fn exempting(root: impl Into<PathBuf>, exempt: fn(&Path) -> bool) -> io::Result<Self> {
		let root = root.into();
		let mut baseline = BTreeMap::new();
		for slot in scan(&root)?.1 {
			let rel = slot_path(&slot);
			if !exempt(&rel) {
				let bytes = std::fs::read(root.join(&rel))?;
				baseline.insert(slot, bytes);
			}
		}
		Ok(Self { root, exempt, baseline, model: Mutex::default() })
	}

	/// The exemption for Turso's files: databases (`*.db`) and their logs (`*.db-log`,
	/// `*.db-wal`, `*.db-shm`). Turso writes them itself, never through [`StoreFs`],
	/// and every COMMIT that returned was FULL fsynced, so the image copies them as
	/// they are at the cut.
	#[must_use]
	pub fn turso_file(rel: &Path) -> bool {
		rel.file_name().and_then(OsStr::to_str).is_some_and(|name| [".db", ".db-log", ".db-wal", ".db-shm"].iter().any(|suffix| name.ends_with(suffix)))
	}

	/// The simulated root.
	#[must_use]
	pub fn root(&self) -> &Path {
		&self.root
	}

	/// How many file syncs have succeeded: `sync_file` calls and
	/// [`SyncPolicy::Full`] writes.
	#[must_use]
	pub fn file_syncs(&self) -> u64 {
		self.lock().file_syncs
	}

	/// How many `sync_dir` calls have succeeded.
	#[must_use]
	pub fn dir_syncs(&self) -> u64 {
		self.lock().dir_syncs
	}

	/// How many directory operations made through `SimFs` a power cut could still
	/// lose.
	#[must_use]
	pub fn pending_dir_ops(&self) -> usize {
		self.lock().pending.len()
	}

	/// Write the image a power cut at this instant could leave, chosen by `seed`,
	/// into `dest`, which must not exist yet (or be empty) and must lie outside the
	/// root. The same seed and history always give the same image.
	///
	/// # Errors
	///
	/// `InvalidInput` if `dest` is inside the root, `AlreadyExists` if it holds
	/// anything, or any error reading the root or writing the image.
	// The model lock is held throughout so the image is one instant's state.
	#[allow(clippy::significant_drop_tightening)]
	pub fn power_cut(&self, seed: u64, dest: &Path) -> io::Result<()> {
		let model = self.lock();
		if dest.starts_with(&self.root) {
			return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("the crash image {} must lie outside the simulated root {}", dest.display(), self.root.display())));
		}
		let fresh = match std::fs::read_dir(dest) {
			Ok(mut entries) => entries.next().is_none(),
			Err(e) if e.kind() == io::ErrorKind::NotFound => true,
			Err(e) => return Err(e),
		};
		if !fresh {
			return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("the crash image directory {} is not empty", dest.display())));
		}
		std::fs::create_dir_all(dest)?;
		let (dirs, files) = scan(&self.root)?;
		// Directories are not modelled: every one that exists now is in the image.
		for dir in &dirs {
			std::fs::create_dir_all(dest.join(dir))?;
		}

		// The names `SimFs` tracks: the durable entries plus a seeded subset of the
		// pending operations, each with the earlier ones it depends on.
		let mut rng = fastrand::Rng::with_seed(seed);
		let flipped: Vec<usize> = (0..model.pending.len()).filter(|_| rng.bool()).collect();
		let applied = with_dependencies(&model.pending, flipped);
		let mut entries = model.durable.clone();
		for (i, op) in model.pending.iter().enumerate() {
			if applied.contains(&i) {
				op.apply(&mut entries);
			}
		}

		// What is on disk now under those names. A change made to them behind
		// `SimFs`'s back is as unordered and unsynced as any other untracked change.
		let mut on_disk: BTreeMap<Ino, Vec<u8>> = BTreeMap::new();
		let mut recreated = Vec::new();
		for slot in &model.known {
			match (model.live.get(slot), read_if_file(&self.root.join(slot_path(slot)))?) {
				(Some(&ino), Some(bytes)) => {
					on_disk.insert(ino, bytes);
				}
				(Some(_), None) => {
					if rng.bool() {
						entries.remove(slot);
					}
				}
				(None, Some(bytes)) => {
					if rng.bool() {
						recreated.push((slot, bytes));
					}
				}
				(None, None) => {}
			}
		}

		// One outcome per file, shared by all of its names.
		let in_image: BTreeSet<Ino> = entries.values().copied().collect();
		let contents: BTreeMap<Ino, Vec<u8>> = in_image
			.into_iter()
			.map(|ino| {
				let inode = &model.inodes[ino];
				(ino, after_cut(&inode.durable, on_disk.get(&ino).unwrap_or(&inode.current), &mut rng))
			})
			.collect();
		let mut image: BTreeMap<Slot, Vec<u8>> = entries.iter().map(|(slot, ino)| (slot.clone(), contents[ino].clone())).collect();
		for (slot, bytes) in recreated {
			image.insert(slot.clone(), after_cut(&[], &bytes, &mut rng));
		}

		// Untracked files: what the baseline holds is durable, and every change since is
		// unsynced. Exempt files are copied as they are.
		let live_untracked: BTreeSet<&Slot> = files.iter().filter(|slot| !model.known.contains(*slot)).collect();
		let untracked: BTreeSet<&Slot> = live_untracked.iter().copied().chain(self.baseline.keys().filter(|slot| !model.known.contains(*slot))).collect();
		for slot in untracked {
			let rel = slot_path(slot);
			let now = if live_untracked.contains(slot) { Some(std::fs::read(self.root.join(&rel))?) } else { None };
			let kept = if (self.exempt)(&rel) {
				now
			} else {
				match (self.baseline.get(slot), now) {
					// The entry is durable; the content changed without a sync.
					(Some(old), Some(now)) => Some(after_cut(old, &now, &mut rng)),
					// Removed without a directory sync, so the removal may be lost.
					(Some(old), None) if !dirs.contains(&rel) => rng.bool().then(|| old.clone()),
					// Created without either sync: absent, or torn.
					(None, Some(now)) => {
						if rng.bool() {
							Some(after_cut(&[], &now, &mut rng))
						} else {
							None
						}
					}
					_ => None,
				}
			};
			if let Some(bytes) = kept {
				image.insert(slot.clone(), bytes);
			}
		}

		for ((dir, name), bytes) in &image {
			let dir = dest.join(dir);
			std::fs::create_dir_all(&dir)?;
			std::fs::write(dir.join(name), bytes)?;
		}
		Ok(())
	}

	fn lock(&self) -> MutexGuard<'_, Model> {
		self.model.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// `path` relative to the root, which must be a plain relative path.
	fn relative<'a>(&self, path: &'a Path) -> io::Result<&'a Path> {
		path.strip_prefix(&self.root).ok().filter(|rel| rel.components().all(|c| matches!(c, Component::Normal(_)))).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not a plain path under the simulated root {}", path.display(), self.root.display())))
	}

	fn slot(&self, path: &Path) -> io::Result<Slot> {
		let rel = self.relative(path)?;
		match (rel.parent(), rel.file_name()) {
			(Some(dir), Some(name)) => Ok((dir.to_path_buf(), name.to_os_string())),
			_ => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} names the simulated root itself, not a file under it", path.display()))),
		}
	}

	/// Start tracking `slot` before the first operation on it. A power cut keeps what
	/// the baseline holds there (or, for an exempt path, what is there now); anything
	/// done to it since, behind `SimFs`'s back, is not durable.
	fn adopt(&self, model: &mut Model, slot: &Slot, path: &Path) -> io::Result<()> {
		if model.known.contains(slot) {
			return Ok(());
		}
		let now = match std::fs::symlink_metadata(path) {
			Ok(meta) if meta.is_dir() => return Err(io::Error::new(io::ErrorKind::Unsupported, format!("SimFs models file entries only, and {} is a directory", path.display()))),
			Ok(_) => Some(std::fs::read(path)?),
			Err(e) if e.kind() == io::ErrorKind::NotFound => None,
			Err(e) => return Err(e),
		};
		let durable = if (self.exempt)(&slot_path(slot)) { now.clone() } else { self.baseline.get(slot).cloned() };
		match (durable, now) {
			(Some(durable), Some(now)) => {
				let ino = model.new_inode(durable, now);
				model.durable.insert(slot.clone(), ino);
				model.live.insert(slot.clone(), ino);
			}
			// Removed behind `SimFs`'s back: the removal is not known to be durable.
			(Some(durable), None) => {
				let ino = model.new_inode(durable.clone(), durable);
				model.durable.insert(slot.clone(), ino);
				model.pending.push(DirOp::Remove { slot: slot.clone() });
			}
			// Created behind `SimFs`'s back: neither its entry nor its bytes are durable.
			(None, Some(now)) => {
				let ino = model.new_inode(Vec::new(), now);
				model.live.insert(slot.clone(), ino);
				model.pending.push(DirOp::Add { slot: slot.clone(), ino });
			}
			(None, None) => {}
		}
		model.known.insert(slot.clone());
		Ok(())
	}

	/// The steps of `create_new_write` after the create, as `RealFs` runs them, with
	/// the model brought up to date after each.
	async fn fill(&self, mut file: File, ino: Ino, bytes: Vec<u8>, policy: SyncPolicy, points: WritePoints) -> io::Result<()> {
		if let Some(point) = points.created {
			fault::hit(point).await?;
		}
		file.write_all(&bytes)?;
		drop(file);
		self.lock().inodes[ino].current = bytes;
		if let Some(point) = points.written {
			fault::hit(point).await?;
		}
		if policy == SyncPolicy::Full {
			let mut model = self.lock();
			let inode = &mut model.inodes[ino];
			inode.durable.clone_from(&inode.current);
			model.file_syncs += 1;
		}
		Ok(())
	}
}

/// `(dir, name)` as a path relative to the root.
fn slot_path((dir, name): &Slot) -> PathBuf {
	dir.join(name)
}

/// Every directory and regular file under `root`, relative to it. Only regular files
/// and directories belong in a store; anything else is skipped.
fn scan(root: &Path) -> io::Result<(BTreeSet<PathBuf>, BTreeSet<Slot>)> {
	let (mut dirs, mut files) = (BTreeSet::new(), BTreeSet::new());
	let mut stack = vec![PathBuf::new()];
	while let Some(rel) = stack.pop() {
		for entry in std::fs::read_dir(root.join(&rel))? {
			let entry = entry?;
			let kind = entry.file_type()?;
			if kind.is_dir() {
				let child = rel.join(entry.file_name());
				dirs.insert(child.clone());
				stack.push(child);
			} else if kind.is_file() {
				files.insert((rel.clone(), entry.file_name()));
			}
		}
	}
	Ok((dirs, files))
}

/// The content of the regular file at `path`, or `None` if no file is there.
fn read_if_file(path: &Path) -> io::Result<Option<Vec<u8>>> {
	match std::fs::symlink_metadata(path) {
		Ok(meta) if meta.is_file() => std::fs::read(path).map(Some),
		Ok(_) => Ok(None),
		Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
		Err(e) => Err(e),
	}
}

// Each method holds the model lock across the real directory operation, so the model
// and the directory never disagree about the order those happened in.
// `create_new_write` lets go after the create: its fault points may pause, and the
// content writes that follow have no order to keep.
#[allow(clippy::significant_drop_tightening)]
#[async_trait]
impl StoreFs for SimFs {
	async fn create_new_write(&self, path: &Path, bytes: Vec<u8>, policy: SyncPolicy, points: WritePoints) -> io::Result<()> {
		let slot = self.slot(path)?;
		let (file, ino) = {
			let mut model = self.lock();
			self.adopt(&mut model, &slot, path)?;
			let file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
			let ino = model.new_inode(Vec::new(), Vec::new());
			model.live.insert(slot.clone(), ino);
			model.pending.push(DirOp::Add { slot, ino });
			(file, ino)
		};
		if let Err(e) = self.fill(file, ino, bytes, policy, points).await {
			// As `RealFs` does, remove the file this call created. The removal is as
			// unsynced as the create, so a power cut may still bring the file back.
			if let Err(remove_err) = self.remove_file(path).await {
				tracing::warn!(path = %path.display(), cause = %e, error = %remove_err, "SimFs could not remove a partially written file");
			}
			return Err(e);
		}
		Ok(())
	}

	async fn sync_file(&self, path: &Path) -> io::Result<()> {
		let slot = self.slot(path)?;
		let mut model = self.lock();
		self.adopt(&mut model, &slot, path)?;
		let ino = model.live_ino(&slot, path)?;
		// The fsync makes durable whatever the file holds now, including bytes written
		// behind `SimFs`'s back.
		let bytes = std::fs::read(path)?;
		let inode = &mut model.inodes[ino];
		inode.durable.clone_from(&bytes);
		inode.current = bytes;
		model.file_syncs += 1;
		Ok(())
	}

	async fn sync_dir(&self, dir: &Path) -> io::Result<()> {
		let rel = if dir == self.root { PathBuf::new() } else { self.relative(dir)?.to_path_buf() };
		if !metadata_blocking(dir)?.is_dir {
			return Err(io::Error::new(io::ErrorKind::NotADirectory, format!("{} is not a directory", dir.display())));
		}
		let mut model = self.lock();
		let in_dir: Vec<usize> = model.pending.iter().enumerate().filter(|(_, op)| op.touches_dir(&rel)).map(|(i, _)| i).collect();
		let durable = with_dependencies(&model.pending, in_dir);
		model.commit(&durable);
		model.dir_syncs += 1;
		Ok(())
	}

	async fn hard_link(&self, src: &Path, dst: &Path) -> io::Result<()> {
		let (src_slot, dst_slot) = (self.slot(src)?, self.slot(dst)?);
		let mut model = self.lock();
		self.adopt(&mut model, &src_slot, src)?;
		self.adopt(&mut model, &dst_slot, dst)?;
		let ino = model.live_ino(&src_slot, src)?;
		std::fs::hard_link(src, dst)?;
		model.live.insert(dst_slot.clone(), ino);
		model.pending.push(DirOp::Add { slot: dst_slot, ino });
		Ok(())
	}

	async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
		let (from_slot, to_slot) = (self.slot(from)?, self.slot(to)?);
		let mut model = self.lock();
		self.adopt(&mut model, &from_slot, from)?;
		self.adopt(&mut model, &to_slot, to)?;
		let ino = model.live_ino(&from_slot, from)?;
		std::fs::rename(from, to)?;
		if from_slot != to_slot {
			model.live.remove(&from_slot);
			model.live.insert(to_slot.clone(), ino);
			model.pending.push(DirOp::Rename { from: from_slot, to: to_slot, ino });
		}
		Ok(())
	}

	async fn remove_file(&self, path: &Path) -> io::Result<()> {
		let slot = self.slot(path)?;
		let mut model = self.lock();
		self.adopt(&mut model, &slot, path)?;
		remove_file_blocking(path)?;
		if model.live.remove(&slot).is_some() {
			model.pending.push(DirOp::Remove { slot });
		}
		Ok(())
	}

	async fn read_dir(&self, dir: &Path) -> io::Result<Vec<FsEntry>> {
		read_dir_blocking(dir)
	}

	async fn metadata(&self, path: &Path) -> io::Result<FsMetadata> {
		metadata_blocking(path)
	}
}

#[cfg(test)]
mod tests {
	use std::collections::HashSet;

	use super::*;
	use crate::types::durable::{
		fault::{arm, FaultAction, FaultPoint}, fs::{write_new_durable, SyncPolicy, WritePoints}
	};

	const SEEDS: u64 = 256;

	/// What a crash image holds where a file was written.
	#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
	enum Outcome {
		Absent,
		Empty,
		Prefix,
		ZeroFilled,
		Complete,
	}

	impl Outcome {
		const fn torn(self) -> bool {
			matches!(self, Self::Empty | Self::Prefix | Self::ZeroFilled)
		}
	}

	/// Classify the image's copy of a file whose full content is `written`, and fail
	/// on anything no power cut could produce.
	fn outcome(image: &Path, rel: &str, written: &[u8]) -> Outcome {
		let bytes = match std::fs::read(image.join(rel)) {
			Err(e) if e.kind() == io::ErrorKind::NotFound => return Outcome::Absent,
			other => other.unwrap(),
		};
		let common = bytes.iter().zip(written).take_while(|(a, b)| a == b).count();
		match bytes.len() {
			0 => Outcome::Empty,
			_ if bytes == written => Outcome::Complete,
			n if n < written.len() && common == n => Outcome::Prefix,
			n if n == written.len() && bytes[common..].iter().all(|&b| b == 0) => Outcome::ZeroFilled,
			_ => panic!("{rel} holds bytes no power cut could leave: {bytes:?}"),
		}
	}

	/// A frame-shaped payload with no zero bytes, so zero fill is recognisable.
	fn payload(len: usize, salt: u8) -> Vec<u8> {
		(0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(salt) | 1).collect()
	}

	/// A store root with a `segments/` directory, and the `SimFs` over it, which
	/// trusts Turso's files as the store's crash tests will.
	fn store() -> (tempfile::TempDir, SimFs) {
		let root = tempfile::tempdir().unwrap();
		std::fs::create_dir(root.path().join("segments")).unwrap();
		let sim = SimFs::exempting(root.path(), SimFs::turso_file).unwrap();
		(root, sim)
	}

	fn cut(sim: &SimFs, seed: u64) -> tempfile::TempDir {
		let image = tempfile::tempdir().unwrap();
		sim.power_cut(seed, image.path()).unwrap();
		image
	}

	fn outcomes(sim: &SimFs, rel: &str, written: &[u8]) -> Vec<Outcome> {
		(0..SEEDS).map(|seed| outcome(cut(sim, seed).path(), rel, written)).collect()
	}

	#[tokio::test]
	async fn synced_data_survives_the_cut() {
		let (root, sim) = store();
		let segments = root.path().join("segments");
		let durable = payload(4096, 1);
		write_new_durable(&sim, &segments, "a~g1~p1.weftseg", durable.clone(), SyncPolicy::Full, WritePoints::NONE).await.unwrap();
		sim.sync_dir(&segments).await.unwrap();
		// Exempt files (Turso's) are copied as they are; unsynced writes after the
		// directory sync must not disturb the synced frame.
		std::fs::write(root.path().join("segment_index.db"), b"turso pages").unwrap();
		write_new_durable(&sim, &segments, "a~g2~p2.weftseg", payload(512, 2), SyncPolicy::None, WritePoints::NONE).await.unwrap();

		for seed in 0..SEEDS {
			let image = cut(&sim, seed);
			assert_eq!(outcome(image.path(), "segments/a~g1~p1.weftseg", &durable), Outcome::Complete, "seed {seed}");
			assert_eq!(std::fs::read(image.path().join("segment_index.db")).unwrap(), b"turso pages", "seed {seed}");
		}
		assert_eq!((sim.file_syncs(), sim.dir_syncs(), sim.pending_dir_ops()), (1, 1, 1));
	}

	#[tokio::test]
	async fn unsynced_variants_are_enumerated_for_fixed_seeds() {
		let (root, sim) = store();
		let segments = root.path().join("segments");
		let written = payload(1000, 3);
		write_new_durable(&sim, &segments, "a~g1~p1.weftseg", written.clone(), SyncPolicy::None, WritePoints::NONE).await.unwrap();

		let seen: HashSet<Outcome> = outcomes(&sim, "segments/a~g1~p1.weftseg", &written).into_iter().collect();
		let every = HashSet::from([Outcome::Absent, Outcome::Empty, Outcome::Prefix, Outcome::ZeroFilled, Outcome::Complete]);
		assert_eq!(seen, every, "seeds 0..{SEEDS} reach every legal outcome of an unsynced file");

		// A seed names one image: replaying it reproduces the image byte for byte.
		for seed in [0, 7, 255] {
			let (first, second) = (cut(&sim, seed), cut(&sim, seed));
			assert_eq!(std::fs::read(first.path().join("segments/a~g1~p1.weftseg")).ok(), std::fs::read(second.path().join("segments/a~g1~p1.weftseg")).ok(), "seed {seed}");
		}
	}

	#[tokio::test]
	async fn write_sync_and_dir_sync_never_tear() {
		let (root, sim) = store();
		let segments = root.path().join("segments");
		let frames: Vec<_> = (0..4u8).map(|i| (format!("a~g{i}~p{i}.weftseg"), payload(300 + 100 * usize::from(i), i))).collect();
		for (name, bytes) in &frames {
			write_new_durable(&sim, &segments, name, bytes.clone(), SyncPolicy::Full, WritePoints::NONE).await.unwrap();
		}
		sim.sync_dir(&segments).await.unwrap();

		for seed in 0..SEEDS {
			let image = cut(&sim, seed);
			for (name, bytes) in &frames {
				assert_eq!(outcome(image.path(), &format!("segments/{name}"), bytes), Outcome::Complete, "seed {seed}, {name}");
			}
		}
	}

	#[tokio::test]
	async fn omitting_the_file_sync_tears_frames() {
		let (root, sim) = store();
		let segments = root.path().join("segments");
		let written = payload(2000, 4);
		write_new_durable(&sim, &segments, "a~g1~p1.weftseg", written.clone(), SyncPolicy::None, WritePoints::NONE).await.unwrap();
		sim.sync_dir(&segments).await.unwrap();

		let seen = outcomes(&sim, "segments/a~g1~p1.weftseg", &written);
		assert!(seen.iter().all(|o| *o != Outcome::Absent), "the directory sync keeps the name");
		assert!(seen.iter().any(|o| o.torn()), "without the file sync, some power cuts tear the frame: {seen:?}");
	}

	#[tokio::test]
	async fn a_synced_file_without_a_dir_sync_is_absent_or_complete() {
		let (root, sim) = store();
		let segments = root.path().join("segments");
		let written = payload(700, 5);
		write_new_durable(&sim, &segments, "a~g1~p1.weftseg", written.clone(), SyncPolicy::Full, WritePoints::NONE).await.unwrap();

		let seen: HashSet<Outcome> = outcomes(&sim, "segments/a~g1~p1.weftseg", &written).into_iter().collect();
		assert_eq!(seen, HashSet::from([Outcome::Absent, Outcome::Complete]), "the bytes are durable, the name is not");

		// Syncing a different directory does not make this entry durable.
		sim.sync_dir(root.path()).await.unwrap();
		assert_eq!(sim.pending_dir_ops(), 1);
	}

	#[tokio::test]
	async fn a_rename_is_atomic_across_the_cut() {
		let (root, sim) = store();
		let segments = root.path().join("segments");
		let written = payload(64, 6);
		write_new_durable(&sim, &segments, ".tmp-a~g1~p1.weftpart", written.clone(), SyncPolicy::Full, WritePoints::NONE).await.unwrap();
		sim.sync_dir(&segments).await.unwrap();
		sim.rename(&segments.join(".tmp-a~g1~p1.weftpart"), &segments.join("a~g1~p1.weftpart")).await.unwrap();
		assert!(segments.join("a~g1~p1.weftpart").exists() && !segments.join(".tmp-a~g1~p1.weftpart").exists(), "the live directory has the rename");

		let mut renamed = 0;
		for seed in 0..SEEDS {
			let image = cut(&sim, seed);
			let tmp = outcome(image.path(), "segments/.tmp-a~g1~p1.weftpart", &written);
			let fin = outcome(image.path(), "segments/a~g1~p1.weftpart", &written);
			assert!(matches!((tmp, fin), (Outcome::Complete, Outcome::Absent) | (Outcome::Absent, Outcome::Complete)), "seed {seed}: exactly one name survives, intact ({tmp:?}, {fin:?})");
			renamed += usize::from(fin == Outcome::Complete);
		}
		assert!(renamed > 0 && renamed < SEEDS as usize, "both sides of the pending rename occur ({renamed} of {SEEDS})");

		sim.sync_dir(&segments).await.unwrap();
		let image = cut(&sim, 0);
		assert_eq!(outcome(image.path(), "segments/a~g1~p1.weftpart", &written), Outcome::Complete);
		assert_eq!(outcome(image.path(), "segments/.tmp-a~g1~p1.weftpart", &written), Outcome::Absent);
	}

	#[tokio::test]
	async fn a_rename_never_outlives_the_create_it_depends_on() {
		let (root, sim) = store();
		let segments = root.path().join("segments");
		let written = payload(64, 7);
		// Neither the create nor the rename is directory-synced.
		write_new_durable(&sim, &segments, ".tmp-x", written.clone(), SyncPolicy::Full, WritePoints::NONE).await.unwrap();
		sim.rename(&segments.join(".tmp-x"), &segments.join("x")).await.unwrap();

		let mut seen = HashSet::new();
		for seed in 0..SEEDS {
			let image = cut(&sim, seed);
			let pair = (outcome(image.path(), "segments/.tmp-x", &written), outcome(image.path(), "segments/x", &written));
			assert_ne!(pair, (Outcome::Complete, Outcome::Complete), "seed {seed}: one file never has both names");
			seen.insert(pair);
		}
		assert_eq!(seen, HashSet::from([(Outcome::Absent, Outcome::Absent), (Outcome::Complete, Outcome::Absent), (Outcome::Absent, Outcome::Complete)]));
	}

	#[tokio::test]
	async fn unsynced_removes_and_links_may_be_lost() {
		let (root, sim) = store();
		let segments = root.path().join("segments");
		let written = payload(128, 8);
		write_new_durable(&sim, &segments, "old", written.clone(), SyncPolicy::Full, WritePoints::NONE).await.unwrap();
		sim.sync_dir(&segments).await.unwrap();
		std::fs::create_dir(root.path().join("backup")).unwrap();
		sim.hard_link(&segments.join("old"), &root.path().join("backup/old")).await.unwrap();
		sim.remove_file(&segments.join("old")).await.unwrap();
		sim.remove_file(&segments.join("old")).await.unwrap();
		assert_eq!(sim.pending_dir_ops(), 2, "removing a missing file is a no-op");

		// The link and the remove are in different directories and neither directory
		// is synced, so each is independently lost or kept. That includes losing the
		// link but keeping the remove, which loses the file: exactly why a backup must
		// fsync its links before anything unlinks the original.
		let mut seen = HashSet::new();
		for seed in 0..SEEDS {
			let image = cut(&sim, seed);
			seen.insert((outcome(image.path(), "segments/old", &written), outcome(image.path(), "backup/old", &written)));
		}
		let (kept, gone) = (Outcome::Complete, Outcome::Absent);
		assert_eq!(seen, HashSet::from([(kept, gone), (kept, kept), (gone, gone), (gone, kept)]));

		sim.sync_dir(&segments).await.unwrap();
		sim.sync_dir(&root.path().join("backup")).await.unwrap();
		let image = cut(&sim, 1);
		assert_eq!((outcome(image.path(), "segments/old", &written), outcome(image.path(), "backup/old", &written)), (Outcome::Absent, Outcome::Complete));
	}

	#[tokio::test]
	async fn writes_behind_simfs_back_are_not_durable() {
		let (root, sim) = store();
		let segments = root.path().join("segments");
		// As the store writes seal frames and sidecars today: with tokio::fs::write,
		// outside StoreFs, and never synced.
		let frame = payload(900, 9);
		std::fs::write(segments.join("a-1.weftseg"), &frame).unwrap();
		std::fs::write(root.path().join("segment_index.db"), b"turso pages").unwrap();
		std::fs::write(root.path().join("segment_index.db-log"), b"turso log").unwrap();

		let seen: HashSet<Outcome> = outcomes(&sim, "segments/a-1.weftseg", &frame).into_iter().collect();
		let every = HashSet::from([Outcome::Absent, Outcome::Empty, Outcome::Prefix, Outcome::ZeroFilled, Outcome::Complete]);
		assert_eq!(seen, every, "a file written behind SimFs's back can come back torn or absent");
		for seed in 0..16 {
			let image = cut(&sim, seed);
			assert_eq!(std::fs::read(image.path().join("segment_index.db")).unwrap(), b"turso pages", "seed {seed}: exempt files are copied as they are");
			assert_eq!(std::fs::read(image.path().join("segment_index.db-log")).unwrap(), b"turso log", "seed {seed}");
		}
		assert!(SimFs::turso_file(Path::new("db/metadata.db")) && !SimFs::turso_file(Path::new("segments/a-1.weftseg")) && !SimFs::turso_file(Path::new("segments/a-1.weftpart")));
	}

	#[tokio::test]
	async fn files_that_predate_the_simulation_are_its_durable_baseline() {
		let root = tempfile::tempdir().unwrap();
		let segments = root.path().join("segments");
		std::fs::create_dir(&segments).unwrap();
		let (kept, old, removed) = (payload(300, 10), payload(400, 11), payload(200, 12));
		std::fs::write(segments.join("kept"), &kept).unwrap();
		std::fs::write(segments.join("rewritten"), &old).unwrap();
		std::fs::write(segments.join("removed"), &removed).unwrap();
		let sim = SimFs::new(root.path()).unwrap();

		// Behind SimFs's back, as the in-place reconcile and the unlinks do today.
		let new = payload(500, 13);
		std::fs::write(segments.join("rewritten"), &new).unwrap();
		std::fs::remove_file(segments.join("removed")).unwrap();

		let (mut rewritten, mut gone) = (HashSet::new(), HashSet::new());
		for seed in 0..SEEDS {
			let image = cut(&sim, seed);
			assert_eq!(outcome(image.path(), "segments/kept", &kept), Outcome::Complete, "seed {seed}: an untouched file is durable");
			let bytes = std::fs::read(image.path().join("segments/rewritten")).unwrap();
			rewritten.insert(if bytes == old { None } else { Some(outcome(image.path(), "segments/rewritten", &new)) });
			gone.insert(outcome(image.path(), "segments/removed", &removed));
		}
		let every_tear = [None, Some(Outcome::Empty), Some(Outcome::Prefix), Some(Outcome::ZeroFilled), Some(Outcome::Complete)];
		assert_eq!(rewritten, HashSet::from(every_tear), "a rewrite in place comes back old, empty, torn or new, never absent");
		assert_eq!(gone, HashSet::from([Outcome::Complete, Outcome::Absent]), "an unsynced removal may be lost");
	}

	#[tokio::test]
	async fn a_file_written_behind_simfs_back_is_not_adopted_as_durable() {
		let (root, sim) = store();
		let segments = root.path().join("segments");
		// Written outside StoreFs, then renamed and directory-synced through it, as a
		// sidecar's tmp-then-rename would be.
		let bytes = payload(600, 14);
		std::fs::write(segments.join(".tmp-a-1.weftpart"), &bytes).unwrap();
		sim.rename(&segments.join(".tmp-a-1.weftpart"), &segments.join("a-1.weftpart")).await.unwrap();
		sim.sync_dir(&segments).await.unwrap();

		let seen: HashSet<Outcome> = outcomes(&sim, "segments/a-1.weftpart", &bytes).into_iter().collect();
		assert_eq!(seen, HashSet::from([Outcome::Empty, Outcome::Prefix, Outcome::ZeroFilled, Outcome::Complete]), "the name is durable, the unsynced bytes are not");
		for seed in 0..16 {
			assert_eq!(outcome(cut(&sim, seed).path(), "segments/.tmp-a-1.weftpart", &bytes), Outcome::Absent, "seed {seed}");
		}

		sim.sync_file(&segments.join("a-1.weftpart")).await.unwrap();
		assert!(outcomes(&sim, "segments/a-1.weftpart", &bytes).iter().all(|o| *o == Outcome::Complete), "a sync through SimFs makes them durable");
	}

	#[tokio::test]
	async fn a_tracked_file_changed_behind_simfs_back_can_tear_or_reappear() {
		let (root, sim) = store();
		let segments = root.path().join("segments");
		let (old, new) = (payload(800, 15), payload(800, 16));
		write_new_durable(&sim, &segments, "a-1.weftseg", old.clone(), SyncPolicy::Full, WritePoints::NONE).await.unwrap();
		write_new_durable(&sim, &segments, "a-2.weftseg", old.clone(), SyncPolicy::Full, WritePoints::NONE).await.unwrap();
		sim.sync_dir(&segments).await.unwrap();
		// An in-place rewrite and an unlink, outside StoreFs.
		std::fs::write(segments.join("a-1.weftseg"), &new).unwrap();
		std::fs::remove_file(segments.join("a-2.weftseg")).unwrap();

		let (mut rewritten, mut removed) = (HashSet::new(), HashSet::new());
		for seed in 0..SEEDS {
			let image = cut(&sim, seed);
			let bytes = std::fs::read(image.path().join("segments/a-1.weftseg")).unwrap();
			rewritten.insert(if bytes == old { None } else { Some(outcome(image.path(), "segments/a-1.weftseg", &new)) });
			removed.insert(outcome(image.path(), "segments/a-2.weftseg", &old));
		}
		assert!(rewritten.contains(&None) && rewritten.iter().any(|o| o.is_some_and(Outcome::torn)), "old or torn: {rewritten:?}");
		assert_eq!(removed, HashSet::from([Outcome::Complete, Outcome::Absent]));
	}

	#[tokio::test]
	async fn syncing_a_links_directory_leaves_the_sources_create_pending() {
		let (root, sim) = store();
		let segments = root.path().join("segments");
		let backup = root.path().join("backup");
		std::fs::create_dir(&backup).unwrap();
		let written = payload(256, 17);
		write_new_durable(&sim, &segments, "f", written.clone(), SyncPolicy::Full, WritePoints::NONE).await.unwrap();
		sim.hard_link(&segments.join("f"), &backup.join("f")).await.unwrap();
		sim.sync_dir(&backup).await.unwrap();
		assert_eq!(sim.pending_dir_ops(), 1, "segments/ was never synced, so its create is still pending");

		let mut seen = HashSet::new();
		for seed in 0..SEEDS {
			let image = cut(&sim, seed);
			seen.insert((outcome(image.path(), "segments/f", &written), outcome(image.path(), "backup/f", &written)));
		}
		assert_eq!(seen, HashSet::from([(Outcome::Complete, Outcome::Complete), (Outcome::Absent, Outcome::Complete)]), "the link survives, with or without the original name");
	}

	#[tokio::test]
	async fn a_create_never_applies_without_the_rename_that_freed_its_name() {
		let (root, sim) = store();
		let segments = root.path().join("segments");
		let (old, new) = (payload(128, 18), payload(96, 19));
		write_new_durable(&sim, &segments, "x", old.clone(), SyncPolicy::Full, WritePoints::NONE).await.unwrap();
		sim.sync_dir(&segments).await.unwrap();
		sim.rename(&segments.join("x"), &segments.join("y")).await.unwrap();
		write_new_durable(&sim, &segments, "x", new.clone(), SyncPolicy::Full, WritePoints::NONE).await.unwrap();

		for seed in 0..SEEDS {
			let image = cut(&sim, seed);
			let (x, y) = (std::fs::read(image.path().join("segments/x")).ok(), std::fs::read(image.path().join("segments/y")).ok());
			assert!(x.as_ref() == Some(&old) || y.as_ref() == Some(&old), "seed {seed}: the old file keeps a name (x={:?}, y={:?})", x.map(|b| b.len()), y.map(|b| b.len()));
		}
	}

	#[tokio::test]
	async fn every_name_of_a_file_shows_the_same_content() {
		let (root, sim) = store();
		let segments = root.path().join("segments");
		let backup = root.path().join("backup");
		std::fs::create_dir(&backup).unwrap();
		let written = payload(1500, 20);
		write_new_durable(&sim, &segments, "f", written.clone(), SyncPolicy::None, WritePoints::NONE).await.unwrap();
		sim.hard_link(&segments.join("f"), &backup.join("f")).await.unwrap();
		sim.sync_dir(&segments).await.unwrap();
		sim.sync_dir(&backup).await.unwrap();

		let mut torn = 0;
		for seed in 0..SEEDS {
			let image = cut(&sim, seed);
			let (a, b) = (std::fs::read(image.path().join("segments/f")).unwrap(), std::fs::read(image.path().join("backup/f")).unwrap());
			assert_eq!(a, b, "seed {seed}: two names of one inode cannot hold different bytes");
			torn += usize::from(outcome(image.path(), "segments/f", &written).torn());
		}
		assert!(torn > 0, "the unsynced bytes do tear");
	}

	#[tokio::test]
	async fn a_failed_write_removes_its_file_but_not_durably() {
		let (root, sim) = store();
		let segments = root.path().join("segments");
		let written = payload(64, 21);
		let point = FaultPoint::MOutputWritten;
		let _armed = arm(point, FaultAction::ReturnErr);
		let err = write_new_durable(&sim, &segments, "out", written.clone(), SyncPolicy::Full, WritePoints { written: Some(point), ..WritePoints::NONE }).await.unwrap_err();
		assert_eq!(crate::types::durable::fault::injected_point(&err), Some(point));
		assert!(!segments.join("out").exists(), "the live directory no longer has the file");
		assert_eq!(sim.file_syncs(), 0, "the write failed before its fsync");

		// Neither the create nor the removal is directory-synced, so a power cut may keep
		// the create alone: litter for recovery, never a durable frame.
		let seen: HashSet<Outcome> = outcomes(&sim, "segments/out", &written).into_iter().collect();
		assert!(seen.contains(&Outcome::Absent) && seen.iter().any(|o| o.torn()), "{seen:?}");
	}

	#[tokio::test]
	async fn operations_pass_through_to_the_real_directory() {
		let root = tempfile::tempdir().unwrap();
		let segments = root.path().join("segments");
		std::fs::create_dir(&segments).unwrap();
		// A file that predates the simulation counts as durable and is never clobbered.
		std::fs::write(segments.join("a-1.weftseg"), b"legacy frame").unwrap();
		let sim = SimFs::new(root.path()).unwrap();
		let err = sim.create_new_write(&segments.join("a-1.weftseg"), b"clobber".to_vec(), SyncPolicy::Full, WritePoints::NONE).await.unwrap_err();
		assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);

		sim.create_new_write(&segments.join("new"), b"live bytes".to_vec(), SyncPolicy::None, WritePoints::NONE).await.unwrap();
		let names: Vec<_> = sim.read_dir(&segments).await.unwrap().into_iter().map(|e| e.name).collect();
		assert_eq!(names, vec![OsString::from("a-1.weftseg"), OsString::from("new")]);
		assert_eq!(sim.metadata(&segments.join("new")).await.unwrap().len, 10);
		assert_eq!(std::fs::read(segments.join("new")).unwrap(), b"live bytes", "the write reached the real file");

		for seed in 0..16 {
			let image = cut(&sim, seed);
			assert_eq!(std::fs::read(image.path().join("segments/a-1.weftseg")).unwrap(), b"legacy frame", "seed {seed}");
		}
		assert_eq!(sim.sync_file(&segments.join("missing")).await.unwrap_err().kind(), io::ErrorKind::NotFound);
	}

	#[tokio::test]
	async fn misuse_is_refused() {
		let (root, sim) = store();
		let outside = tempfile::tempdir().unwrap();
		assert_eq!(sim.create_new_write(&outside.path().join("x"), vec![1], SyncPolicy::Full, WritePoints::NONE).await.unwrap_err().kind(), io::ErrorKind::InvalidInput, "paths outside the root are refused");
		assert_eq!(sim.create_new_write(&root.path().join("segments/../x"), vec![1], SyncPolicy::Full, WritePoints::NONE).await.unwrap_err().kind(), io::ErrorKind::InvalidInput);
		assert_eq!(sim.rename(&root.path().join("segments"), &root.path().join("moved")).await.unwrap_err().kind(), io::ErrorKind::Unsupported, "directory renames are not simulated");
		assert_eq!(sim.power_cut(0, &root.path().join("image")).unwrap_err().kind(), io::ErrorKind::InvalidInput, "the image cannot live inside the root");
		std::fs::write(outside.path().join("stale"), b"x").unwrap();
		assert_eq!(sim.power_cut(0, outside.path()).unwrap_err().kind(), io::ErrorKind::AlreadyExists, "the image directory must be fresh");
	}
}
