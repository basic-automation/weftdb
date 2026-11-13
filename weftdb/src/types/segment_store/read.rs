//! Segment-store implementation: this submodule holds the `read` portion of [`SegmentStore`](super::SegmentStore).
//! Pure move of the former `segment_store.rs` `impl` block; no behaviour change.

use weft_physical_type::merge_newer_wins;

use super::*;

impl SegmentStore {
	/// Pin the reclaim epoch for a read about to prune the index, so that no frame a swap
	/// retires meanwhile is unlinked under it (see `reaper`). Every read path takes one
	/// first; it is released when the read's future is dropped, finished or not.
	pub(crate) fn pin_reads(&self) -> ReadPin<'_> {
		self.reaper.epochs().pin()
	}

	/// Read and decode `row`'s frame for maintenance, checking first that it is the frame
	/// the row was committed with (design section 5.3, M4): `byte_len` long and, once its
	/// CRC is bound, ending in `frame_crc`.
	pub(crate) async fn read_bound_frame(&self, row: &IndexRow) -> Result<DecodedFrame> {
		let descriptor = &row.desc;
		let bytes = self.read_frame(descriptor).await?;
		if bytes.len() as u64 != descriptor.byte_len {
			bail!("segment {} is {} bytes on disk, but its row records {}", descriptor.path, bytes.len(), descriptor.byte_len);
		}
		if let Some(expected) = row.frame_crc {
			let trailer = bytes.get(bytes.len().saturating_sub(4)..).and_then(|tail| <[u8; 4]>::try_from(tail).ok()).map(u32::from_le_bytes);
			if trailer != Some(expected) {
				bail!("segment {} does not end in the CRC its row was committed with ({expected:#010x})", descriptor.path);
			}
		}
		let frame = frame_read::read_full(descriptor.path.as_str(), &bytes)?;
		let rows_per_page = frame.rows_per_page();
		let (timestamps, values) = frame.decode_nullable();
		Ok(DecodedFrame { rows_per_page, timestamps, values })
	}

	/// Read the frames of `members` (ascending by id), checking each is the frame its row
	/// was committed with, and fold their rows into one time-sorted run, the newest member's
	/// value winning at a shared timestamp ([`fold_newest_wins`], on a blocking thread).
	pub(crate) async fn merge_members(&self, members: &[IndexRow]) -> Result<(Vec<i64>, Vec<Option<BigDecimal>>)> {
		let mut frames = Vec::with_capacity(members.len());
		for member in members {
			frames.push(self.read_bound_frame(member).await?);
		}
		tokio::task::spawn_blocking(move || fold_newest_wins(frames)).await.context("merging maintenance inputs")
	}

	/// [`merge_members`](Self::merge_members), and whether the fold is exactly the
	/// lowest-id member's own rows (Phase 4.6 skip-identical, design 9b): the member is
	/// `time_sorted`, and its decoded rows equal the folded ones row for row, each value
	/// compared in its plain form, scale included ([`same_rows`]), so `-3` and `-3.0` are
	/// different rows here even though `BigDecimal` calls them equal (critique F23).
	/// When it is, a reconcile can keep that member as it is instead of writing the fold.
	pub(crate) async fn merge_members_against_first(&self, members: &[IndexRow]) -> Result<(Vec<i64>, Vec<Option<BigDecimal>>, bool)> {
		let mut frames = Vec::with_capacity(members.len());
		for member in members {
			frames.push(self.read_bound_frame(member).await?);
		}
		let sorted_first = members.first().is_some_and(|first| first.desc.time_sorted);
		tokio::task::spawn_blocking(move || {
			let first = frames.first().filter(|_| sorted_first).map(|frame| (frame.timestamps.clone(), frame.values.clone()));
			let (timestamps, values) = fold_newest_wins(frames);
			let identical = first.is_some_and(|(first_ts, first_vs)| same_rows(&first_ts, &first_vs, &timestamps, &values));
			(timestamps, values, identical)
		})
		.await
		.context("merging maintenance inputs")
	}

	/// The file holding `descriptor`'s frame: its recorded path, resolved against this
	/// store's root (see [`resolve_frame_path`]), so that a root that was moved or
	/// restored elsewhere still reads its own frames.
	///
	/// # Errors
	///
	/// A recorded path that does not resolve to a file under `segments/`.
	pub(crate) fn frame_path(&self, descriptor: &SegmentDescriptor) -> Result<PathBuf> {
		resolve_frame_path(&self.root, &descriptor.path)
	}

	/// Read `descriptor`'s frame from [`frame_path`](Self::frame_path).
	pub(crate) async fn read_frame(&self, descriptor: &SegmentDescriptor) -> Result<Vec<u8>> {
		let path = self.frame_path(descriptor)?;
		tokio::fs::read(&path).await.with_context(|| format!("reading segment {}", path.display()))
	}

	/// Load the partial-reduction sidecar for `descriptor`'s segment, or [`None`] when no
	/// sidecar exists **or the one on disk is stale** (its staleness stamp does not match
	/// the live descriptor — a rewritten segment automatically invalidates its old sidecar,
	/// see [`PartialSidecar::matches`]). A malformed/foreign sidecar is treated as absent
	/// too, so a downsample is always correct, at worst un-accelerated.
	///
	/// # Errors
	///
	/// Propagates a filesystem read error other than "not found".
	pub async fn load_partial_sidecar(&self, aspect: &str, descriptor: &SegmentDescriptor, frame_crc: Option<u32>) -> Result<Option<PartialSidecar>> {
		aspect_name::validate(aspect)?;
		let path = self.sidecar_path_for_frame(&descriptor.path)?;
		let bytes = match tokio::fs::read(&path).await {
			Ok(bytes) => bytes,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
			Err(e) => return Err(anyhow::Error::new(e).context(format!("reading partial sidecar {}", path.display()))),
		};
		// A malformed or version-mismatched frame is not an error the caller must handle —
		// it just means "no usable sidecar", so the read falls back to a full decode.
		let Ok(sidecar) = PartialSidecar::from_bytes(&bytes) else { return Ok(None) };
		if !sidecar.matches(descriptor, frame_crc) {
			return Ok(None);
		}
		Ok(Some(sidecar))
	}

	/// Read every row of `aspect` whose timestamp falls in the inclusive range
	/// `[start, end]`, **opening only the segment files that overlap it**.
	///
	/// The index is pruned by time first (a SQL `WHERE` — no file touched for a
	/// disjoint segment); only the surviving descriptors' `.weftseg` files are opened,
	/// decoded, and filtered to the window. Returns parallel `(timestamps, values)`
	/// vectors with `None` at every null row, in segment-seal then in-segment order.
	///
	/// # Errors
	///
	/// Propagates a libSQL prune failure, a filesystem read error, or a frame-decode
	/// failure carrying a [`crate::FrameFaultError`] — the C5 fault class (`integrity`,
	/// `newer_format` or `undecodable`) for a corrupt or unreadable `.weftseg`.
	pub async fn read_time_range(&self, aspect: &str, start: i64, end: i64) -> Result<(Vec<i64>, Vec<Option<BigDecimal>>)> {
		self.read_time_range_of(aspect, crate::SeriesId::UNTAGGED, start, end).await
	}

	/// [`read_time_range`](Self::read_time_range) of one series (TAG-8): only the rows of
	/// `series` are read. [`read_time_range`](Self::read_time_range) is this with
	/// [`SeriesId::UNTAGGED`](crate::SeriesId::UNTAGGED). A series with no rows (or an id
	/// the aspect never registered) reads empty.
	///
	/// Cross-segment duplicate timestamps resolve newest-wins, equal to the point read
	/// and the reconcile merge. A single segment that is internally non-monotonic or
	/// self-duplicating is a corruption case, not handled by this path.
	///
	/// # Errors
	///
	/// As [`read_time_range`](Self::read_time_range).
	pub async fn read_time_range_of(&self, aspect: &str, series: crate::SeriesId, start: i64, end: i64) -> Result<(Vec<i64>, Vec<Option<BigDecimal>>)> {
		aspect_name::validate(aspect)?;
		let _pin = self.pin_reads();
		// Time-only prune (not present-prune): a segment whose every row is null still
		// contributes its null rows to the answer, because a newer null must be able to
		// override an older value at the same instant.  Point reads call
		// `series_present_descriptors` instead (which uses present-prune), so all-null
		// segments are skipped there — a null row is not a value — making point and
		// range reads deliberately asymmetric for the all-null-newer case (issue #443).
		let descriptors: Vec<SegmentDescriptor> = self.index.prune_series_rows_by_time(aspect, series, start, end).await?.into_iter().map(|row| row.desc).collect();
		// Fold overlapping segments with **newer-wins** (upsert).  Descriptors are in seal
		// (id) order, oldest first, matching the contract of `merge_newer_wins` where the
		// second argument supersedes the first at any shared instant.  This makes a range
		// read give the same answer as `read_point` / `read_points` across overlapping
		// segments (last-writer-wins) — and the same answer a later `reconcile_overlaps`
		// merge would fold them to.
		let mut merged: Vec<(i64, Option<BigDecimal>)> = Vec::new();
		for descriptor in &descriptors {
			// Phase 5 memory-mapped reads: with `mmap_reads` on (and the feature compiled in),
			// the window decodes straight from a read-only mapping of the frame instead of a
			// heap copy of the whole file. Same bytes, same decoder, same rows.
			let bytes = if self.mmap_reads && super::mapped::MMAP_COMPILED {
				let path = self.frame_path(descriptor)?;
				super::mapped::map_frame(&path).with_context(|| format!("mapping segment {}", path.display()))?
			} else {
				super::mapped::FrameBytes::Owned(self.read_frame(descriptor).await?)
			};
			// A paged frame (v3) decodes through PagedSegment::read_time_range, which
			// skips pages *within* the file; a single-block frame decodes whole.
			// Windowed read: a regular block-coded frame decodes only the row window (closed-form
			// index range + per-present-row value read) — the paged variant additionally skips whole
			// pages disjoint from the window — instead of the whole segment; any other shape falls
			// back to a full decode. Already filtered to `[start, end]`.
			let (ts, vs) = frame_read::read_range(descriptor.path.as_str(), &bytes, start, end)?;
			let mut rows: Vec<(i64, Option<BigDecimal>)> = Vec::with_capacity(ts.len());
			for (t, v) in ts.into_iter().zip(vs) {
				if start <= t && t <= end {
					rows.push((t, v));
				}
			}
			// Sort each segment's rows by timestamp so that `merge_newer_wins` receives
			// **non-decreasing** inputs, even when a segment's rows are stored out of
			// order (e.g. a pre-v2 store with out-of-order timestamps `[55, 45, 65]`).
			// `fold_newest_wins` (`segment_store.rs`) does the same sort before merging.
			rows.sort_by_key(|(t, _)| *t);
			if merged.is_empty() {
				merged = rows;
			} else {
				merged = merge_newer_wins(&merged, &rows);
			}
		}
		Ok(merged.into_iter().unzip())
	}

	/// **Cross-segment downsample** — reduce `aspect`'s `[start, end]` rows into
	/// grid-aligned buckets **without ever materializing the range**.
	///
	/// The distributed shape of [`weft_reduce::reduce`]: the index is pruned by time, then
	/// each surviving segment is read, windowed, and folded into its own
	/// [`PartialReduction`]; the partials are merged and finished once. Only one segment's
	/// rows are in memory at a time, so a range far larger than RAM still reduces — and
	/// with a `sketch_p*` aggregation the per-bucket state is bounded too, which is the
	/// scenario `DdSketch`'s exact mergeability exists for.
	///
	/// Identical to reading the whole range and reducing it in one pass (merging is exact
	/// for every reduction), which is what the test asserts. Timestamps are the aspect's
	/// declared [`TimeUnit`] epochs, lifted to absolute instants for bucketing.
	///
	/// The grid itself is bounded (ROB-13, 1.0 plan decision 12): a fold that
	/// reaches more than
	/// [`max_downsample_buckets`](crate::SegmentStoreOptions::max_downsample_buckets)
	/// buckets is refused with
	/// [`StoreError::DownsampleTooLarge`](crate::StoreError::DownsampleTooLarge)
	/// while it runs, so no caller of the store — HTTP handler or embedder —
	/// can make it hold an unbounded partial.
	///
	/// # Errors
	///
	/// If `aspect` has no declared schema, if a segment cannot be read or decoded, if a
	/// stored epoch falls outside the representable instant range, or if the reduction
	/// fails.
	pub async fn downsample_range(&self, aspect: &str, start: i64, end: i64, resolution: Resolution, aggregations: &[Aggregation]) -> Result<Vec<Bucket>> {
		self.downsample_range_of(aspect, crate::SeriesId::UNTAGGED, start, end, resolution, aggregations).await
	}

	/// [`downsample_range`](Self::downsample_range) of one series (TAG-8).
	///
	/// # Errors
	///
	/// As [`downsample_range`](Self::downsample_range).
	pub async fn downsample_range_of(&self, aspect: &str, series: crate::SeriesId, start: i64, end: i64, resolution: Resolution, aggregations: &[Aggregation]) -> Result<Vec<Bucket>> {
		// At a plain resolution the finished grid is the partial's own buckets,
		// so the decision 12 cap is the fold's cap unchanged (ROB-13). The grid is
		// finished here, so a single segment may come back already finished (#401).
		match self.downsample_fold(aspect, series, (start, end), weft_reduce::BucketWidth::from(resolution), aggregations).await? {
			None => Ok(Vec::new()),
			Some(Folded::Finished(buckets)) => Ok(buckets),
			Some(Folded::Partial(partial)) => partial.finish(resolution, aggregations).map_err(|e| anyhow::anyhow!("finishing downsample of {aspect:?}: {e}")),
		}
	}

	/// [`downsample_range`](Self::downsample_range) into buckets of a
	/// [`BucketWidth`](weft_reduce::BucketWidth) (`5s`, `15m`): the same cross-segment fold at
	/// the width's unit, each run of `count` unit buckets merged into one before finishing.
	/// Exact for every reduction, as merging is; a stored sidecar partial serves the unit as
	/// it serves [`downsample_range`](Self::downsample_range).
	///
	/// The decision 12 bound is on the grid this returns (ROB-13): the fold at
	/// the unit is capped at `count` times the bucket cap, and the coarsened
	/// grid is refused over the cap itself.
	///
	/// # Errors
	///
	/// As [`downsample_range`](Self::downsample_range).
	pub async fn downsample_range_width(&self, aspect: &str, start: i64, end: i64, width: weft_reduce::BucketWidth, aggregations: &[Aggregation]) -> Result<Vec<Bucket>> {
		self.downsample_range_width_of(aspect, crate::SeriesId::UNTAGGED, start, end, width, aggregations).await
	}

	/// [`downsample_range_width`](Self::downsample_range_width) of one series (TAG-8).
	///
	/// # Errors
	///
	/// As [`downsample_range`](Self::downsample_range).
	pub async fn downsample_range_width_of(&self, aspect: &str, series: crate::SeriesId, start: i64, end: i64, width: weft_reduce::BucketWidth, aggregations: &[Aggregation]) -> Result<Vec<Bucket>> {
		// The fold runs at the width's unit, where one output bucket is `count`
		// unit buckets, so the fold's cap is the decision 12 cap scaled by the
		// count; the cap itself is then enforced exactly on the coarsened
		// grid, which is the grid the caller gets (ROB-13). A fold that finishes
		// on the integer path (#401) is already at the width, under the cap itself.
		// The server reaches every stored downsample through here.
		let partial = match self.downsample_fold(aspect, series, (start, end), width, aggregations).await? {
			None => return Ok(Vec::new()),
			Some(Folded::Finished(buckets)) => return Ok(buckets),
			Some(Folded::Partial(partial)) => partial,
		};
		let coarsened = partial.coarsen(width.count()).map_err(|e| anyhow::anyhow!("finishing downsample of {aspect:?}: {e}"))?;
		if coarsened.len() > self.max_downsample_buckets {
			return Err(StoreError::DownsampleTooLarge { aspect: aspect.to_string(), buckets: coarsened.len(), limit: self.max_downsample_buckets }.into());
		}
		coarsened.finish_width(width, aggregations).map_err(|e| anyhow::anyhow!("finishing downsample of {aspect:?}: {e}"))
	}

	/// The fold behind [`downsample_range`](Self::downsample_range) and
	/// [`downsample_range_width`](Self::downsample_range_width): `aspect`'s `[start, end]`
	/// rows at `width`'s unit, or `None` when no row falls there. It comes back as the
	/// merged [`PartialReduction`] at the unit, which the caller coarsens to `width` and
	/// finishes, or as the buckets at `width` already finished when the read takes the integer
	/// path (#401: see `segment_fold` and `downsample_scaled_rows`).
	///
	/// The bound (ROB-13): a partial is held to `width`'s count times the bucket cap, a
	/// single segment's own or the merged accumulator as each segment folds in, and refused
	/// with [`StoreError::DownsampleTooLarge`](crate::StoreError::DownsampleTooLarge) the
	/// first time it reaches one bucket more, rather than the grid being counted once built;
	/// finished buckets are held to the bucket cap itself.
	async fn downsample_fold(&self, aspect: &str, series: crate::SeriesId, (start, end): (i64, i64), width: weft_reduce::BucketWidth, aggregations: &[Aggregation]) -> Result<Option<Folded>> {
		let resolution = width.unit();
		let max_buckets = self.max_downsample_buckets.saturating_mul(width.count() as usize);
		let too_many = |buckets: &Vec<Bucket>| buckets.len() > self.max_downsample_buckets;
		aspect_name::validate(aspect)?;
		let _pin = self.pin_reads();
		let schema = self.require_schema(aspect).await?;
		// Quality prune, not time prune alone (roadmap Phase 4.3): a segment whose every row
		// is null cannot move a bucket. Its rows contribute to no sum, count, min, max, first
		// or last, and a partial that holds nothing is dropped when buckets merge, so the
		// buckets this fold produces are the same with the segment in or out — and its frame
		// is never opened. `read_time_range` right above deliberately keeps the time-only
		// prune: its answer *is* the rows, nulls included, so dropping that segment there
		// would delete timestamps the caller asked for.
		let (descriptors, frame_crcs): (Vec<SegmentDescriptor>, Vec<Option<u32>>) = self.index.prune_series_present_rows(aspect, series, start, end).await?.into_iter().map(|row| (row.desc, row.frame_crc)).unzip();

		// A persisted per-segment partial can *substitute* for decoding a segment only when
		// it can answer this exact query: every requested reduction must be one the sidecar
		// materializes (the exact percentiles / TWA need the whole bucket, so a query asking
		// for them decodes), one of the sidecar's materialized grids (its base or a coarser
		// rollup tier) must equal or nest in the requested resolution (`partial_for` picks the
		// coarsest that does, re-keying from the fewest buckets), and the window must cover the
		// whole segment (no in-window trimming). When all three hold the stored partial IS the
		// segment's contribution, so the read skips the value-column decode entirely.
		let sidecar_eligible = aggregations.iter().all(|a| a.is_sidecar_materializable());

		// A single pruned segment has nothing to overlap with, so the offload below is
		// pure cost: measured 35.7ms vs 21.0ms inline on a 200k-row single-segment frame
		// (the `spawn_blocking` hand-off, ~1.7x slower, for zero parallelism). One
		// segment is a common shape — a young aspect, or a tight window that prunes to
		// one frame — so it keeps the inline path.
		if descriptors.len() == 1 {
			let descriptor = &descriptors[0];
			let frame_crc = frame_crcs[0];
			// The sidecar fast path: no file decode at all when a matching partial serves it —
			// directly when a materialized grid equals the requested resolution, or re-keyed
			// from the coarsest tier that nests in it.
			if sidecar_eligible && window_covers_segment(descriptor, start, end) {
				if let Some(sidecar) = self.load_partial_sidecar(aspect, descriptor, frame_crc).await? {
					if let Some(partial) = sidecar.partial_for(resolution).map_err(|e| anyhow::anyhow!("re-keying the sidecar of {aspect:?}: {e}"))? {
						return check_partial_len(aspect, partial, max_buckets).map(|partial| Some(Folded::Partial(partial)));
					}
				}
			}
			let bytes = self.read_frame(descriptor).await?;
			return match segment_fold(descriptor, &bytes, (start, end), schema.timestamp_unit, resolution, aggregations, Some(width))? {
				Some(Folded::Partial(partial)) => check_partial_len(aspect, partial, max_buckets).map(|partial| Some(Folded::Partial(partial))),
				Some(Folded::Finished(buckets)) if too_many(&buckets) => Err(StoreError::DownsampleTooLarge { aspect: aspect.to_string(), buckets: buckets.len(), limit: self.max_downsample_buckets }.into()),
				finished => Ok(finished),
			};
		}

		// #401, several segments: a read that asks for a reduction needing every
		// sample (a percentile, a time-weighted average) holds all of them anyway, as the
		// merged partial's `BigDecimal` samples. On a `ScaledI64` aspect it holds the mantissas
		// instead and finishes them once on the integer path; a sketch, or a segment off the
		// aspect's one scale, takes the partial route below.
		let full_bucket = aggregations.iter().any(|a| a.needs_full_bucket()) && aggregations.iter().all(|a| a.sketch_quantile().is_none());
		if full_bucket && matches!(schema.value, weft_physical_type::PhysicalType::ScaledI64 { .. }) {
			if let Some(buckets) = self.downsample_scaled_rows(aspect, &descriptors, (start, end), schema.timestamp_unit, width, aggregations).await? {
				if too_many(&buckets) {
					return Err(StoreError::DownsampleTooLarge { aspect: aspect.to_string(), buckets: buckets.len(), limit: self.max_downsample_buckets }.into());
				}
				return Ok(Some(Folded::Finished(buckets)));
			}
		}

		// Each pruned segment folds into its own `PartialReduction` **concurrently**: the
		// file read is async I/O and the decode + reduce is CPU-bound, so each segment's
		// CPU half runs on the blocking pool and the reads overlap rather than queueing
		// behind one another. A segment served from its sidecar skips the decode (and the
		// `spawn_blocking` hand-off) altogether — just a small async read of the partial.
		// Peak memory is unchanged in kind — a segment's rows are dropped at its partial —
		// but now bounded by the in-flight segment count rather than by one, which is the
		// deliberate trade for the parallelism.
		let tasks = descriptors.into_iter().zip(frame_crcs).map(|(descriptor, frame_crc)| {
			let aggregations = aggregations.to_vec();
			let unit = schema.timestamp_unit;
			async move {
				if sidecar_eligible && window_covers_segment(&descriptor, start, end) {
					if let Some(sidecar) = self.load_partial_sidecar(aspect, &descriptor, frame_crc).await? {
						if let Some(partial) = sidecar.partial_for(resolution).map_err(|e| anyhow::anyhow!("re-keying a sidecar of {aspect:?}: {e}"))? {
							return Ok::<Option<PartialReduction>, anyhow::Error>(Some(partial));
						}
					}
				}
				let bytes = self.read_frame(&descriptor).await?;
				tokio::task::spawn_blocking(move || segment_partial(&descriptor, &bytes, start, end, unit, resolution, &aggregations)).await.context("segment reduce task panicked")?
			}
		});
		// `try_join_all` preserves input order, so the merge below folds partials in
		// descriptor order on every run — merging is exact and order-independent for
		// every reduction, but a deterministic order keeps results reproducible.
		let partials = futures::future::try_join_all(tasks).await?;

		let mut merged: Option<PartialReduction> = None;
		for partial in partials.into_iter().flatten() {
			match merged.as_mut() {
				Some(m) => m.merge(partial).map_err(|e| anyhow::anyhow!("merging a segment partial of {aspect:?}: {e}"))?,
				None => merged = Some(partial),
			}
			// The accumulator is the state the fold holds, so the bound is
			// checked as each segment folds in (ROB-13): the first count over
			// the cap refuses, and the partials still queued are dropped with
			// it instead of being merged into a grid nobody may return.
			if let Some(m) = &merged {
				if m.len() > max_buckets {
					return Err(StoreError::DownsampleTooLarge { aspect: aspect.to_string(), buckets: m.len(), limit: max_buckets }.into());
				}
			}
		}
		Ok(merged.map(Folded::Partial))
	}

	/// Every pruned segment's in-window rows of `aspect` as `(epoch nanos, mantissa)`,
	/// decoded concurrently on the blocking pool and concatenated in descriptor order — the
	/// order the partial route merges in, so each bucket sees its samples in the same order —
	/// then reduced once at `width` by [`weft_reduce::reduce_scaled_width`]. The buckets are the
	/// ones the merged partials finish to at that width. `Ok(None)` when a segment holds anything but `ScaledI64` values, or
	/// a scale other than the first segment's, or `reduce_scaled` declines: the caller then
	/// takes the partial route.
	pub(crate) async fn downsample_scaled_rows(&self, aspect: &str, descriptors: &[SegmentDescriptor], (start, end): (i64, i64), unit: TimeUnit, width: weft_reduce::BucketWidth, aggregations: &[Aggregation]) -> Result<Option<Vec<Bucket>>> {
		let tasks = descriptors.iter().cloned().map(|descriptor| async move {
			let bytes = self.read_frame(&descriptor).await?;
			tokio::task::spawn_blocking(move || frame_read::read_range_physical(descriptor.path.as_str(), &bytes, start, end).map(|(ts, vs)| scaled_rows(&ts, &vs, start, end, unit))).await.context("segment decode task panicked")?
		});
		let decoded = futures::future::try_join_all(tasks).await?;
		let (mut nanos, mut mantissas, mut scale) = (Vec::new(), Vec::new(), None::<u8>);
		for rows in decoded {
			let Some((segment_nanos, segment_mantissas, segment_scale)) = rows else { return Ok(None) };
			if segment_nanos.is_empty() {
				continue;
			}
			if *scale.get_or_insert(segment_scale) != segment_scale {
				return Ok(None);
			}
			nanos.extend(segment_nanos);
			mantissas.extend(segment_mantissas);
		}
		let Some(scale) = scale else { return Ok(Some(Vec::new())) };
		let aggregations = aggregations.to_vec();
		let reduced = tokio::task::spawn_blocking(move || weft_reduce::reduce_scaled_width(&nanos, &mantissas, u32::from(scale), width, None, None, &aggregations)).await.context("downsample reduce task panicked")?;
		reduced.map_err(|e| anyhow::anyhow!("reducing {aspect:?}: {e}"))
	}

	/// **Point lookup** (roadmap Phase 4.6 read-planner): the present value of
	/// `aspect` at exactly timestamp `t`, or [`None`] when no present row carries it.
	///
	/// The index is pruned by time first — a point lookup is
	/// [`prune_by_time`](SegmentIndexStore::prune_by_time) with `start == end == t`,
	/// so only the `.weftseg` files whose `[min_ts, max_ts]` spans `t` are opened. Each
	/// opened segment resolves the instant with its own persisted per-segment order
	/// signal: a `time_sorted` segment binary-searches the timestamp column, an
	/// out-of-order one linear-scans it (see
	/// [`Segment::value_at`](weft_physical_type::Segment::value_at)). Candidates are
	/// pruned in seal-id order, so when overlapping segments each carry a present
	/// value at `t` the most recently sealed one wins (last-writer-wins) — the natural
	/// read-your-writes answer once out-of-order reconciliation (Phase 4.6) can leave
	/// two segments spanning one instant.
	///
	/// **Quality pruning is wired in at both levels of this path** (roadmap Phase 4.3).
	/// The index prune adds a quality term to its `WHERE` clause — time overlap plus
	/// `null_count < row_count` — so a segment whose indexed stats say every row is null
	/// is dropped before its file is ever opened; inside a surviving frame the same test
	/// on the persisted segment and per-page stats skips the pages a point lookup cannot answer
	/// (the paged form under [`read_frame_point`](weft_physical_type::read_frame_point), which
	/// every store read site reaches through `frame_read` — TAG-4).
	/// Neither changes the answer: a null row is not a value.
	///
	/// # Errors
	///
	/// Propagates a libSQL prune failure, a filesystem read error, or a frame-decode
	/// failure carrying a [`crate::FrameFaultError`] — the C5 fault class (`integrity`,
	/// `newer_format` or `undecodable`) for a corrupt or unreadable `.weftseg`.
	pub async fn read_point(&self, aspect: &str, t: i64) -> Result<Option<BigDecimal>> {
		self.read_point_of(aspect, crate::SeriesId::UNTAGGED, t).await
	}

	/// [`read_point`](Self::read_point) of one series (TAG-8).
	///
	/// # Errors
	///
	/// As [`read_point`](Self::read_point).
	pub async fn read_point_of(&self, aspect: &str, series: crate::SeriesId, t: i64) -> Result<Option<BigDecimal>> {
		aspect_name::validate(aspect)?;
		let _pin = self.pin_reads();
		let descriptors = self.series_present_descriptors(aspect, series, t, t).await?;
		let mut found = None;
		for descriptor in &descriptors {
			let bytes = self.read_frame(descriptor).await?;
			// Streaming single-value read: prunes/skips the pages and value-column blocks a point
			// lookup does not touch, unpacking only the one block covering `t` on a per-block codec
			// (roadmap Phase 4/6). Equal to `…read_from(&bytes)?.value_at(t)` for every frame.
			let hit = frame_read::read_point(descriptor.path.as_str(), &bytes, t)?;
			if hit.is_some() {
				found = hit;
			}
		}
		Ok(found)
	}

	/// **Batch point lookup** (roadmap Phase 4/6): the present value of `aspect` at each
	/// instant in `ts`, returned aligned to `ts` (`None` where no present row carries it).
	///
	/// The batch analogue of [`read_point`](SegmentStore::read_point): the index is pruned
	/// **once** by the batch's whole `[min(ts), max(ts)]` span, and each surviving segment is
	/// opened **once** and its timestamp column decoded **once** for the whole batch (via the
	/// frame reader's streaming [`read_frame_points`](weft_physical_type::read_frame_points)), so
	/// looking up `N` instants that share segments pays one file read + one timestamp decode per
	/// segment rather than `N`. As with `read_point`, candidates are merged in seal-id order so
	/// the most recently sealed present value wins per instant (last-writer-wins). An empty `ts`
	/// yields an empty vector without touching the index. The same quality pruning as
	/// [`read_point`](SegmentStore::read_point) applies at both levels: fully null segments are
	/// dropped by the index prune, fully null pages by the frame's page index.
	///
	/// The batch itself is bounded (ROB-13, 1.0 plan decision 12): more than
	/// [`max_point_batch`](crate::SegmentStoreOptions::max_point_batch) instants
	/// is refused with
	/// [`StoreError::PointBatchTooLarge`](crate::StoreError::PointBatchTooLarge)
	/// before the index is pruned. The single lookup,
	/// [`read_point`](SegmentStore::read_point), names one instant and needs
	/// no cap.
	///
	/// # Errors
	///
	/// Propagates a libSQL prune failure, a filesystem read error, or a frame-decode
	/// failure carrying a [`crate::FrameFaultError`] — the C5 fault class (`integrity`,
	/// `newer_format` or `undecodable`) for a corrupt or unreadable `.weftseg`.
	pub async fn read_points(&self, aspect: &str, ts: &[i64]) -> Result<Vec<Option<BigDecimal>>> {
		self.read_points_of(aspect, crate::SeriesId::UNTAGGED, ts).await
	}

	/// [`read_points`](Self::read_points) of one series (TAG-8).
	///
	/// # Errors
	///
	/// As [`read_points`](Self::read_points).
	pub async fn read_points_of(&self, aspect: &str, series: crate::SeriesId, ts: &[i64]) -> Result<Vec<Option<BigDecimal>>> {
		aspect_name::validate(aspect)?;
		let _pin = self.pin_reads();
		if ts.is_empty() {
			return Ok(Vec::new());
		}
		// The batch is the read (ROB-13): over the cap it is refused before a
		// single segment is pruned or opened, the same place the HTTP
		// `at-multi` handler's own ceiling refuses it (ROB-12).
		if ts.len() > self.max_point_batch {
			return Err(StoreError::PointBatchTooLarge { aspect: aspect.to_string(), instants: ts.len(), limit: self.max_point_batch }.into());
		}
		// Prune the index once by the batch's whole span (safe: every instant lies within it).
		let (lo, hi) = ts.iter().fold((i64::MAX, i64::MIN), |(lo, hi), &t| (lo.min(t), hi.max(t)));
		let descriptors = self.series_present_descriptors(aspect, series, lo, hi).await?;
		let mut found = vec![None; ts.len()];
		for descriptor in &descriptors {
			let bytes = self.read_frame(descriptor).await?;
			let hits = frame_read::read_points(descriptor.path.as_str(), &bytes, ts)?;
			// Later (higher seal-id) segments override earlier ones per instant — last-writer-wins.
			for (slot, hit) in found.iter_mut().zip(hits) {
				if hit.is_some() {
					*slot = hit;
				}
			}
		}
		Ok(found)
	}

	/// Read every present row of `aspect` whose **value** falls in the inclusive range
	/// `[lo, hi]`, **opening only the segment files whose value span overlaps it**.
	///
	/// The value column has no SQL ordering (the `BigDecimal` bounds are stored as
	/// text), so the pruning runs through the resident
	/// [`SegmentIndex`](weft_physical_type::SegmentIndex): it is loaded from the
	/// control plane and pruned by value, and only the surviving descriptors' files
	/// are opened. Within each opened segment the rows are filtered to those whose
	/// value is present and in `[lo, hi]`. Returns parallel `(timestamps, values)`
	/// vectors, in segment-seal then in-segment order.
	///
	/// # Errors
	///
	/// Propagates a libSQL read failure, a filesystem read error, or a frame-decode
	/// failure carrying a [`crate::FrameFaultError`] — the C5 fault class (`integrity`,
	/// `newer_format` or `undecodable`) for a corrupt or unreadable `.weftseg`.
	pub async fn read_value_range(&self, aspect: &str, lo: &BigDecimal, hi: &BigDecimal) -> Result<(Vec<i64>, Vec<BigDecimal>)> {
		self.read_value_range_of(aspect, crate::SeriesId::UNTAGGED, lo, hi).await
	}

	/// [`read_value_range`](Self::read_value_range) of one series (TAG-8): the resident
	/// index it prunes by value is built from that series' rows only.
	///
	/// # Errors
	///
	/// As [`read_value_range`](Self::read_value_range).
	pub async fn read_value_range_of(&self, aspect: &str, series: crate::SeriesId, lo: &BigDecimal, hi: &BigDecimal) -> Result<(Vec<i64>, Vec<BigDecimal>)> {
		aspect_name::validate(aspect)?;
		let _pin = self.pin_reads();
		let index = self.index.load_series_index(aspect, series).await?;
		let mut timestamps = Vec::new();
		let mut values = Vec::new();
		for descriptor in index.prune_by_value(lo, hi) {
			let (ts, vs) = self.decode_all(descriptor).await?;
			for (t, v) in ts.into_iter().zip(vs) {
				if let Some(value) = v {
					if lo <= &value && &value <= hi {
						timestamps.push(t);
						values.push(value);
					}
				}
			}
		}
		Ok((timestamps, values))
	}

	/// The present-value descriptors of `series` overlapping `[start, end]`: the
	/// series-scoped form of the aspect-wide present prune (`prune_present_descriptors`, TAG-8).
	async fn series_present_descriptors(&self, aspect: &str, series: crate::SeriesId, start: i64, end: i64) -> Result<Vec<SegmentDescriptor>> {
		Ok(self.index.prune_series_present_rows(aspect, series, start, end).await?.into_iter().map(|row| row.desc).collect())
	}

	/// Resolve `selector` to the one series of `aspect` it names (TAG-8, tags D4), once,
	/// for the `_of` reads.
	///
	/// - [`SeriesSelector::untagged`](crate::SeriesSelector::untagged) is
	///   [`SeriesId::UNTAGGED`](crate::SeriesId::UNTAGGED) without touching the catalog.
	/// - Otherwise every registered series whose [`TagSet`](crate::TagSet) the selector
	///   [matches](crate::SeriesSelector::matches_key) is a candidate, read from the
	///   index's per-aspect catalog cache (no SQL once loaded), and series 0 is one
	///   only when the selector matches the empty tag set **and** series 0 holds rows
	///   (series 0 has no catalog row; without the existence test a loose selector would
	///   always be ambiguous on a tagged aspect).
	/// - No candidate is `Ok(None)`: the read is empty.
	///
	/// # Errors
	///
	/// [`StoreError::AmbiguousSeries`] when two or more series match, an invalid
	/// `aspect` name, or a libSQL read failure.
	pub async fn resolve_series(&self, aspect: &str, selector: &crate::SeriesSelector) -> Result<Option<crate::SeriesId>> {
		aspect_name::validate(aspect)?;
		if *selector == crate::SeriesSelector::untagged() {
			return Ok(Some(crate::SeriesId::UNTAGGED));
		}
		let mut matched = Vec::new();
		if selector.matches(&crate::TagSet::new()) && self.index.series_has_rows(aspect, crate::SeriesId::UNTAGGED).await? {
			matched.push(crate::SeriesId::UNTAGGED);
		}
		for (id, key) in self.index.cached_series_of(aspect).await?.iter() {
			if selector.matches_key(key) {
				matched.push(*id);
			}
		}
		match matched.as_slice() {
			[] => Ok(None),
			[one] => Ok(Some(*one)),
			_ => Err(StoreError::AmbiguousSeries { aspect: aspect.to_string(), matched: matched.len() }.into()),
		}
	}

	/// The tags of one series of `aspect` (TAG-10): the empty set for
	/// [`SeriesId::UNTAGGED`](crate::SeriesId::UNTAGGED), the decoded catalog key for a
	/// registered series, and `None` for an id the catalog does not hold. A read route
	/// reports this beside the rows of the series [`resolve_series`](Self::resolve_series)
	/// picked.
	///
	/// # Errors
	///
	/// An invalid `aspect` name, a libSQL read failure, or a stored key that is not
	/// canonical.
	pub async fn series_tags(&self, aspect: &str, series: crate::SeriesId) -> Result<Option<crate::TagSet>> {
		aspect_name::validate(aspect)?;
		Ok(self.index.series_key_of(aspect, series).await?.map(crate::TagSet::from))
	}

	/// Read and fully decode one segment file (frame-version aware), returning all
	/// rows aligned `(timestamps, values)` with `None` at every null row.
	pub(crate) async fn decode_all(&self, descriptor: &SegmentDescriptor) -> Result<(Vec<i64>, Vec<Option<BigDecimal>>)> {
		let bytes = self.read_frame(descriptor).await?;
		Ok(frame_read::read_full(descriptor.path.as_str(), &bytes)?.decode_nullable())
	}

	/// The **materialized** segment-set rollup for `aspect` — the same aspect-wide
	/// summary as [`aspect_stats`](SegmentStore::aspect_stats), but read as a single
	/// `segment_index.db` row (O(1)) rather than scanning the whole segment index. An aspect
	/// with no sealed segments yields the empty rollup
	/// ([`AspectMetadata::default`]). The rollup also carries the aspect-wide value
	/// span, which the per-segment-scan stats do not.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn aspect_metadata(&self, aspect: &str) -> Result<AspectMetadata> {
		Ok(self.index.rollup(aspect).await?.unwrap_or_default())
	}
}

/// A single segment's partial against the fold's bucket bound (ROB-13): the
/// partial passes unchanged at or under `max_buckets`, and is refused —
/// dropped, with the count it reached — over it.
fn check_partial_len(aspect: &str, partial: PartialReduction, max_buckets: usize) -> Result<PartialReduction> {
	if partial.len() > max_buckets {
		return Err(StoreError::DownsampleTooLarge { aspect: aspect.to_string(), buckets: partial.len(), limit: max_buckets }.into());
	}
	Ok(partial)
}
