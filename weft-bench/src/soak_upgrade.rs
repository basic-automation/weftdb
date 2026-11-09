//! Upgrade lane for the soak harness (1.0 plan SOA-16, W9): run an
//! upgrade inside a soak and judge the store it leaves behind.
//!
//! A soak that never upgrades proves the store survives being read and
//! written at one version; it says nothing about the store surviving a
//! version change mid-run. This lane is the check for that surface.
//! While ingest is running — the store online, ticks still being
//! acknowledged — the driver stops the old server binary and starts the
//! new one against the same store root, following the procedure
//! `UPGRADING.md` (PAC-14a) pins: copy the whole store directory first,
//! start the new binary, let the first open run the layout migration,
//! then read every acknowledged point back out of the upgraded store
//! and hand the read-back to this lane. The lane judges that read-back
//! against the run's whole acknowledged history: every acknowledged
//! tick, complete and exact, and nothing else. An upgrade that loses a
//! point, rewrites one, or invents one fails here even if the new
//! binary started cleanly.
//!
//! **The caller performs the upgrade, the stop, the start and the
//! reads.** Like the oracle core, the derived-read checks and the
//! backup lane, this module performs no upgrade, no stop, no start and
//! no read itself: the caller runs the old and new binaries, maps each
//! point it reads back into an [`ObservedPoint`] grouped by the tick it
//! belongs to, times the upgrade pass, and hands all of it to the
//! checker with the [`UpgradeStep`] describing which versions the pass
//! moved between. Which binaries, which store root, and which clock are
//! the caller's business — the slices that own how a soak is launched
//! are later ones, on the SOA-6/SOA-10/SOA-11 precedent. What this lane
//! adds over the backup lane's restore verification is the step itself:
//! the versions and layouts on either side of the change, and the
//! duration the pass took, so a published run carries *which* upgrade
//! it survived, not just that some store read back.
//!
//! **What an upgrade must preserve.** The check is the whole-history
//! rule, not a prefix: unlike a backup taken while ingest runs, an
//! upgraded store is the live store, so there is no cut and no
//! in-flight window — every acknowledged tick must be complete and
//! exact. Points are judged per cell exactly as SOA-6's oracle judges
//! a raw read-back: same [`ObservedPoint`] form, same exact comparison,
//! no tolerance. A point the migration rewrote (a re-encoded value, a
//! retimestamped row) is a [`UpgradeMismatchKind::WrongValue`] or
//! [`UpgradeMismatchKind::WrongTimestamp`], not a pass: the migration
//! PAC-14a documents is additive DDL that adds tables and columns
//! without rewriting existing data, so a value change across it is a
//! defect, not a format artefact.
//!
//! **The upgrade duration is recorded, never gated.** The caller
//! measures the pass with its own clock — from stopping the old binary
//! to the new one serving reads again — and the lane records it on the
//! [`UpgradeStep`]. Nothing here is a threshold: whether an upgrade
//! that took minutes is acceptable is an SLO judgement, and SOA-13
//! owns the SLO evaluator (R1–R8 are fixed by supervisor decision
//! 2026-11-06; this lane adds no rule). Consistent with SOA-2's,
//! SOA-3's, SOA-6's and SOA-11's refusal to invent pass/fail numbers,
//! the verdict is exact (zero mismatches) and the duration is
//! reported, never gated.
//!
//! **Recorded in the run record.** The combined verdict is recorded on
//! the run it judged through
//! [`SoakHarness::record_upgrade_verdict`](crate::soak::SoakHarness::record_upgrade_verdict),
//! as the `upgrade` field of [`SoakRun`](crate::soak::SoakRun) — beside
//! the oracle, derived-read and backup verdicts it complements. Adding
//! the field is a soak-record change, so
//! [`SOAK_SCHEMA_VERSION`](crate::soak::SOAK_SCHEMA_VERSION) moves to
//! v7 on SOA's own version line (the plan's §3 rule: EVI-3 owns the
//! shared metadata, SOA keeps its own `soak_schema_version`); the
//! field is `#[serde(default)]` and skipped when absent, so v1–v6
//! artifacts still deserialize, with no verdict. The verdict carries no
//! free text — the step's versions are release pins from
//! `compat/releases.toml` (structured `x.y.z` identifiers, not operator
//! text) and the layouts are numbers — so `SoakRun::redacted_json`
//! emits it unchanged and no new redaction path exists.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
	soak_oracle::ObservedPoint, soak_workload::{GridValue, WorkloadModel}
};

/// Cap on the mismatches an [`UpgradeLaneVerdict`] lists.
///
/// The total is always counted in the verdict's `mismatch_count`; the
/// cap only bounds the artifact a badly broken run produces, on the
/// same reasoning as the oracle's
/// [`MAX_REPORTED_MISMATCHES`](crate::soak_oracle::MAX_REPORTED_MISMATCHES).
pub const MAX_REPORTED_UPGRADE_MISMATCHES: usize = 1024;

/// How one point of a post-upgrade read-back differs from the
/// acknowledged history it was judged against. See the module doc for
/// the whole-history rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeMismatchKind {
	/// A cell an acknowledged tick filled has no observation where the
	/// upgraded store was required to return it.
	MissingPoint,
	/// An observation names a cell the model left absent, or outside
	/// the grid.
	UnexpectedPoint,
	/// An observation belongs to a tick that was never acknowledged:
	/// the upgraded store holds a write the server never confirmed.
	UnacknowledgedPoint,
	/// The point was read back at the right timestamp with a different
	/// value.
	WrongValue,
	/// The point was read back at a different timestamp than the model
	/// wrote it at.
	WrongTimestamp,
	/// The same cell was observed more than once.
	DuplicatePoint,
}

/// One point where a post-upgrade read-back disagreed with the
/// acknowledged history.
///
/// Carries both sides of the comparison — the expected and observed
/// values and timestamps — so a reviewer can see the difference
/// without re-running anything. `observed` is `None` exactly for
/// [`UpgradeMismatchKind::MissingPoint`], and `observed_timestamp_ns`
/// is `None` with it; `expected` is `None` exactly for an out-of-grid
/// [`UpgradeMismatchKind::UnexpectedPoint`] and for
/// [`UpgradeMismatchKind::UnacknowledgedPoint`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpgradeMismatch {
	/// Tick the point belongs to.
	pub tick: u64,
	/// Grid column (series) of the point.
	pub series: usize,
	/// Grid row of the point.
	pub row: usize,
	/// How the point differs.
	pub kind: UpgradeMismatchKind,
	/// Value the model wrote, or `None` when the point is outside the
	/// grid or its tick was never acknowledged.
	pub expected: Option<GridValue>,
	/// Value read back, or `None` when the point was never observed.
	pub observed: Option<GridValue>,
	/// Timestamp the model wrote the point at; for an unacknowledged
	/// tick this is the model's would-be timestamp for the cell.
	pub expected_timestamp_ns: i64,
	/// Timestamp the point was read back at, or `None` when it was
	/// never observed.
	pub observed_timestamp_ns: Option<i64>,
}

/// The upgrade pass the lane judged: which versions it moved between,
/// which store layouts, and how long it took.
///
/// The versions are release pins from `compat/releases.toml` (the tag
/// without its leading `v`, e.g. `"0.1.0"` → `"0.3.2"`); the layouts
/// are the `STORE_FORMAT` layout numbers `UPGRADING.md` documents
/// (layout 1 for a v0.1.0 store, layout 2 since v0.2.0). The duration
/// is the caller's own measurement, from stopping the old binary to
/// the new one serving reads again; it is recorded, never gated (see
/// the module doc).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeStep {
	/// Version the upgraded store was running before the pass.
	pub from_version: String,
	/// Version the upgraded store runs after the pass.
	pub to_version: String,
	/// Store layout before the pass.
	pub layout_before: u32,
	/// Store layout after the pass.
	pub layout_after: u32,
	/// Duration of the upgrade pass, in milliseconds.
	pub upgrade_duration_ms: u64,
}

/// The upgrade lane's whole verdict on one run: whether the store the
/// upgrade pass left behind holds the run's whole acknowledged history
/// exactly, and the step that produced it.
///
/// Recorded on the run record via
/// [`SoakHarness::record_upgrade_verdict`](crate::soak::SoakHarness::record_upgrade_verdict);
/// also serializes on its own for artifacts that want it beside the
/// record. Carries no free text.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UpgradeLaneVerdict {
	/// Acknowledged ticks judged — the whole acknowledged history,
	/// not a prefix.
	pub ticks_judged: u64,
	/// Points the acknowledged ticks filled: the points a faithful
	/// upgraded store must return.
	pub points_expected: u64,
	/// Observations supplied, duplicates and unexpected points
	/// included.
	pub points_upgraded: u64,
	/// Total mismatches found, whether listed in [`Self::mismatches`]
	/// or cut off by the cap.
	pub mismatch_count: u64,
	/// The mismatches, in judgement order (acknowledged ticks
	/// ascending; per tick, observations in the order supplied, then
	/// missing cells in series-major grid order), capped at
	/// [`MAX_REPORTED_UPGRADE_MISMATCHES`].
	pub mismatches: Vec<UpgradeMismatch>,
	/// The upgrade pass this verdict judges.
	pub step: UpgradeStep,
}

impl UpgradeLaneVerdict {
	/// Whether the upgraded store matched the acknowledged history
	/// exactly: no mismatch of any kind. A verdict that judged nothing
	/// passes vacuously — callers publishing a run check
	/// [`Self::ticks_judged`] beside this (the SOA-12 skipped-check
	/// rule).
	#[must_use]
	pub const fn passed(&self) -> bool {
		self.mismatch_count == 0
	}

	/// Whether the mismatch list was capped: the total in
	/// [`Self::mismatch_count`] is larger than the list kept.
	#[must_use]
	pub const fn truncated(&self) -> bool {
		#[allow(clippy::cast_possible_truncation)]
		let listed = self.mismatches.len() as u64;
		self.mismatch_count > listed
	}

	/// Record one mismatch, keeping the total count even past the cap.
	fn push(&mut self, mismatch: UpgradeMismatch) {
		self.mismatch_count = self.mismatch_count.saturating_add(1);
		if self.mismatches.len() < MAX_REPORTED_UPGRADE_MISMATCHES {
			self.mismatches.push(mismatch);
		}
	}
}

/// One tick's cell judgement, shared by the upgrade check: the tick's
/// observed points compared against the model's grid for that tick,
/// exactly as the oracle compares a read-back.
struct TickJudgement {
	/// Points the model filled for this tick.
	present: u64,
	/// Observations supplied for this tick.
	observed: u64,
	/// Mismatches found, in judgement order.
	mismatches: Vec<UpgradeMismatch>,
}

/// The upgrade lane checker: judges a post-upgrade read-back against
/// the workload model a run's traffic derived from.
///
/// Borrows nothing and performs no upgrade, stop, start or read — the
/// model is cloned at construction so the checker outlives the
/// caller's handle on it, the same shape as
/// [`SoakOracle`](crate::soak_oracle::SoakOracle),
/// [`DerivedReadChecker`](crate::soak_read::DerivedReadChecker) and
/// [`BackupLaneChecker`](crate::soak_backup::BackupLaneChecker).
#[derive(Debug, Clone)]
pub struct UpgradeLaneChecker {
	model: WorkloadModel,
}

impl UpgradeLaneChecker {
	/// Build a checker over `model` (SOA-4's workload model, derived
	/// from the soak config's seed).
	#[must_use]
	pub const fn new(model: WorkloadModel) -> Self {
		Self { model }
	}

	/// Build a checker from a soak config, via
	/// [`WorkloadModel::from_config`]: the config's seed alone decides
	/// the expected traffic.
	#[must_use]
	pub fn from_config(config: &crate::soak::SoakConfig) -> Self {
		Self { model: WorkloadModel::from_config(config) }
	}

	/// The faithful points of one tick: one [`ObservedPoint`] per
	/// filled cell, at the model's timestamp, with the model's value.
	/// A caller (or a test) that reads a faithful upgraded store
	/// observes exactly these for the tick.
	#[must_use]
	pub fn expected_points(&self, tick: u64) -> Vec<ObservedPoint> {
		let step = self.model.step(tick);
		let mut points = Vec::new();
		for series in 0..step.grid.series {
			for row in 0..step.grid.rows {
				if let Some(value) = step.grid.cell(series, row) {
					points.push(ObservedPoint { series, row, timestamp_ns: self.model.timestamp_ns(tick, row), value: value.clone() });
				}
			}
		}
		points
	}

	/// Judge one tick's observed points against the model's grid for
	/// it, cell by cell, exactly as the oracle judges a read-back.
	fn judge_tick(&self, tick: u64, observed: &[ObservedPoint]) -> TickJudgement {
		let step = self.model.step(tick);
		let grid = &step.grid;
		#[allow(clippy::cast_possible_truncation)]
		let present = grid.present_count() as u64;
		#[allow(clippy::cast_possible_truncation)]
		let observed_count = observed.len() as u64;
		let mut mismatches = Vec::new();
		let mut seen: BTreeMap<(usize, usize), &ObservedPoint> = BTreeMap::new();
		for point in observed {
			let expected_timestamp_ns = self.model.timestamp_ns(tick, point.row);
			let in_grid = grid.contains(point.series, point.row);
			let expected = if in_grid { grid.cell(point.series, point.row).cloned() } else { None };
			if seen.insert((point.series, point.row), point).is_some() {
				mismatches.push(UpgradeMismatch { tick, series: point.series, row: point.row, kind: UpgradeMismatchKind::DuplicatePoint, expected, observed: Some(point.value.clone()), expected_timestamp_ns, observed_timestamp_ns: Some(point.timestamp_ns) });
				continue;
			}
			match expected {
				None => {
					mismatches.push(UpgradeMismatch { tick, series: point.series, row: point.row, kind: UpgradeMismatchKind::UnexpectedPoint, expected: None, observed: Some(point.value.clone()), expected_timestamp_ns, observed_timestamp_ns: Some(point.timestamp_ns) });
				}
				Some(expected_value) => {
					if point.timestamp_ns != expected_timestamp_ns {
						mismatches.push(UpgradeMismatch { tick, series: point.series, row: point.row, kind: UpgradeMismatchKind::WrongTimestamp, expected: Some(expected_value), observed: Some(point.value.clone()), expected_timestamp_ns, observed_timestamp_ns: Some(point.timestamp_ns) });
					} else if point.value != expected_value {
						mismatches.push(UpgradeMismatch { tick, series: point.series, row: point.row, kind: UpgradeMismatchKind::WrongValue, expected: Some(expected_value), observed: Some(point.value.clone()), expected_timestamp_ns, observed_timestamp_ns: Some(point.timestamp_ns) });
					}
				}
			}
		}
		for series in 0..grid.series {
			for row in 0..grid.rows {
				if let Some(value) = grid.cell(series, row) {
					if !seen.contains_key(&(series, row)) {
						mismatches.push(UpgradeMismatch { tick, series, row, kind: UpgradeMismatchKind::MissingPoint, expected: Some(value.clone()), observed: None, expected_timestamp_ns: self.model.timestamp_ns(tick, row), observed_timestamp_ns: None });
					}
				}
			}
		}
		TickJudgement { present, observed: observed_count, mismatches }
	}

	/// Judge the upgrade lane: judge `observed` — the points read back
	/// from the store the upgrade pass left behind, grouped by tick —
	/// against the whole acknowledged history `acknowledged`, and
	/// record the `step` that produced it. Every acknowledged tick
	/// must be complete and exact; an observed tick that was never
	/// acknowledged is reported per point as
	/// [`UpgradeMismatchKind::UnacknowledgedPoint`].
	#[must_use]
	pub fn judge_upgrade(&self, acknowledged: &[u64], observed: &[(u64, Vec<ObservedPoint>)], step: UpgradeStep) -> UpgradeLaneVerdict {
		let mut ack: Vec<u64> = acknowledged.to_vec();
		ack.sort_unstable();
		ack.dedup();
		let ack_set: BTreeSet<u64> = ack.iter().copied().collect();
		let observed_by_tick: BTreeMap<u64, &Vec<ObservedPoint>> = observed.iter().map(|(tick, points)| (*tick, points)).collect();
		let mut verdict = UpgradeLaneVerdict { ticks_judged: ack.len() as u64, step, ..UpgradeLaneVerdict::default() };
		for tick in &ack {
			let points = observed_by_tick.get(tick).map_or(&[][..], |points| points.as_slice());
			let judgement = self.judge_tick(*tick, points);
			verdict.points_expected = verdict.points_expected.saturating_add(judgement.present);
			verdict.points_upgraded = verdict.points_upgraded.saturating_add(judgement.observed);
			for mismatch in judgement.mismatches {
				verdict.push(mismatch);
			}
		}
		for (tick, points) in &observed_by_tick {
			if ack_set.contains(tick) {
				continue;
			}
			verdict.points_upgraded = verdict.points_upgraded.saturating_add(points.len() as u64);
			for point in *points {
				verdict.push(UpgradeMismatch { tick: *tick, series: point.series, row: point.row, kind: UpgradeMismatchKind::UnacknowledgedPoint, expected: None, observed: Some(point.value.clone()), expected_timestamp_ns: self.model.timestamp_ns(*tick, point.row), observed_timestamp_ns: Some(point.timestamp_ns) });
			}
		}
		verdict
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::soak_workload::{LaneWeights, WorkloadParams};

	fn checker(series: usize, rows: usize, density_percent: u32) -> UpgradeLaneChecker {
		let params = WorkloadParams::new(series, rows, density_percent, LaneWeights { ingest: 1, point_lookup: 0, range_scan: 0 });
		UpgradeLaneChecker::new(WorkloadModel::new(42, params).expect("model"))
	}

	fn step(from: &str, to: &str, layout_before: u32, layout_after: u32, duration_ms: u64) -> UpgradeStep {
		UpgradeStep { from_version: from.to_string(), to_version: to.to_string(), layout_before, layout_after, upgrade_duration_ms: duration_ms }
	}

	/// The faithful read-back of `ticks`: each tick grouped with the
	/// points a faithful upgraded store returns for it.
	fn faithful(checker: &UpgradeLaneChecker, ticks: &[u64]) -> Vec<(u64, Vec<ObservedPoint>)> {
		ticks.iter().map(|tick| (*tick, checker.expected_points(*tick))).collect()
	}

	#[test]
	fn a_faithful_upgrade_passes_and_records_the_step() {
		let checker = checker(4, 8, 80);
		let acknowledged = [0, 1, 2, 3];
		let verdict = checker.judge_upgrade(&acknowledged, &faithful(&checker, &acknowledged), step("0.1.0", "0.3.2", 1, 2, 450));
		assert!(verdict.passed(), "{:?}", verdict.mismatches);
		assert_eq!(verdict.ticks_judged, 4);
		assert_eq!(verdict.points_upgraded, verdict.points_expected);
		assert!(verdict.points_expected > 0);
		assert_eq!(verdict.step.from_version, "0.1.0");
		assert_eq!(verdict.step.to_version, "0.3.2");
		assert_eq!(verdict.step.layout_before, 1);
		assert_eq!(verdict.step.layout_after, 2);
		assert_eq!(verdict.step.upgrade_duration_ms, 450);
	}

	#[test]
	fn an_upgrade_that_loses_a_point_fails_as_missing() {
		let checker = checker(4, 8, 100);
		let acknowledged = [0, 1, 2];
		let mut observed = faithful(&checker, &[0, 1, 2]);
		observed[1].1.remove(0);
		let verdict = checker.judge_upgrade(&acknowledged, &observed, step("0.1.0", "0.3.2", 1, 2, 450));
		assert!(!verdict.passed());
		assert!(verdict.mismatches.iter().any(|m| m.kind == UpgradeMismatchKind::MissingPoint && m.tick == 1), "{:?}", verdict.mismatches);
	}

	#[test]
	fn a_rewritten_value_fails_exactly() {
		let checker = checker(4, 8, 100);
		let acknowledged = [0, 1];
		let mut observed = faithful(&checker, &[0, 1]);
		observed[0].1[0].value = GridValue::Float(-999.5);
		let verdict = checker.judge_upgrade(&acknowledged, &observed, step("0.2.0", "0.3.2", 2, 2, 120));
		assert!(!verdict.passed());
		assert!(verdict.mismatches.iter().any(|m| m.kind == UpgradeMismatchKind::WrongValue && m.tick == 0), "{:?}", verdict.mismatches);
	}

	#[test]
	fn a_retimestamped_point_fails_exactly() {
		let checker = checker(4, 8, 100);
		let acknowledged = [0];
		let mut observed = faithful(&checker, &[0]);
		observed[0].1[0].timestamp_ns += 1;
		let verdict = checker.judge_upgrade(&acknowledged, &observed, step("0.2.0", "0.3.2", 2, 2, 120));
		assert!(!verdict.passed());
		assert!(verdict.mismatches.iter().any(|m| m.kind == UpgradeMismatchKind::WrongTimestamp && m.tick == 0), "{:?}", verdict.mismatches);
	}

	#[test]
	fn duplicate_and_unexpected_points_are_classified() {
		let checker = checker(4, 8, 100);
		let acknowledged = [0];
		let mut observed = faithful(&checker, &[0]);
		let duplicate = observed[0].1[0].clone();
		observed[0].1.push(duplicate);
		observed[0].1.push(ObservedPoint { series: 99, row: 0, timestamp_ns: 0, value: GridValue::Float(1.0) });
		let verdict = checker.judge_upgrade(&acknowledged, &observed, step("0.2.0", "0.3.2", 2, 2, 120));
		assert!(!verdict.passed());
		assert!(verdict.mismatches.iter().any(|m| m.kind == UpgradeMismatchKind::DuplicatePoint), "{:?}", verdict.mismatches);
		assert!(verdict.mismatches.iter().any(|m| m.kind == UpgradeMismatchKind::UnexpectedPoint), "{:?}", verdict.mismatches);
	}

	#[test]
	fn a_point_from_an_unacknowledged_tick_is_reported_as_such() {
		let checker = checker(4, 8, 100);
		let acknowledged = [0, 1];
		let mut observed = faithful(&checker, &[0, 1]);
		observed.push((7, checker.expected_points(7)));
		let verdict = checker.judge_upgrade(&acknowledged, &observed, step("0.1.0", "0.3.2", 1, 2, 450));
		assert!(!verdict.passed());
		assert!(verdict.mismatches.iter().all(|m| m.kind == UpgradeMismatchKind::UnacknowledgedPoint && m.tick == 7), "{:?}", verdict.mismatches);
	}

	#[test]
	fn an_upgrade_with_no_acknowledged_history_passes_vacuously() {
		let checker = checker(4, 8, 100);
		let verdict = checker.judge_upgrade(&[], &[], step("0.1.0", "0.3.2", 1, 2, 450));
		assert!(verdict.passed(), "nothing judged, nothing lost");
		assert_eq!(verdict.ticks_judged, 0, "a publisher can see no upgrade was proven");
	}

	#[test]
	fn the_mismatch_list_is_capped_but_the_total_is_kept() {
		let checker = checker(8, 64, 100);
		let acknowledged = [0, 1, 2];
		let verdict = checker.judge_upgrade(&acknowledged, &[], step("0.1.0", "0.3.2", 1, 2, 450));
		assert!(!verdict.passed());
		assert!(verdict.mismatch_count > MAX_REPORTED_UPGRADE_MISMATCHES as u64, "three dense ticks lose more than the cap: {}", verdict.mismatch_count);
		assert_eq!(verdict.mismatches.len(), MAX_REPORTED_UPGRADE_MISMATCHES);
		assert!(verdict.truncated());
	}

	#[test]
	fn the_verdict_round_trips_through_json_with_its_step() {
		let checker = checker(4, 8, 100);
		let acknowledged = [0];
		let verdict = checker.judge_upgrade(&acknowledged, &faithful(&checker, &acknowledged), step("0.1.0", "0.3.2", 1, 2, 450));
		let json = serde_json::to_string(&verdict).expect("verdict serializes");
		let back: UpgradeLaneVerdict = serde_json::from_str(&json).expect("verdict deserializes");
		assert_eq!(back, verdict);
		assert_eq!(back.step.to_version, "0.3.2");
	}
}
