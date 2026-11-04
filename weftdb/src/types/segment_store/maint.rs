//! Segment-store implementation: this submodule holds the `maint` portion of [`SegmentStore`](super::SegmentStore).
//! Pure move of the former `segment_store.rs` `impl` block; no behaviour change.

use super::*;

impl SegmentStore {
	/// Hold `aspect`'s maintenance lock until the returned guard drops, as a maintenance
	/// operation in progress would, without running one. The hook for tests of the layers
	/// above (`weft-server`'s `409 Conflict` and its daemon's skip), which cannot reach the
	/// lock; only `fault-injection` builds have it.
	///
	/// # Errors
	///
	/// [`MaintenanceBusy`] if another operation holds the aspect for longer than
	/// [`maintenance_wait`](Self::maintenance_wait).
	#[cfg(feature = "fault-injection")]
	#[doc(hidden)]
	pub async fn hold_maintenance(&self, aspect: &str) -> Result<MaintenanceHold> {
		Ok(MaintenanceHold { _held: self.maintain(aspect).await? })
	}

	/// Take `aspect`'s maintenance lock for a per-aspect maintenance entry point, waiting
	/// up to [`maintenance_wait`](Self::maintenance_wait).
	pub(crate) async fn maintain(&self, aspect: &str) -> Result<MaintGuard> {
		match self.locks.maint_within(aspect, self.maintenance_wait).await {
			Some(held) => Ok(held),
			None => Err(MaintenanceBusy { aspect: aspect.to_string(), waited: self.maintenance_wait }.into()),
		}
	}

	/// Take `aspect`'s maintenance lock for a store-wide sweep: at once or not at all with
	/// no `deadline` ([`MaintenanceWait::Skip`]), else waiting until `deadline`.
	pub(crate) async fn sweep_maintain(&self, aspect: &str, deadline: Option<tokio::time::Instant>) -> Option<MaintGuard> {
		match deadline {
			None => self.locks.try_maint(aspect),
			Some(deadline) => self.locks.maint_within(aspect, deadline.saturating_duration_since(tokio::time::Instant::now())).await,
		}
	}

	/// Fsync `segments/` through the store's shared directory syncer. A failure poisons
	/// the store (design section 5.1): after a failed directory fsync the kernel may have
	/// dropped the entries, so no later success would prove them durable.
	pub(crate) async fn sync_segments(&self) -> Result<()> {
		if let Err(e) = self.reaper.sync().await {
			let reason = format!("the fsync of {} failed: {e}", self.root.join("segments").display());
			if self.poison.set(reason) {
				tracing::error!(root = %self.root.display(), error = %e, "a segments/ fsync failed; the store refuses writes until the process restarts, and reads go on");
			}
			return Err(anyhow::Error::new(e).context("syncing segments/"));
		}
		Ok(())
	}

	/// Hold the reaper off every retired frame until the returned hold drops, as a backup
	/// that links frames does (design section 9, B1-B4; used by the whole-store
	/// backup in `build_backup`).
	pub(crate) fn hold_reaper(&self) -> crate::types::reaper::GcHold<'_> {
		self.reaper.epochs().hold()
	}

	/// Recompute `aspect`'s rollup from the index and write it into `segment_index.db`,
	/// under its commit lock, which `_commit` shows the caller holds. The write is a
	/// single-op [`IndexTxn`], applied directly (not through [`commit_index`](Self::commit_index)):
	/// the journal replay at open calls this before the store is writable, and a
	/// rebuild must not be refused by a poison another writer set.
	pub(crate) async fn rebuild_rollup_locked(&self, aspect: &str, _commit: &CommitGuard) -> Result<AspectMetadata> {
		let index = self.index.load_index(aspect).await?;
		let meta = AspectMetadata::from_index(&index);
		self.index.apply(&IndexTxn::new(vec![IndexOp::PutRollup { aspect: aspect.to_string(), meta: meta.clone() }])).await?;
		Ok(meta)
	}

	/// The reaper's pass over `held`'s aspect (design section 5.4): G1-G3 through the
	/// reaper (unlink every retired frame no read or backup may still need, and every
	/// abandoned pending output, never one a live row references), G4 one `segments/`
	/// fsync, G5 one commit deleting the processed journal rows. Pending rows are safe to
	/// process here: only a maintenance operation of the aspect writes them, and the
	/// caller holds the aspect. The outputs a swap abandoned ([`PendingOutputs`]) are among
	/// them, and are settled once their rows are gone.
	///
	/// # Errors
	///
	/// [`Poisoned`] when the store is; a failed journal read or commit, or `segments/`
	/// fsync (which poisons the store).
	pub(crate) async fn reap_held(&self, held: &MaintGuard) -> Result<crate::types::reaper::Swept> {
		self.writable()?;
		let aspect = held.aspect();
		let entries = self.index.journal(Some(aspect)).await?;
		if entries.is_empty() {
			// Nothing is journaled, so nothing a swap abandoned is left to settle.
			self.reaper.settle_abandoned(aspect, |_| false);
			return Ok(crate::types::reaper::Swept::default());
		}
		let live = HashMap::from([(aspect.to_string(), LiveFrames::of(&self.index.rows(aspect).await?))]);
		let swept = self.reaper.sweep(&entries, &live, true).await;
		fault::hit(FaultPoint::GUnlinked).await?;
		if swept.unlinked > 0 {
			self.sync_segments().await?;
		}
		fault::hit(FaultPoint::GDirSynced).await?;
		if !swept.processed.is_empty() {
			self.commit_index(&IndexTxn::new(swept.processed.iter().map(|name| IndexOp::ClearJournal { name: name.clone() }).collect())).await?;
		}
		fault::hit(FaultPoint::GJournalDeleted).await?;
		self.reaper.epochs().forget(&swept.processed);
		// An abandoned output is settled once its journal row is gone: processed here, or
		// deleted before (its swap committed after all).
		let left: std::collections::HashSet<&str> = entries.iter().map(|entry| entry.name.as_str()).filter(|name| !swept.processed.iter().any(|done| done == name)).collect();
		self.reaper.settle_abandoned(aspect, |name| left.contains(name));
		Ok(swept)
	}

	/// Run the reaper over every aspect whose frame journal holds rows (design section
	/// 5.4): unlink each retired frame that no running read and no backup may still need,
	/// then delete its journal row.
	///
	/// Every maintenance operation hands the frames it retires to the reaper itself, so
	/// this is for what a read still held then, what the filesystem asked to retry later (a
	/// Windows sharing violation), or the outputs of an operation that stopped part way
	/// (its future dropped) and that no later operation on the aspect has settled yet; a
	/// store that is reopened replays its whole journal anyway. Each aspect is reaped under
	/// its maintenance lock; one another operation holds is skipped or waited for as `wait`
	/// says, and listed in [`ReapSweep::busy`].
	///
	/// # Errors
	///
	/// [`Poisoned`] when the store is, or a failed journal read; per-aspect failures are
	/// reported in [`ReapSweep::failed`].
	pub async fn reap(&self, wait: MaintenanceWait) -> Result<ReapSweep> {
		self.writable()?;
		let aspects: std::collections::BTreeSet<String> = self.index.journal(None).await?.into_iter().map(|entry| entry.aspect).collect();
		let deadline = wait.deadline();
		let mut sweep = ReapSweep::default();
		for aspect in aspects {
			let Some(held) = self.sweep_maintain(&aspect, deadline).await else {
				sweep.busy.push(aspect);
				continue;
			};
			match self.reap_held(&held).await {
				Ok(swept) => {
					sweep.aspects_reaped += 1;
					sweep.frames_unlinked += swept.unlinked;
					sweep.frames_waiting += swept.waiting + swept.deferred.len() + swept.failed.len();
					if !swept.failed.is_empty() {
						let frames: Vec<String> = swept.failed.iter().map(|(name, e)| format!("{name}: {e}")).collect();
						sweep.failed.push((aspect, anyhow::anyhow!("could not unlink {} retired frame(s): {}", frames.len(), frames.join("; "))));
					}
				}
				Err(e) => sweep.failed.push((aspect, e)),
			}
		}
		Ok(sweep)
	}

	/// Take an online, consistent snapshot of this store's **four control-plane
	/// databases** (`segment_index.db`, `metadata.db`, `aspect_catalog.db`, `catalog.db`)
	/// into `dest_dir`, via Turso's `VACUUM INTO` (roadmap **Phase 7.4**). Each copy is
	/// reopened and verified to match its source table-for-table before the call returns.
	///
	/// The `.weftseg` measurement frames under `segments/` are **not** part of this backup
	/// — this is the control-plane (catalog/index/metadata) snapshot only, per the storage
	/// boundary (hard-constraint #3). `dest_dir` must not exist yet: the backup is built
	/// beside it and appears under that name, with a `MANIFEST.json`, only once it is
	/// complete and durable (see [`backup_control_plane_via`](SegmentStore::backup_control_plane_via)).
	/// Its parent is created if absent.
	///
	/// See [`snapshot_and_verify`](crate::snapshot_and_verify) for the consistency scope
	/// of the per-file verification (the row-count match assumes a quiescent source).
	///
	/// # Errors
	///
	/// `dest_dir` already exists, a filesystem error building or publishing the backup,
	/// or any per-database backup/verify failure (libSQL error, a missing table, or a
	/// source/copy mismatch). An error before the backup is published removes what it
	/// built. One after it (only the parent's fsync is left by then) leaves the complete
	/// backup under `dest_dir`; see
	/// [`backup_control_plane_via`](SegmentStore::backup_control_plane_via).
	pub async fn backup_control_plane(&self, dest_dir: impl AsRef<Path>) -> Result<ControlPlaneBackup> {
		self.backup_control_plane_with_verify(dest_dir, crate::VerifyMode::default()).await
	}

	/// Take the same four-database control-plane snapshot as
	/// [`backup_control_plane`](SegmentStore::backup_control_plane), under an explicit
	/// [`VerifyMode`](crate::VerifyMode).
	///
	/// [`VerifyMode::SourceMatch`](crate::VerifyMode::SourceMatch) (the default) verifies
	/// each copy against a fresh source read and assumes a **quiescent** store — the
	/// maintenance-window shape. [`VerifyMode::SnapshotOnly`](crate::VerifyMode::SnapshotOnly)
	/// verifies each copy on its own terms and never re-reads the source, so it is the
	/// mode an **online** backup taken against a live, ingesting store must use: a
	/// concurrent seal committing between a vacuum and its verification would otherwise
	/// be reported as a spurious mismatch.
	///
	/// # Errors
	///
	/// As [`backup_control_plane`](SegmentStore::backup_control_plane).
	pub async fn backup_control_plane_with_verify(&self, dest_dir: impl AsRef<Path>, mode: crate::VerifyMode) -> Result<ControlPlaneBackup> {
		self.backup_control_plane_via(&RealFs, dest_dir, mode).await
	}

	/// [`backup_control_plane_with_verify`](SegmentStore::backup_control_plane_with_verify),
	/// with every directory operation and fsync going through `fs` (the crash tests pass
	/// a `SimFs`).
	///
	/// The backup becomes visible only once it is complete and durable (design section
	/// 9): it is built in `<parent>/.partial-{label}-{nonce}/`, where each database is
	/// vacuumed, verified (including its expected table set) and fsynced; then a
	/// `MANIFEST.json` recording each file's size, tables and rows is written and synced,
	/// the build directory is fsynced, renamed to `dest_dir`, and the parent fsynced.
	///
	/// What a failure leaves depends on when it strikes:
	///
	/// - An error before the rename removes the build directory again, so nothing is
	///   left. The removal is best effort: if it fails, it is logged, the backup's own
	///   error is returned, and the directory is left as a crash would leave it.
	/// - A crash before the rename leaves the `.partial-*` directory. It is never counted
	///   or restored, and [`sweep_backup_staging`](crate::sweep_backup_staging) removes it
	///   once stale. weft-server's backup daemon runs that sweep; where the daemon is not
	///   enabled, such a directory stays until an operator removes it.
	/// - After the rename only the parent's fsync is left. An error there (or an injected
	///   fault at `B-renamed`) is returned although the complete backup is already under
	///   `dest_dir`: it counts toward retention, it restores, and its label is taken. It
	///   is only not yet known to survive power loss, which the next backup's fsync of
	///   the same parent makes it. A crash there leaves the same state.
	///
	/// # Errors
	///
	/// As [`backup_control_plane`](SegmentStore::backup_control_plane), or an injected
	/// fault at a `B-*` point.
	pub async fn backup_control_plane_via(&self, fs: &dyn StoreFs, dest_dir: impl AsRef<Path>, mode: crate::VerifyMode) -> Result<ControlPlaneBackup> {
		let dir = dest_dir.as_ref().to_path_buf();
		let (base, label) = crate::types::backup::split_dir(&dir).with_context(|| format!("choosing where to build backup {}", dir.display()))?;
		if fs.metadata(&dir).await.is_ok() {
			bail!("backup destination {} already exists (a backup is published under a fresh name)", dir.display());
		}
		create_dir_all_durable(fs, &base).await.with_context(|| format!("creating backup base {}", base.display()))?;
		let partial = base.join(crate::types::backup::staging_name(crate::PARTIAL_PREFIX, &label));
		fs.create_dir(&partial).await.with_context(|| format!("creating backup build directory {}", partial.display()))?;

		let ([mut segment_index, mut metadata, mut aspect_catalog, mut registry], _) = match self.build_backup(fs, &partial, &dir, mode, None).await {
			Ok(reports) => reports,
			Err(err) => {
				// An error, unlike a crash, can clean up after itself. The build directory
				// may hold three full database copies, and the sweep that removes a crash's
				// leftovers runs only in weft-server's backup daemon, so a deployment that
				// takes only manual backups would otherwise collect them unseen.
				if let Err(cleanup) = fs.remove_dir_all(&partial).await {
					tracing::warn!(dir = %partial.display(), error = %cleanup, cause = %format!("{err:#}"), "could not remove a failed backup's build directory; the backup daemon's sweep removes it once stale, if the daemon is enabled");
				}
				return Err(err);
			}
		};
		fault::hit(FaultPoint::BRenamed).await?;
		fs.sync_dir(&base).await.with_context(|| format!("syncing backup base {}", base.display()))?;

		for (report, name) in [(&mut segment_index, "segment_index.db"), (&mut metadata, "metadata.db"), (&mut aspect_catalog, "aspect_catalog.db"), (&mut registry, "catalog.db")] {
			report.dest = dir.join(name);
		}
		Ok(ControlPlaneBackup { dir, segment_index, metadata, aspect_catalog, registry })
	}

	/// Take a **whole-store** backup (D-S16; design section 9, B1-B6): the
	/// control plane of [`backup_control_plane`](SegmentStore::backup_control_plane)
	/// plus every `.weftseg` frame its `segment_index.db` snapshot references,
	/// hard-linked (or copied, per `mode` and what the filesystem allows) into
	/// `dest_dir/segments/`, under a whole-store `MANIFEST.json` that also
	/// embeds this store's `STORE_FORMAT` and its per-aspect allocators.
	///
	/// No ingest barrier: the reaper is held off by an RAII `gc_hold` from
	/// before the first snapshot until the frames are linked, frames are
	/// immutable, and swaps are single transactions, so the snapshot's frame
	/// set is exactly the set linked. `segment_index.db` is vacuumed first —
	/// it is the authority the frame set is read from.
	///
	/// The headroom a backup needs depends on `mode`
	/// ([`check_backup_headroom`](crate::check_backup_headroom)): linked frames
	/// cost no space, copied frames cost their bytes; where the destination's
	/// free space can be read, the check runs before anything is built.
	///
	/// # Errors
	///
	/// As [`backup_control_plane`](SegmentStore::backup_control_plane), or the
	/// headroom check, or a frame the snapshot references that cannot be read,
	/// linked or copied.
	pub async fn backup_store(&self, dest_dir: impl AsRef<Path>, mode: crate::BackupMode) -> Result<WholeStoreBackup> {
		self.backup_store_via(&RealFs, dest_dir, crate::VerifyMode::SnapshotOnly, mode).await
	}

	/// [`backup_store`](SegmentStore::backup_store) through `fs`.
	///
	/// # Errors
	///
	/// As [`backup_store`](SegmentStore::backup_store).
	pub async fn backup_store_via(&self, fs: &dyn StoreFs, dest_dir: impl AsRef<Path>, verify: crate::VerifyMode, mode: crate::BackupMode) -> Result<WholeStoreBackup> {
		let dir = dest_dir.as_ref().to_path_buf();
		let (base, label) = crate::types::backup::split_dir(&dir).with_context(|| format!("choosing where to build backup {}", dir.display()))?;
		if fs.metadata(&dir).await.is_ok() {
			bail!("backup destination {} already exists (a backup is published under a fresh name)", dir.display());
		}
		// The headroom check, before anything is built: the control plane is
		// estimated from the live files, the frames from the live index (the
		// snapshot taken under the gc-hold is authoritative for what is
		// actually linked; the estimate only sizes the check).
		let mut control_plane_bytes = 0u64;
		for name in crate::CONTROL_PLANE_FILES {
			if let Ok(meta) = fs.metadata(&self.root.join(name)).await {
				control_plane_bytes = control_plane_bytes.saturating_add(meta.len);
			}
		}
		let mut frame_bytes = 0u64;
		for aspect in self.index.list_aspects().await? {
			for row in self.index.rows(&aspect).await? {
				frame_bytes = frame_bytes.saturating_add(row.desc.byte_len);
			}
		}
		if let Some(free) = crate::free_bytes_at(&base) {
			crate::check_backup_headroom(mode, control_plane_bytes, frame_bytes, free)?;
		}
		create_dir_all_durable(fs, &base).await.with_context(|| format!("creating backup base {}", base.display()))?;
		let partial = base.join(crate::types::backup::staging_name(crate::PARTIAL_PREFIX, &label));
		fs.create_dir(&partial).await.with_context(|| format!("creating backup build directory {}", partial.display()))?;
		let (mut reports, whole) = match self.build_backup(fs, &partial, &dir, verify, Some(mode)).await {
			Ok(pair) => pair,
			Err(err) => {
				if let Err(cleanup) = fs.remove_dir_all(&partial).await {
					tracing::warn!(dir = %partial.display(), error = %cleanup, cause = %format!("{err:#}"), "could not remove a failed whole-store backup's build directory; the backup daemon's sweep removes it once stale, if the daemon is enabled");
				}
				return Err(err);
			}
		};
		fault::hit(FaultPoint::BRenamed).await?;
		fs.sync_dir(&base).await.with_context(|| format!("syncing backup base {}", base.display()))?;
		for (report, name) in reports.iter_mut().zip(crate::CONTROL_PLANE_FILES) {
			report.dest = dir.join(name);
		}
		let [segment_index, metadata, aspect_catalog, registry] = reports;
		let (frames, linked_bytes, copied_bytes) = whole.expect("a whole-store build reports its frames");
		Ok(WholeStoreBackup { dir, segment_index, metadata, aspect_catalog, registry, frames, linked_bytes, copied_bytes })
	}

	/// The steps of [`backup_control_plane_via`](SegmentStore::backup_control_plane_via)
	/// from the freshly created build directory `partial` up to its rename to `dir`:
	/// everything an error can still undo by removing `partial`. Returns the four
	/// snapshots' reports in [`CONTROL_PLANE_FILES`](crate::CONTROL_PLANE_FILES) order,
	/// still naming the files under `partial`.
	pub(crate) async fn build_backup(&self, fs: &dyn StoreFs, partial: &Path, dir: &Path, mode: crate::VerifyMode, whole: Option<crate::BackupMode>) -> Result<([SnapshotReport; 4], Option<(Vec<crate::ManifestFrame>, u64, u64)>)> {
		fault::hit(FaultPoint::BPartialCreated).await?;
		// B1 (whole-store only): hold the reaper off for the whole snapshot +
		// link window. The hold is RAII: any return path releases it.
		let gc_hold = whole.map(|_| self.hold_reaper());

		// B2: segment_index.db first — it is the authority the frame set is
		// read from — then the other three, declarations only grow.
		let segment_index = snapshot_into(fs, 0, self.index.backup_to_with(&partial.join("segment_index.db"), mode)).await.context("backing up segment_index.db")?;
		let metadata = snapshot_into(fs, 1, self.metadata.backup_to_with(&partial.join("metadata.db"), mode)).await.context("backing up metadata.db")?;
		let aspect_catalog = snapshot_into(fs, 2, self.catalog.backup_to_with(&partial.join("aspect_catalog.db"), mode)).await.context("backing up aspect_catalog.db")?;
		let registry = snapshot_into(fs, 3, self.registry.backup_to_with(&partial.join("catalog.db"), mode)).await.context("backing up catalog.db")?;

		// B3 (whole-store only): open the snapshot copy and link or copy every
		// frame it references into the build directory's segments/.
		let mut whole_frames = None;
		if let Some(backup_mode) = whole {
			let snapshot_path = partial.join("segment_index.db");
			let snapshot = crate::SegmentIndexStore::open_unmigrated(snapshot_path.to_str().with_context(|| format!("snapshot path {} is not UTF-8", snapshot_path.display()))?).await.context("opening the backup's segment_index snapshot")?;
			let mut frames = Vec::new();
			let mut allocators = Vec::new();
			let mut linked_bytes = 0u64;
			let mut copied_bytes = 0u64;
			let segments_partial = partial.join("segments");
			fs.create_dir(&segments_partial).await.with_context(|| format!("creating {}", segments_partial.display()))?;
			for aspect in snapshot.list_aspects().await? {
				let seed = snapshot.allocator_seed(&aspect).await?;
				allocators.push(crate::ManifestAllocator { aspect: aspect.clone(), next_id: seed.next_id, next_gen: seed.next_gen, epoch: seed.epoch });
				for row in snapshot.rows(&aspect).await? {
					let Some(file_name) = Path::new(&row.desc.path).file_name().and_then(|name| name.to_str()).map(str::to_string) else { bail!("snapshot row for aspect {aspect:?} names a frame outside segments/: {:?}", row.desc.path) };
					let src = contained_file(&self.root.join("segments"), &file_name)?;
					let dest = segments_partial.join(&file_name);
					if frames.iter().any(|frame: &crate::ManifestFrame| frame.name == file_name) {
						continue;
					}
					let bytes = tokio::fs::read(&src).await.with_context(|| format!("reading frame {} for the backup", src.display()))?;
					// The CRC the manifest binds: the row's, or the frame's own
					// trailer for a legacy row (B5).
					let trailer = bytes.len().checked_sub(4).map(|body| u32::from_le_bytes([bytes[body], bytes[body + 1], bytes[body + 2], bytes[body + 3]]));
					let frame_crc = row.frame_crc.or(trailer);
					let linked = match backup_mode {
						crate::BackupMode::Copy => {
							fs.copy_new(&src, &dest, SyncPolicy::Full).await.with_context(|| format!("copying frame {} into the backup", src.display()))?;
							copied_bytes += bytes.len() as u64;
							false
						}
						crate::BackupMode::Linked => match fs.hard_link(&src, &dest).await {
							Ok(()) => {
								linked_bytes += bytes.len() as u64;
								true
							}
							// EXDEV (another filesystem) or any other link
							// refusal: copy and sync instead (B3).
							Err(_) => {
								fs.copy_new(&src, &dest, SyncPolicy::Full).await.with_context(|| format!("copying frame {} into the backup after linking failed", src.display()))?;
								copied_bytes += bytes.len() as u64;
								false
							}
						},
					};
					frames.push(crate::ManifestFrame { name: file_name, bytes: bytes.len() as u64, frame_crc, linked });
				}
			}
			drop(snapshot);
			fs.sync_dir(&segments_partial).await.with_context(|| format!("syncing {}", segments_partial.display()))?;
			whole_frames = Some((frames, allocators, linked_bytes, copied_bytes));
		}
		fault::hit(FaultPoint::BLinks).await?;
		// B4: the gc-hold is released before the manifest is written.
		drop(gc_hold);

		// B5: the MANIFEST — DB files, frames, allocators, store_uuid (inside
		// the embedded STORE_FORMAT for a whole-store backup). Then fsync.
		let created_ms = u64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis()).unwrap_or(u64::MAX);
		let reports: [(&str, &SnapshotReport); 4] = [("segment_index.db", &segment_index), ("metadata.db", &metadata), ("aspect_catalog.db", &aspect_catalog), ("catalog.db", &registry)];
		let (manifest, whole_out) = match whole_frames {
			Some((frames, allocators, linked_bytes, copied_bytes)) => (crate::BackupManifest::whole_store(created_ms, &reports, frames.clone(), self.store_format(), allocators), Some((frames, linked_bytes, copied_bytes))),
			None => (crate::BackupManifest::new(created_ms, &reports), None),
		};
		let manifest_bytes = serde_json::to_vec_pretty(&manifest).context("encoding the backup manifest")?;
		fs.create_new_write(&partial.join(crate::BACKUP_MANIFEST), manifest_bytes, SyncPolicy::Full, WritePoints::NONE).await.context("writing the backup manifest")?;
		fault::hit(FaultPoint::BManifest).await?;
		fs.sync_dir(partial).await.with_context(|| format!("syncing backup build directory {}", partial.display()))?;

		// B6 / publish. The rename is the instant the backup appears under its
		// label, whole.
		if fs.metadata(dir).await.is_ok() {
			bail!("backup destination {} appeared while the backup was being built", dir.display());
		}
		fs.rename(partial, dir).await.with_context(|| format!("publishing backup {} -> {}", partial.display(), dir.display()))?;
		Ok(([segment_index, metadata, aspect_catalog, registry], whole_out))
	}

	/// **Out-of-order reconciliation** (roadmap Phase 4.6): replace a single
	/// out-of-order segment with a time-sorted one, under its own id.
	///
	/// The first bounded slice of the QuestDB-O3-style reconciliation path — a
	/// per-segment sort rather than the full staging-window cross-segment merge. It
	/// reads segment `id`, stable-sorts its rows by timestamp (equal timestamps keep
	/// their original order, so [`read_point`](SegmentStore::read_point)'s
	/// first-present-of-a-run answer is preserved), and re-seals the sorted rows under
	/// the aspect's declared schema in the same frame kind (a paged segment re-seals
	/// paged at its own page height). The reconciled segment is `time_sorted`, so it
	/// drops out of the `unsorted_segments` order-health count and a point lookup over
	/// it binary-searches. The materialized rollup is rebuilt from the index afterward.
	///
	/// **Write-once** (crash-consistency design section 5.3; slice S8). The sorted rows
	/// go to a new frame under a never-used name (`{aspect}~g{gen}~p{prec}.weftseg`, the
	/// segment's next generation), created, written and fsynced before anything points at
	/// it, with its directory entry fsynced too, and journaled as pending first. One
	/// `segment_index.db` transaction then replaces the segment's row, if it is still the
	/// version that was read (its id, generation and frame CRC), with the new frame's, and
	/// journals the old frame as retired; that commit is the switch. The old frame is never
	/// rewritten: it is unlinked by the reaper once no read that may still open it is
	/// running. A crash or power cut at any point leaves either the old segment or the new
	/// one, whole, and the next open's journal replay removes the other. A segment that
	/// another operation changed meanwhile fails the swap with a conflict error, which
	/// writes nothing back.
	///
	/// Returns `true` when the segment was replaced, `false` when it was already sorted
	/// (a no-op).
	///
	/// # Errors
	///
	/// Returns an error if `aspect` has no declared schema or no segment `id`, or if the
	/// segment's frame is not the one its row was committed with; propagates a
	/// filesystem/read error, a re-seal failure, or a libSQL failure (a `Conflict` when
	/// the segment changed under the reconcile).
	/// A [`MaintenanceBusy`] error if another maintenance operation holds the aspect for
	/// longer than [`maintenance_wait`](Self::maintenance_wait).
	pub async fn reconcile_segment(&self, aspect: &str, id: u64) -> Result<bool> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		let held = self.maintain(aspect).await?;
		self.reconcile_segment_held(&held, id).await
	}

	/// [`reconcile_segment`](Self::reconcile_segment) under the maintenance lock `held`.
	pub(crate) async fn reconcile_segment_held(&self, held: &MaintGuard, id: u64) -> Result<bool> {
		self.writable()?;
		let aspect = held.aspect();
		aspect_name::validate(aspect)?;
		// M1: the input, as the swap's precondition will name it.
		let input = self.index.rows(aspect).await?.into_iter().find(|row| row.desc.id == id).ok_or_else(|| anyhow::anyhow!("aspect {aspect:?} has no segment {id}"))?;
		if input.desc.time_sorted {
			return Ok(false);
		}
		let schema = self.require_schema(aspect).await?;
		self.retired_name(&input)?;
		let frame = self.read_bound_frame(&input).await?;
		// Stable sort by timestamp — equal timestamps keep their ingest order.
		let mut rows: Vec<(i64, Option<BigDecimal>)> = frame.timestamps.into_iter().zip(frame.values).collect();
		rows.sort_by_key(|(t, _)| *t);
		let (timestamps, values): (Vec<i64>, Vec<Option<BigDecimal>>) = rows.into_iter().unzip();
		// M2-M6: one output in the segment's frame kind, under its id (P1).
		self.swap_plan(held, &schema, SwapPlan::cut(vec![input], timestamps, values, frame.rows_per_page, &[])).await?;
		Ok(true)
	}

	/// **Split a sorted segment at a timestamp boundary** (roadmap Phase 4.6 — the
	/// physical mechanism of the split-not-rewrite reconciliation path).
	///
	/// Partitions segment `id` of `aspect` at `boundary` into a **prefix** (rows with
	/// timestamp strictly `< boundary`, kept at the original `id`) and a **suffix**
	/// (rows at or after `boundary`, under a freshly allocated segment id), following
	/// the [`split_index`](weft_physical_type::split_index) partition point. Because the
	/// input is time-sorted, the prefix's every timestamp is `< boundary ≤` the suffix's
	/// every timestamp, so the two results are internally sorted **and disjoint in time**
	/// — the split adds no cross-segment overlap, and a point/range read still opens
	/// exactly one of them for any instant. Both keep the input's frame kind (a paged
	/// segment splits into two paged segments at its own page height).
	///
	/// This is the primitive `QuestDB`'s partition split is built on: once a large cold
	/// prefix is carved into its own segment, later late-data merges touch only the hot
	/// suffix and never rewrite the cold prefix again, bounding write amplification over
	/// the segment's lifetime. Wiring [`SplitPolicy::decide`](weft_physical_type::SplitPolicy)
	/// into [`reconcile_overlaps`](SegmentStore::reconcile_overlaps) to *choose* a split
	/// over a full rewrite is the next slice; this slice ships the mechanism it calls.
	///
	/// **Write-once** (crash-consistency design section 5.3; slice S8), as
	/// [`reconcile_segment`](Self::reconcile_segment): both halves are written to new,
	/// fsynced frames under never-used names, and one transaction replaces the segment's
	/// row with the prefix's, inserts the suffix's and retires the old frame. No reader
	/// ever sees one half without the other, or the suffix's rows twice, and a crash
	/// leaves the segment whole either before or after the split. The suffix's id is
	/// allocated inside that swap, under the aspect's commit lock.
	///
	/// **Precedence** (rule P2 of the release plan's D17). The suffix's id is above every
	/// row of the aspect, so where it shares a timestamp with a newer segment (a higher id)
	/// it would outrank that segment's value. The split is therefore **refused**, with an
	/// error and nothing changed, when any row with a higher id than `id` overlaps the
	/// suffix's span (its first timestamp at or after `boundary` to the segment's last),
	/// checked before anything is written and again against the rows the swap commits
	/// over.
	///
	/// Returns `Some(suffix_id)` — the new segment's id — when a split happened, or
	/// `None` when the split was degenerate (`boundary` falls before the first or after
	/// the last row, so every row lands on one side and there is nothing to carve). A
	/// degenerate split writes nothing.
	///
	/// # Errors
	///
	/// Returns an error if `aspect` has no declared schema or no segment `id`, if `id`
	/// is **not time-sorted** (split requires a sorted segment — reconcile it first, so
	/// the [`split_index`] precondition holds), if a newer segment overlaps the suffix (see
	/// above), or if the segment's frame is not the one its row was committed with, or
	/// propagates a filesystem/decode/re-seal/libSQL failure (a `Conflict` when the
	/// segment changed under the split).
	/// A [`MaintenanceBusy`] error if another maintenance operation holds the aspect for
	/// longer than [`maintenance_wait`](Self::maintenance_wait).
	pub async fn split_segment(&self, aspect: &str, id: u64, boundary: i64) -> Result<Option<u64>> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		let held = self.maintain(aspect).await?;
		self.split_segment_held(&held, id, boundary).await
	}

	/// [`split_segment`](Self::split_segment) under the maintenance lock `held`.
	pub(crate) async fn split_segment_held(&self, held: &MaintGuard, id: u64, boundary: i64) -> Result<Option<u64>> {
		self.writable()?;
		let aspect = held.aspect();
		aspect_name::validate(aspect)?;
		// M1: the input, as the swap's precondition will name it.
		let rows = self.index.rows(aspect).await?;
		let input = rows.iter().find(|row| row.desc.id == id).cloned().ok_or_else(|| anyhow::anyhow!("aspect {aspect:?} has no segment {id}"))?;
		if !input.desc.time_sorted {
			return Err(anyhow::anyhow!("segment {id} of aspect {aspect:?} is out of order; reconcile it before splitting"));
		}
		let schema = self.require_schema(aspect).await?;
		self.retired_name(&input)?;
		// Read once, capturing the paged page height so each half re-seals in kind.
		let frame = self.read_bound_frame(&input).await?;
		let k = split_index(&frame.timestamps, boundary);
		if k == 0 || k == frame.timestamps.len() {
			// Every row is on one side — nothing to carve.
			return Ok(None);
		}
		// M2: the prefix keeps the segment's id (P1); the suffix takes a fresh one (P2),
		// refused here, before anything is written, when a newer segment overlaps it, and
		// again by the swap against the rows it commits over.
		let plan = SwapPlan::cut(vec![input], frame.timestamps, frame.values, frame.rows_per_page, &[boundary]);
		refuse_outranking_outputs(aspect, &plan.members, plan.extra_spans(), &rows)?;
		let swapped = self.swap_plan(held, &schema, plan).await?;
		Ok(swapped.ids.get(1).copied())
	}

	/// **Reconcile every out-of-order segment** of `aspect` (roadmap Phase 4.6),
	/// returning the number rewritten. The natural trigger is a non-zero
	/// [`unsorted_segments`](AspectStorageStats::unsorted_segments) — after this call
	/// it is zero and every point lookup over the aspect binary-searches.
	///
	/// Each out-of-order segment is reconciled in place by
	/// [`reconcile_segment`](SegmentStore::reconcile_segment); already-sorted segments
	/// are skipped. Segments are visited in seal-id order.
	///
	/// # Errors
	///
	/// As [`reconcile_segment`](SegmentStore::reconcile_segment).
	pub async fn reconcile_aspect(&self, aspect: &str) -> Result<usize> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		let held = self.maintain(aspect).await?;
		self.reconcile_aspect_held(&held).await
	}

	/// [`reconcile_aspect`](Self::reconcile_aspect) under the maintenance lock `held`.
	pub(crate) async fn reconcile_aspect_held(&self, held: &MaintGuard) -> Result<usize> {
		self.writable()?;
		aspect_name::validate(held.aspect())?;
		let unsorted_ids: Vec<u64> = self.index.all(held.aspect()).await?.into_iter().filter(|d| !d.time_sorted).map(|d| d.id).collect();
		let mut reconciled = 0;
		for id in unsorted_ids {
			if self.reconcile_segment_held(held, id).await? {
				reconciled += 1;
			}
		}
		Ok(reconciled)
	}

	/// **Threshold-triggered reconciliation** (roadmap Phase 4.6): run
	/// [`reconcile_aspect`](SegmentStore::reconcile_aspect) **only** when the aspect's
	/// out-of-order backlog has grown past `threshold` out-of-order segments,
	/// otherwise leave it untouched.
	///
	/// This is the trigger primitive an automatic background reconcile/compaction
	/// pass keys on, modeled on the QuestDB-style split-count squash policy — that
	/// engine does not rewrite on every late row; it accumulates split partitions and squashes only
	/// once their number crosses `cairo.o3.last.partition.max.splits` (default 20). The
	/// `unsorted_segments` order-health count is WeftDB's analogue: below the threshold the
	/// backlog is cheap enough to answer with a per-segment linear scan, so the write
	/// amplification of a rewrite is not yet worth paying; at or above it the read cost
	/// dominates and a pass is triggered. *(src: automatic squash past a split
	/// threshold — <https://questdb.com/docs/concepts/partitions/>)*
	///
	/// The count is read from the durable index (the same signal
	/// [`aspect_stats`](SegmentStore::aspect_stats) reports), so a caller can poll this
	/// cheaply and only pay the rewrite when it fires.
	///
	/// Returns `Some(reconciled)` — the number of segments rewritten sorted — when the
	/// pass fired (the backlog was `>= threshold`), and `None` when it did not (the
	/// backlog was below `threshold`, so nothing was read or rewritten). A `threshold`
	/// of 0 is clamped to 1: a pass over a fully-ordered aspect would rewrite nothing,
	/// so the smallest meaningful trigger is "at least one out-of-order segment".
	///
	/// # Errors
	///
	/// As [`reconcile_aspect`](SegmentStore::reconcile_aspect); also propagates the
	/// index read behind the backlog count.
	pub async fn reconcile_aspect_if_unsorted_exceeds(&self, aspect: &str, threshold: usize) -> Result<Option<usize>> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		let held = self.maintain(aspect).await?;
		self.reconcile_aspect_if_unsorted_exceeds_held(&held, threshold).await
	}

	/// [`reconcile_aspect_if_unsorted_exceeds`](Self::reconcile_aspect_if_unsorted_exceeds)
	/// under the maintenance lock `held`.
	pub(crate) async fn reconcile_aspect_if_unsorted_exceeds_held(&self, held: &MaintGuard, threshold: usize) -> Result<Option<usize>> {
		self.writable()?;
		aspect_name::validate(held.aspect())?;
		let threshold = threshold.max(1);
		let unsorted = self.index.load_index(held.aspect()).await?.unsorted_count();
		if unsorted < threshold {
			return Ok(None);
		}
		Ok(Some(self.reconcile_aspect_held(held).await?))
	}

	/// **Store-wide threshold sweep** (roadmap Phase 4.6): apply the
	/// [`reconcile_aspect_if_unsorted_exceeds`](SegmentStore::reconcile_aspect_if_unsorted_exceeds)
	/// trigger to **every** declared aspect, reconciling only those whose out-of-order
	/// backlog is at or above `threshold`.
	///
	/// This is the tick an automatic background reconcile daemon calls: on a timer it
	/// sweeps the store's aspects, pays the rewrite only for the ones over threshold,
	/// and leaves the rest untouched. Aspects are visited in declared-name order; a
	/// `threshold` of 0 clamps to 1 (as for the single-aspect trigger). Returns a
	/// [`ReconcileSweep`] summary — how many aspects were scanned, how many fired, and
	/// the total segments rewritten — the numbers a daemon logs and exports per tick.
	///
	/// A per-aspect failure (as [`reconcile_aspect_if_unsorted_exceeds`](SegmentStore::reconcile_aspect_if_unsorted_exceeds))
	/// is recorded in [`ReconcileSweep::failed`] and the sweep continues with the next
	/// aspect, so one bad aspect cannot stall maintenance of every aspect after it.
	/// An aspect another maintenance operation holds is skipped or waited for as `wait`
	/// says, and listed in [`ReconcileSweep::busy`] if the sweep could not take it.
	///
	/// # Errors
	///
	/// Propagates only the aspect-list read; per-aspect failures are reported in
	/// [`ReconcileSweep::failed`].
	pub async fn reconcile_all_over_threshold(&self, threshold: usize, wait: MaintenanceWait) -> Result<ReconcileSweep> {
		self.writable()?;
		let aspects = self.list_declared_aspects().await?;
		let deadline = wait.deadline();
		let mut sweep = ReconcileSweep { aspects_scanned: aspects.len(), ..ReconcileSweep::default() };
		for aspect in &aspects {
			let Some(held) = self.sweep_maintain(aspect, deadline).await else {
				sweep.busy.push(aspect.clone());
				continue;
			};
			match self.reconcile_aspect_if_unsorted_exceeds_held(&held, threshold).await {
				Ok(Some(reconciled)) => {
					sweep.aspects_reconciled += 1;
					sweep.segments_reconciled += reconciled;
				}
				Ok(None) => {}
				Err(err) => sweep.failed.push((aspect.clone(), err)),
			}
		}
		Ok(sweep)
	}

	/// **Hot/cold threshold reconciliation** (roadmap Phase 4.6): reconcile an aspect's
	/// out-of-order segments the way `QuestDB` squashes partitions — **cold** (sealed,
	/// no-longer-appended) segments are rewritten on every pass, while the **hot tail**
	/// (the most-recently sealed segment, still the append target) is deferred until the
	/// aspect's out-of-order backlog reaches `threshold`.
	///
	/// The plain [`reconcile_aspect_if_unsorted_exceeds`](SegmentStore::reconcile_aspect_if_unsorted_exceeds)
	/// gate is all-or-nothing: below the threshold it leaves *every* out-of-order
	/// segment alone, so a cold segment that will never be appended again waits behind
	/// the hot tail's split budget. That over-defers — a cold segment's rewrite is a
	/// one-time cost that a later append cannot undo, so paying it eagerly is strictly
	/// cheaper than carrying its linear-scan point-lookup cost. Only the hot tail is
	/// worth deferring: rewriting the actively-growing segment on every tick would
	/// rewrite the same bytes repeatedly. This method reconciles the cold segments
	/// unconditionally and gates only the hot tail on the backlog, matching `QuestDB`'s
	/// "squash non-active partitions each commit, defer the active partition until the
	/// split threshold" policy. *(src: <https://questdb.com/docs/concepts/partitions/>)*
	///
	/// The **hot tail** is the segment with the largest id — ids are assigned
	/// monotonically on seal, so the newest is the append target. An already-sorted hot
	/// tail needs no rewrite regardless. The backlog is the aspect's out-of-order
	/// segment count observed at entry (before any rewrite this pass), so the hot-tail
	/// decision does not shift as cold segments drop out of the count.
	///
	/// `threshold` is clamped to 1 (as for the plain trigger). Returns a
	/// [`HotColdReconcile`] splitting the rewrite count into cold vs hot.
	///
	/// # Errors
	///
	/// As [`reconcile_segment`](SegmentStore::reconcile_segment); also propagates the
	/// index read behind the segment list.
	pub async fn reconcile_aspect_hot_cold(&self, aspect: &str, threshold: usize) -> Result<HotColdReconcile> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		let held = self.maintain(aspect).await?;
		self.reconcile_aspect_hot_cold_held(&held, threshold).await
	}

	/// [`reconcile_aspect_hot_cold`](Self::reconcile_aspect_hot_cold) under the
	/// maintenance lock `held`.
	pub(crate) async fn reconcile_aspect_hot_cold_held(&self, held: &MaintGuard, threshold: usize) -> Result<HotColdReconcile> {
		self.writable()?;
		aspect_name::validate(held.aspect())?;
		let threshold = threshold.max(1);
		let descriptors = self.index.all(held.aspect()).await?;
		let hot_tail_id = descriptors.iter().map(|d| d.id).max();
		let unsorted: Vec<u64> = descriptors.iter().filter(|d| !d.time_sorted).map(|d| d.id).collect();
		let hot_fires = unsorted.len() >= threshold;
		let mut out = HotColdReconcile::default();
		for id in unsorted {
			if Some(id) == hot_tail_id {
				// The hot tail is deferred until the backlog reaches the threshold.
				if hot_fires && self.reconcile_segment_held(held, id).await? {
					out.hot_reconciled += 1;
				}
			} else if self.reconcile_segment_held(held, id).await? {
				// A cold segment is always worth reconciling.
				out.cold_reconciled += 1;
			}
		}
		Ok(out)
	}

	/// **Store-wide hot/cold reconcile sweep** (roadmap Phase 4.6): apply
	/// [`reconcile_aspect_hot_cold`](SegmentStore::reconcile_aspect_hot_cold) to **every**
	/// declared aspect — eagerly reconciling each aspect's cold segments while deferring
	/// its hot tail until that aspect's own backlog reaches `threshold`.
	///
	/// This is the hot/cold analogue of
	/// [`reconcile_all_over_threshold`](SegmentStore::reconcile_all_over_threshold): the
	/// tick a background daemon calls when it wants cold segments cleaned up on every
	/// sweep without rewriting each aspect's actively-appended tail on every tick.
	/// Aspects are visited in declared-name order; a `threshold` of 0 clamps to 1.
	/// Returns a [`HotColdSweep`] — aspects scanned, aspects that rewrote at least one
	/// segment, and the cold/hot rewrite split — the numbers a daemon logs and exports.
	///
	/// A per-aspect failure (as [`reconcile_aspect_hot_cold`](SegmentStore::reconcile_aspect_hot_cold))
	/// is recorded in [`HotColdSweep::failed`] and the sweep continues with the next aspect.
	/// An aspect another maintenance operation holds is skipped or waited for as `wait`
	/// says, and listed in [`HotColdSweep::busy`] if the sweep could not take it.
	///
	/// # Errors
	///
	/// Propagates only the aspect-list read; per-aspect failures are reported in
	/// [`HotColdSweep::failed`].
	pub async fn reconcile_all_hot_cold(&self, threshold: usize, wait: MaintenanceWait) -> Result<HotColdSweep> {
		self.writable()?;
		let aspects = self.list_declared_aspects().await?;
		let deadline = wait.deadline();
		let mut sweep = HotColdSweep { aspects_scanned: aspects.len(), ..HotColdSweep::default() };
		for aspect in &aspects {
			let Some(held) = self.sweep_maintain(aspect, deadline).await else {
				sweep.busy.push(aspect.clone());
				continue;
			};
			match self.reconcile_aspect_hot_cold_held(&held, threshold).await {
				Ok(outcome) if outcome.total() > 0 => {
					sweep.aspects_reconciled += 1;
					sweep.cold_reconciled += outcome.cold_reconciled;
					sweep.hot_reconciled += outcome.hot_reconciled;
				}
				Ok(_) => {}
				Err(err) => sweep.failed.push((aspect.clone(), err)),
			}
		}
		Ok(sweep)
	}

	/// **Cross-segment out-of-order merge** (roadmap Phase 4.6): collapse every group
	/// of time-**overlapping** segments of `aspect` into a single time-sorted segment,
	/// resolving late data that re-entered an already-covered window.
	///
	/// Where [`reconcile_segment`](SegmentStore::reconcile_segment) fixes *intra*-segment
	/// disorder (the [`unsorted_segments`](AspectStorageStats::unsorted_segments) signal),
	/// this fixes *cross*-segment overlap (the
	/// [`overlapping_segments`](AspectStorageStats::overlapping_segments) signal): two
	/// internally-sorted segments whose windows intersect. Each connected component of
	/// overlapping segments (transitive time overlap) is merged into one segment under the
	/// component's **lowest id** and the other members are deleted, so afterwards
	/// [`SegmentIndex::overlapping_count`](weft_physical_type::SegmentIndex::overlapping_count)
	/// is zero and a point/range read over the merged window opens a single segment.
	///
	/// **Merge semantics — newer wins (upsert).** Members are folded in ascending seal
	/// id (oldest → newest) with [`merge_newer_wins`], so at any timestamp two members
	/// share, the more-recently-sealed value supersedes the older one — the same
	/// last-writer-wins answer [`read_point`](SegmentStore::read_point) already gives
	/// across overlapping segments. This *dedups* cross-segment duplicate timestamps
	/// (a range read no longer returns the superseded row), which is the intended
	/// reconciliation/upsert behaviour. Rows at a timestamp carried by only one member
	/// — including that member's own internal duplicates — are preserved.
	///
	/// Each overlap component is either fully rewritten into one segment (paged once it holds
	/// more than 4,096 rows, whatever kind its members
	/// were) or, when a large **cold prefix**
	/// dominates, **split** (per [`SplitPolicy`]) into an untouched-going-forward prefix
	/// segment plus a merged hot-suffix segment — see
	/// [`reconcile_overlaps_with_policy`](SegmentStore::reconcile_overlaps_with_policy).
	/// Non-overlapping segments are left untouched. This entry point uses
	/// [`SplitPolicy::questdb_default`] (a 50 MiB split floor), so components of small
	/// segments always take the full-rewrite path.
	///
	/// **Write-once** (crash-consistency design sections 5.3 and 7; slice S9). A
	/// component's output is a new frame under a never-used name, written and fsynced
	/// beside its members (the output's row records the frame actually written, whatever
	/// kind its id's member was), and one `segment_index.db` transaction
	/// swaps it in: it replaces the lowest member's row and deletes the others, each only
	/// if it is still the version read, and journals every member's frame as retired, for
	/// the reaper to unlink once no read may still open it. No reader ever sees a member's
	/// rows beside the output's, and a crash or power cut at any point leaves the
	/// component either as it was or merged, whole. A member another operation changed
	/// meanwhile fails the swap with a `Conflict`, which writes nothing back. The
	/// materialized rollup is rebuilt after each component's swap.
	///
	/// Returns the net reduction in segment count across all components — `members − 1`
	/// per fully-rewritten component and `members − 2` per split one (a split keeps two
	/// segments); zero when no two segments overlap (a no-op).
	///
	/// # Errors
	///
	/// Returns an error if `aspect` has no declared schema, or if a member's frame is not
	/// the one its row was committed with; propagates a filesystem read/write error, a
	/// decode/re-seal failure, or a libSQL failure (a `Conflict` when a member changed
	/// under the merge). Components merged before the failure stay merged.
	/// A [`MaintenanceBusy`] error if another maintenance operation holds the aspect for
	/// longer than [`maintenance_wait`](Self::maintenance_wait).
	pub async fn reconcile_overlaps(&self, aspect: &str) -> Result<usize> {
		self.reconcile_overlaps_with_policy(aspect, SplitPolicy::questdb_default()).await
	}

	/// **Cross-segment overlap merge with an explicit split policy** (roadmap Phase 4.6
	/// — the split-not-rewrite path). As [`reconcile_overlaps`](SegmentStore::reconcile_overlaps),
	/// but `policy` governs whether each overlap component is fully rewritten or split.
	///
	/// **Merge semantics — newer wins (upsert).** Every component's members are folded
	/// oldest → newest with [`merge_newer_wins`], so at any shared timestamp the
	/// more-recently-sealed value supersedes the older one (dedup / last-writer-wins),
	/// exactly as [`reconcile_overlaps`](SegmentStore::reconcile_overlaps) documents. The
	/// *logical* result is identical regardless of `policy`; only the on-disk layout
	/// differs.
	///
	/// **Split-not-rewrite.** A component's **cold prefix** is the run of merged rows
	/// before its second-earliest member starts — those timestamps are carried by a
	/// single member, so they saw no cross-segment overlap and need no merge. When that
	/// prefix both clears the policy's [`min_split_bytes`](SplitPolicy::min_split_bytes)
	/// floor and outweighs the hot suffix ([`SplitPolicy::decide`] → [`Split`](SplitDecision::Split)),
	/// the component is laid out as **two** segments — the cold prefix under the lowest
	/// member id and the merged hot suffix under the second lowest (release plan D17, P1:
	/// the `j`-th output in time order takes the `j`-th smallest member id, so a suffix
	/// never outranks a segment sealed after the merge read the index) — instead of one,
	/// cut where the suffix's first timestamp starts. The two are
	/// disjoint in time (prefix < boundary ≤ suffix), so [`overlapping_count`](weft_physical_type::SegmentIndex::overlapping_count)
	/// still lands at zero, and a **later** late arrival that re-enters only the hot
	/// window forms an overlap component with the suffix alone: the cold prefix is never
	/// pulled back in and never rewritten again, bounding write amplification over the
	/// series' lifetime the way `QuestDB`'s partition split does. *(This pass still reads
	/// and rewrites the prefix once to carve it; the saving is amortized over subsequent
	/// reconciles — <https://questdb.com/docs/concepts/partitions/>)*
	///
	/// Byte sizes for the decision are estimated from the component's on-disk bytes and
	/// row counts (uniform per-row encoding), with the merged tail treated as new data
	/// via `decide(prefix, suffix, 0)`.
	///
	/// # Errors
	///
	/// As [`reconcile_overlaps`](SegmentStore::reconcile_overlaps).
	pub async fn reconcile_overlaps_with_policy(&self, aspect: &str, policy: SplitPolicy) -> Result<usize> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		let held = self.maintain(aspect).await?;
		self.reconcile_overlaps_held(&held, policy).await
	}

	/// [`reconcile_overlaps_with_policy`](Self::reconcile_overlaps_with_policy) under the
	/// maintenance lock `held`.
	pub(crate) async fn reconcile_overlaps_held(&self, held: &MaintGuard, policy: SplitPolicy) -> Result<usize> {
		self.writable()?;
		let aspect = held.aspect();
		aspect_name::validate(aspect)?;
		// M1: the rows, as the swaps' preconditions will name them. Only this operation,
		// under the maintenance lock, changes them; a seal meanwhile only adds rows.
		let components = overlap_components(self.index.rows(aspect).await?);
		let schema = self.require_schema(aspect).await?;
		let mut removed = 0;
		for mut members in components {
			if members.len() < 2 {
				continue;
			}
			// Fold members oldest → newest so the most-recently-sealed value wins a tie.
			members.sort_by_key(|member| member.desc.id);
			let component_bytes: u64 = members.iter().map(|member| member.desc.byte_len).sum();
			let component_rows: usize = members.iter().map(|member| member.desc.row_count).sum();
			// The second-earliest member start: rows before it are the cold prefix (carried by
			// one member, no cross-segment overlap). Every member has a time range.
			let mut starts: Vec<i64> = members.iter().filter_map(|member| member.desc.min_ts).collect();
			starts.sort_unstable();
			let cold_boundary = starts.get(1).copied().unwrap_or(i64::MIN);
			let (all_ts, all_vs) = self.merge_members(&members).await?;
			// The cold prefix: merged rows strictly before the second member's start.
			let prefix_len = all_ts.partition_point(|&t| t < cold_boundary);
			// Estimate prefix/suffix bytes from the component's uniform per-row size.
			let bytes_per_row = if component_rows > 0 { component_bytes / component_rows as u64 } else { 0 };
			let prefix_bytes = bytes_per_row.saturating_mul(prefix_len as u64);
			let suffix_bytes = bytes_per_row.saturating_mul((all_ts.len() - prefix_len) as u64);
			let split = prefix_len > 0 && prefix_len < all_ts.len() && policy.decide(prefix_bytes, suffix_bytes, 0) == SplitDecision::Split;
			// Full rewrite: one output under the lowest member id. Split: the cold
			// prefix under the lowest id and the merged hot suffix under the next lowest (P1),
			// cut at the timestamp where the suffix starts; the two are disjoint in time.
			let cuts: &[i64] = if split { &[cold_boundary] } else { &[] };
			let rows_per_page = merge_rows_per_page(all_ts.len());
			let plan = SwapPlan::cut(members, all_ts, all_vs, rows_per_page, cuts);
			removed += plan.removes();
			self.swap_plan(held, &schema, plan).await?;
		}
		Ok(removed)
	}

	/// **Store-wide cross-segment overlap merge** (roadmap Phase 4.6): apply
	/// [`reconcile_overlaps`](SegmentStore::reconcile_overlaps) to **every** declared
	/// aspect, merging each aspect's time-overlap groups.
	///
	/// The store-wide counterpart to the per-aspect merge — the tick a background
	/// daemon calls to keep cross-segment overlap from accumulating store-wide. Aspects
	/// are visited in declared-name order. Returns an [`OverlapSweep`] — how many
	/// aspects were scanned, how many actually merged anything, and the total segments
	/// removed — the numbers a daemon logs and exports.
	///
	/// A per-aspect failure (as [`reconcile_overlaps`](SegmentStore::reconcile_overlaps))
	/// is recorded in [`OverlapSweep::failed`] and the sweep continues with the next aspect.
	/// An aspect another maintenance operation holds is skipped or waited for as `wait`
	/// says, and listed in [`OverlapSweep::busy`] if the sweep could not take it.
	///
	/// # Errors
	///
	/// Propagates only the aspect-list read; per-aspect failures are reported in
	/// [`OverlapSweep::failed`].
	pub async fn reconcile_all_overlaps(&self, wait: MaintenanceWait) -> Result<OverlapSweep> {
		self.writable()?;
		let aspects = self.list_declared_aspects().await?;
		let deadline = wait.deadline();
		let mut sweep = OverlapSweep { aspects_scanned: aspects.len(), ..OverlapSweep::default() };
		for aspect in &aspects {
			let Some(held) = self.sweep_maintain(aspect, deadline).await else {
				sweep.busy.push(aspect.clone());
				continue;
			};
			match self.reconcile_overlaps_held(&held, SplitPolicy::questdb_default()).await {
				Ok(removed) if removed > 0 => {
					sweep.aspects_reconciled += 1;
					sweep.segments_removed += removed;
				}
				Ok(_) => {}
				Err(err) => sweep.failed.push((aspect.clone(), err)),
			}
		}
		Ok(sweep)
	}

	/// **Store-wide overlap merge with an explicit split policy** (roadmap Phase 4.6 —
	/// the split-not-rewrite path). As [`reconcile_all_overlaps`](SegmentStore::reconcile_all_overlaps),
	/// but every aspect's merge runs under `policy` via
	/// [`reconcile_overlaps_with_policy`](SegmentStore::reconcile_overlaps_with_policy),
	/// so a dominant cold prefix is split off rather than fully rewritten.
	///
	/// An aspect is counted as reconciled when it **carried cross-segment overlap**
	/// before the pass (its [`overlapping_segments`](AspectStorageStats::overlapping_segments)
	/// was non-zero — exactly the aspects the merge acts on), rather than by the net
	/// removed count: a two-member split changes the layout while leaving the segment
	/// count unchanged, so a `removed > 0` test would miss it. `segments_removed` remains
	/// the honest **net** reduction (zero for a pure split). For the full-rewrite path
	/// (the default 50 MiB floor) this counts identically to
	/// [`reconcile_all_overlaps`](SegmentStore::reconcile_all_overlaps), since there
	/// overlap-present ⟺ a segment is removed.
	///
	/// A per-aspect failure (the pre-pass overlap read or the merge itself) is recorded in
	/// [`OverlapSweep::failed`] and the sweep continues with the next aspect.
	/// An aspect another maintenance operation holds is skipped or waited for as `wait`
	/// says, and listed in [`OverlapSweep::busy`] if the sweep could not take it.
	///
	/// # Errors
	///
	/// As [`reconcile_all_overlaps`](SegmentStore::reconcile_all_overlaps).
	pub async fn reconcile_all_overlaps_with_policy(&self, policy: SplitPolicy, wait: MaintenanceWait) -> Result<OverlapSweep> {
		self.writable()?;
		let aspects = self.list_declared_aspects().await?;
		let deadline = wait.deadline();
		let mut sweep = OverlapSweep { aspects_scanned: aspects.len(), ..OverlapSweep::default() };
		for aspect in &aspects {
			let Some(held) = self.sweep_maintain(aspect, deadline).await else {
				sweep.busy.push(aspect.clone());
				continue;
			};
			let pass = async {
				let had_overlap = self.index.load_index(aspect).await?.overlapping_count() > 0;
				let removed = self.reconcile_overlaps_held(&held, policy).await?;
				anyhow::Ok((had_overlap, removed))
			};
			match pass.await {
				Ok((true, removed)) => {
					sweep.aspects_reconciled += 1;
					sweep.segments_removed += removed;
				}
				Ok((false, _)) => {}
				Err(err) => sweep.failed.push((aspect.clone(), err)),
			}
		}
		Ok(sweep)
	}

	/// **Squash all of an aspect's segments into one** (roadmap Phase 4.6 — the squash
	/// half of the split-not-rewrite path).
	///
	/// Repeated late arrivals under the split path accumulate small time-disjoint
	/// segments (a growing pile of carved-off cold prefixes). Left unchecked that
	/// fragments an aspect and multiplies the per-segment read/prune overhead. Squash is
	/// the QuestDB-style bound on that fragmentation: it folds **every** segment of
	/// `aspect` — in ascending seal id, [`merge_newer_wins`] so any residual shared
	/// timestamp still resolves last-writer-wins — into a single time-sorted segment
	/// (paged past 4,096 rows) under the lowest id, deleting the rest. After it, the aspect is one segment
	/// with no cross-segment overlap. The natural trigger is a segment count past a
	/// threshold (see [`squash_aspect_if_exceeds`](SegmentStore::squash_aspect_if_exceeds));
	/// squashing trades the split path's low write amplification for a low segment count,
	/// so it is meant to fire rarely, not every commit.
	///
	/// **Write-once** (crash-consistency design sections 5.3 and 7; slice S9), as the
	/// overlap merge ([`reconcile_overlaps`](SegmentStore::reconcile_overlaps)): one swap
	/// replaces every segment the squash read with its one output, at once or not at all.
	/// A seal that commits meanwhile is not one of them, keeps its own id and row, and
	/// outranks the output where they share a timestamp.
	///
	/// Returns the number of segments removed (`count − 1`); zero when the aspect has
	/// fewer than two segments (nothing to squash).
	///
	/// # Errors
	///
	/// Returns an error if `aspect` has no declared schema, or if a segment's frame is not
	/// the one its row was committed with; propagates a filesystem read/write error, a
	/// decode/re-seal failure, or a libSQL failure (a `Conflict` when a segment changed
	/// under the squash).
	/// A [`MaintenanceBusy`] error if another maintenance operation holds the aspect for
	/// longer than [`maintenance_wait`](Self::maintenance_wait).
	pub async fn squash_aspect(&self, aspect: &str) -> Result<usize> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		let held = self.maintain(aspect).await?;
		self.squash_aspect_held(&held).await
	}

	/// [`squash_aspect`](Self::squash_aspect) under the maintenance lock `held`.
	pub(crate) async fn squash_aspect_held(&self, held: &MaintGuard) -> Result<usize> {
		self.writable()?;
		let aspect = held.aspect();
		aspect_name::validate(aspect)?;
		// M1: every row, ascending by id, as the swap's preconditions will name them.
		let members = self.index.rows(aspect).await?;
		if members.len() < 2 {
			return Ok(0);
		}
		let schema = self.require_schema(aspect).await?;
		// Fold oldest → newest so the most-recently-sealed value wins any residual tie.
		let (all_ts, all_vs) = self.merge_members(&members).await?;
		let rows_per_page = merge_rows_per_page(all_ts.len());
		let plan = SwapPlan::cut(members, all_ts, all_vs, rows_per_page, &[]);
		let removed = plan.removes();
		self.swap_plan(held, &schema, plan).await?;
		Ok(removed)
	}

	/// **Threshold-triggered squash** (roadmap Phase 4.6): run
	/// [`squash_aspect`](SegmentStore::squash_aspect) **only** when `aspect` has more than
	/// `max_segments` segments, otherwise leave it untouched.
	///
	/// The trigger a background compaction pass keys on to cap split-path fragmentation,
	/// modeled on `QuestDB`'s `cairo.o3.last.partition.max.splits` squash threshold: below
	/// the cap the fragmentation is cheap enough to tolerate, so the squash's write cost
	/// is not yet worth paying; above it the read/prune overhead of many segments
	/// dominates and a squash fires. A `max_segments` of 0 clamps to 1 (an aspect can
	/// never squash below a single segment).
	///
	/// Returns `Some(removed)` when the squash fired (the count was `> max_segments`) and
	/// `None` when it held. The count is read from the durable index, so a caller can
	/// poll cheaply and pay the rewrite only when it fires.
	///
	/// # Errors
	///
	/// As [`squash_aspect`](SegmentStore::squash_aspect); also propagates the segment-count read.
	pub async fn squash_aspect_if_exceeds(&self, aspect: &str, max_segments: usize) -> Result<Option<usize>> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		let held = self.maintain(aspect).await?;
		self.squash_aspect_if_exceeds_held(&held, max_segments).await
	}

	/// [`squash_aspect_if_exceeds`](Self::squash_aspect_if_exceeds) under the maintenance
	/// lock `held`.
	pub(crate) async fn squash_aspect_if_exceeds_held(&self, held: &MaintGuard, max_segments: usize) -> Result<Option<usize>> {
		self.writable()?;
		aspect_name::validate(held.aspect())?;
		let max_segments = max_segments.max(1);
		if self.index.count(held.aspect()).await? <= max_segments {
			return Ok(None);
		}
		Ok(Some(self.squash_aspect_held(held).await?))
	}

	/// **Store-wide threshold squash** (roadmap Phase 4.6): apply the
	/// [`squash_aspect_if_exceeds`](SegmentStore::squash_aspect_if_exceeds) trigger to
	/// **every** declared aspect, squashing only those whose segment count exceeds
	/// `max_segments`.
	///
	/// The tick a background squash daemon calls to cap split-path fragmentation
	/// store-wide: on a timer it sweeps the aspects, pays the squash only for the ones
	/// over the cap, and leaves the rest untouched. Aspects are visited in declared-name
	/// order; a `max_segments` of 0 clamps to 1. Returns a [`SquashSweep`] — how many
	/// aspects were scanned, how many were squashed, and the total segments removed.
	///
	/// A per-aspect failure (as [`squash_aspect_if_exceeds`](SegmentStore::squash_aspect_if_exceeds))
	/// is recorded in [`SquashSweep::failed`] and the sweep continues with the next aspect.
	/// An aspect another maintenance operation holds is skipped or waited for as `wait`
	/// says, and listed in [`SquashSweep::busy`] if the sweep could not take it.
	///
	/// # Errors
	///
	/// Propagates only the aspect-list read; per-aspect failures are reported in
	/// [`SquashSweep::failed`].
	pub async fn squash_all_over_threshold(&self, max_segments: usize, wait: MaintenanceWait) -> Result<SquashSweep> {
		self.writable()?;
		let aspects = self.list_declared_aspects().await?;
		let deadline = wait.deadline();
		let mut sweep = SquashSweep { aspects_scanned: aspects.len(), ..SquashSweep::default() };
		for aspect in &aspects {
			let Some(held) = self.sweep_maintain(aspect, deadline).await else {
				sweep.busy.push(aspect.clone());
				continue;
			};
			match self.squash_aspect_if_exceeds_held(&held, max_segments).await {
				Ok(Some(removed)) => {
					sweep.aspects_squashed += 1;
					sweep.segments_removed += removed;
				}
				Ok(None) => {}
				Err(err) => sweep.failed.push((aspect.clone(), err)),
			}
		}
		Ok(sweep)
	}

	/// **Compact an aspect toward a target segment size** (roadmap Phase 4.6 — the
	/// size-aware counterpart of [`squash_aspect`](SegmentStore::squash_aspect)).
	///
	/// [`squash_aspect`](SegmentStore::squash_aspect) folds *every* segment into one, which
	/// the `downsample_range` segment-count bench showed over-corrects: at a fixed row count
	/// a single large segment reads **slower** than a handful of mid-sized ones (one
	/// 200k-row segment measured ~32.8 ms vs ~9.7 ms for sixteen ~12.5k-row segments — a
	/// single segment offers no cross-segment parallelism and decodes its whole column in
	/// one shot, while too many segments pay a per-segment fixed cost). This compaction
	/// instead coalesces **consecutive** (seal-id-ordered) segments into groups whose
	/// combined row count first reaches `target_rows`, merging each multi-segment group
	/// into one segment under the group's lowest id and leaving already-large segments
	/// untouched — folding an over-fragmented aspect toward ~`target_rows`-sized segments
	/// rather than a single giant one.
	///
	/// Grouping is by ascending seal id (≈ time order for append-mostly ingest); each group
	/// merges oldest→newest via [`merge_newer_wins`] so a residual shared timestamp still
	/// resolves last-writer-wins, and the merged rows are time-sorted so every output is
	/// sorted. A `target_rows` of 0 clamps to 1, so every segment forms its own singleton
	/// group and the call is a no-op. A single-segment group is never rewritten.
	///
	/// **Write-once** (crash-consistency design sections 5.3 and 7; slice S9), as the
	/// overlap merge ([`reconcile_overlaps`](SegmentStore::reconcile_overlaps)): each group
	/// is one swap of its members for its output. A group is a run of consecutive ids, so
	/// its output, under the group's lowest id, outranks exactly what its members did.
	///
	/// Returns the number of segments removed (`Σ (group_len − 1)`); zero when nothing
	/// coalesced (fewer than two segments, or every segment already ≥ `target_rows`).
	///
	/// # Errors
	///
	/// As [`squash_aspect`](SegmentStore::squash_aspect).
	pub async fn squash_aspect_to_target_rows(&self, aspect: &str, target_rows: usize) -> Result<usize> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		let held = self.maintain(aspect).await?;
		self.squash_aspect_to_target_rows_held(&held, target_rows).await
	}

	/// [`squash_aspect_to_target_rows`](Self::squash_aspect_to_target_rows) under the
	/// maintenance lock `held`.
	pub(crate) async fn squash_aspect_to_target_rows_held(&self, held: &MaintGuard, target_rows: usize) -> Result<usize> {
		self.writable()?;
		let aspect = held.aspect();
		aspect_name::validate(aspect)?;
		let target_rows = target_rows.max(1);
		// M1: every row, ascending by id, as the swaps' preconditions will name them.
		let rows = self.index.rows(aspect).await?;
		if rows.len() < 2 {
			return Ok(0);
		}
		// Greedily group consecutive ids until a group's cumulative row count first reaches
		// the target; a trailing partial group is kept as-is. A group that stays a singleton
		// (a segment already ≥ target, or the lone tail) is skipped below — never rewritten.
		// A group is a run of consecutive live ids, so no other row's id falls between its
		// members' (the precedence argument of P1).
		let mut groups: Vec<Vec<IndexRow>> = Vec::new();
		let mut group: Vec<IndexRow> = Vec::new();
		let mut group_rows = 0usize;
		for row in rows {
			group_rows += row.desc.row_count;
			group.push(row);
			if group_rows >= target_rows {
				groups.push(std::mem::take(&mut group));
				group_rows = 0;
			}
		}
		if !group.is_empty() {
			groups.push(group);
		}
		let schema = self.require_schema(aspect).await?;
		let mut removed = 0usize;
		for members in groups {
			if members.len() < 2 {
				continue;
			}
			// One swap per group: its members folded oldest → newest (the most-recently-sealed
			// value wins any residual tie) into one output under the group's lowest id.
			let (all_ts, all_vs) = self.merge_members(&members).await?;
			let rows_per_page = merge_rows_per_page(all_ts.len());
			let plan = SwapPlan::cut(members, all_ts, all_vs, rows_per_page, &[]);
			removed += plan.removes();
			self.swap_plan(held, &schema, plan).await?;
		}
		// The swaps deleted `removed` index rows; drop the versions they left (see
		// `SegmentIndexStore::checkpoint`). The squash is done either way.
		if removed > 0 {
			if let Err(e) = self.index.checkpoint().await {
				tracing::warn!(aspect, removed, error = %e, "could not checkpoint the segment index after a squash; reads walk the deleted rows' versions until Turso's own checkpoint");
			}
		}
		Ok(removed)
	}

	/// **Store-wide size-targeted compaction** (roadmap Phase 4.6): apply
	/// [`squash_aspect_to_target_rows`](SegmentStore::squash_aspect_to_target_rows) to
	/// **every** declared aspect, coalescing each toward ~`target_rows`-sized segments.
	///
	/// The tick a background compaction daemon calls to hold fragmentation near the
	/// read-optimal segment size store-wide — the size-aware counterpart of
	/// [`squash_all_over_threshold`](SegmentStore::squash_all_over_threshold) (which folds
	/// each over-threshold aspect all the way to one). Aspects are visited in declared-name
	/// order. Returns a [`SquashSweep`]: how many aspects were scanned, how many actually
	/// coalesced at least one segment, and the total segments removed.
	///
	/// A per-aspect failure (as [`squash_aspect_to_target_rows`](SegmentStore::squash_aspect_to_target_rows))
	/// is recorded in [`SquashSweep::failed`] and the sweep continues with the next aspect.
	/// An aspect another maintenance operation holds is skipped or waited for as `wait`
	/// says, and listed in [`SquashSweep::busy`] if the sweep could not take it.
	///
	/// # Errors
	///
	/// Propagates only the aspect-list read; per-aspect failures are reported in
	/// [`SquashSweep::failed`].
	pub async fn squash_all_to_target_rows(&self, target_rows: usize, wait: MaintenanceWait) -> Result<SquashSweep> {
		self.writable()?;
		let aspects = self.list_declared_aspects().await?;
		let deadline = wait.deadline();
		let mut sweep = SquashSweep { aspects_scanned: aspects.len(), ..SquashSweep::default() };
		for aspect in &aspects {
			let Some(held) = self.sweep_maintain(aspect, deadline).await else {
				sweep.busy.push(aspect.clone());
				continue;
			};
			match self.squash_aspect_to_target_rows_held(&held, target_rows).await {
				Ok(removed) if removed > 0 => {
					sweep.aspects_squashed += 1;
					sweep.segments_removed += removed;
				}
				Ok(_) => {}
				Err(err) => sweep.failed.push((aspect.clone(), err)),
			}
		}
		Ok(sweep)
	}

	/// **Fragmentation-gated** size-targeted compaction of one aspect (roadmap Phase 4.6).
	///
	/// The cheap-first guard the background daemon uses so it does not scan every aspect's
	/// segment list on every tick. Reads the **O(1)** per-aspect rollup
	/// ([`aspect_metadata`](Self::aspect_metadata)) for the segment count and total
	/// rows, computes the minimum segments needed to hold that many rows at `target_rows`
	/// (`ideal = ⌈total_rows / target_rows⌉`), and runs
	/// [`squash_aspect_to_target_rows`](SegmentStore::squash_aspect_to_target_rows) **only when
	/// the aspect carries more segments than that** — i.e. it is genuinely over-fragmented.
	/// A well-sized aspect returns `None` without ever reading its segment descriptors, so a
	/// converged aspect costs one control-plane rollup read per tick, not a full index scan.
	///
	/// Returns `Some(removed)` when the compaction ran, `None` when the aspect held (already
	/// as compact as the target allows, unknown, or empty). `target_rows` clamps to 1.
	///
	/// # Errors
	///
	/// As [`squash_aspect_to_target_rows`](SegmentStore::squash_aspect_to_target_rows); also
	/// propagates the rollup read.
	pub async fn squash_aspect_to_target_rows_if_fragmented(&self, aspect: &str, target_rows: usize) -> Result<Option<usize>> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		let held = self.maintain(aspect).await?;
		self.squash_aspect_to_target_rows_if_fragmented_held(&held, target_rows).await
	}

	/// [`squash_aspect_to_target_rows_if_fragmented`](Self::squash_aspect_to_target_rows_if_fragmented)
	/// under the maintenance lock `held`.
	pub(crate) async fn squash_aspect_to_target_rows_if_fragmented_held(&self, held: &MaintGuard, target_rows: usize) -> Result<Option<usize>> {
		self.writable()?;
		aspect_name::validate(held.aspect())?;
		let target_rows = target_rows.max(1);
		let Some(meta) = self.index.rollup(held.aspect()).await? else { return Ok(None) };
		// Minimum segments to hold total_rows at the target; a fully-compacted aspect sits at
		// exactly this count (or below), so more than this means there is fragmentation to fold.
		let ideal = meta.total_rows.div_ceil(target_rows as u64).max(1);
		if (meta.segment_count as u64) <= ideal {
			return Ok(None);
		}
		Ok(Some(self.squash_aspect_to_target_rows_held(held, target_rows).await?))
	}

	/// **Fragmentation-gated store-wide** size-targeted compaction (roadmap Phase 4.6).
	///
	/// The tick the background compaction daemon actually calls: applies
	/// [`squash_aspect_to_target_rows_if_fragmented`](SegmentStore::squash_aspect_to_target_rows_if_fragmented)
	/// to every declared aspect, so a converged store costs one O(1) rollup read per aspect
	/// per tick and rewrites nothing. Unlike [`squash_all_to_target_rows`](SegmentStore::squash_all_to_target_rows)
	/// (which unconditionally scans and coalesces every aspect — the "force" form a manual
	/// caller wants), this skips aspects already at or below their ideal segment count.
	/// Returns a [`SquashSweep`].
	///
	/// A per-aspect failure (as [`squash_aspect_to_target_rows_if_fragmented`](SegmentStore::squash_aspect_to_target_rows_if_fragmented))
	/// is recorded in [`SquashSweep::failed`] and the sweep continues with the next aspect.
	/// An aspect another maintenance operation holds is skipped or waited for as `wait`
	/// says, and listed in [`SquashSweep::busy`] if the sweep could not take it.
	///
	/// # Errors
	///
	/// Propagates only the aspect-list read; per-aspect failures are reported in
	/// [`SquashSweep::failed`].
	pub async fn squash_all_to_target_rows_if_fragmented(&self, target_rows: usize, wait: MaintenanceWait) -> Result<SquashSweep> {
		self.writable()?;
		let aspects = self.list_declared_aspects().await?;
		let deadline = wait.deadline();
		let mut sweep = SquashSweep { aspects_scanned: aspects.len(), ..SquashSweep::default() };
		for aspect in &aspects {
			let Some(held) = self.sweep_maintain(aspect, deadline).await else {
				sweep.busy.push(aspect.clone());
				continue;
			};
			match self.squash_aspect_to_target_rows_if_fragmented_held(&held, target_rows).await {
				Ok(Some(removed)) if removed > 0 => {
					sweep.aspects_squashed += 1;
					sweep.segments_removed += removed;
				}
				Ok(_) => {}
				Err(err) => sweep.failed.push((aspect.clone(), err)),
			}
		}
		Ok(sweep)
	}

	/// Re-derive `aspect`'s materialized `segment_index.db` rollup from the durable segment
	/// index and write it back, returning the reconciled rollup — the **authoritative**
	/// recovery path.
	///
	/// Where each seal folds one descriptor forward (O(1), but assumes it never sees the
	/// same segment twice), this recomputes the rollup from scratch via
	/// [`AspectMetadata::from_index`] over the whole index, so it is correct regardless
	/// of how the row got out of step (a re-seal of an id, a rollup restored from an
	/// older snapshot). Since S11 the seal and swap transactions fold the rollup in
	/// themselves, so there is no longer a crash window between the index commit and
	/// the fold for a row to get out of step through. The
	/// segment index is the source of truth; this makes the rollup match it.
	///
	/// It reads the index and writes the rollup under the aspect's commit lock, where a
	/// seal commits its row and folds it in, so a seal committing meanwhile is either in
	/// the index it reads or folded into the rollup after it writes, never lost.
	///
	/// # Errors
	///
	/// Propagates any libSQL read or write failure.
	pub async fn rebuild_aspect_metadata(&self, aspect: &str) -> Result<AspectMetadata> {
		self.writable()?;
		let commit = self.commit_lock(aspect).await?;
		let meta = self.rebuild_rollup_locked(aspect, &commit).await?;
		drop(commit);
		Ok(meta)
	}

	/// Re-derive **every** aspect's rollup from the durable segment index, returning the
	/// number of aspects reconciled. Rebuilds every `segment_index.db` rollup row from the index —
	/// the recovery path for a lost or stale rollup DB beside an intact index.
	///
	/// The aspect set comes from the index itself
	/// ([`SegmentIndexStore::list_aspects`]), so it reconciles exactly the aspects that
	/// have segments.
	///
	/// # Errors
	///
	/// Propagates any libSQL read or write failure.
	pub async fn rebuild_all_metadata(&self) -> Result<usize> {
		self.writable()?;
		let aspects = self.index.list_aspects().await?;
		let count = aspects.len();
		for aspect in aspects {
			self.rebuild_aspect_metadata(&aspect).await?;
		}
		Ok(count)
	}

	/// Backfill the materialized `segment_index.db` rollup rows that are missing, and
	/// only those — the open-time repair for stores written before S11.
	///
	/// Before S11 the rollup lived in `metadata.db`, so a pre-S11 store has no
	/// `segment_index.db` rows at all, and the journal replay only re-derives the
	/// aspects its journal names: a clean pre-S11 store opens with every rollup
	/// missing, and [`aspect_metadata`](Self::aspect_metadata) reads default zeros
	/// for it (the v0.1.0 compat gate caught this: a migrated aspect read 0 nulls
	/// against the 14 v0.1.0 counted). Rebuilding just the missing rows keeps the
	/// open cheap for S11-native stores, whose rows already exist, while a pre-S11
	/// store reads correctly from its first open; a later seal or swap keeps its
	/// row current by folding, as usual. An aspect with no index rows has no
	/// segments, so it needs no row and its default-zero read is correct.
	///
	/// # Errors
	///
	/// Propagates any libSQL read or write failure.
	pub async fn backfill_missing_rollups(&self) -> Result<usize> {
		self.writable()?;
		let mut backfilled = 0;
		for aspect in self.index.list_aspects().await? {
			if self.index.rollup(&aspect).await?.is_none() {
				self.rebuild_aspect_metadata(&aspect).await?;
				backfilled += 1;
			}
		}
		Ok(backfilled)
	}
}
