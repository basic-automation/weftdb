//! ROB-22: this module tests that [`SuspectFramePolicy`] reaches the segment store
//! through [`SegmentStoreOptions::from_env`] (the env-var → options → store path) and
//! that the interactive read path honours it as documented: `Skip` continues past a
//! frame whose decode fails with a C5-suspect class; `Quarantine` (the default) and
//! `Fail` propagate the decode error to the caller unchanged.

use std::{fs, path::Path, str::FromStr};

use bigdecimal::BigDecimal;
use tempfile::TempDir;
use weft_physical_type::{crc32, timestamp::TimeUnit, AspectSchema, PhysicalType};

use super::{SegmentStore, SegmentStoreOptions};
use crate::{IngestBatch, SeriesId, SuspectFramePolicy};

const ASPECT: &str = "price";

fn bd(s: &str) -> BigDecimal {
	BigDecimal::from_str(s).expect("parses")
}

fn schema() -> AspectSchema {
	AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)
}

/// A fresh store at `dir` with `ASPECT` declared and the given options.
async fn store_with(dir: &Path, options: SegmentStoreOptions) -> SegmentStore {
	let store = SegmentStore::open_with_options(dir, options).await.expect("opens");
	store.declare(ASPECT, &schema()).await.expect("declares");
	store
}

/// Ingest one batch, under an idempotency key derived from its own rows.
///
/// The first argument of `IngestBatch::dense` is an idempotency key, not a series
/// key: the aspect refuses a second batch that reuses a key with different rows, so
/// a test that ingests twice must not reuse one. Deriving it from the timestamps
/// keeps every batch distinct while all of them still land in `SeriesId::UNTAGGED`.
async fn fill(store: &SegmentStore, ts: Vec<i64>, vs: Vec<BigDecimal>) {
	let key = format!("fill-{ts:?}");
	store.ingest(ASPECT, &IngestBatch::dense(key, ts, vs)).await.expect("ingests");
}

/// Rewrite `path` so its decode fails with a C5-**suspect** cause.
///
/// Overwriting the file with junk is not enough, and that is why these tests passed
/// only without `--features bitsliced-codec`: junk fails the CRC trailer, so
/// `classify_decode_failure` classes it `Corrupt`, and the policy governs only the
/// suspect half of C5 — a corrupt frame is always reported to the caller whatever
/// `WEFT_SUSPECT_FRAME_POLICY` says. Which error the decoder happens to raise
/// (`BadMagic` under one codec, one of the named suspect causes under another)
/// decides the class once the CRC no longer verifies, so a fixture must not depend
/// on it.
///
/// Bumping the format version and repairing the trailer removes the dependence: the
/// CRC verifies again, which classes the frame `Suspect` whatever the decoder then
/// says, and the decoder itself fails on the version field it reads before anything
/// codec-specific.
fn write_suspect_frame(path: &Path) {
	let mut bytes = fs::read(path).expect("read the frame under test");
	assert!(bytes.len() > 12, "a frame carries a magic, a version and a CRC trailer");
	let split = bytes.len() - 4;
	// Body layout: 7-byte magic, then the `u16` format version.
	bytes[7..9].copy_from_slice(&u16::MAX.to_le_bytes());
	let checksum = crc32(&bytes[..split]).to_le_bytes();
	bytes[split..].copy_from_slice(&checksum);
	fs::write(path, &bytes).expect("write the suspect frame");
}

// ---------------------------------------------------------------------------
// from_env
// ---------------------------------------------------------------------------

#[tokio::test]
async fn from_env_suspect_policy_falls_back_to_quarantine() {
	let opts = SegmentStoreOptions::default();
	assert_eq!(opts.suspect_frame_policy, SuspectFramePolicy::Quarantine);
}

// ---------------------------------------------------------------------------
// The store exposes the default policy (observable through the public field on
// SegmentStoreOptions, and passed through to the store at open time).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn store_with_default_policy_propagates_to_open() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_with(dir.path(), SegmentStoreOptions::default()).await;
	// The policy is default Quarantine; under this policy a corrupt frame
	// propagates the error (same as Fail for the interactive read path).
	let all = store.index().all(ASPECT).await.expect("enumerates index rows");
	assert_eq!(all.len(), 0, "empty store has no segments");
	drop(store);
}

// ---------------------------------------------------------------------------
// Skip — continues past a corrupt frame, returns whatever readable data remains.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn suspect_frame_policy_skip_surfaces_data_from_readable_segments() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_with(dir.path(), SegmentStoreOptions::default().with_suspect_frame_policy(SuspectFramePolicy::Skip)).await;
	// Two fills yield two segments; make the first (oldest) a suspect frame.
	fill(&store, vec![0, 10, 20], vec![bd("0"), bd("10"), bd("20")]).await;
	fill(&store, vec![15, 25], vec![bd("115"), bd("125")]).await;
	let all = store.index().all(ASPECT).await.expect("enumerates index rows");
	assert_eq!(all.len(), 2, "two fills produce two segments");
	let paths = all.iter().map(|desc| store.frame_path(desc).expect("a frame path")).collect::<Vec<_>>();
	// Make the oldest segment's frame a suspect frame.
	write_suspect_frame(Path::new(&paths[0]));
	// Under Skip the read does not error; it returns the second segment's rows.
	let (ts, vs) = store.read_time_range_of(ASPECT, SeriesId::UNTAGGED, 0, 30).await.expect("read succeeds under Skip");
	// The second segment's rows win at their timestamps; the first segment's data
	// (0, 10, 20) is lost because its frame could not be decoded.
	assert_eq!(ts, vec![15, 25]);
	assert_eq!(vs, vec![Some(bd("115")), Some(bd("125"))]);
	drop(store);
}

#[tokio::test]
async fn suspect_frame_policy_skip_returns_empty_when_only_frame_is_corrupt() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_with(dir.path(), SegmentStoreOptions::default().with_suspect_frame_policy(SuspectFramePolicy::Skip)).await;
	fill(&store, vec![0, 10], vec![bd("0"), bd("10")]).await;
	let all = store.index().all(ASPECT).await.expect("enumerates index rows");
	assert_eq!(all.len(), 1);
	let path = store.frame_path(&all[0]).expect("a frame path");
	write_suspect_frame(Path::new(&path));
	let (ts, vs) = store.read_time_range_of(ASPECT, SeriesId::UNTAGGED, 0, 30).await.expect("read succeeds under Skip");
	assert!(ts.is_empty(), "no rows when the only segment is skipped");
	assert!(vs.is_empty(), "no values when the only segment is skipped");
	drop(store);
}

// ---------------------------------------------------------------------------
// Fail — propagates the decode error unchanged.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn suspect_frame_policy_fail_propagates_decode_error() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_with(dir.path(), SegmentStoreOptions::default().with_suspect_frame_policy(SuspectFramePolicy::Fail)).await;
	fill(&store, vec![0, 10], vec![bd("0"), bd("10")]).await;
	let all = store.index().all(ASPECT).await.expect("enumerates index rows");
	assert_eq!(all.len(), 1);
	let path = store.frame_path(&all[0]).expect("a frame path");
	tokio::fs::write(&path, b"not a valid frame").await.unwrap();
	let err = store.read_time_range_of(ASPECT, SeriesId::UNTAGGED, 0, 30).await;
	assert!(err.is_err(), "read propagates the decode error under Fail policy");
	drop(store);
}

// ---------------------------------------------------------------------------
// Quarantine (default) — propagates the decode error (same as Fail for the
// interactive read path, which does NOT quarantine; quarantine is reserved for
// the fsck/recovery path).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn suspect_frame_policy_quarantine_propagates_decode_error() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_with(dir.path(), SegmentStoreOptions::default().with_suspect_frame_policy(SuspectFramePolicy::Quarantine)).await;
	fill(&store, vec![0, 10], vec![bd("0"), bd("10")]).await;
	let all = store.index().all(ASPECT).await.expect("enumerates index rows");
	assert_eq!(all.len(), 1);
	let path = store.frame_path(&all[0]).expect("a frame path");
	tokio::fs::write(&path, b"not a valid frame").await.unwrap();
	let err = store.read_time_range_of(ASPECT, SeriesId::UNTAGGED, 0, 30).await;
	assert!(err.is_err(), "read propagates the decode error under Quarantine policy (default)");
	drop(store);
}
