//! Benchmark the **cross-segment downsample** (`SegmentStore::downsample_range`) —
//! the bounded-memory reduction that folds each pruned segment into its own
//! mergeable `PartialReduction` and merges once.
//!
//! ## What this measures, and why it exists
//!
//! The roadmap's paired question for this path was whether reducing the pruned
//! segments *concurrently* carries the `dsp-bench --ds-parallel` chunked-partial win
//! (measured 14.7x there) over to **stored** data — with the standing caveat that the
//! per-segment file read may dominate, so the answer had to be measured rather than
//! assumed. This bench is that measurement: it seals a fixed corpus into `segments`
//! separate `.dspseg` files and times the whole `downsample_range` call (index prune →
//! per-segment read + decode + partial reduce → merge → finish).
//!
//! The segment *count* is the swept axis at a fixed total row count, so a run reads
//! the same bytes and reduces the same points however the corpus is split — only the
//! available cross-segment parallelism changes. `sketch_p99` is included because the
//! mergeable sketch is the reduction this surface exists to make bounded.

use std::hint::black_box;

use bigdecimal::BigDecimal;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use database::{PartialSidecarPolicy, SegmentStore};
use dsp_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use dsp_reduce::Aggregation;
use splimes::Resolution;
use tempfile::TempDir;
use tokio::runtime::Runtime;

/// Total rows in the corpus, held constant across the segment-count sweep.
const TOTAL_ROWS: i64 = 200_000;

/// One sample a second, so an hour resolution buckets ~3600 rows together.
const STRIDE_SECS: i64 = 1;

/// Seal `TOTAL_ROWS` rows split evenly into `segments` separate `.dspseg` frames.
///
/// Returns the temp dir (which must outlive the store) and the opened store. The
/// corpus is identical for every `segments` value — same timestamps, same values —
/// so a sweep isolates the segment split, not the data.
async fn sealed_store(dir: &TempDir, segments: i64) -> SegmentStore {
	sealed_store_with_policy(dir, segments, None).await
}

/// As [`sealed_store`], but seal under an optional partial-sidecar `policy` — so a bench
/// can compare a store that materializes per-segment partials against one that does not.
async fn sealed_store_with_policy(dir: &TempDir, segments: i64, policy: Option<PartialSidecarPolicy>) -> SegmentStore {
	let mut store = SegmentStore::open(dir.path()).await.expect("opens store");
	if let Some(policy) = policy {
		store = store.with_partial_sidecar_policy(policy);
	}
	let schema = AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds);
	store.declare("load", &schema).await.expect("declares aspect");
	let per_segment = TOTAL_ROWS / segments;
	for seg in 0..segments {
		let ts: Vec<i64> = (0..per_segment).map(|i| (seg * per_segment + i) * STRIDE_SECS).collect();
		// A bounded sawtooth: enough distinct values for the percentile/sketch
		// reductions to do real work, cheap to generate.
		let vs: Vec<BigDecimal> = (0..per_segment).map(|i| BigDecimal::from((seg * per_segment + i) % 997)).collect();
		store.seal_declared("load", &ts, &vs).await.expect("seals");
	}
	store
}

/// Sweep the segment count at a fixed total row count: 1 segment has no cross-segment
/// parallelism to exploit, 64 has plenty. Any gain from folding segments concurrently
/// shows as a falling wall-clock across this axis.
fn bench_segment_count(c: &mut Criterion) {
	let rt = Runtime::new().expect("tokio runtime");
	let aggs = [Aggregation::Min, Aggregation::Max, Aggregation::Avg, Aggregation::Sum];

	let mut group = c.benchmark_group("downsample_range/segments");
	group.sample_size(10);
	for segments in [1_i64, 4, 16, 64] {
		// Seal once per segment count — the sweep measures the reduction, not the seal.
		let dir = TempDir::new().expect("temp dir");
		let store = rt.block_on(sealed_store(&dir, segments));
		group.bench_with_input(BenchmarkId::from_parameter(segments), &segments, |b, _| {
			// `block_on` drives the multi-threaded runtime, so the per-segment
			// `spawn_blocking` halves still fan out across the blocking pool.
			b.iter(|| {
				let buckets = rt.block_on(store.downsample_range("load", i64::MIN, i64::MAX, Resolution::Hours, &aggs)).expect("downsamples");
				black_box(buckets)
			});
		});
		// Drop the store before the temp dir it lives in.
		drop(store);
		drop(dir);
	}
	group.finish();
}

/// The same sweep for the mergeable `sketch_p99` — the reduction whose bounded
/// per-bucket state is the reason the cross-segment surface exists — beside the exact
/// `p99` it substitutes for.
fn bench_percentiles(c: &mut Criterion) {
	let rt = Runtime::new().expect("tokio runtime");

	let mut group = c.benchmark_group("downsample_range/percentile");
	group.sample_size(10);
	for (label, aggs) in [("sketch_p99", vec![Aggregation::SketchP99]), ("exact_p99", vec![Aggregation::P99])] {
		let dir = TempDir::new().expect("temp dir");
		let store = rt.block_on(sealed_store(&dir, 16));
		group.bench_function(label, |b| {
			b.iter(|| {
				let buckets = rt.block_on(store.downsample_range("load", i64::MIN, i64::MAX, Resolution::Hours, &aggs)).expect("downsamples");
				black_box(buckets)
			});
		});
		drop(store);
		drop(dir);
	}
	group.finish();
}

/// The payoff of the per-segment partial **sidecar**: a full-history downsample of
/// materializable reductions at the sidecar's base resolution merges the stored partials
/// instead of decoding every value column. Two identical 16-segment corpora — one sealed
/// with an HOUR-base sidecar policy, one without — are downsampled at hour resolution over
/// all history, so the `sidecar` arm never opens a value column (just reads + merges the
/// small `.dspart` partials) while the `decode` arm is the shipped read-decode-reduce path.
/// The reduction set is materializable (the six streaming reductions + `sketch_p99`), which
/// is the precondition for the sidecar substitution.
fn bench_sidecar_vs_decode(c: &mut Criterion) {
	let rt = Runtime::new().expect("tokio runtime");
	let aggs = [Aggregation::Min, Aggregation::Max, Aggregation::Avg, Aggregation::Sum, Aggregation::First, Aggregation::Last, Aggregation::SketchP99];

	let mut group = c.benchmark_group("downsample_range/sidecar");
	group.sample_size(10);
	for (label, policy) in [("decode", None), ("sidecar", Some(PartialSidecarPolicy::at(Resolution::Hours, 1)))] {
		let dir = TempDir::new().expect("temp dir");
		let store = rt.block_on(sealed_store_with_policy(&dir, 16, policy));
		group.bench_function(label, |b| {
			b.iter(|| {
				let buckets = rt.block_on(store.downsample_range("load", i64::MIN, i64::MAX, Resolution::Hours, &aggs)).expect("downsamples");
				black_box(buckets)
			});
		});
		drop(store);
		drop(dir);
	}
	group.finish();
}

/// The payoff of the **rollup tier hierarchy**: a coarse (DAY) full-history downsample
/// re-keys from the coarsest materialized tier rather than folding the whole minute base.
/// Two identical 16-segment corpora, both sidecar-accelerated at a MINUTES base — one with
/// no coarser tiers (`single_base`), one with `[HOURS, DAYS]` rollup tiers (`tiered`) — are
/// downsampled at DAY resolution over all history. The `single_base` arm re-keys every
/// minute bucket in each segment up to days; the `tiered` arm serves the DAY tier directly
/// (zero re-key). `sketch_p99` is in the set because each merge is a sketch merge, so
/// folding fewer source buckets is the win this tier hierarchy exists to deliver.
fn bench_tiered_vs_single_base(c: &mut Criterion) {
	let rt = Runtime::new().expect("tokio runtime");
	let aggs = [Aggregation::Min, Aggregation::Max, Aggregation::Avg, Aggregation::Sum, Aggregation::First, Aggregation::Last, Aggregation::SketchP99];

	let mut group = c.benchmark_group("downsample_range/tiers");
	group.sample_size(10);
	let single = PartialSidecarPolicy::at(Resolution::Minutes, 1);
	let tiered = PartialSidecarPolicy::at_tiered(Resolution::Minutes, 1, &[Resolution::Hours, Resolution::Days]);
	for (label, policy) in [("single_base", single), ("tiered", tiered)] {
		let dir = TempDir::new().expect("temp dir");
		let store = rt.block_on(sealed_store_with_policy(&dir, 16, Some(policy)));
		group.bench_function(label, |b| {
			b.iter(|| {
				let buckets = rt.block_on(store.downsample_range("load", i64::MIN, i64::MAX, Resolution::Days, &aggs)).expect("downsamples");
				black_box(buckets)
			});
		});
		drop(store);
		drop(dir);
	}
	group.finish();
}

criterion_group!(benches, bench_segment_count, bench_percentiles, bench_sidecar_vs_decode, bench_tiered_vs_single_base);
criterion_main!(benches);
