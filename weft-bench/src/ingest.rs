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

use std::{
	sync::atomic::{AtomicU64, Ordering}, time::Instant
};

use bigdecimal::{BigDecimal, ToPrimitive};
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weftdb::SegmentStore;

use crate::{
	schema::{BenchResult, CorrectnessReport, DatasetMeta, StorageEstimate, TimingBreakdown, SCHEMA_VERSION}, stats::{BootstrapConfig, LatencyStats}
};

/// Workload class label recorded for the ingest workload.
pub const WORKLOAD_INGEST: &str = "ingest";

/// Default batch size: rows sealed per [`SegmentStore::seal_declared`] call.
pub const DEFAULT_INGEST_BATCH_ROWS: usize = 1024;

/// Counter distinguishing the temporary store directories of concurrent runs.
static NEXT_STORE_ID: AtomicU64 = AtomicU64::new(0);

/// A temporary directory removed on drop, so a failed rep never leaves a store
/// behind. Declared before the store so it drops after the store does.
struct TempStoreDir(std::path::PathBuf);

impl Drop for TempStoreDir {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

/// The ingest workload's knobs: which aspect to declare and how large a batch
/// to seal at once. The corpus itself comes from the caller (today the UCI
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
/// corpus: open a fresh [`SegmentStore`] per rep, declare the aspect as an
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
#[allow(clippy::cast_precision_loss)]
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
	let correctness = CorrectnessReport { output_count_ok: round_trips_exactly, expected_output_points: timestamps.len(), actual_output_points: read_ts.len(), values_finite: read_values.iter().flatten().all(|v| v.to_f64().is_some_and(f64::is_finite)) };

	let storage = Some(StorageEstimate::from_columns(values, timestamps, TimeUnit::Seconds, &BigDecimal::from(0)));
	let latency = LatencyStats::from_samples(&samples_ns);
	let latency_ci = Some(LatencyStats::bootstrap_cis(&samples_ns, &BootstrapConfig::default()));
	let throughput_points_per_sec = if latency.mean_ns > 0 {
		let secs = latency.mean_ns as f64 / 1e9;
		let points = timestamps.len() as f64;
		points / secs
	} else {
		0.0
	};

	Ok(BenchResult { schema_version: SCHEMA_VERSION, profile: profile.name.clone(), adapter: "weftdb".to_string(), workload: WORKLOAD_INGEST.to_string(), reps, dataset: DatasetMeta { input_points: timestamps.len(), output_points: read_ts.len(), irregular: timestamps.windows(3).any(|w| w[2] - w[1] != w[1] - w[0]), missingness_fraction, seed: 0, signal_shape: None }, latency, latency_ci, throughput_points_per_sec, timing, correctness, accuracy: None, storage, cold_warm: crate::schema::ColdWarm::from_samples(&samples_ns), gpu_pool: None, adapter_info: None })
}

/// Elapsed nanoseconds since `since`, saturated into a `u64`.
#[allow(clippy::cast_possible_truncation)]
fn span_ns(since: Instant) -> u64 {
	since.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
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
	async fn invalid_inputs_are_errors_not_silent_passes() {
		let corpus = load_uci_corpus_from_reader(Cursor::new(FIXTURE), DEFAULT_UCI_COLUMN, usize::MAX).expect("loads");
		let profile = IngestProfile::new("ingest-bad", "global_active_power", 2);
		assert!(run_ingest_on(&profile, &corpus.timestamps, &corpus.values, corpus.decimal_scale, 0.0, 0, 0).await.is_err());
		assert!(run_ingest_on(&profile, &corpus.timestamps[..1], &corpus.values[..1], corpus.decimal_scale, 0.0, 0, 1).await.is_err());
		assert!(run_ingest_on(&IngestProfile::new("ingest-bad", "global_active_power", 0), &corpus.timestamps, &corpus.values, corpus.decimal_scale, 0.0, 0, 1).await.is_err());
	}
}
