//! External yardstick for WeftDB's hand-rolled transposed bit-plane decoder: the same values,
//! the same 1024-lane tiles, decoded by `transpose_bitpack_decode` and by spiraldb's published
//! `fastlanes` crate (`BitPacking::unchecked_unpack`, LLVM-auto-vectorized) — roadmap Phase 6.1,
//! "cross-check the hand-rolled bit-plane decoder against the `fastlanes` crate".
//!
//! ## What this measures, and what it does not
//!
//! Both sides decode 1 Mi values into a `Vec<i64>` (zig-zag undone), so the output work is
//! identical; only the unpack kernel differs. The two byte layouts are *not* the same — WeftDB's
//! is bit-plane-major per tile with a one-byte width header, `fastlanes` is its own transposed
//! "FL order" at a fixed width — so this is a decode-throughput comparison at equal bit width,
//! not a format-compatibility test. If WeftDB's decoder is materially slower at the same width,
//! that is a bug to chase, not a design choice.
//!
//! `fastlanes` is a **dev-dependency of this bench only**, pinned exactly; nothing in the
//! library links it (the out-of-core boundary in the roadmap's hard constraints).
//!
//! Corpora: zig-zag widths 3, 10 and 20 bits, so the sweep shows how each decoder's cost
//! scales with width.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use fastlanes::BitPacking;
use weft_physical_type::timestamp::{transpose_bitpack_decode, transpose_bitpack_encode, TRANSPOSE_TILE};

const N: usize = 1 << 20;

const fn zigzag(v: i64) -> u64 {
	#[allow(clippy::cast_sign_loss)] // the bit pattern is exactly the zig-zag mapping.
	let z = ((v << 1) ^ (v >> 63)) as u64;
	z
}

#[allow(clippy::cast_possible_wrap)] // `z >> 1` fits `i64`; the XOR restores the sign.
const fn unzigzag(z: u64) -> i64 {
	((z >> 1) as i64) ^ -((z & 1) as i64)
}

/// A deterministic corpus whose zig-zag codes need exactly `width` bits (values in
/// `[-2^(width-1), 2^(width-1))`, with the extremes present in every tile).
fn corpus(width: u32) -> Vec<i64> {
	let half = 1_i64 << (width - 1);
	(0..N as i64).map(|i| if i % 1024 == 0 { -half } else { (i.wrapping_mul(2_654_435_761) % half).abs() * if i % 2 == 0 { 1 } else { -1 } }).collect()
}

/// Pack with `fastlanes` at a fixed `width`: one `1024 * width / 64`-word block per tile.
fn fl_pack(values: &[i64], width: usize) -> Vec<u64> {
	let words = TRANSPOSE_TILE * width / 64;
	let mut out = vec![0_u64; values.len() / TRANSPOSE_TILE * words];
	let mut tile = [0_u64; 1024];
	for (chunk, dst) in values.as_chunks::<TRANSPOSE_TILE>().0.iter().zip(out.chunks_exact_mut(words)) {
		for (t, &v) in tile.iter_mut().zip(chunk) {
			*t = zigzag(v);
		}
		// SAFETY: `tile` is exactly 1024 elements and `dst` exactly `1024 * width / 64` words.
		unsafe { u64::unchecked_pack(width, &tile, dst) };
	}
	out
}

fn fl_unpack(packed: &[u64], width: usize) -> Vec<i64> {
	let words = TRANSPOSE_TILE * width / 64;
	let mut out = Vec::with_capacity(packed.len() / words * TRANSPOSE_TILE);
	let mut tile = [0_u64; 1024];
	for src in packed.chunks_exact(words) {
		// SAFETY: `src` is exactly `1024 * width / 64` words and `tile` exactly 1024 elements.
		unsafe { u64::unchecked_unpack(width, src, &mut tile) };
		out.extend(tile.iter().map(|&z| unzigzag(z)));
	}
	out
}

fn bench_yardstick(c: &mut Criterion) {
	let mut group = c.benchmark_group("bitplane_decode_vs_fastlanes_1mi");
	group.sample_size(30);
	group.throughput(Throughput::Elements(N as u64));
	for width in [3_u32, 10, 20] {
		let values = corpus(width);
		let weft = transpose_bitpack_encode(&values, TRANSPOSE_TILE);
		let fl = fl_pack(&values, width as usize);
		// Correctness before timing: both decoders reproduce the corpus exactly.
		assert_eq!(transpose_bitpack_decode(&weft, TRANSPOSE_TILE, N), values, "weft decode, width {width}");
		assert_eq!(fl_unpack(&fl, width as usize), values, "fastlanes decode, width {width}");
		eprintln!("width {width}: weft bytes {} · fastlanes bytes {}", weft.len(), fl.len() * 8);

		group.bench_function(format!("weft_transposed/w{width}"), |b| b.iter(|| black_box(transpose_bitpack_decode(black_box(&weft), TRANSPOSE_TILE, N))));
		group.bench_function(format!("fastlanes/w{width}"), |b| b.iter(|| black_box(fl_unpack(black_box(&fl), width as usize))));
	}
	group.finish();
}

criterion_group!(benches, bench_yardstick);
criterion_main!(benches);
