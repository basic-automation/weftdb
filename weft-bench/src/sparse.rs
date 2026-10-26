//! Block-missingness workload — a wide, sparse table where a series is present for a whole
//! block or absent from it.
//!
//! Telemetry does not arrive one sample at a time with random holes; a device talks for a
//! while and then says nothing, so a whole block of rows is missing for that series while its
//! neighbours are full. That is the shape a per-column presence bitmap is meant to pay for
//! (roadmap FRE-2, SOA-6), and it is the shape this workload generates: a (series × block)
//! grid where each cell is present with a seeded probability, and a present cell fills every
//! row of that block for that series.
//!
//! The measured query is the block-aligned scan a segment read performs: for each series, walk
//! its blocks in time order and produce one aggregate over the values that block holds,
//! stepping over the blocks it holds none of. Empty blocks are not free here — the scan still
//! visits their nulls — which is exactly the cost a presence bitmap would later be judged
//! against. So this workload's numbers are the *before* picture and should be cited as such.
//!
//! Values are held the way the store holds them: one Arrow `Float64Array` per series spanning
//! every block, with a validity bitmap carrying the holes. The scan reads `value(i)` where a
//! row is valid and skips the invalid ones, so the cost of a hole is the cost of a hole in a
//! real columnar buffer, not the cost of an `Option` in a `Vec`.

use std::time::Instant;

use arrow_array::{Array, Float64Array};
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;

use crate::{
	schema::{BenchResult, ColdWarm, CorrectnessReport, DatasetMeta, TimingBreakdown, SCHEMA_VERSION}, stats::{BootstrapConfig, LatencyStats}
};

/// Workload class label recorded for the block-missingness workload.
const WORKLOAD_SPARSE: &str = "sparse";

/// Default seed for block-missingness profiles.
pub const DEFAULT_SPARSE_SEED: u64 = 0x50A8_C0DE_1B10_0C00;

/// A value rounded to two decimals, as the other generators round: the exact value does not
/// matter to the measurement, only that the generator and the scan agree on it bit for bit.
fn round2(value: f64) -> f64 {
	(value * 100.0).round() / 100.0
}

/// The value at `row` of series `series`: a per-series amplitude and offset over a slow
/// sinusoid, so a scan does real floating-point work and a wrong block offset shows up.
fn signal(series: usize, row: usize) -> f64 {
	use std::f64::consts::TAU;
	#[allow(clippy::cast_precision_loss)]
	let (series_f, row_f) = (series as f64, row as f64);
	round2((30.0 + 10.0 * (series_f * 0.7).sin()) * (row_f / 512.0 * TAU).sin() + series_f)
}

/// The mean of `sum` over `count` values, or 0.0 for an empty count.
fn mean(sum: f64, count: usize) -> f64 {
	#[allow(clippy::cast_precision_loss)]
	if count > 0 {
		sum / count as f64
	} else {
		0.0
	}
}

/// The tunable knobs of a block-missingness profile.
#[derive(Debug, Clone, PartialEq)]
pub struct SparseParams {
	/// RNG seed; publishing it regenerates the grid and its holes exactly.
	pub seed: u64,
	/// Series in the table, one column each.
	pub series: usize,
	/// Blocks per series.
	pub blocks: usize,
	/// Rows in a block.
	pub rows_per_block: usize,
	/// Share of the (series × block) cells that hold values, in percent (`1..=100`).
	pub density_percent: u32,
}

impl Default for SparseParams {
	/// A wide, mostly-empty table: 16 series, 256 blocks of 128 rows, one cell in five
	/// present — 32 768 rows per series of which about a fifth carry anything.
	fn default() -> Self {
		Self { seed: DEFAULT_SPARSE_SEED, series: 16, blocks: 256, rows_per_block: 128, density_percent: 20 }
	}
}

/// A reproducible block-missingness profile.
#[derive(Debug, Clone, PartialEq)]
pub struct SparseProfile {
	/// Human-readable profile name, recorded in results.
	pub name: String,
	/// The knobs, clamped to their valid ranges.
	pub params: SparseParams,
}

/// A generated table, plus what the generator knows about its own holes so a run can check the
/// scan against it.
#[derive(Debug, Clone)]
pub struct SparseTable {
	/// One column per series, `blocks × rows_per_block` long, null where absent.
	pub columns: Vec<Float64Array>,
	/// The `(series, block)` cells that hold values, in the order the scan visits them.
	pub present: Vec<(usize, usize)>,
	/// The mean of each present cell, in `present` order: what the scan must return.
	pub expected: Vec<f64>,
	/// Values the table would hold if every cell were present.
	pub capacity: usize,
	/// Values it actually holds.
	pub populated: usize,
}

impl SparseProfile {
	/// Build a profile, clamping the knobs: at least one series, block and row, and a density
	/// between 1% and 100% — an entirely empty table has nothing to scan.
	#[must_use]
	pub fn new(name: impl Into<String>, params: SparseParams) -> Self {
		let params = SparseParams { seed: params.seed, series: params.series.max(1), blocks: params.blocks.max(1), rows_per_block: params.rows_per_block.max(1), density_percent: params.density_percent.clamp(1, 100) };
		Self { name: name.into(), params }
	}

	/// Generate the table: walk the (series × block) grid with the profile's seed, draw each
	/// cell's presence, and fill a present cell with every row of that block.
	#[must_use]
	pub fn generate(&self) -> SparseTable {
		let p = &self.params;
		let mut rng = ChaCha8Rng::seed_from_u64(p.seed);
		let share = f64::from(p.density_percent) / 100.0;
		let rows = p.blocks.saturating_mul(p.rows_per_block);
		let mut columns = Vec::with_capacity(p.series);
		let mut present = Vec::new();
		let mut expected = Vec::new();

		for series in 0..p.series {
			let mut values: Vec<Option<f64>> = Vec::with_capacity(rows);
			for block in 0..p.blocks {
				if rng.random_range(0.0..1.0) < share {
					let mut sum = 0_f64;
					for row in 0..p.rows_per_block {
						let value = signal(series, block.saturating_mul(p.rows_per_block).saturating_add(row));
						sum += value;
						values.push(Some(value));
					}
					present.push((series, block));
					expected.push(mean(sum, p.rows_per_block));
				} else {
					values.resize(values.len() + p.rows_per_block, None);
				}
			}
			columns.push(Float64Array::from(values));
		}

		let capacity = p.series.saturating_mul(rows);
		let populated = present.len().saturating_mul(p.rows_per_block);
		SparseTable { columns, present, expected, capacity, populated }
	}
}

/// Scan one column block by block, returning one mean per block that holds values.
///
/// This is the measured operation: a block-aligned read that must look at every block, present
/// or not, because nothing in the column yet says which ones hold values.
fn scan(column: &Float64Array, rows_per_block: usize) -> Vec<f64> {
	let mut out = Vec::new();
	let mut start = 0;
	while start < column.len() {
		let end = (start + rows_per_block).min(column.len());
		let mut sum = 0_f64;
		let mut count = 0_usize;
		for row in start..end {
			if column.is_valid(row) {
				sum += column.value(row);
				count += 1;
			}
		}
		if count > 0 {
			out.push(mean(sum, count));
		}
		start = end;
	}
	out
}

/// Scan every column, concatenating the per-series aggregates in series order.
fn scan_all(table: &SparseTable, rows_per_block: usize) -> Vec<f64> {
	let mut out = Vec::with_capacity(table.expected.len());
	for column in &table.columns {
		out.extend(scan(column, rows_per_block));
	}
	out
}

/// The `(series, block)` cells a grid of `p` leaves empty, given the cells that are present.
#[must_use]
pub fn absent_cells(p: &SparseParams, present: &[(usize, usize)]) -> Vec<(usize, usize)> {
	let known: std::collections::HashSet<(usize, usize)> = present.iter().copied().collect();
	(0..p.series).flat_map(|series| (0..p.blocks).map(move |block| (series, block))).filter(|cell| !known.contains(cell)).collect()
}

/// Run a block-missingness profile for `reps` timed repetitions and return a fully populated
/// [`BenchResult`] (workload `sparse`).
///
/// The table is generated once, as setup; each rep runs the whole scan. Throughput is values
/// read per second, so a sparser table at the same capacity scores lower by design: the
/// quantity of interest is the cost of the holes, not the count of the hits.
///
/// # Errors
///
/// Returns an error if `reps == 0`, or if the generated table holds no values to scan.
pub fn run_sparse(profile: &SparseProfile, reps: usize) -> anyhow::Result<BenchResult> {
	anyhow::ensure!(reps > 0, "reps must be > 0");
	let setup_start = Instant::now();
	let table = profile.generate();
	let generation_ns = span_ns(setup_start);
	anyhow::ensure!(!table.present.is_empty(), "the generated table holds no values (raise --sp-density)");

	let run_start = Instant::now();
	let mut samples_ns = Vec::with_capacity(reps);
	let mut scanned = Vec::new();
	for _ in 0..reps {
		let t0 = Instant::now();
		scanned = scan_all(&table, profile.params.rows_per_block);
		samples_ns.push(span_ns(t0));
	}
	let measured_ns = samples_ns.iter().copied().fold(0_u64, u64::saturating_add);
	let timing = TimingBreakdown { dataset_generation_ns: generation_ns, measured_ns, end_to_end_ns: span_ns(run_start).saturating_add(generation_ns) };

	// Correctness: one aggregate per populated cell, in the order the generator recorded, each
	// equal to the mean the generator computed, and every value finite.
	let aggregates_match = scanned.len() == table.expected.len() && scanned.iter().zip(table.expected.iter()).all(|(got, want)| (got - want).abs() <= 1e-9);
	let correctness = CorrectnessReport { output_count_ok: aggregates_match, expected_output_points: table.expected.len(), actual_output_points: scanned.len(), values_finite: scanned.iter().all(|value| value.is_finite()) };

	let latency = LatencyStats::from_samples(&samples_ns);
	let latency_ci = Some(LatencyStats::bootstrap_cis(&samples_ns, &BootstrapConfig { seed: profile.params.seed ^ 0x50A8_C1EC_7B10, ..BootstrapConfig::default() }));
	#[allow(clippy::cast_precision_loss)]
	let throughput_points_per_sec = if latency.mean_ns > 0 { table.populated as f64 / (latency.mean_ns as f64 / 1e9) } else { 0.0 };
	#[allow(clippy::cast_precision_loss)]
	let missingness_fraction = if table.capacity > 0 { 1.0 - table.populated as f64 / table.capacity as f64 } else { 0.0 };

	Ok(BenchResult { schema_version: SCHEMA_VERSION, profile: profile.name.clone(), adapter: "weftdb".to_string(), workload: WORKLOAD_SPARSE.to_string(), reps, dataset: DatasetMeta { input_points: table.populated, output_points: scanned.len(), irregular: false, missingness_fraction, seed: profile.params.seed, signal_shape: None }, latency, latency_ci, throughput_points_per_sec, timing, correctness, accuracy: None, storage: None, cold_warm: ColdWarm::from_samples(&samples_ns), gpu_pool: None, adapter_info: None })
}

/// The nanoseconds elapsed since `since`, saturating at `u64::MAX`.
fn span_ns(since: Instant) -> u64 {
	u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn tiny(density_percent: u32) -> SparseProfile {
		SparseProfile::new("sp-tiny", SparseParams { seed: 7, series: 3, blocks: 8, rows_per_block: 16, density_percent })
	}

	#[test]
	fn generation_is_seeded_and_holes_are_block_sized() {
		let profile = tiny(40);
		let table = profile.generate();
		assert_eq!(table.expected, profile.generate().expected, "same seed, same holes and means");
		assert_eq!(table.capacity, 3 * 8 * 16);
		assert_eq!(table.populated, table.present.len() * 16);
		for (series, block) in &table.present {
			let column = &table.columns[*series];
			let start = block * 16;
			assert!((start..start + 16).all(|row| column.is_valid(row)), "a present cell fills its whole block");
		}
		for (series, block) in absent_cells(&profile.params, &table.present) {
			let column = &table.columns[series];
			let start = block * 16;
			assert!((start..start + 16).all(|row| !column.is_valid(row)), "an absent cell is empty for its whole block");
		}
	}

	#[test]
	fn density_and_clamping_behave() {
		let full = tiny(100).generate();
		assert_eq!(full.present.len(), 3 * 8, "a dense grid has no holes");
		assert_eq!(full.populated, full.capacity);
		assert!(tiny(1).generate().populated < full.populated, "one percent is sparser than all");
		// Zero is not a usable density: the knobs clamp it up rather than generating nothing.
		assert_eq!(tiny(0).params.density_percent, 1);
		assert_eq!(tiny(1000).params.density_percent, 100);
	}

	#[test]
	fn a_one_row_block_is_still_a_valid_grid() {
		// The smallest block the knobs allow: a cell is a single value, so block granularity and
		// row granularity coincide, and nothing in the scan may divide by a zero row count.
		let profile = SparseProfile::new("sp-rows1", SparseParams { seed: 3, series: 2, blocks: 6, rows_per_block: 0, density_percent: 50 });
		assert_eq!(profile.params.rows_per_block, 1, "the row knob clamps up like the others");
		let table = profile.generate();
		assert_eq!(table.capacity, 12, "2 series x 6 blocks of one row");
		assert_eq!(table.populated, table.present.len(), "a populated cell holds exactly one value");
		assert_eq!(scan_all(&table, profile.params.rows_per_block), table.expected, "each present row is its own mean");
	}

	#[test]
	fn scan_returns_the_generators_means() {
		let profile = tiny(50);
		let table = profile.generate();
		let scanned = scan_all(&table, profile.params.rows_per_block);
		assert_eq!(scanned.len(), table.expected.len());
		for (got, want) in scanned.iter().zip(&table.expected) {
			assert!((got - want).abs() <= 1e-9, "{got} vs {want}");
		}
	}

	#[test]
	fn run_reports_the_holes() {
		let result = run_sparse(&tiny(30), 2).expect("run");
		assert_eq!(result.workload, WORKLOAD_SPARSE);
		assert!(result.correctness.output_count_ok, "the scan matches the generator");
		assert!(result.dataset.missingness_fraction > 0.0, "a 30% grid is mostly holes");
		assert_eq!(result.dataset.input_points, result.dataset.output_points * 16, "16 rows per populated cell");
		assert!(run_sparse(&tiny(30), 0).is_err(), "zero reps is refused");
	}
}
