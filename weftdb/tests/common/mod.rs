//! Support for the integration tests and the benches that create legacy
//! [`Database`](weftdb::Database)s.
//!
//! `Database::new` creates a database under [`weftdb::data_dir`], which is the user's own
//! `~/.weftdb/data` unless `TEST_DATA_DIR` or `WEFT_DATA_DIR` names another directory. A
//! test or bench binary that creates databases calls [`data_dir`] before its first one, so
//! they go to a temporary directory of that binary instead. The benches include this file by
//! path, from `weftdb/benches/common/mod.rs`.

use std::{
	path::{Path, PathBuf}, sync::OnceLock
};

/// A binary's data directory, and whether [`data_dir`] created it.
struct DataDir {
	path: PathBuf,
	/// `false` when an explicit `TEST_DATA_DIR` named it: that directory is not the binary's
	/// to remove.
	temporary: bool,
}

static DATA_DIR: OnceLock<DataDir> = OnceLock::new();

/// This binary's data directory, where `Database::new` creates its databases from the first
/// call on.
///
/// An explicit `TEST_DATA_DIR` names it. Otherwise it is a new temporary directory, which
/// `TEST_DATA_DIR` then names: `WEFT_DATA_DIR` is not a reason to write into a real data
/// directory, since `weft-tui` users may set it in their shell. The variable is set once,
/// under the `OnceLock`, before any database of this binary resolves it, and nothing sets it
/// after.
///
/// Each test removes the databases it created ([`remove_database`]) when it passes. The
/// temporary directory itself (`weftdb-test-data-<uuid>` under the system temp dir) is left in
/// place when a test binary exits: empty after a passing run, and holding the database of any
/// test that failed before its cleanup, for inspection. Nothing removes it at exit, since
/// a test binary has no exit hook and removing it while other tests run would race their
/// `Database::new`. A bench binary has its own `main`, which runs the benches one after
/// another and removes the directory after the last ([`remove_temporary_data_dir`]).
///
/// # Panics
///
/// If the directory cannot be created, or `weftdb::data_dir()` does not resolve to it.
pub fn data_dir() -> &'static Path {
	let dir = DATA_DIR.get_or_init(|| {
		let dir = match std::env::var_os("TEST_DATA_DIR") {
			Some(explicit) if !explicit.is_empty() => DataDir { path: PathBuf::from(explicit), temporary: false },
			_ => {
				let path = std::env::temp_dir().join(format!("weftdb-test-data-{}", uuid::Uuid::new_v4()));
				assert!(!path.starts_with(weftdb::default_data_dir()), "the temporary test data dir {} is inside the default data dir", path.display());
				std::env::set_var("TEST_DATA_DIR", &path);
				DataDir { path, temporary: true }
			}
		};
		std::fs::create_dir_all(&dir.path).unwrap_or_else(|e| panic!("cannot create the test data dir {}: {e}", dir.path.display()));
		dir
	});
	assert_eq!(Path::new(&weftdb::data_dir()), dir.path, "weftdb::data_dir() must resolve to the binary's data dir");
	&dir.path
}

/// Remove database `db_name`, which a test or bench created, from [`data_dir`].
pub fn remove_database(db_name: &str) {
	std::fs::remove_dir_all(data_dir().join(db_name)).ok();
}

/// Remove the data dir, with whatever databases are left in it, if [`data_dir`] created it.
/// A directory that an explicit `TEST_DATA_DIR` named is left alone, and so is everything
/// when [`data_dir`] was never called.
///
/// Only a binary that is done with all of its databases may call this: a bench binary, after
/// its last bench. `TEST_DATA_DIR` keeps naming the removed directory, so a database created
/// after all would still land under the system temp dir, never in `~/.weftdb/data`. A
/// failure (on Windows, a file that a handle still holds open) is reported on stderr and
/// leaves the directory in place.
#[allow(dead_code, reason = "only the benches call it: a test binary is never done with all of its databases (see `data_dir`)")]
pub fn remove_temporary_data_dir() {
	let Some(dir) = DATA_DIR.get().filter(|dir| dir.temporary) else {
		return;
	};
	if let Err(e) = std::fs::remove_dir_all(&dir.path) {
		eprintln!("warning: cannot remove the temporary data dir {}: {e}", dir.path.display());
	}
}
