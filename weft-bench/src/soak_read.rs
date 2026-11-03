//! Derived-read checks for the soak harness (1.0 plan SOA-10, W6).
//!
//! SOA-6's oracle ([`crate::soak_oracle`]) judges a *raw* read-back: the
//! points a caller read, compared cell by cell against the workload
//! model. But a soak that only ever reads raw points back never exercises
//! the reads a real client actually depends on — the reads whose result
//! the server *derives* from what it stored. The stored downsample read
//! (`GET /api/v1/storage/{aspect}/downsample`) is that read: the server
//! folds the aspect's sealed segments into grid-aligned buckets and
//! reports, per bucket, a count and the requested reductions. A server
//! that seals faithfully but reduces wrongly — a bucket that drops a
//! segment, an average divided by the wrong count — passes the oracle
//! and fails its users. This module is the check for that surface, the
//! endpoint-level companion SOA-5 and SOA-6 both reserved to SOA-10.
//!
//! **What is checked.** One derived read per judged tick set: the stored
//! downsample at one-second buckets with the `count`, `min`, `max`,
//! `sum` and `avg` reductions, over the window the judged ticks span.
//! Grid rows are one second apart and land on whole seconds, so each
//! row's bucket is exactly that row's timestamp, and the expected bucket
//! for a row aggregates — across every series — the values the server
//! sealed at that timestamp. Those values are the model's *numeric*
//! cells only: SOA-5 records that the ingest route seals the numeric
//! subset of the named field into the aspect's `f64` schema, so boolean
//! and text cells never reach the store and must not reach the expected
//! bucket either. Integer and unsigned cells enter as the `f64` the
//! server seals; at the magnitudes the workload generator produces
//! (two-decimal signals under 100) every one of those conversions is
//! exact, so — as in the oracle — the comparison is equality, with no
//! tolerance invented.
//!
//! **The caller performs the read.** Like the oracle core, this module
//! performs no read itself: the caller runs the downsample request
//! against its server, maps each returned bucket into a
//! [`DerivedBucket`] (bucket start as nanoseconds, the response's
//! `count`, and its `min`/`max`/`sum`/`avg` entries), and hands the list
//! to [`DerivedReadChecker::judge`]. Which client, which endpoint
//! spelling (GET or POST, JSON or another format that carries the same
//! five numbers) is the caller's business — the slices that own how a
//! soak is launched are later ones, on the SOA-5/SOA-6 precedent.
//!
//! **Exact comparison, classified.** A bucket that differs is one of
//! [`DerivedMismatchKind`]: `MissingBucket` (a row with sealed points
//! produced no bucket), `UnexpectedBucket` (a bucket where no judged
//! row sealed anything, or outside the judged rows entirely),
//! `DuplicateBucket` (the same bucket reported twice), or the first
//! reduction that disagrees — `WrongCount`, `WrongMin`, `WrongMax`,
//! `WrongSum`, `WrongAvg`, checked in that order because a wrong count
//! poisons every reduction after it and a bucket is one derived value,
//! reported once (the oracle's one-mismatch-per-cell rule, at bucket
//! granularity). The sum is accumulated in the order the server seals:
//! ticks in order, and within a tick's bucket, series in grid order —
//! the seal order of the workload body — so an exact sum comparison is
//! attainable rather than hostage to float reassociation.
//!
//! **The verdict is a record, not a threshold.** A
//! [`DerivedReadVerdict`] counts what was judged (ticks, buckets
//! checked, buckets the model filled, points the buckets counted) and
//! lists the mismatches, up to [`MAX_REPORTED_DERIVED_MISMATCHES`]; the
//! total mismatch count is kept even when the list is capped.
//! [`DerivedReadVerdict::passed`] is exact — zero mismatches — not a
//! rate: consistent with SOA-2's, SOA-3's and SOA-6's refusal to invent
//! pass/fail numbers. A verdict that judged nothing passes vacuously,
//! so publishers check [`DerivedReadVerdict::ticks_judged`] beside it
//! (the SOA-12 skipped-check rule).
//!
//! **Recorded in the run record.** A verdict is recorded on the run it
//! judged through
//! [`SoakHarness::record_derived_verdict`](crate::soak::SoakHarness::record_derived_verdict),
//! as the `derived` field of [`SoakRun`](crate::soak::SoakRun) — beside
//! the oracle verdict it complements. Adding the field is a soak-record
//! change, so [`SOAK_SCHEMA_VERSION`](crate::soak::SOAK_SCHEMA_VERSION)
//! moves to v5 on SOA's own version line (the plan's §3 rule: EVI-3 owns
//! the shared metadata, SOA keeps its own `soak_schema_version`); the
//! field is `#[serde(default)]` and skipped when absent, so v1–v4
//! artifacts still deserialize, with no verdict. The verdict carries no
//! free text, so `SoakRun::redacted_json` emits it unchanged and no new
//! redaction path exists.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::soak_workload::{GridValue, WorkloadModel};

/// Cap on the mismatches a [`DerivedReadVerdict`] lists. The total is
/// always counted in [`DerivedReadVerdict::mismatch_count`]; the cap
/// only bounds the artifact a badly broken run produces, on the same
/// reasoning as the oracle's [`MAX_REPORTED_MISMATCHES`](crate::soak_oracle::MAX_REPORTED_MISMATCHES).
pub const MAX_REPORTED_DERIVED_MISMATCHES: usize = 1024;

/// One bucket of a derived read: the reductions the server derived for
/// every point it holds at one grid-aligned bucket start.
///
/// Produced two ways that must agree: from the workload model by
/// [`DerivedReadChecker::expected_buckets`] (what a faithful server
/// derives), or by the caller from a live server's downsample response
/// (what the server actually derived). `timestamp_ns` is the bucket
/// start in nanoseconds — for the one-second buckets this check uses,
/// the judged row's own timestamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DerivedBucket {
	/// Bucket start, in nanoseconds.
	pub timestamp_ns: i64,
	/// Points the bucket counted.
	pub count: u64,
	/// Smallest value in the bucket.
	pub min: f64,
	/// Largest value in the bucket.
	pub max: f64,
	/// Sum of the bucket's values, accumulated in seal order (see the
	/// module doc for why the order is part of the check).
	pub sum: f64,
	/// Mean of the bucket's values (`sum / count` on the expected side;
	/// the server's own reported `avg` on the observed side, so a server
	/// that sums right but averages wrong is still caught).
	pub avg: f64,
}

/// How one bucket of a derived read differs from the expected bucket.
/// See the module doc for the classification rules and the order the
/// `Wrong*` kinds are checked in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DerivedMismatchKind {
	/// A row with sealed points produced no bucket.
	MissingBucket,
	/// A bucket where no judged row sealed anything, or outside the
	/// judged rows entirely.
	UnexpectedBucket,
	/// The same bucket was reported more than once.
	DuplicateBucket,
	/// The bucket counted a different number of points.
	WrongCount,
	/// The bucket's `min` differs.
	WrongMin,
	/// The bucket's `max` differs.
	WrongMax,
	/// The bucket's `sum` differs.
	WrongSum,
	/// The bucket's `avg` differs (with count and sum in agreement, so
	/// the server's own averaging is what is wrong).
	WrongAvg,
}

/// One bucket where a derived read disagreed with the model.
///
/// Carries both sides of the comparison — the expected and observed
/// buckets in full — so a reviewer can see every reduction without
/// re-running anything. `observed` is `None` exactly for
/// [`DerivedMismatchKind::MissingBucket`], and `expected` is `None`
/// exactly for an out-of-model [`DerivedMismatchKind::UnexpectedBucket`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DerivedMismatch {
	/// Bucket start the mismatch belongs to, in nanoseconds.
	pub timestamp_ns: i64,
	/// How the bucket differs.
	pub kind: DerivedMismatchKind,
	/// Bucket the model derives, or `None` when the bucket is outside
	/// the judged rows.
	pub expected: Option<DerivedBucket>,
	/// Bucket the server derived, or `None` when it derived none.
	pub observed: Option<DerivedBucket>,
}

/// The derived-read verdict on the ticks it judged: counts of what was
/// checked and the mismatches found.
///
/// Recorded on the run record via
/// [`SoakHarness::record_derived_verdict`](crate::soak::SoakHarness::record_derived_verdict);
/// also serializes on its own for artifacts that want it beside the
/// record. Carries no free text.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DerivedReadVerdict {
	/// Ticks judged.
	pub ticks_judged: u64,
	/// Buckets examined: every bucket the model filled across the judged
	/// ticks, plus every bucket the server reported.
	pub buckets_checked: u64,
	/// Buckets the model filled across the judged ticks (the buckets a
	/// faithful derived read must return).
	pub buckets_present: u64,
	/// Points the expected buckets counted across the judged ticks.
	pub points_counted: u64,
	/// Total mismatches found, whether listed in [`Self::mismatches`] or
	/// cut off by the cap.
	pub mismatch_count: u64,
	/// The mismatches, in judgement order (observed buckets in the order
	/// supplied, then missing buckets in timestamp order), capped at
	/// [`MAX_REPORTED_DERIVED_MISMATCHES`].
	pub mismatches: Vec<DerivedMismatch>,
}

impl DerivedReadVerdict {
	/// Whether the judged ticks' derived read matched the model exactly:
	/// no mismatch of any kind. A verdict that judged nothing has no
	/// mismatches and so passes vacuously — callers publishing a run
	/// check [`Self::ticks_judged`] beside this, the way a skipped flush
	/// check is not a passed one (SOA-12).
	#[must_use]
	pub const fn passed(&self) -> bool {
		self.mismatch_count == 0
	}

	/// Whether the mismatch list was capped: the total in
	/// [`Self::mismatch_count`] is larger than the list kept.
	#[must_use]
	pub fn truncated(&self) -> bool {
		#[allow(clippy::cast_possible_truncation)]
		let kept = self.mismatches.len() as u64;
		self.mismatch_count > kept
	}

	/// Record one mismatch, keeping the total count even past the cap.
	fn push(&mut self, mismatch: DerivedMismatch) {
		self.mismatch_count += 1;
		if self.mismatches.len() < MAX_REPORTED_DERIVED_MISMATCHES {
			self.mismatches.push(mismatch);
		}
	}
}

/// The value a grid cell contributes to a derived bucket: the `f64` the
/// server seals for it, or `None` for the formats it does not seal.
///
/// SOA-5 records that the ingest route seals the numeric subset of the
/// named field into the aspect's `f64` schema, so float, integer and
/// unsigned cells contribute and boolean and text cells do not. The
/// conversions are exact at the magnitudes the workload generator
/// produces (two-decimal signals under 100, and their truncations).
fn sealed_value(value: &GridValue) -> Option<f64> {
	match value {
		GridValue::Float(value) => Some(*value),
		#[allow(clippy::cast_precision_loss)]
		GridValue::Integer(value) => Some(*value as f64),
		#[allow(clippy::cast_precision_loss)]
		GridValue::Unsigned(value) => Some(*value as f64),
		GridValue::Boolean(_) | GridValue::Text(_) => None,
	}
}

/// Accumulator for one expected bucket, folded in seal order.
#[derive(Debug, Clone, Copy)]
struct BucketAcc {
	count: u64,
	min: f64,
	max: f64,
	sum: f64,
}

impl BucketAcc {
	fn add(&mut self, value: f64) {
		self.count += 1;
		self.min = self.min.min(value);
		self.max = self.max.max(value);
		self.sum += value;
	}

	fn finish(self, timestamp_ns: i64) -> DerivedBucket {
		#[allow(clippy::cast_precision_loss)]
		let avg = self.sum / self.count as f64;
		DerivedBucket { timestamp_ns, count: self.count, min: self.min, max: self.max, sum: self.sum, avg }
	}
}

/// The derived-read checker: derives the expected buckets of a judged
/// tick set from the workload model and judges a server's observed
/// buckets against them.
///
/// Borrows nothing and performs no read — the model is cloned at
/// construction so the checker outlives the caller's handle on it, the
/// same shape as [`SoakOracle`](crate::soak_oracle::SoakOracle).
#[derive(Debug, Clone)]
pub struct DerivedReadChecker {
	model: WorkloadModel,
}

impl DerivedReadChecker {
	/// Build a checker over `model` (SOA-4's workload model, derived
	/// from the soak config's seed).
	#[must_use]
	pub const fn new(model: WorkloadModel) -> Self {
		Self { model }
	}

	/// The expected buckets of `ticks`: one per row that sealed at least
	/// one numeric point, in timestamp order.
	///
	/// Ticks are judged in ascending order with duplicates removed — a
	/// tick judged twice would double-count its points, the duplicated
	/// seal this harness exists to catch rather than commit. Rows whose
	/// cells are all absent, or all in the formats the server does not
	/// seal, fill no bucket and produce none.
	#[must_use]
	pub fn expected_buckets(&self, ticks: &[u64]) -> Vec<DerivedBucket> {
		let mut ticks: Vec<u64> = ticks.to_vec();
		ticks.sort_unstable();
		ticks.dedup();
		let mut buckets: BTreeMap<i64, BucketAcc> = BTreeMap::new();
		for tick in ticks {
			let step = self.model.step(tick);
			for series in 0..step.grid.series {
				for row in 0..step.grid.rows {
					let Some(value) = step.grid.cell(series, row) else {
						continue;
					};
					let Some(value) = sealed_value(value) else {
						continue;
					};
					let timestamp_ns = self.model.timestamp_ns(tick, row);
					buckets.entry(timestamp_ns).or_insert(BucketAcc { count: 0, min: f64::INFINITY, max: f64::NEG_INFINITY, sum: 0.0 }).add(value);
				}
			}
		}
		buckets.into_iter().map(|(timestamp_ns, acc)| acc.finish(timestamp_ns)).collect()
	}

	/// Judge `observed` — the buckets a derived read of the judged
	/// ticks' window returned, mapped by the caller from the server's
	/// response — against the buckets the model derives for `ticks`.
	///
	/// The comparison is exact (see the module doc). Each observed
	/// bucket is matched to the expected bucket at its timestamp;
	/// expected buckets no observation names are then reported missing,
	/// in timestamp order.
	#[must_use]
	pub fn judge(&self, ticks: &[u64], observed: &[DerivedBucket]) -> DerivedReadVerdict {
		let mut ticks_sorted: Vec<u64> = ticks.to_vec();
		ticks_sorted.sort_unstable();
		ticks_sorted.dedup();
		let expected = self.expected_buckets(&ticks_sorted);
		let expected_by_ts: BTreeMap<i64, &DerivedBucket> = expected.iter().map(|bucket| (bucket.timestamp_ns, bucket)).collect();
		let mut verdict = DerivedReadVerdict { ticks_judged: ticks_sorted.len() as u64, buckets_checked: 0, buckets_present: expected.len() as u64, points_counted: expected.iter().map(|bucket| bucket.count).sum(), mismatch_count: 0, mismatches: Vec::new() };
		let mut seen: BTreeSet<i64> = BTreeSet::new();
		for bucket in observed {
			verdict.buckets_checked += 1;
			if !seen.insert(bucket.timestamp_ns) {
				verdict.push(DerivedMismatch { timestamp_ns: bucket.timestamp_ns, kind: DerivedMismatchKind::DuplicateBucket, expected: expected_by_ts.get(&bucket.timestamp_ns).map(|bucket| (*bucket).clone()), observed: Some(bucket.clone()) });
				continue;
			}
			let Some(expected_bucket) = expected_by_ts.get(&bucket.timestamp_ns) else {
				verdict.push(DerivedMismatch { timestamp_ns: bucket.timestamp_ns, kind: DerivedMismatchKind::UnexpectedBucket, expected: None, observed: Some(bucket.clone()) });
				continue;
			};
			let kind = if bucket.count != expected_bucket.count {
				Some(DerivedMismatchKind::WrongCount)
			} else if bucket.min != expected_bucket.min {
				Some(DerivedMismatchKind::WrongMin)
			} else if bucket.max != expected_bucket.max {
				Some(DerivedMismatchKind::WrongMax)
			} else if bucket.sum != expected_bucket.sum {
				Some(DerivedMismatchKind::WrongSum)
			} else if bucket.avg != expected_bucket.avg {
				Some(DerivedMismatchKind::WrongAvg)
			} else {
				None
			};
			if let Some(kind) = kind {
				verdict.push(DerivedMismatch { timestamp_ns: bucket.timestamp_ns, kind, expected: Some((*expected_bucket).clone()), observed: Some(bucket.clone()) });
			}
		}
		for bucket in &expected {
			if !seen.contains(&bucket.timestamp_ns) {
				verdict.push(DerivedMismatch { timestamp_ns: bucket.timestamp_ns, kind: DerivedMismatchKind::MissingBucket, expected: Some(bucket.clone()), observed: None });
			}
		}
		verdict.buckets_checked += verdict.buckets_present;
		verdict
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::soak_workload::{LaneWeights, WorkloadParams};

	fn checker(series: usize, rows: usize, density_percent: u32) -> DerivedReadChecker {
		let params = WorkloadParams::new(series, rows, density_percent, LaneWeights { ingest: 1, point_lookup: 0, range_scan: 0 });
		DerivedReadChecker::new(WorkloadModel::new(42, params).expect("model"))
	}

	#[test]
	fn expected_buckets_count_only_the_sealed_numeric_cells() {
		// Five series cycle through all five formats; only the float,
		// integer and unsigned columns (series 0, 1, 2) seal. At full
		// density every row's bucket therefore counts exactly 3 points.
		let checker = checker(5, 4, 100);
		let buckets = checker.expected_buckets(&[0]);
		assert_eq!(buckets.len(), 4, "one bucket per row");
		for bucket in &buckets {
			assert_eq!(bucket.count, 3, "only the numeric series seal: {bucket:?}");
			assert!(bucket.min <= bucket.avg && bucket.avg <= bucket.max, "reductions agree: {bucket:?}");
			assert_eq!(bucket.avg, bucket.sum / 3.0, "avg is sum over count");
		}
		let model = checker.model.clone();
		assert_eq!(buckets[0].timestamp_ns, model.timestamp_ns(0, 0), "the bucket is the row's own timestamp");
	}

	#[test]
	fn a_single_series_bucket_reduces_to_its_one_value() {
		// One float series at full density: every bucket holds exactly
		// the row's one sealed value, so all four reductions agree.
		let checker = checker(1, 3, 100);
		let buckets = checker.expected_buckets(&[0]);
		assert_eq!(buckets.len(), 3);
		for bucket in &buckets {
			assert_eq!(bucket.count, 1);
			assert_eq!(bucket.min, bucket.max);
			assert_eq!(bucket.sum, bucket.min);
			assert_eq!(bucket.avg, bucket.min);
		}
	}

	#[test]
	fn a_faithful_derived_read_passes() {
		let checker = checker(8, 16, 80);
		let ticks = [0, 1, 2, 3];
		let observed = checker.expected_buckets(&ticks);
		let verdict = checker.judge(&ticks, &observed);
		assert!(verdict.passed(), "a faithful read has no mismatches: {:?}", verdict.mismatches);
		assert_eq!(verdict.ticks_judged, 4);
		assert_eq!(verdict.buckets_present, observed.len() as u64);
		assert!(verdict.points_counted > 0, "the judged ticks sealed points");
		assert!(!verdict.truncated());
	}

	#[test]
	fn judging_no_ticks_passes_vacuously_but_says_so() {
		let checker = checker(8, 16, 80);
		let verdict = checker.judge(&[], &[]);
		assert!(verdict.passed());
		assert_eq!(verdict.ticks_judged, 0, "a publisher can see nothing was judged");
		assert_eq!(verdict.buckets_present, 0);
	}

	#[test]
	fn a_dropped_bucket_is_missing() {
		let checker = checker(8, 16, 100);
		let ticks = [0, 1];
		let mut observed = checker.expected_buckets(&ticks);
		let dropped = observed.remove(0);
		let verdict = checker.judge(&ticks, &observed);
		assert!(!verdict.passed());
		assert_eq!(verdict.mismatch_count, 1);
		assert_eq!(verdict.mismatches[0].kind, DerivedMismatchKind::MissingBucket);
		assert_eq!(verdict.mismatches[0].timestamp_ns, dropped.timestamp_ns);
		assert!(verdict.mismatches[0].observed.is_none());
	}

	#[test]
	fn an_extra_bucket_is_unexpected() {
		let checker = checker(8, 16, 100);
		let ticks = [0];
		let mut observed = checker.expected_buckets(&ticks);
		observed.push(DerivedBucket { timestamp_ns: 1, count: 1, min: 0.0, max: 0.0, sum: 0.0, avg: 0.0 });
		let verdict = checker.judge(&ticks, &observed);
		assert_eq!(verdict.mismatch_count, 1);
		assert_eq!(verdict.mismatches[0].kind, DerivedMismatchKind::UnexpectedBucket);
		assert!(verdict.mismatches[0].expected.is_none());
	}

	#[test]
	fn a_repeated_bucket_is_a_duplicate() {
		let checker = checker(8, 16, 100);
		let ticks = [0];
		let mut observed = checker.expected_buckets(&ticks);
		observed.push(observed[0].clone());
		let verdict = checker.judge(&ticks, &observed);
		assert_eq!(verdict.mismatch_count, 1);
		assert_eq!(verdict.mismatches[0].kind, DerivedMismatchKind::DuplicateBucket);
	}

	#[test]
	fn each_wrong_reduction_is_classified_in_order() {
		let checker = checker(8, 16, 100);
		let ticks = [0];
		let expected = checker.expected_buckets(&ticks);
		let corrupt = |mut bucket: DerivedBucket, kind: DerivedMismatchKind| {
			match kind {
				DerivedMismatchKind::WrongCount => bucket.count += 1,
				DerivedMismatchKind::WrongMin => bucket.min -= 1.0,
				DerivedMismatchKind::WrongMax => bucket.max += 1.0,
				DerivedMismatchKind::WrongSum => bucket.sum += 1.0,
				DerivedMismatchKind::WrongAvg => bucket.avg += 1.0,
				_ => unreachable!(),
			}
			bucket
		};
		for kind in [DerivedMismatchKind::WrongCount, DerivedMismatchKind::WrongMin, DerivedMismatchKind::WrongMax, DerivedMismatchKind::WrongSum, DerivedMismatchKind::WrongAvg] {
			let mut observed = expected.clone();
			observed[0] = corrupt(observed[0].clone(), kind);
			let verdict = checker.judge(&ticks, &observed);
			assert_eq!(verdict.mismatch_count, 1, "{kind:?} produces exactly one mismatch");
			assert_eq!(verdict.mismatches[0].kind, kind, "the first differing reduction is reported");
		}
	}

	#[test]
	fn duplicate_ticks_are_judged_once() {
		let checker = checker(8, 16, 100);
		let once = checker.expected_buckets(&[0, 1]);
		let twice = checker.expected_buckets(&[1, 0, 1, 0]);
		assert_eq!(once, twice, "tick order and repetition do not change the expected buckets");
	}

	#[test]
	fn the_mismatch_list_is_capped_but_the_count_is_not() {
		let checker = checker(8, 16, 100);
		// Judge no ticks but offer buckets at every row timestamp of
		// many ticks: every one is unexpected, far past the cap.
		let model = &checker.model;
		let observed: Vec<DerivedBucket> = (0..(MAX_REPORTED_DERIVED_MISMATCHES as u64 + 10)).map(|tick| DerivedBucket { timestamp_ns: model.timestamp_ns(tick, 0), count: 1, min: 0.0, max: 0.0, sum: 0.0, avg: 0.0 }).collect();
		let verdict = checker.judge(&[], &observed);
		assert_eq!(verdict.mismatch_count, observed.len() as u64);
		assert_eq!(verdict.mismatches.len(), MAX_REPORTED_DERIVED_MISMATCHES);
		assert!(verdict.truncated());
	}

	#[test]
	fn the_verdict_round_trips_through_json() {
		let checker = checker(8, 16, 100);
		let ticks = [0];
		let mut observed = checker.expected_buckets(&ticks);
		observed[0].sum += 1.0;
		let verdict = checker.judge(&ticks, &observed);
		let json = serde_json::to_string(&verdict).expect("serialize");
		let back: DerivedReadVerdict = serde_json::from_str(&json).expect("deserialize");
		assert_eq!(back, verdict, "the verdict carries no state outside its fields");
	}
}
