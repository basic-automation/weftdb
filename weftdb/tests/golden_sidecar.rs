//! The bytes a `.weftpart` sidecar writes, pinned as a committed golden (roadmap slice **FRE-3**).
//!
//! [`weft_physical_type`'s golden suite](../../../weft-physical-type/tests/golden_frames.rs) pins
//! `.weftseg`. The sidecar is the other file a sealed segment leaves on disk, and it is the one whose
//! bytes nothing currently checks: `PartialSidecar` is a `serde` type serialized with `postcard`, so a
//! new field, a reordered field, or a changed `Resolution` enum discriminant silently changes the wire
//! format, and every round-trip test in the crate still passes because writer and reader moved together.
//!
//! That is the exact failure mode a golden exists for. A sidecar written by yesterday's binary is read
//! by today's (`PartialSidecar::matches` is the only guard, and it compares *contents*, not layout), so
//! a layout change strands every sidecar in the store — which is not a correctness disaster, since a
//! sidecar that fails to decode means a slower query rather than a wrong one, but it is a compatibility
//! event that has to be noticed rather than shipped quietly.
//!
//! Two pins per case, as in the frame suite: the bytes, and the **contents** the bytes decode to,
//! compared against a reduction recomputed from the corpus. A re-recorded fixture whose contents drifted
//! fails on the second pin even though the first one was regenerated to match.
//!
//! `PARTIAL_SIDECAR_VERSION` is 3 and the magic's last byte is `\x02`; when D-S15 adds the v4 reader
//! its goldens join this set (1.0 plan, resolution D-S15).
//!
//! Regenerating is `WEFT_UPDATE_GOLDENS=1 cargo test -p weftdb --test golden_sidecar`, and it changes
//! what every sidecar on disk means: say so in the commit message.

use std::{
	path::{Path, PathBuf}, str::FromStr
};

use bigdecimal::BigDecimal;
use chrono::{TimeZone, Utc};
use splimes::{Point, Resolution};
use weft_physical_type::SegmentDescriptor;
use weft_reduce::reduce_partial;
use weftdb::{PartialSidecar, PARTIAL_SIDECAR_VERSION, SIDECAR_AGGREGATIONS};

/// The environment variable that rewrites goldens instead of comparing them.
const UPDATE_VAR: &str = "WEFT_UPDATE_GOLDENS";

/// The committed magic every `.weftpart` reader looks for. Duplicated here on purpose: the constant in
/// the crate is private, and a golden suite that reads its own crate's constant cannot notice that
/// constant changed.
const EXPECTED_MAGIC: &[u8; 8] = b"WEFTPRT\x02";

fn golden_dir() -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn updating() -> bool {
	std::env::var_os(UPDATE_VAR).is_some()
}

/// A golden is compared byte-for-byte; the update path writes what the writer produced today.
fn check_golden(name: &str, fresh: &[u8]) {
	let path = golden_dir().join(name);
	if updating() {
		std::fs::create_dir_all(golden_dir()).expect("tests/golden is creatable in a recording run");
		std::fs::write(&path, fresh).unwrap_or_else(|error| panic!("{}: cannot write the golden: {error}", path.display()));
		return;
	}
	let committed = std::fs::read(&path).unwrap_or_else(|error| panic!("{name}: no committed golden at {}.\nRecord the set with `{UPDATE_VAR}=1 cargo test -p weftdb --test golden_sidecar` and review the diff: {error}", path.display()));
	if committed == fresh {
		return;
	}
	let first_diff = committed.iter().zip(fresh).position(|(a, b)| a != b);
	let shown = first_diff.unwrap_or(std::cmp::min(committed.len(), fresh.len()));
	let window = |bytes: &[u8]| bytes.iter().skip(shown.saturating_sub(8)).take(24).map(|byte| format!("{byte:02x}")).collect::<Vec<_>>().join(" ");
	panic!(
		"{name}: the sidecar bytes are not the committed golden.\n\
		 committed: {} bytes, first difference at {shown}\n\
		 written:   {} bytes, first difference at {shown}\n\
		 committed around the difference: {}\n\
		 written   around the difference: {}\n\
		 \
		 A `.weftpart` file is `postcard`-of-the-struct, so a field added, reordered or retyped in \
		 `PartialSidecar` changes this golden even when every round-trip test still passes. Existing \
		 sidecars do not become wrong, they become unreadable, and every downsample that wanted one \
		 silently falls back to decoding the segment. Decide that is intended, then re-record.",
		committed.len(),
		fresh.len(),
		window(&committed),
		window(&fresh),
	)
}

/// The corpus: 600 points at a 30 s period, values from a fixed 89-cycle. No clock, no randomness, no
/// hash ordering — and values that fit the `i64` scaled reduce path, which is the path a real numeric
/// series takes and the one whose partials get persisted.
fn corpus() -> Vec<Point> {
	(0..600).map(|i| Point { timestamp: Utc.timestamp_opt(i64::from(i) * 30, 0).single().expect("a valid instant"), value: BigDecimal::from_str(&format!("{}.5", (i * 7) % 89)).expect("a valid decimal") }).collect()
}

/// The descriptor the sidecar is stamped against — the staleness stamp a reader compares with the live
/// segment, so it is part of what a golden has to pin.
fn descriptor() -> SegmentDescriptor {
	SegmentDescriptor { id: 1, path: "temperature-1.weftseg".to_string(), format_version: 1, physical_type: None, time_unit: None, row_count: 600, null_count: 0, time_sorted: true, min_ts: Some(0), max_ts: Some(599 * 30), min_value: None, max_value: None, byte_len: 12_345 }
}

#[test]
fn sidecar_base_resolution_matches_its_committed_bytes() {
	let points = corpus();
	let partial = reduce_partial(&points, Resolution::Hours, None, None, &SIDECAR_AGGREGATIONS).expect("a partial reduction of the corpus");
	let sidecar = PartialSidecar::new(Resolution::Hours, &descriptor(), partial);
	let bytes = sidecar.to_bytes().expect("a sidecar serializes");
	assert_eq!(&bytes[..EXPECTED_MAGIC.len()], EXPECTED_MAGIC, "the magic prefix is the frame's only self-identification");
	check_golden("sidecar_v3_hours.bin", &bytes);

	let reloaded = PartialSidecar::from_bytes(&bytes).expect("the pinned bytes decode");
	assert_eq!(reloaded.version, PARTIAL_SIDECAR_VERSION, "the frame records the version it was written under");
	assert!(reloaded.matches(&descriptor()), "the staleness stamp still matches the segment it was built from");
	assert!(!reloaded.matches(&SegmentDescriptor { byte_len: 12_346, ..descriptor() }), "a rewritten segment stops matching");

	// The contents pin: what the bytes decode to must be what the corpus reduces to, so a fixture
	// re-recorded after a codec change cannot smuggle different numbers through.
	let expected = reduce_partial(&points, Resolution::Hours, None, None, &SIDECAR_AGGREGATIONS).expect("a partial reduction of the corpus").finish(Resolution::Hours, &SIDECAR_AGGREGATIONS).expect("finishes");
	let got = reloaded.partial.finish(Resolution::Hours, &SIDECAR_AGGREGATIONS).expect("a reloaded sidecar finishes");
	assert_eq!(got, expected, "the committed sidecar no longer finishes to the corpus it claims to summarize");
}

#[test]
fn sidecar_rollup_tiers_match_their_committed_bytes() {
	let points = corpus();
	let partial = reduce_partial(&points, Resolution::Minutes, None, None, &SIDECAR_AGGREGATIONS).expect("a partial reduction of the corpus");
	// A minutes base with hours and days rolled up: the shape that makes a coarse query fold a few
	// stored buckets instead of thousands of minute buckets, and the shape with the most structure
	// (a chain of `PartialReduction`s) and therefore the most to lose from a layout change.
	let sidecar = PartialSidecar::materialize(Resolution::Minutes, &descriptor(), partial, &[Resolution::Hours, Resolution::Days]).expect("the tiers materialize");
	let bytes = sidecar.to_bytes().expect("a sidecar serializes");
	check_golden("sidecar_v3_minutes_tiered.bin", &bytes);

	let reloaded = PartialSidecar::from_bytes(&bytes).expect("the pinned bytes decode");
	assert_eq!(reloaded.rollups.len(), 2, "both tiers survive the wire");
	for resolution in [Resolution::Minutes, Resolution::Hours, Resolution::Days] {
		let fresh = sidecar.partial_for(resolution).expect("a tier can serve it").expect("the materialized chain covers it");
		let stored = reloaded.partial_for(resolution).expect("a tier can serve it").expect("the materialized chain covers it");
		assert_eq!(stored.finish(resolution, &SIDECAR_AGGREGATIONS).expect("finishes"), fresh.finish(resolution, &SIDECAR_AGGREGATIONS).expect("finishes"), "the {resolution:?} grid decoded from the golden differs from the freshly built one");
	}
	// A grid finer than the base is not served from the sidecar at all — the caller decodes. That the
	// answer is `None` and not a silent wrong-grid answer is part of what these bytes pin.
	assert!(reloaded.partial_for(Resolution::Seconds).expect("no re-key needed to say no").is_none(), "a resolution finer than the base must not be served from a tier");
}
