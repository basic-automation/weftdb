//! TAG-7, the seal lane of tagged series: a tagged batch seals frames in a v7 envelope that
//! carries its series key (critical `EXT_SERIES_KEY`, tags D3), and rows under the series id the
//! commit resolves (or registers) in the aspect's catalog. An untagged batch is unchanged. The
//! library gate opened in TAG-8, so these tests seal tagged batches through the public `ingest`.

use std::{path::Path, str::FromStr};

use bigdecimal::BigDecimal;
use tempfile::TempDir;
use weft_physical_type::{crc32, describe_frame, timestamp::TimeUnit, AspectSchema, FrameExtension, PhysicalType, EXT_SERIES_KEY, FRAME_ENVELOPE, FRAME_PAGED, FRAME_SINGLE_BLOCK, PAGED_SEGMENT_FORMAT_VERSION, SEGMENT_FORMAT_VERSION};

use super::{series_envelope_overhead, tag_frame, IndexRow, SegmentStore, SegmentStoreOptions};
use crate::{IngestBatch, SeriesId, SeriesKey, StoreError, TagSet};

fn bd(s: &str) -> BigDecimal {
	BigDecimal::from_str(s).expect("parses")
}

/// A lossless f64 schema in seconds.
fn schema() -> AspectSchema {
	AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)
}

/// The aspect every test here writes.
const ASPECT: &str = "price";

fn tags(host: &str) -> TagSet {
	TagSet::from_pairs([("host", host)]).expect("valid tags")
}

/// A dense batch of `rows` under `key`, in `groups`.
fn batch(key: &str, groups: TagSet, rows: &[(i64, &str)]) -> IngestBatch {
	let (ts, vs): (Vec<i64>, Vec<BigDecimal>) = rows.iter().map(|(t, v)| (*t, bd(v))).unzip();
	IngestBatch::dense(key, ts, vs).with_groups(groups)
}

/// A fresh store at `root` with [`ASPECT`] declared.
async fn store_at(root: &Path, options: SegmentStoreOptions) -> SegmentStore {
	let store = SegmentStore::open_with_options(root, options).await.expect("opens");
	store.declare(ASPECT, &schema()).await.expect("declares");
	store
}

/// Ingest `batch` with the tags gate open.
async fn ingest_tagged(store: &SegmentStore, batch: &IngestBatch) -> anyhow::Result<crate::IngestReceipt> {
	store.ingest(ASPECT, batch).await
}

/// The rows of [`ASPECT`], ascending by id.
async fn rows(store: &SegmentStore) -> Vec<IndexRow> {
	let mut rows = store.index.rows(ASPECT).await.expect("reads the rows");
	rows.sort_by_key(|row| row.desc.id);
	rows
}

/// The one extension a tagged frame must carry for `groups`.
fn series_extension(groups: &TagSet) -> FrameExtension {
	FrameExtension { id: EXT_SERIES_KEY, critical: true, payload: groups.series_key().as_bytes().to_vec() }
}

/// Check that `row`'s frame is what the row records (its length and trailer CRC), and return
/// its bytes.
fn bound_frame(row: &IndexRow) -> Vec<u8> {
	let bytes = std::fs::read(&row.desc.path).expect("the frame is on disk");
	assert_eq!(bytes.len() as u64, row.desc.byte_len, "segment {}: byte_len is the frame's length on disk", row.desc.id);
	let trailer = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().expect("four bytes"));
	assert_eq!(row.frame_crc, Some(trailer), "segment {}: frame_crc is the frame's trailer", row.desc.id);
	assert_eq!(trailer, crc32(&bytes[..bytes.len() - 4]), "segment {}: and the trailer is the CRC over everything before it", row.desc.id);
	bytes
}

/// An untagged batch seals exactly the frame it sealed before tags: the bytes are the plain
/// v5 encoding of its rows under the store's frame options, with no envelope, and their CRC is
/// pinned to the value the same ingest produced on the commit before TAG-7 (b6a1c67/2316fdc,
/// measured when this test was written). `tag_frame` passes an untagged frame through
/// untouched; the pin moves only if the v5 encoding itself does, which the weft-physical-type
/// goldens would already have flagged.
#[tokio::test]
async fn untagged_ingest_bytes_are_unchanged() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path(), SegmentStoreOptions::default()).await;
	let rows_in = [(0, "1.5"), (10, "2.25"), (20, "3"), (30, "-4.125")];
	let receipt = store.ingest(ASPECT, &batch("k", TagSet::new(), &rows_in)).await.expect("ingests");
	assert_eq!(receipt.series_ids, vec![0]);
	let committed = rows(&store).await;
	assert_eq!(committed.len(), 1);
	assert_eq!(committed[0].series_id, 0, "an untagged row is series 0");
	let bytes = bound_frame(&committed[0]);
	let (ts, vs): (Vec<i64>, Vec<Option<BigDecimal>>) = rows_in.iter().map(|(t, v)| (*t, Some(bd(v)))).unzip();
	let segment = schema().seal_nullable(&ts, &vs).expect("encodes");
	let expected = segment.write_to_with(&store.frame_options(segment.row_count(), segment.benefits_from_checkpoints(), segment.checkpoint_codec_overhead()));
	drop(store);
	assert_eq!(bytes, expected, "the untagged frame is the plain encoding of its rows");
	assert_eq!(describe_frame(&bytes).expect("a frame").version, FRAME_SINGLE_BLOCK, "no envelope");
	assert_eq!(crc32(&bytes), 0x2144_df1c, "the untagged frame's bytes are pinned to what origin/main sealed before TAG-7");
	assert_eq!(tag_frame(bytes.clone(), None).expect("passes through"), bytes);
	assert_eq!(tag_frame(bytes.clone(), Some(&SeriesKey::default())).expect("passes through"), bytes, "the empty key is series 0, never enveloped");
}

/// A tagged batch's frame is a v7 envelope with exactly one extension, the series key,
/// critical, its payload the canonical key; the row's `byte_len` and `frame_crc` are the
/// wrapped frame's; and the descriptor's `format_version` (what `IngestResponse` reports)
/// stays the body's, 5 single-block or 6 paged, which this pins. The rows read back.
#[tokio::test]
async fn tagged_ingest_writes_a_critical_series_key_envelope() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path(), SegmentStoreOptions::default()).await;
	let groups = TagSet::from_pairs([("region", "eu"), ("host", "a")]).expect("valid tags");
	let single = ingest_tagged(&store, &batch("single", groups.clone(), &[(0, "1"), (10, "2"), (20, "3")])).await.expect("ingests");
	let paged = ingest_tagged(&store, &batch("paged", groups.clone(), &[(30, "4"), (40, "5"), (50, "6")]).with_rows_per_page(Some(2))).await.expect("ingests paged");
	assert_eq!((single.series_ids.clone(), paged.series_ids.clone()), (vec![1], vec![1]), "the key's first ingest registers series 1, and the second reuses it");
	let committed = rows(&store).await;
	assert_eq!(committed.len(), 2);
	for (row, (body, format_version)) in committed.iter().zip([(FRAME_SINGLE_BLOCK, SEGMENT_FORMAT_VERSION), (FRAME_PAGED, PAGED_SEGMENT_FORMAT_VERSION)]) {
		assert_eq!(row.series_id, 1, "segment {}: the row carries the resolved id", row.desc.id);
		let bytes = bound_frame(row);
		let info = describe_frame(&bytes).expect("a readable frame");
		assert_eq!((info.version, info.body_version), (FRAME_ENVELOPE, body), "segment {}: a v7 envelope around the body", row.desc.id);
		assert_eq!(info.extensions, vec![series_extension(&groups)], "segment {}: one critical series-key extension", row.desc.id);
		assert!(info.decodable(), "this build knows the extension");
		assert_eq!(row.desc.format_version, format_version, "segment {}: the descriptor records the body's version, not the envelope's", row.desc.id);
		let payload = SeriesKey::from_canonical(&info.extensions[0].payload).expect("the payload is canonical");
		assert_eq!(&payload, groups.series_key(), "the frame records the key, never the id");
	}
	let (ts, vs) = store.read_time_range_of(ASPECT, SeriesId(1), 0, 50).await.expect("the enveloped frames read");
	assert_eq!(store.read_time_range(ASPECT, 0, 50).await.expect("reads series 0"), (vec![], vec![]), "series 0 holds none of them (TAG-8)");
	assert_eq!(ts, vec![0, 10, 20, 30, 40, 50]);
	assert_eq!(vs, ["1", "2", "3", "4", "5", "6"].iter().map(|v| Some(bd(v))).collect::<Vec<_>>());
	assert_eq!(store.index.series_key_of(ASPECT, SeriesId(1)).await.expect("reads").as_ref(), Some(groups.series_key()), "the frame key equals the catalog key for the row's id");
}

/// The envelope a tagged batch's chunking reserves room for is never smaller than the one
/// written, even for the longest key a tag set can have.
#[test]
fn the_tagged_frame_target_covers_the_envelope() {
	let longest = TagSet::from_pairs((0..weft_physical_type::MAX_TAGS).map(|i| (format!("k{i:02}{}", "x".repeat(20)), "v".repeat(30)))).expect("valid tags");
	for groups in [tags("a"), longest] {
		let segment = schema().seal(&[0, 10], &[bd("1"), bd("2")]).expect("encodes");
		let plain = segment.write_to();
		let wrapped = tag_frame(plain.clone(), Some(groups.series_key())).expect("wraps");
		let overhead = (wrapped.len() - plain.len()) as u64;
		assert!(overhead <= series_envelope_overhead(groups.series_key()), "{overhead} bytes of envelope over the {} reserved", series_envelope_overhead(groups.series_key()));
		assert_eq!(super::ingest_frame_target(&groups), crate::MAX_ENCODED_FRAME_BYTES - series_envelope_overhead(groups.series_key()), "a tagged batch's chunks leave room for the envelope");
	}
	assert!(series_envelope_overhead(TagSet::from_pairs([("k", "v")]).expect("valid").series_key()) < 1_046);
	assert_eq!(super::ingest_frame_target(&TagSet::new()), crate::MAX_ENCODED_FRAME_BYTES, "only tagged batches pay for an envelope");
}

/// One id per key: a new key takes the aspect's next series id, registered in the catalog in
/// the same commit as its rows, and a key seen before reuses its id. Ids start at 1, and the
/// untagged series stays 0 beside them.
#[tokio::test]
async fn tagged_ingest_allocates_one_id_per_key_and_reuses_it() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path(), SegmentStoreOptions::default()).await;
	let mut ids = Vec::new();
	for (key, host) in [("1", "a"), ("2", "b"), ("3", "a"), ("4", "c"), ("5", "b")] {
		ids.push(ingest_tagged(&store, &batch(key, tags(host), &[(i64::from(key.parse::<u8>().expect("a number")) * 10, "1")])).await.expect("ingests").series_ids);
	}
	let untagged = ingest_tagged(&store, &batch("6", TagSet::new(), &[(60, "1")])).await.expect("ingests untagged");
	assert_eq!(ids, vec![vec![1], vec![2], vec![1], vec![3], vec![2]]);
	assert_eq!(untagged.series_ids, vec![0], "the empty tag set is series 0 under either policy");
	let catalog = store.index.series_of(ASPECT).await.expect("reads the catalog");
	assert_eq!(catalog, vec![(SeriesId(1), tags("a").series_key().clone()), (SeriesId(2), tags("b").series_key().clone()), (SeriesId(3), tags("c").series_key().clone())], "one catalog row per key, and none for series 0");
	assert_eq!(store.index.next_series_id(ASPECT).await.expect("reads"), 4, "the allocator moved past every registered id");
	assert_eq!(rows(&store).await.iter().map(|row| row.series_id).collect::<Vec<_>>(), vec![1, 2, 1, 3, 2, 0]);
	let root = dir.path().to_path_buf();
	drop(store);
	// The catalog is durable: after a restart the same key resolves to the same id.
	let reopened = SegmentStore::open(&root).await.expect("reopens");
	assert_eq!(ingest_tagged(&reopened, &batch("7", tags("c"), &[(70, "1")])).await.expect("ingests").series_ids, vec![3]);
	assert_eq!(ingest_tagged(&reopened, &batch("8", tags("d"), &[(80, "1")])).await.expect("ingests").series_ids, vec![4]);
}

/// A tagged batch offered again under its key is answered from the ledger, before anything is
/// encoded or resolved: the recorded receipt (with its series id), nothing written, and no
/// series registered twice. The same rows in another series are a different batch.
#[tokio::test]
async fn a_replayed_tagged_batch_is_a_ledger_hit() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path(), SegmentStoreOptions::default()).await;
	let tagged = batch("k", tags("a"), &[(0, "1"), (10, "2")]);
	let first = ingest_tagged(&store, &tagged).await.expect("ingests");
	let second = ingest_tagged(&store, &tagged).await.expect("replays");
	assert!(!first.duplicate && second.duplicate);
	assert_eq!((second.segment_ids.clone(), second.series_ids.clone()), (first.segment_ids.clone(), vec![1]));
	assert_eq!(store.segment_count(ASPECT).await.expect("counts"), 1, "the replay wrote nothing");
	let entry = store.index.ledger_entry(ASPECT, "k").await.expect("reads").expect("a ledger row");
	assert_eq!(entry.series_ids, vec![1], "the ledger records the resolved id");
	let err = ingest_tagged(&store, &batch("k", tags("b"), &[(0, "1"), (10, "2")])).await.expect_err("the same rows in another series are another batch");
	assert!(matches!(err.downcast_ref::<StoreError>(), Some(StoreError::IdempotencyConflict { .. })), "{err:#}");
	assert_eq!(store.index.series_of(ASPECT).await.expect("reads").len(), 1, "the refused batch registered nothing");
	let root = dir.path().to_path_buf();
	drop(store);
	let reopened = SegmentStore::open(&root).await.expect("reopens");
	let third = ingest_tagged(&reopened, &tagged).await.expect("replays after a restart");
	assert!(third.duplicate);
	assert_eq!(third.series_ids, vec![1]);
	assert_eq!(reopened.segment_count(ASPECT).await.expect("counts"), 1);
}

/// The series cap refuses the next new key with the typed `SeriesLimit`, before any frame is
/// written, while every key already registered (and the untagged series, which never counts)
/// stays writable.
#[tokio::test]
async fn series_limit_refuses_the_next_new_key_and_keeps_existing_ones_writable() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path(), SegmentStoreOptions::default().with_max_series_per_aspect(2)).await;
	assert_eq!(SegmentStoreOptions::default().max_series_per_aspect, 100_000, "the default cap");
	ingest_tagged(&store, &batch("1", tags("a"), &[(0, "1")])).await.expect("the first series");
	ingest_tagged(&store, &batch("2", tags("b"), &[(10, "1")])).await.expect("the second series");
	let frames = std::fs::read_dir(dir.path().join("segments")).expect("lists").count();
	let err = ingest_tagged(&store, &batch("3", tags("c"), &[(20, "1")])).await.expect_err("a third series is past the cap");
	assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::SeriesLimit { aspect: ASPECT.to_string(), limit: 2 }), "{err:#}");
	assert_eq!(std::fs::read_dir(dir.path().join("segments")).expect("lists").count(), frames, "the refusal wrote no frame");
	assert!(store.index.ledger_entry(ASPECT, "3").await.expect("reads").is_none(), "and recorded no key");
	assert_eq!(ingest_tagged(&store, &batch("4", tags("a"), &[(30, "1")])).await.expect("an existing series stays writable").series_ids, vec![1]);
	assert_eq!(ingest_tagged(&store, &batch("5", tags("b"), &[(40, "1")])).await.expect("so does the other").series_ids, vec![2]);
	assert_eq!(ingest_tagged(&store, &batch("6", TagSet::new(), &[(50, "1")])).await.expect("series 0 never counts").series_ids, vec![0]);
	assert_eq!(store.index.series_of(ASPECT).await.expect("reads").len(), 2);
	// The commit's own check refuses too, for a new key that got past the early one.
	let mut ops = Vec::new();
	let err = store.commit_series_id(ASPECT, &tags("c"), &mut ops).await.expect_err("the commit-time check");
	drop(store);
	assert!(matches!(err.downcast_ref::<StoreError>(), Some(StoreError::SeriesLimit { limit: 2, .. })), "{err:#}");
	assert!(ops.is_empty(), "nothing to register");
}

/// Two concurrent ingests of different new keys at `limit - 1` can both pass the early check
/// and both write frames; the commit-time check refuses one, and that refusal unlinks the
/// frames it wrote, so exactly one series registers and no refused frame is left for recovery
/// to journal as an orphan.
#[tokio::test]
async fn concurrent_new_keys_at_the_cap_register_one_series_and_leave_no_refused_frames() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path(), SegmentStoreOptions::default().with_max_series_per_aspect(2)).await;
	ingest_tagged(&store, &batch("1", tags("a"), &[(0, "1")])).await.expect("the first series");
	let (b, c) = (batch("2", tags("b"), &[(10, "1")]), batch("3", tags("c"), &[(20, "1")]));
	let (left, right) = tokio::join!(ingest_tagged(&store, &b), ingest_tagged(&store, &c));
	let refusals = [&left, &right].iter().filter(|r| r.is_err()).count();
	for err in [left.as_ref().err(), right.as_ref().err()].into_iter().flatten() {
		assert!(matches!(err.downcast_ref::<StoreError>(), Some(StoreError::SeriesLimit { limit: 2, .. })), "{err:#}");
	}
	assert_eq!(refusals, 1, "exactly one new key is refused");
	assert_eq!(store.index.series_of(ASPECT).await.expect("reads").len(), 2, "exactly one new series registers");
	let frames = std::fs::read_dir(dir.path().join("segments")).expect("lists").filter_map(Result::ok).filter(|e| e.path().extension().is_some_and(|x| x == "weftseg")).count();
	assert_eq!(frames, store.segment_count(ASPECT).await.expect("counts"), "every frame on disk is named by a row");
}

/// A maintenance output of a tagged series keeps its series key: a squash (P1, outputs under
/// member ids) and a split (P2, an output under a fresh id) of a tagged series' segments write
/// their outputs in the same critical series-key envelope, with `byte_len` and `frame_crc` the
/// wrapped frame's, under the plan's series id, so no commit ever names an untagged frame from
/// a non-zero row.
#[tokio::test]
async fn a_tagged_compaction_output_keeps_its_series_key() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path(), SegmentStoreOptions::default()).await;
	let groups = tags("a");
	ingest_tagged(&store, &batch("1", groups.clone(), &[(0, "1"), (10, "2")])).await.expect("ingests");
	ingest_tagged(&store, &batch("2", groups.clone(), &[(10, "20"), (20, "3")]).with_rows_per_page(Some(1))).await.expect("ingests");
	ingest_tagged(&store, &batch("3", groups.clone(), &[(30, "4"), (40, "5")])).await.expect("ingests");
	assert_eq!(store.squash_aspect(ASPECT).await.expect("squashes"), 2, "three segments fold into one");
	let squashed = rows(&store).await;
	assert_eq!(squashed.len(), 1);
	let check = |row: &IndexRow| {
		assert_eq!(row.series_id, 1, "segment {}: the output keeps the plan's series", row.desc.id);
		let info = describe_frame(&bound_frame(row)).expect("a readable frame");
		assert_eq!(info.version, FRAME_ENVELOPE, "segment {}: the output is enveloped", row.desc.id);
		assert_eq!(info.extensions, vec![series_extension(&groups)], "segment {}: with its series key", row.desc.id);
	};
	check(&squashed[0]);
	let id = squashed[0].desc.id;
	let fresh = store.split_segment(ASPECT, id, 25).await.expect("splits").expect("the split takes a fresh id");
	let split = rows(&store).await;
	assert_eq!(split.iter().map(|row| row.desc.id).collect::<Vec<_>>(), vec![id, fresh]);
	split.iter().for_each(check);
	let (ts, vs) = store.read_time_range_of(ASPECT, SeriesId(split[0].series_id), i64::MIN, i64::MAX).await.expect("reads");
	drop(store);
	assert_eq!(ts, vec![0, 10, 20, 30, 40]);
	assert_eq!(vs, ["1", "20", "3", "4", "5"].iter().map(|v| Some(bd(v))).collect::<Vec<_>>(), "the newer value wins at 10");
}

/// Set only in the child process
/// `a_power_cut_between_tagged_frames_leaves_no_catalog_row_without_rows` starts: the root to
/// write.
const CRASH_CHILD_ROOT: &str = "WEFT_TEST_TAG_SEAL_CRASH_ROOT";

/// The body that child runs: a tagged ingest of a new key, aborted by the parent's
/// `WEFT_FAULT=S-dir-synced:abort` once its frame is durable and before its commit, so the
/// process dies with the frame on disk and neither its rows nor its catalog row committed. In
/// a normal test run the variable is unset and this does nothing.
#[tokio::test]
async fn tag_seal_crash_child() {
	let Some(root) = std::env::var_os(CRASH_CHILD_ROOT) else { return };
	crate::types::durable::fault::suppress_core_dump();
	let store = store_at(Path::new(&root), SegmentStoreOptions::default()).await;
	let result = ingest_tagged(&store, &batch("k", tags("a"), &[(0, "1"), (10, "2")])).await;
	drop(store);
	panic!("the tagged ingest was to abort at S-dir-synced, and returned {result:?}");
}

/// A process that dies between a tagged batch's durable frame and its commit leaves no catalog
/// row without rows, and no rows without a catalog row: the registration travels in the rows'
/// transaction, so neither committed. The frame is crash litter, still a readable series-key
/// envelope. No id leaked: the retry registers the key as series 1.
#[tokio::test]
async fn a_power_cut_between_tagged_frames_leaves_no_catalog_row_without_rows() {
	let dir = TempDir::new().expect("tempdir");
	let root = dir.path().join("store");
	let exe = std::env::current_exe().expect("finds the test binary");
	let out = tokio::process::Command::new(exe).args(["types::segment_store::tag_seal_tests::tag_seal_crash_child", "--exact", "--nocapture", "--test-threads=1"]).env(CRASH_CHILD_ROOT, &root).env(crate::durable::fault::FAULT_ENV, "S-dir-synced:abort").output().await.expect("runs the child");
	assert!(!out.status.success(), "the child aborted: {}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
	let frames: Vec<_> = std::fs::read_dir(root.join("segments")).expect("lists").map(|e| e.expect("entry").path()).filter(|path| path.extension().is_some_and(|ext| ext == "weftseg")).collect();
	assert_eq!(frames.len(), 1, "the frame was durable before the abort: {frames:?}");
	let litter = std::fs::read(&frames[0]).expect("reads the frame");
	assert_eq!(describe_frame(&litter).expect("a readable frame").extensions, vec![series_extension(&tags("a"))], "the litter is a whole series-key envelope");

	let store = SegmentStore::open(&root).await.expect("reopens");
	assert!(store.recovery_report().orphans >= 1, "{:?}", store.recovery_report());
	assert!(rows(&store).await.is_empty(), "no rows committed");
	assert!(store.index.series_of(ASPECT).await.expect("reads").is_empty(), "and no catalog row");
	assert_eq!(store.index.next_series_id(ASPECT).await.expect("reads"), 1, "no id was handed out");
	assert!(store.index.ledger_entry(ASPECT, "k").await.expect("reads").is_none(), "nor the key recorded");
	let retried = ingest_tagged(&store, &batch("k", tags("a"), &[(0, "1"), (10, "2")])).await.expect("the retry commits");
	assert!(!retried.duplicate);
	assert_eq!(retried.series_ids, vec![1]);
	assert_eq!(rows(&store).await.iter().map(|row| row.series_id).collect::<Vec<_>>(), vec![1]);
	assert_eq!(store.index.series_of(ASPECT).await.expect("reads"), vec![(SeriesId(1), tags("a").series_key().clone())]);
	drop(store);
}
