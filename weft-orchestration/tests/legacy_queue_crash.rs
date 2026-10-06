//! Crash tests for the legacy (rows-mode) ingest queue and its consumer
//! (crash-consistency design, S18).
//!
//! - **Write-ahead enqueue** (legacy-measurements-committed-not-queued,
//!   legacy-capture-measurement-tail): ingest queues a batch's timestamps before it
//!   inserts any row, so the unbatched queue is always a superset of the rows that
//!   landed, and the incremental build covers whatever a failed call left behind.
//! - **Batch dedupe** (legacy-queue-consumer-batch-before-dequeue): a consumer that
//!   dies after inserting its batches but before dequeuing their timestamps rebuilds
//!   the same windows on its next run; storing a batch skips it when its hash is already
//!   queued or processed, so the re-run adds no duplicate batches and the pattern
//!   dictionary no duplicate occurrences.
//!
//! A failure is injected with `ReturnErr` at a fault point and the store then
//! "restarts": every handle is released and the database is reopened cold, so no cache
//! of the crashed run survives, as after a real process crash. The consumer crash is also
//! run as one: a re-executed child aborts at the point (`WEFT_FAULT=<point>:abort`).

use std::collections::{BTreeSet, HashSet};

use anyhow::{Context, Result};
use bigdecimal::BigDecimal;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serial_test::serial;
use splimes::{Resolution, Spline};
use uuid::Uuid;
use weft_orchestration::{batch_utils::build_incremental_unprocessed_queue, build_patterns_queue, build_processed_batch_queue};
use weftdb::{
	database::traits::{AspectStructure, DatabaseStructure, Inputs, Outputs}, durable::fault::{self, FaultAction, FaultPoint}, AspectId, BatchedMeasurement, Database, DatasetId, Dictionary, DictionaryConstraints, InputMeasurement, DATABASES
};

/// Points per batch: small, so a dozen rows make several sliding windows. The datasets
/// stay small because the consumer interpolates every affected window separately, which
/// is slow in a debug build.
const BATCH_SIZE: usize = 5;

/// Rows the consumer tests ingest.
const CONSUMER_ROWS: i64 = 12;

/// A batch row as stored: its `batch_hash` and the timestamps of its points.
type BatchRow = (String, Vec<DateTime<Utc>>);

struct Store {
	/// The data directory; removed when the store drops.
	dir: tempfile::TempDir,
	name: String,
	db: Database,
	aspect: AspectId,
}

/// A [`Store`] this process holds no handle on.
struct Closed {
	dir: tempfile::TempDir,
	name: String,
	aspect: AspectId,
}

impl Closed {
	async fn reopen(self) -> Result<Store> {
		let Self { dir, name, aspect } = self;
		let db = Database::existing(&name).await.context("cold reopen")?;
		Ok(Store { dir, name, db, aspect })
	}
}

impl Store {
	/// A fresh legacy database with one minute-resolution aspect, in its own data dir.
	async fn new(tag: &str) -> Result<Self> {
		let dir = tempfile::tempdir()?;
		std::env::set_var("TEST_DATA_DIR", dir.path().to_str().context("temp data dir is not valid UTF-8")?);
		let name = format!("{tag}_{}", Uuid::new_v4().simple());
		let db = Database::new(&name).await?;
		let subject = db.observe_subject("subject").await?;
		let aspect = db.track_aspect(&subject.id(), "aspect", &Resolution::Minutes, None).await?.id();
		Ok(Self { dir, name, db, aspect })
	}

	/// Release every handle and reopen cold, as a restarted process would: the crashed
	/// run's in-memory caches (for example the unprocessed-batch list) are gone.
	async fn restart(self) -> Result<Self> {
		self.close().await.reopen().await
	}

	/// Release every handle this process holds on the database, so that the next open goes
	/// back to disk, and another process can open it (Turso locks an open database file).
	///
	/// Turso shares one database per path through a registry of weak references, so the
	/// `Database`, its `DATABASES` entry (which owns the subject and aspect handles) and
	/// WeftDB's connection cache must all let go; otherwise a reopen would be handed the
	/// crashed run's live databases.
	async fn close(self) -> Closed {
		let Self { dir, name, db, aspect } = self;
		let id = db.id();
		drop(db);
		DATABASES.lock().await.remove(&id);
		weftdb::clear_connection_cache_by_name(&name).await;
		Closed { dir, name, aspect }
	}

	async fn build(&self) -> Result<()> {
		build_incremental_unprocessed_queue(&self.db, &self.aspect, &Resolution::Minutes, &Spline::Linear, BATCH_SIZE).await
	}

	async fn queued(&self) -> Result<BTreeSet<DateTime<Utc>>> {
		Ok(self.db.get_unbatched_measurements(&self.aspect).await?.into_iter().collect())
	}

	/// Every batch row in the unprocessed (or processed) batches DB, read with plain SQL
	/// so that no cache can hide a row.
	async fn batch_rows(&self, processed: bool) -> Result<Vec<BatchRow>> {
		let db = if processed { self.db.get_processed_batches_db(&self.aspect).await? } else { self.db.get_unprocessed_batches_db(&self.aspect).await? };
		let conn = db.connect()?;
		let mut rows = conn.query("SELECT batch_hash, measurements FROM batches", ()).await?;
		let mut batches = Vec::new();
		while let Some(row) = rows.next().await? {
			let hash: String = row.get(0)?;
			let measurements: String = row.get(1)?;
			let measurements: Vec<BatchedMeasurement> = serde_json::from_str(&measurements)?;
			batches.push((hash, measurements.iter().map(|m| m.point().timestamp).collect()));
		}
		Ok(batches)
	}

	/// The timestamps of every point in the unprocessed batches.
	async fn covered(&self) -> Result<BTreeSet<DateTime<Utc>>> {
		Ok(self.batch_rows(false).await?.into_iter().flat_map(|(_, points)| points).collect())
	}
}

fn base() -> DateTime<Utc> {
	Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
}

/// One row per minute in `range`, with a value pattern that is not flat.
fn minutes(range: std::ops::Range<i64>) -> Vec<InputMeasurement> {
	range.map(|i| InputMeasurement::new(base() + Duration::minutes(i), BigDecimal::from((i * 7) % 11))).collect()
}

fn timestamps(rows: &[InputMeasurement]) -> BTreeSet<DateTime<Utc>> {
	rows.iter().map(InputMeasurement::timestamp).collect()
}

fn assert_unique_hashes(batches: &[BatchRow], what: &str) {
	let mut seen = HashSet::new();
	for (hash, points) in batches {
		assert!(seen.insert(hash), "{what}: batch {hash} (points {:?}..{:?}) is stored twice", points.first(), points.last());
	}
}

/// Process every queued batch and extract patterns, then check that no pattern holds
/// the same occurrence twice and that there is exactly one occurrence span per batch.
async fn assert_no_duplicate_occurrences(store: &Store, batches: usize) -> Result<()> {
	build_processed_batch_queue(&store.db, &store.aspect).await?;
	let mut dictionary = Dictionary::new("crash".to_string(), "S18 crash test".to_string(), DictionaryConstraints::default());
	build_patterns_queue(&store.db, &store.aspect, &mut dictionary).await?;
	let mut spans = HashSet::new();
	for pattern in dictionary.patterns() {
		let mut in_pattern = HashSet::new();
		for occurrence in pattern.occurrences() {
			let span = (*occurrence.beginning(), *occurrence.end());
			assert!(in_pattern.insert(span), "pattern {:?} holds the occurrence {span:?} twice", pattern.id());
			spans.insert(span);
		}
	}
	assert_eq!(spans.len(), batches, "one occurrence span per batch");
	Ok(())
}

/// Ingest a series and crash the consumer after it inserted its batches and before it
/// dequeued their timestamps (`L-consumer-batches`).
async fn crash_the_consumer(store: &Store) -> Result<Vec<BatchRow>> {
	let rows = minutes(0..CONSUMER_ROWS);
	store.db.batch_capture_measurements(store.aspect, DatasetId::new(), rows.clone()).await?;
	{
		let _armed = fault::arm(FaultPoint::LConsumerBatches, FaultAction::ReturnErr);
		let err = store.build().await.expect_err("the consumer fails at L-consumer-batches");
		assert!(format!("{err:#}").contains("injected fault at L-consumer-batches"), "{err:#}");
	}
	let batches = store.batch_rows(false).await?;
	assert!(batches.len() > 1, "precondition: the crashed run inserted its batches");
	assert_unique_hashes(&batches, "the crashed run");
	assert_eq!(store.queued().await?, timestamps(&rows), "precondition: the crash came before the dequeue");
	Ok(batches)
}

/// **Regression (legacy-queue-consumer-batch-before-dequeue).** A consumer crash before
/// the dequeue, then a re-run, creates no duplicate batches or pattern occurrences. On
/// main the re-run inserted every batch a second time.
#[tokio::test]
#[serial]
async fn a_consumer_rerun_after_a_crash_before_the_dequeue_adds_no_duplicates() -> Result<()> {
	let store = Store::new("consumer_unprocessed").await?;
	let crashed = crash_the_consumer(&store).await?;

	let store = store.restart().await?;
	store.build().await.context("the consumer re-run")?;
	let after = store.batch_rows(false).await?;
	assert_unique_hashes(&after, "after the re-run");
	assert_eq!(after.len(), crashed.len(), "the re-run must not insert the crashed run's batches again");
	assert!(store.queued().await?.len() < timestamps(&minutes(0..CONSUMER_ROWS)).len(), "the re-run dequeued the timestamps its batches cover");

	assert_no_duplicate_occurrences(&store, crashed.len()).await
}

/// The same crash, but a batch processor moves the crashed run's batches to the
/// processed DB before the consumer re-runs. The dedupe also checks the processed
/// batches, so the re-run still adds nothing. On main the re-run queued every batch
/// again and each one was processed, and turned into an occurrence, twice.
#[tokio::test]
#[serial]
async fn a_consumer_rerun_after_its_batches_were_processed_adds_no_duplicates() -> Result<()> {
	let store = Store::new("consumer_processed").await?;
	let crashed = crash_the_consumer(&store).await?;
	build_processed_batch_queue(&store.db, &store.aspect).await?;
	assert_eq!(store.batch_rows(true).await?.len(), crashed.len(), "precondition: the crashed run's batches are processed");

	let store = store.restart().await?;
	store.build().await.context("the consumer re-run")?;
	assert_eq!(store.batch_rows(false).await?.len(), 0, "the re-run must not queue batches that are already processed");
	assert_unique_hashes(&store.batch_rows(true).await?, "the processed batches");

	assert_no_duplicate_occurrences(&store, crashed.len()).await
}

/// Set only in the child process [`a_consumer_killed_before_the_dequeue_reruns_without_duplicates`]
/// spawns; it holds the database name and the aspect id, separated by a space.
const CHILD_ENV: &str = "WEFT_LEGACY_CONSUMER_CRASH_CHILD";

/// The body the re-executed child runs: open the database named by [`CHILD_ENV`] and run
/// the consumer, which aborts at the point `WEFT_FAULT` arms. In a normal run the variable
/// is unset and this does nothing.
#[tokio::test]
async fn consumer_child() -> Result<()> {
	let Some(spec) = std::env::var_os(CHILD_ENV) else { return Ok(()) };
	suppress_core_dump();
	let spec = spec.into_string().map_err(|raw| anyhow::anyhow!("{raw:?} is not UTF-8"))?;
	let (name, aspect) = spec.split_once(' ').context("the child spec is `<name> <aspect id>`")?;
	let db = Database::existing(name).await?;
	let aspect = AspectId::from_uuid(Uuid::parse_str(aspect)?);
	build_incremental_unprocessed_queue(&db, &aspect, &Resolution::Minutes, &Spline::Linear, BATCH_SIZE).await?;
	panic!("{} did not abort the consumer", fault::FAULT_ENV);
}

/// Keep the child's deliberate abort from dumping core, as weftdb's own fault tests do:
/// this machine (and CI) may run systemd-coredump, which would otherwise store a core of
/// the test binary on every run.
fn suppress_core_dump() {
	#[cfg(unix)]
	{
		let none = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
		// SAFETY: setrlimit only reads the struct, for the duration of the call.
		unsafe { libc::setrlimit(libc::RLIMIT_CORE, &raw const none) };
	}
	// A pipe `core_pattern` (systemd-coredump) ignores RLIMIT_CORE, but the kernel never
	// dumps a process that is not dumpable.
	#[cfg(target_os = "linux")]
	{
		let not_dumpable: libc::c_ulong = 0;
		// SAFETY: PR_SET_DUMPABLE takes one integer and changes only this process's
		// dumpable flag.
		unsafe { libc::prctl(libc::PR_SET_DUMPABLE, not_dumpable) };
	}
}

/// The consumer crash as a real process crash: a child process runs the consumer and
/// aborts at `L-consumer-batches`, after its batches committed and before the dequeue.
/// The re-run in this process then adds no duplicate batches or pattern occurrences.
#[tokio::test]
#[serial]
async fn a_consumer_killed_before_the_dequeue_reruns_without_duplicates() -> Result<()> {
	let store = Store::new("consumer_abort").await?;
	let rows = minutes(0..CONSUMER_ROWS);
	store.db.batch_capture_measurements(store.aspect, DatasetId::new(), rows.clone()).await?;
	let closed = store.close().await;

	let data_dir = closed.dir.path().to_str().context("temp data dir is not valid UTF-8")?;
	let point = FaultPoint::LConsumerBatches;
	let output = std::process::Command::new(std::env::current_exe()?).args(["consumer_child", "--exact", "--nocapture", "--test-threads=1"]).env("TEST_DATA_DIR", data_dir).env(CHILD_ENV, format!("{} {}", closed.name, closed.aspect.as_uuid())).env(fault::FAULT_ENV, format!("{point}:abort")).output()?;
	assert!(!output.status.success(), "the child aborted: {output:?}");
	#[cfg(unix)]
	{
		use std::os::unix::process::ExitStatusExt;
		assert_eq!(output.status.signal(), Some(6), "killed by SIGABRT: {output:?}");
	}
	assert!(String::from_utf8_lossy(&output.stderr).contains(&format!("aborting at fault point {point}")), "the child reached the point: {output:?}");

	let store = closed.reopen().await?;
	let crashed = store.batch_rows(false).await?;
	assert!(crashed.len() > 1, "precondition: the killed run committed its batches");
	assert_unique_hashes(&crashed, "the killed run");
	assert_eq!(store.queued().await?, timestamps(&rows), "precondition: the kill came before the dequeue");

	store.build().await.context("the consumer re-run")?;
	let after = store.batch_rows(false).await?;
	assert_unique_hashes(&after, "after the re-run");
	assert_eq!(after.len(), crashed.len(), "the re-run must not insert the killed run's batches again");

	assert_no_duplicate_occurrences(&store, crashed.len()).await
}

/// **Regression (legacy-measurements-committed-not-queued).** A failure after the first
/// chunk of a `batch_capture_measurements` committed: the rows that landed are queued,
/// so the incremental build covers them. On main the enqueue ran after the chunk loop,
/// so it never ran, and the landed rows were never batched (the aspect already had
/// batches, so the consumer did not fall back to a full rebuild).
#[tokio::test]
#[serial]
async fn a_failure_after_a_chunk_leaves_the_landed_rows_queued_and_batched() -> Result<()> {
	let store = Store::new("chunk_crash").await?;
	store.db.batch_capture_measurements(store.aspect, DatasetId::new(), minutes(0..8)).await?;
	store.build().await?;
	assert!(store.db.count_unprocessed_batches(&store.aspect).await? > 0, "precondition: the aspect has batches, so the consumer runs incrementally");

	let landed = minutes(8..16);
	{
		let _armed = fault::arm(FaultPoint::LChunk(0), FaultAction::ReturnErr);
		let err = store.db.batch_capture_measurements(store.aspect, DatasetId::new(), landed.clone()).await.expect_err("the ingest fails after its first chunk");
		assert!(format!("{err:#}").contains("injected fault at L-chunk(0)"), "{err:#}");
	}
	let store = store.restart().await?;
	assert_eq!(store.db.get_latest_measurement(&store.aspect).await?, Some(base() + Duration::minutes(15)), "precondition: the chunk landed");
	let queued = store.queued().await?;
	assert!(timestamps(&landed).is_subset(&queued), "every landed row is queued; missing: {:?}", timestamps(&landed).difference(&queued).collect::<Vec<_>>());

	store.build().await?;
	let covered = store.covered().await?;
	let missing: Vec<_> = timestamps(&landed).difference(&covered).copied().collect();
	assert!(missing.is_empty(), "the incremental build did not batch the landed rows {missing:?}");
	Ok(())
}

/// A failure after the write-ahead enqueue and before any chunk (`L-enqueued`): nothing
/// landed, the queue holds the batch's timestamps (a superset of the data), and the
/// incremental build neither fails nor invents batches for rows that do not exist. The
/// client's retry then lands the rows, and the build covers them. `capture_measurement`
/// follows the same order.
#[tokio::test]
#[serial]
async fn a_failure_after_the_enqueue_leaves_a_superset_queue_the_build_tolerates() -> Result<()> {
	let store = Store::new("enqueue_crash").await?;
	let rows = minutes(0..10);
	let single = InputMeasurement::new(base() + Duration::minutes(100), BigDecimal::from(1));
	{
		let _armed = fault::arm(FaultPoint::LEnqueued, FaultAction::ReturnErr);
		let err = store.db.batch_capture_measurements(store.aspect, DatasetId::new(), rows.clone()).await.expect_err("the ingest fails after its enqueue");
		assert!(format!("{err:#}").contains("injected fault at L-enqueued"), "{err:#}");
		let err = store.db.capture_measurement(&store.aspect, &DatasetId::new(), &single).await.expect_err("capture_measurement fails after its enqueue");
		assert!(format!("{err:#}").contains("injected fault at L-enqueued"), "{err:#}");
	}
	let store = store.restart().await?;
	assert_eq!(store.db.get_latest_measurement(&store.aspect).await?, None, "no row landed before the failure");
	let mut expected = timestamps(&rows);
	expected.insert(single.timestamp());
	assert_eq!(store.queued().await?, expected, "the queue holds every timestamp of the failed calls");

	store.build().await.context("the build over a queue whose rows never landed")?;
	assert_eq!(store.db.count_unprocessed_batches(&store.aspect).await?, 0, "no batch is built from rows that do not exist");
	assert_eq!(store.queued().await?, expected, "the queued timestamps wait for their rows");

	store.db.batch_capture_measurements(store.aspect, DatasetId::new(), rows.clone()).await.context("the client's retry")?;
	store.build().await?;
	let covered = store.covered().await?;
	let missing: Vec<_> = timestamps(&rows).difference(&covered).copied().collect();
	assert!(missing.is_empty(), "the build after the retry did not batch {missing:?}");
	Ok(())
}
