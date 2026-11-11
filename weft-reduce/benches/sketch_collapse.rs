//! What the uniform collapse of [`UddSketch`] costs next to [`DdSketch`]'s lowest-bucket
//! collapse, for the two operations a reduction performs: folding samples in, and merging
//! per-segment sketches.
//!
//! Two synthetic inputs, both seeded and fixed, 100,000 samples each:
//!
//! - `walk`: a multiplicative random walk from 100 with steps within ±0.1%, the shape of a
//!   price or a latency series. Consecutive samples usually share a bucket, and neither
//!   sketch collapses at a 2048-bucket budget.
//! - `wide`: log-uniform over eighteen decades (1e-9 to 1e9), which at 1% accuracy would
//!   occupy about 2,070 buckets. Both sketches run at a 256-bucket budget, so both
//!   collapse — the regime that separates them.
//!
//! `sketch_add/*` folds one input into an empty sketch. `sketch_merge/*` merges 64 sketches
//! of 1,562 `wide` samples each into an empty one, as a cross-segment read does.
//!
//! Before timing, each `UddSketch` is checked against an exact sort: every quantile read
//! must be inside the bound the sketch reports.
//!
//! It also prints, without timing it, what each collapse costs in accuracy on real data: the
//! relative error of both sketches at p1, p50 and p99 over every BTC/USD one-minute close in
//! `database/datasets/btc_1min.csv` (`WEFT_BTC_CSV` overrides the path; skipped with a note
//! when the file is absent), at budgets of 256, 64 and 16 buckets.
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]

use std::{
	hint::black_box, io::{BufRead, BufReader}
};

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use weft_reduce::{DdSketch, UddSketch};

const N: u32 = 100_000;
const ALPHA: f64 = 0.01;
const ROOMY: usize = 2048;
const TIGHT: usize = 256;
const PARTS: usize = 64;

fn xorshift(seed: u64) -> impl FnMut() -> u64 {
	let mut state = seed;
	move || {
		state ^= state << 13;
		state ^= state >> 7;
		state ^= state << 17;
		state
	}
}

/// A draw in `[0, 1)` with 32 bits of resolution.
fn unit(next: &mut impl FnMut() -> u64) -> f64 {
	f64::from(u32::try_from(next() >> 32).unwrap_or(0)) / 4_294_967_296.0
}

fn walk() -> Vec<f64> {
	let mut next = xorshift(0x6a09_e667_f3bc_c909);
	(0..N).scan(100.0_f64, |price, _| {
		*price *= unit(&mut next).mul_add(2e-3, 1.0 - 1e-3);
		Some(*price)
	})
	.collect()
}

fn wide() -> Vec<f64> {
	let mut next = xorshift(0x9e37_79b9_7f4a_7c15);
	(0..N).map(|_| 10_f64.powf(unit(&mut next).mul_add(18.0, -9.0))).collect()
}

fn dd(values: &[f64], max_bins: usize) -> DdSketch {
	let mut s = DdSketch::with_max_bins(ALPHA, max_bins).expect("valid");
	for &v in values {
		s.add(v).expect("in range");
	}
	s
}

fn udd(values: &[f64], max_bins: usize) -> UddSketch {
	let mut s = UddSketch::new(ALPHA, max_bins).expect("valid");
	for &v in values {
		s.add(v).expect("in range");
	}
	s
}

/// Every quantile `sketch` reports must be inside the bound it reports.
fn check(values: &[f64], sketch: &UddSketch) {
	let mut sorted = values.to_vec();
	sorted.sort_by(f64::total_cmp);
	let n = sorted.len();
	let bound = sketch.relative_accuracy();
	for per_mille in [0_usize, 1, 10, 100, 500, 900, 990, 999, 1000] {
		let ordinal = (per_mille * n).div_ceil(1000).clamp(1, n);
		let want = sorted[ordinal - 1];
		let q = f64::from(u16::try_from(per_mille).unwrap_or(0)) / 1000.0;
		let got = sketch.quantile(q).expect("non-empty");
		let rel = (got - want).abs() / want.abs();
		assert!(rel <= bound + 1e-12, "q = {q}: {got} against {want}, relative error {rel} over the reported {bound}");
	}
}

/// The exact nearest-rank sample of `sorted` at `per_mille` thousandths.
fn exact(sorted: &[f64], per_mille: usize) -> f64 {
	let n = sorted.len();
	sorted[(per_mille * n).div_ceil(1000).clamp(1, n) - 1]
}

/// Print each sketch's relative error at p1, p50 and p99 over the BTC closes, per budget.
fn report_real_corpus() {
	let path = std::env::var("WEFT_BTC_CSV").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../database/datasets/btc_1min.csv").to_string());
	let Ok(file) = std::fs::File::open(&path) else {
		eprintln!("sketch_collapse: {path} not found; the real-corpus error table is skipped");
		return;
	};
	let mut closes: Vec<f64> = BufReader::new(file).lines().skip(1).filter_map(|line| line.ok()?.split(',').nth(4)?.parse().ok()).filter(|v: &f64| v.is_finite() && *v > 0.0).collect();
	let (dd_all, udd_all): (Vec<DdSketch>, Vec<UddSketch>) = [256, 64, 16].into_iter().map(|bins| (dd(&closes, bins), udd(&closes, bins))).unzip();
	closes.sort_by(f64::total_cmp);
	eprintln!("btc closes: {} samples, {} to {}; relative error at p1 / p50 / p99", closes.len(), closes[0], closes[closes.len() - 1]);
	for (bins, (dd, udd)) in [256, 64, 16].into_iter().zip(dd_all.iter().zip(&udd_all)) {
		let errors = |quantile: &dyn Fn(f64) -> Option<f64>| {
			[(10, 0.01), (500, 0.5), (990, 0.99)].map(|(per_mille, q)| {
				let want = exact(&closes, per_mille);
				(quantile(q).expect("non-empty") - want).abs() / want
			})
		};
		let (d, u) = (errors(&|q| dd.quantile(q)), errors(&|q| udd.quantile(q)));
		let bound = udd.relative_accuracy();
		assert!(u.iter().all(|e| *e <= bound + 1e-12), "UddSketch outside its reported bound {bound} at {bins} buckets: {u:?}");
		eprintln!("  {bins:>3} buckets: DdSketch {:.4} / {:.4} / {:.4}; UddSketch {:.4} / {:.4} / {:.4} ({} collapse(s), bound {bound:.4})", d[0], d[1], d[2], u[0], u[1], u[2], udd.collapses());
	}
}

fn bench_add(c: &mut Criterion) {
	report_real_corpus();
	let (walk, wide) = (walk(), wide());
	let (roomy, tight) = (udd(&walk, ROOMY), udd(&wide, TIGHT));
	assert_eq!(roomy.collapses(), 0, "the walk fits the roomy budget");
	assert!(tight.collapses() > 0, "the wide input must collapse at the tight budget");
	check(&walk, &roomy);
	check(&wide, &tight);
	eprintln!("wide at {TIGHT} buckets: UddSketch collapsed {} time(s), reported bound {:.4}", tight.collapses(), tight.relative_accuracy());

	let mut group = c.benchmark_group("sketch_add");
	group.throughput(Throughput::Elements(u64::from(N)));
	group.bench_function("dd_walk", |b| b.iter(|| black_box(dd(black_box(&walk), ROOMY))));
	group.bench_function("udd_walk", |b| b.iter(|| black_box(udd(black_box(&walk), ROOMY))));
	group.bench_function("dd_wide_collapsing", |b| b.iter(|| black_box(dd(black_box(&wide), TIGHT))));
	group.bench_function("udd_wide_collapsing", |b| b.iter(|| black_box(udd(black_box(&wide), TIGHT))));
	group.finish();
}

fn bench_merge(c: &mut Criterion) {
	let wide = wide();
	let per_part = wide.len() / PARTS;
	let dd_parts: Vec<DdSketch> = wide.chunks_exact(per_part).map(|part| dd(part, TIGHT)).collect();
	let udd_parts: Vec<UddSketch> = wide.chunks_exact(per_part).map(|part| udd(part, TIGHT)).collect();

	// The property the merge exists for: the parts merge to the one-pass sketch exactly.
	let covered = &wide[..per_part * PARTS];
	let mut merged = UddSketch::new(ALPHA, TIGHT).expect("valid");
	for part in &udd_parts {
		merged.merge(part).expect("same accuracy");
	}
	assert_eq!(merged, udd(covered, TIGHT), "merged parts are the one-pass sketch");

	let mut group = c.benchmark_group("sketch_merge");
	group.throughput(Throughput::Elements(u64::try_from(PARTS).unwrap_or(0)));
	group.bench_function("dd_wide_collapsing", |b| {
		b.iter(|| {
			let mut all = DdSketch::with_max_bins(ALPHA, TIGHT).expect("valid");
			for part in black_box(&dd_parts) {
				all.merge(part).expect("same accuracy");
			}
			black_box(all)
		});
	});
	group.bench_function("udd_wide_collapsing", |b| {
		b.iter(|| {
			let mut all = UddSketch::new(ALPHA, TIGHT).expect("valid");
			for part in black_box(&udd_parts) {
				all.merge(part).expect("same accuracy");
			}
			black_box(all)
		});
	});
	group.finish();
}

criterion_group!(benches, bench_add, bench_merge);
criterion_main!(benches);
