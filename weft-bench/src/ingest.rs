//! Ingest workload over [`SegmentStore`] — the W2 EVI-6 slice.
//!
//! The earlier storage workloads (point-lookup, range-fetch, compression) seal
//! a corpus into bare `.weftseg` segment bytes ([`weft_physical_type::Segment`]).
//! That exercises the codec, not the store: nothing is declared, indexed, or
//! read back through the catalog. This workload drives the path a real ingest
//! takes instead — open a [`SegmentStore`], declare an aspect, seal the corpus
//! in batches with [`SegmentStore::seal_declared`], then read the whole range
//! back through [`SegmentStore::read_time_range`] and gate correctness on the
//! round-trip. The legacy rows path (`rows_mode`) is not involved anywhere.
//!
//! The aspect is declared as `ScaledI64 { scale }` with tolerance `0`, where
//! `scale` is the corpus loader's measured decimal scale (see
//! [`crate::uci::UciCorpus::decimal_scale`]): UCI values are exact decimal
//! text, so a scaled-integer aspect holds them exactly and the correctness
//! gate can demand exact equality. An `f64` aspect would not do: a decimal
//! like `4.216` is not exactly representable in `f64`, so a tolerance-`0` seal
//! would reject the corpus outright, and a loose tolerance would weaken the
//! gate to "close enough".
//!
//! Timing follows the other storage workloads: opening the store and declaring
//! the aspect are setup (excluded from latency); only the batch seals are
//! timed. Each rep ingests into a fresh store in a fresh temporary directory,
//! so reps never see each other's segments, and the directory is removed when
//! the rep ends. The report goes through the same [`BenchResult`] / manifest
//! path as every other workload, via the binary's shared `finish` tail.
//!
//! [`run_ingest_growth`] is the growth curve (#145): one store ingests up to each of a rising
//! set of checkpoints (1M → 10M → 100M → 1B rows), and each stage reports its own seal
//! throughput and per-batch latency together with what the store then holds. A real corpus is
//! far smaller than a billion rows, so it is laid end to end, each copy shifted past the last
//! one's span. The values stay the corpus's own: the generated ingest corpus is known to
//! flatter WeftDB (ROADMAP, "Benchmark hygiene"). A billion rows cannot be read back into
//! memory, so each checkpoint is gated on the store's own accounting (row count, time span,
//! segment order) and on an exact read-back of the first and last [`GROWTH_VERIFY_ROWS`].

use std::{
	sync::{
		atomic::{AtomicBool, AtomicU64, Ordering}, Arc
	}, time::Instant
};

use bigdecimal::{BigDecimal, ToPrimitive};
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weftdb::SegmentStore;

use crate::{
	schema::{BenchResult, CorrectnessReport, DatasetMeta, StorageEstimate, TimingBreakdown, SCHEMA_VERSION}, stats::{BootstrapConfig, LatencyStats}
};

/// Workload class label recorded for the ingest workload.
pub const WORKLOAD_INGEST: &str = "ingest";

/// Workload class label recorded for each checkpoint of the ingest growth curve.
pub const WORKLOAD_INGEST_GROWTH: &str = "ingest_growth";

/// Rows read back exactly at each end of the store at every growth checkpoint.
pub const GROWTH_VERIFY_ROWS: usize = 65_536;

/// Default batch size: rows sealed per [`SegmentStore::seal_declared`] call.
pub const DEFAULT_INGEST_BATCH_ROWS: usize = 1024;

/// Counter distinguishing the temporary store directories of concurrent runs.
static NEXT_STORE_ID: AtomicU64 = AtomicU64::new(0);

/// A temporary directory removed on drop, so a failed rep never leaves a store
/// behind. Declared before the store so it drops after the store does.
pub(crate) struct TempStoreDir(std::path::PathBuf);

impl Drop for TempStoreDir {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

/// The ingest workload's knobs: which aspect to declare and how large a batch
/// to seal at once.
///
/// The corpus itself comes from the caller (today the UCI
/// loader), so there is no seed: a real corpus is its own identity, and the
/// result records seed `0` exactly as the compression workload's
/// [`crate::CorpusRun`] does for a real corpus.
#[derive(Debug, Clone)]
pub struct IngestProfile {
	/// Profile name recorded in the report.
	pub name: String,
	/// Aspect name to declare in the store. Must satisfy the store's new-name
	/// rules (lowercase `[a-z0-9_]` etc.); the binary lowercases the UCI column
	/// name for this.
	pub aspect: String,
	/// Rows per seal call (`>= 1`).
	pub batch_rows: usize,
}

impl IngestProfile {
	/// Create a profile. `batch_rows` is validated by [`run_ingest_on`].
	#[must_use]
	pub fn new(name: impl Into<String>, aspect: impl Into<String>, batch_rows: usize) -> Self {
		Self { name: name.into(), aspect: aspect.into(), batch_rows }
	}
}

/// Run the ingest workload over an explicit time-sorted `(timestamps, values)`
/// corpus.
///
/// This is open a fresh [`SegmentStore`] per rep, declare the aspect as an
/// exact scaled-integer column, seal the corpus in [`IngestProfile::batch_rows`]
/// batches, and gate correctness on reading every row back exactly.
///
/// `value_scale` is the shared scaled-integer scale (the loader's measured
/// decimal scale); `missingness_fraction` is the share of the source corpus
/// dropped before ingest (the UCI loader's gap share), recorded in the dataset
/// metadata so a run over a gappy corpus does not read as a run over a full
/// one. `generation_ns` (producing or loading the corpus) is charged to setup,
/// never to latency.
///
/// # Errors
///
/// Returns an error if `reps == 0`, if the corpus has fewer than two rows or
/// mismatched columns, if `batch_rows == 0`, or if any store operation fails.
/// A read-back mismatch is not an error: it fails the correctness gate in the
/// returned [`BenchResult`], exactly as the other workloads report a failed
/// gate, so the run is non-publishable and the binary exits non-zero.
pub async fn run_ingest_on(profile: &IngestProfile, timestamps: &[i64], values: &[BigDecimal], value_scale: u8, missingness_fraction: f64, generation_ns: u64, reps: usize) -> anyhow::Result<BenchResult> {
	anyhow::ensure!(reps > 0, "reps must be > 0");
	anyhow::ensure!(timestamps.len() >= 2, "an ingest corpus needs at least two rows");
	anyhow::ensure!(timestamps.len() == values.len(), "ingest corpus timestamps and values differ in length");
	anyhow::ensure!(profile.batch_rows > 0, "ingest batch_rows must be > 0");

	let run_start = Instant::now();
	let schema = AspectSchema::new(PhysicalType::ScaledI64 { scale: value_scale }, BigDecimal::from(0), TimeUnit::Seconds);
	let mut samples_ns: Vec<u64> = Vec::with_capacity(reps);
	let mut last_read: (Vec<i64>, Vec<Option<BigDecimal>>) = (Vec::new(), Vec::new());
	for _ in 0..reps {
		// Fresh store per rep, in a directory that vanishes with the rep.
		let root = std::env::temp_dir().join(format!("weft-bench-ingest-{}-{}", std::process::id(), NEXT_STORE_ID.fetch_add(1, Ordering::Relaxed)));
		std::fs::create_dir_all(&root).map_err(|e| anyhow::anyhow!("cannot create ingest store dir {}: {e}", root.display()))?;
		let _store_dir = TempStoreDir(root.clone());
		let store = SegmentStore::open(&root).await.map_err(|e| anyhow::anyhow!("cannot open ingest store in {}: {e:#}", root.display()))?;
		store.declare(&profile.aspect, &schema).await.map_err(|e| anyhow::anyhow!("cannot declare aspect {}: {e:#}", profile.aspect))?;

		// Only the seals are timed: this is the ingest under measurement.
		let t0 = Instant::now();
		for (chunk_ts, chunk_values) in timestamps.chunks(profile.batch_rows).zip(values.chunks(profile.batch_rows)) {
			store.seal_declared(&profile.aspect, chunk_ts, chunk_values).await.map_err(|e| anyhow::anyhow!("cannot seal {} rows into aspect {}: {e:#}", chunk_ts.len(), profile.aspect))?;
		}
		samples_ns.push(span_ns(t0));

		// Correctness is checked after timing, against the full read-back.
		let (first, last) = (timestamps[0], timestamps[timestamps.len() - 1]);
		last_read = store.read_time_range(&profile.aspect, first, last).await.map_err(|e| anyhow::anyhow!("cannot read back aspect {}: {e:#}", profile.aspect))?;
		drop(store);
	}

	let measured_ns = samples_ns.iter().copied().fold(0_u64, u64::saturating_add);
	let timing = TimingBreakdown { dataset_generation_ns: generation_ns, measured_ns, end_to_end_ns: span_ns(run_start).saturating_add(generation_ns) };

	let (read_ts, read_values) = &last_read;
	let round_trips_exactly = read_ts == timestamps && read_values.iter().map(Option::as_ref).eq(values.iter().map(Some));
	let correctness = CorrectnessReport { output_count_ok: round_trips_exactly, expected_output_points: timestamps.len(), actual_output_points: read_ts.len(), values_finite: read_values.iter().flatten().all(|v| v.to_f64().is_some_and(f64::is_finite)), result_sha256: Some(crate::schema::column_digest(read_ts, read_values)) };

	let storage = Some(StorageEstimate::from_columns(values, timestamps, TimeUnit::Seconds, &BigDecimal::from(0)));
	let latency = LatencyStats::from_samples(&samples_ns);
	let latency_ci = Some(LatencyStats::bootstrap_cis(&samples_ns, &BootstrapConfig::default()));
	let throughput_points_per_sec = if latency.mean_ns > 0 {
		let secs = crate::num::u64_to_f64(latency.mean_ns) / 1e9;
		let points = crate::num::usize_to_f64(timestamps.len());
		points / secs
	} else {
		0.0
	};

	Ok(BenchResult { schema_version: SCHEMA_VERSION, profile: profile.name.clone(), adapter: "weftdb".to_string(), workload: WORKLOAD_INGEST.to_string(), reps, dataset: DatasetMeta { input_points: timestamps.len(), output_points: read_ts.len(), irregular: timestamps.windows(3).any(|w| w[2] - w[1] != w[1] - w[0]), missingness_fraction, seed: 0, signal_shape: None }, latency, latency_ci, throughput_points_per_sec, timing, correctness, accuracy: None, storage, cold_warm: crate::schema::ColdWarm::from_samples(&samples_ns), gpu_pool: None, adapter_info: None, growth: None, online: None, compressed_query: None, analytics: None, samples_ns: samples_ns.clone(), between_run: None, gpu_economics: None })
}

/// What the store held at one checkpoint of an ingest growth curve (schema v24).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GrowthPoint {
	/// Rows in the store when the stage ended: the checkpoint.
	pub rows_total: u64,
	/// Rows this stage sealed: the checkpoint minus the one before it.
	pub rows_stage: u64,
	/// Copies of the source corpus laid end to end so far (fractional while one is partial).
	pub corpus_copies: f64,
	/// Segments the aspect holds.
	pub segments: usize,
	/// Realized `.weftseg` bytes across those segments, headers and indexes included.
	pub segment_bytes: u64,
	/// `segment_bytes` per stored row.
	pub bytes_per_point: f64,
	/// Rows read back and compared exactly (the first and last windows).
	pub rows_verified: usize,
	/// Wall time to read those windows back, in nanoseconds.
	pub verify_read_ns: u64,
}

/// The `index`th row of `copies` of a corpus laid end to end: copy `k` is the corpus shifted
/// by `k` periods, where a period is the corpus span plus its first stride, so the copies
/// neither overlap nor touch.
pub(crate) struct Tiled<'a> {
	pub(crate) timestamps: &'a [i64],
	pub(crate) values: &'a [BigDecimal],
	pub(crate) period: i64,
}

impl Tiled<'_> {
	pub(crate) fn timestamp(&self, index: u64) -> anyhow::Result<i64> {
		let n = self.timestamps.len() as u64;
		let copy = i64::try_from(index / n)?;
		let shift = copy.checked_mul(self.period).ok_or_else(|| anyhow::anyhow!("copy {copy} of the corpus overflows the timestamp range"))?;
		self.timestamps[usize::try_from(index % n)?].checked_add(shift).ok_or_else(|| anyhow::anyhow!("copy {copy} of the corpus overflows the timestamp range"))
	}

	/// Rows `start..end`, which must lie within one copy.
	pub(crate) fn window(&self, start: u64, end: u64) -> anyhow::Result<(Vec<i64>, &[BigDecimal])> {
		let n = self.timestamps.len() as u64;
		let at = usize::try_from(start % n)?;
		let len = usize::try_from(end - start)?;
		let timestamps = (start..end).map(|i| self.timestamp(i)).collect::<anyhow::Result<_>>()?;
		Ok((timestamps, &self.values[at..at + len]))
	}
}

/// Run the ingest growth curve: one store ingests the corpus, laid end to end, up to each checkpoint.
///
/// Every checkpoint becomes one [`BenchResult`] (workload [`WORKLOAD_INGEST_GROWTH`]) for the
/// stage that reached it, handed to `on_checkpoint` as soon as the stage is checked, so a long
/// curve keeps the stages it finished if it is stopped before the last one.
///
/// A stage's latency samples are its per-batch seal times, so the report shows whether a seal
/// slows as the store grows; its throughput is the stage's rows over their summed seal time.
/// Building each batch's timestamps and the checks at a checkpoint are outside the timing. A
/// stage passes when the store reports exactly the rows and time span ingested so far, no
/// unsorted or overlapping segment, and the first and last [`GROWTH_VERIFY_ROWS`] rows read
/// back exactly. There is no bootstrap interval: a stage can hold a million batches.
///
/// # Errors
///
/// Returns an error if the corpus has fewer than two rows or mismatched columns, if
/// `batch_rows == 0`, if `checkpoints` is empty or not strictly rising, if a copy would leave
/// the timestamp range, if any store operation fails, or if `on_checkpoint` fails. A failed
/// check is not an error: it fails that checkpoint's correctness gate.
pub async fn run_ingest_growth(profile: &IngestProfile, timestamps: &[i64], values: &[BigDecimal], value_scale: u8, missingness_fraction: f64, generation_ns: u64, checkpoints: &[u64], mut on_checkpoint: impl FnMut(BenchResult) -> anyhow::Result<()>) -> anyhow::Result<()> {
	anyhow::ensure!(timestamps.len() >= 2, "an ingest corpus needs at least two rows");
	anyhow::ensure!(timestamps.len() == values.len(), "ingest corpus timestamps and values differ in length");
	anyhow::ensure!(profile.batch_rows > 0, "ingest batch_rows must be > 0");
	anyhow::ensure!(!checkpoints.is_empty() && checkpoints[0] >= 2 && checkpoints.windows(2).all(|w| w[0] < w[1]), "growth checkpoints must be at least 2 and strictly rising");

	let stride = timestamps[1] - timestamps[0];
	anyhow::ensure!(stride > 0 && timestamps.windows(2).all(|w| w[0] < w[1]), "a growth corpus must have strictly rising timestamps");
	let tiled = Tiled { timestamps, values, period: timestamps[timestamps.len() - 1] - timestamps[0] + stride };
	tiled.timestamp(checkpoints[checkpoints.len() - 1] - 1)?;
	let n = timestamps.len() as u64;
	let batch = profile.batch_rows as u64;

	let root = std::env::temp_dir().join(format!("weft-bench-growth-{}-{}", std::process::id(), NEXT_STORE_ID.fetch_add(1, Ordering::Relaxed)));
	std::fs::create_dir_all(&root).map_err(|e| anyhow::anyhow!("cannot create ingest store dir {}: {e}", root.display()))?;
	let _store_dir = TempStoreDir(root.clone());
	let store = SegmentStore::open(&root).await.map_err(|e| anyhow::anyhow!("cannot open ingest store in {}: {e:#}", root.display()))?;
	let schema = AspectSchema::new(PhysicalType::ScaledI64 { scale: value_scale }, BigDecimal::from(0), TimeUnit::Seconds);
	store.declare(&profile.aspect, &schema).await.map_err(|e| anyhow::anyhow!("cannot declare aspect {}: {e:#}", profile.aspect))?;
	let irregular = timestamps.windows(3).any(|w| w[2] - w[1] != w[1] - w[0]);

	let mut sealed = 0_u64;
	for &checkpoint in checkpoints {
		let stage_start = Instant::now();
		let rows_stage = checkpoint - sealed;
		let mut samples_ns = Vec::with_capacity(usize::try_from(rows_stage.div_ceil(batch))?);
		while sealed < checkpoint {
			// A batch never spans two copies, so its values are one slice of the corpus.
			let end = (sealed + batch).min(checkpoint).min((sealed / n + 1) * n);
			let (chunk_ts, chunk_values) = tiled.window(sealed, end)?;
			let t0 = Instant::now();
			store.seal_declared(&profile.aspect, &chunk_ts, chunk_values).await.map_err(|e| anyhow::anyhow!("cannot seal {} rows into aspect {}: {e:#}", chunk_ts.len(), profile.aspect))?;
			samples_ns.push(span_ns(t0));
			sealed = end;
		}

		let stats = store.aspect_stats(&profile.aspect).await.map_err(|e| anyhow::anyhow!("cannot read the stats of aspect {}: {e:#}", profile.aspect))?;
		let accounted = stats.total_rows == checkpoint && stats.time_range == Some((tiled.timestamp(0)?, tiled.timestamp(checkpoint - 1)?)) && stats.unsorted_segments == 0 && stats.overlapping_segments == 0;
		let verify_start = Instant::now();
		let mut rows_verified = 0;
		let mut exact = true;
		let mut finite = true;
		let mut digest = crate::schema::ResultDigest::default();
		let window = (GROWTH_VERIFY_ROWS as u64).min(checkpoint);
		for (from, to) in [(0, window), (checkpoint - window, checkpoint)] {
			// The window at the end may cross a copy boundary: compare it one copy at a time.
			let (read_ts, read_values) = store.read_time_range(&profile.aspect, tiled.timestamp(from)?, tiled.timestamp(to - 1)?).await.map_err(|e| anyhow::anyhow!("cannot read back aspect {}: {e:#}", profile.aspect))?;
			for (t, v) in read_ts.iter().zip(&read_values) {
				digest.row(t, crate::schema::nullable_text(v.as_ref()));
			}
			let mut expected_ts = Vec::new();
			let mut expected_values: Vec<&BigDecimal> = Vec::new();
			let mut at = from;
			while at < to {
				let end = to.min((at / n + 1) * n);
				let (ts, vals) = tiled.window(at, end)?;
				expected_ts.extend(ts);
				expected_values.extend(vals);
				at = end;
			}
			exact &= read_ts == expected_ts && read_values.iter().map(Option::as_ref).eq(expected_values.into_iter().map(Some));
			finite &= read_values.iter().flatten().all(|v| v.to_f64().is_some_and(f64::is_finite));
			rows_verified += read_ts.len();
		}
		let verify_read_ns = span_ns(verify_start);

		let measured_ns = samples_ns.iter().copied().fold(0_u64, u64::saturating_add);
		let throughput_points_per_sec = if measured_ns > 0 { crate::num::u64_to_f64(rows_stage) / (crate::num::u64_to_f64(measured_ns) / 1e9) } else { 0.0 };
		let growth = GrowthPoint { rows_total: checkpoint, rows_stage, corpus_copies: crate::num::u64_to_f64(checkpoint) / crate::num::u64_to_f64(n), segments: stats.segment_count, segment_bytes: stats.total_bytes, bytes_per_point: stats.bytes_per_point, rows_verified, verify_read_ns };
		let correctness = CorrectnessReport { output_count_ok: accounted && exact, expected_output_points: usize::try_from(checkpoint)?, actual_output_points: usize::try_from(stats.total_rows)?, values_finite: finite, result_sha256: Some(digest.finish()) };
		let timing = TimingBreakdown { dataset_generation_ns: generation_ns, measured_ns, end_to_end_ns: span_ns(stage_start) };
		on_checkpoint(BenchResult { schema_version: SCHEMA_VERSION, profile: format!("{}-{}", profile.name, count_label(checkpoint)), adapter: "weftdb".to_string(), workload: WORKLOAD_INGEST_GROWTH.to_string(), reps: samples_ns.len(), dataset: DatasetMeta { input_points: usize::try_from(checkpoint)?, output_points: usize::try_from(stats.total_rows)?, irregular, missingness_fraction, seed: 0, signal_shape: None }, latency: LatencyStats::from_samples(&samples_ns), latency_ci: None, throughput_points_per_sec, timing, correctness, accuracy: None, storage: None, cold_warm: crate::schema::ColdWarm::from_samples(&samples_ns), gpu_pool: None, adapter_info: None, growth: Some(growth), online: None, compressed_query: None, analytics: None, samples_ns: samples_ns.clone(), between_run: None, gpu_economics: None })?;
	}
	drop(store);
	Ok(())
}

/// The baselines and query accounting of an online ingest run (schema v25).
///
/// The [`BenchResult`] carrying this reports the concurrent half: its latency samples are the
/// queries answered while the corpus was being sealed, and its throughput is the seal rate with
/// those queries running. These fields hold what that is compared against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OnlinePoint {
	/// Reader tasks issuing queries while the writer sealed.
	pub readers: usize,
	/// Rows each query asks for (fewer near a copy boundary or the start of the store).
	pub window_rows: usize,
	/// Rows sealed, in the run with readers and in the run without.
	pub rows: u64,
	/// Seal throughput with no reader running, rows per second.
	pub ingest_alone_rows_per_sec: f64,
	/// Queries answered during the ingest whose window ended at the newest sealed row.
	pub tail_queries: usize,
	/// Queries answered on the finished store with no ingest running.
	pub idle_queries: usize,
	/// Median latency of those idle queries, in nanoseconds.
	pub idle_p50_ns: u64,
	/// 99th-percentile latency of those idle queries, in nanoseconds.
	pub idle_p99_ns: u64,
	/// Rows compared exactly against the corpus, across every query of both phases.
	pub rows_verified: u64,
	/// Queries whose rows differed from the corpus in any way (missing, extra, or changed).
	pub mismatched_queries: usize,
	/// Queries the store answered with an error instead of rows, across both phases.
	#[serde(default)]
	pub failed_queries: usize,
	/// The first such error, verbatim.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub first_error: Option<String>,
}

/// Workload class label recorded for the online ingest + query workload.
pub const WORKLOAD_INGEST_ONLINE: &str = "ingest_online";

/// The knobs of [`run_ingest_online`].
#[derive(Debug, Clone, Copy)]
pub struct OnlineParams {
	/// Rows to seal; the corpus is laid end to end as the growth curve lays it.
	pub rows: u64,
	/// Concurrent reader tasks (`>= 1`).
	pub readers: usize,
	/// Rows per query (`>= 1`).
	pub window_rows: usize,
	/// Seed for the query windows.
	pub seed: u64,
}

/// What one reader saw.
#[derive(Default)]
struct ReaderTally {
	samples_ns: Vec<u64>,
	tail_queries: usize,
	rows_verified: u64,
	rows_read: u64,
	mismatched_queries: usize,
	failed_queries: usize,
	first_error: Option<String>,
	finite: bool,
}

/// Run the online ingest + query workload: queries against a store that is being written.
///
/// Three phases, each over the corpus laid end to end to `params.rows` rows:
///
/// 1. **ingest alone** — seal every row into a fresh store with nothing else running;
/// 2. **online** — seal every row into a second fresh store while `params.readers` tasks query it
///    without pause until the writer finishes. Each query picks a window of
///    `params.window_rows` rows that are already sealed: every other one ends at the newest
///    sealed row (a dashboard following live data), the rest start anywhere in what is sealed
///    (history). A row counts as sealed only once its seal returned, so it must be visible;
///    every query's rows are compared exactly against the corpus;
/// 3. **idle** — on the finished second store, each reader runs as many queries as it ran
///    during the ingest (at least 64), now with no writer.
///
/// The result's latency samples are the online queries, its throughput the online seal rate;
/// [`OnlinePoint`] holds the ingest-alone and idle baselines. The runtime is a multi-threaded
/// one of its own with a worker per reader plus the writer, so the readers and the writer
/// really run at once even when the caller's runtime is single-threaded.
///
/// # Errors
///
/// Returns an error if the corpus has fewer than two rows, mismatched columns or timestamps
/// that do not strictly rise, if `batch_rows`, `readers` or `window_rows` is zero, if
/// `params.rows < 2` or a copy would leave the timestamp range, or if any store operation
/// fails. A query that reads the wrong rows, or that the store refuses (a read can meet the
/// writer's lock), is not an error: it is counted and fails the correctness gate.
pub async fn run_ingest_online(profile: &IngestProfile, timestamps: &[i64], values: &[BigDecimal], value_scale: u8, missingness_fraction: f64, generation_ns: u64, params: OnlineParams) -> anyhow::Result<BenchResult> {
	anyhow::ensure!(timestamps.len() >= 2, "an ingest corpus needs at least two rows");
	anyhow::ensure!(timestamps.len() == values.len(), "ingest corpus timestamps and values differ in length");
	anyhow::ensure!(profile.batch_rows > 0, "ingest batch_rows must be > 0");
	anyhow::ensure!(params.readers > 0 && params.window_rows > 0, "online ingest needs at least one reader and one row per query");
	anyhow::ensure!(params.rows >= 2, "online ingest needs at least two rows");
	let stride = timestamps[1] - timestamps[0];
	anyhow::ensure!(stride > 0 && timestamps.windows(2).all(|w| w[0] < w[1]), "an online ingest corpus must have strictly rising timestamps");
	let period = timestamps[timestamps.len() - 1] - timestamps[0] + stride;
	Tiled { timestamps, values, period }.timestamp(params.rows - 1)?;

	// The reader tasks must be 'static, so they share owned copies of the corpus.
	let corpus: Arc<(Vec<i64>, Vec<BigDecimal>)> = Arc::new((timestamps.to_vec(), values.to_vec()));
	let profile = profile.clone();
	let irregular = timestamps.windows(3).any(|w| w[2] - w[1] != w[1] - w[0]);
	let run_start = Instant::now();
	// The phases run on a runtime of their own, on a thread of their own: a runtime cannot be
	// entered from inside the caller's.
	let (alone_ns, online_ns, online, idle, stats) = std::thread::scope(|scope| {
		scope.spawn(|| {
			let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(params.readers + 1).enable_all().build()?;
			runtime.block_on(online_phases(&profile, corpus, period, value_scale, params))
		})
		.join()
		.map_err(|_| anyhow::anyhow!("the online ingest thread panicked"))?
	})?;

	let rows = params.rows;
	let throughput = |ns: u64| if ns > 0 { crate::num::u64_to_f64(rows) / (crate::num::u64_to_f64(ns) / 1e9) } else { 0.0 };
	let idle_latency = LatencyStats::from_samples(&idle.samples_ns);
	let accounted = stats.0 == rows && stats.1;
	let mismatched_queries = online.mismatched_queries + idle.mismatched_queries;
	let failed_queries = online.failed_queries + idle.failed_queries;
	let point = OnlinePoint { readers: params.readers, window_rows: params.window_rows, rows, ingest_alone_rows_per_sec: throughput(alone_ns), tail_queries: online.tail_queries, idle_queries: idle.samples_ns.len(), idle_p50_ns: idle_latency.p50_ns, idle_p99_ns: idle_latency.p99_ns, rows_verified: online.rows_verified + idle.rows_verified, mismatched_queries, failed_queries, first_error: online.first_error.or(idle.first_error) };
	let correctness = CorrectnessReport { output_count_ok: accounted && mismatched_queries == 0 && failed_queries == 0, expected_output_points: usize::try_from(point.rows_verified)?, actual_output_points: usize::try_from(online.rows_read + idle.rows_read)?, values_finite: online.finite && idle.finite, result_sha256: None };
	let timing = TimingBreakdown { dataset_generation_ns: generation_ns, measured_ns: online_ns, end_to_end_ns: span_ns(run_start).saturating_add(generation_ns) };
	Ok(BenchResult { schema_version: SCHEMA_VERSION, profile: profile.name, adapter: "weftdb".to_string(), workload: WORKLOAD_INGEST_ONLINE.to_string(), reps: online.samples_ns.len(), dataset: DatasetMeta { input_points: usize::try_from(rows)?, output_points: usize::try_from(stats.0)?, irregular, missingness_fraction, seed: params.seed, signal_shape: None }, latency: LatencyStats::from_samples(&online.samples_ns), latency_ci: None, throughput_points_per_sec: throughput(online_ns), timing, correctness, accuracy: None, storage: None, cold_warm: crate::schema::ColdWarm::from_samples(&online.samples_ns), gpu_pool: None, adapter_info: None, growth: None, online: Some(point), compressed_query: None, analytics: None, samples_ns: online.samples_ns.clone(), between_run: None, gpu_economics: None })
}

/// A fresh store with the aspect declared, in a directory removed when the guard drops.
pub(crate) async fn fresh_store(label: &str, aspect: &str, value_scale: u8) -> anyhow::Result<(SegmentStore, TempStoreDir)> {
	let root = std::env::temp_dir().join(format!("weft-bench-{label}-{}-{}", std::process::id(), NEXT_STORE_ID.fetch_add(1, Ordering::Relaxed)));
	std::fs::create_dir_all(&root).map_err(|e| anyhow::anyhow!("cannot create ingest store dir {}: {e}", root.display()))?;
	let guard = TempStoreDir(root.clone());
	let store = SegmentStore::open(&root).await.map_err(|e| anyhow::anyhow!("cannot open ingest store in {}: {e:#}", root.display()))?;
	let schema = AspectSchema::new(PhysicalType::ScaledI64 { scale: value_scale }, BigDecimal::from(0), TimeUnit::Seconds);
	store.declare(aspect, &schema).await.map_err(|e| anyhow::anyhow!("cannot declare aspect {aspect}: {e:#}"))?;
	Ok((store, guard))
}

/// Seal rows `0..rows` of the tiled corpus, publishing each batch's end to `sealed` once its
/// seal has returned; returns the summed seal time.
pub(crate) async fn seal_tiled(store: &SegmentStore, aspect: &str, tiled: &Tiled<'_>, rows: u64, batch: u64, sealed: &AtomicU64) -> anyhow::Result<u64> {
	let n = tiled.timestamps.len() as u64;
	let mut at = 0;
	let mut seal_ns = 0_u64;
	while at < rows {
		let end = (at + batch).min(rows).min((at / n + 1) * n);
		let (chunk_ts, chunk_values) = tiled.window(at, end)?;
		let t0 = Instant::now();
		store.seal_declared(aspect, &chunk_ts, chunk_values).await.map_err(|e| anyhow::anyhow!("cannot seal {} rows into aspect {aspect}: {e:#}", chunk_ts.len()))?;
		seal_ns = seal_ns.saturating_add(span_ns(t0));
		sealed.store(end, Ordering::Release);
		at = end;
	}
	Ok(seal_ns)
}

/// One query: rows `from..to`, which lie within one copy, read back and compared exactly. A
/// read the store refuses is counted in the tally, not returned as an error.
async fn query(store: &SegmentStore, aspect: &str, tiled: &Tiled<'_>, from: u64, to: u64, tally: &mut ReaderTally) -> anyhow::Result<()> {
	let (start, end) = (tiled.timestamp(from)?, tiled.timestamp(to - 1)?);
	let t0 = Instant::now();
	let (read_ts, read_values) = match store.read_time_range(aspect, start, end).await {
		Ok(rows) => rows,
		Err(e) => {
			// A refused read is a result, not the end of the run: count it and fail the gate.
			tally.failed_queries += 1;
			tally.first_error.get_or_insert_with(|| format!("{e:#}"));
			return Ok(());
		}
	};
	tally.samples_ns.push(span_ns(t0));
	let (expected_ts, expected_values) = tiled.window(from, to)?;
	if read_ts != expected_ts || !read_values.iter().map(Option::as_ref).eq(expected_values.iter().map(Some)) {
		tally.mismatched_queries += 1;
	}
	tally.finite &= read_values.iter().flatten().all(|v| v.to_f64().is_some_and(f64::is_finite));
	tally.rows_verified += to - from;
	tally.rows_read += read_ts.len() as u64;
	Ok(())
}

/// A window of `window` rows inside `0..sealed` that does not cross a copy boundary: ending at
/// `sealed` when `tail`, otherwise starting anywhere.
fn pick_window(rng: &mut ChaCha8Rng, sealed: u64, n: u64, window: u64, tail: bool) -> (u64, u64) {
	if tail {
		let to = sealed;
		let copy_start = (to - 1) / n * n;
		(to.saturating_sub(window).max(copy_start), to)
	} else {
		let from = rng.random_range(0..sealed);
		(from, (from + window).min(sealed).min((from / n + 1) * n))
	}
}

/// Run one reader: query `sealed` until `done`, or `fixed` queries over a finished store.
async fn reader(shared: Arc<OnlineShared>, index: usize, fixed: Option<usize>) -> anyhow::Result<ReaderTally> {
	let (timestamps, values) = &*shared.corpus;
	let tiled = Tiled { timestamps, values, period: shared.period };
	let n = timestamps.len() as u64;
	let mut rng = ChaCha8Rng::seed_from_u64(shared.seed ^ (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ u64::from(fixed.is_some()));
	let mut tally = ReaderTally { finite: true, ..ReaderTally::default() };
	let mut asked = 0_usize;
	loop {
		match fixed {
			Some(count) if asked == count => break,
			None if shared.done.load(Ordering::Acquire) => break,
			_ => {}
		}
		let sealed = shared.sealed.load(Ordering::Acquire);
		if sealed == 0 {
			tokio::task::yield_now().await;
			continue;
		}
		let tail = asked.is_multiple_of(2);
		let (from, to) = pick_window(&mut rng, sealed, n, shared.window, tail);
		query(&shared.store, &shared.aspect, &tiled, from, to, &mut tally).await?;
		tally.tail_queries += usize::from(tail);
		asked += 1;
	}
	Ok(tally)
}

/// What the reader tasks share with the writer.
struct OnlineShared {
	store: SegmentStore,
	aspect: String,
	corpus: Arc<(Vec<i64>, Vec<BigDecimal>)>,
	period: i64,
	window: u64,
	seed: u64,
	sealed: AtomicU64,
	done: AtomicBool,
}

/// Merge reader tallies into one.
fn merge(tallies: Vec<ReaderTally>) -> ReaderTally {
	tallies.into_iter().fold(ReaderTally { finite: true, ..ReaderTally::default() }, |mut all, t| {
		all.samples_ns.extend(t.samples_ns);
		all.tail_queries += t.tail_queries;
		all.rows_verified += t.rows_verified;
		all.rows_read += t.rows_read;
		all.mismatched_queries += t.mismatched_queries;
		all.failed_queries += t.failed_queries;
		all.first_error = all.first_error.take().or(t.first_error);
		all.finite &= t.finite;
		all
	})
}

/// The three phases of [`run_ingest_online`]: returns the ingest-alone and online seal times,
/// the online and idle tallies, and whether the online store holds exactly what was sealed.
async fn online_phases(profile: &IngestProfile, corpus: Arc<(Vec<i64>, Vec<BigDecimal>)>, period: i64, value_scale: u8, params: OnlineParams) -> anyhow::Result<(u64, u64, ReaderTally, ReaderTally, (u64, bool))> {
	let batch = profile.batch_rows as u64;
	let alone_ns = {
		let (store, _dir) = fresh_store("online-alone", &profile.aspect, value_scale).await?;
		let tiled = Tiled { timestamps: &corpus.0, values: &corpus.1, period };
		seal_tiled(&store, &profile.aspect, &tiled, params.rows, batch, &AtomicU64::new(0)).await?
	};

	let (store, _dir) = fresh_store("online", &profile.aspect, value_scale).await?;
	let shared = Arc::new(OnlineShared { store, aspect: profile.aspect.clone(), corpus, period, window: params.window_rows as u64, seed: params.seed, sealed: AtomicU64::new(0), done: AtomicBool::new(false) });
	let readers: Vec<_> = (0..params.readers).map(|i| tokio::spawn(reader(Arc::clone(&shared), i, None))).collect();
	let writer = {
		let shared = Arc::clone(&shared);
		tokio::spawn(async move {
			let tiled = Tiled { timestamps: &shared.corpus.0, values: &shared.corpus.1, period: shared.period };
			let sealed = seal_tiled(&shared.store, &shared.aspect, &tiled, params.rows, batch, &shared.sealed).await;
			shared.done.store(true, Ordering::Release);
			sealed
		})
	};
	let online_ns = writer.await.map_err(|e| anyhow::anyhow!("the online writer failed: {e}"))??;
	let mut tallies = Vec::with_capacity(readers.len());
	for handle in readers {
		tallies.push(handle.await.map_err(|e| anyhow::anyhow!("an online reader failed: {e}"))??);
	}
	// A refused read returns at once, so a reader the store keeps refusing asks again and again; sizing the idle
	// rerun from the answered queries keeps it comparable, where counting the refusals made it run for hours.
	let idle_handles: Vec<_> = tallies.iter().enumerate().map(|(i, t)| tokio::spawn(reader(Arc::clone(&shared), i, Some(t.samples_ns.len().max(64))))).collect();
	let online = merge(tallies);
	let mut idle = Vec::with_capacity(idle_handles.len());
	for handle in idle_handles {
		idle.push(handle.await.map_err(|e| anyhow::anyhow!("an idle reader failed: {e}"))??);
	}

	let stats = shared.store.aspect_stats(&profile.aspect).await.map_err(|e| anyhow::anyhow!("cannot read the stats of aspect {}: {e:#}", profile.aspect))?;
	let tiled = Tiled { timestamps: &shared.corpus.0, values: &shared.corpus.1, period };
	let ordered = stats.time_range == Some((tiled.timestamp(0)?, tiled.timestamp(params.rows - 1)?)) && stats.unsorted_segments == 0 && stats.overlapping_segments == 0;
	Ok((alone_ns, online_ns, online, merge(idle), (stats.total_rows, ordered)))
}

/// `1000000` as `1M`, `1000000000` as `1B`; anything not a round thousand stays a number.
#[must_use]
pub fn count_label(n: u64) -> String {
	match n {
		n if n >= 1_000_000_000 && n % 1_000_000_000 == 0 => format!("{}B", n / 1_000_000_000),
		n if n >= 1_000_000 && n % 1_000_000 == 0 => format!("{}M", n / 1_000_000),
		n if n >= 1_000 && n % 1_000 == 0 => format!("{}k", n / 1_000),
		n => n.to_string(),
	}
}

/// Elapsed nanoseconds since `since`, saturated into a `u64`.
pub(crate) fn span_ns(since: Instant) -> u64 {
	crate::num::u128_to_u64(since.elapsed().as_nanos())
}

#[cfg(test)]
mod tests {
	use std::io::Cursor;

	use super::*;
	use crate::uci::{load_uci_corpus_from_reader, DEFAULT_UCI_COLUMN};

	/// The UCI fixture's rows, as a corpus the workload can ingest.
	const FIXTURE: &str = "Date;Time;Global_active_power;Global_reactive_power;Voltage;Global_intensity;Sub_metering_1;Sub_metering_2;Sub_metering_3\n\
		16/12/2006;17:24:00;4.216;0.418;234.840;18.400;0.000;1.000;17.000\n\
		16/12/2006;17:25:00;5.360;0.436;233.630;23.000;0.000;1.000;16.000\n\
		16/12/2006;17:26:00;5.374;0.498;233.290;23.000;0.000;2.000;17.000\n\
		16/12/2006;17:27:00;5.388;0.502;233.740;23.000;0.000;1.000;17.000\n";

	#[tokio::test]
	async fn a_uci_corpus_ingests_over_segment_store_and_reads_back_exactly() {
		let corpus = load_uci_corpus_from_reader(Cursor::new(FIXTURE), DEFAULT_UCI_COLUMN, usize::MAX).expect("loads");
		let profile = IngestProfile::new("ingest-uci-small", "global_active_power", 2);
		let result = run_ingest_on(&profile, &corpus.timestamps, &corpus.values, corpus.decimal_scale, corpus.missingness_fraction(), 0, 2).await.expect("ingest runs");
		assert!(result.correctness.passed(), "read-back must equal the ingested corpus: {:?}", result.correctness);
		assert_eq!(result.workload, "ingest");
		assert_eq!(result.adapter, "weftdb");
		assert_eq!(result.dataset.input_points, 4);
		assert_eq!(result.dataset.output_points, 4);
		assert!(!result.dataset.irregular, "the fixture is a constant 60 s stride");
		assert!(result.storage.is_some());
		assert!(result.throughput_points_per_sec > 0.0);
	}

	#[tokio::test]
	async fn a_single_batch_and_a_batch_per_row_both_round_trip() {
		let corpus = load_uci_corpus_from_reader(Cursor::new(FIXTURE), DEFAULT_UCI_COLUMN, usize::MAX).expect("loads");
		for batch_rows in [usize::MAX, 1] {
			let profile = IngestProfile::new("ingest-uci-batches", "global_active_power", batch_rows);
			let result = run_ingest_on(&profile, &corpus.timestamps, &corpus.values, corpus.decimal_scale, 0.0, 0, 1).await.expect("ingest runs");
			assert!(result.correctness.passed(), "batch_rows={batch_rows}: {:?}", result.correctness);
		}
	}

	#[tokio::test]
	async fn the_growth_curve_lays_the_corpus_end_to_end_and_gates_each_checkpoint() {
		let corpus = load_uci_corpus_from_reader(Cursor::new(FIXTURE), DEFAULT_UCI_COLUMN, usize::MAX).expect("loads");
		let profile = IngestProfile::new("growth", "global_active_power", 3);
		let mut results = Vec::new();
		run_ingest_growth(&profile, &corpus.timestamps, &corpus.values, corpus.decimal_scale, 0.0, 0, &[6, 10], |r| {
			results.push(r);
			Ok(())
		})
		.await
		.expect("growth runs");
		assert_eq!(results.iter().map(|r| r.profile.as_str()).collect::<Vec<_>>(), ["growth-6", "growth-10"]);
		for result in &results {
			assert!(result.correctness.passed(), "{}: {:?}", result.profile, result.correctness);
			assert_eq!(result.workload, "ingest_growth");
			assert!(result.latency_ci.is_none());
		}
		let (first, second) = (results[0].growth.as_ref().expect("growth"), results[1].growth.as_ref().expect("growth"));
		assert_eq!((first.rows_total, first.rows_stage, first.rows_verified), (6, 6, 12), "both windows cover all six rows");
		assert_eq!((second.rows_total, second.rows_stage), (10, 4));
		assert!((second.corpus_copies - 2.5).abs() < 1e-12);
		assert_eq!(results[0].reps, 3, "rows 0..3, 3..4 and 4..6: a batch never spans two copies");
		assert!(second.segments >= results[0].growth.as_ref().expect("growth").segments);
		assert!(second.bytes_per_point > 0.0);
	}

	#[tokio::test]
	async fn online_queries_run_against_a_store_being_written_and_all_read_back_exactly() {
		let corpus = load_uci_corpus_from_reader(Cursor::new(FIXTURE), DEFAULT_UCI_COLUMN, usize::MAX).expect("loads");
		let profile = IngestProfile::new("online", "global_active_power", 1);
		let params = OnlineParams { rows: 40, readers: 2, window_rows: 3, seed: 7 };
		let result = run_ingest_online(&profile, &corpus.timestamps, &corpus.values, corpus.decimal_scale, 0.0, 0, params).await.expect("online runs");
		assert!(result.correctness.passed(), "{:?}", result.correctness);
		assert_eq!(result.workload, "ingest_online");
		assert_eq!(result.dataset.output_points, 40);
		let online = result.online.as_ref().expect("online");
		assert_eq!(online.mismatched_queries, 0);
		assert!(online.idle_queries >= 2 * 64, "each reader repeats at least 64 queries idle");
		assert!(online.ingest_alone_rows_per_sec > 0.0 && result.throughput_points_per_sec > 0.0);
		assert_eq!(online.rows_verified, result.correctness.expected_output_points as u64);
	}

	#[test]
	fn query_windows_stay_inside_what_is_sealed_and_inside_one_copy() {
		let mut rng = ChaCha8Rng::seed_from_u64(1);
		assert_eq!(pick_window(&mut rng, 10, 4, 3, true), (8, 10), "the tail window ends at the newest row but not before its copy");
		assert_eq!(pick_window(&mut rng, 9, 4, 3, true), (8, 9));
		assert_eq!(pick_window(&mut rng, 2, 4, 3, true), (0, 2));
		for _ in 0..1_000 {
			let (from, to) = pick_window(&mut rng, 10, 4, 3, false);
			assert!(from < to && to <= 10 && to - from <= 3 && from / 4 == (to - 1) / 4, "{from}..{to}");
		}
	}

	#[tokio::test]
	async fn online_ingest_refuses_empty_knobs() {
		let corpus = load_uci_corpus_from_reader(Cursor::new(FIXTURE), DEFAULT_UCI_COLUMN, usize::MAX).expect("loads");
		let profile = IngestProfile::new("online", "global_active_power", 2);
		for params in [OnlineParams { rows: 8, readers: 0, window_rows: 2, seed: 0 }, OnlineParams { rows: 8, readers: 1, window_rows: 0, seed: 0 }, OnlineParams { rows: 1, readers: 1, window_rows: 2, seed: 0 }] {
			assert!(run_ingest_online(&profile, &corpus.timestamps, &corpus.values, corpus.decimal_scale, 0.0, 0, params).await.is_err(), "{params:?}");
		}
	}

	#[test]
	fn copies_are_shifted_past_the_previous_span_and_counts_are_labelled() {
		let tiled = Tiled { timestamps: &[100, 160, 220], values: &[BigDecimal::from(1), BigDecimal::from(2), BigDecimal::from(3)], period: 180 };
		assert_eq!((0..7).map(|i| tiled.timestamp(i).expect("in range")).collect::<Vec<_>>(), [100, 160, 220, 280, 340, 400, 460]);
		assert!(Tiled { timestamps: &[0, 1], values: &[BigDecimal::from(1), BigDecimal::from(2)], period: i64::MAX }.timestamp(4).is_err(), "an overflowing copy is refused");
		assert_eq!([1_000_000, 10_000_000, 1_000_000_000, 5_000, 1_234].map(count_label), ["1M", "10M", "1B", "5k", "1234"]);
	}

	#[tokio::test]
	async fn growth_checkpoints_must_rise() {
		let corpus = load_uci_corpus_from_reader(Cursor::new(FIXTURE), DEFAULT_UCI_COLUMN, usize::MAX).expect("loads");
		let profile = IngestProfile::new("growth", "global_active_power", 2);
		for bad in [&[][..], &[8, 8][..], &[8, 4][..], &[1][..]] {
			assert!(run_ingest_growth(&profile, &corpus.timestamps, &corpus.values, corpus.decimal_scale, 0.0, 0, bad, |_| Ok(())).await.is_err(), "{bad:?}");
		}
	}

	#[tokio::test]
	async fn invalid_inputs_are_errors_not_silent_passes() {
		let corpus = load_uci_corpus_from_reader(Cursor::new(FIXTURE), DEFAULT_UCI_COLUMN, usize::MAX).expect("loads");
		let profile = IngestProfile::new("ingest-bad", "global_active_power", 2);
		assert!(run_ingest_on(&profile, &corpus.timestamps, &corpus.values, corpus.decimal_scale, 0.0, 0, 0).await.is_err());
		assert!(run_ingest_on(&profile, &corpus.timestamps[..1], &corpus.values[..1], corpus.decimal_scale, 0.0, 0, 1).await.is_err());
		assert!(run_ingest_on(&IngestProfile::new("ingest-bad", "global_active_power", 0), &corpus.timestamps, &corpus.values, corpus.decimal_scale, 0.0, 0, 1).await.is_err());
	}
}
