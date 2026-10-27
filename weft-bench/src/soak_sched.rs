//! Open-loop scheduler, HDR interval logs and harness self-monitoring
//! (1.0 plan SOA-3, W2).
//!
//! This is the slice SOA-1's module doc promised: the scheduler moves a
//! harness through its lifecycle, on the scheduler path it must use
//! [`SoakHarness::start_checked`] (never the ungated `start`), it calls
//! [`SoakHarness::record_step`] once per executed tick, and it ends the run
//! with [`SoakHarness::finish`] or [`SoakHarness::abort`]. SOA-4 supplies
//! the work each step performs; until then the step is whatever the caller
//! passes — a [`SoakServer`] via [`OpenLoopScheduler::run_with_server`]
//! (the null server runs the whole harness with no live server), or a
//! caller-measured step closure via [`OpenLoopScheduler::run_gated`].
//!
//! **Open-loop, not closed-loop.** Ticks are scheduled on the clock, not
//! on completion: the scheduler plans one tick per `interval_ms` for the
//! run's `duration_secs`, and a slow step does not push later ticks later.
//! When a step overruns its tick — the wall clock is already past the next
//! tick's deadline — that tick is *skipped* and counted as an overrun, not
//! executed late: executing it late would drift the whole schedule and
//! silently turn an open-loop run into a closed-loop one. Skipped ticks
//! are visible in the self-monitor, never hidden.
//!
//! **The schedule is computed, not materialized.** A 24 h soak at a 1 ms
//! interval is ~86M ticks; building that vector up front would allocate
//! hundreds of megabytes for a schedule the loop only ever walks in order.
//! [`SchedulerConfig::tick_count`] and the loop's `index * interval_ms`
//! offset give the same plan with O(1) memory.
//!
//! **Interval logs.** Step durations are grouped by wall-clock interval
//! (`interval_log_secs`, from each tick's planned offset) into HDR
//! histograms ([`crate::soak_hdr::HdrHistogram`]); each interval closes
//! as an [`IntervalRecord`] carrying count, exact min/max/mean and the
//! p50/p90/p99/p999 read from its histogram. Intervals with no executed
//! tick still appear, with count zero and no percentiles, so a gap in the
//! log is a visible gap, not a missing row.
//!
//! **Self-monitoring.** The scheduler watches itself while it runs: the
//! [`SelfMonitor`] counts scheduled, executed and skipped-overrun ticks
//! and tracks the slowest step. [`SelfMonitor::warnings`] names every
//! structurally bad condition — a skipped tick, a step slower than the
//! tick interval — as warnings, not invented thresholds: each one is a
//! fact about the schedule, not a judgement about whether the rate was
//! "good". A failed step aborts the run and surfaces as the returned
//! error, not as a report warning.
//!
//! **No schema bump.** The scheduler's report ([`SchedulerReport`]) is its
//! own artifact, beside the run record, not a field on [`crate::soak::SoakRun`]: SOA-1's
//! bump rule says a field on the run record would move
//! [`crate::soak::SOAK_SCHEMA_VERSION`], and the scheduler adds none, so the soak
//! schema stays at v1. The report carries no free text — no label, no
//! path — so it needs no redaction pass.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::{
	soak::{SoakHarness, SoakPreflight, SoakState}, soak_hdr::HdrHistogram, soak_null::SoakServer
};

/// Configuration for one scheduled soak run.
///
/// Validated on construction: an interval of zero would schedule ticks
/// infinitely fast, and an interval-log window of zero would divide by
/// zero when grouping ticks, so both are rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulerConfig {
	/// Milliseconds between scheduled ticks.
	pub interval_ms: u64,
	/// Seconds per interval-log window.
	pub interval_log_secs: u64,
	/// Cap on executed ticks, if any. `None` runs every planned tick for
	/// the run's duration; a cap lets a caller run a prefix of a long run
	/// (a smoke run against a 24 h config) without editing the config.
	pub max_steps: Option<u64>,
}

impl SchedulerConfig {
	/// Build a config, rejecting a zero tick interval or a zero log window.
	///
	/// # Errors
	///
	/// Returns an error when `interval_ms` or `interval_log_secs` is zero.
	pub fn new(interval_ms: u64, interval_log_secs: u64) -> anyhow::Result<Self> {
		anyhow::ensure!(interval_ms > 0, "scheduler interval_ms must be positive");
		anyhow::ensure!(interval_log_secs > 0, "scheduler interval_log_secs must be positive");
		Ok(Self { interval_ms, interval_log_secs, max_steps: None })
	}

	/// Cap the run at `max_steps` executed ticks.
	#[must_use]
	pub const fn with_max_steps(mut self, max_steps: u64) -> Self {
		self.max_steps = Some(max_steps);
		self
	}

	/// Number of ticks planned for a run of `duration_secs` at this
	/// interval: the floor of `duration_secs * 1000 / interval_ms`. A
	/// duration shorter than one interval plans zero ticks — the run
	/// still starts and finishes, with zero steps, rather than inventing
	/// a tick the schedule never asked for.
	#[must_use]
	pub fn tick_count(&self, duration_secs: u64) -> u64 {
		duration_secs.saturating_mul(1000) / self.interval_ms
	}

	/// Length of one interval-log window in milliseconds.
	#[must_use]
	pub fn interval_log_ms(&self) -> u64 {
		self.interval_log_secs.saturating_mul(1000)
	}
}

/// What the tick gate decides for one scheduled tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickDecision {
	/// Run the step for this tick.
	Execute,
	/// Skip this tick (an overrun: the clock is already past its
	/// deadline). Counted in the self-monitor, never executed late.
	Skip,
}

/// The scheduler's self-monitoring counters for one run.
///
/// Counts only — no free text, no thresholds. Ratios and warnings are
/// derived from these counts by the accessors, so the artifact stays a
/// record of what happened and the interpretation stays beside it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SelfMonitor {
	/// Ticks the schedule planned (after any `max_steps` cap).
	pub scheduled: u64,
	/// Ticks whose step ran and was recorded.
	pub executed: u64,
	/// Ticks skipped because the clock was already past their deadline.
	pub skipped: u64,
	/// Slowest executed step in nanoseconds, if any step ran.
	pub max_step_ns: Option<u64>,
	/// Sum of executed step durations in nanoseconds.
	pub total_step_ns: u128,
}

impl SelfMonitor {
	/// Fraction of scheduled ticks skipped as overruns (0 when nothing
	/// was scheduled).
	#[must_use]
	pub fn overrun_ratio(&self) -> f64 {
		if self.scheduled == 0 {
			0.0
		} else {
			self.skipped as f64 / self.scheduled as f64
		}
	}

	/// Structurally bad conditions in this run, each naming the fact:
	/// skipped ticks, and a step slower than the tick
	/// interval (a step that slow guarantees the next tick overruns).
	/// An empty list means the schedule ran exactly as planned.
	#[must_use]
	pub fn warnings(&self, interval_ms: u64) -> Vec<String> {
		let mut warnings = Vec::new();
		if self.skipped > 0 {
			warnings.push(format!("{} tick(s) skipped as overruns", self.skipped));
		}
		if let Some(max) = self.max_step_ns {
			if max > interval_ms.saturating_mul(1_000_000) {
				warnings.push(format!("slowest step took {max} ns, longer than the {interval_ms} ms tick interval"));
			}
		}
		warnings
	}
}

/// One closed interval-log window: the HDR summary of the step durations
/// whose ticks fell in it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntervalRecord {
	/// Window index from the start of the run (0-based).
	pub interval_index: usize,
	/// Window start as a planned offset from the run start, in ms.
	pub start_offset_ms: u64,
	/// Window end as a planned offset from the run start, in ms.
	pub end_offset_ms: u64,
	/// Executed steps in this window.
	pub count: u64,
	/// Fastest step in this window, if any step ran.
	pub min_ns: Option<u64>,
	/// Slowest step in this window, if any step ran.
	pub max_ns: Option<u64>,
	/// Mean step duration in this window, if any step ran.
	pub mean_ns: Option<f64>,
	/// Median step duration in this window, if any step ran.
	pub p50_ns: Option<u64>,
	/// 90th percentile step duration in this window, if any step ran.
	pub p90_ns: Option<u64>,
	/// 99th percentile step duration in this window, if any step ran.
	pub p99_ns: Option<u64>,
	/// 99.9th percentile step duration in this window, if any step ran.
	pub p999_ns: Option<u64>,
}

impl IntervalRecord {
	/// Build a record from a window's histogram.
	fn from_histogram(interval_index: usize, start_offset_ms: u64, end_offset_ms: u64, histogram: &HdrHistogram) -> Self {
		Self { interval_index, start_offset_ms, end_offset_ms, count: histogram.total_count(), min_ns: histogram.min_ns(), max_ns: histogram.max_ns(), mean_ns: histogram.mean_ns(), p50_ns: histogram.p50_ns(), p90_ns: histogram.p90_ns(), p99_ns: histogram.p99_ns(), p999_ns: histogram.p999_ns() }
	}
}

/// The HDR interval log for one run: one record per window, in order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct IntervalLog {
	/// Seconds per window, from the scheduler config.
	pub interval_log_secs: u64,
	/// Records in window order. Every window up to the last planned tick
	/// appears, including empty ones.
	pub intervals: Vec<IntervalRecord>,
}

impl IntervalLog {
	/// Total executed steps across all windows.
	#[must_use]
	pub fn total_count(&self) -> u64 {
		self.intervals.iter().map(|interval| interval.count).sum()
	}

	/// Serialize the log as a pretty JSON artifact.
	///
	/// # Errors
	///
	/// Returns an error when serialization fails.
	pub fn to_json(&self) -> anyhow::Result<String> {
		Ok(serde_json::to_string_pretty(self)?)
	}
}

/// The scheduler's report for one run: the self-monitor, the interval log,
/// and the harness state the run ended in. Serialized as the scheduler's
/// own artifact; it is not part of the soak run record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchedulerReport {
	/// Self-monitoring counters for the run.
	pub monitor: SelfMonitor,
	/// HDR interval log for the run.
	pub interval_log: IntervalLog,
	/// Lifecycle state the harness ended in (`Finished`, or `Aborted`
	/// when a step failed — never returned alongside the error, since the
	/// caller gets the error instead; see [`OpenLoopScheduler::run_gated`]).
	pub final_state: SoakState,
	/// Steps recorded on the harness (equals `monitor.executed`).
	pub steps: u64,
}

impl SchedulerReport {
	/// Serialize the report as a pretty JSON artifact.
	///
	/// # Errors
	///
	/// Returns an error when serialization fails.
	pub fn to_json(&self) -> anyhow::Result<String> {
		Ok(serde_json::to_string_pretty(self)?)
	}
}

/// The open-loop scheduler: plans ticks on the clock and drives a harness
/// through them.
///
/// The scheduler holds no harness state itself — it borrows the harness
/// for one run — so the same scheduler can drive run after run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenLoopScheduler {
	config: SchedulerConfig,
}

impl OpenLoopScheduler {
	/// Build a scheduler from a validated config.
	#[must_use]
	pub const fn new(config: SchedulerConfig) -> Self {
		Self { config }
	}

	/// This scheduler's config.
	#[must_use]
	pub const fn config(&self) -> SchedulerConfig {
		self.config
	}

	/// Run a harness through its schedule in real time: ticks fire on the
	/// wall clock, a step that overruns its tick causes the next tick to
	/// be skipped (open-loop cadence, never drift), and the step's
	/// duration is measured around the call.
	///
	/// The harness must be `Created` and the preflight must have passed;
	/// this is [`SoakHarness::start_checked`]'s gate, and its error is
	/// returned unchanged when the run cannot start.
	///
	/// # Errors
	///
	/// Returns an error when the preflight gate refuses the start, when
	/// a step fails (the harness is aborted first), or when a harness
	/// transition fails.
	pub fn run(&self, harness: &mut SoakHarness, preflight: &SoakPreflight, mut step: impl FnMut(u64) -> anyhow::Result<()>) -> anyhow::Result<SchedulerReport> {
		let start = Instant::now();
		self.run_gated(
			harness,
			preflight,
			|tick| {
				let started = Instant::now();
				let result = step(tick);
				(started.elapsed(), result)
			},
			move |_index, offset_ms| {
				let elapsed_ms = start.elapsed().as_millis() as u64;
				if elapsed_ms > offset_ms {
					TickDecision::Skip
				} else {
					let remaining = offset_ms - elapsed_ms;
					if remaining > 0 {
						std::thread::sleep(Duration::from_millis(remaining));
					}
					TickDecision::Execute
				}
			},
		)
	}

	/// Run a harness through its schedule against a server in real time:
	/// each executed tick submits to the server. With a [`crate::soak_null::NullServer`]
	/// this runs the whole harness with no live server.
	///
	/// # Errors
	///
	/// Returns an error on the same conditions as [`OpenLoopScheduler::run`].
	pub fn run_with_server(&self, harness: &mut SoakHarness, preflight: &SoakPreflight, server: &mut impl SoakServer) -> anyhow::Result<SchedulerReport> {
		self.run(harness, preflight, |tick| server.handle(tick))
	}

	/// Run a harness through its schedule with a caller-supplied gate and
	/// caller-measured step durations. This is the deterministic path:
	/// tests (and callers replaying a schedule) pass an always-execute
	/// gate and fake durations, so the report is fully determined by its
	/// inputs and no wall clock is involved.
	///
	/// The step closure returns the step's duration alongside its result,
	/// so the interval log records what the caller measured, not a
	/// second measurement taken here.
	///
	/// On a step failure the harness is aborted and the error is
	/// returned (with the failing tick named); no report is returned in
	/// that case, because the run did not complete — the caller reads the
	/// aborted state off the harness itself.
	///
	/// # Errors
	///
	/// Returns an error when the preflight gate refuses the start, when
	/// a step fails (the harness is aborted first), or when a harness
	/// transition fails.
	pub fn run_gated(&self, harness: &mut SoakHarness, preflight: &SoakPreflight, mut step: impl FnMut(u64) -> (Duration, anyhow::Result<()>), mut gate: impl FnMut(usize, u64) -> TickDecision) -> anyhow::Result<SchedulerReport> {
		harness.start_checked(preflight)?;
		let planned = self.config.tick_count(harness.run_record().config.duration_secs);
		let scheduled = self.config.max_steps.map_or(planned, |cap| planned.min(cap));
		let window_ms = self.config.interval_log_ms();
		let mut monitor = SelfMonitor { scheduled, ..SelfMonitor::default() };
		let mut windows: Vec<HdrHistogram> = Vec::new();
		for index in 0..scheduled {
			let offset_ms = index.saturating_mul(self.config.interval_ms);
			if gate(index as usize, offset_ms) == TickDecision::Skip {
				monitor.skipped = monitor.skipped.saturating_add(1);
				continue;
			}
			let (duration, result) = step(index);
			if let Err(error) = result {
				harness.abort()?;
				return Err(anyhow::anyhow!("soak step {index} failed: {error:#}"));
			}
			harness.record_step()?;
			monitor.executed = monitor.executed.saturating_add(1);
			let duration_ns = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
			monitor.max_step_ns = Some(monitor.max_step_ns.map_or(duration_ns, |max| max.max(duration_ns)));
			monitor.total_step_ns += u128::from(duration_ns);
			let window = (offset_ms / window_ms) as usize;
			if windows.len() <= window {
				windows.resize_with(window + 1, HdrHistogram::new);
			}
			windows[window].record(duration_ns);
		}
		harness.finish()?;
		// A run that planned no ticks logs no windows; otherwise every
		// window up to the last planned tick appears, empty ones included.
		let intervals = if scheduled == 0 {
			Vec::new()
		} else {
			let last_window = ((scheduled - 1).saturating_mul(self.config.interval_ms) / window_ms) as usize;
			(0..=last_window)
				.map(|window| {
					let start_offset_ms = (window as u64).saturating_mul(window_ms);
					let end_offset_ms = start_offset_ms.saturating_add(window_ms);
					windows.get(window).map_or_else(|| IntervalRecord::from_histogram(window, start_offset_ms, end_offset_ms, &HdrHistogram::new()), |histogram| IntervalRecord::from_histogram(window, start_offset_ms, end_offset_ms, histogram))
				})
				.collect()
		};
		Ok(SchedulerReport { monitor, interval_log: IntervalLog { interval_log_secs: self.config.interval_log_secs, intervals }, final_state: harness.state(), steps: harness.run_record().steps })
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		report::RunMetadata, soak::{SoakConfig, SOAK_SCHEMA_VERSION}, soak_null::NullServer
	};

	fn metadata() -> RunMetadata {
		RunMetadata::capture("2026-10-25T00:00:00+00:00".to_string())
	}

	fn harness_and_preflight(duration_secs: u64) -> (SoakHarness, SoakPreflight) {
		let config = SoakConfig::new("smoke".to_string(), String::new(), 7, duration_secs).expect("valid config");
		let harness = SoakHarness::new(config, metadata());
		let preflight = harness.preflight(&std::env::temp_dir()).expect("preflight from Created");
		assert!(preflight.passed(), "preflight on the temp dir must pass: {:?}", preflight.checks);
		(harness, preflight)
	}

	fn always_execute(_index: usize, _offset_ms: u64) -> TickDecision {
		TickDecision::Execute
	}

	#[test]
	fn config_rejects_zero_interval_and_zero_log_window() {
		assert!(SchedulerConfig::new(0, 1).is_err(), "zero tick interval must be rejected");
		assert!(SchedulerConfig::new(100, 0).is_err(), "zero log window must be rejected");
		assert!(SchedulerConfig::new(100, 1).is_ok(), "positive interval and window must be accepted");
	}

	#[test]
	fn tick_count_is_the_floor_and_costs_no_allocation() {
		let config = SchedulerConfig::new(100, 1).expect("valid config");
		assert_eq!(config.tick_count(10), 100, "10 s at 100 ms plans 100 ticks");
		assert_eq!(config.tick_count(1), 10, "1 s at 100 ms plans 10 ticks");
		assert_eq!(config.tick_count(0), 0, "a zero duration plans no ticks");
		// A duration shorter than one interval plans no tick rather than
		// inventing one; a 24 h run at 1 ms is a count, not a vector.
		assert_eq!(SchedulerConfig::new(150, 1).expect("valid config").tick_count(1), 6);
		assert_eq!(SchedulerConfig::new(1, 1).expect("valid config").tick_count(86_400), 86_400_000);
	}

	#[test]
	fn gated_run_finishes_with_hdr_interval_records() {
		let (mut harness, preflight) = harness_and_preflight(2);
		let scheduler = OpenLoopScheduler::new(SchedulerConfig::new(100, 1).expect("valid config"));
		let report = scheduler.run_gated(&mut harness, &preflight, |_| (Duration::from_nanos(500), Ok(())), always_execute).expect("gated run");
		assert_eq!(report.final_state, SoakState::Finished);
		assert_eq!(report.steps, 20, "2 s at 100 ms executes 20 ticks");
		assert_eq!(report.monitor.scheduled, 20);
		assert_eq!(report.monitor.executed, 20);
		assert_eq!(report.monitor.skipped, 0);
		assert!(report.monitor.warnings(100).is_empty(), "an exact run warns about nothing");
		assert_eq!(report.interval_log.intervals.len(), 2, "2 s at a 1 s window logs 2 intervals");
		assert_eq!(report.interval_log.total_count(), 20);
		for interval in &report.interval_log.intervals {
			assert_eq!(interval.count, 10);
			assert_eq!(interval.min_ns, Some(500));
			assert_eq!(interval.max_ns, Some(500));
			assert_eq!(interval.p50_ns, Some(500));
		}
		assert_eq!(harness.run_record().steps, 20);
		assert_eq!(harness.run_record().soak_schema_version, SOAK_SCHEMA_VERSION, "the scheduler adds no run-record field, so the soak schema stays at v1");
	}

	#[test]
	fn skipped_ticks_are_counted_and_never_executed_late() {
		let (mut harness, preflight) = harness_and_preflight(1);
		let scheduler = OpenLoopScheduler::new(SchedulerConfig::new(100, 1).expect("valid config"));
		let mut executed_ticks = Vec::new();
		let report = scheduler
			.run_gated(
				&mut harness,
				&preflight,
				|tick| {
					executed_ticks.push(tick);
					(Duration::from_nanos(100), Ok(()))
				},
				|index, _| {
					if index % 2 == 0 {
						TickDecision::Execute
					} else {
						TickDecision::Skip
					}
				},
			)
			.expect("gated run with skips");
		assert_eq!(report.monitor.scheduled, 10);
		assert_eq!(report.monitor.executed, 5);
		assert_eq!(report.monitor.skipped, 5);
		assert_eq!(executed_ticks, vec![0, 2, 4, 6, 8], "skipped ticks must not run at all, late or otherwise");
		assert_eq!(report.final_state, SoakState::Finished, "skips do not abort the run");
		assert!(!report.monitor.warnings(100).is_empty(), "skipped ticks must warn");
		assert!(report.monitor.overrun_ratio() > 0.0);
	}

	#[test]
	fn failing_step_aborts_the_harness_and_names_the_tick() {
		let (mut harness, preflight) = harness_and_preflight(1);
		let scheduler = OpenLoopScheduler::new(SchedulerConfig::new(100, 1).expect("valid config"));
		let error = scheduler
			.run_gated(
				&mut harness,
				&preflight,
				|tick| {
					if tick == 3 {
						(Duration::ZERO, Err(anyhow::anyhow!("server down")))
					} else {
						(Duration::from_nanos(100), Ok(()))
					}
				},
				always_execute,
			)
			.expect_err("a failing step must fail the run");
		assert!(error.to_string().contains("soak step 3 failed"), "the error must name the failing tick: {error}");
		assert_eq!(harness.state(), SoakState::Aborted);
		assert_eq!(harness.run_record().steps, 3, "only the ticks before the failure were recorded");
	}

	#[test]
	fn gated_run_refuses_a_failed_preflight_without_starting() {
		let config = SoakConfig::new("smoke".to_string(), String::new(), 7, 1).expect("valid config");
		let mut harness = SoakHarness::new(config, metadata());
		let missing = std::env::temp_dir().join("weftdb-soa-3-no-such-store-dir");
		let preflight = harness.preflight(&missing).expect("a failed preflight still produces a report");
		assert!(!preflight.passed());
		let scheduler = OpenLoopScheduler::new(SchedulerConfig::new(100, 1).expect("valid config"));
		assert!(scheduler.run_gated(&mut harness, &preflight, |_| (Duration::ZERO, Ok(())), always_execute).is_err(), "the scheduler path must use start_checked");
		assert_eq!(harness.state(), SoakState::Created, "a refused start leaves the harness Created");
	}

	#[test]
	fn max_steps_caps_a_long_run() {
		let (mut harness, preflight) = harness_and_preflight(10);
		let scheduler = OpenLoopScheduler::new(SchedulerConfig::new(100, 1).expect("valid config").with_max_steps(3));
		let report = scheduler.run_gated(&mut harness, &preflight, |_| (Duration::from_nanos(100), Ok(())), always_execute).expect("capped run");
		assert_eq!(report.monitor.scheduled, 3);
		assert_eq!(report.steps, 3);
		assert_eq!(report.final_state, SoakState::Finished);
	}

	#[test]
	fn empty_intervals_still_appear_in_the_log() {
		let (mut harness, preflight) = harness_and_preflight(2);
		let scheduler = OpenLoopScheduler::new(SchedulerConfig::new(100, 1).expect("valid config"));
		let report = scheduler
			.run_gated(
				&mut harness,
				&preflight,
				|_| (Duration::from_nanos(100), Ok(())),
				|index, _| {
					if index < 10 {
						TickDecision::Execute
					} else {
						TickDecision::Skip
					}
				},
			)
			.expect("gated run");
		assert_eq!(report.interval_log.intervals.len(), 2, "both windows appear even though the second ran nothing");
		assert_eq!(report.interval_log.intervals[1].count, 0);
		assert_eq!(report.interval_log.intervals[1].p50_ns, None, "an empty window has no percentiles");
	}

	#[test]
	fn slow_step_warns_about_the_interval_it_breaks() {
		let (mut harness, preflight) = harness_and_preflight(1);
		let scheduler = OpenLoopScheduler::new(SchedulerConfig::new(100, 1).expect("valid config"));
		let report = scheduler.run_gated(&mut harness, &preflight, |_| (Duration::from_millis(250), Ok(())), always_execute).expect("gated run");
		let warnings = report.monitor.warnings(100);
		assert_eq!(warnings.len(), 1, "a step slower than the interval is the one warning: {warnings:?}");
		assert!(report.interval_log.intervals[0].max_ns.expect("max") >= 250_000_000);
	}

	#[test]
	fn null_server_run_completes_with_no_live_server() {
		let (mut harness, preflight) = harness_and_preflight(1);
		let scheduler = OpenLoopScheduler::new(SchedulerConfig::new(100, 1).expect("valid config"));
		let mut server = NullServer::new();
		assert!(!server.is_live());
		let report = scheduler.run_with_server(&mut harness, &preflight, &mut server).expect("null server run");
		assert_eq!(report.final_state, SoakState::Finished);
		// `run_with_server` is real time: a runner that stalls past a tick's deadline skips it, by
		// design, so the executed count is only bounded here. The exact count is the next test's,
		// through the path that has no clock.
		assert_eq!(report.monitor.scheduled, 10, "1 s at 100 ms plans 10 ticks");
		assert_eq!(report.monitor.executed + report.monitor.skipped, 10, "every planned tick either ran or was skipped, none vanished");
		assert_eq!(report.steps, report.monitor.executed);
		assert_eq!(server.steps_accepted(), report.monitor.executed, "every executed tick reached the null server");
		assert_eq!(report.interval_log.total_count(), report.monitor.executed);
	}

	#[test]
	fn null_server_takes_every_tick_when_none_is_skipped() {
		let (mut harness, preflight) = harness_and_preflight(1);
		let scheduler = OpenLoopScheduler::new(SchedulerConfig::new(100, 1).expect("valid config"));
		let mut server = NullServer::new();
		let report = scheduler.run_gated(&mut harness, &preflight, |tick| (Duration::ZERO, server.handle(tick)), always_execute).expect("gated null server run");
		assert_eq!(report.final_state, SoakState::Finished);
		assert_eq!(report.steps, 10, "1 s at 100 ms runs 10 ticks against the null server");
		assert_eq!(server.steps_accepted(), 10, "every executed tick reached the null server");
		assert_eq!(report.interval_log.total_count(), 10);
	}

	#[test]
	fn report_round_trips_through_json() {
		let (mut harness, preflight) = harness_and_preflight(1);
		let scheduler = OpenLoopScheduler::new(SchedulerConfig::new(100, 1).expect("valid config"));
		let report = scheduler.run_gated(&mut harness, &preflight, |_| (Duration::from_nanos(700), Ok(())), always_execute).expect("gated run");
		let json = report.to_json().expect("serialize report");
		let back: SchedulerReport = serde_json::from_str(&json).expect("deserialize report");
		assert_eq!(report, back);
		let log_json = report.interval_log.to_json().expect("serialize log");
		let log_back: IntervalLog = serde_json::from_str(&log_json).expect("deserialize log");
		assert_eq!(report.interval_log, log_back);
	}
}
