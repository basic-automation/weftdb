//! Range-fetch workload — the windowed range read as a customer-runnable benchmark.
//!
//! Beside the streaming point read, the 2026-07-13 read-path arc shipped the
//! **windowed range read**: `dspseg::read_segment_range` returns only the rows in a
//! `[start, end]` window, and for a regular constant-stride column with a
//! random-access value codec it computes the row window in closed form and unpacks
//! **only the present values inside it** — decoding ~`window` values instead of the
//! whole segment (benchmarked ~20× at the codec layer). This module lifts that into
//! a DSP-Bench workload (`range_fetch`), the roadmap's pending "raw range fetch"
//! profile, with p50/p95/p99 latency and a correctness gate.
//!
//! It is the sibling of [`crate::point_lookup`]: same seeded clustered high-base
//! `ScaledI64` corpus (so the value block realizes the Frame-of-Reference codec and
//! the windowed read takes its closed-form fast path), same parallel-runner shape,
//! same serializable [`BenchResult`] (workload `range_fetch`, no `accuracy` — a
//! range read materializes stored values, not a reconstruction). The correctness
//! gate is the same the codec-layer bench uses: the windowed read must equal a full
//! decode filtered to the window, for every window.

use std::time::Instant;

use bigdecimal::{BigDecimal, ToPrimitive};
use dsp_physical_type::{
	dspseg::{read_paged_segment_range, read_segment_range}, PagedSegment, Segment, TimeUnit
};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

use crate::{
	schema::{BenchResult, CorrectnessReport, DatasetMeta, StorageEstimate, TimingBreakdown, SCHEMA_VERSION}, stats::{BootstrapConfig, LatencyStats}
};

/// Workload class label recorded for the range-fetch workload.
const WORKLOAD_RANGE_FETCH: &str = "range_fetch";

/// One fetched window's decoded rows: aligned `(timestamps, values)`.
type WindowRows = (Vec<i64>, Vec<Option<BigDecimal>>);

/// The epoch unit the range-fetch segment is sealed in (matches the point-lookup
/// corpus so both workloads share a stride convention).
const SEGMENT_UNIT: TimeUnit = TimeUnit::Millis;

/// The base epoch (ms) the corpus starts at.
const EPOCH_ANCHOR_MS: i64 = 1_000;

/// The constant stride (ms) between rows of a regular corpus.
const REGULAR_STRIDE_MS: i64 = 10;

/// The tunable knobs for a range-fetch profile.
///
/// [`Default`] is the flagship regular (constant-stride) profile — the case the
/// windowed read resolves in closed form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeFetchParams {
	/// RNG seed; publishing it regenerates the dataset + windows exactly.
	pub seed: u64,
	/// Rows sealed into the segment (the stored corpus size).
	pub point_count: usize,
	/// Rows spanned by each range window (the selectivity of the fetch).
	pub window_rows: usize,
	/// Number of windows fetched per timed rep.
	pub window_count: usize,
	/// Constant-stride timestamps (`true` — closed-form window) vs jittered
	/// strictly-increasing timestamps (`false` — decode + filter).
	pub regular: bool,
	/// Rows per page when sealing a **paged** segment (exercises the on-disk
	/// page-skipping range read). `0` seals a single-block segment instead.
	pub rows_per_page: usize,
}

impl Default for RangeFetchParams {
	/// The flagship range-fetch knob set: a 20 000-row segment, 32 windows of 100
	/// rows each (a selective range over a large segment — the windowed read's
	/// regime), sealed as a single-block segment.
	fn default() -> Self {
		Self { seed: DEFAULT_RANGE_FETCH_SEED, point_count: 20_000, window_rows: 100, window_count: 32, regular: true, rows_per_page: 0 }
	}
}

/// Default seed for range-fetch profiles.
pub const DEFAULT_RANGE_FETCH_SEED: u64 = 0x00DB_5EED_2A09;

/// A reproducible range-fetch workload profile.
///
/// The dataset is the same sorted clustered high-base `ScaledI64` corpus as the
/// point-lookup workload (so the value block realizes the FOR codec); the workload
/// seals it into a `.dspseg` segment and fetches `window_count` selective
/// `[start, end]` windows against it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeFetchProfile {
	/// Human-readable profile name, recorded in results.
	pub name: String,
	/// RNG seed; publishing this regenerates the dataset + windows exactly.
	pub seed: u64,
	/// Rows sealed into the segment.
	pub point_count: usize,
	/// Rows spanned by each range window.
	pub window_rows: usize,
	/// Number of windows fetched per timed rep.
	pub window_count: usize,
	/// Constant-stride (regular) vs jittered (irregular) timestamps.
	pub regular: bool,
	/// Rows per page for a paged segment; `0` seals a single-block segment.
	pub rows_per_page: usize,
}

impl RangeFetchProfile {
	/// The flagship **regular** (constant-stride) range-fetch profile — the case the
	/// windowed read resolves the row window in closed form.
	#[must_use]
	pub fn regular(name: impl Into<String>) -> Self {
		Self::new(name, RangeFetchParams::default())
	}

	/// The **irregular** (jittered, strictly-increasing) range-fetch profile — the
	/// case the windowed read must decode + filter the timestamp column.
	#[must_use]
	pub fn irregular(name: impl Into<String>) -> Self {
		Self::new(name, RangeFetchParams { regular: false, ..RangeFetchParams::default() })
	}

	/// Build a range-fetch profile from an explicit [`RangeFetchParams`] knob set.
	#[must_use]
	pub fn new(name: impl Into<String>, params: RangeFetchParams) -> Self {
		let RangeFetchParams { seed, point_count, window_rows, window_count, regular, rows_per_page } = params;
		Self { name: name.into(), seed, point_count: point_count.max(2), window_rows: window_rows.max(1), window_count: window_count.max(1), regular, rows_per_page }
	}

	/// Generate the seeded, sorted `(timestamps, values)` corpus (identical shape to
	/// the point-lookup corpus: a clustered high-base 2-decimal `ScaledI64` column,
	/// constant-stride when [`Self::regular`], otherwise deterministically jittered).
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
			let mut rng = ChaCha8Rng::seed_from_u64(self.seed ^ 0x_1F0E_2D3C_4B5A_6978);
			let mut cur = EPOCH_ANCHOR_MS;
			(0..n).map(|_| {
				cur += 2 + i64::from(rng.random::<u8>() % 20);
				cur
			}).collect()
		};
		(timestamps, values)
	}

	/// Build the seeded set of `[start, end]` fetch windows against a generated
	/// `timestamps` slice. Each window spans [`Self::window_rows`] rows starting at a
	/// pseudo-random row, expressed as the `[ts[lo], ts[hi]]` timestamp bounds (so it
	/// is a real time-range query, not a row-index one). A window near the tail is
	/// clamped to the last row.
	///
	/// # Panics
	///
	/// Panics if `timestamps` has fewer than two rows, which [`Self::generate`] never
	/// produces.
	#[must_use]
	pub fn windows(&self, timestamps: &[i64]) -> Vec<(i64, i64)> {
		assert!(timestamps.len() >= 2, "a range-fetch corpus needs at least two rows");
		let mut rng = ChaCha8Rng::seed_from_u64(self.seed ^ 0x_C0DE_5EED_B20B);
		let n = timestamps.len();
		let span = self.window_rows.max(1);
		(0..self.window_count).map(|_| {
			let lo = rng.random_range(0..n);
			let hi = (lo + span - 1).min(n - 1);
			(timestamps[lo], timestamps[hi])
		}).collect()
	}
}

/// Elapsed nanoseconds since `since`, saturated into a `u64`.
#[allow(clippy::cast_possible_truncation)]
fn span_ns(since: Instant) -> u64 {
	since.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

/// Fetch every window in `windows` against `bytes`, returning the per-window
/// `(timestamps, values)` result. `paged` selects the paged range reader (on-disk
/// page skipping) over the single-block one.
fn fetch(bytes: &[u8], windows: &[(i64, i64)], paged: bool) -> anyhow::Result<Vec<WindowRows>> {
	windows.iter().map(|&(start, end)| if paged { read_paged_segment_range(bytes, start, end) } else { read_segment_range(bytes, start, end) }.map_err(anyhow::Error::from)).collect()
}

/// Run a range-fetch profile for `reps` timed repetitions and return a
/// fully-populated [`BenchResult`] (workload `range_fetch`).
///
/// The corpus is generated and sealed **once** (setup, excluded from latency); each
/// rep re-fetches the whole window set against the sealed `.dspseg` bytes and the
/// timed span covers only that. Correctness is gated against the full-decode ground
/// truth: each windowed read must equal a full decode filtered to the window (the
/// same guard the codec-layer benchmark uses), so a codec or read-path regression
/// fails the run rather than skewing a number.
///
/// # Errors
///
/// Returns an error if `reps == 0`, if the corpus cannot be sealed into a segment,
/// or if any rep's fetch fails.
pub fn run_range_fetch(profile: &RangeFetchProfile, reps: usize) -> anyhow::Result<BenchResult> {
	anyhow::ensure!(reps > 0, "reps must be > 0");

	let run_start = Instant::now();

	let setup_start = Instant::now();
	let (timestamps, values) = profile.generate();
	let windows = profile.windows(&timestamps);
	let paged = profile.rows_per_page > 0;
	// Seal the corpus once (single-block or paged) and take the full-decode ground
	// truth from the same in-memory segment. `decode_nullable` reads the
	// already-decoded segment, so the reference is independent of the windowed read
	// path under test.
	let (bytes, all_ts, all_vals) = if paged {
		let segment = PagedSegment::build(&timestamps, &values, SEGMENT_UNIT, &BigDecimal::from(0), profile.rows_per_page).map_err(|e| anyhow::anyhow!("cannot seal paged range-fetch segment: {e:?}"))?;
		let (t, v) = segment.decode_nullable();
		(segment.write_to(), t, v)
	} else {
		let segment = Segment::build_sorted(&timestamps, &values, SEGMENT_UNIT, &BigDecimal::from(0)).map_err(|e| anyhow::anyhow!("cannot seal range-fetch segment: {e:?}"))?;
		let (t, v) = segment.decode_nullable();
		(segment.write_to(), t, v)
	};
	let setup_ns = span_ns(setup_start);

	let expected: Vec<WindowRows> = windows.iter().map(|&(start, end)| all_ts.iter().copied().zip(all_vals.iter().cloned()).filter(|(t, _)| start <= *t && *t <= end).unzip()).collect();

	let mut samples_ns: Vec<u64> = Vec::with_capacity(reps);
	let mut last_output: Vec<WindowRows> = Vec::new();
	for _ in 0..reps {
		let t0 = Instant::now();
		let output = fetch(&bytes, &windows, paged)?;
		samples_ns.push(span_ns(t0));
		last_output = output;
	}

	let measured_ns = samples_ns.iter().copied().fold(0_u64, u64::saturating_add);
	let end_to_end_ns = span_ns(run_start);
	let timing = TimingBreakdown { dataset_generation_ns: setup_ns, measured_ns, end_to_end_ns };

	// Correctness: every windowed read must reproduce the full-decode-filtered window,
	// and every materialized value must be finite. The output count is the total rows
	// fetched across all windows (the range read's "output").
	let windowed_matches_truth = last_output == expected;
	let rows_fetched: usize = last_output.iter().map(|(t, _)| t.len()).sum();
	let expected_rows: usize = expected.iter().map(|(t, _)| t.len()).sum();
	let values_finite = last_output.iter().flat_map(|(_, v)| v.iter().flatten()).all(|v| v.to_f64().is_some_and(f64::is_finite));
	let correctness = CorrectnessReport { output_count_ok: rows_fetched == expected_rows && windowed_matches_truth, expected_output_points: expected_rows, actual_output_points: rows_fetched, values_finite };

	let storage = Some(StorageEstimate::from_columns(&values, &timestamps, SEGMENT_UNIT, &BigDecimal::from(0)));

	let latency = LatencyStats::from_samples(&samples_ns);
	let latency_ci = Some(LatencyStats::bootstrap_cis(&samples_ns, &BootstrapConfig { seed: profile.seed ^ 0x_C0FF_EE15_C0DE_u64, ..BootstrapConfig::default() }));
	// Throughput is rows fetched per second (a "point" here is one materialized row).
	let throughput_points_per_sec = if latency.mean_ns > 0 {
		#[allow(clippy::cast_precision_loss)]
		let secs = latency.mean_ns as f64 / 1e9;
		#[allow(clippy::cast_precision_loss)]
		let points = rows_fetched as f64;
		points / secs
	} else {
		0.0
	};

	Ok(BenchResult {
		schema_version: SCHEMA_VERSION, profile: profile.name.clone(), adapter: "dsp".to_string(), workload: WORKLOAD_RANGE_FETCH.to_string(), reps, dataset: DatasetMeta { input_points: timestamps.len(), output_points: rows_fetched, irregular: !profile.regular, missingness_fraction: 0.0, seed: profile.seed, signal_shape: None }, latency, latency_ci, throughput_points_per_sec, timing, correctness, accuracy: None, storage
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A small profile so the correctness/round-trip tests stay fast.
	fn small(regular: bool) -> RangeFetchProfile {
		RangeFetchProfile::new(if regular { "rf-regular-small" } else { "rf-irregular-small" }, RangeFetchParams { point_count: 2_000, window_rows: 50, window_count: 16, regular, ..RangeFetchParams::default() })
	}

	#[test]
	fn generation_is_deterministic_for_a_seed() {
		let profile = small(false);
		assert_eq!(profile.generate(), profile.generate(), "same seed -> identical corpus");
		let (ts, _) = profile.generate();
		assert_eq!(profile.windows(&ts), profile.windows(&ts), "same seed -> identical windows");
	}

	#[test]
	fn windows_span_the_configured_row_count() {
		let profile = small(true);
		let (ts, _) = profile.generate();
		let windows = profile.windows(&ts);
		assert_eq!(windows.len(), 16);
		// A regular stride of 10ms over 50 rows spans (50-1)*10 = 490ms — unless the
		// window is clamped at the tail (start row within the last 49).
		for &(start, end) in &windows {
			assert!(start <= end, "window bounds must be ordered");
			assert!(end - start <= 49 * REGULAR_STRIDE_MS, "an unclamped window spans at most window_rows-1 strides");
		}
	}

	#[test]
	fn regular_run_passes_correctness_and_carries_storage_and_latency() {
		let result = run_range_fetch(&small(true), 3).expect("regular range-fetch run succeeds");
		assert_eq!(result.adapter, "dsp");
		assert_eq!(result.workload, "range_fetch");
		assert_eq!(result.profile, "rf-regular-small");
		assert_eq!(result.reps, 3);
		assert_eq!(result.latency.count, 3);
		assert_eq!(result.dataset.input_points, 2_000);
		assert!(result.dataset.output_points > 0, "windows must materialize rows");
		assert!(!result.dataset.irregular);
		assert!(result.accuracy.is_none(), "a range fetch has no analytic ground truth");
		assert!(result.correctness.passed(), "correctness must pass: {:?}", result.correctness);
		assert!(result.is_publishable());
		assert!(result.throughput_points_per_sec > 0.0);
		assert!(result.timing.measured_ns > 0);
		assert!(result.timing.end_to_end_ns >= result.timing.dataset_generation_ns + result.timing.measured_ns, "end-to-end must cover setup + measured: {:?}", result.timing);

		let storage = result.storage.expect("a range-fetch run carries a storage estimate");
		assert_eq!(storage.physical_type, "scaled_i64");
		assert_eq!(storage.value_codec, "scaled_for", "the clustered mantissas realize the FOR value codec (windowed fast path)");
	}

	#[test]
	fn irregular_run_passes_correctness() {
		let result = run_range_fetch(&small(false), 3).expect("irregular range-fetch run succeeds");
		assert_eq!(result.workload, "range_fetch");
		assert!(result.dataset.irregular);
		assert!(result.correctness.passed(), "correctness must pass on an irregular corpus: {:?}", result.correctness);
		assert!(result.is_publishable());
	}

	#[test]
	fn windowed_fetch_equals_full_decode_filtered() {
		// The core honesty check: the windowed read must equal a full decode filtered
		// to each window, for every window, on both corpus shapes.
		for regular in [true, false] {
			let profile = small(regular);
			let (ts, vals) = profile.generate();
			let segment = Segment::build_sorted(&ts, &vals, SEGMENT_UNIT, &BigDecimal::from(0)).expect("seals");
			let bytes = segment.write_to();
			let (all_ts, all_vals) = segment.decode_nullable();
			for &(start, end) in &profile.windows(&ts) {
				let (wt, wv) = read_segment_range(&bytes, start, end).expect("windowed read");
				let expected: (Vec<i64>, Vec<Option<BigDecimal>>) = all_ts.iter().copied().zip(all_vals.iter().cloned()).filter(|(t, _)| start <= *t && *t <= end).unzip();
				assert_eq!((wt, wv), expected, "windowed read must equal full-decode filter (regular={regular})");
			}
		}
	}

	#[test]
	fn paged_run_passes_correctness_over_the_page_skipping_read_path() {
		// A paged segment (rows_per_page > 0) drives the paged range reader (on-disk
		// page skipping); the windowed read must still equal the full-decode-filtered
		// window on both corpus shapes.
		for regular in [true, false] {
			let profile = RangeFetchProfile::new("rf-paged", RangeFetchParams { point_count: 3_000, window_rows: 60, window_count: 12, regular, rows_per_page: 512, ..RangeFetchParams::default() });
			let result = run_range_fetch(&profile, 2).expect("paged range-fetch run succeeds");
			assert!(result.correctness.passed(), "paged correctness must pass (regular={regular}): {:?}", result.correctness);
			assert!(result.is_publishable());
		}
	}

	#[test]
	fn zero_reps_is_rejected() {
		assert!(run_range_fetch(&small(true), 0).is_err(), "zero reps must be rejected");
	}

	#[test]
	fn result_round_trips_through_json() {
		let result = run_range_fetch(&small(true), 3).expect("run succeeds");
		let json = serde_json::to_string(&result).expect("serialize");
		let back: BenchResult = serde_json::from_str(&json).expect("deserialize");
		assert_eq!(result.workload, back.workload);
		assert_eq!(result.dataset, back.dataset);
		assert_eq!(result.correctness, back.correctness);
		assert!(back.accuracy.is_none());
		assert!(back.storage.is_some());
	}
}
