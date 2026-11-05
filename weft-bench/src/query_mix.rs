//! Seeded query mix over [`SegmentStore`](weftdb::SegmentStore) (fair protocol 1.1, #147).
//!
//! Every other store workload times one kind of operation at a time. A real store sees writes and
//! reads interleaved, and an engine tuned for one can lose on the mix. This workload replays the
//! soak harness's lane model ([`WorkloadModel::lane_for`]) against one store: each operation's
//! lane — ingest a batch, look up one point, or scan a range — is drawn from the seed and the
//! operation's index, so a published seed and lane weights regenerate the exact sequence on any
//! machine.
//!
//! The store starts with `rows` of the corpus laid end to end, as the other ingest workloads lay
//! it. An ingest seals the next batch; a point lookup reads one seeded row of what is stored so
//! far; a range scan reads a seeded window of `range_rows`. Every read is checked exactly against
//! the corpus it came from, and each lane becomes one result, its samples the lane's operations in
//! the order they ran and its digest over every answer the lane returned.

use std::time::Instant;

use bigdecimal::BigDecimal;
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;

use crate::{
	ingest::{fresh_store, seal_tiled, span_ns, IngestProfile, Tiled}, schema::{BenchResult, ColdWarm, CorrectnessReport, DatasetMeta, ResultDigest, TimingBreakdown, SCHEMA_VERSION}, soak_workload::{LaneWeights, WorkloadLane, WorkloadModel, WorkloadParams}, stats::LatencyStats
};

/// Workload class label recorded for each lane of the query mix.
pub const WORKLOAD_QUERY_MIX: &str = "query_mix";

/// The knobs of [`run_query_mix`].
#[derive(Debug, Clone, Copy)]
pub struct QueryMixParams {
	/// Rows the store holds before the first operation.
	pub rows: u64,
	/// Operations in the mix (`>= 1`).
	pub ops: usize,
	/// Rows a range scan reads (fewer only while the store is smaller).
	pub range_rows: u64,
	/// The lane mix; the soak harness's by default.
	pub weights: LaneWeights,
	/// Seed for the lanes and the read positions.
	pub seed: u64,
}

/// Salt separating the read positions' stream from the lanes' (which [`WorkloadModel`] draws).
const POSITION_SALT: u64 = 0x9E37_79B9_7F4A_7C15;

#[derive(Default)]
struct Lane {
	samples: Vec<u64>,
	rows: u64,
	exact: bool,
	digest: ResultDigest,
}

/// Run `params.ops` seeded operations against one store, handing each lane's result to `on_lane`
/// (ingest, point lookup, range scan, in that order; a lane the mix never drew is skipped).
///
/// # Errors
///
/// Returns an error if the corpus has fewer than two rows, mismatched columns or timestamps that
/// do not strictly rise, if a knob is zero or the weights sum to zero, if a copy would leave the
/// timestamp range, if a store operation fails, or if `on_lane` fails. A read that answers wrongly
/// is not an error: it fails that lane's gate.
pub async fn run_query_mix(profile: &IngestProfile, timestamps: &[i64], values: &[BigDecimal], value_scale: u8, missingness_fraction: f64, generation_ns: u64, params: QueryMixParams, mut on_lane: impl FnMut(BenchResult) -> anyhow::Result<()>) -> anyhow::Result<()> {
	anyhow::ensure!(timestamps.len() >= 2 && timestamps.len() == values.len(), "a query-mix corpus needs at least two rows and as many values as timestamps");
	anyhow::ensure!(profile.batch_rows > 0 && params.ops > 0 && params.range_rows > 0 && params.rows > 0, "query mix needs rows, ops, range rows and a batch size above zero");
	let stride = timestamps[1] - timestamps[0];
	anyhow::ensure!(stride > 0 && timestamps.windows(2).all(|w| w[0] < w[1]), "a query-mix corpus must have strictly rising timestamps");
	let tiled = Tiled { timestamps, values, period: timestamps[timestamps.len() - 1] - timestamps[0] + stride };
	let irregular = timestamps.windows(3).any(|w| w[2] - w[1] != w[1] - w[0]);
	let model = WorkloadModel::new(params.seed, WorkloadParams { lane_weights: params.weights, ..WorkloadParams::default() })?;
	let n = timestamps.len() as u64;
	let batch = profile.batch_rows as u64;

	let started = Instant::now();
	let (store, _dir) = fresh_store("mix", &profile.aspect, value_scale).await?;
	seal_tiled(&store, &profile.aspect, &tiled, params.rows, batch, &std::sync::atomic::AtomicU64::new(0)).await?;
	let mut stored = params.rows;
	let mut rng = ChaCha8Rng::seed_from_u64(params.seed ^ POSITION_SALT);
	let mut lanes: [Lane; 3] = std::array::from_fn(|_| Lane { exact: true, ..Lane::default() });
	for op in 0..params.ops as u64 {
		match model.lane_for(op) {
			WorkloadLane::Ingest => {
				// One batch, kept within one copy of the corpus as `seal_tiled` keeps it.
				let end = (stored + batch).min((stored / n + 1) * n);
				let chunk_ts = (stored..end).map(|i| tiled.timestamp(i)).collect::<anyhow::Result<Vec<_>>>()?;
				let at = usize::try_from(stored % n)?;
				let chunk_values = &values[at..at + chunk_ts.len()];
				let t0 = Instant::now();
				store.seal_declared(&profile.aspect, &chunk_ts, chunk_values).await.map_err(|e| anyhow::anyhow!("cannot seal {} rows into aspect {}: {e:#}", chunk_ts.len(), profile.aspect))?;
				let lane = &mut lanes[0];
				lane.samples.push(span_ns(t0));
				lane.rows += end - stored;
				lane.digest.row(stored, end);
				stored = end;
			}
			lane @ (WorkloadLane::PointLookup | WorkloadLane::RangeScan) => {
				let (slot, len) = if lane == WorkloadLane::PointLookup { (1, 1) } else { (2, params.range_rows.min(stored)) };
				let from = rng.random_range(0..=stored - len);
				let (start, end) = (tiled.timestamp(from)?, tiled.timestamp(from + len - 1)?);
				let t0 = Instant::now();
				let (read_ts, read_values) = store.read_time_range(&profile.aspect, start, end).await.map_err(|e| anyhow::anyhow!("cannot read aspect {}: {e:#}", profile.aspect))?;
				let lane = &mut lanes[slot];
				lane.samples.push(span_ns(t0));
				lane.rows += read_ts.len() as u64;
				let mut exact = read_ts.len() as u64 == len;
				for (i, (t, v)) in read_ts.iter().zip(&read_values).enumerate() {
					let index = from + i as u64;
					exact &= *t == tiled.timestamp(index)? && v.as_ref() == Some(&values[usize::try_from(index % n)?]);
					lane.digest.row(t, v.as_ref().map_or_else(String::new, |v| v.normalized().to_string()));
				}
				lane.exact &= exact;
			}
		}
	}

	for (lane, name) in lanes.into_iter().zip([WorkloadLane::Ingest, WorkloadLane::PointLookup, WorkloadLane::RangeScan]) {
		if lane.samples.is_empty() {
			continue;
		}
		let measured_ns = lane.samples.iter().copied().fold(0_u64, u64::saturating_add);
		on_lane(BenchResult { schema_version: SCHEMA_VERSION, profile: format!("{}-{}", profile.name, name.as_str()), adapter: "weftdb".to_string(), workload: WORKLOAD_QUERY_MIX.to_string(), reps: lane.samples.len(), dataset: DatasetMeta { input_points: usize::try_from(stored)?, output_points: usize::try_from(lane.rows)?, irregular, missingness_fraction, seed: params.seed, signal_shape: None }, latency: LatencyStats::from_samples(&lane.samples), latency_ci: None, throughput_points_per_sec: if measured_ns > 0 { lane.rows as f64 / (measured_ns as f64 / 1e9) } else { 0.0 }, timing: TimingBreakdown { dataset_generation_ns: generation_ns, measured_ns, end_to_end_ns: span_ns(started) }, correctness: CorrectnessReport { output_count_ok: lane.exact, expected_output_points: usize::try_from(lane.rows)?, actual_output_points: usize::try_from(lane.rows)?, values_finite: true, result_sha256: Some(lane.digest.finish()) }, accuracy: None, storage: None, cold_warm: ColdWarm::from_samples(&lane.samples), gpu_pool: None, adapter_info: None, growth: None, online: None, compressed_query: None, analytics: None, samples_ns: lane.samples, between_run: None, gpu_economics: None })?;
	}
	Ok(())
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

	async fn mix(weights: LaneWeights, seed: u64) -> Vec<BenchResult> {
		let corpus = load_uci_corpus_from_reader(Cursor::new(FIXTURE), DEFAULT_UCI_COLUMN, usize::MAX).expect("loads");
		let profile = IngestProfile::new("mix", "global_active_power", 5);
		let mut results = Vec::new();
		run_query_mix(&profile, &corpus.timestamps, &corpus.values, corpus.decimal_scale, 0.0, 0, QueryMixParams { rows: 40, ops: 60, range_rows: 9, weights, seed }, |r| {
			results.push(r);
			Ok(())
		})
		.await
		.expect("runs");
		results
	}

	#[tokio::test]
	async fn every_lane_answers_exactly_and_a_seed_replays_the_same_mix() {
		let results = mix(LaneWeights::default(), 11).await;
		assert_eq!(results.iter().map(|r| r.profile.as_str()).collect::<Vec<_>>(), ["mix-ingest", "mix-point_lookup", "mix-range_scan"]);
		assert_eq!(results.iter().map(|r| r.reps).sum::<usize>(), 60);
		for r in &results {
			assert!(r.correctness.passed(), "{}: {:?}", r.profile, r.correctness);
		}
		// Ingest grew the store while it was read: a scan sees the rows sealed before it.
		let ingested = results[0].dataset.output_points;
		assert_eq!(results[0].dataset.input_points, 40 + ingested);
		assert_eq!(results[1].dataset.output_points, results[1].reps, "a point lookup returns one row");
		let again = mix(LaneWeights::default(), 11).await;
		let digests = |rs: &[BenchResult]| rs.iter().map(|r| (r.reps, r.correctness.result_sha256.clone())).collect::<Vec<_>>();
		assert_eq!(digests(&results), digests(&again), "the same seed is the same mix and the same answers");
		assert_ne!(digests(&results), digests(&mix(LaneWeights::default(), 12).await));
	}

	#[tokio::test]
	async fn a_lane_weighted_out_is_not_run() {
		let results = mix(LaneWeights { ingest: 0, point_lookup: 1, range_scan: 0 }, 3).await;
		assert_eq!(results.iter().map(|r| r.profile.as_str()).collect::<Vec<_>>(), ["mix-point_lookup"]);
		assert!(mix_err(LaneWeights { ingest: 0, point_lookup: 0, range_scan: 0 }).await);
	}

	async fn mix_err(weights: LaneWeights) -> bool {
		let corpus = load_uci_corpus_from_reader(Cursor::new(FIXTURE), DEFAULT_UCI_COLUMN, usize::MAX).expect("loads");
		run_query_mix(&IngestProfile::new("mix", "a", 5), &corpus.timestamps, &corpus.values, corpus.decimal_scale, 0.0, 0, QueryMixParams { rows: 4, ops: 1, range_rows: 1, weights, seed: 0 }, |_| Ok(())).await.is_err()
	}
}
