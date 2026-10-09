//! Support for the tests that create legacy [`Database`](weftdb::Database)s.
//!
//! `Database::new` creates a database under [`weftdb::data_dir`], which is the user's own
//! `~/.weftdb/data` unless `TEST_DATA_DIR` or `WEFT_DATA_DIR` names another directory. Every
//! test that creates or opens a database calls [`data_dir`] first, so this test binary's
//! databases go to a temporary directory instead. The same helper serves `weftdb`'s
//! integration tests (`weftdb/tests/common/mod.rs`).

use std::{
	path::{Path, PathBuf}, sync::OnceLock
};

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// This test binary's data directory, where `Database::new` creates its databases from the
/// first call on.
///
/// An explicit `TEST_DATA_DIR` names it, which is how `test_api` and `test_pipeline_api`
/// reach a real `Crypto` corpus (`TEST_DATA_DIR=~/.weftdb/data`). Otherwise it is a new
/// temporary directory, which `TEST_DATA_DIR` then names: `WEFT_DATA_DIR` is not a reason
/// to write into a real data directory, since `weft-tui` users may set it in their shell.
/// The variable is set once, under the `OnceLock`, before any database of this binary
/// resolves it, and no test sets it after.
///
/// Each test removes the databases it created ([`remove_database`]) when it passes. The
/// temporary directory itself (`weft-orchestration-test-data-<uuid>` under the system temp dir) is left in
/// place when the binary exits: empty after a passing run, and holding the database of any
/// test that failed before its cleanup, for inspection. Nothing removes it at exit, since
/// a test binary has no exit hook and removing it while other tests run would race their
/// `Database::new`.
///
/// # Panics
///
/// If the directory cannot be created, or `weftdb::data_dir()` does not resolve to it.
pub fn data_dir() -> &'static Path {
	let dir = DATA_DIR.get_or_init(|| {
		let dir = match std::env::var_os("TEST_DATA_DIR") {
			Some(explicit) if !explicit.is_empty() => PathBuf::from(explicit),
			_ => {
				let dir = std::env::temp_dir().join(format!("weft-orchestration-test-data-{}", uuid::Uuid::new_v4()));
				assert!(!dir.starts_with(weftdb::default_data_dir()), "the temporary test data dir {} is inside the default data dir", dir.display());
				std::env::set_var("TEST_DATA_DIR", &dir);
				dir
			}
		};
		std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("cannot create the test data dir {}: {e}", dir.display()));
		dir
	});
	assert_eq!(Path::new(&weftdb::data_dir()), dir, "weftdb::data_dir() must resolve to the test binary's data dir");
	dir
}

/// Remove database `db_name`, which a test created, from [`data_dir`].
pub fn remove_database(db_name: &str) {
	std::fs::remove_dir_all(data_dir().join(db_name)).ok();
}
