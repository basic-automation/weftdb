//! Crash-consistency S8: write-once maintenance for `reconcile_segment` and
//! `split_segment` (docs/design/crash-consistency.md sections 5.3-5.4).

use std::{path::Path, str::FromStr, sync::Arc};

use bigdecimal::BigDecimal;
use tempfile::TempDir;
use weft_physical_type::{timestamp::TimeUnit, AspectSchema, PhysicalType};

use super::{SegmentStore, SegmentStoreOptions, DEFAULT_DATABASE, DEFAULT_SUBJECT};
use crate::durable::{FaultPoint, SimFs, StoreFs};

fn bd(s: &str) -> BigDecimal {
	BigDecimal::from_str(s).expect("parses")
}

/// A lossless f64 schema in seconds.
fn schema() -> AspectSchema {
	AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)
}

/// The aspect every test here maintains.
const ASPECT: &str = "price";

/// The two maintenance operations S8 makes write-once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
	Reconcile,
	Split,
}

impl Op {
	const ALL: [Self; 2] = [Self::Reconcile, Self::Split];
}

/// A segment sealed before the maintained one, which no operation here touches.
const BEFORE: [(i64, &str); 3] = [(0, "1"), (10, "2"), (20, "3")];

/// The rows of the segment `op` maintains: out of order for a reconcile, in order for a
/// split (which [`SPLIT_AT`] carves after its second row).
fn target_rows(op: Op) -> Vec<(i64, &'static str)> {
	match op {
		Op::Reconcile => vec![(130, "7"), (100, "4"), (120, "6"), (110, "5")],
		Op::Split => vec![(100, "4"), (110, "5"), (120, "6"), (130, "7")],
	}
}

/// Where a split carves [`target_rows`].
const SPLIT_AT: i64 = 115;

/// Every row of the fixture, in time order: what the aspect holds before and after any
/// maintenance here, since neither operation changes logical content (I5).
fn expected_rows(op: Op) -> Vec<(i64, String)> {
	let mut rows: Vec<(i64, String)> = BEFORE.iter().copied().chain(target_rows(op)).map(|(t, v)| (t, v.to_string())).collect();
	rows.sort_by_key(|(t, _)| *t);
	rows
}

/// Seal the fixture into a new store at `root` with the real filesystem, returning the
/// id of the segment `op` maintains. The store is closed again.
async fn seal_fixture(root: &Path, op: Op) -> u64 {
	let store = SegmentStore::open(root).await.expect("opens");
	store.declare(ASPECT, &schema()).await.expect("declares");
	let (ts, vs): (Vec<i64>, Vec<BigDecimal>) = BEFORE.iter().map(|(t, v)| (*t, bd(v))).unzip();
	store.seal(ASPECT, &schema(), &ts, &vs).await.expect("seals the untouched segment");
	let (ts, vs): (Vec<i64>, Vec<BigDecimal>) = target_rows(op).into_iter().map(|(t, v)| (t, bd(v))).unzip();
	let target = store.seal(ASPECT, &schema(), &ts, &vs).await.expect("seals the maintained segment").id;
	drop(store);
	target
}

/// Run `op` on segment `target` of `store`, asserting that it changed something.
async fn run(store: &SegmentStore, op: Op, target: u64) -> anyhow::Result<()> {
	match op {
		Op::Reconcile => {
			let rewrote = store.reconcile_segment(ASPECT, target).await?;
			anyhow::ensure!(rewrote, "the reconcile found nothing to rewrite");
		}
		Op::Split => {
			let suffix = store.split_segment(ASPECT, target, SPLIT_AT).await?;
			anyhow::ensure!(suffix.is_some(), "the split found nothing to carve");
		}
	}
	Ok(())
}

/// Every row `store` reads for the aspect, in time order, as `(timestamp, value)`.
async fn rows_of(store: &SegmentStore) -> anyhow::Result<Vec<(i64, String)>> {
	let (ts, vs) = store.read_time_range(ASPECT, i64::MIN, i64::MAX).await?;
	let mut rows: Vec<(i64, String)> = ts.into_iter().zip(vs).map(|(t, v)| (t, v.map_or_else(|| "null".to_string(), |v| v.to_plain_string()))).collect();
	rows.sort_by_key(|(t, _)| *t);
	Ok(rows)
}

/// The exemption for the store's own Turso databases, directly under the root: Turso
/// FULL-syncs every COMMIT that returned, so a power-cut image copies them as they are.
fn live_database(rel: &Path) -> bool {
	rel.parent() == Some(Path::new("")) && SimFs::turso_file(rel)
}

/// Open the store at `root` with every durable write going through `fs`.
async fn open_on(fs: Arc<dyn StoreFs>, root: &Path) -> SegmentStore {
	SegmentStore::open_on(fs, root, DEFAULT_DATABASE, DEFAULT_SUBJECT, SegmentStoreOptions::default()).await.expect("opens on the simulated filesystem")
}

/// The `.weftseg` files directly under `root/segments`, by name.
fn frames_in(root: &Path) -> Vec<String> {
	let mut names: Vec<String> = std::fs::read_dir(root.join("segments")).expect("lists segments/").map(|entry| entry.expect("an entry").file_name().to_string_lossy().into_owned()).filter(|name| name.ends_with(".weftseg")).collect();
	names.sort();
	names
}

/// Crash-consistency S8, windows powerloss-inplace-reconcile and
/// powerloss-split-halves-unordered: a reconcile and a split that returned success
/// survive a power cut right after with every row, each exactly once, in every image
/// sixteen seeds give. Both used to rewrite the segment's frame in place with an
/// unsynced `tokio::fs::write` (and a split wrote its suffix the same way), so an image
/// could hold the frame empty, torn or zero-filled under a committed row: its rows lost
/// and every read over them failing.
#[tokio::test]
async fn a_power_cut_after_a_reported_reconcile_or_split_loses_no_row() {
	for op in Op::ALL {
		let dir = TempDir::new().expect("tempdir");
		let root = dir.path().join("store");
		let target = seal_fixture(&root, op).await;
		// Everything sealed so far is the durable baseline.
		let sim = Arc::new(SimFs::exempting(&root, live_database).expect("simulates the root"));
		let store = open_on(sim.clone(), &root).await;
		run(&store, op, target).await.unwrap_or_else(|e| panic!("{op:?}: {e:#}"));
		drop(store);
		for seed in 0..16 {
			let image = TempDir::new().expect("tempdir");
			sim.power_cut(seed, image.path()).expect("cuts the power");
			let store = SegmentStore::open(image.path()).await.unwrap_or_else(|e| panic!("{op:?}, seed {seed}: the image opens: {e:#}"));
			let rows = rows_of(&store).await.unwrap_or_else(|e| panic!("{op:?}, seed {seed}: the image reads: {e:#}"));
			drop(store);
			assert_eq!(rows, expected_rows(op), "{op:?}, seed {seed}: every row survives, once ({:?})", frames_in(image.path()));
		}
	}
}

/// Crash-consistency S8 (design section 5.3, M2): a split's suffix takes an id above
/// every row, so it would outrank a newer segment that overlaps it. The split is refused
/// when a segment with a higher id overlaps `[boundary, max_ts]`, and the store is left
/// as it was.
#[tokio::test]
async fn a_split_is_refused_when_a_newer_segment_overlaps_its_suffix() {
	let dir = TempDir::new().expect("tempdir");
	let root = dir.path().join("store");
	let target = seal_fixture(&root, Op::Split).await;
	let store = SegmentStore::open(&root).await.expect("opens");
	// Newer than the target and inside [SPLIT_AT, 130]: the newer value wins at 120.
	store.seal(ASPECT, &schema(), &[120], &[bd("60")]).await.expect("seals a newer overlapping segment");
	let before_files = frames_in(&root);
	let before_ids: Vec<u64> = store.index().all(ASPECT).await.expect("reads").iter().map(|d| d.id).collect();
	let refused = store.split_segment(ASPECT, target, SPLIT_AT).await;
	let after_files = frames_in(&root);
	let journal = journal_of(&store).await;
	let at_120 = store.read_point(ASPECT, 120).await.expect("reads a point");
	let after_ids: Vec<u64> = store.index().all(ASPECT).await.expect("reads").iter().map(|d| d.id).collect();
	// A newer segment that overlaps only what stays in the prefix does not stand in the
	// way: the prefix keeps its lower id, so the newer value still wins at 120.
	let past_it = store.split_segment(ASPECT, target, 125).await;
	let at_120_after = store.read_point(ASPECT, 120).await.expect("reads a point");
	let rows = rows_of(&store).await.expect("reads");
	drop(store);
	let err = refused.expect_err("the split is refused");
	assert!(format!("{err:#}").contains("overlaps"), "the error names the cause: {err:#}");
	assert_eq!(after_ids, before_ids, "a refused split changes no row");
	assert_eq!(after_files, before_files, "and leaves no frame behind");
	assert_eq!(journal, Vec::<(String, String)>::new(), "nor a journal row");
	assert_eq!(at_120, Some(bd("60")), "the newer value still wins");
	assert!(past_it.as_ref().is_ok_and(Option::is_some), "a split whose suffix [125, 130] overlaps nothing newer goes ahead: {past_it:?}");
	assert_eq!(at_120_after, Some(bd("60")), "and the newer value still wins after it");
	assert_eq!(rows.len(), expected_rows(Op::Split).len() + 1, "no row was lost or duplicated");
}

/// A reader looping `read_time_range` while reconciles and splits rewrite the aspect
/// never fails and never sees a row twice. Every operation swaps its outputs in with
/// one transaction, and the frames it retires stay on disk until no read that might
/// still open them is running (its reclaim pin). An in-place rewrite could be read half
/// written, and a split used to commit its suffix before shrinking its prefix, so a
/// read in between saw the suffix rows twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reader_looping_during_reconciles_and_splits_never_errors() {
	const ROUNDS: i64 = 24;
	const ROWS: i64 = 2_000;
	let dir = TempDir::new().expect("tempdir");
	let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
	store.declare(ASPECT, &schema()).await.expect("declares");
	let (ts, vs): (Vec<i64>, Vec<BigDecimal>) = BEFORE.iter().map(|(t, v)| (*t, bd(v))).unzip();
	store.seal(ASPECT, &schema(), &ts, &vs).await.expect("seals");
	let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
	let reader = tokio::spawn({
		let (store, stop) = (store.clone(), stop.clone());
		async move {
			let mut reads = 0_u64;
			while !stop.load(std::sync::atomic::Ordering::SeqCst) {
				let rows = rows_of(&store).await.map_err(|e| format!("read {reads} failed: {e:#}"))?;
				let distinct: std::collections::BTreeSet<i64> = rows.iter().map(|(t, _)| *t).collect();
				if distinct.len() != rows.len() {
					return Err(format!("read {reads} saw {} rows at {} timestamps", rows.len(), distinct.len()));
				}
				let batch_rows = usize::try_from(ROWS).expect("small");
				if (rows.len() - BEFORE.len()) % batch_rows != 0 {
					return Err(format!("read {reads} saw {} rows, not whole batches", rows.len()));
				}
				reads += 1;
			}
			Ok::<u64, String>(reads)
		}
	});
	for round in 0..ROUNDS {
		let base = 1_000 + round * 10 * ROWS;
		// Out of order: the batch's halves swapped.
		let ts: Vec<i64> = (ROWS / 2..ROWS).chain(0..ROWS / 2).map(|i| base + i * 10).collect();
		let vs: Vec<BigDecimal> = ts.iter().map(|t| BigDecimal::from(*t)).collect();
		let id = store.seal(ASPECT, &schema(), &ts, &vs).await.expect("seals").id;
		assert!(store.reconcile_segment(ASPECT, id).await.expect("reconciles"), "round {round}: the reconcile rewrote the batch");
		let suffix = store.split_segment(ASPECT, id, base + ROWS * 5).await.expect("splits");
		assert!(suffix.is_some(), "round {round}: the split carved the batch");
	}
	stop.store(true, std::sync::atomic::Ordering::SeqCst);
	let reads = reader.await.expect("the reader joins").unwrap_or_else(|e| panic!("{e}"));
	let rows = rows_of(&store).await.expect("reads");
	drop(store);
	assert!(reads > 0, "the reader read while the aspect was rewritten");
	assert_eq!(rows.len(), BEFORE.len() + usize::try_from(ROUNDS * ROWS).expect("small"), "every row reads back once");
}

/// The `frame_journal` rows of `store`'s index, as `(name, state)`, by name.
async fn journal_of(store: &SegmentStore) -> Vec<(String, String)> {
	let conn = store.index().database().connect().expect("connects");
	let mut rows = conn.query("SELECT name, state FROM frame_journal ORDER BY name", ()).await.expect("reads the journal");
	let mut out = Vec::new();
	while let Some(row) = rows.next().await.expect("reads a row") {
		out.push((row.get_value(0).expect("a name").as_text().cloned().expect("text"), row.get_value(1).expect("a state").as_text().cloned().expect("text")));
	}
	out
}

/// The file names the aspect's live rows reference, by name.
async fn referenced_frames(store: &SegmentStore) -> Vec<String> {
	let mut names: Vec<String> = store.index().all(ASPECT).await.expect("reads the index").iter().map(|d| d.path.rsplit(['/', '\\']).next().expect("a file name").to_string()).collect();
	names.sort();
	names
}

/// Set only in the child process [`a_reconcile_whose_segment_a_racing_squash_merged_fails_with_conflict`]
/// starts: the root to open.
const CONFLICT_CHILD_ROOT: &str = "WEFT_TEST_S8_CONFLICT_CHILD_ROOT";

/// The body that child runs, in a process of its own because it arms `M-planned`
/// in-process (a point every reconcile passes) with a pause.
///
/// A reconcile of segment 1 is parked once it has planned its output; meanwhile a squash
/// that does not take the aspect's maintenance lock (as a pre-S7 binary, or a bug, would
/// not) merges segments 0, 1 and 2 into 0. The reconcile then goes on, and its swap's
/// precondition on segment 1's `(id, gen, frame_crc)` fails: it returns `Conflict`,
/// writes nothing back, and leaves no output frame and no journal row behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conflict_child() {
	let Some(root) = std::env::var_os(CONFLICT_CHILD_ROOT) else { return };
	let store = Arc::new(SegmentStore::open(std::path::PathBuf::from(root)).await.expect("opens"));
	store.declare(ASPECT, &schema()).await.expect("declares");
	store.seal(ASPECT, &schema(), &[0, 10], &[bd("1"), bd("2")]).await.expect("seals segment 0");
	store.seal(ASPECT, &schema(), &[30, 20], &[bd("30"), bd("20")]).await.expect("seals segment 1, out of order");
	store.seal(ASPECT, &schema(), &[20, 40], &[bd("200"), bd("400")]).await.expect("seals segment 2");

	let point = crate::durable::FaultPoint::MPlanned;
	let resume = Arc::new(tokio::sync::Notify::new());
	let armed = crate::durable::fault::arm(point, crate::durable::fault::FaultAction::Pause(resume.clone()));
	let hits = crate::durable::fault::hits(point);
	let reconcile = tokio::spawn({
		let store = store.clone();
		async move { store.reconcile_segment(ASPECT, 1).await }
	});
	tokio::time::timeout(std::time::Duration::from_secs(30), crate::durable::fault::reached(point, hits + 1)).await.expect("the reconcile reaches M-planned");
	drop(armed);
	let squashed = store.squash_aspect_held(&crate::types::aspect_locks::MaintGuard::unlocked(ASPECT)).await.expect("the racing squash merges every segment into 0");
	resume.notify_one();
	let reconciled = reconcile.await.expect("the reconcile joins");

	let ids: Vec<u64> = store.index().all(ASPECT).await.expect("reads").iter().map(|d| d.id).collect();
	let rows = rows_of(&store).await.expect("reads");
	let journal = journal_of(&store).await;
	let referenced = referenced_frames(&store).await;
	let root = store.root().to_path_buf();
	drop(store);
	let err = reconciled.expect_err("the reconcile's swap finds segment 1 gone");
	let kind = err.chain().find_map(|cause| cause.downcast_ref::<crate::types::index_txn::IndexTxnError>()).map(|e| e.kind);
	assert_eq!(kind, Some(crate::types::index_txn::TxnErrorKind::Conflict), "{err:#}");
	assert_eq!(squashed, 2);
	assert_eq!(ids, vec![0], "no merged member came back");
	assert_eq!(rows, vec![(0, "1".to_string()), (10, "2".to_string()), (20, "200".to_string()), (30, "30".to_string()), (40, "400".to_string())], "every row reads once, the newer value at 20");
	assert_eq!(journal, Vec::<(String, String)>::new(), "the conflicted reconcile cleared its pending journal row");
	assert_eq!(frames_in(&root), referenced, "and removed its output frame");
}

/// Crash-consistency S8, window race-reconcile-resurrects-merged-member at the swap: a
/// reconcile whose segment a racing squash merged away fails with `Conflict` instead of
/// writing it back (see [`conflict_child`], which runs in a child process).
#[tokio::test]
async fn a_reconcile_whose_segment_a_racing_squash_merged_fails_with_conflict() {
	let dir = TempDir::new().expect("tempdir");
	let exe = std::env::current_exe().expect("finds the test binary");
	let out = tokio::process::Command::new(exe).args(["types::segment_store::swap_tests::conflict_child", "--exact", "--nocapture", "--test-threads=1"]).env(CONFLICT_CHILD_ROOT, dir.path()).output().await.expect("runs the child");
	assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).contains("1 passed"), "the reconcile failed with Conflict and left nothing behind: {}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
}

/// The maintenance and reaper fault points of design section 11, in the order a swap
/// reaches them.
const POINTS: [FaultPoint; 11] = [FaultPoint::MPlanned, FaultPoint::MPendingCommitted, FaultPoint::MOutputWritten, FaultPoint::MOutputSynced, FaultPoint::MDirSynced, FaultPoint::MSwapBegun, FaultPoint::MSwapPhantom, FaultPoint::MSwapped, FaultPoint::GUnlinked, FaultPoint::GDirSynced, FaultPoint::GJournalDeleted];

/// The points the process-abort part of the matrix covers: one in each stretch the
/// others separate (an output on disk but unsynced, every output durable, the swap's
/// transaction open, the swap committed, the reaper half way).
const ABORT_POINTS: [FaultPoint; 5] = [FaultPoint::MOutputWritten, FaultPoint::MDirSynced, FaultPoint::MSwapBegun, FaultPoint::MSwapPhantom, FaultPoint::GUnlinked];

/// Whether a swap stopped at `point` has committed.
fn swapped_by(point: FaultPoint) -> bool {
	POINTS.iter().position(|p| *p == point) >= POINTS.iter().position(|p| *p == FaultPoint::MSwapPhantom)
}

impl Op {
	fn name(self) -> &'static str {
		match self {
			Self::Reconcile => "reconcile",
			Self::Split => "split",
		}
	}

	fn parse(name: &str) -> Self {
		match name {
			"reconcile" => Self::Reconcile,
			"split" => Self::Split,
			other => panic!("unknown operation {other:?}"),
		}
	}

	/// How many outputs (and so generations) one run of the operation writes.
	const fn outputs(self) -> u64 {
		match self {
			Self::Reconcile => 1,
			Self::Split => 2,
		}
	}
}

/// Every invariant a store recovered from a crash during `op` at `point` must hold
/// (design section 3), checked by opening the store at `root`: it opens; it holds the
/// segment as it was if the swap had not committed and as the operation left it if it had;
/// every row reads back exactly once (I1, I5), through every read path (I2); the frame
/// journal is empty (I4); the frames in `segments/` are exactly those the live rows
/// reference, each as long as its row says (I4); the rollup is what the index says (I4).
/// Then the operation runs again (a no-op if it had committed): the rows still read back,
/// its retired frame is reaped, and it never reuses a generation the crashed run had
/// journaled (I6).
async fn verify_recovered(root: &Path, op: Op, point: FaultPoint, label: &str) {
	let store = SegmentStore::open(root).await.unwrap_or_else(|e| panic!("{label}: the store opens: {e:#}"));
	let expected = expected_rows(op);
	let instants: Vec<i64> = expected.iter().map(|(t, _)| *t).collect();
	let rows = rows_of(&store).await.unwrap_or_else(|e| panic!("{label}: read_time_range: {e:#}"));
	let points = store.read_points(ASPECT, &instants).await.unwrap_or_else(|e| panic!("{label}: read_points: {e:#}"));
	let mut each = Vec::new();
	for t in &instants {
		each.push(store.read_point(ASPECT, *t).await.unwrap_or_else(|e| panic!("{label}: read_point({t}): {e:#}")));
	}
	let (by_value, _) = store.read_value_range(ASPECT, &bd("-1000"), &bd("1000")).await.unwrap_or_else(|e| panic!("{label}: read_value_range: {e:#}"));
	let max = store.downsample_range(ASPECT, i64::MIN, i64::MAX, splimes::Resolution::Hours, &[weft_reduce::Aggregation::Max]).await.unwrap_or_else(|e| panic!("{label}: downsample_range: {e:#}"));
	let journal = journal_of(&store).await;
	let referenced = referenced_frames(&store).await;
	let index_rows = store.index().rows(ASPECT).await.expect("reads");
	// The rows record the root the frames were written under; an image lives elsewhere.
	let lengths: Vec<(String, u64, u64)> = index_rows
		.iter()
		.map(|row| {
			let name = row.desc.path.rsplit(['/', '\\']).next().expect("a file name");
			(name.to_string(), row.desc.byte_len, std::fs::metadata(root.join("segments").join(name)).map_or(0, |m| m.len()))
		})
		.collect();
	let swapped = index_rows.iter().any(|row| row.gen > 0);
	let rollup = store.aspect_metadata(ASPECT).await.expect("reads the rollup");
	let derived = crate::AspectMetadata::from_index(&store.index().load_index(ASPECT).await.expect("loads the index"));
	drop(store);
	let values: Vec<Option<BigDecimal>> = expected.iter().map(|(_, v)| Some(bd(v))).collect();
	assert_eq!(swapped, swapped_by(point), "{label}: the swap committed exactly when the crash came after its COMMIT");
	assert_eq!(rows, expected, "{label}: every row reads back exactly once");
	assert_eq!(points, values, "{label}: read_points");
	assert_eq!(each, values, "{label}: read_point");
	assert_eq!(by_value.len(), expected.len(), "{label}: read_value_range");
	assert_eq!(max.len(), 1, "{label}: one downsample bucket: {max:?}");
	assert_eq!(journal, Vec::<(String, String)>::new(), "{label}: the journal is empty after the reopen");
	assert_eq!(frames_in(root), referenced, "{label}: no frame but the live rows' is left");
	for (path, recorded, on_disk) in lengths {
		assert_eq!(recorded, on_disk, "{label}: {path} is as long as its row says");
	}
	assert_eq!(rollup, derived, "{label}: the rollup matches the index");

	// Maintenance goes on: the operation runs again on the recovered store.
	let store = SegmentStore::open(root).await.unwrap_or_else(|e| panic!("{label}: the store reopens: {e:#}"));
	let target = store.index().all(ASPECT).await.expect("reads").iter().find(|d| d.min_ts == Some(100)).map(|d| d.id).expect("the maintained segment");
	match op {
		Op::Reconcile => drop(store.reconcile_segment(ASPECT, target).await.unwrap_or_else(|e| panic!("{label}: a reconcile after recovery: {e:#}"))),
		Op::Split => drop(store.split_segment(ASPECT, target, SPLIT_AT).await.unwrap_or_else(|e| panic!("{label}: a split after recovery: {e:#}"))),
	}
	let rows = rows_of(&store).await.unwrap_or_else(|e| panic!("{label}: reads after the second run: {e:#}"));
	let journal = journal_of(&store).await;
	let gens: Vec<u64> = store.index().rows(ASPECT).await.expect("reads").iter().map(|row| row.gen).filter(|gen| *gen > 0).collect();
	let referenced = referenced_frames(&store).await;
	drop(store);
	assert_eq!(rows, expected, "{label}: and after the second run");
	assert_eq!(journal, Vec::<(String, String)>::new(), "{label}: the second run reaped what it retired");
	assert_eq!(frames_in(root), referenced, "{label}: and left no other frame");
	assert_eq!(gens.len(), usize::try_from(op.outputs()).expect("small"), "{label}: the operation's outputs are in: {gens:?}");
	// A run that journaled its outputs before the crash took generations 1 to `outputs`;
	// the persisted counter keeps the second run from naming a frame with one of them.
	let journaled = point != FaultPoint::MPlanned && !swapped_by(point);
	let floor = if journaled { 1 + op.outputs() } else { 1 };
	assert!(gens.iter().all(|gen| *gen >= floor), "{label}: generations {gens:?}, none below {floor}");
}

/// The child process every part of the crash matrix runs in, so that it can arm fault
/// points in-process (process-global) without failing a maintenance operation of another
/// test, and abort. Its spec is `<mode>:<op>`; its work directory the other variable.
const MATRIX_CHILD: &str = "WEFT_TEST_S8_MATRIX";
const MATRIX_DIR: &str = "WEFT_TEST_S8_MATRIX_DIR";

/// The body of every crash-matrix child (see [`MATRIX_CHILD`]):
///
/// - `err:<op>`: for each of [`POINTS`], a fresh store runs `op` with the point armed to
///   return an error, is dropped there (a process crash) and reopened, and recovers;
/// - `cut:<op>`: for each of [`POINTS`], a fresh store opened on [`SimFs`] runs `op`
///   parked at the point, the power is cut there with sixteen seeds, and every image
///   recovers;
/// - `abort:<op>`: a fresh store runs `op` under the parent's `WEFT_FAULT=<point>:abort`,
///   which never returns; the parent checks the store.
///
/// In a normal test run the variable is unset and this does nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn matrix_child() {
	use crate::durable::fault::{self, FaultAction};

	let Some(spec) = std::env::var_os(MATRIX_CHILD) else { return };
	crate::types::durable::fault::suppress_core_dump();
	let spec = spec.into_string().expect("a UTF-8 spec");
	let dir = std::path::PathBuf::from(std::env::var_os(MATRIX_DIR).expect("a work directory"));
	let (mode, op) = spec.split_once(':').expect("<mode>:<op>");
	let op = Op::parse(op);
	match mode {
		"err" => {
			for point in POINTS {
				let root = dir.join(point.to_string());
				let target = seal_fixture(&root, op).await;
				let store = SegmentStore::open(&root).await.expect("opens");
				let hits = fault::hits(point);
				let armed = fault::arm(point, FaultAction::ReturnErr);
				let result = run(&store, op, target).await;
				drop(armed);
				let reached = fault::hits(point) > hits;
				drop(store);
				assert!(reached, "{op:?}: the operation passes {point}");
				// The reaper's points fail only its own pass, which the swap logs; every point
				// before it fails the operation.
				let reaper = matches!(point, FaultPoint::GUnlinked | FaultPoint::GDirSynced | FaultPoint::GJournalDeleted);
				assert_eq!(result.is_ok(), reaper, "{op:?} at {point}: {result:?}");
				verify_recovered(&root, op, point, &format!("{op:?}, error at {point}")).await;
			}
		}
		"cut" => {
			for point in POINTS {
				let root = dir.join(point.to_string());
				let images = dir.join(format!("{point}-images"));
				let target = seal_fixture(&root, op).await;
				let sim = Arc::new(SimFs::exempting(&root, live_database).expect("simulates the root"));
				let store = Arc::new(open_on(sim.clone(), &root).await);
				let resume = Arc::new(tokio::sync::Notify::new());
				let hits = fault::hits(point);
				let armed = fault::arm(point, FaultAction::Pause(resume.clone()));
				let task = tokio::spawn({
					let store = store.clone();
					async move { run(&store, op, target).await }
				});
				tokio::time::timeout(std::time::Duration::from_secs(60), fault::reached(point, hits + 1)).await.unwrap_or_else(|_| panic!("{op:?}: the operation never reached {point}"));
				for seed in 0..16 {
					sim.power_cut(seed, &images.join(seed.to_string())).expect("cuts the power");
				}
				drop(armed);
				resume.notify_one();
				let finished = task.await.expect("the operation joins");
				drop(store);
				assert!(finished.is_ok(), "{op:?}: parked at {point} and let go, the operation completes: {finished:?}");
				for seed in 0..16 {
					verify_recovered(&images.join(seed.to_string()), op, point, &format!("{op:?}, power cut at {point}, seed {seed}")).await;
				}
			}
		}
		"abort" => {
			let root = dir.join("store");
			let target = seal_fixture(&root, op).await;
			let store = SegmentStore::open(&root).await.expect("opens");
			let result = run(&store, op, target).await;
			panic!("{op:?}: the operation was to abort at the armed point, and returned {result:?}");
		}
		other => panic!("unknown matrix mode {other:?}"),
	}
}

/// Run [`matrix_child`] with `spec` in `dir`, and `WEFT_FAULT` set to `fault` if given.
async fn run_matrix_child(spec: &str, dir: &Path, fault: Option<&str>) -> std::process::Output {
	let exe = std::env::current_exe().expect("finds the test binary");
	let mut command = tokio::process::Command::new(exe);
	command.args(["types::segment_store::swap_tests::matrix_child", "--exact", "--nocapture", "--test-threads=1"]).env(MATRIX_CHILD, spec).env(MATRIX_DIR, dir);
	if let Some(fault) = fault {
		command.env(crate::durable::fault::FAULT_ENV, fault);
	}
	command.output().await.expect("runs the child")
}

/// Assert that a matrix child ran its body and passed.
fn assert_passed(what: &str, out: &std::process::Output) {
	assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).contains("1 passed"), "{what}: {}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
}

/// Crash-consistency S8, the process-crash matrix: a reconcile and a split stopped by an
/// error at every `M-*` and `G-*` point, after which the store is dropped and reopened,
/// lose no row and duplicate none, every read works, the journal is empty after the
/// reopen, no frame is left that no row references, and maintenance goes on.
#[tokio::test]
async fn the_crash_matrix_with_errors_at_every_point_loses_nothing() {
	let dir = TempDir::new().expect("tempdir");
	let runs = Op::ALL.map(|op| {
		let dir = dir.path().join(op.name());
		async move {
			std::fs::create_dir_all(&dir).expect("creates the work directory");
			(op, run_matrix_child(&format!("err:{}", op.name()), &dir, None).await)
		}
	});
	for (op, out) in futures::future::join_all(runs).await {
		assert_passed(&format!("{op:?} with an error at each point"), &out);
	}
}

/// Crash-consistency S8, the power-loss matrix (and the regression test that fails on
/// main, with [`a_power_cut_after_a_reported_reconcile_or_split_loses_no_row`]): the power
/// is cut with sixteen seeds while a reconcile and a split are parked at every `M-*` and
/// `G-*` point, and every image recovers to the store before or after the operation,
/// whole.
#[tokio::test]
async fn the_crash_matrix_with_power_cuts_at_every_point_loses_nothing() {
	let dir = TempDir::new().expect("tempdir");
	let runs = Op::ALL.map(|op| {
		let dir = dir.path().join(op.name());
		async move {
			std::fs::create_dir_all(&dir).expect("creates the work directory");
			(op, run_matrix_child(&format!("cut:{}", op.name()), &dir, None).await)
		}
	});
	for (op, out) in futures::future::join_all(runs).await {
		assert_passed(&format!("{op:?} with power cuts at each point"), &out);
	}
}

/// Crash-consistency S8, the abort subset of the matrix: a reconcile and a split killed
/// by `abort()` at a point in each stretch of the protocol leave a store that recovers as
/// after an error there.
#[tokio::test]
async fn the_crash_matrix_with_aborts_loses_nothing() {
	let dir = TempDir::new().expect("tempdir");
	let mut runs = Vec::new();
	for op in Op::ALL {
		for point in ABORT_POINTS {
			let dir = dir.path().join(format!("{}-{point}", op.name()));
			runs.push(async move {
				std::fs::create_dir_all(&dir).expect("creates the work directory");
				let out = run_matrix_child(&format!("abort:{}", op.name()), &dir, Some(&format!("{point}:abort"))).await;
				(op, point, dir, out)
			});
		}
	}
	for (op, point, dir, out) in futures::future::join_all(runs).await {
		#[cfg(unix)]
		{
			use std::os::unix::process::ExitStatusExt;
			assert_eq!(out.status.signal(), Some(6), "{op:?} at {point}: the child was killed by SIGABRT: {}", String::from_utf8_lossy(&out.stderr));
		}
		assert!(!out.status.success(), "{op:?} at {point}: the child aborted");
		assert!(String::from_utf8_lossy(&out.stderr).contains(&format!("aborting at fault point {point}")), "{op:?} at {point}: {}", String::from_utf8_lossy(&out.stderr));
		verify_recovered(&dir.join("store"), op, point, &format!("{op:?}, abort at {point}")).await;
	}
}

/// The release plan's D-S8 amendment, at the store: a read in flight pins the frames it
/// may open, so the frame a reconcile retires meanwhile stays on disk (and journaled)
/// while it runs; the pin is RAII, so when the read's future is dropped unfinished (a
/// request deadline firing, here raced with `select!` so the test controls when; the
/// reaper's own tests drop one with `tokio::time::timeout`) the pin goes with it, and the
/// next reaper pass unlinks the frame and empties the journal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_in_flight_keeps_a_retired_frame_until_its_future_is_dropped() {
	let dir = TempDir::new().expect("tempdir");
	let root = dir.path().join("store");
	let target = seal_fixture(&root, Op::Reconcile).await;
	let store = Arc::new(SegmentStore::open(&root).await.expect("opens"));
	let old = format!("{ASPECT}-{target}.weftseg");
	let (pinned, is_pinned) = tokio::sync::oneshot::channel();
	let (deadline, expire) = tokio::sync::oneshot::channel::<()>();
	let read = tokio::spawn({
		let store = store.clone();
		async move {
			// What every read path does first, then a read that never finishes on its own.
			let read = async {
				let _pin = store.pin_reads();
				pinned.send(()).expect("reports the pin");
				std::future::pending::<()>().await;
			};
			tokio::select! {
				() = read => false,
				_ = expire => true,
			}
		}
	});
	is_pinned.await.expect("the read pinned");
	run(&store, Op::Reconcile, target).await.expect("reconciles");
	let kept = (frames_in(&root).contains(&old), journal_of(&store).await);
	let rows = rows_of(&store).await.expect("reads");
	let waiting = store.reap(super::MaintenanceWait::Skip).await.expect("reaps");
	deadline.send(()).expect("the deadline fires");
	let expired = read.await.expect("joins");
	let pins = store.reaper.epochs().pins();
	let reaped = store.reap(super::MaintenanceWait::Skip).await.expect("reaps");
	let after = (frames_in(&root).contains(&old), journal_of(&store).await);
	drop(store);
	assert_eq!(kept, (true, vec![(old.clone(), "retired".to_string())]), "the pinned read keeps the retired frame");
	assert_eq!(rows, expected_rows(Op::Reconcile), "the swap is visible to new reads");
	assert_eq!((waiting.frames_unlinked, waiting.frames_waiting), (0, 1), "a reaper pass leaves it while the read runs");
	assert!(expired, "the read was dropped unfinished");
	assert_eq!(pins, 0, "and its dropped future released the pin");
	assert_eq!((reaped.frames_unlinked, reaped.frames_waiting), (1, 0));
	assert_eq!(after, (false, Vec::new()), "the reaper took the frame and its row");
}

/// A backup's hold keeps every retired frame until it drops, pins or no pins.
#[tokio::test]
async fn a_backup_hold_keeps_retired_frames_until_it_drops() {
	let dir = TempDir::new().expect("tempdir");
	let root = dir.path().join("store");
	let target = seal_fixture(&root, Op::Split).await;
	let store = SegmentStore::open(&root).await.expect("opens");
	let old = format!("{ASPECT}-{target}.weftseg");
	let hold = store.hold_reaper();
	run(&store, Op::Split, target).await.expect("splits");
	let reaped_while_held = store.reap(super::MaintenanceWait::Skip).await.expect("reaps");
	let kept = frames_in(&root).contains(&old);
	drop(hold);
	let reaped = store.reap(super::MaintenanceWait::Skip).await.expect("reaps");
	let journal = journal_of(&store).await;
	let referenced = referenced_frames(&store).await;
	drop(store);
	assert_eq!((reaped_while_held.frames_unlinked, reaped_while_held.frames_waiting), (0, 1));
	assert!(kept, "the held frame stays");
	assert_eq!(reaped.frames_unlinked, 1);
	assert!(journal.is_empty());
	assert_eq!(frames_in(&root), referenced, "only the split's halves are left");
}

/// The release plan's D-S8 amendment: an unlink the filesystem refuses for now (a
/// Windows sharing violation: another process has the frame open; here a busy file) is
/// not an error. The reconcile succeeds, the frame stays journaled, and a later pass
/// unlinks it once the file is free.
#[tokio::test]
async fn a_busy_retired_frame_is_retried_later_not_failed() {
	let dir = TempDir::new().expect("tempdir");
	let root = dir.path().join("store");
	let target = seal_fixture(&root, Op::Reconcile).await;
	let fs = Arc::new(crate::types::reaper::testing::BusyFs::default());
	let old = format!("{ASPECT}-{target}.weftseg");
	fs.set_busy(&old, true);
	let store = open_on(fs.clone(), &root).await;
	let reconciled = run(&store, Op::Reconcile, target).await;
	let busy = (frames_in(&root).contains(&old), journal_of(&store).await);
	let waiting = store.reap(super::MaintenanceWait::Skip).await.expect("reaps");
	fs.set_busy(&old, false);
	let reaped = store.reap(super::MaintenanceWait::Skip).await.expect("reaps");
	let rows = rows_of(&store).await.expect("reads");
	let journal = journal_of(&store).await;
	drop(store);
	assert!(reconciled.is_ok(), "a busy frame does not fail the reconcile: {reconciled:?}");
	assert_eq!(busy, (true, vec![(old.clone(), "retired".to_string())]), "the busy frame stays journaled");
	assert!(waiting.failed.is_empty() && waiting.frames_waiting == 1 && waiting.frames_unlinked == 0, "still busy: retried later, not failed: {waiting:?}");
	assert_eq!(reaped.frames_unlinked, 1);
	assert!(!frames_in(&root).contains(&old) && journal.is_empty(), "once free, the frame is reaped");
	assert_eq!(rows, expected_rows(Op::Reconcile));
}

/// Set only in the child process [`a_split_is_refused_when_a_newer_segment_commits_before_its_swap`]
/// starts: the root to open.
const SPLIT_RACE_ROOT: &str = "WEFT_TEST_S8_SPLIT_RACE_ROOT";

/// The body that child runs, in a process of its own because it arms `M-dir-synced`
/// in-process with a pause. A split passes its precedence check on the rows it read, then
/// writes and syncs both halves; meanwhile a seal (which never waits for maintenance)
/// commits a newer segment overlapping the suffix. The swap re-checks under the commit
/// lock, refuses, and removes both halves and their journal rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn split_race_child() {
	use crate::durable::fault::{self, FaultAction};

	let Some(root) = std::env::var_os(SPLIT_RACE_ROOT) else { return };
	let root = std::path::PathBuf::from(root);
	let target = seal_fixture(&root, Op::Split).await;
	let store = Arc::new(SegmentStore::open(&root).await.expect("opens"));
	let point = FaultPoint::MDirSynced;
	let resume = Arc::new(tokio::sync::Notify::new());
	let hits = fault::hits(point);
	let armed = fault::arm(point, FaultAction::Pause(resume.clone()));
	let split = tokio::spawn({
		let store = store.clone();
		async move { store.split_segment(ASPECT, target, SPLIT_AT).await }
	});
	tokio::time::timeout(std::time::Duration::from_secs(30), fault::reached(point, hits + 1)).await.expect("the split reaches M-dir-synced");
	drop(armed);
	let written = frames_in(&root).len();
	store.seal(ASPECT, &schema(), &[120], &[bd("60")]).await.expect("a seal commits meanwhile");
	resume.notify_one();
	let refused = split.await.expect("joins");
	let journal = journal_of(&store).await;
	let referenced = referenced_frames(&store).await;
	let at_120 = store.read_point(ASPECT, 120).await.expect("reads");
	drop(store);
	let err = refused.expect_err("the split is refused at its swap");
	assert!(format!("{err:#}").contains("overlaps"), "{err:#}");
	assert_eq!(written, 4, "both halves had been written beside the two segments");
	assert!(journal.is_empty(), "their journal rows are gone: {journal:?}");
	assert_eq!(frames_in(&root), referenced, "and so are they");
	assert_eq!(at_120, Some(bd("60")), "the newer value wins");
}

/// Crash-consistency S8 (design section 5.3, M2): the precedence check of a split runs
/// again under the commit lock, against the rows its swap commits over (see
/// [`split_race_child`], which runs in a child process).
#[tokio::test]
async fn a_split_is_refused_when_a_newer_segment_commits_before_its_swap() {
	let dir = TempDir::new().expect("tempdir");
	let exe = std::env::current_exe().expect("finds the test binary");
	let out = tokio::process::Command::new(exe).args(["types::segment_store::swap_tests::split_race_child", "--exact", "--nocapture", "--test-threads=1"]).env(SPLIT_RACE_ROOT, dir.path().join("store")).output().await.expect("runs the child");
	assert_passed("a split racing a newer seal", &out);
}

/// A frame generation is never reissued (I6): the counter starts above the persisted
/// `aspect_seq.next_gen` and above every write-once frame of the aspect on disk, here a
/// frame of generation 7 that nothing references, and a reopened store goes on above
/// what it used.
#[tokio::test]
async fn generations_start_above_every_frame_on_disk_and_survive_a_reopen() {
	let dir = TempDir::new().expect("tempdir");
	let root = dir.path().join("store");
	let target = seal_fixture(&root, Op::Reconcile).await;
	std::fs::write(root.join("segments").join("price~g7~p0.weftseg"), b"litter").expect("writes a stray frame");
	let store = SegmentStore::open(&root).await.expect("opens");
	run(&store, Op::Reconcile, target).await.expect("reconciles");
	let first = store.index().rows(ASPECT).await.expect("reads").iter().map(|row| row.gen).max();
	let second = store.seal(ASPECT, &schema(), &[230, 210], &[bd("9"), bd("8")]).await.expect("seals").id;
	drop(store);
	let store = SegmentStore::open(&root).await.expect("reopens");
	run(&store, Op::Reconcile, second).await.expect("reconciles again");
	let gens: Vec<(u64, u64)> = store.index().rows(ASPECT).await.expect("reads").iter().map(|row| (row.desc.id, row.gen)).collect();
	drop(store);
	assert_eq!(first, Some(8), "above the stray generation 7");
	assert_eq!(gens, vec![(0, 0), (target, 8), (second, 9)], "the reopened store goes on from the persisted counter");
}

/// An aspect name that encodes past the file-name limit (160 bytes of non-ASCII letters,
/// every byte escaped) still reconciles and splits: its frames take the bounded, hashed
/// form of the name, and read back.
#[tokio::test]
async fn an_aspect_whose_encoded_name_is_too_long_still_reconciles_and_splits() {
	let dir = TempDir::new().expect("tempdir");
	let store = SegmentStore::open(dir.path()).await.expect("opens");
	let aspect = "Ä".repeat(80);
	assert_eq!(aspect.len(), crate::aspect_name::MAX_ASPECT_NAME_BYTES);
	store.declare(&aspect, &schema()).await.expect("declares");
	let id = store.seal(&aspect, &schema(), &[30, 10, 20, 40], &[bd("3"), bd("1"), bd("2"), bd("4")]).await.expect("seals").id;
	let reconciled = store.reconcile_segment(&aspect, id).await.expect("reconciles");
	let suffix = store.split_segment(&aspect, id, 25).await.expect("splits");
	let (ts, vs) = store.read_time_range(&aspect, i64::MIN, i64::MAX).await.expect("reads");
	let names: Vec<String> = store.index().all(&aspect).await.expect("reads").iter().map(|d| d.path.rsplit('/').next().expect("a name").to_string()).collect();
	drop(store);
	assert!(reconciled && suffix.is_some());
	assert_eq!(ts, vec![10, 20, 30, 40]);
	assert_eq!(vs.into_iter().flatten().map(|v| v.to_plain_string()).collect::<Vec<_>>(), vec!["1", "2", "3", "4"]);
	for name in names {
		assert!(name.len() <= 255 && name.contains("~h"), "{name} ({} bytes)", name.len());
	}
}

/// A swap's outputs get the sidecars the policy wants, written through the store's
/// filesystem under a temporary name and renamed into place, so none is left behind, and
/// each matches the frame its id now names.
#[tokio::test]
async fn swapped_outputs_get_fresh_sidecars_and_leave_no_temporary_file() {
	use splimes::Resolution;

	let dir = TempDir::new().expect("tempdir");
	let store = SegmentStore::open(dir.path()).await.expect("opens").with_partial_sidecar_policy(crate::PartialSidecarPolicy::at(Resolution::Minutes, 1));
	store.declare(ASPECT, &schema()).await.expect("declares");
	let id = store.seal(ASPECT, &schema(), &[130, 100, 120, 110], &[bd("7"), bd("4"), bd("6"), bd("5")]).await.expect("seals").id;
	assert!(store.reconcile_segment(ASPECT, id).await.expect("reconciles"));
	let suffix = store.split_segment(ASPECT, id, SPLIT_AT).await.expect("splits").expect("a real split");
	let mut matched = Vec::new();
	for descriptor in store.index().all(ASPECT).await.expect("reads") {
		matched.push((descriptor.id, store.load_partial_sidecar(ASPECT, &descriptor).await.expect("reads the sidecar").is_some_and(|sidecar| sidecar.matches(&descriptor))));
	}
	drop(store);
	let mut names: Vec<String> = std::fs::read_dir(dir.path().join("segments")).expect("lists").map(|e| e.expect("entry").file_name().to_string_lossy().into_owned()).collect();
	names.sort();
	assert_eq!(matched, vec![(id, true), (suffix, true)], "each half's sidecar matches its frame");
	assert!(names.iter().all(|name| !name.starts_with(".tmp-")), "no temporary sidecar is left: {names:?}");
	assert_eq!(names, vec![format!("{ASPECT}-{id}.weftpart"), format!("{ASPECT}-{suffix}.weftpart"), format!("{ASPECT}~g2~p0.weftseg"), format!("{ASPECT}~g3~p0.weftseg")], "only the halves and their sidecars");
}
