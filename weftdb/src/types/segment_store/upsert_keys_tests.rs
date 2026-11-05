//! Phase 4.6 upsert keys (the narrowed form): an aspect may declare `timestamp` plus a set
//! of tag keys, every ingested row's tags must fall inside them, and the declaration is
//! immutable. Dedup within a key is the store's newer-wins, before and after reconcile.

use std::str::FromStr;

use bigdecimal::BigDecimal;
use tempfile::TempDir;
use weft_physical_type::{timestamp::TimeUnit, AspectSchema, PhysicalType};

use super::SegmentStore;
use crate::{IngestBatch, StoreError, TagSet, UpsertKeys};

fn bd(s: &str) -> BigDecimal {
	BigDecimal::from_str(s).expect("parses")
}

const ASPECT: &str = "cpu";

fn schema() -> AspectSchema {
	AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)
}

fn tags(pairs: &[(&str, &str)]) -> TagSet {
	TagSet::from_pairs(pairs.iter().copied()).expect("valid tags")
}

fn host_key() -> UpsertKeys {
	UpsertKeys::new(["host"]).expect("valid keys")
}

/// A tag outside the declared keys is refused, typed, and nothing is written; a tag set
/// inside them (or the empty one) is accepted. An undeclared aspect accepts any tags.
#[tokio::test]
async fn undeclared_tag_is_refused_when_upsert_keys_are_declared() {
	let dir = TempDir::new().expect("tempdir");
	let store = SegmentStore::open(dir.path()).await.expect("opens");
	store.declare_with_upsert_keys(ASPECT, &schema(), Some(&host_key())).await.expect("declares");
	store.declare("free", &schema()).await.expect("declares an aspect without keys");
	let refused = store.ingest(ASPECT, &IngestBatch::dense("r", vec![10], vec![bd("1")]).with_groups(tags(&[("host", "a"), ("region", "eu")]))).await;
	let segments = store.index().all(ASPECT).await.expect("reads").len();
	store.ingest(ASPECT, &IngestBatch::dense("a", vec![10], vec![bd("1")]).with_groups(tags(&[("host", "a")]))).await.expect("a tag inside the keys is accepted");
	store.ingest(ASPECT, &IngestBatch::dense("u", vec![10], vec![bd("1")])).await.expect("so is the empty tag set");
	store.ingest("free", &IngestBatch::dense("f", vec![10], vec![bd("1")]).with_groups(tags(&[("host", "a"), ("region", "eu")]))).await.expect("an aspect without keys takes any tags");
	let declared = (store.upsert_keys(ASPECT).await.expect("reads"), store.upsert_keys("free").await.expect("reads"));
	drop(store);
	let err = refused.expect_err("region is not an upsert key");
	assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::NotAnUpsertKey { aspect: ASPECT.to_string(), key: "region".to_string(), declared: host_key() }), "{err:#}");
	assert_eq!(segments, 0, "nothing was written");
	assert_eq!(declared, (Some(host_key()), None));
}

/// The keys are immutable: the same keys again is a no-op, other keys, none where some are
/// declared, or some where none are, conflict and change nothing. Plain `declare` does not
/// compare them.
#[tokio::test]
async fn redeclaring_with_different_keys_conflicts() {
	let dir = TempDir::new().expect("tempdir");
	let store = SegmentStore::open(dir.path()).await.expect("opens");
	store.declare_with_upsert_keys(ASPECT, &schema(), Some(&host_key())).await.expect("declares");
	store.declare_with_upsert_keys(ASPECT, &schema(), Some(&host_key())).await.expect("the same keys again is a no-op");
	store.declare(ASPECT, &schema()).await.expect("plain declare leaves the keys out");
	let other = UpsertKeys::new(["host", "region"]).expect("valid keys");
	let conflicts = [store.declare_with_upsert_keys(ASPECT, &schema(), Some(&other)).await, store.declare_with_upsert_keys(ASPECT, &schema(), None).await];
	store.declare_with_upsert_keys("free", &schema(), None).await.expect("declares without keys");
	let late = store.declare_with_upsert_keys("free", &schema(), Some(&host_key())).await;
	let schema_first = store.declare_with_upsert_keys(ASPECT, &AspectSchema::new(PhysicalType::F32, bd("0"), TimeUnit::Seconds), Some(&other)).await;
	let declared = (store.upsert_keys(ASPECT).await.expect("reads"), store.upsert_keys("free").await.expect("reads"));
	drop(store);
	for (result, offered) in conflicts.into_iter().zip([Some(other.clone()), None]) {
		let err = result.expect_err("other keys conflict");
		assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::UpsertKeysConflict { aspect: ASPECT.to_string(), declared: Some(host_key()), offered }), "{err:#}");
	}
	let err = late.expect_err("keys cannot be added later");
	assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::UpsertKeysConflict { aspect: "free".to_string(), declared: None, offered: Some(host_key()) }), "{err:#}");
	let err = schema_first.expect_err("a schema conflict is refused too");
	assert!(matches!(err.downcast_ref::<StoreError>(), Some(StoreError::DeclarationConflict { .. })), "a schema that conflicts is reported as that: {err:#}");
	assert_eq!(declared, (Some(host_key()), None), "the declarations stand");
}

/// Two writes at one timestamp in one key (`host=a`) resolve newer-wins, read before the
/// reconcile and after it; the other key's row at that timestamp is its own series.
#[tokio::test]
async fn two_writes_at_one_ts_in_one_key_resolve_newer_wins_before_and_after_reconcile() {
	let dir = TempDir::new().expect("tempdir");
	let store = SegmentStore::open(dir.path()).await.expect("opens");
	store.declare_with_upsert_keys(ASPECT, &schema(), Some(&host_key())).await.expect("declares");
	let a = tags(&[("host", "a")]);
	store.ingest(ASPECT, &IngestBatch::dense("1", vec![10, 20], vec![bd("1"), bd("2")]).with_groups(a.clone())).await.expect("ingests");
	store.ingest(ASPECT, &IngestBatch::dense("2", vec![10], vec![bd("100")]).with_groups(a.clone())).await.expect("ingests the newer write");
	store.ingest(ASPECT, &IngestBatch::dense("3", vec![10], vec![bd("7")]).with_groups(tags(&[("host", "b")]))).await.expect("ingests another key");
	let series = store.index.series_id_for(ASPECT, a.series_key()).await.expect("reads").expect("registered");
	let before = store.read_point_of(ASPECT, series, 10).await.expect("reads");
	let removed = store.reconcile_overlaps(ASPECT).await.expect("reconciles");
	let after = store.read_point_of(ASPECT, series, 10).await.expect("reads");
	let range = store.read_time_range_of(ASPECT, series, 0, 100).await.expect("reads");
	let other = store.resolve_series(ASPECT, &crate::SeriesSelector::exact([("host", Some("b"))]).expect("valid")).await.expect("resolves").expect("one series");
	let other_point = store.read_point_of(ASPECT, other, 10).await.expect("reads");
	drop(store);
	assert_eq!(before, Some(bd("100")), "newer wins before the reconcile");
	assert_eq!(removed, 1, "the key's two segments merge");
	assert_eq!(after, Some(bd("100")), "and after it");
	assert_eq!(range, (vec![10, 20], vec![Some(bd("100")), Some(bd("2"))]), "one row per timestamp");
	assert_eq!(other_point, Some(bd("7")), "the other key is untouched");
}
