//! Timestamp-column half of the Pcodec evaluation (roadmap Phase 6.1, "run the bench head to
//! head per column — pco `IntMult` vs the shipped delta/blocked/Gorilla timestamp codecs"):
//! bits/value of the shipped delta-of-delta codec selector against `pco` on the raw epochs, and
//! decode time for `pco`.
//!
//! The shipped side is sized exactly as a sealed segment sizes it:
//! `encode_delta_of_delta(ts, unit).best_estimated_bytes()` over plain varint, RLE, fixed and
//! per-block bit-packing, and Gorilla, labelled with the codec that wins. The `pco` side is
//! `pco::standalone::simple_compress` at its default level 8, asserted to round-trip exactly.
//!
//! ## Corpora
//!
//! - `btc_minutes`: 1 Mi real one-minute BTC timestamps in seconds from
//!   `database/datasets/btc_1min.csv` (rows 3,000,000 onward; `WEFT_BTC_CSV` overrides the
//!   path; skipped with a note when absent). Mostly a regular 60 s stride, with the feed's
//!   real gaps.
//! - `ms_as_micros`: an irregular event stream whose instants are millisecond-precise but
//!   stored in microseconds, the regime pco's `IntMult` mode names ("ms-precise timestamps
//!   stored as us").
//! - `jittered_micros`: an irregular stream with full microsecond jitter, where no common
//!   multiple exists.
//!
//! `pco` is a bench-only dev-dependency, pinned exactly; the library does not link it.

use std::{
	hint::black_box, io::{BufRead, BufReader}
};

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use weft_physical_type::timestamp::{encode_delta_of_delta, TimeUnit};

const N: usize = 1 << 20;
const BTC_SKIP_ROWS: usize = 3_000_000;

fn btc_minutes() -> Option<Vec<i64>> {
	let path = std::env::var("WEFT_BTC_CSV").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../database/datasets/btc_1min.csv").to_string());
	let Ok(file) = std::fs::File::open(&path) else {
		eprintln!("btc_minutes: {path} not found; corpus skipped");
		return None;
	};
	// The file stores epochs as `1325412060.0`; the integer part is the instant in seconds.
	let ts: Vec<i64> = BufReader::new(file).lines().skip(1 + BTC_SKIP_ROWS).take(N).map(|line| line.expect("reads a line").split(['.', ',']).next().expect("a timestamp").parse().expect("an integer epoch")).collect();
	assert_eq!(ts.len(), N, "the BTC corpus is shorter than {BTC_SKIP_ROWS} + {N} rows");
	Some(ts)
}

/// A deterministic xorshift stream, so every corpus is reproducible.
fn noise(seed: u64) -> impl FnMut() -> u64 {
	let mut state = seed;
	move || {
		state ^= state << 13;
		state ^= state >> 7;
		state ^= state << 17;
		state
	}
}

/// Instants 1-500 ms apart, millisecond-precise, stored in microseconds.
fn ms_as_micros() -> Vec<i64> {
	let mut next = noise(0x2545_f491_4f6c_dd1d);
	let mut t = 1_700_000_000_000_000_i64;
	(0..N)
		.map(|_| {
			t += i64::try_from(next() % 500 + 1).unwrap_or(1) * 1_000;
			t
		})
		.collect()
}

/// Instants 1-500 ms apart with full microsecond jitter.
fn jittered_micros() -> Vec<i64> {
	let mut next = noise(0x9e37_79b9_7f4a_7c15);
	let mut t = 1_700_000_000_000_000_i64;
	(0..N)
		.map(|_| {
			t += i64::try_from(next() % 500_000 + 1_000).unwrap_or(1_000);
			t
		})
		.collect()
}

fn bits_per_value(bytes: usize) -> f64 {
	bytes as f64 * 8.0 / N as f64
}

fn bench_timestamps(c: &mut Criterion) {
	let corpora: Vec<(&str, Vec<i64>, TimeUnit)> = [btc_minutes().map(|ts| ("btc_minutes", ts, TimeUnit::Seconds)), Some(("ms_as_micros", ms_as_micros(), TimeUnit::Micros)), Some(("jittered_micros", jittered_micros(), TimeUnit::Micros))].into_iter().flatten().collect();
	for (name, ts, unit) in &corpora {
		let dod = encode_delta_of_delta(ts, *unit);
		let pco_bytes = pco::standalone::simple_compress(ts, &pco::ChunkConfig::default()).expect("pco compresses");
		assert_eq!(pco::standalone::simple_decompress::<i64>(&pco_bytes).expect("pco decompresses"), *ts, "{name}: pco must round-trip exactly");
		eprintln!("== {name} ({N} timestamps) — bits/value");
		eprintln!("  weft dod best ({:<22}) {:>7.3}", dod.best_encoding_name(), bits_per_value(dod.best_estimated_bytes()));
		eprintln!("  pco                                {:>7.3}", bits_per_value(pco_bytes.len()));

		let mut group = c.benchmark_group(format!("timestamp_decode_1mi/{name}"));
		group.sample_size(20);
		group.throughput(Throughput::Elements(N as u64));
		group.bench_function("pco", |b| b.iter(|| black_box(pco::standalone::simple_decompress::<i64>(black_box(&pco_bytes)).expect("pco decompresses"))));
		group.finish();
	}
}

criterion_group!(benches, bench_timestamps);
criterion_main!(benches);
