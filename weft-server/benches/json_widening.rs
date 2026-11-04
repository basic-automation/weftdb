//! The compute endpoints' JSON `f64` → `BigDecimal` input widening, old against new
//! (roadmap Phase 5, FOUND 2026-10-09).
//!
//! The 2026-10-09 measurement compared the downsample generator's exact-binary-expansion
//! values (`--ds-decimals full`) with two-decimal values: 13.7x slower hourly `avg` and
//! 13.1x slower exact `p99` at 500k points. The compute endpoints widened JSON input
//! the first way (`BigDecimal::from_f64`); `weft_server::downsample::widen_json_value`
//! now widens through the shortest round-trip decimal. This bench reproduces that
//! comparison on the endpoint's own parse + reduce path: 500k one-second samples of
//! two-decimal values (as a client sends prices or sensor readings), each widened
//! either way, then reduced into hourly buckets.
//!
//! Groups (each with `from_f64` and `shortest` members):
//! - `widen`: the value lift alone (the `downsample.parse` span's work);
//! - `hourly_avg`: lift + hourly `avg`;
//! - `hourly_p99`: lift + hourly exact `p99`.
//!
//! `WEFT_WIDEN_BENCH_POINTS` overrides the point count (default 500000), e.g. for a
//! smoke run: `WEFT_WIDEN_BENCH_POINTS=5000 cargo bench -p weft-server --bench
//! json_widening -- --test`.

use std::{hint::black_box, time::Duration};

use bigdecimal::{BigDecimal, FromPrimitive};
use chrono::{DateTime, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, Criterion};
use splimes::{Point, Resolution};
use weft_reduce::Aggregation;
use weft_server::downsample::widen_json_value;

const DEFAULT_POINTS: usize = 500_000;

fn point_count() -> usize {
	std::env::var("WEFT_WIDEN_BENCH_POINTS").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_POINTS)
}

/// splitmix64, so the corpus is seeded without a dependency.
fn splitmix(state: &mut u64) -> u64 {
	*state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
	let mut z = *state;
	z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
	z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
	z ^ (z >> 31)
}

/// The request body's samples as the server deserialises them: one-second timestamps and
/// `f64` values parsed from two-decimal JSON text (`20 sin + 50`, plus noise in [-1, 1],
/// the shape of weft-bench's downsample generator).
fn corpus(n: usize) -> Vec<(DateTime<Utc>, f64)> {
	let anchor = Utc.timestamp_opt(1_577_836_800, 0).single().expect("valid anchor");
	let mut state = 0x00F6_4D1D_u64;
	(0..n).map(|i| {
		#[allow(clippy::cast_precision_loss)]
		let phase = i as f64 / 500.0;
		#[allow(clippy::cast_precision_loss)]
		let noise = (splitmix(&mut state) >> 11) as f64 / (1_u64 << 53) as f64 * 2.0 - 1.0;
		let signal = 20.0_f64.mul_add((phase * std::f64::consts::TAU).sin(), 50.0) + noise;
		// Two decimals as text, read back as JSON would read it.
		let value: f64 = format!("{signal:.2}").parse().expect("parses");
		#[allow(clippy::cast_possible_wrap)]
		(anchor + chrono::Duration::seconds(i as i64), value)
	})
	.collect()
}

fn lift(samples: &[(DateTime<Utc>, f64)], widen: fn(f64) -> Option<BigDecimal>) -> Vec<Point> {
	samples.iter().map(|&(t, v)| Point::new(t, widen(v).expect("finite"))).collect()
}

fn reduce(points: &[Point], aggregation: Aggregation) -> usize {
	weft_reduce::reduce(points, Resolution::Hours, None, None, &[aggregation]).expect("reduces").len()
}

fn bench(c: &mut Criterion) {
	let n = point_count();
	let samples = corpus(n);
	let ways: [(&str, fn(f64) -> Option<BigDecimal>); 2] = [("from_f64", BigDecimal::from_f64), ("shortest", widen_json_value)];

	// The two ways must agree once narrowed back to the wire f64, and the old one must
	// actually carry the long expansions this bench is about.
	let old = lift(&samples, BigDecimal::from_f64);
	let new = lift(&samples, widen_json_value);
	for aggregation in [Aggregation::Avg, Aggregation::P99] {
		let a = weft_reduce::reduce(&old, Resolution::Hours, None, None, &[aggregation]).expect("reduces");
		let b = weft_reduce::reduce(&new, Resolution::Hours, None, None, &[aggregation]).expect("reduces");
		assert_eq!(a.len(), b.len());
		for (x, y) in a.iter().zip(&b) {
			let (x, y) = (x.values.values().next().expect("one value"), y.values.values().next().expect("one value"));
			let (xf, yf) = (bigdecimal::ToPrimitive::to_f64(x).expect("finite"), bigdecimal::ToPrimitive::to_f64(y).expect("finite"));
			assert!((xf - yf).abs() <= f64::EPSILON * xf.abs().max(1.0), "{aggregation:?}: {xf} vs {yf}");
		}
	}
	drop((old, new));

	let mut group = c.benchmark_group("widen");
	group.sample_size(10).measurement_time(Duration::from_secs(10));
	for (name, widen) in ways {
		group.bench_function(name, |b| b.iter(|| black_box(lift(black_box(&samples), widen))));
	}
	group.finish();

	for (label, aggregation) in [("hourly_avg", Aggregation::Avg), ("hourly_p99", Aggregation::P99)] {
		let mut group = c.benchmark_group(label);
		group.sample_size(10).measurement_time(Duration::from_secs(20));
		for (name, widen) in ways {
			group.bench_function(name, |b| b.iter(|| black_box(reduce(&lift(black_box(&samples), widen), aggregation))));
		}
		group.finish();
	}
}

criterion_group!(benches, bench);
criterion_main!(benches);
