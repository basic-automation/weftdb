//! # Per-segment materialized partial reductions — the `.weftpart` sidecar
//!
//! A sealed segment is **immutable**, so a [`PartialReduction`] computed over it at seal
//! time can never go stale — which is exactly what makes it worth persisting. A later
//! cross-segment downsample ([`SegmentStore::downsample_range`](super::SegmentStore::downsample_range))
//! can then *merge stored partials* instead of decoding every segment's value column, the
//! way `TimescaleDB`'s continuous aggregates store partial aggregates and finalize them at
//! query time — but without a refresh policy, because immutability gives WeftDB the
//! never-stale invariant for free.
//!
//! This module is the persistence half: a [`PartialSidecar`] frame written beside each
//! `.weftseg` file (`{aspect}-{id}.weftpart`), holding the segment's mergeable partial at a
//! declared **base resolution** plus a staleness stamp. Only the **bounded** reductions
//! are materialized (see [`SIDECAR_AGGREGATIONS`]): the six streaming reductions carry
//! O(1) per-bucket state and the `sketch_p*` reductions carry a bounded `DdSketch`, so
//! the sidecar's size is a function of the *distinct base buckets*, never the sample
//! count. The exact percentiles and time-weighted averages need the whole bucket
//! materialized, so they stay a decode-time reduction and are deliberately absent here.
//!
//! **Staleness is handled by construction, not by a refresh policy.** The frame records
//! the `(row_count, byte_len)` of the exact segment bytes it was built from; a reader
//! ([`PartialSidecar::matches`]) compares that against the live [`SegmentDescriptor`] and
//! ignores the sidecar on any mismatch, falling back to a full decode. So a segment that
//! is later rewritten (an out-of-order reconcile, a dedup merge) simply stops matching its
//! stale sidecar — the read is always correct, at worst un-accelerated, until the sidecar
//! is regenerated.
//!
//! *(src: partials stored + finalized at query time —
//! <https://www.tigerdata.com/learn/continuous-aggregates-timescaledb>)*

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use splimes::Resolution;
use weft_physical_type::SegmentDescriptor;
use weft_reduce::{grids_nest, Aggregation, PartialReduction, ReduceError};

/// Magic prefix identifying a `.weftpart` frame — guards against feeding a foreign or
/// truncated file to [`PartialSidecar::from_bytes`].
const PARTIAL_SIDECAR_MAGIC: &[u8; 8] = b"WEFTPRT\x01";

/// The frame layout version. Bumped if the on-disk shape changes incompatibly; a reader
/// that sees a newer version treats the sidecar as absent rather than misreading it.
///
/// **v2** added the coarser [`rollups`](PartialSidecar::rollups) tier chain. A v1 frame
/// (base only) fails the version check and is treated as absent — correct, because a
/// sidecar is a pure accelerator (regenerated on the next seal, or the read simply
/// decodes), so no committed data depends on it.
pub const PARTIAL_SIDECAR_VERSION: u16 = 2;

/// The **bounded** reductions a sidecar materializes — every [`Aggregation`] whose
/// per-bucket state stays constant-sized ([`Aggregation::is_sidecar_materializable`]).
///
/// The six streaming reductions plus the four `sketch_p*` quantiles. A single
/// `DdSketch`(weft_reduce::DdSketch) per bucket serves all four sketch quantiles, so
/// including all four costs no extra state — the partial built from this set can finish
/// *any* subset of these later. `avg` adds no state of its own (it is `sum / count` at
/// finish), so it rides along for free.
pub const SIDECAR_AGGREGATIONS: [Aggregation; 10] = [Aggregation::Min, Aggregation::Max, Aggregation::Avg, Aggregation::Sum, Aggregation::First, Aggregation::Last, Aggregation::SketchP50, Aggregation::SketchP90, Aggregation::SketchP95, Aggregation::SketchP99];

/// Default minimum row count below which a segment gets no sidecar — a tiny segment's
/// partial saves too little decode to be worth the extra file.
pub const DEFAULT_PARTIAL_SIDECAR_MIN_ROWS: usize = 4096;

/// The most coarser rollup tiers a sidecar materializes beside its base.
///
/// Bounds the [`PartialSidecarPolicy`]'s tier array so the policy stays `Copy`. Four is
/// ample for a realistic hierarchy (e.g. `minutes` base → `hours` → `days` → `weeks` →
/// `months`).
pub const MAX_SIDECAR_TIERS: usize = 4;

/// Build the coarser rollup tiers of `base_partial` (bucketed at `base`) for each
/// resolution in `tiers`, ordered fine→coarse, **each re-keyed from the tier below** —
/// the hierarchy `TimescaleDB` (continuous aggregates on continuous aggregates) and
/// `ClickHouse` (`raw→hourly→daily` via `-State`/`-Merge`) both converge on. Building tier
/// `i` from tier `i-1` rather than from the base is exact (re-keying is associative over
/// nesting grids) and cheaper — each step folds only the previous tier's buckets.
///
/// A declared tier that is not strictly coarser-and-nesting than the previous grid is
/// **skipped**, so the returned chain is a strictly-coarsening prefix of `tiers`. Pure;
/// used at seal time.
///
/// # Errors
///
/// Propagates a [`ReduceError`] from a re-key (a bucket-start overflow at an extreme
/// resolution/magnitude).
fn build_rollups(base: Resolution, base_partial: &PartialReduction, tiers: &[Resolution]) -> Result<Vec<(Resolution, PartialReduction)>, ReduceError> {
	let mut rollups: Vec<(Resolution, PartialReduction)> = Vec::new();
	let mut prev_res = base;
	for &tier in tiers {
		// Only a strictly-coarser grid whose boundaries the previous grid nests inside can
		// be rolled up without re-reading data; anything else is skipped.
		if tier == prev_res || !grids_nest(prev_res, tier) {
			continue;
		}
		// Borrow the previous partial (base, or the last rollup) for the re-key, then drop
		// the borrow before pushing.
		let rolled = {
			let prev_partial = rollups.last().map_or(base_partial, |(_, p)| p);
			prev_partial.rebucket(prev_res, tier)?
		};
		if let Some(rolled) = rolled {
			rollups.push((tier, rolled));
			prev_res = tier;
		}
	}
	Ok(rollups)
}

/// A persisted per-segment partial reduction: the segment's mergeable state at a base
/// resolution, plus the staleness stamp that ties it to the exact bytes it was built from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartialSidecar {
	/// The frame version (see [`PARTIAL_SIDECAR_VERSION`]).
	pub version: u16,
	/// The resolution the partial's buckets are aligned to — the finest grid the sidecar
	/// can answer a downsample at (a coarser requested resolution re-buckets from it).
	pub base: Resolution,
	/// Total rows in the segment this partial was built from — half the staleness stamp.
	pub seg_row_count: u64,
	/// On-disk byte length of the `.weftseg` frame this partial was built from — the other
	/// half of the staleness stamp. Together with `seg_row_count` it pins the sidecar to
	/// the exact segment bytes.
	pub seg_byte_len: u64,
	/// The materialized mergeable partial at [`base`](Self::base) resolution (built with
	/// [`SIDECAR_AGGREGATIONS`]).
	pub partial: PartialReduction,
	/// Coarser materialized rollup tiers of [`partial`](Self::partial), ordered fine→coarse,
	/// each a `rebucket` of the tier below (see `build_rollups`). Empty for the historical
	/// single-base sidecar. A coarse downsample re-keys from the **coarsest** tier that nests
	/// in it ([`partial_for`](Self::partial_for)), folding far fewer buckets than re-keying
	/// the finest base — the in-storage analogue of hierarchical continuous aggregates.
	pub rollups: Vec<(Resolution, PartialReduction)>,
}

impl PartialSidecar {
	/// Wrap a freshly built `partial` (at `base` resolution) with the staleness stamp for
	/// the segment `descriptor` it was computed from, carrying no coarser rollup tiers (the
	/// historical single-base sidecar). Use [`with_rollups`](Self::with_rollups) to
	/// materialize a tier hierarchy.
	#[must_use]
	pub const fn new(base: Resolution, descriptor: &SegmentDescriptor, partial: PartialReduction) -> Self {
		Self { version: PARTIAL_SIDECAR_VERSION, base, seg_row_count: descriptor.row_count as u64, seg_byte_len: descriptor.byte_len, partial, rollups: Vec::new() }
	}

	/// Wrap a freshly built base `partial` (at `base`) together with its coarser `rollups`
	/// (fine→coarse, built by `build_rollups`) and the staleness stamp for `descriptor`.
	#[must_use]
	pub const fn with_rollups(base: Resolution, descriptor: &SegmentDescriptor, partial: PartialReduction, rollups: Vec<(Resolution, PartialReduction)>) -> Self {
		Self { version: PARTIAL_SIDECAR_VERSION, base, seg_row_count: descriptor.row_count as u64, seg_byte_len: descriptor.byte_len, partial, rollups }
	}

	/// Build a sidecar for `descriptor`'s segment from its base `partial`, materializing the
	/// coarser rollup `tiers` (each re-keyed from the tier below). A convenience over
	/// `build_rollups` + [`with_rollups`](Self::with_rollups).
	///
	/// # Errors
	///
	/// Propagates a [`ReduceError`] from building a rollup tier.
	pub fn materialize(base: Resolution, descriptor: &SegmentDescriptor, partial: PartialReduction, tiers: &[Resolution]) -> Result<Self, ReduceError> {
		let rollups = build_rollups(base, &partial, tiers)?;
		Ok(Self::with_rollups(base, descriptor, partial, rollups))
	}

	/// The [`PartialReduction`] this sidecar contributes to a downsample at `resolution`, or
	/// `None` when no materialized grid can serve it (the requested resolution is finer than
	/// the base, or does not nest in it — the caller then decodes the segment).
	///
	/// Picks the **coarsest** materialized grid (the base or a rollup tier) that equals or
	/// nests in `resolution`, so a coarse query re-keys from the fewest buckets: an exact
	/// tier is served with no re-key at all, otherwise the coarsest nesting grid is
	/// re-bucketed up. This is why the tiers are worth materializing — a `days` query over a
	/// `minutes`-base segment folds the `days` (or `hours`) tier, not thousands of minute
	/// buckets.
	///
	/// # Errors
	///
	/// Propagates a [`ReduceError`] from the re-key (a bucket-start overflow at an extreme
	/// resolution/magnitude).
	pub fn partial_for(&self, resolution: Resolution) -> Result<Option<PartialReduction>, ReduceError> {
		// Materialized grids fine→coarse: base, then the rollup chain. Scan for the coarsest
		// that can serve `resolution`; because the iteration is fine→coarse, the last grid
		// that nests is the coarsest one (fewest source buckets to re-key).
		let grids = std::iter::once((self.base, &self.partial)).chain(self.rollups.iter().map(|(r, p)| (*r, p)));
		let mut best: Option<(Resolution, &PartialReduction)> = None;
		for (res, partial) in grids {
			if res == resolution {
				// An exact tier — the stored partial IS the answer's grid, no re-key.
				return Ok(Some(partial.clone()));
			}
			if grids_nest(res, resolution) {
				best = Some((res, partial));
			}
		}
		best.map(|(res, partial)| partial.rebucket(res, resolution)).transpose().map(Option::flatten)
	}

	/// Serialize to a `.weftpart` frame: the magic prefix followed by the bincode body.
	///
	/// # Errors
	///
	/// Propagates a bincode encoding failure (in practice unreachable for this type).
	pub fn to_bytes(&self) -> Result<Vec<u8>> {
		let body = bincode::serialize(self).context("serializing a partial sidecar")?;
		let mut out = Vec::with_capacity(PARTIAL_SIDECAR_MAGIC.len() + body.len());
		out.extend_from_slice(PARTIAL_SIDECAR_MAGIC);
		out.extend_from_slice(&body);
		Ok(out)
	}

	/// Parse a `.weftpart` frame, validating the magic prefix and version.
	///
	/// # Errors
	///
	/// Errors on a missing/wrong magic prefix, a bincode decode failure, or a frame whose
	/// recorded version is not [`PARTIAL_SIDECAR_VERSION`] (a reader never misreads a
	/// newer/foreign frame — the caller treats the error as "no usable sidecar").
	pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
		let body = bytes.strip_prefix(PARTIAL_SIDECAR_MAGIC.as_slice()).context("not a .weftpart frame (bad magic)")?;
		let sidecar: Self = bincode::deserialize(body).context("decoding a partial sidecar")?;
		if sidecar.version != PARTIAL_SIDECAR_VERSION {
			bail!("partial sidecar version {} is not the supported {PARTIAL_SIDECAR_VERSION}", sidecar.version);
		}
		Ok(sidecar)
	}

	/// Whether this sidecar was built from exactly the segment `descriptor` names — the
	/// staleness check. A reader that gets `false` ignores the sidecar and decodes the
	/// segment instead (correct, just un-accelerated).
	#[must_use]
	pub const fn matches(&self, descriptor: &SegmentDescriptor) -> bool {
		self.seg_row_count == descriptor.row_count as u64 && self.seg_byte_len == descriptor.byte_len
	}
}

/// When to write a per-segment partial sidecar.
///
/// Mirrors [`CheckpointPolicy`](super::CheckpointPolicy): **off by default** (opt-in), so
/// an unconfigured store writes exactly the historical `.weftseg` files and nothing beside
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartialSidecarPolicy {
	/// The base resolution to materialize at, or [`None`] to write no sidecar.
	base: Option<Resolution>,
	/// Segments below this row count get no sidecar.
	min_rows: usize,
	/// The coarser rollup tiers to also materialize beside the base, in declared order
	/// (fine→coarse), padded with [`None`]. Kept as a fixed array so the policy stays
	/// `Copy`; a non-nesting/finer entry is skipped at build time (see `build_rollups`).
	tiers: [Option<Resolution>; MAX_SIDECAR_TIERS],
}

impl PartialSidecarPolicy {
	/// The disabled policy: no sidecar is ever written.
	pub const DISABLED: Self = Self { base: None, min_rows: DEFAULT_PARTIAL_SIDECAR_MIN_ROWS, tiers: [None; MAX_SIDECAR_TIERS] };

	/// A policy that materializes at `base` for segments of at least `min_rows` rows, with
	/// no coarser rollup tiers (the single-base sidecar).
	#[must_use]
	pub const fn at(base: Resolution, min_rows: usize) -> Self {
		Self { base: Some(base), min_rows, tiers: [None; MAX_SIDECAR_TIERS] }
	}

	/// A policy that materializes at `base` plus the coarser rollup `tiers` (fine→coarse;
	/// only the first [`MAX_SIDECAR_TIERS`] are kept), for segments of at least `min_rows`.
	#[must_use]
	pub fn at_tiered(base: Resolution, min_rows: usize, tiers: &[Resolution]) -> Self {
		let mut arr = [None; MAX_SIDECAR_TIERS];
		for (slot, &res) in arr.iter_mut().zip(tiers) {
			*slot = Some(res);
		}
		Self { base: Some(base), min_rows, tiers: arr }
	}

	/// Read the policy from the environment: `WEFT_SEGMENT_PARTIAL_BASE` (a resolution
	/// token — `seconds`/`minutes`/`hours`/…; absent or unparseable → [`DISABLED`](Self::DISABLED)),
	/// `WEFT_SEGMENT_PARTIAL_MIN_ROWS` (→ [`DEFAULT_PARTIAL_SIDECAR_MIN_ROWS`]), and
	/// `WEFT_SEGMENT_PARTIAL_TIERS` (a comma-separated fine→coarse list of coarser rollup
	/// resolutions, e.g. `hours,days`; unparseable entries are dropped, the first
	/// [`MAX_SIDECAR_TIERS`] are kept).
	#[must_use]
	pub fn from_env() -> Self {
		let base = std::env::var("WEFT_SEGMENT_PARTIAL_BASE").ok().and_then(|v| v.trim().parse::<Resolution>().ok());
		let min_rows = std::env::var("WEFT_SEGMENT_PARTIAL_MIN_ROWS").ok().and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(DEFAULT_PARTIAL_SIDECAR_MIN_ROWS);
		let mut tiers = [None; MAX_SIDECAR_TIERS];
		if let Ok(raw) = std::env::var("WEFT_SEGMENT_PARTIAL_TIERS") {
			let parsed = raw.split(',').filter_map(|t| t.trim().parse::<Resolution>().ok());
			for (slot, res) in tiers.iter_mut().zip(parsed) {
				*slot = Some(res);
			}
		}
		Self { base, min_rows, tiers }
	}

	/// The base resolution to materialize a `row_count`-row segment at, or [`None`] when
	/// the policy is disabled or the segment is too small.
	#[must_use]
	pub fn base_for(self, row_count: usize) -> Option<Resolution> {
		self.base.filter(|_| row_count >= self.min_rows)
	}

	/// The coarser rollup tiers this policy materializes beside the base, in declared
	/// fine→coarse order (empty for a single-base policy).
	#[must_use]
	pub fn tiers(self) -> Vec<Resolution> {
		self.tiers.into_iter().flatten().collect()
	}
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use bigdecimal::BigDecimal;
	use chrono::{TimeZone, Utc};
	use splimes::Point;
	use weft_reduce::reduce_partial;

	use super::*;

	fn pt(secs: i64, value: &str) -> Point {
		Point { timestamp: Utc.timestamp_opt(secs, 0).single().expect("valid instant"), value: BigDecimal::from_str(value).expect("valid decimal") }
	}

	fn descriptor(row_count: usize, byte_len: u64) -> SegmentDescriptor {
		SegmentDescriptor { id: 1, path: "x-1.weftseg".to_string(), format_version: 1, physical_type: None, time_unit: None, row_count, null_count: 0, time_sorted: true, min_ts: Some(0), max_ts: Some(100), min_value: None, max_value: None, byte_len }
	}

	#[test]
	fn sidecar_round_trips_and_finishes_to_the_same_buckets() {
		let points: Vec<Point> = (0..600).map(|i| pt(i64::from(i) * 30, &format!("{}.5", (i * 7) % 89))).collect();
		let partial = reduce_partial(&points, Resolution::Hours, None, None, &SIDECAR_AGGREGATIONS).expect("partial");
		let expected = reduce_partial(&points, Resolution::Hours, None, None, &SIDECAR_AGGREGATIONS).expect("partial").finish(Resolution::Hours, &SIDECAR_AGGREGATIONS).expect("finishes");

		let sidecar = PartialSidecar::new(Resolution::Hours, &descriptor(600, 12_345), partial);
		let bytes = sidecar.to_bytes().expect("serializes");
		let reloaded = PartialSidecar::from_bytes(&bytes).expect("reloads");

		assert_eq!(reloaded.version, PARTIAL_SIDECAR_VERSION);
		assert_eq!(reloaded.base, Resolution::Hours);
		assert_eq!(reloaded.seg_row_count, 600);
		assert_eq!(reloaded.seg_byte_len, 12_345);
		let got = reloaded.partial.finish(Resolution::Hours, &SIDECAR_AGGREGATIONS).expect("finishes");
		assert_eq!(got, expected, "a reloaded sidecar finishes to the same buckets a fresh reduction would");
	}

	#[test]
	fn matches_only_the_exact_segment_bytes() {
		let partial = reduce_partial(&[pt(0, "1"), pt(30, "2")], Resolution::Hours, None, None, &SIDECAR_AGGREGATIONS).expect("partial");
		let sidecar = PartialSidecar::new(Resolution::Hours, &descriptor(600, 12_345), partial);
		assert!(sidecar.matches(&descriptor(600, 12_345)), "the exact segment matches");
		assert!(!sidecar.matches(&descriptor(600, 12_346)), "a different byte length (a rewrite) does not match");
		assert!(!sidecar.matches(&descriptor(599, 12_345)), "a different row count (a dedup merge) does not match");
	}

	#[test]
	fn from_bytes_rejects_a_foreign_frame() {
		assert!(PartialSidecar::from_bytes(b"not a weftpart frame at all").is_err(), "a bad magic prefix is rejected");
		assert!(PartialSidecar::from_bytes(&[]).is_err(), "an empty buffer is rejected");
	}

	#[test]
	fn policy_is_off_by_default_and_reads_the_base() {
		assert_eq!(PartialSidecarPolicy::DISABLED.base_for(1_000_000), None, "disabled writes no sidecar");
		assert!(PartialSidecarPolicy::DISABLED.tiers().is_empty(), "disabled declares no tiers");
		let policy = PartialSidecarPolicy::at(Resolution::Minutes, 4096);
		assert_eq!(policy.base_for(4095), None, "below the row floor writes no sidecar");
		assert_eq!(policy.base_for(4096), Some(Resolution::Minutes), "at the floor writes at the base");
		assert!(policy.tiers().is_empty(), "the single-base policy declares no tiers");
	}

	#[test]
	fn tiered_policy_keeps_the_declared_tiers_up_to_the_cap() {
		let policy = PartialSidecarPolicy::at_tiered(Resolution::Minutes, 1, &[Resolution::Hours, Resolution::Days]);
		assert_eq!(policy.tiers(), vec![Resolution::Hours, Resolution::Days], "declared tiers survive in order");
		// More than MAX_SIDECAR_TIERS entries → only the first MAX_SIDECAR_TIERS are kept.
		let many = [Resolution::Hours, Resolution::Days, Resolution::Weeks, Resolution::Months, Resolution::Years];
		let capped = PartialSidecarPolicy::at_tiered(Resolution::Minutes, 1, &many);
		assert_eq!(capped.tiers().len(), MAX_SIDECAR_TIERS, "the tier array caps at MAX_SIDECAR_TIERS");
		assert_eq!(capped.tiers(), many[..MAX_SIDECAR_TIERS].to_vec(), "the first MAX_SIDECAR_TIERS tiers are kept");
	}

	/// ~50 hours of 5-minute samples — spans multiple hour and day buckets, so the base
	/// (minutes) grid holds far more buckets than the hour/day rollups.
	fn multi_day_points() -> Vec<Point> {
		(0..600).map(|i| pt(i64::from(i) * 300, &format!("{}.25", (i * 13) % 71))).collect()
	}

	#[test]
	fn materialize_builds_the_coarser_rollup_chain() {
		let points = multi_day_points();
		let base = reduce_partial(&points, Resolution::Minutes, None, None, &SIDECAR_AGGREGATIONS).expect("base partial");
		let sidecar = PartialSidecar::materialize(Resolution::Minutes, &descriptor(600, 1), base, &[Resolution::Hours, Resolution::Days]).expect("materializes tiers");
		let tiers: Vec<Resolution> = sidecar.rollups.iter().map(|(r, _)| *r).collect();
		assert_eq!(tiers, vec![Resolution::Hours, Resolution::Days], "both coarser tiers materialized, fine→coarse");
	}

	#[test]
	fn materialize_skips_finer_or_non_nesting_tiers() {
		let points = multi_day_points();
		let base = reduce_partial(&points, Resolution::Minutes, None, None, &SIDECAR_AGGREGATIONS).expect("base partial");
		// `seconds` is finer than the `minutes` base (skip); `hours` nests (keep); a second
		// `minutes` is finer than the `hours` we're now at (skip); `days` nests in `hours` (keep).
		let sidecar = PartialSidecar::materialize(Resolution::Minutes, &descriptor(600, 1), base, &[Resolution::Seconds, Resolution::Hours, Resolution::Minutes, Resolution::Days]).expect("materializes");
		let tiers: Vec<Resolution> = sidecar.rollups.iter().map(|(r, _)| *r).collect();
		assert_eq!(tiers, vec![Resolution::Hours, Resolution::Days], "only the strictly-coarsening nesting tiers survive");
	}

	#[test]
	fn partial_for_serves_every_materialized_and_rekeyed_resolution() {
		let points = multi_day_points();
		let base = reduce_partial(&points, Resolution::Minutes, None, None, &SIDECAR_AGGREGATIONS).expect("base partial");
		let sidecar = PartialSidecar::materialize(Resolution::Minutes, &descriptor(600, 1), base, &[Resolution::Hours, Resolution::Days]).expect("materializes");

		// For each of {base, a materialized tier, a re-keyed-from-a-tier resolution} the
		// sidecar must finish to exactly what a fresh single-pass reduce at that resolution
		// would — the tier hierarchy is a pure acceleration, never a different answer.
		for res in [Resolution::Minutes, Resolution::Hours, Resolution::Days, Resolution::Weeks] {
			let ground_truth = reduce_partial(&points, res, None, None, &SIDECAR_AGGREGATIONS).expect("truth partial").finish(res, &SIDECAR_AGGREGATIONS).expect("truth finishes");
			let served = sidecar.partial_for(res).expect("serves").unwrap_or_else(|| panic!("a materialized-or-coarser grid serves {res:?}")).finish(res, &SIDECAR_AGGREGATIONS).expect("served finishes");
			assert_eq!(served, ground_truth, "the tiered sidecar answers {res:?} exactly like a direct reduce");
		}
		// A resolution finer than the base cannot be served — the caller decodes instead.
		assert!(sidecar.partial_for(Resolution::Seconds).expect("no error").is_none(), "a sub-base resolution has no materialized grid");
	}

	#[test]
	fn tiered_sidecar_round_trips_the_rollup_chain() {
		let points = multi_day_points();
		let base = reduce_partial(&points, Resolution::Minutes, None, None, &SIDECAR_AGGREGATIONS).expect("base partial");
		let sidecar = PartialSidecar::materialize(Resolution::Minutes, &descriptor(600, 4_242), base, &[Resolution::Hours, Resolution::Days]).expect("materializes");
		let reloaded = PartialSidecar::from_bytes(&sidecar.to_bytes().expect("serializes")).expect("reloads");

		assert_eq!(reloaded.version, PARTIAL_SIDECAR_VERSION);
		assert_eq!(reloaded.rollups.iter().map(|(r, _)| *r).collect::<Vec<_>>(), vec![Resolution::Hours, Resolution::Days], "the rollup tiers survive a round trip");
		// A reloaded tiered sidecar still answers a coarse query exactly.
		let truth = reduce_partial(&points, Resolution::Days, None, None, &SIDECAR_AGGREGATIONS).expect("truth").finish(Resolution::Days, &SIDECAR_AGGREGATIONS).expect("finishes");
		let served = reloaded.partial_for(Resolution::Days).expect("serves").expect("a tier serves days").finish(Resolution::Days, &SIDECAR_AGGREGATIONS).expect("finishes");
		assert_eq!(served, truth, "a reloaded tiered sidecar answers days exactly");
	}
}
