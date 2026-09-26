//! Point-lookup workload — the customer-facing capstone of the read-path arc.
//!
//! The whole 2026-07-13 read-path arc built WeftDB's streaming point read: a
//! single-instant lookup that never materializes the value column — it decodes
//! only the timestamp block to locate the row (or resolves it in closed form for a
//! regular constant-stride column), then unpacks the *one* covering value block
//! (`weftseg::read_segment_point`), and amortizes the timestamp decode across many
//! instants in one pass (`read_segment_points`). Those wins were benchmarked at the
//! codec layer (criterion `benches/pointread.rs`, ~59× single / ~27× batch / ~7×
//! regular-vs-irregular). This module lifts that into a *customer-runnable* Weft-Bench
//! workload with p50/p95/p99 latency and a correctness gate, the foundation for the
//! cross-engine point-lookup comparison (vs `ClickHouse` ASOF / `QuestDB`) and the
//! evidence for the *When WeftDB beats general TSDBs* point-lookup positioning.
//!
//! It is a **parallel** runner to [`crate::run_profile`], not an extension: the
//! interpolation `run` path is deeply spline-shaped (dense output grid, accuracy vs
//! analytic truth), while a point lookup resolves a set of instants against stored
//! state. Both emit the same serializable [`BenchResult`] (here with the
//! `point_lookup` workload label and no `accuracy`, since a lookup has no ground
//! truth beyond the stored value), so the JSON/HTML report machinery is shared.
//!
//! The storage-backed path drives WeftDB's own hot path directly: it seals a
//! `weft-physical-type` [`Segment`] from the profile dataset and times
//! `read_segment_point` / `read_segment_points` over the `.weftseg` bytes — no
//! `database`/libSQL control plane is pulled into the harness (keeping Weft-Bench a
//! vendor-neutral leaf crate), and the sealed corpus is a clustered high-base
//! `ScaledI64` column so the value block picks the Frame-of-Reference codec and the
//! streaming read takes its designed block-skip fast path.

use std::time::Instant;

use bigdecimal::{BigDecimal, ToPrimitive};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};
use weft_physical_type::{
	weftseg::{read_paged_segment_point, read_paged_segment_points, read_segment_point, read_segment_points}, PagedSegment, Segment, TimeUnit
};

use crate::{
	schema::{BenchResult, CorrectnessReport, DatasetMeta, StorageEstimate, TimingBreakdown, SCHEMA_VERSION}, stats::{BootstrapConfig, LatencyStats}
};

/// Workload class label recorded for the point-lookup workload.
const WORKLOAD_POINT_LOOKUP: &str = "point_lookup";

/// The epoch unit the point-lookup segment is sealed in. Milliseconds match the
/// codec-layer benchmark's corpus and keep the strides comfortably above 1 so a
/// deliberate off-grid miss (`ts + 1`) always lands strictly between two rows.
const SEGMENT_UNIT: TimeUnit = TimeUnit::Millis;

/// How the query batch is issued against the sealed segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LookupMode {
	/// One `read_segment_point` call per instant — every instant re-reads the frame
	/// and re-decodes the timestamp column. The un-amortized baseline.
	Single,
	/// One `read_segment_points` call for the whole batch — the timestamp column is
	/// decoded once and shared across every instant (the amortized fast path).
	Batch,
}

impl LookupMode {
	/// Stable lowercase token for reports / labels.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Single => "single",
			Self::Batch => "batch",
		}
	}
}

/// The tunable knobs for a point-lookup profile.
///
/// [`Default`] is the flagship regular (constant-stride) profile. Overriding one
/// field while keeping the rest is a `..Default::default()` away, mirroring
/// [`crate::SyntheticParams`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointLookupParams {
	/// RNG seed; publishing it regenerates the dataset + query set exactly.
	pub seed: u64,
	/// Rows sealed into the segment (the stored corpus size).
	pub point_count: usize,
	/// Instants resolved per timed rep (the query batch size).
	pub query_count: usize,
	/// Constant-stride timestamps (`true` — the closed-form regular path) vs
	/// jittered strictly-increasing timestamps (`false` — decode + binary search).
	pub regular: bool,
	/// Fraction of the query batch made deliberately off-grid (guaranteed misses),
	/// so the workload exercises the not-found path as well as hits (`0.0..=1.0`).
	pub absent_fraction: f64,
	/// How the batch is issued (single per-instant reads vs one amortized batch read).
	pub mode: LookupMode,
	/// Rows per page when sealing a **paged** segment (exercises the on-disk
	/// page-pruning read path). `0` seals a single-block segment instead.
	pub rows_per_page: usize,
}

impl Default for PointLookupParams {
	/// The flagship point-lookup knob set: 20 000 regular rows, a 128-instant batch
	/// read with 25% deliberate misses, sealed as a single-block segment.
	fn default() -> Self {
		Self { seed: DEFAULT_POINT_LOOKUP_SEED, point_count: 20_000, query_count: 128, regular: true, absent_fraction: 0.25, mode: LookupMode::Batch, rows_per_page: 0 }
	}
}

/// Default seed for point-lookup profiles. Published in results so the dataset and
/// query set regenerate byte-for-byte.
pub const DEFAULT_POINT_LOOKUP_SEED: u64 = 0x00DB_5EED_1004;

/// The base epoch (ms) the corpus starts at. A non-zero anchor keeps the stored
/// timestamps stable and machine-independent, as the reproducibility rules require.
const EPOCH_ANCHOR_MS: i64 = 1_000;

/// The constant stride (ms) between rows of a regular corpus. Above 1 so a
/// `ts + 1` off-grid query is always strictly between two rows.
const REGULAR_STRIDE_MS: i64 = 10;

/// A reproducible point-lookup workload profile.
///
/// The dataset is a sorted, clustered high-base `ScaledI64` column (so the value
/// block picks the Frame-of-Reference codec and the streaming read takes its
/// block-skip fast path); the timestamps are either constant-stride (regular) or
/// deterministically jittered (irregular). The workload seals the column into a
/// `.weftseg` segment and resolves `query_count` instants against it.
#[derive(Debug, Clone, PartialEq)]
pub struct PointLookupProfile {
	/// Human-readable profile name, recorded in results.
	pub name: String,
	/// RNG seed; publishing this regenerates the dataset + queries exactly.
	pub seed: u64,
	/// Rows sealed into the segment.
	pub point_count: usize,
	/// Instants resolved per timed rep.
	pub query_count: usize,
	/// Constant-stride (regular) vs jittered (irregular) timestamps.
	pub regular: bool,
	/// Fraction of the query batch made deliberately off-grid (`0.0..=1.0`).
	pub absent_fraction: f64,
	/// How the batch is issued.
	pub mode: LookupMode,
	/// Rows per page for a paged segment; `0` seals a single-block segment.
	pub rows_per_page: usize,
}

impl PointLookupProfile {
	/// The flagship **regular** (constant-stride) point-lookup profile — the case a
	/// closed-form row index resolves without materializing the timestamp column.
	#[must_use]
	pub fn regular(name: impl Into<String>) -> Self {
		Self::new(name, PointLookupParams::default())
	}

	/// The **irregular** (jittered, strictly-increasing) point-lookup profile — the
	/// case the streaming read must decode + binary-search the timestamp column.
	#[must_use]
	pub fn irregular(name: impl Into<String>) -> Self {
		Self::new(name, PointLookupParams { regular: false, ..PointLookupParams::default() })
	}

	/// Build a point-lookup profile from an explicit [`PointLookupParams`] knob set.
	#[must_use]
	pub fn new(name: impl Into<String>, params: PointLookupParams) -> Self {
		let PointLookupParams { seed, point_count, query_count, regular, absent_fraction, mode, rows_per_page } = params;
		Self { name: name.into(), seed, point_count: point_count.max(2), query_count: query_count.max(1), regular, absent_fraction: absent_fraction.clamp(0.0, 1.0), mode, rows_per_page }
	}

	/// Generate the seeded, sorted `(timestamps, values)` corpus.
	///
	/// The values are a clustered high-base 2-decimal `ScaledI64` corpus (the
	/// Frame-of-Reference regime); the timestamps are constant-stride when
	/// [`Self::regular`], otherwise deterministically jittered with a strictly-positive
	/// gap of at least 2 (so a `ts + 1` query always misses between two rows).
	///
	/// # Panics
	///
	/// Panics only if the compiled-in decimal literal fails to parse, which cannot
	/// happen for the fixed format string.
	#[must_use]
	pub fn generate(&self) -> (Vec<i64>, Vec<BigDecimal>) {
		let n = self.point_count.max(2);
		let values: Vec<BigDecimal> = (0..n).map(|i| format!("10000000.{:02}", i % 97).parse().expect("literal decimal parses")).collect();

		let timestamps: Vec<i64> = if self.regular {
			// `n` is a row count (never near `i64::MAX`), so the index cast cannot wrap.
			#[allow(clippy::cast_possible_wrap)]
			(0..n as i64).map(|i| EPOCH_ANCHOR_MS + i * REGULAR_STRIDE_MS).collect()
		} else {
			// Deterministic jitter: a strictly-increasing series whose gaps are in
			// `[2, 21]`, so it is genuinely irregular yet a `ts + 1` off-grid query is
			// always strictly between two adjacent rows.
			let mut rng = ChaCha8Rng::seed_from_u64(self.seed ^ 0x_1F0E_2D3C_4B5A_6978);
			let mut cur = EPOCH_ANCHOR_MS;
			(0..n).map(|_| {
				cur += 2 + i64::from(rng.random::<u8>() % 20);
				cur
			})
			.collect()
		};

		(timestamps, values)
	}

	/// Build the seeded query batch against a generated `timestamps` slice.
	///
	/// A `(1 - absent_fraction)` share of the batch are present instants (exact stored
	/// timestamps, spread across the segment) and the rest are deliberate off-grid
	/// misses (`ts + 1`, strictly between two rows). The ordering is shuffled by the
	/// seed so the batch is not monotonic (a realistic scattered access pattern).
	///
	/// # Panics
	///
	/// Panics if `timestamps` has fewer than two rows (a lookup corpus needs at least
	/// two), which [`Self::generate`] never produces.
	#[must_use]
	pub fn queries(&self, timestamps: &[i64]) -> Vec<i64> {
		assert!(timestamps.len() >= 2, "a point-lookup corpus needs at least two rows");
		let mut rng = ChaCha8Rng::seed_from_u64(self.seed ^ 0x_C0DE_5EED_A11A_B0BA);
		let n = timestamps.len();
		#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
		let absent = ((self.query_count as f64) * self.absent_fraction).round() as usize;
		let present = self.query_count.saturating_sub(absent);

		let mut queries: Vec<i64> = Vec::with_capacity(self.query_count);
		// Present instants: exact stored timestamps at pseudo-random rows.
		for _ in 0..present {
			queries.push(timestamps[rng.random_range(0..n)]);
		}
		// Absent instants: `ts + 1` for a non-last row is strictly between it and the
		// next, so it can never be a stored timestamp (all gaps are >= 2 / stride 10).
		for _ in 0..absent {
			let i = rng.random_range(0..n - 1);
			queries.push(timestamps[i] + 1);
		}
		// Shuffle so the batch is a scattered access pattern, not present-then-absent.
		for i in (1..queries.len()).rev() {
			queries.swap(i, rng.random_range(0..=i));
		}
		queries
	}
}

/// Elapsed nanoseconds since `since`, saturated into a `u64`.
#[allow(clippy::cast_possible_truncation)]
fn span_ns(since: Instant) -> u64 {
	since.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

/// Resolve every query in `queries` against `bytes` in the requested `mode`.
///
/// `Single` issues one point read per instant; `Batch` issues one batch read for the
/// whole slice. `paged` selects the paged-segment readers (page pruning) over the
/// single-block readers. Both return values aligned to `queries`.
fn resolve(bytes: &[u8], queries: &[i64], mode: LookupMode, paged: bool) -> anyhow::Result<Vec<Option<BigDecimal>>> {
	match (mode, paged) {
		(LookupMode::Single, false) => queries.iter().map(|&t| read_segment_point(bytes, t).map_err(anyhow::Error::from)).collect(),
		(LookupMode::Batch, false) => read_segment_points(bytes, queries).map_err(anyhow::Error::from),
		(LookupMode::Single, true) => queries.iter().map(|&t| read_paged_segment_point(bytes, t).map_err(anyhow::Error::from)).collect(),
		(LookupMode::Batch, true) => read_paged_segment_points(bytes, queries).map_err(anyhow::Error::from),
	}
}

/// Run a point-lookup profile for `reps` timed repetitions and return a
/// fully-populated [`BenchResult`] (workload `point_lookup`).
///
/// The corpus is generated and sealed **once** (setup, excluded from latency); each
/// rep re-resolves the whole query batch against the sealed `.weftseg` bytes and the
/// timed span covers only that resolution. Correctness is gated against the
/// full-decode ground truth: the streaming read of every instant must equal the
/// in-memory [`Segment::value_at`] (the same guard the codec-layer benchmark uses),
/// so a codec or read-path regression fails the run rather than silently skewing a
/// number.
///
/// # Errors
///
/// Returns an error if `reps == 0`, if the corpus cannot be sealed into a segment,
/// or if any rep's read fails.
pub fn run_point_lookup(profile: &PointLookupProfile, reps: usize) -> anyhow::Result<BenchResult> {
	anyhow::ensure!(reps > 0, "reps must be > 0");

	// The end-to-end span covers the whole timed function; the setup (dataset gen +
	// seal + query build) is measured separately and excluded from latency, exactly
	// as `run_profile` excludes dataset generation.
	let run_start = Instant::now();

	let setup_start = Instant::now();
	let (timestamps, values) = profile.generate();
	let queries = profile.queries(&timestamps);
	let paged = profile.rows_per_page > 0;
	// Seal the corpus once (single-block or paged) and compute the full-decode ground
	// truth for the correctness gate from the same in-memory segment. `value_at`
	// searches the already-decoded segment, so the reference is cheap and independent
	// of the streaming read path under test.
	let (bytes, expected): (Vec<u8>, Vec<Option<BigDecimal>>) = if paged {
		let segment = PagedSegment::build(&timestamps, &values, SEGMENT_UNIT, &BigDecimal::from(0), profile.rows_per_page).map_err(|e| anyhow::anyhow!("cannot seal paged point-lookup segment: {e:?}"))?;
		let expected = queries.iter().map(|&t| segment.value_at(t)).collect();
		(segment.write_to(), expected)
	} else {
		let segment = Segment::build_sorted(&timestamps, &values, SEGMENT_UNIT, &BigDecimal::from(0)).map_err(|e| anyhow::anyhow!("cannot seal point-lookup segment: {e:?}"))?;
		let expected = queries.iter().map(|&t| segment.value_at(t)).collect();
		(segment.write_to(), expected)
	};
	let setup_ns = span_ns(setup_start);

	let mut samples_ns: Vec<u64> = Vec::with_capacity(reps);
	let mut last_output: Vec<Option<BigDecimal>> = Vec::new();
	for _ in 0..reps {
		let t0 = Instant::now();
		let output = resolve(&bytes, &queries, profile.mode, paged)?;
		samples_ns.push(span_ns(t0));
		last_output = output;
	}

	let measured_ns = samples_ns.iter().copied().fold(0_u64, u64::saturating_add);
	let end_to_end_ns = span_ns(run_start);
	let timing = TimingBreakdown { dataset_generation_ns: setup_ns, measured_ns, end_to_end_ns };

	// Correctness: the streaming read must reproduce the full-decode value for every
	// instant (hits *and* misses), and every found value must be finite.
	let streaming_matches_truth = last_output == expected;
	let resolved_count = last_output.len();
	let values_finite = last_output.iter().flatten().all(|v| v.to_f64().is_some_and(f64::is_finite));
	let correctness = CorrectnessReport { output_count_ok: resolved_count == queries.len() && streaming_matches_truth, expected_output_points: queries.len(), actual_output_points: resolved_count, values_finite };

	// North-star storage term for the *stored* corpus (value + timestamp columns),
	// the same figure a sealed segment reports — the bytes/point the point-lookup
	// latency is amortized against.
	let storage = Some(StorageEstimate::from_columns(&values, &timestamps, SEGMENT_UNIT, &BigDecimal::from(0)));

	let latency = LatencyStats::from_samples(&samples_ns);
	let latency_ci = Some(LatencyStats::bootstrap_cis(&samples_ns, &BootstrapConfig { seed: profile.seed ^ 0x_C0FF_EE15_C0DE_u64, ..BootstrapConfig::default() }));
	// Throughput is resolved instants per second (a "point" here is one lookup).
	let throughput_points_per_sec = if latency.mean_ns > 0 {
		#[allow(clippy::cast_precision_loss)]
		let secs = latency.mean_ns as f64 / 1e9;
		#[allow(clippy::cast_precision_loss)]
		let points = queries.len() as f64;
		points / secs
	} else {
		0.0
	};

	Ok(BenchResult { schema_version: SCHEMA_VERSION, profile: profile.name.clone(), adapter: "weftdb".to_string(), workload: WORKLOAD_POINT_LOOKUP.to_string(), reps, dataset: DatasetMeta { input_points: timestamps.len(), output_points: queries.len(), irregular: !profile.regular, missingness_fraction: 0.0, seed: profile.seed, signal_shape: None }, latency, latency_ci, throughput_points_per_sec, timing, correctness, accuracy: None, storage })
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A small profile so the correctness/round-trip tests stay fast.
	fn small(regular: bool, mode: LookupMode) -> PointLookupProfile {
		PointLookupProfile::new(if regular { "pl-regular-small" } else { "pl-irregular-small" }, PointLookupParams { point_count: 2_000, query_count: 64, regular, mode, ..PointLookupParams::default() })
	}

	#[test]
	fn generation_is_deterministic_for_a_seed() {
		let profile = small(false, LookupMode::Batch);
		assert_eq!(profile.generate(), profile.generate(), "same seed -> identical corpus");
		let (ts, _) = profile.generate();
		assert_eq!(profile.queries(&ts), profile.queries(&ts), "same seed -> identical query batch");
	}

	#[test]
	fn corpus_is_sorted_and_strictly_increasing_for_both_shapes() {
		for regular in [true, false] {
			let (ts, vals) = small(regular, LookupMode::Batch).generate();
			assert_eq!(ts.len(), vals.len());
			assert!(ts.len() >= 2);
			for w in ts.windows(2) {
				assert!(w[0] < w[1], "timestamps must be strictly increasing ({regular})");
			}
		}
	}

	#[test]
	fn queries_split_present_and_absent_as_configured() {
		let profile = PointLookupProfile::new("split", PointLookupParams { point_count: 1_000, query_count: 100, absent_fraction: 0.25, regular: true, ..PointLookupParams::default() });
		let (ts, _) = profile.generate();
		let stored: std::collections::HashSet<i64> = ts.iter().copied().collect();
		let queries = profile.queries(&ts);
		assert_eq!(queries.len(), 100);
		let present = queries.iter().filter(|q| stored.contains(q)).count();
		let absent = queries.len() - present;
		// 25% of 100 -> 25 deliberate misses, 75 present.
		assert_eq!(absent, 25, "off-grid misses must match the absent fraction");
		assert_eq!(present, 75, "the rest must be present instants");
	}

	#[test]
	fn regular_run_passes_correctness_and_carries_storage_and_latency() {
		let profile = small(true, LookupMode::Batch);
		let result = run_point_lookup(&profile, 3).expect("regular point-lookup run succeeds");

		assert_eq!(result.adapter, "weftdb");
		assert_eq!(result.workload, "point_lookup");
		assert_eq!(result.profile, "pl-regular-small");
		assert_eq!(result.reps, 3);
		assert_eq!(result.latency.count, 3);
		assert_eq!(result.dataset.input_points, 2_000);
		assert_eq!(result.dataset.output_points, 64, "one resolved value per query");
		assert!(!result.dataset.irregular, "a regular profile is not irregular");
		assert!(result.accuracy.is_none(), "a point lookup has no analytic ground truth");
		assert!(result.correctness.passed(), "correctness must pass: {:?}", result.correctness);
		assert!(result.is_publishable(), "a passing run must be publishable");
		assert!(result.throughput_points_per_sec > 0.0, "throughput must be positive");
		assert!(result.timing.measured_ns > 0, "the measured span must be non-zero");
		assert!(result.timing.end_to_end_ns >= result.timing.dataset_generation_ns + result.timing.measured_ns, "end-to-end must cover setup + measured: {:?}", result.timing);

		let storage = result.storage.expect("a point-lookup run carries a storage estimate");
		assert_eq!(storage.physical_type, "scaled_i64", "the clustered high-base corpus stores as scaled_i64");
		assert_eq!(storage.value_codec, "scaled_for", "the clustered mantissas realize the FOR value codec (streaming block-skip path)");
	}

	#[test]
	fn irregular_run_passes_correctness() {
		let result = run_point_lookup(&small(false, LookupMode::Batch), 3).expect("irregular point-lookup run succeeds");
		assert_eq!(result.workload, "point_lookup");
		assert!(result.dataset.irregular, "an irregular profile is marked irregular");
		assert!(result.correctness.passed(), "correctness must pass on an irregular corpus: {:?}", result.correctness);
		assert!(result.is_publishable());
	}

	#[test]
	fn single_and_batch_modes_resolve_identically() {
		// The two issue modes are timed differently but must resolve to the same
		// values — the amortized batch read is an optimization, never a behavior change.
		let (ts, vals) = small(true, LookupMode::Batch).generate();
		let segment = Segment::build_sorted(&ts, &vals, SEGMENT_UNIT, &BigDecimal::from(0)).expect("seals");
		let bytes = segment.write_to();
		let queries = small(true, LookupMode::Batch).queries(&ts);
		let single = resolve(&bytes, &queries, LookupMode::Single, false).expect("single resolves");
		let batch = resolve(&bytes, &queries, LookupMode::Batch, false).expect("batch resolves");
		assert_eq!(single, batch, "single and batch reads must agree");
		// And both must equal the full-decode ground truth.
		let truth: Vec<Option<BigDecimal>> = queries.iter().map(|&t| segment.value_at(t)).collect();
		assert_eq!(batch, truth, "the streaming read must equal value_at for every instant");
	}

	#[test]
	fn absent_queries_resolve_to_none_and_still_pass_correctness() {
		// A profile that is entirely off-grid misses: every resolved value is None, the
		// streaming read still matches value_at (also None), and correctness passes.
		let profile = PointLookupProfile::new("all-absent", PointLookupParams { point_count: 500, query_count: 32, absent_fraction: 1.0, regular: true, ..PointLookupParams::default() });
		let result = run_point_lookup(&profile, 2).expect("all-absent run succeeds");
		assert!(result.correctness.passed(), "an all-miss batch must still pass: {:?}", result.correctness);
		assert!(result.is_publishable());
	}

	#[test]
	fn paged_run_passes_correctness_over_the_page_pruning_read_path() {
		// A paged segment (rows_per_page > 0) drives the paged readers; the streaming
		// paged read must still equal the full-decode value_at for every instant, on
		// both corpus shapes and both issue modes.
		for regular in [true, false] {
			for mode in [LookupMode::Single, LookupMode::Batch] {
				let profile = PointLookupProfile::new("pl-paged", PointLookupParams { point_count: 3_000, query_count: 48, regular, mode, rows_per_page: 512, ..PointLookupParams::default() });
				let result = run_point_lookup(&profile, 2).expect("paged point-lookup run succeeds");
				assert!(result.correctness.passed(), "paged correctness must pass (regular={regular}, mode={mode:?}): {:?}", result.correctness);
				assert!(result.is_publishable());
			}
		}
	}

	#[test]
	fn zero_reps_is_rejected() {
		assert!(run_point_lookup(&small(true, LookupMode::Batch), 0).is_err(), "zero reps must be rejected");
	}

	#[test]
	fn result_round_trips_through_json() {
		let result = run_point_lookup(&small(true, LookupMode::Batch), 3).expect("run succeeds");
		let json = serde_json::to_string(&result).expect("serialize");
		let back: BenchResult = serde_json::from_str(&json).expect("deserialize");
		assert_eq!(result.workload, back.workload);
		assert_eq!(result.dataset, back.dataset);
		assert_eq!(result.latency, back.latency);
		assert_eq!(result.timing, back.timing);
		assert_eq!(result.correctness, back.correctness);
		assert!(back.accuracy.is_none(), "a point-lookup result carries no accuracy");
		assert!(back.storage.is_some());
	}
}
