//! `weft-xbench`: run the cross-engine interpolation benchmark and write its JSON report.
//!
//! ```text
//! TMPDIR=/mnt/weftbench weft-xbench [--scales tiny,small,medium] [--reps N] [--load-reps N]
//!                                   [--seed N] [--uci FILE] [--tsbs FILE] [--clickhouse URL] [--timescale URL]
//!                                   [--questdb ADDR] [--influxdb3 URL] [--iotdb URL]
//!                                   [--cold-hook CMD [--cold-reps N]] [--container ENGINE=NAME]...
//!                                   [--out DIR]
//! ```
//!
//! Without `--uci` the UCI datasets are skipped and the report says which datasets ran, and without
//! `--tsbs` (a `tsbs_generate_data --use-case=cpu-only --format=influx` file) the TSBS ones are; without
//! `--clickhouse` (an HTTP endpoint such as `http://127.0.0.1:8123`) the `ClickHouse` arms are,
//! and without `--timescale` (a `PostgreSQL` connection string for a server with `TimescaleDB`)
//! the `TimescaleDB` arms are, and without `--questdb` (a `QuestDB` server's `host:port`) the
//! `QuestDB` arms are, and without `--influxdb3` (an `InfluxDB` 3 HTTP endpoint) the `InfluxDB` 3
//! arm is, and without `--iotdb` (an Apache `IoTDB` REST service) the `IoTDB` arm is. The process exits 1 when any result fails its correctness gate, after writing the report.
//!
//! `--cold-hook CMD` adds OS-cold and post-restart reps (#146), `--cold-reps` of each (3 by
//! default) per phase, beside the warm ones; see [`weft_xbench::Cold`] for what the hook is asked
//! to do, and `scripts/xbench/cold-hook.sh` for the reference host's.
//!
//! `--container ENGINE=NAME` records that engine server's container (image digest, command and
//! environment, secrets redacted) in the report's run manifest, beside the input files' and the
//! binary's hashes (#147).
//!
//! `weft-xbench combine RUN.json RUN.json... --out DIR` pools separate runs into one report whose
//! intervals carry the spread between them, and renders it ([`weft_xbench::combine`]).
//!
//! `weft-xbench render REPORT.json [--out DIR]` writes `report.md` and `report.html` from a
//! report, with the generated honesty section (EVI-15R).

#![warn(clippy::pedantic, clippy::nursery, clippy::all)]

use std::path::PathBuf;

use weft_bench::{load_uci_corpus, scale::ScalePreset, RunMetadata, DEFAULT_UCI_COLUMN};
use weft_xbench::{clickhouse, combine, render, run, tsbs, Cold, Container, Dataset, DatasetKind, InputFile, Protocol, Servers, XbenchReport, DEFAULT_SEED};

const USAGE: &str = "usage: weft-xbench [--scales tiny,small,medium] [--reps N] [--load-reps N] [--seed N] [--uci FILE] [--tsbs FILE] [--clickhouse URL] [--timescale URL] [--questdb ADDR] [--influxdb3 URL] [--iotdb URL] [--cold-hook CMD [--cold-reps N]] [--container ENGINE=NAME]... [--out DIR]\n       weft-xbench combine RUN.json RUN.json... --out DIR\n       weft-xbench render REPORT.json [--out DIR]";

struct Cli {
	scales: Vec<ScalePreset>,
	reps: usize,
	load_reps: usize,
	seed: u64,
	uci: Option<PathBuf>,
	tsbs: Option<PathBuf>,
	clickhouse: Option<String>,
	timescale: Option<String>,
	questdb: Option<String>,
	influxdb3: Option<String>,
	iotdb: Option<String>,
	cold_hook: Option<String>,
	cold_reps: usize,
	containers: Vec<(String, String)>,
	out: PathBuf,
}

fn parse(args: impl Iterator<Item = String>) -> Result<Cli, String> {
	let mut cli = Cli { scales: vec![ScalePreset::Tiny, ScalePreset::Small, ScalePreset::Medium], reps: 10, load_reps: 3, seed: DEFAULT_SEED, uci: None, tsbs: None, clickhouse: None, timescale: None, questdb: None, influxdb3: None, iotdb: None, cold_hook: None, cold_reps: 3, containers: Vec::new(), out: PathBuf::from("reports/xbench") };
	let mut args = args;
	while let Some(flag) = args.next() {
		let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value\n{USAGE}"));
		match flag.as_str() {
			"--scales" => cli.scales = value()?.split(',').map(|s| ScalePreset::parse(s).ok_or_else(|| format!("unknown scale {s:?}; one of {}", ScalePreset::names()))).collect::<Result<_, _>>()?,
			"--reps" => cli.reps = value()?.parse().map_err(|e| format!("--reps: {e}"))?,
			"--load-reps" => cli.load_reps = value()?.parse().map_err(|e| format!("--load-reps: {e}"))?,
			"--seed" => cli.seed = value()?.parse().map_err(|e| format!("--seed: {e}"))?,
			"--uci" => cli.uci = Some(PathBuf::from(value()?)),
			"--tsbs" => cli.tsbs = Some(PathBuf::from(value()?)),
			"--clickhouse" => cli.clickhouse = Some(value()?),
			"--timescale" => cli.timescale = Some(value()?),
			"--questdb" => cli.questdb = Some(value()?),
			"--influxdb3" => cli.influxdb3 = Some(value()?),
			"--iotdb" => cli.iotdb = Some(value()?),
			"--cold-hook" => cli.cold_hook = Some(value()?),
			"--cold-reps" => cli.cold_reps = value()?.parse().map_err(|e| format!("--cold-reps: {e}"))?,
			"--container" => {
				let spec = value()?;
				let (engine, name) = spec.split_once('=').filter(|(e, n)| !e.is_empty() && !n.is_empty()).ok_or_else(|| format!("--container: {spec:?} is not ENGINE=NAME"))?;
				cli.containers.push((engine.to_string(), name.to_string()));
			}
			"--out" => cli.out = PathBuf::from(value()?),
			"-h" | "--help" => return Err(USAGE.to_string()),
			other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
		}
	}
	if cli.reps == 0 || cli.load_reps == 0 || cli.cold_reps == 0 {
		return Err("--reps, --load-reps and --cold-reps must be > 0".into());
	}
	Ok(cli)
}

/// `HEAD`, with `+dirty` when the checkout has changes or new files, so a report never names a
/// commit its binary was not built from.
fn source_commit() -> Option<String> {
	let git = |args: &[&str]| std::process::Command::new("git").args(args).current_dir(env!("CARGO_MANIFEST_DIR")).output().ok().filter(|out| out.status.success()).map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
	let head = git(&["rev-parse", "HEAD"])?;
	Some(match git(&["status", "--porcelain"]) {
		Some(changes) if changes.is_empty() => head,
		_ => format!("{head}+dirty"),
	})
}

/// `combine RUN.json RUN.json... --out DIR`: pool the runs, write the combined report and render it.
fn combine_command(args: &[String]) -> anyhow::Result<()> {
	let mut paths = Vec::new();
	let mut out = None;
	let mut args = args.iter();
	while let Some(arg) = args.next() {
		if arg == "--out" {
			out = args.next().map(PathBuf::from);
		} else {
			paths.push(PathBuf::from(arg));
		}
	}
	let Some(out) = out.filter(|_| paths.len() >= 2) else { anyhow::bail!("usage: weft-xbench combine RUN.json RUN.json... --out DIR") };
	let (runs, sources) = combine::read_runs(&paths)?;
	let report = combine::combine(&runs, sources)?;
	std::fs::create_dir_all(&out)?;
	let path = out.join("xbench.json");
	std::fs::write(&path, serde_json::to_vec_pretty(&report)?)?;
	for r in &report.results {
		if let Some(b) = &r.between_run {
			let gate = r.correctness.as_ref().map_or(String::new(), |c| format!("  {}", if c.passed { "PASS" } else { "FAIL" }));
			#[allow(clippy::cast_precision_loss)]
			let (lo, hi) = (r.latency_ci.p50.lower_ns as f64 / 1e6, r.latency_ci.p50.upper_ns as f64 / 1e6);
			println!("{:<14} {:<18} {:<11} {:<9} p50 [{lo:>9.3}, {hi:>9.3}] ms  spread {:>5.1}%{}{gate}", r.dataset, r.arm.arm, format!("{:?}", r.phase).to_lowercase(), r.warmth.as_str(), b.p50_spread * 100.0, if b.digests_agree { "" } else { "  ANSWERS DIFFER" });
		}
	}
	let blocks = render::blocks(&report);
	std::fs::write(out.join("report.md"), render::to_markdown(&blocks))?;
	std::fs::write(out.join("report.html"), render::to_html(&blocks))?;
	println!("combined {} runs: {} (+ report.md, report.html)", runs.len(), path.display());
	if !report.all_correct() {
		eprintln!("weft-xbench: at least one combined result fails its gate; see the report");
		std::process::exit(1);
	}
	Ok(())
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
	match args.first().map(String::as_str) {
		Some("render") => return render_command(&args[1..]),
		Some("combine") => return combine_command(&args[1..]),
		_ => {}
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
	if let Some(path) = &cli.tsbs {
		let span = cli.scales.iter().map(|s| s.point_count()).max().unwrap_or(0);
		let series = tsbs::load(path, tsbs::DEFAULT_HOST, tsbs::DEFAULT_FIELD, i64::try_from(span)?)?;
		eprintln!("weft-xbench: tsbs {} {} from {} ({} readings, {} hosts seen)", series.host, series.field, path.display(), series.points.len(), series.hosts);
		for &scale in &cli.scales {
			datasets.push(Dataset::tsbs(&series, scale)?);
		}
	}
	eprintln!("weft-xbench: {} datasets, scratch in {}", datasets.len(), std::env::temp_dir().display());
	let metadata = RunMetadata::capture(chrono::Utc::now().to_rfc3339());
	let servers = Servers { clickhouse: cli.clickhouse.as_deref().map(clickhouse::Server::new), timescale: cli.timescale.clone(), questdb: cli.questdb.as_deref().map(weft_xbench::questdb::Server::new), influxdb3: cli.influxdb3.as_deref().map(weft_xbench::influxdb3::Server::new), iotdb: cli.iotdb.as_deref().map(weft_xbench::iotdb::Server::new) };
	if let Some(server) = &servers.clickhouse {
		eprintln!("weft-xbench: clickhouse {} at {}", server.version()?, cli.clickhouse.as_deref().unwrap_or_default());
	}
	if let Some(url) = &servers.timescale {
		eprintln!("weft-xbench: timescaledb {}", weft_xbench::timescale::version(url).await?);
	}
	if let Some(server) = &servers.questdb {
		eprintln!("weft-xbench: questdb {}", server.version()?);
	}
	if let Some(server) = &servers.influxdb3 {
		eprintln!("weft-xbench: influxdb3 {}", server.version()?);
	}
	if let Some(server) = &servers.iotdb {
		eprintln!("weft-xbench: iotdb {}", server.version()?);
	}
	let cold = cli.cold_hook.clone().map(|hook| Cold { hook, reps: cli.cold_reps });
	if let Some(cold) = &cold {
		eprintln!("weft-xbench: {} OS-cold and {} post-restart reps per phase, via {}", cold.reps, cold.reps, cold.hook);
	}
	let protocol = Protocol { load_reps: cli.load_reps, reps: cli.reps, seed: cli.seed, cold };
	// The manifest is taken before the run, so a container that dies mid-run is still described.
	let mut inputs = Vec::new();
	for (role, path) in [("uci", &cli.uci), ("tsbs", &cli.tsbs)] {
		if let Some(path) = path {
			inputs.push(InputFile::hash(role, path)?);
		}
	}
	let containers = cli.containers.iter().map(|(engine, name)| Container::inspect(engine, name)).collect::<anyhow::Result<Vec<_>>>()?;
	let binary_sha256 = std::env::current_exe().ok().and_then(|exe| InputFile::hash("binary", &exe).ok()).map(|f| f.sha256);
	let mut report = run(&datasets, &servers, &protocol, metadata, source_commit(), invocation).await?;
	report.inputs = inputs;
	report.containers = containers;
	report.binary_sha256 = binary_sha256;

	std::fs::create_dir_all(&cli.out)?;
	let path = cli.out.join("xbench.json");
	std::fs::write(&path, serde_json::to_vec_pretty(&report)?)?;
	for r in &report.results {
		let gate = r.correctness.as_ref().map_or(String::new(), |c| format!("  {}", if c.passed { "PASS" } else { "FAIL" }));
		#[allow(clippy::cast_precision_loss)]
		let p50_ms = r.latency.p50_ns as f64 / 1e6;
		println!("{:<14} {:<18} {:<11} {:<9} p50 {:>10.3} ms{gate}", r.dataset, r.arm.arm, format!("{:?}", r.phase).to_lowercase(), r.warmth.as_str(), p50_ms);
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
		assert!(c.uci.is_none() && c.tsbs.is_none() && c.clickhouse.is_none());
	}

	#[test]
	fn flags_parse_and_bad_values_are_refused() {
		let c = cli(&["--scales", "tiny", "--reps", "2", "--load-reps", "1", "--uci", "x.txt", "--tsbs", "cpu.lp", "--clickhouse", "http://127.0.0.1:8123"]).expect("parses");
		assert_eq!(c.tsbs, Some(PathBuf::from("cpu.lp")));
		assert_eq!(c.clickhouse.as_deref(), Some("http://127.0.0.1:8123"));
		assert_eq!((c.scales.len(), c.reps, c.load_reps), (1, 2, 1));
		assert!(cli(&["--scales", "huge"]).is_err());
		assert!(cli(&["--reps", "0"]).is_err());
		assert!(cli(&["--bogus"]).is_err());
		let c = cli(&["--cold-hook", "scripts/xbench/cold-hook.sh", "--cold-reps", "2"]).expect("parses");
		assert_eq!((c.cold_hook.as_deref(), c.cold_reps), (Some("scripts/xbench/cold-hook.sh"), 2));
		assert!(cli(&["--cold-reps", "0"]).is_err());
		let c = cli(&["--container", "clickhouse=xbench-ch", "--container", "iotdb=xbench-iotdb"]).expect("parses");
		assert_eq!(c.containers, vec![("clickhouse".to_string(), "xbench-ch".to_string()), ("iotdb".to_string(), "xbench-iotdb".to_string())]);
		assert!(cli(&["--container", "clickhouse"]).is_err());
		assert!(cli(&["--container", "=x"]).is_err());
	}
}
