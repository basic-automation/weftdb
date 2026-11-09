//! A pipeline's dictionary stays bounded over a long run of noisy minute data.
//!
//! Each "run" here is what `Pipeline::run` does to a dictionary: build and process the new
//! batches, then load the dictionary from its store, extract the batches into it and write
//! it back (`Pipeline::extract_patterns`). Before the fix every load added the stored
//! patterns on top of the in-memory ones and every write appended the whole dictionary to
//! the store again, so the stored rows (and the memory a load took) grew geometrically:
//! 73 MB to 3.5 GB of `preamble.db` per aspect within ten minutes in the Fan Control
//! daemon. The dictionary in this test has no variability constraints, like that one, so
//! nothing merges: each batch is one pattern with one occurrence, and the store must
//! grow linearly with the batches, never by re-storing what it already holds.

use anyhow::{Context, Result};
use bigdecimal::{BigDecimal, FromPrimitive};
use chrono::{DateTime, Duration, TimeZone, Utc};
use serial_test::serial;
use splimes::{Resolution, Spline};
use uuid::Uuid;
use weft_orchestration::{batch_utils::build_incremental_unprocessed_queue, build_patterns_queue, build_processed_batch_queue, load_dictionary};
use weftdb::{
	database::traits::{AspectStructure, DatabaseStructure, Inputs}, AspectId, Database, DatasetId, Dictionary, DictionaryConstraints, InputMeasurement, Steps
};

const BATCH_SIZE: usize = 5;
const RUNS: i64 = 30;
const ROWS_PER_RUN: i64 = 10;

fn base() -> DateTime<Utc> {
	Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
}

/// A temperature-like minute series: a slow drift plus deterministic noise.
fn minutes(range: std::ops::Range<i64>) -> Vec<InputMeasurement> {
	range.map(|i| {
		let noise = ((i * 7919) % 101) as f64 / 25.0;
		let value = 45.0 + 10.0 * ((i as f64) / 40.0).sin() + noise;
		InputMeasurement::new(base() + Duration::minutes(i), BigDecimal::from_f64(value).unwrap())
	})
	.collect()
}

async fn count(db: &Database, aspect: &AspectId, name: &str, sql: &str) -> Result<i64> {
	let conn = db.get_dictionary_db(aspect, name).await?.connect()?;
	let mut rows = conn.query(sql, ()).await?;
	let row = rows.next().await?.context("count row")?;
	Ok(row.get(0)?)
}

#[serial]
#[tokio::test(flavor = "multi_thread")]
async fn repeated_runs_store_each_pattern_once() -> Result<()> {
	let dir = tempfile::tempdir()?;
	std::env::set_var("WEFT_DATA_DIR", dir.path().to_str().context("temp dir not UTF-8")?);
	let db = Database::new(&format!("bounded_{}", Uuid::new_v4().simple())).await?;
	let subject = db.observe_subject("subject").await?;
	let aspect = db.track_aspect(&subject.id(), "aspect", &Resolution::Minutes, None).await?.id();

	let constraints = DictionaryConstraints::new(Some(Steps::new(10, Spline::Cubic)), None);
	let mut dictionary = Dictionary::new("preamble".to_string(), "bounded test".to_string(), constraints);

	let mut prev = 0;
	for run in 0..RUNS {
		db.batch_capture_measurements(aspect, DatasetId::new(), minutes(run * ROWS_PER_RUN..(run + 1) * ROWS_PER_RUN)).await?;
		build_incremental_unprocessed_queue(&db, &aspect, &Resolution::Minutes, &Spline::Linear, BATCH_SIZE).await?;
		build_processed_batch_queue(&db, &aspect).await?;
		// `Pipeline::extract_patterns`: load, extract and write back, reload.
		load_dictionary(&db, &aspect, &mut dictionary).await?;
		build_patterns_queue(&db, &aspect, &mut dictionary).await?;
		load_dictionary(&db, &aspect, &mut dictionary).await?;

		let in_memory = dictionary.len();
		let stored = count(&db, &aspect, "preamble", "SELECT COUNT(*) FROM patterns").await?;
		let distinct = count(&db, &aspect, "preamble", "SELECT COUNT(DISTINCT id) FROM patterns").await?;
		let occurrences = count(&db, &aspect, "preamble", "SELECT COUNT(*) FROM pattern_occurrences").await?;
		let relatives = count(&db, &aspect, "preamble", "SELECT COUNT(*) FROM pattern_relatives").await?;
		println!("run {run}: in memory {in_memory}, stored {stored} ({distinct} distinct), occurrences {occurrences}, relatives {relatives}");

		assert_eq!(stored, distinct, "run {run}: a pattern is stored more than once");
		assert_eq!(usize::try_from(stored)?, in_memory, "run {run}: the store and memory disagree");
		assert_eq!(occurrences, stored, "run {run}: an occurrence is stored more than once");
		assert_eq!(relatives, stored * 10, "run {run}: each pattern keeps its 10 steps");
		assert!(in_memory >= prev, "run {run}: patterns were lost");
		assert!(in_memory - prev <= ROWS_PER_RUN as usize + BATCH_SIZE, "run {run}: grew by {} from {prev}, more than this run could produce (sliding windows)", in_memory - prev);
		prev = in_memory;
	}
	assert!(prev > 0, "no patterns were extracted");
	Ok(())
}
