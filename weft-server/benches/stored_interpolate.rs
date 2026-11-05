//! End-to-end benchmark of **interpolation on read over stored data through HTTP**:
//! `GET /api/v1/storage/{aspect}/interpolate{,.csv,.arrow,.parquet}`, in process, from the
//! request to the last response byte — index prune → `.weftseg` read and decode → knots →
//! splimes (`Backend::Auto`) → wire serialization. This is the roadmap's Phase 5 "end-to-end
//! GPU interpolation benchmark (storage read → … → API serialization, with p95)"; the storage
//! half below the HTTP layer is `weftdb/benches/interpolate_range.rs`.
//!
//! Each window is also timed through the library call the route makes
//! (`SegmentStore::interpolate_range_f64`, the `lib` row), so the gap between `lib` and a
//! format's row is the HTTP layer and that format's serialization.
//!
//! The corpus is the storage bench's: irregular two-decimal samples jittered around a 10-second
//! stride, a sinusoid plus noise, sealed as 16 segments; each window is reconstructed as a
//! one-second cubic grid, so the output is ~10× the knots read. Hand-rolled (no criterion) so
//! it reports p50, p95 and max per case, the figures the roadmap asks for.
//!
//! Knobs (environment):
//!
//! - `WEFT_SI_ROWS` — stored samples (default 1048576);
//! - `WEFT_SI_KNOTS` — comma-separated window sizes in stored samples (default `10000,100000`);
//! - `WEFT_SI_REPS` — timed repetitions per case after one untimed warm-up (default 20);
//! - `WEFT_GPU_CALIBRATE` — as on the server: unset calibrates `Backend::Auto` first (so `Auto`
//!   may pick the GPU), `0` skips it (CPU-only defaults), `force` calibrates a software adapter.
//!
//! Run: `cargo bench -p weft-server --bench stored_interpolate`.

use std::{
	sync::Arc, time::{Duration, Instant}
};

use axum::{body::Body, http::Request};
use bigdecimal::BigDecimal;
use splimes::{Interpolator, Resolution, Spline};
use tempfile::TempDir;
use tower::ServiceExt;
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weft_server::{
	app_with_state, gpu::{self, CalibrationMode}, AppState
};
use weftdb::SegmentStore;

/// Segments the corpus is sealed as.
const SEGMENTS: i64 = 16;

/// Nominal seconds between samples (each is jittered by up to ±3 s).
const STRIDE_SECS: i64 = 10;

fn env_parse<T: std::str::FromStr>(name: &str, default: T) -> T {
	std::env::var(name).ok().and_then(|raw| raw.trim().parse().ok()).unwrap_or(default)
}

/// Seal `rows` samples as [`SEGMENTS`] segments of aspect `sensor`.
async fn sealed_store(dir: &TempDir, rows: i64) -> SegmentStore {
	let store = SegmentStore::open(dir.path()).await.expect("opens store");
	store.declare("sensor", &AspectSchema::new(PhysicalType::ScaledI64 { scale: 2 }, BigDecimal::from(0), TimeUnit::Seconds)).await.expect("declares aspect");
	let mut state = 0x9e37_79b9_7f4a_7c15_u64;
	let mut next = move || {
		state ^= state << 13;
		state ^= state >> 7;
		state ^= state << 17;
		state
	};
	let per_segment = (rows / SEGMENTS).max(1);
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

/// The `q`-quantile (nearest rank) of sorted `samples`.
fn quantile(samples: &[Duration], q: f64) -> Duration {
	#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
	let rank = ((q * samples.len() as f64).ceil() as usize).clamp(1, samples.len());
	samples[rank - 1]
}

fn ms(d: Duration) -> f64 {
	d.as_secs_f64() * 1e3
}

fn report(case: &str, knots: i64, points: usize, bytes: usize, backend: &str, mut samples: Vec<Duration>) {
	samples.sort_unstable();
	println!("{case:<8} knots={knots:<8} points={points:<9} bytes={bytes:<10} backend={backend:<9} reps={:<3} p50={:>9.3}ms p95={:>9.3}ms max={:>9.3}ms", samples.len(), ms(quantile(&samples, 0.5)), ms(quantile(&samples, 0.95)), ms(*samples.last().expect("reps > 0")));
}

fn main() {
	// `cargo bench` passes `--bench`; a filter argument is not supported, so ignore arguments.
	let rows: i64 = env_parse("WEFT_SI_ROWS", 1_i64 << 20);
	let reps: usize = env_parse::<usize>("WEFT_SI_REPS", 20).max(1);
	let windows: Vec<i64> = std::env::var("WEFT_SI_KNOTS").ok().map_or_else(|| vec![10_000, 100_000], |raw| raw.split(',').filter_map(|k| k.trim().parse().ok()).collect());
	let mode = CalibrationMode::from_env_value(std::env::var(gpu::GPU_CALIBRATE_ENV).ok().as_deref());

	let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
	let dir = TempDir::new().expect("temp dir");
	let store = Arc::new(rt.block_on(sealed_store(&dir, rows)));
	println!("stored_interpolate: {rows} rows in {SEGMENTS} segments, {reps} reps per case, calibration {mode:?}");
	if mode != CalibrationMode::Off {
		for line in gpu::calibrate(mode) {
			println!("{line}");
		}
	}
	let router = app_with_state(AppState::new().with_store(Arc::clone(&store)));

	for knots in windows {
		let knots = knots.clamp(1, rows);
		let first = (rows - knots) / 2;
		let (start, end) = (first * STRIDE_SECS, (first + knots) * STRIDE_SECS);
		// A margin of a few strides gives the cubic its edge knots.
		let margin = 4 * STRIDE_SECS;

		// The library call the route makes, below the HTTP layer.
		let interpolator = Interpolator::new(Spline::Cubic, Resolution::Seconds).max_points(weft_server::interpolate::MAX_INTERPOLATE_OUTPUT_POINTS);
		let mut samples = Vec::with_capacity(reps);
		let (mut points, mut backend) = (0, String::new());
		for rep in 0..=reps {
			let began = Instant::now();
			let series = rt.block_on(store.interpolate_range_f64("sensor", start, end, margin, interpolator)).expect("interpolates");
			let took = began.elapsed();
			points = series.len();
			backend = format!("{:?}", series.backend());
			if rep > 0 {
				samples.push(took);
			}
		}
		report("lib", knots, points, 0, &backend, samples);

		for suffix in ["", ".csv", ".arrow", ".parquet"] {
			let uri = format!("/api/v1/storage/sensor/interpolate{suffix}?start={start}&end={end}&resolution=seconds&spline=cubic&margin={margin}");
			let mut samples = Vec::with_capacity(reps);
			let mut bytes = 0;
			for rep in 0..=reps {
				let began = Instant::now();
				let body = rt.block_on(async {
					let response = router.clone().oneshot(Request::builder().uri(&uri).body(Body::empty()).expect("request")).await.expect("infallible");
					assert!(response.status().is_success(), "{uri}: {}", response.status());
					axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body")
				});
				let took = began.elapsed();
				bytes = body.len();
				if rep > 0 {
					samples.push(took);
				}
			}
			let case = if suffix.is_empty() { "json" } else { &suffix[1..] };
			report(case, knots, points, bytes, "auto", samples);
		}
	}
	drop(router);
	drop(store);
}
