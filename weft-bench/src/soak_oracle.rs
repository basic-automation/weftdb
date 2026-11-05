//! Soak oracle core (1.0 plan SOA-6, W4).
//!
//! A soak run is only worth publishing if what the server kept is what the
//! run sent. SOA-4's workload model made that checkable: a
//! [`WorkloadModel`] is a pure function of a soak config's seed, so the
//! traffic of any tick can be regenerated exactly, on any machine, without
//! replaying its predecessors. SOA-5's ingest client put that traffic on
//! the wire and deliberately stopped there — it owns no reads, because a
//! read result is only meaningful once something can judge it. This module
//! is that judge: the oracle core. SOA-7 (W5) builds on this core: the
//! window digests in [`crate::soak_digest`] hash the same expected and
//! observed points this module compares, and the oracle self-test
//! ([`SoakOracle::self_test`]) exercises this core against known-good
//! and known-corrupt read-backs so a broken judge is caught before a
//! run trusts its verdict.
//!
//! **What the oracle judges.** The caller supplies what a read-back
//! observed — one [`ObservedPoint`] per point it found, each naming its
//! tick's grid cell by (`series`, `row`) with the timestamp and value it
//! read — and the oracle regenerates the expected grid for that tick from
//! the model and compares, cell by cell. How the read-back is performed
//! (which endpoint, which client) is the caller's business, the same way
//! SOA-5's client is one filler of SOA-3's server seam; the derived-read
//! checks built on read endpoints are SOA-10's, and window digests are
//! SOA-7's ([`crate::soak_digest`]). The core here is the comparison and
//! the verdict, nothing else.
//!
//! **Exact comparison, no invented tolerance.** Grids are format-exact by
//! construction (SOA-4): a cell's value is derived deterministically and
//! encoded in its format's exact wire form, and SOA-5 records that every
//! numeric workload value is exact in the `f64` the server seals. So the
//! oracle compares for equality — value, timestamp, presence — and any
//! difference is a [`MismatchKind`], classified:
//!
//! * [`MismatchKind::Missing`] — a cell the model wrote has no observation;
//! * [`MismatchKind::Unexpected`] — an observation names a cell the model
//!   left absent, or a cell outside the grid entirely;
//! * [`MismatchKind::WrongValue`] — the cell was read back at the right
//!   timestamp with a different value;
//! * [`MismatchKind::WrongTimestamp`] — the cell was read back, but not at
//!   the timestamp the model wrote it at (the value is not compared
//!   further: a point at the wrong instant is a different point);
//! * [`MismatchKind::Duplicate`] — a cell was observed more than once in
//!   one tick. This is the failure SOA-5 refused to risk hiding: a
//!   duplicated seal (a retried POST, a replayed body) keeps the first
//!   observation's value and adds points, so it surfaces here rather than
//!   as a silent double-count.
//!
//! **The verdict is a record, not a threshold.** An [`OracleVerdict`]
//! counts what was judged (ticks, cells checked, cells the model filled,
//! points observed) and lists the mismatches, up to
//! [`MAX_REPORTED_MISMATCHES`]; the total mismatch count is kept even when
//! the list is capped, so a capped verdict still says how bad the run was.
//! [`OracleVerdict::passed`] is exact — zero mismatches — not a rate or a
//! tolerance: consistent with SOA-2's and SOA-3's refusal to invent
//! pass/fail numbers, the oracle states what differed and lets the
//! zero-difference rule, which the format-exact workload makes attainable,
//! decide. Latency is never judged here; that is SOA-13's SLO evaluator.
//!
//! **Recorded in the run record.** A verdict is recorded on the run it
//! judged through [`SoakHarness::record_oracle_verdict`](crate::soak::SoakHarness::record_oracle_verdict), as the
//! `oracle` field of [`SoakRun`](crate::soak::SoakRun) — the verdict is part of the run's
//! metadata, beside EVI-3's [`RunMetadata`](crate::report::RunMetadata)
//! the record already carries. Adding the field is a soak-record change,
//! so [`SOAK_SCHEMA_VERSION`](crate::soak::SOAK_SCHEMA_VERSION) moves to v2 on SOA's own version line (the
//! plan's §3 rule: EVI-3 owns the shared metadata, SOA keeps its own
//! `soak_schema_version`); the field is `#[serde(default)]` and skipped
//! when absent, so v1 artifacts still deserialize, with no verdict. The
//! verdict carries no free text — no label, no path, no server message —
//! so it passes through [`SoakRun::redacted_json`](crate::soak::SoakRun::redacted_json) unchanged and needs no
//! redaction pass of its own.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::soak_workload::{GridValue, WorkloadModel, WorkloadStep};

/// Cap on the mismatches an [`OracleVerdict`] lists.
///
/// The total is always
/// counted in [`OracleVerdict::mismatch_count`]; the cap only bounds the
/// artifact a badly broken run produces — a 24 h run whose store lost a
/// segment could otherwise mint a mismatch per point for hours.
pub const MAX_REPORTED_MISMATCHES: usize = 1024;

/// One point a read-back observed, addressed to the grid cell of one tick
/// it belongs to.
///
/// The caller builds these from whatever read path it used; the oracle
/// never performs a read itself. `series` and `row` are the cell's grid
/// coordinates (the workload body names them as the `series=s<series>` tag
/// and the row's position in the step), `timestamp_ns` is the timestamp
/// the point was read back at, and `value` is the value read back, in the
/// grid's own [`GridValue`] form so the comparison is format-exact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedPoint {
	/// Grid column (series) this point was read from.
	pub series: usize,
	/// Grid row this point was read from.
	pub row: usize,
	/// Timestamp the point carries, in nanoseconds.
	pub timestamp_ns: i64,
	/// Value read back.
	pub value: GridValue,
}

/// How one observed cell differs from the expected grid. See the module
/// doc for the classification rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MismatchKind {
	/// A cell the model filled has no observation.
	Missing,
	/// An observation names a cell the model left absent, or outside the
	/// grid.
	Unexpected,
	/// The cell was read back at the right timestamp with a different
	/// value.
	WrongValue,
	/// The cell was read back at a different timestamp than the model
	/// wrote it at.
	WrongTimestamp,
	/// The cell was observed more than once in one tick.
	Duplicate,
}

/// One cell where a read-back disagreed with the expected grid.
///
/// Carries both sides of the comparison — the expected and observed
/// values and timestamps — so a reviewer can see the difference without
/// re-running anything. `observed` is `None` exactly for
/// [`MismatchKind::Missing`], and `observed_timestamp_ns` is `None` with
/// it; `expected` is `None` exactly for an out-of-grid
/// [`MismatchKind::Unexpected`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OracleMismatch {
	/// Tick the cell belongs to.
	pub tick: u64,
	/// Grid column (series) of the cell.
	pub series: usize,
	/// Grid row of the cell.
	pub row: usize,
	/// How the cell differs.
	pub kind: MismatchKind,
	/// Value the model wrote, or `None` when the cell is outside the
	/// grid.
	pub expected: Option<GridValue>,
	/// Value read back, or `None` when the cell was never observed.
	pub observed: Option<GridValue>,
	/// Timestamp the model wrote the cell at.
	pub expected_timestamp_ns: i64,
	/// Timestamp the point was read back at, or `None` when it was never
	/// observed.
	pub observed_timestamp_ns: Option<i64>,
}

/// The oracle's verdict on the ticks it judged: counts of what was
/// checked and the mismatches found.
///
/// Recorded on the run record via
/// [`SoakHarness::record_oracle_verdict`](crate::soak::SoakHarness::record_oracle_verdict);
/// also serializes on its own for artifacts that want it beside the
/// record. Carries no free text.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OracleVerdict {
	/// Ticks judged.
	pub ticks_judged: u64,
	/// Grid cells examined: every cell of every judged tick, present or
	/// absent.
	pub cells_checked: u64,
	/// Cells the model filled across the judged ticks (the points a
	/// faithful read-back must return).
	pub cells_present: u64,
	/// Observations supplied across the judged ticks, duplicates and
	/// unexpected points included.
	pub points_observed: u64,
	/// Total mismatches found, whether listed in [`Self::mismatches`] or
	/// cut off by the cap.
	pub mismatch_count: u64,
	/// The mismatches, in judgement order (per tick: observations in the
	/// order supplied, then missing cells in series-major grid order),
	/// capped at [`MAX_REPORTED_MISMATCHES`].
	pub mismatches: Vec<OracleMismatch>,
}

impl OracleVerdict {
	/// Whether the judged ticks matched the model exactly: no mismatch of
	/// any kind. A verdict that judged nothing has no mismatches and so
	/// passes vacuously — callers publishing a run check
	/// [`Self::ticks_judged`] beside this, the way a skipped flush check
	/// is not a passed one (SOA-12).
	#[must_use]
	pub const fn passed(&self) -> bool {
		self.mismatch_count == 0
	}

	/// Whether the mismatch list was capped: the total in
	/// [`Self::mismatch_count`] is larger than the list kept.
	#[must_use]
	pub const fn truncated(&self) -> bool {
		let listed = self.mismatches.len() as u64;
		self.mismatch_count > listed
	}

	/// Mismatches of one kind, as listed (subject to the same cap as
	/// [`Self::mismatches`]).
	#[must_use]
	pub fn count_kind(&self, kind: MismatchKind) -> usize {
		self.mismatches.iter().filter(|mismatch| mismatch.kind == kind).count()
	}

	/// Fold another verdict into this one: counts add, and the mismatch
	/// list extends up to the cap (the totals still add in full). This is
	/// how per-tick verdicts combine into a run verdict.
	pub fn merge(&mut self, other: &Self) {
		self.ticks_judged = self.ticks_judged.saturating_add(other.ticks_judged);
		self.cells_checked = self.cells_checked.saturating_add(other.cells_checked);
		self.cells_present = self.cells_present.saturating_add(other.cells_present);
		self.points_observed = self.points_observed.saturating_add(other.points_observed);
		let total = self.mismatch_count.saturating_add(other.mismatch_count);
		for mismatch in &other.mismatches {
			if self.mismatches.len() < MAX_REPORTED_MISMATCHES {
				self.mismatches.push(mismatch.clone());
			}
		}
		self.mismatch_count = total;
	}

	/// Record one mismatch: always counted, listed only under the cap.
	fn push_capped(&mut self, mismatch: OracleMismatch) {
		self.mismatch_count = self.mismatch_count.saturating_add(1);
		if self.mismatches.len() < MAX_REPORTED_MISMATCHES {
			self.mismatches.push(mismatch);
		}
	}

	/// Serialize this verdict as a pretty JSON artifact.
	///
	/// # Errors
	///
	/// Returns an error when serialization fails.
	pub fn to_json(&self) -> anyhow::Result<String> {
		Ok(serde_json::to_string_pretty(self)?)
	}
}

/// One check in the oracle self-test (SOA-7): a fixed name from the
/// closed set [`SoakOracle::self_test`] runs, and whether the core
/// produced the judgement the check's construction requires.
///
/// The names
/// are constants of the self-test, never free text from a caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfTestCheck {
	/// Stable name of the check (e.g. `missing_point_is_caught`).
	pub name: String,
	/// Whether the core judged this check's read-back correctly.
	pub passed: bool,
}

/// The oracle self-test's report (SOA-7): every check it ran and
/// whether each passed.
///
/// Produced by [`SoakOracle::self_test`]; carries
/// no free text beyond the checks' fixed names, so it needs no
/// redaction pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OracleSelfTestReport {
	/// The checks, in the order the self-test runs them.
	pub checks: Vec<SelfTestCheck>,
}

impl OracleSelfTestReport {
	/// Whether every check passed — and at least one ran. A report with
	/// no checks is not a passed self-test (the SOA-12 skipped-check
	/// rule, as with the oracle's own vacuous pass).
	#[must_use]
	pub fn passed(&self) -> bool {
		!self.checks.is_empty() && self.checks.iter().all(|check| check.passed)
	}

	/// How many checks the self-test ran.
	#[must_use]
	pub const fn checks_run(&self) -> usize {
		self.checks.len()
	}

	/// Names of the checks that failed, in run order.
	#[must_use]
	pub fn failed_checks(&self) -> Vec<&str> {
		self.checks.iter().filter(|check| !check.passed).map(|check| check.name.as_str()).collect()
	}

	/// Serialize this report as a pretty JSON artifact.
	///
	/// # Errors
	///
	/// Returns an error when serialization fails.
	pub fn to_json(&self) -> anyhow::Result<String> {
		Ok(serde_json::to_string_pretty(self)?)
	}
}

/// The soak oracle: the workload model a run's traffic derived from, and
/// the comparisons against it.
///
/// The oracle holds no server, store, or socket — it is the pure core
/// SOA-7's self-test exercises and SOA-10's derived-read checks build on.
/// Judging is a pure function of the model and the observations supplied,
/// so the same observations judged twice give the same verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoakOracle {
	model: WorkloadModel,
}

impl SoakOracle {
	/// Build an oracle over `model`: the model the run under judgement
	/// derived its workload from (same seed, same params — a different
	/// model judges against a different run's traffic and finds it all
	/// wrong, which is the correct answer).
	#[must_use]
	pub const fn new(model: WorkloadModel) -> Self {
		Self { model }
	}

	/// Build an oracle from a soak config, via
	/// [`WorkloadModel::from_config`]: the config's seed alone decides
	/// the expected traffic.
	#[must_use]
	pub fn from_config(config: &crate::soak::SoakConfig) -> Self {
		Self { model: WorkloadModel::from_config(config) }
	}

	/// The model this oracle judges against.
	#[must_use]
	pub const fn model(&self) -> &WorkloadModel {
		&self.model
	}

	/// The step tick `tick` was expected to perform: its lane, its grid,
	/// and the body encoding the grid. Pure, like the model's.
	#[must_use]
	pub fn expected_step(&self, tick: u64) -> WorkloadStep {
		self.model.step(tick)
	}

	/// Judge one tick: compare `observed` (everything a read-back found
	/// for this tick) against the tick's expected grid, exactly.
	#[must_use]
	pub fn judge_tick(&self, tick: u64, observed: &[ObservedPoint]) -> OracleVerdict {
		let mut verdict = OracleVerdict::default();
		self.judge_tick_into(tick, observed, &mut verdict);
		verdict
	}

	/// Judge a run's ticks: each entry is a tick and everything a
	/// read-back found for it. Ticks are judged in the order given; a
	/// tick listed twice is judged twice (its counts and any mismatches
	/// land twice), so callers pass each tick once.
	#[must_use]
	pub fn judge_run(&self, ticks: &[(u64, Vec<ObservedPoint>)]) -> OracleVerdict {
		let mut verdict = OracleVerdict::default();
		for (tick, observed) in ticks {
			self.judge_tick_into(*tick, observed, &mut verdict);
		}
		verdict
	}

	/// The expected window digest of ticks
	/// `start_tick..start_tick + tick_count` (SOA-7): the hash form of
	/// this oracle's expectations for the window, from the model alone.
	/// Compared against
	/// [`observed_window_digest`](crate::soak_digest::observed_window_digest)
	/// of a read-back via [`WindowDigest::matches`](crate::soak_digest::WindowDigest::matches);
	/// a window whose digests differ is re-judged tick by tick with
	/// [`Self::judge_tick`] to classify what changed.
	#[must_use]
	pub fn window_digest(&self, start_tick: u64, tick_count: u64) -> crate::soak_digest::WindowDigest {
		crate::soak_digest::expected_window_digest(&self.model, start_tick, tick_count)
	}

	/// The oracle self-test (SOA-7): exercise this core against
	/// read-backs whose correct judgements are known by construction,
	/// and report whether the core produced them.
	///
	/// A soak verdict is evidence only if the judge works. The self-test
	/// builds its own small deterministic model — it does not use this
	/// oracle's model, so a misconfigured run cannot make it vacuous —
	/// and checks: a faithful read-back passes exactly; each corruption
	/// class the core classifies (missing, wrong value, wrong timestamp,
	/// unexpected, out-of-grid, duplicate) is caught as exactly that
	/// kind; and SOA-7's window digests agree with a faithful read-back
	/// and disagree after a dropped point. It performs no read and
	/// touches no server, so a driver can run it before a soak starts
	/// and a publisher can run it before trusting a recorded verdict.
	/// Pure and deterministic: the same core gives the same report on
	/// any machine.
	///
	/// # Panics
	///
	/// If the fixed self-test workload parameters are refused, which they never are.
	#[must_use]
	pub fn self_test() -> OracleSelfTestReport {
		use crate::soak_workload::{LaneWeights, WorkloadParams};

		let model = WorkloadModel::new(7, WorkloadParams::new(3, 4, 50, LaneWeights { ingest: 1, point_lookup: 0, range_scan: 0 })).expect("self-test model params are valid");
		let oracle = Self::new(model.clone());
		let faithful_tick = |tick: u64| -> Vec<ObservedPoint> {
			let step = model.step(tick);
			let mut points = Vec::new();
			for series in 0..step.grid.series {
				for row in 0..step.grid.rows {
					if let Some(value) = step.grid.cell(series, row) {
						points.push(ObservedPoint { series, row, timestamp_ns: model.timestamp_ns(tick, row), value: value.clone() });
					}
				}
			}
			points
		};
		// A tick with at least one present and one absent cell, so every
		// corruption has somewhere to land.
		let mixed_tick = (0..64)
			.find(|&tick| {
				let step = model.step(tick);
				step.grid.present_count() > 0 && step.grid.present_count() < step.grid.series * step.grid.rows
			})
			.expect("a 50% grid has a mixed tick within 64 ticks");
		let absent_cell = {
			let step = model.step(mixed_tick);
			(0..step.grid.series).find_map(|series| (0..step.grid.rows).find(|&row| step.grid.cell(series, row).is_none()).map(|row| (series, row))).expect("the mixed tick has an absent cell")
		};
		let single_kind = |verdict: &OracleVerdict, kind: MismatchKind| verdict.mismatch_count == 1 && verdict.count_kind(kind) == 1;

		let mut checks: Vec<SelfTestCheck> = Vec::new();
		let mut check = |name: &str, passed: bool| checks.push(SelfTestCheck { name: name.to_string(), passed });

		let faithful: Vec<(u64, Vec<ObservedPoint>)> = (0..8).map(|tick| (tick, faithful_tick(tick))).collect();
		let verdict = oracle.judge_run(&faithful);
		check("faithful_read_back_passes", verdict.passed() && verdict.ticks_judged == 8 && verdict.points_observed > 0);

		let mut dropped = faithful_tick(mixed_tick);
		dropped.remove(0);
		check("missing_point_is_caught", single_kind(&oracle.judge_tick(mixed_tick, &dropped), MismatchKind::Missing));

		let mut wrong_value = faithful_tick(mixed_tick);
		wrong_value[0].value = GridValue::Text("self-test tamper".to_string());
		check("wrong_value_is_caught", single_kind(&oracle.judge_tick(mixed_tick, &wrong_value), MismatchKind::WrongValue));

		let mut wrong_timestamp = faithful_tick(mixed_tick);
		wrong_timestamp[0].timestamp_ns += 1;
		check("wrong_timestamp_is_caught", single_kind(&oracle.judge_tick(mixed_tick, &wrong_timestamp), MismatchKind::WrongTimestamp));

		let mut unexpected = faithful_tick(mixed_tick);
		unexpected.push(ObservedPoint { series: absent_cell.0, row: absent_cell.1, timestamp_ns: model.timestamp_ns(mixed_tick, absent_cell.1), value: GridValue::Float(0.0) });
		check("unexpected_point_is_caught", single_kind(&oracle.judge_tick(mixed_tick, &unexpected), MismatchKind::Unexpected));

		let mut out_of_grid = faithful_tick(mixed_tick);
		out_of_grid.push(ObservedPoint { series: 99, row: 0, timestamp_ns: model.timestamp_ns(mixed_tick, 0), value: GridValue::Float(1.0) });
		check("out_of_grid_point_is_caught", single_kind(&oracle.judge_tick(mixed_tick, &out_of_grid), MismatchKind::Unexpected));

		let mut duplicated = faithful_tick(mixed_tick);
		duplicated.push(duplicated[0].clone());
		check("duplicate_point_is_caught", single_kind(&oracle.judge_tick(mixed_tick, &duplicated), MismatchKind::Duplicate));

		let window: Vec<(u64, Vec<ObservedPoint>)> = (0..4).map(|tick| (tick, faithful_tick(tick))).collect();
		check("window_digest_agrees_with_a_faithful_read_back", oracle.window_digest(0, 4).matches(&crate::soak_digest::observed_window_digest(0, 4, &window)));
		let mut short_window = window;
		short_window[0].1.remove(0);
		check("window_digest_detects_a_dropped_point", !oracle.window_digest(0, 4).matches(&crate::soak_digest::observed_window_digest(0, 4, &short_window)));

		OracleSelfTestReport { checks }
	}

	/// Judge one tick into an accumulating verdict.
	fn judge_tick_into(&self, tick: u64, observed: &[ObservedPoint], verdict: &mut OracleVerdict) {
		let step = self.model.step(tick);
		let grid = &step.grid;
		verdict.ticks_judged = verdict.ticks_judged.saturating_add(1);
		let cells = (grid.series as u64).saturating_mul(grid.rows as u64);
		verdict.cells_checked = verdict.cells_checked.saturating_add(cells);
		let present = grid.present_count() as u64;
		verdict.cells_present = verdict.cells_present.saturating_add(present);
		let supplied = observed.len() as u64;
		verdict.points_observed = verdict.points_observed.saturating_add(supplied);

		let mut seen: BTreeMap<(usize, usize), &ObservedPoint> = BTreeMap::new();
		for point in observed {
			let expected_timestamp_ns = self.model.timestamp_ns(tick, point.row);
			let in_grid = grid.contains(point.series, point.row);
			let expected = if in_grid { grid.cell(point.series, point.row).cloned() } else { None };
			if seen.insert((point.series, point.row), point).is_some() {
				verdict.push_capped(OracleMismatch { tick, series: point.series, row: point.row, kind: MismatchKind::Duplicate, expected, observed: Some(point.value.clone()), expected_timestamp_ns, observed_timestamp_ns: Some(point.timestamp_ns) });
				continue;
			}
			match expected {
				None => {
					verdict.push_capped(OracleMismatch { tick, series: point.series, row: point.row, kind: MismatchKind::Unexpected, expected: None, observed: Some(point.value.clone()), expected_timestamp_ns, observed_timestamp_ns: Some(point.timestamp_ns) });
				}
				Some(expected_value) => {
					if point.timestamp_ns != expected_timestamp_ns {
						verdict.push_capped(OracleMismatch { tick, series: point.series, row: point.row, kind: MismatchKind::WrongTimestamp, expected: Some(expected_value), observed: Some(point.value.clone()), expected_timestamp_ns, observed_timestamp_ns: Some(point.timestamp_ns) });
					} else if point.value != expected_value {
						verdict.push_capped(OracleMismatch { tick, series: point.series, row: point.row, kind: MismatchKind::WrongValue, expected: Some(expected_value), observed: Some(point.value.clone()), expected_timestamp_ns, observed_timestamp_ns: Some(point.timestamp_ns) });
					}
				}
			}
		}
		for series in 0..grid.series {
			for row in 0..grid.rows {
				if let Some(value) = grid.cell(series, row) {
					if !seen.contains_key(&(series, row)) {
						verdict.push_capped(OracleMismatch { tick, series, row, kind: MismatchKind::Missing, expected: Some(value.clone()), observed: None, expected_timestamp_ns: self.model.timestamp_ns(tick, row), observed_timestamp_ns: None });
					}
				}
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		report::RunMetadata, soak::{SoakConfig, SoakHarness, SoakState}, soak_workload::{LaneWeights, WorkloadParams}
	};

	fn model() -> WorkloadModel {
		WorkloadModel::new(7, WorkloadParams::new(3, 4, 100, LaneWeights { ingest: 1, point_lookup: 0, range_scan: 0 })).expect("valid model")
	}

	fn oracle() -> SoakOracle {
		SoakOracle::new(model())
	}

	fn metadata() -> RunMetadata {
		RunMetadata::capture("2026-10-25T00:00:00+00:00".to_string())
	}

	/// The faithful read-back of one tick: one observation per present
	/// cell, at the model's timestamp, with the model's value.
	fn faithful(model: &WorkloadModel, tick: u64) -> Vec<ObservedPoint> {
		let step = model.step(tick);
		let mut points = Vec::new();
		for series in 0..step.grid.series {
			for row in 0..step.grid.rows {
				if let Some(value) = step.grid.cell(series, row) {
					points.push(ObservedPoint { series, row, timestamp_ns: model.timestamp_ns(tick, row), value: value.clone() });
				}
			}
		}
		points
	}

	#[test]
	fn a_faithful_read_back_passes_exactly() {
		let model = model();
		let oracle = SoakOracle::new(model.clone());
		for tick in 0..8 {
			let verdict = oracle.judge_tick(tick, &faithful(&model, tick));
			assert!(verdict.passed(), "tick {tick} read back faithfully must pass: {:?}", verdict.mismatches);
			assert_eq!(verdict.ticks_judged, 1);
			assert_eq!(verdict.cells_checked, 12, "3 series x 4 rows");
			assert_eq!(verdict.cells_present, 12, "density 100 fills every cell");
			assert_eq!(verdict.points_observed, 12);
			assert_eq!(verdict.mismatch_count, 0);
		}
	}

	#[test]
	fn judgement_is_pure_in_the_model_and_the_observations() {
		let oracle = oracle();
		let observed = faithful(&model(), 3);
		assert_eq!(oracle.judge_tick(3, &observed), oracle.judge_tick(3, &observed), "the same observations judged twice give the same verdict");
		assert_eq!(oracle.expected_step(3), model().step(3), "the oracle's expectation is the model's step");
	}

	#[test]
	fn from_config_judges_the_config_seed_workload() {
		let config = SoakConfig::new("smoke".to_string(), String::new(), 7, 60).expect("valid config");
		let oracle = SoakOracle::from_config(&config);
		assert_eq!(oracle.model(), &WorkloadModel::from_config(&config));
		let model = WorkloadModel::from_config(&config);
		assert!(oracle.judge_tick(0, &faithful(&model, 0)).passed(), "the config-seed workload read back faithfully must pass");
	}

	#[test]
	fn a_missing_point_is_a_missing_mismatch() {
		let model = model();
		let oracle = SoakOracle::new(model.clone());
		let mut observed = faithful(&model, 0);
		let dropped = observed.remove(0);
		let verdict = oracle.judge_tick(0, &observed);
		assert!(!verdict.passed());
		assert_eq!(verdict.mismatch_count, 1);
		let mismatch = &verdict.mismatches[0];
		assert_eq!(mismatch.kind, MismatchKind::Missing);
		assert_eq!((mismatch.series, mismatch.row), (dropped.series, dropped.row));
		assert_eq!(mismatch.expected, Some(dropped.value));
		assert_eq!(mismatch.observed, None);
		assert_eq!(mismatch.observed_timestamp_ns, None);
	}

	#[test]
	fn a_wrong_value_is_a_wrong_value_mismatch() {
		let model = model();
		let oracle = SoakOracle::new(model.clone());
		let mut observed = faithful(&model, 0);
		observed[0].value = GridValue::Text("tampered".to_string());
		let verdict = oracle.judge_tick(0, &observed);
		assert_eq!(verdict.mismatch_count, 1);
		let mismatch = &verdict.mismatches[0];
		assert_eq!(mismatch.kind, MismatchKind::WrongValue);
		assert_eq!(mismatch.observed, Some(GridValue::Text("tampered".to_string())));
		assert!(mismatch.expected.is_some(), "the expected side of the comparison is recorded");
	}

	#[test]
	fn a_wrong_timestamp_is_reported_before_the_value_is_compared() {
		let model = model();
		let oracle = SoakOracle::new(model.clone());
		let mut observed = faithful(&model, 0);
		observed[0].timestamp_ns += 1;
		observed[0].value = GridValue::Text("also wrong".to_string());
		let verdict = oracle.judge_tick(0, &observed);
		assert_eq!(verdict.mismatch_count, 1, "one point at the wrong instant is one mismatch, not two");
		assert_eq!(verdict.mismatches[0].kind, MismatchKind::WrongTimestamp);
		assert_eq!(verdict.mismatches[0].observed_timestamp_ns, Some(observed[0].timestamp_ns));
	}

	#[test]
	fn an_absent_cell_observed_is_unexpected() {
		let sparse_model = WorkloadModel::new(7, WorkloadParams::new(3, 4, 50, LaneWeights { ingest: 1, point_lookup: 0, range_scan: 0 })).expect("valid model");
		let oracle = SoakOracle::new(sparse_model.clone());
		// Find a tick with at least one absent cell.
		let (tick, absent) = (0..64)
			.find_map(|tick| {
				let step = sparse_model.step(tick);
				for series in 0..step.grid.series {
					for row in 0..step.grid.rows {
						if step.grid.cell(series, row).is_none() {
							return Some((tick, (series, row)));
						}
					}
				}
				None
			})
			.expect("a 50% grid has an absent cell within 64 ticks");
		let mut observed = faithful(&sparse_model, tick);
		observed.push(ObservedPoint { series: absent.0, row: absent.1, timestamp_ns: sparse_model.timestamp_ns(tick, absent.1), value: GridValue::Float(0.0) });
		let verdict = oracle.judge_tick(tick, &observed);
		assert_eq!(verdict.mismatch_count, 1);
		assert_eq!(verdict.mismatches[0].kind, MismatchKind::Unexpected);
		assert_eq!(verdict.mismatches[0].expected, None, "an absent cell has no expected value");
	}

	#[test]
	fn an_out_of_grid_observation_is_unexpected() {
		let model = model();
		let oracle = SoakOracle::new(model.clone());
		let mut observed = faithful(&model, 0);
		observed.push(ObservedPoint { series: 99, row: 0, timestamp_ns: model.timestamp_ns(0, 0), value: GridValue::Float(1.0) });
		let verdict = oracle.judge_tick(0, &observed);
		assert_eq!(verdict.mismatch_count, 1);
		let mismatch = &verdict.mismatches[0];
		assert_eq!(mismatch.kind, MismatchKind::Unexpected);
		assert_eq!(mismatch.series, 99);
		assert_eq!(mismatch.expected, None);
	}

	#[test]
	fn a_duplicated_point_is_a_duplicate_mismatch() {
		let model = model();
		let oracle = SoakOracle::new(model.clone());
		let mut observed = faithful(&model, 0);
		observed.push(observed[0].clone());
		let verdict = oracle.judge_tick(0, &observed);
		assert_eq!(verdict.mismatch_count, 1, "a duplicated seal surfaces as exactly one duplicate, not a pass: {:?}", verdict.mismatches);
		assert_eq!(verdict.mismatches[0].kind, MismatchKind::Duplicate);
		assert_eq!(verdict.points_observed, 13, "the duplicate is counted as observed");
	}

	#[test]
	fn judge_run_accumulates_counts_across_ticks() {
		let model = model();
		let oracle = SoakOracle::new(model.clone());
		let mut short = faithful(&model, 1);
		short.remove(0);
		let verdict = oracle.judge_run(&[(0, faithful(&model, 0)), (1, short), (2, faithful(&model, 2))]);
		assert_eq!(verdict.ticks_judged, 3);
		assert_eq!(verdict.cells_checked, 36);
		assert_eq!(verdict.points_observed, 35);
		assert_eq!(verdict.mismatch_count, 1);
		assert_eq!(verdict.mismatches[0].tick, 1);
		assert!(!verdict.passed());
	}

	#[test]
	fn merge_combines_verdicts_and_respects_the_cap() {
		let oracle = oracle();
		let first = oracle.judge_tick(0, &[]);
		let second = oracle.judge_tick(1, &[]);
		let mut merged = first.clone();
		merged.merge(&second);
		assert_eq!(merged.ticks_judged, 2);
		assert_eq!(merged.mismatch_count, first.mismatch_count + second.mismatch_count);
		assert_eq!(merged.mismatches.len(), first.mismatches.len() + second.mismatches.len());
	}

	#[test]
	fn the_mismatch_list_is_capped_but_the_count_is_not() {
		// A model far wider than the cap, judged with no observations at
		// all: every present cell is missing.
		let wide = WorkloadModel::new(7, WorkloadParams::new(64, 64, 100, LaneWeights { ingest: 1, point_lookup: 0, range_scan: 0 })).expect("valid model");
		let oracle = SoakOracle::new(wide);
		let verdict = oracle.judge_tick(0, &[]);
		assert_eq!(verdict.mismatch_count, 64 * 64);
		assert_eq!(verdict.mismatches.len(), MAX_REPORTED_MISMATCHES);
		assert!(verdict.truncated(), "a capped verdict says so");
		assert_eq!(verdict.count_kind(MismatchKind::Missing), MAX_REPORTED_MISMATCHES);
	}

	#[test]
	fn a_verdict_round_trips_through_json() {
		let model = model();
		let oracle = SoakOracle::new(model.clone());
		let mut observed = faithful(&model, 0);
		observed.remove(0);
		let verdict = oracle.judge_tick(0, &observed);
		let json = verdict.to_json().expect("serialize verdict");
		let back: OracleVerdict = serde_json::from_str(&json).expect("deserialize verdict");
		assert_eq!(verdict, back);
	}

	#[test]
	fn the_verdict_is_recorded_on_the_run_record() {
		let config = SoakConfig::new("smoke".to_string(), "label".to_string(), 7, 60).expect("valid config");
		let mut harness = SoakHarness::new(config, metadata());
		let oracle = SoakOracle::new(model());
		let verdict = oracle.judge_tick(0, &faithful(&model(), 0));

		assert!(harness.record_oracle_verdict(verdict.clone()).is_err(), "a verdict cannot be recorded before the run starts");
		harness.start().expect("start from Created");
		harness.record_oracle_verdict(verdict.clone()).expect("record while Running");
		assert_eq!(harness.run_record().oracle.as_ref(), Some(&verdict), "the verdict is in the run metadata");
		assert!(harness.record_oracle_verdict(verdict).is_err(), "a second verdict must not overwrite the first");
		harness.finish().expect("finish from Running");
		assert_eq!(harness.run_record().state, SoakState::Finished);
		assert!(harness.run_record().oracle.is_some(), "the verdict survives the finish transition");

		let json = harness.run_record().redacted_json().expect("redacted run JSON");
		assert!(json.contains("\"oracle\""), "the recorded verdict is in the run's output artifact: {json}");
		let back: crate::soak::SoakRun = serde_json::from_str(&json).expect("run artifact deserializes");
		assert_eq!(back.oracle, harness.run_record().oracle);
	}

	#[test]
	fn the_oracle_self_test_passes_against_this_core() {
		let report = SoakOracle::self_test();
		assert!(report.passed(), "the self-test must pass against the core it ships with; failures: {:?}", report.failed_checks());
		assert_eq!(report.checks_run(), 9, "nine checks: one faithful pass, six corruption classes, two window digests");
		assert_eq!(report.failed_checks(), [] as [&str; 0]);
	}

	#[test]
	fn the_self_test_is_deterministic_and_round_trips() {
		let first = SoakOracle::self_test();
		assert_eq!(first, SoakOracle::self_test(), "the self-test is a pure function of the core");
		let json = first.to_json().expect("serialize self-test report");
		let back: OracleSelfTestReport = serde_json::from_str(&json).expect("deserialize self-test report");
		assert_eq!(first, back);
		let empty = OracleSelfTestReport::default();
		assert!(!empty.passed(), "a report that ran no checks is not a passed self-test");
	}

	#[test]
	fn the_oracle_window_digest_matches_the_digest_module() {
		let model = model();
		let oracle = SoakOracle::new(model.clone());
		let ticks: Vec<(u64, Vec<ObservedPoint>)> = (0..4).map(|tick| (tick, faithful(&model, tick))).collect();
		let expected = oracle.window_digest(0, 4);
		assert!(expected.matches(&crate::soak_digest::observed_window_digest(0, 4, &ticks)), "the oracle's expected digest agrees with a faithful read-back's");
		assert_eq!(expected, crate::soak_digest::expected_window_digest(&model, 0, 4), "the oracle's digest is the module's expected digest of its model");
	}

	#[test]
	fn a_v1_run_artifact_without_a_verdict_still_deserializes() {
		let config = SoakConfig::new("smoke".to_string(), String::new(), 7, 60).expect("valid config");
		let harness = SoakHarness::new(config, metadata());
		let mut value = serde_json::to_value(harness.run_record()).expect("serialize run");
		value.as_object_mut().expect("run is an object").remove("oracle");
		value["soak_schema_version"] = serde_json::json!(1);
		let back: crate::soak::SoakRun = serde_json::from_value(value).expect("v1 artifact deserializes");
		assert_eq!(back.oracle, None, "a v1 artifact carries no verdict");
	}
}
