//! Failure tests (fair protocol 1.1, #147): what a store holds after the process writing to it dies.
//!
//! A benchmark that only times a healthy store says nothing about the store a crash leaves behind.
//! Each test here runs the work in a **child process** (this binary, `weft-bench __failure-child`)
//! that the harness kills with `SIGKILL` at a seeded moment, then reopens the store in the harness
//! and checks it against the corpus it was fed:
//!
//! - **`kill-ingest`** — the child seals the corpus in batches and reports each batch as its seal
//!   returns; it is killed once a seeded number of batches are acknowledged, while the next is in
//!   flight. The reopened store must hold every acknowledged row, at most the one batch in flight
//!   beyond them, each exactly; then the ingest resumes from what the store holds and must finish
//!   whole.
//! - **`kill-compaction`** — the harness seals the corpus in small batches, and the child compacts
//!   the store ([`SegmentStore::squash_aspect_to_target_rows`](weftdb::SegmentStore::squash_aspect_to_target_rows)) and is killed a seeded number of
//!   milliseconds in. The reopened store must hold every row exactly, whatever share of the
//!   compaction committed; then the compaction reruns and the rows must still read back exactly.
//! - **`corrupt-segment`** — no process dies: the harness seals the corpus, closes the store, flips
//!   one seeded byte in one seeded segment file, and reopens it. A read may fail, or the damaged
//!   segment may be quarantined (the store's open-time recovery does this), but a value read back
//!   must never be wrong, and every row the read no longer returns must be under a quarantine
//!   record.
//!
//! Every rep is one sample of the reopen (recovery) time, and a test passes only if every rep did.
//! The rows a rep found and the outcome are hashed into the result's digest, so a seed's failures
//! replay to the same digest when the store's recovery behaves the same.

use std::{
	collections::HashMap, io::{BufRead, BufReader}, path::{Path, PathBuf}, process::{Command, Stdio}, time::{Duration, Instant}
};

use bigdecimal::BigDecimal;
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weftdb::SegmentStore;

use crate::{
	ingest::{span_ns, IngestProfile, Tiled}, schema::{BenchResult, ColdWarm, CorrectnessReport, DatasetMeta, ResultDigest, TimingBreakdown, SCHEMA_VERSION}, stats::LatencyStats
};

/// Workload class label recorded for each failure test.
pub const WORKLOAD_FAILURE: &str = "failure";

/// The hidden subcommand the harness runs its child processes as.
pub const CHILD_COMMAND: &str = "__failure-child";

/// The failure a test injects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
	/// `SIGKILL` the process sealing batches.
	KillIngest,
	/// `SIGKILL` the process compacting the store.
	KillCompaction,
	/// Flip one byte of one segment file of a closed store.
	CorruptSegment,
}

impl Failure {
	/// Every failure, in the order a `--failure all` run takes them.
	pub const ALL: [Self; 3] = [Self::KillIngest, Self::KillCompaction, Self::CorruptSegment];

	/// The name the CLI and the report use.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::KillIngest => "kill-ingest",
			Self::KillCompaction => "kill-compaction",
			Self::CorruptSegment => "corrupt-segment",
		}
	}

	/// Parse a CLI name.
	#[must_use]
	pub fn parse(s: &str) -> Option<Self> {
		Self::ALL.into_iter().find(|f| f.as_str() == s)
	}
}

/// The knobs of [`run_failure`].
#[derive(Debug, Clone)]
pub struct FailureParams {
	/// The UCI file the child reloads the corpus from (the child shares no memory with the harness).
	pub uci: PathBuf,
	/// The corpus column.
	pub column: String,
	/// Rows of the file the corpus was loaded from, so the child tiles the same corpus.
	pub corpus_rows: usize,
	/// Rows each rep's store is fed.
	pub rows: u64,
	/// Failures injected per test (`>= 1`).
	pub reps: usize,
	/// Rows a compaction coalesces toward.
	pub compact_to: u64,
	/// Upper bound of the seeded delay before a compaction is killed.
	pub kill_within: Duration,
	/// Seed for the kill points, the corrupted file and byte.
	pub seed: u64,
	/// This binary, which the child processes run as.
	pub exe: PathBuf,
}

/// Run one failure test, `params.reps` times; one result.
///
/// # Errors
///
/// A knob of zero, a corpus too small, a child that cannot start or exits before it is killed, or
/// a store operation the harness itself makes failing. What the reopened store holds is not an
/// error: a wrong answer fails the result's gate.
pub async fn run_failure(failure: Failure, profile: &IngestProfile, timestamps: &[i64], values: &[BigDecimal], value_scale: u8, params: &FailureParams) -> anyhow::Result<BenchResult> {
	anyhow::ensure!(params.reps > 0 && params.rows >= 2 && profile.batch_rows > 0 && params.compact_to > 0, "failure tests need reps, rows, a batch size and a compaction target above zero");
	anyhow::ensure!(timestamps.len() >= 2 && timestamps.len() == values.len(), "a failure-test corpus needs at least two rows and as many values as timestamps");
	let stride = timestamps[1] - timestamps[0];
	let tiled = Tiled { timestamps, values, period: timestamps[timestamps.len() - 1] - timestamps[0] + stride };
	let batch = profile.batch_rows as u64;
	let mut rng = ChaCha8Rng::seed_from_u64(params.seed ^ failure as u64);
	let started = Instant::now();
	let (mut samples, mut passed, mut digest, mut notes) = (Vec::with_capacity(params.reps), true, ResultDigest::default(), Vec::new());
	for rep in 0..params.reps {
		let root = std::env::temp_dir().join(format!("weft-bench-failure-{}-{}-{rep}", failure.as_str(), std::process::id()));
		let _ = std::fs::remove_dir_all(&root);
		let _guard = RemoveOnDrop(root.clone());
		let (recovery_ns, held, ok, note) = match failure {
			Failure::KillIngest => {
				let batches = params.rows.div_ceil(batch);
				let kill_after = rng.random_range(1..=batches.saturating_sub(1).max(1));
				let mut child = spawn(&params.exe, &["ingest", path(&root), &profile.aspect, &value_scale.to_string(), path(&params.uci), &params.column, &params.corpus_rows.to_string(), &params.rows.to_string(), &batch.to_string()])?;
				let acked = read_until(&mut child, |line| line.strip_prefix("sealed ").and_then(|n| n.parse::<u64>().ok()), |acks| acks.len() as u64 >= kill_after)?;
				// SIGKILL while the next batch is in flight.
				child.kill()?;
				child.wait()?;
				let acked = acked.last().copied().unwrap_or(0);
				let t0 = Instant::now();
				let store = open(&root).await?;
				let recovery = span_ns(t0);
				let held = rows_held(&store, &profile.aspect).await?;
				let next = (acked + batch).min(params.rows);
				let mut ok = (held == acked || held == next) && exact(&store, &profile.aspect, &tiled, held).await?;
				// Resume from what the store holds: the ingest must finish whole.
				let mut at = held;
				while at < params.rows {
					let end = (at + batch).min(params.rows).min((at / timestamps.len() as u64 + 1) * timestamps.len() as u64);
					let (chunk_ts, chunk_values) = tiled.window(at, end)?;
					store.seal_declared(&profile.aspect, &chunk_ts, chunk_values).await.map_err(|e| anyhow::anyhow!("cannot resume the ingest at row {at}: {e:#}"))?;
					at = end;
				}
				ok &= rows_held(&store, &profile.aspect).await? == params.rows && exact(&store, &profile.aspect, &tiled, params.rows).await?;
				drop(store);
				(recovery, held, ok, format!("killed after {acked} acknowledged rows; reopened holding {held}"))
			}
			Failure::KillCompaction => {
				seal(&root, &profile.aspect, value_scale, &tiled, params.rows, batch).await?;
				let kill_ms = rng.random_range(0..=u64::try_from(params.kill_within.as_millis()).unwrap_or(u64::MAX));
				let mut child = spawn(&params.exe, &["compact", path(&root), &profile.aspect, &params.compact_to.to_string()])?;
				read_until(&mut child, |line| (line == "compacting").then_some(()), |seen| !seen.is_empty())?;
				std::thread::sleep(Duration::from_millis(kill_ms));
				let finished = child.try_wait()?.is_some();
				let _ = child.kill();
				let _ = child.wait();
				let t0 = Instant::now();
				let store = open(&root).await?;
				let recovery = span_ns(t0);
				let held = rows_held(&store, &profile.aspect).await?;
				let report = store.fsck(true).await.map_err(|e| anyhow::anyhow!("fsck after the killed compaction: {e:#}"))?;
				let mut ok = held == params.rows && report.quarantined == 0 && report.suspected == 0 && exact(&store, &profile.aspect, &tiled, params.rows).await?;
				store.squash_aspect_to_target_rows(&profile.aspect, usize::try_from(params.compact_to)?).await.map_err(|e| anyhow::anyhow!("cannot rerun the compaction: {e:#}"))?;
				ok &= rows_held(&store, &profile.aspect).await? == params.rows && exact(&store, &profile.aspect, &tiled, params.rows).await?;
				drop(store);
				(recovery, held, ok, format!("killed {kill_ms} ms in{}; {} orphan frames", if finished { " (after it finished)" } else { "" }, report.orphans))
			}
			Failure::CorruptSegment => {
				seal(&root, &profile.aspect, value_scale, &tiled, params.rows, batch).await?;
				let mut files: Vec<PathBuf> = walk(&root.join("segments"))?;
				files.sort();
				anyhow::ensure!(!files.is_empty(), "the sealed store has no segment files under {}", root.join("segments").display());
				let file = &files[rng.random_range(0..files.len())];
				let mut bytes = std::fs::read(file)?;
				let at = rng.random_range(0..bytes.len());
				bytes[at] ^= 0x5A;
				std::fs::write(file, &bytes)?;
				let t0 = Instant::now();
				let store = open(&root).await?;
				let recovery = span_ns(t0);
				// Open-time recovery has already checked the frames; what it set aside is on record.
				let quarantined = store.quarantine_list(Some(&profile.aspect)).await.map_err(|e| anyhow::anyhow!("cannot list quarantine: {e:#}"))?.len();
				// Whatever the store still serves must be right; rows it lost must be accounted for.
				let truth: HashMap<i64, &BigDecimal> = (0..params.rows).map(|i| Ok((tiled.timestamp(i)?, &values[usize::try_from(i % timestamps.len() as u64)?]))).collect::<anyhow::Result<_>>()?;
				let (start, end) = (tiled.timestamp(0)?, tiled.timestamp(params.rows - 1)?);
				let (ok, held, outcome) = match store.read_time_range(&profile.aspect, start, end).await {
					Ok((ts, vs)) => {
						let right = ts.iter().zip(&vs).all(|(t, v)| truth.get(t).is_some_and(|want| v.as_ref() == Some(*want)));
						let lost = params.rows - ts.len() as u64;
						(right && (lost == 0 || quarantined > 0), ts.len() as u64, format!("read {} rows, {lost} under quarantine", ts.len()))
					}
					Err(e) => (true, 0, format!("read refused: {e}")),
				};
				drop(store);
				(recovery, held, ok, format!("byte {at} of {} flipped; {quarantined} segment(s) quarantined at open; {outcome}", file.file_name().unwrap_or_default().to_string_lossy()))
			}
		};
		samples.push(recovery_ns);
		passed &= ok;
		digest.row(rep, format!("{held} {ok}"));
		notes.push(note);
	}
	for (rep, note) in notes.iter().enumerate() {
		eprintln!("failure {} rep {rep}: {note}", failure.as_str());
	}
	let measured_ns = samples.iter().copied().fold(0_u64, u64::saturating_add);
	Ok(BenchResult { schema_version: SCHEMA_VERSION, profile: format!("{}-{}", profile.name, failure.as_str()), adapter: "weftdb".to_string(), workload: WORKLOAD_FAILURE.to_string(), reps: samples.len(), dataset: DatasetMeta { input_points: usize::try_from(params.rows)?, output_points: samples.len(), irregular: false, missingness_fraction: 0.0, seed: params.seed, signal_shape: None }, latency: LatencyStats::from_samples(&samples), latency_ci: None, throughput_points_per_sec: 0.0, timing: TimingBreakdown { dataset_generation_ns: 0, measured_ns, end_to_end_ns: span_ns(started) }, correctness: CorrectnessReport { output_count_ok: passed, expected_output_points: samples.len(), actual_output_points: samples.len(), values_finite: true, result_sha256: Some(digest.finish()) }, accuracy: None, storage: None, cold_warm: ColdWarm::from_samples(&samples), gpu_pool: None, adapter_info: None, growth: None, online: None, compressed_query: None, analytics: None, samples_ns: samples, between_run: None, gpu_economics: None })
}

/// The child process: `ingest ROOT ASPECT SCALE UCI COLUMN CORPUS_ROWS ROWS BATCH` or
/// `compact ROOT ASPECT TARGET_ROWS`. It reports progress on stdout, one line at a time, and is
/// meant to be killed.
///
/// # Errors
///
/// Arguments that do not parse, or a store operation failing.
pub async fn child(args: &[String]) -> anyhow::Result<()> {
	match args {
		[mode, root, aspect, scale, uci, column, corpus_rows, rows, batch] if mode == "ingest" => {
			let corpus = crate::uci::load_uci_corpus(Path::new(uci), column, corpus_rows.parse()?)?;
			anyhow::ensure!(corpus.timestamps.len() >= 2, "{uci}: the corpus needs at least two rows");
			let stride = corpus.timestamps[1] - corpus.timestamps[0];
			let tiled = Tiled { timestamps: &corpus.timestamps, values: &corpus.values, period: corpus.timestamps[corpus.timestamps.len() - 1] - corpus.timestamps[0] + stride };
			let store = declared(Path::new(root), aspect, scale.parse()?).await?;
			let (rows, batch, n): (u64, u64, u64) = (rows.parse()?, batch.parse()?, corpus.timestamps.len() as u64);
			let mut at = 0;
			while at < rows {
				let end = (at + batch).min(rows).min((at / n + 1) * n);
				let (chunk_ts, chunk_values) = tiled.window(at, end)?;
				store.seal_declared(aspect, &chunk_ts, chunk_values).await.map_err(|e| anyhow::anyhow!("{e:#}"))?;
				say(&format!("sealed {end}"))?;
				at = end;
			}
			drop(store);
		}
		[mode, root, aspect, target] if mode == "compact" => {
			let store = open(Path::new(root)).await?;
			say("compacting")?;
			store.squash_aspect_to_target_rows(aspect, target.parse()?).await.map_err(|e| anyhow::anyhow!("{e:#}"))?;
			drop(store);
			say("compacted")?;
		}
		_ => anyhow::bail!("{CHILD_COMMAND}: expected `ingest ROOT ASPECT SCALE UCI COLUMN CORPUS_ROWS ROWS BATCH` or `compact ROOT ASPECT TARGET_ROWS`"),
	}
	Ok(())
}

/// One progress line to the harness, flushed at once: it reads them as they come.
fn say(line: &str) -> std::io::Result<()> {
	use std::io::Write as _;
	let mut out = std::io::stdout().lock();
	writeln!(out, "{line}")?;
	out.flush()
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

fn path(p: &Path) -> &str {
	p.to_str().unwrap_or_default()
}

fn spawn(exe: &Path, args: &[&str]) -> anyhow::Result<std::process::Child> {
	Command::new(exe).arg(CHILD_COMMAND).args(args).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().map_err(|e| anyhow::anyhow!("cannot start {} {CHILD_COMMAND}: {e}", exe.display()))
}

/// Read the child's lines, keeping those `parse` accepts, until `done` holds of them. A child that
/// exits first is an error: there was nothing to kill.
fn read_until<T>(child: &mut std::process::Child, parse: impl Fn(&str) -> Option<T>, done: impl Fn(&[T]) -> bool) -> anyhow::Result<Vec<T>> {
	let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("the child has no stdout"))?;
	let mut seen = Vec::new();
	for line in BufReader::new(stdout).lines() {
		if let Some(item) = parse(&line?) {
			seen.push(item);
			if done(&seen) {
				return Ok(seen);
			}
		}
	}
	let status = child.wait()?;
	anyhow::bail!("the child exited ({status}) before the failure point")
}

async fn open(root: &Path) -> anyhow::Result<SegmentStore> {
	SegmentStore::open(root).await.map_err(|e| anyhow::anyhow!("cannot open the store in {}: {e:#}", root.display()))
}

async fn declared(root: &Path, aspect: &str, scale: u8) -> anyhow::Result<SegmentStore> {
	std::fs::create_dir_all(root)?;
	let store = open(root).await?;
	let schema = AspectSchema::new(PhysicalType::ScaledI64 { scale }, BigDecimal::from(0), TimeUnit::Seconds);
	store.declare(aspect, &schema).await.map_err(|e| anyhow::anyhow!("cannot declare aspect {aspect}: {e:#}"))?;
	Ok(store)
}

/// Seal rows `0..rows` into a fresh store at `root` in the harness, then close it.
async fn seal(root: &Path, aspect: &str, scale: u8, tiled: &Tiled<'_>, rows: u64, batch: u64) -> anyhow::Result<()> {
	let store = declared(root, aspect, scale).await?;
	crate::ingest::seal_tiled(&store, aspect, tiled, rows, batch, &std::sync::atomic::AtomicU64::new(0)).await?;
	Ok(())
}

async fn rows_held(store: &SegmentStore, aspect: &str) -> anyhow::Result<u64> {
	Ok(store.aspect_stats(aspect).await.map_err(|e| anyhow::anyhow!("cannot read the stats of aspect {aspect}: {e:#}"))?.total_rows)
}

/// Rows `0..rows` read back exactly, and nothing else.
async fn exact(store: &SegmentStore, aspect: &str, tiled: &Tiled<'_>, rows: u64) -> anyhow::Result<bool> {
	if rows == 0 {
		return Ok(true);
	}
	let (start, end) = (tiled.timestamp(0)?, tiled.timestamp(rows - 1)?);
	let (ts, vs) = store.read_time_range(aspect, start, end).await.map_err(|e| anyhow::anyhow!("cannot read aspect {aspect}: {e:#}"))?;
	if ts.len() as u64 != rows {
		return Ok(false);
	}
	let n = tiled.timestamps.len() as u64;
	for (i, (t, v)) in ts.iter().zip(&vs).enumerate() {
		let i = i as u64;
		if *t != tiled.timestamp(i)? || v.as_ref() != Some(&tiled.values[usize::try_from(i % n)?]) {
			return Ok(false);
		}
	}
	Ok(true)
}

fn walk(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
	let mut files = Vec::new();
	for entry in std::fs::read_dir(dir)? {
		let path = entry?.path();
		if path.is_dir() {
			files.extend(walk(&path)?);
		} else {
			files.push(path);
		}
	}
	Ok(files)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn failures_parse_by_name() {
		for f in Failure::ALL {
			assert_eq!(Failure::parse(f.as_str()), Some(f));
		}
		assert_eq!(Failure::parse("power-loss"), None);
	}
}
