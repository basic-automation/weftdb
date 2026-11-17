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
//! - **`kill-ingest-tagged`** (TAG-12, the multi-series crash cell) — the same kill, but the child
//!   ingests every batch into [`TAGGED_SERIES`] tagged series of one aspect (`host=s0`, `host=s1`,
//!   …, each offset by its series number so a row read from the wrong series is a wrong value),
//!   through the tagged [`SegmentStore::ingest`](weftdb::SegmentStore::ingest) path, and reports
//!   each ingest as it returns. It is killed once a seeded number of ingests are acknowledged.
//!   Each series of the reopened store must hold exactly its own acknowledged rows, and only the
//!   one series whose ingest was in flight may hold that one batch beyond them; then every series
//!   resumes from what it holds and must finish whole.
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
use weftdb::{IngestBatch, SegmentStore, SeriesSelector, TagSet};

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
	/// `SIGKILL` the process ingesting batches into several tagged series of one aspect.
	KillIngestTagged,
	/// `SIGKILL` the process compacting the store.
	KillCompaction,
	/// Flip one byte of one segment file of a closed store.
	CorruptSegment,
}

impl Failure {
	/// Every failure, in the order a `--failure all` run takes them.
	pub const ALL: [Self; 4] = [Self::KillIngest, Self::KillIngestTagged, Self::KillCompaction, Self::CorruptSegment];

	/// The name the CLI and the report use.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::KillIngest => "kill-ingest",
			Self::KillIngestTagged => "kill-ingest-tagged",
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
					let end = batch_end(at, batch, params.rows, timestamps.len() as u64);
					let (chunk_ts, chunk_values) = tiled.window(at, end)?;
					store.seal_declared(&profile.aspect, &chunk_ts, chunk_values).await.map_err(|e| anyhow::anyhow!("cannot resume the ingest at row {at}: {e:#}"))?;
					at = end;
				}
				ok &= rows_held(&store, &profile.aspect).await? == params.rows && exact(&store, &profile.aspect, &tiled, params.rows).await?;
				drop(store);
				(recovery, held, ok, format!("killed after {acked} acknowledged rows; reopened holding {held}"))
			}
			// Boxed: the tagged rep's future is large enough to push every caller of `run_failure` over
			// clippy's `large_futures` ceiling if it were held inline.
			Failure::KillIngestTagged => Box::pin(kill_ingest_tagged(profile, &tiled, value_scale, params, &root, &mut rng)).await?,
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

/// The child process, which reports progress on stdout one line at a time and is meant to be killed.
///
/// Its modes are `ingest ROOT ASPECT SCALE UCI COLUMN CORPUS_ROWS ROWS BATCH`,
/// `ingest-tagged ROOT ASPECT SCALE UCI COLUMN CORPUS_ROWS ROWS BATCH SERIES` and
/// `compact ROOT ASPECT TARGET_ROWS`.
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
				let end = batch_end(at, batch, rows, n);
				let (chunk_ts, chunk_values) = tiled.window(at, end)?;
				store.seal_declared(aspect, &chunk_ts, chunk_values).await.map_err(|e| anyhow::anyhow!("{e:#}"))?;
				say(&format!("sealed {end}"))?;
				at = end;
			}
			drop(store);
		}
		[mode, root, aspect, scale, uci, column, corpus_rows, rows, batch, series] if mode == "ingest-tagged" => {
			let corpus = crate::uci::load_uci_corpus(Path::new(uci), column, corpus_rows.parse()?)?;
			anyhow::ensure!(corpus.timestamps.len() >= 2, "{uci}: the corpus needs at least two rows");
			let stride = corpus.timestamps[1] - corpus.timestamps[0];
			let tiled = Tiled { timestamps: &corpus.timestamps, values: &corpus.values, period: corpus.timestamps[corpus.timestamps.len() - 1] - corpus.timestamps[0] + stride };
			let store = declared(Path::new(root), aspect, scale.parse()?).await?;
			let (rows, batch, series, n): (u64, u64, u64, u64) = (rows.parse()?, batch.parse()?, series.parse()?, corpus.timestamps.len() as u64);
			let mut at = 0;
			while at < rows {
				let end = batch_end(at, batch, rows, n);
				for s in 0..series {
					ingest_series(&store, aspect, &tiled, s, at, end).await?;
					say(&format!("sealed {s} {end}"))?;
				}
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
		_ => anyhow::bail!("{CHILD_COMMAND}: expected `ingest ROOT ASPECT SCALE UCI COLUMN CORPUS_ROWS ROWS BATCH`, `ingest-tagged ROOT ASPECT SCALE UCI COLUMN CORPUS_ROWS ROWS BATCH SERIES` or `compact ROOT ASPECT TARGET_ROWS`"),
	}
	Ok(())
}

/// Series a `kill-ingest-tagged` test writes into its one aspect.
pub const TAGGED_SERIES: u64 = 3;

/// The end of the batch that starts at row `at`: `batch` rows, cut at `rows` and at the end of the
/// corpus tile (`n` rows), so one batch never wraps the corpus. The child and the harness both cut
/// batches here, so the harness knows exactly where the batch in flight would have ended.
const fn batch_end(at: u64, batch: u64, rows: u64, n: u64) -> u64 {
	let end = at.saturating_add(batch);
	let tile_end = (at / n + 1) * n;
	let end = if end < rows { end } else { rows };
	if end < tile_end {
		end
	} else {
		tile_end
	}
}

/// The tag value of series `s`.
fn series_host(s: u64) -> String {
	format!("s{s}")
}

/// Corpus row `i` as series `s` stores it: the corpus value plus `s`, so each series holds
/// different values and a row served from the wrong series cannot read back as right.
fn series_value(tiled: &Tiled<'_>, s: u64, i: u64) -> anyhow::Result<BigDecimal> {
	let n = tiled.timestamps.len() as u64;
	Ok(&tiled.values[usize::try_from(i % n)?] + BigDecimal::from(s))
}

/// Ingest rows `at..end` into series `s` through the tagged ingest path. The idempotency key names
/// the series and the first row, so a batch offered again after a crash replays, never doubles.
async fn ingest_series(store: &SegmentStore, aspect: &str, tiled: &Tiled<'_>, s: u64, at: u64, end: u64) -> anyhow::Result<()> {
	let (chunk_ts, _) = tiled.window(at, end)?;
	let values = (at..end).map(|i| series_value(tiled, s, i)).collect::<anyhow::Result<Vec<_>>>()?;
	let groups = TagSet::from_pairs([("host", series_host(s))]).map_err(|e| anyhow::anyhow!("series {s}: {e}"))?;
	let batch = IngestBatch::dense(format!("{}-{at}", series_host(s)), chunk_ts, values).with_groups(groups);
	store.ingest(aspect, &batch).await.map_err(|e| anyhow::anyhow!("cannot ingest rows {at}..{end} into series {s}: {e:#}"))?;
	Ok(())
}

/// The rows series `s` holds of `0..rows`, and whether they are exactly rows `0..held` of that
/// series and nothing else. A series the catalog does not hold yet holds nothing.
async fn series_held(store: &SegmentStore, aspect: &str, tiled: &Tiled<'_>, s: u64, rows: u64) -> anyhow::Result<(u64, bool)> {
	let selector = SeriesSelector::exact([("host", Some(series_host(s)))]).map_err(|e| anyhow::anyhow!("series {s}: {e}"))?;
	let Some(id) = store.resolve_series(aspect, &selector).await.map_err(|e| anyhow::anyhow!("cannot resolve series {s}: {e:#}"))? else {
		return Ok((0, true));
	};
	let (start, end) = (tiled.timestamp(0)?, tiled.timestamp(rows - 1)?);
	let (ts, vs) = store.read_time_range_of(aspect, id, start, end).await.map_err(|e| anyhow::anyhow!("cannot read series {s}: {e:#}"))?;
	let mut exact = ts.len() == vs.len();
	for (i, (t, v)) in ts.iter().zip(&vs).enumerate() {
		let i = i as u64;
		exact &= *t == tiled.timestamp(i)? && v.as_ref() == Some(&series_value(tiled, s, i)?);
	}
	Ok((ts.len() as u64, exact))
}

/// `kill-ingest-tagged`: one rep. Returns the reopen time, the rows held across every series,
/// whether the store passed, and the rep's note.
async fn kill_ingest_tagged(profile: &IngestProfile, tiled: &Tiled<'_>, value_scale: u8, params: &FailureParams, root: &Path, rng: &mut ChaCha8Rng) -> anyhow::Result<(u64, u64, bool, String)> {
	let (n, batch, rows) = (tiled.timestamps.len() as u64, profile.batch_rows as u64, params.rows);
	let mut batches = 0_u64;
	let mut at = 0;
	while at < rows {
		at = batch_end(at, batch, rows, n);
		batches += 1;
	}
	let ingests = batches * TAGGED_SERIES;
	let kill_after = rng.random_range(1..=ingests.saturating_sub(1).max(1));
	let mut child = spawn(&params.exe, &["ingest-tagged", path(root), &profile.aspect, &value_scale.to_string(), path(&params.uci), &params.column, &params.corpus_rows.to_string(), &rows.to_string(), &batch.to_string(), &TAGGED_SERIES.to_string()])?;
	let parse = |line: &str| {
		let (s, end) = line.strip_prefix("sealed ")?.split_once(' ')?;
		Some((s.parse::<u64>().ok()?, end.parse::<u64>().ok()?))
	};
	let acks = read_until(&mut child, parse, |acks| acks.len() as u64 >= kill_after)?;
	// SIGKILL while the next ingest is in flight.
	child.kill()?;
	child.wait()?;
	let mut acked = vec![0_u64; usize::try_from(TAGGED_SERIES)?];
	for &(s, end) in &acks {
		acked[usize::try_from(s)?] = end;
	}
	// The child ingests series 0, 1, … for each batch in turn, so the one in flight follows the
	// last one acknowledged, and starts where that series' acknowledged rows end.
	let in_flight = acks.last().map_or(0, |&(s, _)| (s + 1) % TAGGED_SERIES);
	let t0 = Instant::now();
	let store = open(root).await?;
	let recovery = span_ns(t0);
	let mut ok = true;
	let mut held = Vec::with_capacity(acked.len());
	for (s, &acked_rows) in (0..TAGGED_SERIES).zip(&acked) {
		let (rows_held, exact) = series_held(&store, &profile.aspect, tiled, s, rows).await?;
		let allowed = rows_held == acked_rows || (s == in_flight && rows_held == batch_end(acked_rows, batch, rows, n));
		ok &= allowed && exact;
		held.push(rows_held);
	}
	// Resume every series from what it holds: each must finish whole.
	for (s, &from) in (0..TAGGED_SERIES).zip(&held) {
		let mut at = from;
		while at < rows {
			let end = batch_end(at, batch, rows, n);
			ingest_series(&store, &profile.aspect, tiled, s, at, end).await?;
			at = end;
		}
	}
	for s in 0..TAGGED_SERIES {
		let (rows_held, exact) = series_held(&store, &profile.aspect, tiled, s, rows).await?;
		ok &= rows_held == rows && exact;
	}
	drop(store);
	let total = held.iter().sum();
	Ok((recovery, total, ok, format!("killed after {} acknowledged ingests across {TAGGED_SERIES} series (series {in_flight} in flight); reopened holding {held:?} of acknowledged {acked:?}", acks.len())))
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

	#[test]
	fn a_batch_never_crosses_the_row_count_or_the_corpus_tile() {
		assert_eq!(batch_end(0, 16, 400, 5), 5, "the tile cuts first");
		assert_eq!(batch_end(5, 16, 400, 5), 10);
		assert_eq!(batch_end(0, 3, 400, 5), 3, "the batch cuts first");
		assert_eq!(batch_end(3, 3, 400, 5), 5, "then the tile");
		assert_eq!(batch_end(395, 16, 398, 5), 398, "the row count cuts last");
	}
}
