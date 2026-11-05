//! Window digests for the soak harness (1.0 plan SOA-7, W5).
//!
//! SOA-6's oracle judges a tick exactly, cell by cell — the right check,
//! and an expensive shape for a 24 h run: judging every tick means
//! keeping (or re-reading) every observation until the verdict is
//! recorded. A window digest is the compact form of the same evidence.
//! A *window* is a contiguous range of ticks; its digest is a SHA-256
//! over the canonical encoding of every point in the window — tick,
//! series, row, timestamp, and the value in its format-exact wire form
//! ([`GridValue::encode_field`]) — computed two ways that must agree:
//!
//! * **expected**, from the workload model alone
//!   ([`expected_window_digest`], or
//!   [`SoakOracle::window_digest`](crate::soak_oracle::SoakOracle::window_digest)):
//!   what a faithful run wrote in that window;
//! * **observed**, from the points a read-back supplied for the window
//!   ([`observed_window_digest`]): what the server actually kept.
//!
//! When the two digests of a window match, the window's read-back is
//! exactly the model's traffic — the same statement the oracle's
//! per-cell judgement makes for those ticks, at the cost of one hash
//! comparison instead of a mismatch list. When they differ, the window
//! is where the damage is: the driver re-judges just that window's ticks
//! with the oracle core and gets the classified mismatches. Digests
//! therefore *localize*; the oracle still *judges*. A digest comparison
//! is never a verdict on its own, and nothing here introduces a
//! tolerance: the encoding is format-exact (SOA-4), so equality is the
//! only comparison, exactly as in the oracle.
//!
//! **Canonical order.** The expected side walks ticks in order and each
//! tick's grid series-major — the order the workload body writes its
//! lines. The observed side is canonicalized into the same order
//! (by tick, then series, then row, then timestamp, then encoded value)
//! before hashing, so a read-back that returns a window's points in a
//! different order digests identically; a duplicated point is hashed
//! twice, so a duplicated seal changes the observed digest, the same
//! failure the oracle surfaces as
//! [`MismatchKind::Duplicate`](crate::soak_oracle::MismatchKind::Duplicate).
//! Points observed outside the window's tick range are not part of the
//! window and are not hashed; points observed for a tick inside the
//! range are hashed whether or not the model filled their cell — an
//! unexpected point changes the digest just as it mismatches the oracle.
//!
//! **Recorded in the run record.** Digests are recorded on the run they
//! summarize through
//! [`SoakHarness::record_window_digest`](crate::soak::SoakHarness::record_window_digest),
//! in window order, while the run is `Running` — the soak's own evidence
//! trail, beside the oracle verdict (SOA-6). Recording a field on
//! [`SoakRun`](crate::soak::SoakRun) takes
//! [`SOAK_SCHEMA_VERSION`](crate::soak::SOAK_SCHEMA_VERSION) to v3 on
//! SOA's own version line; the field is `#[serde(default)]` and skipped
//! when empty, so v1 and v2 artifacts still deserialize, with no
//! windows. A digest carries no free text — two counts, a tick range and
//! a hex hash — so it passes through redaction unchanged.
//!
//! **The digest algorithm is versioned in its own input.** The first
//! line hashed is a domain separator naming the algorithm
//! (`weftdb-soak-window-digest/1`): if the canonical encoding ever
//! changes, the separator changes with it, and digests from the two
//! algorithms can never compare equal by accident.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
	soak_oracle::ObservedPoint, soak_workload::{GridValue, WorkloadModel}
};

/// Domain separator hashed ahead of every window's points: the digest
/// algorithm's name and version, so an encoding change is a new
/// algorithm rather than a silent change to what an old digest means.
const DIGEST_DOMAIN: &str = "weftdb-soak-window-digest/1";

/// One window's digest: the ticks it covers, how many points were
/// hashed, and the hash itself.
///
/// Produced by [`expected_window_digest`] (from the model) or
/// [`observed_window_digest`] (from a read-back); the two are compared
/// with [`Self::matches`]. Recorded on the run record through the
/// harness. Carries no free text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowDigest {
	/// First tick of the window.
	pub start_tick: u64,
	/// Ticks in the window: the window covers
	/// `start_tick..start_tick + tick_count`.
	pub tick_count: u64,
	/// Points hashed: present cells for an expected digest, observations
	/// supplied (duplicates included) for an observed one.
	pub point_count: u64,
	/// SHA-256 of the window's canonical encoding, lowercase hex.
	pub sha256: String,
}

impl WindowDigest {
	/// The first tick after this window.
	#[must_use]
	pub const fn end_tick(&self) -> u64 {
		self.start_tick.saturating_add(self.tick_count)
	}

	/// Whether two digests state the same window contents: same range,
	/// same point count, same hash. A digest from a different range never
	/// matches, even if the points hashed happened to agree.
	#[must_use]
	pub fn matches(&self, other: &Self) -> bool {
		self.start_tick == other.start_tick && self.tick_count == other.tick_count && self.point_count == other.point_count && self.sha256 == other.sha256
	}

	/// Serialize this digest as a pretty JSON artifact.
	///
	/// # Errors
	///
	/// Returns an error when serialization fails.
	pub fn to_json(&self) -> anyhow::Result<String> {
		Ok(serde_json::to_string_pretty(self)?)
	}
}

/// Feed one point into a window hash in canonical line form: tick,
/// series, row, timestamp, and the value's exact wire encoding.
fn feed_point(hasher: &mut Sha256, tick: u64, series: usize, row: usize, timestamp_ns: i64, value: &GridValue) {
	hasher.update(format!("{tick},{series},{row},{timestamp_ns},{}\n", value.encode_field()).as_bytes());
}

/// Start a window hash: the algorithm's domain separator, so a digest
/// is only ever compared against digests of the same algorithm.
fn new_window_hasher() -> Sha256 {
	let mut hasher = Sha256::new();
	hasher.update(DIGEST_DOMAIN.as_bytes());
	hasher.update(b"\n");
	hasher
}

/// Finish a window hash as lowercase hex, the form the manifest's
/// digests use.
fn finish_hex(hasher: Sha256) -> String {
	let digest = hasher.finalize();
	let mut out = String::with_capacity(digest.len() * 2);
	for byte in digest {
		let _ = write!(out, "{byte:02x}");
	}
	out
}

/// The expected digest of a window.
///
/// This is what the model wrote in ticks
/// `start_tick..start_tick + tick_count`, regenerated from the model
/// alone — the same regeneration the oracle judges against, hashed
/// instead of compared cell by cell.
///
#[must_use]
pub fn expected_window_digest(model: &WorkloadModel, start_tick: u64, tick_count: u64) -> WindowDigest {
	let mut hasher = new_window_hasher();
	let mut point_count: u64 = 0;
	for offset in 0..tick_count {
		let tick = start_tick.saturating_add(offset);
		let step = model.step(tick);
		for series in 0..step.grid.series {
			for row in 0..step.grid.rows {
				if let Some(value) = step.grid.cell(series, row) {
					feed_point(&mut hasher, tick, series, row, model.timestamp_ns(tick, row), value);
					point_count = point_count.saturating_add(1);
				}
			}
		}
	}
	WindowDigest { start_tick, tick_count, point_count, sha256: finish_hex(hasher) }
}

/// The observed digest of a window: the points a read-back found for
/// ticks `start_tick..start_tick + tick_count`, canonicalized before
/// hashing so the read-back's return order does not matter.
///
/// `ticks` is the read-back in the oracle's shape — one entry per tick,
/// with everything found for it. A tick with no entry contributes no
/// points (a window whose read-back found nothing digests as empty,
/// which cannot match a non-empty expected window — the digest form of
/// the oracle's `Missing`). Entries outside the window's range are not
/// part of the window and are ignored.
#[must_use]
pub fn observed_window_digest(start_tick: u64, tick_count: u64, ticks: &[(u64, Vec<ObservedPoint>)]) -> WindowDigest {
	let mut points: Vec<(u64, usize, usize, i64, String, &GridValue)> = Vec::new();
	for offset in 0..tick_count {
		let tick = start_tick.saturating_add(offset);
		if let Some((_, observed)) = ticks.iter().find(|(t, _)| *t == tick) {
			for point in observed {
				points.push((tick, point.series, point.row, point.timestamp_ns, point.value.encode_field(), &point.value));
			}
		}
	}
	points.sort_by(|a, b| (a.0, a.1, a.2, a.3, &a.4).cmp(&(b.0, b.1, b.2, b.3, &b.4)));
	let mut hasher = new_window_hasher();
	for (tick, series, row, timestamp_ns, _, value) in &points {
		feed_point(&mut hasher, *tick, *series, *row, *timestamp_ns, value);
	}
	let point_count = points.len() as u64;
	WindowDigest { start_tick, tick_count, point_count, sha256: finish_hex(hasher) }
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::soak_workload::{LaneWeights, WorkloadParams};

	fn model() -> WorkloadModel {
		WorkloadModel::new(7, WorkloadParams::new(3, 4, 100, LaneWeights { ingest: 1, point_lookup: 0, range_scan: 0 })).expect("valid model")
	}

	fn sparse_model() -> WorkloadModel {
		WorkloadModel::new(7, WorkloadParams::new(3, 4, 50, LaneWeights { ingest: 1, point_lookup: 0, range_scan: 0 })).expect("valid model")
	}

	/// The faithful read-back of a tick range, in the oracle's input
	/// shape: one observation per present cell.
	fn faithful(model: &WorkloadModel, start_tick: u64, tick_count: u64) -> Vec<(u64, Vec<ObservedPoint>)> {
		(start_tick..start_tick + tick_count)
			.map(|tick| {
				let step = model.step(tick);
				let mut points = Vec::new();
				for series in 0..step.grid.series {
					for row in 0..step.grid.rows {
						if let Some(value) = step.grid.cell(series, row) {
							points.push(ObservedPoint { series, row, timestamp_ns: model.timestamp_ns(tick, row), value: value.clone() });
						}
					}
				}
				(tick, points)
			})
			.collect()
	}

	#[test]
	fn a_faithful_read_back_digests_exactly_like_the_model() {
		let model = model();
		let expected = expected_window_digest(&model, 0, 8);
		let observed = observed_window_digest(0, 8, &faithful(&model, 0, 8));
		assert!(expected.matches(&observed), "a faithful window must digest identically: {expected:?} vs {observed:?}");
		assert_eq!(expected.point_count, 8 * 12, "8 ticks of a full 3x4 grid");
		assert_eq!(expected.end_tick(), 8);
	}

	#[test]
	fn digests_are_deterministic_and_window_scoped() {
		let model = model();
		assert_eq!(expected_window_digest(&model, 4, 4), expected_window_digest(&model, 4, 4), "the same window digests the same way twice");
		let first = expected_window_digest(&model, 0, 4);
		let second = expected_window_digest(&model, 4, 4);
		assert_ne!(first.sha256, second.sha256, "different windows of the same run digest differently");
		assert!(!first.matches(&second), "a digest never matches a different window's, even from the same run");
		// At density 100 the grid is seed-independent (every cell present,
		// values from the deterministic signal), so the seed check uses
		// the sparse model, whose presence pattern is stream-drawn.
		let sparse = sparse_model();
		let other_seed = WorkloadModel::new(8, sparse.params).expect("valid model");
		assert_ne!(expected_window_digest(&other_seed, 0, 4).sha256, expected_window_digest(&sparse, 0, 4).sha256, "a different seed's sparse window digests differently");
	}

	#[test]
	fn the_observed_digest_ignores_read_back_order() {
		let model = sparse_model();
		let mut read_back = faithful(&model, 0, 4);
		let in_order = observed_window_digest(0, 4, &read_back);
		for (_, points) in &mut read_back {
			points.reverse();
		}
		read_back.reverse();
		assert!(in_order.matches(&observed_window_digest(0, 4, &read_back)), "canonicalization makes the read-back's order irrelevant");
	}

	#[test]
	fn every_corruption_the_oracle_classifies_changes_the_digest() {
		let model = sparse_model();
		let expected = expected_window_digest(&model, 0, 4);

		// Missing: drop one point.
		let mut read_back = faithful(&model, 0, 4);
		read_back[0].1.remove(0);
		assert!(!expected.matches(&observed_window_digest(0, 4, &read_back)), "a missing point must change the digest");

		// Wrong value.
		let mut read_back = faithful(&model, 0, 4);
		read_back[0].1[0].value = GridValue::Text("tampered".to_string());
		assert!(!expected.matches(&observed_window_digest(0, 4, &read_back)), "a wrong value must change the digest");

		// Wrong timestamp.
		let mut read_back = faithful(&model, 0, 4);
		read_back[0].1[0].timestamp_ns += 1;
		assert!(!expected.matches(&observed_window_digest(0, 4, &read_back)), "a wrong timestamp must change the digest");

		// Duplicate: the duplicated seal.
		let mut read_back = faithful(&model, 0, 4);
		let duplicate = read_back[0].1[0].clone();
		read_back[0].1.push(duplicate);
		assert!(!expected.matches(&observed_window_digest(0, 4, &read_back)), "a duplicated point must change the digest");

		// Unexpected: a point in a cell the model left absent.
		let mut read_back = faithful(&model, 0, 4);
		let absent = (0..4).find_map(|tick| {
			let step = model.step(tick);
			(0..step.grid.series).find_map(|series| (0..step.grid.rows).find(|&row| step.grid.cell(series, row).is_none()).map(|row| (tick, series, row)))
		});
		if let Some((tick, series, row)) = absent {
			read_back[crate::num::u64_to_usize(tick)].1.push(ObservedPoint { series, row, timestamp_ns: model.timestamp_ns(tick, row), value: GridValue::Float(0.0) });
			assert!(!expected.matches(&observed_window_digest(0, 4, &read_back)), "an unexpected point must change the digest");
		}
	}

	#[test]
	fn an_empty_read_back_never_matches_a_written_window() {
		let model = model();
		let expected = expected_window_digest(&model, 0, 2);
		let observed = observed_window_digest(0, 2, &[]);
		assert_eq!(observed.point_count, 0);
		assert!(!expected.matches(&observed), "a window that read back nothing cannot match the model's window");
	}

	#[test]
	fn a_zero_tick_window_digests_as_empty_on_both_sides() {
		let model = model();
		let expected = expected_window_digest(&model, 5, 0);
		let observed = observed_window_digest(5, 0, &faithful(&model, 5, 0));
		assert_eq!(expected.point_count, 0);
		assert!(expected.matches(&observed), "an empty window is the one window an empty read-back matches");
	}

	#[test]
	fn observations_outside_the_window_are_not_hashed() {
		let model = model();
		let mut read_back = faithful(&model, 0, 6);
		let inside_only = observed_window_digest(2, 2, &read_back);
		read_back[0].1.clear();
		read_back[5].1.clear();
		assert!(inside_only.matches(&observed_window_digest(2, 2, &read_back)), "ticks outside the window do not reach its digest");
		assert!(inside_only.matches(&expected_window_digest(&model, 2, 2)), "the window cut out of a larger read-back still matches the model");
	}

	#[test]
	fn a_digest_round_trips_through_json() {
		let digest = expected_window_digest(&model(), 0, 4);
		let json = digest.to_json().expect("serialize digest");
		let back: WindowDigest = serde_json::from_str(&json).expect("deserialize digest");
		assert_eq!(digest, back);
	}
}
