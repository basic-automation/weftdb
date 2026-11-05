//! Segment-store implementation: this submodule holds the `open` portion of [`SegmentStore`](super::SegmentStore).
//! Pure move of the former `segment_store.rs` `impl` block; no behaviour change.

use super::*;

impl SegmentStore {
	/// Open (creating if absent) a segment store rooted at `root` under the `default`
	/// database/subject namespace, with [`SegmentStoreOptions::default`].
	///
	/// Use [`open_scoped`](SegmentStore::open_scoped) to place the store's declared
	/// schemas under a named `(database, subject)` instead, and
	/// [`open_with_options`](SegmentStore::open_with_options) to configure it.
	///
	/// # Errors
	///
	/// As [`open_scoped`](SegmentStore::open_scoped).
	pub async fn open(root: impl AsRef<Path>) -> Result<Self> {
		Self::open_scoped(root, DEFAULT_DATABASE, DEFAULT_SUBJECT).await
	}

	/// [`open`](SegmentStore::open), configured by `options` rather than the defaults.
	///
	/// # Errors
	///
	/// As [`open_scoped`](SegmentStore::open_scoped).
	pub async fn open_with_options(root: impl AsRef<Path>, options: SegmentStoreOptions) -> Result<Self> {
		Self::open_scoped_with_options(root, DEFAULT_DATABASE, DEFAULT_SUBJECT, options).await
	}

	/// Open (creating if absent) a segment store rooted at `root` whose declared aspect
	/// schemas live under the `(database, subject)` namespace, with
	/// [`SegmentStoreOptions::default`].
	///
	/// The segment files and index rows are keyed by the flat aspect name, which is
	/// unique within one subject; the aspect-schema catalog records the full
	/// `(database, subject, aspect)` triple so a reopened store recovers the encoding
	/// it sealed under.
	///
	/// The open follows design section 5.5, as the freeze design (§4.3, FRE-12a) amends
	/// it:
	///
	/// 0. refuse to open at all, touching nothing, while the process is write-poisoned
	///    ([`Poisoned`]; see [`poison_global`]): the open writes;
	/// 1. create the root if it is missing (a new store);
	/// 2. take the root's `LOCK`, before anything else of the root is read or written, so
	///    one store (in one process) owns it;
	/// 3. read the root's `STORE_FORMAT` marker. A root without one and without
	///    control-plane databases is a new store, and its marker is written before
	///    anything else is created;
	/// 4. refuse the store, having touched only `LOCK` and `LOCK.holder`, when its
	///    `min_write_layout` is newer than [`SUPPORTED_LAYOUT`]
	///    ([`StoreError::IncompatibleLayout`]): the marker's before any database opens, and
	///    for a root without a marker the copy in `segment_index.db`'s `store_meta`, read
	///    before anything is written to it (step 7). A root without a marker is judged by
	///    that copy throughout: a lost marker is rebuilt from it, and a store a pre-marker
	///    WeftDB wrote, which has none, is layout 1. A newer layout is also refused when
	///    the root holds no database ([`StoreError::NewerStoreWithoutDatabases`]);
	/// 5. raise the marker's floors to what the pending migrations need, fsynced, before
	///    any of them runs, so an older WeftDB sees them even if a migration dies half
	///    way; for an existing store, only once every pending migration's precheck has
	///    passed (step 8), so a store one refuses is left without a raised marker;
	/// 6. create `segments/`;
	/// 7. open the four control-plane databases with no DDL, each refusing to open unless
	///    it runs MVCC and a new connection syncs FULL, and each syncing its MVCC header:
	///    a new store's here, an existing store's right after the marker's gate (4),
	///    `segment_index.db` first;
	/// 8. run the prechecks of the registered migrations `store_migrations` does not list,
	///    then the migrations themselves ([`OpenReport::applied`]), unless the store's
	///    layout is newer than this build's, in which case nothing is migrated and recovery
	///    is report-only ([`OpenReport::recovery_report_only`]);
	/// 9. mirror the marker into `store_meta`, then write the marker in its final form,
	///    with `last_written_layout` set to this build's layout;
	/// 10. fsync `segments/`, the root, any directory this open created above it and the
	///     root's parent, so the database files and their `-log` files keep their
	///     directory entries through a power cut (Turso truncates a `-log` in place, so
	///     one fsync covers it for good);
	/// 11. register the store's `(database, subject)` scope in one catalog transaction,
	///     after the fsyncs, so its commit lands in a `-log` whose entry is already
	///     durable;
	/// 12. replay the frame journal (design section 6, R6; slice S8), unless the layout is
	///     newer than this build's: unlink every maintenance output a crash left before
	///     its swap and every frame a swap retired that no live row references, fsync
	///     `segments/`, and delete the journal rows, so the store starts with an empty
	///     journal.
	/// 13. backfill the rollup rows the journal replay did not write (D-S11): a pre-S11
	///     store has no `segment_index.db` rows, and the replay only re-derives the
	///     aspects its journal names, so a clean pre-S11 store would otherwise read
	///     default zeros until its next seal or swap.
	/// 14. run startup recovery (D-S12, [`SegmentStore::fsck`]): classify every
	///     indexed frame and every orphan under `segments/`, quarantine the
	///     corrupt and the suspect, re-bind rows to their frames' keys, seed
	///     `next_series_id` (R10) and purge TTL-expired corrupt records — unless
	///     the layout is newer than this build's, in which case the same pass runs
	///     report-only: it counts what it finds and changes nothing.
	///
	/// The lock is held until the store drops.
	///
	/// # Errors
	///
	/// [`Poisoned`] while the process is write-poisoned; a [`StoreLocked`] error if the
	/// root is already open, in another process or in this one; a [`StoreError`] when the
	/// marker is unreadable, the store is too new to write, or a layout-1 index records
	/// frames outside `segments/`; an error if a database cannot run MVCC or does not sync
	/// FULL, if a migration fails, or if a directory fsync fails; and any filesystem error
	/// creating the layout or libSQL failure opening the databases.
	pub async fn open_scoped(root: impl AsRef<Path>, database: &str, subject: &str) -> Result<Self> {
		Self::open_scoped_with_options(root, database, subject, SegmentStoreOptions::default()).await
	}

	/// [`open_scoped`](SegmentStore::open_scoped), configured by `options`.
	///
	/// # Errors
	///
	/// As [`open_scoped`](SegmentStore::open_scoped).
	pub async fn open_scoped_with_options(root: impl AsRef<Path>, database: &str, subject: &str, options: SegmentStoreOptions) -> Result<Self> {
		Self::open_on(Arc::new(RealFs), root.as_ref(), database, subject, options).await
	}

	/// [`open_scoped_with_options`](SegmentStore::open_scoped_with_options), with its
	/// marker writes and directory fsyncs going through `fs`, so a test can see what the
	/// open writes and syncs, fail a step, or cut the power there.
	pub(crate) async fn open_on(fs: Arc<dyn StoreFs>, root: &Path, database: &str, subject: &str, options: SegmentStoreOptions) -> Result<Self> {
		let root = root.to_path_buf();
		let segments_dir = root.join("segments");
		let opening = || format!("opening segment store {}", root.display());
		// 0. A write-poisoned process opens no store: the open writes (the marker, the
		// migrations, store_meta, the scope), through the engine the poison distrusts.
		if let Some(poisoned) = process_poisoned() {
			return Err(anyhow::Error::new(poisoned).context(opening()));
		}
		// 1. The root, and any missing ancestor, for a new store.
		let created = missing_dirs(&segments_dir).await;
		tokio::fs::create_dir_all(&root).await.with_context(|| format!("creating store root {}", root.display()))?;
		// 2. LOCK.
		let root_lock = lock_root(&root).await?;
		// 3. The marker.
		let found = store_format::read_marker(&root).await.map_err(anyhow::Error::new).with_context(opening)?;
		let had_databases = control_plane_present(&root).await;
		let scope = StoreScope { database: database.to_string(), subject: subject.to_string() };
		let names = CONTROL_PLANE_FILES.map(|file| root.join(file).to_string_lossy().into_owned());
		// 4. The marker's gate, before any database is opened: nothing but LOCK and
		// LOCK.holder has been touched.
		if let Some(marker) = found.as_ref().filter(|marker| !marker.writable_by(SUPPORTED_LAYOUT)) {
			return Err(anyhow::Error::new(StoreError::IncompatibleLayout { min_write: marker.min_write_layout, supported: SUPPORTED_LAYOUT }).context(opening()));
		}
		// 4a. A root with no marker is not necessarily an empty directory: the 0.1.x line was
		// a data directory with one database per aspect, and its top-level database carries
		// this build's control-plane name. Opening it as a store would build the rest of a
		// store beside that data and report it empty with no error, so it is refused here,
		// before any database opens and before any file of ours exists in the directory.
		if found.is_none() {
			let aspects = store_format::rows_mode_aspects(&root).await;
			if !aspects.is_empty() {
				return Err(anyhow::Error::new(StoreError::RowsModeDataDirectory { aspects }).context(opening()));
			}
		}
		// 7, for a root that has databases: they open now, before anything is written to the
		// root. segment_index.db opens first and refuses a write floor its store_meta records
		// newer than this build (the gate of a root without a marker, 4) before anything is
		// written to it; its store_meta copy stands in for a lost marker; and the pending
		// migrations' prechecks read them.
		let opened = if had_databases { Some(ControlPlaneDbs::open(&names).await?) } else { None };
		let mut format = match (&found, &opened) {
			(Some(format), _) => format.clone(),
			(None, Some(databases)) => recorded_format(&databases.index, &names[0], scope.clone()).await?.unwrap_or_else(|| StoreFormat::legacy(scope)),
			(None, None) => StoreFormat::new_store(scope),
		};
		let layout_before = format.layout_version;
		let created_new = !had_databases || found.as_ref().is_some_and(StoreFormat::is_unfinished_creation);
		// A marker rebuilt from store_meta passed the index's check already; this holds the
		// gate for every format the open goes on with.
		if !format.writable_by(SUPPORTED_LAYOUT) {
			return Err(anyhow::Error::new(StoreError::IncompatibleLayout { min_write: format.min_write_layout, supported: SUPPORTED_LAYOUT }).context(opening()));
		}
		let newer = format.layout_version > SUPPORTED_LAYOUT;
		if newer && !had_databases {
			return Err(anyhow::Error::new(StoreError::NewerStoreWithoutDatabases { layout: format.layout_version, supported: SUPPORTED_LAYOUT }).context(opening()));
		}
		// The pending migrations' prechecks, before anything is written; for a new store,
		// whose databases do not exist yet, at step 8, before any migration applies.
		let prechecked = match (&opened, newer) {
			(Some(databases), false) => {
				let done = migrations::applied(migrations::Db { db: databases.index.database(), name: &names[0] }).await?;
				let pending = migrations::pending(migrations::REGISTRY, &done);
				migrations::precheck(&databases.control_plane(&names), &pending).await.with_context(opening)?;
				Some((done, pending))
			}
			_ => None,
		};
		let mut on_disk = found;
		// 5. Floors first, by what the marker says is still to run.
		if !newer {
			let expected = migrations::after(migrations::REGISTRY, format.applied_through.as_deref());
			format = raised_for(&format, &expected);
			if on_disk.as_ref() != Some(&format) {
				store_format::write_marker(fs.as_ref(), &root, &format).await.with_context(opening)?;
				on_disk = Some(format.clone());
			}
		}
		// 6. segments/.
		tokio::fs::create_dir_all(&segments_dir).await.with_context(|| format!("creating segments dir {}", segments_dir.display()))?;
		// 7. The databases of a new store: pragmas and probes, no DDL.
		let databases = match opened {
			Some(databases) => databases,
			None => ControlPlaneDbs::open(&names).await?,
		};
		let cp = databases.control_plane(&names);
		// 8. The migrations the store has not applied, their prechecks first, and its floors
		// raised for any the marker did not expect.
		let mut applied = Vec::new();
		if !newer {
			let (done, pending) = if let Some(prechecked) = prechecked {
				prechecked
			} else {
				let done = migrations::applied(migrations::Db { db: databases.index.database(), name: &names[0] }).await?;
				let pending = migrations::pending(migrations::REGISTRY, &done);
				migrations::precheck(&cp, &pending).await.with_context(opening)?;
				(done, pending)
			};
			format = raised_for(&format, &pending);
			if on_disk.as_ref() != Some(&format) {
				store_format::write_marker(fs.as_ref(), &root, &format).await.with_context(opening)?;
				on_disk = Some(format.clone());
			}
			applied = migrations::apply(&cp, &pending).await?;
			if format.last_written_layout < format.layout_version {
				let mut all = done;
				all.extend(applied.iter().map(|id| (*id).to_string()));
				migrations::rederive(&cp, migrations::REGISTRY, &all, format.last_written_layout).await?;
			}
		}
		// 9 and 10. The store_meta mirror, then the final marker.
		let store_uuid = databases.index.meta("store_uuid").await.with_context(|| format!("{}: reading the store UUID", names[0]))?.unwrap_or_else(|| format.store_uuid.clone());
		let settled = settled_format(&format, newer, store_uuid.clone());
		mirror_into_store_meta(&databases.index, &names[0], &settled).await?;
		if on_disk.as_ref() != Some(&settled) {
			store_format::write_marker(fs.as_ref(), &root, &settled).await.with_context(opening)?;
		}
		// 11. The layout's directory entries, then the scope.
		sync_layout(fs.as_ref(), &root, &segments_dir, &created).await?;
		// Record this store's place in the hierarchy so the control plane can enumerate
		// the databases/subjects a root holds (idempotent, and atomic).
		databases.registry.register_scope(database, subject).await?;
		let migrated_from = (!created_new && layout_before < settled.layout_version).then_some(layout_before);
		let report = OpenReport { created_new, migrated_from, applied: applied.iter().map(|id| (*id).to_string()).collect(), recovery_report_only: newer };
		if newer {
			tracing::warn!(root = %root.display(), layout = settled.layout_version, supported = SUPPORTED_LAYOUT, "this store's layout is newer than this WeftDB's; it opened without migrating, and recovery only reports what it finds");
		}
		let floor_ensured = AtomicU32::new(settled.min_read_layout.min(settled.min_write_layout));
		let ControlPlaneDbs { index, metadata, catalog, registry } = databases;
		let reaper = Reaper::new(fs.clone(), segments_dir);
		let replaying = opening();
		// The previous process's clean-shutdown flag, as this open finds it, before
		// this open resets it: a later crash must not inherit a clean verdict.
		let previous_clean_shutdown = index.meta("clean_shutdown").await.with_context(|| format!("{}: reading the previous clean-shutdown flag", names[0]))?.as_deref() == Some("1");
		let store = Self { root, index, metadata, catalog, registry, database: database.to_string(), subject: subject.to_string(), checkpoints: options.checkpoints, partials: options.partials, transposed: options.transposed, poison: StorePoison::default(), fs, format: RwLock::new(settled), marker_writer: tokio::sync::Mutex::new(()), floor_ensured, store_uuid, report, recovery: RwLock::new(RecoveryReport::default()), suspect_present: AtomicBool::new(false), previous_clean_shutdown, locks: AspectLocks::default(), legacy_ids: tokio::sync::OnceCell::new(), frame_gens: tokio::sync::OnceCell::new(), reaper, maintenance_wait: DEFAULT_MAINTENANCE_WAIT, maintenance_breaker: options.breaker, max_series_per_aspect: options.max_series_per_aspect, max_downsample_buckets: options.max_downsample_buckets, max_point_batch: options.max_point_batch, breaker_state: Mutex::new(HashMap::new()), index_conflicts: AtomicU64::new(0), _root_lock: root_lock };
		// 12. The frame journal's replay (R6), before anything is served: unless recovery
		// must only report what it finds, a newer WeftDB's store.
		// 13. Backfill the rollup rows the journal replay did not write: a pre-S11
		// store has none, and the replay only re-derives the aspects its journal
		// names, so without this a clean pre-S11 store would read default zeros.
		if !newer {
			store.replay_journal().await.context(replaying)?;
			store.backfill_missing_rollups().await.context("backfilling the rollup rows the journal replay did not write")?;
			// This open has not shut down cleanly yet: say so now, so a crash
			// before any clean shutdown is never mistaken for one.
			store.commit_index(&IndexTxn::new(vec![IndexOp::MetaSet { key: "clean_shutdown".to_string(), value: "0".to_string() }])).await.context("recording that this open has not shut down cleanly yet")?;
		}
		// 14. Startup recovery, report-only exactly when the layout is newer.
		// A root carrying a restore intent marker (D-S16, C-7) is opened in
		// restore mode instead: its orphans are adopted via describe_frame,
		// and the marker is removed once that recovery completes, so the
		// restore is a one-open event. A newer layout still only reports.
		let intent = if newer { None } else { crate::read_restore_intent(&store.root).await.context("reading the restore intent marker")? };
		match intent {
			Some(_) => {
				store.fsck_restore().await.context("running restore-mode recovery")?;
				let marker = store.root.join(crate::RESTORE_INTENT_FILE);
				if let Err(e) = store.fs.remove_file(&marker).await {
					tracing::warn!(root = %store.root.display(), error = %e, "could not remove the restore intent marker after restore-mode recovery; the next open runs it again (adoption is idempotent: adopted frames have rows then)");
				} else {
					store.fs.sync_dir(&store.root).await.context("syncing the restore intent marker's removal")?;
				}
			}
			None => {
				store.fsck(newer).await.context("running startup recovery")?;
			}
		}
		Ok(store)
	}

	/// What this store's open found and did (release plan C-5).
	#[must_use]
	pub const fn open_report(&self) -> &OpenReport {
		&self.report
	}

	/// The store's identity: the `store_uuid` of its `STORE_FORMAT` marker, mirrored in
	/// `store_meta`. It survives reopens, relocations and restores of the root.
	#[must_use]
	pub fn store_uuid(&self) -> &str {
		&self.store_uuid
	}

	/// The store's `STORE_FORMAT` marker, as this store last wrote it.
	#[must_use]
	pub fn store_format(&self) -> StoreFormat {
		self.format.read().unwrap_or_else(PoisonError::into_inner).clone()
	}

	/// Make sure the store's floors are at least `level` before this store writes
	/// something only a WeftDB that knows layout `level` may read (freeze design §4.3).
	/// The `STORE_FORMAT` marker is raised first and fsynced, so an older WeftDB refuses
	/// the store from then on even if the write never commits; the op this returns raises
	/// `store_meta`'s copy and belongs in the caller's own transaction. After the first
	/// call for a level nothing is written and `None` is returned: the marker, which the
	/// open's gate reads, already holds the floor, and the next open mirrors it into
	/// `store_meta` if the transaction that carried the op did not commit.
	///
	/// # Errors
	///
	/// A `level` newer than [`SUPPORTED_LAYOUT`], or a failed marker write.
	#[cfg_attr(not(test), expect(dead_code, reason = "the first writer of a post-1.0 frame feature (D-S10 on; the write-once maintenance of D-S8 and D-S9 writes only frames a layout-2 reader reads) calls it; until then only the tests do"))]
	pub(crate) async fn ensure_floor(&self, level: u32) -> Result<Option<IndexOp>> {
		if level > SUPPORTED_LAYOUT {
			bail!("cannot raise the store's floors to layout {level}: this WeftDB writes layouts up to {SUPPORTED_LAYOUT}");
		}
		if self.floor_ensured.load(Ordering::Acquire) >= level {
			return Ok(None);
		}
		let _writer = self.marker_writer.lock().await;
		let current = self.store_format();
		if current.min_read_layout < level || current.min_write_layout < level {
			let raised = StoreFormat { layout_version: current.layout_version.max(level), min_read_layout: current.min_read_layout.max(level), min_write_layout: current.min_write_layout.max(level), ..current };
			store_format::write_marker(self.fs.as_ref(), &self.root, &raised).await.context("raising the store's floors")?;
			*self.format.write().unwrap_or_else(PoisonError::into_inner) = raised;
		}
		self.floor_ensured.fetch_max(level, Ordering::AcqRel);
		Ok(Some(IndexOp::RaiseFloor { level }))
	}

	/// The store's write poison, or `None` while it accepts writes.
	///
	/// A store poisons itself when a `segment_index.db` COMMIT fails in a way that may
	/// still have committed, because that transaction may or may not be durable (see
	/// [`Poisoned`]); a conflict Turso finds while validating the commit is not one of
	/// them, since it rolls the transaction back before writing anything. Every store is
	/// also poisoned once the process is ([`poison_global`]: a panic inside a
	/// control-plane write), and then no store opens. From then on every write fails with
	/// that [`Poisoned`] error and reads go on working, until the process restarts.
	/// `weft-server`'s `/ready` reports it as `poisoned` and `restart_required`.
	#[must_use]
	pub fn poisoned(&self) -> Option<Poisoned> {
		self.poison.get().or_else(process_poisoned)
	}

	/// A watch of this store's own write poison: `None` until an ambiguous COMMIT
	/// poisons it, then the [`Poisoned`] error its writes fail with. The process-wide
	/// poison has its own watch ([`GlobalPoison::subscribe`](crate::durable::GlobalPoison::subscribe));
	/// [`wait_until_poisoned`](Self::wait_until_poisoned) waits for either.
	#[must_use]
	pub fn subscribe_poison(&self) -> tokio::sync::watch::Receiver<Option<Poisoned>> {
		self.poison.tx.subscribe()
	}

	/// Wait until this store is write-poisoned, by its own ambiguous COMMIT or by the
	/// process-wide poison, and return the poison ([`poisoned`](Self::poisoned)). At once
	/// if it already is.
	///
	/// The library never exits the process (release plan C-1); an embedder that wants to
	/// exit on poison, as `weft-server` does under `WEFT_ON_AMBIGUOUS_COMMIT=exit`, waits
	/// here and exits itself.
	pub async fn wait_until_poisoned(&self) -> Poisoned {
		let mut own = self.subscribe_poison();
		let mut global = poison_global().subscribe();
		loop {
			if let Some(poisoned) = self.poisoned() {
				return poisoned;
			}
			// Both senders outlive this call (the store's is borrowed, the process's is
			// static), so neither wait can fail.
			tokio::select! {
				_ = own.changed() => {}
				_ = global.changed() => {}
			}
		}
	}

	/// Write-poison the store for `reason`, as an ambiguous COMMIT would, without staging
	/// one. The hook for tests of the layers above (`weft-server`'s `/ready`), which
	/// cannot reach the store's commit path; only `fault-injection` builds have it.
	#[cfg(feature = "fault-injection")]
	#[doc(hidden)]
	pub fn inject_poison(&self, reason: impl Into<String>) {
		self.poison.set(reason.into());
	}

	/// Refuse a write while the store, or the process, is poisoned. Every write entry
	/// point calls this first, before it reads or writes anything.
	pub(crate) fn writable(&self) -> Result<()> {
		match self.poisoned() {
			Some(poisoned) => Err(poisoned.into()),
			None => Ok(()),
		}
	}

	/// How long a per-aspect maintenance entry point waits for an aspect another
	/// maintenance operation holds before it fails with [`MaintenanceBusy`]
	/// ([`DEFAULT_MAINTENANCE_WAIT`] unless changed with
	/// [`with_maintenance_wait`](Self::with_maintenance_wait)).
	#[must_use]
	pub const fn maintenance_wait(&self) -> Duration {
		self.maintenance_wait
	}

	/// Override how long a per-aspect maintenance entry point waits for a busy aspect; see
	/// [`maintenance_wait`](Self::maintenance_wait). `Duration::ZERO` makes it take the
	/// aspect only if it is free.
	#[must_use]
	pub const fn with_maintenance_wait(mut self, wait: Duration) -> Self {
		self.maintenance_wait = wait;
		self
	}

	/// The per-aspect circuit breaker this store's store-wide maintenance sweeps
	/// run under (ROB-19): from the open options
	/// ([`SegmentStoreOptions::breaker`], [`MaintenanceBreaker::default`] unless
	/// changed), or [`with_maintenance_breaker`](Self::with_maintenance_breaker).
	#[must_use]
	pub const fn maintenance_breaker(&self) -> MaintenanceBreaker {
		self.maintenance_breaker
	}

	/// Override the maintenance circuit breaker policy; see
	/// [`maintenance_breaker`](Self::maintenance_breaker). A
	/// `failure_threshold` of 0 disables the breaker. Breaker state accumulated
	/// under the old policy is kept: counts and cooldowns already in force stand.
	#[must_use]
	pub const fn with_maintenance_breaker(mut self, breaker: MaintenanceBreaker) -> Self {
		self.maintenance_breaker = breaker;
		self
	}

	/// Whether `aspect`'s maintenance circuit breaker is currently tripped: a
	/// store-wide sweep would skip it until the cooldown ends. Always `false`
	/// when the breaker is disabled or the aspect has never failed a sweep pass.
	#[must_use]
	pub fn breaker_tripped(&self, aspect: &str) -> bool {
		if self.maintenance_breaker.failure_threshold == 0 {
			return false;
		}
		let state = self.breaker_state.lock().unwrap_or_else(PoisonError::into_inner);
		state.get(aspect).and_then(|aspect_state| aspect_state.open_until).is_some_and(|until| until > Instant::now())
	}

	/// The open's replay of the frame journal (design section 6, R6, in its minimal S8
	/// form): every pending output no row references is unlinked (its swap never
	/// committed) and every retired frame no row references (its swap did), never a frame
	/// a live row references; `segments/` is fsynced once; the processed rows are deleted
	/// in one commit. Nothing can be reading yet, so no pin is consulted. An aspect whose
	/// retired frames it processed had a swap commit, whose transaction already folded
	/// its rollup in (S11); the rollup is still re-derived here, as a no-op
	/// reconciliation for those aspects, so a rollup that predates S11 (or was
	/// clobbered) is repaired at open. An unlink that fails stays
	/// journaled for the reaper, and is logged.
	pub(crate) async fn replay_journal(&self) -> Result<()> {
		let entries = self.index.journal(None).await.context("reading the frame journal")?;
		if entries.is_empty() {
			return Ok(());
		}
		let mut live = HashMap::new();
		for entry in &entries {
			if !live.contains_key(&entry.aspect) {
				let rows = self.index.rows(&entry.aspect).await?;
				live.insert(entry.aspect.clone(), LiveFrames::of(&rows));
			}
		}
		let swept = self.reaper.sweep(&entries, &live, false).await;
		if swept.unlinked > 0 {
			self.sync_segments().await.context("making the journal replay's unlinks durable")?;
		}
		if !swept.processed.is_empty() {
			self.commit_index(&IndexTxn::new(swept.processed.iter().map(|name| IndexOp::ClearJournal { name: name.clone() }).collect())).await.context("deleting the replayed journal rows")?;
		}
		for aspect in &swept.swapped {
			let commit = self.commit_lock(aspect).await?;
			self.rebuild_rollup_locked(aspect, &commit).await.with_context(|| format!("rebuilding the rollup of {aspect:?} after the journal replay"))?;
		}
		let left = swept.deferred.len() + swept.failed.len();
		tracing::info!(root = %self.root.display(), rows = entries.len(), unlinked = swept.unlinked, left, "replayed the frame journal");
		Ok(())
	}

	/// Override this store's [`CheckpointPolicy`] (set at open from its
	/// [`SegmentStoreOptions`], [`CheckpointPolicy::DISABLED`] by default).
	#[must_use]
	pub const fn with_checkpoint_policy(mut self, policy: CheckpointPolicy) -> Self {
		self.checkpoints = policy;
		self
	}

	/// The checkpoint policy new seals are written under.
	#[must_use]
	pub const fn checkpoint_policy(&self) -> CheckpointPolicy {
		self.checkpoints
	}

	/// Override this store's [`TransposedPolicy`] (set at open from its
	/// [`SegmentStoreOptions`], [`TransposedPolicy::DISABLED`] by default).
	#[must_use]
	pub const fn with_transposed_policy(mut self, policy: TransposedPolicy) -> Self {
		self.transposed = policy;
		self
	}

	/// The transposed-value-codec policy new seals are written under.
	#[must_use]
	pub const fn transposed_policy(&self) -> TransposedPolicy {
		self.transposed
	}

	/// The [`FrameOptions`] a fresh seal of a `row_count`-row segment writes under, folding
	/// this store's checkpoint and transposed policies into the one options value both frame
	/// writers take. `benefits`/`codec_overhead` are the segment's checkpoint gates (see
	/// [`CheckpointPolicy::stride_for`]).
	pub(crate) fn frame_options(&self, row_count: usize, benefits: bool, codec_overhead: f64) -> FrameOptions {
		FrameOptions { checkpoint_stride: self.checkpoints.stride_for(row_count, benefits, codec_overhead), transposed_max_overhead: self.transposed.max_overhead }
	}

	/// Override this store's [`PartialSidecarPolicy`] (set at open from its
	/// [`SegmentStoreOptions`], [`PartialSidecarPolicy::DISABLED`] by default).
	#[must_use]
	pub const fn with_partial_sidecar_policy(mut self, policy: PartialSidecarPolicy) -> Self {
		self.partials = policy;
		self
	}

	/// The partial-sidecar policy new seals are written under.
	#[must_use]
	pub const fn partial_sidecar_policy(&self) -> PartialSidecarPolicy {
		self.partials
	}

	/// The control-plane index backing this store, for pruning/accounting queries
	/// ([`prune_by_time`](SegmentIndexStore::prune_by_time),
	/// [`load_index`](SegmentIndexStore::load_index), …).
	///
	/// For reads. Its writers ([`insert`](SegmentIndexStore::insert),
	/// [`delete`](SegmentIndexStore::delete)) bypass this store's write poison: they
	/// write even while it is [`poisoned`](Self::poisoned), and an ambiguous COMMIT in
	/// them does not poison it. Write through this store's own entry points.
	#[must_use]
	pub const fn index(&self) -> &SegmentIndexStore {
		&self.index
	}

	/// The aspect-schema catalog backing this store.
	///
	/// For reads. Its writer ([`declare`](AspectCatalog::declare)) bypasses this store's
	/// write poison; declare through [`SegmentStore::declare`] instead.
	#[must_use]
	pub const fn catalog(&self) -> &AspectCatalog {
		&self.catalog
	}

	/// The `metadata.db` store backing this store: since S11 the segment-set rollup
	/// lives in `segment_index.db` (see [`aspect_metadata`](Self::aspect_metadata)),
	/// and this store's seals and swaps no longer write it. What `metadata.db` still
	/// holds is the rows-mode legacy catalog; the rollup table it
	/// also carries is the pre-S11 home, kept by the migration registry but no longer
	/// written by the store.
	///
	/// For reads of that legacy catalog. Its writers
	/// ([`put`](AspectMetadataStore::put),
	/// [`record_seal`](AspectMetadataStore::record_seal),
	/// [`remove`](AspectMetadataStore::remove)) bypass this store's write poison; rebuild
	/// a rollup through [`rebuild_aspect_metadata`](Self::rebuild_aspect_metadata) instead.
	#[must_use]
	pub const fn metadata(&self) -> &AspectMetadataStore {
		&self.metadata
	}

	/// The DB/subject registry backing this store, for enumerating the databases and
	/// subjects a root holds ([`list_databases`](CatalogStore::list_databases),
	/// [`list_subjects`](CatalogStore::list_subjects)).
	#[must_use]
	pub const fn registry(&self) -> &CatalogStore {
		&self.registry
	}

	/// The database namespace this store's aspect schemas are declared under (the
	/// upper half of its `(database, subject)` scope — fixed at
	/// [`open_scoped`](SegmentStore::open_scoped), `default` for
	/// [`open`](SegmentStore::open)).
	#[must_use]
	pub fn database(&self) -> &str {
		&self.database
	}

	/// The subject namespace this store's aspect schemas are declared under (the
	/// lower half of its `(database, subject)` scope).
	#[must_use]
	pub fn subject(&self) -> &str {
		&self.subject
	}

	/// The store root — the directory holding `segments/` and the four control-plane
	/// DBs. The natural base for a default [`backup_control_plane`](SegmentStore::backup_control_plane)
	/// destination.
	#[must_use]
	pub fn root(&self) -> &Path {
		&self.root
	}

	/// The aspects [`declare`](SegmentStore::declare)d in this store's
	/// `(database, subject)` scope, in name order.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn list_declared_aspects(&self) -> Result<Vec<String>> {
		self.catalog.list_aspects(&self.database, &self.subject).await
	}

	/// Declare `aspect`'s [`AspectSchema`] in this store's catalog, so later
	/// [`seal_declared`](SegmentStore::seal_declared) calls need not be handed the
	/// schema. Declarations are immutable once made (FRE-7b): re-declaring the
	/// identical schema is a no-op, and the one change accepted is a widening
	/// of the value encoding through
	/// [`widen_value_encoding`](weft_physical_type::widen_value_encoding) (to
	/// `BigDecimalText`, tolerance and timestamp unit unchanged) — see
	/// [`AspectCatalog::declare`].
	///
	/// The name also names the aspect's segment files, so it must pass
	/// [`aspect_name::validate`] (no path separators, no leading `.`, no control
	/// characters, …); a name that does not is refused before anything is written.
	///
	/// # Errors
	///
	/// An [`InvalidAspectName`](crate::InvalidAspectName) when `aspect` is not a valid
	/// name; a [`StoreError::DeclarationConflict`] when the aspect is already
	/// declared under a schema this one does not equal or widen; otherwise
	/// propagates any libSQL write failure.
	pub async fn declare(&self, aspect: &str, schema: &AspectSchema) -> Result<()> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		self.catalog.declare(&self.database, &self.subject, aspect, schema).await
	}

	/// [`declare`](Self::declare) with an upsert-key declaration (roadmap Phase 4.6; see
	/// [`UpsertKeys`](crate::UpsertKeys)): `upsert_keys` [`None`] declares none, the
	/// default identity `(timestamp, full tag set)`. With keys declared, an
	/// [`ingest`](Self::ingest) whose tags name a key outside them is refused. The keys
	/// are immutable once declared and take part in the declaration's equality (see
	/// [`AspectCatalog::declare_with_upsert_keys`]); the
	/// [`AspectSchema`] is untouched by them.
	///
	/// # Errors
	///
	/// As [`declare`](Self::declare), and a [`StoreError::UpsertKeysConflict`] when the
	/// aspect is declared with other upsert keys (or none, or some where none are offered).
	pub async fn declare_with_upsert_keys(&self, aspect: &str, schema: &AspectSchema, upsert_keys: Option<&crate::UpsertKeys>) -> Result<()> {
		aspect_name::validate(aspect)?;
		self.writable()?;
		self.catalog.declare_with_upsert_keys(&self.database, &self.subject, aspect, schema, upsert_keys).await
	}

	/// The upsert keys `aspect` is declared with (roadmap Phase 4.6), or [`None`] when it
	/// declares none (or is not declared): its dedup identity is then the timestamp and
	/// the full tag set.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn upsert_keys(&self, aspect: &str) -> Result<Option<crate::UpsertKeys>> {
		self.catalog.upsert_keys(&self.database, &self.subject, aspect).await
	}

	/// The declared [`AspectSchema`] for `aspect`, or [`None`] if it has not been
	/// [`declare`](SegmentStore::declare)d in this store.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn schema_for(&self, aspect: &str) -> Result<Option<AspectSchema>> {
		self.catalog.get(&self.database, &self.subject, aspect).await
	}

	/// Record the 0.1.x catalog rows a rows-mode import carried, into this store's
	/// `metadata.db` (`legacy_catalog`, migration `0003_fre14_legacy_catalog`).
	/// Crate-private: only the rows-mode import writes it, and it writes exactly the
	/// rows the import read — one per migrated aspect, under this store's scope, so
	/// re-running it for the same import replaces the same rows rather than adding
	/// to them.
	///
	/// # Errors
	///
	/// Propagates any libSQL write failure.
	pub(crate) async fn record_legacy_catalog(&self, database: &str, subject: &str, aspects: &[crate::types::rows_mode::LegacyCatalogAspect]) -> Result<()> {
		self.writable()?;
		let conn = crate::types::durable::control_plane::connect(self.metadata.database()).await?;
		conn.execute("BEGIN CONCURRENT", turso::params![]).await?;
		let mut res = Ok(());
		for aspect in aspects {
			let text = |value: &Option<String>| value.clone().map_or(turso::Value::Null, turso::Value::Text);
			let created = aspect.created_at.map_or(turso::Value::Null, turso::Value::Integer);
			res = conn
				.execute(
					"INSERT OR REPLACE INTO legacy_catalog
					(database, subject, aspect, legacy_aspect_id, resolution, created_at, earliest_measurement, latest_measurement, compression_config)
					VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
					turso::params![database.to_string(), subject.to_string(), aspect.name.clone(), text(&aspect.legacy_aspect_id), text(&aspect.resolution), created, text(&aspect.earliest_measurement), text(&aspect.latest_measurement), text(&aspect.compression_config)],
				)
				.await
				.map(|_| ());
			if res.is_err() {
				break;
			}
		}
		match res {
			Ok(()) => {
				conn.execute("COMMIT", turso::params![]).await?;
				Ok(())
			}
			Err(e) => {
				conn.execute("ROLLBACK", turso::params![]).await.ok();
				Err(e).context("recording the legacy catalog rows")
			}
		}
	}

	/// The declared schema for `aspect`, or an error naming the aspect if it has not
	/// been declared.
	pub(crate) async fn require_schema(&self, aspect: &str) -> Result<AspectSchema> {
		self.schema_for(aspect).await?.ok_or_else(|| anyhow::Error::new(StoreError::UnknownAspect { aspect: aspect.to_string() }).context(format!("aspect {aspect:?} has no declared schema in {}/{}", self.database, self.subject)))
	}
}
