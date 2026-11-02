//! Segment-store implementation: this submodule holds the `read` portion of [`SegmentStore`](super::SegmentStore).
//! Pure move of the former `segment_store.rs` `impl` block; no behaviour change.

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
		aspect_name::validate(aspect)?;
		let _pin = self.pin_reads();
		let descriptors = self.index.prune_by_time(aspect, start, end).await?;
		let mut timestamps = Vec::new();
		let mut values = Vec::new();
		for descriptor in &descriptors {
			let bytes = self.read_frame(descriptor).await?;
			// A paged frame (v3) decodes through PagedSegment::read_time_range, which
			// skips pages *within* the file; a single-block frame decodes whole.
			// Windowed read: a regular block-coded frame decodes only the row window (closed-form
			// index range + per-present-row value read) — the paged variant additionally skips whole
			// pages disjoint from the window — instead of the whole segment; any other shape falls
			// back to a full decode. Already filtered to `[start, end]`.
			let (ts, vs) = frame_read::read_range(descriptor.path.as_str(), &bytes, start, end)?;
			for (t, v) in ts.into_iter().zip(vs) {
				if start <= t && t <= end {
					timestamps.push(t);
					values.push(v);
				}
			}
		}
		Ok((timestamps, values))
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
	/// # Errors
	///
	/// If `aspect` has no declared schema, if a segment cannot be read or decoded, if a
	/// stored epoch falls outside the representable instant range, or if the reduction
	/// fails.
	pub async fn downsample_range(&self, aspect: &str, start: i64, end: i64, resolution: Resolution, aggregations: &[Aggregation]) -> Result<Vec<Bucket>> {
		let partial = self.downsample_partial(aspect, start, end, resolution, aggregations).await?;
		partial.map_or_else(|| Ok(Vec::new()), |p| p.finish(resolution, aggregations).map_err(|e| anyhow::anyhow!("finishing downsample of {aspect:?}: {e}")))
	}

	/// [`downsample_range`](Self::downsample_range) into buckets of a
	/// [`BucketWidth`](weft_reduce::BucketWidth) (`5s`, `15m`): the same cross-segment fold at
	/// the width's unit, each run of `count` unit buckets merged into one before finishing.
	/// Exact for every reduction, as merging is; a stored sidecar partial serves the unit as
	/// it serves [`downsample_range`](Self::downsample_range).
	///
	/// # Errors
	///
	/// As [`downsample_range`](Self::downsample_range).
	pub async fn downsample_range_width(&self, aspect: &str, start: i64, end: i64, width: weft_reduce::BucketWidth, aggregations: &[Aggregation]) -> Result<Vec<Bucket>> {
		let partial = self.downsample_partial(aspect, start, end, width.unit(), aggregations).await?;
		let Some(partial) = partial else { return Ok(Vec::new()) };
		partial.coarsen(width.count()).and_then(|p| p.finish_width(width, aggregations)).map_err(|e| anyhow::anyhow!("finishing downsample of {aspect:?}: {e}"))
	}

	/// The merged [`PartialReduction`] of `aspect`'s `[start, end]` rows at `resolution`, or
	/// `None` when no row falls there: the fold behind
	/// [`downsample_range`](Self::downsample_range) and
	/// [`downsample_range_width`](Self::downsample_range_width).
	pub(crate) async fn downsample_partial(&self, aspect: &str, start: i64, end: i64, resolution: Resolution, aggregations: &[Aggregation]) -> Result<Option<PartialReduction>> {
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
		let (descriptors, frame_crcs): (Vec<SegmentDescriptor>, Vec<Option<u32>>) = self.index.prune_present_rows(aspect, start, end).await?.into_iter().map(|row| (row.desc, row.frame_crc)).unzip();

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
						return Ok(Some(partial));
					}
				}
			}
			let bytes = self.read_frame(descriptor).await?;
			return segment_partial(descriptor, &bytes, start, end, schema.timestamp_unit, resolution, aggregations);
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
		}
		Ok(merged)
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
		aspect_name::validate(aspect)?;
		let _pin = self.pin_reads();
		let descriptors = self.index.prune_present_descriptors(aspect, t, t).await?;
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
	/// # Errors
	///
	/// Propagates a libSQL prune failure, a filesystem read error, or a frame-decode
	/// failure carrying a [`crate::FrameFaultError`] — the C5 fault class (`integrity`,
	/// `newer_format` or `undecodable`) for a corrupt or unreadable `.weftseg`.
	pub async fn read_points(&self, aspect: &str, ts: &[i64]) -> Result<Vec<Option<BigDecimal>>> {
		aspect_name::validate(aspect)?;
		let _pin = self.pin_reads();
		if ts.is_empty() {
			return Ok(Vec::new());
		}
		// Prune the index once by the batch's whole span (safe: every instant lies within it).
		let (lo, hi) = ts.iter().fold((i64::MAX, i64::MIN), |(lo, hi), &t| (lo.min(t), hi.max(t)));
		let descriptors = self.index.prune_present_descriptors(aspect, lo, hi).await?;
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
		aspect_name::validate(aspect)?;
		let _pin = self.pin_reads();
		let index = self.index.load_index(aspect).await?;
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
