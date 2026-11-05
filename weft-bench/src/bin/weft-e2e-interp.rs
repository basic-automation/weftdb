//! `weft-e2e-interp` — the end-to-end GPU interpolation benchmark (roadmap Phase 5, P5-e2e).
//!
//! ```text
//! weft-e2e-interp [--out DIR] [--sizes 10k,100k,1M] [--backends cpu,parallel,gpu,gpu-f32,auto]
//!                 [--reps N] [--warmup N] [--seed S] [--knot-step-ms MS] [--margin-knots K]
//!                 [--resolution milliseconds] [--spline linear|quadratic|cubic]
//!                 [--uci FILE [--uci-column NAME] [--uci-rows N]]
//!                 [--http HOST:PORT [--http-aspect NAME] [--http-skip-ingest] [--http-timeout-s S]]
//!                 [--no-library] [--mmap-reads] [--smoke]
//! ```
//!
//! Writes `DIR/e2e.json` (the `E2eReport`) and `DIR/e2e.md`, and exits 1 if any correctness
//! gate failed. See `weft_bench::e2e_interpolation` for what each stage times.

#![warn(clippy::pedantic, clippy::nursery, clippy::all)]

use std::{path::PathBuf, process::ExitCode, time::Duration};

use weft_bench::e2e_interpolation::{self, parse_count, parse_resolution, BackendLane, CorpusSource, E2eConfig, HttpTarget, DEFAULT_E2E_SEED, E2E_ASPECT};

const USAGE: &str = "usage: weft-e2e-interp [--out DIR] [--sizes 10k,100k,1M] [--backends cpu,parallel,gpu,gpu-f32,auto] [--reps N] [--warmup N] [--seed S] [--knot-step-ms MS] [--margin-knots K] [--resolution milliseconds] [--spline linear|quadratic|cubic] [--uci FILE [--uci-column NAME] [--uci-rows N]] [--http HOST:PORT [--http-aspect NAME] [--http-skip-ingest] [--http-timeout-s S]] [--no-library] [--mmap-reads] [--smoke]";

fn parse(args: &[String]) -> Result<(E2eConfig, PathBuf), String> {
	let mut config = E2eConfig { sizes: vec![10_000, 100_000, 1_000_000, 10_000_000], reps: 30, warmup: 3, ..E2eConfig::smoke() };
	let mut out = PathBuf::from("bench/reports/e2e-gpu-interpolation");
	let (mut seed, mut knot_step_ms) = (DEFAULT_E2E_SEED, 100.0_f64);
	let (mut uci, mut uci_column, mut uci_rows) = (None::<PathBuf>, weft_bench::uci::DEFAULT_UCI_COLUMN.to_string(), usize::MAX);
	let (mut http, mut http_aspect, mut skip_ingest, mut timeout_s) = (None::<String>, E2E_ASPECT.to_string(), false, 600_u64);
	let (mut no_library, mut resolution_set) = (false, false);
	let mut it = args.iter();
	while let Some(flag) = it.next() {
		let mut value = || it.next().cloned().ok_or_else(|| format!("{flag} needs a value"));
		match flag.as_str() {
			"-h" | "--help" => return Err(USAGE.to_string()),
			"--smoke" => {
				let smoke = E2eConfig::smoke();
				config.sizes = smoke.sizes;
				config.reps = smoke.reps;
				config.warmup = smoke.warmup;
			}
			"--out" => out = PathBuf::from(value()?),
			"--sizes" => config.sizes = value()?.split(',').map(parse_count).collect::<Result<_, _>>()?,
			"--backends" => config.backends = value()?.split(',').map(|t| BackendLane::parse(t.trim()).ok_or_else(|| format!("--backends: unknown lane {t:?} (cpu, parallel, gpu, gpu-f32, auto)"))).collect::<Result<_, _>>()?,
			"--reps" => config.reps = parse_count(&value()?)?,
			"--warmup" => config.warmup = value()?.parse().map_err(|_| "--warmup needs a count".to_string())?,
			"--seed" => seed = value()?.parse().map_err(|_| "--seed needs an integer".to_string())?,
			"--knot-step-ms" => knot_step_ms = value()?.parse().ok().filter(|v: &f64| *v > 0.0).ok_or_else(|| "--knot-step-ms needs a positive number".to_string())?,
			"--margin-knots" => config.margin_knots = value()?.parse().ok().filter(|v: &i64| *v >= 1).ok_or_else(|| "--margin-knots needs an integer of at least 1".to_string())?,
			"--resolution" => {
				config.resolution = parse_resolution(&value()?)?;
				resolution_set = true;
			}
			"--spline" => {
				config.spline = match value()?.as_str() {
					"linear" => splimes::Spline::Linear,
					"quadratic" => splimes::Spline::Quadratic,
					"cubic" => splimes::Spline::Cubic,
					other => return Err(format!("--spline {other:?}: one of linear, quadratic, cubic")),
				}
			}
			"--uci" => uci = Some(PathBuf::from(value()?)),
			"--uci-column" => uci_column = value()?,
			"--uci-rows" => uci_rows = parse_count(&value()?)?,
			"--http" => http = Some(value()?),
			"--http-aspect" => http_aspect = value()?,
			"--http-skip-ingest" => skip_ingest = true,
			"--http-timeout-s" => timeout_s = value()?.parse().ok().filter(|v| *v > 0).ok_or_else(|| "--http-timeout-s needs a positive integer".to_string())?,
			"--no-library" => no_library = true,
			"--mmap-reads" => config.mmap_reads = true,
			other => return Err(format!("unknown flag {other:?}\n{USAGE}")),
		}
	}
	config.corpus = match uci {
		Some(path) => {
			// One-minute data: a seconds grid by default, so a window holds many knots.
			if !resolution_set {
				config.resolution = splimes::Resolution::Seconds;
			}
			CorpusSource::Uci { path, column: uci_column, max_rows: uci_rows }
		}
		None => CorpusSource::Synthetic { seed, knot_step_ns: (knot_step_ms * 1e6).round() as i64 },
	};
	if no_library {
		config.backends.clear();
	}
	config.http = http.map(|authority| HttpTarget { authority, aspect: http_aspect, skip_ingest, timeout: Duration::from_secs(timeout_s) });
	Ok((config, out))
}

fn main() -> ExitCode {
	let args: Vec<String> = std::env::args().skip(1).collect();
	let (config, out) = match parse(&args) {
		Ok(parsed) => parsed,
		Err(message) => {
			eprintln!("{message}");
			return ExitCode::from(2);
		}
	};
	let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
		Ok(rt) => rt,
		Err(e) => {
			eprintln!("weft-e2e-interp: cannot start the runtime: {e}");
			return ExitCode::FAILURE;
		}
	};
	let report = match runtime.block_on(e2e_interpolation::run(&config, &mut |step| eprintln!("weft-e2e-interp: {step}"))) {
		Ok(report) => report,
		Err(e) => {
			eprintln!("weft-e2e-interp: {e:#}");
			return ExitCode::FAILURE;
		}
	};
	let written = std::fs::create_dir_all(&out).map_err(|e| e.to_string()).and_then(|()| serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())).and_then(|json| std::fs::write(out.join("e2e.json"), json).map_err(|e| e.to_string())).and_then(|()| std::fs::write(out.join("e2e.md"), e2e_interpolation::markdown(&report)).map_err(|e| e.to_string()));
	if let Err(e) = written {
		eprintln!("weft-e2e-interp: writing {}: {e}", out.display());
		return ExitCode::FAILURE;
	}
	println!("{}", e2e_interpolation::markdown(&report));
	if report.all_ok {
		ExitCode::SUCCESS
	} else {
		eprintln!("weft-e2e-interp: a correctness gate failed; see {}", out.join("e2e.json").display());
		ExitCode::from(1)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn args(s: &str) -> Vec<String> {
		s.split_whitespace().map(str::to_string).collect()
	}

	#[test]
	fn flags_parse() {
		let (config, out) = parse(&args("--sizes 1k,2M --backends cpu,gpu-f32 --reps 5 --warmup 0 --http 127.0.0.1:8080 --out x")).expect("parses");
		assert_eq!(config.sizes, vec![1_000, 2_000_000]);
		assert_eq!(config.backends, vec![BackendLane::Cpu, BackendLane::GpuF32]);
		assert_eq!((config.reps, config.warmup), (5, 0));
		assert_eq!(config.http.as_ref().map(|h| h.authority.as_str()), Some("127.0.0.1:8080"));
		assert_eq!(out, PathBuf::from("x"));
		assert!(parse(&args("--backends tpu")).is_err());
		assert!(parse(&args("--bogus")).is_err());
		let (uci, _) = parse(&args("--uci f.txt --no-library --http h:1")).expect("parses");
		assert!(uci.backends.is_empty() && uci.resolution == splimes::Resolution::Seconds);
		assert!(!uci.mmap_reads, "buffered reads unless asked");
		let (mapped, _) = parse(&args("--smoke --mmap-reads")).expect("parses");
		assert!(mapped.mmap_reads);
	}
}
