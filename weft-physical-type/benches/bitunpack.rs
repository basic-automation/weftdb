//! Decode-throughput benchmark for the value/timestamp bit-pack codecs.
//!
//! Roadmap Phase 6.1 "FastLanes transposed bit-unpack" — the realized `ScaledI64`
//! value codec and the timestamp bit-pack codec pack scalar LSB-first, so decode is a
//! per-value bit loop ([`bitpack_decode`]/[`blocked_bitpack_decode`]). The transposed
//! layout ([`transpose_bitpack_decode`]) stores each tile bit-plane-major, so the decoder
//! reads `u64` words and walks only the *set* bits, skipping the empty high planes of a
//! small-magnitude stream wholesale. This bench measures that decode-latency win at an
//! **identical byte footprint** (the transpose is a permutation of the same bits).
//!
//! The corpus is a realistic small-magnitude difference stream (the regime the value/
//! timestamp codecs actually see after delta transforms): mostly narrow jitter with a
//! sparse scatter of wider values, so the high bit-planes are sparse — exactly where
//! plane-skipping pays. bytes/point is unchanged; this is a decode-throughput artifact.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use weft_physical_type::timestamp::{bitpack_decode, bitpack_encode, blocked_bitpack_bytes, blocked_bitpack_decode, blocked_bitpack_encode, transpose_bitpack_bytes, transpose_bitpack_decode, transpose_bitpack_encode, TRANSPOSE_TILE};

/// A deterministic small-magnitude difference stream: mostly ±7 jitter with a wider
/// value every 37th position (so the upper bit-planes are sparse, not empty). No RNG dep —
/// a cheap xorshift keyed off the index keeps the bench reproducible.
fn corpus(n: usize) -> Vec<i64> {
	(0..n)
		.map(|i| {
			let mut x = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
			x ^= x >> 29;
			x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
			x ^= x >> 32;
			if i % 37 == 0 {
				// Occasional wider value (few thousand) — a sparse high bit-plane.
				i64::from((x % 8_000) as u32) - 4_000
			} else {
				// Narrow ±7 jitter — the common case, only a few low planes populated.
				i64::from((x % 15) as u32) - 7
			}
		})
		.collect()
}

fn bench_decode(c: &mut Criterion) {
	let n = 1 << 20; // 1,048,576 values
	let values = corpus(n);

	// Footprints: the transposed layout must match the linear per-block layout at the same
	// tile (the win is speed, not size). Asserted here so a regression in the layout is loud.
	let blocked_bytes = blocked_bitpack_bytes(&values, TRANSPOSE_TILE);
	let transpose_bytes = transpose_bitpack_bytes(&values, TRANSPOSE_TILE);
	assert_eq!(blocked_bytes, transpose_bytes, "transposed footprint must equal the linear per-block footprint");

	let (global_width, global_packed) = bitpack_encode(&values);
	let blocked_packed = blocked_bitpack_encode(&values, TRANSPOSE_TILE);
	let transpose_packed = transpose_bitpack_encode(&values, TRANSPOSE_TILE);

	// Correctness guard: every decoder reproduces the input before we time them.
	assert_eq!(bitpack_decode(global_width, &global_packed, n), values);
	assert_eq!(blocked_bitpack_decode(&blocked_packed, TRANSPOSE_TILE, n), values);
	assert_eq!(transpose_bitpack_decode(&transpose_packed, TRANSPOSE_TILE, n), values);

	let mut group = c.benchmark_group("bit_unpack_decode_1Mi");
	group.throughput(Throughput::Elements(n as u64));

	group.bench_function("scalar_global", |b| b.iter(|| bitpack_decode(black_box(global_width), black_box(&global_packed), black_box(n))));
	group.bench_function("scalar_blocked_1024", |b| b.iter(|| blocked_bitpack_decode(black_box(&blocked_packed), black_box(TRANSPOSE_TILE), black_box(n))));
	group.bench_function("transposed_1024", |b| b.iter(|| transpose_bitpack_decode(black_box(&transpose_packed), black_box(TRANSPOSE_TILE), black_box(n))));

	group.finish();
}

criterion_group!(benches, bench_decode);
criterion_main!(benches);
