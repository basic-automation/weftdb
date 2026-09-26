//! Benchmark the **cross-segment downsample** (`SegmentStore::downsample_range`) —
//! the bounded-memory reduction that folds each pruned segment into its own
//! mergeable `PartialReduction` and merges once.
//!
//! ## What this measures, and why it exists
//!
//! The roadmap's paired question for this path was whether reducing the pruned
//! segments *concurrently* carries the `weft-bench --ds-parallel` chunked-partial win
//! (measured 14.7x there) over to **stored** data — with the standing caveat that the
//! per-segment file read may dominate, so the answer had to be measured rather than
//! assumed. This bench is that measurement: it seals a fixed corpus into `segments`
//! separate `.weftseg` files and times the whole `downsample_range` call (index prune →
//! per-segment read + decode + partial reduce → merge → finish).
//!
//! The segment *count* is the swept axis at a fixed total row count, so a run reads
//! the same bytes and reduces the same points however the corpus is split — only the
//! available cross-segment parallelism changes. `sketch_p99` is included because the
//! mergeable sketch is the reduction this surface exists to make bounded.

use std::hint::black_box;

use bigdecimal::BigDecimal;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use splimes::Resolution;
use tempfile::TempDir;
use tokio::runtime::Runtime;
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weft_reduce::Aggregation;
use weftdb::{PartialSidecarPolicy, SegmentStore};

/// Total rows in the corpus, held constant across the segment-count sweep.
const TOTAL_ROWS: i64 = 200_000;

/// One sample a second, so an hour resolution buckets ~3600 rows together.
const STRIDE_SECS: i64 = 1;

/// Seal `TOTAL_ROWS` rows split evenly into `segments` separate `.weftseg` frames.
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
	sealed_store_spanning(dir, segments, policy, STRIDE_SECS).await
}

/// As [`sealed_store_with_policy`], but with the sample `stride_secs` as a parameter — so a
/// bench can stretch the corpus's *time span* while holding its row count fixed.
///
/// The span is what drives the number of **base buckets per segment** a coarse downsample
/// must re-key when no coarser tier is materialized: at a MINUTES base, a segment covering
/// `span` seconds carries `min(rows_per_segment, span / 60)` base buckets. Holding rows
/// constant and widening the stride therefore isolates the *re-key* cost — the exact work a
/// rollup tier elides — from the file read and value decode, which are unchanged.
async fn sealed_store_spanning(dir: &TempDir, segments: i64, policy: Option<PartialSidecarPolicy>, stride_secs: i64) -> SegmentStore {
	let mut store = SegmentStore::open(dir.path()).await.expect("opens store");
	if let Some(policy) = policy {
		store = store.with_partial_sidecar_policy(policy);
	}
	let schema = AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds);
	store.declare("load", &schema).await.expect("declares aspect");
	let per_segment = TOTAL_ROWS / segments;
	for seg in 0..segments {
		let ts: Vec<i64> = (0..per_segment).map(|i| (seg * per_segment + i) * stride_secs).collect();
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
	// Swept past 64 to locate the per-segment-overhead knee: at a fixed total row count,
	// more segments means more file reads + index rows + partial merges but fewer rows to
	// decode per segment, so beyond some count the fixed per-segment cost dominates and
	// wall-clock climbs. Where that knee sits bears on a target-segment-size / compaction
	// policy (WeftDB already has `squash_aspect`).
	for segments in [1_i64, 4, 16, 64, 128, 256] {
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
/// small `.weftpart` partials) while the `decode` arm is the shipped read-decode-reduce path.
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

/// Does the rollup-tier win **scale** with the number of base buckets a coarse query would
/// otherwise re-key? [`bench_tiered_vs_single_base`] measured only ~1.16x, and recorded the
/// honest caveat that its corpus is short-span (a 1 s stride puts just ~208 minute buckets in
/// each segment), so the re-key the tier elides is small. This bench is that caveat's test.
///
/// The row count and segment count are held fixed while the sample stride widens, so every
/// arm reads the same bytes and merges the same number of segment partials — only the base
/// buckets per segment change (~208 → ~3125 → ~12500, saturating at one bucket per row). If
/// the tier's advantage is the elided re-key, the speedup must climb across this axis; if it
/// stays flat, the win is a fixed cost and the tiers are not worth their extra sidecar bytes.
fn bench_tiered_span(c: &mut Criterion) {
	let rt = Runtime::new().expect("tokio runtime");
	let aggs = [Aggregation::Min, Aggregation::Max, Aggregation::Avg, Aggregation::Sum, Aggregation::First, Aggregation::Last, Aggregation::SketchP99];

	let mut group = c.benchmark_group("downsample_range/tier_span");
	group.sample_size(10);
	for stride in [1_i64, 15, 60] {
		let single = PartialSidecarPolicy::at(Resolution::Minutes, 1);
		let tiered = PartialSidecarPolicy::at_tiered(Resolution::Minutes, 1, &[Resolution::Hours, Resolution::Days]);
		for (label, policy) in [("single_base", single), ("tiered", tiered)] {
			let dir = TempDir::new().expect("temp dir");
			let store = rt.block_on(sealed_store_spanning(&dir, 16, Some(policy), stride));
			group.bench_with_input(BenchmarkId::new(label, stride), &stride, |b, _| {
				b.iter(|| {
					let buckets = rt.block_on(store.downsample_range("load", i64::MIN, i64::MAX, Resolution::Days, &aggs)).expect("downsamples");
					black_box(buckets)
				});
			});
			drop(store);
			drop(dir);
		}
	}
	group.finish();
}

criterion_group!(benches, bench_segment_count, bench_percentiles, bench_sidecar_vs_decode, bench_tiered_vs_single_base, bench_tiered_span);
criterion_main!(benches);
