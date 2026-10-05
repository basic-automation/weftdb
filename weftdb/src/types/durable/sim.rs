//! A power-cut simulating [`StoreFs`] for the crash tests (design section 11).
//!
//! [`SimFs`] performs every operation on a real directory, so the store, Turso and
//! the read paths see an ordinary live filesystem. Alongside, it keeps a model of
//! what a power cut would preserve: per file, the bytes made durable by `sync_file`
//! versus the bytes merely written; per directory entry, the state made durable by
//! `sync_dir` versus the operations still pending. [`SimFs::power_cut`] then writes
//! one legal post-crash image of the root into a fresh directory, chosen by a seed:
//!
//! - an unsynced file comes back absent (its entry was lost), empty, as a prefix of
//!   what was written, as a prefix followed by zero fill (the size reached the disk
//!   but the data blocks did not), or complete;
//! - each pending directory operation is either applied or not. An operation is
//!   applied only together with the earlier pending operations it depends on (those
//!   on the same name or the same file), so the image never holds a state no
//!   filesystem could produce, such as a rename's target without its source ever
//!   having existed;
//! - everything `SimFs` never touched (Turso's database files, for example) is copied
//!   as it is. That is sound for Turso because every COMMIT that returned was FULL
//!   fsynced (asserted at open in S3).
//!
//! The model is deliberately harsher than ext4 or XFS in ordered mode, which commit
//! metadata as a journal prefix: a directory fsync here makes durable only the
//! operations in that directory (plus their dependencies), which is all POSIX
//! promises. Directories themselves are not modelled: they must exist before `SimFs`
//! touches them and are assumed durable, and renaming a directory through `SimFs` is
//! refused rather than simulated wrongly.

use std::{
	collections::{BTreeMap, BTreeSet}, ffi::OsString, io, path::{Component, Path, PathBuf}, sync::{Mutex, MutexGuard, PoisonError}
};

use async_trait::async_trait;

use super::fs::{create_new_write_blocking, metadata_blocking, read_dir_blocking, remove_file_blocking, FsEntry, FsMetadata, StoreFs};

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

impl Inode {
	/// The content this file has after a power cut, drawn from `rng`.
	fn after_cut(&self, rng: &mut fastrand::Rng) -> Vec<u8> {
		let (durable, current) = (&self.durable, &self.current);
		if durable == current {
			return current.clone();
		}
		// Files are written once, so the unsynced bytes always extend the durable ones;
		// anything else could only be the old or the new content.
		if !current.starts_with(durable) {
			return if rng.bool() { durable.clone() } else { current.clone() };
		}
		match rng.u8(0..4) {
			// Part of the unsynced bytes reached the disk.
			1 if current.len() - durable.len() >= 2 => current[..rng.usize(durable.len() + 1..current.len())].to_vec(),
			// The new size reached the disk but not all the data blocks.
			2 => {
				let mut torn = current[..rng.usize(durable.len()..current.len())].to_vec();
				torn.resize(current.len(), 0);
				torn
			}
			3 => current.clone(),
			// None of them did, so a new file comes back empty. (A one-byte tail has no
			// partial prefix, so that draw lands here too.)
			_ => durable.clone(),
		}
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

	/// The file the operation needs to exist. A removal needs only its own name.
	const fn ino(&self) -> Option<Ino> {
		match self {
			Self::Add { ino, .. } | Self::Rename { ino, .. } => Some(*ino),
			Self::Remove { .. } => None,
		}
	}

	fn touches_dir(&self, dir: &Path) -> bool {
		self.slots().any(|(slot_dir, _)| slot_dir == dir)
	}

	/// Whether this operation is only meaningful once `earlier` has been applied.
	fn depends_on(&self, earlier: &Self) -> bool {
		let same_slot = self.slots().any(|slot| earlier.slots().any(|e| e == slot));
		let same_file = matches!((self.ino(), earlier.ino()), (Some(a), Some(b)) if a == b);
		same_slot || same_file
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
	/// Every entry the model has an opinion about. Untracked entries are copied into
	/// an image as they are.
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

	/// Start tracking `slot` before the first operation on it. A file already on disk
	/// predates the simulation and counts as durable.
	fn adopt(&mut self, slot: &Slot, path: &Path) -> io::Result<()> {
		if self.known.contains(slot) {
			return Ok(());
		}
		match std::fs::symlink_metadata(path) {
			Ok(meta) if meta.is_dir() => return Err(io::Error::new(io::ErrorKind::Unsupported, format!("SimFs models file entries only, and {} is a directory", path.display()))),
			Ok(_) => {
				let bytes = std::fs::read(path)?;
				let ino = self.new_inode(bytes.clone(), bytes);
				self.durable.insert(slot.clone(), ino);
				self.live.insert(slot.clone(), ino);
			}
			Err(e) if e.kind() == io::ErrorKind::NotFound => {}
			Err(e) => return Err(e),
		}
		self.known.insert(slot.clone());
		Ok(())
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
#[derive(Debug)]
pub struct SimFs {
	root: PathBuf,
	model: Mutex<Model>,
}

impl SimFs {
	/// Simulate over the existing directory `root`. Every path passed to the
	/// [`StoreFs`] methods must lie under it.
	pub fn new(root: impl Into<PathBuf>) -> Self {
		Self { root: root.into(), model: Mutex::default() }
	}

	/// The simulated root.
	#[must_use]
	pub fn root(&self) -> &Path {
		&self.root
	}

	/// How many `sync_file` calls have succeeded.
	#[must_use]
	pub fn file_syncs(&self) -> u64 {
		self.lock().file_syncs
	}

	/// How many `sync_dir` calls have succeeded.
	#[must_use]
	pub fn dir_syncs(&self) -> u64 {
		self.lock().dir_syncs
	}

	/// How many directory operations a power cut could still lose.
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
		copy_untracked(&self.root, dest, Path::new(""), &model.known)?;

		let mut rng = fastrand::Rng::with_seed(seed);
		let flipped: Vec<usize> = (0..model.pending.len()).filter(|_| rng.bool()).collect();
		let applied = with_dependencies(&model.pending, flipped);
		let mut entries = model.durable.clone();
		for (i, op) in model.pending.iter().enumerate() {
			if applied.contains(&i) {
				op.apply(&mut entries);
			}
		}

		// One outcome per file, shared by all of its names.
		let files: BTreeSet<Ino> = entries.values().copied().collect();
		let contents: BTreeMap<Ino, Vec<u8>> = files.into_iter().map(|ino| (ino, model.inodes[ino].after_cut(&mut rng))).collect();
		for ((dir, name), ino) in &entries {
			let dir = dest.join(dir);
			std::fs::create_dir_all(&dir)?;
			std::fs::write(dir.join(name), &contents[ino])?;
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
}

/// Copy every file under `src/rel` that the model does not track into `dest/rel`,
/// recreating the directory tree. Only regular files and directories belong in a
/// store; anything else is skipped.
fn copy_untracked(src: &Path, dest: &Path, rel: &Path, known: &BTreeSet<Slot>) -> io::Result<()> {
	for entry in std::fs::read_dir(src.join(rel))? {
		let entry = entry?;
		let name = entry.file_name();
		let child = rel.join(&name);
		let kind = entry.file_type()?;
		if kind.is_dir() {
			std::fs::create_dir_all(dest.join(&child))?;
			copy_untracked(src, dest, &child, known)?;
		} else if kind.is_file() && !known.contains(&(rel.to_path_buf(), name)) {
			std::fs::copy(src.join(&child), dest.join(&child))?;
		}
	}
	Ok(())
}

// Each method holds the model lock across the real operation, so the model and the
// directory never disagree about the order operations happened in.
#[allow(clippy::significant_drop_tightening)]
#[async_trait]
impl StoreFs for SimFs {
	async fn create_new_write(&self, path: &Path, bytes: Vec<u8>) -> io::Result<()> {
		let slot = self.slot(path)?;
		let mut model = self.lock();
		model.adopt(&slot, path)?;
		create_new_write_blocking(path, &bytes)?;
		let ino = model.new_inode(Vec::new(), bytes);
		model.live.insert(slot.clone(), ino);
		model.pending.push(DirOp::Add { slot, ino });
		Ok(())
	}

	async fn sync_file(&self, path: &Path) -> io::Result<()> {
		let slot = self.slot(path)?;
		let mut model = self.lock();
		model.adopt(&slot, path)?;
		let ino = model.live_ino(&slot, path)?;
		let inode = &mut model.inodes[ino];
		inode.durable.clone_from(&inode.current);
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
		model.adopt(&src_slot, src)?;
		model.adopt(&dst_slot, dst)?;
		let ino = model.live_ino(&src_slot, src)?;
		std::fs::hard_link(src, dst)?;
		model.live.insert(dst_slot.clone(), ino);
		model.pending.push(DirOp::Add { slot: dst_slot, ino });
		Ok(())
	}

	async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
		let (from_slot, to_slot) = (self.slot(from)?, self.slot(to)?);
		let mut model = self.lock();
		model.adopt(&from_slot, from)?;
		model.adopt(&to_slot, to)?;
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
		model.adopt(&slot, path)?;
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
	use crate::types::durable::fs::{write_new_durable, SyncPolicy};

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

	/// A store root with a `segments/` directory, and the `SimFs` over it.
	fn store() -> (tempfile::TempDir, SimFs) {
		let root = tempfile::tempdir().unwrap();
		std::fs::create_dir(root.path().join("segments")).unwrap();
		let sim = SimFs::new(root.path());
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
		write_new_durable(&sim, &segments, "a~g1~p1.weftseg", durable.clone(), SyncPolicy::Full).await.unwrap();
		sim.sync_dir(&segments).await.unwrap();
		// Untracked files (Turso's) are copied as they are; unsynced writes after the
		// directory sync must not disturb the synced frame.
		std::fs::write(root.path().join("segment_index.db"), b"turso pages").unwrap();
		write_new_durable(&sim, &segments, "a~g2~p2.weftseg", payload(512, 2), SyncPolicy::None).await.unwrap();

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
		write_new_durable(&sim, &segments, "a~g1~p1.weftseg", written.clone(), SyncPolicy::None).await.unwrap();

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
			write_new_durable(&sim, &segments, name, bytes.clone(), SyncPolicy::Full).await.unwrap();
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
		write_new_durable(&sim, &segments, "a~g1~p1.weftseg", written.clone(), SyncPolicy::None).await.unwrap();
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
		write_new_durable(&sim, &segments, "a~g1~p1.weftseg", written.clone(), SyncPolicy::Full).await.unwrap();

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
		write_new_durable(&sim, &segments, ".tmp-a~g1~p1.weftpart", written.clone(), SyncPolicy::Full).await.unwrap();
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
		write_new_durable(&sim, &segments, ".tmp-x", written.clone(), SyncPolicy::Full).await.unwrap();
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
		write_new_durable(&sim, &segments, "old", written.clone(), SyncPolicy::Full).await.unwrap();
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
	async fn operations_pass_through_to_the_real_directory() {
		let (root, sim) = store();
		let segments = root.path().join("segments");
		// A file that predates the simulation counts as durable and is never clobbered.
		std::fs::write(segments.join("a-1.weftseg"), b"legacy frame").unwrap();
		let err = sim.create_new_write(&segments.join("a-1.weftseg"), b"clobber".to_vec()).await.unwrap_err();
		assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);

		sim.create_new_write(&segments.join("new"), b"live bytes".to_vec()).await.unwrap();
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
		assert_eq!(sim.create_new_write(&outside.path().join("x"), vec![1]).await.unwrap_err().kind(), io::ErrorKind::InvalidInput, "paths outside the root are refused");
		assert_eq!(sim.create_new_write(&root.path().join("segments/../x"), vec![1]).await.unwrap_err().kind(), io::ErrorKind::InvalidInput);
		assert_eq!(sim.rename(&root.path().join("segments"), &root.path().join("moved")).await.unwrap_err().kind(), io::ErrorKind::Unsupported, "directory renames are not simulated");
		assert_eq!(sim.power_cut(0, &root.path().join("image")).unwrap_err().kind(), io::ErrorKind::InvalidInput, "the image cannot live inside the root");
		std::fs::write(outside.path().join("stale"), b"x").unwrap();
		assert_eq!(sim.power_cut(0, outside.path()).unwrap_err().kind(), io::ErrorKind::AlreadyExists, "the image directory must be fresh");
	}
}
