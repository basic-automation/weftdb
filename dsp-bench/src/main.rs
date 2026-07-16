//! `dsp-bench` — the command-line runner for DSP-Bench.
//!
//! Up to now the line-protocol → interpolation path existed only at the library
//! level: [`dsp_bench::InterpolationProfile::from_line_protocol`] could turn a
//! TSBS / `InfluxDB`-Line-Protocol payload into a workload, but nothing on disk
//! could *run* it. This binary closes that loop — it reads a `.lp` file, drives
//! the chosen numeric field through the same DSP interpolation harness, correctness
//! gate, and reporting as every other DSP-Bench run, and persists the result as a
//! durable `reports/json/` artifact (the "keep raw results" rule).
//!
//! ```text
//! dsp-bench --input data.lp --field usage \
//!           --precision s --spline cubic --resolution minutes --reps 10
//! ```
//!
//! It deliberately stays dependency-free (hand-rolled arg parsing, no `clap`):
//! `dsp-bench` is a leaf crate and the CLI surface is small, so the parser lives
//! here as a pure, unit-tested function rather than pulling a new dependency into
//! the workspace.

#![warn(clippy::pedantic, clippy::nursery, clippy::all)]

use std::{path::PathBuf, process::ExitCode};

use chrono::Utc;
use dsp_bench::{
	report::{default_filename, default_html_filename}, run_compression, run_downsample, run_point_lookup, run_profile, run_range_fetch, Aggregation, BaselineLinearAdapter, BenchReport, BenchResult, CompressionParams, CompressionProfile, DownsampleParams, DownsampleProfile, DspAdapter, ForwardFillAdapter, InterpolationProfile, LookupMode, PointLookupParams, PointLookupProfile, RangeFetchParams, RangeFetchProfile, RunMetadata, SignalShape, SyntheticParams, TimestampPrecision, ValueShape
};
use splimes::{Resolution, Spline};

/// Program name used in usage / error output.
const PROG: &str = "dsp-bench";

fn main() -> ExitCode {
	let command = match Cli::from_args(std::env::args().skip(1)) {
		Ok(c) => c,
		Err(e) => {
			eprintln!("{PROG}: {e}\n");
			eprint!("{USAGE}");
			return ExitCode::FAILURE;
		}
	};

	let cli = match command {
		Command::Help => {
			print!("{USAGE}");
			return ExitCode::SUCCESS;
		}
		Command::Run(cli) => *cli,
	};

	// A multi-threaded runtime is unnecessary for a single sequential run; a
	// current-thread runtime keeps the binary lean and startup cheap.
	let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
		Ok(rt) => rt,
		Err(e) => {
			eprintln!("{PROG}: failed to start async runtime: {e}");
			return ExitCode::FAILURE;
		}
	};

	match runtime.block_on(run(cli)) {
		Ok(exit) => exit,
		Err(e) => {
			eprintln!("{PROG}: {e:#}");
			ExitCode::FAILURE
		}
	}
}

/// Load the dataset, run the selected workload, and persist the report.
async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
	// The point-lookup workload is a parallel path: it seals a columnar segment from
	// a seeded corpus and times DSP's streaming point read, so it builds its own
	// report and shares the persist/summarize tail rather than the interpolation
	// profile plumbing.
	if cli.mode == InputMode::PointLookup {
		return run_point_lookup_workload(&cli);
	}
	if cli.mode == InputMode::RangeFetch {
		return run_range_fetch_workload(&cli);
	}
	if cli.mode == InputMode::Compression {
		return run_compression_workload(&cli);
	}
	if cli.mode == InputMode::Downsample {
		return run_downsample_workload(&cli);
	}

	// Build the profile from whichever input mode was selected. Synthetic mode
	// carries a known analytic ground truth, so its report will also include
	// accuracy metrics; line-protocol mode does not.
	let profile = if cli.mode == InputMode::Synthetic {
		let profile_name = cli.name.clone().unwrap_or_else(|| "interpolation-heavy-irregular".to_string());
		let params = SyntheticParams { seed: cli.seed, input_points: cli.points, missingness_fraction: cli.missingness, jitter_fraction: cli.jitter, noise_amplitude: cli.noise, signal_shape: cli.shape, spline: cli.spline, resolution: cli.resolution };
		InterpolationProfile::synthetic(profile_name, params)
	} else {
		// Validated in `from_args`: line-protocol mode always carries input + field.
		let input = cli.input.as_ref().expect("line-protocol mode has an input path");
		let field = cli.field.as_ref().expect("line-protocol mode has a field");
		let payload = std::fs::read_to_string(input).map_err(|e| anyhow::anyhow!("cannot read input file {}: {e}", input.display()))?;
		let profile_name = cli.name.clone().unwrap_or_else(|| derive_profile_name(input));
		InterpolationProfile::from_line_protocol(profile_name, &payload, field, cli.precision, cli.spline, cli.resolution).map_err(|e| anyhow::anyhow!("cannot build a profile from {}: {e}", input.display()))?
	};

	// DSP is always run; under `--compare` the full portable-baseline suite runs
	// too — linear (fair-protocol class C) and forward-fill/LOCF (class B, the
	// in-process mirror of native TSDB `FILL(previous)`) — so the report carries a
	// real multi-system comparison rather than a lone number.
	let mut results: Vec<BenchResult> = Vec::with_capacity(if cli.compare { 3 } else { 1 });
	let dsp_result = run_profile(&DspAdapter::new(), &profile, cli.reps).await.map_err(|e| anyhow::anyhow!("DSP benchmark run failed: {e}"))?;
	results.push(dsp_result);
	if cli.compare {
		let linear = run_profile(&BaselineLinearAdapter::new(), &profile, cli.reps).await.map_err(|e| anyhow::anyhow!("linear baseline run failed: {e}"))?;
		results.push(linear);
		let forward_fill = run_profile(&ForwardFillAdapter::new(), &profile, cli.reps).await.map_err(|e| anyhow::anyhow!("forward-fill baseline run failed: {e}"))?;
		results.push(forward_fill);
	}

	let metadata = RunMetadata::capture(Utc::now().to_rfc3339());
	let report = BenchReport::with_results(metadata, results);
	finish(&cli, &report)
}

/// Run the point-lookup workload: build a seeded [`PointLookupProfile`] from the
/// point-lookup CLI knobs, seal a columnar segment, time DSP's streaming point read
/// (`read_segment_point` / `read_segment_points`), and persist the report.
///
/// This is the storage-backed sibling of the interpolation path — it has no
/// analytic ground truth (a lookup returns the stored value), so the report carries
/// no accuracy, only latency, throughput, and the north-star storage estimate.
fn run_point_lookup_workload(cli: &Cli) -> anyhow::Result<ExitCode> {
	let profile_name = cli.name.clone().unwrap_or_else(|| if cli.irregular { "point-lookup-irregular".to_string() } else { "point-lookup-regular".to_string() });
	let params = PointLookupParams { seed: cli.seed, point_count: cli.pl_rows, query_count: cli.pl_queries, regular: !cli.irregular, absent_fraction: cli.pl_absent, mode: cli.pl_mode, rows_per_page: cli.pl_rows_per_page };
	let profile = PointLookupProfile::new(profile_name, params);
	let result = run_point_lookup(&profile, cli.reps).map_err(|e| anyhow::anyhow!("point-lookup benchmark run failed: {e}"))?;
	let metadata = RunMetadata::capture(Utc::now().to_rfc3339());
	let report = BenchReport::with_results(metadata, vec![result]);
	finish(cli, &report)
}

/// Run the range-fetch workload: build a seeded [`RangeFetchProfile`] from the
/// range-fetch CLI knobs, seal a columnar segment, time DSP's windowed range read
/// (`read_segment_range`), and persist the report.
///
/// Like the point-lookup workload it is storage-backed with no analytic ground
/// truth (a range read materializes stored rows), so the report carries latency,
/// throughput, and the storage estimate but no accuracy.
fn run_range_fetch_workload(cli: &Cli) -> anyhow::Result<ExitCode> {
	let profile_name = cli.name.clone().unwrap_or_else(|| if cli.irregular { "range-fetch-irregular".to_string() } else { "range-fetch-regular".to_string() });
	let params = RangeFetchParams { seed: cli.seed, point_count: cli.rf_rows, window_rows: cli.rf_window, window_count: cli.rf_windows, regular: !cli.irregular, rows_per_page: cli.rf_rows_per_page };
	let profile = RangeFetchProfile::new(profile_name, params);
	let result = run_range_fetch(&profile, cli.reps).map_err(|e| anyhow::anyhow!("range-fetch benchmark run failed: {e}"))?;
	let metadata = RunMetadata::capture(Utc::now().to_rfc3339());
	let report = BenchReport::with_results(metadata, vec![result]);
	finish(cli, &report)
}

/// Run the compression workload: build a seeded [`CompressionProfile`] from the
/// compression CLI knobs, seal a columnar segment, time a full decode, and persist
/// the report (headline: realized bytes/point + compression ratio + decode
/// throughput). No analytic ground truth — correctness is an exact decode round-trip.
fn run_compression_workload(cli: &Cli) -> anyhow::Result<ExitCode> {
	let profile_name = cli.name.clone().unwrap_or_else(|| format!("compression-{}", cli.comp_shape.as_str()));
	let params = CompressionParams { seed: cli.seed, point_count: cli.comp_rows, regular: !cli.irregular, value_shape: cli.comp_shape };
	let profile = CompressionProfile::new(profile_name, params);
	let result = run_compression(&profile, cli.reps).map_err(|e| anyhow::anyhow!("compression benchmark run failed: {e}"))?;
	let metadata = RunMetadata::capture(Utc::now().to_rfc3339());
	let report = BenchReport::with_results(metadata, vec![result]);
	finish(cli, &report)
}

/// Run the downsample workload: build a seeded [`DownsampleProfile`] from the
/// downsample CLI knobs, generate a dense series, and time DSP's canonical reduction
/// (`dsp-reduce`) into grid-aligned buckets. No analytic ground truth — correctness
/// is that the reduction is total (the bucket counts sum to the input size).
fn run_downsample_workload(cli: &Cli) -> anyhow::Result<ExitCode> {
	let profile_name = cli.name.clone().unwrap_or_else(|| "downsample".to_string());
	let params = DownsampleParams { seed: cli.seed, point_count: cli.ds_points, input_stride_secs: cli.ds_stride, bucket_resolution: cli.ds_bucket, aggregations: cli.ds_aggs.clone(), parallel_chunks: cli.ds_parallel };
	let profile = DownsampleProfile::new(profile_name, params);
	let result = run_downsample(&profile, cli.reps).map_err(|e| anyhow::anyhow!("downsample benchmark run failed: {e}"))?;
	let metadata = RunMetadata::capture(Utc::now().to_rfc3339());
	let report = BenchReport::with_results(metadata, vec![result]);
	finish(cli, &report)
}

/// Persist a completed report (JSON, and optionally HTML) and print the summary,
/// returning the process exit code. Shared by every workload path so a write
/// failure always surfaces as a non-zero exit and the summary/artifact layout is
/// identical regardless of workload.
fn finish(cli: &Cli, report: &BenchReport) -> anyhow::Result<ExitCode> {
	// The artifact name tags every adapter in the report (e.g. `dsp+baseline-linear`)
	// so a comparison and a solo run never collide on disk.
	let adapter_tag = report.results.iter().map(|r| r.adapter.as_str()).collect::<Vec<_>>().join("+");

	// Persist before printing so a write failure surfaces as a non-zero exit even
	// if the summary already streamed.
	let out_path = cli.out_dir.join(default_filename(&report.results[0].profile, &adapter_tag));
	report.write_json(&out_path).map_err(|e| anyhow::anyhow!("cannot write report to {}: {e}", out_path.display()))?;

	// Optionally also emit a human-readable HTML view of the same results,
	// co-located with the JSON artifact (same stem, `.html`).
	let html_path = if cli.html {
		let path = cli.out_dir.join(default_html_filename(&report.results[0].profile, &adapter_tag));
		report.write_html(&path).map_err(|e| anyhow::anyhow!("cannot write HTML report to {}: {e}", path.display()))?;
		Some(path)
	} else {
		None
	};

	print_summary(report, &out_path);
	if let Some(path) = &html_path {
		println!("  html report  : {}", path.display());
	}

	// The run is honest about its own verdict: a correctness failure is a non-zero
	// exit so a CI / scripted caller can gate on it.
	Ok(if report.is_publishable() { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

/// Print a concise human-readable summary of a report, one block per adapter.
fn print_summary(report: &BenchReport, out_path: &std::path::Path) {
	println!("DSP-Bench run complete");
	let first = &report.results[0];
	println!("  profile      : {}", first.profile);
	println!("  workload     : {}", first.workload);
	// Surface the captured CPU model when available (the full hardware block —
	// cores, RAM — lands in the artifact and the HTML report).
	if let Some(cpu) = &report.metadata.cpu_model {
		println!("  cpu          : {cpu}");
	}
	// Synthetic runs record which analytic ground-truth shape was generated; a
	// line-protocol run has none, so the line is skipped.
	if let Some(shape) = first.dataset.signal_shape {
		println!("  signal shape : {shape:?}");
	}
	println!("  reps         : {}", first.reps);
	for r in &report.results {
		let l = &r.latency;
		println!("  --- {} ---", r.adapter);
		println!("    input points : {}", r.dataset.input_points);
		println!("    output points: {}", r.dataset.output_points);
		println!("    latency (ms) : p50={:.3} p95={:.3} p99={:.3} mean={:.3}", ms(l.p50_ns), ms(l.p95_ns), ms(l.p99_ns), ms(l.mean_ns));
		println!("    throughput   : {:.0} points/sec", r.throughput_points_per_sec);
		println!("    correctness  : {}", if r.correctness.passed() { "PASS" } else { "FAIL" });
		// Accuracy is present only for a synthetic profile (known ground truth);
		// a line-protocol source has none, so the line is simply skipped.
		if let Some(a) = &r.accuracy {
			println!("    accuracy     : rmse={:.4} mae={:.4} max={:.4} bias={:+.4}", a.rmse, a.mae, a.max_abs_error, a.bias);
		}
		// Storage is the north-star cost term (and the compression workload's headline):
		// the realized value codec, the value-column compression ratio (realized /
		// naive fixed-width), and total bytes/point. Present once the run estimates it.
		if let Some(s) = &r.storage {
			#[allow(clippy::cast_precision_loss)]
			let ratio = if s.estimated_value_bytes > 0 { s.realized_value_bytes as f64 / s.estimated_value_bytes as f64 } else { 1.0 };
			println!("    storage      : codec={}{} ratio={ratio:.3} val={:.2} total={:.2} B/pt", s.value_codec, if s.is_exact { "" } else { "*" }, s.bytes_per_point, s.total_bytes_per_point);
		}
	}
	// When the run carries accuracy (synthetic mode), name the quality winner so a
	// comparison report answers "which method recovered the signal best?" at a glance.
	if let Some(best) = report.most_accurate() {
		if let Some(a) = &best.accuracy {
			println!("  most accurate: {} (rmse={:.4})", best.adapter, a.rmse);
		}
	}
	println!("  publishable  : {}", if report.is_publishable() { "yes" } else { "no" });
	println!("  report       : {}", out_path.display());
}

/// Nanoseconds rendered as fractional milliseconds for display.
#[allow(clippy::cast_precision_loss)]
fn ms(ns: u64) -> f64 {
	ns as f64 / 1e6
}

/// Derive a profile name from the input file stem, e.g. `cpu-usage.lp` →
/// `cpu-usage`. Falls back to `line-protocol` when the path has no usable stem.
fn derive_profile_name(input: &std::path::Path) -> String {
	input.file_stem().and_then(|s| s.to_str()).map_or_else(|| "line-protocol".to_string(), ToString::to_string)
}

/// The parsed, validated run configuration.
///
/// The input source is exactly one of two modes: a line-protocol file
/// (`input` + `field`) or the seeded `synthetic` generator. Synthetic mode is the
/// only one with a known analytic ground truth, so it is the only one that yields
/// accuracy metrics.
#[derive(Debug, Clone, PartialEq)]
struct Cli {
	/// Path to the `.lp` / TSBS line-protocol input file (line-protocol mode).
	input: Option<PathBuf>,
	/// Numeric field to project onto the interpolated series (line-protocol mode).
	field: Option<String>,
	/// Which input mode / workload was selected.
	mode: InputMode,
	/// Compression mode: rows sealed into the segment (the stored corpus size).
	comp_rows: usize,
	/// Compression mode: the value-column shape (which codec regime to exercise).
	comp_shape: ValueShape,
	/// Downsample mode: input sample count.
	ds_points: usize,
	/// Downsample mode: input sample spacing, in seconds.
	ds_stride: i64,
	/// Downsample mode: bucket resolution the series is reduced to.
	ds_bucket: Resolution,
	/// Downsample mode: the reductions computed per bucket.
	ds_aggs: Vec<Aggregation>,
	/// Reduce in this many parallel chunks (partial reductions merged); 1 = serial.
	ds_parallel: usize,
	/// Storage-workload knob (point-lookup / range-fetch): jittered (irregular)
	/// timestamps instead of a constant-stride regular corpus.
	irregular: bool,
	/// Point-lookup mode: rows sealed into the segment (the stored corpus size).
	pl_rows: usize,
	/// Point-lookup mode: instants resolved per timed rep (the query batch size).
	pl_queries: usize,
	/// Point-lookup mode: fraction of the query batch made deliberately off-grid.
	pl_absent: f64,
	/// Point-lookup mode: how the query batch is issued (single reads vs one batch).
	pl_mode: LookupMode,
	/// Point-lookup mode: rows per page (`0` = single-block; `>0` = paged segment).
	pl_rows_per_page: usize,
	/// Range-fetch mode: rows sealed into the segment (the stored corpus size).
	rf_rows: usize,
	/// Range-fetch mode: rows spanned by each fetched window (the selectivity).
	rf_window: usize,
	/// Range-fetch mode: number of windows fetched per timed rep.
	rf_windows: usize,
	/// Range-fetch mode: rows per page (`0` = single-block; `>0` = paged segment).
	rf_rows_per_page: usize,
	/// Synthetic generator seed (published for reproducibility).
	seed: u64,
	/// Synthetic post-missingness target sample count.
	points: usize,
	/// Synthetic missingness fraction (`0.0..=1.0`).
	missingness: f64,
	/// Synthetic timestamp jitter fraction (`0.0..=1.0`).
	jitter: f64,
	/// Synthetic additive-noise amplitude (`>= 0`; `0` puts samples on truth).
	noise: f64,
	/// Analytic shape of the synthetic ground-truth signal.
	shape: SignalShape,
	/// Timestamp precision of the input file.
	precision: TimestampPrecision,
	/// Spline method requested of the adapter.
	spline: Spline,
	/// Output grid resolution.
	resolution: Resolution,
	/// Number of timed repetitions.
	reps: usize,
	/// Directory the JSON report is written under.
	out_dir: PathBuf,
	/// Optional explicit profile name (defaults to the input file stem).
	name: Option<String>,
	/// Also run the portable baseline suite (linear + forward-fill) and emit a
	/// comparison report.
	compare: bool,
	/// Also write a human-readable HTML report alongside the JSON artifact.
	html: bool,
}

/// Which input mode / workload a run drives. Exactly one is selected per
/// invocation (enforced in [`Cli::from_args`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
	/// Interpolate a numeric field from a line-protocol / TSBS file.
	LineProtocol,
	/// Interpolate the seeded synthetic generator (carries a known ground truth).
	Synthetic,
	/// The storage-backed point-lookup workload (seals a segment, times the read).
	PointLookup,
	/// The storage-backed range-fetch workload (seals a segment, times windowed reads).
	RangeFetch,
	/// The storage-backed compression workload (seals a segment, times a full decode).
	Compression,
	/// The downsample workload (reduces a generated series into grid-aligned buckets).
	Downsample,
}

/// What the parsed command line asks the program to do.
#[derive(Debug, Clone, PartialEq)]
enum Command {
	/// Print usage and exit successfully.
	Help,
	/// Run a benchmark with the given configuration. Boxed because [`Cli`] is large
	/// relative to the unit `Help` variant.
	Run(Box<Cli>),
}

impl Cli {
	/// Parse a benchmark invocation from raw CLI arguments (already stripped of
	/// the program name).
	///
	/// Accepts both `--key value` and `--key=value` forms. `--input` and
	/// `--field` are required; everything else has a sensible default. Returns
	/// [`Command::Help`] for `-h` / `--help`.
	///
	/// # Errors
	///
	/// Returns a human-readable message for an unknown flag, a flag missing its
	/// value, a malformed enum/number value, or a missing required argument.
	fn from_args(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
		// Synthetic-knob defaults come from the flagship knob set so the CLI never
		// hardcodes a second copy of those constants.
		let defaults = SyntheticParams::default();

		// Storage-workload knob defaults come from the flagship profiles so the CLI
		// never hardcodes a second copy of those constants.
		let pl_defaults = PointLookupParams::default();
		let rf_defaults = RangeFetchParams::default();
		let comp_defaults = CompressionParams::default();
		let ds_defaults = DownsampleParams::default();

		let mut input: Option<PathBuf> = None;
		let mut field: Option<String> = None;
		let mut synthetic = false;
		let mut point_lookup = false;
		let mut range_fetch = false;
		let mut compression = false;
		let mut downsample = false;
		let (mut comp_rows, mut comp_shape) = (comp_defaults.point_count, comp_defaults.value_shape);
		let (mut ds_points, mut ds_stride, mut ds_bucket) = (ds_defaults.point_count, ds_defaults.input_stride_secs, ds_defaults.bucket_resolution);
		let mut ds_aggs = ds_defaults.aggregations;
		let mut ds_parallel = ds_defaults.parallel_chunks.max(1);
		let mut irregular = false;
		let (mut pl_rows, mut pl_queries, mut pl_absent, mut pl_mode, mut pl_rows_per_page) = (pl_defaults.point_count, pl_defaults.query_count, pl_defaults.absent_fraction, pl_defaults.mode, pl_defaults.rows_per_page);
		let (mut rf_rows, mut rf_window, mut rf_windows, mut rf_rows_per_page) = (rf_defaults.point_count, rf_defaults.window_rows, rf_defaults.window_count, rf_defaults.rows_per_page);
		let mut seed = defaults.seed;
		let mut points = defaults.input_points;
		let mut missingness = defaults.missingness_fraction;
		let mut jitter = defaults.jitter_fraction;
		let mut noise = defaults.noise_amplitude;
		let mut shape = defaults.signal_shape;
		let mut precision = TimestampPrecision::Nanoseconds;
		let mut spline = Spline::Cubic;
		let mut resolution = Resolution::Seconds;
		let mut reps: usize = 10;
		let mut out_dir = PathBuf::from("reports").join("json");
		let mut name: Option<String> = None;
		let mut compare = false;
		let mut html = false;

		let mut iter = args.into_iter();
		while let Some(token) = iter.next() {
			// Split `--key=value` once; otherwise treat the next token as the value.
			let (key, inline) = match token.split_once('=') {
				Some((k, v)) => (k.to_string(), Some(v.to_string())),
				None => (token.clone(), None),
			};

			// A small helper to obtain the value for a flag, preferring an inline
			// `=value` and otherwise consuming the next token.
			let mut take_value = |k: &str| -> Result<String, String> {
				if let Some(v) = inline.clone() {
					return Ok(v);
				}
				iter.next().ok_or_else(|| format!("flag `{k}` requires a value"))
			};

			match key.as_str() {
				"-h" | "--help" => return Ok(Command::Help),
				"-i" | "--input" => input = Some(PathBuf::from(take_value(&key)?)),
				"-f" | "--field" => field = Some(take_value(&key)?),
				"-s" | "--synthetic" => synthetic = true,
				"-p" | "--point-lookup" => point_lookup = true,
				"-r" | "--range-fetch" => range_fetch = true,
				"-z" | "--compression" => compression = true,
				"--comp-rows" => comp_rows = parse_points_count(&take_value(&key)?)?,
				"--comp-shape" => comp_shape = parse_value_shape(&take_value(&key)?)?,
				"-d" | "--downsample" => downsample = true,
				"--ds-points" => ds_points = parse_points_count(&take_value(&key)?)?,
				"--ds-stride" => ds_stride = parse_stride_secs(&take_value(&key)?)?,
				"--ds-bucket" => ds_bucket = parse_resolution(&take_value(&key)?)?,
				"--ds-aggs" => ds_aggs = parse_aggregations(&take_value(&key)?)?,
				"--ds-parallel" => ds_parallel = take_value(&key)?.parse::<usize>().map_err(|_| "--ds-parallel must be a positive integer".to_string())?.max(1),
				"--irregular" | "--pl-irregular" | "--rf-irregular" => irregular = true,
				"--pl-rows" => pl_rows = parse_points_count(&take_value(&key)?)?,
				"--pl-queries" => pl_queries = parse_query_count(&take_value(&key)?)?,
				"--pl-absent" => pl_absent = parse_fraction(&take_value(&key)?, "pl-absent")?,
				"--pl-mode" => pl_mode = parse_lookup_mode(&take_value(&key)?)?,
				"--pl-rows-per-page" => pl_rows_per_page = parse_rows_per_page(&take_value(&key)?)?,
				"--rf-rows" => rf_rows = parse_points_count(&take_value(&key)?)?,
				"--rf-window" => rf_window = parse_query_count(&take_value(&key)?)?,
				"--rf-windows" => rf_windows = parse_query_count(&take_value(&key)?)?,
				"--rf-rows-per-page" => rf_rows_per_page = parse_rows_per_page(&take_value(&key)?)?,
				"--seed" => seed = parse_seed(&take_value(&key)?)?,
				"--points" => points = parse_points_count(&take_value(&key)?)?,
				"--missingness" => missingness = parse_fraction(&take_value(&key)?, "missingness")?,
				"--jitter" => jitter = parse_fraction(&take_value(&key)?, "jitter")?,
				"--noise" => noise = parse_noise(&take_value(&key)?)?,
				"--shape" => shape = parse_shape(&take_value(&key)?)?,
				"--precision" => precision = parse_precision(&take_value(&key)?)?,
				"--spline" => spline = parse_spline(&take_value(&key)?)?,
				"--resolution" => resolution = parse_resolution(&take_value(&key)?)?,
				"--reps" => reps = parse_reps(&take_value(&key)?)?,
				"--out-dir" => out_dir = PathBuf::from(take_value(&key)?),
				"--name" => name = Some(take_value(&key)?),
				"-c" | "--compare" => compare = true,
				"--html" => html = true,
				other if other.starts_with('-') => return Err(format!("unknown flag `{other}`")),
				// A bare positional is taken as the input path if one is not set yet.
				other => {
					if input.is_some() {
						return Err(format!("unexpected argument `{other}`"));
					}
					input = Some(PathBuf::from(other));
				}
			}
		}

		// Exactly one workload mode may be selected; the rest default to line protocol.
		let mode = select_workload_mode(&[(synthetic, InputMode::Synthetic), (point_lookup, InputMode::PointLookup), (range_fetch, InputMode::RangeFetch), (compression, InputMode::Compression), (downsample, InputMode::Downsample)])?;
		validate_mode(mode, input.as_ref(), field.as_deref(), compare)?;

		Ok(Command::Run(Box::new(Self { input, field, mode, comp_rows, comp_shape, ds_points, ds_stride, ds_bucket, ds_aggs, ds_parallel, irregular, pl_rows, pl_queries, pl_absent, pl_mode, pl_rows_per_page, rf_rows, rf_window, rf_windows, rf_rows_per_page, seed, points, missingness, jitter, noise, shape, precision, spline, resolution, reps, out_dir, name, compare, html })))
	}
}

/// Select the single workload mode from the `(flag_set, mode)` candidates, defaulting
/// to line protocol when none is set and rejecting more than one.
fn select_workload_mode(candidates: &[(bool, InputMode)]) -> Result<InputMode, String> {
	let selected: Vec<InputMode> = candidates.iter().filter(|(set, _)| *set).map(|(_, m)| *m).collect();
	if selected.len() > 1 {
		return Err("only one workload mode may be given (--synthetic / --point-lookup / --range-fetch / --compression / --downsample)".to_string());
	}
	Ok(selected.first().copied().unwrap_or(InputMode::LineProtocol))
}

/// The flag name for a self-generating workload mode — one that produces its own
/// corpus and so rejects every external-input flag (`--input`/`--field`) and
/// `--compare` — or `None` for line protocol and synthetic (whose validation differs).
const fn self_generating_mode_flag(mode: InputMode) -> Option<&'static str> {
	match mode {
		InputMode::PointLookup => Some("--point-lookup"),
		InputMode::RangeFetch => Some("--range-fetch"),
		InputMode::Compression => Some("--compression"),
		InputMode::Downsample => Some("--downsample"),
		InputMode::LineProtocol | InputMode::Synthetic => None,
	}
}

/// Validate the selected [`InputMode`] against the input flags, rejecting every
/// conflicting combination.
///
/// The storage workloads (point-lookup, range-fetch, compression) seal their own
/// corpus, so they reject every interpolation input flag (and `--compare`, which has
/// no meaning for them); synthetic mode generates its own data, so an input file or
/// `--field` projection is a conflict; line-protocol mode requires both `--input` and
/// `--field`. The "exactly one workload mode" check is the caller's, which keeps this
/// free of the mode-selector booleans.
fn validate_mode(mode: InputMode, input: Option<&PathBuf>, field: Option<&str>, compare: bool) -> Result<(), String> {
	if let Some(mode_flag) = self_generating_mode_flag(mode) {
		if input.is_some() {
			return Err(format!("`{mode_flag}` cannot be combined with an input file"));
		}
		if field.is_some() {
			return Err(format!("`--field` has no meaning in `{mode_flag}` mode"));
		}
		if compare {
			return Err(format!("`--compare` has no meaning in `{mode_flag}` mode"));
		}
		return Ok(());
	}
	match mode {
		InputMode::Synthetic => {
			if input.is_some() {
				return Err("`--synthetic` cannot be combined with an input file".to_string());
			}
			if field.is_some() {
				return Err("`--field` has no meaning in `--synthetic` mode".to_string());
			}
		}
		InputMode::LineProtocol => {
			if input.is_none() {
				return Err("missing required `--input <file.lp>` (or pass `--synthetic` / `--point-lookup` / `--range-fetch` / `--compression` / `--downsample`)".to_string());
			}
			if field.is_none() {
				return Err("missing required `--field <name>`".to_string());
			}
		}
		InputMode::PointLookup | InputMode::RangeFetch | InputMode::Compression | InputMode::Downsample => unreachable!("handled by self_generating_mode_flag above"),
	}
	Ok(())
}

/// Parse a timestamp-precision token (`ns`/`us`/`ms`/`s` and long forms).
fn parse_precision(s: &str) -> Result<TimestampPrecision, String> {
	match s.to_ascii_lowercase().as_str() {
		"ns" | "nanoseconds" | "nanos" => Ok(TimestampPrecision::Nanoseconds),
		"us" | "µs" | "microseconds" | "micros" => Ok(TimestampPrecision::Microseconds),
		"ms" | "milliseconds" | "millis" => Ok(TimestampPrecision::Milliseconds),
		"s" | "sec" | "secs" | "seconds" => Ok(TimestampPrecision::Seconds),
		other => Err(format!("invalid precision `{other}` (expected ns|us|ms|s)")),
	}
}

/// Parse a spline token. `polynomial`/`poly` accepts an optional `:<degree>`
/// suffix (e.g. `poly:4`); without one it defaults to degree 3.
fn parse_spline(s: &str) -> Result<Spline, String> {
	let lower = s.to_ascii_lowercase();
	if let Some(rest) = lower.strip_prefix("polynomial").or_else(|| lower.strip_prefix("poly")) {
		let degree = if rest.is_empty() {
			3
		} else {
			let digits = rest.strip_prefix(':').unwrap_or(rest);
			digits.parse::<usize>().map_err(|_| format!("invalid polynomial degree in `{s}`"))?
		};
		return Ok(Spline::Polynomial(degree, None));
	}
	match lower.as_str() {
		"linear" => Ok(Spline::Linear),
		"quadratic" | "quad" => Ok(Spline::Quadratic),
		"cubic" => Ok(Spline::Cubic),
		other => Err(format!("invalid spline `{other}` (expected linear|quadratic|cubic|poly[:N])")),
	}
}

/// Parse an output-grid resolution token.
fn parse_resolution(s: &str) -> Result<Resolution, String> {
	match s.to_ascii_lowercase().as_str() {
		"ns" | "nanoseconds" => Ok(Resolution::Nanoseconds),
		"us" | "µs" | "microseconds" => Ok(Resolution::Microseconds),
		"ms" | "milliseconds" => Ok(Resolution::Milliseconds),
		"s" | "sec" | "secs" | "seconds" => Ok(Resolution::Seconds),
		"m" | "min" | "mins" | "minutes" => Ok(Resolution::Minutes),
		"h" | "hour" | "hours" => Ok(Resolution::Hours),
		"d" | "day" | "days" => Ok(Resolution::Days),
		"w" | "week" | "weeks" => Ok(Resolution::Weeks),
		"mo" | "month" | "months" => Ok(Resolution::Months),
		"y" | "year" | "years" => Ok(Resolution::Years),
		other => Err(format!("invalid resolution `{other}` (expected ns|us|ms|s|m|h|d|w|mo|y)")),
	}
}

/// Parse the repetition count, rejecting zero (the harness requires `reps > 0`).
fn parse_reps(s: &str) -> Result<usize, String> {
	let n = s.parse::<usize>().map_err(|_| format!("invalid --reps `{s}` (expected a positive integer)"))?;
	if n == 0 {
		return Err("--reps must be > 0".to_string());
	}
	Ok(n)
}

/// Parse a synthetic seed, accepting either a decimal or a `0x`-prefixed hex
/// literal (seeds are often published in hex).
fn parse_seed(s: &str) -> Result<u64, String> {
	let parsed = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).map_or_else(|| s.parse::<u64>(), |hex| u64::from_str_radix(hex, 16));
	parsed.map_err(|_| format!("invalid --seed `{s}` (expected a u64, decimal or 0x-hex)"))
}

/// Parse a synthetic sample count, requiring at least two points (a spline needs
/// two points to interpolate between).
fn parse_points_count(s: &str) -> Result<usize, String> {
	let n = s.parse::<usize>().map_err(|_| format!("invalid --points `{s}` (expected a positive integer)"))?;
	if n < 2 {
		return Err("--points must be >= 2".to_string());
	}
	Ok(n)
}

/// Parse the point-lookup query-batch size, requiring at least one instant.
fn parse_query_count(s: &str) -> Result<usize, String> {
	let n = s.parse::<usize>().map_err(|_| format!("invalid --pl-queries `{s}` (expected a positive integer)"))?;
	if n == 0 {
		return Err("--pl-queries must be >= 1".to_string());
	}
	Ok(n)
}

/// Parse a rows-per-page value for the storage workloads. `0` (single-block) is
/// allowed; any positive count seals a paged segment.
fn parse_rows_per_page(s: &str) -> Result<usize, String> {
	s.parse::<usize>().map_err(|_| format!("invalid --*-rows-per-page `{s}` (expected a non-negative integer; 0 = single-block)"))
}

/// Parse a comma-separated list of downsample aggregation tokens (e.g.
/// `min,max,p99`) into reductions, rejecting an unknown token or an empty list.
fn parse_aggregations(s: &str) -> Result<Vec<Aggregation>, String> {
	let aggs: Vec<Aggregation> = s.split(',').map(str::trim).filter(|t| !t.is_empty()).map(|t| Aggregation::from_token(t).ok_or_else(|| format!("invalid --ds-aggs token `{t}` (use min/max/avg/sum/first/last/p50/p90/p95/p99/twa/twa_linear/twa_bucket_end/sketch_p50/sketch_p90/sketch_p95/sketch_p99)"))).collect::<Result<_, _>>()?;
	if aggs.is_empty() {
		return Err("--ds-aggs must name at least one reduction".to_string());
	}
	Ok(aggs)
}

/// Parse a downsample input-stride value (seconds between samples, `>= 1`).
fn parse_stride_secs(s: &str) -> Result<i64, String> {
	let n = s.parse::<i64>().map_err(|_| format!("invalid --ds-stride `{s}` (expected a positive integer of seconds)"))?;
	if n < 1 {
		return Err("--ds-stride must be >= 1".to_string());
	}
	Ok(n)
}

/// Parse a compression value-shape token (`clustered` | `trending` | `jitter`).
fn parse_value_shape(s: &str) -> Result<ValueShape, String> {
	match s.to_ascii_lowercase().as_str() {
		"clustered" | "cluster" | "for" => Ok(ValueShape::Clustered),
		"trending" | "trend" | "counter" => Ok(ValueShape::Trending),
		"jitter" | "jittery" | "noise" => Ok(ValueShape::Jitter),
		other => Err(format!("invalid --comp-shape `{other}` (expected clustered|trending|jitter)")),
	}
}

/// Parse a point-lookup issue mode token (`single` | `batch`).
fn parse_lookup_mode(s: &str) -> Result<LookupMode, String> {
	match s.to_ascii_lowercase().as_str() {
		"single" | "singles" | "one" => Ok(LookupMode::Single),
		"batch" | "batched" => Ok(LookupMode::Batch),
		other => Err(format!("invalid --pl-mode `{other}` (expected single|batch)")),
	}
}

/// Parse a fraction in `[0.0, 1.0]` for `--missingness` / `--jitter`.
fn parse_fraction(s: &str, flag: &str) -> Result<f64, String> {
	let v = s.parse::<f64>().map_err(|_| format!("invalid --{flag} `{s}` (expected a number in [0, 1])"))?;
	if !(0.0..=1.0).contains(&v) {
		return Err(format!("--{flag} must be in [0, 1], got {v}"));
	}
	Ok(v)
}

/// Parse the `--noise` amplitude: any finite, non-negative number (`0` puts every
/// synthetic sample exactly on the analytic ground truth).
fn parse_noise(s: &str) -> Result<f64, String> {
	let v = s.parse::<f64>().map_err(|_| format!("invalid --noise `{s}` (expected a non-negative number)"))?;
	if !v.is_finite() || v < 0.0 {
		return Err(format!("--noise must be finite and >= 0, got {v}"));
	}
	Ok(v)
}

/// Parse a synthetic signal-shape token (case-insensitive). Selects the analytic
/// ground-truth curve the generator samples in `--synthetic` mode.
fn parse_shape(s: &str) -> Result<SignalShape, String> {
	match s.to_ascii_lowercase().replace(['-', '_'], "").as_str() {
		"multisine" | "sine" | "sines" => Ok(SignalShape::MultiSine),
		"sawtooth" | "saw" | "ramp" => Ok(SignalShape::Sawtooth),
		"step" | "square" => Ok(SignalShape::Step),
		"dampedsine" | "damped" | "decay" => Ok(SignalShape::DampedSine),
		other => Err(format!("invalid shape `{other}` (expected multisine|sawtooth|step|dampedsine)")),
	}
}

/// Usage text shown for `--help` and on a parse error.
const USAGE: &str = "\
dsp-bench — run a DSP interpolation benchmark over a line-protocol file or the
seeded synthetic generator

USAGE:
    dsp-bench --input <FILE.lp> --field <NAME> [OPTIONS]
    dsp-bench <FILE.lp> --field <NAME> [OPTIONS]
    dsp-bench --synthetic [OPTIONS]
    dsp-bench --point-lookup [POINT-LOOKUP OPTIONS]
    dsp-bench --range-fetch [RANGE-FETCH OPTIONS]
    dsp-bench --compression [COMPRESSION OPTIONS]
    dsp-bench --downsample [DOWNSAMPLE OPTIONS]

INPUT MODE (choose one):
    -i, --input <FILE>       Line-protocol input file (.lp / TSBS payload)
    -f, --field <NAME>       Numeric field to interpolate (with --input)
    -s, --synthetic          Generate a seeded synthetic series instead. Only this
                             mode has a known ground truth, so only it reports
                             accuracy (RMSE/MAE/max-error/bias).
    -p, --point-lookup       Run the storage-backed point-lookup workload instead of
                             interpolation: seal a seeded columnar segment and time
                             DSP's streaming point read (p50/p95/p99 + bytes/point).
                             No ground truth, so no accuracy — the streaming read is
                             gated against the full-decode value.
    -r, --range-fetch        Run the storage-backed range-fetch workload: seal a
                             seeded columnar segment and time DSP's windowed range
                             read (only the rows in each window are decoded). Gated
                             against a full decode filtered to the window.
    -z, --compression        Run the storage-backed compression workload: seal a
                             seeded columnar segment and report realized bytes/point,
                             the value-column compression ratio, and decode throughput
                             (time a full decode). Gated on an exact round-trip.
    -d, --downsample         Run the downsample (aggregation) workload: generate a
                             dense series and time DSP's canonical reduction (min/max/
                             avg/sum/first/last) into grid-aligned buckets. Gated on
                             the reduction being total (bucket counts sum to input).

STORAGE-WORKLOAD OPTIONS (with --point-lookup or --range-fetch):
        --irregular          Jittered timestamps (decode + search/filter) instead of
                             the constant-stride regular (closed-form) corpus

POINT-LOOKUP OPTIONS (with --point-lookup):
        --pl-rows <N>        Rows sealed into the segment (>=2)     [default: 20000]
        --pl-queries <N>     Instants resolved per rep (>=1)           [default: 128]
        --pl-absent <F>      Off-grid-miss fraction in [0, 1]          [default: 0.25]
        --pl-mode <M>        Issue mode: single|batch                [default: batch]
                             (batch amortizes the timestamp decode across the batch)
        --pl-rows-per-page <N>  Rows/page; 0 = single-block, >0 = paged  [default: 0]
                             (a paged segment exercises the page-pruning read path)

RANGE-FETCH OPTIONS (with --range-fetch):
        --rf-rows <N>        Rows sealed into the segment (>=2)     [default: 20000]
        --rf-window <N>      Rows spanned by each fetched window (>=1)  [default: 100]
        --rf-windows <N>     Windows fetched per rep (>=1)              [default: 32]
        --rf-rows-per-page <N>  Rows/page; 0 = single-block, >0 = paged  [default: 0]
                             (a paged segment exercises the page-skipping range read)

COMPRESSION OPTIONS (with --compression):
        --comp-rows <N>      Rows sealed into the segment (>=2)     [default: 20000]
        --comp-shape <S>     Value shape: clustered|trending|jitter
                             (each exercises a different codec) [default: clustered]

DOWNSAMPLE OPTIONS (with --downsample):
        --ds-points <N>      Input sample count (>=2)              [default: 60000]
        --ds-parallel <N>    Reduce in N parallel chunks (partials merged);
                             the merged result is identical to serial [default: 1]
        --ds-stride <N>      Seconds between input samples (>=1)        [default: 1]
        --ds-bucket <R>      Bucket resolution: s|m|h|d|w|mo|y     [default: minutes]
        --ds-aggs <LIST>     Reductions, comma-separated: min,max,avg,sum,first,
                             last,p50,p90,p95,p99          [default: min,max,avg,
                             sum,first,last]

SYNTHETIC OPTIONS (with --synthetic):
        --seed <N>           Generator seed, decimal or 0x-hex     [default: flagship]
        --points <N>         Sample count (>=2)                    [default: 480]
        --missingness <F>    Gap fraction in [0, 1]                [default: 0.20]
        --jitter <F>         Timestamp jitter fraction in [0, 1]   [default: 0.60]
        --noise <F>          Sample noise amplitude (>=0; 0=clean) [default: 2.0]
        --shape <S>          Ground-truth signal: multisine|sawtooth|step|
                             dampedsine                       [default: multisine]

OPTIONS:
        --precision <P>      Timestamp precision: ns|us|ms|s         [default: ns]
        --spline <S>         Spline: linear|quadratic|cubic|poly[:N] [default: cubic]
        --resolution <R>     Output grid: ns|us|ms|s|m|h|d|w|mo|y    [default: s]
        --reps <N>           Timed repetitions (>0)                  [default: 10]
        --out-dir <DIR>      Report output directory          [default: reports/json]
        --name <NAME>        Profile name      [default: input stem / flagship name]
    -c, --compare            Also run the portable baseline suite (linear +
                             forward-fill) for comparison
        --html               Also write a human-readable HTML report alongside
                             the JSON artifact
    -h, --help               Print this help

The report is written as <out-dir>/<profile>__<adapters>.json (e.g.
`__dsp.json`, or `__dsp+baseline-linear+baseline-forward-fill.json` under
--compare) and the process exits non-zero if any run's correctness gate does not
pass.
";

#[cfg(test)]
mod tests {
	use super::*;

	fn run_cli(args: &[&str]) -> Result<Command, String> {
		Cli::from_args(args.iter().map(ToString::to_string))
	}

	fn expect_run(args: &[&str]) -> Cli {
		match run_cli(args).expect("args should parse") {
			Command::Run(cli) => *cli,
			Command::Help => panic!("expected a run, got help"),
		}
	}

	#[test]
	fn parses_required_flags_with_defaults() {
		let cli = expect_run(&["--input", "data.lp", "--field", "usage"]);
		assert_eq!(cli.input, Some(PathBuf::from("data.lp")));
		assert_eq!(cli.field, Some("usage".to_string()));
		assert_eq!(cli.mode, InputMode::LineProtocol, "line-protocol is the default mode");
		assert_eq!(cli.precision, TimestampPrecision::Nanoseconds);
		assert_eq!(cli.spline, Spline::Cubic);
		assert_eq!(cli.resolution, Resolution::Seconds);
		assert_eq!(cli.reps, 10);
		assert_eq!(cli.out_dir, PathBuf::from("reports").join("json"));
		assert_eq!(cli.name, None);
		assert!(!cli.compare, "comparison is off unless requested");
	}

	#[test]
	fn synthetic_mode_needs_no_input_and_carries_knob_defaults() {
		let cli = expect_run(&["--synthetic"]);
		assert_eq!(cli.mode, InputMode::Synthetic);
		assert_eq!(cli.input, None);
		assert_eq!(cli.field, None);
		// Knob defaults mirror the flagship profile.
		let flagship = InterpolationProfile::interpolation_heavy_irregular();
		assert_eq!(cli.seed, flagship.seed);
		assert_eq!(cli.points, flagship.input_points);
		assert!((cli.missingness - flagship.missingness_fraction).abs() < f64::EPSILON);
		assert!((cli.jitter - flagship.jitter_fraction).abs() < f64::EPSILON);
		assert!((cli.noise - flagship.noise_amplitude).abs() < f64::EPSILON);
	}

	#[test]
	fn synthetic_knobs_parse_including_hex_seed() {
		let cli = expect_run(&["-s", "--seed", "0xBEEF", "--points", "120", "--missingness", "0.1", "--jitter", "0.0"]);
		assert_eq!(cli.mode, InputMode::Synthetic);
		assert_eq!(cli.seed, 0xBEEF);
		assert_eq!(cli.points, 120);
		assert!((cli.missingness - 0.1).abs() < 1e-12);
		assert!((cli.jitter - 0.0).abs() < f64::EPSILON);
	}

	#[test]
	fn point_lookup_mode_needs_no_input_and_carries_knob_defaults() {
		let cli = expect_run(&["--point-lookup"]);
		assert_eq!(cli.mode, InputMode::PointLookup);
		assert_eq!(cli.input, None);
		assert_eq!(cli.field, None);
		// Knob defaults mirror the flagship point-lookup profile.
		let pl = PointLookupParams::default();
		assert_eq!(cli.pl_rows, pl.point_count);
		assert_eq!(cli.pl_queries, pl.query_count);
		assert!(!cli.irregular, "regular (closed-form) is the default corpus");
		assert!((cli.pl_absent - pl.absent_fraction).abs() < f64::EPSILON);
		assert_eq!(cli.pl_mode, pl.mode);
	}

	#[test]
	fn point_lookup_knobs_parse_including_mode_and_irregular() {
		let cli = expect_run(&["-p", "--pl-rows", "5000", "--pl-queries", "256", "--irregular", "--pl-absent", "0.5", "--pl-mode", "single"]);
		assert_eq!(cli.mode, InputMode::PointLookup);
		assert_eq!(cli.pl_rows, 5_000);
		assert_eq!(cli.pl_queries, 256);
		assert!(cli.irregular);
		assert!((cli.pl_absent - 0.5).abs() < 1e-12);
		assert_eq!(cli.pl_mode, LookupMode::Single, "single mode parsed");
	}

	#[test]
	fn range_fetch_mode_needs_no_input_and_carries_knob_defaults() {
		let cli = expect_run(&["--range-fetch"]);
		assert_eq!(cli.mode, InputMode::RangeFetch);
		assert_eq!(cli.input, None);
		assert_eq!(cli.field, None);
		let rf = RangeFetchParams::default();
		assert_eq!(cli.rf_rows, rf.point_count);
		assert_eq!(cli.rf_window, rf.window_rows);
		assert_eq!(cli.rf_windows, rf.window_count);
		assert!(!cli.irregular, "regular is the default corpus");
	}

	#[test]
	fn range_fetch_knobs_parse() {
		let cli = expect_run(&["-r", "--rf-rows", "8000", "--rf-window", "250", "--rf-windows", "48", "--irregular"]);
		assert_eq!(cli.mode, InputMode::RangeFetch);
		assert_eq!(cli.rf_rows, 8_000);
		assert_eq!(cli.rf_window, 250);
		assert_eq!(cli.rf_windows, 48);
		assert!(cli.irregular);
	}

	#[test]
	fn rows_per_page_parses_zero_and_positive_for_both_storage_workloads() {
		// The default is single-block (0) for both.
		let pl = PointLookupParams::default();
		let rf = RangeFetchParams::default();
		assert_eq!(expect_run(&["-p"]).pl_rows_per_page, pl.rows_per_page);
		assert_eq!(expect_run(&["-r"]).rf_rows_per_page, rf.rows_per_page);
		// A positive value seals a paged segment; 0 is explicitly allowed.
		assert_eq!(expect_run(&["-p", "--pl-rows-per-page", "512"]).pl_rows_per_page, 512);
		assert_eq!(expect_run(&["-r", "--rf-rows-per-page", "1024"]).rf_rows_per_page, 1_024);
		assert_eq!(expect_run(&["-p", "--pl-rows-per-page", "0"]).pl_rows_per_page, 0);
		assert!(run_cli(&["-p", "--pl-rows-per-page", "-1"]).unwrap_err().contains("rows-per-page"));
	}

	#[test]
	fn range_fetch_conflicts_are_rejected() {
		assert!(run_cli(&["--range-fetch", "--point-lookup"]).unwrap_err().contains("only one workload mode"));
		assert!(run_cli(&["--range-fetch", "--synthetic"]).unwrap_err().contains("only one workload mode"));
		assert!(run_cli(&["--range-fetch", "--input", "x.lp"]).unwrap_err().contains("cannot be combined with an input file"));
		assert!(run_cli(&["--range-fetch", "--compare"]).unwrap_err().contains("`--compare` has no meaning"));
	}

	#[test]
	fn downsample_mode_parses_with_knob_defaults_and_conflicts_rejected() {
		let cli = expect_run(&["--downsample"]);
		assert_eq!(cli.mode, InputMode::Downsample);
		let ds = DownsampleParams::default();
		assert_eq!(cli.ds_points, ds.point_count);
		assert_eq!(cli.ds_stride, ds.input_stride_secs);
		assert_eq!(cli.ds_bucket, ds.bucket_resolution);
		// Knobs parse, including the bucket resolution.
		let cli = expect_run(&["-d", "--ds-points", "5000", "--ds-stride", "5", "--ds-bucket", "h"]);
		assert_eq!(cli.ds_points, 5_000);
		assert_eq!(cli.ds_stride, 5);
		assert_eq!(cli.ds_bucket, Resolution::Hours);
		assert!(run_cli(&["-d", "--ds-stride", "0"]).unwrap_err().contains("--ds-stride must be >= 1"));
		// Aggregations parse from a comma list (including percentiles) and reject unknowns.
		assert_eq!(cli.ds_aggs, Aggregation::ALL.to_vec(), "default is the six streaming reductions");
		assert_eq!(expect_run(&["-d", "--ds-aggs", "min,max,p99"]).ds_aggs, vec![Aggregation::Min, Aggregation::Max, Aggregation::P99]);
		assert!(run_cli(&["-d", "--ds-aggs", "min,bogus"]).unwrap_err().contains("invalid --ds-aggs token"));
		assert!(run_cli(&["-d", "--ds-aggs", " "]).unwrap_err().contains("at least one reduction"));
		// Conflicts: another mode, an input file, and --compare.
		assert!(run_cli(&["--downsample", "--compression"]).unwrap_err().contains("only one workload mode"));
		assert!(run_cli(&["--downsample", "--input", "x.lp"]).unwrap_err().contains("cannot be combined with an input file"));
		assert!(run_cli(&["--downsample", "--compare"]).unwrap_err().contains("`--compare` has no meaning"));
	}

	#[test]
	fn compression_mode_parses_with_knob_defaults_and_conflicts_rejected() {
		let cli = expect_run(&["--compression"]);
		assert_eq!(cli.mode, InputMode::Compression);
		let comp = CompressionParams::default();
		assert_eq!(cli.comp_rows, comp.point_count);
		assert_eq!(cli.comp_shape, comp.value_shape);
		// Knobs parse, including every shape alias.
		let cli = expect_run(&["-z", "--comp-rows", "7000", "--comp-shape", "trending"]);
		assert_eq!(cli.comp_rows, 7_000);
		assert_eq!(cli.comp_shape, ValueShape::Trending);
		assert_eq!(expect_run(&["-z", "--comp-shape", "jitter"]).comp_shape, ValueShape::Jitter);
		assert!(run_cli(&["-z", "--comp-shape", "triangle"]).unwrap_err().contains("invalid --comp-shape"));
		// Conflicts: another mode, an input file, and --compare.
		assert!(run_cli(&["--compression", "--synthetic"]).unwrap_err().contains("only one workload mode"));
		assert!(run_cli(&["--compression", "--input", "x.lp"]).unwrap_err().contains("cannot be combined with an input file"));
		assert!(run_cli(&["--compression", "--compare"]).unwrap_err().contains("`--compare` has no meaning"));
	}

	#[test]
	fn range_fetch_knob_bounds_are_enforced() {
		assert!(run_cli(&["-r", "--rf-rows", "1"]).unwrap_err().contains(">= 2"));
		assert!(run_cli(&["-r", "--rf-window", "0"]).unwrap_err().contains(">= 1"));
		assert!(run_cli(&["-r", "--rf-windows", "0"]).unwrap_err().contains(">= 1"));
	}

	#[test]
	fn point_lookup_mode_flag_parses_single_and_batch() {
		assert_eq!(expect_run(&["-p", "--pl-mode", "single"]).pl_mode, LookupMode::Single);
		assert_eq!(expect_run(&["-p", "--pl-mode", "batch"]).pl_mode, LookupMode::Batch);
		assert_eq!(expect_run(&["-p", "--pl-mode=Batch"]).pl_mode, LookupMode::Batch);
		assert!(run_cli(&["-p", "--pl-mode", "triple"]).unwrap_err().contains("invalid --pl-mode"));
	}

	#[test]
	fn point_lookup_conflicts_are_rejected() {
		assert!(run_cli(&["--point-lookup", "--synthetic"]).unwrap_err().contains("only one workload mode"));
		assert!(run_cli(&["--point-lookup", "--input", "x.lp"]).unwrap_err().contains("cannot be combined with an input file"));
		assert!(run_cli(&["--point-lookup", "--field", "v"]).unwrap_err().contains("no meaning"));
		assert!(run_cli(&["--point-lookup", "--compare"]).unwrap_err().contains("`--compare` has no meaning"));
	}

	#[test]
	fn point_lookup_knob_bounds_are_enforced() {
		assert!(run_cli(&["-p", "--pl-rows", "1"]).unwrap_err().contains(">= 2"));
		assert!(run_cli(&["-p", "--pl-queries", "0"]).unwrap_err().contains(">= 1"));
		assert!(run_cli(&["-p", "--pl-absent", "1.5"]).unwrap_err().contains("[0, 1]"));
	}

	#[test]
	fn synthetic_conflicts_with_input_and_field_are_rejected() {
		assert!(run_cli(&["--synthetic", "--input", "x.lp"]).unwrap_err().contains("cannot be combined"));
		assert!(run_cli(&["--synthetic", "data.lp"]).unwrap_err().contains("cannot be combined"));
		assert!(run_cli(&["--synthetic", "--field", "v"]).unwrap_err().contains("no meaning"));
	}

	#[test]
	fn synthetic_knob_bounds_are_enforced() {
		assert!(run_cli(&["-s", "--points", "1"]).unwrap_err().contains(">= 2"));
		assert!(run_cli(&["-s", "--missingness", "1.5"]).unwrap_err().contains("[0, 1]"));
		assert!(run_cli(&["-s", "--jitter", "-0.1"]).unwrap_err().contains("[0, 1]"));
		assert!(run_cli(&["-s", "--seed", "notanumber"]).unwrap_err().contains("--seed"));
		assert!(run_cli(&["-s", "--noise", "-1"]).unwrap_err().contains("--noise"));
	}

	#[test]
	fn noise_knob_accepts_zero_and_amplitudes_above_one() {
		// Unlike the [0, 1] fractions, noise is an unbounded non-negative amplitude.
		assert!((expect_run(&["-s", "--noise", "0"]).noise - 0.0).abs() < f64::EPSILON);
		assert!((expect_run(&["-s", "--noise", "5.5"]).noise - 5.5).abs() < 1e-12);
	}

	#[test]
	fn shape_defaults_to_multisine_and_parses_every_form() {
		// Default mirrors the flagship signal.
		assert_eq!(expect_run(&["-s"]).shape, SignalShape::MultiSine);
		// Each variant's canonical token and an alias, case- and separator-insensitive.
		assert_eq!(expect_run(&["-s", "--shape", "multisine"]).shape, SignalShape::MultiSine);
		assert_eq!(expect_run(&["-s", "--shape", "Sawtooth"]).shape, SignalShape::Sawtooth);
		assert_eq!(expect_run(&["-s", "--shape=saw"]).shape, SignalShape::Sawtooth);
		assert_eq!(expect_run(&["-s", "--shape", "step"]).shape, SignalShape::Step);
		assert_eq!(expect_run(&["-s", "--shape", "damped-sine"]).shape, SignalShape::DampedSine);
		assert_eq!(expect_run(&["-s", "--shape", "DECAY"]).shape, SignalShape::DampedSine);
	}

	#[test]
	fn an_unknown_shape_is_rejected() {
		assert!(run_cli(&["-s", "--shape", "triangle"]).unwrap_err().contains("invalid shape"));
	}

	#[test]
	fn compare_flag_is_parsed_in_both_forms() {
		assert!(expect_run(&["data.lp", "-f", "v", "--compare"]).compare);
		assert!(expect_run(&["data.lp", "-f", "v", "-c"]).compare);
		assert!(!expect_run(&["data.lp", "-f", "v"]).compare);
	}

	#[test]
	fn html_flag_is_off_by_default_and_parsed_when_present() {
		assert!(!expect_run(&["data.lp", "-f", "v"]).html, "html is off unless requested");
		assert!(expect_run(&["data.lp", "-f", "v", "--html"]).html);
		assert!(expect_run(&["-s", "--html"]).html);
	}

	#[test]
	fn accepts_short_flags_and_positional_input() {
		let cli = expect_run(&["data.lp", "-f", "usage"]);
		assert_eq!(cli.input, Some(PathBuf::from("data.lp")));
		assert_eq!(cli.field, Some("usage".to_string()));

		let cli = expect_run(&["-i", "x.lp", "-f", "v"]);
		assert_eq!(cli.input, Some(PathBuf::from("x.lp")));
	}

	#[test]
	fn accepts_key_equals_value_form() {
		let cli = expect_run(&["--input=data.lp", "--field=usage", "--reps=25", "--spline=linear"]);
		assert_eq!(cli.input, Some(PathBuf::from("data.lp")));
		assert_eq!(cli.field, Some("usage".to_string()));
		assert_eq!(cli.reps, 25);
		assert_eq!(cli.spline, Spline::Linear);
	}

	#[test]
	fn parses_every_precision_form() {
		for (tok, want) in [("ns", TimestampPrecision::Nanoseconds), ("US", TimestampPrecision::Microseconds), ("ms", TimestampPrecision::Milliseconds), ("seconds", TimestampPrecision::Seconds)] {
			assert_eq!(parse_precision(tok).unwrap(), want, "precision {tok}");
		}
		assert!(parse_precision("fortnights").is_err());
	}

	#[test]
	fn parses_spline_forms_including_polynomial() {
		assert_eq!(parse_spline("Linear").unwrap(), Spline::Linear);
		assert_eq!(parse_spline("quad").unwrap(), Spline::Quadratic);
		assert_eq!(parse_spline("cubic").unwrap(), Spline::Cubic);
		assert_eq!(parse_spline("poly").unwrap(), Spline::Polynomial(3, None));
		assert_eq!(parse_spline("poly:5").unwrap(), Spline::Polynomial(5, None));
		assert_eq!(parse_spline("polynomial:2").unwrap(), Spline::Polynomial(2, None));
		assert!(parse_spline("poly:x").is_err());
		assert!(parse_spline("bezier").is_err());
	}

	#[test]
	fn parses_resolution_forms() {
		assert_eq!(parse_resolution("m").unwrap(), Resolution::Minutes);
		assert_eq!(parse_resolution("minutes").unwrap(), Resolution::Minutes);
		assert_eq!(parse_resolution("S").unwrap(), Resolution::Seconds);
		assert_eq!(parse_resolution("mo").unwrap(), Resolution::Months);
		assert_eq!(parse_resolution("days").unwrap(), Resolution::Days);
		assert!(parse_resolution("eons").is_err());
	}

	#[test]
	fn help_flag_short_circuits() {
		assert_eq!(run_cli(&["--help"]).unwrap(), Command::Help);
		assert_eq!(run_cli(&["-h", "--input", "x"]).unwrap(), Command::Help);
	}

	#[test]
	fn missing_required_arguments_error() {
		assert!(run_cli(&["--field", "usage"]).unwrap_err().contains("--input"));
		assert!(run_cli(&["--input", "data.lp"]).unwrap_err().contains("--field"));
	}

	#[test]
	fn rejects_unknown_flag_and_zero_reps_and_dangling_value() {
		assert!(run_cli(&["--bogus", "x"]).unwrap_err().contains("unknown flag"));
		assert!(run_cli(&["--input", "d.lp", "--field", "v", "--reps", "0"]).unwrap_err().contains("> 0"));
		assert!(run_cli(&["--input"]).unwrap_err().contains("requires a value"));
	}

	#[test]
	fn second_positional_is_rejected() {
		let err = run_cli(&["a.lp", "b.lp", "--field", "v"]).unwrap_err();
		assert!(err.contains("unexpected argument"), "got: {err}");
	}

	#[test]
	fn derive_profile_name_uses_file_stem() {
		assert_eq!(derive_profile_name(std::path::Path::new("cpu-usage.lp")), "cpu-usage");
		assert_eq!(derive_profile_name(std::path::Path::new("/data/sensors/temp.lp")), "temp");
	}
}
