//! ALP adopt-or-drop benchmark (roadmap Phase 6.1, "use the `alp` crate to get the ALP adopt
//! benchmarked, rather than hand-rolling it first"): bytes/value **and** decode throughput for
//! classic ALP against every f64 codec WeftDB ships (Gorilla, Chimp, Chimp128, Elf) and against
//! the realized exact path (`ScaledI64` mantissas under the default value-codec selector), and
//! the advisory decimal-exponent FOR (`dfor`) over the same exact mantissas.
//!
//! ## The ALP arm
//!
//! spiraldb's `alp` crate does the float→integer step; this bench supplies the container the
//! crate leaves to its caller, mirroring the Parquet ALP layout (encoding 10): one vector per
//! 1024 values, each with its own exponent pair (2 B), a frame-of-reference minimum (8 B), a
//! bit width (1 B), the FOR residuals bit-packed with `fastlanes` at that width, an exception
//! count (2 B) and `u16` position + exact IEEE-754 value per exception (10 B). The column adds a
//! 7 B header and a 4 B offset per vector. ALP-RD ("real doubles") is sized from the crate's
//! split but not timed; it is the fallback, not the candidate.
//!
//! ## Corpora
//!
//! - `btc_close`: 1 Mi real BTC/USD one-minute closes from `database/datasets/btc_1min.csv`,
//!   starting 3,000,000 rows in (the head of that file is degenerate, constant 2012 ticks, so a
//!   head-of-corpus number is not a result). Skipped with a note when the file is absent;
//!   override the path with `WEFT_BTC_CSV`.
//! - `sensor_2dp`: a deterministic two-decimal random walk around 100.00.
//! - `real_doubles`: full-mantissa doubles (a scaled sine plus noise), ALP's worst case.
//!
//! Every arm is asserted to decode its corpus exactly before anything is timed. Both
//! `alp` and `fastlanes` are bench-only dev-dependencies, pinned exactly; the library links
//! neither.

use std::{
	hint::black_box, io::{BufRead, BufReader}, str::FromStr
};

use bigdecimal::BigDecimal;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use fastlanes::BitPacking;
use weft_physical_type::{
	column::recommend_encoding, floatcodec::{chimp128_f64_decode, chimp128_f64_encode, chimp_f64_decode, chimp_f64_encode, elf_f64_decode, elf_f64_encode, xor_f64_decode, xor_f64_encode}, timestamp::{blocked_bitpack_decode, blocked_bitpack_encode, dfor_bitpack_decode, dfor_bitpack_encode, for_bitpack_decode, for_bitpack_encode, BLOCKED_BITPACK_BLOCK}, PhysicalType
};

const N: usize = 1 << 20;
const VECTOR: usize = alp::ENCODE_CHUNK_SIZE;
const BTC_SKIP_ROWS: usize = 3_000_000;

/// One ALP vector: up to 1024 values sharing an exponent pair and a FOR frame.
struct AlpVector {
	exponents: alp::Exponents,
	reference: i64,
	width: usize,
	packed: Vec<u64>,
	len: usize,
	patch_positions: Vec<u16>,
	patch_values: Vec<f64>,
}

fn alp_encode(values: &[f64]) -> Vec<AlpVector> {
	values
		.chunks(VECTOR)
		.map(|chunk| {
			let (exponents, encoded, positions, patch_values, _offsets) = alp::encode::<f64>(chunk, None);
			let reference = encoded.iter().copied().min().unwrap_or(0);
			let max = encoded.iter().copied().max().unwrap_or(0);
			let width = alp::bit_width(max.wrapping_sub(reference).cast_unsigned()) as usize;
			let mut tile = [0_u64; VECTOR];
			for (t, &e) in tile.iter_mut().zip(&encoded) {
				*t = e.wrapping_sub(reference).cast_unsigned();
			}
			let mut packed = vec![0_u64; VECTOR * width / 64];
			if width > 0 {
				// SAFETY: `tile` is exactly 1024 elements and `packed` exactly `1024 * width / 64` words.
				unsafe { u64::unchecked_pack(width, &tile, &mut packed) };
			}
			let patch_positions = positions.iter().map(|&p| u16::try_from(p).expect("a position inside one 1024-value vector")).collect();
			AlpVector { exponents, reference, width, packed, len: chunk.len(), patch_positions, patch_values }
		})
		.collect()
}

/// Serialized size of the Parquet-style container described in the module docs.
fn alp_bytes(vectors: &[AlpVector]) -> usize {
	7 + vectors.iter().map(|v| 4 + 2 + 8 + 1 + v.packed.len() * 8 + 2 + v.patch_positions.len() * (2 + 8)).sum::<usize>()
}

fn alp_decode(vectors: &[AlpVector], out: &mut Vec<f64>) {
	out.clear();
	let mut tile = [0_u64; VECTOR];
	let mut ints = [0_i64; VECTOR];
	for v in vectors {
		if v.width == 0 {
			tile = [0; VECTOR];
		} else {
			// SAFETY: `packed` is exactly `1024 * width / 64` words and `tile` exactly 1024 elements.
			unsafe { u64::unchecked_unpack(v.width, &v.packed, &mut tile) };
		}
		for (i, &t) in ints.iter_mut().zip(&tile) {
			*i = t.cast_signed().wrapping_add(v.reference);
		}
		let start = out.len();
		out.resize(start + v.len, 0.0);
		alp::decode_into::<f64>(&ints[..v.len], v.exponents, &mut out[start..]);
		for (&p, &value) in v.patch_positions.iter().zip(&v.patch_values) {
			out[start + usize::from(p)] = value;
		}
	}
}

/// ALP-RD size: dictionary codes + right parts at their widths, the dictionary, and `u16`
/// position + `u16` left part per exception.
fn alp_rd_bytes(values: &[f64]) -> usize {
	let sample: Vec<f64> = values.iter().step_by(values.len().div_ceil(VECTOR).max(1)).copied().collect();
	let split = alp::RDEncoder::new(&sample).split(values);
	let bits = values.len() * (usize::from(split.left_parts_bit_width()) + usize::from(split.right_parts_bit_width()));
	bits.div_ceil(8) + split.left_dict().len() * 2 + split.left_exceptions().len() * 4 + 2
}

struct Corpus {
	name: &'static str,
	floats: Vec<f64>,
	decimals: Vec<BigDecimal>,
}

fn btc_close() -> Option<Corpus> {
	let path = std::env::var("WEFT_BTC_CSV").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../database/datasets/btc_1min.csv").to_string());
	let Ok(file) = std::fs::File::open(&path) else {
		eprintln!("btc_close: {path} not found; corpus skipped");
		return None;
	};
	let closes: Vec<String> = BufReader::new(file).lines().skip(1 + BTC_SKIP_ROWS).take(N).map(|line| line.expect("reads a line").split(',').nth(4).expect("a Close column").to_string()).collect();
	assert_eq!(closes.len(), N, "the BTC corpus is shorter than {BTC_SKIP_ROWS} + {N} rows");
	let floats = closes.iter().map(|s| s.parse().expect("a float close")).collect();
	let decimals = closes.iter().map(|s| BigDecimal::from_str(s).expect("a decimal close")).collect();
	Some(Corpus { name: "btc_close", floats, decimals })
}

/// A deterministic xorshift stream, so every corpus is reproducible.
fn noise(n: usize, seed: u64) -> impl Iterator<Item = u64> {
	let mut state = seed;
	(0..n).map(move |_| {
		state ^= state << 13;
		state ^= state >> 7;
		state ^= state << 17;
		state
	})
}

fn from_floats(name: &'static str, floats: Vec<f64>) -> Corpus {
	let decimals = floats.iter().map(|v| BigDecimal::from_str(&v.to_string()).expect("a shortest-repr decimal")).collect();
	Corpus { name, floats, decimals }
}

fn sensor_2dp() -> Corpus {
	let mut cents = 10_000_i64;
	let floats = noise(N, 0x9e37_79b9_7f4a_7c15)
		.map(|r| {
			cents += (r % 7).cast_signed() - 3;
			BigDecimal::new(cents.into(), 2).to_string().parse().expect("a two-decimal float")
		})
		.collect();
	from_floats("sensor_2dp", floats)
}

fn real_doubles() -> Corpus {
	let floats = noise(N, 0xd1b5_4a32_d192_ed03).enumerate().map(|(i, r)| (i as f64 * 1e-3).sin() * 1_000.0 + (r >> 11) as f64 / (1_u64 << 53) as f64).collect();
	from_floats("real_doubles", floats)
}

/// The advisory decimal-exponent FOR arm: the same exact mantissas as the realized arm, with
/// each 64-value block's common power of ten factored out before FOR packing.
fn weft_dfor(corpus: &Corpus) -> Option<(Vec<u8>, u8)> {
	let encoding = recommend_encoding(&corpus.decimals, &BigDecimal::from(0));
	let PhysicalType::ScaledI64 { scale } = encoding.physical_type else {
		return None;
	};
	let bytes = dfor_bitpack_encode(&encoding.scaled_i64_mantissas()?, BLOCKED_BITPACK_BLOCK);
	assert_eq!(Some(bytes.len()), encoding.dfor_value_bytes(), "the bench must size what the advisory estimate sizes");
	Some((bytes, scale))
}

fn weft_dfor_decode(bytes: &[u8], scale: u8) -> Vec<f64> {
	let divisor = 10_f64.powi(i32::from(scale));
	dfor_bitpack_decode(bytes, BLOCKED_BITPACK_BLOCK, N).iter().map(|&m| m as f64 / divisor).collect()
}

/// The realized exact path for a corpus: the codec the default selector picks, and a decoder
/// that yields the same `f64`s (`mantissa / 10^scale` is one correctly rounded division, so it
/// equals parsing the decimal while the mantissa is below 2^53).
fn weft_scaled(corpus: &Corpus) -> Option<(&'static str, Vec<u8>, u8)> {
	let encoding = recommend_encoding(&corpus.decimals, &BigDecimal::from(0));
	let PhysicalType::ScaledI64 { scale } = encoding.physical_type else {
		eprintln!("{}: recommend_encoding picked {:?}, not ScaledI64; the realized-scaled arm is skipped", corpus.name, encoding.physical_type);
		return None;
	};
	let mantissas = encoding.scaled_i64_mantissas()?;
	let codec = encoding.best_value_codec();
	let bytes = match codec {
		"scaled_for" => for_bitpack_encode(&mantissas, BLOCKED_BITPACK_BLOCK),
		"scaled_blocked" => blocked_bitpack_encode(&mantissas, BLOCKED_BITPACK_BLOCK),
		other => {
			eprintln!("{}: default selector picked {other}; the realized-scaled arm only times FOR/blocked", corpus.name);
			return None;
		}
	};
	assert_eq!(bytes.len(), encoding.best_serialized_bytes(), "the bench must size the codec the selector sized");
	Some((codec, bytes, scale))
}

fn weft_scaled_decode(codec: &str, bytes: &[u8], scale: u8) -> Vec<f64> {
	let mantissas = if codec == "scaled_for" { for_bitpack_decode(bytes, BLOCKED_BITPACK_BLOCK, N) } else { blocked_bitpack_decode(bytes, BLOCKED_BITPACK_BLOCK, N) };
	let divisor = 10_f64.powi(i32::from(scale));
	mantissas.iter().map(|&m| m as f64 / divisor).collect()
}

fn bits_per_value(bytes: usize) -> f64 {
	bytes as f64 * 8.0 / N as f64
}

fn bench_alp(c: &mut Criterion) {
	let corpora: Vec<Corpus> = [btc_close(), Some(sensor_2dp()), Some(real_doubles())].into_iter().flatten().collect();
	for corpus in &corpora {
		let values = &corpus.floats;
		let alp = alp_encode(values);
		let exceptions: usize = alp.iter().map(|v| v.patch_positions.len()).sum();
		let mut decoded = Vec::with_capacity(N);
		alp_decode(&alp, &mut decoded);
		assert!(decoded.iter().zip(values).all(|(a, b)| a.to_bits() == b.to_bits()), "{}: ALP must round-trip bit-exactly", corpus.name);

		let gorilla = xor_f64_encode(values);
		let chimp = chimp_f64_encode(values);
		let chimp128 = chimp128_f64_encode(values);
		let elf = elf_f64_encode(values);
		assert_eq!(xor_f64_decode(&gorilla, N), *values);
		assert_eq!(chimp_f64_decode(&chimp, N), *values);
		assert_eq!(chimp128_f64_decode(&chimp128, N), *values);
		if let Some(elf) = &elf {
			assert_eq!(elf_f64_decode(elf, N), *values);
		}
		let scaled = weft_scaled(corpus);
		if let Some((codec, bytes, scale)) = &scaled {
			assert_eq!(weft_scaled_decode(codec, bytes, *scale), *values, "{}: the realized scaled path must reproduce the floats", corpus.name);
		}
		let dfor = weft_dfor(corpus);
		if let Some((bytes, scale)) = &dfor {
			assert_eq!(weft_dfor_decode(bytes, *scale), *values, "{}: the decimal-exponent FOR must reproduce the floats", corpus.name);
		}

		eprintln!("== {} ({N} values) — bits/value", corpus.name);
		eprintln!("  alp (FOR + fastlanes pack) {:>7.3}   exceptions {exceptions} ({:.3}%)", bits_per_value(alp_bytes(&alp)), exceptions as f64 * 100.0 / N as f64);
		eprintln!("  alp_rd (sized only)        {:>7.3}", bits_per_value(alp_rd_bytes(values)));
		eprintln!("  gorilla                    {:>7.3}", bits_per_value(gorilla.len()));
		eprintln!("  chimp                      {:>7.3}", bits_per_value(chimp.len()));
		eprintln!("  chimp128                   {:>7.3}", bits_per_value(chimp128.len()));
		match &elf {
			Some(elf) => eprintln!("  elf                        {:>7.3}", bits_per_value(elf.len())),
			None => eprintln!("  elf                        n/a (no shared decimal grid)"),
		}
		if let Some((codec, bytes, scale)) = &scaled {
			eprintln!("  weft {codec} (scale {scale})  {:>7.3}", bits_per_value(bytes.len()));
		}
		if let Some((bytes, _)) = &dfor {
			eprintln!("  weft dfor (advisory)       {:>7.3}", bits_per_value(bytes.len()));
		}

		let mut group = c.benchmark_group(format!("f64_decode_1mi/{}", corpus.name));
		group.sample_size(20);
		group.throughput(Throughput::Elements(N as u64));
		group.bench_function("alp", |b| {
			let mut out = Vec::with_capacity(N);
			b.iter(|| {
				alp_decode(black_box(&alp), &mut out);
				black_box(out.len())
			});
		});
		group.bench_function("gorilla", |b| b.iter(|| black_box(xor_f64_decode(black_box(&gorilla), N))));
		group.bench_function("chimp", |b| b.iter(|| black_box(chimp_f64_decode(black_box(&chimp), N))));
		group.bench_function("chimp128", |b| b.iter(|| black_box(chimp128_f64_decode(black_box(&chimp128), N))));
		if let Some(elf) = &elf {
			group.bench_function("elf", |b| b.iter(|| black_box(elf_f64_decode(black_box(elf), N))));
		}
		if let Some((codec, bytes, scale)) = &scaled {
			group.bench_function(format!("weft_{codec}"), |b| b.iter(|| black_box(weft_scaled_decode(codec, black_box(bytes), *scale))));
		}
		if let Some((bytes, scale)) = &dfor {
			group.bench_function("weft_dfor", |b| b.iter(|| black_box(weft_dfor_decode(black_box(bytes), *scale))));
		}
		group.finish();
	}
}

criterion_group!(benches, bench_alp);
criterion_main!(benches);
