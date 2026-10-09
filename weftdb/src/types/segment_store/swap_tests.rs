//! Crash-consistency S8 and S9: write-once maintenance (docs/design/crash-consistency.md
//! sections 5.3-5.4) for `reconcile_segment` and `split_segment` (S8), and for the
//! overlap merge, squash and size-targeted compaction (S9).

use std::{path::Path, str::FromStr, sync::Arc};

use bigdecimal::BigDecimal;
use tempfile::TempDir;
use weft_physical_type::{timestamp::TimeUnit, AspectSchema, PhysicalType, SplitPolicy};

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

/// The maintenance operations that write once: S8's reconcile and split of one segment,
/// and S9's overlap merge (a full rewrite of a component, or one whose cold prefix is
/// split off), squash and size-targeted compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
	Reconcile,
	Split,
	OverlapFull,
	OverlapSplit,
	Squash,
	Compact,
}

impl Op {
	const ALL: [Self; 6] = [Self::Reconcile, Self::Split, Self::OverlapFull, Self::OverlapSplit, Self::Squash, Self::Compact];
}

/// A segment sealed before the maintained ones, which only a squash touches.
const BEFORE: [(i64, &str); 3] = [(0, "1"), (10, "2"), (20, "3")];

/// One segment a fixture seals: its rows, in the order sealed, and the page height it is
/// sealed paged at, if any.
struct Sealed {
	rows: &'static [(i64, &'static str)],
	paged: Option<usize>,
}

/// The segments of `op`'s fixture, in the order they are sealed (so in id order).
///
/// - Reconcile: [`BEFORE`] and an out-of-order segment.
/// - Split: [`BEFORE`] and a sorted segment, which [`SPLIT_AT`] carves after its second row.
/// - Overlap merge: [`BEFORE`], a paged segment and a newer one overlapping it (at 110 for
///   the full rewrite, at 130 for the split, whose cold prefix is then 100-120).
/// - Squash: [`BEFORE`], paged, and two newer segments sharing timestamp 110; all three
///   fold into one.
/// - Compaction toward 3 rows: [`BEFORE`] (3 rows, a group of its own) and two 2-row
///   segments sharing timestamp 110, the first paged, which coalesce.
///
/// In every merge the lowest-id member is paged, so its frame kind differs from the
/// single-block output's (design window merge-paged-target-format-wedge).
fn fixture(op: Op) -> Vec<Sealed> {
	let before = Sealed { rows: &BEFORE, paged: None };
	match op {
		Op::Reconcile => vec![before, Sealed { rows: &[(130, "7"), (100, "4"), (120, "6"), (110, "5")], paged: None }],
		Op::Split => vec![before, Sealed { rows: &[(100, "4"), (110, "5"), (120, "6"), (130, "7")], paged: None }],
		Op::OverlapFull => vec![before, Sealed { rows: &[(100, "4"), (110, "5"), (120, "6"), (130, "7")], paged: Some(2) }, Sealed { rows: &[(110, "50"), (140, "8")], paged: None }],
		Op::OverlapSplit => vec![before, Sealed { rows: &[(100, "4"), (110, "5"), (120, "6"), (130, "7")], paged: Some(2) }, Sealed { rows: &[(130, "70"), (140, "8")], paged: None }],
		Op::Squash => vec![Sealed { rows: &BEFORE, paged: Some(2) }, Sealed { rows: &[(100, "4"), (110, "5")], paged: None }, Sealed { rows: &[(110, "50"), (120, "6")], paged: None }],
		Op::Compact => vec![before, Sealed { rows: &[(100, "4"), (110, "5")], paged: Some(1) }, Sealed { rows: &[(110, "50"), (120, "6")], paged: None }],
	}
}

/// Where a split carves its segment.
const SPLIT_AT: i64 = 115;

/// The row target a compaction coalesces toward.
const COMPACT_TARGET: usize = 3;

/// Every row of `op`'s fixture, in time order and, at a timestamp two segments share, in
/// seal order: what a range read returns before `op` has merged anything.
fn pre_rows(op: Op) -> Vec<(i64, String)> {
	let mut rows: Vec<(i64, String)> = fixture(op).iter().flat_map(|sealed| sealed.rows.iter().map(|(t, v)| (*t, (*v).to_string()))).collect();
	rows.sort_by_key(|(t, _)| *t);
	rows
}

/// The newest value at each timestamp of `op`'s fixture, in time order: what the aspect
/// holds before and after any maintenance here, since none changes logical content (I5),
/// and what a range read returns once `op` has merged the overlaps away.
fn expected_rows(op: Op) -> Vec<(i64, String)> {
	let mut newest = std::collections::BTreeMap::new();
	for sealed in fixture(op) {
		for (t, v) in sealed.rows {
			newest.insert(*t, (*v).to_string());
		}
	}
	newest.into_iter().collect()
}

/// Seal `op`'s fixture into a new store at `root` with the real filesystem, returning the
/// id of its second segment (the one a reconcile or split maintains). The store is closed
/// again.
async fn seal_fixture(root: &Path, op: Op) -> u64 {
	let store = SegmentStore::open(root).await.expect("opens");
	store.declare(ASPECT, &schema()).await.expect("declares");
	let mut ids = Vec::new();
	for sealed in fixture(op) {
		let (ts, vs): (Vec<i64>, Vec<BigDecimal>) = sealed.rows.iter().map(|(t, v)| (*t, bd(v))).unzip();
		let descriptor = match sealed.paged {
			Some(rows_per_page) => store.seal_paged(ASPECT, &schema(), &ts, &vs, rows_per_page).await,
			None => store.seal(ASPECT, &schema(), &ts, &vs).await,
		};
		ids.push(descriptor.expect("seals a fixture segment").id);
	}
	drop(store);
	ids[1]
}

/// Run `op` on `store` (`target` names the segment a reconcile or split maintains),
/// asserting that it changed what it should.
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
		Op::OverlapFull => {
			let removed = store.reconcile_overlaps(ASPECT).await?;
			anyhow::ensure!(removed == 1, "the merge removed {removed} segments, not 1");
		}
		Op::OverlapSplit => {
			let removed = store.reconcile_overlaps_with_policy(ASPECT, SplitPolicy::new(1)).await?;
			let stats = store.aspect_stats(ASPECT).await?;
			anyhow::ensure!(removed == 0 && stats.segment_count == 3 && stats.overlapping_segments == 0, "the merge did not split the component in two: removed {removed}, {stats:?}");
		}
		Op::Squash => {
			let removed = store.squash_aspect(ASPECT).await?;
			anyhow::ensure!(removed == 2, "the squash removed {removed} segments, not 2");
		}
		Op::Compact => {
			let removed = store.squash_aspect_to_target_rows(ASPECT, COMPACT_TARGET).await?;
			anyhow::ensure!(removed == 1, "the compaction removed {removed} segments, not 1");
		}
	}
	Ok(())
}

/// Run `op` again on a recovered `store`: it does its work if the first run's swap had not
/// committed, and nothing (or a no-op rewrite) if it had.
async fn run_again(store: &SegmentStore, op: Op) -> anyhow::Result<()> {
	match op {
		Op::Reconcile | Op::Split => {
			let target = store.index().all(ASPECT).await?.iter().find(|d| d.min_ts == Some(100)).map(|d| d.id).ok_or_else(|| anyhow::anyhow!("the maintained segment is gone"))?;
			if op == Op::Reconcile {
				store.reconcile_segment(ASPECT, target).await?;
			} else {
				store.split_segment(ASPECT, target, SPLIT_AT).await?;
			}
		}
		Op::OverlapFull => drop(store.reconcile_overlaps(ASPECT).await?),
		Op::OverlapSplit => drop(store.reconcile_overlaps_with_policy(ASPECT, SplitPolicy::new(1)).await?),
		Op::Squash => drop(store.squash_aspect(ASPECT).await?),
		Op::Compact => drop(store.squash_aspect_to_target_rows(ASPECT, COMPACT_TARGET).await?),
	}
	Ok(())
}

/// Every row `store` reads for the aspect, in time order (a range read's own order at a
/// shared timestamp), as `(timestamp, value)`.
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

/// Crash-consistency S8 and S9, windows powerloss-inplace-reconcile,
/// powerloss-split-halves-unordered, inplace-reseal-torn and
/// powerloss-merge-members-deleted-target-not-durable: every maintenance operation that
/// returned success survives a power cut right after with every row, each newest value
/// once, in every image sixteen seeds give. Each used to rewrite a frame in place with an
/// unsynced `tokio::fs::write` (a merge also deleted its other members in commits of
/// their own), so an image could hold a frame empty, torn or zero-filled under a
/// committed row: its rows lost and every read over them failing.
#[tokio::test]
async fn a_power_cut_after_a_reported_maintenance_operation_loses_no_row() {
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
			Self::OverlapFull => "overlap-full",
			Self::OverlapSplit => "overlap-split",
			Self::Squash => "squash",
			Self::Compact => "compact",
		}
	}

	fn parse(name: &str) -> Self {
		Self::ALL.into_iter().find(|op| op.name() == name).unwrap_or_else(|| panic!("unknown operation {name:?}"))
	}

	/// How many outputs (and so generations) one run of the operation writes.
	const fn outputs(self) -> u64 {
		match self {
			Self::Reconcile | Self::OverlapFull | Self::Squash | Self::Compact => 1,
			Self::Split | Self::OverlapSplit => 2,
		}
	}
}

/// Every invariant a store recovered from a crash during `op` at `point` must hold
/// (design section 3), checked by opening the store at `root`: it opens; it holds the
/// segments as they were if the swap had not committed and as the operation left them if
/// it had; the newest value of every timestamp is unchanged (I5), through every read path
/// (I2), and a range read shows each row once, apart from the overlaps the fixture itself
/// holds before a merge (no row of a member is visible beside its output); the frame
/// journal is empty (I4); the frames in `segments/` are exactly those the live rows
/// reference, each as long as its row says (I4); the rollup is what the index says (I4).
/// Then the operation runs again (a no-op if it had committed): the rows still read back,
/// its retired frames are reaped, and it never reuses a generation the crashed run had
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
	let visible = if swapped { expected.clone() } else { pre_rows(op) };
	assert_eq!(swapped, swapped_by(point), "{label}: the swap committed exactly when the crash came after its COMMIT");
	assert_eq!(rows, visible, "{label}: every row reads back, none twice beside its merge");
	assert_eq!(points, values, "{label}: read_points");
	assert_eq!(each, values, "{label}: read_point");
	assert_eq!(by_value.len(), visible.len(), "{label}: read_value_range");
	assert_eq!(max.len(), 1, "{label}: one downsample bucket: {max:?}");
	assert_eq!(journal, Vec::<(String, String)>::new(), "{label}: the journal is empty after the reopen");
	assert_eq!(frames_in(root), referenced, "{label}: no frame but the live rows' is left");
	for (path, recorded, on_disk) in lengths {
		assert_eq!(recorded, on_disk, "{label}: {path} is as long as its row says");
	}
	assert_eq!(rollup, derived, "{label}: the rollup matches the index");

	// Maintenance goes on: the operation runs again on the recovered store.
	let store = SegmentStore::open(root).await.unwrap_or_else(|e| panic!("{label}: the store reopens: {e:#}"));
	run_again(&store, op).await.unwrap_or_else(|e| panic!("{label}: the operation after recovery: {e:#}"));
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
const MATRIX_CHILD: &str = "WEFT_TEST_SWAP_MATRIX";
const MATRIX_DIR: &str = "WEFT_TEST_SWAP_MATRIX_DIR";

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

/// Crash-consistency S8 and S9, the process-crash matrix: every maintenance operation (a
/// reconcile, a split, an overlap merge that rewrites its component and one that splits
/// it, a squash and a compaction, each merge over a paged lowest-id member) stopped by an
/// error at every `M-*` and `G-*` point, after which the store is dropped and reopened,
/// loses no row and shows none twice, every read works, the journal is empty after the
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

/// Crash-consistency S8 and S9, the power-loss matrix (and the regression test that fails
/// on main, with [`a_power_cut_after_a_reported_maintenance_operation_loses_no_row`]): the
/// power is cut with sixteen seeds while every maintenance operation is parked at every
/// `M-*` and `G-*` point, and every image recovers to the store before or after the
/// operation, whole.
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

/// Crash-consistency S8 and S9, the abort subset of the matrix: every maintenance
/// operation killed by `abort()` at a point in each stretch of the protocol leaves a store
/// that recovers as after an error there.
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

/// Seal `rows` into the aspect of `store`, as one segment.
async fn seal_rows(store: &SegmentStore, rows: &[(i64, &str)]) -> anyhow::Result<weft_physical_type::SegmentDescriptor> {
	let (ts, vs): (Vec<i64>, Vec<BigDecimal>) = rows.iter().map(|(t, v)| (*t, bd(v))).unzip();
	store.seal(ASPECT, &schema(), &ts, &vs).await
}

/// The value `store` reads at each of `rows`' timestamps, beside the value `rows` says.
async fn newest_values(store: &SegmentStore, rows: &[(i64, &str)]) -> (Vec<Option<BigDecimal>>, Vec<Option<BigDecimal>>) {
	let ts: Vec<i64> = rows.iter().map(|(t, _)| *t).collect();
	let read = store.read_points(ASPECT, &ts).await.expect("reads the points");
	(read, rows.iter().map(|(_, v)| Some(bd(v))).collect())
}

/// Set only in the child process
/// [`an_overlap_split_suffix_never_outranks_a_seal_that_commits_after_its_snapshot`]
/// starts: the directory to work in.
const SUFFIX_RACE_DIR: &str = "WEFT_TEST_S9_SUFFIX_RACE_DIR";

/// The cold base the suffix race merges, and the newer late batch overlapping its end,
/// whose start (130) is where the split policy cuts: the suffix is `[130, 140]`.
const COLD_BASE: [(i64, &str); 5] = [(90, "0"), (100, "1"), (110, "2"), (120, "3"), (130, "4")];
const LATE: [(i64, &str); 2] = [(130, "40"), (140, "5")];
/// The seal that races the merge, at the suffix's last timestamp.
const RACING: [(i64, &str); 1] = [(140, "500")];
/// What the aspect holds once the racing seal is in: its value wins at 140.
const RACED: [(i64, &str); 6] = [(90, "0"), (100, "1"), (110, "2"), (120, "3"), (130, "40"), (140, "500")];

/// The body that child runs, in a process of its own because it arms `S-frame-written`
/// and `M-planned` in-process with pauses. Two interleavings of an overlap merge that
/// splits its component (cold prefix `[90, 120]`, merged suffix `[130, 140]`) and a seal
/// that shares timestamp 140 with the suffix:
///
/// - the seal takes its id (2) and parks before committing; the merge runs; the seal
///   commits after it;
/// - the merge parks once it has planned (its snapshot read); the seal takes its id and
///   commits; the merge goes on.
///
/// Either way the seal committed after the merge read the index, so its value must win
/// at 140. The suffix used to take an id from the allocator when the merge wrote it
/// (`next_id`), above the seal's, and outrank it there; it now takes a member's id, and
/// the merge allocates none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn suffix_race_child() {
	use crate::durable::fault::{self, FaultAction};

	let Some(dir) = std::env::var_os(SUFFIX_RACE_DIR) else { return };
	let dir = std::path::PathBuf::from(dir);
	for (phase, point) in [("the seal took its id first", FaultPoint::SFrameWritten), ("the merge read the index first", FaultPoint::MPlanned)] {
		let root = dir.join(point.to_string());
		let store = Arc::new(SegmentStore::open(&root).await.expect("opens"));
		store.declare(ASPECT, &schema()).await.expect("declares");
		seal_rows(&store, &COLD_BASE).await.expect("seals the cold base");
		seal_rows(&store, &LATE).await.expect("seals the late batch");
		let resume = Arc::new(tokio::sync::Notify::new());
		let hits = fault::hits(point);
		let armed = fault::arm(point, FaultAction::Pause(resume.clone()));
		let (sealed, merged) = if point == FaultPoint::SFrameWritten {
			let seal = tokio::spawn({
				let store = store.clone();
				async move { seal_rows(&store, &RACING).await }
			});
			tokio::time::timeout(std::time::Duration::from_secs(30), fault::reached(point, hits + 1)).await.unwrap_or_else(|_| panic!("{phase}: the seal reaches {point}"));
			drop(armed);
			let merged = store.reconcile_overlaps_with_policy(ASPECT, SplitPolicy::new(1)).await;
			resume.notify_one();
			(seal.await.expect("the seal joins"), merged)
		} else {
			let merge = tokio::spawn({
				let store = store.clone();
				async move { store.reconcile_overlaps_with_policy(ASPECT, SplitPolicy::new(1)).await }
			});
			tokio::time::timeout(std::time::Duration::from_secs(30), fault::reached(point, hits + 1)).await.unwrap_or_else(|_| panic!("{phase}: the merge reaches {point}"));
			drop(armed);
			let sealed = seal_rows(&store, &RACING).await;
			resume.notify_one();
			(sealed, merge.await.expect("the merge joins"))
		};
		let (read, expected) = newest_values(&store, &RACED).await;
		let ids: Vec<u64> = store.index().all(ASPECT).await.expect("reads").iter().map(|d| d.id).collect();
		let next = seal_rows(&store, &[(1000, "9")]).await.expect("seals afterwards").id;
		let journal = journal_of(&store).await;
		let referenced = referenced_frames(&store).await;
		drop(store);
		assert_eq!(sealed.as_ref().map(|d| d.id).ok(), Some(2), "{phase}: the seal succeeded under id 2: {sealed:?}");
		assert_eq!(merged.as_ref().ok(), Some(&0), "{phase}: the merge split its two members into two outputs: {merged:?}");
		assert_eq!(read, expected, "{phase}: the seal, committed after the merge's snapshot, wins at 140");
		assert_eq!(ids, vec![0, 1, 2], "{phase}: the prefix and suffix took the members' ids 0 and 1");
		assert_eq!(next, 3, "{phase}: the merge allocated no id");
		assert_eq!(journal, Vec::<(String, String)>::new(), "{phase}: the merge reaped what it retired");
		assert_eq!(frames_in(&root), referenced, "{phase}: and left no other frame");
	}
}

/// Crash-consistency S9 (design section 5.3, M2; release plan D17, P1): an overlap merge's
/// split suffix never outranks a seal that commits after the merge read the index (see
/// [`suffix_race_child`], which runs in a child process). It used to take a fresh id from
/// the allocator, above the seal's.
#[tokio::test]
async fn an_overlap_split_suffix_never_outranks_a_seal_that_commits_after_its_snapshot() {
	let dir = TempDir::new().expect("tempdir");
	let exe = std::env::current_exe().expect("finds the test binary");
	let out = tokio::process::Command::new(exe).args(["types::segment_store::swap_tests::suffix_race_child", "--exact", "--nocapture", "--test-threads=1"]).env(SUFFIX_RACE_DIR, dir.path()).output().await.expect("runs the child");
	assert_passed("an overlap split racing a seal", &out);
}

/// Set only in the child process
/// [`a_seal_racing_a_two_member_squash_keeps_the_acknowledged_seal`] starts: the directory
/// to work in.
const SQUASH_RACE_DIR: &str = "WEFT_TEST_S9_SQUASH_RACE_DIR";

/// The two members of the squash the seal races, sharing timestamp 110.
const OLDER: [(i64, &str); 2] = [(100, "1"), (110, "2")];
const NEWER: [(i64, &str); 2] = [(110, "20"), (120, "3")];
/// The racing seal: it shares 120 with the newer member, and its value wins there.
const ACKED: [(i64, &str); 2] = [(120, "300"), (130, "4")];
/// What the aspect holds once the seal is in.
const SQUASHED_AND_ACKED: [(i64, &str); 4] = [(100, "1"), (110, "20"), (120, "300"), (130, "4")];

/// The body that child runs, in a process of its own because it arms fault points
/// in-process with pauses. A squash of two segments races a seal, which is never a member:
///
/// - the seal takes its id and parks before committing; the squash runs; the seal
///   commits after it;
/// - the squash parks at each step of its swap in turn (planned, outputs journaled, an
///   output written, outputs synced, swap transaction begun, swapped, reaping), and the
///   seal runs meanwhile: to completion, except where the squash holds the aspect's commit
///   lock (inside its swap transaction), where it commits right after the squash.
///
/// Every time, both succeed, the seal's batch reads back whole under its own id with its
/// value winning at 120, the squash left one segment for its two members, nothing is
/// left in the journal or on disk that no row references, and the rollup matches the
/// index. A squash used to merge into its lowest id in place and delete its other members
/// by id and their frames by `{aspect}-{id}` name, in commits of their own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn squash_race_child() {
	use crate::durable::fault::{self, FaultAction};

	let Some(dir) = std::env::var_os(SQUASH_RACE_DIR) else { return };
	let dir = std::path::PathBuf::from(dir);
	let points = [FaultPoint::SFrameWritten, FaultPoint::MPlanned, FaultPoint::MPendingCommitted, FaultPoint::MOutputWritten, FaultPoint::MDirSynced, FaultPoint::MSwapBegun, FaultPoint::MSwapped, FaultPoint::GUnlinked];
	for point in points {
		let label = format!("parked at {point}");
		let root = dir.join(point.to_string());
		let store = Arc::new(SegmentStore::open(&root).await.expect("opens"));
		store.declare(ASPECT, &schema()).await.expect("declares");
		seal_rows(&store, &OLDER).await.expect("seals the older member");
		seal_rows(&store, &NEWER).await.expect("seals the newer member");
		let resume = Arc::new(tokio::sync::Notify::new());
		let hits = fault::hits(point);
		let armed = fault::arm(point, FaultAction::Pause(resume.clone()));
		let reached = |what: &'static str| {
			let label = label.clone();
			async move { tokio::time::timeout(std::time::Duration::from_secs(30), fault::reached(point, hits + 1)).await.unwrap_or_else(|_| panic!("{label}: the {what} never got there")) }
		};
		let (sealed, squashed) = if point == FaultPoint::SFrameWritten {
			let seal = tokio::spawn({
				let store = store.clone();
				async move { seal_rows(&store, &ACKED).await }
			});
			reached("seal").await;
			drop(armed);
			let squashed = store.squash_aspect(ASPECT).await;
			resume.notify_one();
			(seal.await.expect("the seal joins"), squashed)
		} else {
			let squash = tokio::spawn({
				let store = store.clone();
				async move { store.squash_aspect(ASPECT).await }
			});
			reached("squash").await;
			drop(armed);
			let seal = tokio::spawn({
				let store = store.clone();
				async move { seal_rows(&store, &ACKED).await }
			});
			// Inside its swap transaction the squash holds the commit lock the seal needs.
			let sealed = if point == FaultPoint::MSwapBegun {
				resume.notify_one();
				seal.await.expect("the seal joins")
			} else {
				let sealed = tokio::time::timeout(std::time::Duration::from_secs(30), seal).await.unwrap_or_else(|_| panic!("{label}: the seal waited for the squash")).expect("the seal joins");
				resume.notify_one();
				sealed
			};
			(sealed, squash.await.expect("the squash joins"))
		};
		let (read, expected) = newest_values(&store, &SQUASHED_AND_ACKED).await;
		let rows = rows_of(&store).await.expect("reads");
		let ids: Vec<u64> = store.index().all(ASPECT).await.expect("reads").iter().map(|d| d.id).collect();
		store.reap(super::MaintenanceWait::Skip).await.expect("reaps");
		let journal = journal_of(&store).await;
		let referenced = referenced_frames(&store).await;
		let rollup = store.aspect_metadata(ASPECT).await.expect("reads the rollup");
		let derived = crate::AspectMetadata::from_index(&store.index().load_index(ASPECT).await.expect("loads the index"));
		drop(store);
		assert_eq!(sealed.as_ref().map(|d| d.id).ok(), Some(2), "{label}: the seal succeeded under id 2: {sealed:?}");
		assert_eq!(squashed.as_ref().ok(), Some(&1), "{label}: the squash folded its two members into one: {squashed:?}");
		assert_eq!(ids, vec![0, 2], "{label}: the squash's output and the seal");
		assert_eq!(read, expected, "{label}: the acknowledged seal reads back, its value winning at 120");
		let at: Vec<(i64, String)> = rows.iter().filter(|(t, _)| *t >= 120).cloned().collect();
		assert_eq!(at, vec![(120, "3".to_string()), (120, "300".to_string()), (130, "4".to_string())], "{label}: the seal's whole batch is there, beside the squash's row at 120");
		assert_eq!(journal, Vec::<(String, String)>::new(), "{label}: nothing is left journaled");
		assert_eq!(frames_in(&root), referenced, "{label}: nor on disk");
		assert_eq!(rollup, derived, "{label}: the rollup matches the index");
	}
}

/// Crash-consistency S9 (design section 11, "a seal racing a two-member squash is lost"):
/// a seal and a squash of two other segments interleaved at every step of the squash keep
/// the acknowledged seal (see [`squash_race_child`], which runs in a child process).
#[tokio::test]
async fn a_seal_racing_a_two_member_squash_keeps_the_acknowledged_seal() {
	let dir = TempDir::new().expect("tempdir");
	let exe = std::env::current_exe().expect("finds the test binary");
	let out = tokio::process::Command::new(exe).args(["types::segment_store::swap_tests::squash_race_child", "--exact", "--nocapture", "--test-threads=1"]).env(SQUASH_RACE_DIR, dir.path()).output().await.expect("runs the child");
	assert_passed("a seal racing a two-member squash", &out);
}

/// Crash-consistency S9 (design section 5.3, M2; release plan D17, P1): a merge's outputs
/// take its members' ids, the j-th output in time order the j-th smallest, with a new
/// generation each; the members left over are deleted. Each output's adoption order
/// (`prec`, in its row and its frame name) is the largest of its members' (a legacy
/// member counts as 0). Here an overlap component of three members, two of which record
/// adoption orders 3 and 7, splits into a cold prefix and a merged suffix: the prefix takes
/// id 0, the suffix id 1, segment 2 is deleted, both carry adoption order 7, and the merge
/// allocates no id. The suffix used to take a fresh id from the allocator, and every
/// output was a legacy row with no adoption order.
#[tokio::test]
async fn merge_outputs_take_the_lowest_member_ids_in_time_order_and_the_largest_member_prec() {
	use crate::types::index_txn::{IndexOp, IndexRow, IndexTxn};

	let dir = TempDir::new().expect("tempdir");
	let store = SegmentStore::open(dir.path()).await.expect("opens");
	store.declare(ASPECT, &schema()).await.expect("declares");
	seal_rows(&store, &COLD_BASE).await.expect("seals the cold base");
	seal_rows(&store, &LATE).await.expect("seals a late batch");
	seal_rows(&store, &[(135, "6"), (140, "50")]).await.expect("seals a later one");
	// What a write-once member records (seals record no adoption order until S10).
	for (id, prec) in [(1, 3), (2, 7)] {
		let row = store.index().rows(ASPECT).await.expect("reads").into_iter().find(|row| row.desc.id == id).expect("the segment");
		let ordered = IndexRow { prec: Some(prec), ..row.clone() };
		store.commit_index(&IndexTxn::new(vec![IndexOp::ReplaceExpected { aspect: ASPECT.to_string(), expected: row.version(), row: ordered }])).await.expect("records the adoption order");
	}
	let removed = store.reconcile_overlaps_with_policy(ASPECT, SplitPolicy::new(1)).await.expect("merges");
	let rows = store.index().rows(ASPECT).await.expect("reads");
	let (read, expected) = newest_values(&store, &[(90, "0"), (100, "1"), (110, "2"), (120, "3"), (130, "40"), (135, "6"), (140, "50")]).await;
	let next = seal_rows(&store, &[(1000, "9")]).await.expect("seals afterwards").id;
	drop(store);
	let shape: Vec<(u64, Option<i64>, Option<i64>, Option<u64>)> = rows.iter().map(|row| (row.desc.id, row.desc.min_ts, row.desc.max_ts, row.prec)).collect();
	assert_eq!(removed, 1, "three members, two outputs");
	assert_eq!(shape, vec![(0, Some(90), Some(120), Some(7)), (1, Some(130), Some(140), Some(7))], "the prefix took id 0 and the suffix id 1, both of adoption order 7");
	assert!(rows.iter().all(|row| row.gen > 0 && row.desc.path.ends_with("~p7.weftseg")), "both are write-once frames named for adoption order 7: {rows:?}");
	assert_eq!(read, expected, "the newest value wins everywhere");
	assert_eq!(next, 3, "the merge allocated no id");
}

/// A reader looping `read_time_range` while overlap merges and squashes rewrite the
/// aspect never fails, and never sees a member's rows beside the output that replaced it.
/// Each round seals a batch and a newer one overlapping its second half, then merges
/// them; every fourth round also squashes the aspect. A round therefore reads as its
/// first batch alone, both batches, or their merge, which holds each timestamp once.
/// A merge used to rewrite its lowest member in place and delete the others in commits of
/// their own, so a read in between saw the merged rows beside a member's (and one during
/// the rewrite could read a torn frame).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reader_looping_during_merges_never_errors_or_sees_a_member_beside_its_output() {
	const ROUNDS: i64 = 16;
	const ROWS: i64 = 1_000;
	const SPAN: i64 = 10 * 2 * ROWS;
	let dir = TempDir::new().expect("tempdir");
	let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
	store.declare(ASPECT, &schema()).await.expect("declares");
	let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
	let reader = tokio::spawn({
		let (store, stop) = (store.clone(), stop.clone());
		async move {
			let mut reads = 0_u64;
			while !stop.load(std::sync::atomic::Ordering::SeqCst) {
				let rows = rows_of(&store).await.map_err(|e| format!("read {reads} failed: {e:#}"))?;
				let mut rounds: std::collections::BTreeMap<i64, Vec<i64>> = std::collections::BTreeMap::new();
				for (t, _) in &rows {
					rounds.entry(t / SPAN).or_default().push(*t);
				}
				for (round, ts) in rounds {
					let count = i64::try_from(ts.len()).expect("small");
					let distinct = ts.iter().collect::<std::collections::BTreeSet<_>>().len();
					let merged = count == 3 * ROWS / 2 && distinct == ts.len();
					if count != ROWS && count != 2 * ROWS && !merged {
						return Err(format!("read {reads} saw {count} rows ({distinct} timestamps) of round {round}"));
					}
				}
				reads += 1;
			}
			Ok::<u64, String>(reads)
		}
	});
	for round in 0..ROUNDS {
		let base = round * SPAN;
		let first: Vec<i64> = (0..ROWS).map(|i| base + i * 10).collect();
		let second: Vec<i64> = (ROWS / 2..ROWS / 2 + ROWS).map(|i| base + i * 10).collect();
		for (ts, offset) in [(first, 0), (second, 1_000_000)] {
			let vs: Vec<BigDecimal> = ts.iter().map(|t| BigDecimal::from(t + offset)).collect();
			store.seal(ASPECT, &schema(), &ts, &vs).await.expect("seals");
		}
		assert_eq!(store.reconcile_overlaps(ASPECT).await.expect("merges"), 1, "round {round}: the two batches merged");
		if round % 4 == 3 {
			store.squash_aspect(ASPECT).await.expect("squashes");
		}
	}
	stop.store(true, std::sync::atomic::Ordering::SeqCst);
	let reads = reader.await.expect("the reader joins").unwrap_or_else(|e| panic!("{e}"));
	let rows = rows_of(&store).await.expect("reads");
	store.reap(super::MaintenanceWait::Skip).await.expect("reaps");
	let journal = journal_of(&store).await;
	drop(store);
	assert!(reads > 0, "the reader read while the aspect was rewritten");
	assert_eq!(rows.len(), usize::try_from(ROUNDS * 3 * ROWS / 2).expect("small"), "every timestamp reads back once");
	assert_eq!(journal, Vec::<(String, String)>::new(), "once no read runs, the reaper takes every retired frame");
}

/// Release plan D17: a plan's cuts are timestamps, so each falls between two distinct
/// timestamps and a run of equal ones stays whole in the later output; stretches with no
/// rows are dropped, and a plan always has an output.
#[test]
fn a_cut_falls_between_timestamps_never_inside_a_run() {
	let rows = [10, 20, 20, 20, 30, 40];
	let shape = |cuts: &[i64]| -> Vec<Vec<i64>> {
		let values: Vec<Option<BigDecimal>> = rows.iter().map(|t| Some(BigDecimal::from(*t))).collect();
		let plan = super::SwapPlan::cut(Vec::new(), rows.to_vec(), values, None, cuts);
		assert!(plan.outputs.iter().all(|output| output.values.iter().zip(&output.timestamps).all(|(v, t)| v == &Some(BigDecimal::from(*t)))), "each value stays with its timestamp");
		plan.outputs.into_iter().map(|output| output.timestamps).collect()
	};
	assert_eq!(shape(&[]), vec![rows.to_vec()]);
	assert_eq!(shape(&[20]), vec![vec![10], vec![20, 20, 20, 30, 40]], "the run at 20 starts the later output");
	assert_eq!(shape(&[25]), vec![vec![10, 20, 20, 20], vec![30, 40]], "a cut between timestamps");
	assert_eq!(shape(&[99, 40, 20, 5, 20]), vec![vec![10], vec![20, 20, 20, 30], vec![40]], "cuts in any order; empty stretches dropped");
	let empty = super::SwapPlan::cut(Vec::new(), Vec::new(), Vec::new(), Some(4), &[10]);
	assert_eq!((empty.outputs.len(), empty.outputs[0].timestamps.len(), empty.outputs[0].rows_per_page), (1, 0, Some(4)), "no rows still makes one output");
}

/// Tags amendment A5: one swap transaction carries several independent plans of an
/// aspect, with one pending-journal commit and one swap commit (the aspect's epoch moves
/// once). Preconditions stay per member row: when one plan's member changed since the
/// plans were read, the joint transaction rolls back and each plan is swapped alone, so
/// the other still commits, and the stale one fails with `Conflict` and leaves no output
/// frame or journal row behind.
#[tokio::test]
async fn one_swap_carries_several_plans_and_swaps_each_alone_after_a_conflict() {
	use crate::types::index_txn::{IndexTxnError, TxnErrorKind};

	const GROUPS: [&[(i64, &str)]; 4] = [&[(0, "1"), (10, "2")], &[(10, "20"), (20, "3")], &[(100, "4"), (110, "5")], &[(120, "7"), (110, "50")]];
	let newest = [(0, "1"), (10, "20"), (20, "3"), (100, "4"), (110, "50"), (120, "7")];
	for stale in [false, true] {
		let dir = TempDir::new().expect("tempdir");
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare(ASPECT, &schema()).await.expect("declares");
		for rows in GROUPS {
			seal_rows(&store, rows).await.expect("seals");
		}
		// Two compaction groups: segments 0 and 1, and 2 and 3 (out of order).
		let rows = store.index().rows(ASPECT).await.expect("reads");
		let held = store.maintain(ASPECT).await.expect("holds the aspect");
		let mut plans = Vec::new();
		for members in [rows[..2].to_vec(), rows[2..].to_vec()] {
			let (ts, vs) = store.merge_members(&members).await.expect("merges");
			plans.push(super::SwapPlan::cut(members, ts, vs, None, &[]));
		}
		drop(held);
		if stale {
			// Segment 3 moves to a new generation under the second plan.
			assert!(store.reconcile_segment(ASPECT, 3).await.expect("reconciles"));
		}
		let held = store.maintain(ASPECT).await.expect("holds the aspect");
		let epoch = store.index().allocator_seed(ASPECT).await.expect("reads").epoch;
		let outcomes = store.swap_plans(&held, &schema(), plans).await.expect("the swap runs");
		let swapped_epoch = store.index().allocator_seed(ASPECT).await.expect("reads").epoch;
		drop(held);
		let ids: Vec<u64> = store.index().all(ASPECT).await.expect("reads").iter().map(|d| d.id).collect();
		let (read, expected) = newest_values(&store, &newest).await;
		let journal = journal_of(&store).await;
		let referenced = referenced_frames(&store).await;
		drop(store);
		let label = if stale { "a stale second plan" } else { "two fresh plans" };
		assert_eq!(outcomes.len(), 2, "{label}");
		assert_eq!(outcomes[0].as_ref().map(|swapped| swapped.ids.clone()).ok(), Some(vec![0]), "{label}: the first group swapped in under id 0");
		if stale {
			let err = outcomes[1].as_ref().err().unwrap_or_else(|| panic!("{label}: the stale plan fails"));
			let kind = err.chain().find_map(|cause| cause.downcast_ref::<IndexTxnError>()).map(|e| e.kind);
			assert_eq!(kind, Some(TxnErrorKind::Conflict), "{label}: {err:#}");
			assert_eq!(ids, vec![0, 2, 3], "{label}: the second group is as the reconcile left it");
			assert_eq!(swapped_epoch, epoch + 1, "{label}: only the first plan's own retry committed");
		} else {
			assert_eq!(outcomes[1].as_ref().map(|swapped| swapped.ids.clone()).ok(), Some(vec![2]), "{label}: the second group swapped in under id 2");
			assert_eq!(ids, vec![0, 2], "{label}");
			assert_eq!(swapped_epoch, epoch + 1, "{label}: both plans committed in one transaction");
		}
		assert_eq!(read, expected, "{label}: the newest value wins everywhere");
		assert_eq!(journal, Vec::<(String, String)>::new(), "{label}: no journal row is left");
		assert_eq!(frames_in(dir.path()), referenced, "{label}: nor a frame no row references");
	}
}

/// Set only in the child process [`a_dropped_swap_hands_its_outputs_to_the_reaper`]
/// starts: the directory to work in.
const DROPPED_SWAP_DIR: &str = "WEFT_TEST_S9_DROPPED_SWAP_DIR";

/// The body that child runs, in a process of its own because it arms fault points
/// in-process with pauses. A squash is parked after its outputs were journaled as pending
/// (`M-pending-committed`, before any is written; `M-dir-synced`, once they are written
/// and synced), and its task is aborted there, as a cancelled request's would be: the
/// future is dropped with the outputs neither swapped in nor discarded. The guard that
/// held them hands them to the reaper, and the next reaper pass over the aspect (an
/// explicit `reap`, or the start of the aspect's next swap) unlinks them and deletes their
/// journal rows; the store is as before the squash, which then runs again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropped_swap_child() {
	use crate::durable::fault::{self, FaultAction};

	let Some(dir) = std::env::var_os(DROPPED_SWAP_DIR) else { return };
	let dir = std::path::PathBuf::from(dir);
	for point in [FaultPoint::MPendingCommitted, FaultPoint::MDirSynced] {
		let root = dir.join(point.to_string());
		seal_fixture(&root, Op::Squash).await;
		let store = Arc::new(SegmentStore::open(&root).await.expect("opens"));
		let before = frames_in(&root);
		let resume = Arc::new(tokio::sync::Notify::new());
		let hits = fault::hits(point);
		let armed = fault::arm(point, FaultAction::Pause(resume.clone()));
		let squash = tokio::spawn({
			let store = store.clone();
			async move { store.squash_aspect(ASPECT).await }
		});
		tokio::time::timeout(std::time::Duration::from_secs(30), fault::reached(point, hits + 1)).await.unwrap_or_else(|_| panic!("the squash reaches {point}"));
		squash.abort();
		let cancelled = squash.await.expect_err("the squash was aborted").is_cancelled();
		drop(armed);
		let abandoned = store.reaper.abandoned(ASPECT);
		let journal = journal_of(&store).await;
		let rows = rows_of(&store).await.expect("reads");
		let written = frames_in(&root).len() - before.len();
		let removed = if point == FaultPoint::MPendingCommitted {
			let reaped = store.reap(super::MaintenanceWait::Skip).await.expect("reaps");
			assert_eq!((reaped.aspects_reaped, reaped.frames_unlinked), (1, 1), "{point}: the reaper took the abandoned output");
			assert_eq!((store.reaper.abandoned(ASPECT), journal_of(&store).await, frames_in(&root)), (Vec::new(), Vec::new(), before.clone()), "{point}: and left the store as before the squash");
			store.squash_aspect(ASPECT).await.expect("squashes again")
		} else {
			store.squash_aspect(ASPECT).await.expect("squashes again, settling the abandoned output first")
		};
		let after = (store.reaper.abandoned(ASPECT), journal_of(&store).await, rows_of(&store).await.expect("reads"));
		let referenced = referenced_frames(&store).await;
		drop(store);
		assert!(cancelled, "{point}: the squash's future was dropped");
		assert_eq!(abandoned.len(), 1, "{point}: the guard handed its one output to the reaper: {abandoned:?}");
		assert_eq!(journal, vec![(abandoned[0].clone(), "pending".to_string())], "{point}: still journaled as pending");
		assert_eq!(written, usize::from(point == FaultPoint::MDirSynced), "{point}: the output is on disk once written");
		assert_eq!(rows, pre_rows(Op::Squash), "{point}: nothing was swapped in");
		assert_eq!(removed, 2, "{point}: the squash runs again");
		assert_eq!(after, (Vec::new(), Vec::new(), expected_rows(Op::Squash)), "{point}: nothing is abandoned or journaled, and the rows are squashed");
		assert_eq!(frames_in(&root), referenced, "{point}: no frame is left but the live rows'");
	}
}

/// Release plan D-S9 (the robustness track's `PendingOutputs`, ROB-21): a swap's pending
/// outputs are held by a guard that hands them to the reaper when it is dropped unsettled,
/// here by a squash whose future is dropped half way (see [`dropped_swap_child`], which
/// runs in a child process).
#[tokio::test]
async fn a_dropped_swap_hands_its_outputs_to_the_reaper() {
	let dir = TempDir::new().expect("tempdir");
	let exe = std::env::current_exe().expect("finds the test binary");
	let out = tokio::process::Command::new(exe).args(["types::segment_store::swap_tests::dropped_swap_child", "--exact", "--nocapture", "--test-threads=1"]).env(DROPPED_SWAP_DIR, dir.path()).output().await.expect("runs the child");
	assert_passed("a squash dropped half way", &out);
}

/// A merge's outputs get the sidecars the policy wants, under the ids they take, and the
/// members it deleted lose theirs: after a squash of three segments only the output's
/// sidecar is left, matching the frame its id now names, and no temporary file.
#[tokio::test]
async fn a_merge_leaves_a_fresh_sidecar_for_its_output_and_none_for_the_members_it_deleted() {
	use splimes::Resolution;

	let dir = TempDir::new().expect("tempdir");
	let store = SegmentStore::open(dir.path()).await.expect("opens").with_partial_sidecar_policy(crate::PartialSidecarPolicy::at(Resolution::Minutes, 1));
	store.declare(ASPECT, &schema()).await.expect("declares");
	for rows in [&[(0, "1"), (10, "2")][..], &[(10, "20"), (20, "3")], &[(30, "4"), (40, "5")]] {
		seal_rows(&store, rows).await.expect("seals");
	}
	let before: Vec<String> = std::fs::read_dir(dir.path().join("segments")).expect("lists").map(|e| e.expect("entry").file_name().to_string_lossy().into_owned()).filter(|name| name.ends_with(".weftpart")).collect();
	assert_eq!(store.squash_aspect(ASPECT).await.expect("squashes"), 2);
	let descriptor = store.index().all(ASPECT).await.expect("reads").pop().expect("the output");
	let matched = store.load_partial_sidecar(ASPECT, &descriptor).await.expect("reads the sidecar").is_some_and(|sidecar| sidecar.matches(&descriptor));
	drop(store);
	let mut names: Vec<String> = std::fs::read_dir(dir.path().join("segments")).expect("lists").map(|e| e.expect("entry").file_name().to_string_lossy().into_owned()).collect();
	names.sort();
	assert_eq!(before.len(), 3, "each seal wrote a sidecar");
	assert!(matched, "the output's sidecar matches its frame");
	assert_eq!(names, vec![format!("{ASPECT}-0.weftpart"), format!("{ASPECT}~g1~p0.weftseg")], "only the output and its sidecar are left");
}
