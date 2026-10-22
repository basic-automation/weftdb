//! The X-FX gate (1.0 plan): open what the **v0.1.0** release wrote with **this** build.
//!
//! `compat/fixtures/v0.1.0/` holds three trees written by v0.1.0 binaries — a Storage-v2
//! root, the backup that server took, and a rows-mode database — plus the reads that same
//! v0.1.0 server recorded while it owned them (`expected_reads.txt`) and the store's own
//! account of itself (`aspects.json`, `aspect_stats.txt`, `stats.json`). The trees are
//! checked in unchanged; `scripts/compat/make-v010-fixtures.sh` regenerates them from the
//! tag, and `MANIFEST.sha256`/`binaries.sha256` say who wrote what.
//!
//! What these tests pin down:
//!
//! - a v0.1.0 root **opens**, is recognised as a pre-marker store (layout 1), is migrated
//!   forward, and keeps the same values across the migration — including across a reopen,
//!   so the migrated tree is stable rather than rewritten on every start;
//! - the three segment shapes a v0.1.0 store could hold — overlapping batches, a batch
//!   reconciled in place, and a squashed aspect — all still read;
//! - the encoding a v0.1.0 declared aspect carries (scaled_i64 scale 4, seconds) survives,
//!   and null rows keep being nulls;
//! - what a v0.1.0 backup actually contains, which is **not** the segment data. That is a
//!   finding, not an aspiration: see [`v010_backup_covers_the_control_plane_only`].
//!
//! The rows-mode tree is refused on `open`: an open of it names the aspect databases and
//! writes nothing (FRE-14a, [`a_v010_rows_mode_directory_is_refused_and_writes_nothing`]).
//! It is also now read, deliberately, through [`weftdb::rows_mode`] — the second half of
//! FRE-14 ([`the_v010_rows_mode_tree_reads_into_a_store_of_its_own_database`]). The tooling an
//! operator would run (FRE-14c) is still open; the tree is here so both halves are driven by
//! what the release wrote instead of by a fixture someone invented.

use std::{
	collections::{BTreeMap, BTreeSet}, num::NonZeroUsize, path::{Path, PathBuf}
};

use weft_physical_type::{PhysicalType, TimeUnit};
use weftdb::{SegmentStore, StoreError};

/// The committed fixture set, relative to this crate.
fn fixture() -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join("../compat/fixtures/v0.1.0")
}

/// Copy a tree — including the `-wal`/`-log` sidecars a v0.1.0 server leaves — into `to`.
///
/// Every test works on a copy: opening a store writes to it (the format marker, the
/// migrations, the lock), and the checked-in trees must stay exactly as the release left
/// them so a later run compares against the same bytes.
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

/// A decimal as canonical text, so a scale difference is not mistaken for a different
/// number: a v0.1.0 scaled_i64 column reads back as `100.2500` while a re-encoded one may
/// say `100.25`. These tests compare values, not formatting.
fn canonical(text: &str) -> String {
	let text = text.trim();
	let (sign, rest) = match text.strip_prefix('-') {
		Some(rest) => ("-", rest),
		None => ("", text),
	};
	match rest.split_once('.') {
		Some((int, frac)) => {
			let frac = frac.trim_end_matches('0');
			if frac.is_empty() {
				format!("{sign}{int}")
			} else {
				format!("{sign}{int}.{frac}")
			}
		}
		None => format!("{sign}{rest}"),
	}
}

/// One recorded read: the aspect, the query the v0.1.0 server was given, and the rows it
/// answered — `(timestamp, value)`, an empty field being a null.
struct Recorded {
	kind: String,
	aspect: String,
	query: String,
	rows: Vec<(i64, Option<String>)>,
}

/// Parse `expected_reads.txt`: a `# kind aspect query` header opens a section, the lines
/// under it are that section's body. Only the `range.csv`/`value-range.csv` bodies are
/// `(timestamp,value)` row lists; the `at` JSON and the `downsample.csv` bucket sweep have
/// their own shapes and are left as the record they are, so a parser that met them would
/// be inventing a meaning. Section kinds that are not row lists keep their bodies out of
/// `rows`.
fn recorded_reads() -> Vec<Recorded> {
	let text = std::fs::read_to_string(fixture().join("expected_reads.txt")).expect("the recorded reads are part of the fixture");
	let mut sections = Vec::new();
	let mut current: Option<Recorded> = None;
	for line in text.lines() {
		if let Some(header) = line.strip_prefix("# ") {
			if let Some(section) = current.take() {
				sections.push(section);
			}
			let mut fields = header.splitn(3, ' ');
			let (kind, aspect, query) = (fields.next().unwrap_or_default().to_string(), fields.next().unwrap_or_default().to_string(), fields.next().unwrap_or_default().to_string());
			current = Some(Recorded { kind, aspect, query, rows: Vec::new() });
			continue;
		}
		let line = line.trim_end_matches('\r');
		let Some(section) = current.as_mut() else { continue };
		if line.is_empty() || line.starts_with("timestamp,") {
			continue;
		}
		if matches!(section.kind.as_str(), "range.csv" | "value-range.csv") {
			let Some((ts, value)) = line.split_once(',') else { continue };
			section.rows.push((ts.trim().parse().expect("a recorded timestamp"), if value.trim().is_empty() { None } else { Some(value.trim().to_string()) }));
		}
	}
	if let Some(section) = current.take() {
		sections.push(section);
	}
	sections
}

/// The query string's parameters, so a recorded window can be replayed against a store.
fn params(query: &str) -> BTreeMap<String, String> {
	query.split('&').filter_map(|pair| pair.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn files_under(root: &Path) -> Vec<String> {
	let mut out = Vec::new();
	let mut pending = vec![(root.to_path_buf(), String::new())];
	while let Some((dir, prefix)) = pending.pop() {
		for entry in std::fs::read_dir(&dir).unwrap_or_else(|err| panic!("reading {}: {err}", dir.display())) {
			let entry = entry.expect("a readable directory entry");
			let (name, file_type) = (entry.file_name().to_string_lossy().into_owned(), entry.file_type().expect("a file type"));
			let relative = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
			if file_type.is_dir() {
				pending.push((entry.path(), relative));
			} else {
				out.push(relative);
			}
		}
	}
	out.sort();
	out
}

#[tokio::test]
async fn a_v010_store_migrates_forward_and_keeps_reading_its_own_values() {
	let store_dir = tempfile::tempdir().expect("tempdir for the copied store");
	copy_tree(&fixture().join("store"), store_dir.path());

	let store = SegmentStore::open(store_dir.path()).await.expect("this build opens a store the v0.1.0 server wrote");
	let report = store.open_report().clone();
	assert_eq!(report.migrated_from, Some(1), "a v0.1.0 root has no format marker, so it must be recognised as layout 1; the open reported {report:?}");
	assert!(!report.applied.is_empty(), "the open recorded in the fixture predates the marker and the migrations, so at least one must apply; the open reported {report:?}");
	assert!(!report.created_new, "an existing store's open does not create it");

	// The three aspects the fixture script declared — three different segment shapes, all
	// of which a reader still has to serve.
	let mut aspects = store.list_declared_aspects().await.expect("listing the declared aspects");
	aspects.sort();
	assert_eq!(aspects, ["flow", "price", "temp"], "the v0.1.0 store declared three aspects; this build sees {aspects:?}");

	let mut replay = Vec::new();
	for section in recorded_reads() {
		if section.kind != "range.csv" {
			continue;
		}
		let query = params(&section.query);
		let (start, end) = (query["start"].parse::<i64>().expect("a recorded start"), query["end"].parse::<i64>().expect("a recorded end"));
		let (timestamps, values) = store.read_time_range(&section.aspect, start, end).await.unwrap_or_else(|err| panic!("reading {} [{start},{end}] out of the v0.1.0 store: {err}", section.aspect));
		let got: Vec<(i64, Option<String>)> = timestamps.iter().zip(values.iter()).map(|(ts, value)| (*ts, value.as_ref().map(|v| canonical(&v.to_string())))).collect();
		let want: Vec<(i64, Option<String>)> = section.rows.iter().map(|(ts, value)| (*ts, value.as_ref().map(|v| canonical(v)))).collect();
		assert_eq!(got, want, "the window `GET /api/v1/storage/{}/{}?{}` answered one way at v0.1.0 and another here", section.aspect, section.kind, section.query);
		replay.push((section.aspect, start, end, got));
	}

	// One guard on the fixture itself: the recorded `price` window has to hold nulls,
	// otherwise the equality above says nothing about how a null in a **scaled** column
	// survives a version boundary. Seven of the 244 grid rows are null and two ingest routes
	// carried them, so the window the tag recorded answers with fourteen of them.
	let price = replay.iter().find(|(aspect, ..)| aspect == "price").expect("the fixture records a price window");
	assert!(price.3.iter().any(|(_, value)| value.is_none()), "the recorded price window must hold null rows, or the read comparison says nothing about a scaled aspect's nulls");

	// Reopen: the marker now exists and the migrations have run, so the second open moves
	// nothing — the migrated tree is a stable state, not something rewritten per start.
	drop(store);
	let reopened = SegmentStore::open(store_dir.path()).await.expect("the migrated store reopens");
	let second = reopened.open_report().clone();
	assert_eq!(second.migrated_from, None, "a migrated store reopens without migrating again; the second open reported {second:?}");
	assert!(second.applied.is_empty(), "no migration applies twice; the second open reported {second:?}");
	for (aspect, start, end, want) in replay {
		let (timestamps, values) = reopened.read_time_range(&aspect, start, end).await.unwrap_or_else(|err| panic!("re-reading {aspect} [{start},{end}] from the reopened store: {err}"));
		let got: Vec<(i64, Option<String>)> = timestamps.iter().zip(values.iter()).map(|(ts, value)| (*ts, value.as_ref().map(|v| canonical(&v.to_string())))).collect();
		assert_eq!(got, want, "the migrated store and the reopened store disagree about {aspect} [{start},{end}]");
	}
}

#[tokio::test]
async fn a_v010_aspect_keeps_its_declared_encoding_and_its_nulls() {
	let store_dir = tempfile::tempdir().expect("tempdir for the copied store");
	copy_tree(&fixture().join("store"), store_dir.path());
	let store = SegmentStore::open(store_dir.path()).await.expect("this build opens a v0.1.0 store");

	let schema = store.schema_for("price").await.expect("reading the schema").expect("`price` was declared in the v0.1.0 store");
	assert_eq!(schema.timestamp_unit, TimeUnit::Seconds, "the fixture declared `price` in seconds");
	assert_eq!(schema.value, PhysicalType::ScaledI64 { scale: 4 }, "`price` was declared scaled_i64 with scale 4; this build recovered {:?}", schema.value);
	assert_eq!(schema.value_tolerance.to_string(), "0", "the fixture declared an exact encoding");

	// The fixture's `temp` aspect holds seven null rows, sealed by a version that predates
	// this build's quality-mask handling; a null must still read as a null, not as zero and
	// not as a missing row.
	let (timestamps, values) = store.read_time_range("temp", 1767225600, 1767232999).await.expect("reading temp");
	let nulls = values.iter().filter(|value| value.is_none()).count();
	assert_eq!(timestamps.len(), 72, "temp holds 60 grid rows plus a 12-row out-of-order batch");
	assert_eq!(nulls, 7, "seven of them are null; this build reads {nulls}");

	let at = store.read_point("price", 1767225660).await.expect("reading one point").expect("that timestamp is in the fixture");
	assert_eq!(canonical(&at.to_string()), "102", "the v0.1.0 server answered 102.0000 for that point");
}

#[tokio::test]
async fn v010_segments_carry_the_three_shapes_a_reader_can_meet() {
	let listing = files_under(&fixture().join("store"));
	let segments: Vec<&String> = listing.iter().filter(|name| name.starts_with("segments/") && name.ends_with(".weftseg")).collect();
	assert_eq!(segments.len(), 6, "the fixture holds price-0..2 (three overlapping batches), temp-0/1 (sorted, then one reconciled in place) and flow-0 (squashed); found {segments:?}");
	for absent in ["store.json", "segment_manifest.json"] {
		assert!(!listing.iter().any(|name| name.as_str() == absent), "a v0.1.0 root holds no {absent}; if the fixture has one, it was not written by the tag's binary");
	}
	// The descriptors live in `segment_index.db`, not a manifest file — that is what makes
	// a marker-less root classifiable rather than guessable.
	assert!(listing.iter().any(|name| name.as_str() == "segment_index.db"), "a v0.1.0 root keys its segments in segment_index.db");
}

#[tokio::test]
async fn v010_backup_covers_the_control_plane_only() {
	let listing = files_under(&fixture().join("backup"));
	assert!(!listing.is_empty(), "the backup tree is part of the fixture");
	let databases: Vec<&String> = listing.iter().filter(|name| name.ends_with(".db")).collect();
	assert!(!databases.is_empty(), "the v0.1.0 backup holds the store's control-plane databases; found {listing:?}");
	assert!(listing.iter().all(|name| !name.contains("segments/")), "a v0.1.0 backup contains no segment file: {listing:?}");
	// This is a finding about the released behaviour, stated plainly because it bounds what
	// a restore across versions can promise: at v0.1.0 `POST /api/v1/storage/backup` copied
	// the four control-plane databases and none of `segments/`, so the backup of an ingested
	// store could not bring its data back. The server's own drill report (`restore_drill.json`,
	// taken by the v0.1.0 server) lists the same four databases and nothing else.
	let drill = std::fs::read_to_string(fixture().join("restore_drill.json")).expect("the v0.1.0 restore-drill report is part of the fixture");
	let report: serde_json::Value = serde_json::from_str(&drill).expect("the drill report is JSON");
	let names: Vec<&str> = report["databases"].as_array().expect("the drill lists databases").iter().map(|entry| entry["name"].as_str().expect("a named database")).collect();
	assert_eq!(names, ["segment_index.db", "metadata.db", "aspect_catalog.db", "catalog.db"], "the v0.1.0 restore drill covered exactly these four databases");
}

#[tokio::test]
async fn v010_legacy_tree_is_a_rows_mode_database_of_the_expected_shape() {
	let listing = files_under(&fixture().join("legacy"));
	assert!(listing.iter().any(|name| name.as_str() == "v010_legacy/metadata.db"), "a v0.1.0 database roots its control plane in metadata.db; found {:?}", listing.iter().take(8).collect::<Vec<_>>());
	for aspect in ["plant/price", "plant/temp", "plant/flow"] {
		assert!(listing.iter().any(|name| name.as_str() == format!("v010_legacy/{aspect}/measurements.db").as_str()), "the v0.1.0 generator wrote rows for {aspect} into its own measurements.db");
	}
	// Rows-mode has no null: the generator skips null grid rows, so the legacy tree is the
	// non-null subset of `rows.csv`. Treating the two trees as holding identical row counts
	// would be wrong; count what the grid carries instead, so a grid that drifts is visible
	// here rather than in a diff of a Turso file nobody can read. (13 nulls: 7 on the scaled
	// `price` aspect, 6 on `temp`. The store reports 14 and 7 — one extra `temp` null its
	// out-of-order batch re-seals over a window that already held one, and price's nulls
	// arriving twice because two ingest routes carried them.)
	let grid_nulls = std::fs::read_to_string(fixture().join("rows.csv")).expect("the shared grid is part of the fixture").lines().filter(|line| line.ends_with(",")).count();
	assert_eq!(grid_nulls, 13, "the shared grid carries 13 null rows for Storage-v2 to seal; the grid changed");
}

/// The shape 0.1.x left behind when it kept one database per aspect: a root with a
/// control-plane `metadata.db` and `<subject>/<aspect>/measurements.db` below it. This
/// build reads stores, and opening that root would otherwise succeed: `metadata.db` is one
/// of `[`weftdb::CONTROL_PLANE_FILES`]`, so the open finds a control plane, creates the three
/// databases it is missing, adds `segments/`, writes a marker beside the old data, and
/// reports an **empty** store. The refusal has to be loud, and it has to leave nothing.
#[tokio::test]
async fn a_v010_rows_mode_directory_is_refused_and_writes_nothing() {
	let source = fixture().join("legacy/v010_legacy");
	let dir = tempfile::tempdir().unwrap();
	let root = dir.path().join("plant-data");
	copy_tree(&source, &root);
	let before = files_under(&root);

	let error = match SegmentStore::open(&root).await {
		Ok(_) => panic!("a rows-mode directory is not a store, and saying so is the whole point"),
		Err(error) => error,
	};
	let typed = error.downcast_ref::<StoreError>().unwrap_or_else(|| panic!("the refusal is typed, not a sentence: {error:#}"));
	let StoreError::RowsModeDataDirectory { aspects } = typed else {
		panic!("the refusal names the rows-mode shape: {error:#}");
	};
	assert_eq!(aspects.as_slice(), ["plant/flow", "plant/price", "plant/temp"], "the refusal names every aspect database it found, in path order");
	// The sentence is what an operator reads, so it is asserted rather than sampled: it says
	// what the directory is, what was not changed, and which files the open did touch. It is
	// read off the typed error because the surrounding `anyhow` context replaces `Display`
	// with its own sentence — `{error:#}` below is what a CLI prints.
	assert_eq!(typed.to_string(), format!("this is a WeftDB 0.1.x data directory, keeping a database per aspect ({}); this version reads stores, and nothing here was changed (only LOCK and LOCK.holder were written)", ["\"plant/flow\"", "\"plant/price\"", "\"plant/temp\""].join(", ")));
	assert!(format!("{error:#}").starts_with(&format!("opening segment store {}", root.display())), "the refusal still says which open failed: {error:#}");

	// Nothing was created but the two lock files. The three databases a store needs, the
	// `segments/` directory, and the `STORE_FORMAT` marker are the writes this refusal exists
	// to prevent, and their absence is what makes the refusal safe to retry.
	let mut created = files_under(&root).into_iter().filter(|name| !before.contains(name)).collect::<Vec<_>>();
	created.sort();
	assert_eq!(created, ["LOCK", "LOCK.holder"], "a refused open leaves the data directory exactly as it found it");
	assert!(!before.iter().any(|name| name == "STORE_FORMAT"), "the fixture never had a marker, and the refusal does not add one");

	// A second open refuses the same way: the refusal is not a state transition.
	let again = match SegmentStore::open(&root).await {
		Ok(_) => panic!("the same directory is refused twice"),
		Err(error) => error,
	};
	assert!(format!("{again:#}").contains("a WeftDB 0.1.x data directory"), "{again:#}");
}

/// One aspect's measured facts, as the integers a comparison is made of: segment count,
/// rows, nulls, bytes, unsorted segments, overlapping segments, then the timestamp span.
async fn measured(store: &SegmentStore, aspect: &str) -> Vec<u64> {
	let stats = store.aspect_stats(aspect).await.expect("the aspect is declared");
	let meta = store.aspect_metadata(aspect).await.expect("the aspect has metadata");
	let (lo, hi) = stats.time_range.expect("a sealed aspect covers a span");
	vec![stats.segment_count as u64, stats.total_rows, meta.total_nulls, stats.total_bytes, stats.unsorted_segments as u64, stats.overlapping_segments as u64, lo as u64, hi as u64]
}

/// The same eight facts as v0.1.0 wrote them into `aspect_stats.txt`.
fn recorded(recorded: &serde_json::Value, keys: [&str; 6]) -> Vec<u64> {
	let span = recorded["time_range"].as_array().expect("a recorded timestamp span");
	let numbers = |value: &serde_json::Value| value.as_u64().expect("a recorded count");
	let mut out = keys.map(|key| numbers(&recorded[key])).to_vec();
	out.extend(span.iter().map(numbers));
	out
}

/// The store-wide totals, from the same build of integers: aspects, segments, rows, nulls,
/// bytes, unsorted segments, span.
async fn measured_store(store: &SegmentStore) -> Vec<u64> {
	let stats = store.store_stats().await.expect("the store reports itself");
	let (lo, hi) = stats.time_range.expect("a store with segments covers a span");
	vec![stats.aspect_count as u64, stats.segment_count as u64, stats.total_rows, stats.total_nulls, stats.total_bytes, stats.unsorted_segments as u64, lo as u64, hi as u64]
}

/// The X-FX gate the plan asks for in one place: the measurements a v0.1.0 release counted
/// are the measurements this build counts after the layout-1 upgrade, and after the store
/// has been moved to another path. `aspect_stats.txt` and `stats.json` are v0.1.0's own
/// numbers, so the comparison is with the release rather than with this build talking to
/// itself. Bytes are part of the comparison on purpose: nothing in this build's migration
/// re-encodes a frame, so a total that moves is a frame that changed.
#[tokio::test]
async fn the_migrated_measurements_are_the_ones_v010_counted() {
	let stats_text = std::fs::read_to_string(fixture().join("aspect_stats.txt")).expect("v0.1.0 recorded its own per-aspect stats");
	let per_aspect: BTreeMap<String, serde_json::Value> = stats_text
		.lines()
		.filter(|line| !line.starts_with('#') && !line.trim().is_empty())
		.map(|line| {
			let value: serde_json::Value = serde_json::from_str(line.trim()).expect("a recorded stats line");
			(value["aspect"].as_str().expect("a named stats row").to_owned(), value)
		})
		.collect();
	assert_eq!(per_aspect.keys().collect::<Vec<_>>(), ["flow", "price", "temp"], "the fixture records stats for exactly the three aspects it wrote");
	let store_totals: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(fixture().join("stats.json")).expect("v0.1.0 recorded its own store totals")).expect("the store totals are JSON");

	let dir = tempfile::tempdir().unwrap();
	let from = dir.path().join("original");
	let to = dir.path().join("relocated");
	copy_tree(&fixture().join("store"), &from);

	let store = SegmentStore::open(&from).await.expect("a v0.1.0 store opens");
	let report = store.open_report();
	assert_eq!(report.migrated_from, Some(1), "the store was upgraded from a pre-marker layout");
	assert!(!report.applied.is_empty(), "the upgrade ran here, so it is named: {:?}", report.applied);
	for aspect in ["flow", "price", "temp"] {
		assert_eq!(measured(&store, aspect).await, recorded(&per_aspect[aspect], ["segment_count", "total_rows", "total_nulls", "total_bytes", "unsorted_segments", "overlapping_segments"]), "{aspect}: this build counts what v0.1.0 counted");
	}
	assert_eq!(measured_store(&store).await, recorded(&store_totals, ["aspect_count", "segment_count", "total_rows", "total_nulls", "total_bytes", "unsorted_segments"]), "the store totals are the ones v0.1.0 printed");
	// Bytes per point is a division rather than a count, so it is compared loosely: the
	// recorded value is v0.1.0's float, and a re-derived frame would move it by more than
	// this while a different rounding of the same quotient cannot.
	for aspect in ["flow", "price", "temp"] {
		let recorded = per_aspect[aspect]["bytes_per_point"].as_f64().expect("a recorded ratio");
		let measured = store.aspect_stats(aspect).await.expect("the aspect is declared").bytes_per_point;
		assert!((recorded - measured).abs() < 1e-9, "{aspect}: {measured} against the recorded {recorded}");
	}
	// The value span is recorded as scaled decimal text, which is what makes it comparable
	// through `canonical`: 100.2500 and 100.25 are the same measurement.
	for aspect in ["flow", "price", "temp"] {
		let want = (canonical(per_aspect[aspect]["value_range"][0].as_str().expect("a recorded low")), canonical(per_aspect[aspect]["value_range"][1].as_str().expect("a recorded high")));
		let got = store.aspect_metadata(aspect).await.expect("the aspect has metadata").value_range.map(|(lo, hi)| (canonical(&lo.to_string()), canonical(&hi.to_string())));
		assert_eq!(got.as_ref(), Some(&want), "{aspect}: the value range survives the upgrade");
	}

	// The upgrade settles: it ran once, and the tree it left behind is what a later start
	// reads without running anything again.
	drop(store);
	std::fs::rename(&from, &to).expect("the store moves");
	let moved = SegmentStore::open(&to).await.expect("the same store opens at a new path");
	let report = moved.open_report();
	assert_eq!(report.migrated_from, None, "a migrated store does not migrate again");
	assert!(report.applied.is_empty(), "nothing ran on the second open: {:?}", report.applied);
	for aspect in ["flow", "price", "temp"] {
		assert_eq!(measured(&moved, aspect).await, recorded(&per_aspect[aspect], ["segment_count", "total_rows", "total_nulls", "total_bytes", "unsorted_segments", "overlapping_segments"]), "{aspect}: a moved store counts the same measurements");
	}
	assert_eq!(measured_store(&moved).await, recorded(&store_totals, ["aspect_count", "segment_count", "total_rows", "total_nulls", "total_bytes", "unsorted_segments"]), "the moved store's totals are still v0.1.0's");
	// The frames themselves: the segment files under the moved root are the ones the fixture
	// shipped, which is why the byte totals above can be trusted as identity.
	let frames = files_under(&to).into_iter().filter(|name| name.ends_with(".weftseg")).collect::<Vec<_>>();
	assert_eq!(frames.iter().filter(|name| name.starts_with("segments/")).count(), 6, "the upgrade moved six frames forward and did not rewrite or add any: {frames:?}");
}

/// One row of `rows.csv`: subject, aspect, the instant in **seconds** as the grid spells it,
/// and the value — `None` for the empty field that means a null in this file.
fn grid() -> Vec<(String, String, i64, Option<String>)> {
	let text = std::fs::read_to_string(fixture().join("rows.csv")).expect("the shared grid is part of the fixture");
	let mut rows = Vec::new();
	for (number, line) in text.lines().enumerate() {
		if line.is_empty() {
			continue;
		}
		let fields: Vec<&str> = line.split(',').collect();
		let [subject, aspect, when, value] = fields.as_slice() else {
			panic!("grid line {}: expected subject,aspect,timestamp,value in {line:?}", number + 1);
		};
		let Ok(when) = when.parse::<i64>() else {
			panic!("grid line {}: timestamp {when:?} is not an integer", number + 1);
		};
		rows.push((subject.to_string(), aspect.to_string(), when, (!value.is_empty()).then(|| value.to_string())));
	}
	rows
}

/// The second half of FRE-14: the rows-mode tree is not only refused on `open`, it can be
/// **read** — [`weftdb::rows_mode`] walks the tree the release wrote and `import_into` carries
/// one subject's series into a store scoped to that database. FRE-14c, the tooling an operator
/// runs, is still open; this is the ground it gets built on, driven by the release's bytes
/// rather than by a tree this build invented.
///
/// The scale assertions are why this runs against the fixture and not only against synthetic
/// trees. rows-mode stamped `timestamp_millis()` into `timestamp`
/// (`v0.1.0:weftdb/src/types/database/inputs.rs`, `capture_measurement`), while the Storage-v2
/// batches fed from **the same grid** kept seconds
/// (`v0.1.0:weftdb/src/types/batches/batch.rs`: `BigDecimal::from_i64(….timestamp())`) — which
/// is why the migrated store above declares [`TimeUnit::Seconds`] while an import of this tree
/// declares `Millis`. One grid, two scales, a factor of a thousand apart: declare the wrong one
/// and every migrated row lands six decades early, with nothing to show it.
#[tokio::test]
async fn the_v010_rows_mode_tree_reads_into_a_store_of_its_own_database() {
	let source = fixture().join("legacy/v010_legacy");
	let dir = tempfile::tempdir().unwrap();
	let tree = dir.path().join("v010_legacy");
	copy_tree(&source, &tree);

	let found = weftdb::rows_mode::discover(&tree).await.expect("the tree the release wrote is a tree this build can read");
	assert_eq!(found.database, "v010_legacy", "0.1.x named its database by that top directory, and that name is what the store's scope takes");
	assert_eq!(found.subjects(), ["plant"], "the fixture's tree holds one subject, named by its directory under the root");

	// Row counts and spans are derived from the grid, not hard-coded, so a regenerated grid
	// shows up here rather than in a diff of a Turso file nobody can read.
	let grid = grid();
	for aspect in ["flow", "price", "temp"] {
		let carried: Vec<&(String, String, i64, Option<String>)> = grid.iter().filter(|(_, name, _, value)| name == aspect && value.is_some()).collect();
		let series = found.aspects.iter().find(|series| series.name == aspect).unwrap_or_else(|| panic!("the tree holds a series of {aspect}"));
		assert_eq!(series.subject, "plant", "the aspect's subject is the directory it sits in — 0.1.x wrote no subject into `measurements.db`");
		assert_eq!(usize::try_from(series.rows).expect("a row count in range"), carried.len(), "{aspect}: rows-mode carries the grid's non-null rows and skips the null ones rather than writing a sentinel");
		let (first, last) = carried.iter().fold((i64::MAX, i64::MIN), |(lo, hi), (_, _, when, _)| (lo.min(*when), hi.max(*when)));
		assert_eq!((series.first_timestamp, series.last_timestamp), (Some(first * 1000), Some(last * 1000)), "{aspect}: the tree's `timestamp` is milliseconds where the grid says seconds — see the test's doc");
	}

	// What stays behind is named, per path, rather than dropped quietly.
	let refused: BTreeSet<&str> = found.not_carried.iter().map(|entry| entry.path.as_str()).collect();
	assert!(refused.contains("metadata.db"), "the 0.1.x catalog stays behind, and says why: {refused:?}");
	assert!(refused.contains("plant/price/dictionaries"), "an aspect's compression dictionaries stay behind: {refused:?}");
	for file in files_under(&tree) {
		if !file.ends_with(".db") || file.ends_with("/measurements.db") {
			continue;
		}
		assert!(refused.contains(file.as_str()) || refused.iter().any(|entry| file.starts_with(&format!("{entry}/"))), "{file} is a database in the tree that the readable leg neither carries nor names");
	}
	assert!(found.not_carried.iter().all(|entry| !entry.reason.is_empty()), "every path left behind states why, in the words of the subsystem that owned it");

	let store = SegmentStore::open_scoped(dir.path().join("plant-store"), "v010_legacy", "plant").await.expect("a store scoped to the tree's own database and subject opens");
	let record = weftdb::rows_mode::import_into(&store, &tree, "plant", NonZeroUsize::new(100).unwrap()).await.expect("the subject's series reads into the store of its own database");
	assert_eq!(record.timestamp_unit, weftdb::TIMESTAMP_UNIT, "the import declares the unit the timestamps carry; it does not infer it from how big the numbers look");
	assert_eq!(record.database, "v010_legacy");
	assert_eq!(record.subject, "plant");
	assert_eq!(record.aspects.len(), 3, "one entry per aspect the tree carried");
	assert_eq!(record.rows(), 371, "237 price + 54 temp + 80 flow: the grid's non-null rows, none lost and none added");
	for aspect in found.for_subject("plant") {
		let entry = record.aspects.iter().find(|entry| entry.aspect == aspect.name).unwrap_or_else(|| panic!("the record covers {}", aspect.name));
		assert_eq!(entry.rows, aspect.rows, "{}: the record counts what the survey counted", aspect.name);
		assert!(entry.segments >= 1, "{}: the rows were sealed", aspect.name);
		assert!(!entry.encoding.is_empty(), "{}: the record names the encoding its values were declared in", aspect.name);
	}

	// Values and instants, exactly, for all three aspects: the grid row for row, at a
	// thousand times its spelled instant. Sorted on both sides, because two rows of `price`
	// share the epoch and the store does not promise an order between equals.
	for aspect in ["flow", "price", "temp"] {
		let (timestamps, values) = store.read_time_range(aspect, i64::MIN, i64::MAX).await.expect("the migrated aspect reads back");
		let mut expected: Vec<String> = grid.iter().filter(|(_, name, _, value)| name == aspect && value.is_some()).map(|(_, _, when, value)| format!("{}|{}", when * 1000, canonical(value.as_deref().expect("a carried row has a value")))).collect();
		expected.sort();
		let mut actual: Vec<String> = timestamps.iter().zip(&values).map(|(when, value)| format!("{when}|{}", value.as_ref().map(|value| canonical(&value.to_string())).unwrap_or_default())).collect();
		actual.sort();
		assert_eq!(actual, expected, "{aspect}: every value of the tree arrives at the same instant, and no decimal was traded for a nearby one");
	}

	// The tree cannot be read twice into the same store: after the first import there is no
	// way to tell a migrated row from one the store sealed itself.
	let again = weftdb::rows_mode::import_into(&store, &tree, "plant", NonZeroUsize::new(100).unwrap()).await.expect_err("reading the same tree twice would double the rows, and nothing would say so");
	assert!(matches!(again.downcast_ref::<weftdb::RowsModeError>(), Some(weftdb::RowsModeError::AlreadyImported { aspects: 3, .. })), "the refusal is typed and counts what it refused to write again: {again}");
	let stored = weftdb::rows_mode::read_import_record(&store).await.expect("the record reads back").expect("the import left its record in the store's own index");
	assert_eq!(stored.source, tree.to_string_lossy(), "the record names the tree it read");
	assert_eq!(stored.rows(), 371, "the record's own account adds up to what the store holds");
	assert_eq!(store.read_time_range("price", i64::MIN, i64::MAX).await.expect("the aspect still reads").0.len(), 237, "the refusal added nothing");

	// Reading must not write. `discover` opens every `measurements.db`, and each of the
	// release's databases sits beside a `-wal` and a `-log`: a reader that checkpoints them
	// would change the very bytes the fixture gate compares. The test works on a copy, and
	// this says the copy is still what the release left, file for file and byte for byte.
	let files = files_under(&tree);
	assert!(!files.is_empty(), "the copied tree has files to compare");
	for name in files {
		let (fixture_bytes, copy_bytes) = (std::fs::read(source.join(&name)).expect("the fixture holds the file"), std::fs::read(tree.join(&name)).expect("the copy holds the file"));
		assert_eq!(fixture_bytes.len(), copy_bytes.len(), "reading {name} changed its length: the readable leg writes to the tree it reads");
		assert_eq!(fixture_bytes, copy_bytes, "reading {name} changed its bytes: the readable leg writes to the tree it reads");
	}
}
