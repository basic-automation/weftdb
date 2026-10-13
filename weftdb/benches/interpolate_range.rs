//! Benchmark **interpolate-on-read over stored segments** (`SegmentStore::interpolate_range`)
//! end to end below the HTTP layer: index prune → `.weftseg` read and decode → knots →
//! splimes on the blocking pool → a `BigDecimal` series with provenance.
//!
//! This is the storage-backed half of the roadmap's end-to-end GPU interpolation benchmark
//! (Phase 5: "storage read → … → API serialization, with p95"); serialization is the HTTP
//! layer's and is not timed here. The corpus is 1 Mi irregular two-decimal samples (jittered
//! around a 10-second stride, a sinusoid plus noise) sealed as 16 segments; each iteration
//! reconstructs a one-second cubic grid over a window, so the output is ~10× the knots read.
//! Three backends are timed on the same windows: one CPU thread, rayon's pool, and the GPU
//! (`f64`; the GPU group is skipped with a note when no hardware GPU starts).

use std::hint::black_box;

use bigdecimal::BigDecimal;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use splimes::{Backend, Interpolator, Resolution, Spline};
use tempfile::TempDir;
use tokio::runtime::Runtime;
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weftdb::SegmentStore;

/// Stored samples.
const ROWS: i64 = 1 << 20;

/// Segments the corpus is sealed as.
const SEGMENTS: i64 = 16;

/// Nominal seconds between samples (each is jittered by up to ±3 s).
const STRIDE_SECS: i64 = 10;

/// Seal the corpus; the temp dir must outlive the store.
async fn sealed_store(dir: &TempDir) -> SegmentStore {
	let store = SegmentStore::open(dir.path()).await.expect("opens store");
	store.declare("sensor", &AspectSchema::new(PhysicalType::ScaledI64 { scale: 2 }, BigDecimal::from(0), TimeUnit::Seconds)).await.expect("declares aspect");
	let mut state = 0x9e37_79b9_7f4a_7c15_u64;
	let mut next = move || {
		state ^= state << 13;
		state ^= state >> 7;
		state ^= state << 17;
		state
	};
	let per_segment = ROWS / SEGMENTS;
	for seg in 0..SEGMENTS {
		let mut ts = Vec::with_capacity(usize::try_from(per_segment).unwrap_or(0));
		let mut vs = Vec::with_capacity(ts.capacity());
		for i in 0..per_segment {
			let n = seg * per_segment + i;
			ts.push(n * STRIDE_SECS + i64::try_from(next() % 7).unwrap_or(0) - 3);
			#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
			let cents = ((20.0_f64.mul_add((n as f64 / 360.0).sin(), 50.0) + (next() % 200) as f64 / 100.0) * 100.0).round() as i64;
			vs.push(BigDecimal::new(cents.into(), 2));
		}
		store.seal_declared("sensor", &ts, &vs).await.expect("seals");
	}
	store
}

fn bench_interpolate_range(c: &mut Criterion) {
	let rt = Runtime::new().expect("tokio runtime");
	let dir = TempDir::new().expect("temp dir");
	let store = rt.block_on(sealed_store(&dir));
	let gpu = match splimes::prewarm_gpu() {
		Ok(info) if info.device_type != "cpu" => true,
		Ok(info) => {
			eprintln!("interpolate_range: {} is a CPU adapter; skipping the GPU backend", info.name);
			false
		}
		Err(e) => {
			eprintln!("interpolate_range: no GPU ({e}); skipping the GPU backend");
			false
		}
	};
	let mut group = c.benchmark_group("interpolate_range");
	group.sample_size(10);
	// Windows of 10k and 100k knots: ~100k and ~1M one-second output points.
	for knots in [10_000_i64, 100_000] {
		let (start, end) = (ROWS / 2 * STRIDE_SECS, (ROWS / 2 + knots) * STRIDE_SECS);
		group.throughput(Throughput::Elements(u64::try_from(end - start + 1).unwrap_or(0)));
		let backends: &[(&str, Backend)] = if gpu { &[("cpu", Backend::Cpu), ("parallel", Backend::Parallel), ("gpu", Backend::Gpu)] } else { &[("cpu", Backend::Cpu), ("parallel", Backend::Parallel)] };
		for &(label, backend) in backends {
			let interpolator = Interpolator::new(Spline::Cubic, Resolution::Seconds).backend(backend);
			group.bench_with_input(BenchmarkId::new(label, knots), &knots, |b, _| b.iter(|| black_box(rt.block_on(store.interpolate_range("sensor", start, end, 60, interpolator)).expect("interpolates"))));
			group.bench_with_input(BenchmarkId::new(format!("{label}_f64"), knots), &knots, |b, _| b.iter(|| black_box(rt.block_on(store.interpolate_range_f64("sensor", start, end, 60, interpolator)).expect("interpolates"))));
		}
	}
	group.finish();

	// Where the time goes, on the 100k-knot window: the storage read and decode alone, then
	// the same interpolation on already-lifted knots with an `f64` result (`run_f64`, no
	// per-point `BigDecimal`) and with the `BigDecimal` result `interpolate_range` returns.
	let knots = 100_000_i64;
	let (start, end) = (ROWS / 2 * STRIDE_SECS, (ROWS / 2 + knots) * STRIDE_SECS);
	let mut group = c.benchmark_group("interpolate_range_parts");
	group.sample_size(10);
	group.bench_function("read_time_range", |b| b.iter(|| black_box(rt.block_on(store.read_time_range("sensor", start - 60, end + 60)).expect("reads"))));
	let (ts, vs) = rt.block_on(store.read_time_range("sensor", start - 60, end + 60)).expect("reads");
	let instants: Vec<chrono::DateTime<chrono::Utc>> = ts.iter().map(|&t| chrono::DateTime::from_timestamp(t, 0).expect("in range")).collect();
	let values: Vec<BigDecimal> = vs.into_iter().map(|v| v.expect("present")).collect();
	let floats: Vec<f64> = values.iter().map(|v| bigdecimal::ToPrimitive::to_f64(v).expect("finite")).collect();
	let points: Vec<splimes::Point> = instants.iter().zip(&values).map(|(&t, v)| splimes::Point::new(t, v.clone())).collect();
	let (from, to) = (chrono::DateTime::from_timestamp(start, 0).expect("in range"), chrono::DateTime::from_timestamp(end, 0).expect("in range"));
	let backends: &[(&str, Backend)] = if gpu { &[("parallel", Backend::Parallel), ("gpu", Backend::Gpu)] } else { &[("parallel", Backend::Parallel)] };
	for &(label, backend) in backends {
		let interpolator = Interpolator::new(Spline::Cubic, Resolution::Seconds).backend(backend);
		group.bench_function(format!("run_f64/{label}"), |b| b.iter(|| black_box(interpolator.run_f64(&instants, &floats, from, to).expect("interpolates"))));
		group.bench_function(format!("run_bigdecimal/{label}"), |b| b.iter(|| black_box(interpolator.run(&points, from, to).expect("interpolates"))));
	}
	group.finish();
	drop(store);
	drop(dir);
}

criterion_group!(benches, bench_interpolate_range);
criterion_main!(benches);
