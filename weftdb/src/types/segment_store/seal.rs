//! Segment-store implementation: this submodule holds the `seal` portion of [`SegmentStore`](super::SegmentStore).
//! Pure move of the former `segment_store.rs` `impl` block; no behaviour change.

use super::*;

impl SegmentStore {
	/// Commit `txn` to the segment index, unless the store is poisoned. An ambiguous
	/// failure poisons the store before the error is returned, so no later write builds on
	/// it. The transaction runs inside the control-plane write scope
	/// ([`control_plane_write`](crate::exec::control_plane_write)).
	///
	/// The poison is checked again before every retry of a conflict, so a transaction
	/// waiting out a conflict while another writer poisons the store gives up with
	/// [`Poisoned`] instead of committing after it. An attempt already running when the
	/// poison is set is not stopped, and its COMMIT can still land: like every write
	/// entry point, the poison refuses what starts after it.
	///
	/// # Errors
	///
	/// [`Poisoned`] if the store already is, or became so while the transaction waited to
	/// retry a conflict; otherwise the transaction's [`IndexTxnError`].
	pub(crate) async fn commit_index(&self, txn: &IndexTxn) -> Result<TxnApplied> {
		self.writable()?;
		let result = self.index.apply_while(txn, || self.poisoned().is_none()).await;
		// Every attempt before the last lost a conflict, and so did the last one when the
		// conflict is what the transaction failed with.
		let lost = match &result {
			Ok(applied) => applied.attempts.saturating_sub(1),
			Err(e) if e.kind == TxnErrorKind::Retryable => e.attempts,
			Err(e) => e.attempts.saturating_sub(1),
		};
		if lost > 0 {
			self.index_conflicts.fetch_add(u64::from(lost), Ordering::Relaxed);
		}
		match result {
			Ok(applied) => Ok(applied),
			Err(e) => {
				if e.is_ambiguous() {
					self.on_ambiguous_commit(&e);
				} else if e.kind == TxnErrorKind::Retryable {
					// The conflict may have gone unretried because the store was poisoned
					// meanwhile; then the poison is the reason to report.
					self.writable()?;
				}
				Err(e.into())
			}
		}
	}

	/// Poison the store after an ambiguous COMMIT. The library never exits the process
	/// (release plan C-1): an embedder that wants that waits for the poison
	/// ([`wait_until_poisoned`](Self::wait_until_poisoned)).
	pub(crate) fn on_ambiguous_commit(&self, error: &IndexTxnError) {
		if self.poison.set(error.to_string()) {
			tracing::error!(root = %self.root.display(), %error, "a segment_index COMMIT is ambiguous; the store refuses writes until the process restarts, and reads go on");
		}
	}

	/// Take `aspect`'s commit lock, seeding its id allocator the first time.
	pub(crate) async fn commit_lock(&self, aspect: &str) -> Result<CommitGuard> {
		self.locks.commit(aspect, || self.seed_allocator(aspect)).await
	}

	/// Where `aspect`'s id allocator starts: above every id it can have used, which is the
	/// largest of its persisted `aspect_seq.next_id`, one past its largest indexed id, and
	/// one past the largest id a legacy-named file of it in `segments/` carries. The last
	/// covers a frame whose seal crashed before committing it, which nothing in the index
	/// records.
	pub(crate) async fn seed_allocator(&self, aspect: &str) -> Result<AspectState> {
		let seed = self.index.allocator_seed(aspect).await?;
		let legacy = self.legacy_ids.get_or_try_init(|| legacy_file_ids(self.root.join("segments"))).await?.get(aspect).copied();
		let past = |id: Option<u64>| id.map_or(0, |id| id.saturating_add(1));
		let next_id = seed.next_id.unwrap_or(0).max(past(seed.max_id)).max(past(legacy));
		Ok(AspectState { next_id, epoch: seed.epoch })
	}

	/// Commit a freshly sealed write-once frame's row under `aspect`'s commit lock, and
	/// return its descriptor with the id that commit assigned it: the id is handed out
	/// here, inside the commit, never before the frame was written, so a seal that
	/// crashes while writing its frame consumes no id and leaves no id-shaped litter
	/// behind — only its generation-named frame, which no later seal reuses. The row
	/// carries the frame's generation, its adoption order (`prec`, its own generation
	/// for a seal), its trailer CRC and the commit epoch, which a seal advances like a
	/// swap does; the persisted allocator is raised past both the id and the
	/// generation in the same transaction. Since S11 that transaction also folds the
	/// segment into the aspect's rollup row in `segment_index.db` (the rollup the
	/// transaction commits over, folded forward — never a second transaction, so a
	/// crash between the row insert and the fold cannot leave the rollup behind the
	/// index). The commit lock makes the read-fold-write atomic against every other
	/// writer of the aspect, and is held across the transaction's retries, which is
	/// what makes the fold retry-safe.
	pub(crate) async fn publish_seal(&self, aspect: &str, descriptor: &SegmentDescriptor, gen: u64, frame_crc: u32) -> Result<SegmentDescriptor> {
		let mut commit = self.commit_lock(aspect).await?;
		let id = commit.allocate()?;
		let epoch = commit.state().epoch.saturating_add(1);
		let descriptor = SegmentDescriptor { id, ..descriptor.clone() };
		let row = IndexRow { desc: descriptor.clone(), gen, prec: Some(gen), frame_crc: Some(frame_crc), commit_epoch: Some(epoch), series_id: 0 };
		let rollup = self.index.rollup(aspect).await?.unwrap_or_default().folded(&descriptor);
		// The seal's index write, spanned like `SegmentIndexStore::insert` (roadmap Phase 3). Its
		// `rows_changed` is the affected-row count of the first op, the `InsertNew`.
		let span = tracing::debug_span!("control_plane.index.insert", aspect, id = descriptor.id, rows_changed = tracing::field::Empty);
		let applied = self.commit_index(&IndexTxn::new(vec![IndexOp::InsertNew { aspect: aspect.to_string(), row }, seq_bump(aspect, &commit, gen.saturating_add(1), epoch), IndexOp::PutRollup { aspect: aspect.to_string(), meta: rollup }])).instrument(span.clone()).await?;
		span.record("rows_changed", applied.changes.first().copied().unwrap_or(0));
		commit.set_epoch(epoch);
		drop(commit);
		Ok(descriptor)
	}

	/// Where `aspect`'s generation counter starts: above every generation the aspect can
	/// have used, which is the largest of its persisted `aspect_seq.next_gen`, one past the
	/// largest generation among its rows, and one past the largest a write-once frame of it
	/// in `segments/` carries (an output a crashed maintenance operation wrote that the
	/// journal's replay could not remove). Never 0, the legacy generation.
	pub(crate) async fn seed_gen(&self, aspect: &str) -> Result<u64> {
		let seed = self.index.allocator_seed(aspect).await?;
		let segments = self.root.join("segments");
		let on_disk = self.frame_gens.get_or_try_init(|| frame_gens_on_disk(self.fs.as_ref(), &segments)).await?.get(&frame_name::stem(aspect)).copied();
		let past = |gen: Option<u64>| gen.map_or(0, |gen| gen.saturating_add(1));
		Ok(seed.next_gen.unwrap_or(0).max(past(seed.max_gen)).max(past(on_disk)).max(1))
	}

	/// Hand out `count` consecutive generations of `aspect`, returning the first: never
	/// held by any frame, and never handed out again, whether or not the caller commits
	/// them. Lock-free once the aspect's counter is seeded.
	pub(crate) async fn allocate_gens(&self, aspect: &str, count: u64) -> Result<u64> {
		self.locks.allocate_gens(aspect, count, || self.seed_gen(aspect)).await
	}

	/// The rollup the swap transaction of `ops` commits over: `rows` (the aspect's
	/// rows under the commit lock, so no other writer of the aspect can move them)
	/// with each row op applied — an insert adds its row, a replace swaps its row
	/// in, a delete removes it — then derived from the rows that remain. Journal,
	/// allocator and rollup ops carry no rows and are skipped. Deriving from the
	/// post-swap rows, rather than folding each output forward, keeps the rollup
	/// exact when a swap deletes members without replacing them (a squash that
	/// narrows an aspect) as well as when it replaces them.
	pub(crate) fn post_swap_rollup(rows: &[IndexRow], ops: &[IndexOp]) -> AspectMetadata {
		use std::collections::HashMap;
		let mut by_id: HashMap<u64, SegmentDescriptor> = rows.iter().map(|row| (row.desc.id, row.desc.clone())).collect();
		for op in ops {
			match op {
				IndexOp::InsertNew { row, .. } | IndexOp::Upsert { row, .. } => {
					by_id.insert(row.desc.id, row.desc.clone());
				}
				IndexOp::ReplaceExpected { row, .. } => {
					by_id.insert(row.desc.id, row.desc.clone());
				}
				IndexOp::DeleteExpected { expected, .. } => {
					by_id.remove(&expected.id);
				}
				IndexOp::DeleteEveryExpected { expected, .. } => {
					for version in expected {
						by_id.remove(&version.id);
					}
				}
				IndexOp::Delete { id, .. } => {
					by_id.remove(id);
				}
				_ => {}
			}
		}
		let mut index = SegmentIndex::new();
		let mut ids: Vec<u64> = by_id.keys().copied().collect();
		ids.sort_unstable();
		for id in ids {
			if let Some(desc) = by_id.remove(&id) {
				index.push(desc);
			}
		}
		AspectMetadata::from_index(&index)
	}

	/// The path of `aspect`'s frame file `name` directly under `segments/`: the one place
	/// a write-once frame name becomes a path. The aspect name is checked first, as at
	/// every name-to-path step, and then that the path is a direct child of `segments/`.
	pub(crate) fn output_path(&self, aspect: &str, name: &str) -> Result<PathBuf> {
		aspect_name::validate(aspect)?;
		contained_file(&self.root.join("segments"), name)
	}

	/// The file name `row`'s frame has directly under `segments/`, which a swap journals
	/// when it retires the frame. Refuses a row whose frame resolves anywhere else: the
	/// reaper unlinks by name in `segments/`, so it must be that file.
	pub(crate) fn retired_name(&self, row: &IndexRow) -> Result<String> {
		let path = self.frame_path(&row.desc)?;
		let segments = self.root.join("segments");
		match path.file_name().and_then(OsStr::to_str) {
			Some(name) if path.parent() == Some(segments.as_path()) => Ok(name.to_string()),
			_ => bail!("segment {} ({}) is not a frame directly inside {}, so maintenance cannot retire it", row.desc.id, row.desc.path, segments.display()),
		}
	}

	/// Write one planned output of `aspect` under its never-used name (design section 5.3,
	/// M4, `write_output`): encode it in its frame kind under `schema` on a blocking
	/// thread, then create the file with `create_new`, write it and `sync_all` it through
	/// one handle ([`write_new_durable`], `SyncPolicy::Full` in every durability mode).
	/// Returns its name, CRC and descriptor; nothing indexes it yet. Its directory entry
	/// becomes durable with the swap's one `segments/` fsync. The descriptor is the frame
	/// actually written's, so the row the swap commits carries that frame's
	/// `format_version` and statistics, not those of the member whose id it takes (a paged
	/// member merged into a single-block output; design window
	/// merge-paged-target-format-wedge).
	pub(crate) async fn write_output(&self, aspect: &str, schema: &AspectSchema, output: PlannedOutput) -> Result<WrittenOutput> {
		let PlannedOutput { gen, prec, timestamps, values, rows_per_page } = output;
		let name = frame_name::frame_name(aspect, gen, prec);
		let path = self.output_path(aspect, &name)?;
		let stored = path.to_string_lossy().into_owned();
		let schema = schema.clone();
		let encode = move || -> Result<EncodedOutput> {
			let (bytes, descriptor) = if let Some(rows_per_page) = rows_per_page {
				let segment = schema.seal_paged_nullable(&timestamps, &values, rows_per_page).map_err(|e| anyhow::anyhow!("sealing a maintenance output failed: {e}"))?;
				let bytes = segment.write_to();
				let descriptor = SegmentDescriptor::of_paged_segment(0, stored, bytes.len() as u64, &segment);
				(bytes, descriptor)
			} else {
				let segment = schema.seal_nullable(&timestamps, &values).map_err(|e| anyhow::anyhow!("sealing a maintenance output failed: {e}"))?;
				let bytes = segment.write_to();
				let descriptor = SegmentDescriptor::of_segment(0, stored, bytes.len() as u64, &segment);
				(bytes, descriptor)
			};
			Ok((bytes, descriptor, timestamps, values))
		};
		let (bytes, descriptor, timestamps, values) = tokio::task::spawn_blocking(encode).await.context("encoding a maintenance output")??;
		let points = WritePoints { created: None, written: Some(FaultPoint::MOutputWritten) };
		let frame_crc = write_new_durable(self.fs.as_ref(), &self.root.join("segments"), &name, bytes, SyncPolicy::Full, points).await.with_context(|| format!("writing maintenance output {}", path.display()))?;
		fault::hit(FaultPoint::MOutputSynced).await?;
		Ok(WrittenOutput { name, gen, prec, frame_crc, descriptor, timestamps, values })
	}

	/// Steps M3 and M4 of a swap of `aspect` (design section 5.3): journal every planned
	/// output's name as pending, in one commit that also raises the persisted generation
	/// counter past them; write and sync each output; fsync `segments/` once. An output a
	/// crash leaves on disk before its swap is in the journal, which the next open replays.
	/// The returned guard holds every journaled output until the swap settles it. A failure
	/// after the journal commit discards what was written before it returns, and leaves the
	/// rest to the reaper.
	pub(crate) async fn write_outputs(&self, aspect: &str, schema: &AspectSchema, planned: Vec<PlannedOutput>) -> Result<(Vec<WrittenOutput>, PendingOutputs<'_>)> {
		let names: Vec<String> = planned.iter().map(|output| frame_name::frame_name(aspect, output.gen, output.prec)).collect();
		let next_gen = planned.iter().map(|output| output.gen.saturating_add(1)).max().unwrap_or(0);
		// Under the commit lock, like every control-plane write of the aspect, so the
		// allocator row it raises never conflicts with a seal's.
		let commit = self.commit_lock(aspect).await?;
		let created_ms = now_ms();
		let mut ops: Vec<IndexOp> = names.iter().map(|name| IndexOp::JournalPending { aspect: aspect.to_string(), name: name.clone(), created_ms }).collect();
		let state = commit.state();
		ops.push(IndexOp::SeqBump { aspect: aspect.to_string(), next_id: state.next_id, next_gen, epoch: state.epoch });
		self.commit_index(&IndexTxn::new(ops)).await.context("journaling the maintenance outputs as pending")?;
		drop(commit);
		let mut pending = PendingOutputs { store: self, aspect: aspect.to_string(), names };
		let mut written = Vec::with_capacity(planned.len());
		let result = async {
			fault::hit(FaultPoint::MPendingCommitted).await?;
			for output in planned {
				written.push(self.write_output(aspect, schema, output).await?);
			}
			self.sync_segments().await?;
			fault::hit(FaultPoint::MDirSynced).await?;
			Ok::<(), anyhow::Error>(())
		}
		.await;
		if let Err(e) = result {
			// What this call wrote is unlinked and unjournaled now. An output whose write
			// failed removed its own file, and one never begun has none; the guard hands both
			// to the reaper when it drops here, which deletes their pending rows (and unlinks
			// what is somehow there), or the next open's replay does.
			let created: Vec<String> = written.iter().map(|output: &WrittenOutput| output.name.clone()).collect();
			pending.discard(&created).await;
			return Err(e);
		}
		Ok((written, pending))
	}

	/// Undo the outputs `names` of a swap of `aspect` that will not commit, every one of
	/// which the swap wrote or journaled: unlink them, fsync `segments/`, then delete their
	/// pending journal rows, in that order, so that a crash at any step leaves only what
	/// the journal's replay removes. Best effort: whatever fails is logged and left to the
	/// reaper or the replay. Returns whether every step succeeded.
	pub(crate) async fn discard_outputs(&self, aspect: &str, names: &[String]) -> bool {
		if names.is_empty() {
			return true;
		}
		for name in names {
			let removed = match self.output_path(aspect, name) {
				Ok(path) => self.fs.remove_file(&path).await.map_err(anyhow::Error::from),
				Err(e) => Err(e),
			};
			if let Err(e) = removed {
				tracing::warn!(aspect, frame = name, error = %e, "could not remove an abandoned maintenance output; the journal keeps it for the reaper");
				return false;
			}
		}
		if let Err(e) = self.sync_segments().await {
			tracing::warn!(aspect, error = %e, "could not sync the removal of abandoned maintenance outputs; the journal keeps them for the next open");
			return false;
		}
		let clear = IndexTxn::new(names.iter().map(|name| IndexOp::ClearJournal { name: name.clone() }).collect());
		if let Err(e) = self.commit_index(&clear).await {
			tracing::warn!(aspect, error = %e, "could not clear the journal rows of abandoned maintenance outputs; the reaper does");
			return false;
		}
		true
	}

	/// Swap one plan (see [`swap_plans`](Self::swap_plans)), returning its outputs' ids.
	///
	/// # Errors
	///
	/// As [`swap_plans`](Self::swap_plans), or the plan's own failure (a `Conflict` when a
	/// member changed since M1 read it, a refused fresh id).
	pub(crate) async fn swap_plan(&self, held: &MaintGuard, schema: &AspectSchema, plan: SwapPlan) -> Result<Swapped> {
		self.swap_plans(held, schema, vec![plan]).await?.into_iter().next().unwrap_or_else(|| Err(anyhow::anyhow!("a swap of aspect {:?} returned no outcome for its plan", held.aspect())))
	}

	/// Steps M2-M6 of a swap of `plans` of `held`'s aspect (design section 5.3): every plan's
	/// outputs are written once, as new frames, and swapped in by one `segment_index.db`
	/// transaction, which is the swap's commit point.
	///
	/// - M2: each output gets a fresh generation, and the largest adoption order (`prec`)
	///   of its plan's members (a legacy member counts as 0); M-planned is hit first.
	/// - M3, M4: every plan's outputs are journaled as pending in one commit, written,
	///   fsynced, and `segments/` fsynced ([`write_outputs`](Self::write_outputs)); a guard
	///   holds them until the swap settles them.
	/// - M5, under the aspect's commit lock: the outputs that take fresh ids (P2, see
	///   [`SwapPlan`]) are checked against the rows the swap commits over and allocated;
	///   then one transaction carries every plan's ops ([`PreparedPlan::ops`]) and one
	///   allocator raise. Preconditions are per member row: a `Conflict` rolls back every
	///   plan, and each is then retried alone (tags amendment A5), so one stale plan does
	///   not hold up the others.
	/// - M6: the outputs get their sidecars and the deleted
	///   members lose theirs, and the reaper takes the retired frames (the rollup was
	///   already folded into the swap's transaction at M5, S11).
	///
	/// Up to [`MAX_PLANS_PER_TXN`] plans, independent of each other (no member in two).
	/// Every caller today swaps one at a time; per-series maintenance batches them.
	///
	/// Returns each plan's outcome, in order. A plan refused or in conflict at M5 has its
	/// outputs discarded and fails alone.
	///
	/// # Errors
	///
	/// A failure that stops every plan: too many plans, a plan without members, members of
	/// different series, a member whose frame is not directly in `segments/`, a failure to
	/// journal, write or sync the outputs (those written are discarded), a failure of the
	/// joint transaction other than a `Conflict` (its outputs are discarded, or after an
	/// ambiguous COMMIT, which poisons the store, left to the next open), or an error
	/// injected at M-swapped.
	pub(crate) async fn swap_plans(&self, held: &MaintGuard, schema: &AspectSchema, plans: Vec<SwapPlan>) -> Result<Vec<Result<Swapped>>> {
		let aspect = held.aspect();
		if plans.len() > MAX_PLANS_PER_TXN {
			bail!("a swap of aspect {aspect:?} carries at most {MAX_PLANS_PER_TXN} plans, not {}", plans.len());
		}
		// Outputs an earlier swap of the aspect abandoned (a dropped guard) are settled first,
		// now that the aspect is held again.
		if self.reaper.has_abandoned(aspect) {
			if let Err(e) = self.reap_held(held).await {
				tracing::warn!(aspect, error = %e, "could not settle the outputs an earlier maintenance operation abandoned; the reaper tries again");
			}
		}
		let mut prepared = Vec::with_capacity(plans.len());
		let mut rows = Vec::with_capacity(plans.len());
		for SwapPlan { members, outputs } in plans {
			let Some(series_id) = members.first().map(|first| first.series_id) else { bail!("a swap plan of aspect {aspect:?} has no members") };
			// Tags I12: a maintenance output never folds rows of different series.
			if members.iter().any(|member| member.series_id != series_id) {
				bail!("a swap plan of aspect {aspect:?} mixes the series of segments {:?}", members.iter().map(|member| (member.desc.id, member.series_id)).collect::<Vec<_>>());
			}
			let retired = members.iter().map(|member| self.retired_name(member)).collect::<Result<Vec<String>>>()?;
			let prec = members.iter().map(|member| member.prec.unwrap_or(0)).max().unwrap_or(0);
			prepared.push(PreparedPlan { members, retired, series_id, prec, fresh: Vec::new(), outputs: Vec::new() });
			rows.push(outputs);
		}
		fault::hit(FaultPoint::MPlanned).await?;
		// M2: a run of fresh generations for every output, in plan and time order.
		let counts: Vec<usize> = rows.iter().map(Vec::len).collect();
		let count: usize = counts.iter().sum();
		let mut gen = self.allocate_gens(aspect, u64::try_from(count).unwrap_or(u64::MAX)).await?;
		let mut planned = Vec::with_capacity(count);
		for (outputs, plan) in rows.into_iter().zip(&prepared) {
			for PlanOutput { timestamps, values, rows_per_page } in outputs {
				planned.push(PlannedOutput { gen, prec: plan.prec, timestamps, values, rows_per_page });
				gen = gen.saturating_add(1);
			}
		}
		// M3, M4.
		let (written, mut pending) = self.write_outputs(aspect, schema, planned).await?;
		if written.len() != count {
			pending.discard_all().await;
			bail!("a swap of aspect {aspect:?} wrote {} outputs of {count}", written.len());
		}
		let mut written = written.into_iter();
		for (prepared, count) in prepared.iter_mut().zip(counts) {
			prepared.outputs = written.by_ref().take(count).collect();
		}
		// M5, under the commit lock, which every writer of the aspect's rows takes: the rows
		// read now are the ones the swap commits over.
		let mut commit = match self.commit_lock(aspect).await {
			Ok(commit) => commit,
			Err(e) => {
				pending.discard_all().await;
				return Err(e);
			}
		};
		let mut outcomes: Vec<Option<Result<Swapped>>> = prepared.iter().map(|_| None).collect();
		let committed = self.swap_in(aspect, &mut commit, &mut prepared, &mut pending, &mut outcomes).await?;
		let mut outputs = Vec::new();
		let mut deleted = Vec::new();
		for (i, plan) in prepared.iter_mut().enumerate().filter(|(i, _)| committed.contains(i)) {
			pending.swapped(&plan.output_names());
			let ids = plan.output_ids();
			deleted.extend(plan.deleted_frames());
			outcomes[i] = Some(Ok(Swapped { ids: ids.clone() }));
			outputs.extend(ids.into_iter().zip(std::mem::take(&mut plan.outputs)));
		}
		drop(pending);
		// M6.
		if committed.is_empty() {
			drop(commit);
		} else {
			self.finish_swap(held, commit, outputs, &deleted).await?;
		}
		Ok(outcomes.into_iter().map(|outcome| outcome.unwrap_or_else(|| Err(anyhow::anyhow!("a swap plan of aspect {aspect:?} has no outcome")))).collect())
	}

	/// Step M5 of [`swap_plans`](Self::swap_plans) for the written plans `prepared`, under
	/// the aspect's commit lock `commit`: allocate the fresh ids of the outputs that take
	/// them (P2) where no row the swap commits over outranks them, then commit every plan
	/// in one transaction or, after a `Conflict`, each alone. Returns the indexes of the
	/// plans that committed; a plan that failed on its own has its outputs discarded through
	/// `pending` and its error in `outcomes`.
	///
	/// # Errors
	///
	/// A failure that stops every plan: a failed read of the rows, an ambiguous COMMIT
	/// (which poisoned the store; the outputs are left to the next open), or another failure
	/// of the joint transaction (the outputs are discarded).
	pub(crate) async fn swap_in(&self, aspect: &str, commit: &mut CommitGuard, prepared: &mut [PreparedPlan], pending: &mut PendingOutputs<'_>, outcomes: &mut [Option<Result<Swapped>>]) -> Result<Vec<usize>> {
		let current = if prepared.iter().any(|plan| plan.extra() > 0) {
			match self.index.rows(aspect).await {
				Ok(rows) => Some(rows),
				Err(e) => {
					pending.discard_all().await;
					return Err(e);
				}
			}
		} else {
			None
		};
		let mut ready = Vec::with_capacity(prepared.len());
		for (i, plan) in prepared.iter_mut().enumerate() {
			let extra = plan.extra();
			let allowed = match &current {
				Some(rows) if extra > 0 => refuse_outranking_outputs(aspect, &plan.members, plan.outputs.iter().skip(plan.members.len()).map(|output| output.descriptor.time_range()), rows).and_then(|()| (0..extra).map(|_| commit.allocate()).collect::<Result<Vec<u64>>>()),
				_ => Ok(Vec::new()),
			};
			match allowed {
				Ok(fresh) => {
					plan.fresh = fresh;
					ready.push(i);
				}
				Err(e) => {
					pending.discard(&plan.output_names()).await;
					outcomes[i] = Some(Err(e));
				}
			}
		}
		if ready.is_empty() {
			return Ok(ready);
		}
		match self.commit_plans(aspect, commit, prepared, &ready).await {
			Ok(()) => Ok(ready),
			Err(e) if is_ambiguous(&e) => {
				pending.left_for_replay();
				Err(e)
			}
			Err(e) if is_conflict(&e) && ready.len() > 1 => {
				tracing::debug!(aspect, plans = ready.len(), error = %e, "a member of one plan changed under the swap; swapping each plan alone");
				let mut committed = Vec::with_capacity(ready.len());
				for i in ready {
					match self.commit_plans(aspect, commit, prepared, &[i]).await {
						Ok(()) => committed.push(i),
						Err(e) if is_ambiguous(&e) => {
							pending.left_for_replay();
							return Err(e);
						}
						Err(e) => {
							pending.discard(&prepared[i].output_names()).await;
							outcomes[i] = Some(Err(e));
						}
					}
				}
				Ok(committed)
			}
			Err(e) if is_conflict(&e) => {
				// The one plan's member changed since M1 read it.
				pending.discard_all().await;
				if let Some(&i) = ready.first() {
					outcomes[i] = Some(Err(e));
				}
				Ok(Vec::new())
			}
			Err(e) => {
				pending.discard_all().await;
				Err(e)
			}
		}
	}

	/// Step M5's transaction for the plans `which` of `prepared`, under the caller's
	/// commit lock: their ops, one allocator raise, and — since S11 — the rollup the
	/// post-swap rows derive, in aspect epoch `epoch + 1`, hitting `M-swap-begun`
	/// once it has begun and `M-swap-phantom` once COMMIT returned. **This is
	/// the swap's commit point.** Once it committed, the epoch is advanced and the retired
	/// frames are handed to the reclaim epochs. The rollup is computed from the rows
	/// the commit lock holds still (see [`post_swap_rollup`](Self::post_swap_rollup)),
	/// so it is the swap's own rows it summarizes, in the same transaction that
	/// commits them — never a rebuild after, which a crash could have stopped.
	pub(crate) async fn commit_plans(&self, aspect: &str, commit: &mut CommitGuard, prepared: &[PreparedPlan], which: &[usize]) -> Result<()> {
		let epoch = commit.state().epoch.saturating_add(1);
		let created_ms = now_ms();
		let mut ops = Vec::new();
		let mut next_gen = 0;
		for plan in which.iter().filter_map(|&i| prepared.get(i)) {
			plan.ops(aspect, epoch, created_ms, &mut ops);
			next_gen = plan.outputs.iter().map(|output| output.gen.saturating_add(1)).fold(next_gen, u64::max);
		}
		let current = self.index.rows(aspect).await.with_context(|| format!("reading the {aspect:?} rows a swap folds its rollup over"))?;
		let rollup = Self::post_swap_rollup(&current, &ops);
		ops.push(IndexOp::SeqBump { aspect: aspect.to_string(), next_id: commit.state().next_id, next_gen, epoch });
		ops.push(IndexOp::PutRollup { aspect: aspect.to_string(), meta: rollup });
		let txn = IndexTxn::new(ops).with_points(TxnPoints { begun: Some(FaultPoint::MSwapBegun), after_op: None, phantom: Some(FaultPoint::MSwapPhantom) });
		self.commit_index(&txn).await.with_context(|| format!("swapping the maintenance outputs of aspect {aspect:?} in"))?;
		commit.set_epoch(epoch);
		self.reaper.epochs().retire(which.iter().filter_map(|&i| prepared.get(i)).flat_map(|plan| plan.retired.iter().cloned()));
		Ok(())
	}

	/// What follows a swap's commit (design section 5.3, steps 4-6): the rollup is
	/// already folded into the swap's transaction (S11), so this releases the lock,
	/// refreshes the outputs' sidecars (each `(id, output)`), removes the
	/// sidecars beside the `deleted` members' frames, and hands the retired frames to the
	/// reaper. The swap is done whatever these do, so their failures are logged, not
	/// returned; only an error injected at `M-swapped` is.
	pub(crate) async fn finish_swap(&self, held: &MaintGuard, commit: CommitGuard, outputs: Vec<(u64, WrittenOutput)>, deleted: &[String]) -> Result<()> {
		let aspect = held.aspect();
		drop(commit);
		fault::hit(FaultPoint::MSwapped).await?;
		for (id, output) in outputs {
			let descriptor = SegmentDescriptor { id, ..output.descriptor };
			self.refresh_output_sidecar(aspect, &descriptor, output.gen, output.prec, output.frame_crc, output.timestamps, output.values).await;
		}
		for frame in deleted {
			if let Err(e) = self.remove_sidecar(frame).await {
				tracing::warn!(aspect, frame, error = %e, "could not remove the partial sidecar of a segment a merge deleted");
			}
		}
		if let Err(e) = self.reap_held(held).await {
			tracing::warn!(aspect, error = %e, "the reaper's pass after a maintenance swap failed; the retired frames stay journaled for the next pass");
		}
		Ok(())
	}

	/// Ingest `batch` into `aspect` — the idempotent atomic ingest entry
	/// point (1.0 plan §5, slice D-S13).
	///
	/// **Atomic.** The batch's segments, the allocator raise, the rollup
	/// fold and the ledger row commit in one `segment_index.db` transaction:
	/// the batch is in the store whole, or not at all. A batch too large for
	/// one encoded frame is first cut into segments at timestamp boundaries
	/// (see [`IngestBatch`]), and all of them travel in that same
	/// transaction.
	///
	/// **Idempotent.** Before anything is written, the batch's
	/// [`fingerprint`](IngestBatch::fingerprint) is looked up in
	/// `ingest_ledger` under `(aspect, batch.key)`. A recorded key with the
	/// same fingerprint is a replay: the recorded [`IngestReceipt`] is
	/// returned with `duplicate` set, and nothing is written again — the
	/// same batch twice commits once. A recorded key with a different
	/// fingerprint is a typed
	/// [`StoreError::IdempotencyConflict`]. Because the ledger row commits
	/// in the same transaction as the segments, a crash cannot leave one
	/// without the other; a replay after a restart answers from the ledger.
	///
	/// **Phases.** The ingest advances through [`IngestPhase`] as it goes,
	/// and sets [`IngestPhase::Committing`] just before its COMMIT, so a
	/// failure is attributable to the phase that raised it (plan C12/D21).
	/// Each advance is also mirrored into the task-local phase
	/// ([`crate::exec::set_ingest_phase`], ROB-3), so a panic caught by
	/// [`crate::exec::catch`] inside an
	/// [`ingest_phase_scope`](crate::exec::ingest_phase_scope) reports the
	/// phase it reached, not just the error path's context string.
	///
	/// # Errors
	///
	/// Typed [`StoreError`]s, recovered with `downcast_ref::<StoreError>()`:
	/// [`UnknownAspect`](StoreError::UnknownAspect) when `aspect` has no
	/// declared schema; [`GroupsUnsupported`](StoreError::GroupsUnsupported)
	/// for a non-empty `batch.groups`;
	/// [`EmptyIngestBatch`](StoreError::EmptyIngestBatch) for a batch with
	/// no rows; [`IngestLengthMismatch`](StoreError::IngestLengthMismatch)
	/// when the timestamp and value columns differ in length;
	/// [`IngestValueOutOfRange`](StoreError::IngestValueOutOfRange) for a
	/// value outside the decimal domain;
	/// [`IdempotencyConflict`](StoreError::IdempotencyConflict) for a
	/// different batch under a recorded key. Otherwise a
	/// [`SealError`](weft_physical_type::SealError) from encoding, a
	/// filesystem write error, or a libSQL index failure (an ambiguous
	/// COMMIT poisons the store, as for every write).
	pub async fn ingest(&self, aspect: &str, batch: &IngestBatch) -> Result<IngestReceipt> {
		self.writable()?;
		aspect_name::validate(aspect)?;
		let mut phase = IngestPhase::Validating;
		crate::exec::set_ingest_phase(phase);
		let schema = self.require_schema(aspect).await.map_err(|e| e.context(format!("ingest of aspect {aspect:?} failed while {phase}")))?;
		if !batch.groups.is_empty() {
			return Err(StoreError::GroupsUnsupported { aspect: aspect.to_string() }.into());
		}
		if batch.timestamps.len() != batch.values.len() {
			return Err(StoreError::IngestLengthMismatch { timestamps: batch.timestamps.len(), values: batch.values.len() }.into());
		}
		if batch.timestamps.is_empty() {
			return Err(StoreError::EmptyIngestBatch { aspect: aspect.to_string() }.into());
		}
		let fingerprint = batch.fingerprint()?;
		if let Some(entry) = self.index.ledger_entry(aspect, &batch.key).await? {
			if entry.fingerprint != fingerprint {
				return Err(StoreError::IdempotencyConflict { aspect: aspect.to_string(), key: batch.key.clone(), recorded_fingerprint: entry.fingerprint, offered_fingerprint: fingerprint }.into());
			}
			let mut receipt = IngestReceipt::from_json(&entry.receipt_json).with_context(|| format!("reading the recorded receipt of ingest key {:?} of aspect {aspect:?}", batch.key))?;
			receipt.duplicate = true;
			return Ok(receipt);
		}
		phase = IngestPhase::Encoding;
		crate::exec::set_ingest_phase(phase);
		// Encode the whole batch once, then cut the encoded run at
		// timestamp boundaries so no frame exceeds the target: the cut is
		// over rows, and each chunk re-seals from its own rows, so a chunk
		// boundary is a segment boundary and nothing else.
		let whole = encode_ingest_chunk(&schema, &batch.timestamps, &batch.values, batch.rows_per_page).map_err(|e| e.context(format!("ingest of aspect {aspect:?} failed while {phase}")))?;
		let chunks = if whole.write_to_with(&self.frame_options(whole.row_count(), false, 0.0)).len() as u64 <= crate::MAX_ENCODED_FRAME_BYTES {
			vec![(batch.timestamps.clone(), batch.values.clone())]
		} else {
			let mut chunks: Vec<(Vec<i64>, Vec<Option<BigDecimal>>)> = Vec::new();
			for (timestamps, values) in chunk_ingest_rows(&batch.timestamps, &batch.values) {
				// Prove each chunk's frame fits before any frame is
				// written: a chunk that still exceeds the target holds a
				// single timestamp run, the smallest cut there is.
				let segment = encode_ingest_chunk(&schema, &timestamps, &values, batch.rows_per_page)?;
				let bytes = segment.write_to_with(&self.frame_options(segment.row_count(), false, 0.0));
				if bytes.len() as u64 > crate::MAX_ENCODED_FRAME_BYTES && timestamps.len() > 1 {
					bail!("an ingest chunk of {} rows still encodes past the {}-byte frame target", timestamps.len(), crate::MAX_ENCODED_FRAME_BYTES);
				}
				chunks.push((timestamps, values));
			}
			chunks
		};
		phase = IngestPhase::WritingFrames;
		crate::exec::set_ingest_phase(phase);
		let mut written: Vec<(SegmentDescriptor, u64, u32, Vec<i64>, Vec<Option<BigDecimal>>)> = Vec::with_capacity(chunks.len());
		for (timestamps, values) in &chunks {
			let segment = encode_ingest_chunk(&schema, timestamps, values, batch.rows_per_page).map_err(|e| e.context(format!("ingest of aspect {aspect:?} failed while {phase}")))?;
			let bytes = segment.write_to_with(&self.frame_options(segment.row_count(), segment.benefits_from_checkpoints(), segment.checkpoint_codec_overhead()));
			let byte_len = bytes.len() as u64;
			let (gen, frame_crc, path) = self.write_seal_frame(aspect, bytes).await.with_context(|| format!("ingest of aspect {aspect:?} failed while {phase}"))?;
			if written.len() + 1 < chunks.len() {
				fault::hit(FaultPoint::SBetweenFrames).await?;
			}
			let descriptor = segment.descriptor(path.to_string_lossy().into_owned(), byte_len);
			written.push((descriptor, gen, frame_crc, timestamps.clone(), values.clone()));
		}
		phase = IngestPhase::Committing;
		crate::exec::set_ingest_phase(phase);
		let receipt = self.publish_ingest(aspect, batch, fingerprint, &written).await.with_context(|| format!("ingest of aspect {aspect:?} failed while {phase}"))?;
		// Per-segment partial sidecars (opt-in), as in `persist`: a pure
		// read accelerator, never failing the ingest.
		for (descriptor, gen, crc, timestamps, values) in &written {
			if self.partials.base_for(descriptor.row_count).is_some() {
				if let Err(e) = self.write_partial_sidecar(aspect, descriptor, *gen, *gen, *crc, timestamps.clone(), values.clone()).await {
					tracing::warn!(aspect, id = descriptor.id, error = %e, "failed to write partial sidecar; downsample will fall back to a full decode");
				}
			}
		}
		Ok(receipt)
	}

	/// Commit an ingest's written frames in one index transaction (see
	/// [`ingest`](Self::ingest)): one [`InsertNew`](IndexOp::InsertNew) per
	/// segment, the allocator raise, the rollup folded over every segment,
	/// and the [`RecordIngest`](IndexOp::RecordIngest) ledger row — under
	/// the aspect's commit lock, which is what makes the ledger check, the
	/// ids and the fold atomic against every other writer of the aspect.
	/// The caller has set its phase to
	/// [`IngestPhase::Committing`] before calling this.
	pub(crate) async fn publish_ingest(&self, aspect: &str, batch: &IngestBatch, fingerprint: i64, written: &[(SegmentDescriptor, u64, u32, Vec<i64>, Vec<Option<BigDecimal>>)]) -> Result<IngestReceipt> {
		let mut commit = self.commit_lock(aspect).await?;
		// The ledger is re-checked under the commit lock: a commit of the
		// same key that landed while this ingest wrote its frames is a
		// replay or a conflict, never a second commit. (The frames just
		// written stay behind as crash litter for recovery to classify,
		// exactly like a seal that crashes before its commit.)
		if let Some(entry) = self.index.ledger_entry(aspect, &batch.key).await? {
			if entry.fingerprint != fingerprint {
				return Err(StoreError::IdempotencyConflict { aspect: aspect.to_string(), key: batch.key.clone(), recorded_fingerprint: entry.fingerprint, offered_fingerprint: fingerprint }.into());
			}
			let mut receipt = IngestReceipt::from_json(&entry.receipt_json).context("reading the recorded ingest receipt")?;
			receipt.duplicate = true;
			return Ok(receipt);
		}
		let epoch = commit.state().epoch.saturating_add(1);
		let mut ops: Vec<IndexOp> = Vec::with_capacity(written.len() + 3);
		let mut descriptors: Vec<SegmentDescriptor> = Vec::with_capacity(written.len());
		let mut max_gen = 0u64;
		for (descriptor, gen, frame_crc, _ts, _vs) in written {
			let id = commit.allocate()?;
			let descriptor = SegmentDescriptor { id, ..descriptor.clone() };
			max_gen = max_gen.max(*gen);
			ops.push(IndexOp::InsertNew { aspect: aspect.to_string(), row: IndexRow { desc: descriptor.clone(), gen: *gen, prec: Some(*gen), frame_crc: Some(*frame_crc), commit_epoch: Some(epoch), series_id: 0 } });
			descriptors.push(descriptor);
		}
		let mut rollup = self.index.rollup(aspect).await?.unwrap_or_default();
		for descriptor in &descriptors {
			rollup = rollup.folded(descriptor);
		}
		let (min_ts, max_ts) = span_of(&batch.timestamps).map_or((None, None), |(lo, hi)| (Some(lo), Some(hi)));
		let receipt = IngestReceipt { aspect: aspect.to_string(), key: batch.key.clone(), fingerprint, row_count: batch.timestamps.len(), min_ts, max_ts, segment_ids: descriptors.iter().map(|d| d.id).collect(), series_ids: batch.series_ids(), commit_epoch: epoch, duplicate: false };
		let entry = LedgerEntry { aspect: aspect.to_string(), key: batch.key.clone(), fingerprint, row_count: batch.timestamps.len(), min_ts, max_ts, id_lo: receipt.segment_ids.first().copied(), id_hi: receipt.segment_ids.last().copied(), receipt_json: receipt.to_json(), series_ids: batch.series_ids(), commit_epoch: epoch, created_ms: now_ms() };
		ops.push(seq_bump(aspect, &commit, max_gen.saturating_add(1), epoch));
		ops.push(IndexOp::PutRollup { aspect: aspect.to_string(), meta: rollup });
		ops.push(IndexOp::RecordIngest { entry });
		self.commit_index(&IndexTxn::new(ops)).await?;
		commit.set_epoch(epoch);
		drop(commit);
		Ok(receipt)
	}

	/// Seal a dense `(timestamp, value)` batch under `aspect`'s **declared** schema —
	/// the catalog-backed counterpart of [`seal`](SegmentStore::seal) that looks the
	/// encoding up rather than taking it as a parameter.
	///
	/// # Errors
	///
	/// Returns an error if `aspect` has no declared schema; otherwise as
	/// [`seal`](SegmentStore::seal).
	pub async fn seal_declared(&self, aspect: &str, timestamps: &[i64], values: &[BigDecimal]) -> Result<SegmentDescriptor> {
		self.writable()?;
		let schema = self.require_schema(aspect).await?;
		self.seal(aspect, &schema, timestamps, values).await
	}

	/// Seal a **nullable** batch under `aspect`'s declared schema — the catalog-backed
	/// counterpart of [`seal_nullable`](SegmentStore::seal_nullable).
	///
	/// # Errors
	///
	/// Returns an error if `aspect` has no declared schema; otherwise as
	/// [`seal_nullable`](SegmentStore::seal_nullable).
	pub async fn seal_declared_nullable(&self, aspect: &str, timestamps: &[i64], values: &[Option<BigDecimal>]) -> Result<SegmentDescriptor> {
		self.writable()?;
		let schema = self.require_schema(aspect).await?;
		self.seal_nullable(aspect, &schema, timestamps, values).await
	}

	/// Seal a dense batch into a **paged** segment under `aspect`'s declared schema —
	/// the catalog-backed counterpart of [`seal_paged`](SegmentStore::seal_paged).
	///
	/// # Errors
	///
	/// Returns an error if `aspect` has no declared schema; otherwise as
	/// [`seal_paged`](SegmentStore::seal_paged).
	pub async fn seal_declared_paged(&self, aspect: &str, timestamps: &[i64], values: &[BigDecimal], rows_per_page: usize) -> Result<SegmentDescriptor> {
		self.writable()?;
		let schema = self.require_schema(aspect).await?;
		self.seal_paged(aspect, &schema, timestamps, values, rows_per_page).await
	}

	/// Seal a dense `(timestamp, value)` batch into a `.weftseg` file under `aspect`'s
	/// declared `schema` and record it in the index, returning the descriptor.
	///
	/// The batch is encoded under exactly the schema's declared
	/// [`PhysicalType`](weft_physical_type::PhysicalType): an unrepresentable value or
	/// one whose error exceeds the schema tolerance fails the seal rather than
	/// downcasting silently (hard constraint #4). The segment claims the next
	/// monotonic id for the aspect.
	///
	/// # Errors
	///
	/// Propagates a [`weft_physical_type::SealError`] (length mismatch, unrepresentable
	/// value, tolerance exceeded), a filesystem write error, or a libSQL index
	/// failure.
	pub async fn seal(&self, aspect: &str, schema: &AspectSchema, timestamps: &[i64], values: &[BigDecimal]) -> Result<SegmentDescriptor> {
		self.writable()?;
		let segment = schema.seal(timestamps, values).map_err(seal_failed)?;
		self.persist(aspect, &segment).await
	}

	/// Seal a **nullable** batch (a dense timestamp column and a `&[Option<BigDecimal>]`
	/// value column) into a `.weftseg` file and record it — the quality-column seal.
	///
	/// Present values are encoded densely under the declared encoding; `None` rows
	/// become cleared bits in the segment's quality mask. Enforcement mirrors
	/// [`seal`](SegmentStore::seal).
	///
	/// # Errors
	///
	/// As [`seal`](SegmentStore::seal).
	pub async fn seal_nullable(&self, aspect: &str, schema: &AspectSchema, timestamps: &[i64], values: &[Option<BigDecimal>]) -> Result<SegmentDescriptor> {
		self.writable()?;
		let segment = schema.seal_nullable(timestamps, values).map_err(seal_failed)?;
		self.persist(aspect, &segment).await
	}

	/// Seal a dense batch into a **paged** `.weftseg` segment (intra-segment page
	/// subdivision) and record it. Rows are partitioned into pages of `rows_per_page`,
	/// each independently encoded with its own min/max stats, so a later
	/// [`read_time_range`](SegmentStore::read_time_range) skips pages *within* the
	/// file, not just whole files.
	///
	/// # Errors
	///
	/// As [`seal`](SegmentStore::seal), plus a [`weft_physical_type::SealError::EmptyPageSize`]
	/// if `rows_per_page` is zero.
	pub async fn seal_paged(&self, aspect: &str, schema: &AspectSchema, timestamps: &[i64], values: &[BigDecimal], rows_per_page: usize) -> Result<SegmentDescriptor> {
		self.writable()?;
		let segment = schema.seal_paged(timestamps, values, rows_per_page).map_err(paged_seal_failed)?;
		self.persist_paged(aspect, &segment).await
	}

	/// Seal a **nullable** batch into a paged `.weftseg` segment and record it — the
	/// quality-column paged seal.
	///
	/// # Errors
	///
	/// As [`seal_paged`](SegmentStore::seal_paged).
	pub async fn seal_paged_nullable(&self, aspect: &str, schema: &AspectSchema, timestamps: &[i64], values: &[Option<BigDecimal>], rows_per_page: usize) -> Result<SegmentDescriptor> {
		self.writable()?;
		let segment = schema.seal_paged_nullable(timestamps, values, rows_per_page).map_err(paged_seal_failed)?;
		self.persist_paged(aspect, &segment).await
	}

	/// Write a freshly sealed segment to disk and record its descriptor. Shared by
	/// the dense and nullable seal paths.
	///
	/// The frame is write-once: it goes to a never-used generation-named frame
	/// (`{enc(aspect)}~g{gen}~p{gen}.weftseg`), created with `create_new` and fsynced
	/// with `segments/` fsynced after it, all before anything points at it — nothing
	/// ever truncates or replaces it. The frame is written outside the commit lock,
	/// so seals of one aspect write their frames in parallel; the id, the row, the
	/// allocator's raise and the rollup fold then commit under it
	/// ([`publish_seal`](Self::publish_seal)), which is where the id is assigned.
	pub(crate) async fn persist(&self, aspect: &str, segment: &Segment) -> Result<SegmentDescriptor> {
		aspect_name::validate(aspect)?;
		// Both opt-in layouts are applied here: the timestamp checkpoint index only when
		// configured AND the segment's shape actually benefits (sorted + irregular + big
		// enough), and the transposed value codec only when configured AND within its overhead
		// ceiling. With neither configured this writes the historical frame byte-for-byte, and
		// every combination reads identically.
		let bytes = segment.write_to_with(&self.frame_options(segment.stats.row_count, segment.benefits_from_checkpoints(), segment.checkpoint_codec_overhead()));
		let byte_len = bytes.len() as u64;
		let (gen, frame_crc, path) = self.write_seal_frame(aspect, bytes).await?;
		let descriptor = SegmentDescriptor::of_segment(0, path.to_string_lossy().into_owned(), byte_len, segment);
		let descriptor = self.publish_seal(aspect, &descriptor, gen, frame_crc).await?;
		// Per-segment partial sidecar (opt-in) — a pure read accelerator, so decode only
		// when the policy actually wants one, and never fail the seal on a sidecar error.
		if self.partials.base_for(segment.stats.row_count).is_some() {
			let (ts, vs) = segment.decode_nullable();
			if let Err(e) = self.write_partial_sidecar(aspect, &descriptor, gen, gen, frame_crc, ts, vs).await {
				tracing::warn!(aspect, id = descriptor.id, error = %e, "failed to write partial sidecar; downsample will fall back to a full decode");
			}
		}
		Ok(descriptor)
	}

	/// Write a freshly sealed **paged** segment to disk (format-version-3 frame) and
	/// record its descriptor. The paged analogue of [`persist`](SegmentStore::persist), with
	/// the same write-once frame and id-at-commit.
	pub(crate) async fn persist_paged(&self, aspect: &str, segment: &PagedSegment) -> Result<SegmentDescriptor> {
		aspect_name::validate(aspect)?;
		// As `persist`, though the checkpoint win is far smaller here — page pruning already
		// bounds a probe's decode to `rows_per_page`.
		let bytes = segment.write_to_with(&self.frame_options(segment.stats.row_count, segment.benefits_from_checkpoints(), segment.checkpoint_codec_overhead()));
		let byte_len = bytes.len() as u64;
		let (gen, frame_crc, path) = self.write_seal_frame(aspect, bytes).await?;
		let descriptor = SegmentDescriptor::of_paged_segment(0, path.to_string_lossy().into_owned(), byte_len, segment);
		let descriptor = self.publish_seal(aspect, &descriptor, gen, frame_crc).await?;
		// As `persist`: opt-in per-segment partial sidecar, never failing the seal.
		if self.partials.base_for(segment.stats.row_count).is_some() {
			let (ts, vs) = segment.decode_nullable();
			if let Err(e) = self.write_partial_sidecar(aspect, &descriptor, gen, gen, frame_crc, ts, vs).await {
				tracing::warn!(aspect, id = descriptor.id, error = %e, "failed to write partial sidecar; downsample will fall back to a full decode");
			}
		}
		Ok(descriptor)
	}

	/// Write `bytes` as `aspect`'s next write-once seal frame and make it durable,
	/// returning its generation, its trailer CRC and its path. The generation comes
	/// from the per-aspect counter that never hands a value out twice, so the name is
	/// never reused; the file is created with `create_new` (a name already taken fails
	/// rather than truncating another writer's frame) and fsynced in full, and
	/// `segments/` is fsynced after it, before the commit that names it runs. The
	/// fault points fire after the durable write, so a seal that stops at one leaves
	/// its frame behind as crash litter for recovery (S12) to classify — the durable
	/// write itself never removes a frame it wrote.
	pub(crate) async fn write_seal_frame(&self, aspect: &str, bytes: Vec<u8>) -> Result<(u64, u32, PathBuf)> {
		let gen = self.allocate_gens(aspect, 1).await?;
		let name = frame_name::frame_name(aspect, gen, gen);
		let path = self.segment_path(aspect, gen, gen)?;
		let frame_crc = write_new_durable(self.fs.as_ref(), &self.root.join("segments"), &name, bytes, SyncPolicy::Full, WritePoints::NONE).await.with_context(|| format!("writing segment {}", path.display()))?;
		fault::hit(FaultPoint::SFrameWritten).await?;
		fault::hit(FaultPoint::SFrameSynced).await?;
		self.sync_segments().await?;
		fault::hit(FaultPoint::SDirSynced).await?;
		Ok((gen, frame_crc, path))
	}

	/// The on-disk path of `aspect`'s write-once frame of generation `gen` and adoption
	/// order `prec` — the contained frame path builder (1.0 plan §5, D-S10): the name
	/// comes from [`frame_name::frame_name`], which carries the aspect through
	/// [`aspect_name::encode`], and the join is refused unless the result is a direct
	/// child of `segments/`. Every frame a seal or a maintenance output creates, and
	/// every frame the reaper unlinks, goes through this builder or
	/// [`output_path`](Self::output_path); nothing is created or unlinked anywhere
	/// else under the store root except under `segments/quarantine/`, which only
	/// recovery (S12) writes.
	///
	/// # Errors
	///
	/// An [`InvalidAspectName`](crate::InvalidAspectName) when `aspect` fails
	/// [`aspect_name::validate`], or an error when the joined path is somehow not a
	/// direct child of `segments/` (defence in depth behind the name check).
	pub(crate) fn segment_path(&self, aspect: &str, gen: u64, prec: u64) -> Result<PathBuf> {
		aspect_name::validate(aspect)?;
		contained_file(&self.root.join("segments"), &frame_name::frame_name(aspect, gen, prec))
	}

	/// The on-disk path of the partial-reduction sidecar beside `aspect`'s write-once
	/// frame of generation `gen` and adoption order `prec`: the same stem, generation
	/// and precedence as the frame, with the `.weftpart` extension, directly under
	/// `segments/` through the same containment check as
	/// [`segment_path`](Self::segment_path). A sidecar is named by its frame, not by
	/// the segment id — an id is assigned at commit and may be taken by a later
	/// output, while a frame name is never reused.
	///
	/// # Errors
	///
	/// As [`segment_path`](Self::segment_path).
	pub(crate) fn sidecar_path(&self, aspect: &str, gen: u64, prec: u64) -> Result<PathBuf> {
		aspect_name::validate(aspect)?;
		contained_file(&self.root.join("segments"), &frame_name::sidecar_name(aspect, gen, prec))
	}

	/// The on-disk path of the sidecar beside the frame at `frame_path`: the frame's
	/// own file name with its `.weftseg` extension exchanged for `.weftpart`, directly
	/// under `segments/` through the same containment check. This is how a sidecar is
	/// found for a segment whose generation the caller does not know — a legacy frame
	/// resolves to its legacy `{aspect}-{id}.weftpart`, a write-once frame to its
	/// generation-named one — so a reader never has to guess which scheme wrote it.
	///
	/// # Errors
	///
	/// An error when `frame_path` has no file name, is not a `.weftseg` frame,
	/// or the sidecar is somehow not a direct child of `segments/`.
	pub(crate) fn sidecar_path_for_frame(&self, frame_path: &str) -> Result<PathBuf> {
		let name = Path::new(frame_path).file_name().and_then(OsStr::to_str).ok_or_else(|| anyhow::anyhow!("segment {frame_path} has no frame file name"))?;
		let stem = name.strip_suffix(".weftseg").ok_or_else(|| anyhow::anyhow!("segment {frame_path} is not a .weftseg frame"))?;
		contained_file(&self.root.join("segments"), &format!("{stem}.weftpart"))
	}

	/// Materialize a per-segment partial-reduction sidecar for a just-sealed segment, when
	/// the [`PartialSidecarPolicy`] calls for one. Decodes the segment's rows, folds the
	/// present ones into a [`PartialReduction`] at `base` over [`SIDECAR_AGGREGATIONS`], and
	/// writes the `.weftpart` frame beside the `.weftseg`. A no-op (returns `Ok(())`) when the
	/// policy is disabled/too-small, the segment has no declared time unit, or it holds no
	/// present rows.
	///
	/// A sidecar is a *pure acceleration*: a failure to write one must never fail the seal,
	/// so the persist paths call this and log-and-ignore any error rather than propagating.
	pub(crate) async fn write_partial_sidecar(&self, aspect: &str, descriptor: &SegmentDescriptor, gen: u64, prec: u64, frame_crc: u32, timestamps: Vec<i64>, values: Vec<Option<BigDecimal>>) -> Result<()> {
		let Some(bytes) = self.partial_sidecar_bytes(descriptor, frame_crc, timestamps, values)? else { return Ok(()) };
		let path = self.sidecar_path(aspect, gen, prec)?;
		self.fs.create_new_write(&path, bytes, SyncPolicy::None, WritePoints::NONE).await.with_context(|| format!("writing partial sidecar {}", path.display()))?;
		Ok(())
	}

	/// The partial-reduction sidecar the [`PartialSidecarPolicy`] wants for `descriptor`'s
	/// rows, encoded, or `None` when it wants none (see
	/// [`write_partial_sidecar`](Self::write_partial_sidecar)).
	pub(crate) fn partial_sidecar_bytes(&self, descriptor: &SegmentDescriptor, frame_crc: u32, timestamps: Vec<i64>, values: Vec<Option<BigDecimal>>) -> Result<Option<Vec<u8>>> {
		let Some(base) = self.partials.base_for(descriptor.row_count) else { return Ok(None) };
		let Some(unit) = descriptor.time_unit else { return Ok(None) };
		let mut points: Vec<Point> = Vec::with_capacity(timestamps.len());
		for (t, v) in timestamps.into_iter().zip(values) {
			if let Some(value) = v {
				points.push(Point::new(instant_from_epoch(t, unit).ok_or_else(|| anyhow::anyhow!("segment {} holds epoch {t} outside the representable instant range", descriptor.path))?, value));
			}
		}
		if points.is_empty() {
			return Ok(None);
		}
		let partial = weft_reduce::reduce_partial(&points, base, None, None, &SIDECAR_AGGREGATIONS).map_err(|e| anyhow::anyhow!("reducing segment {} for its partial sidecar: {e}", descriptor.path))?;
		// Materialize the coarser rollup tiers the policy declares (each re-keyed from the
		// tier below), so a coarse downsample folds a coarse tier rather than the whole base.
		let sidecar = PartialSidecar::materialize(base, descriptor, frame_crc, partial, &self.partials.tiers()).map_err(|e| anyhow::anyhow!("building rollup tiers for segment {}: {e}", descriptor.path))?;
		Ok(Some(sidecar.to_bytes()?))
	}

	/// Give a maintenance output that a swap has just committed under `descriptor` the
	/// sidecar the policy wants for its rows, or remove the stale one its frame may carry
	/// (design section 5.3, M6). Through the store's filesystem: the new sidecar is written
	/// under a `.tmp-*` name and renamed over the frame's, so a reader never sees half of one;
	/// nothing is fsynced, since a sidecar is an accelerator a reader checks against the
	/// frame. Best effort: a failure is logged, never returned.
	pub(crate) async fn refresh_output_sidecar(&self, aspect: &str, descriptor: &SegmentDescriptor, gen: u64, prec: u64, frame_crc: u32, timestamps: Vec<i64>, values: Vec<Option<BigDecimal>>) {
		let result = async {
			let path = self.sidecar_path(aspect, gen, prec)?;
			let Some(bytes) = self.partial_sidecar_bytes(descriptor, frame_crc, timestamps, values)? else {
				self.fs.remove_file(&path).await.with_context(|| format!("removing stale partial sidecar {}", path.display()))?;
				return Ok(());
			};
			let segments = self.root.join("segments");
			let file_name = path.file_name().and_then(OsStr::to_str).ok_or_else(|| anyhow::anyhow!("partial sidecar {} has no file name", path.display()))?;
			let tmp = contained_file(&segments, &format!(".tmp-{}-{file_name}", uuid::Uuid::new_v4().simple()))?;
			self.fs.create_new_write(&tmp, bytes, SyncPolicy::None, WritePoints::NONE).await.with_context(|| format!("writing partial sidecar {}", tmp.display()))?;
			if let Err(e) = self.fs.rename(&tmp, &path).await {
				if let Err(cleanup) = self.fs.remove_file(&tmp).await {
					tracing::warn!(path = %tmp.display(), error = %cleanup, "could not remove a temporary partial sidecar");
				}
				return Err(anyhow::Error::new(e).context(format!("renaming partial sidecar {} into place", path.display())));
			}
			Ok::<(), anyhow::Error>(())
		}
		.await;
		if let Err(e) = result {
			tracing::warn!(aspect, id = descriptor.id, error = %e, "failed to refresh the partial sidecar of a maintenance output; downsample will fall back to a full decode");
		}
	}

	/// Delete the partial sidecar for `aspect`/`id`, if one exists, through the store's
	/// filesystem: what a merge does for each member it deletes, whose id no segment takes
	/// again. A missing sidecar is success, since the aim is only that no orphaned
	/// `.weftpart` is left behind.
	///
	/// # Errors
	///
	/// Propagates a filesystem error other than "not found".
	/// Delete the partial sidecar beside the frame at `frame_path`, if one exists,
	/// through the store's filesystem: what a merge does for each member it deletes.
	/// A missing sidecar is success, since the aim is only that no orphaned
	/// `.weftpart` is left behind.
	///
	/// # Errors
	///
	/// Propagates a filesystem error other than "not found".
	pub(crate) async fn remove_sidecar(&self, frame_path: &str) -> Result<()> {
		let path = self.sidecar_path_for_frame(frame_path)?;
		self.fs.remove_file(&path).await.with_context(|| format!("removing partial sidecar {}", path.display()))
	}
}
