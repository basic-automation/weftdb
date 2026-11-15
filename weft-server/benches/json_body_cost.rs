//! Request-level benchmark: what `float_roundtrip` costs a whole JSON compute request.
//!
//! Drives one `POST /api/v1/interpolate` with a body of N float values through the
//! real router (no store, stateless compute path), and reports p50/p95 latency plus
//! nanoseconds per value for the whole request.
//!
//! The magnitude mix matches the Phase 11 parse-only measurement (magnitudes
//! 1e-19..1e15, ryu-formatted), so the two are comparable.
//!
//! Knobs (environment):
//!
//! - `WEFT_JBC_VALUES` — number of float values in the request body (default 100 000)
//! - `WEFT_JBC_REPS` — timed repetitions after one untimed warm-up (default 20)
//!
//! Run: `cargo bench -p weft-server --bench json_body_cost`.

use std::{
	hint::black_box,
	time::{Duration, Instant},
};

use axum::{body::Body, http::Request};
use chrono::{TimeZone, Utc};
use tower::ServiceExt as _;

const DEFAULT_VALUES: usize = 100_000;

fn env_parse<T: std::str::FromStr>(name: &str, default: T) -> T {
	std::env::var(name).ok().and_then(|raw| raw.trim().parse().ok()).unwrap_or(default)
}

fn ms(d: Duration) -> f64 {
	d.as_secs_f64() * 1e3
}

fn ns_per(d: Duration, n: usize) -> f64 {
	d.as_nanos() as f64 / n as f64
}

fn quantile(samples: &[Duration], q: f64) -> Duration {
	#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
	let rank = ((q * samples.len() as f64).ceil() as usize).clamp(1, samples.len());
	samples[rank - 1]
}

/// xorshift64, matching the Phase 11 measurement's generator.
fn xorshift(state: &mut u64) -> u64 {
	*state ^= *state << 13;
	*state ^= *state >> 7;
	*state ^= *state << 17;
	*state
}

/// u64 → f64 conversion matching `num::u64_to_f64` (the exact formula, inlined
/// because `mod num` is private to the library crate).
fn u64_to_f64(n: u64) -> f64 {
	const TWO_POW_32: f64 = 4_294_967_296.0;
	let high = u32::try_from(n >> 32).unwrap_or(u32::MAX);
	let low = u32::try_from(n & u64::from(u32::MAX)).unwrap_or(u32::MAX);
	f64::from(high).mul_add(TWO_POW_32, f64::from(low))
}

/// Generate a JSON interpolate-request body with `n` float values in the same
/// magnitude mix as Phase 11 (1e-19..1e15, ryu-formatted).
fn generate_body(n: usize) -> Vec<u8> {
	let mut state = 0x9E37_79B9_7F4A_7C15_u64;
	let anchor = Utc.timestamp_opt(1_577_836_800, 0).single().expect("valid anchor");

	let points: Vec<serde_json::Value> = (0..n).map(|i| {
		let rnd = xorshift(&mut state);
		let unit = (u64_to_f64(rnd >> 11) / u64_to_f64(1_u64 << 53)).mul_add(2.0, -1.0);
		#[allow(clippy::cast_possible_truncation)]
		let exponent = i32::try_from(xorshift(&mut state) % 35).expect("below 35") - 19;
		let value = unit * 10_f64.powi(exponent);
		let ts = anchor + chrono::Duration::seconds(i as i64);
		serde_json::json!({ "timestamp": ts.to_rfc3339(), "value": value })
	}).collect();

	let end_ts = anchor + chrono::Duration::seconds(n as i64 - 1);
	serde_json::to_vec(&serde_json::json!({
		"spline": "cubic",
		"resolution": "seconds",
		"start": anchor.to_rfc3339(),
		"end": end_ts.to_rfc3339(),
		"points": points,
	})).expect("serialises the request body")
}

fn run_one(runtime: &tokio::runtime::Runtime, router: &axum::Router, body: Vec<u8>) {
	let _ = runtime.block_on(async {
		let request = Request::builder()
			.method("POST")
			.uri("/api/v1/interpolate")
			.header("content-type", "application/json")
			.body(Body::from(body))
			.expect("request");
		let response = router.clone().oneshot(request).await.expect("the router answers");
		assert!(response.status().is_success(), "status {}", response.status());
		black_box(axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body").len())
	});
}

fn main() {
	let values: usize = env_parse("WEFT_JBC_VALUES", DEFAULT_VALUES);
	let reps: usize = env_parse::<usize>("WEFT_JBC_REPS", 20).max(1);

	let body = generate_body(values);
	println!("json_body_cost: generating {values} values, {reps} reps, body {} bytes", body.len());

	let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
	let router = weft_server::app();

	// Untimed warm-up.
	run_one(&runtime, &router, body.clone());

	// Timed repetitions.
	let mut samples = Vec::with_capacity(reps);
	for _ in 0..reps {
		let began = Instant::now();
		run_one(&runtime, &router, body.clone());
		samples.push(began.elapsed());
	}
	samples.sort_unstable();

	let p50 = quantile(&samples, 0.5);
	let p95 = quantile(&samples, 0.95);
	let per_val = ns_per(p50, values);

	println!("json_body_cost: values={values} reps={reps}");
	println!("p50={:>9.3}ms  p95={:>9.3}ms  ns/value={:.1}", ms(p50), ms(p95), per_val);
}