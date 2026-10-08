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
//! - writes that bypass `SimFs` are not trusted. [`SimFs::new`] records every file and
//!   directory under the root as the durable baseline. A file or directory created,
//!   rewritten or removed since then by anything other than `SimFs` (`tokio::fs::write`,
//!   say), including one `SimFs` tracks, gets the outcomes of an unsynced one, so a
//!   write path that bypasses [`StoreFs`] shows up in the crash tests as torn or lost
//!   data instead of passing unexamined. Only the files the caller exempts are copied
//!   as they are: [`SimFs::turso_file`] exempts Turso's databases and logs, which is
//!   sound because every COMMIT that returned was FULL fsynced (asserted at open in
//!   S3).
//!
//! Directories are modelled like files: a directory is an inode, and an entry is
//! keyed by the inode of the directory holding it, not by its path. So creating,
//! renaming or removing a directory is a pending operation in its parent, durable only
//! once the parent is synced, and a directory carries its whole subtree: a file
//! durably created inside a directory whose own entry was lost is lost with it, and a
//! rename that does not survive the cut leaves the subtree, including entries made
//! after the rename, under the old name. Removing a directory tree is one pending
//! removal per entry plus one per directory, so a cut can leave a directory with only
//! some of its entries gone, the state a backup's `.deleting-*` rename exists to hide
//! (design section 9). An exempt file survives exactly when its directory does.
//!
//! The model is deliberately harsher than ext4 or XFS in ordered mode, which commit
//! metadata as a journal prefix: a directory fsync here makes durable only the
//! operations in that directory (plus their dependencies), which is all POSIX
//! promises. Changes made behind `SimFs`'s back carry no order at all, so a test that
//! needs one must route the operation through [`StoreFs`].

use std::{
	collections::{BTreeMap, BTreeSet}, ffi::{OsStr, OsString}, fmt, fs::File, io::{self, Write}, path::{Component, Path, PathBuf}, sync::{Mutex, MutexGuard, PoisonError}
};

use async_trait::async_trait;

use super::{
	fault, fs::{metadata_blocking, read_dir_blocking, remove_file_blocking, FsEntry, FsMetadata, StoreFs, SyncPolicy, WritePoints}
};

/// An index into [`Model::nodes`]. Hard links share a file's; a directory has one name.
type Ino = usize;
/// The simulated root directory's inode.
const ROOT: Ino = 0;
/// A directory entry: (the inode of the directory holding it, its name). Keying by the
/// directory's inode rather than its path is what lets a directory rename carry its
/// subtree whatever has or has not been synced inside it.
type Slot = (Ino, OsString);

/// A file or directory.
#[derive(Debug)]
enum Node {
	/// A regular file: what a power cut is guaranteed to keep, and what is written.
	File { durable: Vec<u8>, current: Vec<u8> },
	/// A directory. Its entries are the model's slots that name it as their directory.
	Dir,
}

impl Node {
	const fn kind(&self) -> Kind {
		match self {
			Self::File { .. } => Kind::File,
			Self::Dir => Kind::Dir,
		}
	}
}

/// What is at a path on disk, among the two things a store holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
	File,
	Dir,
}

/// What is at `path` on disk: a regular file, a directory, or (for nothing, or for
/// anything else, which has no place in a store) `None`. Symlinks are not followed.
fn disk_kind(path: &Path) -> io::Result<Option<Kind>> {
	match std::fs::symlink_metadata(path) {
		Ok(meta) if meta.is_dir() => Ok(Some(Kind::Dir)),
		Ok(meta) if meta.is_file() => Ok(Some(Kind::File)),
		Ok(_) => Ok(None),
		Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
		Err(e) => Err(e),
	}
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
	/// `slot` now names `ino`: a create, a hard link or a `mkdir`.
	Add { slot: Slot, ino: Ino },
	/// `slot` no longer names anything: an unlink or an `rmdir`.
	Remove { slot: Slot },
	/// `from`'s file or directory moved to `to`, replacing whatever `to` named.
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

	fn touches_dir(&self, dir: Ino) -> bool {
		self.slots().any(|(slot_dir, _)| *slot_dir == dir)
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
	/// involves the same file. Nor is an entry inside a directory dependent on the
	/// directory's own entry: it is durable in the directory's inode, and is simply
	/// unreachable in an image that lost the directory.
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

/// The entries of `map` held by directory `dir`, in name order.
fn entries_of(map: &BTreeMap<Slot, Ino>, dir: Ino) -> impl Iterator<Item = (&OsString, Ino)> {
	map.range((dir, OsString::new())..).take_while(move |((holder, _), _)| *holder == dir).map(|((_, name), ino)| (name, *ino))
}

#[derive(Debug)]
struct Model {
	/// Every file and directory the model has seen; `nodes[ROOT]` is the root.
	nodes: Vec<Node>,
	/// The entries that survive any power cut.
	durable: BTreeMap<Slot, Ino>,
	/// The entries as `SimFs` last saw them. A change made behind its back is folded in
	/// when an operation next touches the entry, or judged at the cut.
	live: BTreeMap<Slot, Ino>,
	/// Directory operations since the last `sync_dir` covering them, in order.
	pending: Vec<DirOp>,
	file_syncs: u64,
	dir_syncs: u64,
}

impl Default for Model {
	fn default() -> Self {
		Self { nodes: vec![Node::Dir], durable: BTreeMap::new(), live: BTreeMap::new(), pending: Vec::new(), file_syncs: 0, dir_syncs: 0 }
	}
}

impl Model {
	fn new_node(&mut self, node: Node) -> Ino {
		self.nodes.push(node);
		self.nodes.len() - 1
	}

	/// Record `slot` as naming a new `node` now, with the entry not yet durable.
	fn add(&mut self, slot: Slot, node: Node) -> Ino {
		let ino = self.new_node(node);
		self.live.insert(slot.clone(), ino);
		self.pending.push(DirOp::Add { slot, ino });
		ino
	}

	/// Record that `slot` no longer names anything, if it did.
	fn remove(&mut self, slot: Slot) {
		if self.live.remove(&slot).is_some() {
			self.pending.push(DirOp::Remove { slot });
		}
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
		self.live.get(slot).copied().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("SimFs has nothing at {}", path.display())))
	}
}

/// A [`StoreFs`] over a real directory that can also produce the image a power cut
/// at this instant could leave behind. See the module docs for the crash model.
pub struct SimFs {
	root: PathBuf,
	/// Files (by path relative to the root) trusted as durable whatever happens to
	/// them. They are never modelled; an image copies them as they are at the cut.
	exempt: fn(&Path) -> bool,
	model: Mutex<Model>,
}

impl fmt::Debug for SimFs {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("SimFs").field("root", &self.root).field("nodes", &self.lock().nodes.len()).finish_non_exhaustive()
	}
}

/// The state a power cut is computed over: the image's entries, and what is on disk
/// for the files in them.
struct Cut<'a> {
	model: &'a Model,
	rng: fastrand::Rng,
	entries: BTreeMap<Slot, Ino>,
	/// What modelled files hold on disk at the cut, by inode.
	on_disk: BTreeMap<Ino, Vec<u8>>,
	/// Files and directories made behind `SimFs`'s back, numbered after the model's
	/// nodes. A file's content here is already its post-cut content.
	unmodelled: BTreeMap<Ino, Option<Vec<u8>>>,
}

impl Cut<'_> {
	/// Put an entry the model never saw into the image; `None` is a directory.
	fn add_unmodelled(&mut self, slot: Slot, content: Option<Vec<u8>>) -> Ino {
		let ino = self.model.nodes.len() + self.unmodelled.len();
		self.unmodelled.insert(ino, content);
		self.entries.insert(slot, ino);
		ino
	}

	fn is_dir(&self, ino: Ino) -> bool {
		self.model.nodes.get(ino).map_or_else(|| matches!(self.unmodelled.get(&ino), Some(None)), |node| node.kind() == Kind::Dir)
	}
}

impl SimFs {
	/// Simulate over the existing directory `root`, taking every file and directory
	/// under it now as durable and every later change not made through `SimFs` as
	/// unsynced. Every path passed to the [`StoreFs`] methods must lie under the root.
	///
	/// # Errors
	///
	/// Any error reading the root.
	pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
		Self::exempting(root, |_| false)
	}

	/// Like [`new`](Self::new), but a file whose path relative to the root satisfies
	/// `exempt` is trusted as durable whatever happens to it, and is copied into every
	/// image as it is (when its directory survives). Pass [`SimFs::turso_file`] when the
	/// root holds Turso databases.
	///
	/// # Errors
	///
	/// Any error reading the root.
	pub fn exempting(root: impl Into<PathBuf>, exempt: fn(&Path) -> bool) -> io::Result<Self> {
		let sim = Self { root: root.into(), exempt, model: Mutex::default() };
		{
			let mut model = sim.lock();
			sim.record_baseline(&mut model, ROOT, &sim.root)?;
		}
		Ok(sim)
	}

	/// Record everything under `path` (directory `dir`) as durable as it is now.
	fn record_baseline(&self, model: &mut Model, dir: Ino, path: &Path) -> io::Result<()> {
		for entry in read_dir_blocking(path)? {
			let child = path.join(&entry.name);
			let node = match disk_kind(&child)? {
				Some(Kind::Dir) => Node::Dir,
				Some(Kind::File) if !self.is_exempt(&child) => {
					let bytes = std::fs::read(&child)?;
					Node::File { durable: bytes.clone(), current: bytes }
				}
				_ => continue,
			};
			let is_dir = node.kind() == Kind::Dir;
			let ino = model.new_node(node);
			let slot = (dir, entry.name);
			model.durable.insert(slot.clone(), ino);
			model.live.insert(slot, ino);
			if is_dir {
				self.record_baseline(model, ino, &child)?;
			}
		}
		Ok(())
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
	/// [`SyncPolicy::Full`] writes and copies.
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

		// The durable entries plus a seeded subset of the pending operations, each with
		// the earlier ones it depends on.
		let mut rng = fastrand::Rng::with_seed(seed);
		let flipped: Vec<usize> = (0..model.pending.len()).filter(|_| rng.bool()).collect();
		let applied = with_dependencies(&model.pending, flipped);
		let mut entries = model.durable.clone();
		for (i, op) in model.pending.iter().enumerate() {
			if applied.contains(&i) {
				op.apply(&mut entries);
			}
		}

		// Then whatever changed behind `SimFs`'s back, as unordered and unsynced as any
		// other untracked change.
		let mut cut = Cut { model: &model, rng, entries, on_disk: BTreeMap::new(), unmodelled: BTreeMap::new() };
		self.survey(&mut cut, ROOT, &self.root)?;

		// Only what is reachable from the root is in the image: an entry whose directory
		// lost its own entry is lost with it.
		let mut files: BTreeMap<Ino, Vec<PathBuf>> = BTreeMap::new();
		let mut visited = BTreeSet::from([ROOT]);
		let mut stack = vec![(ROOT, dest.to_path_buf())];
		while let Some((dir, path)) = stack.pop() {
			for (name, ino) in entries_of(&cut.entries, dir) {
				let child = path.join(name);
				if !cut.is_dir(ino) {
					files.entry(ino).or_default().push(child);
				} else if visited.insert(ino) {
					std::fs::create_dir(&child)?;
					stack.push((ino, child));
				}
			}
		}

		// One outcome per file, shared by all of its names.
		for (ino, names) in files {
			let bytes = match (model.nodes.get(ino), cut.unmodelled.get(&ino)) {
				(Some(Node::File { durable, current }), _) => after_cut(durable, cut.on_disk.get(&ino).unwrap_or(current), &mut cut.rng),
				(_, Some(Some(bytes))) => bytes.clone(),
				_ => continue,
			};
			for name in names {
				std::fs::write(name, &bytes)?;
			}
		}
		Ok(())
	}

	/// Walk directory `dir` (at `path` on disk) and fold into the cut every difference
	/// between the disk and the model's live entries: a removal made behind `SimFs`'s
	/// back may be lost, and a file or directory made behind its back may be absent (a
	/// directory with its whole subtree) or, for a file, torn. Exempt files are copied
	/// as they are.
	fn survey(&self, cut: &mut Cut<'_>, dir: Ino, path: &Path) -> io::Result<()> {
		let mut names: BTreeSet<OsString> = read_dir_blocking(path)?.into_iter().map(|entry| entry.name).collect();
		names.extend(entries_of(&cut.model.live, dir).map(|(name, _)| name.clone()));
		for name in names {
			let child = path.join(&name);
			let slot = (dir, name);
			let on_disk = disk_kind(&child)?;
			if let Some(&ino) = cut.model.live.get(&slot) {
				match (cut.model.nodes[ino].kind(), on_disk) {
					(Kind::File, Some(Kind::File)) => {
						cut.on_disk.insert(ino, std::fs::read(&child)?);
						continue;
					}
					(Kind::Dir, Some(Kind::Dir)) => {
						self.survey(cut, ino, &child)?;
						continue;
					}
					// Removed, or replaced by another kind of entry, behind `SimFs`'s back:
					// the removal may not have reached the disk.
					_ => {
						if cut.rng.bool() && cut.entries.get(&slot) == Some(&ino) {
							cut.entries.remove(&slot);
						}
					}
				}
			}
			match on_disk {
				Some(Kind::File) if self.is_exempt(&child) => {
					cut.add_unmodelled(slot, Some(std::fs::read(&child)?));
				}
				// Created without either sync: absent, or torn.
				Some(Kind::File) => {
					if cut.rng.bool() {
						let bytes = std::fs::read(&child)?;
						let torn = after_cut(&[], &bytes, &mut cut.rng);
						cut.add_unmodelled(slot, Some(torn));
					}
				}
				// A directory whose entry reached the disk holds only entries made behind
				// `SimFs`'s back too; one whose entry did not loses its whole subtree.
				Some(Kind::Dir) if cut.rng.bool() => {
					let ino = cut.add_unmodelled(slot, None);
					self.survey(cut, ino, &child)?;
				}
				Some(Kind::Dir) | None => {}
			}
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

	fn is_exempt(&self, path: &Path) -> bool {
		path.strip_prefix(&self.root).is_ok_and(|rel| (self.exempt)(rel))
	}

	/// The live inode of the directory at `rel` (relative to the root), folding into
	/// the model any directory on the way that was made behind `SimFs`'s back.
	fn dir_at(&self, model: &mut Model, rel: &Path) -> io::Result<Ino> {
		let mut dir = ROOT;
		let mut path = self.root.clone();
		for component in rel.components() {
			path.push(component);
			let slot = (dir, component.as_os_str().to_os_string());
			self.reconcile(model, &slot, &path)?;
			dir = match model.live.get(&slot) {
				Some(&ino) if model.nodes[ino].kind() == Kind::Dir => ino,
				Some(_) => return Err(io::Error::new(io::ErrorKind::NotADirectory, format!("{} is not a directory", path.display()))),
				None => return Err(io::Error::new(io::ErrorKind::NotFound, format!("no directory at {}", path.display()))),
			};
		}
		Ok(dir)
	}

	/// The entry `path` names.
	fn slot(&self, model: &mut Model, path: &Path) -> io::Result<Slot> {
		let rel = self.relative(path)?;
		match (rel.parent(), rel.file_name()) {
			(Some(dir), Some(name)) => Ok((self.dir_at(model, dir)?, name.to_os_string())),
			_ => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} names the simulated root itself, not an entry under it", path.display()))),
		}
	}

	/// Bring the model's live entry at `slot` in line with what is at `path` on disk
	/// before an operation on it. A removal made behind `SimFs`'s back becomes a pending
	/// removal, and a file or directory made behind its back a pending create whose
	/// content (for a file) is not durable either. A file rewritten behind its back
	/// keeps its entry: the cut judges its content against the disk. Exempt files are
	/// never modelled.
	fn reconcile(&self, model: &mut Model, slot: &Slot, path: &Path) -> io::Result<()> {
		let on_disk = disk_kind(path)?;
		if let Some(&ino) = model.live.get(slot) {
			if Some(model.nodes[ino].kind()) == on_disk {
				return Ok(());
			}
			model.remove(slot.clone());
		}
		match on_disk {
			Some(Kind::Dir) => {
				model.add(slot.clone(), Node::Dir);
			}
			Some(Kind::File) if !self.is_exempt(path) => {
				let bytes = std::fs::read(path)?;
				model.add(slot.clone(), Node::File { durable: Vec::new(), current: bytes });
			}
			_ => {}
		}
		Ok(())
	}

	/// Remove every entry under directory `dir` (at `path`), deepest first, each as the
	/// separate unlink or `rmdir` it is on disk.
	fn remove_tree(&self, model: &mut Model, dir: Ino, path: &Path) -> io::Result<()> {
		let mut names: BTreeSet<OsString> = read_dir_blocking(path)?.into_iter().map(|entry| entry.name).collect();
		names.extend(entries_of(&model.live, dir).map(|(name, _)| name.clone()));
		for name in names {
			let child = path.join(&name);
			if self.is_exempt(&child) && disk_kind(&child)? == Some(Kind::File) {
				remove_file_blocking(&child)?;
				continue;
			}
			let slot = (dir, name);
			self.reconcile(model, &slot, &child)?;
			let Some(&ino) = model.live.get(&slot) else { continue };
			if model.nodes[ino].kind() == Kind::Dir {
				self.remove_tree(model, ino, &child)?;
				std::fs::remove_dir(&child)?;
			} else {
				remove_file_blocking(&child)?;
			}
			model.remove(slot);
		}
		Ok(())
	}

	/// The steps of `create_new_write` after the create, as `RealFs` runs them, with
	/// the model (for a modelled file, `ino`) brought up to date after each.
	async fn fill(&self, mut file: File, ino: Option<Ino>, bytes: Vec<u8>, policy: SyncPolicy, points: WritePoints) -> io::Result<()> {
		if let Some(point) = points.created {
			fault::hit(point).await?;
		}
		file.write_all(&bytes)?;
		drop(file);
		self.set_current(ino, bytes);
		if let Some(point) = points.written {
			fault::hit(point).await?;
		}
		if policy == SyncPolicy::Full {
			let mut model = self.lock();
			if let Some(Node::File { durable, current }) = ino.and_then(|ino| model.nodes.get_mut(ino)) {
				durable.clone_from(current);
			}
			model.file_syncs += 1;
		}
		Ok(())
	}

	/// Record that modelled file `ino` (if any) now holds `bytes`.
	fn set_current(&self, ino: Option<Ino>, bytes: Vec<u8>) {
		let mut model = self.lock();
		if let Some(Node::File { current, .. }) = ino.and_then(|ino| model.nodes.get_mut(ino)) {
			*current = bytes;
		}
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
		let (file, ino) = {
			let mut model = self.lock();
			let slot = self.slot(&mut model, path)?;
			if self.is_exempt(path) {
				(std::fs::OpenOptions::new().write(true).create_new(true).open(path)?, None)
			} else {
				self.reconcile(&mut model, &slot, path)?;
				let file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
				(file, Some(model.add(slot, Node::File { durable: Vec::new(), current: Vec::new() })))
			}
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
		let mut model = self.lock();
		let slot = self.slot(&mut model, path)?;
		if self.is_exempt(path) {
			metadata_blocking(path)?;
		} else {
			self.reconcile(&mut model, &slot, path)?;
			let ino = model.live_ino(&slot, path)?;
			let Node::File { durable, current } = &mut model.nodes[ino] else {
				return Err(io::Error::new(io::ErrorKind::IsADirectory, format!("{} is a directory; sync it with sync_dir", path.display())));
			};
			// The fsync makes durable whatever the file holds now, including bytes
			// written behind `SimFs`'s back.
			let bytes = std::fs::read(path)?;
			durable.clone_from(&bytes);
			*current = bytes;
		}
		model.file_syncs += 1;
		Ok(())
	}

	async fn sync_dir(&self, dir: &Path) -> io::Result<()> {
		let mut model = self.lock();
		let rel = self.relative(dir)?;
		let ino = self.dir_at(&mut model, rel)?;
		let in_dir: Vec<usize> = model.pending.iter().enumerate().filter(|(_, op)| op.touches_dir(ino)).map(|(i, _)| i).collect();
		let durable = with_dependencies(&model.pending, in_dir);
		model.commit(&durable);
		model.dir_syncs += 1;
		Ok(())
	}

	async fn hard_link(&self, src: &Path, dst: &Path) -> io::Result<()> {
		let mut model = self.lock();
		let (src_slot, dst_slot) = (self.slot(&mut model, src)?, self.slot(&mut model, dst)?);
		if self.is_exempt(src) || self.is_exempt(dst) {
			return std::fs::hard_link(src, dst);
		}
		self.reconcile(&mut model, &src_slot, src)?;
		self.reconcile(&mut model, &dst_slot, dst)?;
		let ino = model.live_ino(&src_slot, src)?;
		std::fs::hard_link(src, dst)?;
		model.live.insert(dst_slot.clone(), ino);
		model.pending.push(DirOp::Add { slot: dst_slot, ino });
		Ok(())
	}

	async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
		let mut model = self.lock();
		let (from_slot, to_slot) = (self.slot(&mut model, from)?, self.slot(&mut model, to)?);
		if self.is_exempt(from) || self.is_exempt(to) {
			return std::fs::rename(from, to);
		}
		self.reconcile(&mut model, &from_slot, from)?;
		self.reconcile(&mut model, &to_slot, to)?;
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
		let mut model = self.lock();
		let slot = match self.slot(&mut model, path) {
			Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
			other => other?,
		};
		if self.is_exempt(path) {
			return remove_file_blocking(path);
		}
		self.reconcile(&mut model, &slot, path)?;
		remove_file_blocking(path)?;
		model.remove(slot);
		Ok(())
	}

	async fn create_dir(&self, path: &Path) -> io::Result<()> {
		let mut model = self.lock();
		let slot = self.slot(&mut model, path)?;
		self.reconcile(&mut model, &slot, path)?;
		std::fs::create_dir(path)?;
		model.add(slot, Node::Dir);
		Ok(())
	}

	async fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
		let mut model = self.lock();
		let slot = match self.slot(&mut model, path) {
			Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
			other => other?,
		};
		self.reconcile(&mut model, &slot, path)?;
		let Some(&ino) = model.live.get(&slot) else { return Ok(()) };
		if model.nodes[ino].kind() != Kind::Dir {
			return Err(io::Error::new(io::ErrorKind::NotADirectory, format!("{} is not a directory", path.display())));
		}
		self.remove_tree(&mut model, ino, path)?;
		std::fs::remove_dir(path)?;
		model.remove(slot);
		Ok(())
	}

	async fn copy_new(&self, src: &Path, dst: &Path, policy: SyncPolicy) -> io::Result<u64> {
		// Reads pass through, so the source may lie anywhere; the copy is a new file
		// like any other `create_new_write`.
		let bytes = std::fs::read(src)?;
		let len = bytes.len() as u64;
		self.create_new_write(dst, bytes, policy, WritePoints::NONE).await?;
		Ok(len)
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

	/// A store root with `segments/` and `backup/` directories, and the `SimFs` over
	/// it, which trusts Turso's files as the store's crash tests will. Both
	/// directories predate the simulation, so they are durable in every image.
	fn store() -> (tempfile::TempDir, SimFs) {
		let root = tempfile::tempdir().unwrap();
		std::fs::create_dir(root.path().join("segments")).unwrap();
		std::fs::create_dir(root.path().join("backup")).unwrap();
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
		// A point no store path reaches: armed points are process-global, and
		// `M-output-written`, which every reconcile and split passes, would fail one running
		// in another test.
		let point = FaultPoint::LChunk(u32::MAX - 3);
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

		// A copy reads its source wherever it lies and is a new file like any other.
		let outside = tempfile::tempdir().unwrap();
		std::fs::write(outside.path().join("src"), b"copied bytes").unwrap();
		assert_eq!(sim.copy_new(&outside.path().join("src"), &segments.join("copy"), SyncPolicy::Full).await.unwrap(), 12);
		assert_eq!(sim.copy_new(&outside.path().join("src"), &segments.join("copy"), SyncPolicy::Full).await.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
		sim.sync_dir(&segments).await.unwrap();
		for seed in 0..16 {
			assert_eq!(std::fs::read(cut(&sim, seed).path().join("segments/copy")).unwrap(), b"copied bytes", "seed {seed}: a synced copy in a synced directory is durable");
		}
	}

	#[tokio::test]
	async fn misuse_is_refused() {
		let (root, sim) = store();
		let outside = tempfile::tempdir().unwrap();
		assert_eq!(sim.create_new_write(&outside.path().join("x"), vec![1], SyncPolicy::Full, WritePoints::NONE).await.unwrap_err().kind(), io::ErrorKind::InvalidInput, "paths outside the root are refused");
		assert_eq!(sim.create_new_write(&root.path().join("segments/../x"), vec![1], SyncPolicy::Full, WritePoints::NONE).await.unwrap_err().kind(), io::ErrorKind::InvalidInput);
		assert_eq!(sim.rename(root.path(), &root.path().join("moved")).await.unwrap_err().kind(), io::ErrorKind::InvalidInput, "the root itself is not an entry");
		assert_eq!(sim.remove_dir_all(root.path()).await.unwrap_err().kind(), io::ErrorKind::InvalidInput);
		assert_eq!(sim.remove_dir_all(&root.path().join("segments/missing")).await.map_err(|e| e.kind()), Ok(()), "a missing directory is already removed");
		std::fs::write(root.path().join("segments/file"), b"x").unwrap();
		assert_eq!(sim.remove_dir_all(&root.path().join("segments/file")).await.unwrap_err().kind(), io::ErrorKind::NotADirectory);
		assert_eq!(sim.create_dir(&root.path().join("segments")).await.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
		assert_eq!(sim.power_cut(0, &root.path().join("image")).unwrap_err().kind(), io::ErrorKind::InvalidInput, "the image cannot live inside the root");
		std::fs::write(outside.path().join("stale"), b"x").unwrap();
		assert_eq!(sim.power_cut(0, outside.path()).unwrap_err().kind(), io::ErrorKind::AlreadyExists, "the image directory must be fresh");
	}

	#[tokio::test]
	async fn directories_that_predate_the_simulation_are_durable() {
		let root = tempfile::tempdir().unwrap();
		std::fs::create_dir_all(root.path().join("empty/nested")).unwrap();
		std::fs::create_dir(root.path().join("segments")).unwrap();
		let sim = SimFs::new(root.path()).unwrap();
		for seed in 0..16 {
			let image = cut(&sim, seed);
			assert!(image.path().join("empty/nested").is_dir() && image.path().join("segments").is_dir(), "seed {seed}");
		}
	}

	#[tokio::test]
	async fn a_new_directory_is_durable_only_once_its_parent_is_synced() {
		let (root, sim) = store();
		let dir = root.path().join("d");
		let written = payload(300, 22);
		sim.create_dir(&dir).await.unwrap();
		write_new_durable(&sim, &dir, "f", written.clone(), SyncPolicy::Full, WritePoints::NONE).await.unwrap();
		sim.sync_dir(&dir).await.unwrap();

		// The file's entry is durable inside the directory, but the directory's own entry
		// is not, and a file is never reachable without its directory.
		let mut seen = HashSet::new();
		for seed in 0..SEEDS {
			let image = cut(&sim, seed);
			let has_dir = image.path().join("d").is_dir();
			let file = outcome(image.path(), "d/f", &written);
			assert_eq!(file, if has_dir { Outcome::Complete } else { Outcome::Absent }, "seed {seed}");
			seen.insert(has_dir);
		}
		assert_eq!(seen, HashSet::from([true, false]), "both sides of the unsynced mkdir occur");

		sim.sync_dir(root.path()).await.unwrap();
		assert_eq!(sim.pending_dir_ops(), 0);
		for seed in 0..16 {
			assert_eq!(outcome(cut(&sim, seed).path(), "d/f", &written), Outcome::Complete, "seed {seed}");
		}
	}

	#[tokio::test]
	async fn a_directory_rename_moves_its_whole_subtree_atomically() {
		let (root, sim) = store();
		let (partial, published) = (root.path().join(".partial-x"), root.path().join("x"));
		let written = payload(256, 23);
		sim.create_dir(&partial).await.unwrap();
		write_new_durable(&sim, &partial, "MANIFEST", written.clone(), SyncPolicy::Full, WritePoints::NONE).await.unwrap();
		sim.sync_dir(&partial).await.unwrap();
		sim.sync_dir(root.path()).await.unwrap();
		sim.rename(&partial, &published).await.unwrap();
		assert!(published.join("MANIFEST").exists() && !partial.exists(), "the live directory has the rename");

		let mut renamed = 0;
		for seed in 0..SEEDS {
			let image = cut(&sim, seed);
			let (old, new) = (outcome(image.path(), ".partial-x/MANIFEST", &written), outcome(image.path(), "x/MANIFEST", &written));
			assert!(matches!((old, new), (Outcome::Complete, Outcome::Absent) | (Outcome::Absent, Outcome::Complete)), "seed {seed}: the subtree is under exactly one name, intact ({old:?}, {new:?})");
			renamed += usize::from(new == Outcome::Complete);
		}
		assert!(renamed > 0 && renamed < SEEDS as usize, "both sides of the pending rename occur ({renamed} of {SEEDS})");

		sim.sync_dir(root.path()).await.unwrap();
		for seed in 0..16 {
			let image = cut(&sim, seed);
			assert_eq!(outcome(image.path(), "x/MANIFEST", &written), Outcome::Complete, "seed {seed}");
			assert!(!image.path().join(".partial-x").exists(), "seed {seed}");
		}
	}

	#[tokio::test]
	async fn an_entry_made_after_an_unsynced_rename_stays_with_its_directory() {
		let (root, sim) = store();
		let (a, b) = (root.path().join("a"), root.path().join("b"));
		sim.create_dir(&a).await.unwrap();
		sim.sync_dir(root.path()).await.unwrap();
		sim.rename(&a, &b).await.unwrap();
		let written = payload(128, 24);
		write_new_durable(&sim, &b, "f", written.clone(), SyncPolicy::Full, WritePoints::NONE).await.unwrap();
		sim.sync_dir(&b).await.unwrap();

		// The file is durable in the directory's inode, so it follows the directory to
		// whichever name it has after the cut: a path-keyed model would lose it.
		let mut seen = HashSet::new();
		for seed in 0..SEEDS {
			let image = cut(&sim, seed);
			let pair = (outcome(image.path(), "a/f", &written), outcome(image.path(), "b/f", &written));
			assert!(matches!(pair, (Outcome::Complete, Outcome::Absent) | (Outcome::Absent, Outcome::Complete)), "seed {seed}: {pair:?}");
			seen.insert(pair);
		}
		assert_eq!(seen.len(), 2, "the file appears under either name of its directory");
	}

	#[tokio::test]
	async fn an_unsynced_tree_removal_can_leave_a_partial_directory() {
		let (root, sim) = store();
		let dir = root.path().join("victim");
		sim.create_dir(&dir).await.unwrap();
		std::fs::create_dir(dir.join("sub")).unwrap();
		let files: Vec<_> = (0..4u8).map(|i| (format!("f{i}"), payload(64, 30 + i))).collect();
		for (name, bytes) in &files {
			write_new_durable(&sim, &dir, name, bytes.clone(), SyncPolicy::Full, WritePoints::NONE).await.unwrap();
		}
		sim.sync_dir(&dir).await.unwrap();
		sim.sync_dir(root.path()).await.unwrap();
		sim.remove_dir_all(&dir).await.unwrap();
		assert!(!dir.exists(), "the live tree is gone, including what was made behind SimFs's back");
		sim.remove_dir_all(&dir).await.expect("removing a missing directory is a success");

		// The removal is one unlink per entry plus the rmdir, none of them synced, so a
		// cut can keep the directory with only some of its files: the window a pruned
		// backup's `.deleting-*` rename closes.
		let mut partial = 0;
		for seed in 0..SEEDS {
			let image = cut(&sim, seed);
			let has_dir = image.path().join("victim").is_dir();
			let kept = files.iter().filter(|(name, bytes)| outcome(image.path(), &format!("victim/{name}"), bytes) == Outcome::Complete).count();
			assert!(has_dir || kept == 0, "seed {seed}: no file outlives its directory");
			partial += usize::from(has_dir && kept < files.len());
		}
		assert!(partial > 0, "some cuts leave a half-removed directory");

		sim.sync_dir(root.path()).await.unwrap();
		for seed in 0..16 {
			assert!(!cut(&sim, seed).path().join("victim").exists(), "seed {seed}: once the rmdir is synced, the whole tree is gone");
		}
	}

	#[tokio::test]
	async fn a_directory_made_behind_simfs_back_can_vanish_with_its_subtree() {
		let (root, sim) = store();
		std::fs::create_dir_all(root.path().join("x/y")).unwrap();
		let written = payload(200, 25);
		std::fs::write(root.path().join("x/y/f"), &written).unwrap();
		std::fs::write(root.path().join("x/y/state.db"), b"turso pages").unwrap();

		let mut seen = HashSet::new();
		for seed in 0..SEEDS {
			let image = cut(&sim, seed);
			let dirs = (image.path().join("x").is_dir(), image.path().join("x/y").is_dir());
			assert!(dirs.0 || !dirs.1, "seed {seed}: a subtree never outlives its directory");
			let db = std::fs::read(image.path().join("x/y/state.db")).ok();
			assert_eq!(db.as_deref(), dirs.1.then_some(&b"turso pages"[..]), "seed {seed}: an exempt file survives, as it is, exactly when its directory does");
			let file = outcome(image.path(), "x/y/f", &written);
			assert!(dirs.1 || file == Outcome::Absent, "seed {seed}");
			seen.insert(dirs);
		}
		assert_eq!(seen, HashSet::from([(false, false), (true, false), (true, true)]));

		// A file synced through SimFs inside such a directory is durable only once every
		// directory on its path is: the directories' own entries are still pending.
		sim.sync_file(&root.path().join("x/y/f")).await.unwrap();
		sim.sync_dir(&root.path().join("x/y")).await.unwrap();
		assert_eq!(sim.pending_dir_ops(), 2, "x in the root and y in x are still pending");
		sim.sync_dir(&root.path().join("x")).await.unwrap();
		sim.sync_dir(root.path()).await.unwrap();
		for seed in 0..16 {
			assert_eq!(outcome(cut(&sim, seed).path(), "x/y/f", &written), Outcome::Complete, "seed {seed}");
		}
	}
}
