//! `weft-xbench`: run the cross-engine interpolation benchmark and write its JSON report.
//!
//! ```text
//! TMPDIR=/mnt/weftbench weft-xbench [--scales tiny,small,medium] [--reps N] [--load-reps N]
//!                                   [--seed N] [--uci FILE] [--out DIR]
//! ```
//!
//! Without `--uci` the UCI datasets are skipped and the report says which datasets ran. The
//! process exits 1 when any result fails its correctness gate, after writing the report.
//!
//! `weft-xbench render REPORT.json [--out DIR]` writes `report.md` and `report.html` from a
//! report, with the generated honesty section (EVI-15R).

#![warn(clippy::pedantic, clippy::nursery, clippy::all)]

use std::path::PathBuf;

use weft_bench::{load_uci_corpus, scale::ScalePreset, RunMetadata, DEFAULT_UCI_COLUMN};
use weft_xbench::{render, run, Dataset, DatasetKind, XbenchReport, DEFAULT_SEED};

const USAGE: &str = "usage: weft-xbench [--scales tiny,small,medium] [--reps N] [--load-reps N] [--seed N] [--uci FILE] [--out DIR]";

struct Cli {
	scales: Vec<ScalePreset>,
	reps: usize,
	load_reps: usize,
	seed: u64,
	uci: Option<PathBuf>,
	out: PathBuf,
}

fn parse(args: impl Iterator<Item = String>) -> Result<Cli, String> {
	let mut cli = Cli { scales: vec![ScalePreset::Tiny, ScalePreset::Small, ScalePreset::Medium], reps: 5, load_reps: 3, seed: DEFAULT_SEED, uci: None, out: PathBuf::from("reports/xbench") };
	let mut args = args;
	while let Some(flag) = args.next() {
		let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value\n{USAGE}"));
		match flag.as_str() {
			"--scales" => cli.scales = value()?.split(',').map(|s| ScalePreset::parse(s).ok_or_else(|| format!("unknown scale {s:?}; one of {}", ScalePreset::names()))).collect::<Result<_, _>>()?,
			"--reps" => cli.reps = value()?.parse().map_err(|e| format!("--reps: {e}"))?,
			"--load-reps" => cli.load_reps = value()?.parse().map_err(|e| format!("--load-reps: {e}"))?,
			"--seed" => cli.seed = value()?.parse().map_err(|e| format!("--seed: {e}"))?,
			"--uci" => cli.uci = Some(PathBuf::from(value()?)),
			"--out" => cli.out = PathBuf::from(value()?),
			"-h" | "--help" => return Err(USAGE.to_string()),
			other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
		}
	}
	if cli.reps == 0 || cli.load_reps == 0 {
		return Err("--reps and --load-reps must be > 0".into());
	}
	Ok(cli)
}

fn source_commit() -> Option<String> {
	let out = std::process::Command::new("git").args(["rev-parse", "HEAD"]).current_dir(env!("CARGO_MANIFEST_DIR")).output().ok()?;
	out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `render REPORT.json [--out DIR]`: the report's Markdown and HTML, beside it by default.
fn render_command(args: &[String]) -> anyhow::Result<()> {
	let (path, out) = match args {
		[path] => (PathBuf::from(path), None),
		[path, flag, dir] if flag == "--out" => (PathBuf::from(path), Some(PathBuf::from(dir))),
		_ => anyhow::bail!("usage: weft-xbench render REPORT.json [--out DIR]"),
	};
	let report: XbenchReport = serde_json::from_slice(&std::fs::read(&path)?).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
	let out = out.unwrap_or_else(|| path.parent().map(PathBuf::from).unwrap_or_default());
	std::fs::create_dir_all(&out)?;
	let blocks = render::blocks(&report);
	std::fs::write(out.join("report.md"), render::to_markdown(&blocks))?;
	std::fs::write(out.join("report.html"), render::to_html(&blocks))?;
	println!("wrote {} and {}", out.join("report.md").display(), out.join("report.html").display());
	Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let args: Vec<String> = std::env::args().skip(1).collect();
	if args.first().map(String::as_str) == Some("render") {
		return render_command(&args[1..]);
	}
	let invocation: Vec<String> = std::iter::once("weft-xbench".to_string()).chain(args.iter().cloned()).collect();
	let cli = match parse(args.into_iter()) {
		Ok(cli) => cli,
		Err(message) => {
			eprintln!("{message}");
			std::process::exit(2);
		}
	};
	let mut datasets = Vec::new();
	for &scale in &cli.scales {
		datasets.push(Dataset::synthetic(DatasetKind::Mcar, scale, cli.seed)?);
		datasets.push(Dataset::synthetic(DatasetKind::Block, scale, cli.seed)?);
	}
	if let Some(path) = &cli.uci {
		let largest = cli.scales.iter().map(|s| s.point_count()).max().unwrap_or(0);
		let corpus = load_uci_corpus(path, DEFAULT_UCI_COLUMN, largest)?;
		for &scale in &cli.scales {
			datasets.push(Dataset::uci(&corpus, scale)?);
		}
	}
	eprintln!("weft-xbench: {} datasets, scratch in {}", datasets.len(), std::env::temp_dir().display());
	let metadata = RunMetadata::capture(chrono::Utc::now().to_rfc3339());
	let report = run(&datasets, cli.load_reps, cli.reps, cli.seed, metadata, source_commit(), invocation).await?;

	std::fs::create_dir_all(&cli.out)?;
	let path = cli.out.join("xbench.json");
	std::fs::write(&path, serde_json::to_vec_pretty(&report)?)?;
	for r in &report.results {
		let gate = r.correctness.as_ref().map_or(String::new(), |c| format!("  {}", if c.passed { "PASS" } else { "FAIL" }));
		#[allow(clippy::cast_precision_loss)]
		let p50_ms = r.latency.p50_ns as f64 / 1e6;
		println!("{:<14} {:<18} {:<11} p50 {:>10.3} ms{gate}", r.dataset, r.arm.arm, format!("{:?}", r.phase).to_lowercase(), p50_ms);
	}
	let blocks = render::blocks(&report);
	std::fs::write(cli.out.join("report.md"), render::to_markdown(&blocks))?;
	std::fs::write(cli.out.join("report.html"), render::to_html(&blocks))?;
	println!("report: {} (+ report.md, report.html)", path.display());
	if !report.all_correct() {
		eprintln!("weft-xbench: at least one arm disagrees with the reference; see the report");
		std::process::exit(1);
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn cli(args: &[&str]) -> Result<Cli, String> {
		parse(args.iter().map(|s| (*s).to_string()))
	}

	#[test]
	fn defaults_are_three_scales_and_no_uci() {
		let c = cli(&[]).expect("parses");
		assert_eq!(c.scales, vec![ScalePreset::Tiny, ScalePreset::Small, ScalePreset::Medium]);
		assert!(c.uci.is_none());
	}

	#[test]
	fn flags_parse_and_bad_values_are_refused() {
		let c = cli(&["--scales", "tiny", "--reps", "2", "--load-reps", "1", "--uci", "x.txt"]).expect("parses");
		assert_eq!((c.scales.len(), c.reps, c.load_reps), (1, 2, 1));
		assert!(cli(&["--scales", "huge"]).is_err());
		assert!(cli(&["--reps", "0"]).is_err());
		assert!(cli(&["--bogus"]).is_err());
	}
}
