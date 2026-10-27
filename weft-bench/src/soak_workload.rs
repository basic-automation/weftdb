//! Deterministic workload model, lanes, format-exact value grids and body
//! encoders (1.0 plan SOA-4, W2).
//!
//! This is the slice SOA-1's module doc promised: the workload model derives
//! its work from [`SoakConfig::seed`](crate::soak::SoakConfig) alone — no wall
//! clock, no process state, no second seed — so the same seed and the same
//! parameters build the same workload on any machine, and a published seed
//! regenerates a soak's exact traffic. SOA-3's scheduler supplies the ticks;
//! this module supplies what each tick does.
//!
//! **Model.** A [`WorkloadModel`] is a seed plus [`WorkloadParams`]. Every
//! operation on it is a pure function of the tick: [`WorkloadModel::step`]
//! returns the same [`WorkloadStep`] for the same tick every time, and any
//! tick can be replayed without generating its predecessors, because the
//! per-tick random stream is seeded from the seed and the tick (never from
//! a running generator's state).
//!
//! **Lanes.** A tick belongs to exactly one [`WorkloadLane`]: `ingest`
//! (write new points), `point_lookup`, or `range_scan` — the three
//! operation families the bench crate already measures
//! ([`crate::ingest`], [`crate::point_lookup`], [`crate::range_fetch`]).
//! The lane mix is [`LaneWeights`]: a tick's lane is drawn from the
//! tick's stream against the cumulative weights, so the mix is a property
//! of the model, not of the machine running it. The weights sum must be
//! positive; a model with no lane to run is a construction error, not a
//! run that silently does nothing.
//!
//! **Format-exact value grids.** A step's data is a [`ValueGrid`]: one
//! column per series, one row per point, each cell present with the
//! model's density or absent (the sparse shape, walked series-major like
//! [`crate::sparse`]). Each column carries exactly one [`ValueFormat`],
//! cycling through the five field types the line protocol speaks, so a
//! column's values are always encoded in that format's exact wire form —
//! an integer is never silently widened to a float, a boolean is never
//! written as `1`, a string is never left unquoted. [`GridValue::encode_field`]
//! is that encoding, and parsing it back through [`crate::line_protocol::parse`]
//! returns the same value: the encoder and the parser agree by test, not
//! by convention.
//!
//! **Body encoders.** A step's [`WorkloadStep::body`] is line-protocol
//! text: one line per present cell, `soak,lane=<lane>,series=s<series>
//! value=<field> <timestamp_ns>`, with timestamps advancing one second
//! per row from a fixed epoch (2020-01-01T00:00:00Z, the same anchor the
//! synthetic profiles use) so a body is fully determined by its tick.
//! The body carries no free text — series names are generated
//! (`s0`, `s1`, …), lane names are fixed — so, like the scheduler's
//! report, it needs no redaction pass.
//!
//! **No schema bump.** The workload model is not part of the run record:
//! it adds no field to [`SoakRun`](crate::soak::SoakRun) or
//! [`SoakConfig`], so
//! [`SOAK_SCHEMA_VERSION`](crate::soak::SOAK_SCHEMA_VERSION) stays at v1
//! per SOA-1's bump rule. The model and its steps serialize on their own
//! for artifacts that want them.

use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

use crate::{line_protocol::FieldValue, soak::SoakConfig};

/// Measurement name every workload body is written under.
pub const SOAK_WORKLOAD_MEASUREMENT: &str = "soak";

/// Epoch anchor for workload timestamps, in nanoseconds.
///
/// 2020-01-01T00:00:00Z, the same anchor the synthetic profiles use, so
/// generated timestamps are stable across machines and runs.
pub const SOAK_WORKLOAD_EPOCH_NS: i64 = 1_577_836_800_000_000_000;

/// Nanoseconds between consecutive grid rows (one second per row).
const ROW_INTERVAL_NS: i64 = 1_000_000_000;

/// A value format: one of the field types the line protocol speaks. A grid
/// column carries exactly one of these, and every value in the column is
/// encoded in that format's exact wire form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ValueFormat {
	/// A floating-point field: a bare number (`1.5`, `20`).
	Float,
	/// A signed-integer field: a number with the `i` suffix (`-42i`).
	Integer,
	/// An unsigned-integer field: a number with the `u` suffix (`42u`).
	Unsigned,
	/// A boolean field: `t` or `f`.
	Boolean,
	/// A string field: double-quoted, with `"` and `\` escaped.
	Text,
}

impl ValueFormat {
	/// All formats, in the order columns cycle through them: series `n`
	/// uses format `n` modulo this list's length.
	const ALL: [Self; 5] = [Self::Float, Self::Integer, Self::Unsigned, Self::Boolean, Self::Text];

	/// The format of series `series`: the cycle above, so a grid's columns
	/// cover every format once before repeating.
	#[must_use]
	pub const fn for_series(series: usize) -> Self {
		Self::ALL[series % Self::ALL.len()]
	}
}

/// One grid cell's value, in exactly one [`ValueFormat`]. This mirrors
/// [`FieldValue`] (the parsed form) without depending on the parser's
/// type: the grid owns its values, the parser checks them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GridValue {
	/// A floating-point value, encoded as a bare number.
	Float(f64),
	/// A signed-integer value, encoded with the `i` suffix.
	Integer(i64),
	/// An unsigned-integer value, encoded with the `u` suffix.
	Unsigned(u64),
	/// A boolean value, encoded as `t` or `f`.
	Boolean(bool),
	/// A string value, encoded double-quoted with `"` and `\` escaped.
	Text(String),
}

impl GridValue {
	/// This value's format.
	#[must_use]
	pub const fn format(&self) -> ValueFormat {
		match self {
			Self::Float(_) => ValueFormat::Float,
			Self::Integer(_) => ValueFormat::Integer,
			Self::Unsigned(_) => ValueFormat::Unsigned,
			Self::Boolean(_) => ValueFormat::Boolean,
			Self::Text(_) => ValueFormat::Text,
		}
	}

	/// Encode this value in its format's exact line-protocol field form.
	///
	/// Floats use Rust's shortest round-trip rendering; integers carry
	/// their suffix; booleans are `t`/`f`; strings are quoted with the
	/// line protocol's escapes. Encoding then parsing returns the same
	/// value (see the round-trip tests).
	#[must_use]
	pub fn encode_field(&self) -> String {
		match self {
			Self::Float(value) => format!("{value}"),
			Self::Integer(value) => format!("{value}i"),
			Self::Unsigned(value) => format!("{value}u"),
			Self::Boolean(true) => "t".to_string(),
			Self::Boolean(false) => "f".to_string(),
			Self::Text(value) => format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"")),
		}
	}

	/// This value as the parsed [`FieldValue`] it must decode to.
	#[must_use]
	pub fn to_field_value(&self) -> FieldValue {
		match self {
			Self::Float(value) => FieldValue::Float(*value),
			Self::Integer(value) => FieldValue::Integer(*value),
			Self::Unsigned(value) => FieldValue::Unsigned(*value),
			Self::Boolean(value) => FieldValue::Boolean(*value),
			Self::Text(value) => FieldValue::Str(value.clone()),
		}
	}
}

/// One operation lane: the kind of work a soak tick performs. A tick
/// belongs to exactly one lane, so a run's lane mix is countable from its
/// steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadLane {
	/// Write new points (the ingest family).
	Ingest,
	/// Read one point (the point-lookup family).
	PointLookup,
	/// Read a range of points (the range-fetch family).
	RangeScan,
}

impl WorkloadLane {
	/// The lane's tag value in workload bodies.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Ingest => "ingest",
			Self::PointLookup => "point_lookup",
			Self::RangeScan => "range_scan",
		}
	}
}

/// The lane mix: relative weights a tick's lane is drawn against. A weight
/// of zero removes its lane from the mix; all three at zero is rejected
/// at model construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneWeights {
	/// Relative weight of the ingest lane.
	pub ingest: u32,
	/// Relative weight of the point-lookup lane.
	pub point_lookup: u32,
	/// Relative weight of the range-scan lane.
	pub range_scan: u32,
}

impl LaneWeights {
	/// Total weight across all lanes.
	#[must_use]
	pub const fn total(self) -> u64 {
		self.ingest as u64 + self.point_lookup as u64 + self.range_scan as u64
	}
}

impl Default for LaneWeights {
	/// The default mix: ingest-heavy, as a soak's traffic is — six writes
	/// for every two point reads and two range scans.
	fn default() -> Self {
		Self { ingest: 60, point_lookup: 20, range_scan: 20 }
	}
}

/// The tunable knobs of a workload model (everything except the seed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadParams {
	/// Series in the grid, one column each.
	pub series: usize,
	/// Points per step, one row each.
	pub points_per_step: usize,
	/// Share of grid cells that hold values, in percent (`1..=100`).
	pub density_percent: u32,
	/// The lane mix.
	pub lane_weights: LaneWeights,
}

impl Default for WorkloadParams {
	/// A small default grid: 8 series, 16 points per step, four cells in
	/// five present, the default lane mix.
	fn default() -> Self {
		Self { series: 8, points_per_step: 16, density_percent: 80, lane_weights: LaneWeights::default() }
	}
}

impl WorkloadParams {
	/// Build params, clamping the knobs the way the sparse profile does:
	/// at least one series and one point per step, and a density between
	/// 1% and 100% — an entirely empty grid has nothing to encode. Lane
	/// weights are not clamped: a zero total is a construction error on
	/// the model, not a value to silently fix.
	#[must_use]
	pub fn new(series: usize, points_per_step: usize, density_percent: u32, lane_weights: LaneWeights) -> Self {
		Self { series: series.max(1), points_per_step: points_per_step.max(1), density_percent: density_percent.clamp(1, 100), lane_weights }
	}
}

/// A (series × row) grid of typed, possibly absent values: one column per
/// series, each column in exactly one [`ValueFormat`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValueGrid {
	/// Series in the grid (columns).
	pub series: usize,
	/// Rows per series.
	pub rows: usize,
	/// Cells in series-major order: `cells[series][row]`, `None` where
	/// the cell is absent.
	pub cells: Vec<Vec<Option<GridValue>>>,
}

impl ValueGrid {
	/// The cell at (`series`, `row`), or `None` when out of range or absent.
	#[must_use]
	pub fn cell(&self, series: usize, row: usize) -> Option<&GridValue> {
		self.cells.get(series).and_then(|column| column.get(row)).and_then(Option::as_ref)
	}

	/// Number of present cells in the grid.
	#[must_use]
	pub fn present_count(&self) -> usize {
		self.cells.iter().flatten().filter(|cell| cell.is_some()).count()
	}
}

/// One tick's work: its lane, its grid, and the line-protocol body that
/// encodes the grid's present cells.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkloadStep {
	/// The scheduler tick this step belongs to.
	pub tick: u64,
	/// The lane this tick performs.
	pub lane: WorkloadLane,
	/// The grid this step's body encodes.
	pub grid: ValueGrid,
	/// Line-protocol body: one line per present cell, in series-major
	/// order (the order the grid was walked in).
	pub body: String,
}

impl WorkloadStep {
	/// Number of lines in the body: one per present cell.
	#[must_use]
	pub fn line_count(&self) -> usize {
		self.grid.present_count()
	}
}

/// The deterministic workload model: a seed plus params. Every method is
/// a pure function of its inputs, so the same seed and params build the
/// same workload on any machine, in any order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadModel {
	/// Seed everything derives from: a [`SoakConfig`]'s seed, by default.
	pub seed: u64,
	/// The model's knobs.
	pub params: WorkloadParams,
}

/// A value rounded to two decimals, as the sparse generator rounds: the
/// exact value does not matter to the workload, only that the generator
/// and any checker agree on it bit for bit.
fn round2(value: f64) -> f64 {
	(value * 100.0).round() / 100.0
}

/// The deterministic signal a cell's value derives from: a per-series
/// amplitude and offset over a slow sinusoid, as in the sparse workload,
/// so a wrong series or row offset shows up in the values.
fn signal(series: usize, row: usize) -> f64 {
	use std::f64::consts::TAU;
	#[allow(clippy::cast_precision_loss)]
	let (series_f, row_f) = (series as f64, row as f64);
	let amplitude = 10.0_f64.mul_add((series_f * 0.7).sin(), 30.0);
	round2(amplitude.mul_add((row_f / 512.0 * TAU).sin(), series_f))
}

/// A per-tick random stream, seeded from the model's seed, the tick, and
/// a salt separating the streams: the lane stream and the grid stream
/// draw independently, so asking for a step's lane never shifts its grid.
fn rng_for(seed: u64, tick: u64, salt: u64) -> ChaCha8Rng {
	ChaCha8Rng::seed_from_u64(seed.wrapping_add(salt).wrapping_add(tick.wrapping_mul(0x9E37_79B9_7F4A_7C15)))
}

impl WorkloadModel {
	/// Build a model from a soak config: the config's seed, default
	/// params. This is the seam SOA-1 defined — the workload derives from
	/// the config's seed alone.
	#[must_use]
	pub fn from_config(config: &SoakConfig) -> Self {
		Self { seed: config.seed, params: WorkloadParams::default() }
	}

	/// Build a model from a seed and params.
	///
	/// # Errors
	///
	/// Returns an error when the lane weights sum to zero (a model with
	/// no lane to run).
	pub fn new(seed: u64, params: WorkloadParams) -> anyhow::Result<Self> {
		anyhow::ensure!(params.lane_weights.total() > 0, "workload lane weights must sum to more than zero");
		Ok(Self { seed, params })
	}

	/// Replace this model's params, keeping its seed.
	///
	/// # Errors
	///
	/// Returns an error when the new lane weights sum to zero.
	pub fn with_params(&self, params: WorkloadParams) -> anyhow::Result<Self> {
		Self::new(self.seed, params)
	}

	/// The lane tick `tick` performs, drawn from the tick's lane stream
	/// against the cumulative weights.
	#[must_use]
	pub fn lane_for(&self, tick: u64) -> WorkloadLane {
		let weights = self.params.lane_weights;
		let total = weights.total();
		let mut rng = rng_for(self.seed, tick, 0x1A4E);
		let draw = rng.random_range(0..total);
		if draw < u64::from(weights.ingest) {
			WorkloadLane::Ingest
		} else if draw < u64::from(weights.ingest) + u64::from(weights.point_lookup) {
			WorkloadLane::PointLookup
		} else {
			WorkloadLane::RangeScan
		}
	}

	/// The grid tick `tick` works on, generated from the tick's grid
	/// stream: series-major, each cell present with the model's density,
	/// each present cell's value derived from its series, row and column
	/// format.
	#[must_use]
	pub fn grid_for(&self, tick: u64) -> ValueGrid {
		let params = self.params;
		let mut rng = rng_for(self.seed, tick, 0x6E1D);
		#[allow(clippy::cast_precision_loss)]
		let share = f64::from(params.density_percent) / 100.0;
		let mut cells = Vec::with_capacity(params.series);
		for series in 0..params.series {
			let format = ValueFormat::for_series(series);
			let mut column = Vec::with_capacity(params.points_per_step);
			for row in 0..params.points_per_step {
				if rng.random_range(0.0..1.0) < share {
					let base = signal(series, row);
					column.push(Some(match format {
						ValueFormat::Float => GridValue::Float(base),
						ValueFormat::Integer =>
						{
							#[allow(clippy::cast_possible_truncation)]
							GridValue::Integer(base.trunc() as i64)
						}
						ValueFormat::Unsigned =>
						{
							#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
							GridValue::Unsigned(base.abs().trunc() as u64)
						}
						ValueFormat::Boolean => GridValue::Boolean(base >= 0.0),
						ValueFormat::Text => GridValue::Text(format!("{base:.2}")),
					}));
				} else {
					column.push(None);
				}
			}
			cells.push(column);
		}
		ValueGrid { series: params.series, rows: params.points_per_step, cells }
	}

	/// The timestamp of grid row `row` of tick `tick`, in nanoseconds:
	/// the epoch anchor plus one second per row, advancing one step's
	/// worth of rows per tick, saturating rather than overflowing.
	#[must_use]
	pub fn timestamp_ns(&self, tick: u64, row: usize) -> i64 {
		let index = tick.saturating_mul(self.params.points_per_step as u64).saturating_add(row as u64);
		let offset = i64::try_from(index).unwrap_or(i64::MAX).saturating_mul(ROW_INTERVAL_NS);
		SOAK_WORKLOAD_EPOCH_NS.saturating_add(offset)
	}

	/// Encode a step's body: one line-protocol line per present cell, in
	/// series-major order, `soak,lane=<lane>,series=s<series>
	/// value=<field> <timestamp_ns>`.
	#[must_use]
	pub fn body_for(&self, lane: WorkloadLane, tick: u64, grid: &ValueGrid) -> String {
		use std::fmt::Write as _;
		let mut body = String::new();
		for series in 0..grid.series {
			for row in 0..grid.rows {
				if let Some(value) = grid.cell(series, row) {
					let _ = writeln!(body, "{SOAK_WORKLOAD_MEASUREMENT},lane={},series=s{series} value={} {}", lane.as_str(), value.encode_field(), self.timestamp_ns(tick, row));
				}
			}
		}
		body
	}

	/// The work tick `tick` performs: its lane, its grid, and the body
	/// encoding the grid. Pure: calling it twice for one tick returns
	/// the same step, and it agrees with [`Self::lane_for`] and
	/// [`Self::grid_for`] for that tick.
	#[must_use]
	pub fn step(&self, tick: u64) -> WorkloadStep {
		let lane = self.lane_for(tick);
		let grid = self.grid_for(tick);
		let body = self.body_for(lane, tick, &grid);
		WorkloadStep { tick, lane, grid, body }
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		line_protocol::parse, report::RunMetadata, soak::{SoakConfig, SoakHarness, SoakState, SOAK_SCHEMA_VERSION}, soak_sched::{OpenLoopScheduler, SchedulerConfig, TickDecision}
	};

	fn model() -> WorkloadModel {
		WorkloadModel::new(7, WorkloadParams::default()).expect("default weights sum positive")
	}

	fn metadata() -> RunMetadata {
		RunMetadata::capture("2026-10-25T00:00:00+00:00".to_string())
	}

	#[test]
	fn params_clamp_like_the_sparse_profile() {
		let params = WorkloadParams::new(0, 0, 0, LaneWeights::default());
		assert_eq!(params.series, 1, "zero series clamps to one");
		assert_eq!(params.points_per_step, 1, "zero points clamps to one");
		assert_eq!(params.density_percent, 1, "zero density clamps to one percent");
		let params = WorkloadParams::new(2, 3, 101, LaneWeights::default());
		assert_eq!(params.density_percent, 100, "density above 100 clamps to 100");
	}

	#[test]
	fn zero_lane_weights_are_a_construction_error() {
		let params = WorkloadParams::new(2, 2, 100, LaneWeights { ingest: 0, point_lookup: 0, range_scan: 0 });
		assert!(WorkloadModel::new(7, params).is_err(), "a model with no lane to run must not construct");
	}

	#[test]
	fn same_seed_and_params_build_the_same_workload() {
		let first = model();
		let second = WorkloadModel::new(7, WorkloadParams::default()).expect("same params");
		for tick in 0..32 {
			assert_eq!(first.step(tick), second.step(tick), "tick {tick} must be identical across models");
			assert_eq!(first.step(tick), first.step(tick), "replaying a tick must return the same step");
		}
	}

	#[test]
	fn a_different_seed_builds_a_different_workload() {
		let first = model();
		let second = WorkloadModel::new(8, WorkloadParams::default()).expect("different seed");
		let differs = (0..32).any(|tick| first.step(tick) != second.step(tick));
		assert!(differs, "a different seed must change at least one step in 32 ticks");
	}

	#[test]
	fn step_agrees_with_lane_for_and_grid_for() {
		let model = model();
		for tick in 0..16 {
			let step = model.step(tick);
			assert_eq!(step.lane, model.lane_for(tick), "step lane must equal lane_for for tick {tick}");
			assert_eq!(step.grid, model.grid_for(tick), "step grid must equal grid_for for tick {tick}");
			assert_eq!(step.line_count(), step.grid.present_count(), "one body line per present cell");
		}
	}

	#[test]
	fn columns_cycle_through_every_format() {
		let grid = model().grid_for(0);
		for series in 0..grid.series {
			let format = ValueFormat::for_series(series);
			for row in 0..grid.rows {
				if let Some(value) = grid.cell(series, row) {
					assert_eq!(value.format(), format, "series {series} must stay in its column format");
				}
			}
		}
		assert_eq!(ValueFormat::for_series(0), ValueFormat::Float);
		assert_eq!(ValueFormat::for_series(1), ValueFormat::Integer);
		assert_eq!(ValueFormat::for_series(2), ValueFormat::Unsigned);
		assert_eq!(ValueFormat::for_series(3), ValueFormat::Boolean);
		assert_eq!(ValueFormat::for_series(4), ValueFormat::Text);
		assert_eq!(ValueFormat::for_series(5), ValueFormat::Float, "the cycle repeats after five columns");
	}

	#[test]
	fn field_encodings_are_format_exact() {
		assert_eq!(GridValue::Float(1.25).encode_field(), "1.25");
		assert_eq!(GridValue::Integer(-42).encode_field(), "-42i");
		assert_eq!(GridValue::Unsigned(42).encode_field(), "42u");
		assert_eq!(GridValue::Boolean(true).encode_field(), "t");
		assert_eq!(GridValue::Boolean(false).encode_field(), "f");
		assert_eq!(GridValue::Text("plain".to_string()).encode_field(), "\"plain\"");
		assert_eq!(GridValue::Text("a\"b\\c".to_string()).encode_field(), "\"a\\\"b\\\\c\"", "string escapes must be the line protocol's");
	}

	#[test]
	fn every_encoded_field_parses_back_to_its_value() {
		let values = vec![GridValue::Float(1.25), GridValue::Float(-0.5), GridValue::Integer(-42), GridValue::Integer(0), GridValue::Unsigned(42), GridValue::Boolean(true), GridValue::Boolean(false), GridValue::Text("12.34".to_string()), GridValue::Text("a\"b\\c".to_string())];
		for value in values {
			let line = format!("{SOAK_WORKLOAD_MEASUREMENT} value={} 0", value.encode_field());
			let records = parse(&line).expect("encoded field must parse");
			assert_eq!(records.len(), 1);
			assert_eq!(records[0].field("value"), Some(&value.to_field_value()), "encoded {value:?} must parse back to itself");
		}
	}

	#[test]
	fn step_bodies_parse_back_to_their_grids() {
		let model = model();
		for tick in 0..8 {
			let step = model.step(tick);
			let records = parse(&step.body).expect("step body must parse");
			assert_eq!(records.len(), step.grid.present_count(), "one record per present cell for tick {tick}");
			let mut index = 0;
			for series in 0..step.grid.series {
				for row in 0..step.grid.rows {
					if let Some(value) = step.grid.cell(series, row) {
						let record = &records[index];
						index += 1;
						assert_eq!(record.measurement, SOAK_WORKLOAD_MEASUREMENT);
						assert_eq!(record.tag("lane"), Some(step.lane.as_str()));
						assert_eq!(record.tag("series"), Some(format!("s{series}").as_str()), "series tag must name its column");
						assert_eq!(record.field("value"), Some(&value.to_field_value()), "record field must be the grid cell's value");
						assert_eq!(record.timestamp, Some(model.timestamp_ns(tick, row)), "record timestamp must be the row's timestamp");
					}
				}
			}
		}
	}

	#[test]
	fn single_lane_weights_run_only_that_lane() {
		let ingest_only = WorkloadModel::new(7, WorkloadParams::new(2, 2, 100, LaneWeights { ingest: 1, point_lookup: 0, range_scan: 0 })).expect("single lane");
		for tick in 0..50 {
			assert_eq!(ingest_only.lane_for(tick), WorkloadLane::Ingest, "a zero-weighted lane must never run");
		}
	}

	#[test]
	fn default_mix_runs_every_lane() {
		let model = model();
		let lanes: Vec<WorkloadLane> = (0..500).map(|tick| model.lane_for(tick)).collect();
		assert!(lanes.contains(&WorkloadLane::Ingest), "default mix must run ingest");
		assert!(lanes.contains(&WorkloadLane::PointLookup), "default mix must run point lookups");
		assert!(lanes.contains(&WorkloadLane::RangeScan), "default mix must run range scans");
	}

	#[test]
	fn from_config_takes_the_config_seed_and_nothing_else() {
		let config = SoakConfig::new("smoke".to_string(), String::new(), 7, 60).expect("valid config");
		let from_config = WorkloadModel::from_config(&config);
		assert_eq!(from_config.seed, 7);
		assert_eq!(from_config, model(), "from_config with seed 7 must equal the seed-7 model");
		let other_config = SoakConfig::new("other".to_string(), "different label".to_string(), 7, 3600).expect("valid config");
		assert_eq!(WorkloadModel::from_config(&other_config), from_config, "name, label and duration must not change the workload: only the seed does");
	}

	#[test]
	fn model_and_step_round_trip_through_json() {
		let model = model();
		let json = serde_json::to_string(&model).expect("serialize model");
		let back: WorkloadModel = serde_json::from_str(&json).expect("deserialize model");
		assert_eq!(model, back);
		let step = model.step(3);
		let json = serde_json::to_string(&step).expect("serialize step");
		let back: WorkloadStep = serde_json::from_str(&json).expect("deserialize step");
		assert_eq!(step, back);
	}

	#[test]
	fn timestamps_advance_one_second_per_row_from_the_epoch() {
		let model = model();
		assert_eq!(model.timestamp_ns(0, 0), SOAK_WORKLOAD_EPOCH_NS);
		assert_eq!(model.timestamp_ns(0, 1), SOAK_WORKLOAD_EPOCH_NS + ROW_INTERVAL_NS);
		let points = i64::try_from(model.params.points_per_step).expect("points per step fits in i64");
		assert_eq!(model.timestamp_ns(1, 0), SOAK_WORKLOAD_EPOCH_NS + points * ROW_INTERVAL_NS, "tick 1 starts one step's rows after tick 0");
	}

	#[test]
	fn workload_drives_a_scheduled_run_through_the_scheduler_seam() {
		let config = SoakConfig::new("smoke".to_string(), String::new(), 7, 1).expect("valid config");
		let mut harness = SoakHarness::new(config, metadata());
		let preflight = harness.preflight(&std::env::temp_dir()).expect("preflight from Created");
		assert!(preflight.passed(), "preflight on the temp dir must pass: {:?}", preflight.checks);
		let model = model();
		let scheduler = OpenLoopScheduler::new(SchedulerConfig::new(100, 1).expect("valid config"));
		let mut bodies = Vec::new();
		let report = scheduler
			.run_gated(
				&mut harness,
				&preflight,
				|tick| {
					let step = model.step(tick);
					bodies.push(step.body);
					(std::time::Duration::from_nanos(100), Ok(()))
				},
				|_, _| TickDecision::Execute,
			)
			.expect("gated workload run");
		assert_eq!(report.final_state, SoakState::Finished);
		assert_eq!(report.steps, 10, "1 s at 100 ms runs 10 workload ticks");
		assert_eq!(bodies.len(), 10);
		for (tick, body) in bodies.iter().enumerate() {
			let expected = model.step(tick as u64);
			assert_eq!(body, &expected.body, "tick {tick}'s submitted body must be the model's step");
			assert!(!parse(body).expect("submitted body must parse").is_empty(), "a submitted body must carry records");
		}
		assert_eq!(harness.run_record().soak_schema_version, SOAK_SCHEMA_VERSION, "the workload adds no run-record field, so the soak schema stays at v1");
	}
}
