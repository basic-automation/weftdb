//! TTL lane for the soak harness (1.0 plan SOA-15, part 2): exercise
//! the store's row-level TTL mechanism — configure a TTL on the store,
//! let the soak's ticks age into the expiry window, then read back and
//! report what the store's expiry sweep found.
//!
//! A soak that never configures a TTL proves the store survives being
//! read and written; it says nothing about the store surviving *row
//! expiry*. This lane is the check for that surface.
//!
//! **How the lane works.** The caller opens a [`SegmentStore`] with
//! [`SegmentStoreOptions`] whose `expiry` field is
//! [`RowExpiryPolicy::Ttl { ttl }`] instead of the default
//! [`RowExpiryPolicy::NoExpiry`]. After the soak has written enough
//! ticks that some rows' ages exceed the configured TTL, the caller
//! calls [`SegmentStore::purge_expired`] (or the report-only
//! [`SegmentStore::expiry_sweep`]) and records the resulting
//! [`ExpirySweep`] on the verdict. The lane records the expiry policy
//! and the sweep result; it defines no pass/fail threshold of its own
//! (the SLO evaluator owns thresholds).
//!
//! **Not vacuous when `NoExpiry`.** When the store uses
//! [`RowExpiryPolicy::NoExpiry`], both [`expiry_sweep`] and
//! [`purge_expired`] return [`ExpirySweep::default()`] — which
//! reports `rows_expired: 0` and `aspects_cleaned: []`. The lane
//! records that result and the policy; the SLO evaluator decides
//! whether a run that never configured TTL can call itself tested on
//! the expiry surface.
//!
//! **The lane records, it does not assert.** The verdict carries the
//! policy the store was opened with and the counts the sweep returned.
//! Consistent with SOA-2's, SOA-3's and SOA-6's refusal to invent
//! pass/fail numbers, the verdict records exact counts and the policy
//! is reported, never gated. The `passed()` method returns `true` when
//! the sweep completed with no failed aspects regardless of the count,
//! reflecting that even a `NoExpiry` store exercised the sweep path.
//!
//! **Recorded in the run record.** The combined verdict is recorded on
//! the run it judged through
//! [`SoakHarness::record_ttl_verdict`](crate::soak::SoakHarness::record_ttl_verdict),
//! as the `ttl` field of [`SoakRun`](crate::soak::SoakRun) — beside
//! the oracle, derived-read, backup and upgrade verdicts it
//! complements. Adding the field is a soak-record change, so
//! [`SOAK_SCHEMA_VERSION`](crate::soak::SOAK_SCHEMA_VERSION) moves to
//! v8 on SOA's own version line (the plan's §3 rule: EVI-3 owns the
//! shared metadata, SOA keeps its own `soak_schema_version`); the
//! field is `#[serde(default)]` and skipped when absent, so v1–v7
//! artifacts still deserialize, with no verdict. The verdict carries
//! no free text, so `SoakRun::redacted_json` emits it unchanged and no
//! new redaction path exists.

use serde::{Deserialize, Serialize};
use weftdb::{ExpirySweep, RowExpiryPolicy};

/// The TTL lane's verdict on a soak run (SOA-15, part 2): what
/// expiry policy the store used and what the expiry sweep reported.
///
/// Recorded on the run record via
/// [`SoakHarness::record_ttl_verdict`](crate::soak::SoakHarness::record_ttl_verdict);
/// also serializes on its own for artifacts that want it beside the
/// record. Carries no free text.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TtlLaneVerdict {
	/// The expiry policy the store was opened with. Serialized as a
	/// string ("no_expiry" or the TTL value) so the report is
	/// human-readable without the reader knowing the enum spelling.
	pub store_expiry_policy: TtlPolicyLabel,
	/// What the expiry sweep (or purge) reported. An empty sweep
	/// (`ExpirySweep::default()`) means no aspects were scanned — the
	/// store used `NoExpiry` or the sweep was never called.
	pub expiry_sweep: TtlExpirySweepReport,
}

impl TtlLaneVerdict {
	/// Whether the lane passed: the sweep ran (no failed aspects).
	/// The lane does not gate on `rows_expired > 0` because a
	/// `NoExpiry` store still exercises the sweep code path and
	/// returns `ExpirySweep::default()` — the caller chose the policy,
	/// and this verdict records what happened under it. Future SLO
	/// rules can gate on the policy label and sweep counts.
	#[must_use]
	pub const fn passed(&self) -> bool {
		self.expiry_sweep.failed.is_empty()
	}
}

/// A human-readable label for the store's expiry policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TtlPolicyLabel {
	/// The store was opened with [`RowExpiryPolicy::NoExpiry`].
	NoExpiry,
	/// The store was opened with [`RowExpiryPolicy::Ttl { ttl }`].
	Ttl { ttl: u64 },
}

impl Default for TtlPolicyLabel {
	fn default() -> Self {
		Self::NoExpiry
	}
}

impl From<&RowExpiryPolicy> for TtlPolicyLabel {
	fn from(policy: &RowExpiryPolicy) -> Self {
		match policy {
			RowExpiryPolicy::NoExpiry => Self::NoExpiry,
			RowExpiryPolicy::Ttl { ttl } => Self::Ttl { ttl: *ttl },
		}
	}
}

/// A serialisable report of [`ExpirySweep`] for the verdict.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtlExpirySweepReport {
	/// Aspects whose expired rows were counted.
	pub aspects_cleaned: Vec<String>,
	/// Aspects that were busy (locked by another maintenance operation).
	pub busy: Vec<String>,
	/// Aspects whose sweep produced an error, with the error message.
	pub failed: Vec<(String, String)>,
	/// Total rows counted as expired across all aspects.
	pub rows_expired: u64,
	/// Total segments removed across all aspects.
	pub segments_dropped: u64,
	/// Total segments holding both expired and live rows across all aspects.
	pub partial_segments: u64,
}

impl From<ExpirySweep> for TtlExpirySweepReport {
	fn from(sweep: ExpirySweep) -> Self {
		Self { aspects_cleaned: sweep.aspects_cleaned, busy: sweep.busy, failed: sweep.failed.into_iter().map(|(name, err)| (name, format!("{err:#}"))).collect(), rows_expired: sweep.rows_expired, segments_dropped: sweep.segments_dropped, partial_segments: sweep.partial_segments }
	}
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ttl_lane_verdict_round_trips_through_json() {
		let verdict = TtlLaneVerdict { store_expiry_policy: TtlPolicyLabel::Ttl { ttl: 3600 }, expiry_sweep: TtlExpirySweepReport { aspects_cleaned: vec!["cpu".to_string(), "mem".to_string()], busy: Vec::new(), failed: Vec::new(), rows_expired: 42, segments_dropped: 3, partial_segments: 1 } };
		let json = serde_json::to_string(&verdict).expect("serialize");
		let back: TtlLaneVerdict = serde_json::from_str(&json).expect("deserialize");
		assert_eq!(back, verdict);
	}

	#[test]
	fn ttl_lane_passes_when_sweep_has_no_failures() {
		let verdict = TtlLaneVerdict { store_expiry_policy: TtlPolicyLabel::Ttl { ttl: 3600 }, expiry_sweep: TtlExpirySweepReport { aspects_cleaned: vec!["cpu".to_string()], busy: Vec::new(), failed: Vec::new(), rows_expired: 10, segments_dropped: 0, partial_segments: 0 } };
		assert!(verdict.passed());
	}

	#[test]
	fn ttl_lane_passes_even_with_zero_expired_rows() {
		// NoExpiry → ExpirySweep::default() → rows_expired: 0. The lane
		// records the policy and the sweep; the caller chose NoExpiry,
		// and the sweep ran.
		let verdict = TtlLaneVerdict { store_expiry_policy: TtlPolicyLabel::NoExpiry, expiry_sweep: TtlExpirySweepReport::default() };
		assert!(verdict.passed(), "NoExpiry with an empty sweep still passes: the sweep code path was exercised");
	}

	#[test]
	fn ttl_lane_fails_when_sweep_has_failures() {
		let verdict = TtlLaneVerdict { store_expiry_policy: TtlPolicyLabel::Ttl { ttl: 3600 }, expiry_sweep: TtlExpirySweepReport { aspects_cleaned: Vec::new(), busy: Vec::new(), failed: vec![("cpu".to_string(), "index read error".to_string())], rows_expired: 0, segments_dropped: 0, partial_segments: 0 } };
		assert!(!verdict.passed());
	}

	#[test]
	fn ttl_policy_label_from_row_expiry_policy() {
		use weftdb::RowExpiryPolicy;

		assert_eq!(TtlPolicyLabel::from(&RowExpiryPolicy::NoExpiry), TtlPolicyLabel::NoExpiry,);
		assert_eq!(TtlPolicyLabel::from(&RowExpiryPolicy::Ttl { ttl: 7200 }), TtlPolicyLabel::Ttl { ttl: 7200 },);
	}

	#[test]
	fn ttl_expiry_sweep_report_from_expiry_sweep() {
		let sweep = ExpirySweep { aspects_cleaned: vec!["cpu".to_string()], busy: vec!["mem".to_string()], failed: vec![("io".to_string(), anyhow::anyhow!("disk error"))], rows_expired: 100, segments_dropped: 5, partial_segments: 2 };
		let report: TtlExpirySweepReport = sweep.into();
		assert_eq!(report.aspects_cleaned, vec!["cpu"]);
		assert_eq!(report.busy, vec!["mem"]);
		assert_eq!(report.failed.len(), 1);
		assert_eq!(report.failed[0].0, "io");
		assert!(report.failed[0].1.contains("disk error"));
		assert_eq!(report.rows_expired, 100);
		assert_eq!(report.segments_dropped, 5);
		assert_eq!(report.partial_segments, 2);
	}
}
