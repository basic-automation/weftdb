//! WeftDB's own **exact-decimal vs `f64` slowdown factor** for a bucketed reduction (roadmap
//! "positioning alert": QuestDB documents its `DECIMAL` at ~2× `DOUBLE`, so WeftDB has to state
//! its own figure or the precision wedge is asserted rather than measured).
//!
//! One workload, three arithmetic paths, the same real data: 1 Mi BTC/USD one-minute closes
//! (rows 3,000,000 onward of `database/datasets/btc_1min.csv`; `WEFT_BTC_CSV` overrides the
//! path; the bench is skipped with a note when it is absent), reduced to an hourly `avg`.
//!
//! - `bigdecimal_reduce`: the shipped path, [`weft_reduce::reduce`] over `BigDecimal` points.
//! - `f64_loop`: the same buckets and `avg` in `f64`, the lossy baseline every float TSDB uses.
//! - `scaled_i64_loop`: the same buckets over the **exact** scale-8 `i64` mantissas a
//!   `ScaledI64` column already stores, summed in `i128`. It is exact like `BigDecimal` and
//!   integer-fast like `f64`, and it is what a `ScaledI64`-native reduction would cost.
//! - `reduce_scaled`: the shipped integer-native reduction ([`weft_reduce::reduce_scaled`]) over
//!   the same mantissas, producing the same `Bucket`s as `bigdecimal_reduce` (asserted equal).
//!
//! A second pair, `segment_*`, times the two routes `SegmentStore::downsample_range` takes for one
//! sealed segment: the window read as `BigDecimal` and reduced by `reduce_partial`, against the
//! window read physically and reduced by `reduce_partial_scaled`. It covers decode plus
//! reduction for one real 1 Mi-row `.weftseg` frame.
//!
//! Before timing, the three are checked against each other: the integer sums must equal the
//! `BigDecimal` sums exactly, and the `f64` averages must agree to 1e-9 relative.

use std::{
	hint::black_box, io::{BufRead, BufReader}, str::FromStr
};

use bigdecimal::{BigDecimal, ToPrimitive};
use chrono::DateTime;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use splimes::{Point, Resolution};
use weft_physical_type::{
	weftseg::{read_segment_range, read_segment_range_physical}, PhysicalValue, Segment, TimeUnit
};
use weft_reduce::{reduce, reduce_partial, reduce_partial_scaled, reduce_scaled, Aggregation, Bucket};

const N: usize = 1 << 20;
const SKIP: usize = 3_000_000;
const SCALE: i64 = 8;
const HOUR: i64 = 3_600;

struct Corpus {
	points: Vec<Point>,
	seconds: Vec<i64>,
	floats: Vec<f64>,
	mantissas: Vec<i64>,
}

fn load() -> Option<Corpus> {
	let path = std::env::var("WEFT_BTC_CSV").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../database/datasets/btc_1min.csv").to_string());
	let Ok(file) = std::fs::File::open(&path) else {
		eprintln!("decimal_tax: {path} not found; bench skipped");
		return None;
	};
	let rows: Vec<(i64, String)> = BufReader::new(file)
		.lines()
		.skip(1 + SKIP)
		.take(N)
		.map(|line| {
			let line = line.expect("reads a line");
			let mut fields = line.split(',');
			let ts = fields.next().and_then(|f| f.split('.').next()).and_then(|f| f.parse().ok()).expect("an integer epoch");
			(ts, fields.nth(3).expect("a Close column").to_string())
		})
		.collect();
	assert_eq!(rows.len(), N, "the BTC corpus is shorter than {SKIP} + {N} rows");
	let scale = 10_i64.pow(u32::try_from(SCALE).unwrap_or(0));
	let points = rows.iter().map(|(t, v)| Point { timestamp: DateTime::from_timestamp(*t, 0).expect("in range"), value: BigDecimal::from_str(v).expect("a decimal") }).collect::<Vec<_>>();
	let mantissas = points.iter().map(|p| (p.value.clone() * BigDecimal::from(scale)).to_i64().expect("fits i64 at scale 8")).collect();
	Some(Corpus { seconds: rows.iter().map(|(t, _)| *t).collect(), floats: rows.iter().map(|(_, v)| v.parse().expect("a float")).collect(), mantissas, points })
}

/// Hourly `(bucket, sum, count)` in `f64`, then `avg`.
fn f64_hourly_avg(seconds: &[i64], values: &[f64]) -> Vec<(i64, f64)> {
	let mut out: Vec<(i64, f64)> = Vec::new();
	let (mut bucket, mut sum, mut count) = (seconds[0].div_euclid(HOUR), 0.0_f64, 0_u32);
	for (&t, &v) in seconds.iter().zip(values) {
		let b = t.div_euclid(HOUR);
		if b != bucket {
			out.push((bucket * HOUR, sum / f64::from(count)));
			(bucket, sum, count) = (b, 0.0, 0);
		}
		sum += v;
		count += 1;
	}
	out.push((bucket * HOUR, sum / f64::from(count)));
	out
}

/// Hourly exact `(bucket, sum of scale-8 mantissas, count)` in `i128`.
fn scaled_hourly_sums(seconds: &[i64], mantissas: &[i64]) -> Vec<(i64, i128, i64)> {
	let mut out: Vec<(i64, i128, i64)> = Vec::new();
	let (mut bucket, mut sum, mut count) = (seconds[0].div_euclid(HOUR), 0_i128, 0_i64);
	for (&t, &m) in seconds.iter().zip(mantissas) {
		let b = t.div_euclid(HOUR);
		if b != bucket {
			out.push((bucket * HOUR, sum, count));
			(bucket, sum, count) = (b, 0, 0);
		}
		sum += i128::from(m);
		count += 1;
	}
	out.push((bucket * HOUR, sum, count));
	out
}

fn bench_decimal_tax(c: &mut Criterion) {
	let Some(corpus) = load() else {
		return;
	};
	// Cross-check the three paths before timing anything.
	let shipped = reduce(&corpus.points, Resolution::Hours, None, None, &[Aggregation::Sum, Aggregation::Avg]).expect("reduces");
	let floats = f64_hourly_avg(&corpus.seconds, &corpus.floats);
	let exact = scaled_hourly_sums(&corpus.seconds, &corpus.mantissas);
	assert_eq!((shipped.len(), floats.len()), (exact.len(), exact.len()), "every path must produce the same buckets");
	let scale = BigDecimal::from(10_i64.pow(u32::try_from(SCALE).unwrap_or(0)));
	for (bucket, (start, avg), (_, sum, count)) in shipped.iter().zip(&floats).zip(&exact).map(|((b, f), e)| (b, f, e)) {
		assert_eq!(bucket.count, usize::try_from(*count).unwrap_or(0));
		let shipped_sum = bucket.values.get(Aggregation::Sum.as_str()).expect("sum");
		assert_eq!(shipped_sum * &scale, BigDecimal::from(*sum), "integer sum must equal the BigDecimal sum exactly (bucket {start})");
		let shipped_avg = bucket.values.get(Aggregation::Avg.as_str()).and_then(ToPrimitive::to_f64).expect("avg");
		assert!((shipped_avg - avg).abs() <= 1e-9 * shipped_avg.abs(), "f64 avg {avg} vs BigDecimal avg {shipped_avg} (bucket {start})");
	}
	let nanos: Vec<i64> = corpus.seconds.iter().map(|&t| t * 1_000_000_000).collect();
	let scale_u32 = u32::try_from(SCALE).unwrap_or(0);
	let avg_only = reduce(&corpus.points, Resolution::Hours, None, None, &[Aggregation::Avg]).expect("reduces");
	assert_eq!(reduce_scaled(&nanos, &corpus.mantissas, scale_u32, Resolution::Hours, None, None, &[Aggregation::Avg]).expect("reduces"), Some(avg_only), "reduce_scaled must equal reduce bucket for bucket");
	eprintln!("decimal_tax: {} hourly buckets over {N} real closes; all four paths agree", exact.len());

	let mut group = c.benchmark_group("hourly_avg_1mi_btc");
	group.sample_size(10);
	group.throughput(Throughput::Elements(N as u64));
	group.bench_function("bigdecimal_reduce", |b| b.iter(|| black_box(reduce(black_box(&corpus.points), Resolution::Hours, None, None, &[Aggregation::Avg]).expect("reduces"))));
	group.bench_function("f64_loop", |b| b.iter(|| black_box(f64_hourly_avg(black_box(&corpus.seconds), black_box(&corpus.floats)))));
	group.bench_function("reduce_scaled", |b| b.iter(|| black_box(reduce_scaled(black_box(&nanos), black_box(&corpus.mantissas), scale_u32, Resolution::Hours, None, None, &[Aggregation::Avg]).expect("reduces"))));
	group.bench_function("scaled_i64_loop", |b| b.iter(|| black_box(scaled_hourly_sums(black_box(&corpus.seconds), black_box(&corpus.mantissas)))));
	group.finish();

	// The same two shipped paths with `sum` only, which isolates the per-bucket `avg` division
	// (a `BigDecimal` division at its default precision) from the per-sample accumulation.
	let sum_only = reduce(&corpus.points, Resolution::Hours, None, None, &[Aggregation::Sum]).expect("reduces");
	assert_eq!(reduce_scaled(&nanos, &corpus.mantissas, scale_u32, Resolution::Hours, None, None, &[Aggregation::Sum]).expect("reduces"), Some(sum_only));
	let mut group = c.benchmark_group("hourly_sum_1mi_btc");
	group.sample_size(10);
	group.throughput(Throughput::Elements(N as u64));
	group.bench_function("bigdecimal_reduce", |b| b.iter(|| black_box(reduce(black_box(&corpus.points), Resolution::Hours, None, None, &[Aggregation::Sum]).expect("reduces"))));
	group.bench_function("reduce_scaled", |b| b.iter(|| black_box(reduce_scaled(black_box(&nanos), black_box(&corpus.mantissas), scale_u32, Resolution::Hours, None, None, &[Aggregation::Sum]).expect("reduces"))));
	group.finish();
}

/// The `BigDecimal` route through one sealed segment: windowed read, lift to points, reduce.
fn segment_bigdecimal(bytes: &[u8], aggs: &[Aggregation]) -> Vec<Bucket> {
	let (ts, vs) = read_segment_range(bytes, i64::MIN, i64::MAX).expect("reads");
	let points: Vec<Point> = ts.into_iter().zip(vs).filter_map(|(t, v)| v.map(|value| Point { timestamp: DateTime::from_timestamp(t, 0).expect("in range"), value })).collect();
	reduce_partial(&points, Resolution::Hours, None, None, aggs).expect("reduces").finish(Resolution::Hours, aggs).expect("finishes")
}

/// The integer route: physical windowed read, mantissas, `reduce_partial_scaled`.
fn segment_scaled(bytes: &[u8], aggs: &[Aggregation]) -> Vec<Bucket> {
	let (ts, vs) = read_segment_range_physical(bytes, i64::MIN, i64::MAX).expect("reads");
	let (mut nanos, mut mantissas, mut scale) = (Vec::with_capacity(ts.len()), Vec::with_capacity(ts.len()), 0_u8);
	for (t, v) in ts.into_iter().zip(vs) {
		if let Some(PhysicalValue::ScaledI64 { mantissa, scale: s }) = v {
			nanos.push(t * 1_000_000_000);
			mantissas.push(mantissa);
			scale = s;
		}
	}
	reduce_partial_scaled(&nanos, &mantissas, u32::from(scale), Resolution::Hours, None, None, aggs).expect("reduces").expect("covered").finish(Resolution::Hours, aggs).expect("finishes")
}

fn bench_segment_downsample(c: &mut Criterion) {
	let Some(corpus) = load() else {
		return;
	};
	let values: Vec<BigDecimal> = corpus.points.iter().map(|p| p.value.clone()).collect();
	let bytes = Segment::build_sorted(&corpus.seconds, &values, TimeUnit::Seconds, &BigDecimal::from(0)).expect("seals").write_to();
	let streaming = [Aggregation::Min, Aggregation::Max, Aggregation::Avg, Aggregation::Sum, Aggregation::First, Aggregation::Last];
	let with_sketch = [Aggregation::Avg, Aggregation::SketchP99];
	for aggs in [&streaming[..], &with_sketch[..]] {
		assert_eq!(segment_scaled(&bytes, aggs), segment_bigdecimal(&bytes, aggs), "both segment routes must produce the same buckets");
	}
	eprintln!("segment_downsample: {} byte frame, both routes agree", bytes.len());
	let mut group = c.benchmark_group("segment_downsample_1mi_btc");
	group.sample_size(10);
	group.throughput(Throughput::Elements(N as u64));
	group.bench_function("bigdecimal_streaming6", |b| b.iter(|| black_box(segment_bigdecimal(black_box(&bytes), &streaming))));
	group.bench_function("scaled_streaming6", |b| b.iter(|| black_box(segment_scaled(black_box(&bytes), &streaming))));
	group.bench_function("bigdecimal_avg_sketch_p99", |b| b.iter(|| black_box(segment_bigdecimal(black_box(&bytes), &with_sketch))));
	group.bench_function("scaled_avg_sketch_p99", |b| b.iter(|| black_box(segment_scaled(black_box(&bytes), &with_sketch))));
	group.finish();
}

criterion_group!(benches, bench_decimal_tax, bench_segment_downsample);
criterion_main!(benches);
