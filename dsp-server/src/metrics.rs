//! In-process server metrics and the Prometheus exposition endpoint
//! (`GET /metrics`).
//!
//! This is the first instrumentation slice (roadmap Phase 3 / priority #4,
//! Prometheus). Before optimizing anything we need bottlenecks and load to be
//! visible; this layer makes the server's own work countable. It is deliberately
//! dependency-free: the Prometheus text exposition format (v0.0.4) is simple
//! enough to render by hand, so no client library is pulled into the core HTTP
//! surface.
//!
//! Counters are process-lifetime monotonic and updated with `Relaxed` ordering —
//! exact cross-counter consistency is not required for a scrape, and the relaxed
//! path keeps the request hot path cheap.

use std::{
	sync::{
		atomic::{AtomicU64, Ordering}, Arc
	}, time::Duration
};

use axum::{extract::State, http::header, response::IntoResponse, Json};
use serde::Serialize;

/// Shared, cheaply-cloneable handle to the server's metrics.
pub type SharedMetrics = Arc<Metrics>;

/// Upper bounds (inclusive, in seconds) of the request-latency histogram
/// buckets, spanning sub-millisecond to multi-second. These are the `le`
/// boundaries a Prometheus/Grafana rule reads to compute the north-star p95/p99
/// interpolated-latency target (`histogram_quantile(0.95, …)`). Kept ascending;
/// an observation past the last bound falls into the implicit `+Inf` bucket.
const LATENCY_BUCKETS_SECS: [f64; 12] = [0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5];

/// Process-lifetime server counters, grouped per endpoint so each new endpoint
/// gets its own sub-metrics without crowding a single flat namespace.
#[derive(Debug, Default)]
pub struct Metrics {
	/// Counters for `POST /api/v1/interpolate`.
	pub interpolate: InterpolateMetrics,
	/// Counters for `POST /api/v1/downsample`.
	pub downsample: DownsampleMetrics,
	/// Counters for the storage-ingest endpoints (`…/points`, `…/ilp`).
	pub ingest: IngestMetrics,
	/// End-to-end latency for the interpolate endpoints (JSON + ILP).
	pub interpolate_latency: LatencyHistogram,
	/// End-to-end latency for the downsample endpoints (JSON + ILP).
	pub downsample_latency: LatencyHistogram,
}

/// A cumulative, fixed-bucket latency histogram rendered in the Prometheus
/// histogram exposition format (`_bucket{le=…}` / `_sum` / `_count`).
///
/// Buckets are stored non-cumulatively (each observation lands in exactly one
/// bucket) and accumulated at render time, the way a Prometheus client library
/// exposes them. The running sum is kept in integer microseconds so the whole
/// path stays lock-free on `AtomicU64` (no float CAS loop) and dependency-free;
/// it is rendered back to fractional seconds, the Prometheus convention.
#[derive(Debug)]
pub struct LatencyHistogram {
	/// Per-bucket observation counts aligned with [`LATENCY_BUCKETS_SECS`].
	buckets: [AtomicU64; LATENCY_BUCKETS_SECS.len()],
	/// Observations exceeding the largest finite bound (the `+Inf` bucket).
	overflow: AtomicU64,
	/// Sum of all observed durations, in microseconds.
	sum_micros: AtomicU64,
	/// Total number of observations (equals the `+Inf` cumulative bucket).
	count: AtomicU64,
}

impl Default for LatencyHistogram {
	fn default() -> Self {
		Self { buckets: std::array::from_fn(|_| AtomicU64::new(0)), overflow: AtomicU64::new(0), sum_micros: AtomicU64::new(0), count: AtomicU64::new(0) }
	}
}

impl LatencyHistogram {
	/// Record one observed request duration into the matching bucket, the sum,
	/// and the count. Lock-free; safe to call from concurrent request handlers.
	pub fn observe(&self, elapsed: Duration) {
		let secs = elapsed.as_secs_f64();
		// Saturate the microsecond sum rather than wrap — a run long enough to
		// overflow `u64` micros (~584 000 years) is not a real concern, but the
		// saturating form keeps the counter monotonic under any pathological input.
		let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
		match LATENCY_BUCKETS_SECS.iter().position(|&bound| secs <= bound) {
			Some(i) => {
				self.buckets[i].fetch_add(1, Ordering::Relaxed);
			},
			None => {
				self.overflow.fetch_add(1, Ordering::Relaxed);
			},
		}
		self.sum_micros.fetch_add(micros, Ordering::Relaxed);
		self.count.fetch_add(1, Ordering::Relaxed);
	}

	/// Take a point-in-time read of the histogram.
	#[must_use]
	pub fn snapshot(&self) -> LatencyHistogramSnapshot {
		let mut buckets = [0_u64; LATENCY_BUCKETS_SECS.len()];
		for (dst, src) in buckets.iter_mut().zip(self.buckets.iter()) {
			*dst = src.load(Ordering::Relaxed);
		}
		LatencyHistogramSnapshot { buckets, overflow: self.overflow.load(Ordering::Relaxed), sum_micros: self.sum_micros.load(Ordering::Relaxed), count: self.count.load(Ordering::Relaxed) }
	}

	/// Append this histogram's Prometheus exposition (`_bucket`/`_sum`/`_count`)
	/// under `name`, accumulating the stored per-bucket counts into the
	/// cumulative `le` series Prometheus expects.
	fn render_prometheus(&self, out: &mut String, name: &str, help: &str) {
		use std::fmt::Write as _;
		let snap = self.snapshot();
		let _ = writeln!(out, "# HELP {name} {help}");
		let _ = writeln!(out, "# TYPE {name} histogram");
		let mut cumulative = 0_u64;
		for (bound, count) in LATENCY_BUCKETS_SECS.iter().zip(snap.buckets.iter()) {
			cumulative += *count;
			let _ = writeln!(out, "{name}_bucket{{le=\"{bound}\"}} {cumulative}");
		}
		// The `+Inf` bucket is the total observation count by definition.
		let _ = writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {}", snap.count);
		let _ = writeln!(out, "{name}_sum {:.6}", snap.sum_seconds());
		let _ = writeln!(out, "{name}_count {}", snap.count);
	}
}

/// A point-in-time read of a [`LatencyHistogram`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencyHistogramSnapshot {
	/// Non-cumulative per-bucket counts aligned with [`LATENCY_BUCKETS_SECS`].
	pub buckets: [u64; LATENCY_BUCKETS_SECS.len()],
	/// Observations past the largest finite bound (the `+Inf` overflow).
	pub overflow: u64,
	/// Sum of all observed durations, in microseconds.
	pub sum_micros: u64,
	/// Total number of observations.
	pub count: u64,
}

impl LatencyHistogramSnapshot {
	/// The summed observed latency in fractional seconds (Prometheus `_sum`).
	#[must_use]
	#[allow(clippy::cast_precision_loss)] // exact for any realistic total: f64 holds integers < 2^53.
	pub fn sum_seconds(&self) -> f64 {
		self.sum_micros as f64 / 1_000_000.0
	}

	/// Estimate the `q`-quantile (`0.0..=1.0`) latency in seconds from the
	/// bucket counts, using Prometheus `histogram_quantile` semantics: locate the
	/// bucket the rank falls in and linearly interpolate between its lower and
	/// upper bounds. Returns `None` when there are no observations; a rank landing
	/// in the open-ended `+Inf` bucket is clamped to the largest finite bound
	/// (the histogram cannot resolve a value beyond its last edge).
	#[must_use]
	#[allow(clippy::cast_precision_loss)] // counts < 2^53 in any realistic run; f64 is exact there.
	pub fn quantile(&self, q: f64) -> Option<f64> {
		if self.count == 0 {
			return None;
		}
		let rank = q.clamp(0.0, 1.0) * self.count as f64;
		let mut cum_prev = 0_u64;
		let mut lower = 0.0_f64;
		for (&bound, &in_bucket) in LATENCY_BUCKETS_SECS.iter().zip(self.buckets.iter()) {
			let cum = cum_prev + in_bucket;
			if cum as f64 >= rank {
				if in_bucket == 0 {
					return Some(bound);
				}
				let frac = (rank - cum_prev as f64) / in_bucket as f64;
				return Some((bound - lower).mul_add(frac, lower));
			}
			cum_prev = cum;
			lower = bound;
		}
		// Rank falls in the `+Inf` bucket — clamp to the largest finite edge.
		LATENCY_BUCKETS_SECS.last().copied()
	}
}

/// Counters for the interpolation endpoint.
#[derive(Debug, Default)]
pub struct InterpolateMetrics {
	requests: AtomicU64,
	errors: AtomicU64,
	output_points: AtomicU64,
}

/// Counters for the downsample endpoint.
#[derive(Debug, Default)]
pub struct DownsampleMetrics {
	requests: AtomicU64,
	errors: AtomicU64,
	output_buckets: AtomicU64,
}

/// Counters for the storage-ingest endpoints (batch JSON + ILP seal).
#[derive(Debug, Default)]
pub struct IngestMetrics {
	requests: AtomicU64,
	errors: AtomicU64,
	rows_sealed: AtomicU64,
	segments_sealed: AtomicU64,
}

/// A point-in-time read of [`Metrics`], convenient for assertions and rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricsSnapshot {
	/// Interpolation-endpoint counters.
	pub interpolate: InterpolateSnapshot,
	/// Downsample-endpoint counters.
	pub downsample: DownsampleSnapshot,
	/// Storage-ingest counters.
	pub ingest: IngestSnapshot,
}

/// A point-in-time read of [`InterpolateMetrics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterpolateSnapshot {
	/// Total interpolation requests received (including failures).
	pub requests: u64,
	/// Interpolation requests that returned an error.
	pub errors: u64,
	/// Total interpolated output points served across all successful requests.
	pub output_points: u64,
}

/// A point-in-time read of [`DownsampleMetrics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownsampleSnapshot {
	/// Total downsample requests received (including failures).
	pub requests: u64,
	/// Downsample requests that returned an error.
	pub errors: u64,
	/// Total non-empty buckets served across all successful requests.
	pub output_buckets: u64,
}

/// A point-in-time read of [`IngestMetrics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct IngestSnapshot {
	/// Total storage-ingest requests received (including failures).
	pub requests: u64,
	/// Storage-ingest requests that returned an error.
	pub errors: u64,
	/// Total rows sealed across all successful ingests (present and null).
	pub rows_sealed: u64,
	/// Total segments sealed across all successful ingests.
	pub segments_sealed: u64,
}

impl Metrics {
	/// Count an interpolation request (record on entry, before validation).
	pub fn record_interpolate_request(&self) {
		self.interpolate.requests.fetch_add(1, Ordering::Relaxed);
	}

	/// Count an interpolation request that failed.
	pub fn record_interpolate_error(&self) {
		self.interpolate.errors.fetch_add(1, Ordering::Relaxed);
	}

	/// Add to the running total of interpolated output points served.
	pub fn add_output_points(&self, count: u64) {
		self.interpolate.output_points.fetch_add(count, Ordering::Relaxed);
	}

	/// Count a downsample request (record on entry, before validation).
	pub fn record_downsample_request(&self) {
		self.downsample.requests.fetch_add(1, Ordering::Relaxed);
	}

	/// Count a downsample request that failed.
	pub fn record_downsample_error(&self) {
		self.downsample.errors.fetch_add(1, Ordering::Relaxed);
	}

	/// Add to the running total of downsample buckets served.
	pub fn add_downsample_buckets(&self, count: u64) {
		self.downsample.output_buckets.fetch_add(count, Ordering::Relaxed);
	}

	/// Count a storage-ingest request (record on entry, before validation).
	pub fn record_ingest_request(&self) {
		self.ingest.requests.fetch_add(1, Ordering::Relaxed);
	}

	/// Count a storage-ingest request that failed.
	pub fn record_ingest_error(&self) {
		self.ingest.errors.fetch_add(1, Ordering::Relaxed);
	}

	/// Record a successful seal: add its row count and count the one segment.
	pub fn record_ingest_seal(&self, rows: u64) {
		self.ingest.rows_sealed.fetch_add(rows, Ordering::Relaxed);
		self.ingest.segments_sealed.fetch_add(1, Ordering::Relaxed);
	}

	/// Record the end-to-end handling latency of one interpolation request
	/// (success or error alike — latency of failures is part of the SLO).
	pub fn observe_interpolate_latency(&self, elapsed: Duration) {
		self.interpolate_latency.observe(elapsed);
	}

	/// Record the end-to-end handling latency of one downsample request.
	pub fn observe_downsample_latency(&self, elapsed: Duration) {
		self.downsample_latency.observe(elapsed);
	}

	/// Take a consistent-enough snapshot of all counters.
	#[must_use]
	pub fn snapshot(&self) -> MetricsSnapshot {
		MetricsSnapshot { interpolate: InterpolateSnapshot { requests: self.interpolate.requests.load(Ordering::Relaxed), errors: self.interpolate.errors.load(Ordering::Relaxed), output_points: self.interpolate.output_points.load(Ordering::Relaxed) }, downsample: DownsampleSnapshot { requests: self.downsample.requests.load(Ordering::Relaxed), errors: self.downsample.errors.load(Ordering::Relaxed), output_buckets: self.downsample.output_buckets.load(Ordering::Relaxed) }, ingest: IngestSnapshot { requests: self.ingest.requests.load(Ordering::Relaxed), errors: self.ingest.errors.load(Ordering::Relaxed), rows_sealed: self.ingest.rows_sealed.load(Ordering::Relaxed), segments_sealed: self.ingest.segments_sealed.load(Ordering::Relaxed) } }
	}

	/// Render the counters in the Prometheus text exposition format (v0.0.4).
	#[must_use]
	pub fn render_prometheus(&self) -> String {
		use std::fmt::Write as _;
		let snap = self.snapshot();
		let mut out = String::with_capacity(512);
		let counters = [("dsp_interpolate_requests_total", "Total interpolation requests received.", snap.interpolate.requests), ("dsp_interpolate_errors_total", "Interpolation requests that returned an error.", snap.interpolate.errors), ("dsp_interpolate_output_points_total", "Total interpolated output points served.", snap.interpolate.output_points), ("dsp_downsample_requests_total", "Total downsample requests received.", snap.downsample.requests), ("dsp_downsample_errors_total", "Downsample requests that returned an error.", snap.downsample.errors), ("dsp_downsample_output_buckets_total", "Total downsample buckets served.", snap.downsample.output_buckets), ("dsp_ingest_requests_total", "Total storage-ingest requests received.", snap.ingest.requests), ("dsp_ingest_errors_total", "Storage-ingest requests that returned an error.", snap.ingest.errors), ("dsp_ingest_rows_sealed_total", "Total rows sealed across all storage ingests.", snap.ingest.rows_sealed), ("dsp_ingest_segments_sealed_total", "Total segments sealed across all storage ingests.", snap.ingest.segments_sealed)];
		for (name, help, value) in counters {
			// `writeln!` into a String is infallible.
			let _ = writeln!(out, "# HELP {name} {help}");
			let _ = writeln!(out, "# TYPE {name} counter");
			let _ = writeln!(out, "{name} {value}");
		}
		// Latency histograms — the p95/p99 surface for the north-star target.
		self.interpolate_latency.render_prometheus(&mut out, "dsp_interpolate_duration_seconds", "End-to-end handling latency for the interpolate endpoints (POST /api/v1/interpolate and /interpolate/ilp).");
		self.downsample_latency.render_prometheus(&mut out, "dsp_downsample_duration_seconds", "End-to-end handling latency for the downsample endpoints (POST /api/v1/downsample and /downsample/ilp).");
		out
	}
}

/// Handle `GET /metrics`: render the server counters for a Prometheus scrape.
pub async fn metrics(State(state): State<SharedMetrics>) -> impl IntoResponse {
	([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], state.render_prometheus())
}

/// The estimated latency quantiles for one endpoint, in seconds.
///
/// The machine-readable form of the north-star p95/p99 target a benchmark
/// harness reads without standing up a Prometheus server. Quantiles are `None`
/// before the first request.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct LatencyProfile {
	/// Total observed requests (the histogram's `_count`).
	pub count: u64,
	/// Summed observed latency in seconds (the histogram's `_sum`).
	pub sum_seconds: f64,
	/// Estimated median (p50) latency in seconds.
	pub p50_seconds: Option<f64>,
	/// Estimated p95 latency in seconds — the governing SLO term.
	pub p95_seconds: Option<f64>,
	/// Estimated p99 latency in seconds.
	pub p99_seconds: Option<f64>,
}

impl LatencyProfile {
	/// Derive the profile (count, sum, p50/p95/p99) from a histogram snapshot.
	#[must_use]
	pub fn from_snapshot(s: &LatencyHistogramSnapshot) -> Self {
		Self { count: s.count, sum_seconds: s.sum_seconds(), p50_seconds: s.quantile(0.50), p95_seconds: s.quantile(0.95), p99_seconds: s.quantile(0.99) }
	}
}

/// Per-endpoint counters plus latency quantiles for the interpolation endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct InterpolateProfile {
	/// Total interpolation requests (including failures).
	pub requests: u64,
	/// Interpolation requests that returned an error.
	pub errors: u64,
	/// Total interpolated output points served.
	pub output_points: u64,
	/// End-to-end latency quantiles for `POST /api/v1/interpolate`.
	pub latency_seconds: LatencyProfile,
}

/// Per-endpoint counters plus latency quantiles for the downsample endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct DownsampleProfile {
	/// Total downsample requests (including failures).
	pub requests: u64,
	/// Downsample requests that returned an error.
	pub errors: u64,
	/// Total non-empty buckets served.
	pub output_buckets: u64,
	/// End-to-end latency quantiles for `POST /api/v1/downsample`.
	pub latency_seconds: LatencyProfile,
}

/// The whole-server profile snapshot served at `GET /debug/profile/current`.
///
/// Roadmap Phase 3: a benchmark harness reads the live p95 target straight out
/// of this JSON — no Prometheus/Grafana rule required.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ProfileReport {
	/// Logical service name.
	pub service: &'static str,
	/// Running build version.
	pub version: &'static str,
	/// Interpolation-endpoint counters + latency quantiles.
	pub interpolate: InterpolateProfile,
	/// Downsample-endpoint counters + latency quantiles.
	pub downsample: DownsampleProfile,
	/// Storage-ingest counters (no latency histogram yet).
	pub ingest: IngestSnapshot,
}

impl Metrics {
	/// Build the whole-server [`ProfileReport`]: the counter snapshot plus the
	/// estimated p50/p95/p99 latency for each instrumented compute endpoint.
	#[must_use]
	pub fn profile(&self) -> ProfileReport {
		let snap = self.snapshot();
		ProfileReport { service: crate::SERVICE, version: crate::VERSION, interpolate: InterpolateProfile { requests: snap.interpolate.requests, errors: snap.interpolate.errors, output_points: snap.interpolate.output_points, latency_seconds: LatencyProfile::from_snapshot(&self.interpolate_latency.snapshot()) }, downsample: DownsampleProfile { requests: snap.downsample.requests, errors: snap.downsample.errors, output_buckets: snap.downsample.output_buckets, latency_seconds: LatencyProfile::from_snapshot(&self.downsample_latency.snapshot()) }, ingest: snap.ingest }
	}
}

/// Handle `GET /debug/profile/current`: the live counter + latency-quantile
/// snapshot as JSON, so a benchmark run can read the p95 target directly.
pub async fn profile_current(State(state): State<SharedMetrics>) -> Json<ProfileReport> {
	Json(state.profile())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn counters_accumulate() {
		let m = Metrics::default();
		m.record_interpolate_request();
		m.record_interpolate_request();
		m.record_interpolate_error();
		m.add_output_points(61);
		m.add_output_points(9);
		let snap = m.snapshot();
		assert_eq!(snap.interpolate.requests, 2);
		assert_eq!(snap.interpolate.errors, 1);
		assert_eq!(snap.interpolate.output_points, 70);
	}

	#[test]
	fn prometheus_text_is_well_formed() {
		let m = Metrics::default();
		m.record_interpolate_request();
		m.add_output_points(5);
		let text = m.render_prometheus();
		// Every counter carries HELP, TYPE, and a value line.
		assert!(text.contains("# HELP dsp_interpolate_requests_total"));
		assert!(text.contains("# TYPE dsp_interpolate_requests_total counter"));
		assert!(text.contains("dsp_interpolate_requests_total 1"));
		assert!(text.contains("dsp_interpolate_output_points_total 5"));
		// No value line is left dangling without a preceding TYPE line: 10
		// counters + the 2 latency histograms.
		assert_eq!(text.matches("# TYPE ").count(), 12);
		// The downsample counters are exposed too.
		assert!(text.contains("# TYPE dsp_downsample_requests_total counter"));
		// And the storage-ingest counters.
		assert!(text.contains("# TYPE dsp_ingest_rows_sealed_total counter"));
	}

	#[test]
	fn latency_histogram_buckets_and_sum() {
		let h = LatencyHistogram::default();
		// 0.3 ms → first bucket (le=0.0005); 3 ms → le=0.005; 30 ms → le=0.05.
		h.observe(Duration::from_micros(300));
		h.observe(Duration::from_micros(3_000));
		h.observe(Duration::from_micros(30_000));
		// 10 s exceeds the largest finite bound (2.5) → +Inf overflow.
		h.observe(Duration::from_secs(10));
		let snap = h.snapshot();
		assert_eq!(snap.count, 4);
		assert_eq!(snap.overflow, 1);
		// Bucket 0 is le=0.0005, bucket 3 is le=0.005, bucket 6 is le=0.05.
		assert_eq!(snap.buckets[0], 1);
		assert_eq!(snap.buckets[3], 1);
		assert_eq!(snap.buckets[6], 1);
		// Sum = 0.3ms + 3ms + 30ms + 10s = 10.0333 s.
		assert!((snap.sum_seconds() - 10.033_3).abs() < 1e-4, "sum was {}", snap.sum_seconds());
	}

	#[test]
	fn latency_histogram_prometheus_is_cumulative() {
		let m = Metrics::default();
		m.observe_interpolate_latency(Duration::from_micros(300)); // le=0.0005
		m.observe_interpolate_latency(Duration::from_micros(3_000)); // le=0.005
		let text = m.render_prometheus();
		assert!(text.contains("# TYPE dsp_interpolate_duration_seconds histogram"));
		// Cumulative: the le=0.0005 bucket holds 1, and by le=0.005 both fall in.
		assert!(text.contains("dsp_interpolate_duration_seconds_bucket{le=\"0.0005\"} 1"), "{text}");
		assert!(text.contains("dsp_interpolate_duration_seconds_bucket{le=\"0.005\"} 2"), "{text}");
		assert!(text.contains("dsp_interpolate_duration_seconds_bucket{le=\"+Inf\"} 2"), "{text}");
		assert!(text.contains("dsp_interpolate_duration_seconds_count 2"), "{text}");
		// The downsample histogram is exposed even with no observations.
		assert!(text.contains("# TYPE dsp_downsample_duration_seconds histogram"));
		assert!(text.contains("dsp_downsample_duration_seconds_count 0"));
	}

	#[test]
	fn latency_quantile_interpolates_within_the_bucket() {
		let h = LatencyHistogram::default();
		// No observations → no quantile.
		assert_eq!(h.snapshot().quantile(0.95), None);
		// 100 observations all at ~3 ms → land in the le=0.005 bucket (index 3,
		// lower edge 0.0025). Every quantile interpolates inside [0.0025, 0.005].
		for _ in 0..100 {
			h.observe(Duration::from_micros(3_000));
		}
		let snap = h.snapshot();
		let p95 = snap.quantile(0.95).unwrap();
		assert!((0.0025..=0.005).contains(&p95), "p95 was {p95}");
		let p50 = snap.quantile(0.50).unwrap();
		assert!((0.0025..=0.005).contains(&p50), "p50 was {p50}");
	}

	#[test]
	fn latency_quantile_clamps_to_the_last_finite_bound_on_overflow() {
		let h = LatencyHistogram::default();
		// A 10 s observation exceeds the largest finite bound (2.5) → +Inf; the
		// quantile can only be resolved to that last finite edge.
		h.observe(Duration::from_secs(10));
		assert_eq!(h.snapshot().quantile(0.99), Some(2.5));
	}

	#[test]
	fn profile_reports_counts_and_quantiles() {
		let m = Metrics::default();
		m.record_interpolate_request();
		m.add_output_points(21);
		m.observe_interpolate_latency(Duration::from_micros(3_000));
		m.record_downsample_request();
		m.add_downsample_buckets(2);
		m.observe_downsample_latency(Duration::from_micros(70));
		m.record_ingest_seal(5);
		let p = m.profile();
		assert_eq!(p.service, crate::SERVICE);
		assert_eq!(p.interpolate.requests, 1);
		assert_eq!(p.interpolate.output_points, 21);
		assert_eq!(p.interpolate.latency_seconds.count, 1);
		assert!(p.interpolate.latency_seconds.p95_seconds.is_some());
		assert_eq!(p.downsample.output_buckets, 2);
		assert_eq!(p.ingest.rows_sealed, 5);
		assert_eq!(p.ingest.segments_sealed, 1);
		// Serializes to an object carrying the nested latency block.
		let json = serde_json::to_value(p).unwrap();
		assert!(json["interpolate"]["latency_seconds"]["p95_seconds"].is_number());
		assert_eq!(json["service"], crate::SERVICE);
	}

	#[test]
	fn ingest_counters_accumulate() {
		let m = Metrics::default();
		m.record_ingest_request();
		m.record_ingest_request();
		m.record_ingest_error();
		m.record_ingest_seal(5);
		m.record_ingest_seal(3);
		let snap = m.snapshot();
		assert_eq!(snap.ingest.requests, 2);
		assert_eq!(snap.ingest.errors, 1);
		assert_eq!(snap.ingest.rows_sealed, 8);
		assert_eq!(snap.ingest.segments_sealed, 2);
	}
}
