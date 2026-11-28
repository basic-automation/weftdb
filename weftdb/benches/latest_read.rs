#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]
//! Benchmark the **per-series latest-row read** (`SegmentStore::read_latest_of`, TAG-12)
//! against the read it replaces in TSBS's `lastpoint`: a whole-series
//! `read_time_range_of(i64::MIN, i64::MAX)` whose last row is kept.
//!
//! The corpus is a fixed row count sealed into `segments` separate `.weftseg` frames, so
//! the sweep changes only how the series is split. The whole-series read decodes every
//! row of every segment; the latest read prunes to the segment holding the series'
//! greatest `max_ts` and decodes the window `[latest, latest]` there.

use std::hint::black_box;

use bigdecimal::BigDecimal;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use tempfile::TempDir;
use tokio::runtime::Runtime;
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weftdb::{SegmentStore, SeriesId};

/// Total rows in the corpus, held constant across the segment-count sweep.
const TOTAL_ROWS: i64 = 200_000;

/// Seal `TOTAL_ROWS` one-second rows split evenly into `segments` frames.
async fn sealed_store(dir: &TempDir, segments: i64) -> SegmentStore {
	let store = SegmentStore::open(dir.path()).await.expect("opens store");
	let schema = AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds);
	store.declare("load", &schema).await.expect("declares aspect");
	let per_segment = TOTAL_ROWS / segments;
	for seg in 0..segments {
		let ts: Vec<i64> = (0..per_segment).map(|i| seg * per_segment + i).collect();
		let vs: Vec<BigDecimal> = ts.iter().map(|t| BigDecimal::from(t % 997)).collect();
		store.seal_declared("load", &ts, &vs).await.expect("seals");
	}
	store
}

fn bench_latest(c: &mut Criterion) {
	let rt = Runtime::new().expect("a runtime");
	let mut group = c.benchmark_group("latest_read");
	group.sample_size(20);
	for segments in [1_i64, 8, 64] {
		let dir = TempDir::new().expect("tempdir");
		let store = rt.block_on(sealed_store(&dir, segments));
		let want = rt.block_on(store.read_latest_of("load", SeriesId::UNTAGGED)).expect("reads");
		assert_eq!(want, Some((TOTAL_ROWS - 1, Some(BigDecimal::from((TOTAL_ROWS - 1) % 997)))));
		group.bench_with_input(BenchmarkId::new("whole_series_last", segments), &segments, |b, _| {
			b.iter(|| {
				let (ts, vs) = rt.block_on(store.read_time_range_of("load", SeriesId::UNTAGGED, i64::MIN, i64::MAX)).expect("reads");
				black_box((ts.last().copied(), vs.into_iter().next_back()))
			});
		});
		group.bench_with_input(BenchmarkId::new("read_latest_of", segments), &segments, |b, _| {
			b.iter(|| black_box(rt.block_on(store.read_latest_of("load", SeriesId::UNTAGGED)).expect("reads")));
		});
		drop(store);
	}
	group.finish();
}

criterion_group!(benches, bench_latest);
criterion_main!(benches);
