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
use weft_physical_type::{weftseg::{read_paged_segment_point, read_segment, read_segment_point, read_segment_points, read_segment_range}, timestamp::TimeUnit, PagedSegment, Segment};

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

fn bench_batch_point_read(c: &mut Criterion) {
	let n = 100_000;
	let segment = build_segment(n);
	let bytes = segment.write_to();

	// 64 instants spread across the segment — all present grid points.
	let instants: Vec<i64> = (0..64).map(|k| 1_000 + (k * (n as i64 / 64)) * 10).collect();

	// Correctness guard: the batch equals the per-instant reads.
	let batch = read_segment_points(&bytes, &instants).expect("reads");
	let singles: Vec<_> = instants.iter().map(|&t| read_segment_point(&bytes, t).expect("reads")).collect();
	assert_eq!(batch, singles, "batch read must equal the per-instant reads");
	assert!(batch.iter().all(Option::is_some), "every queried instant must be present");

	let mut group = c.benchmark_group("segment_batch_point_lookup_100k_x64");

	// N single reads: each re-reads the frame and re-decodes the timestamp column.
	group.bench_function("n_single_reads", |b| {
		b.iter(|| {
			let mut out = Vec::with_capacity(instants.len());
			for &t in &instants {
				out.push(read_segment_point(black_box(&bytes), black_box(t)).expect("reads"));
			}
			black_box(out)
		})
	});
	// One batch read: the timestamp column is decoded once for all 64 instants.
	group.bench_function("one_batch_read", |b| b.iter(|| black_box(read_segment_points(black_box(&bytes), black_box(&instants)).expect("reads"))));

	group.finish();
}

fn bench_regular_vs_irregular(c: &mut Criterion) {
	let n = 100_000;
	// A regular (constant-stride) FOR segment — the point read resolves the row in closed form.
	let regular = build_segment(n);
	let regular_bytes = regular.write_to();
	// An irregular segment (a deterministic pseudo-random jitter on each gap) — the point read must
	// reconstruct + binary-search the timestamp column.
	let mut ts = Vec::with_capacity(n);
	let mut cur = 1_000_i64;
	for i in 0..n {
		let mut x = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
		x ^= x >> 29;
		cur += 1 + i64::from((x % 20) as u32); // strictly increasing, irregular gaps
		ts.push(cur);
	}
	let vs: Vec<BigDecimal> = (0..n).map(|i| format!("10000000.{:02}", i % 97).parse().expect("parses")).collect();
	let irregular = Segment::build_sorted(&ts, &vs, TimeUnit::Millis, &BigDecimal::from(0)).expect("builds");
	let irregular_bytes = irregular.write_to();

	// Look up the mid instant of each (both present, both interior).
	let t_regular = 1_000 + (n as i64 / 2) * 10;
	let t_irregular = ts[n / 2];
	assert!(read_segment_point(&regular_bytes, t_regular).expect("reads").is_some());
	assert!(read_segment_point(&irregular_bytes, t_irregular).expect("reads").is_some());

	let mut group = c.benchmark_group("point_lookup_timestamp_100k");
	// Regular: closed-form index, no timestamp materialization.
	group.bench_function("regular_closed_form", |b| b.iter(|| black_box(read_segment_point(black_box(&regular_bytes), black_box(t_regular)).expect("reads"))));
	// Irregular: full delta-of-delta decode + binary search.
	group.bench_function("irregular_decode_search", |b| b.iter(|| black_box(read_segment_point(black_box(&irregular_bytes), black_box(t_irregular)).expect("reads"))));
	group.finish();
}

fn bench_range_read(c: &mut Criterion) {
	let n = 100_000;
	let segment = build_segment(n); // regular ts, FOR value codec
	let bytes = segment.write_to();

	// A selective 100-row window in the middle of a 100k-row segment.
	let start = 1_000 + (n as i64 / 2) * 10;
	let end = start + 99 * 10;

	// Correctness guard: the windowed read equals the full decode + filter.
	let (wt, wv) = read_segment_range(&bytes, start, end).expect("reads");
	let (ft, fv) = read_segment(&bytes).expect("reads").decode_nullable();
	let expected: (Vec<i64>, Vec<Option<BigDecimal>>) = ft.into_iter().zip(fv).filter(|(t, _)| start <= *t && *t <= end).unzip();
	assert_eq!((wt.clone(), wv.clone()), expected, "windowed range read must equal the full decode + filter");
	assert_eq!(wt.len(), 100, "the window must be 100 rows");

	let mut group = c.benchmark_group("range_read_100k_window100");
	// Full decode then filter — decodes all 100k values.
	group.bench_function("full_decode_filter", |b| {
		b.iter(|| {
			let (t, v) = read_segment(black_box(&bytes)).expect("reads").decode_nullable();
			let out: (Vec<i64>, Vec<Option<BigDecimal>>) = t.into_iter().zip(v).filter(|(t, _)| start <= *t && *t <= end).unzip();
			black_box(out)
		})
	});
	// Windowed read — closed-form row window, unpacks only the ~100 values inside it.
	group.bench_function("windowed_read", |b| b.iter(|| black_box(read_segment_range(black_box(&bytes), black_box(start), black_box(end)).expect("reads"))));
	group.finish();
}

criterion_group!(benches, bench_point_read, bench_paged_point_read, bench_batch_point_read, bench_regular_vs_irregular, bench_range_read);
criterion_main!(benches);
