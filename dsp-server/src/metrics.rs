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

use std::sync::{
	atomic::{AtomicU64, Ordering}, Arc
};

use axum::{extract::State, http::header, response::IntoResponse};

/// Shared, cheaply-cloneable handle to the server's metrics.
pub type SharedMetrics = Arc<Metrics>;

/// Process-lifetime server counters, grouped per endpoint so each new endpoint
/// gets its own sub-metrics without crowding a single flat namespace.
#[derive(Debug, Default)]
pub struct Metrics {
	/// Counters for `POST /api/v1/interpolate`.
	pub interpolate: InterpolateMetrics,
	/// Counters for `POST /api/v1/downsample`.
	pub downsample: DownsampleMetrics,
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

/// A point-in-time read of [`Metrics`], convenient for assertions and rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricsSnapshot {
	/// Interpolation-endpoint counters.
	pub interpolate: InterpolateSnapshot,
	/// Downsample-endpoint counters.
	pub downsample: DownsampleSnapshot,
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

	/// Take a consistent-enough snapshot of all counters.
	#[must_use]
	pub fn snapshot(&self) -> MetricsSnapshot {
		MetricsSnapshot { interpolate: InterpolateSnapshot { requests: self.interpolate.requests.load(Ordering::Relaxed), errors: self.interpolate.errors.load(Ordering::Relaxed), output_points: self.interpolate.output_points.load(Ordering::Relaxed) }, downsample: DownsampleSnapshot { requests: self.downsample.requests.load(Ordering::Relaxed), errors: self.downsample.errors.load(Ordering::Relaxed), output_buckets: self.downsample.output_buckets.load(Ordering::Relaxed) } }
	}

	/// Render the counters in the Prometheus text exposition format (v0.0.4).
	#[must_use]
	pub fn render_prometheus(&self) -> String {
		use std::fmt::Write as _;
		let snap = self.snapshot();
		let mut out = String::with_capacity(512);
		let counters = [("dsp_interpolate_requests_total", "Total interpolation requests received.", snap.interpolate.requests), ("dsp_interpolate_errors_total", "Interpolation requests that returned an error.", snap.interpolate.errors), ("dsp_interpolate_output_points_total", "Total interpolated output points served.", snap.interpolate.output_points), ("dsp_downsample_requests_total", "Total downsample requests received.", snap.downsample.requests), ("dsp_downsample_errors_total", "Downsample requests that returned an error.", snap.downsample.errors), ("dsp_downsample_output_buckets_total", "Total downsample buckets served.", snap.downsample.output_buckets)];
		for (name, help, value) in counters {
			// `writeln!` into a String is infallible.
			let _ = writeln!(out, "# HELP {name} {help}");
			let _ = writeln!(out, "# TYPE {name} counter");
			let _ = writeln!(out, "{name} {value}");
		}
		out
	}
}

/// Handle `GET /metrics`: render the server counters for a Prometheus scrape.
pub async fn metrics(State(state): State<SharedMetrics>) -> impl IntoResponse {
	([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], state.render_prometheus())
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
		// No value line is left dangling without a preceding TYPE line.
		assert_eq!(text.matches("# TYPE ").count(), 6);
		// The downsample counters are exposed too.
		assert!(text.contains("# TYPE dsp_downsample_requests_total counter"));
	}
}
