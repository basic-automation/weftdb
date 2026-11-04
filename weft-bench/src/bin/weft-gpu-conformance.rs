//! `weft-gpu-conformance` — the GPU conformance matrix (roadmap 5.6).
//!
//! ```text
//! weft-gpu-conformance matrix [--out DIR] [--size smoke|full] [--repeats N] [--uci FILE] [--uci-rows N]
//!                             [--env NAME:VAR=VAL[,VAR=VAL...]]... [--no-lavapipe]
//! weft-gpu-conformance run    --env-name NAME --out DIR [--size ...] [--repeats N] [--uci FILE] [--uci-rows N] [--gpu-only]
//! ```
//!
//! `matrix` runs one child process per adapter environment (splimes' GPU is a process-wide
//! singleton): `host` (the default adapter, plus `Cpu` and `Parallel`), `lavapipe` when Mesa's
//! lavapipe Vulkan ICD is installed (selected with `VK_ICD_FILENAMES`), and each `--env`. It
//! merges their results into `DIR/matrix.json` and `DIR/matrix.md`, and exits 1 if any
//! target broke the published bound (or another promise of the contract). `run` is what each
//! child does: `DIR/<env>.json` and the exact values in `DIR/<env>.values.bin`.
//!
//! Not a timing benchmark: the wall times recorded are informational.

#![warn(clippy::pedantic, clippy::nursery, clippy::all)]

use std::{
	collections::BTreeMap, fs::File, io::{BufReader, BufWriter}, path::{Path, PathBuf}, process::{Command, ExitCode}
};

use anyhow::{bail, Context};
use weft_bench::conformance::{self, CorpusSize, EnvironmentRun, RunOptions, Target};

/// Where Mesa installs Vulkan ICD manifests.
const ICD_DIRS: [&str; 2] = ["/usr/share/vulkan/icd.d", "/etc/vulkan/icd.d"];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Common {
	out: PathBuf,
	size: CorpusSize,
	repeats: usize,
	uci: Option<PathBuf>,
	uci_rows: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Cli {
	Matrix { common: Common, envs: Vec<(String, BTreeMap<String, String>)>, lavapipe: bool },
	Run { common: Common, env_name: String, gpu_only: bool },
}

fn usage() -> &'static str {
	"usage:\n  weft-gpu-conformance matrix [--out DIR] [--size smoke|full] [--repeats N] [--uci FILE] [--uci-rows N] [--env NAME:VAR=VAL[,VAR=VAL...]]... [--no-lavapipe]\n  weft-gpu-conformance run --env-name NAME [--out DIR] [--size smoke|full] [--repeats N] [--uci FILE] [--uci-rows N] [--gpu-only]"
}

fn parse_env(spec: &str) -> Result<(String, BTreeMap<String, String>), String> {
	let (name, vars) = spec.split_once(':').unwrap_or((spec, ""));
	if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
		return Err(format!("--env: bad environment name {name:?} (letters, digits, - and _)"));
	}
	let mut map = BTreeMap::new();
	for pair in vars.split(',').map(str::trim).filter(|p| !p.is_empty()) {
		let (k, v) = pair.split_once('=').ok_or_else(|| format!("--env {name}: {pair:?} is not VAR=VALUE"))?;
		map.insert(k.trim().to_owned(), v.trim().to_owned());
	}
	Ok((name.to_owned(), map))
}

fn parse(args: &[String]) -> Result<Cli, String> {
	let (command, rest) = args.split_first().ok_or_else(|| usage().to_owned())?;
	let mut common = Common { out: PathBuf::from("target/gpu-conformance"), size: CorpusSize::Full, repeats: 3, uci: None, uci_rows: 2_000 };
	let (mut envs, mut lavapipe, mut env_name, mut gpu_only) = (Vec::new(), true, None, false);
	let mut it = rest.iter();
	while let Some(flag) = it.next() {
		let mut value = || it.next().cloned().ok_or_else(|| format!("{flag} needs a value"));
		match flag.as_str() {
			"--out" => common.out = PathBuf::from(value()?),
			"--size" => {
				common.size = match value()?.as_str() {
					"smoke" => CorpusSize::Smoke,
					"full" => CorpusSize::Full,
					other => return Err(format!("--size: {other:?} is not smoke or full")),
				}
			}
			"--repeats" => common.repeats = value()?.parse().ok().filter(|&n| n >= 1).ok_or("--repeats needs a whole number ≥ 1")?,
			"--uci" => common.uci = Some(PathBuf::from(value()?)),
			"--uci-rows" => common.uci_rows = value()?.parse().ok().filter(|&n| n >= 2).ok_or("--uci-rows needs a whole number ≥ 2")?,
			"--env" if command == "matrix" => envs.push(parse_env(&value()?)?),
			"--no-lavapipe" if command == "matrix" => lavapipe = false,
			"--env-name" if command == "run" => env_name = Some(parse_env(&value()?)?.0),
			"--gpu-only" if command == "run" => gpu_only = true,
			other => return Err(format!("unknown option {other:?} for {command}\n{}", usage())),
		}
	}
	match command.as_str() {
		"matrix" => {
			if envs.iter().any(|(n, _)| n == "host" || (lavapipe && n == "lavapipe")) {
				return Err("--env: host and lavapipe are built in".to_owned());
			}
			Ok(Cli::Matrix { common, envs, lavapipe })
		}
		"run" => Ok(Cli::Run { common, env_name: env_name.ok_or("run needs --env-name")?, gpu_only }),
		other => Err(format!("unknown command {other:?}\n{}", usage())),
	}
}

/// Mesa's lavapipe ICD manifest, when installed.
fn lavapipe_icd() -> Option<PathBuf> {
	ICD_DIRS.iter().filter_map(|d| std::fs::read_dir(d).ok()).flatten().flatten().map(|e| e.path()).find(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("lvp_icd") && n.ends_with(".json")))
}

fn real_points(common: &Common) -> anyhow::Result<Option<Vec<splimes::Point>>> {
	let Some(path) = &common.uci else { return Ok(None) };
	let corpus = weft_bench::uci::load_uci_corpus(path, weft_bench::uci::UCI_COLUMNS[2], common.uci_rows)?;
	Ok(Some(conformance::uci_points(&corpus)))
}

fn run(common: &Common, env_name: &str, gpu_only: bool) -> anyhow::Result<()> {
	std::fs::create_dir_all(&common.out).with_context(|| format!("creating {}", common.out.display()))?;
	// Record the variables that pick the adapter, whatever started this process.
	let env_vars = ["VK_ICD_FILENAMES", "VK_DRIVER_FILES", "WGPU_ADAPTER_NAME", "WGPU_BACKEND", "WGPU_POWER_PREF"].iter().filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_owned(), v))).collect();
	let targets = if gpu_only { vec![Target::GpuF64, Target::GpuF32] } else { Target::ALL.to_vec() };
	let options = RunOptions { env: env_name.to_owned(), env_vars, size: common.size, repeats: common.repeats, targets, real: real_points(common)? };
	let (report, values) = conformance::run_environment(&options);
	let json = common.out.join(format!("{env_name}.json"));
	serde_json::to_writer_pretty(BufWriter::new(File::create(&json)?), &report)?;
	conformance::write_values(BufWriter::new(File::create(common.out.join(format!("{env_name}.values.bin")))?), &values)?;
	eprintln!("{env_name}: {} ({}), wrote {}", report.adapter.as_ref().map_or("no GPU", |a| a.name.as_str()), report.targets.iter().filter(|t| t.unavailable.is_none()).count(), json.display());
	Ok(())
}

fn load(out: &Path, env: &str) -> anyhow::Result<(EnvironmentRun, Vec<Vec<Option<Vec<u64>>>>)> {
	let report: EnvironmentRun = serde_json::from_reader(BufReader::new(File::open(out.join(format!("{env}.json")))?))?;
	let values = conformance::read_values(BufReader::new(File::open(out.join(format!("{env}.values.bin")))?))?;
	Ok((report, values))
}

fn matrix(common: &Common, extra: &[(String, BTreeMap<String, String>)], lavapipe: bool) -> anyhow::Result<bool> {
	std::fs::create_dir_all(&common.out)?;
	let mut envs: Vec<(String, BTreeMap<String, String>, bool)> = vec![("host".to_owned(), BTreeMap::new(), false)];
	let mut skipped = Vec::new();
	if lavapipe {
		match lavapipe_icd() {
			Some(icd) => {
				let icd = icd.display().to_string();
				envs.push(("lavapipe".to_owned(), [("VK_ICD_FILENAMES", icd.as_str()), ("VK_DRIVER_FILES", icd.as_str()), ("WGPU_ADAPTER_NAME", "llvmpipe"), ("WGPU_BACKEND", "vulkan")].into_iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect(), true));
			}
			None => skipped.push(("lavapipe".to_owned(), format!("Mesa's lavapipe Vulkan ICD (lvp_icd*.json) is not installed under {}", ICD_DIRS.join(" or ")))),
		}
	}
	envs.extend(extra.iter().map(|(n, v)| (n.clone(), v.clone(), true)));
	// The children are this same binary, so they run exactly what was just built.
	let exe = std::env::current_exe().context("locating this binary")?;
	let mut runs = Vec::new();
	for (name, vars, gpu_only) in &envs {
		let mut cmd = Command::new(&exe);
		cmd.args(["run", "--env-name", name, "--out"]).arg(&common.out).args(["--size", common.size.as_str(), "--repeats", &common.repeats.to_string(), "--uci-rows", &common.uci_rows.to_string()]);
		if let Some(uci) = &common.uci {
			cmd.arg("--uci").arg(uci);
		}
		if *gpu_only {
			cmd.arg("--gpu-only");
		}
		cmd.envs(vars);
		eprintln!("running {name}…");
		match cmd.status() {
			Ok(status) if status.success() => runs.push(load(&common.out, name).with_context(|| format!("reading {name}'s results"))?),
			Ok(status) => skipped.push((name.clone(), format!("the run exited with {status}"))),
			Err(e) => skipped.push((name.clone(), format!("could not start: {e}"))),
		}
	}
	if runs.is_empty() {
		bail!("no environment produced results: {skipped:?}");
	}
	// The reference fixtures, rebuilt once here for the drift scales; each environment's hash
	// must match them.
	let corpus = conformance::corpus(common.size, real_points(common)?);
	let fixtures = conformance::fixtures(&corpus, common.size);
	let hash = conformance::corpus_hash(&corpus, &fixtures);
	let m = conformance::build_matrix(&fixtures, &hash, runs, skipped);
	serde_json::to_writer_pretty(BufWriter::new(File::create(common.out.join("matrix.json"))?), &m)?;
	let md = conformance::render_markdown(&m);
	std::fs::write(common.out.join("matrix.md"), &md)?;
	println!("{md}");
	eprintln!("wrote {} and {}", common.out.join("matrix.md").display(), common.out.join("matrix.json").display());
	Ok(m.failures.is_empty())
}

fn main() -> ExitCode {
	let args: Vec<String> = std::env::args().skip(1).collect();
	let cli = match parse(&args) {
		Ok(cli) => cli,
		Err(e) => {
			eprintln!("{e}");
			return ExitCode::from(2);
		}
	};
	let result = match &cli {
		Cli::Run { common, env_name, gpu_only } => run(common, env_name, *gpu_only).map(|()| true),
		Cli::Matrix { common, envs, lavapipe } => matrix(common, envs, *lavapipe),
	};
	match result {
		Ok(true) => ExitCode::SUCCESS,
		Ok(false) => {
			eprintln!("conformance FAILED: see matrix.md's verdict");
			ExitCode::FAILURE
		}
		Err(e) => {
			eprintln!("error: {e:#}");
			ExitCode::from(2)
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn args(a: &[&str]) -> Vec<String> {
		a.iter().map(|s| (*s).to_owned()).collect()
	}

	#[test]
	fn parses_matrix_and_run() {
		let Ok(Cli::Matrix { common, envs, lavapipe }) = parse(&args(&["matrix", "--size", "smoke", "--repeats", "2", "--env", "nv-gl:WGPU_BACKEND=gl,WGPU_ADAPTER_NAME=nvidia", "--no-lavapipe"])) else { panic!("matrix parses") };
		assert_eq!((common.size, common.repeats, lavapipe), (CorpusSize::Smoke, 2, false));
		assert_eq!(envs[0].0, "nv-gl");
		assert_eq!(envs[0].1.get("WGPU_BACKEND").map(String::as_str), Some("gl"));
		assert_eq!(parse(&args(&["run", "--env-name", "host", "--gpu-only"])), Ok(Cli::Run { common: Common { out: PathBuf::from("target/gpu-conformance"), size: CorpusSize::Full, repeats: 3, uci: None, uci_rows: 2_000 }, env_name: "host".to_owned(), gpu_only: true }));
		assert!(parse(&args(&["run"])).unwrap_err().contains("--env-name"));
		assert!(parse(&args(&["matrix", "--repeats", "0"])).is_err());
		assert!(parse(&args(&["matrix", "--size", "huge"])).is_err());
		assert!(parse(&args(&["matrix", "--env", "host"])).is_err());
		assert!(parse(&args(&["matrix", "--env", "x:NOVALUE"])).is_err());
		assert!(parse(&args(&["run", "--env-name", "a", "--no-lavapipe"])).is_err());
		assert!(parse(&args(&["bogus"])).is_err());
	}
}
