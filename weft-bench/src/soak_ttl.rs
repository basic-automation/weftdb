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
use weft_physical_type::timestamp::TimeUnit;
use weftdb::{ExpirySweep, RowExpiryPolicy, SegmentStore, SegmentStoreOptions};

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

/// Drive a TTL lane end to end: open a temporary store with a configured TTL,
/// ingest rows whose timestamps straddle the expiry cutoff, run the expiry sweep
/// and purge, then record what happened.
///
/// **Store-side, not over HTTP.** Because `weft-bench` runs against a
/// [`NullServer`](crate::soak_null::NullServer) in the soak harness (no live process,
/// no port), this lane opens its own [`SegmentStore`] directly with
/// [`SegmentStoreOptions`] and configures the expiry policy there. The report section
/// flags this arrangement so a reader knows the lane exercises only the library's expiry
/// path, not the full daemon → HTTP → store chain.
///
/// # Errors
///
/// Returns an error when the store cannot be opened, rows cannot be ingested, or
/// the sweep or purge fails.
pub async fn run_ttl_lane(ttl_secs: u64, now_ns: i64) -> anyhow::Result<TtlLaneVerdict> {
	use std::str::FromStr;

	use bigdecimal::BigDecimal;
	use weft_physical_type::{AspectSchema, PhysicalType};
	use weftdb::SeriesId;

	let dir = tempfile::tempdir().map_err(|e| anyhow::anyhow!("failed to create temp dir for TTL lane: {e}"))?;
	let opts = SegmentStoreOptions::default().with_expiry(RowExpiryPolicy::Ttl { ttl: ttl_secs });
	let store = SegmentStore::open_with_options(dir.path(), opts).await?;

	let aspect = "ttl_lane_temp";
	let schema = AspectSchema::new(PhysicalType::F64, BigDecimal::from_str("0").unwrap(), TimeUnit::Seconds);
	store.declare(aspect, &schema).await?;

	let ttl_signed = i64::try_from(ttl_secs).unwrap_or(i64::MAX);
	let now_unit = now_ns / 1_000_000_000;
	let cutoff = now_unit.saturating_sub(ttl_signed);

	// Ingest rows: some before cutoff (will be expired), some at and after (live).
	let expired_ts: Vec<i64> = if cutoff - 4 >= i64::MIN { vec![cutoff - 4, cutoff - 2] } else { Vec::new() };
	let live_ts: Vec<i64> = vec![cutoff, cutoff + 2];
	let all_ts: Vec<i64> = expired_ts.iter().copied().chain(live_ts.iter().copied()).collect();
	let all_vs: Vec<BigDecimal> = all_ts.iter().map(|t| BigDecimal::from(*t as i32)).collect();

	store.ingest(aspect, &weftdb::IngestBatch::dense("s1", all_ts, all_vs)).await?;

	// Read back through the expiry-aware path BEFORE anything is deleted. The expired
	// rows are still physically on disk here, so this is the check that the read
	// filter hides them; a purge-first order would pass the same read even if the
	// filter regressed, because the rows would be gone for the wrong reason.
	anyhow::ensure!(!expired_ts.is_empty(), "TTL lane: the fixture produced no expired rows (cutoff {cutoff}), so the read-filter check would be vacuous");
	let window_end = now_unit.saturating_add(60);
	let (visible, _) = store.read_time_range_of_with_expiry(aspect, SeriesId::UNTAGGED, cutoff.saturating_sub(60), window_end, now_ns).await?;
	if visible.len() != live_ts.len() {
		return Err(anyhow::anyhow!("TTL lane: the expiry-aware read returned {} rows, expected the {} live rows, with {} expired rows hidden by the read filter", visible.len(), live_ts.len(), expired_ts.len()));
	}
	if let Some(t) = visible.iter().find(|t| **t < cutoff) {
		return Err(anyhow::anyhow!("TTL lane: the expiry-aware read returned {t}, at or before the expiry cutoff {cutoff}"));
	}

	// Run expiry sweep (report-only).
	store.expiry_sweep(now_ns).await?;

	// Run purge (physical deletion).
	let purge = store.purge_expired(now_ns).await?;

	// The same read after the purge returns the same live set, now because the
	// expired rows were unlinked rather than because the filter hid them.
	let (after_purge, _) = store.read_time_range_of_with_expiry(aspect, SeriesId::UNTAGGED, cutoff.saturating_sub(60), window_end, now_ns).await?;
	if after_purge.len() != live_ts.len() {
		return Err(anyhow::anyhow!("TTL lane: after purge_expired the read returned {} rows, expected the {} live rows", after_purge.len(), live_ts.len()));
	}

	let rowexp = RowExpiryPolicy::Ttl { ttl: ttl_secs };
	let policy_label = TtlPolicyLabel::from(&rowexp);

	// Build the combined report from purge (which includes the drops).
	let report = TtlExpirySweepReport { aspects_cleaned: purge.aspects_cleaned.clone(), busy: purge.busy.clone(), failed: purge.failed.into_iter().map(|(n, e)| (n, format!("{e:#}"))).collect(), rows_expired: purge.rows_expired, segments_dropped: purge.segments_dropped, partial_segments: purge.partial_segments };

	drop(store);
	dir.close().map_err(|e| anyhow::anyhow!("failed to close temp dir: {e}"))?;

	Ok(TtlLaneVerdict { store_expiry_policy: policy_label, expiry_sweep: report })
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
	fn ttl_expiry_sweep_report_from_default_expiry_sweep() {
		// Verifies the From impl: a default ExpirySweep (NoExpiry) maps to
		// an empty TtlExpirySweepReport.
		let sweep = ExpirySweep::default();
		let report: TtlExpirySweepReport = sweep.into();
		assert!(report.aspects_cleaned.is_empty());
		assert!(report.busy.is_empty());
		assert!(report.failed.is_empty());
		assert_eq!(report.rows_expired, 0);
		assert_eq!(report.segments_dropped, 0);
		assert_eq!(report.partial_segments, 0);
	}

	/// The lane is not vacuous: `NoExpiry` policy produces 0 expired rows,
	/// while `Ttl` with rows past the cutoff produces > 0.
	#[tokio::test]
	async fn ttl_lane_is_not_vacuous_expiry_vs_no_expiry() {
		let now_ns = 100_000_000_000i64; // T = 100s

		// Run with TTL = 10s — rows at t < 90 are expired.
		let verdict = run_ttl_lane(10, now_ns).await.expect("ttl lane with ttl=10");
		assert!(verdict.expiry_sweep.rows_expired > 0, "TTL lane with rows before cutoff must report >0 expired rows (got {}); if this fails the lane is vacuous", verdict.expiry_sweep.rows_expired);

		// Run with NoExpiry — should report 0 expired.
		// We bypass run_ttl_lane because it always sets TTL; open directly.
		use weftdb::SegmentStoreOptions;
		let dir = tempfile::tempdir().expect("tempdir");
		let opts = SegmentStoreOptions::default().with_expiry(RowExpiryPolicy::NoExpiry);
		let store = SegmentStore::open_with_options(dir.path(), opts).await.expect("open");
		use std::str::FromStr;

		use bigdecimal::BigDecimal;
		use weft_physical_type::{AspectSchema, PhysicalType};
		store.declare("noexp_test", &AspectSchema::new(PhysicalType::F64, BigDecimal::from_str("0").unwrap(), TimeUnit::Seconds)).await.expect("declare");
		store.ingest("noexp_test", &weftdb::IngestBatch::dense("s1", vec![50, 95], vec![BigDecimal::from(1), BigDecimal::from(2)])).await.expect("ingest");
		let sweep = store.expiry_sweep(now_ns).await.expect("sweep");
		assert_eq!(sweep.rows_expired, 0, "NoExpiry policy must produce 0 expired rows");
		drop(store);
		dir.close().expect("close");
	}

	/// Real store round-trip: open store with TTL, ingest, expire, sweep, purge,
	/// read back — assert exact counts of expired rows and surviving rows.
	#[tokio::test]
	async fn ttl_lane_store_round_trip_exact_counts() {
		let now_ns = 100_000_000_000i64; // T = 100s
		let ttl_secs = 10u64;

		use std::str::FromStr;

		use bigdecimal::BigDecimal;
		use weft_physical_type::{AspectSchema, PhysicalType};
		use weftdb::{IngestBatch, SeriesId};

		let dir = tempfile::tempdir().expect("tempdir");
		let opts = SegmentStoreOptions::default().with_expiry(RowExpiryPolicy::Ttl { ttl: ttl_secs });
		let store = SegmentStore::open_with_options(dir.path(), opts).await.expect("open");

		store.declare("roundtrip", &AspectSchema::new(PhysicalType::F64, BigDecimal::from_str("0").unwrap(), TimeUnit::Seconds)).await.expect("declare");

		// t=85 (expired: age 15 > 10), t=95 (live: age 5 ≤ 10), t=100 (live: age 0)
		store.ingest("roundtrip", &IngestBatch::dense("s1", vec![85, 95, 100], vec![BigDecimal::from(1), BigDecimal::from(2), BigDecimal::from(3)])).await.expect("ingest");

		// Sweep: 1 row expired (t=85)
		let sweep = store.expiry_sweep(now_ns).await.expect("sweep");
		assert_eq!(sweep.rows_expired, 1, "exactly one row is expired (t=85)");

		// Purge
		let _purge = store.purge_expired(now_ns).await.expect("purge");

		// Read back: only live rows visible
		let (ts, vs) = store.read_time_range_of_with_expiry("roundtrip", SeriesId::UNTAGGED, 0, 200, now_ns).await.expect("read");
		assert_eq!(ts, vec![95i64, 100], "live rows survive: t=95 and t=100 visible");
		assert_eq!(vs, vec![Some(BigDecimal::from(2)), Some(BigDecimal::from(3))], "live row values intact");

		drop(store);
		dir.close().expect("close");
	}
}
