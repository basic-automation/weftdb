//! K-way merge vs the serial pairwise fold: one merge of `k` overlapping frames.
//!
//! ROB-20 G3 (`docs/maintenance-budgets-plan.md` slice 3) replaces the pairwise fold of a
//! maintenance merge's inputs — `k` rounds of two-way merging, each of which re-clones the
//! whole merged prefix — with a single heap pass over `k` cursors. The merge output is
//! identical either way, asserted below before anything is timed, so what is measured is the
//! cost of the fold rather than a behaviour change. The frames overlap on purpose: each
//! frame slides past the previous one by a few instants, so a shared timestamp makes the
//! merge drop real runs instead of concatenating. Both arms see the same inputs.
#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};
use weft_physical_type::{merge_k_way, merge_newer_wins};

/// `k` frames of `rows` rows each, sliding past one another by `shift` instants so that
/// adjacent frames share most of their timestamps. Values carry their frame and their
/// position, so an arm that drops or reorders a row fails the equality assert instead of
/// reporting a faster number.
fn frames(k: usize, rows: usize, shift: i64) -> Vec<Vec<(i64, i64)>> {
	(0..i64::try_from(k).expect("k fits in i64")).map(|frame| (0..i64::try_from(rows).expect("rows fit in i64")).map(|index| (index + frame * shift, frame * 1_000_000 + index)).collect()).collect()
}

/// The k-way arm: one heap pass over `k` cursors.
fn k_way(runs: &[Vec<(i64, i64)>]) -> Vec<(i64, i64)> {
	let refs: Vec<&[(i64, i64)]> = runs.iter().map(Vec::as_slice).collect();
	merge_k_way(&refs)
}

/// The arm being replaced: fold the prefix against each frame in turn.
fn pairwise(runs: &[Vec<(i64, i64)>]) -> Vec<(i64, i64)> {
	let mut merged: Vec<(i64, i64)> = Vec::new();
	for run in runs {
		merged = merge_newer_wins(&merged, run);
	}
	merged
}

fn bench_merge(c: &mut Criterion) {
	for k in [2_usize, 4, 8, 16, 32] {
		let runs = frames(k, 2048, 3);
		assert_eq!(k_way(&runs), pairwise(&runs), "k={k}: the k-way merge must equal the pairwise fold");
		c.bench_function(&format!("kway_k{k}"), |b| b.iter(|| k_way(black_box(&runs))));
		c.bench_function(&format!("pairwise_k{k}"), |b| b.iter(|| pairwise(black_box(&runs))));
	}
}

criterion_group!(benches, bench_merge);
criterion_main!(benches);
