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
	/// Counters for the out-of-order reconciliation pass (`…/reconcile`).
	pub reconcile: ReconcileMetrics,
	/// Counters for the online control-plane backup (`…/storage/backup`).
	pub backup: BackupMetrics,
	/// End-to-end latency for the interpolate endpoints (JSON + ILP).
	pub interpolate_latency: LatencyHistogram,
	/// End-to-end latency for the downsample endpoints (JSON + ILP).
	pub downsample_latency: LatencyHistogram,
	/// Seal-path latency for the storage-ingest endpoints (JSON / CSV / ILP) —
	/// the ingest side of the "predictable p95/p99 under ingest + query" claim.
	pub ingest_latency: LatencyHistogram,
	/// Duration of each completed, verified control-plane backup snapshot
	/// (EVI-25): the manual endpoint and the daemon tick both observe here, so
	/// the backup-duration half of the RTO inputs is a scrapeable histogram
	/// rather than a log line or a one-off bench number.
	pub backup_latency: LatencyHistogram,
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
	/// Per-bucket observation counts aligned with `LATENCY_BUCKETS_SECS`.
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
			}
			None => {
				self.overflow.fetch_add(1, Ordering::Relaxed);
			}
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
	/// Non-cumulative per-bucket counts aligned with `LATENCY_BUCKETS_SECS`.
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
	pub fn sum_seconds(&self) -> f64 {
		crate::num::u64_to_f64(self.sum_micros) / 1_000_000.0
	}

	/// Estimate the `q`-quantile (`0.0..=1.0`) latency in seconds from the
	/// bucket counts, using Prometheus `histogram_quantile` semantics: locate the
	/// bucket the rank falls in and linearly interpolate between its lower and
	/// upper bounds. Returns `None` when there are no observations; a rank landing
	/// in the open-ended `+Inf` bucket is clamped to the largest finite bound
	/// (the histogram cannot resolve a value beyond its last edge).
	#[must_use]
	pub fn quantile(&self, q: f64) -> Option<f64> {
		if self.count == 0 {
			return None;
		}
		let rank = q.clamp(0.0, 1.0) * crate::num::u64_to_f64(self.count);
		let mut cum_prev = 0_u64;
		let mut lower = 0.0_f64;
		for (&bound, &in_bucket) in LATENCY_BUCKETS_SECS.iter().zip(self.buckets.iter()) {
			let cum = cum_prev + in_bucket;
			if crate::num::u64_to_f64(cum) >= rank {
				if in_bucket == 0 {
					return Some(bound);
				}
				let frac = (rank - crate::num::u64_to_f64(cum_prev)) / crate::num::u64_to_f64(in_bucket);
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
///
/// Beyond requests / errors / output points, the provenance counters ROB-16
/// adds: output points split by the [`ValueKind`](weftdb::ValueKind) they were
/// served with, requests split by the compute backend that actually ran, and
/// the count of `auto` requests that tried the GPU and fell back — the three
/// numbers an operator needs to see what the GPU configuration is doing.
#[derive(Debug, Default)]
pub struct InterpolateMetrics {
	requests: AtomicU64,
	errors: AtomicU64,
	output_points: AtomicU64,
	output_raw_points: AtomicU64,
	output_interpolated_points: AtomicU64,
	output_extrapolated_points: AtomicU64,
	backend_cpu_requests: AtomicU64,
	backend_parallel_requests: AtomicU64,
	backend_gpu_requests: AtomicU64,
	gpu_fallbacks: AtomicU64,
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

/// Counters for the out-of-order reconciliation pass (`POST …/{aspect}/reconcile`).
///
/// The observability side of the Phase-4.6 threshold-triggered reconcile: a
/// background trigger keyed on `unsorted_segments` needs its firing rate and the
/// write amplification it drives to be countable, so an operator can tune the
/// threshold against how often a pass actually runs and how much it rewrites.
#[derive(Debug, Default)]
pub struct ReconcileMetrics {
	passes: AtomicU64,
	segments_reconciled: AtomicU64,
	failed_passes: AtomicU64,
}

/// Counters for the online control-plane backup (`POST …/storage/backup`, roadmap
/// Phase 7.4).
///
/// An operator needs to see how many snapshots ran and the total bytes
/// written to reason about backup cadence and storage growth — and, since EVI-18,
/// whether the scheduled daemon is healthy: how many of its ticks failed, and when
/// the last verified snapshot landed (`last_success_unix_secs`, 0 when none has
/// been recorded yet, so a scrape can alert on a stale backup).
#[derive(Debug, Default)]
pub struct BackupMetrics {
	snapshots: AtomicU64,
	bytes_written: AtomicU64,
	/// Daemon ticks that failed to snapshot (a failed tick retries on the next
	/// tick; this counter is how a permanently failing daemon stays visible).
	failures: AtomicU64,
	/// Unix seconds of the most recent verified snapshot, daemon tick or manual
	/// endpoint alike. `0` means no snapshot has been recorded by this process
	/// (the daemon seeds it from the newest on-disk snapshot at start).
	last_success_unix_secs: AtomicU64,
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
	/// Reconciliation-pass counters.
	pub reconcile: ReconcileSnapshot,
	/// Control-plane backup counters.
	pub backup: BackupSnapshot,
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
	/// Output points served with provenance `raw`.
	pub output_raw_points: u64,
	/// Output points served with provenance `interpolated`.
	pub output_interpolated_points: u64,
	/// Output points served with provenance `extrapolated`.
	pub output_extrapolated_points: u64,
	/// Successful requests the serial CPU backend produced.
	pub backend_cpu_requests: u64,
	/// Successful requests the rayon CPU pool produced.
	pub backend_parallel_requests: u64,
	/// Successful requests the GPU produced.
	pub backend_gpu_requests: u64,
	/// Successful requests where `auto` tried the GPU and fell back to the CPU pool.
	pub gpu_fallbacks: u64,
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

/// A point-in-time read of [`ReconcileMetrics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ReconcileSnapshot {
	/// Reconciliation passes that actually ran (rewrote the backlog). A
	/// threshold-gated call that held below its threshold is **not** counted.
	pub passes: u64,
	/// Total out-of-order segments rewritten sorted across all passes.
	pub segments_reconciled: u64,
	/// Per-aspect passes that failed inside a store-wide sweep (daemon tick or
	/// `POST /api/v1/storage/reconcile`). The sweep skips a failed aspect and carries on,
	/// so this counter is how a permanently failing aspect stays visible.
	pub failed_passes: u64,
}

/// A point-in-time read of [`BackupMetrics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BackupSnapshot {
	/// Control-plane snapshots that completed and verified.
	pub snapshots: u64,
	/// Total bytes written across all control-plane snapshots.
	pub bytes_written: u64,
	/// Daemon ticks that failed to snapshot.
	pub failures: u64,
	/// Unix seconds of the most recent verified snapshot, or 0 when none has
	/// been recorded yet.
	pub last_success_unix_secs: u64,
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

	/// Record one successful interpolation's provenance (ROB-16): the backend
	/// token that produced it (`cpu`, `parallel` or `gpu` — anything else is
	/// not counted, since the engine only ever names those three), whether
	/// `auto` fell back from the GPU, and the provenance kind of every output
	/// point. Called alongside [`add_output_points`](Self::add_output_points),
	/// which stays the kind-agnostic total.
	pub fn record_interpolate_output<'a>(&self, backend: &str, gpu_fallback: bool, kinds: impl Iterator<Item = &'a weftdb::ValueKind>) {
		match backend {
			"cpu" => self.interpolate.backend_cpu_requests.fetch_add(1, Ordering::Relaxed),
			"parallel" => self.interpolate.backend_parallel_requests.fetch_add(1, Ordering::Relaxed),
			"gpu" => self.interpolate.backend_gpu_requests.fetch_add(1, Ordering::Relaxed),
			_ => 0,
		};
		if gpu_fallback {
			self.interpolate.gpu_fallbacks.fetch_add(1, Ordering::Relaxed);
		}
		let (mut raw, mut interpolated, mut extrapolated) = (0_u64, 0_u64, 0_u64);
		for kind in kinds {
			match kind {
				weftdb::ValueKind::Raw => raw += 1,
				weftdb::ValueKind::Interpolated => interpolated += 1,
				weftdb::ValueKind::Extrapolated => extrapolated += 1,
				_ => {}
			}
		}
		self.interpolate.output_raw_points.fetch_add(raw, Ordering::Relaxed);
		self.interpolate.output_interpolated_points.fetch_add(interpolated, Ordering::Relaxed);
		self.interpolate.output_extrapolated_points.fetch_add(extrapolated, Ordering::Relaxed);
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

	/// Count the further segments of a multi-segment ingest commit
	/// (D-S14): [`record_ingest_seal`](Self::record_ingest_seal) already
	/// counted the first segment and every row, so only the extra
	/// segments land here.
	pub fn record_ingest_extra_segments(&self, segments: u64) {
		self.ingest.segments_sealed.fetch_add(segments, Ordering::Relaxed);
	}

	/// Record one reconciliation pass that actually ran, adding the number of
	/// out-of-order segments it rewrote. A threshold-gated call that held below its
	/// threshold must not call this — only a pass that did work is counted.
	pub fn record_reconcile_pass(&self, segments_reconciled: u64) {
		self.reconcile.passes.fetch_add(1, Ordering::Relaxed);
		self.reconcile.segments_reconciled.fetch_add(segments_reconciled, Ordering::Relaxed);
	}

	/// Record one background sweep's reconciliations (roadmap Phase 4.6 daemon):
	/// each reconciled aspect is one pass (consistent with
	/// [`record_reconcile_pass`](Metrics::record_reconcile_pass), which reconciles a
	/// single aspect), plus the segments those passes rewrote. A sweep that
	/// reconciled nothing records nothing, so a quiet daemon never bumps the counter.
	pub fn record_reconcile_sweep(&self, aspects_reconciled: u64, segments_reconciled: u64) {
		self.reconcile.passes.fetch_add(aspects_reconciled, Ordering::Relaxed);
		self.reconcile.segments_reconciled.fetch_add(segments_reconciled, Ordering::Relaxed);
	}

	/// Record the per-aspect passes a store-wide sweep could not complete (crash-consistency
	/// S4). A sweep now records a failing aspect and moves on rather than aborting, so the
	/// failure no longer surfaces as the tick's error; this counter keeps it visible.
	pub fn record_reconcile_failures(&self, failed_passes: u64) {
		self.reconcile.failed_passes.fetch_add(failed_passes, Ordering::Relaxed);
	}

	/// Record one completed, verified control-plane backup snapshot and the bytes it
	/// wrote (roadmap Phase 7.4). Only a snapshot that succeeded should call this.
	/// Also stamps the last-success time, so the health gauge tracks the newest
	/// verified snapshot whichever path took it (daemon tick or manual endpoint).
	pub fn record_backup(&self, bytes_written: u64) {
		self.backup.snapshots.fetch_add(1, Ordering::Relaxed);
		self.backup.bytes_written.fetch_add(bytes_written, Ordering::Relaxed);
		let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_secs());
		self.seed_backup_last_success(now);
	}

	/// Record one backup-daemon tick that failed to snapshot. The daemon retries
	/// on the next tick, so a transient failure is invisible without this counter.
	pub fn record_backup_failure(&self) {
		self.backup.failures.fetch_add(1, Ordering::Relaxed);
	}

	/// Move the last-success time forward to `unix_secs` when it is newer than
	/// the recorded one (never backward: a concurrent tick must not lose to a
	/// stale seed). The daemon calls this at start with the newest on-disk
	/// snapshot's stamp, so the gauge survives a restart instead of reading 0
	/// until the first tick.
	pub fn seed_backup_last_success(&self, unix_secs: u64) {
		self.backup.last_success_unix_secs.fetch_max(unix_secs, Ordering::Relaxed);
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

	/// Record the seal-path latency of one storage-ingest request.
	pub fn observe_ingest_latency(&self, elapsed: Duration) {
		self.ingest_latency.observe(elapsed);
	}

	/// Record the wall-clock duration of one completed, verified control-plane
	/// backup snapshot (EVI-25). Only a snapshot that succeeded should call
	/// this, matching [`Metrics::record_backup`]; a failed tick is visible in
	/// the failure counter instead, not as a duration.
	pub fn observe_backup_duration(&self, elapsed: Duration) {
		self.backup_latency.observe(elapsed);
	}

	/// Take a consistent-enough snapshot of all counters.
	#[must_use]
	pub fn snapshot(&self) -> MetricsSnapshot {
		MetricsSnapshot {
			interpolate: InterpolateSnapshot { requests: self.interpolate.requests.load(Ordering::Relaxed), errors: self.interpolate.errors.load(Ordering::Relaxed), output_points: self.interpolate.output_points.load(Ordering::Relaxed), output_raw_points: self.interpolate.output_raw_points.load(Ordering::Relaxed), output_interpolated_points: self.interpolate.output_interpolated_points.load(Ordering::Relaxed), output_extrapolated_points: self.interpolate.output_extrapolated_points.load(Ordering::Relaxed), backend_cpu_requests: self.interpolate.backend_cpu_requests.load(Ordering::Relaxed), backend_parallel_requests: self.interpolate.backend_parallel_requests.load(Ordering::Relaxed), backend_gpu_requests: self.interpolate.backend_gpu_requests.load(Ordering::Relaxed), gpu_fallbacks: self.interpolate.gpu_fallbacks.load(Ordering::Relaxed) },
			downsample: DownsampleSnapshot { requests: self.downsample.requests.load(Ordering::Relaxed), errors: self.downsample.errors.load(Ordering::Relaxed), output_buckets: self.downsample.output_buckets.load(Ordering::Relaxed) },
			ingest: IngestSnapshot { requests: self.ingest.requests.load(Ordering::Relaxed), errors: self.ingest.errors.load(Ordering::Relaxed), rows_sealed: self.ingest.rows_sealed.load(Ordering::Relaxed), segments_sealed: self.ingest.segments_sealed.load(Ordering::Relaxed) },
			reconcile: ReconcileSnapshot { passes: self.reconcile.passes.load(Ordering::Relaxed), segments_reconciled: self.reconcile.segments_reconciled.load(Ordering::Relaxed), failed_passes: self.reconcile.failed_passes.load(Ordering::Relaxed) },
			backup: BackupSnapshot { snapshots: self.backup.snapshots.load(Ordering::Relaxed), bytes_written: self.backup.bytes_written.load(Ordering::Relaxed), failures: self.backup.failures.load(Ordering::Relaxed), last_success_unix_secs: self.backup.last_success_unix_secs.load(Ordering::Relaxed) },
		}
	}

	/// Render the counters in the Prometheus text exposition format (v0.0.4).
	#[must_use]
	pub fn render_prometheus(&self) -> String {
		use std::fmt::Write as _;
		let snap = self.snapshot();
		let mut out = String::with_capacity(512);
		let counters = [("weft_interpolate_requests_total", "Total interpolation requests received.", snap.interpolate.requests), ("weft_interpolate_errors_total", "Interpolation requests that returned an error.", snap.interpolate.errors), ("weft_interpolate_output_points_total", "Total interpolated output points served.", snap.interpolate.output_points), ("weft_downsample_requests_total", "Total downsample requests received.", snap.downsample.requests), ("weft_downsample_errors_total", "Downsample requests that returned an error.", snap.downsample.errors), ("weft_downsample_output_buckets_total", "Total downsample buckets served.", snap.downsample.output_buckets), ("weft_ingest_requests_total", "Total storage-ingest requests received.", snap.ingest.requests), ("weft_ingest_errors_total", "Storage-ingest requests that returned an error.", snap.ingest.errors), ("weft_ingest_rows_sealed_total", "Total rows sealed across all storage ingests.", snap.ingest.rows_sealed), ("weft_ingest_segments_sealed_total", "Total segments sealed across all storage ingests.", snap.ingest.segments_sealed), ("weft_reconcile_passes_total", "Out-of-order reconciliation passes that actually ran (threshold-gated holds excluded).", snap.reconcile.passes), ("weft_reconcile_segments_reconciled_total", "Total out-of-order segments rewritten sorted across all reconciliation passes.", snap.reconcile.segments_reconciled), ("weft_reconcile_failed_passes_total", "Per-aspect passes that failed inside a store-wide maintenance sweep (the sweep skipped the aspect and continued).", snap.reconcile.failed_passes), ("weft_backup_snapshots_total", "Control-plane backup snapshots that completed and verified.", snap.backup.snapshots), ("weft_backup_bytes_written_total", "Total bytes written across all control-plane backup snapshots.", snap.backup.bytes_written), ("weft_backup_failures_total", "Backup-daemon ticks that failed to snapshot (a failed tick retries on the next tick).", snap.backup.failures)];
		for (name, help, value) in counters {
			// `writeln!` into a String is infallible.
			let _ = writeln!(out, "# HELP {name} {help}");
			let _ = writeln!(out, "# TYPE {name} counter");
			let _ = writeln!(out, "{name} {value}");
		}
		// Interpolation provenance (ROB-16): output points by the kind they were
		// served with, and requests by the backend that actually ran — the GPU
		// configuration made countable.
		let _ = writeln!(out, "# HELP weft_interpolate_output_points_by_kind_total Total interpolated output points served, by provenance kind.");
		let _ = writeln!(out, "# TYPE weft_interpolate_output_points_by_kind_total counter");
		for (kind, value) in [("raw", snap.interpolate.output_raw_points), ("interpolated", snap.interpolate.output_interpolated_points), ("extrapolated", snap.interpolate.output_extrapolated_points)] {
			let _ = writeln!(out, "weft_interpolate_output_points_by_kind_total{{kind=\"{kind}\"}} {value}");
		}
		let _ = writeln!(out, "# HELP weft_interpolate_backend_requests_total Total successful interpolation requests, by the compute backend that produced the values.");
		let _ = writeln!(out, "# TYPE weft_interpolate_backend_requests_total counter");
		for (backend, value) in [("cpu", snap.interpolate.backend_cpu_requests), ("parallel", snap.interpolate.backend_parallel_requests), ("gpu", snap.interpolate.backend_gpu_requests)] {
			let _ = writeln!(out, "weft_interpolate_backend_requests_total{{backend=\"{backend}\"}} {value}");
		}
		let _ = writeln!(out, "# HELP weft_interpolate_gpu_fallbacks_total Total successful interpolation requests where the auto backend tried the GPU and fell back to the CPU pool.");
		let _ = writeln!(out, "# TYPE weft_interpolate_gpu_fallbacks_total counter");
		let _ = writeln!(out, "weft_interpolate_gpu_fallbacks_total {}", snap.interpolate.gpu_fallbacks);
		// Health gauges: a scrape alerts on a stale backup from the last-success
		// time, not from a counter it would have to difference itself.
		let _ = writeln!(out, "# HELP weft_backup_last_success_seconds Unix seconds of the most recent verified control-plane backup snapshot, or 0 when none has been recorded yet.");
		let _ = writeln!(out, "# TYPE weft_backup_last_success_seconds gauge");
		let _ = writeln!(out, "weft_backup_last_success_seconds {}", snap.backup.last_success_unix_secs);
		// Latency histograms — the p95/p99 surface for the north-star target.
		self.interpolate_latency.render_prometheus(&mut out, "weft_interpolate_duration_seconds", "End-to-end handling latency for the interpolate endpoints (POST /api/v1/interpolate and /interpolate/ilp).");
		self.downsample_latency.render_prometheus(&mut out, "weft_downsample_duration_seconds", "End-to-end handling latency for the downsample endpoints (POST /api/v1/downsample and /downsample/ilp).");
		self.ingest_latency.render_prometheus(&mut out, "weft_ingest_duration_seconds", "Seal-path latency for the storage-ingest endpoints (JSON / CSV / ILP).");
		self.backup_latency.render_prometheus(&mut out, "weft_backup_duration_seconds", "Wall-clock duration of each completed, verified control-plane backup snapshot (manual endpoint and backup daemon alike).");
		// Phase 5 (5.7): splimes' open GPU devices, one series per device; nothing until
		// the GPU has started.
		let devices: Vec<GpuDeviceRow> = splimes::gpu_devices().into_iter().map(|device| GpuDeviceRow { name: device.info.name, api: device.info.api, device_type: device.info.device_type, calls: device.calls, chunks: device.chunks, in_flight: device.in_flight as u64, lost: device.lost, idle_pool_bytes: device.pool.idle_bytes }).collect();
		render_gpu_devices(&mut out, &devices);
		out
	}
}

/// One open GPU device as `/metrics` reports it: the fields of `splimes::GpuDeviceStats`
/// the exposition uses (that type cannot be built outside splimes, so the renderer takes
/// this one and stays testable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuDeviceRow {
	/// Adapter name.
	pub name: String,
	/// Graphics API.
	pub api: String,
	/// `discrete`, `integrated`, `virtual`, `cpu` or `other`.
	pub device_type: String,
	/// Calls, or shares of a split call, the device has run.
	pub calls: u64,
	/// Chunks it has dispatched.
	pub chunks: u64,
	/// Calls, or shares of one, running on it now.
	pub in_flight: u64,
	/// Whether the device was lost (it gets no more work).
	pub lost: bool,
	/// Bytes its buffer pool holds idle.
	pub idle_pool_bytes: u64,
}

/// Escape a Prometheus label value (backslash, double quote, newline).
fn label_value(raw: &str) -> String {
	raw.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// One per-device metric family: its name, Prometheus type, help text, and the
/// device row's value for it.
type DeviceFamily = (&'static str, &'static str, &'static str, fn(&GpuDeviceRow) -> u64);

/// Render the per-device GPU series (5.7).
///
/// Nothing at all for an empty slice, so a server
/// that never started the GPU exposes no GPU families. `device` is the index in
/// `splimes::gpu_devices()`; device 0 is the one `splimes::gpu_info()` reports.
pub fn render_gpu_devices(out: &mut String, devices: &[GpuDeviceRow]) {
	use std::fmt::Write as _;
	if devices.is_empty() {
		return;
	}
	let _ = writeln!(out, "# HELP weft_gpu_device_info The GPU devices splimes opened (1 per device; WEFT_GPU_DEVICES opens more than one).");
	let _ = writeln!(out, "# TYPE weft_gpu_device_info gauge");
	for (index, device) in devices.iter().enumerate() {
		let _ = writeln!(out, "weft_gpu_device_info{{device=\"{index}\",name=\"{}\",api=\"{}\",type=\"{}\"}} 1", label_value(&device.name), label_value(&device.api), label_value(&device.device_type));
	}
	let families: [DeviceFamily; 5] = [("weft_gpu_device_calls_total", "counter", "Interpolation calls, or shares of a split call, the device has run.", |d| d.calls), ("weft_gpu_device_chunks_total", "counter", "Chunks the device has dispatched.", |d| d.chunks), ("weft_gpu_device_in_flight", "gauge", "Calls, or shares of one, running on the device now.", |d| d.in_flight), ("weft_gpu_device_lost", "gauge", "1 when the device was lost and gets no more work, else 0.", |d| u64::from(d.lost)), ("weft_gpu_device_pool_idle_bytes", "gauge", "Bytes the device's buffer pool holds idle for reuse.", |d| d.idle_pool_bytes)];
	for (name, kind, help, value) in families {
		let _ = writeln!(out, "# HELP {name} {help}");
		let _ = writeln!(out, "# TYPE {name} {kind}");
		for (index, device) in devices.iter().enumerate() {
			let _ = writeln!(out, "{name}{{device=\"{index}\"}} {}", value(device));
		}
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
	/// Output points served with provenance `raw`.
	pub output_raw_points: u64,
	/// Output points served with provenance `interpolated`.
	pub output_interpolated_points: u64,
	/// Output points served with provenance `extrapolated`.
	pub output_extrapolated_points: u64,
	/// Successful requests the serial CPU backend produced.
	pub backend_cpu_requests: u64,
	/// Successful requests the rayon CPU pool produced.
	pub backend_parallel_requests: u64,
	/// Successful requests the GPU produced.
	pub backend_gpu_requests: u64,
	/// Successful requests where `auto` tried the GPU and fell back.
	pub gpu_fallbacks: u64,
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

/// Storage-ingest counters plus the seal-path latency quantiles.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct IngestProfile {
	/// Total storage-ingest requests (including failures).
	pub requests: u64,
	/// Storage-ingest requests that returned an error.
	pub errors: u64,
	/// Total rows sealed across all successful ingests.
	pub rows_sealed: u64,
	/// Total segments sealed across all successful ingests.
	pub segments_sealed: u64,
	/// Seal-path latency quantiles for the storage-ingest endpoints.
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
	/// Storage-ingest counters + seal-path latency quantiles.
	pub ingest: IngestProfile,
}

impl Metrics {
	/// Build the whole-server [`ProfileReport`]: the counter snapshot plus the
	/// estimated p50/p95/p99 latency for each instrumented compute endpoint.
	#[must_use]
	pub fn profile(&self) -> ProfileReport {
		let snap = self.snapshot();
		ProfileReport { service: crate::SERVICE, version: crate::VERSION, interpolate: InterpolateProfile { requests: snap.interpolate.requests, errors: snap.interpolate.errors, output_points: snap.interpolate.output_points, output_raw_points: snap.interpolate.output_raw_points, output_interpolated_points: snap.interpolate.output_interpolated_points, output_extrapolated_points: snap.interpolate.output_extrapolated_points, backend_cpu_requests: snap.interpolate.backend_cpu_requests, backend_parallel_requests: snap.interpolate.backend_parallel_requests, backend_gpu_requests: snap.interpolate.backend_gpu_requests, gpu_fallbacks: snap.interpolate.gpu_fallbacks, latency_seconds: LatencyProfile::from_snapshot(&self.interpolate_latency.snapshot()) }, downsample: DownsampleProfile { requests: snap.downsample.requests, errors: snap.downsample.errors, output_buckets: snap.downsample.output_buckets, latency_seconds: LatencyProfile::from_snapshot(&self.downsample_latency.snapshot()) }, ingest: IngestProfile { requests: snap.ingest.requests, errors: snap.ingest.errors, rows_sealed: snap.ingest.rows_sealed, segments_sealed: snap.ingest.segments_sealed, latency_seconds: LatencyProfile::from_snapshot(&self.ingest_latency.snapshot()) } }
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

	/// 5.7: no devices, no GPU families; two devices, one series each, labels escaped.
	#[test]
	fn gpu_devices_are_rendered_per_device() {
		let mut out = String::new();
		render_gpu_devices(&mut out, &[]);
		assert!(out.is_empty(), "a server that never started the GPU reports none");
		let row = |name: &str, device_type: &str, calls: u64, lost: bool| GpuDeviceRow { name: name.to_string(), api: "vulkan".to_string(), device_type: device_type.to_string(), calls, chunks: calls * 2, in_flight: 0, lost, idle_pool_bytes: 1024 };
		render_gpu_devices(&mut out, &[row("NVIDIA GeForce RTX 4070 Ti SUPER", "discrete", 7, false), row("AMD \"Radeon\" Graphics", "integrated", 3, true)]);
		assert!(out.contains("weft_gpu_device_info{device=\"0\",name=\"NVIDIA GeForce RTX 4070 Ti SUPER\",api=\"vulkan\",type=\"discrete\"} 1"), "{out}");
		assert!(out.contains("name=\"AMD \\\"Radeon\\\" Graphics\""), "quotes are escaped: {out}");
		assert!(out.contains("weft_gpu_device_calls_total{device=\"0\"} 7"));
		assert!(out.contains("weft_gpu_device_calls_total{device=\"1\"} 3"));
		assert!(out.contains("weft_gpu_device_chunks_total{device=\"0\"} 14"));
		assert!(out.contains("weft_gpu_device_lost{device=\"1\"} 1"));
		assert!(out.contains("weft_gpu_device_lost{device=\"0\"} 0"));
		assert!(out.contains("weft_gpu_device_pool_idle_bytes{device=\"1\"} 1024"));
		assert_eq!(out.matches("# TYPE ").count(), 6);
	}

	#[test]
	fn prometheus_text_is_well_formed() {
		let m = Metrics::default();
		m.record_interpolate_request();
		m.add_output_points(5);
		m.record_reconcile_pass(3);
		m.record_reconcile_failures(2);
		m.record_backup(4096);
		m.record_backup_failure();
		let text = m.render_prometheus();
		// Every counter carries HELP, TYPE, and a value line.
		assert!(text.contains("# HELP weft_interpolate_requests_total"));
		assert!(text.contains("# TYPE weft_interpolate_requests_total counter"));
		assert!(text.contains("weft_interpolate_requests_total 1"));
		assert!(text.contains("weft_interpolate_output_points_total 5"));
		// No value line is left dangling without a preceding TYPE line: 16
		// counters + the backup last-success gauge + the 4 latency histograms
		// (EVI-25 adds the backup one) + ROB-16's 3 provenance series.
		// (The GPU device families appear only once a GPU has started, which another test
		// in this process may do, so they are left out of the count.)
		assert_eq!(text.lines().filter(|line| line.starts_with("# TYPE ") && !line.starts_with("# TYPE weft_gpu_")).count(), 24);
		// The downsample counters are exposed too.
		assert!(text.contains("# TYPE weft_downsample_requests_total counter"));
		// And the storage-ingest counters.
		assert!(text.contains("# TYPE weft_ingest_rows_sealed_total counter"));
		// And the reconciliation-pass counters (one pass, three segments rewritten).
		assert!(text.contains("# TYPE weft_reconcile_passes_total counter"));
		assert!(text.contains("weft_reconcile_passes_total 1"));
		assert!(text.contains("weft_reconcile_segments_reconciled_total 3"));
		// And the per-aspect sweep failure counter (two failed aspect passes).
		assert!(text.contains("# TYPE weft_reconcile_failed_passes_total counter"));
		assert!(text.contains("weft_reconcile_failed_passes_total 2"));
		// And the control-plane backup counters (one snapshot, 4096 bytes written).
		assert!(text.contains("# TYPE weft_backup_snapshots_total counter"));
		assert!(text.contains("weft_backup_snapshots_total 1"));
		assert!(text.contains("weft_backup_bytes_written_total 4096"));
		// And the backup health signals: one failed daemon tick, and a
		// last-success gauge stamped by the successful snapshot (nonzero, and
		// no later than now).
		assert!(text.contains("# TYPE weft_backup_failures_total counter"));
		assert!(text.contains("weft_backup_failures_total 1"));
		assert!(text.contains("# TYPE weft_backup_last_success_seconds gauge"));
		let gauge_line = text.lines().find(|line| line.starts_with("weft_backup_last_success_seconds ")).expect("the gauge has a value line");
		let stamped: u64 = gauge_line.rsplit(' ').next().unwrap().parse().unwrap();
		let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
		assert!(stamped > 0 && stamped <= now, "the gauge stamps the successful snapshot: {stamped} vs now {now}");
	}

	#[test]
	fn interpolate_provenance_counters_accumulate() {
		let m = Metrics::default();
		let kinds = [weftdb::ValueKind::Raw, weftdb::ValueKind::Interpolated, weftdb::ValueKind::Interpolated, weftdb::ValueKind::Extrapolated];
		m.record_interpolate_output("gpu", false, kinds.iter());
		m.record_interpolate_output("parallel", true, kinds.iter());
		m.record_interpolate_output("cpu", false, std::iter::empty());
		let snap = m.snapshot();
		assert_eq!(snap.interpolate.output_raw_points, 2);
		assert_eq!(snap.interpolate.output_interpolated_points, 4);
		assert_eq!(snap.interpolate.output_extrapolated_points, 2);
		assert_eq!(snap.interpolate.backend_gpu_requests, 1);
		assert_eq!(snap.interpolate.backend_parallel_requests, 1);
		assert_eq!(snap.interpolate.backend_cpu_requests, 1);
		assert_eq!(snap.interpolate.gpu_fallbacks, 1);
		let text = m.render_prometheus();
		assert!(text.contains("weft_interpolate_output_points_by_kind_total{kind=\"interpolated\"} 4"), "{text}");
		assert!(text.contains("weft_interpolate_backend_requests_total{backend=\"gpu\"} 1"), "{text}");
		assert!(text.contains("weft_interpolate_gpu_fallbacks_total 1"), "{text}");
		// The profile carries the same provenance counts.
		let profile = m.profile();
		assert_eq!(profile.interpolate.output_interpolated_points, 4);
		assert_eq!(profile.interpolate.gpu_fallbacks, 1);
	}

	#[test]
	fn backup_last_success_seed_never_moves_backward() {
		let m = Metrics::default();
		assert_eq!(m.snapshot().backup.last_success_unix_secs, 0, "no snapshot recorded yet");
		m.seed_backup_last_success(1_000);
		assert_eq!(m.snapshot().backup.last_success_unix_secs, 1_000);
		m.seed_backup_last_success(500);
		assert_eq!(m.snapshot().backup.last_success_unix_secs, 1_000, "a stale seed must not lose to a newer tick");
	}

	#[test]
	fn backup_duration_histogram_is_rendered() {
		let m = Metrics::default();
		m.observe_backup_duration(Duration::from_millis(3));
		let text = m.render_prometheus();
		assert!(text.contains("# TYPE weft_backup_duration_seconds histogram"), "{text}");
		assert!(text.contains("weft_backup_duration_seconds_bucket{le=\"0.005\"} 1"), "{text}");
		assert!(text.contains("weft_backup_duration_seconds_count 1"), "{text}");
	}

	#[test]
	fn latency_histogram_buckets_and_sum() {
		let h = LatencyHistogram::default();
		// 0.3 ms → first bucket (le=0.0005); 3 ms → le=0.005; 30 ms → le=0.05.
		h.observe(Duration::from_micros(300));
		h.observe(Duration::from_micros(3_000));
		h.observe(Duration::from_millis(30));
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
		assert!(text.contains("# TYPE weft_interpolate_duration_seconds histogram"));
		// Cumulative: the le=0.0005 bucket holds 1, and by le=0.005 both fall in.
		assert!(text.contains("weft_interpolate_duration_seconds_bucket{le=\"0.0005\"} 1"), "{text}");
		assert!(text.contains("weft_interpolate_duration_seconds_bucket{le=\"0.005\"} 2"), "{text}");
		assert!(text.contains("weft_interpolate_duration_seconds_bucket{le=\"+Inf\"} 2"), "{text}");
		assert!(text.contains("weft_interpolate_duration_seconds_count 2"), "{text}");
		// The downsample histogram is exposed even with no observations.
		assert!(text.contains("# TYPE weft_downsample_duration_seconds histogram"));
		assert!(text.contains("weft_downsample_duration_seconds_count 0"));
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
