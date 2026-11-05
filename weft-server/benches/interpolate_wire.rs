//! What the `f64` engine path saves `POST /api/v1/interpolate` (roadmap Phase 5, "the
//! compute endpoints pay the same tax").
//!
//! Two groups, over one request: 1,000 cubic knots onto a one-second grid of
//! `WEFT_BENCH_INTERPOLATE_POINTS` output points (default 1,000,000).
//!
//! - `interpolate_wire/engine_shape/{bigdecimal,f64}`: the engine call plus the shaping
//!   of every output value into its wire `f64`, in process: splimes' `run` with a
//!   `BigDecimal` narrowed per point (the path the endpoints ran before) against
//!   `run_f64` (the path they run now). The before/after on one build.
//! - `interpolate_wire/e2e/{json,arrow}`: the whole request through the router
//!   (`weft_server::app`), body read included. It uses only the public router, so the
//!   same file measures any commit: run it on the base for "before".
//!
//! Both run splimes' default `Backend::Auto` with its built-in thresholds (the server's
//! startup calibration does not run here), so a large grid may take the GPU.
//!
//! ```text
//! cargo bench -p weft-server --bench interpolate_wire
//! WEFT_BENCH_INTERPOLATE_POINTS=10000 cargo bench -p weft-server --bench interpolate_wire -- --quick   # smoke
//! ```

use std::hint::black_box;

use axum::{
	body::Body, http::{Request, StatusCode}
};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use chrono::{DateTime, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use splimes::{Interpolator, Point, Resolution, Spline};
use tower::ServiceExt as _;

const KNOTS: i64 = 1_000;

fn output_points() -> i64 {
	std::env::var("WEFT_BENCH_INTERPOLATE_POINTS").ok().and_then(|v| v.parse().ok()).filter(|&n: &i64| n >= 2).unwrap_or(1_000_000)
}

fn at(secs: i64) -> DateTime<Utc> {
	Utc.timestamp_opt(secs, 0).single().expect("valid timestamp")
}

/// `KNOTS` samples of a smooth, non-trivial signal spread evenly over `0..points` seconds.
fn knots(points: i64) -> Vec<(DateTime<Utc>, f64)> {
	let step = ((points - 1) / (KNOTS - 1)).max(1);
	(0..KNOTS).map(|i| (at(i * step), (i as f64 * 0.37).sin() * 1_000.0 + i as f64 * 0.125)).collect()
}

fn engine_shape(c: &mut Criterion) {
	let points = output_points();
	let samples = knots(points);
	let (start, end) = (at(0), at(points - 1));
	let interpolator = Interpolator::new(Spline::Cubic, Resolution::Seconds).max_points(usize::MAX);
	let big: Vec<Point> = samples.iter().map(|&(t, v)| Point::new(t, BigDecimal::from_f64(v).expect("finite"))).collect();
	let timestamps: Vec<DateTime<Utc>> = samples.iter().map(|&(t, _)| t).collect();
	let values: Vec<f64> = samples.iter().map(|&(_, v)| v).collect();

	let mut group = c.benchmark_group("interpolate_wire/engine_shape");
	group.sample_size(10);
	group.throughput(Throughput::Elements(points as u64));
	group.bench_function(BenchmarkId::new("bigdecimal", points), |b| {
		b.iter(|| {
			let output = interpolator.run(black_box(&big), start, end).expect("interpolates");
			let wire: Vec<f64> = output.iter().map(|(_, v, _)| v.to_f64().filter(|v| v.is_finite()).expect("finite")).collect();
			black_box(wire)
		});
	});
	group.bench_function(BenchmarkId::new("f64", points), |b| {
		b.iter(|| {
			let output = interpolator.run_f64(black_box(&timestamps), black_box(&values), start, end).expect("interpolates");
			let wire: Vec<f64> = output.iter().map(|(_, &v, _)| v + 0.0).collect();
			black_box(wire)
		});
	});
	group.finish();
}

fn e2e(c: &mut Criterion) {
	let points = output_points();
	let samples = knots(points);
	let body = serde_json::to_vec(&serde_json::json!({
		"spline": "cubic",
		"resolution": "seconds",
		"start": at(0).to_rfc3339(),
		"end": at(points - 1).to_rfc3339(),
		"points": samples.iter().map(|(t, v)| serde_json::json!({ "timestamp": t.to_rfc3339(), "value": v })).collect::<Vec<_>>(),
	}))
	.expect("serialises");
	let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
	let router = weft_server::app();

	let mut group = c.benchmark_group("interpolate_wire/e2e");
	group.sample_size(10);
	group.throughput(Throughput::Elements(points as u64));
	for (name, uri) in [("json", "/api/v1/interpolate"), ("arrow", "/api/v1/interpolate/arrow")] {
		group.bench_function(BenchmarkId::new(name, points), |b| {
			b.iter(|| {
				runtime.block_on(async {
					let request = Request::builder().method("POST").uri(uri).header("content-type", "application/json").body(Body::from(body.clone())).expect("request");
					let response = router.clone().oneshot(request).await.expect("the router answers");
					assert_eq!(response.status(), StatusCode::OK);
					black_box(axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body").len())
				})
			});
		});
	}
	group.finish();
}

criterion_group!(benches, engine_shape, e2e);
criterion_main!(benches);
