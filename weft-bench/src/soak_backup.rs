//! Backup lane for the soak harness (1.0 plan SOA-11, W7): the online
//! prefix check, the final restore verification, and the RPO/RTO the
//! lane records.
//!
//! A soak that never backs up proves the store survives being read and
//! written; it says nothing about the store surviving being *lost*.
//! This lane is the check for that surface. While ingest is running —
//! the store online, ticks still being acknowledged — the driver takes
//! a backup through the server's backup path (the daemon's tick or
//! `POST /api/v1/storage/backup`, over D-S16's whole-store backup), and
//! afterwards restores the run's final backup through PAC-13's restore
//! path (`weft-server restore`, C19's one CLI, over
//! [`restore_store`](weftdb::restore_store) /
//! [`restore_control_plane`](weftdb::restore_control_plane)) and reads
//! the restored store back. This module judges both read-backs against
//! SOA-4's workload model and records the recovery objectives the lane
//! measured. EVI-19's RPO/RTO page names this slice as the one that
//! times the soak backup lane and the final restore verification; until
//! it landed, end-to-end RTO was recorded there as unmeasured.
//!
//! **The online prefix check.** A backup taken against a live store
//! cannot hold the run's whole acknowledged history — ingest kept
//! moving while the snapshot pass ran (that window is `d_max` in the
//! RPO formula). What it must hold is a *prefix* of it: the snapshot is
//! a point-in-time image, so there is a cut in the acknowledged tick
//! sequence such that the backup holds every point of every
//! acknowledged tick up to the cut, complete and exact, and no point
//! from any later tick, and no point that was never acknowledged. A
//! backup that holds a later tick while an earlier acknowledged tick
//! is partial or missing is not a snapshot of any moment the store
//! passed through — it is a torn image, and the check fails it.
//! [`BackupLaneChecker::judge_prefix`] makes that judgement: it finds
//! the cut (the last acknowledged tick the backup holds completely,
//! with every acknowledged tick before it also complete) and classifies
//! every deviation — see [`BackupMismatchKind`]. Points are judged per
//! cell exactly as SOA-6's oracle judges a raw read-back: same
//! [`ObservedPoint`] form, same exact comparison, no tolerance.
//!
//! **The final restore verification.** The prefix check judges a
//! backup still sitting beside the live store; the restore check
//! judges what the backup is *for*. The caller restores the final
//! backup through PAC-13's restore path into a fresh root, reads every
//! point back out of the restored store, and
//! [`BackupLaneChecker::judge_restore`] requires the restored store to
//! hold the run's whole acknowledged history — every acknowledged
//! tick, complete and exact — and nothing else. A restore that loses a
//! point the backup held, or invents one, fails here even if the
//! backup itself passed the prefix check.
//!
//! **RPO/RTO, recorded not asserted.** [`RecoveryTiming`] carries the
//! three durations the caller measured with its own clock — the lane's
//! backup interval, one backup pass's duration, and the final
//! restore's duration — and derives the two objectives by the formulas
//! EVI-19's page pins: `RPO = max(I, d_max) + d_max`, where `I` is the
//! backup interval and `d_max` the backup pass's own duration (the
//! window of acknowledged-but-not-yet-snapshotted writes), and RTO is
//! the time to a restored store serving again *including* taking the
//! backup it restores from, so `RTO = backup + restore`. The lane
//! records the numbers; it invents no threshold for them — whether an
//! RPO or RTO is acceptable is an SLO judgement, and SOA-13 owns the
//! SLO evaluator. Consistent with SOA-2's, SOA-3's and SOA-6's refusal
//! to invent pass/fail numbers, the verdicts here are exact (zero
//! mismatches) and the timings are reported, never gated.
//!
//! **The caller performs the backup, the restore and the reads.** Like
//! the oracle core and the derived-read checks, this module performs
//! no backup, no restore and no read itself: the caller runs the
//! backup against its live server, restores through PAC-13's path,
//! maps each point it reads back into an [`ObservedPoint`] grouped by
//! the tick it belongs to, times the passes, and hands all of it to
//! the checker. Which client, which backup route spelling, and which
//! clock are the caller's business — the slices that own how a soak is
//! launched are later ones, on the SOA-6/SOA-10 precedent.
//!
//! **Recorded in the run record.** The combined verdict is recorded on
//! the run it judged through
//! [`SoakHarness::record_backup_verdict`](crate::soak::SoakHarness::record_backup_verdict),
//! as the `backup` field of [`SoakRun`](crate::soak::SoakRun) — beside
//! the oracle and derived-read verdicts it complements. Adding the
//! field is a soak-record change, so
//! [`SOAK_SCHEMA_VERSION`](crate::soak::SOAK_SCHEMA_VERSION) moves to
//! v6 on SOA's own version line (the plan's §3 rule: EVI-3 owns the
//! shared metadata, SOA keeps its own `soak_schema_version`); the
//! field is `#[serde(default)]` and skipped when absent, so v1–v5
//! artifacts still deserialize, with no verdict. The verdict carries
//! no free text, so `SoakRun::redacted_json` emits it unchanged and no
//! new redaction path exists.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
	soak_oracle::ObservedPoint, soak_workload::{GridValue, WorkloadModel}
};

/// Cap on the mismatches a [`BackupPrefixVerdict`] or
/// [`RestoreVerdict`] lists.
///
/// The total is always counted in the
/// verdict's `mismatch_count`; the cap only bounds the artifact a
/// badly broken run produces, on the same reasoning as the oracle's
/// [`MAX_REPORTED_MISMATCHES`](crate::soak_oracle::MAX_REPORTED_MISMATCHES).
pub const MAX_REPORTED_BACKUP_MISMATCHES: usize = 1024;

/// How one point of a backup or restored read-back differs from the
/// acknowledged prefix (or, for the restore, the acknowledged whole)
/// it was judged against. See the module doc for the prefix rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupMismatchKind {
	/// A cell an acknowledged tick filled has no observation where the
	/// prefix (or the restored whole) required it.
	MissingPoint,
	/// An observation names a cell the model left absent, or outside
	/// the grid.
	UnexpectedPoint,
	/// An observation belongs to a tick that was never acknowledged:
	/// the backup (or the restore) holds a write the server never
	/// confirmed — the one content no snapshot of the store can hold.
	UnacknowledgedPoint,
	/// The point was read back at the right timestamp with a different
	/// value.
	WrongValue,
	/// The point was read back at a different timestamp than the model
	/// wrote it at.
	WrongTimestamp,
	/// The same cell was observed more than once.
	DuplicatePoint,
	/// A point from an acknowledged tick *after* the cut, observed
	/// while an earlier acknowledged tick is incomplete: the image is
	/// torn — it is not the prefix of any moment the store passed
	/// through. Reported once per offending tick, at its first point.
	BeyondPrefix,
}

/// One point where a backup or restored read-back disagreed with the
/// model.
///
/// Carries both sides of the comparison — the expected and observed
/// values and timestamps — so a reviewer can see the difference
/// without re-running anything. `observed` is `None` exactly for
/// [`BackupMismatchKind::MissingPoint`], and `observed_timestamp_ns`
/// is `None` with it; `expected` is `None` exactly for an out-of-grid
/// [`BackupMismatchKind::UnexpectedPoint`] and for
/// [`BackupMismatchKind::UnacknowledgedPoint`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupMismatch {
	/// Tick the point belongs to.
	pub tick: u64,
	/// Grid column (series) of the point.
	pub series: usize,
	/// Grid row of the point.
	pub row: usize,
	/// How the point differs.
	pub kind: BackupMismatchKind,
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

/// The durations a backup lane measured, and the recovery objectives
/// derived from them by EVI-19's formulas (see the module doc).
///
/// The caller measures all three with its own clock: the interval it
/// configured the lane's backups on, how long one backup pass took
/// (the final pass, the one the restore restores from), and how long
/// the final restore through PAC-13's path took. Nothing here is a
/// threshold: the objectives are recorded so a published run carries
/// its RPO/RTO with it, and SOA-13's SLO evaluator is what judges
/// them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryTiming {
	/// `I`: the lane's backup interval, in milliseconds — how often
	/// the lane took a backup while ingest ran.
	pub backup_interval_ms: u64,
	/// `d_max`: one backup pass's duration, in milliseconds — the
	/// window during which writes are acknowledged but not yet in any
	/// snapshot (the vacuum, verify, fsync, manifest and rename of
	/// D-S16's backup pass).
	pub backup_duration_ms: u64,
	/// The final restore's duration, in milliseconds: PAC-13's restore
	/// path from invocation to a published, verified root.
	pub restore_duration_ms: u64,
}

impl RecoveryTiming {
	/// The recovery-point objective, in milliseconds:
	/// `max(I, d_max) + d_max`, the corrected formula EVI-19's page
	/// pins (plan decision D-S21, superseding crash-design §517's).
	/// The `max` term is why a backup pass slower than its interval
	/// hurts twice: the effective interval stretches to the pass
	/// length, and the pass's own window is added on top.
	#[must_use]
	pub const fn rpo_ms(&self) -> u64 {
		let interval = if self.backup_interval_ms > self.backup_duration_ms { self.backup_interval_ms } else { self.backup_duration_ms };
		interval.saturating_add(self.backup_duration_ms)
	}

	/// The recovery-time objective, in milliseconds: the backup the
	/// restore restores from, plus the restore itself. EVI-19's page
	/// defines RTO from the start of an outage to a restored store
	/// serving again *including* taking the backup it restores from
	/// and running the restore, so the two measured durations add.
	#[must_use]
	pub const fn rto_ms(&self) -> u64 {
		self.backup_duration_ms.saturating_add(self.restore_duration_ms)
	}
}

/// The online prefix check's verdict on one backup: where its cut fell
/// in the acknowledged tick sequence, what was checked, and every way
/// the image deviated from a prefix of the acknowledged history.
///
/// Recorded on the run record as part of [`BackupLaneVerdict`]; also
/// serializes on its own. Carries no free text.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BackupPrefixVerdict {
	/// Acknowledged ticks the backup was judged against.
	pub acknowledged_ticks: u64,
	/// Distinct ticks the backup held at least one point of.
	pub ticks_in_backup: u64,
	/// The cut: the last acknowledged tick the backup holds
	/// completely, with every acknowledged tick before it also
	/// complete. `None` when the backup holds no complete acknowledged
	/// tick (an empty backup, or a torn first tick).
	pub cut_tick: Option<u64>,
	/// Points the acknowledged ticks up to the backup's furthest
	/// observed tick filled — the points a prefix reaching that far
	/// must hold exactly.
	pub points_expected: u64,
	/// Observations supplied, duplicates and unexpected points
	/// included.
	pub points_in_backup: u64,
	/// Total mismatches found, whether listed in [`Self::mismatches`]
	/// or cut off by the cap.
	pub mismatch_count: u64,
	/// The mismatches, in judgement order (acknowledged ticks
	/// ascending; per tick, observations in the order supplied, then
	/// missing cells in series-major grid order, then any
	/// beyond-prefix ticks), capped at
	/// [`MAX_REPORTED_BACKUP_MISMATCHES`].
	pub mismatches: Vec<BackupMismatch>,
}

impl BackupPrefixVerdict {
	/// Whether the backup is an exact prefix of the acknowledged
	/// history: no mismatch of any kind. A verdict that judged no
	/// acknowledged ticks passes vacuously, so publishers check
	/// [`Self::acknowledged_ticks`] and [`Self::cut_tick`] beside
	/// this (the SOA-12 skipped-check rule).
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
	fn push(&mut self, mismatch: BackupMismatch) {
		self.mismatch_count = self.mismatch_count.saturating_add(1);
		if self.mismatches.len() < MAX_REPORTED_BACKUP_MISMATCHES {
			self.mismatches.push(mismatch);
		}
	}
}

/// The final restore verification's verdict: whether the store
/// restored through PAC-13's restore path holds the run's whole
/// acknowledged history exactly, and nothing else.
///
/// Recorded on the run record as part of [`BackupLaneVerdict`]; also
/// serializes on its own. Carries no free text.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RestoreVerdict {
	/// Acknowledged ticks judged — the whole acknowledged history,
	/// not a prefix.
	pub ticks_judged: u64,
	/// Points the acknowledged ticks filled: the points a faithful
	/// restored store must return.
	pub points_expected: u64,
	/// Observations supplied, duplicates and unexpected points
	/// included.
	pub points_restored: u64,
	/// Total mismatches found, whether listed in [`Self::mismatches`]
	/// or cut off by the cap.
	pub mismatch_count: u64,
	/// The mismatches, in judgement order (acknowledged ticks
	/// ascending; per tick, observations in the order supplied, then
	/// missing cells in series-major grid order), capped at
	/// [`MAX_REPORTED_BACKUP_MISMATCHES`].
	pub mismatches: Vec<BackupMismatch>,
}

impl RestoreVerdict {
	/// Whether the restored store matched the acknowledged history
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
	fn push(&mut self, mismatch: BackupMismatch) {
		self.mismatch_count = self.mismatch_count.saturating_add(1);
		if self.mismatches.len() < MAX_REPORTED_BACKUP_MISMATCHES {
			self.mismatches.push(mismatch);
		}
	}
}

/// The backup lane's whole verdict on one run: the online prefix
/// check on a backup taken while ingest ran, the final restore
/// verification through PAC-13's restore path, and the RPO/RTO the
/// lane measured.
///
/// Recorded on the run record via
/// [`SoakHarness::record_backup_verdict`](crate::soak::SoakHarness::record_backup_verdict);
/// also serializes on its own for artifacts that want it beside the
/// record. Carries no free text.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BackupLaneVerdict {
	/// The online prefix check's verdict on the lane's backup.
	pub prefix: BackupPrefixVerdict,
	/// The final restore verification's verdict.
	pub restore: RestoreVerdict,
	/// The durations the lane measured and the objectives derived
	/// from them (see [`RecoveryTiming::rpo_ms`] /
	/// [`RecoveryTiming::rto_ms`]).
	pub timing: RecoveryTiming,
}

impl BackupLaneVerdict {
	/// Whether the lane passed: the backup is an exact prefix of the
	/// acknowledged history *and* the restored store holds that
	/// history whole and exact. The timings are recorded, never gated
	/// (see the module doc), so they play no part here. A lane that
	/// judged nothing passes vacuously — publishers check the
	/// verdicts' tick counts beside this (the SOA-12 rule).
	#[must_use]
	pub const fn passed(&self) -> bool {
		self.prefix.passed() && self.restore.passed()
	}

	/// The run's recorded RPO, in milliseconds, from the lane's
	/// measured timing (EVI-19's formula; see
	/// [`RecoveryTiming::rpo_ms`]).
	#[must_use]
	pub const fn rpo_ms(&self) -> u64 {
		self.timing.rpo_ms()
	}

	/// The run's recorded RTO, in milliseconds, from the lane's
	/// measured timing (see [`RecoveryTiming::rto_ms`]).
	#[must_use]
	pub const fn rto_ms(&self) -> u64 {
		self.timing.rto_ms()
	}
}

/// One tick's cell judgement, shared by the prefix check and the
/// restore verification: the tick's observed points compared against
/// the model's grid for that tick, exactly as the oracle compares a
/// read-back.
struct TickJudgement {
	/// Points the model filled for this tick.
	present: u64,
	/// Observations supplied for this tick.
	observed: u64,
	/// Mismatches found, in judgement order.
	mismatches: Vec<BackupMismatch>,
}

impl TickJudgement {
	/// Whether the tick was observed complete and exact: every filled
	/// cell observed once, at its timestamp, with its value, and no
	/// other observation.
	const fn complete(&self) -> bool {
		self.mismatches.is_empty()
	}
}

/// The backup lane checker: judges a backup's and a restored store's
/// read-backs against the workload model a run's traffic derived from.
///
/// Borrows nothing and performs no backup, restore or read — the
/// model is cloned at construction so the checker outlives the
/// caller's handle on it, the same shape as
/// [`SoakOracle`](crate::soak_oracle::SoakOracle) and
/// [`DerivedReadChecker`](crate::soak_read::DerivedReadChecker).
#[derive(Debug, Clone)]
pub struct BackupLaneChecker {
	model: WorkloadModel,
}

impl BackupLaneChecker {
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
	/// A caller (or a test) that reads a faithful backup or restored
	/// store observes exactly these for the tick.
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
			let in_grid = point.series < grid.series && point.row < grid.rows;
			let expected = if in_grid { grid.cell(point.series, point.row).cloned() } else { None };
			if seen.insert((point.series, point.row), point).is_some() {
				mismatches.push(BackupMismatch { tick, series: point.series, row: point.row, kind: BackupMismatchKind::DuplicatePoint, expected, observed: Some(point.value.clone()), expected_timestamp_ns, observed_timestamp_ns: Some(point.timestamp_ns) });
				continue;
			}
			match expected {
				None => {
					mismatches.push(BackupMismatch { tick, series: point.series, row: point.row, kind: BackupMismatchKind::UnexpectedPoint, expected: None, observed: Some(point.value.clone()), expected_timestamp_ns, observed_timestamp_ns: Some(point.timestamp_ns) });
				}
				Some(expected_value) => {
					if point.timestamp_ns != expected_timestamp_ns {
						mismatches.push(BackupMismatch { tick, series: point.series, row: point.row, kind: BackupMismatchKind::WrongTimestamp, expected: Some(expected_value), observed: Some(point.value.clone()), expected_timestamp_ns, observed_timestamp_ns: Some(point.timestamp_ns) });
					} else if point.value != expected_value {
						mismatches.push(BackupMismatch { tick, series: point.series, row: point.row, kind: BackupMismatchKind::WrongValue, expected: Some(expected_value), observed: Some(point.value.clone()), expected_timestamp_ns, observed_timestamp_ns: Some(point.timestamp_ns) });
					}
				}
			}
		}
		for series in 0..grid.series {
			for row in 0..grid.rows {
				if let Some(value) = grid.cell(series, row) {
					if !seen.contains_key(&(series, row)) {
						mismatches.push(BackupMismatch { tick, series, row, kind: BackupMismatchKind::MissingPoint, expected: Some(value.clone()), observed: None, expected_timestamp_ns: self.model.timestamp_ns(tick, row), observed_timestamp_ns: None });
					}
				}
			}
		}
		TickJudgement { present, observed: observed_count, mismatches }
	}

	/// The online prefix check: judge `observed` — the points read
	/// back from a backup taken while ingest was running, grouped by
	/// tick — against the acknowledged history `acknowledged`.
	///
	/// Acknowledged ticks are judged in ascending order with
	/// duplicates removed (a tick acknowledged twice is still one
	/// tick of history). The check finds the furthest tick the backup
	/// holds any point of; every acknowledged tick up to there must be
	/// complete and exact, and the cut is the last of the unbroken run
	/// of complete ticks from the start. A tick after the cut that the
	/// backup holds any point of is reported once as
	/// [`BackupMismatchKind::BeyondPrefix`] (its own cell mismatches
	/// are not also reported — the tear is the finding). Observed
	/// ticks that were never acknowledged are reported per point as
	/// [`BackupMismatchKind::UnacknowledgedPoint`]. Acknowledged ticks
	/// past the furthest observed tick are simply not in the backup
	/// yet — that is what the cut records, not a failure.
	#[must_use]
	pub fn judge_prefix(&self, acknowledged: &[u64], observed: &[(u64, Vec<ObservedPoint>)]) -> BackupPrefixVerdict {
		let mut ack: Vec<u64> = acknowledged.to_vec();
		ack.sort_unstable();
		ack.dedup();
		let ack_set: BTreeSet<u64> = ack.iter().copied().collect();
		let observed_by_tick: BTreeMap<u64, &Vec<ObservedPoint>> = observed.iter().map(|(tick, points)| (*tick, points)).collect();
		let mut verdict = BackupPrefixVerdict { acknowledged_ticks: ack.len() as u64, ..BackupPrefixVerdict::default() };

		// The furthest acknowledged tick the backup holds any point of
		// bounds the prefix: everything up to it must be complete.
		let furthest = ack.iter().rev().find(|tick| observed_by_tick.get(tick).is_some_and(|points| !points.is_empty())).copied();
		let mut cut: Option<u64> = None;
		let mut cut_broken = false;
		if let Some(furthest) = furthest {
			for tick in ack.iter().take_while(|tick| **tick <= furthest) {
				let points = observed_by_tick.get(tick).map_or(&[][..], |points| points.as_slice());
				let judgement = self.judge_tick(*tick, points);
				verdict.points_expected = verdict.points_expected.saturating_add(judgement.present);
				verdict.points_in_backup = verdict.points_in_backup.saturating_add(judgement.observed);
				if judgement.complete() {
					if !cut_broken {
						cut = Some(*tick);
					}
				} else {
					cut_broken = true;
					for mismatch in judgement.mismatches {
						verdict.push(mismatch);
					}
				}
			}
		}
		verdict.cut_tick = cut;

		// Ticks the backup holds points of that the prefix region did
		// not judge: only the unacknowledged ones remain — an
		// acknowledged tick at or below the furthest observed tick was
		// judged cell by cell above, and one past it holds no points
		// by construction. An unacknowledged tick was never the
		// store's to snapshot; report each of its points.
		for (tick, points) in &observed_by_tick {
			if points.is_empty() {
				continue;
			}
			verdict.ticks_in_backup = verdict.ticks_in_backup.saturating_add(1);
			if !ack_set.contains(tick) {
				verdict.points_in_backup = verdict.points_in_backup.saturating_add(points.len() as u64);
				for point in *points {
					verdict.push(BackupMismatch { tick: *tick, series: point.series, row: point.row, kind: BackupMismatchKind::UnacknowledgedPoint, expected: None, observed: Some(point.value.clone()), expected_timestamp_ns: self.model.timestamp_ns(*tick, point.row), observed_timestamp_ns: Some(point.timestamp_ns) });
				}
			}
		}
		// Beyond-prefix: an acknowledged tick past the cut, up to the
		// furthest observed, whose cells were judged above already had
		// its deviations listed; a tick past the furthest observed has
		// no points and cannot tear the image. The tear itself — a
		// later tick present while an earlier one is incomplete — is
		// recorded once, at the first present tick past the cut, when
		// the cut was broken by an incomplete tick inside the region.
		if cut_broken {
			if let Some(furthest) = furthest {
				if cut.is_none() || furthest > cut.unwrap_or(0) {
					// The first observed point of the furthest tick is
					// the evidence the image reached past the break.
					if let Some(points) = observed_by_tick.get(&furthest) {
						if let Some(point) = points.first() {
							verdict.push(BackupMismatch { tick: furthest, series: point.series, row: point.row, kind: BackupMismatchKind::BeyondPrefix, expected: None, observed: Some(point.value.clone()), expected_timestamp_ns: self.model.timestamp_ns(furthest, point.row), observed_timestamp_ns: Some(point.timestamp_ns) });
						}
					}
				}
			}
		}
		verdict
	}

	/// The final restore verification: judge `observed` — the points
	/// read back from the store restored through PAC-13's restore
	/// path, grouped by tick — against the whole acknowledged history
	/// `acknowledged`. Every acknowledged tick must be complete and
	/// exact; an observed tick that was never acknowledged is reported
	/// per point as [`BackupMismatchKind::UnacknowledgedPoint`].
	#[must_use]
	pub fn judge_restore(&self, acknowledged: &[u64], observed: &[(u64, Vec<ObservedPoint>)]) -> RestoreVerdict {
		let mut ack: Vec<u64> = acknowledged.to_vec();
		ack.sort_unstable();
		ack.dedup();
		let ack_set: BTreeSet<u64> = ack.iter().copied().collect();
		let observed_by_tick: BTreeMap<u64, &Vec<ObservedPoint>> = observed.iter().map(|(tick, points)| (*tick, points)).collect();
		let mut verdict = RestoreVerdict { ticks_judged: ack.len() as u64, ..RestoreVerdict::default() };
		for tick in &ack {
			let points = observed_by_tick.get(tick).map_or(&[][..], |points| points.as_slice());
			let judgement = self.judge_tick(*tick, points);
			verdict.points_expected = verdict.points_expected.saturating_add(judgement.present);
			verdict.points_restored = verdict.points_restored.saturating_add(judgement.observed);
			for mismatch in judgement.mismatches {
				verdict.push(mismatch);
			}
		}
		for (tick, points) in &observed_by_tick {
			if ack_set.contains(tick) {
				continue;
			}
			verdict.points_restored = verdict.points_restored.saturating_add(points.len() as u64);
			for point in *points {
				verdict.push(BackupMismatch { tick: *tick, series: point.series, row: point.row, kind: BackupMismatchKind::UnacknowledgedPoint, expected: None, observed: Some(point.value.clone()), expected_timestamp_ns: self.model.timestamp_ns(*tick, point.row), observed_timestamp_ns: Some(point.timestamp_ns) });
			}
		}
		verdict
	}

	/// Judge the whole lane: the online prefix check on `online`
	/// (the backup taken while ingest ran), the final restore
	/// verification on `restored` (the read-back of the store PAC-13's
	/// restore path published), and the caller's measured `timing`,
	/// combined into the verdict recorded on the run record.
	#[must_use]
	pub fn judge_lane(&self, acknowledged: &[u64], online: &[(u64, Vec<ObservedPoint>)], restored: &[(u64, Vec<ObservedPoint>)], timing: RecoveryTiming) -> BackupLaneVerdict {
		BackupLaneVerdict { prefix: self.judge_prefix(acknowledged, online), restore: self.judge_restore(acknowledged, restored), timing }
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::soak_workload::{LaneWeights, WorkloadParams};

	fn checker(series: usize, rows: usize, density_percent: u32) -> BackupLaneChecker {
		let params = WorkloadParams::new(series, rows, density_percent, LaneWeights { ingest: 1, point_lookup: 0, range_scan: 0 });
		BackupLaneChecker::new(WorkloadModel::new(42, params).expect("model"))
	}

	/// The faithful read-back of `ticks`: each tick grouped with the
	/// points a faithful store returns for it.
	fn faithful(checker: &BackupLaneChecker, ticks: &[u64]) -> Vec<(u64, Vec<ObservedPoint>)> {
		ticks.iter().map(|tick| (*tick, checker.expected_points(*tick))).collect()
	}

	#[test]
	fn a_backup_holding_an_exact_prefix_passes_and_records_the_cut() {
		let checker = checker(4, 8, 100);
		let acknowledged = [0, 1, 2, 3, 4, 5];
		// The backup was taken while ticks 3.. were still in flight:
		// it holds ticks 0..=2 complete and nothing later.
		let verdict = checker.judge_prefix(&acknowledged, &faithful(&checker, &[0, 1, 2]));
		assert!(verdict.passed(), "an exact prefix must pass: {:?}", verdict.mismatches);
		assert_eq!(verdict.cut_tick, Some(2), "the cut is the last complete tick held");
		assert_eq!(verdict.acknowledged_ticks, 6);
		assert_eq!(verdict.ticks_in_backup, 3);
		assert_eq!(verdict.points_in_backup, verdict.points_expected);
		assert!(verdict.points_expected > 0);
	}

	#[test]
	fn a_backup_holding_everything_acknowledged_passes_with_the_last_tick_as_cut() {
		let checker = checker(4, 8, 80);
		let acknowledged = [0, 1, 2];
		let verdict = checker.judge_prefix(&acknowledged, &faithful(&checker, &acknowledged));
		assert!(verdict.passed(), "{:?}", verdict.mismatches);
		assert_eq!(verdict.cut_tick, Some(2));
	}

	#[test]
	fn an_empty_backup_passes_vacuously_but_records_no_cut() {
		let checker = checker(4, 8, 100);
		let verdict = checker.judge_prefix(&[0, 1], &[]);
		assert!(verdict.passed(), "nothing held, nothing torn");
		assert_eq!(verdict.cut_tick, None, "a publisher can see no prefix was proven");
		assert_eq!(verdict.ticks_in_backup, 0);
	}

	#[test]
	fn a_partial_tick_inside_the_prefix_is_missing_points_and_breaks_the_cut() {
		let checker = checker(4, 8, 100);
		let acknowledged = [0, 1, 2];
		let mut observed = faithful(&checker, &[0, 1, 2]);
		observed[1].1.remove(0);
		let verdict = checker.judge_prefix(&acknowledged, &observed);
		assert!(!verdict.passed());
		assert_eq!(verdict.cut_tick, Some(0), "the cut stops at the last complete tick before the break");
		assert!(verdict.mismatches.iter().any(|m| m.kind == BackupMismatchKind::MissingPoint && m.tick == 1), "the dropped point is a missing point: {:?}", verdict.mismatches);
		assert!(verdict.mismatches.iter().any(|m| m.kind == BackupMismatchKind::BeyondPrefix && m.tick == 2), "a later tick present past the break is the tear: {:?}", verdict.mismatches);
	}

	#[test]
	fn a_gap_tick_is_a_torn_image() {
		let checker = checker(4, 8, 100);
		let acknowledged = [0, 1, 2];
		// Tick 1 absent entirely, tick 2 present: no moment of the
		// store looked like this.
		let verdict = checker.judge_prefix(&acknowledged, &faithful(&checker, &[0, 2]));
		assert!(!verdict.passed());
		assert_eq!(verdict.cut_tick, Some(0));
		assert!(verdict.mismatches.iter().any(|m| m.kind == BackupMismatchKind::MissingPoint && m.tick == 1));
		assert!(verdict.mismatches.iter().any(|m| m.kind == BackupMismatchKind::BeyondPrefix && m.tick == 2));
	}

	#[test]
	fn a_wrong_value_in_the_prefix_fails_exactly() {
		let checker = checker(4, 8, 100);
		let acknowledged = [0, 1];
		let mut observed = faithful(&checker, &[0, 1]);
		observed[0].1[0].value = GridValue::Float(-999.5);
		let verdict = checker.judge_prefix(&acknowledged, &observed);
		assert!(!verdict.passed());
		assert_eq!(verdict.cut_tick, None, "the first tick is broken, so no cut exists");
		assert!(verdict.mismatches.iter().any(|m| m.kind == BackupMismatchKind::WrongValue && m.tick == 0), "{:?}", verdict.mismatches);
	}

	#[test]
	fn a_point_from_an_unacknowledged_tick_is_reported_as_such() {
		let checker = checker(4, 8, 100);
		let acknowledged = [0, 1];
		let mut observed = faithful(&checker, &[0, 1]);
		observed.push((7, checker.expected_points(7)));
		let verdict = checker.judge_prefix(&acknowledged, &observed);
		assert!(!verdict.passed());
		assert!(verdict.mismatches.iter().all(|m| m.kind == BackupMismatchKind::UnacknowledgedPoint && m.tick == 7), "every tick-7 point is unacknowledged: {:?}", verdict.mismatches);
		assert_eq!(verdict.cut_tick, Some(1), "the acknowledged prefix itself is intact");
	}

	#[test]
	fn duplicate_and_unexpected_points_in_the_prefix_are_classified() {
		let checker = checker(4, 8, 100);
		let acknowledged = [0];
		let mut observed = faithful(&checker, &[0]);
		let duplicate = observed[0].1[0].clone();
		observed[0].1.push(duplicate);
		observed[0].1.push(ObservedPoint { series: 99, row: 0, timestamp_ns: 0, value: GridValue::Float(1.0) });
		let verdict = checker.judge_prefix(&acknowledged, &observed);
		assert!(!verdict.passed());
		assert!(verdict.mismatches.iter().any(|m| m.kind == BackupMismatchKind::DuplicatePoint), "{:?}", verdict.mismatches);
		assert!(verdict.mismatches.iter().any(|m| m.kind == BackupMismatchKind::UnexpectedPoint), "{:?}", verdict.mismatches);
	}

	#[test]
	fn a_faithful_restore_passes_the_final_verification() {
		let checker = checker(4, 8, 80);
		let acknowledged = [0, 1, 2, 3];
		let verdict = checker.judge_restore(&acknowledged, &faithful(&checker, &acknowledged));
		assert!(verdict.passed(), "{:?}", verdict.mismatches);
		assert_eq!(verdict.ticks_judged, 4);
		assert_eq!(verdict.points_restored, verdict.points_expected);
		assert!(verdict.points_expected > 0);
	}

	#[test]
	fn a_restore_that_loses_a_point_fails() {
		let checker = checker(4, 8, 100);
		let acknowledged = [0, 1, 2];
		let mut observed = faithful(&checker, &acknowledged);
		observed[2].1.remove(0);
		let verdict = checker.judge_restore(&acknowledged, &observed);
		assert!(!verdict.passed());
		assert_eq!(verdict.mismatch_count, 1);
		assert_eq!(verdict.mismatches[0].kind, BackupMismatchKind::MissingPoint);
		assert_eq!(verdict.mismatches[0].tick, 2);
	}

	#[test]
	fn a_restore_with_an_unacknowledged_tick_fails() {
		let checker = checker(4, 8, 100);
		let acknowledged = [0, 1];
		let mut observed = faithful(&checker, &acknowledged);
		observed.push((5, checker.expected_points(5)));
		let verdict = checker.judge_restore(&acknowledged, &observed);
		assert!(!verdict.passed());
		assert!(verdict.mismatches.iter().all(|m| m.kind == BackupMismatchKind::UnacknowledgedPoint && m.tick == 5));
	}

	#[test]
	fn judging_no_ticks_passes_vacuously_but_says_so() {
		let checker = checker(4, 8, 100);
		let verdict = checker.judge_restore(&[], &[]);
		assert!(verdict.passed());
		assert_eq!(verdict.ticks_judged, 0, "a publisher can see nothing was judged");
	}

	#[test]
	fn rpo_follows_evi19s_formula() {
		// I = 60 s, d_max = 4 s: RPO = max(60, 4) + 4 = 64 s — the
		// worked example on EVI-19's page.
		let timing = RecoveryTiming { backup_interval_ms: 60_000, backup_duration_ms: 4_000, restore_duration_ms: 9_000 };
		assert_eq!(timing.rpo_ms(), 64_000);
		// A pass slower than its interval hurts twice.
		let slow = RecoveryTiming { backup_interval_ms: 2_000, backup_duration_ms: 5_000, restore_duration_ms: 0 };
		assert_eq!(slow.rpo_ms(), 10_000);
	}

	#[test]
	fn rto_includes_taking_the_backup_it_restores_from() {
		let timing = RecoveryTiming { backup_interval_ms: 60_000, backup_duration_ms: 4_000, restore_duration_ms: 9_000 };
		assert_eq!(timing.rto_ms(), 13_000, "backup plus restore, per EVI-19's definition");
	}

	#[test]
	fn the_lane_verdict_combines_prefix_restore_and_timing() {
		let checker = checker(4, 8, 100);
		let acknowledged = [0, 1, 2, 3];
		let timing = RecoveryTiming { backup_interval_ms: 1_000, backup_duration_ms: 120, restore_duration_ms: 340 };
		let verdict = checker.judge_lane(&acknowledged, &faithful(&checker, &[0, 1]), &faithful(&checker, &acknowledged), timing);
		assert!(verdict.passed(), "{:?} / {:?}", verdict.prefix.mismatches, verdict.restore.mismatches);
		assert_eq!(verdict.prefix.cut_tick, Some(1));
		assert_eq!(verdict.rpo_ms(), 1_120);
		assert_eq!(verdict.rto_ms(), 460);
		let json = serde_json::to_string(&verdict).expect("serialize");
		let back: BackupLaneVerdict = serde_json::from_str(&json).expect("deserialize");
		assert_eq!(back, verdict, "the lane verdict carries no state outside its fields");
	}

	#[test]
	fn the_mismatch_list_is_capped_but_the_count_is_not() {
		let checker = checker(4, 8, 100);
		// Offer many unacknowledged ticks: every point is a mismatch,
		// far past the cap.
		let observed: Vec<(u64, Vec<ObservedPoint>)> = (100..(100 + MAX_REPORTED_BACKUP_MISMATCHES as u64 + 10)).map(|tick| (tick, checker.expected_points(tick))).collect();
		let total: u64 = observed.iter().map(|(_, points)| points.len() as u64).sum();
		let verdict = checker.judge_restore(&[], &observed);
		assert_eq!(verdict.mismatch_count, total);
		assert_eq!(verdict.mismatches.len(), MAX_REPORTED_BACKUP_MISMATCHES);
		assert!(verdict.truncated());
	}
}
