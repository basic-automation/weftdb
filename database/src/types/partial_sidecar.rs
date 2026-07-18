//! # Per-segment materialized partial reductions — the `.dspart` sidecar
//!
//! A sealed segment is **immutable**, so a [`PartialReduction`] computed over it at seal
//! time can never go stale — which is exactly what makes it worth persisting. A later
//! cross-segment downsample ([`SegmentStore::downsample_range`](super::SegmentStore::downsample_range))
//! can then *merge stored partials* instead of decoding every segment's value column, the
//! way `TimescaleDB`'s continuous aggregates store partial aggregates and finalize them at
//! query time — but without a refresh policy, because immutability gives DSP the
//! never-stale invariant for free.
//!
//! This module is the persistence half: a [`PartialSidecar`] frame written beside each
//! `.dspseg` file (`{aspect}-{id}.dspart`), holding the segment's mergeable partial at a
//! declared **base resolution** plus a staleness stamp. Only the **bounded** reductions
//! are materialized (see [`SIDECAR_AGGREGATIONS`]): the six streaming reductions carry
//! O(1) per-bucket state and the `sketch_p*` reductions carry a bounded [`DdSketch`], so
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
use dsp_physical_type::SegmentDescriptor;
use dsp_reduce::{Aggregation, PartialReduction};
use serde::{Deserialize, Serialize};
use splimes::Resolution;

/// Magic prefix identifying a `.dspart` frame — guards against feeding a foreign or
/// truncated file to [`PartialSidecar::from_bytes`].
const PARTIAL_SIDECAR_MAGIC: &[u8; 8] = b"DSPART\0\x01";

/// The frame layout version. Bumped if the on-disk shape changes incompatibly; a reader
/// that sees a newer version treats the sidecar as absent rather than misreading it.
pub const PARTIAL_SIDECAR_VERSION: u16 = 1;

/// The **bounded** reductions a sidecar materializes — every [`Aggregation`] whose
/// per-bucket state stays constant-sized ([`Aggregation::is_sidecar_materializable`]).
///
/// The six streaming reductions plus the four `sketch_p*` quantiles. A single
/// [`DdSketch`](dsp_reduce::DdSketch) per bucket serves all four sketch quantiles, so
/// including all four costs no extra state — the partial built from this set can finish
/// *any* subset of these later. `avg` adds no state of its own (it is `sum / count` at
/// finish), so it rides along for free.
pub const SIDECAR_AGGREGATIONS: [Aggregation; 10] = [Aggregation::Min, Aggregation::Max, Aggregation::Avg, Aggregation::Sum, Aggregation::First, Aggregation::Last, Aggregation::SketchP50, Aggregation::SketchP90, Aggregation::SketchP95, Aggregation::SketchP99];

/// Default minimum row count below which a segment gets no sidecar — a tiny segment's
/// partial saves too little decode to be worth the extra file.
pub const DEFAULT_PARTIAL_SIDECAR_MIN_ROWS: usize = 4096;

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
	/// On-disk byte length of the `.dspseg` frame this partial was built from — the other
	/// half of the staleness stamp. Together with `seg_row_count` it pins the sidecar to
	/// the exact segment bytes.
	pub seg_byte_len: u64,
	/// The materialized mergeable partial (built with [`SIDECAR_AGGREGATIONS`]).
	pub partial: PartialReduction,
}

impl PartialSidecar {
	/// Wrap a freshly built `partial` (at `base` resolution) with the staleness stamp for
	/// the segment `descriptor` it was computed from.
	#[must_use]
	pub const fn new(base: Resolution, descriptor: &SegmentDescriptor, partial: PartialReduction) -> Self {
		Self { version: PARTIAL_SIDECAR_VERSION, base, seg_row_count: descriptor.row_count as u64, seg_byte_len: descriptor.byte_len, partial }
	}

	/// Serialize to a `.dspart` frame: the magic prefix followed by the bincode body.
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

	/// Parse a `.dspart` frame, validating the magic prefix and version.
	///
	/// # Errors
	///
	/// Errors on a missing/wrong magic prefix, a bincode decode failure, or a frame whose
	/// recorded version is not [`PARTIAL_SIDECAR_VERSION`] (a reader never misreads a
	/// newer/foreign frame — the caller treats the error as "no usable sidecar").
	pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
		let body = bytes.strip_prefix(PARTIAL_SIDECAR_MAGIC.as_slice()).context("not a .dspart frame (bad magic)")?;
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
/// an unconfigured store writes exactly the historical `.dspseg` files and nothing beside
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartialSidecarPolicy {
	/// The base resolution to materialize at, or [`None`] to write no sidecar.
	base: Option<Resolution>,
	/// Segments below this row count get no sidecar.
	min_rows: usize,
}

impl PartialSidecarPolicy {
	/// The disabled policy: no sidecar is ever written.
	pub const DISABLED: Self = Self { base: None, min_rows: DEFAULT_PARTIAL_SIDECAR_MIN_ROWS };

	/// A policy that materializes at `base` for segments of at least `min_rows` rows.
	#[must_use]
	pub const fn at(base: Resolution, min_rows: usize) -> Self {
		Self { base: Some(base), min_rows }
	}

	/// Read the policy from the environment: `DSP_SEGMENT_PARTIAL_BASE` (a resolution
	/// token — `seconds`/`minutes`/`hours`/…; absent or unparseable → [`DISABLED`](Self::DISABLED))
	/// and `DSP_SEGMENT_PARTIAL_MIN_ROWS` (→ [`DEFAULT_PARTIAL_SIDECAR_MIN_ROWS`]).
	#[must_use]
	pub fn from_env() -> Self {
		let base = std::env::var("DSP_SEGMENT_PARTIAL_BASE").ok().and_then(|v| v.trim().parse::<Resolution>().ok());
		let min_rows = std::env::var("DSP_SEGMENT_PARTIAL_MIN_ROWS").ok().and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(DEFAULT_PARTIAL_SIDECAR_MIN_ROWS);
		Self { base, min_rows }
	}

	/// The base resolution to materialize a `row_count`-row segment at, or [`None`] when
	/// the policy is disabled or the segment is too small.
	#[must_use]
	pub fn base_for(self, row_count: usize) -> Option<Resolution> {
		self.base.filter(|_| row_count >= self.min_rows)
	}
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use bigdecimal::BigDecimal;
	use chrono::{TimeZone, Utc};
	use dsp_reduce::reduce_partial;
	use splimes::Point;

	use super::*;

	fn pt(secs: i64, value: &str) -> Point {
		Point { timestamp: Utc.timestamp_opt(secs, 0).single().expect("valid instant"), value: BigDecimal::from_str(value).expect("valid decimal") }
	}

	fn descriptor(row_count: usize, byte_len: u64) -> SegmentDescriptor {
		SegmentDescriptor { id: 1, path: "x-1.dspseg".to_string(), format_version: 1, physical_type: None, time_unit: None, row_count, null_count: 0, time_sorted: true, min_ts: Some(0), max_ts: Some(100), min_value: None, max_value: None, byte_len }
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
		assert!(PartialSidecar::from_bytes(b"not a dspart frame at all").is_err(), "a bad magic prefix is rejected");
		assert!(PartialSidecar::from_bytes(&[]).is_err(), "an empty buffer is rejected");
	}

	#[test]
	fn policy_is_off_by_default_and_reads_the_base() {
		assert_eq!(PartialSidecarPolicy::DISABLED.base_for(1_000_000), None, "disabled writes no sidecar");
		let policy = PartialSidecarPolicy::at(Resolution::Minutes, 4096);
		assert_eq!(policy.base_for(4095), None, "below the row floor writes no sidecar");
		assert_eq!(policy.base_for(4096), Some(Resolution::Minutes), "at the floor writes at the base");
	}
}
