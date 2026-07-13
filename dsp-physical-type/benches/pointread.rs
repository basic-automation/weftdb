//! Point-lookup latency: streaming single-value read vs full-segment decode.
//!
//! Roadmap Phase 4/6 — `read_segment_point` wires the block-level random-access primitives up
//! to the framed single-block segment: on a per-block value codec ([`VAL_CODEC_FOR`] /
//! [`VAL_CODEC_BLOCKED`]) it skips the value column by its framing, decodes only the timestamp
//! block to locate the row, and unpacks the *one* covering value block — instead of
//! materializing every value in the segment ([`read_segment`] + [`Segment::value_at`], the path
//! the storage read planner used before). This bench measures that point-lookup win.
//!
//! The corpus is a large `ScaledI64` column clustered at a high base with tiny 2-decimal
//! variation — the regime the Frame-of-Reference codec wins (an all-positive, small-range
//! block packs to a handful of bits over a per-block reference). The value block therefore
//! uses [`VAL_CODEC_FOR`], so the streaming read takes the block-skip fast path; the assertion
//! below makes a codec regression loud. The timestamps are sorted, so the lookup binary-searches.

use std::hint::black_box;

use bigdecimal::BigDecimal;
use criterion::{criterion_group, criterion_main, Criterion};
use dsp_physical_type::{dspseg::{read_paged_segment_point, read_segment_point}, timestamp::TimeUnit, PagedSegment, Segment};

/// The FOR-packing corpus values: a high base (`10000000`) with a tiny 2-decimal wobble
/// (`.00`..`.96`) that f64 cannot represent exactly, so `recommend_encoding` selects `ScaledI64`
/// and the clustered mantissas pick `VAL_CODEC_FOR` (the block-skip fast path).
fn corpus(n: usize) -> (Vec<i64>, Vec<BigDecimal>) {
	let timestamps: Vec<i64> = (0..n as i64).map(|i| 1_000 + i * 10).collect();
	let values: Vec<BigDecimal> = (0..n).map(|i| format!("10000000.{:02}", i % 97).parse().expect("literal parses")).collect();
	(timestamps, values)
}

/// A large sorted single-block `ScaledI64`/FOR segment.
fn build_segment(n: usize) -> Segment {
	let (timestamps, values) = corpus(n);
	Segment::build_sorted(&timestamps, &values, TimeUnit::Millis, &BigDecimal::from(0)).expect("builds")
}

/// The same corpus sealed as a paged frame (`rows_per_page` pages of FOR-packed values).
fn build_paged_segment(n: usize, rows_per_page: usize) -> PagedSegment {
	let (timestamps, values) = corpus(n);
	PagedSegment::build(&timestamps, &values, TimeUnit::Millis, &BigDecimal::from(0), rows_per_page).expect("builds")
}

fn bench_point_read(c: &mut Criterion) {
	let n = 100_000;
	let segment = build_segment(n);
	assert_eq!(segment.values.best_value_codec(), "scaled_for", "the corpus must pick the FOR value codec so the streaming read takes the block-skip fast path");
	let bytes = segment.write_to();

	// A mid-segment timestamp (present) — a representative interior point lookup.
	let t = 1_000 + (n as i64 / 2) * 10;

	// Correctness guard: the two paths must agree before we time them.
	let streaming = read_segment_point(&bytes, t).expect("reads");
	let full = Segment::read_from(&bytes).expect("reads").value_at(t);
	assert_eq!(streaming, full, "streaming point read must equal the full-decode value_at");
	assert!(streaming.is_some(), "the queried timestamp must be present");

	let mut group = c.benchmark_group("segment_point_lookup_100k");

	// The old read path: decode the whole segment (both columns), then binary-search the value.
	group.bench_function("full_decode_value_at", |b| b.iter(|| black_box(Segment::read_from(black_box(&bytes)).expect("reads").value_at(black_box(t)))));
	// The streaming read: skip the value column, decode only timestamps, unpack one value block.
	group.bench_function("streaming_point", |b| b.iter(|| black_box(read_segment_point(black_box(&bytes), black_box(t)).expect("reads"))));

	group.finish();
}

fn bench_paged_point_read(c: &mut Criterion) {
	let n = 100_000;
	let rows_per_page = 4_096; // ~25 pages
	let segment = build_paged_segment(n, rows_per_page);
	assert!(matches!(segment.pages[0].values.best_value_codec(), "scaled_for" | "scaled_blocked"), "a page must pick a per-block codec so the streaming read takes the fast path");
	let bytes = segment.write_to();

	// A mid-segment timestamp (present) — lands in an interior page.
	let t = 1_000 + (n as i64 / 2) * 10;

	// Correctness guard: the streaming paged read must equal the full paged decode + value_at.
	let streaming = read_paged_segment_point(&bytes, t).expect("reads");
	let full = PagedSegment::read_from(&bytes).expect("reads").value_at(t);
	assert_eq!(streaming, full, "streaming paged point read must equal the full-decode value_at");
	assert!(streaming.is_some(), "the queried timestamp must be present");

	let mut group = c.benchmark_group("paged_segment_point_lookup_100k");

	// The old read path: decode every page (all value + timestamp columns), then prune + search.
	group.bench_function("full_decode_value_at", |b| b.iter(|| black_box(PagedSegment::read_from(black_box(&bytes)).expect("reads").value_at(black_box(t)))));
	// The streaming read: prune pages on the index, decode only the surviving page's columns.
	group.bench_function("streaming_point", |b| b.iter(|| black_box(read_paged_segment_point(black_box(&bytes), black_box(t)).expect("reads"))));

	group.finish();
}

criterion_group!(benches, bench_point_read, bench_paged_point_read);
criterion_main!(benches);
