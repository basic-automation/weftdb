//! GPU economics workload (roadmap 5.5) — what does running interpolation on the GPU cost,
//! compared with running it on the CPU only?
//!
//! Five lanes, each written as its own result (one report per lane, as the query-mix and
//! compressed-query runs write one per lane / window):
//!
//! - **Cold start** (`cold_start`): a fresh child process per sample (`__gpu-economics-child`),
//!   timed from the parent's spawn to the child's first GPU result. `prewarm` children call
//!   `splimes::prewarm_gpu` first (device creation and kernel compilation, timed apart), then
//!   the first call; `lazy` children make the first `Backend::Gpu` call directly, so that call
//!   opens the device. Each child also times a few warm calls, so first vs warm is in the same
//!   process. The first child of a run may also pay for a cold driver shader cache; the
//!   samples are kept in order so that is visible.
//! - **No adapter** (`no_adapter`): a child with every Vulkan driver and EGL vendor hidden
//!   (`VK_ICD_FILENAMES`, `VK_DRIVER_FILES`, `__EGL_VENDOR_LIBRARY_FILENAMES` pointed at a
//!   file that does not exist), which records what `prewarm_gpu`, a `Backend::Gpu` call and a
//!   `Backend::Auto` call do there. Whether the adapter really was hidden is recorded, not
//!   assumed.
//! - **Fallback** (`fallback`): the same call (one knot set, one grid) on `Cpu`, `Parallel`,
//!   `Gpu` in `f64` and in `f32`, and `Auto`, timed per call in this process, with each
//!   backend's largest difference from the `Cpu` output.
//! - **Contention** (`contention`): 1, 2, 4, 8 and 16 concurrent callers (threads released
//!   together by a barrier), each making the same number of calls on one backend. Throughput
//!   is output points over the wall time; the latency is per call.
//! - **Utilization**: while each GPU lane runs, `nvidia-smi` is sampled out of process
//!   (`utilization.gpu`, `memory.used`), beside a baseline taken before any GPU work. These
//!   are device-wide counters: on a shared GPU they include every other process, which the
//!   baseline shows. Absent `nvidia-smi` (a non-NVIDIA machine), the block is empty.
//!
//! Every lane's correctness gate: every call returns its full grid of finite values. The
//! cost arithmetic ($ per billion points) is in `bench/reports/gpu-economics/README.md`,
//! computed from these throughputs; the bench itself records no prices.

use std::{
	io::{BufRead, BufReader}, path::Path, process::{Child, Command, Stdio}, sync::{Arc, Barrier}, thread::JoinHandle, time::{Duration as StdDuration, Instant, SystemTime, UNIX_EPOCH}
};

use chrono::{DateTime, Duration, TimeZone, Utc};
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};
use splimes::{Backend, Interpolator, Precision, Resolution, Spline};

use crate::{
	engine::{is_software_adapter, CalibrationStatus, EngineMetadata}, schema::{BenchResult, ColdWarm, CorrectnessReport, DatasetMeta, TimingBreakdown, SCHEMA_VERSION}, stats::LatencyStats
};

/// Workload class label recorded for every GPU economics lane.
const WORKLOAD_GPU_ECONOMICS: &str = "gpu_economics";

/// The hidden subcommand the cold-start and no-adapter children run as.
pub const CHILD_COMMAND: &str = "__gpu-economics-child";

/// Default seed for GPU economics profiles.
pub const DEFAULT_GPU_ECONOMICS_SEED: u64 = 0x6EC0_5EED;

/// The prefix of the one line a child prints its measurements on.
const CHILD_LINE: &str = "GE ";

/// A path that does not exist, used to hide every GPU driver from the no-adapter child.
const NO_DRIVERS: &str = "/nonexistent/weft-bench-no-gpu-drivers.json";

/// Where a lane's calls run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
	/// `Backend::Cpu`: the calling thread only.
	Cpu,
	/// `Backend::Parallel`: rayon's pool.
	Parallel,
	/// `Backend::Gpu` with `f64` shaders.
	GpuF64,
	/// `Backend::Gpu` with `f32` shaders.
	GpuF32,
	/// `Backend::Auto` with the GPU started and splimes' default thresholds.
	Auto,
}

impl Lane {
	/// Every lane, in the default fallback order (`Cpu` first: the others are diffed against it).
	pub const ALL: [Self; 5] = [Self::Cpu, Self::Parallel, Self::GpuF64, Self::GpuF32, Self::Auto];

	/// The lane's name, as the flags and profile names spell it.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Cpu => "cpu",
			Self::Parallel => "parallel",
			Self::GpuF64 => "gpu-f64",
			Self::GpuF32 => "gpu-f32",
			Self::Auto => "auto",
		}
	}

	/// Parse a lane name ([`Self::as_str`]).
	#[must_use]
	pub fn parse(name: &str) -> Option<Self> {
		Self::ALL.into_iter().find(|l| l.as_str() == name)
	}

	/// Whether the lane needs a started GPU.
	#[must_use]
	pub const fn needs_gpu(self) -> bool {
		matches!(self, Self::GpuF64 | Self::GpuF32 | Self::Auto)
	}

	/// The interpolator the lane's calls use.
	#[must_use]
	pub const fn interpolator(self) -> Interpolator {
		let base = Interpolator::new(Spline::Cubic, Resolution::Seconds);
		match self {
			Self::Cpu => base.backend(Backend::Cpu),
			Self::Parallel => base.backend(Backend::Parallel),
			Self::GpuF64 => base.backend(Backend::Gpu).gpu_precision(Precision::F64),
			Self::GpuF32 => base.backend(Backend::Gpu).gpu_precision(Precision::F32),
			Self::Auto => base.backend(Backend::Auto),
		}
	}
}

/// The tunable knobs for a GPU economics profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuEconomicsParams {
	/// RNG seed for the knots.
	pub seed: u64,
	/// Output grid points per call.
	pub points: usize,
	/// Input knots per call (irregular, spread over the grid).
	pub knots: usize,
	/// Timed calls per fallback lane.
	pub calls: usize,
	/// The fallback lanes, in order.
	pub lanes: Vec<Lane>,
	/// Concurrent callers per contention step.
	pub concurrency: Vec<usize>,
	/// Calls each contention caller makes.
	pub calls_per_caller: usize,
	/// The contention lanes.
	pub contention: Vec<Lane>,
	/// Child processes per cold-start mode (`prewarm`, `lazy`); 0 skips the cold-start lanes.
	pub cold_spawns: usize,
	/// Whether to run the no-adapter child.
	pub no_adapter: bool,
	/// `nvidia-smi` sampling interval in milliseconds; 0 turns the sampling off.
	pub smi_interval_ms: u64,
}

impl Default for GpuEconomicsParams {
	/// 1 Mi-point calls over 10,000 knots: 30 calls per fallback lane, 1–16 callers of 10 calls
	/// each on the GPU (`f64`) and the CPU, 5 cold children per mode, `nvidia-smi` every 100 ms.
	fn default() -> Self {
		Self { seed: DEFAULT_GPU_ECONOMICS_SEED, points: 1 << 20, knots: 10_000, calls: 30, lanes: Lane::ALL.to_vec(), concurrency: vec![1, 2, 4, 8, 16], calls_per_caller: 10, contention: vec![Lane::GpuF64, Lane::Cpu], cold_spawns: 5, no_adapter: true, smi_interval_ms: 100 }
	}
}

/// A reproducible GPU economics workload profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuEconomicsProfile {
	/// Human-readable profile name; each lane's result is named `<name>-<lane>`.
	pub name: String,
	/// The knobs.
	pub params: GpuEconomicsParams,
}

impl GpuEconomicsProfile {
	/// Build a profile, clamping every count to something runnable.
	#[must_use]
	pub fn new(name: impl Into<String>, params: GpuEconomicsParams) -> Self {
		let mut concurrency: Vec<usize> = params.concurrency.into_iter().map(|c| c.max(1)).collect();
		concurrency.dedup();
		let mut lanes = params.lanes;
		lanes.dedup();
		let mut contention = params.contention;
		contention.dedup();
		Self { name: name.into(), params: GpuEconomicsParams { points: params.points.max(2), knots: params.knots.max(2), calls: params.calls.max(1), calls_per_caller: params.calls_per_caller.max(1), concurrency, lanes, contention, ..params } }
	}
}

/// One cold-start child's measurements.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColdStartSample {
	/// `prewarm` or `lazy`.
	pub mode: String,
	/// From the parent's spawn to the child's `main` (exec, dynamic linking, runtime start).
	pub spawn_to_main_ns: u64,
	/// `prewarm_gpu`: adapter, device and kernel compilation. `None` for a `lazy` child.
	pub device_ns: Option<u64>,
	/// The first GPU call (in a `lazy` child this includes opening the device).
	pub first_call_ns: u64,
	/// The median of the warm calls after the first.
	pub warm_call_ns: u64,
	/// From the parent's spawn to the child holding its first GPU result.
	pub spawn_to_first_result_ns: u64,
	/// The adapter the child ran on.
	pub adapter: String,
}

/// What the child with every GPU driver hidden saw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoAdapterOutcome {
	/// No adapter was found, so the outcome below is the no-GPU path. `false` means hiding the
	/// drivers did not work on this machine and the lane demonstrates nothing.
	pub adapter_hidden: bool,
	/// The adapter `prewarm_gpu` found anyway, if any.
	pub adapter_seen: Option<String>,
	/// `prewarm_gpu`'s error.
	pub prewarm_error: Option<String>,
	/// How long `prewarm_gpu` took to give its answer.
	pub prewarm_ns: u64,
	/// A `Backend::Gpu` call's error (it must fail, not fall back).
	pub gpu_call_error: Option<String>,
	/// The backend a `Backend::Auto` call ran on.
	pub auto_backend: String,
	/// Whether the `Auto` call returned its full grid of finite values.
	pub auto_complete: bool,
}

/// `nvidia-smi` utilization for one GPU over one lane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuUtilization {
	/// `nvidia-smi`'s GPU index.
	pub gpu_index: u32,
	/// Samples taken during the lane.
	pub samples: usize,
	/// Mean `utilization.gpu` over the lane, in percent (device-wide, every process).
	pub mean_utilization_pct: f64,
	/// Highest `utilization.gpu` sample.
	pub max_utilization_pct: u32,
	/// Highest `memory.used` sample, in MiB (device-wide).
	pub max_memory_used_mib: u64,
	/// The same mean before any GPU work in this run (other processes' load).
	pub baseline_mean_utilization_pct: Option<f64>,
	/// The highest `memory.used` before any GPU work in this run.
	pub baseline_max_memory_used_mib: Option<u64>,
}

/// One lane of a GPU economics run (the report's `gpu_economics` block, schema v29).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuEconomicsPoint {
	/// `cold_start`, `no_adapter`, `fallback` or `contention`.
	pub lane: String,
	/// The requested backend: `cpu`, `parallel`, `gpu-f64`, `gpu-f32`, `auto`, or the cold
	/// child's mode (`prewarm`, `lazy`).
	pub backend: String,
	/// The backend that produced the values (what `Auto` picked), when one call ran.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub backend_used: Option<String>,
	/// Concurrent callers (`contention` only).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub concurrency: Option<usize>,
	/// The largest absolute difference from the `Cpu` lane's output (`fallback` only).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub max_abs_diff_vs_cpu: Option<f64>,
	/// Every cold child, in spawn order (`cold_start` only).
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub cold_starts: Vec<ColdStartSample>,
	/// The no-adapter child's outcome (`no_adapter` only).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub no_adapter: Option<NoAdapterOutcome>,
	/// `nvidia-smi` samples over the lane, per GPU; empty without `nvidia-smi` and for CPU lanes.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub utilization: Vec<GpuUtilization>,
	/// This process's device memory per `nvidia-smi --query-compute-apps` at the lane's end,
	/// in MiB, when it lists the process.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub process_gpu_memory_mib: Option<u64>,
}

/// One call's input: knot instants and values.
type Knots = (Vec<DateTime<Utc>>, Vec<f64>);

/// The fixed grid start (2020-01-01T00:00:00Z).
fn anchor() -> DateTime<Utc> {
	Utc.timestamp_opt(1_577_836_800, 0).single().expect("valid fixed epoch anchor")
}

/// The grid's last instant for `points` one-second points.
fn grid_end(points: usize) -> DateTime<Utc> {
	anchor() + Duration::seconds(i64::try_from(points - 1).unwrap_or(i64::MAX))
}

/// Seeded irregular knots spread over a one-second grid of `points` points.
fn knots(seed: u64, count: usize, points: usize) -> Knots {
	let mut rng = ChaCha8Rng::seed_from_u64(seed);
	let span = i64::try_from(points - 1).unwrap_or(i64::MAX);
	let mut offsets: Vec<i64> = (0..count).map(|_| rng.random_range(0..=span)).collect();
	offsets.extend([0, span]);
	offsets.sort_unstable();
	offsets.dedup();
	let values = offsets.iter().map(|&o| 20.0_f64.mul_add((o as f64 / 600.0).sin(), 50.0) + rng.random_range(-1.0..=1.0)).collect();
	(offsets.into_iter().map(|o| anchor() + Duration::seconds(o)).collect(), values)
}

/// Elapsed nanoseconds since `since`, saturated into a `u64`.
fn span_ns(since: Instant) -> u64 {
	crate::num::u128_to_u64(since.elapsed().as_nanos())
}

/// Nanoseconds since the Unix epoch (the wall clock parent and child share).
fn unix_ns() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| crate::num::u128_to_u64(d.as_nanos()))
}

/// The median of `samples` (0 when empty).
fn median(samples: &[u64]) -> u64 {
	let mut sorted = samples.to_vec();
	sorted.sort_unstable();
	sorted.get(sorted.len() / 2).copied().unwrap_or(0)
}

/// Output points per second over `ns`.
fn throughput(points: usize, ns: u64) -> f64 {
	if ns > 0 {
		points as f64 / (ns as f64 / 1e9)
	} else {
		0.0
	}
}

// ---------------------------------------------------------------------------------------------
// nvidia-smi sampling
// ---------------------------------------------------------------------------------------------

/// One `nvidia-smi --query-gpu=index,utilization.gpu,memory.used` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmiSample {
	/// GPU index.
	pub index: u32,
	/// `utilization.gpu`, percent.
	pub utilization_pct: u32,
	/// `memory.used`, MiB.
	pub memory_used_mib: u64,
}

/// Parse one `csv,noheader,nounits` row (`"0, 37, 1234"`); `None` for anything else (such
/// as `[N/A]` fields).
#[must_use]
pub fn parse_smi_line(line: &str) -> Option<SmiSample> {
	let mut fields = line.split(',').map(str::trim);
	let index = fields.next()?.parse().ok()?;
	let utilization_pct = fields.next()?.parse().ok()?;
	let memory_used_mib = fields.next()?.parse().ok()?;
	fields.next().is_none().then_some(SmiSample { index, utilization_pct, memory_used_mib })
}

/// A running `nvidia-smi --loop-ms` sampler.
pub struct SmiSampler {
	child: Child,
	reader: JoinHandle<Vec<SmiSample>>,
}

impl SmiSampler {
	/// Start sampling every `interval_ms`; `None` when `interval_ms` is 0 or `nvidia-smi`
	/// cannot be started.
	#[must_use]
	pub fn start(interval_ms: u64) -> Option<Self> {
		if interval_ms == 0 {
			return None;
		}
		let mut child = Command::new("nvidia-smi").args(["--query-gpu=index,utilization.gpu,memory.used", "--format=csv,noheader,nounits", &format!("--loop-ms={interval_ms}")]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
		let stdout = child.stdout.take()?;
		let reader = std::thread::spawn(move || BufReader::new(stdout).lines().map_while(Result::ok).filter_map(|l| parse_smi_line(&l)).collect());
		Some(Self { child, reader })
	}

	/// Stop sampling and return the samples taken.
	#[must_use]
	pub fn stop(mut self) -> Vec<SmiSample> {
		let _ = self.child.kill();
		let _ = self.child.wait();
		self.reader.join().unwrap_or_default()
	}
}

/// Summarize `samples` per GPU, beside the `baseline` taken before any GPU work.
#[must_use]
pub fn summarize_utilization(samples: &[SmiSample], baseline: &[SmiSample]) -> Vec<GpuUtilization> {
	let mut indices: Vec<u32> = samples.iter().map(|s| s.index).collect();
	indices.sort_unstable();
	indices.dedup();
	let mean = |of: &[&SmiSample]| (!of.is_empty()).then(|| of.iter().map(|s| f64::from(s.utilization_pct)).sum::<f64>() / of.len() as f64);
	indices.into_iter()
		.map(|index| {
			let lane: Vec<&SmiSample> = samples.iter().filter(|s| s.index == index).collect();
			let base: Vec<&SmiSample> = baseline.iter().filter(|s| s.index == index).collect();
			GpuUtilization { gpu_index: index, samples: lane.len(), mean_utilization_pct: mean(&lane).unwrap_or(0.0), max_utilization_pct: lane.iter().map(|s| s.utilization_pct).max().unwrap_or(0), max_memory_used_mib: lane.iter().map(|s| s.memory_used_mib).max().unwrap_or(0), baseline_mean_utilization_pct: mean(&base), baseline_max_memory_used_mib: base.iter().map(|s| s.memory_used_mib).max() }
		})
		.collect()
}

/// Sample the idle baseline for ten intervals (empty when sampling is off or unavailable).
#[must_use]
pub fn sample_baseline(params: &GpuEconomicsParams) -> Vec<SmiSample> {
	SmiSampler::start(params.smi_interval_ms).map_or_else(Vec::new, |s| {
		std::thread::sleep(StdDuration::from_millis(params.smi_interval_ms.saturating_mul(10)));
		s.stop()
	})
}

/// This process's device memory in MiB per `nvidia-smi --query-compute-apps`, if listed.
#[must_use]
pub fn process_gpu_memory_mib() -> Option<u64> {
	let out = Command::new("nvidia-smi").args(["--query-compute-apps=pid,used_memory", "--format=csv,noheader,nounits"]).stderr(Stdio::null()).output().ok()?;
	let pid = std::process::id().to_string();
	String::from_utf8_lossy(&out.stdout).lines().find_map(|l| {
		let (p, mib) = l.split_once(',')?;
		(p.trim() == pid).then(|| mib.trim().parse().ok()).flatten()
	})
}

// ---------------------------------------------------------------------------------------------
// Children
// ---------------------------------------------------------------------------------------------

/// What a cold-start child prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ChildCold {
	main_unix_ns: u64,
	first_result_unix_ns: u64,
	device_ns: Option<u64>,
	first_call_ns: u64,
	warm_ns: Vec<u64>,
	adapter: String,
}

/// Print the child's one result line.
fn say<T: Serialize>(value: &T) -> anyhow::Result<()> {
	use std::io::Write as _;
	let mut out = std::io::stdout().lock();
	writeln!(out, "{CHILD_LINE}{}", serde_json::to_string(value)?)?;
	out.flush()?;
	Ok(())
}

/// The child's entry point (`weft-bench __gpu-economics-child …`):
/// `cold <prewarm|lazy> POINTS KNOTS SEED <gpu-f64|gpu-f32>` or `no-adapter POINTS KNOTS SEED`.
///
/// # Errors
///
/// Bad arguments, or (cold) a GPU that does not start or a call that fails.
pub fn child(args: &[String]) -> anyhow::Result<()> {
	let main_unix_ns = unix_ns();
	let usage = || anyhow::anyhow!("{CHILD_COMMAND}: expected `cold <prewarm|lazy> POINTS KNOTS SEED <gpu-f64|gpu-f32>` or `no-adapter POINTS KNOTS SEED`");
	let num = |i: usize| args.get(i).and_then(|v| v.parse::<u64>().ok()).ok_or_else(usage);
	match args.first().map(String::as_str) {
		Some("cold") => {
			let prewarm = match args.get(1).map(String::as_str) {
				Some("prewarm") => true,
				Some("lazy") => false,
				_ => return Err(usage()),
			};
			let (points, knot_count, seed) = (usize::try_from(num(2)?)?, usize::try_from(num(3)?)?, num(4)?);
			let lane = args.get(5).and_then(|l| Lane::parse(l)).filter(|l| matches!(l, Lane::GpuF64 | Lane::GpuF32)).ok_or_else(usage)?;
			let (timestamps, values) = knots(seed, knot_count, points);
			let device_ns = if prewarm {
				let t0 = Instant::now();
				splimes::prewarm_gpu().map_err(|e| anyhow::anyhow!("no GPU: {e}"))?;
				Some(span_ns(t0))
			} else {
				None
			};
			let interpolator = lane.interpolator();
			let call = || -> anyhow::Result<u64> {
				let t0 = Instant::now();
				let series = interpolator.run_f64(&timestamps, &values, anchor(), grid_end(points)).map_err(|e| anyhow::anyhow!("GPU call failed: {e}"))?;
				let ns = span_ns(t0);
				anyhow::ensure!(series.len() == points && series.values().iter().all(|v| v.is_finite()), "the GPU call returned an incomplete or non-finite grid");
				Ok(ns)
			};
			let first_call_ns = call()?;
			let first_result_unix_ns = unix_ns();
			let warm_ns = (0..3).map(|_| call()).collect::<anyhow::Result<Vec<_>>>()?;
			let adapter = splimes::gpu_info().map(|i| i.name).unwrap_or_default();
			say(&ChildCold { main_unix_ns, first_result_unix_ns, device_ns, first_call_ns, warm_ns, adapter })
		}
		Some("no-adapter") => {
			let (points, knot_count, seed) = (usize::try_from(num(1)?)?, usize::try_from(num(2)?)?, num(3)?);
			let (timestamps, values) = knots(seed, knot_count, points);
			let t0 = Instant::now();
			let prewarm = splimes::prewarm_gpu();
			let prewarm_ns = span_ns(t0);
			let gpu = Lane::GpuF64.interpolator().run_f64(&timestamps, &values, anchor(), grid_end(points));
			let auto = Lane::Auto.interpolator().run_f64(&timestamps, &values, anchor(), grid_end(points));
			let (auto_backend, auto_complete) = match &auto {
				Ok(s) => (s.backend().to_string(), s.len() == points && s.values().iter().all(|v| v.is_finite())),
				Err(e) => (format!("error: {e}"), false),
			};
			say(&NoAdapterOutcome { adapter_hidden: prewarm.is_err(), adapter_seen: prewarm.as_ref().ok().map(|i| i.name.clone()), prewarm_error: prewarm.err().map(|e| e.to_string()), prewarm_ns, gpu_call_error: gpu.err().map(|e| e.to_string()), auto_backend, auto_complete })
		}
		_ => Err(usage()),
	}
}

/// Run one child and parse its result line.
fn run_child<T: for<'de> Deserialize<'de>>(exe: &Path, args: &[String], hide_drivers: bool) -> anyhow::Result<(u64, T)> {
	let mut command = Command::new(exe);
	command.arg(CHILD_COMMAND).args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
	if hide_drivers {
		command.env("VK_ICD_FILENAMES", NO_DRIVERS).env("VK_DRIVER_FILES", NO_DRIVERS).env_remove("VK_ADD_DRIVER_FILES").env("__EGL_VENDOR_LIBRARY_FILENAMES", NO_DRIVERS);
	}
	let spawn_unix_ns = unix_ns();
	let out = command.output().map_err(|e| anyhow::anyhow!("cannot start {} {CHILD_COMMAND}: {e}", exe.display()))?;
	let stdout = String::from_utf8_lossy(&out.stdout);
	let line = stdout.lines().find_map(|l| l.strip_prefix(CHILD_LINE));
	match line {
		Some(json) if out.status.success() => Ok((spawn_unix_ns, serde_json::from_str(json)?)),
		_ => anyhow::bail!("the child {} failed ({}): {}", args.join(" "), out.status, String::from_utf8_lossy(&out.stderr).trim()),
	}
}

// ---------------------------------------------------------------------------------------------
// Lanes
// ---------------------------------------------------------------------------------------------

/// Assemble one lane's [`BenchResult`].
fn result(profile: &GpuEconomicsProfile, suffix: &str, samples_ns: Vec<u64>, measured_ns: u64, output_points: usize, expected: usize, complete: bool, finite: bool, point: GpuEconomicsPoint, started: Instant) -> BenchResult {
	let p = &profile.params;
	BenchResult { schema_version: SCHEMA_VERSION, profile: format!("{}-{suffix}", profile.name), adapter: "weftdb".to_string(), workload: WORKLOAD_GPU_ECONOMICS.to_string(), reps: samples_ns.len(), dataset: DatasetMeta { input_points: p.knots, output_points, irregular: true, missingness_fraction: 0.0, seed: p.seed, signal_shape: None }, latency: LatencyStats::from_samples(&samples_ns), latency_ci: None, throughput_points_per_sec: if point.lane == "cold_start" || point.lane == "no_adapter" { 0.0 } else { throughput(output_points, measured_ns) }, timing: TimingBreakdown { dataset_generation_ns: 0, measured_ns, end_to_end_ns: span_ns(started) }, correctness: CorrectnessReport { output_count_ok: complete && output_points == expected, expected_output_points: expected, actual_output_points: output_points, values_finite: finite, result_sha256: None }, accuracy: None, storage: None, cold_warm: ColdWarm::from_samples(&samples_ns), gpu_pool: None, adapter_info: None, growth: None, online: None, compressed_query: None, analytics: None, samples_ns, between_run: None, gpu_economics: Some(point) }
}

/// A lane point with only the lane and backend set.
fn point(lane: &str, backend: &str) -> GpuEconomicsPoint {
	GpuEconomicsPoint { lane: lane.to_string(), backend: backend.to_string(), backend_used: None, concurrency: None, max_abs_diff_vs_cpu: None, cold_starts: Vec::new(), no_adapter: None, utilization: Vec::new(), process_gpu_memory_mib: None }
}

/// The child-process lanes: cold start (`prewarm`, then `lazy`) and no adapter. Run these
/// before this process starts the GPU. A mode whose children fail (no GPU) is reported on
/// stderr and skipped.
///
/// # Errors
///
/// Whatever `on_lane` returns.
pub fn run_child_lanes(profile: &GpuEconomicsProfile, exe: &Path, mut on_lane: impl FnMut(BenchResult) -> anyhow::Result<()>) -> anyhow::Result<()> {
	let p = &profile.params;
	if p.cold_spawns > 0 {
		for mode in ["prewarm", "lazy"] {
			let started = Instant::now();
			let args: Vec<String> = ["cold", mode, &p.points.to_string(), &p.knots.to_string(), &p.seed.to_string(), Lane::GpuF64.as_str()].map(str::to_string).to_vec();
			let mut samples = Vec::new();
			for _ in 0..p.cold_spawns {
				match run_child::<ChildCold>(exe, &args, false) {
					Ok((spawn, c)) => samples.push(ColdStartSample { mode: mode.to_string(), spawn_to_main_ns: c.main_unix_ns.saturating_sub(spawn), device_ns: c.device_ns, first_call_ns: c.first_call_ns, warm_call_ns: median(&c.warm_ns), spawn_to_first_result_ns: c.first_result_unix_ns.saturating_sub(spawn), adapter: c.adapter }),
					Err(e) => {
						eprintln!("gpu-economics: cold start ({mode}) skipped: {e:#}");
						samples.clear();
						break;
					}
				}
			}
			if samples.is_empty() {
				continue;
			}
			let latency: Vec<u64> = samples.iter().map(|s| s.spawn_to_first_result_ns).collect();
			let n = samples.len();
			let measured = latency.iter().copied().fold(0_u64, u64::saturating_add);
			on_lane(result(profile, &format!("cold-{mode}"), latency, measured, n * p.points, n * p.points, true, true, GpuEconomicsPoint { cold_starts: samples, ..point("cold_start", mode) }, started))?;
		}
	}
	if p.no_adapter {
		let started = Instant::now();
		let args: Vec<String> = ["no-adapter".to_string(), p.points.to_string(), p.knots.to_string(), p.seed.to_string()].to_vec();
		match run_child::<NoAdapterOutcome>(exe, &args, true) {
			Ok((_, outcome)) => {
				if !outcome.adapter_hidden {
					eprintln!("gpu-economics: hiding the GPU drivers did not work here ({} was still found); the no-adapter lane demonstrates nothing", outcome.adapter_seen.as_deref().unwrap_or("an adapter"));
				}
				let gate = outcome.auto_complete && (!outcome.adapter_hidden || outcome.gpu_call_error.is_some());
				let ns = outcome.prewarm_ns;
				let used = outcome.auto_backend.clone();
				on_lane(result(profile, "no-adapter", vec![ns], ns, if gate { p.points } else { 0 }, p.points, gate, outcome.auto_complete, GpuEconomicsPoint { backend_used: Some(used), no_adapter: Some(outcome), ..point("no_adapter", "auto") }, started))?;
			}
			Err(e) => eprintln!("gpu-economics: no-adapter child failed: {e:#}"),
		}
	}
	Ok(())
}

/// Start the GPU in this process and describe it; `Err` (no GPU, or a CPU/software adapter)
/// means the GPU lanes are skipped and only the CPU lanes run.
///
/// # Errors
///
/// No GPU starts, or it is a CPU/software adapter.
pub fn start_gpu() -> anyhow::Result<EngineMetadata> {
	let info = splimes::prewarm_gpu().map_err(|e| anyhow::anyhow!("no GPU: {e}"))?;
	anyhow::ensure!(!is_software_adapter(&info.device_type, &info.name), "{} is a CPU/software adapter", info.name);
	let gpu = format!("{} ({}, {}; f64 shaders: {})", info.name, info.api, info.device_type, if info.supports_f64 { "yes" } else { "no" });
	Ok(EngineMetadata::new(CalibrationStatus::Disabled, Some(gpu), Some("gpu-economics workload: backends forced per lane; Auto uses the default thresholds".to_string()), splimes::auto_thresholds()).with_gpu_driver(&info.driver))
}

/// The in-process lanes: fallback, then contention. GPU lanes run only when `gpu` is true
/// (the GPU started, [`start_gpu`]); `baseline` is the idle `nvidia-smi` sample.
///
/// # Errors
///
/// A call fails, or whatever `on_lane` returns.
pub fn run_in_process_lanes(profile: &GpuEconomicsProfile, gpu: bool, baseline: &[SmiSample], mut on_lane: impl FnMut(BenchResult) -> anyhow::Result<()>) -> anyhow::Result<()> {
	let p = &profile.params;
	let (timestamps, values) = knots(p.seed, p.knots, p.points);
	let end = grid_end(p.points);
	let skip = |lane: Lane| {
		let skip = lane.needs_gpu() && !gpu;
		if skip {
			eprintln!("gpu-economics: lane {} skipped: no GPU", lane.as_str());
		}
		skip
	};

	let mut cpu_values: Option<Vec<f64>> = None;
	for &lane in &p.lanes {
		if skip(lane) {
			continue;
		}
		let started = Instant::now();
		let sampler = if lane.needs_gpu() { SmiSampler::start(p.smi_interval_ms) } else { None };
		let interpolator = lane.interpolator();
		let (mut samples, mut complete, mut finite, mut out_points) = (Vec::with_capacity(p.calls), true, true, 0_usize);
		let (mut last, mut used) = (Vec::new(), String::new());
		for _ in 0..p.calls {
			let t0 = Instant::now();
			let series = interpolator.run_f64(&timestamps, &values, anchor(), end).map_err(|e| anyhow::anyhow!("{} call of {} points failed: {e}", lane.as_str(), p.points))?;
			samples.push(span_ns(t0));
			complete &= series.len() == p.points;
			finite &= series.values().iter().all(|v| v.is_finite());
			out_points += series.len();
			used = series.backend().to_string();
			last = series.values().to_vec();
		}
		let utilization = sampler.map(|s| summarize_utilization(&s.stop(), baseline)).unwrap_or_default();
		let diff = cpu_values.as_ref().filter(|c| c.len() == last.len()).map(|c| c.iter().zip(&last).map(|(a, b)| (a - b).abs()).fold(0.0_f64, f64::max));
		if lane == Lane::Cpu {
			cpu_values = Some(last);
		}
		let measured = samples.iter().copied().fold(0_u64, u64::saturating_add);
		let pt = GpuEconomicsPoint { backend_used: Some(used), max_abs_diff_vs_cpu: diff, utilization, process_gpu_memory_mib: if lane.needs_gpu() { process_gpu_memory_mib() } else { None }, ..point("fallback", lane.as_str()) };
		on_lane(result(profile, &format!("fallback-{}", lane.as_str()), samples, measured, out_points, p.points * p.calls, complete, finite, pt, started))?;
	}

	let timestamps = Arc::new(timestamps);
	let values = Arc::new(values);
	for &lane in &p.contention {
		if skip(lane) {
			continue;
		}
		for &callers in &p.concurrency {
			let started = Instant::now();
			let sampler = if lane.needs_gpu() { SmiSampler::start(p.smi_interval_ms) } else { None };
			let barrier = Arc::new(Barrier::new(callers + 1));
			let handles: Vec<JoinHandle<anyhow::Result<(Vec<u64>, bool, usize)>>> = (0..callers)
				.map(|_| {
					let (timestamps, values, barrier, calls, points) = (Arc::clone(&timestamps), Arc::clone(&values), Arc::clone(&barrier), p.calls_per_caller, p.points);
					std::thread::spawn(move || {
						let interpolator = lane.interpolator();
						let (mut samples, mut ok, mut out) = (Vec::with_capacity(calls), true, 0_usize);
						barrier.wait();
						for _ in 0..calls {
							let t0 = Instant::now();
							let series = interpolator.run_f64(&timestamps, &values, anchor(), end).map_err(|e| anyhow::anyhow!("{} call failed under contention: {e}", lane.as_str()))?;
							samples.push(span_ns(t0));
							ok &= series.len() == points && series.values().iter().all(|v| v.is_finite());
							out += series.len();
						}
						Ok((samples, ok, out))
					})
				})
				.collect();
			barrier.wait();
			let wall_start = Instant::now();
			let (mut samples, mut ok, mut out_points) = (Vec::new(), true, 0_usize);
			for h in handles {
				let (s, o, n) = h.join().map_err(|_| anyhow::anyhow!("a contention caller panicked"))??;
				samples.extend(s);
				ok &= o;
				out_points += n;
			}
			let wall_ns = span_ns(wall_start);
			let utilization = sampler.map(|s| summarize_utilization(&s.stop(), baseline)).unwrap_or_default();
			let pt = GpuEconomicsPoint { concurrency: Some(callers), utilization, process_gpu_memory_mib: if lane.needs_gpu() { process_gpu_memory_mib() } else { None }, ..point("contention", lane.as_str()) };
			on_lane(result(profile, &format!("contention-{}-c{callers}", lane.as_str()), samples, wall_ns, out_points, p.points * p.calls_per_caller * callers, ok, ok, pt, started))?;
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn smoke() -> GpuEconomicsProfile {
		GpuEconomicsProfile::new("ge-test", GpuEconomicsParams { points: 4_096, knots: 64, calls: 2, concurrency: vec![1, 2], calls_per_caller: 2, cold_spawns: 0, no_adapter: false, smi_interval_ms: 0, ..GpuEconomicsParams::default() })
	}

	#[test]
	fn profiles_clamp_their_knobs() {
		let p = GpuEconomicsProfile::new("ge", GpuEconomicsParams { points: 0, knots: 1, calls: 0, calls_per_caller: 0, concurrency: vec![0, 1, 1, 4], ..GpuEconomicsParams::default() }).params;
		assert_eq!((p.points, p.knots, p.calls, p.calls_per_caller), (2, 2, 1, 1));
		assert_eq!(p.concurrency, vec![1, 4]);
	}

	#[test]
	fn lanes_round_trip_their_names() {
		for lane in Lane::ALL {
			assert_eq!(Lane::parse(lane.as_str()), Some(lane));
		}
		assert_eq!(Lane::parse("tpu"), None);
		assert!(!Lane::Cpu.needs_gpu() && Lane::Auto.needs_gpu());
	}

	#[test]
	fn smi_rows_parse_and_summarize_per_gpu() {
		assert_eq!(parse_smi_line("0, 37, 1234"), Some(SmiSample { index: 0, utilization_pct: 37, memory_used_mib: 1_234 }));
		assert_eq!(parse_smi_line("0, [N/A], 1234"), None);
		assert_eq!(parse_smi_line("0, 1, 2, 3"), None);
		let s = |index, utilization_pct, memory_used_mib| SmiSample { index, utilization_pct, memory_used_mib };
		let summary = summarize_utilization(&[s(0, 50, 100), s(0, 100, 300), s(1, 10, 5)], &[s(0, 20, 90)]);
		assert_eq!(summary.len(), 2);
		assert_eq!((summary[0].samples, summary[0].max_utilization_pct, summary[0].max_memory_used_mib), (2, 100, 300));
		assert!((summary[0].mean_utilization_pct - 75.0).abs() < 1e-9);
		assert_eq!((summary[0].baseline_mean_utilization_pct, summary[0].baseline_max_memory_used_mib), (Some(20.0), Some(90)));
		assert_eq!((summary[1].baseline_mean_utilization_pct, summary[1].baseline_max_memory_used_mib), (None, None));
	}

	#[test]
	fn the_child_rejects_bad_arguments() {
		let args = |a: &[&str]| a.iter().map(ToString::to_string).collect::<Vec<_>>();
		assert!(child(&args(&[])).is_err());
		assert!(child(&args(&["cold", "warm", "10", "4", "1", "gpu-f64"])).is_err());
		assert!(child(&args(&["cold", "lazy", "10", "4", "1", "cpu"])).is_err());
		assert!(child(&args(&["no-adapter", "x"])).is_err());
	}

	#[test]
	fn the_cpu_lanes_run_without_a_gpu_and_pass() {
		let profile = GpuEconomicsProfile { params: GpuEconomicsParams { lanes: vec![Lane::Cpu, Lane::Parallel], contention: vec![Lane::Cpu], ..smoke().params }, ..smoke() };
		let mut results = Vec::new();
		run_in_process_lanes(&profile, false, &[], |r| {
			results.push(r);
			Ok(())
		})
		.expect("runs");
		let names: Vec<&str> = results.iter().map(|r| r.profile.as_str()).collect();
		assert_eq!(names, ["ge-test-fallback-cpu", "ge-test-fallback-parallel", "ge-test-contention-cpu-c1", "ge-test-contention-cpu-c2"]);
		assert!(results.iter().all(|r| r.correctness.passed()), "{:?}", results.iter().map(|r| &r.correctness).collect::<Vec<_>>());
		let parallel = results[1].gpu_economics.as_ref().expect("point");
		assert!(parallel.max_abs_diff_vs_cpu.expect("diffed against cpu") < 1e-9);
		assert_eq!(results[3].reps, 4, "two callers, two calls each");
		assert_eq!(results[3].gpu_economics.as_ref().and_then(|p| p.concurrency), Some(2));
	}

	#[test]
	fn gpu_lanes_are_skipped_without_a_gpu() {
		let profile = GpuEconomicsProfile { params: GpuEconomicsParams { lanes: vec![Lane::GpuF64, Lane::Auto], contention: vec![Lane::GpuF32], ..smoke().params }, ..smoke() };
		let mut n = 0;
		run_in_process_lanes(&profile, false, &[], |_| {
			n += 1;
			Ok(())
		})
		.expect("runs");
		assert_eq!(n, 0);
	}

	/// Runs on a hardware GPU only (skips otherwise, unless `WEFT_REQUIRE_GPU` is set).
	#[test]
	fn the_gpu_lanes_pass_on_a_real_gpu() {
		if let Err(e) = start_gpu() {
			assert!(std::env::var_os("WEFT_REQUIRE_GPU").is_none(), "WEFT_REQUIRE_GPU is set but: {e}");
			eprintln!("skipping: {e}");
			return;
		}
		let profile = GpuEconomicsProfile { params: GpuEconomicsParams { lanes: vec![Lane::Cpu, Lane::GpuF64, Lane::GpuF32], contention: vec![Lane::GpuF64], ..smoke().params }, ..smoke() };
		let mut results = Vec::new();
		run_in_process_lanes(&profile, true, &[], |r| {
			results.push(r);
			Ok(())
		})
		.expect("runs");
		assert_eq!(results.len(), 5);
		assert!(results.iter().all(|r| r.correctness.passed()));
		let f64_lane = results[1].gpu_economics.as_ref().expect("point");
		assert_eq!(f64_lane.backend_used.as_deref(), Some("gpu"));
		assert!(f64_lane.max_abs_diff_vs_cpu.expect("diffed") < 1e-6, "{f64_lane:?}");
	}
}
