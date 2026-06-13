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
use dsp_bench::{report::default_filename, run_profile, BaselineLinearAdapter, BenchReport, BenchResult, DspAdapter, ForwardFillAdapter, InterpolationProfile, RunMetadata, TimestampPrecision};
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
		Command::Run(cli) => cli,
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

/// Load the dataset, run the DSP interpolation profile, and persist the report.
async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
	let payload = std::fs::read_to_string(&cli.input).map_err(|e| anyhow::anyhow!("cannot read input file {}: {e}", cli.input.display()))?;

	let profile_name = cli.name.clone().unwrap_or_else(|| derive_profile_name(&cli.input));
	let profile = InterpolationProfile::from_line_protocol(profile_name, &payload, &cli.field, cli.precision, cli.spline, cli.resolution).map_err(|e| anyhow::anyhow!("cannot build a profile from {}: {e}", cli.input.display()))?;

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
	// The artifact name tags every adapter in the report (e.g. `dsp+baseline-linear`)
	// so a comparison and a solo run never collide on disk.
	let adapter_tag = results.iter().map(|r| r.adapter.as_str()).collect::<Vec<_>>().join("+");
	let report = BenchReport::with_results(metadata, results);

	// Persist before printing so a write failure surfaces as a non-zero exit even
	// if the summary already streamed.
	let out_path = cli.out_dir.join(default_filename(&report.results[0].profile, &adapter_tag));
	report.write_json(&out_path).map_err(|e| anyhow::anyhow!("cannot write report to {}: {e}", out_path.display()))?;

	print_summary(&report, &out_path);

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
#[derive(Debug, Clone, PartialEq)]
struct Cli {
	/// Path to the `.lp` / TSBS line-protocol input file.
	input: PathBuf,
	/// Numeric field to project onto the interpolated series.
	field: String,
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
}

/// What the parsed command line asks the program to do.
#[derive(Debug, Clone, PartialEq)]
enum Command {
	/// Print usage and exit successfully.
	Help,
	/// Run a benchmark with the given configuration.
	Run(Cli),
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
		let mut input: Option<PathBuf> = None;
		let mut field: Option<String> = None;
		let mut precision = TimestampPrecision::Nanoseconds;
		let mut spline = Spline::Cubic;
		let mut resolution = Resolution::Seconds;
		let mut reps: usize = 10;
		let mut out_dir = PathBuf::from("reports").join("json");
		let mut name: Option<String> = None;
		let mut compare = false;

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
				"--precision" => precision = parse_precision(&take_value(&key)?)?,
				"--spline" => spline = parse_spline(&take_value(&key)?)?,
				"--resolution" => resolution = parse_resolution(&take_value(&key)?)?,
				"--reps" => reps = parse_reps(&take_value(&key)?)?,
				"--out-dir" => out_dir = PathBuf::from(take_value(&key)?),
				"--name" => name = Some(take_value(&key)?),
				"-c" | "--compare" => compare = true,
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

		let input = input.ok_or_else(|| "missing required `--input <file.lp>`".to_string())?;
		let field = field.ok_or_else(|| "missing required `--field <name>`".to_string())?;

		Ok(Command::Run(Self { input, field, precision, spline, resolution, reps, out_dir, name, compare }))
	}
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

/// Usage text shown for `--help` and on a parse error.
const USAGE: &str = "\
dsp-bench — run a DSP interpolation benchmark over an InfluxDB-Line-Protocol / TSBS file

USAGE:
    dsp-bench --input <FILE.lp> --field <NAME> [OPTIONS]
    dsp-bench <FILE.lp> --field <NAME> [OPTIONS]

REQUIRED:
    -i, --input <FILE>       Line-protocol input file (.lp / TSBS payload)
    -f, --field <NAME>       Numeric field to interpolate

OPTIONS:
        --precision <P>      Timestamp precision: ns|us|ms|s         [default: ns]
        --spline <S>         Spline: linear|quadratic|cubic|poly[:N] [default: cubic]
        --resolution <R>     Output grid: ns|us|ms|s|m|h|d|w|mo|y    [default: s]
        --reps <N>           Timed repetitions (>0)                  [default: 10]
        --out-dir <DIR>      Report output directory          [default: reports/json]
        --name <NAME>        Profile name            [default: input file stem]
    -c, --compare            Also run the portable baseline suite (linear +
                             forward-fill) for comparison
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
			Command::Run(cli) => cli,
			Command::Help => panic!("expected a run, got help"),
		}
	}

	#[test]
	fn parses_required_flags_with_defaults() {
		let cli = expect_run(&["--input", "data.lp", "--field", "usage"]);
		assert_eq!(cli.input, PathBuf::from("data.lp"));
		assert_eq!(cli.field, "usage");
		assert_eq!(cli.precision, TimestampPrecision::Nanoseconds);
		assert_eq!(cli.spline, Spline::Cubic);
		assert_eq!(cli.resolution, Resolution::Seconds);
		assert_eq!(cli.reps, 10);
		assert_eq!(cli.out_dir, PathBuf::from("reports").join("json"));
		assert_eq!(cli.name, None);
		assert!(!cli.compare, "comparison is off unless requested");
	}

	#[test]
	fn compare_flag_is_parsed_in_both_forms() {
		assert!(expect_run(&["data.lp", "-f", "v", "--compare"]).compare);
		assert!(expect_run(&["data.lp", "-f", "v", "-c"]).compare);
		assert!(!expect_run(&["data.lp", "-f", "v"]).compare);
	}

	#[test]
	fn accepts_short_flags_and_positional_input() {
		let cli = expect_run(&["data.lp", "-f", "usage"]);
		assert_eq!(cli.input, PathBuf::from("data.lp"));
		assert_eq!(cli.field, "usage");

		let cli = expect_run(&["-i", "x.lp", "-f", "v"]);
		assert_eq!(cli.input, PathBuf::from("x.lp"));
	}

	#[test]
	fn accepts_key_equals_value_form() {
		let cli = expect_run(&["--input=data.lp", "--field=usage", "--reps=25", "--spline=linear"]);
		assert_eq!(cli.input, PathBuf::from("data.lp"));
		assert_eq!(cli.field, "usage");
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
