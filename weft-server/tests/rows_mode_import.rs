//! The operator's migration route, end to end: a 0.1.x data directory in, a Storage v2
//! store out, on the tree a real 0.1.x release wrote.
//!
//! FRE-14c added `weft-server import-rows-mode` because the readable leg of the migration
//! existed only as a library call: an operator holding a 0.1.x directory had a route they
//! could not run. These tests run that route as the command runs it — through
//! [`import_rows_mode::import`] with the same [`Request`] the parser builds — and then read
//! the result the way a server reads it, under the scope the tree names.
//!
//! The tree is `compat/fixtures/v0.1.0/legacy/v010_legacy`, written by the v0.1.0 binary
//! itself and checked in byte for byte; `weftdb`'s own `compat_v010` suite proves what it
//! contains. Copying it per test is what an operator does too: the migration never writes
//! to the tree it reads.

use std::{
	fs, num::NonZeroUsize, path::{Path, PathBuf}
};

use weft_server::{
	import_rows_mode::{self, Request}, store_open
};

/// The repo root, which is where `compat/fixtures` lives.
fn repo_root() -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("weft-server sits in the workspace root").to_path_buf()
}

/// The tree a 0.1.x release wrote, with its `metadata.db` and `plant/<aspect>/measurements.db`.
fn fixture_tree() -> PathBuf {
	repo_root().join("compat/fixtures/v0.1.0/legacy/v010_legacy")
}

/// A private copy of the tree, as an operator's migrated directory is private to the run.
fn copy_tree(from: &Path, to: &Path) {
	fs::create_dir_all(to).expect("the copy's root is created");
	for entry in fs::read_dir(from).expect("the fixture tree is readable") {
		let entry = entry.expect("a readable fixture entry");
		let target = to.join(entry.file_name());
		if entry.file_type().expect("a fixture entry has a type").is_dir() {
			copy_tree(&entry.path(), &target);
		} else {
			fs::copy(entry.path(), target).expect("a fixture file is copied");
		}
	}
}

/// The command's request for one subject of `tree`, written into `store`.
fn request(tree: &Path, store: &Path, subject: &str) -> Request {
	Request { tree: tree.to_path_buf(), subject: subject.to_string(), database: None, store: Some(store.to_path_buf()), rows_per_segment: NonZeroUsize::new(100).expect("100 is not zero"), json: false }
}

#[tokio::test]
async fn a_0_1_x_tree_migrates_into_a_store_under_the_name_the_tree_gave_it() {
	let dir = tempfile::tempdir().expect("a scratch directory");
	let tree = dir.path().join("v010_legacy");
	copy_tree(&fixture_tree(), &tree);
	let store_root = dir.path().join("store");

	let record = import_rows_mode::import(&request(&tree, &store_root, "plant")).await.expect("the fixture's one subject migrates with no database named");
	assert_eq!(record.database, "v010_legacy", "with no --database the store takes the database the tree's top directory names, which is what 0.1.x called it");
	assert_eq!(record.subject, "plant");
	assert_eq!(record.source, tree.to_string_lossy(), "the record names the tree it read, in the words it was given");
	assert_eq!(record.aspects.len(), 3, "one entry per series the tree holds");
	assert_eq!(record.rows(), 371, "the grid's non-null rows: 237 price, 80 flow, 54 temp");

	// The point of the scope the import chose: a store opened the way a server opens one,
	// under this scope, answers for the migrated aspects.
	let store = store_open::open_scoped_with_env_options(&store_root, "v010_legacy", "plant").await.expect("the migrated store opens under the tree's scope");
	let (timestamps, values) = store.read_time_range("price", i64::MIN, i64::MAX).await.expect("the migrated aspect reads back");
	assert_eq!(timestamps.len(), 237, "every price row the tree held is in the store");
	assert!(values.iter().all(Option::is_some), "a migrated row carries a value; the import skipped the grid's null rows rather than sealing them");

	// What stays behind is named, not swallowed: the 0.1.x catalog is not a Storage v2
	// catalog, and the operator has to be told the queue and dictionaries did not move.
	assert!(record.not_carried.iter().any(|entry| entry.path == "metadata.db" && !entry.reason.is_empty()), "the catalog stays behind with a reason: {:?}", record.not_carried);
	let summary = import_rows_mode::summary(&record);
	assert!(summary.contains("stays behind: metadata.db"), "the operator's summary lists it too: {summary}");
	assert!(summary.contains("WEFT_STORE_DATABASE=v010_legacy WEFT_STORE_SUBJECT=plant"), "the summary ends by naming the two variables the server needs: {summary}");
}

#[tokio::test]
async fn a_migrated_store_is_not_readable_under_the_default_scope() {
	// The import writes under the tree's scope, so the migration is invisible to a server
	// that never sets WEFT_STORE_DATABASE. That is the behaviour the operator must be told
	// about — an import that appeared to do nothing would be worse than one that refused.
	let dir = tempfile::tempdir().expect("a scratch directory");
	let tree = dir.path().join("v010_legacy");
	copy_tree(&fixture_tree(), &tree);
	let store_root = dir.path().join("store");
	import_rows_mode::import(&request(&tree, &store_root, "plant")).await.expect("the subject migrates");

	// A root opens in one process only once, so the two scopes are read one after the other.
	let default = store_open::open_scoped_with_env_options(&store_root, "default", "default").await.expect("the same root opens under the default scope");
	let default_root = default.root().to_path_buf();
	assert!(weftdb::rows_mode::read_import_record(&default).await.expect("the default scope's index reads").is_none(), "the import record lives in the scope the tree named, not in the default one");
	drop(default);
	let scoped = store_open::open_scoped_with_env_options(&store_root, "v010_legacy", "plant").await.expect("the root reopens under the tree's scope");
	assert!(weftdb::rows_mode::read_import_record(&scoped).await.expect("that index reads").is_some(), "the store the tree's scope opens is the one holding the migration");
	assert_eq!(default_root, store_root, "both scopes are the same root: the difference is the namespace, not the directory");
}

#[tokio::test]
async fn a_tree_is_read_once_into_a_store() {
	// The readable leg cannot tell the tree's rows apart from rows the store sealed itself,
	// so a second run refuses rather than doubling. The command must surface that refusal
	// rather than reporting a successful import of nothing.
	let dir = tempfile::tempdir().expect("a scratch directory");
	let tree = dir.path().join("v010_legacy");
	copy_tree(&fixture_tree(), &tree);
	let store_root = dir.path().join("store");
	import_rows_mode::import(&request(&tree, &store_root, "plant")).await.expect("the first reading is accepted");

	let error = import_rows_mode::import(&request(&tree, &store_root, "plant")).await.expect_err("a second reading of the same tree would double the rows");
	assert!(matches!(error.downcast_ref::<weftdb::RowsModeError>(), Some(weftdb::RowsModeError::AlreadyImported { aspects: 3, rows: 371, .. })), "the refusal is the typed one and counts what it refused to write again: {error}");
}

#[tokio::test]
async fn a_subject_the_tree_does_not_hold_names_the_subjects_it_does() {
	// `import_into` refuses a missing subject on the absent `measurements.db`; the command
	// refuses one level up, where it can list what the operator should have typed.
	let dir = tempfile::tempdir().expect("a scratch directory");
	let tree = dir.path().join("v010_legacy");
	copy_tree(&fixture_tree(), &tree);

	let error = import_rows_mode::import(&request(&tree, &dir.path().join("store"), "garage")).await.expect_err("the fixture holds no subject named garage");
	let message = error.to_string();
	assert!(message.contains("garage") && message.contains("plant"), "the refusal names the subject asked for and the ones the tree holds: {message}");
	assert!(!dir.path().join("store").exists(), "refusing wrote nothing: no store was opened for a subject the tree does not hold");
}

#[tokio::test]
async fn a_tree_that_is_not_a_0_1_x_tree_is_refused_with_its_own_name() {
	let dir = tempfile::tempdir().expect("a scratch directory");
	let not_a_tree = dir.path().join("empty");
	fs::create_dir(&not_a_tree).expect("an empty directory is created");

	let error = import_rows_mode::import(&request(&not_a_tree, &dir.path().join("store"), "plant")).await.expect_err("an empty directory is not a 0.1.x tree");
	assert!(error.to_string().contains("metadata.db") || error.to_string().contains("no subject"), "the refusal says what a 0.1.x tree has to have: {error}");
}

#[tokio::test]
async fn the_record_the_command_reports_is_the_record_the_store_keeps() {
	// The summary an operator reads and the record a later reader finds in the store cannot
	// disagree, or the store's own account stops being evidence of anything.
	let dir = tempfile::tempdir().expect("a scratch directory");
	let tree = dir.path().join("v010_legacy");
	copy_tree(&fixture_tree(), &tree);
	let store_root = dir.path().join("store");
	let reported = import_rows_mode::import(&request(&tree, &store_root, "plant")).await.expect("the subject migrates");

	let store = store_open::open_scoped_with_env_options(&store_root, "v010_legacy", "plant").await.expect("the store opens");
	let kept = weftdb::rows_mode::read_import_record(&store).await.expect("the record reads back").expect("the import left its record in the store's index");
	assert_eq!(kept.source, reported.source);
	assert_eq!((kept.database.as_str(), kept.subject.as_str()), (reported.database.as_str(), reported.subject.as_str()));
	assert_eq!(kept.rows(), reported.rows(), "the operator's numbers and the store's numbers add up the same way");
	assert_eq!(kept.not_carried.len(), reported.not_carried.len(), "the store keeps the same list of what stayed behind");
}
