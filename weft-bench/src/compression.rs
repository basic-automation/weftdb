//! Compression workload — storage cost and decode throughput as a benchmark.
//!
//! WeftDB's north star is *dollars per billion interpolated points at a p95 target*,
//! and **storage bytes/point** is the cost term that rests on. Every other workload
//! already carries a [`StorageEstimate`] as a side field; this workload makes storage
//! the *headline* and adds the metric the read-latency workloads do not measure:
//! **decode throughput** (points/sec, bytes/sec) — the cost of materializing a whole
//! sealed segment, the baseline the streaming point/range reads are compared against
//! and a roadmap Phase 6.4 requirement ("compress/decompress throughput").
//!
//! The workload seals the corpus into a `.weftseg` segment, then times a full decode
//! ([`read_segment`] + [`Segment::decode_nullable`]) across reps, gating correctness
//! on an exact round-trip (the decoded `(timestamps, values)` must equal the input).
//! The [`BenchResult`] carries the realized bytes/point and the value-column
//! compression ratio (`realized / logical`) in its storage estimate, throughput as
//! points decoded per second, and the `compression` workload label.
//!
//! Like the other storage workloads it drives WeftDB's own `weft-physical-type` path
//! directly (no `database`/libSQL), keeping Weft-Bench a vendor-neutral leaf crate.

use std::time::Instant;

use bigdecimal::{BigDecimal, ToPrimitive};
use weft_physical_type::{weftseg::read_segment, Segment, TimeUnit};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

use crate::{
	schema::{BenchResult, CorrectnessReport, DatasetMeta, StorageEstimate, TimingBreakdown, SCHEMA_VERSION}, stats::{BootstrapConfig, LatencyStats}
};

/// Workload class label recorded for the compression workload.
const WORKLOAD_COMPRESSION: &str = "compression";

/// The epoch unit the compression corpus is sealed in.
const SEGMENT_UNIT: TimeUnit = TimeUnit::Millis;

/// The base epoch (ms) the corpus starts at.
const EPOCH_ANCHOR_MS: i64 = 1_000;

/// The constant stride (ms) between rows of a regular corpus.
const REGULAR_STRIDE_MS: i64 = 10;

/// Default seed for compression profiles.
pub const DEFAULT_COMPRESSION_SEED: u64 = 0x00DB_5EED_3C0D;

/// The analytic shape of a compression corpus's value column.
///
/// Each shape stresses a different codec regime, so the workload surfaces how WeftDB's
/// typed columnar encoding compresses genuinely different data, not one favourable
/// curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueShape {
	/// A high base with a tiny 2-decimal wobble — the Frame-of-Reference regime (an
	/// all-positive, small-range column packs to a handful of residual bits).
	Clustered,
	/// A monotone counter (a high base plus a fixed step) — the delta-cascade regime
	/// (the magnitude every single-level codec pays for collapses to a constant delta).
	Trending,
	/// A scattered small-magnitude jitter (`0.00`..`0.09`) — a low-entropy column the
	/// bit-pack/blocked codecs crush.
	Jitter,
}

impl ValueShape {
	/// Stable lowercase token for reports / labels.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Clustered => "clustered",
			Self::Trending => "trending",
			Self::Jitter => "jitter",
		}
	}

	/// The decimal value at row `i` under this shape (as a parseable string). Every
	/// shape yields exact 2-decimal values so `recommend_encoding` lands on
	/// `ScaledI64` (the typed hot-path encoding), isolating the codec's own ratio.
	#[must_use]
	fn value_at(self, i: usize) -> String {
		match self {
			Self::Clustered => format!("10000000.{:02}", i % 97),
			Self::Trending => format!("50000000.{:02}", i % 100),
			Self::Jitter => format!("0.{:02}", i % 10),
		}
	}
}

/// The tunable knobs for a compression profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressionParams {
	/// RNG seed; publishing it regenerates the corpus exactly.
	pub seed: u64,
	/// Rows sealed into the segment (the stored corpus size).
	pub point_count: usize,
	/// Constant-stride (regular) vs jittered (irregular) timestamps.
	pub regular: bool,
	/// Analytic shape of the value column.
	pub value_shape: ValueShape,
}

impl Default for CompressionParams {
	/// The flagship compression knob set: 20 000 regular rows of the clustered
	/// (Frame-of-Reference) corpus.
	fn default() -> Self {
		Self { seed: DEFAULT_COMPRESSION_SEED, point_count: 20_000, regular: true, value_shape: ValueShape::Clustered }
	}
}

/// A reproducible compression workload profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressionProfile {
	/// Human-readable profile name, recorded in results.
	pub name: String,
	/// RNG seed; publishing this regenerates the corpus exactly.
	pub seed: u64,
	/// Rows sealed into the segment.
	pub point_count: usize,
	/// Constant-stride (regular) vs jittered (irregular) timestamps.
	pub regular: bool,
	/// Analytic shape of the value column.
	pub value_shape: ValueShape,
}

impl CompressionProfile {
	/// A compression profile over the clustered (Frame-of-Reference) corpus.
	#[must_use]
	pub fn clustered(name: impl Into<String>) -> Self {
		Self::new(name, CompressionParams::default())
	}

	/// Build a compression profile from an explicit [`CompressionParams`] knob set.
	#[must_use]
	pub fn new(name: impl Into<String>, params: CompressionParams) -> Self {
		let CompressionParams { seed, point_count, regular, value_shape } = params;
		Self { name: name.into(), seed, point_count: point_count.max(2), regular, value_shape }
	}

	/// Generate the seeded, sorted `(timestamps, values)` corpus.
	///
	/// # Panics
	///
	/// Panics only if a compiled-in shape literal fails to parse, which cannot happen
	/// for the fixed format strings.
	#[must_use]
	pub fn generate(&self) -> (Vec<i64>, Vec<BigDecimal>) {
		let n = self.point_count.max(2);
		let values: Vec<BigDecimal> = (0..n).map(|i| self.value_shape.value_at(i).parse().expect("literal decimal parses")).collect();
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
}

/// Elapsed nanoseconds since `since`, saturated into a `u64`.
#[allow(clippy::cast_possible_truncation)]
fn span_ns(since: Instant) -> u64 {
	since.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

/// Run a compression profile for `reps` timed repetitions and return a
/// fully-populated [`BenchResult`] (workload `compression`).
///
/// The corpus is sealed **once** (setup, excluded from latency); each rep re-decodes
/// the whole segment from its `.weftseg` bytes (`read_segment` + `decode_nullable`) and
/// the timed span covers only that decode — so `latency` is the decode-latency
/// distribution and `throughput_points_per_sec` is decode throughput. Correctness is
/// gated on an exact round-trip: the decoded `(timestamps, values)` must equal the
/// input. The storage estimate carries the realized bytes/point and the value-column
/// compression ratio (`realized_value_bytes / estimated_value_bytes`).
///
/// # Errors
///
/// Returns an error if `reps == 0`, if the corpus cannot be sealed, or if a decode
/// fails.
pub fn run_compression(profile: &CompressionProfile, reps: usize) -> anyhow::Result<BenchResult> {
	anyhow::ensure!(reps > 0, "reps must be > 0");

	let run_start = Instant::now();

	let setup_start = Instant::now();
	let (timestamps, values) = profile.generate();
	let segment = Segment::build_sorted(&timestamps, &values, SEGMENT_UNIT, &BigDecimal::from(0)).map_err(|e| anyhow::anyhow!("cannot seal compression segment: {e:?}"))?;
	let bytes = segment.write_to();
	let setup_ns = span_ns(setup_start);

	// The expected round-trip: the input values as `Some` (the corpus has no nulls).
	let expected_values: Vec<Option<BigDecimal>> = values.iter().cloned().map(Some).collect();

	let mut samples_ns: Vec<u64> = Vec::with_capacity(reps);
	let mut last_decoded: (Vec<i64>, Vec<Option<BigDecimal>>) = (Vec::new(), Vec::new());
	for _ in 0..reps {
		let t0 = Instant::now();
		let decoded = read_segment(&bytes).map_err(anyhow::Error::from)?.decode_nullable();
		samples_ns.push(span_ns(t0));
		last_decoded = decoded;
	}

	let measured_ns = samples_ns.iter().copied().fold(0_u64, u64::saturating_add);
	let end_to_end_ns = span_ns(run_start);
	let timing = TimingBreakdown { dataset_generation_ns: setup_ns, measured_ns, end_to_end_ns };

	// Correctness: the decode must round-trip the input exactly (timestamps + values),
	// and every decoded value must be finite.
	let (dec_ts, dec_vals) = &last_decoded;
	let round_trips = dec_ts == &timestamps && dec_vals == &expected_values;
	let values_finite = dec_vals.iter().flatten().all(|v| v.to_f64().is_some_and(f64::is_finite));
	let correctness = CorrectnessReport { output_count_ok: dec_ts.len() == timestamps.len() && round_trips, expected_output_points: timestamps.len(), actual_output_points: dec_ts.len(), values_finite };

	let storage = Some(StorageEstimate::from_columns(&values, &timestamps, SEGMENT_UNIT, &BigDecimal::from(0)));

	let latency = LatencyStats::from_samples(&samples_ns);
	let latency_ci = Some(LatencyStats::bootstrap_cis(&samples_ns, &BootstrapConfig { seed: profile.seed ^ 0x_C0FF_EE15_C0DE_u64, ..BootstrapConfig::default() }));
	// Throughput is points decoded per second (the whole segment per rep).
	let throughput_points_per_sec = if latency.mean_ns > 0 {
		#[allow(clippy::cast_precision_loss)]
		let secs = latency.mean_ns as f64 / 1e9;
		#[allow(clippy::cast_precision_loss)]
		let points = timestamps.len() as f64;
		points / secs
	} else {
		0.0
	};

	Ok(BenchResult {
		schema_version: SCHEMA_VERSION, profile: profile.name.clone(), adapter: "weftdb".to_string(), workload: WORKLOAD_COMPRESSION.to_string(), reps, dataset: DatasetMeta { input_points: timestamps.len(), output_points: dec_ts.len(), irregular: !profile.regular, missingness_fraction: 0.0, seed: profile.seed, signal_shape: None }, latency, latency_ci, throughput_points_per_sec, timing, correctness, accuracy: None, storage
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	fn small(shape: ValueShape, regular: bool) -> CompressionProfile {
		CompressionProfile::new("comp-small", CompressionParams { point_count: 2_000, regular, value_shape: shape, ..CompressionParams::default() })
	}

	#[test]
	fn generation_is_deterministic_for_a_seed() {
		let profile = small(ValueShape::Jitter, false);
		assert_eq!(profile.generate(), profile.generate(), "same seed -> identical corpus");
	}

	#[test]
	fn clustered_run_passes_round_trip_and_realizes_the_for_codec() {
		let result = run_compression(&small(ValueShape::Clustered, true), 3).expect("compression run succeeds");
		assert_eq!(result.adapter, "weftdb");
		assert_eq!(result.workload, "compression");
		assert_eq!(result.reps, 3);
		assert_eq!(result.latency.count, 3);
		assert_eq!(result.dataset.input_points, 2_000);
		assert_eq!(result.dataset.output_points, 2_000, "the whole segment decodes");
		assert!(result.accuracy.is_none());
		assert!(result.correctness.passed(), "the decode must round-trip the input: {:?}", result.correctness);
		assert!(result.is_publishable());
		assert!(result.throughput_points_per_sec > 0.0, "decode throughput must be positive");
		assert!(result.timing.measured_ns > 0);

		let storage = result.storage.expect("a compression run carries a storage estimate");
		assert_eq!(storage.physical_type, "scaled_i64");
		assert_eq!(storage.value_codec, "scaled_for", "the clustered corpus realizes the FOR value codec");
		// The realized value column is well below the naive fixed-width baseline — a real
		// compression ratio, the headline the workload exists to surface.
		assert!(storage.realized_value_bytes < storage.estimated_value_bytes, "the corpus must actually compress: realized {} vs logical {}", storage.realized_value_bytes, storage.estimated_value_bytes);
	}

	#[test]
	fn every_value_shape_round_trips_on_both_timestamp_shapes() {
		for shape in [ValueShape::Clustered, ValueShape::Trending, ValueShape::Jitter] {
			for regular in [true, false] {
				let result = run_compression(&small(shape, regular), 2).expect("run succeeds");
				assert!(result.correctness.passed(), "{}/{regular} must round-trip: {:?}", shape.as_str(), result.correctness);
				assert!(result.is_publishable());
				let storage = result.storage.expect("storage estimate");
				assert_eq!(storage.physical_type, "scaled_i64", "{}: exact 2-decimals pick scaled_i64", shape.as_str());
			}
		}
	}

	#[test]
	fn zero_reps_is_rejected() {
		assert!(run_compression(&small(ValueShape::Clustered, true), 0).is_err(), "zero reps must be rejected");
	}

	#[test]
	fn result_round_trips_through_json() {
		let result = run_compression(&small(ValueShape::Clustered, true), 3).expect("run succeeds");
		let json = serde_json::to_string(&result).expect("serialize");
		let back: BenchResult = serde_json::from_str(&json).expect("deserialize");
		assert_eq!(result.workload, back.workload);
		assert_eq!(result.dataset, back.dataset);
		assert_eq!(result.correctness, back.correctness);
		assert!(back.storage.is_some());
	}
}
