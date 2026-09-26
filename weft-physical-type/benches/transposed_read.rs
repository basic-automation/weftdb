//! End-to-end read cost of the **transposed (`FastLanes`-layout) value codec** on disk vs the
//! linear per-block codec it permutes — roadmap Phase 6.1, the "realize the transposed layout
//! on disk" residue.
//!
//! ## What this measures, and why it exists
//!
//! `benches/bitunpack.rs` measured the transposed *unpack* alone at ~5.7× the linear per-block
//! decode on a small-magnitude stream. The roadmap's standing caveat is that a columnar read is
//! often bandwidth-bound, so a kernel-level unpack win need not survive the whole read path
//! (frame parse → CRC → value block → timestamp block → null mask → logical values). This bench
//! is that end-to-end measurement: the same corpus sealed twice — once with the default selector
//! (`scaled_blocked`/`scaled_bitpack`, the linear layout) and once with the transposed policy —
//! then timed through the three read entry points a store actually uses:
//!
//! - **full decode** (`read_segment`, the range-scan / downsample path),
//! - **streaming point read** (`read_segment_point`, `GET …/storage/{aspect}/at`), where the
//!   transposed layout must decode a whole 1024-lane tile to serve one value while the linear
//!   codec decodes a 64-value block — the honest cost side of the trade,
//! - **windowed range read** (`read_segment_range`, `GET …/points?start=&end=`).
//!
//! The corpus is a zero-straddling small-magnitude `ScaledI64` column (mantissas in
//! `[-500, 500]`): the regime where the plain bit-pack family wins the size race, so the
//! transposed layout is byte-comparable and the policy's overhead ceiling admits it. On a
//! clustered-high-base column FOR wins by a wide margin and the ceiling rejects the transposed
//! layout — that case is not benchmarked because it is never written.

use std::hint::black_box;

use bigdecimal::BigDecimal;
use criterion::{criterion_group, criterion_main, Criterion};
use weft_physical_type::{
	timestamp::TimeUnit, weftseg::{frame_value_codec, read_segment, read_segment_point, read_segment_range, FrameOptions}, Segment
};

/// A zero-straddling small-magnitude corpus: two-decimal values in `[-5.00, 5.00]` on a
/// regular 10 ms grid. Mantissas span ±500 (10 zig-zag bits), so per-block/global bit-packing
/// wins the size race and the transposed layout is admitted at parity. Scale 2 keeps most
/// values inexact in binary floating point, so the zero-tolerance encoder picks `ScaledI64`
/// (the only payload the transposed codec is defined over) rather than `F64`.
fn corpus(n: usize) -> (Vec<i64>, Vec<BigDecimal>) {
	let timestamps: Vec<i64> = (0..n as i64).map(|i| 1_000 + i * 10).collect();
	let values: Vec<BigDecimal> = (0..n as i64).map(|i| BigDecimal::new((((i * 37) % 1_001) - 500).into(), 2)).collect();
	(timestamps, values)
}

fn bench_transposed_read(c: &mut Criterion) {
	let n = 1_000_000;
	let (timestamps, values) = corpus(n);
	let segment = Segment::build_sorted(&timestamps, &values, TimeUnit::Millis, &BigDecimal::from(0)).expect("builds");

	let linear = segment.write_to();
	let transposed = segment.write_to_with(&FrameOptions { checkpoint_stride: None, transposed_max_overhead: Some(1.05) });
	let linear_codec = frame_value_codec(&linear).expect("reads codec");
	let transposed_codec = frame_value_codec(&transposed).expect("reads codec");
	assert!(matches!(linear_codec, "scaled_blocked" | "scaled_bitpack"), "the corpus must pick a plain bit-pack codec by default, got {linear_codec}");
	assert_eq!(transposed_codec, "scaled_transposed", "the transposed policy must admit this corpus");
	eprintln!("frame bytes: linear ({linear_codec}) = {} · transposed = {} ({:+.2}%)", linear.len(), transposed.len(), (transposed.len() as f64 / linear.len() as f64 - 1.0) * 100.0);

	// Correctness guards before timing: both frames decode to the same segment, and the
	// streaming/windowed reads agree across layouts.
	let t = 1_000 + (n as i64 / 2) * 10;
	assert_eq!(read_segment(&linear).expect("reads"), read_segment(&transposed).expect("reads"));
	assert_eq!(read_segment_point(&linear, t).expect("reads"), read_segment_point(&transposed, t).expect("reads"));
	let (w_start, w_end) = (t, t + 10 * 999);
	assert_eq!(read_segment_range(&linear, w_start, w_end).expect("reads"), read_segment_range(&transposed, w_start, w_end).expect("reads"));

	let mut group = c.benchmark_group("transposed_value_codec_1m");
	group.sample_size(20);
	for (label, bytes) in [("linear", &linear), ("transposed", &transposed)] {
		group.bench_function(format!("full_decode/{label}"), |b| b.iter(|| black_box(read_segment(black_box(bytes)).expect("reads"))));
		group.bench_function(format!("point_read/{label}"), |b| b.iter(|| black_box(read_segment_point(black_box(bytes), t).expect("reads"))));
		group.bench_function(format!("range_1000/{label}"), |b| b.iter(|| black_box(read_segment_range(black_box(bytes), w_start, w_end).expect("reads"))));
	}
	group.finish();
}

criterion_group!(benches, bench_transposed_read);
criterion_main!(benches);
