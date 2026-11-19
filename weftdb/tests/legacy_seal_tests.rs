#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]
//! Seal-backed legacy aspects (slice D-S19): an aspect created while
//! `WEFT_LEGACY_DEFAULT_STORAGE=segments` stores through the seal store,
//! reads back through it, replays a retried batch instead of duplicating
//! it, and widens its declared encoding when a value needs it — while an
//! aspect created without the variable, or with a compression config,
//! stays in rows mode.
//!
//! The two bulk entry points that do not read the stored range first
//! (`capture_new_measurement`, `batch_capture_new_measurements`) route there
//! too (#232): an un-routed write landed in the per-aspect rows-mode file that
//! a seal-backed aspect's reads never consult, so it succeeded and read back
//! as nothing.

use std::str::FromStr;

use anyhow::Result;
use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use futures::StreamExt;
use serial_test::serial;
use tempfile::TempDir;
use uuid::Uuid;
use weftdb::legacy_seal::{aspect_key, ingest_batch_with_gate, read_measurements, read_rows};

/// The fragmentation gate at a small target converges repeated ingests of the same aspect.
///
/// This test must fail if the gate call inside `ingest_batch_with_gate` is removed:
/// without it, many small ingests leave many small sealed segments whose count is not
/// collapsed by the size-targeted compaction.
#[serial]
#[tokio::test]
async fn bulk_ingest_converges_at_small_target() -> Result<()> {
	std::env::set_var("WEFT_LEGACY_DEFAULT_STORAGE", "segments");
	let (_dir, db) = setup().await?;
	let subject = db.observe_subject("s").await?;
	let aspect_id = db.track_aspect(&subject.id(), "converge_test", &splimes::Resolution::Milliseconds, None).await?.id();

	let dataset = DatasetId::new();
	let store = db.seal_store().await?;
	let base = Utc::now().timestamp_millis();

	// 20 batches of 3 rows each at a target of 4: the gate should squash to ~15 segments.
	// Without the gate we would have 20+ segments.
	for i in 0..20 {
		let offset = i64::from(i) * 1000;
		let rows = vec![(base + offset, bd(&format!("{}.{}", i + 1, offset))), (base + offset + 1, bd(&format!("{}.{}", i + 2, offset))), (base + offset + 2, bd(&format!("{}.{}", i + 3, offset)))];
		ingest_batch_with_gate(&store, &aspect_id, &dataset, None, &rows, 4).await?;
	}

	// Count the segments after all ingests.
	let key = aspect_key(&aspect_id);
	let segment_count = store.index().count(&key).await?;
	// 20 batches × 3 rows = 60 rows ÷ 4 target = 15 ideal, with three segments of slack for
	// the tail of the last fold. The old bound of 25 was looser than the un-gated count
	// (~20, one segment per batch), so the test passed with the gate call deleted.
	assert!(segment_count <= 18, "the fragmentation gate should have collapsed {segment_count} segments to roughly 15 at a target of 4");

	// The comparison that makes the bound above mean something: the same volume of traffic
	// into a second aspect, at a target so loose the gate stays idle (60 rows ÷ 2 = 30 ideal,
	// above the ~20 segments the batches produce), must end with strictly more segments.
	// Delete the gate call and both assertions fail together.
	let ungated_aspect = db.track_aspect(&subject.id(), "converge_idle", &splimes::Resolution::Milliseconds, None).await?.id();
	for i in 0..20 {
		let offset = i64::from(i) * 1000;
		let rows = vec![(base + offset, bd(&format!("{}.{}", i + 1, offset))), (base + offset + 1, bd(&format!("{}.{}", i + 2, offset))), (base + offset + 2, bd(&format!("{}.{}", i + 3, offset)))];
		ingest_batch_with_gate(&store, &ungated_aspect, &dataset, None, &rows, 2).await?;
	}
	let idle_count = store.index().count(&aspect_key(&ungated_aspect)).await?;
	assert!(idle_count > segment_count, "the gated aspect converged to {segment_count} segments while the same traffic with the gate idle left {idle_count}; the gate is what makes bulk ingest converge");

	drop(store);
	std::env::remove_var("WEFT_LEGACY_DEFAULT_STORAGE");
	Ok(())
}

/// After a gated ingest, every row is readable exactly once, in order,
/// with the same `MeasurementId` derivation as a second read.
#[serial]
#[tokio::test]
async fn gated_bulk_read_path_correctness() -> Result<()> {
	std::env::set_var("WEFT_LEGACY_DEFAULT_STORAGE", "segments");
	let (_dir, db) = setup().await?;
	let subject = db.observe_subject("s").await?;
	let aspect_id = db.track_aspect(&subject.id(), "read_correct", &splimes::Resolution::Milliseconds, None).await?.id();

	let dataset = DatasetId::new();
	let store = db.seal_store().await?;
	let base = Utc::now().timestamp_millis();

	// Ingest 10 batches of 2 rows each through the gate path.
	let mut all_expected: Vec<(i64, BigDecimal)> = Vec::new();
	for i in 0..10 {
		let offset = i64::from(i) * 2000;
		let rows = vec![(base + offset, bd(&format!("{i}.0"))), (base + offset + 1000, bd(&format!("{i}.5")))];
		ingest_batch_with_gate(&store, &aspect_id, &dataset, None, &rows, 4).await?;
		all_expected.extend(rows);
	}
	all_expected.sort_by_key(|(ts, _)| *ts);

	// Read rows back.
	let read_back = read_rows(&store, &aspect_id, None, None).await?;
	assert_eq!(read_back.len(), all_expected.len(), "every row is returned exactly once");
	for (got, expected) in read_back.iter().zip(all_expected.iter()) {
		assert_eq!(got, expected, "row values match in order");
	}

	// Read measurements back: ids are deterministic.
	let ms1 = read_measurements(&store, &aspect_id, None, None).await?;
	let ms2 = read_measurements(&store, &aspect_id, None, None).await?;
	assert_eq!(ms1.len(), all_expected.len(), "every row returned as a Measurement");
	for (m, e) in ms1.iter().zip(all_expected.iter()) {
		assert_eq!(m.timestamp().timestamp_millis(), e.0, "timestamp from Measurement matches");
		assert_eq!(*m.value(), e.1, "value from Measurement matches");
	}
	// A second read produces the same ids.
	for (m1, m2) in ms1.iter().zip(ms2.iter()) {
		assert_eq!(m1.id(), m2.id(), "second read produces the same MeasurementId");
	}

	drop(store);
	std::env::remove_var("WEFT_LEGACY_DEFAULT_STORAGE");
	Ok(())
}

/// A degenerate squash target must not damage the ingest contract. `target_rows = 0`
/// is clamped to 1 by the gate internals (`maint.rs:2060`), and at two rows the aspect
/// is not over-fragmented, so the gate runs and returns without squashing anything.
/// What this test therefore proves is that a nonsense target still ingests both rows
/// and returns both `TxId`s. It does **not** prove that a gate *error* is swallowed:
/// forcing one needs the `fault-injection` feature, and that gap is tracked separately.
#[serial]
#[tokio::test]
async fn gate_at_degenerate_target_still_ingests() -> Result<()> {
	std::env::set_var("WEFT_LEGACY_DEFAULT_STORAGE", "segments");
	let (_dir, db) = setup().await?;
	let subject = db.observe_subject("s").await?;
	let aspect_id = db.track_aspect(&subject.id(), "gate_fail", &splimes::Resolution::Milliseconds, None).await?.id();

	let dataset = DatasetId::new();
	let store = db.seal_store().await?;
	let base = Utc::now().timestamp_millis();

	let rows = vec![(base, bd("42.5")), (base + 1000, bd("43.5"))];
	// target=0 is clamped to 1 by the gate internals; the gate runs and
	// the ingest succeeds regardless.
	let tx_ids = ingest_batch_with_gate(&store, &aspect_id, &dataset, None, &rows, 0).await?;
	assert_eq!(tx_ids.len(), 2, "two TxIds returned despite gate target=0");

	// Rows are readable.
	let read_back = read_rows(&store, &aspect_id, None, None).await?;
	assert_eq!(read_back.len(), 2, "rows readable after gated ingest");
	assert!(read_back.contains(&(base, bd("42.5"))));
	assert!(read_back.contains(&(base + 1000, bd("43.5"))));

	drop(store);
	std::env::remove_var("WEFT_LEGACY_DEFAULT_STORAGE");
	Ok(())
}

/// Idempotency still holds when the fragmentation gate is enabled: replaying the same
/// batch with the same explicit token returns the same synthetic `TxIds` and does not
/// duplicate rows.
#[serial]
#[tokio::test]
async fn gated_ingest_idempotent_replay() -> Result<()> {
	std::env::set_var("WEFT_LEGACY_DEFAULT_STORAGE", "segments");
	let (_dir, db) = setup().await?;
	let subject = db.observe_subject("s").await?;
	let aspect_id = db.track_aspect(&subject.id(), "idempotent", &splimes::Resolution::Milliseconds, None).await?.id();

	let dataset = DatasetId::new();
	let store = db.seal_store().await?;
	let base = Utc::now().timestamp_millis();

	let token = "replay-token";
	let rows = vec![(base, bd("10.0")), (base + 1000, bd("20.0")), (base + 2000, bd("30.0"))];

	// First call with explicit token.
	let tx1 = ingest_batch_with_gate(&store, &aspect_id, &dataset, Some(token), &rows, 4).await?;
	assert_eq!(tx1.len(), 3, "first call returns 3 TxIds");

	// Replay the same batch with the same token.
	let tx2 = ingest_batch_with_gate(&store, &aspect_id, &dataset, Some(token), &rows, 4).await?;
	assert_eq!(tx2, tx1, "replay with the same token returns the same TxIds");

	// No duplicate rows.
	let read_back = read_rows(&store, &aspect_id, None, None).await?;
	assert_eq!(read_back.len(), 3, "no duplicate rows after replay with gate");

	drop(store);
	std::env::remove_var("WEFT_LEGACY_DEFAULT_STORAGE");
	Ok(())
}
use weftdb::{
	database::traits::{AspectStructure, DatabaseStructure, Inputs, Outputs}, Database, DatasetId, InputMeasurement, StorageMode
};

fn bd(s: &str) -> BigDecimal {
	BigDecimal::from_str(s).expect("decimal")
}

async fn setup() -> Result<(TempDir, Database)> {
	let temp_dir = tempfile::tempdir()?;
	std::env::set_var("WEFT_DATA_DIR", temp_dir.path().to_str().unwrap());
	let db_name = format!("seal_db_{}", Uuid::new_v4());
	let db = Database::new(&db_name).await?;
	Ok((temp_dir, db))
}

#[serial]
#[tokio::test]
async fn seal_backed_aspect_round_trips_replays_and_widens() -> Result<()> {
	std::env::set_var("WEFT_LEGACY_DEFAULT_STORAGE", "segments");
	let (_dir, db) = setup().await?;
	let subject = db.observe_subject("s").await?;
	let aspect = db.track_aspect(&subject.id(), "sealed", &splimes::Resolution::Milliseconds, None).await?;
	let aspect_id = aspect.id();
	assert_eq!(aspect.storage_mode(), StorageMode::Segments, "the aspect is created seal-backed");
	assert_eq!(db.aspect_storage_mode(&aspect_id).await?, StorageMode::Segments);

	let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
	let dataset = DatasetId::new();
	let rows: Vec<InputMeasurement> = (0..5).map(|i| InputMeasurement::new(base + Duration::seconds(i), bd(&format!("{}.5", i + 1)))).collect();

	// One batch ingest: all-or-nothing.  Without an explicit token each call mints a
	// fresh nonce, so a second call with the same payload is a new write.
	let tx1 = db.batch_capture_measurements(aspect_id, dataset, rows.clone()).await?;
	let tx2 = db.batch_capture_measurements(aspect_id, dataset, rows.clone()).await?;
	assert_ne!(tx2, tx1, "a second call without an explicit token produces fresh TxIds");
	assert_eq!(db.get_measurements_count(&aspect_id).await?, 10, "both writes landed, giving 2x the rows");

	// Raw reads come back in timestamp order with the stored values.
	let raw: Vec<_> = db.get_raw_measurements(&aspect_id, None, None, 100, 0).await?.collect::<Vec<_>>().await.into_iter().collect::<Result<Vec<_>>>()?;
	assert_eq!(raw.len(), 5);
	for (i, m) in raw.iter().enumerate() {
		assert_eq!(m.timestamp(), base + Duration::seconds(i64::try_from(i).expect("fits an i64")));
		assert_eq!(*m.value(), bd(&format!("{}.5", i + 1)));
	}

	// Range, boundary, earliest and latest agree.
	let ranged: Vec<_> = db.fetch_measurements_for_range(&aspect_id, base + Duration::seconds(1), base + Duration::seconds(3)).await?.collect::<Vec<_>>().await.into_iter().collect::<Result<Vec<_>>>()?;
	assert_eq!(ranged.len(), 3, "the range read is inclusive, as rows mode's was");
	assert_eq!(db.get_earliest_measurement(&aspect_id).await?, Some(base));
	assert_eq!(db.get_latest_measurement(&aspect_id).await?, Some(base + Duration::seconds(4)));
	let boundary: Vec<_> = db.get_boundary_measurements(&aspect_id).await?.collect::<Vec<_>>().await.into_iter().collect::<Result<Vec<_>>>()?;
	assert_eq!(boundary.len(), 4, "earliest two and latest two");
	assert_eq!(boundary.first().map(weftdb::Measurement::timestamp), Some(base));
	assert_eq!(boundary.last().map(weftdb::Measurement::timestamp), Some(base + Duration::seconds(4)));

	// A single capture is a one-row ingest.
	db.capture_measurement(&aspect_id, &dataset, &InputMeasurement::new(base + Duration::seconds(10), bd("99.25"))).await?;
	assert_eq!(db.get_measurements_count(&aspect_id).await?, 6);
	assert_eq!(db.get_latest_measurement(&aspect_id).await?, Some(base + Duration::seconds(10)));

	// Widening: a value Decimal128 cannot hold exactly widens the aspect's
	// declared encoding to BigDecimalText, and reads back exactly, beside
	// the frames sealed before the widening.
	let wide = "12345678901234567890123456789012345678901234567890.5";
	db.batch_capture_measurements(aspect_id, dataset, vec![InputMeasurement::new(base + Duration::seconds(20), bd(wide))]).await?;
	let store = db.seal_store().await?;
	let schema = store.schema_for(&weftdb::legacy_seal::aspect_key(&aspect_id)).await?.expect("the aspect is declared");
	drop(store);
	assert_eq!(schema.value, weft_physical_type::PhysicalType::BigDecimalText, "the declaration widened through widen_value_encoding");
	let all: Vec<_> = db.get_raw_measurements(&aspect_id, None, None, 100, 0).await?.collect::<Vec<_>>().await.into_iter().collect::<Result<Vec<_>>>()?;
	assert_eq!(all.len(), 7);
	assert_eq!(*all.last().expect("the wide row").value(), bd(wide), "the wide value round-trips exactly");
	assert_eq!(*all.first().expect("the first row").value(), bd("1.5"), "pre-widening frames still read");

	// The frames live under the database's segments_v2 root.
	let db_dir = std::path::Path::new(db.metadata_path()).parent().expect("db dir").to_path_buf();
	assert!(db_dir.join("segments_v2").join("segments").exists(), "frames live in segments_v2/segments");

	std::env::remove_var("WEFT_LEGACY_DEFAULT_STORAGE");
	Ok(())
}

/// #768: seal-backed is the default now, so an aspect nobody configured gets the
/// seal store. The mode is persisted at creation and read back through the ordinary
/// read paths, and the frames land under `segments_v2` - which is what makes the
/// bulk writers routed in #232 part 1 the path a fresh deployment takes. This test
/// fails on a tree without the flip.
#[serial]
#[tokio::test]
async fn a_new_aspect_is_seal_backed_without_being_told() -> Result<()> {
	std::env::remove_var("WEFT_LEGACY_DEFAULT_STORAGE");
	let (_dir, db) = setup().await?;
	let subject = db.observe_subject("s").await?;
	let aspect = db.track_aspect(&subject.id(), "unspoken", &splimes::Resolution::Milliseconds, None).await?;
	assert_eq!(aspect.storage_mode(), StorageMode::Segments, "a new aspect is seal-backed unless someone asks for rows mode");
	assert_eq!(db.aspect_storage_mode(&aspect.id()).await?, StorageMode::Segments, "the choice is persisted at creation, not kept in memory");

	let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
	let rows: Vec<InputMeasurement> = (0..5).map(|i| InputMeasurement::new(base + Duration::seconds(i), bd(&format!("{}.5", i + 1)))).collect();
	db.batch_capture_measurements(aspect.id(), DatasetId::new(), rows).await?;
	assert_eq!(db.get_measurements_count(&aspect.id()).await?, 5, "the batch reached the store a seal-backed aspect reads from");
	assert_eq!(db.get_latest_measurement(&aspect.id()).await?, Some(base + Duration::seconds(4)), "the newest row is the newest sealed row");

	let db_dir = std::path::Path::new(db.metadata_path()).parent().expect("db dir").to_path_buf();
	assert!(db_dir.join("segments_v2").join("segments").exists(), "frames live in segments_v2/segments");
	Ok(())
}

#[serial]
#[tokio::test]
async fn the_rows_mode_opt_out_and_a_compression_config_both_keep_rows_mode() -> Result<()> {
	// Rows mode is the opt-out since #768: the test names it instead of relying on an
	// absent variable, which is exactly what a deployment that must stay rollable to an
	// older build does.
	std::env::set_var("WEFT_LEGACY_DEFAULT_STORAGE", "rows");
	let (_dir, db) = setup().await?;
	let subject = db.observe_subject("s").await?;
	let plain = db.track_aspect(&subject.id(), "plain", &splimes::Resolution::Milliseconds, None).await?;
	assert_eq!(plain.storage_mode(), StorageMode::Rows);
	assert_eq!(db.aspect_storage_mode(&plain.id()).await?, StorageMode::Rows);

	// Even with the variable set, a compression config keeps rows mode.
	std::env::set_var("WEFT_LEGACY_DEFAULT_STORAGE", "segments");
	let compressed = db.track_aspect(&subject.id(), "compressed", &splimes::Resolution::Milliseconds, Some(weftdb::CompressionConfig::default())).await;
	drop(subject);
	std::env::remove_var("WEFT_LEGACY_DEFAULT_STORAGE");
	if let Ok(compressed) = &compressed {
		assert_eq!(compressed.storage_mode(), StorageMode::Rows, "compression deletes ranges, so it stays rows mode");
		assert_eq!(db.aspect_storage_mode(&compressed.id()).await?, StorageMode::Rows);
	}
	drop(compressed);

	// The rows-mode aspect still stores and reads as before.
	let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
	db.batch_capture_measurements(plain.id(), DatasetId::new(), vec![InputMeasurement::new(base, bd("3.5"))]).await?;
	assert_eq!(db.get_measurements_count(&plain.id()).await?, 1);

	// The un-routed bulk entry points are unchanged in rows mode (#232): every row
	// is inserted, and a timestamp already stored is stored again — rows mode has no
	// unique constraint to violate, and nothing averages or rejects it.
	db.batch_capture_new_measurements(&plain.id(), &DatasetId::new(), vec![InputMeasurement::new(base + Duration::seconds(1), bd("4.5"))]).await?;
	db.capture_new_measurement(&plain.id(), &DatasetId::new(), &InputMeasurement::new(base, bd("3.75"))).await?;
	assert_eq!(db.get_measurements_count(&plain.id()).await?, 3, "rows mode stores every row, duplicate timestamp included");
	assert!(db.batch_capture_new_measurements(&plain.id(), &DatasetId::new(), Vec::new()).await?.is_empty(), "an empty batch through the un-routed entry point stores nothing");
	drop(plain);
	Ok(())
}

/// What #232 fixes, stated as the failure it removes: on `main` these two calls
/// returned `Ok` and the aspect's stored count stayed at zero, because the write
/// went to the per-aspect rows-mode `measurements.db` while every read of a
/// seal-backed aspect dispatches to the seal store (slice D-S19).
#[serial]
#[tokio::test]
async fn seal_backed_new_measurement_entry_points_reach_the_seal() -> Result<()> {
	std::env::set_var("WEFT_LEGACY_DEFAULT_STORAGE", "segments");
	let (_dir, db) = setup().await?;
	let subject = db.observe_subject("s").await?;
	let aspect_id = db.track_aspect(&subject.id(), "sealed_new", &splimes::Resolution::Milliseconds, None).await?.id();
	assert_eq!(db.aspect_storage_mode(&aspect_id).await?, StorageMode::Segments, "the aspect is created seal-backed");

	let base = Utc.with_ymd_and_hms(2024, 3, 1, 0, 0, 0).unwrap();
	let base_ms = base.timestamp_millis();
	let dataset = DatasetId::new();
	let rows: Vec<InputMeasurement> = (0..4).map(|i| InputMeasurement::new(base + Duration::seconds(i), bd(&format!("{}.25", i + 1)))).collect();

	// The bulk entry point is one ingest transaction.  Without an explicit token each
	// call mints a fresh nonce, so a repeated call with the same payload is a new write.
	let tx1 = db.batch_capture_new_measurements(&aspect_id, &dataset, rows.clone()).await?;
	assert_eq!(tx1.len(), 4, "one synthetic TxId per row of the batch");
	assert_eq!(db.get_measurements_count(&aspect_id).await?, 4, "the batch reached the store a seal-backed aspect reads from");
	let tx2 = db.batch_capture_new_measurements(&aspect_id, &dataset, rows.clone()).await?;
	assert_ne!(tx2, tx1, "a second call without an explicit token produces fresh TxIds");
	assert_eq!(db.get_measurements_count(&aspect_id).await?, 8, "both writes landed, giving 2x the rows");

	// An empty batch is a no-op, not the adapter's empty-payload error, and it is
	// checked before the seal dispatch so both modes behave the same.
	assert!(db.batch_capture_new_measurements(&aspect_id, &dataset, Vec::new()).await?.is_empty(), "an empty batch stores nothing and errors not");

	// The single-row entry point is a one-row ingest, and the range facts it feeds
	// (latest, boundary) see the write.
	db.capture_new_measurement(&aspect_id, &dataset, &InputMeasurement::new(base + Duration::seconds(9), bd("9.5"))).await?;
	assert_eq!(db.get_measurements_count(&aspect_id).await?, 5, "the single capture reaches the seal too");
	assert_eq!(db.get_latest_measurement(&aspect_id).await?, Some(base + Duration::seconds(9)), "the aspect's latest timestamp sees it");

	// Every row the two entry points wrote is in the seal frames, read back through
	// the same adapter the read path uses. Whether a rows-mode `measurements.db` also
	// exists on disk for this aspect is not asserted here: creation may open it for
	// reasons unrelated to these writes, and the point of the fix is what the reads
	// see, not what the bookkeeping leaves behind. The frames' own row total is not
	// asserted either: nothing here compares it to anything, so a length that agreed
	// with the rollup only by luck would fail without saying anything true.
	let store = db.seal_store().await?;
	let sealed = weftdb::legacy_seal::read_rows(&store, &aspect_id, None, None).await?;
	drop(store);
	assert!(sealed.contains(&(base_ms, bd("1.25"))), "the batch's first row is in the seal frames");
	assert!(sealed.contains(&(base_ms + 9_000, bd("9.5"))), "the single-row write is in the seal frames, stored as written");

	// A second write at a timestamp the aspect already holds replaces the old value:
	// the newest call's rows win at a shared timestamp on seal-backed aspects
	// (SEAL-DUP #802). Duplicates inside the same call are all kept; duplicates
	// across calls replace.
	db.capture_new_measurement(&aspect_id, &dataset, &InputMeasurement::new(base, bd("100.5"))).await?;
	assert_eq!(db.get_measurements_count(&aspect_id).await?, 5, "a second row at a timestamp the aspect already has leaves the count where it was");

	// The new value is present and the old value is absent at the rewritten timestamp.
	let store2 = db.seal_store().await?;
	let sealed2 = weftdb::legacy_seal::read_rows(&store2, &aspect_id, None, None).await?;
	drop(store2);
	assert!(sealed2.contains(&(base_ms, bd("100.5"))), "the single-row duplicate write replaced the value at base with 100.5");
	assert!(!sealed2.contains(&(base_ms, bd("1.25"))), "the old value at base was replaced and is gone");

	// Batch write with overlapping timestamps also replaces: the new batch's rows
	// at a shared timestamp supersede the older rows.
	db.batch_capture_new_measurements(&aspect_id, &dataset, vec![InputMeasurement::new(base + Duration::seconds(1), bd("200.5")), InputMeasurement::new(base + Duration::seconds(2), bd("300.5"))]).await?;
	assert_eq!(db.get_measurements_count(&aspect_id).await?, 5, "the batch duplicate write replaced two rows; count stays at 5");

	let store3 = db.seal_store().await?;
	let sealed3 = weftdb::legacy_seal::read_rows(&store3, &aspect_id, None, None).await?;
	drop(store3);
	// New values present.
	assert!(sealed3.contains(&(base_ms + 1000, bd("200.5"))), "the batch write replaced the value at base+1s with 200.5");
	assert!(sealed3.contains(&(base_ms + 2000, bd("300.5"))), "the batch write replaced the value at base+2s with 300.5");
	// Old values absent at the overwritten timestamps.
	assert!(!sealed3.contains(&(base_ms + 1000, bd("2.25"))), "the old value at base+1s is gone after the batch duplicate write");
	assert!(!sealed3.contains(&(base_ms + 2000, bd("3.25"))), "the old value at base+2s is gone after the batch duplicate write");
	// Un-overwritten rows are still there: base+3s (4.25) and base+9s (9.5).
	assert!(sealed3.contains(&(base_ms + 3000, bd("4.25"))), "the un-touched row at base+3s is still present after the batch duplicate write");
	assert!(sealed3.contains(&(base_ms + 9_000, bd("9.5"))), "the un-touched row at base+9s is still present");

	std::env::remove_var("WEFT_LEGACY_DEFAULT_STORAGE");
	Ok(())
}

/// No pure `(DatasetId, payload) -> key` derivation can satisfy both replay of a retry and
/// fresh-key for an identical resubmission after a different write (the A-B-A hole).  The fix
/// makes the idempotency unit a caller-supplied per-call token instead:
///
///   (a) same explicit token + identical payload → replay (tx2 == tx1, one row).
///   (b) different token (or None) + identical payload → a real second write (tx2 != tx1,
///       two rows).
#[serial]
#[tokio::test]
async fn reused_dataset_id_swallows_last_write_in_aba() -> Result<()> {
	std::env::set_var("WEFT_LEGACY_DEFAULT_STORAGE", "segments");
	let (_dir, db) = setup().await?;
	let subject = db.observe_subject("s").await?;
	let aspect_id = db.track_aspect(&subject.id(), "reused_id", &splimes::Resolution::Milliseconds, None).await?.id();

	let dataset = DatasetId::new();
	let store = db.seal_store().await?;
	let base = Utc::now().timestamp_millis();

	// --- (a) Same explicit token + identical payload → replay ---
	let replay_token = "replay-aba";
	let tx1 = ingest_batch_with_gate(&store, &aspect_id, &dataset, Some(replay_token), &[(base, bd("5"))], 4).await?;
	assert_eq!(tx1.len(), 1, "first call returns one TxId");
	let after_first = read_rows(&store, &aspect_id, None, None).await?;
	assert_eq!(after_first.len(), 1, "one row after first call");

	let tx2 = ingest_batch_with_gate(&store, &aspect_id, &dataset, Some(replay_token), &[(base, bd("5"))], 4).await?;
	assert_eq!(tx2, tx1, "same token + same payload replays, returning the same TxId");
	let after_replay = read_rows(&store, &aspect_id, None, None).await?;
	assert_eq!(after_replay.len(), 1, "replay stored nothing twice");

	// --- (b) Different token (or None) + identical payload → a real second write ---
	let fresh_token = "fresh-aba";
	let tx3 = ingest_batch_with_gate(&store, &aspect_id, &dataset, Some(fresh_token), &[(base, bd("5"))], 4).await?;
	assert_ne!(tx3, tx1, "different token produces fresh TxIds");
	let after_fresh = read_rows(&store, &aspect_id, None, None).await?;
	assert_eq!(after_fresh.len(), 2, "a different token landed a second row at the same timestamp");

	drop(store);
	std::env::remove_var("WEFT_LEGACY_DEFAULT_STORAGE");
	Ok(())
}

/// `capture_measurement_chunk` was the remaining entry point that did not route
/// to the seal store on a seal-backed aspect (#779 follow-up for the #232 class).
/// Without the fix, a chunk written through this path lands in the per-aspect
/// rows-mode `measurements.db` where no seal-backed read can find it.
#[serial]
#[tokio::test]
async fn seal_backed_capture_measurement_chunk_reaches_the_seal() -> Result<()> {
	std::env::set_var("WEFT_LEGACY_DEFAULT_STORAGE", "segments");
	let (_dir, db) = setup().await?;
	let subject = db.observe_subject("s").await?;
	let aspect_id = db.track_aspect(&subject.id(), "sealed_chunk", &splimes::Resolution::Milliseconds, None).await?.id();
	assert_eq!(db.aspect_storage_mode(&aspect_id).await?, StorageMode::Segments, "the aspect is created seal-backed");

	let base = Utc.with_ymd_and_hms(2024, 4, 1, 0, 0, 0).unwrap();
	let base_ms = base.timestamp_millis();
	let rows: Vec<InputMeasurement> = (0..10).map(|i| InputMeasurement::new(base + Duration::seconds(i), bd(&format!("{i}.0")))).collect();

	// This entry point writes a chunk through the seal store when the aspect
	// is seal-backed (#779). Before the fix it wrote to rows-mode measurements.db
	// and the count was 0 after this call, because reads go to the seal.
	let tx_ids = db.capture_measurement_chunk(&aspect_id, DatasetId::new(), &rows).await?;
	assert_eq!(tx_ids.len(), 10, "one synthetic TxId per row");
	assert_eq!(db.get_measurements_count(&aspect_id).await?, 10, "the chunk reached the seal store that reads consult");

	// The rows are in the seal frames, not in rows-mode bricks that no read will touch.
	let store = db.seal_store().await?;
	let sealed = weftdb::legacy_seal::read_rows(&store, &aspect_id, None, None).await?;
	drop(store);
	for i in 0..10 {
		assert!(sealed.contains(&(base_ms + i64::from(i) * 1000, bd(&format!("{i}.0")))), "row {i} is in the seal frames");
	}

	std::env::remove_var("WEFT_LEGACY_DEFAULT_STORAGE");
	Ok(())
}

/// A **red** test on `main` -- two separate calls with one `DatasetId` and the **identical**
/// single row write an identical key, and the second call replays the first's receipt -
/// silently losing the write.  After the fix each call with `None` (the default) mints a
/// fresh token, so the second write lands and the read returns **two** rows at that
/// timestamp (matching rows-mode's append contract, which seal must match).
#[serial]
#[tokio::test]
async fn identical_resubmission_under_one_dataset_id_is_not_silently_dropped() -> Result<()> {
	std::env::set_var("WEFT_LEGACY_DEFAULT_STORAGE", "segments");
	let (_dir, db) = setup().await?;
	let subject = db.observe_subject("s").await?;
	let aspect_id = db.track_aspect(&subject.id(), "identical_resubmission", &splimes::Resolution::Milliseconds, None).await?.id();

	let dataset = DatasetId::new();
	let store = db.seal_store().await?;
	let base = Utc::now().timestamp_millis();

	// Two separate write calls, same DatasetId, same single row.
	let tx1 = ingest_batch_with_gate(&store, &aspect_id, &dataset, None, &[(base, bd("5"))], 4).await?;
	assert_eq!(tx1.len(), 1, "first call returns one TxId");
	let tx2 = ingest_batch_with_gate(&store, &aspect_id, &dataset, None, &[(base, bd("5"))], 4).await?;
	assert_eq!(tx2.len(), 1, "second call returns one TxId");

	// *** Red assertion: the read must return two rows, proving the second call was not silently dropped. ***
	let read_back = read_rows(&store, &aspect_id, None, None).await?;
	assert_eq!(read_back.len(), 2, "two rows at the same timestamp: the second call was not silently dropped (bug: 1 on main)");

	// Both rows have the value we wrote (the store's newer-wins merge preserves both).
	let matching = read_back.iter().filter(|(_, v)| *v == bd("5")).count();
	assert_eq!(matching, 2, "both stored rows carry the written value");

	drop(store);
	std::env::remove_var("WEFT_LEGACY_DEFAULT_STORAGE");
	Ok(())
}
/// The count and the range-read length must agree after two batches whose timestamps
/// overlap for the same series, even when the fragmentation gate is idle.
///
/// On `origin/main` without the `reconcile_overlaps` fix, the count is the sum of both
/// segments' `row_count` (6) while the read merges newest-wins and returns 4 rows.
/// The assertion then fails. After the fix, `reconcile_overlaps` folds the overlap at
/// write time, so the count and the read agree.
#[serial]
#[tokio::test]
async fn count_agrees_with_range_read_after_overlapping_ingests() -> Result<()> {
	std::env::set_var("WEFT_LEGACY_DEFAULT_STORAGE", "segments");
	let (_dir, db) = setup().await?;
	let subject = db.observe_subject("s").await?;
	let aspect_id = db.track_aspect(&subject.id(), "count_parity", &splimes::Resolution::Milliseconds, None).await?.id();

	let dataset = DatasetId::new();
	let store = db.seal_store().await?;
	let base = Utc::now().timestamp_millis();

	// Two batches whose time ranges overlap.  target_rows=4 is small enough that
	// the gate stays idle (segment_count(2) <= ideal(total_rows=6).div_ceil(4)=2).
	// Without the reconcile_overlaps fix the count is the sum of both row_counts (6)
	// while a range read returns 4 rows (newer-wins at the overlapping timestamps).
	let rows1 = vec![(base, bd("10.0")), (base + 1000, bd("20.0")), (base + 2000, bd("30.0"))];
	let rows2 = vec![(base + 1000, bd("21.0")), (base + 2000, bd("31.0")), (base + 3000, bd("40.0"))];

	ingest_batch_with_gate(&store, &aspect_id, &dataset, None, &rows1, 4).await?;
	ingest_batch_with_gate(&store, &aspect_id, &dataset, None, &rows2, 4).await?;

	let count = db.get_measurements_count(&aspect_id).await?;
	let read = read_rows(&store, &aspect_id, None, None).await?;

	assert_eq!(count, read.len(), "the seal-backed count ({count}) equals the number of rows a range read returns ({}) after overlapping segments", read.len());

	drop(store);
	std::env::remove_var("WEFT_LEGACY_DEFAULT_STORAGE");
	Ok(())
}
