//! End-to-end GPU interpolation benchmark (roadmap Phase 5, P5-e2e).
//!
//! Storage read → decode →
//! upload → kernel → readback → serialization, with p50/p95/p99 per stage, over output sizes
//! and backends, through the **library path** and the **HTTP route**.
//!
//! The corpus (seeded synthetic irregular knots, or one column of the UCI household-power file)
//! is sealed into a real [`SegmentStore`] for the library path, and, with `--http`, ingested
//! into a running `weft-server` over its ILP route for the HTTP path. Both paths then
//! reconstruct the same windows: `size` grid points at `resolution`, anchored one margin into
//! the corpus, with the same explicit `margin`.
//!
//! ## Library path stages (per timed rep)
//!
//! | stage | what is timed |
//! |---|---|
//! | `storage_read` | [`SegmentStore::read_time_range`] over `[start − margin, end + margin]`: index pruning, reading the `.weftseg` frames and decoding them into rows. WeftDB's public read API fuses frame I/O and frame decode, so they are one stage here. |
//! | `decode` | the rows → `f64` knot columns, exactly what `interpolate_range_f64` does before it calls splimes (`weft_reduce::decimal_to_f64`). |
//! | `compute` | splimes' `Interpolator::run_epoch` on the requested backend: host preparation, and on the GPU backends the upload, the kernel and the readback. |
//! | `upload`, `readback` | GPU backends only: a **transfer probe**, see below. |
//! | `kernel` | GPU backends only: **derived**, `compute − upload − readback` (saturating at 0). It also holds splimes' host-side preparation. |
//! | `serialize` | the result written as the route's JSON point list (`timestamp` RFC 3339, `value`, `kind`) into a counting sink, so no buffer of the whole body is held. |
//! | `total` | the sum of the timed stages `storage_read + decode + compute + serialize` (upload, readback and kernel are inside `compute`). |
//!
//! ### Why upload and readback are a probe
//!
//! splimes 1.3's public API does not time its own upload, dispatch and readback: the GPU
//! module is private and `Interpolation` carries no stage timings. The probe therefore opens
//! its own wgpu device **on the adapter splimes opened** (matched by name) and times, per
//! rep, exactly the transfers splimes' `run.rs` makes for the call's sizes: `queue.write_buffer`
//! of the knots and the grid times plus a submit-and-wait (the upload), and a
//! buffer-to-staging copy, `map_async`, wait and copy-out of the grid's values (the readback).
//! The byte model is `(2 · knots + grid) · scalar` up and `grid · scalar` down, `scalar` being
//! 8 bytes in `f64` and 4 in `f32`. Buffer creation is not timed (splimes pools its grid and
//! output buffers). The probe runs between the timed `compute` calls, never during one. The
//! report states all of this in `transfer_probe`, and the transfer share
//! (`(upload + readback) / total`, per sample) is reported explicitly per cell, because the
//! mmap GPU I/O item depends on it.
//!
//! ## HTTP path
//!
//! `GET /api/v1/storage/{aspect}/interpolate?start=&end=&resolution=&spline=&margin=` (JSON,
//! streamed since P5-5.3), one fresh connection per request (`Connection: close`, so connect is
//! in the sample). Timed per rep: time to the response's status line (`ttfb`, which on the
//! streamed route covers the server's read, decode and first chunk) and to the body's last
//! byte (`total`); `body = total − ttfb`. The body is read and discarded as it arrives; only
//! its last bytes are kept, to read `output_points` and `backend` (the backend the server's
//! `Auto` chose). The server's own stage split is not observable from the client: the library
//! path at the same size is the per-stage breakdown, and the difference of the two totals is
//! what the HTTP layer costs.
//!
//! ## Correctness gates
//!
//! Library: every cell's output has exactly `size` points, all finite, and on every backend
//! other than `cpu` the largest absolute difference from the `cpu` output at that size is
//! recorded (computed once per size, untimed). A requested `gpu` that ran elsewhere fails the
//! cell. HTTP: status 200 and `output_points == size`.
//!
//! The report (`E2eReport`) carries [`SCHEMA_VERSION`] (v31 introduced it); no timing is
//! claimed by this module: numbers come only from a run.

use std::{
	fmt::Write as _, io::{BufRead, BufReader, Read, Write}, net::{TcpStream, ToSocketAddrs}, path::{Path, PathBuf}, str::FromStr, sync::mpsc, time::{Duration, Instant}
};

use anyhow::{bail, ensure, Context};
use bigdecimal::BigDecimal;
use chrono::{SecondsFormat, Utc};
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};
use splimes::{Backend, Interpolation, Interpolator, PointKind, Precision, Resolution, Spline, TickUnit};
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weftdb::SegmentStore;

use crate::{
	num::{f64_to_usize, i64_to_f64, u128_to_u64, u64_to_f64, usize_to_f64}, schema::SCHEMA_VERSION, stats::LatencyStats
};

/// The report's `kind`, so a reader can tell it from a `BenchReport`.
pub const E2E_REPORT_KIND: &str = "e2e_gpu_interpolation";

/// The aspect the library path seals into, and the default HTTP aspect.
pub const E2E_ASPECT: &str = "e2e_interp";

/// The ILP measurement and field the HTTP ingest writes.
const ILP_MEASUREMENT: &str = "e2e";
const ILP_FIELD: &str = "value";

/// Rows per sealed segment on the library path, and lines per ILP POST on the HTTP path.
const SEAL_BATCH_ROWS: usize = 65_536;
const ILP_BATCH_LINES: usize = 50_000;

/// Default seed of the synthetic corpus.
pub const DEFAULT_E2E_SEED: u64 = 0xE2E0_5EED;

/// How long one probe transfer may wait for the GPU.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Bytes of a response body kept to read its trailing fields.
const TAIL_BYTES: usize = 4096;

/// A backend lane of the library path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendLane {
	/// `Backend::Cpu`.
	Cpu,
	/// `Backend::Parallel`.
	Parallel,
	/// `Backend::Gpu`, `f64` kernel.
	Gpu,
	/// `Backend::Gpu`, `f32` kernel.
	GpuF32,
	/// `Backend::Auto` (after the GPU was started).
	Auto,
}

impl BackendLane {
	/// Every lane, in report order.
	pub const ALL: [Self; 5] = [Self::Cpu, Self::Parallel, Self::Gpu, Self::GpuF32, Self::Auto];

	/// The CLI token.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Cpu => "cpu",
			Self::Parallel => "parallel",
			Self::Gpu => "gpu",
			Self::GpuF32 => "gpu-f32",
			Self::Auto => "auto",
		}
	}

	/// Parse a CLI token.
	#[must_use]
	pub fn parse(token: &str) -> Option<Self> {
		Self::ALL.into_iter().find(|lane| lane.as_str() == token)
	}

	const fn backend(self) -> Backend {
		match self {
			Self::Cpu => Backend::Cpu,
			Self::Parallel => Backend::Parallel,
			Self::Gpu | Self::GpuF32 => Backend::Gpu,
			Self::Auto => Backend::Auto,
		}
	}

	const fn precision(self) -> Precision {
		match self {
			Self::GpuF32 => Precision::F32,
			_ => Precision::F64,
		}
	}

	/// Whether the lane needs the GPU started.
	#[must_use]
	pub const fn needs_gpu(self) -> bool {
		matches!(self, Self::Gpu | Self::GpuF32)
	}
}

/// Parse a count with an optional `k` / `M` / `G` suffix (`10k`, `1M`).
///
/// # Errors
///
/// Returns an error for an empty, zero or unparseable count.
pub fn parse_count(token: &str) -> Result<usize, String> {
	let token = token.trim();
	let (digits, scale) = match token.chars().last() {
		Some('k' | 'K') => (&token[..token.len() - 1], 1_000),
		Some('m' | 'M') => (&token[..token.len() - 1], 1_000_000),
		Some('g' | 'G') => (&token[..token.len() - 1], 1_000_000_000),
		_ => (token, 1),
	};
	let n: usize = digits.parse().map_err(|_| format!("{token:?} is not a count (e.g. 10k, 1M)"))?;
	let n = n.checked_mul(scale).ok_or_else(|| format!("{token:?} overflows"))?;
	if n == 0 {
		return Err(format!("{token:?}: a count must be positive"));
	}
	Ok(n)
}

/// Parse a grid resolution token (`nanoseconds` .. `days`; months and years have no fixed
/// step, so a window cannot be sized from them).
///
/// # Errors
///
/// Returns an error for any other token.
pub fn parse_resolution(token: &str) -> Result<Resolution, String> {
	[Resolution::Nanoseconds, Resolution::Microseconds, Resolution::Milliseconds, Resolution::Seconds, Resolution::Minutes, Resolution::Hours, Resolution::Days].into_iter().find(|r| r.as_str() == token).ok_or_else(|| format!("resolution {token:?}: one of nanoseconds, microseconds, milliseconds, seconds, minutes, hours, days"))
}

/// Where the corpus comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorpusSource {
	/// Seeded synthetic irregular knots, `knot_step_ns` apart on average (±40% jitter).
	Synthetic {
		/// RNG seed.
		seed: u64,
		/// Mean knot spacing in nanoseconds.
		knot_step_ns: i64,
	},
	/// One column of the UCI household-power file (one-minute cadence).
	Uci {
		/// The file.
		path: PathBuf,
		/// The column projected.
		column: String,
		/// Data rows examined at most.
		max_rows: usize,
	},
}

/// The HTTP path's target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpTarget {
	/// `host:port` of a running `weft-server` (plain HTTP, auth off: loopback, no keys file).
	pub authority: String,
	/// The aspect to ingest into and read from.
	pub aspect: String,
	/// Skip the declare and ingest (the aspect already holds this corpus).
	pub skip_ingest: bool,
	/// Connect / read timeout.
	pub timeout: Duration,
}

/// A run's settings.
#[derive(Debug, Clone, PartialEq)]
pub struct E2eConfig {
	/// The corpus.
	pub corpus: CorpusSource,
	/// Output grid sizes (points per request).
	pub sizes: Vec<usize>,
	/// Library-path backends. Empty skips the library path.
	pub backends: Vec<BackendLane>,
	/// Grid resolution.
	pub resolution: Resolution,
	/// Spline.
	pub spline: Spline,
	/// Timed reps per cell.
	pub reps: usize,
	/// Untimed warm-up reps per cell.
	pub warmup: usize,
	/// Knot spacings of margin on each side of a window.
	pub margin_knots: i64,
	/// The HTTP path, if any.
	pub http: Option<HttpTarget>,
	/// Open the library path's store with memory-mapped range reads
	/// ([`weftdb::SegmentStoreOptions::mmap_reads`], roadmap Phase 5 memory-mapped GPU I/O), so
	/// `storage_read` decodes from mapped frames. Needs this crate's `mmap-reads` feature. The
	/// HTTP path's server decides for itself (`WEFT_SEGMENT_MMAP_READS` on a server built with
	/// its own `mmap-reads` feature).
	pub mmap_reads: bool,
}

impl E2eConfig {
	/// A smoke-size configuration (tiny sizes, two reps), for a quick check that every stage runs.
	#[must_use]
	pub fn smoke() -> Self {
		Self { corpus: CorpusSource::Synthetic { seed: DEFAULT_E2E_SEED, knot_step_ns: 100_000_000 }, sizes: vec![1_000, 10_000], backends: BackendLane::ALL.to_vec(), resolution: Resolution::Milliseconds, spline: Spline::Cubic, reps: 2, warmup: 1, margin_knots: 4, http: None, mmap_reads: false }
	}

	fn validate(&self) -> anyhow::Result<()> {
		ensure!(!self.sizes.is_empty(), "no sizes to run");
		ensure!(self.sizes.iter().all(|&s| s >= 2), "every size must be at least 2 points");
		ensure!(self.reps > 0, "reps must be positive");
		ensure!(self.margin_knots >= 1, "margin must be at least one knot spacing");
		ensure!(!self.backends.is_empty() || self.http.is_some(), "nothing to run: no library backends and no --http");
		ensure!(!self.mmap_reads || cfg!(feature = "mmap-reads"), "--mmap-reads needs weft-bench built with `--features mmap-reads`; without it the store would silently read buffered");
		Ok(())
	}
}

/// The corpus, as stored: nanosecond epochs and exact decimals, plus the `f64` image the
/// HTTP ingest writes.
#[derive(Debug, Clone)]
pub struct Corpus {
	/// What it is, for the report.
	pub info: CorpusInfo,
	/// Nanosecond epochs, strictly ascending.
	pub timestamps: Vec<i64>,
	/// Exact values, scale `info.decimal_scale`.
	pub values: Vec<BigDecimal>,
}

/// The corpus as the report records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorpusInfo {
	/// `synthetic` or `uci:<column>`.
	pub source: String,
	/// Seed (synthetic) or `None`.
	pub seed: Option<u64>,
	/// Knots stored.
	pub rows: usize,
	/// Mean spacing between knots (ns).
	pub mean_knot_step_ns: i64,
	/// Fractional digits of the stored `scaled_i64` values.
	pub decimal_scale: u8,
	/// First and last stored instant (ns epoch).
	pub first_ns: i64,
	/// Last stored instant (ns epoch).
	pub last_ns: i64,
}

/// 2020-01-01T00:00:00Z in nanoseconds: the synthetic corpus' first instant.
const SYNTHETIC_ANCHOR_NS: i64 = 1_577_836_800_000_000_000;

/// The window a size reconstructs: grid `[start, end]` in ns, margin in ns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Window {
	start: i64,
	end: i64,
	margin: i64,
}

const fn step_ns(resolution: Resolution) -> i64 {
	resolution.step_nanos()
}

/// The window of `size` points at `resolution`, one margin after the corpus' first knot.
fn window(info: &CorpusInfo, size: usize, resolution: Resolution, margin_knots: i64) -> anyhow::Result<Window> {
	let margin = info.mean_knot_step_ns.checked_mul(margin_knots).context("margin overflows")?;
	let start = info.first_ns.checked_add(margin).context("window start overflows")?;
	let width = i64::try_from(size - 1).ok().and_then(|n| n.checked_mul(step_ns(resolution))).context("window width overflows")?;
	let end = start.checked_add(width).context("window end overflows")?;
	ensure!(end.saturating_add(margin) <= info.last_ns, "the corpus ({} rows over {} ns) is too short for {size} points at {} with a {margin_knots}-knot margin; use a longer corpus or a coarser knot step", info.rows, info.last_ns - info.first_ns, resolution.as_str());
	Ok(Window { start, end, margin })
}

/// Build the synthetic corpus long enough for the largest window.
///
/// # Errors
///
/// Returns an error if the span overflows or the knot step is not positive.
pub fn synthetic_corpus(seed: u64, knot_step_ns: i64, largest: usize, resolution: Resolution, margin_knots: i64) -> anyhow::Result<Corpus> {
	ensure!(knot_step_ns > 0, "knot step must be positive");
	let width = i64::try_from(largest.saturating_sub(1)).ok().and_then(|n| n.checked_mul(step_ns(resolution))).context("the largest window overflows i64 nanoseconds")?;
	// Two margins, plus slack for the jitter at both ends.
	let span = width.checked_add(knot_step_ns.saturating_mul(2 * margin_knots + 4)).context("corpus span overflows")?;
	let rows = usize::try_from(span / knot_step_ns + 2).context("corpus row count")?;
	let mut rng = ChaCha8Rng::seed_from_u64(seed);
	let jitter = (knot_step_ns * 2) / 5;
	let mut timestamps = Vec::with_capacity(rows);
	let mut values = Vec::with_capacity(rows);
	for i in 0..rows {
		let base = SYNTHETIC_ANCHOR_NS + i64::try_from(i).unwrap_or(i64::MAX) * knot_step_ns;
		let t = if i == 0 || jitter == 0 { base } else { base + rng.random_range(-jitter..=jitter) };
		// Jitter is under half a step, so the knots stay strictly ascending.
		let x = i64_to_f64(t - SYNTHETIC_ANCHOR_NS) / 6.0e11;
		let v = 20.0_f64.mul_add(x.sin(), 50.0) + rng.random_range(-1.0..=1.0);
		timestamps.push(t);
		values.push(BigDecimal::from_str(&format!("{v:.3}")).context("synthetic value")?);
	}
	let (first_ns, last_ns) = (timestamps[0], timestamps[rows - 1]);
	let info = CorpusInfo { source: "synthetic".to_string(), seed: Some(seed), rows, mean_knot_step_ns: knot_step_ns, decimal_scale: 3, first_ns, last_ns };
	Ok(Corpus { info, timestamps, values })
}

/// Load the UCI corpus (epoch seconds → nanoseconds).
///
/// # Errors
///
/// As [`crate::uci::load_uci_corpus`], and for a corpus of fewer than two rows.
pub fn uci_corpus(path: &Path, column: &str, max_rows: usize) -> anyhow::Result<Corpus> {
	let uci = crate::uci::load_uci_corpus(path, column, max_rows)?;
	ensure!(uci.timestamps.len() >= 2, "the UCI corpus needs at least two rows");
	let timestamps: Vec<i64> = uci.timestamps.iter().map(|s| s.checked_mul(1_000_000_000).context("UCI instant overflows ns")).collect::<anyhow::Result<_>>()?;
	let rows = timestamps.len();
	let (first_ns, last_ns) = (timestamps[0], timestamps[rows - 1]);
	let mean = (last_ns - first_ns) / i64::try_from(rows - 1).unwrap_or(i64::MAX);
	let info = CorpusInfo { source: format!("uci:{}", uci.column), seed: None, rows, mean_knot_step_ns: mean.max(1), decimal_scale: uci.decimal_scale, first_ns, last_ns };
	Ok(Corpus { info, timestamps, values: uci.values })
}

/// One timed library-path rep. Every field is in nanoseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageSample {
	/// `read_time_range` (frame I/O + frame decode), ns.
	pub storage_read: u64,
	/// Rows → `f64` knot columns, ns.
	pub decode: u64,
	/// splimes `run_epoch` on the backend, ns.
	pub compute: u64,
	/// Probe upload (GPU lanes), ns, else `None`.
	pub upload: Option<u64>,
	/// Probe readback (GPU lanes), ns, else `None`.
	pub readback: Option<u64>,
	/// Derived `compute − upload − readback` (GPU lanes), ns, else `None`.
	pub kernel: Option<u64>,
	/// JSON serialization into a counting sink, ns.
	pub serialize: u64,
	/// `storage_read + decode + compute + serialize`, ns.
	pub total: u64,
}

impl StageSample {
	/// `(upload + readback) / total`, when the transfers were probed.
	#[must_use]
	pub fn transfer_share(&self) -> Option<f64> {
		let (up, down) = (self.upload?, self.readback?);
		(self.total > 0).then(|| u64_to_f64(up + down) / u64_to_f64(self.total))
	}

	const fn with_probe(mut self, upload: u64, readback: u64) -> Self {
		self.upload = Some(upload);
		self.readback = Some(readback);
		self.kernel = Some(self.compute.saturating_sub(upload).saturating_sub(readback));
		self
	}
}

/// Per-stage latency summaries of a cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageStats {
	/// `storage_read`.
	pub storage_read: LatencyStats,
	/// `decode`.
	pub decode: LatencyStats,
	/// `compute`.
	pub compute: LatencyStats,
	/// Probe `upload` (GPU lanes).
	pub upload: Option<LatencyStats>,
	/// Probe `readback` (GPU lanes).
	pub readback: Option<LatencyStats>,
	/// Derived `kernel` (GPU lanes).
	pub kernel: Option<LatencyStats>,
	/// `serialize`.
	pub serialize: LatencyStats,
	/// `total`.
	pub total: LatencyStats,
}

impl StageStats {
	/// Summarize samples.
	#[must_use]
	pub fn of(samples: &[StageSample]) -> Self {
		let col = |f: fn(&StageSample) -> u64| LatencyStats::from_samples(&samples.iter().map(f).collect::<Vec<_>>());
		let opt = |f: fn(&StageSample) -> Option<u64>| samples.iter().map(f).collect::<Option<Vec<u64>>>().filter(|v| !v.is_empty()).map(|v| LatencyStats::from_samples(&v));
		Self { storage_read: col(|s| s.storage_read), decode: col(|s| s.decode), compute: col(|s| s.compute), upload: opt(|s| s.upload), readback: opt(|s| s.readback), kernel: opt(|s| s.kernel), serialize: col(|s| s.serialize), total: col(|s| s.total) }
	}
}

/// The transfer share of a cell's samples: nearest-rank percentiles of the per-sample
/// `(upload + readback) / total`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TransferShare {
	/// Median share.
	pub p50: f64,
	/// 95th percentile share.
	pub p95: f64,
	/// 99th percentile share.
	pub p99: f64,
}

impl TransferShare {
	/// `None` unless every sample was probed.
	#[must_use]
	pub fn of(samples: &[StageSample]) -> Option<Self> {
		let mut shares = samples.iter().map(StageSample::transfer_share).collect::<Option<Vec<f64>>>()?;
		if shares.is_empty() {
			return None;
		}
		shares.sort_by(f64::total_cmp);
		let rank = |q: f64| shares[f64_to_usize((q * usize_to_f64(shares.len())).ceil()).clamp(1, shares.len()) - 1];
		Some(Self { p50: rank(0.50), p95: rank(0.95), p99: rank(0.99) })
	}
}

/// A cell's correctness gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CellCorrectness {
	/// Points expected (`size`).
	pub expected_output_points: usize,
	/// Points returned.
	pub actual_output_points: usize,
	/// Every value finite.
	pub values_finite: bool,
	/// Largest |value − cpu value| at this size (not on the `cpu` lane itself).
	pub max_abs_diff_vs_cpu: Option<f64>,
	/// The requested backend ran (a requested `gpu` that ran elsewhere fails).
	pub backend_as_requested: bool,
	/// The gate.
	pub ok: bool,
}

/// One library-path cell: one size on one backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryCell {
	/// Output grid points.
	pub size: usize,
	/// Requested lane.
	pub lane: BackendLane,
	/// The backend splimes reports it ran on.
	pub backend_ran: String,
	/// The kernel precision splimes reports.
	pub precision: String,
	/// Knots read for the window (non-null rows in `[start − margin, end + margin]`).
	pub knots: usize,
	/// Bytes the transfer probe uploaded / read back per rep (GPU lanes).
	pub probe_bytes: Option<(u64, u64)>,
	/// Serialized JSON bytes.
	pub serialized_bytes: u64,
	/// Per-stage summaries.
	pub stages: StageStats,
	/// `(upload + readback) / total` (GPU lanes).
	pub transfer_share: Option<TransferShare>,
	/// The gate.
	pub correctness: CellCorrectness,
	/// The raw samples, in run order.
	pub samples: Vec<StageSample>,
}

/// A library lane that could not run (for example `gpu` without an adapter).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedLane {
	/// The lane.
	pub lane: BackendLane,
	/// Why.
	pub reason: String,
}

/// One HTTP-path cell: one size against the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpCell {
	/// Output grid points requested.
	pub size: usize,
	/// The backend the server reports (`Auto`'s choice), from the last rep.
	pub server_backend: Option<String>,
	/// `output_points` the server reported, from the last rep.
	pub output_points: Option<usize>,
	/// Response body bytes, from the last rep.
	pub response_bytes: u64,
	/// Every rep answered 200 with `output_points == size`.
	pub ok: bool,
	/// Request start → status line read.
	pub ttfb: LatencyStats,
	/// Status line → last body byte.
	pub body: LatencyStats,
	/// Request start → last body byte (connect included).
	pub total: LatencyStats,
	/// Raw `(ttfb_ns, total_ns)` samples, in run order.
	pub samples: Vec<(u64, u64)>,
}

/// What the HTTP ingest did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpIngest {
	/// Whether it ran (`false` with `--http-skip-ingest`).
	pub ran: bool,
	/// Lines posted.
	pub lines: usize,
	/// POSTs made.
	pub requests: usize,
	/// Wall time (ns), untimed by the cells.
	pub wall_ns: u64,
}

/// The GPU the run used, as splimes reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuDescription {
	/// Adapter name.
	pub name: String,
	/// Graphics API.
	pub api: String,
	/// Device type.
	pub device_type: String,
	/// Driver.
	pub driver: String,
	/// `f64` kernels available.
	pub supports_f64: bool,
	/// `prewarm_gpu` wall time (ns): device open and kernel compile, untimed by the cells.
	pub prewarm_ns: u64,
}

/// The report `weft-e2e-interp` writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct E2eReport {
	/// [`SCHEMA_VERSION`].
	pub schema_version: u32,
	/// [`E2E_REPORT_KIND`].
	pub kind: String,
	/// The corpus.
	pub corpus: CorpusInfo,
	/// Grid resolution token.
	pub resolution: String,
	/// Spline token.
	pub spline: String,
	/// Knot spacings of margin each side.
	pub margin_knots: i64,
	/// Timed reps per cell.
	pub reps: usize,
	/// Warm-up reps per cell.
	pub warmup: usize,
	/// The GPU, when it started.
	pub gpu: Option<GpuDescription>,
	/// Why the GPU did not start, when it did not.
	pub gpu_error: Option<String>,
	/// How upload and readback were measured, and the probe's state.
	pub transfer_probe: String,
	/// Library-path cells.
	pub library: Vec<LibraryCell>,
	/// Lanes that could not run.
	pub skipped: Vec<SkippedLane>,
	/// HTTP ingest.
	pub http_ingest: Option<HttpIngest>,
	/// HTTP-path cells.
	pub http: Vec<HttpCell>,
	/// Every gate passed.
	pub all_ok: bool,
	/// The library path read its frames memory-mapped (`--mmap-reads`, roadmap Phase 5). Absent
	/// in reports written before it existed, which read buffered.
	#[serde(default)]
	pub mmap_reads: bool,
}

/// What the probe description says.
const PROBE_NOTE: &str = "upload and readback are a transfer probe, not splimes instrumentation: a second wgpu device on the adapter splimes opened times write_buffer+submit+wait of (2*knots+grid)*scalar bytes and copy+map+wait+copy-out of grid*scalar bytes per rep, between the timed compute calls; kernel = compute - upload - readback (saturating), and includes splimes' host-side preparation";

/// The transfer probe: its own device on splimes' adapter.
struct TransferProbe {
	device: wgpu::Device,
	queue: wgpu::Queue,
	max_buffer: u64,
}

impl TransferProbe {
	/// Open a device on the adapter named `name` (the one splimes opened).
	fn open(name: &str) -> Result<Self, String> {
		let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { flags: wgpu::InstanceFlags::from_build_config(), ..wgpu::InstanceDescriptor::new_without_display_handle() }.with_env());
		let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
		let adapter = adapters.into_iter().find(|a| a.get_info().name == name).ok_or_else(|| format!("transfer probe: no adapter named {name:?}"))?;
		let al = adapter.limits();
		let limits = wgpu::Limits { max_storage_buffer_binding_size: al.max_storage_buffer_binding_size, max_buffer_size: al.max_buffer_size, ..wgpu::Limits::default() };
		let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { label: Some("weft-bench transfer probe"), required_features: wgpu::Features::empty(), required_limits: limits, ..wgpu::DeviceDescriptor::default() })).map_err(|e| format!("transfer probe: could not open {name}: {e}"))?;
		Ok(Self { device, queue, max_buffer: al.max_buffer_size })
	}

	/// Round `bytes` up to the copy alignment (4).
	const fn aligned(bytes: u64) -> u64 {
		bytes.div_ceil(4) * 4
	}

	fn wait(&self, submission: wgpu::SubmissionIndex) -> Result<(), String> {
		self.device.poll(wgpu::PollType::Wait { submission_index: Some(submission), timeout: Some(PROBE_TIMEOUT) }).map(|_| ()).map_err(|e| format!("transfer probe: waiting for the GPU: {e}"))
	}

	/// Time `write_buffer` of `bytes` bytes plus a submit-and-wait.
	fn upload(&self, bytes: u64) -> Result<u64, String> {
		let size = Self::aligned(bytes.max(4));
		if size > self.max_buffer {
			return Err(format!("transfer probe: {size} bytes exceeds the adapter's buffer limit"));
		}
		let host = vec![0x5A_u8; usize::try_from(size).map_err(|_| "transfer probe: size".to_string())?];
		let buffer = self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("probe upload"), size, usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
		let started = Instant::now();
		self.queue.write_buffer(&buffer, 0, &host);
		let submission = self.queue.submit(None);
		self.wait(submission)?;
		Ok(elapsed_ns(started))
	}

	/// Time a buffer → staging copy, `map_async`, wait, and copy-out of `bytes` bytes.
	fn readback(&self, bytes: u64) -> Result<u64, String> {
		let size = Self::aligned(bytes.max(4));
		if size > self.max_buffer {
			return Err(format!("transfer probe: {size} bytes exceeds the adapter's buffer limit"));
		}
		let source = self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("probe source"), size, usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC, mapped_at_creation: false });
		let staging = self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("probe staging"), size, usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
		let started = Instant::now();
		let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("probe readback") });
		encoder.copy_buffer_to_buffer(&source, 0, &staging, 0, size);
		let submission = self.queue.submit(Some(encoder.finish()));
		let slice = staging.slice(..size);
		let (tx, rx) = mpsc::channel();
		slice.map_async(wgpu::MapMode::Read, move |r| {
			let _ = tx.send(r);
		});
		self.wait(submission)?;
		rx.recv().map_err(|_| "transfer probe: mapping abandoned".to_string())?.map_err(|e| format!("transfer probe: mapping: {e}"))?;
		let copied: Vec<u8> = {
			let view = slice.get_mapped_range().map_err(|e| format!("transfer probe: reading the mapped range: {e}"))?;
			view.to_vec()
		};
		staging.unmap();
		std::hint::black_box(&copied);
		Ok(elapsed_ns(started))
	}
}

fn elapsed_ns(started: Instant) -> u64 {
	u128_to_u64(started.elapsed().as_nanos())
}

/// Bytes per scalar on the GPU at `precision`.
const fn scalar_bytes(precision: Precision) -> u64 {
	match precision {
		Precision::F32 => 4,
		_ => 8,
	}
}

/// The probe's byte model for one call: `((2·knots + grid)·scalar, grid·scalar)`.
#[must_use]
pub const fn probe_bytes(knots: usize, grid: usize, f32_kernel: bool) -> (u64, u64) {
	let scalar = if f32_kernel { 4 } else { 8 };
	(((2 * knots + grid) as u64) * scalar, (grid as u64) * scalar)
}

/// A `Write` that counts and discards.
#[derive(Debug, Default)]
struct CountingSink(u64);

impl Write for CountingSink {
	fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
		self.0 += u64::try_from(buf.len()).unwrap_or(u64::MAX);
		Ok(buf.len())
	}

	fn flush(&mut self) -> std::io::Result<()> {
		Ok(())
	}
}

const fn kind_token(kind: PointKind) -> &'static str {
	match kind {
		PointKind::Raw => "raw",
		PointKind::Interpolated => "interpolated",
		PointKind::Extrapolated => "extrapolated",
	}
}

/// Serialize as the route's JSON point list into a counting sink; returns the bytes written.
fn serialize_json(series: &Interpolation<f64>) -> std::io::Result<u64> {
	let mut sink = std::io::BufWriter::with_capacity(1 << 16, CountingSink::default());
	sink.write_all(b"{\"points\":[")?;
	for (i, (timestamp, value, kind)) in series.iter().enumerate() {
		if i > 0 {
			sink.write_all(b",")?;
		}
		write!(sink, "{{\"timestamp\":\"{}\",\"value\":{value},\"kind\":\"{}\"}}", timestamp.to_rfc3339_opts(SecondsFormat::AutoSi, true), kind_token(kind))?;
	}
	write!(sink, "],\"output_points\":{}}}", series.len())?;
	sink.flush()?;
	let counter = sink.into_inner().map_err(std::io::IntoInnerError::into_error)?;
	Ok(counter.0)
}

/// A temporary store directory removed on drop.
struct TempStore {
	store: Option<SegmentStore>,
	dir: PathBuf,
}

impl Drop for TempStore {
	fn drop(&mut self) {
		drop(self.store.take());
		let _ = std::fs::remove_dir_all(&self.dir);
	}
}

/// Seal the corpus into a fresh store in a temporary directory.
async fn seal_corpus(corpus: &Corpus, mmap_reads: bool) -> anyhow::Result<TempStore> {
	let dir = std::env::temp_dir().join(format!("weft-bench-e2e-{}-{}", std::process::id(), Utc::now().timestamp_nanos_opt().unwrap_or_default()));
	std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
	let mut guard = TempStore { store: None, dir: dir.clone() };
	let options = weftdb::SegmentStoreOptions::default().with_mmap_reads(mmap_reads);
	let store = SegmentStore::open_with_options(&dir, options).await.with_context(|| format!("opening a store in {}", dir.display()))?;
	store.declare(E2E_ASPECT, &AspectSchema::new(PhysicalType::ScaledI64 { scale: corpus.info.decimal_scale }, BigDecimal::from(0), TimeUnit::Nanos)).await.context("declaring the e2e aspect")?;
	for (ts, vs) in corpus.timestamps.chunks(SEAL_BATCH_ROWS).zip(corpus.values.chunks(SEAL_BATCH_ROWS)) {
		store.seal_declared(E2E_ASPECT, ts, vs).await.context("sealing the corpus")?;
	}
	guard.store = Some(store);
	Ok(guard)
}

/// Rows → `f64` knot columns, as `interpolate_range_f64` does.
fn knot_columns(timestamps: Vec<i64>, values: Vec<Option<BigDecimal>>) -> anyhow::Result<(Vec<i64>, Vec<f64>)> {
	let (mut ticks, mut floats) = (Vec::with_capacity(timestamps.len()), Vec::with_capacity(timestamps.len()));
	for (t, v) in timestamps.into_iter().zip(values) {
		if let Some(value) = v {
			ticks.push(t);
			floats.push(weft_reduce::decimal_to_f64(&value).filter(|f| f.is_finite()).with_context(|| format!("stored value {value} has no finite f64 image"))?);
		}
	}
	Ok((ticks, floats))
}

/// One library rep: read, decode, compute, serialize. Returns the sample and the output.
async fn library_rep(store: &SegmentStore, w: Window, interpolator: Interpolator) -> anyhow::Result<(StageSample, Interpolation<f64>, usize, u64)> {
	let t0 = Instant::now();
	let (timestamps, values) = store.read_time_range(E2E_ASPECT, w.start - w.margin, w.end + w.margin).await.context("storage read")?;
	let storage_read = elapsed_ns(t0);

	let t1 = Instant::now();
	let (ticks, floats) = knot_columns(timestamps, values)?;
	let decode = elapsed_ns(t1);

	let t2 = Instant::now();
	let series = interpolator.run_epoch(&ticks, TickUnit::NANOSECONDS, &floats, w.start, w.end).context("interpolation")?;
	let compute = elapsed_ns(t2);

	let t3 = Instant::now();
	let bytes = serialize_json(&series).context("serialization")?;
	let serialize = elapsed_ns(t3);

	let sample = StageSample { storage_read, decode, compute, upload: None, readback: None, kernel: None, serialize, total: storage_read + decode + compute + serialize };
	Ok((sample, series, ticks.len(), bytes))
}

fn max_abs_diff(a: &[f64], b: &[f64]) -> Option<f64> {
	(a.len() == b.len()).then(|| a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0_f64, f64::max))
}

/// The lanes that can run, recording the others in `skipped`.
fn runnable_lanes(backends: &[BackendLane], gpu: Option<&GpuDescription>, skipped: &mut Vec<SkippedLane>) -> Vec<BackendLane> {
	let mut lanes = Vec::with_capacity(backends.len());
	for &lane in backends {
		let reason = if lane.needs_gpu() && gpu.is_none() {
			Some("the GPU did not start")
		} else if lane == BackendLane::Gpu && gpu.is_some_and(|g| !g.supports_f64) {
			Some("the adapter has no f64 kernels")
		} else {
			None
		};
		match reason {
			Some(reason) => skipped.push(SkippedLane { lane, reason: reason.to_string() }),
			None => lanes.push(lane),
		}
	}
	lanes
}

/// One probed GPU call: `(upload, readback)` ns, then `(upload, readback)` bytes.
type TransferProbeCall = ((u64, u64), (u64, u64));

/// Probe the transfers of one GPU call: `(upload, readback)` ns and the bytes moved.
fn probe_call(probe: &TransferProbe, knots: usize, grid: usize, precision: Precision) -> Result<TransferProbeCall, String> {
	let (up_bytes, down_bytes) = probe_bytes(knots, grid, scalar_bytes(precision) == 4);
	let upload = probe.upload(up_bytes)?;
	let readback = probe.readback(down_bytes)?;
	Ok(((upload, readback), (up_bytes, down_bytes)))
}

/// One cell's timed reps, after its warm-up.
struct CellRun {
	samples: Vec<StageSample>,
	series: Interpolation<f64>,
	knots: usize,
	bytes: u64,
	probe_bytes: Option<(u64, u64)>,
	probe_error: Option<String>,
}

async fn run_cell(config: &E2eConfig, store: &SegmentStore, w: Window, lane: BackendLane, interpolator: Interpolator, probe: Option<&TransferProbe>) -> anyhow::Result<CellRun> {
	for _ in 0..config.warmup {
		library_rep(store, w, interpolator).await?;
	}
	let mut samples = Vec::with_capacity(config.reps);
	let (mut last, mut probe_bytes_used, mut probe_error) = (None, None, None);
	for _ in 0..config.reps {
		let (mut sample, series, knots, bytes) = library_rep(store, w, interpolator).await?;
		let on_gpu = lane.needs_gpu() || (lane == BackendLane::Auto && series.backend() == Backend::Gpu);
		if let Some(probe) = probe.filter(|_| on_gpu) {
			match probe_call(probe, knots, series.len(), series.precision()) {
				Ok(((upload, readback), moved)) => {
					sample = sample.with_probe(upload, readback);
					probe_bytes_used = Some(moved);
				}
				Err(err) => probe_error = Some(err),
			}
		}
		samples.push(sample);
		last = Some((series, knots, bytes));
	}
	let (series, knots, bytes) = last.context("no reps ran")?;
	if probe_error.is_some() {
		// One failed probe makes the cell's transfer columns partial; drop them all rather than mix.
		for sample in &mut samples {
			sample.upload = None;
			sample.readback = None;
			sample.kernel = None;
		}
		probe_bytes_used = None;
	}
	Ok(CellRun { samples, series, knots, bytes, probe_bytes: probe_bytes_used, probe_error })
}

/// The cell record of a run, gated against the `cpu` reference.
fn library_cell(size: usize, lane: BackendLane, run: CellRun, reference: &Interpolation<f64>) -> LibraryCell {
	let CellRun { samples, series, knots, bytes, probe_bytes, .. } = run;
	let values_finite = series.values().iter().all(|v| v.is_finite());
	let backend_as_requested = !lane.needs_gpu() || series.backend() == Backend::Gpu;
	let max_abs_diff_vs_cpu = if lane == BackendLane::Cpu { None } else { max_abs_diff(series.values(), reference.values()) };
	let ok = series.len() == size && values_finite && backend_as_requested;
	let correctness = CellCorrectness { expected_output_points: size, actual_output_points: series.len(), values_finite, max_abs_diff_vs_cpu, backend_as_requested, ok };
	LibraryCell { size, lane, backend_ran: series.backend().to_string(), precision: series.precision().to_string(), knots, probe_bytes, serialized_bytes: bytes, stages: StageStats::of(&samples), transfer_share: TransferShare::of(&samples), correctness, samples }
}

/// Run the library path for every size and lane.
async fn run_library(config: &E2eConfig, corpus: &Corpus, gpu: Option<&GpuDescription>, probe: Option<&TransferProbe>, skipped: &mut Vec<SkippedLane>, progress: &mut (dyn FnMut(&str) + Send)) -> anyhow::Result<Vec<LibraryCell>> {
	let sealed = seal_corpus(corpus, config.mmap_reads).await?;
	let lanes = runnable_lanes(&config.backends, gpu, skipped);
	let cells = library_cells(config, corpus, sealed.store.as_ref().context("store")?, &lanes, probe, skipped, progress).await?;
	drop(sealed);
	Ok(cells)
}

/// The library-path cells of every size and lane, read from the sealed `store`.
async fn library_cells(config: &E2eConfig, corpus: &Corpus, store: &SegmentStore, lanes: &[BackendLane], probe: Option<&TransferProbe>, skipped: &mut Vec<SkippedLane>, progress: &mut (dyn FnMut(&str) + Send)) -> anyhow::Result<Vec<LibraryCell>> {
	let mut cells = Vec::new();
	for &size in &config.sizes {
		let w = window(&corpus.info, size, config.resolution, config.margin_knots)?;
		let base = Interpolator::new(config.spline, config.resolution).max_points(usize::MAX);
		// The reference values at this size, untimed.
		let (_, reference, _, _) = library_rep(store, w, base.backend(Backend::Cpu)).await?;
		for &lane in lanes {
			progress(&format!("library size={size} lane={}", lane.as_str()));
			let interpolator = base.backend(lane.backend()).gpu_precision(lane.precision());
			let run = run_cell(config, store, w, lane, interpolator, probe).await?;
			if let Some(err) = &run.probe_error {
				skipped.push(SkippedLane { lane, reason: format!("size {size}: transfer probe failed, upload/readback not recorded: {err}") });
			}
			cells.push(library_cell(size, lane, run, &reference));
		}
	}
	Ok(cells)
}

/// A minimal HTTP/1.1 response read: status, and the body streamed into a byte count and a tail.
struct HttpOutcome {
	status: u16,
	ttfb_ns: u64,
	total_ns: u64,
	body_bytes: u64,
	tail: Vec<u8>,
}

fn http_request(target: &HttpTarget, method: &str, path: &str, content_type: &str, body: &[u8]) -> anyhow::Result<HttpOutcome> {
	let started = Instant::now();
	let address = target.authority.to_socket_addrs().with_context(|| format!("resolving {}", target.authority))?.next().with_context(|| format!("{} resolved to nothing", target.authority))?;
	let mut stream = TcpStream::connect_timeout(&address, target.timeout).with_context(|| format!("connecting to {}", target.authority))?;
	stream.set_nodelay(true)?;
	stream.set_read_timeout(Some(target.timeout))?;
	stream.set_write_timeout(Some(target.timeout))?;
	let head = format!("{method} {path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: weft-bench-e2e/0.1\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nAccept: application/json\r\nConnection: close\r\n\r\n", target.authority, body.len());
	stream.write_all(head.as_bytes())?;
	stream.write_all(body)?;
	stream.flush()?;
	let mut reader = BufReader::with_capacity(1 << 16, stream);
	let mut line = String::new();
	reader.read_line(&mut line).context("reading the status line")?;
	let ttfb_ns = elapsed_ns(started);
	let status = line.split_whitespace().nth(1).and_then(|s| s.parse::<u16>().ok()).with_context(|| format!("malformed status line {line:?}"))?;
	let (mut content_length, mut chunked) = (None, false);
	loop {
		line.clear();
		reader.read_line(&mut line)?;
		let trimmed = line.trim_end();
		if trimmed.is_empty() {
			break;
		}
		if let Some((name, value)) = trimmed.split_once(':') {
			match name.trim().to_ascii_lowercase().as_str() {
				"content-length" => content_length = value.trim().parse::<u64>().ok(),
				"transfer-encoding" => chunked |= value.trim().eq_ignore_ascii_case("chunked"),
				_ => {}
			}
		}
	}
	let mut tail = TailSink::default();
	if chunked {
		loop {
			line.clear();
			reader.read_line(&mut line)?;
			let size_token = line.trim().split(';').next().unwrap_or_default();
			let size = u64::from_str_radix(size_token, 16).with_context(|| format!("malformed chunk size {size_token:?}"))?;
			if size == 0 {
				break;
			}
			let copied = std::io::copy(&mut (&mut reader).take(size), &mut tail)?;
			ensure!(copied == size, "the connection closed inside a chunk");
			line.clear();
			reader.read_line(&mut line)?;
		}
	} else if let Some(length) = content_length {
		let copied = std::io::copy(&mut (&mut reader).take(length), &mut tail)?;
		ensure!(copied == length, "the connection closed inside the body");
	} else {
		std::io::copy(&mut reader, &mut tail)?;
	}
	Ok(HttpOutcome { status, ttfb_ns, total_ns: elapsed_ns(started), body_bytes: tail.count, tail: tail.bytes })
}

/// Keeps the last [`TAIL_BYTES`] written, and counts them all.
#[derive(Debug, Default)]
struct TailSink {
	count: u64,
	bytes: Vec<u8>,
}

impl Write for TailSink {
	fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
		self.count += u64::try_from(buf.len()).unwrap_or(u64::MAX);
		self.bytes.extend_from_slice(buf);
		if self.bytes.len() > 2 * TAIL_BYTES {
			let cut = self.bytes.len() - TAIL_BYTES;
			self.bytes.drain(..cut);
		}
		Ok(buf.len())
	}

	fn flush(&mut self) -> std::io::Result<()> {
		Ok(())
	}
}

/// `"key":<number>` from a JSON tail.
fn tail_number(tail: &str, key: &str) -> Option<usize> {
	let at = tail.rfind(&format!("\"{key}\":"))? + key.len() + 3;
	let digits: String = tail[at..].trim_start().chars().take_while(char::is_ascii_digit).collect();
	digits.parse().ok()
}

/// `"key":"<text>"` from a JSON tail.
fn tail_string(tail: &str, key: &str) -> Option<String> {
	let at = tail.rfind(&format!("\"{key}\":"))? + key.len() + 3;
	let rest = tail[at..].trim_start().strip_prefix('"')?;
	Some(rest[..rest.find('"')?].to_string())
}

/// The ILP lines of `corpus[range]`.
fn ilp_lines(corpus: &Corpus, range: std::ops::Range<usize>) -> String {
	let mut body = String::with_capacity(range.len() * 40);
	for i in range {
		let _ = writeln!(body, "{ILP_MEASUREMENT} {ILP_FIELD}={} {}", corpus.values[i], corpus.timestamps[i]);
	}
	body
}

fn http_ingest(target: &HttpTarget, corpus: &Corpus) -> anyhow::Result<HttpIngest> {
	if target.skip_ingest {
		return Ok(HttpIngest { ran: false, lines: 0, requests: 0, wall_ns: 0 });
	}
	let started = Instant::now();
	let declare = format!("{{\"name\":\"{}\",\"physical_type\":\"scaled_i64\",\"scale\":{},\"timestamp_unit\":\"nanos\"}}", target.aspect, corpus.info.decimal_scale);
	let answer = http_request(target, "POST", "/api/v1/storage/aspects", "application/json", declare.as_bytes())?;
	ensure!((200..300).contains(&answer.status), "declaring aspect {:?} answered {}: {}", target.aspect, answer.status, String::from_utf8_lossy(&answer.tail));
	let path = format!("/api/v1/storage/{}/ilp?measurement={ILP_MEASUREMENT}&field={ILP_FIELD}&precision=ns", target.aspect);
	let mut requests = 0;
	let rows = corpus.timestamps.len();
	let mut from = 0;
	while from < rows {
		let to = (from + ILP_BATCH_LINES).min(rows);
		let answer = http_request(target, "POST", &path, "text/plain", ilp_lines(corpus, from..to).as_bytes())?;
		ensure!((200..300).contains(&answer.status), "ILP ingest of rows {from}..{to} answered {}: {}", answer.status, String::from_utf8_lossy(&answer.tail));
		requests += 1;
		from = to;
	}
	Ok(HttpIngest { ran: true, lines: rows, requests, wall_ns: elapsed_ns(started) })
}

fn run_http(config: &E2eConfig, target: &HttpTarget, corpus: &Corpus, progress: &mut (dyn FnMut(&str) + Send)) -> anyhow::Result<Vec<HttpCell>> {
	let mut cells = Vec::new();
	for &size in &config.sizes {
		progress(&format!("http size={size}"));
		let w = window(&corpus.info, size, config.resolution, config.margin_knots)?;
		let path = format!("/api/v1/storage/{}/interpolate?start={}&end={}&resolution={}&spline={}&margin={}", target.aspect, w.start, w.end, config.resolution.as_str(), spline_token(config.spline), w.margin);
		for _ in 0..config.warmup {
			http_request(target, "GET", &path, "application/json", &[])?;
		}
		let mut samples = Vec::with_capacity(config.reps);
		let mut ok = true;
		let mut last = None;
		for _ in 0..config.reps {
			let outcome = http_request(target, "GET", &path, "application/json", &[])?;
			let tail = String::from_utf8_lossy(&outcome.tail).into_owned();
			let points = tail_number(&tail, "output_points");
			ok &= outcome.status == 200 && points == Some(size);
			samples.push((outcome.ttfb_ns, outcome.total_ns));
			last = Some((outcome, tail, points));
		}
		let (outcome, tail, points) = last.context("no reps ran")?;
		if outcome.status != 200 {
			bail!("GET {path} answered {}: {tail}", outcome.status);
		}
		let ttfb: Vec<u64> = samples.iter().map(|s| s.0).collect();
		let total: Vec<u64> = samples.iter().map(|s| s.1).collect();
		let body: Vec<u64> = samples.iter().map(|s| s.1.saturating_sub(s.0)).collect();
		cells.push(HttpCell { size, server_backend: tail_string(&tail, "backend"), output_points: points, response_bytes: outcome.body_bytes, ok, ttfb: LatencyStats::from_samples(&ttfb), body: LatencyStats::from_samples(&body), total: LatencyStats::from_samples(&total), samples });
	}
	Ok(cells)
}

const fn spline_token(spline: Spline) -> &'static str {
	match spline {
		Spline::Linear => "linear",
		Spline::Quadratic => "quadratic",
		Spline::Cubic => "cubic",
		_ => "polynomial (not a route token)",
	}
}

/// Start splimes' GPU, if any lane wants it.
fn start_gpu() -> Result<GpuDescription, String> {
	let started = Instant::now();
	let info = splimes::prewarm_gpu().map_err(|err| ToString::to_string(&err))?;
	Ok(GpuDescription { name: info.name, api: info.api, device_type: info.device_type, driver: info.driver, supports_f64: info.supports_f64, prewarm_ns: elapsed_ns(started) })
}

/// Load the configured corpus.
///
/// # Errors
///
/// As [`synthetic_corpus`] and [`uci_corpus`].
pub fn load_corpus(config: &E2eConfig) -> anyhow::Result<Corpus> {
	let largest = config.sizes.iter().copied().max().unwrap_or(2);
	match &config.corpus {
		CorpusSource::Synthetic { seed, knot_step_ns } => synthetic_corpus(*seed, *knot_step_ns, largest, config.resolution, config.margin_knots),
		CorpusSource::Uci { path, column, max_rows } => uci_corpus(path, column, *max_rows),
	}
}

/// Run the benchmark.
///
/// # Errors
///
/// Returns an error for an invalid configuration, a corpus too short for a size, a store or
/// HTTP failure. A GPU that does not start is recorded, not an error: its lanes are skipped.
pub async fn run(config: &E2eConfig, progress: &mut (dyn FnMut(&str) + Send)) -> anyhow::Result<E2eReport> {
	config.validate()?;
	let corpus = load_corpus(config)?;
	// Fail early on a corpus too short for the largest window.
	for &size in &config.sizes {
		window(&corpus.info, size, config.resolution, config.margin_knots)?;
	}
	let wants_gpu = config.backends.iter().any(|l| l.needs_gpu() || *l == BackendLane::Auto);
	let started = if wants_gpu { Some(start_gpu()) } else { None };
	let gpu_error = started.as_ref().and_then(|r| r.as_ref().err().cloned());
	let gpu = started.and_then(Result::ok);
	let opened = gpu.as_ref().map(|g| TransferProbe::open(&g.name));
	let probe_state = match (&gpu, &opened) {
		(Some(g), Some(Ok(_))) => format!("{PROBE_NOTE}; probe device open on {:?}", g.name),
		(_, Some(Err(err))) => format!("{PROBE_NOTE}; probe NOT available ({err}), so upload/readback/kernel are absent"),
		_ => format!("{PROBE_NOTE}; no GPU, so the probe did not run"),
	};
	let probe = opened.and_then(Result::ok);
	let mut skipped = Vec::new();
	let library = if config.backends.is_empty() { Vec::new() } else { run_library(config, &corpus, gpu.as_ref(), probe.as_ref(), &mut skipped, progress).await? };
	drop(probe);
	let (mut http_ingested, mut http) = (None, Vec::new());
	if let Some(target) = &config.http {
		progress("http ingest");
		http_ingested = Some(http_ingest(target, &corpus)?);
		http = run_http(config, target, &corpus, progress)?;
	}
	let all_ok = library.iter().all(|c| c.correctness.ok) && http.iter().all(|c| c.ok);
	Ok(E2eReport { schema_version: SCHEMA_VERSION, kind: E2E_REPORT_KIND.to_string(), corpus: corpus.info, resolution: config.resolution.as_str().to_string(), spline: spline_token(config.spline).to_string(), margin_knots: config.margin_knots, reps: config.reps, warmup: config.warmup, gpu, gpu_error, transfer_probe: probe_state, library, skipped, http_ingest: http_ingested, http, all_ok, mmap_reads: config.mmap_reads })
}

fn ms(ns: u64) -> String {
	format!("{:.3}", u64_to_f64(ns) / 1e6)
}

/// A Markdown summary: one row per cell, p50/p95/p99 of each stage in ms, and the transfer share.
#[must_use]
pub fn markdown(report: &E2eReport) -> String {
	let mut out = String::new();
	let _ = write!(out, "# End-to-end GPU interpolation ({})\n\n", report.corpus.source);
	let _ = write!(out, "schema v{}, {} knots, resolution {}, spline {}, {} reps (+{} warm-up), GPU: {}\n\n", report.schema_version, report.corpus.rows, report.resolution, report.spline, report.reps, report.warmup, report.gpu.as_ref().map_or_else(|| format!("none ({})", report.gpu_error.as_deref().unwrap_or("not requested")), |g| format!("{} ({}, {})", g.name, g.api, g.device_type)));
	let _ = write!(out, "Transfer probe: {}\n\n", report.transfer_probe);
	out.push_str("## Library path (ms, p50 / p95 / p99)\n\n| size | lane | ran | knots | storage read | decode | compute | upload | kernel (derived) | readback | serialize | total | transfer share p50 / p95 | ok |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n");
	let tri = |s: &LatencyStats| format!("{} / {} / {}", ms(s.p50_ns), ms(s.p95_ns), ms(s.p99_ns));
	let opt = |s: Option<&LatencyStats>| s.map_or_else(|| "-".to_string(), tri);
	for c in &report.library {
		let share = c.transfer_share.map_or_else(|| "-".to_string(), |t| format!("{:.1}% / {:.1}%", t.p50 * 100.0, t.p95 * 100.0));
		let _ = writeln!(out, "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |", c.size, c.lane.as_str(), c.backend_ran, c.knots, tri(&c.stages.storage_read), tri(&c.stages.decode), tri(&c.stages.compute), opt(c.stages.upload.as_ref()), opt(c.stages.kernel.as_ref()), opt(c.stages.readback.as_ref()), tri(&c.stages.serialize), tri(&c.stages.total), share, if c.correctness.ok { "yes" } else { "NO" });
	}
	for s in &report.skipped {
		let _ = write!(out, "\nSkipped `{}`: {}\n", s.lane.as_str(), s.reason);
	}
	if !report.http.is_empty() {
		out.push_str("\n## HTTP path (ms, p50 / p95 / p99)\n\n| size | server backend | bytes | ttfb | body | total | ok |\n|---|---|---|---|---|---|---|\n");
		for c in &report.http {
			let _ = writeln!(out, "| {} | {} | {} | {} | {} | {} | {} |", c.size, c.server_backend.as_deref().unwrap_or("?"), c.response_bytes, tri(&c.ttfb), tri(&c.body), tri(&c.total), if c.ok { "yes" } else { "NO" });
		}
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn counts_parse_with_suffixes() {
		assert_eq!(parse_count("10k"), Ok(10_000));
		assert_eq!(parse_count("1M"), Ok(1_000_000));
		assert_eq!(parse_count("7"), Ok(7));
		assert!(parse_count("0").is_err());
		assert!(parse_count("x").is_err());
	}

	#[test]
	fn lanes_round_trip_their_tokens() {
		for lane in BackendLane::ALL {
			assert_eq!(BackendLane::parse(lane.as_str()), Some(lane));
		}
		assert_eq!(BackendLane::parse("tpu"), None);
		assert!(parse_resolution("months").is_err());
		assert_eq!(parse_resolution("milliseconds"), Ok(Resolution::Milliseconds));
	}

	#[test]
	fn the_probe_byte_model_is_knots_pairs_plus_grid_up_and_grid_down() {
		assert_eq!(probe_bytes(10, 100, false), ((2 * 10 + 100) * 8, 100 * 8));
		assert_eq!(probe_bytes(10, 100, true), ((2 * 10 + 100) * 4, 100 * 4));
	}

	#[test]
	fn transfer_share_and_derived_kernel() {
		let s = StageSample { storage_read: 10, decode: 10, compute: 60, serialize: 20, total: 100, ..StageSample::default() }.with_probe(15, 5);
		assert_eq!(s.kernel, Some(40));
		assert!(s.transfer_share().is_some_and(|share| (share - 0.2).abs() < 1e-12));
		// The kernel saturates rather than going negative when the probe exceeds compute.
		assert_eq!(StageSample { compute: 10, total: 10, ..StageSample::default() }.with_probe(8, 8).kernel, Some(0));
		assert_eq!(StageSample::default().transfer_share(), None);
		let share = TransferShare::of(&[s, s]).expect("probed");
		assert!((share.p50 - 0.2).abs() < 1e-12 && (share.p99 - 0.2).abs() < 1e-12);
		assert!(TransferShare::of(&[StageSample::default()]).is_none());
		let stats = StageStats::of(&[s, s]);
		assert_eq!(stats.upload.map(|u| u.p50_ns), Some(15));
		assert!(StageStats::of(&[StageSample::default()]).upload.is_none());
	}

	#[test]
	fn json_tail_fields_are_read() {
		let tail = "...}],\"spline\":\"cubic\",\"backend\":\"Gpu\",\"output_points\":12345}";
		assert_eq!(tail_number(tail, "output_points"), Some(12_345));
		assert_eq!(tail_string(tail, "backend").as_deref(), Some("Gpu"));
		assert_eq!(tail_number(tail, "absent"), None);
	}

	#[test]
	fn the_tail_sink_counts_everything_and_keeps_the_end() {
		let mut sink = TailSink::default();
		for _ in 0..10 {
			sink.write_all(&[b'a'; 1000]).unwrap();
		}
		sink.write_all(b"END").unwrap();
		assert_eq!(sink.count, 10_003);
		assert!(sink.bytes.len() <= 2 * TAIL_BYTES && sink.bytes.ends_with(b"END"));
	}

	#[test]
	fn the_synthetic_corpus_covers_the_largest_window_and_ascends() {
		let corpus = synthetic_corpus(7, 100_000_000, 50_000, Resolution::Milliseconds, 4).expect("corpus");
		assert!(corpus.timestamps.windows(2).all(|w| w[0] < w[1]));
		assert_eq!(corpus.timestamps.len(), corpus.values.len());
		let w = window(&corpus.info, 50_000, Resolution::Milliseconds, 4).expect("fits");
		assert_eq!((w.end - w.start) / 1_000_000 + 1, 50_000);
		assert!(window(&corpus.info, 5_000_000, Resolution::Milliseconds, 4).is_err());
		// Same seed, same corpus.
		assert_eq!(synthetic_corpus(7, 100_000_000, 50_000, Resolution::Milliseconds, 4).expect("corpus").values, corpus.values);
	}

	#[test]
	fn ilp_lines_are_measurement_field_value_timestamp() {
		let corpus = synthetic_corpus(1, 1_000_000_000, 10, Resolution::Seconds, 1).expect("corpus");
		let body = ilp_lines(&corpus, 0..2);
		let first = body.lines().next().expect("a line");
		assert_eq!(first, format!("e2e value={} {}", corpus.values[0], corpus.timestamps[0]));
		assert_eq!(body.lines().count(), 2);
	}

	/// The library path end to end on the CPU lanes (no GPU needed): every stage runs, the
	/// output has `size` finite points and `parallel` matches `cpu`.
	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn the_library_path_runs_on_the_cpu_lanes() {
		let mut config = E2eConfig::smoke();
		config.sizes = vec![500];
		config.backends = vec![BackendLane::Cpu, BackendLane::Parallel];
		config.reps = 2;
		config.warmup = 0;
		let report = run(&config, &mut |_| {}).await.expect("runs");
		assert_eq!(report.schema_version, SCHEMA_VERSION);
		assert_eq!(report.library.len(), 2);
		for cell in &report.library {
			assert!(cell.correctness.ok, "{cell:?}");
			assert_eq!(cell.samples.len(), 2);
			assert!(cell.stages.upload.is_none() && cell.transfer_share.is_none());
			assert!(cell.serialized_bytes > 0);
			assert!(cell.samples.iter().all(|s| s.total == s.storage_read + s.decode + s.compute + s.serialize));
		}
		// Both CPU backends compute in f64 from the same knots.
		assert!(report.library[1].correctness.max_abs_diff_vs_cpu.is_some_and(|d| d < 1e-9));
		assert!(report.all_ok);
		assert!(markdown(&report).contains("| 500 | parallel |"));
	}
}
