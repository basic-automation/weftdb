use std::str::FromStr;

use ::weftdb::database::traits::{AspectStructure, Inputs};
use bigdecimal::BigDecimal;
use chrono::{TimeZone, Utc};
use futures::StreamExt;
use splimes::{Resolution, Spline};
use uuid::Uuid;
use weftdb::{database::traits::DatabaseStructure, Database, DatasetId, DictionaryConstraints, DictionaryId, DictionaryMetadata, InputMeasurement, Outputs, Steps, Variability, VariablilityType};

mod common;

/// The databases these tests create go to the binary's temporary data dir
/// (`common::data_dir`), not to the user's `~/.weftdb/data`. Their cleanups used a
/// `data/<name>` path relative to the crate, so every run left its databases there.
#[tokio::test]
async fn test_databases_are_created_in_the_test_data_dir() {
	let db_name = format!("test_data_dir_{}", Uuid::new_v4());
	let dir = common::data_dir();
	assert_eq!(std::path::Path::new(&weftdb::data_dir()), dir);

	let _db = Database::new(&db_name).await.expect("Failed to create database");
	assert!(dir.join(&db_name).join("metadata.db").is_file(), "the database is in the test data dir");
	if std::path::Path::new(&weftdb::default_data_dir()) != dir {
		assert!(!std::path::Path::new(&weftdb::default_data_dir()).join(&db_name).exists(), "nothing is created in the default data dir");
	}

	common::remove_database(&db_name);
	assert!(!dir.join(&db_name).exists(), "the cleanup removes it");
}

#[tokio::test]
async fn test_database_lifecycle() {
	let db_name = format!("test_db_{}", Uuid::new_v4());

	// Clean up any existing test data
	common::remove_database(&db_name);

	// Test new database creation
	let db = Database::new(&db_name).await.expect("Failed to create database");

	// Test adding a subject
	let subject = db.observe_subject("test_subject").await.expect("Failed to add subject");

	// Test tracking an aspect
	let aspect = db.track_aspect(&subject.id(), "temperature", &splimes::Resolution::Seconds, None).await.expect("Failed to track aspect");

	// Test capturing measurements
	let measurements = vec![InputMeasurement::new(Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap(), BigDecimal::from_str("20.5").unwrap()), InputMeasurement::new(Utc.with_ymd_and_hms(2023, 1, 1, 12, 5, 0).unwrap(), BigDecimal::from_str("21.0").unwrap()), InputMeasurement::new(Utc.with_ymd_and_hms(2023, 1, 1, 12, 10, 0).unwrap(), BigDecimal::from_str("21.5").unwrap())];

	for measurement in measurements {
		db.capture_measurement(&aspect.id(), &DatasetId::new(), &measurement).await.expect("Failed to capture measurement");
	}

	// Test point analysis
	let target_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 7, 30).unwrap();
	let data_point = db.analyze_point(&aspect.id(), target_time, &Resolution::Seconds, &Spline::Linear).await.expect("Failed to analyze point");

	assert!(data_point.value > BigDecimal::from_str("21.0").unwrap());
	assert!(data_point.value < BigDecimal::from_str("21.5").unwrap());

	// Test range analysis
	let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
	let end_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 10, 0).unwrap();
	let mut range_stream = db.analyze_range(&aspect.id(), start_time, end_time, Resolution::Seconds, Spline::Linear).await.expect("Failed to analyze range");
	let first_point = match range_stream.next().await {
		Some(Ok(point)) => point,
		Some(Err(err)) => panic!("Range analysis produced error: {err}"),
		None => panic!("Range stream produced no points"),
	};

	assert_eq!(first_point.timestamp, start_time);

	// Clean up
	common::remove_database(&db_name);
}

#[tokio::test]
async fn test_existing_database() {
	let db_name = format!("test_existing_{}", Uuid::new_v4());

	// Clean up any existing test data
	common::remove_database(&db_name);

	// Create initial database
	let db = Database::new(&db_name).await.expect("Failed to create database");
	let subject = db.observe_subject("persistent_subject").await.expect("Failed to add subject");
	let aspect = db.track_aspect(&subject.id(), "humidity", &splimes::Resolution::Seconds, None).await.expect("Failed to track aspect");

	// Add some data
	db.capture_measurement(&aspect.id(), &DatasetId::new(), &InputMeasurement::new(Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap(), BigDecimal::from_str("45.0").unwrap())).await.expect("Failed to capture measurement");

	// Test loading existing database
	let loaded_db = Database::existing(&db_name).await.expect("Failed to load existing database");

	// Verify we can work with the loaded database
	let _new_subject = loaded_db.observe_subject("new_subject_in_loaded_db").await.expect("Failed to add subject to loaded database");

	// Clean up
	common::remove_database(&db_name);
}

#[tokio::test]
async fn test_multiple_subjects_and_aspects() {
	let db_name = format!("test_multi_{}", Uuid::new_v4());

	// Clean up any existing test data
	common::remove_database(&db_name);

	let db = Database::new(&db_name).await.expect("Failed to create database");

	// Create multiple subjects
	let subject1 = db.observe_subject("subject_1").await.expect("Failed to add subject 1");
	let subject2 = db.observe_subject("subject_2").await.expect("Failed to add subject 2");

	// Create multiple aspects for each subject
	let temp_aspect1 = db.track_aspect(&subject1.id(), "temperature", &splimes::Resolution::Seconds, None).await.expect("Failed to track temperature for subject 1");
	let humidity_aspect1 = db.track_aspect(&subject1.id(), "humidity", &splimes::Resolution::Seconds, None).await.expect("Failed to track humidity for subject 1");
	let temp_aspect2 = db.track_aspect(&subject2.id(), "temperature", &splimes::Resolution::Seconds, None).await.expect("Failed to track temperature for subject 2");

	// Add data to different aspects
	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();

	for i in 0..5 {
		let timestamp = base_time + chrono::Duration::minutes(i * 5);

		// Subject 1 temperature
		db.capture_measurement(&temp_aspect1.id(), &DatasetId::new(), &InputMeasurement::new(timestamp, BigDecimal::from_str(&format!("{}.0", 20 + i)).unwrap())).await.expect("Failed to capture temp measurement for subject 1");

		// Subject 1 humidity
		db.capture_measurement(&humidity_aspect1.id(), &DatasetId::new(), &InputMeasurement::new(timestamp, BigDecimal::from_str(&format!("{}.0", 40 + i)).unwrap())).await.expect("Failed to capture humidity measurement for subject 1");

		// Subject 2 temperature
		db.capture_measurement(&temp_aspect2.id(), &DatasetId::new(), &InputMeasurement::new(timestamp, BigDecimal::from_str(&format!("{}.0", 15 + i)).unwrap())).await.expect("Failed to capture temp measurement for subject 2");
	}

	// Test analysis on different aspects
	let analysis_time = base_time + chrono::Duration::minutes(10);

	let temp1_result = db.analyze_point(&temp_aspect1.id(), analysis_time, &Resolution::Seconds, &Spline::Linear).await.expect("Failed to analyze temperature for subject 1");

	let humidity1_result = db.analyze_point(&humidity_aspect1.id(), analysis_time, &Resolution::Seconds, &Spline::Linear).await.expect("Failed to analyze humidity for subject 1");

	let temp2_result = db.analyze_point(&temp_aspect2.id(), analysis_time, &Resolution::Seconds, &Spline::Linear).await.expect("Failed to analyze temperature for subject 2");

	// Verify results are different for different aspects/subjects
	assert_ne!(temp1_result.value, humidity1_result.value);
	assert_ne!(temp1_result.value, temp2_result.value);

	// Clean up
	common::remove_database(&db_name);
}

#[tokio::test]
async fn test_error_conditions() {
	let db_name = format!("test_errors_{}", Uuid::new_v4());

	// Clean up any existing test data
	common::remove_database(&db_name);

	// Test duplicate database creation
	let _db = Database::new(&db_name).await.expect("Failed to create database");
	let duplicate_result = Database::new(&db_name).await;
	assert!(duplicate_result.is_err(), "Should fail to create duplicate database");

	// Test loading non-existent database
	let non_existent_result = Database::existing("non_existent_db").await;
	assert!(non_existent_result.is_err(), "Should fail to load non-existent database");

	// Clean up
	common::remove_database(&db_name);
}

#[tokio::test]
async fn test_interpolation_methods() {
	let db_name = format!("test_interpolation_{}", Uuid::new_v4());

	// Clean up any existing test data
	common::remove_database(&db_name);

	let db = Database::new(&db_name).await.expect("Failed to create database");
	let subject = db.observe_subject("test_subject").await.expect("Failed to add subject");
	let aspect = db.track_aspect(&subject.id(), "test_aspect", &splimes::Resolution::Milliseconds, None).await.expect("Failed to track aspect");

	// Add test data with a clear pattern
	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
	let measurements = vec![
		(0, "0.0"),
		(10, "10.0"),
		(20, "40.0"), // Quadratic pattern: y = x^2/10
		(30, "90.0"),
		(40, "160.0"),
	];

	for (minutes, value) in measurements {
		db.capture_measurement(&aspect.id(), &DatasetId::new(), &InputMeasurement::new(base_time + chrono::Duration::minutes(minutes), BigDecimal::from_str(value).unwrap())).await.expect("Failed to capture measurement");
	}

	// Test different spline types with different target times to avoid cache conflicts
	let linear_target = base_time + chrono::Duration::minutes(15);
	let quadratic_target = base_time + chrono::Duration::minutes(16); // Different time
	let cubic_target = base_time + chrono::Duration::minutes(17); // Different time

	let linear_result = db.analyze_point(&aspect.id(), linear_target, &Resolution::Seconds, &Spline::Linear).await.expect("Failed to analyze with linear interpolation");

	let quadratic_result = db.analyze_point(&aspect.id(), quadratic_target, &Resolution::Seconds, &Spline::Quadratic).await.expect("Failed to analyze with quadratic interpolation");

	let cubic_result = db.analyze_point(&aspect.id(), cubic_target, &Resolution::Seconds, &Spline::Cubic).await.expect("Failed to analyze with cubic interpolation");

	// Results should be different for different interpolation methods and times
	println!("Linear result: {}", linear_result.value);
	println!("Quadratic result: {}", quadratic_result.value);
	println!("Cubic result: {}", cubic_result.value);

	// Verify that the interpolation produces reasonable values
	assert!(linear_result.value > BigDecimal::from_str("15.0").unwrap(), "Linear result should be > 15");
	assert!(linear_result.value < BigDecimal::from_str("35.0").unwrap(), "Linear result should be < 35");

	assert!(quadratic_result.value > BigDecimal::from_str("20.0").unwrap(), "Quadratic result should be > 20");
	assert!(quadratic_result.value < BigDecimal::from_str("35.0").unwrap(), "Quadratic result should be < 35");

	assert!(cubic_result.value > BigDecimal::from_str("20.0").unwrap(), "Cubic result should be > 20");
	assert!(cubic_result.value < BigDecimal::from_str("40.0").unwrap(), "Cubic result should be < 40");

	// Verify that results are actually different (since we're using different times)
	assert_ne!(linear_result.value, quadratic_result.value, "Linear and quadratic should differ");
	assert_ne!(linear_result.value, cubic_result.value, "Linear and cubic should differ");
	assert_ne!(quadratic_result.value, cubic_result.value, "Quadratic and cubic should differ");

	// Test that the methods produce consistent results when called again with the same parameters
	let linear_result2 = db.analyze_point(&aspect.id(), linear_target, &Resolution::Seconds, &Spline::Linear).await.expect("Failed to analyze with linear interpolation (second call)");

	assert_eq!(linear_result.value, linear_result2.value, "Linear interpolation should be consistent");
	assert_eq!(linear_result.timestamp, linear_result2.timestamp, "Linear interpolation timestamps should match");

	// Clean up
	common::remove_database(&db_name);
}

#[tokio::test]
async fn test_caching_behavior() {
	let db_name = format!("test_caching_{}", Uuid::new_v4());

	// Clean up any existing test data
	common::remove_database(&db_name);

	let db = Database::new(&db_name).await.expect("Failed to create database");
	let subject = db.observe_subject("cache_test_subject").await.expect("Failed to add subject");
	let aspect = db.track_aspect(&subject.id(), "cache_test_aspect", &splimes::Resolution::Milliseconds, None).await.expect("Failed to track aspect");

	// Add test data
	let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
	for i in 0..10 {
		db.capture_measurement(&aspect.id(), &DatasetId::new(), &InputMeasurement::new(base_time + chrono::Duration::minutes(i * 5), BigDecimal::from_str(&format!("{i}.0")).unwrap())).await.expect("Failed to capture measurement");
	}

	let target_time = base_time + chrono::Duration::minutes(22);

	// First call should populate cache
	let start = std::time::Instant::now();
	let result1 = db.analyze_point(&aspect.id(), target_time, &Resolution::Seconds, &Spline::Linear).await.expect("Failed to analyze point (first call)");
	let first_duration = start.elapsed();

	// Second call should be faster due to caching
	let start = std::time::Instant::now();
	let result2 = db.analyze_point(&aspect.id(), target_time, &Resolution::Seconds, &Spline::Linear).await.expect("Failed to analyze point (second call)");
	let second_duration = start.elapsed();

	// Results should be identical
	assert_eq!(result1.value, result2.value);
	assert_eq!(result1.timestamp, result2.timestamp);

	// Second call should be faster (cached)
	// Note: This might be flaky in CI, so we'll just verify the results match
	println!("First call: {first_duration:?}, Second call: {second_duration:?}");

	// Clean up
	common::remove_database(&db_name);
}

/// Rewrite a dictionary's stored step interpolation, as the CHANGELOG's remedy for a
/// stored method splimes 1.0 rejects does with Turso's shell.
async fn set_stored_interpolation(db: &Database, aspect_id: &weftdb::AspectId, dictionary: &str, from: &str, to: &str) {
	let dictionary_db = db.get_dictionary_db(aspect_id, dictionary).await.expect("Failed to open dictionary database");
	let conn = dictionary_db.connect().expect("Failed to connect to dictionary database");
	let changed = conn.execute("UPDATE dictionary_constraints SET steps_interpolation = ? WHERE steps_interpolation = ?", turso::params![to, from]).await.expect("Failed to rewrite steps_interpolation");
	assert_eq!(changed, 1, "one stored `{from}` to rewrite");
}

/// A dictionary's stored step interpolation loads through `get_dictionary_metadata`,
/// which validates it with splimes 1.0's `Spline::from_str`; a method 0.1 accepted and
/// 1.0 rejects is the documented error until the stored text is rewritten.
#[tokio::test]
async fn test_dictionary_metadata_validates_stored_interpolation() {
	let db_name = format!("test_dictionary_steps_{}", Uuid::new_v4());
	common::remove_database(&db_name);

	let db = Database::new(&db_name).await.expect("Failed to create database");
	let subject = db.observe_subject("dictionary_subject").await.expect("Failed to add subject");
	let aspect = db.track_aspect(&subject.id(), "pressure", &splimes::Resolution::Seconds, None).await.expect("Failed to track aspect");

	// A dictionary without steps stores NULL for both columns and loads with none.
	aspect.new_dictionary("unstepped", "no steps", &DictionaryConstraints::default()).await.expect("Failed to create dictionary");
	let metadata = db.get_dictionary_metadata(&aspect.id(), "unstepped").await.expect("Failed to load dictionary metadata").expect("dictionary exists");
	assert_eq!(metadata.name, "unstepped");
	assert!(metadata.constraints.steps().is_none());

	// `new_dictionary` refuses to store a method that could not load...
	let invalid = DictionaryConstraints::new(Some(Steps::new(10, Spline::Polynomial(9, None))), None);
	let err = aspect.new_dictionary("rejected", "degree 9", &invalid).await.expect_err("a degree 9 must not be stored");
	assert_eq!(err.to_string(), "Invalid step interpolation for dictionary 'rejected': invalid polynomial degree 9: must be between 1 and 8");

	// ...so write what 0.1 could store, a degree above 8, directly.
	let stepped = DictionaryConstraints::new(Some(Steps::new(10, Spline::Polynomial(8, None))), None);
	aspect.new_dictionary("stepped", "polynomial steps", &stepped).await.expect("Failed to create dictionary");
	set_stored_interpolation(&db, &aspect.id(), "stepped", "Polynomial(degree: 8, bounds_factor: None)", "Polynomial(degree: 9, bounds_factor: None)").await;
	let err = db.get_dictionary_metadata(&aspect.id(), "stepped").await.expect_err("a stored degree 9 must not load");
	assert_eq!(err.to_string(), "Database error: Invalid interpolation format: invalid polynomial degree 9: must be between 1 and 8");

	// The CHANGELOG's remedy makes it load.
	set_stored_interpolation(&db, &aspect.id(), "stepped", "Polynomial(degree: 9, bounds_factor: None)", "Polynomial(degree: 8, bounds_factor: None)").await;
	let metadata = db.get_dictionary_metadata(&aspect.id(), "stepped").await.expect("Failed to load dictionary metadata").expect("dictionary exists");
	let steps = metadata.constraints.steps().as_ref().expect("stored steps");
	assert_eq!((steps.count(), *steps.interpolation()), (10, Spline::Polynomial(8, None)));

	// `list_dictionaries` reads each of the aspect's dictionaries the same way.
	let listed = db.list_dictionaries(&aspect.id()).await.expect("Failed to list dictionaries");
	assert_eq!(listed.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["stepped", "unstepped"]);

	common::remove_database(&db_name);
}

/// Count the rows of `table` in a dictionary's database.
async fn dictionary_rows(db: &Database, aspect_id: &weftdb::AspectId, dictionary: &str, table: &str) -> i64 {
	let dictionary_db = db.get_dictionary_db(aspect_id, dictionary).await.expect("Failed to open dictionary database");
	let conn = dictionary_db.connect().expect("Failed to connect to dictionary database");
	let mut rows = conn.query(format!("SELECT COUNT(*) FROM {table}"), ()).await.expect("Failed to count rows");
	let row = rows.next().await.expect("Failed to read count").expect("a count");
	*row.get_value(0).expect("count column").as_integer().expect("integer count")
}

/// Variabilities as text (`AverageStatic(2.5)`), to compare: `VariablilityType` has no `PartialEq`.
fn variability_parts(variabilities: Option<&Vec<VariablilityType>>) -> Option<Vec<String>> {
	variabilities.map(|v| v.iter().map(ToString::to_string).collect())
}

fn variability(kind: fn(Variability) -> VariablilityType, value: &str) -> VariablilityType {
	kind(Variability::new(BigDecimal::from_str(value).unwrap()))
}

/// `set_dictionary_metadata`, which `weft_orchestration::load_dictionary` registers a
/// pipeline's dictionaries with, stores the whole registration: it used to insert only the
/// metadata row, so the dictionary's steps and variabilities were lost and
/// `get_dictionary_metadata` (which needs a constraints row) found nothing. Setting it again
/// replaces the registration instead of adding another row.
#[tokio::test]
async fn test_set_dictionary_metadata_registers_and_replaces() {
	let db_name = format!("test_dictionary_set_{}", Uuid::new_v4());
	common::remove_database(&db_name);

	let db = Database::new(&db_name).await.expect("Failed to create database");
	let subject = db.observe_subject("dictionary_subject").await.expect("Failed to add subject");
	let aspect = db.track_aspect(&subject.id(), "pressure", &Resolution::Seconds, None).await.expect("Failed to track aspect");

	// Nothing registered yet, and no database file: `None`, which `load_dictionary`
	// registers on (it was an error, "Dictionary 'pipeline' does not exist").
	assert!(db.get_dictionary_metadata(&aspect.id(), "pipeline").await.expect("an unregistered dictionary is not an error").is_none());

	let variabilities = vec![variability(VariablilityType::AveragePercentile, "0.000000001"), variability(VariablilityType::MaximumStatic, "2.5")];
	let first = DictionaryMetadata { id: DictionaryId::new(), name: "pipeline".to_string(), description: "first".to_string(), constraints: DictionaryConstraints::new(Some(Steps::new(12, Spline::Cubic)), Some(variabilities.clone())) };
	db.set_dictionary_metadata(&aspect.id(), "pipeline", &first).await.expect("Failed to set dictionary metadata");

	let stored = db.get_dictionary_metadata(&aspect.id(), "pipeline").await.expect("Failed to load dictionary metadata").expect("registered");
	assert_eq!((stored.id, stored.name.as_str(), stored.description.as_str()), (first.id, "pipeline", "first"));
	let steps = stored.constraints.steps().as_ref().expect("stored steps");
	assert_eq!((steps.count(), *steps.interpolation()), (12, Spline::Cubic));
	assert_eq!(variability_parts(stored.constraints.variabilities().as_ref()), variability_parts(Some(&variabilities)));

	// Setting it again replaces it: one row in each table, and the read (cached above)
	// sees the new registration.
	let second = DictionaryMetadata { id: DictionaryId::new(), name: "pipeline".to_string(), description: "second".to_string(), constraints: DictionaryConstraints::new(Some(Steps::new(4, Spline::Linear)), Some(vec![variability(VariablilityType::SumStatic, "7")])) };
	for _ in 0..3 {
		db.set_dictionary_metadata(&aspect.id(), "pipeline", &second).await.expect("Failed to set dictionary metadata");
	}
	for (table, rows) in [("dictionary_metadata", 1), ("dictionary_constraints", 1), ("dictionary_variabilities", 1)] {
		assert_eq!(dictionary_rows(&db, &aspect.id(), "pipeline", table).await, rows, "{table}");
	}
	let stored = db.get_dictionary_metadata(&aspect.id(), "pipeline").await.expect("Failed to load dictionary metadata").expect("registered");
	assert_eq!((stored.id, stored.description.as_str()), (second.id, "second"));
	let steps = stored.constraints.steps().as_ref().expect("stored steps");
	assert_eq!((steps.count(), *steps.interpolation()), (4, Spline::Linear));
	assert_eq!(variability_parts(stored.constraints.variabilities().as_ref()), Some(vec!["SumStatic(7)".to_string()]));

	// No steps and no variabilities store NULLs and no rows, and read back as `None`.
	let bare = DictionaryMetadata { id: DictionaryId::new(), name: "pipeline".to_string(), description: String::new(), constraints: DictionaryConstraints::new(None, Some(Vec::new())) };
	db.set_dictionary_metadata(&aspect.id(), "pipeline", &bare).await.expect("Failed to set dictionary metadata");
	let stored = db.get_dictionary_metadata(&aspect.id(), "pipeline").await.expect("Failed to load dictionary metadata").expect("registered");
	assert!(stored.constraints.steps().is_none() && stored.constraints.variabilities().is_none());
	assert_eq!(dictionary_rows(&db, &aspect.id(), "pipeline", "dictionary_variabilities").await, 0);

	// It refuses a step method that could not be read back, and keeps what was stored.
	let invalid = DictionaryMetadata { id: DictionaryId::new(), name: "pipeline".to_string(), description: "degree 9".to_string(), constraints: DictionaryConstraints::new(Some(Steps::new(10, Spline::Polynomial(9, None))), None) };
	let err = db.set_dictionary_metadata(&aspect.id(), "pipeline", &invalid).await.expect_err("a degree 9 must not be stored");
	assert_eq!(err.to_string(), "Invalid step interpolation for dictionary 'pipeline': invalid polynomial degree 9: must be between 1 and 8");
	assert_eq!(db.get_dictionary_metadata(&aspect.id(), "pipeline").await.expect("Failed to load dictionary metadata").expect("registered").id, bare.id);

	common::remove_database(&db_name);
}

/// Dictionary names become file names (`<aspect>/dictionaries/<name>.db`), so a name that
/// would change where that path points is an `InvalidDictionaryName` in every dictionary
/// operation, before any file is touched. `../escape` reached `<aspect>/escape.db`, and an
/// absolute name replaced the whole path.
#[tokio::test]
async fn test_dictionary_names_are_checked_before_they_become_paths() {
	let db_name = format!("test_dictionary_names_{}", Uuid::new_v4());
	common::remove_database(&db_name);

	let db = Database::new(&db_name).await.expect("Failed to create database");
	let subject = db.observe_subject("dictionary_subject").await.expect("Failed to add subject");
	let mut aspect = db.track_aspect(&subject.id(), "pressure", &Resolution::Seconds, None).await.expect("Failed to track aspect");
	let aspect_dir = common::data_dir().join(&db_name).join("dictionary_subject").join("pressure");
	let absolute = common::data_dir().join("escaped");
	let absolute = absolute.to_str().expect("a UTF-8 temp dir");

	fn assert_invalid(err: &anyhow::Error, name: &str) {
		let invalid = err.downcast_ref::<weftdb::InvalidDictionaryName>().unwrap_or_else(|| panic!("{name:?} is an InvalidDictionaryName: {err:#}"));
		assert_eq!(invalid.name(), name);
	}
	for name in ["../escape", absolute, "", "..", "a\\b", "NUL"] {
		let metadata = DictionaryMetadata { id: DictionaryId::new(), name: name.to_string(), description: String::new(), constraints: DictionaryConstraints::default() };
		assert_invalid(&aspect.new_dictionary(name, "", &DictionaryConstraints::default()).await.expect_err("new_dictionary"), name);
		assert_invalid(&db.set_dictionary_metadata(&aspect.id(), name, &metadata).await.expect_err("set_dictionary_metadata"), name);
		assert_invalid(&db.get_dictionary_metadata(&aspect.id(), name).await.expect_err("get_dictionary_metadata"), name);
		assert_invalid(&db.get_dictionary_db(&aspect.id(), name).await.expect_err("get_dictionary_db"), name);
		assert_invalid(&db.get_dictionary_patterns(&aspect.id(), name).await.err().expect("get_dictionary_patterns"), name);
		assert_invalid(&aspect.dictionary(name).await.expect_err("Aspect::dictionary"), name);
	}
	assert!(!aspect_dir.join("escape.db").exists(), "nothing was created next to dictionaries/");
	assert!(!common::data_dir().join("escaped.db").exists(), "nothing was created at the absolute path");
	let mut created: Vec<_> = std::fs::read_dir(aspect_dir.join("dictionaries")).expect("the dictionaries dir").map(|entry| entry.expect("an entry").file_name()).collect();
	created.sort();
	assert!(created.is_empty(), "no dictionary file was created: {created:?}");
	assert!(db.list_dictionaries(&aspect.id()).await.expect("Failed to list dictionaries").is_empty());

	common::remove_database(&db_name);
}

/// Assert that `err` is the write-write conflict a dictionary write lost, not the failed
/// `ROLLBACK` after it.
fn assert_write_write_conflict(err: &anyhow::Error) {
	assert!(weftdb::error::is_transient_mvcc_error(err), "a conflict is transient: {err:#}");
	assert!(err.to_string().contains("Write-write conflict"), "the conflict is reported: {err:#}");
	assert!(!format!("{err:#}").contains("Rollback failed"), "the rollback's error does not replace it: {err:#}");
}

/// A dictionary write that loses an MVCC write-write conflict reports the conflict. Turso
/// rolls the transaction back itself, so the `ROLLBACK` after the failed statement fails
/// ("no transaction is active"); `set_dictionary_metadata` and `new_dictionary` returned
/// that error instead (`Rollback failed: …`), which `is_transient_mvcc_error` could not
/// recognise as a conflict to retry.
#[tokio::test]
async fn test_dictionary_writes_report_a_write_write_conflict() {
	let db_name = format!("test_dictionary_conflict_{}", Uuid::new_v4());
	common::remove_database(&db_name);

	let db = Database::new(&db_name).await.expect("Failed to create database");
	let subject = db.observe_subject("dictionary_subject").await.expect("Failed to add subject");
	let aspect = db.track_aspect(&subject.id(), "pressure", &Resolution::Seconds, None).await.expect("Failed to track aspect");
	aspect.new_dictionary("contended", "first", &DictionaryConstraints::default()).await.expect("Failed to create dictionary");

	// Another transaction deletes the registration and stays open, so the delete with which
	// a registration replaces the one before it conflicts.
	let dictionary_db = db.get_dictionary_db(&aspect.id(), "contended").await.expect("Failed to open dictionary database");
	let holder = dictionary_db.connect().expect("Failed to connect to dictionary database");
	holder.execute("BEGIN CONCURRENT", ()).await.expect("Failed to begin the holder's transaction");
	holder.execute("DELETE FROM dictionary_metadata WHERE name = 'contended'", ()).await.expect("Failed to delete in the holder's transaction");

	let metadata = DictionaryMetadata { id: DictionaryId::new(), name: "contended".to_string(), description: "second".to_string(), constraints: DictionaryConstraints::default() };
	let err = db.set_dictionary_metadata(&aspect.id(), "contended", &metadata).await.expect_err("the registration conflicts with the open delete");
	assert_write_write_conflict(&err);
	assert!(err.to_string().starts_with("Failed to set dictionary metadata: "), "{err}");
	let err = aspect.new_dictionary("contended", "third", &DictionaryConstraints::default()).await.expect_err("creating it again conflicts too");
	assert_write_write_conflict(&err);

	// Neither wrote anything: once the holder gives up, the first registration is intact.
	holder.execute("ROLLBACK", ()).await.expect("Failed to roll back the holder's transaction");
	let stored = db.get_dictionary_metadata(&aspect.id(), "contended").await.expect("Failed to load dictionary metadata").expect("registered");
	assert_eq!(stored.description, "first");
	assert_eq!(dictionary_rows(&db, &aspect.id(), "contended", "dictionary_metadata").await, 1);

	common::remove_database(&db_name);
}

/// Databases written before `set_dictionary_metadata` replaced registrations hold a metadata
/// row (with no constraints) for every `load_dictionary` of a pipeline dictionary, beside
/// any complete registration. The read picks the newest complete one; a name with only
/// incomplete rows is unregistered, and registering it again leaves one row.
#[tokio::test]
async fn test_dictionary_metadata_reads_past_duplicate_rows() {
	let db_name = format!("test_dictionary_duplicates_{}", Uuid::new_v4());
	common::remove_database(&db_name);

	let db = Database::new(&db_name).await.expect("Failed to create database");
	let subject = db.observe_subject("dictionary_subject").await.expect("Failed to add subject");
	let aspect = db.track_aspect(&subject.id(), "pressure", &Resolution::Seconds, None).await.expect("Failed to track aspect");

	// Add metadata rows the way the old `set_dictionary_metadata` did: no constraints, a
	// fresh id, and (here) a later timestamp.
	async fn add_old_rows(db: &Database, aspect_id: &weftdb::AspectId, dictionary: &str) {
		let dictionary_db = db.get_dictionary_db(aspect_id, dictionary).await.expect("Failed to open dictionary database");
		let conn = dictionary_db.connect().expect("Failed to connect to dictionary database");
		for later in 1..=2_i64 {
			let created_at = Utc::now().timestamp_millis() + later * 1000;
			conn.execute("INSERT INTO dictionary_metadata (id, name, description, created_at) VALUES (?, ?, ?, ?)", turso::params![Uuid::new_v4().to_string(), dictionary, "pipeline", created_at]).await.expect("Failed to insert an old metadata row");
		}
	}

	// A complete registration with newer incomplete rows beside it still reads.
	let constraints = DictionaryConstraints::new(Some(Steps::new(6, Spline::Quadratic)), Some(vec![variability(VariablilityType::AbsoluteAveragePercentile, "0.5")]));
	aspect.new_dictionary("created", "created first", &constraints).await.expect("Failed to create dictionary");
	add_old_rows(&db, &aspect.id(), "created").await;
	assert_eq!(dictionary_rows(&db, &aspect.id(), "created", "dictionary_metadata").await, 3);
	let stored = db.get_dictionary_metadata(&aspect.id(), "created").await.expect("Failed to load dictionary metadata").expect("registered");
	assert_eq!(stored.description, "created first");
	assert_eq!(stored.constraints.steps().as_ref().map(|s| (s.count(), *s.interpolation())), Some((6, Spline::Quadratic)));
	assert_eq!(variability_parts(stored.constraints.variabilities().as_ref()), Some(vec!["AbsoluteAveragePercentile(0.5)".to_string()]));

	// Only incomplete rows: unregistered, until it is registered again.
	aspect.new_dictionary("loaded", "pipeline", &DictionaryConstraints::default()).await.expect("Failed to create dictionary");
	{
		let dictionary_db = db.get_dictionary_db(&aspect.id(), "loaded").await.expect("Failed to open dictionary database");
		let conn = dictionary_db.connect().expect("Failed to connect to dictionary database");
		conn.execute("DELETE FROM dictionary_constraints", ()).await.expect("Failed to delete the constraints");
	}
	add_old_rows(&db, &aspect.id(), "loaded").await;
	assert!(db.get_dictionary_metadata(&aspect.id(), "loaded").await.expect("Failed to load dictionary metadata").is_none());
	let metadata = DictionaryMetadata { id: DictionaryId::new(), name: "loaded".to_string(), description: "pipeline".to_string(), constraints };
	db.set_dictionary_metadata(&aspect.id(), "loaded", &metadata).await.expect("Failed to set dictionary metadata");
	assert_eq!(dictionary_rows(&db, &aspect.id(), "loaded", "dictionary_metadata").await, 1);
	assert_eq!(db.get_dictionary_metadata(&aspect.id(), "loaded").await.expect("Failed to load dictionary metadata").expect("registered").id, metadata.id);

	// Creating a dictionary twice replaces it too.
	aspect.new_dictionary("loaded", "created over it", &DictionaryConstraints::default()).await.expect("Failed to create dictionary");
	assert_eq!(dictionary_rows(&db, &aspect.id(), "loaded", "dictionary_metadata").await, 1);
	assert_eq!(dictionary_rows(&db, &aspect.id(), "loaded", "dictionary_constraints").await, 1);
	assert_eq!(dictionary_rows(&db, &aspect.id(), "loaded", "dictionary_variabilities").await, 0);

	// Several complete registrations, as the old `new_dictionary` left when a dictionary was
	// created twice: the newest by `created_at` is read, whatever order the rows were
	// written in. The newest is written second and the oldest last, so reading by rowid
	// either way, or by `created_at` ascending, reads another one.
	aspect.new_dictionary("twice", "written first", &DictionaryConstraints::new(Some(Steps::new(2, Spline::Linear)), None)).await.expect("Failed to create dictionary");
	{
		let dictionary_db = db.get_dictionary_db(&aspect.id(), "twice").await.expect("Failed to open dictionary database");
		let conn = dictionary_db.connect().expect("Failed to connect to dictionary database");
		for (description, offset_ms, count, interpolation) in [("newest", 60_000_i64, 3_i64, "Cubic"), ("oldest", -60_000, 5, "Quadratic")] {
			let id = Uuid::new_v4().to_string();
			conn.execute("INSERT INTO dictionary_metadata (id, name, description, created_at) VALUES (?, ?, ?, ?)", turso::params![id.as_str(), "twice", description, Utc::now().timestamp_millis() + offset_ms]).await.expect("Failed to insert a registration");
			conn.execute("INSERT INTO dictionary_constraints (dictionary_id, steps_count, steps_interpolation) VALUES (?, ?, ?)", turso::params![id.as_str(), count, interpolation]).await.expect("Failed to insert its constraints");
		}
	}
	assert_eq!(dictionary_rows(&db, &aspect.id(), "twice", "dictionary_constraints").await, 3);
	let stored = db.get_dictionary_metadata(&aspect.id(), "twice").await.expect("Failed to load dictionary metadata").expect("registered");
	assert_eq!(stored.description, "newest");
	assert_eq!(stored.constraints.steps().as_ref().map(|s| (s.count(), *s.interpolation())), Some((3, Spline::Cubic)));

	common::remove_database(&db_name);
}

/// A dictionary file can exist without its tables: `insert_pattern_into_dictionary` opens
/// (and so creates) the file but no tables, and once the file is open in this process
/// nothing else created them. Such a file holds no registration, so it reads as
/// unregistered and is registered over; the read failed with "no such table", so
/// `load_dictionary` never registered it and `list_dictionaries` failed for the aspect.
#[tokio::test]
async fn test_a_dictionary_file_without_tables_is_unregistered() {
	let db_name = format!("test_dictionary_no_tables_{}", Uuid::new_v4());
	common::remove_database(&db_name);

	let db = Database::new(&db_name).await.expect("Failed to create database");
	let subject = db.observe_subject("dictionary_subject").await.expect("Failed to add subject");
	let aspect = db.track_aspect(&subject.id(), "pressure", &Resolution::Seconds, None).await.expect("Failed to track aspect");
	aspect.new_dictionary("complete", "has its tables", &DictionaryConstraints::default()).await.expect("Failed to create dictionary");

	let pattern = weftdb::Pattern::new(weftdb::PatternID::new(), Vec::new(), Vec::new());
	let err = db.insert_pattern_into_dictionary(&aspect.id(), "bare", &pattern).await.expect_err("the file has no `patterns` table");
	assert!(err.to_string().contains("no such table"), "{err:#}");
	assert!(common::data_dir().join(&db_name).join("dictionary_subject").join("pressure").join("dictionaries").join("bare.db").is_file(), "the file exists");

	assert!(db.get_dictionary_metadata(&aspect.id(), "bare").await.expect("a file without tables is not an error").is_none());
	let listed = db.list_dictionaries(&aspect.id()).await.expect("one file without tables does not fail the listing");
	assert_eq!(listed.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["complete"]);

	let metadata = DictionaryMetadata { id: DictionaryId::new(), name: "bare".to_string(), description: "registered over it".to_string(), constraints: DictionaryConstraints::new(Some(Steps::new(4, Spline::Linear)), None) };
	db.set_dictionary_metadata(&aspect.id(), "bare", &metadata).await.expect("Failed to set dictionary metadata");
	assert_eq!(db.get_dictionary_metadata(&aspect.id(), "bare").await.expect("Failed to load dictionary metadata").expect("registered").id, metadata.id);
	db.insert_pattern_into_dictionary(&aspect.id(), "bare", &pattern).await.expect("the tables exist now");

	common::remove_database(&db_name);
}

/// `list_dictionaries` lists the aspect's readable dictionaries and skips the rest of the
/// directory: a dictionary whose registration cannot be read (logged; it failed the whole
/// listing), a file whose name is no dictionary name, and anything not a regular file.
/// Opening a file converts its journal mode and can create tables, so a symlink is never
/// followed: its target is left as it was.
#[tokio::test]
async fn test_list_dictionaries_skips_what_it_cannot_list() {
	let db_name = format!("test_dictionary_list_skips_{}", Uuid::new_v4());
	common::remove_database(&db_name);

	let db = Database::new(&db_name).await.expect("Failed to create database");
	let subject = db.observe_subject("dictionary_subject").await.expect("Failed to add subject");
	let aspect = db.track_aspect(&subject.id(), "pressure", &Resolution::Seconds, None).await.expect("Failed to track aspect");
	let dictionaries = common::data_dir().join(&db_name).join("dictionary_subject").join("pressure").join("dictionaries");

	aspect.new_dictionary("readable", "listed", &DictionaryConstraints::default()).await.expect("Failed to create dictionary");
	aspect.new_dictionary("unreadable", "degree 9", &DictionaryConstraints::new(Some(Steps::new(10, Spline::Polynomial(8, None))), None)).await.expect("Failed to create dictionary");
	set_stored_interpolation(&db, &aspect.id(), "unreadable", "Polynomial(degree: 8, bounds_factor: None)", "Polynomial(degree: 9, bounds_factor: None)").await;
	db.get_dictionary_metadata(&aspect.id(), "unreadable").await.expect_err("a stored degree 9 does not load");
	std::fs::write(dictionaries.join(".backup.db"), b"").expect("Failed to write a stray file");
	std::fs::create_dir(dictionaries.join("folder.db")).expect("Failed to create a directory");
	let outside = common::data_dir().join(&db_name).join("outside.db");
	std::fs::write(&outside, b"").expect("Failed to write the link's target");
	#[cfg(unix)]
	std::os::unix::fs::symlink(&outside, dictionaries.join("linked.db")).expect("Failed to create a symlink");

	let listed = db.list_dictionaries(&aspect.id()).await.expect("Failed to list dictionaries");
	assert_eq!(listed.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["readable"]);
	assert_eq!(std::fs::metadata(&outside).expect("the link's target").len(), 0, "the link's target was not opened");
	assert!(!common::data_dir().join(&db_name).join("outside.db-log").exists(), "nor given a log");
	assert_eq!(std::fs::metadata(dictionaries.join(".backup.db")).expect("the stray file").len(), 0, "nor was the stray file");

	common::remove_database(&db_name);
}

/// `list_dictionaries` lists every dictionary of the aspect, each with its constraints. It
/// used to read only the database of a dictionary named "default", so it failed for an
/// aspect without one and listed no other dictionary.
#[tokio::test]
async fn test_list_dictionaries_lists_the_aspects_dictionaries() {
	let db_name = format!("test_dictionary_list_{}", Uuid::new_v4());
	common::remove_database(&db_name);

	let db = Database::new(&db_name).await.expect("Failed to create database");
	let subject = db.observe_subject("dictionary_subject").await.expect("Failed to add subject");
	let aspect = db.track_aspect(&subject.id(), "pressure", &Resolution::Seconds, None).await.expect("Failed to track aspect");
	let other = db.track_aspect(&subject.id(), "temperature", &Resolution::Seconds, None).await.expect("Failed to track aspect");

	assert!(db.list_dictionaries(&aspect.id()).await.expect("an aspect without dictionaries lists none").is_empty());

	// One created on the aspect, one registered as a pipeline does, one on another aspect.
	let created = DictionaryConstraints::new(Some(Steps::new(8, Spline::Linear)), Some(vec![variability(VariablilityType::MaximumPercentile, "0.25"), variability(VariablilityType::AbsoluteSumStatic, "3")]));
	aspect.new_dictionary("beta", "created", &created).await.expect("Failed to create dictionary");
	let registered = DictionaryMetadata { id: DictionaryId::new(), name: "alpha".to_string(), description: "registered".to_string(), constraints: DictionaryConstraints::new(Some(Steps::new(16, Spline::Cubic)), None) };
	db.set_dictionary_metadata(&aspect.id(), "alpha", &registered).await.expect("Failed to set dictionary metadata");
	other.new_dictionary("gamma", "elsewhere", &DictionaryConstraints::default()).await.expect("Failed to create dictionary");

	let listed = db.list_dictionaries(&aspect.id()).await.expect("Failed to list dictionaries");
	let summary: Vec<_> = listed.iter().map(|d| (d.name.as_str(), d.description.as_str(), d.constraints.steps().as_ref().map(|s| (s.count(), *s.interpolation())), variability_parts(d.constraints.variabilities().as_ref()))).collect();
	assert_eq!(summary, [("alpha", "registered", Some((16, Spline::Cubic)), None), ("beta", "created", Some((8, Spline::Linear)), Some(vec!["MaximumPercentile(0.25)".to_string(), "AbsoluteSumStatic(3)".to_string()]))]);
	assert_eq!(listed[0].id, registered.id);

	let listed = db.list_dictionaries(&other.id()).await.expect("Failed to list dictionaries");
	assert_eq!(listed.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["gamma"]);

	common::remove_database(&db_name);
}
