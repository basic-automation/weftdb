//! Compressed-query workload over [`SegmentStore`] (#145).
//!
//! The question is what answering an aggregate straight from the compressed segments buys over
//! the way a client without that path answers it: read the raw rows out, then reduce them. Both
//! run against one store holding the real UCI corpus (laid end to end to `--cq-rows`, as the
//! growth curve lays it), sealed in `--ingest-batch` rows:
//!
//! - **in-store** — [`SegmentStore::downsample_range`], which prunes the index by time and folds
//!   each surviving segment into a mergeable partial (on the integer path for a scaled column),
//!   never materializing the range;
//! - **decode then reduce** — [`SegmentStore::read_time_range`] for the same window, then
//!   [`weft_reduce::reduce`] over the rows it returned, as a client holding the rows would.
//!
//! Each window size (an hour, a day, a week, 30 days and a year of minute rows) is one result.
//! Every query picks a seeded random window, runs both paths (alternating which goes first, so
//! neither always meets a cache the other warmed), and is gated on the two answers being
//! identical bucket for bucket.
//!
//! With [`CompressedQueryParams::compact_to`] the run then compacts the store
//! ([`SegmentStore::squash_aspect_to_target_rows`]) and asks the same seeded windows again, as
//! `…-compacted` results: the post-compaction reads of the fair protocol (1.1). A compacted
//! window must also answer exactly as it did before the compaction, digest for digest.
//!
//! The aggregations are the streaming ones (min, max, avg, sum,
//! first, last) at hourly buckets: an exact percentile needs every row of a bucket, so it has no
//! compressed shortcut to measure.

use std::time::Instant;

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};
use splimes::{Point, Resolution};
use weft_reduce::{reduce, Aggregation};

use crate::{
	ingest::{fresh_store, seal_tiled, span_ns, IngestProfile, Tiled}, schema::{BenchResult, CorrectnessReport, DatasetMeta, TimingBreakdown, SCHEMA_VERSION}, stats::LatencyStats
};

/// Workload class label recorded for each window size of the compressed-query workload.
pub const WORKLOAD_COMPRESSED_QUERY: &str = "compressed_query";

/// The reductions both paths compute: the streaming ones, which a segment can fold on its own.
pub const COMPRESSED_QUERY_AGGREGATIONS: [Aggregation; 6] = [Aggregation::Min, Aggregation::Max, Aggregation::Avg, Aggregation::Sum, Aggregation::First, Aggregation::Last];

/// The window sizes, in rows of the minute-stride corpus, with their labels.
pub const COMPRESSED_QUERY_WINDOWS: [(&str, u64); 5] = [("1h", 60), ("1d", 1_440), ("1w", 10_080), ("30d", 43_200), ("1y", 525_600)];

/// The knobs of [`run_compressed_query`].
#[derive(Debug, Clone, Copy)]
pub struct CompressedQueryParams {
	/// Rows the store holds; the corpus is laid end to end to reach it.
	pub rows: u64,
	/// Queries per window size (`>= 1`).
	pub queries: usize,
	/// Seed for the query windows.
	pub seed: u64,
	/// Compact the store to segments of this many rows, then ask the same windows again.
	pub compact_to: Option<u64>,
}

/// The decode-then-reduce baseline and the store's footprint for one window size (schema v26).
///
/// The [`BenchResult`] carrying this reports the in-store path: its latency samples are the
/// `downsample_range` calls, its throughput the rows those windows cover per second of them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompressedQueryPoint {
	/// The window size's label (`1h`, `1d`, …).
	pub window: String,
	/// Rows each window asks for (fewer only if the store is smaller).
	pub window_rows: u64,
	/// Rows the windows covered, summed over the queries.
	pub rows_scanned: u64,
	/// Buckets the queries returned, summed.
	pub buckets: usize,
	/// Median latency of the decode-then-reduce path, in nanoseconds.
	pub decode_p50_ns: u64,
	/// 99th-percentile latency of the decode-then-reduce path, in nanoseconds.
	pub decode_p99_ns: u64,
	/// Mean latency of the decode-then-reduce path, in nanoseconds.
	pub decode_mean_ns: u64,
	/// The decode path's median over the in-store path's: above 1 the store is faster.
	pub speedup_p50: f64,
	/// Segments the store holds.
	pub segments: usize,
	/// Bytes per stored row on disk, segment headers and indexes included.
	pub bytes_per_point: f64,
}

/// Run the compressed-query workload over a store of `params.rows` rows.
///
/// For each window size, `params.queries` seeded windows go through both paths, and each size's
/// result reaches `on_window` as soon as it is measured.
///
/// # Errors
///
/// Returns an error if the corpus has fewer than two rows, mismatched columns or timestamps that
/// do not strictly rise, if `batch_rows` or `queries` is zero, if `params.rows < 2` or a copy
/// would leave the timestamp range, if a store operation or a reduction fails, or if
/// `on_window` fails. Two paths disagreeing is not an error: it fails that size's gate.
#[allow(clippy::cast_precision_loss, clippy::too_many_arguments)]
pub async fn run_compressed_query(profile: &IngestProfile, timestamps: &[i64], values: &[BigDecimal], value_scale: u8, missingness_fraction: f64, generation_ns: u64, params: CompressedQueryParams, mut on_window: impl FnMut(BenchResult) -> anyhow::Result<()>) -> anyhow::Result<()> {
	anyhow::ensure!(timestamps.len() >= 2, "a compressed-query corpus needs at least two rows");
	anyhow::ensure!(timestamps.len() == values.len(), "compressed-query corpus timestamps and values differ in length");
	anyhow::ensure!(profile.batch_rows > 0, "ingest batch_rows must be > 0");
	anyhow::ensure!(params.queries > 0, "compressed query needs at least one query per window");
	anyhow::ensure!(params.rows >= 2, "compressed query needs at least two rows");
	let stride = timestamps[1] - timestamps[0];
	anyhow::ensure!(stride > 0 && timestamps.windows(2).all(|w| w[0] < w[1]), "a compressed-query corpus must have strictly rising timestamps");
	let tiled = Tiled { timestamps, values, period: timestamps[timestamps.len() - 1] - timestamps[0] + stride };
	tiled.timestamp(params.rows - 1)?;
	let irregular = timestamps.windows(3).any(|w| w[2] - w[1] != w[1] - w[0]);

	let (store, _dir) = fresh_store("cq", &profile.aspect, value_scale).await?;
	seal_tiled(&store, &profile.aspect, &tiled, params.rows, profile.batch_rows as u64, &std::sync::atomic::AtomicU64::new(0)).await?;
	let stats = store.aspect_stats(&profile.aspect).await.map_err(|e| anyhow::anyhow!("cannot read the stats of aspect {}: {e:#}", profile.aspect))?;
	anyhow::ensure!(stats.total_rows == params.rows, "the store holds {} rows after sealing {}", stats.total_rows, params.rows);

	let before = windows_pass(&Pass { store: &store, profile, tiled: &tiled, params, stats: &stats, irregular, missingness_fraction, generation_ns, suffix: "", before: None }, &mut on_window).await?;
	if let Some(target) = params.compact_to {
		let t0 = Instant::now();
		store.squash_aspect_to_target_rows(&profile.aspect, usize::try_from(target)?).await.map_err(|e| anyhow::anyhow!("cannot compact aspect {}: {e:#}", profile.aspect))?;
		let compact_ns = span_ns(t0);
		let stats_after = store.aspect_stats(&profile.aspect).await.map_err(|e| anyhow::anyhow!("cannot read the stats of aspect {}: {e:#}", profile.aspect))?;
		anyhow::ensure!(stats_after.total_rows == params.rows, "the store holds {} rows after compacting {}", stats_after.total_rows, params.rows);
		eprintln!("compacted {} -> {} segments in {:.3} s", stats.segment_count, stats_after.segment_count, std::time::Duration::from_nanos(compact_ns).as_secs_f64());
		windows_pass(&Pass { store: &store, profile, tiled: &tiled, params, stats: &stats_after, irregular, missingness_fraction, generation_ns, suffix: "-compacted", before: Some(&before) }, &mut on_window).await?;
	}
	Ok(())
}

/// One pass of every window size over the store as it stands.
struct Pass<'a> {
	store: &'a weftdb::SegmentStore,
	profile: &'a IngestProfile,
	tiled: &'a Tiled<'a>,
	params: CompressedQueryParams,
	stats: &'a weftdb::AspectStorageStats,
	irregular: bool,
	missingness_fraction: f64,
	generation_ns: u64,
	/// Appended to each result's profile (`-compacted`).
	suffix: &'a str,
	/// The digests an earlier pass returned, which this one must reproduce.
	before: Option<&'a [String]>,
}

/// Run every window size once; returns each size's answer digest, in order.
#[allow(clippy::cast_precision_loss)]
async fn windows_pass(pass: &Pass<'_>, on_window: &mut impl FnMut(BenchResult) -> anyhow::Result<()>) -> anyhow::Result<Vec<String>> {
	let Pass { store, profile, tiled, params, stats, irregular, missingness_fraction, generation_ns, suffix, before } = *pass;
	let mut rng = ChaCha8Rng::seed_from_u64(params.seed);
	let mut digests = Vec::with_capacity(COMPRESSED_QUERY_WINDOWS.len());
	for (label, nominal) in COMPRESSED_QUERY_WINDOWS {
		let window_rows = nominal.min(params.rows);
		let window_start = Instant::now();
		let (mut in_store_ns, mut decode_ns) = (Vec::with_capacity(params.queries), Vec::with_capacity(params.queries));
		let (mut rows_scanned, mut in_store_buckets, mut decode_buckets) = (0_u64, 0_usize, 0_usize);
		let mut identical = true;
		let mut digest = crate::schema::ResultDigest::default();
		for query in 0..params.queries {
			let from = rng.random_range(0..=params.rows - window_rows);
			let (start, end) = (tiled.timestamp(from)?, tiled.timestamp(from + window_rows - 1)?);
			let mut in_store = None;
			let mut decoded = None;
			// Alternate the order, so neither path always runs on a cache the other warmed.
			for path in if query % 2 == 0 { [0, 1] } else { [1, 0] } {
				let t0 = Instant::now();
				if path == 0 {
					in_store = Some(store.downsample_range(&profile.aspect, start, end, Resolution::Hours, &COMPRESSED_QUERY_AGGREGATIONS).await.map_err(|e| anyhow::anyhow!("cannot downsample aspect {}: {e:#}", profile.aspect))?);
					in_store_ns.push(span_ns(t0));
				} else {
					let (read_ts, read_values) = store.read_time_range(&profile.aspect, start, end).await.map_err(|e| anyhow::anyhow!("cannot read aspect {}: {e:#}", profile.aspect))?;
					let points = points(&read_ts, read_values)?;
					let buckets = reduce(&points, Resolution::Hours, None, None, &COMPRESSED_QUERY_AGGREGATIONS).map_err(|e| anyhow::anyhow!("cannot reduce {} rows: {e}", points.len()))?;
					decode_ns.push(span_ns(t0));
					rows_scanned += read_ts.len() as u64;
					decoded = Some(buckets);
				}
			}
			let (in_store, decoded) = (in_store.unwrap_or_default(), decoded.unwrap_or_default());
			for (start, row) in in_store.iter().map(crate::schema::bucket_row) {
				digest.row(start, row);
			}
			identical &= in_store == decoded && in_store.iter().map(|b| b.count as u64).sum::<u64>() == window_rows;
			in_store_buckets += in_store.len();
			decode_buckets += decoded.len();
		}

		let digest = digest.finish();
		// After a compaction a window must answer exactly as it did before.
		identical &= before.is_none_or(|b| b.get(digests.len()) == Some(&digest));
		digests.push(digest.clone());
		let latency = LatencyStats::from_samples(&in_store_ns);
		let decode = LatencyStats::from_samples(&decode_ns);
		let measured_ns = in_store_ns.iter().copied().fold(0_u64, u64::saturating_add);
		let point = CompressedQueryPoint { window: label.to_string(), window_rows, rows_scanned, buckets: in_store_buckets, decode_p50_ns: decode.p50_ns, decode_p99_ns: decode.p99_ns, decode_mean_ns: decode.mean_ns, speedup_p50: if latency.p50_ns > 0 { decode.p50_ns as f64 / latency.p50_ns as f64 } else { 0.0 }, segments: stats.segment_count, bytes_per_point: stats.bytes_per_point };
		on_window(BenchResult { schema_version: SCHEMA_VERSION, profile: format!("{}-{label}{suffix}", profile.name), adapter: "weftdb".to_string(), workload: WORKLOAD_COMPRESSED_QUERY.to_string(), reps: in_store_ns.len(), dataset: DatasetMeta { input_points: usize::try_from(params.rows)?, output_points: in_store_buckets, irregular, missingness_fraction, seed: params.seed, signal_shape: None }, throughput_points_per_sec: if measured_ns > 0 { rows_scanned as f64 / (measured_ns as f64 / 1e9) } else { 0.0 }, cold_warm: crate::schema::ColdWarm::from_samples(&in_store_ns), latency, latency_ci: None, timing: TimingBreakdown { dataset_generation_ns: generation_ns, measured_ns, end_to_end_ns: span_ns(window_start) }, correctness: CorrectnessReport { output_count_ok: identical, expected_output_points: decode_buckets, actual_output_points: in_store_buckets, values_finite: true, result_sha256: Some(digest) }, accuracy: None, storage: None, gpu_pool: None, adapter_info: None, growth: None, online: None, compressed_query: Some(point), analytics: None, samples_ns: in_store_ns.clone(), between_run: None })?;
	}
	Ok(digests)
}

/// The rows a range read returned, as the points a client would reduce (absent values skipped).
fn points(timestamps: &[i64], values: Vec<Option<BigDecimal>>) -> anyhow::Result<Vec<Point>> {
	timestamps.iter().zip(values).filter_map(|(&t, v)| v.map(|v| DateTime::<Utc>::from_timestamp(t, 0).map(|at| Point::new(at, v)).ok_or_else(|| anyhow::anyhow!("epoch {t} is outside the representable instant range")))).collect()
}

#[cfg(test)]
mod tests {
	use std::io::Cursor;

	use super::*;
	use crate::uci::{load_uci_corpus_from_reader, DEFAULT_UCI_COLUMN};

	const FIXTURE: &str = "Date;Time;Global_active_power;Global_reactive_power;Voltage;Global_intensity;Sub_metering_1;Sub_metering_2;Sub_metering_3\n\
		16/12/2006;17:24:00;4.216;0.418;234.840;18.400;0.000;1.000;17.000\n\
		16/12/2006;17:25:00;5.360;0.436;233.630;23.000;0.000;1.000;16.000\n\
		16/12/2006;17:26:00;5.374;0.498;233.290;23.000;0.000;2.000;17.000\n\
		16/12/2006;17:27:00;5.388;0.502;233.740;23.000;0.000;1.000;17.000\n";

	#[tokio::test]
	async fn both_paths_agree_bucket_for_bucket_at_every_window_size() {
		let corpus = load_uci_corpus_from_reader(Cursor::new(FIXTURE), DEFAULT_UCI_COLUMN, usize::MAX).expect("loads");
		let profile = IngestProfile::new("cq", "global_active_power", 7);
		let mut results = Vec::new();
		run_compressed_query(&profile, &corpus.timestamps, &corpus.values, corpus.decimal_scale, 0.0, 0, CompressedQueryParams { rows: 200, queries: 5, seed: 3, compact_to: None }, |r| {
			results.push(r);
			Ok(())
		})
		.await
		.expect("runs");
		assert_eq!(results.iter().map(|r| r.profile.as_str()).collect::<Vec<_>>(), ["cq-1h", "cq-1d", "cq-1w", "cq-30d", "cq-1y"]);
		for result in &results {
			assert!(result.correctness.passed(), "{}: {:?}", result.profile, result.correctness);
			assert_eq!((result.workload.as_str(), result.reps), ("compressed_query", 5));
			let point = result.compressed_query.as_ref().expect("point");
			assert!(point.window_rows <= 200 && point.rows_scanned == 5 * point.window_rows, "{point:?}");
			assert!(point.buckets > 0 && point.segments >= 200 / 7);
		}
		assert_eq!(results[0].compressed_query.as_ref().expect("point").window_rows, 60);
		assert_eq!(results[4].compressed_query.as_ref().expect("point").window_rows, 200, "a window larger than the store asks for the whole store");
	}

	#[tokio::test]
	async fn after_compaction_the_same_windows_answer_the_same_from_fewer_segments() {
		let corpus = load_uci_corpus_from_reader(Cursor::new(FIXTURE), DEFAULT_UCI_COLUMN, usize::MAX).expect("loads");
		let profile = IngestProfile::new("cq", "global_active_power", 7);
		let mut results = Vec::new();
		run_compressed_query(&profile, &corpus.timestamps, &corpus.values, corpus.decimal_scale, 0.0, 0, CompressedQueryParams { rows: 200, queries: 4, seed: 9, compact_to: Some(100) }, |r| {
			results.push(r);
			Ok(())
		})
		.await
		.expect("runs");
		assert_eq!(results.len(), 10);
		let (before, after) = results.split_at(5);
		for (b, a) in before.iter().zip(after) {
			assert_eq!(a.profile, format!("{}-compacted", b.profile));
			assert!(a.correctness.passed(), "{}: {:?}", a.profile, a.correctness);
			assert_eq!(a.correctness.result_sha256, b.correctness.result_sha256, "compaction must not change an answer");
			let segments = |r: &BenchResult| r.compressed_query.as_ref().expect("point").segments;
			assert!(segments(a) < segments(b), "{} -> {}", segments(b), segments(a));
		}
	}

	#[tokio::test]
	async fn compressed_query_refuses_empty_knobs() {
		let corpus = load_uci_corpus_from_reader(Cursor::new(FIXTURE), DEFAULT_UCI_COLUMN, usize::MAX).expect("loads");
		let profile = IngestProfile::new("cq", "global_active_power", 2);
		for params in [CompressedQueryParams { rows: 8, queries: 0, seed: 0, compact_to: None }, CompressedQueryParams { rows: 1, queries: 1, seed: 0, compact_to: None }] {
			assert!(run_compressed_query(&profile, &corpus.timestamps, &corpus.values, corpus.decimal_scale, 0.0, 0, params, |_| Ok(())).await.is_err(), "{params:?}");
		}
	}
}
