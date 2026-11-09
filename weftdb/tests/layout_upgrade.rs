#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]
//! Layout-1 to layout-2 upgrade test — roadmap item **FRE-13**.
//!
//! What these tests pin that the internal
//! [`a_layout_1_store_upgrades_to_layout_2_and_accepts_new_writes`] does not: the
//! upgrade contract is exercised through the **public** API, verifying that a consumer
//! who calls only [`SegmentStore::open`], [`SegmentStore::seal`] and
//! [`SegmentStore::read_time_range`] sees what the freeze design (§4.3) promises.
//!
//! The internal test at `segment_store.rs:6925` is more thorough (it checks schemas,
//! column-by-column DDL, store_meta mirroring, frame paths, etc.) — this suite checks
//! the outcome a documented API user cares about.
//!
//! # On-disk difference between layout 1 and layout 2
//!
//! `LEGACY_LAYOUT = 1`, `SUPPORTED_LAYOUT = 2`. The marker (`STORE_FORMAT` JSON) is the
//! **only** on-disk difference the store makes between layouts — there is no byte-level
//! marker (`WEFTL1\n`) or separate data format version. The `0001_baseline` migration
//! creates the control-plane databases (`segment_index.db`, `aspect_catalog.db`,
//! `metadata.db`, `catalog.db`); later migrations add tables/columns to them. No
//! migration rewrites existing segment frames, so data preservation across an upgrade is
//! a weak claim: the frames never change, and the index entries created by
//! `seal_legacy_layout` survive because no migration drops columns from `SegmentV1`'s
//! schema.
//!
//! **What this means for tests**: the `synthetic_marker_layout_1_detected_and_upgraded`
//! test rewrites the JSON marker alone — it cannot prove the migrations handle a
//! genuinely old segment shape. The `pre_v2_store` fixture (a real pre-marker root) does
//! exercise `seal_legacy_layout` segment paths and the full migration pipeline.
//!
//! # Reversibility
//!
//! `docs/upgrade-notes.md` (§v0.1.0 → v0.2.0) promises the upgrade is one-way and
//! warns against running v0.1.0 on a migrated store. The migration is NOT tested for
//! reversibility — no test in this file or elsewhere verifies that a layout-2 store can
//! be rolled back to a layout-1-only binary. Doing so would need a v0.1.0 binary in CI.
//!
//! # Fixture
//!
//! Uses the checked-in [`pre_v2_store`](tests/fixtures/pre_v2_store/) fixture — a
//! segment store root exactly as WeftDB wrote it before store layout 2, at commit
//! `bcf0304`. It has no `STORE_FORMAT` marker and contains the aspects `price` and
//! `temp` as sealed by `seal_legacy_layout`.

use std::path::{Path, PathBuf};

use bigdecimal::BigDecimal;
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weftdb::{migrations, SegmentStore, StoreFormat, STORE_FORMAT_FILE, SUPPORTED_LAYOUT};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The committed pre-v2 fixture root.
fn fixture_root() -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pre_v2_store/root")
}

/// Copy a directory tree — including `-wal`/`-log` sidecars — into `to`.
fn copy_tree(from: &Path, to: &Path) {
	std::fs::create_dir_all(to).expect("creating the copy root");
	let mut pending = vec![(from.to_path_buf(), to.to_path_buf())];
	while let Some((src, dst)) = pending.pop() {
		for entry in std::fs::read_dir(&src).unwrap_or_else(|err| panic!("reading {}: {err}", src.display())) {
			let entry = entry.expect("a readable directory entry");
			let (name, file_type) = (entry.file_name(), entry.file_type().expect("a file type"));
			let (child_src, child_dst) = (entry.path(), dst.join(&name));
			if file_type.is_dir() {
				std::fs::create_dir_all(&child_dst).expect("creating a copied directory");
				pending.push((child_src, child_dst));
			} else {
				std::fs::copy(&child_src, &child_dst).unwrap_or_else(|err| panic!("copying {}: {err}", child_src.display()));
			}
		}
	}
}

/// A `BigDecimal` literal, matching the convention in the crate's tests.
fn bd(s: &str) -> BigDecimal {
	s.parse().expect("a valid BigDecimal literal")
}

/// The schema the fixture's `price` aspect was sealed with: F64, zero default,
/// seconds.
fn price_schema() -> AspectSchema {
	AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)
}

/// Read a `STORE_FORMAT` marker file and return its JSON contents as a
/// [`StoreFormat`].  Returns `None` when the file does not exist.
///
/// This mirrors the crate's [`store_format::read_marker`] free function, which is
/// not re-exported.  Its visibility is crate-internal; the integration test reads
/// it through the JSON file directly.
async fn read_marker(root: &Path) -> Option<StoreFormat> {
	let path = root.join(STORE_FORMAT_FILE);
	let bytes = match tokio::fs::read(&path).await {
		Ok(bytes) => bytes,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
		Err(e) => panic!("reading {}: {e}", path.display()),
	};
	let format: StoreFormat = serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("decoding {}: {e}", path.display()));
	Some(format)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A store written by a pre-marker WeftDB (layout 1) upgrades to layout 2 on
/// open, reports that it came from layout 1, applies every registered migration,
/// and serves the data the old store wrote.
///
/// This pins the **consequence** a consumer sees, documented in
/// [`OpenReport::migrated_from`] and [`SegmentStore::store_format`]: an open of a
/// pre-v2 root marks the marker, runs the migrations, and raises
/// `layout_version` to 2.
#[tokio::test]
async fn pre_v2_store_upgrades_from_layout_1_to_2() {
	let dir = tempfile::TempDir::new().expect("a temp directory for the store copy");
	let root = dir.path().join("store");
	copy_tree(&fixture_root(), &root);

	// The fixture predates the STORE_FORMAT marker.
	assert!(!root.join(STORE_FORMAT_FILE).exists(), "the pre-v2 fixture has no STORE_FORMAT marker, confirming it is a legacy store");

	let store = SegmentStore::open(&root).await.unwrap_or_else(|e| panic!("opening the pre-v2 store for the first time: {e:#}"));
	let report = store.open_report();

	assert_eq!(
		report.migrated_from,
		Some(1),
		"the pre-v2 store has no STORE_FORMAT marker so the open recognises it as layout 1 \
		 and migrates it forward; migrated_from is Some(1)"
	);
	assert!(
		report.applied.len() >= 8,
		"the first open runs every registered migration (0001_baseline through \
		 0008_upsert_keys); got {}: {:?}",
		report.applied.len(),
		report.applied
	);
	assert!(!report.created_new, "the store existed — this is not a new store");

	// The marker is now layout 2.
	assert_eq!(store.store_format().layout_version, SUPPORTED_LAYOUT, "after the migration the store's layout is {} (SUPPORTED_LAYOUT)", SUPPORTED_LAYOUT);
	assert_eq!(store.store_format().min_read_layout, SUPPORTED_LAYOUT, "min_read_layout is also raised to {}", SUPPORTED_LAYOUT);
	assert_eq!(store.store_format().min_write_layout, SUPPORTED_LAYOUT, "min_write_layout is also raised to {}", SUPPORTED_LAYOUT);

	// Fixture data is readable after the migration.
	assert_eq!(store.read_point("price", 0).await.expect("reading price at timestamp 0"), Some(bd("0.5")), "price=0.5 at t=0 survived the migration");
	assert_eq!(store.read_point("price", 50).await.expect("reading price at timestamp 50"), Some(bd("5.5")), "price=5.5 at t=50 survived the migration");
	assert_eq!(store.read_point("temp", 0).await.expect("reading temp at timestamp 0"), Some(bd("20.5")), "temp=20.5 at t=0 survived the migration");
	assert_eq!(store.read_point("temp", 180).await.expect("reading temp at timestamp 180"), Some(bd("22")), "temp=22 at t=180 survived the migration");

	// A range read across the fixture's timestamps works.
	let (ts, vs) = store.read_time_range("price", 0, 300).await.expect("range read of the migrated price aspect");
	assert!(!ts.is_empty(), "range read of price after migration returns points");
	assert_eq!(ts.len(), vs.len(), "timestamp and value arrays must have the same length");

	drop(store);
}

/// The upgrade is **idempotent**: a second open of the now-upgraded store reports
/// no migration, does not rewrite the marker, and keeps all data accessible.
#[tokio::test]
async fn upgraded_store_opens_idempotently() {
	let dir = tempfile::TempDir::new().expect("a temp directory for the store copy");
	let root = dir.path().join("store");
	copy_tree(&fixture_root(), &root);

	// First open — migrate from layout 1.
	let store = SegmentStore::open(&root).await.unwrap_or_else(|e| panic!("first open: {e:#}"));

	let first_uuid = store.store_uuid().to_string();
	drop(store);

	// Read the on-disk marker after the first open.
	let marker1 = read_marker(&root).await.expect("the marker exists after the first open");

	// Second open — the store is settled.
	let store2 = SegmentStore::open(&root).await.unwrap_or_else(|e| panic!("second open: {e:#}"));

	let report2 = store2.open_report();
	assert_eq!(
		report2.migrated_from, None,
		"a second open of the already-upgraded store reports no migration \
		 (migrated_from = None) — the marker already says layout 2"
	);
	assert!(report2.applied.is_empty(), "no migrations run on the second open");

	// The store identity is stable.
	assert_eq!(store2.store_uuid(), first_uuid, "the store_uuid survives the reopen");

	// The on-disk marker after the second open matches the first open's marker.
	let marker2 = read_marker(&root).await.expect("the marker exists after the second open");
	assert_eq!(marker2.layout_version, SUPPORTED_LAYOUT, "the on-disk marker is still layout {} after the second open", SUPPORTED_LAYOUT);
	assert_eq!(
		(marker1.layout_version, marker1.min_read_layout, marker1.min_write_layout),
		(marker2.layout_version, marker2.min_read_layout, marker2.min_write_layout),
		"the markers written by the first and second open agree on \
		 layout_version, min_read_layout and min_write_layout — \
		 the second open did not rewrite them"
	);

	// Fixture data remains readable after the second open.
	assert_eq!(store2.read_point("price", 0).await.expect("reading price at t=0 after reopen"), Some(bd("0.5")), "price=0.5 at t=0 is still readable after the second open");
	assert_eq!(store2.read_point("price", 50).await.expect("reading price at t=50 after reopen"), Some(bd("5.5")), "price=5.5 at t=50 is still readable after the second open");

	drop(store2);
}

/// Data written **before** the upgrade still reads back after it, and a write +
/// read after the upgrade works: an upgraded store is still a working store.
#[tokio::test]
async fn write_and_read_after_upgrade_works() {
	let dir = tempfile::TempDir::new().expect("a temp directory for the store copy");
	let root = dir.path().join("store");
	copy_tree(&fixture_root(), &root);

	// First open — migrates layout 1 → 2.
	let store = SegmentStore::open(&root).await.unwrap_or_else(|e| panic!("first open: {e:#}"));

	// Verify pre-upgrade data reads correctly.
	let (pre_ts, _pre_vs): (Vec<i64>, Vec<Option<BigDecimal>>) = store.read_time_range("price", 0, 200).await.expect("reading fixture data after migration");
	assert!(!pre_ts.is_empty(), "fixture data is readable after the migration");

	// Write new data — timestamps outside the fixture's range so there is no
	// ambiguity about which row came from the upgrade.
	let new_ts = vec![500i64, 510, 520];
	let new_vals = vec![bd("55"), bd("66"), bd("77")];
	let schema = price_schema();
	store.seal("price", &schema, &new_ts, &new_vals).await.expect("sealing new data into the upgraded store — an upgraded store is writable");

	// Read back the new data.
	let (post_ts, post_vs): (Vec<i64>, Vec<Option<BigDecimal>>) = store.read_time_range("price", 500, 600).await.expect("reading the just-written range");
	assert_eq!(post_ts, new_ts, "the timestamps written after the upgrade read back correctly");
	let expected_vals: Vec<Option<BigDecimal>> = new_vals.into_iter().map(Some).collect();
	assert_eq!(post_vs, expected_vals, "the values written after the upgrade read back correctly");
	drop(store);

	// Reopen and verify both old and new data survived.
	let store2 = SegmentStore::open(&root).await.unwrap_or_else(|e| panic!("reopen after writing new data: {e:#}"));
	assert_eq!(store2.read_point("price", 0).await.expect("reading price at t=0 after reopen"), Some(bd("0.5")), "pre-upgrade data (price=0.5 at t=0) survived the new write and reopen");
	let (reopen_ts, reopen_vs): (Vec<i64>, Vec<Option<BigDecimal>>) = store2.read_time_range("price", 500, 600).await.expect("reading the post-upgrade range after reopen");
	assert_eq!(reopen_ts, post_ts, "post-upgrade data persisted through reopen (timestamps match)");
	assert!(reopen_vs.iter().any(|v| v.is_some()), "post-upgrade data persisted through reopen (at least one value is present)");
	drop(store2);
}

/// A synthetic store whose marker is rewritten to layout 1 is detected as
/// migrating from layout 1 on the next open, OR the migration list surprises a
/// future developer who moved a migration without updating the doc comment.
///
/// This tests the **marker-based** detection path (a `STORE_FORMAT` whose
/// `layout_version` is 1), which is separate from the no-marker path the
/// `pre_v2_store` fixture exercises. What this cannot cover: a genuinely old
/// segment shape or pre-migration control-plane DDL, since the store was created
/// at layout 2 and only its marker was downgraded. A store that was never
/// migrated needs a fixture written by an older binary — see `compat/fixtures` or
/// `tests/fixtures/pre_v2_store`.
///
/// The `after(REGISTRY, Some("0001_baseline"))` assertion replaces the obvious
/// `assert_eq!(..layout_version, SUPPORTED_LAYOUT)` — which cannot fail because
/// `SUPPORTED_LAYOUT` is the constant the code uses — with a check that breaks if
/// migrations are added, removed, or reordered without updating this list.
#[tokio::test]
async fn synthetic_marker_layout_1_detected_and_upgraded() {
	// First, pin the migrations that run after "0001_baseline" (layout 1's only
	// migration). Future changes that add or reorder migrations must update this
	// list or the test fails — it is the developer's signal to check whether the
	// upgrade doc comment still matches.
	let after_baseline: Vec<&str> = migrations::after(migrations::REGISTRY, Some("0001_baseline")).iter().map(|m| m.id).collect();
	assert_eq!(
		after_baseline,
		["0002_s6_s7", "0003_fre14_legacy_catalog", "0004_d_s12", "0005_d_s13", "0006_tag5_series", "0007_series_time_index", "0008_upsert_keys",],
		"the migrations after '0001_baseline' are the ones a layout-1 upgrade runs; \
		 adding or reordering migrations must update the doc comment and this assertion"
	);

	let dir = tempfile::TempDir::new().expect("a temp directory");
	let root = dir.path().join("store");

	// Create a fresh store (layout 2).
	let store = SegmentStore::open(&root).await.unwrap_or_else(|e| panic!("creating a fresh store: {e:#}"));
	let original_uuid = store.store_uuid().to_string();
	drop(store);

	// Read the marker, rewrite its layout_version to 1.
	let format = read_marker(&root).await.expect("the marker exists after creation");

	// Construct a layout-1 marker: same identity, but every layout field set to 1.
	let downgraded = StoreFormat { layout_version: 1, min_read_layout: 1, min_write_layout: 1, last_written_layout: 1, migrating_to: None, applied_through: None, ..format.clone() };
	let marker_bytes = serde_json::to_vec(&downgraded).expect("encoding the downgraded marker");
	std::fs::write(root.join(STORE_FORMAT_FILE), &marker_bytes).unwrap_or_else(|e| panic!("writing the downgraded marker: {e}"));

	// Reopen — should detect layout 1 and "migrate" (the databases are already
	// up to date, so no migration actually runs, but migrated_from is Some(1)).
	let store2 = SegmentStore::open(&root).await.unwrap_or_else(|e| panic!("reopening after marker downgrade: {e:#}"));
	let report2 = store2.open_report();
	assert_eq!(
		report2.migrated_from,
		Some(1),
		"a store whose marker says layout_version=1 is detected as migrating from \
		 layout 1 even when its databases are already at layout 2; \
		 migrated_from = Some(1)"
	);
	assert_eq!(store2.store_uuid(), original_uuid, "the store_uuid is unchanged after the upgrade");
	drop(store2);
}

/// A store whose marker says `min_write_layout=2` is refused by a build that
/// understands only layout 1, via [`StoreFormat::writable_by`].
///
/// This pins the downgrade-refusal contract in-process. What it cannot exercise:
/// the actual open path in a layout-1-only binary loading a layout-2 marker — that
/// needs a cross-version CI job with a v0.1.0 build.
///
/// Reversibility of the 1→2 upgrade is **not tested**. `docs/upgrade-notes.md`
/// (§v0.1.0 → v0.2.0) labels the upgrade one-way and warns against running v0.1.0
/// on a migrated store, but no test in this file or elsewhere verifies that a
/// layout-2 store can be safely rolled back.
#[tokio::test]
async fn layout_2_store_is_refused_by_writable_by_1() {
	let format_v2 = StoreFormat { layout_version: 2, min_read_layout: 2, min_write_layout: 2, last_written_layout: 2, migrating_to: None, applied_through: Some("0008_upsert_keys".into()), store_uuid: "test".into(), kind: "weftdb-store".into(), scope: weftdb::StoreScope { database: "default".into(), subject: "default".into() } };
	assert!(
		!format_v2.writable_by(1),
		"a StoreFormat with min_write_layout=2 is NOT writable_by(1) — \
		 a layout-1-only binary must refuse the store before opening anything"
	);
	// Also verify the symmetric case works (a layout-1 marker IS writable by build 2).
	let format_v1 = StoreFormat { layout_version: 1, min_read_layout: 1, min_write_layout: 1, last_written_layout: 1, migrating_to: None, applied_through: None, store_uuid: "test".into(), kind: "weftdb-store".into(), scope: weftdb::StoreScope { database: "default".into(), subject: "default".into() } };
	assert!(
		format_v1.writable_by(2),
		"a StoreFormat with min_write_layout=1 IS writable_by(2) — \
		 a layout-2 build opens a layout-1 store (which triggers the upgrade)"
	);
}
