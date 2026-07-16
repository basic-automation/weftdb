//! Point-lookup benchmark for a **sorted irregular** delta-of-delta timestamp column.
//!
//! Roadmap Phase 4/6 "block-random-access timestamp search for irregular sorted
//! columns". A *regular* (constant-stride) column resolves a row in `O(1)` closed form
//! (`arithmetic_stride`), but an irregular one has no closed form: because a
//! delta-of-delta stream is sequential (row `r` depends on every dod before it), the
//! streaming point read must reconstruct the **whole** column before it can
//! binary-search — an `O(n)` cost that dominates once the value block is skipped.
//!
//! [`DeltaOfDeltaColumn::checkpoints`] records `(row, timestamp, delta)` every `stride`
//! rows, so a probe binary-searches the sparse index and then reconstructs at most
//! `stride` rows: `O(log(n/stride) + stride)`. This bench measures that lookup win
//! against the current whole-column decode + binary search on the same corpus.
//!
//! The corpus is a sorted *irregular* column (a monotone series with varying gaps) —
//! the shape the closed form cannot serve and the one this index exists for.

use std::{hint::black_box, str::FromStr};

use bigdecimal::BigDecimal;
use criterion::{criterion_group, criterion_main, Criterion};
use dsp_physical_type::{
	dspseg::{read_paged_segment_point, read_segment_point, write_paged_segment, write_paged_segment_checkpointed, write_segment, write_segment_checkpointed}, page::PagedSegment, timestamp::{decode_delta_of_delta, encode_delta_of_delta, DeltaOfDeltaColumn, TimeUnit}, Segment
};

/// A deterministic sorted-irregular epoch column: a base stride of ~1000 ms perturbed by
/// a reproducible jitter, accumulated so the column is strictly increasing. No RNG dep —
/// a cheap xorshift keyed off the index keeps the bench reproducible.
fn corpus(n: usize) -> Vec<i64> {
	let mut ts: i64 = 1_600_000_000_000;
	(0..n)
		.map(|i| {
			let mut x = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
			x ^= x >> 29;
			x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
			x ^= x >> 32;
			// Gap in [1, 2000] ms: irregular, always forward (so the column stays sorted).
			ts = ts.wrapping_add(1 + i64::try_from(x % 2000).unwrap_or(1));
			ts
		})
		.collect()
}

/// The status quo: reconstruct the whole column, then binary-search it.
fn full_decode_search(col: &DeltaOfDeltaColumn, target: i64) -> Option<usize> {
	let all = decode_delta_of_delta(col);
	all.binary_search(&target).ok().map(|mut i| {
		// Match the checkpointed search's first-occurrence semantics.
		while i > 0 && all[i - 1] == target {
			i -= 1;
		}
		i
	})
}

fn bench(c: &mut Criterion) {
	const N: usize = 100_000;
	let values = corpus(N);
	let col = encode_delta_of_delta(&values, TimeUnit::Millis);
	// Probe spread across the column so neither approach benefits from locality.
	let probes: Vec<i64> = (0..64).map(|k| values[k * (N / 64)]).collect();

	let mut group = c.benchmark_group("dod_point_lookup_irregular_100k");

	group.bench_function("full_decode_then_binary_search", |b| {
		b.iter(|| {
			for &t in &probes {
				black_box(full_decode_search(black_box(&col), black_box(t)));
			}
		});
	});

	// The index is built once per column (amortized across probes), as it would be on
	// disk; the per-probe cost is what the point read pays.
	for stride in [64_usize, 256, 1024] {
		let cps = col.checkpoints(stride);
		group.bench_function(format!("checkpointed_search_stride_{stride}"), |b| {
			b.iter(|| {
				for &t in &probes {
					black_box(cps.search_sorted(black_box(&col), black_box(t)));
				}
			});
		});
	}
	group.finish();
}

/// The end-to-end claim: a real `.dspseg` point read on an irregular sorted frame, plain
/// vs checkpointed. This is what a caller actually pays — frame parse, value-block skip,
/// timestamp resolution, value unpack — not just the search step above.
fn bench_frame(c: &mut Criterion) {
	const N: usize = 100_000;
	let ts = corpus(N);
	let vs: Vec<BigDecimal> = (0..N).map(|i| BigDecimal::from_str(&format!("{}.25", 1_000 + i % 500)).expect("valid")).collect();
	let seg = Segment::build(&ts, &vs, TimeUnit::Millis, &BigDecimal::from(0)).expect("builds");
	assert!(seg.stats.time_sorted && seg.timestamps.arithmetic_stride().is_none(), "fixture must be sorted and irregular");

	let plain = write_segment(&seg);
	let probes: Vec<i64> = (0..64).map(|k| ts[k * (N / 64)]).collect();

	let mut group = c.benchmark_group("dspseg_point_read_irregular_100k");
	group.bench_function("plain_frame", |b| {
		b.iter(|| {
			for &t in &probes {
				black_box(read_segment_point(black_box(&plain), black_box(t)).expect("reads"));
			}
		});
	});
	for stride in [64_usize, 256, 1024] {
		let frame = write_segment_checkpointed(&seg, stride);
		// Report the storage cost of the index alongside the speed — the trade-off is the
		// whole reason default adoption is owner-gated.
		let overhead = (frame.len() as f64 - plain.len() as f64) / plain.len() as f64 * 100.0;
		println!("checkpointed stride={stride}: {} B vs plain {} B ({overhead:+.2}% frame bytes)", frame.len(), plain.len());
		group.bench_function(format!("checkpointed_frame_stride_{stride}"), |b| {
			b.iter(|| {
				for &t in &probes {
					black_box(read_segment_point(black_box(&frame), black_box(t)).expect("reads"));
				}
			});
		});
	}
	group.finish();
}

/// The paged sibling. A paged frame already prunes to one page on the indexed min/max, so
/// the timestamp decode it avoids is bounded by `rows_per_page` — the win should therefore
/// be real but smaller than the single-block frame's. Measured, not assumed.
fn bench_paged_frame(c: &mut Criterion) {
	const N: usize = 100_000;
	const ROWS_PER_PAGE: usize = 4_096;
	let ts = corpus(N);
	let vs: Vec<BigDecimal> = (0..N).map(|i| BigDecimal::from_str(&format!("{}.25", 1_000 + i % 500)).expect("valid")).collect();
	let seg = PagedSegment::build(&ts, &vs, TimeUnit::Millis, &BigDecimal::from(0), ROWS_PER_PAGE).expect("builds");

	let plain = write_paged_segment(&seg);
	let probes: Vec<i64> = (0..64).map(|k| ts[k * (N / 64)]).collect();

	let mut group = c.benchmark_group("dspseg_paged_point_read_irregular_100k");
	group.bench_function("plain_paged_frame", |b| {
		b.iter(|| {
			for &t in &probes {
				black_box(read_paged_segment_point(black_box(&plain), black_box(t)).expect("reads"));
			}
		});
	});
	for stride in [256_usize, 1024] {
		let frame = write_paged_segment_checkpointed(&seg, stride);
		let overhead = (frame.len() as f64 - plain.len() as f64) / plain.len() as f64 * 100.0;
		println!("checkpointed paged stride={stride}: {} B vs plain {} B ({overhead:+.2}% frame bytes)", frame.len(), plain.len());
		group.bench_function(format!("checkpointed_paged_frame_stride_{stride}"), |b| {
			b.iter(|| {
				for &t in &probes {
					black_box(read_paged_segment_point(black_box(&frame), black_box(t)).expect("reads"));
				}
			});
		});
	}
	group.finish();
}

/// **Size cost of the codec override, across corpus shapes** — not a latency benchmark,
/// a measurement the owner-gated default-adoption decision needs.
///
/// A checkpointed frame forces the inner dod stream to `TS_CODEC_BLOCKED` (the only
/// range-decodable codec) instead of `best_encoding_name`'s pick. Where the best codec is
/// something else — Gorilla on scattered jitter, RLE on long constant runs, varint on a
/// regular column — that override costs bytes *beyond* the checkpoint index itself. The
/// frame-level `+0.41%` figure is the two costs combined on one corpus; this separates
/// them per shape, so the trade can be judged on the shapes a deployment actually holds.
fn report_codec_override_cost(_c: &mut Criterion) {
	// (name, timestamps) — the shapes DSP's timestamp columns actually take.
	let mut regular_jitter = 1_000_000_i64;
	let shapes: Vec<(&str, Vec<i64>)> = vec![
		("regular (constant stride)", (0..10_000).map(|i| 1_000_000 + i * 1_000).collect()),
		("irregular (bounded jitter)", corpus(10_000)),
		(
			"scattered single jitter",
			(0..10_000_i64)
				.map(|i| {
					regular_jitter += 1_000 + if i % 16 == 0 { 500 } else { 0 };
					regular_jitter
				})
				.collect(),
		),
		("long constant runs (RLE regime)", (0..10_000_i64).map(|i| 1_000_000 + (i / 100) * 5_000).collect()),
	];
	println!("\n--- checkpointed-frame codec override: forced BLOCKED vs the best codec ---");
	for (name, values) in shapes {
		let col = encode_delta_of_delta(&values, TimeUnit::Millis);
		let best = col.best_estimated_bytes();
		let blocked = col.blocked_estimated_bytes();
		let pct = (blocked as f64 - best as f64) / best as f64 * 100.0;
		println!("{name:32} best={:<8} ({:<24}) blocked={:<8} override {pct:+7.2}%", best, col.best_encoding_name(), blocked);
	}
	println!("--- (the checkpoint index itself is extra, ~rows/stride entries) ---\n");
}

criterion_group!(benches, bench, bench_frame, bench_paged_frame, report_codec_override_cost);
criterion_main!(benches);
