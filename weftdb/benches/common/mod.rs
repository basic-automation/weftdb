//! Support for the benches that create legacy [`Database`](weftdb::Database)s.
//!
//! `Database::new` creates a database under [`weftdb::data_dir`], which is the user's own
//! `~/.weftdb/data` unless `TEST_DATA_DIR` or `WEFT_DATA_DIR` names another directory. A bench
//! binary's databases go to a temporary data dir of that binary instead, set up by the
//! integration tests' helper (`weftdb/tests/common/mod.rs`, included here by path):
//! [`criterion_main`] sets it up before the first bench and removes it after the last.
//!
//! The benches therefore measure the filesystem of the system temp dir (`TMPDIR`, often a
//! tmpfs). To measure another one, name a directory on it in `TEST_DATA_DIR`: the benches then
//! create their databases there and remove each one, but never the directory itself.

#[path = "../../tests/common/mod.rs"]
mod tests_common;

pub use tests_common::remove_database;

/// The benches' `main`, in place of `criterion_main!`: run the criterion `groups` and print
/// criterion's summary, as `criterion_main!` does, with the binary's databases in its own data
/// dir ([`tests_common::data_dir`]).
///
/// The data dir is set up before the first group, so no `Database::new` of the binary can
/// resolve `~/.weftdb/data`, and removed after the last group
/// ([`tests_common::remove_temporary_data_dir`]). That is safe here, unlike in a test binary:
/// the groups run one after another in this one process, and each has dropped its runtime,
/// and with it every task that could still write to a database, by the time it returns. A
/// bench that panics never gets there, which leaves its database in the directory for
/// inspection.
pub fn criterion_main(groups: &[fn()]) {
	tests_common::data_dir();
	for group in groups {
		group();
	}
	criterion::Criterion::default().configure_from_args().final_summary();
	tests_common::remove_temporary_data_dir();
}

/// Forget database `db_name`, which a bench created, and remove it from the data dir: its
/// entry in `weftdb::DATABASES` and its cached connections go first, so that weftdb's caches
/// no longer hold its files open when they are removed.
///
/// `Database::close` evicts only the metadata connection; the aspects' connections stay open
/// in weftdb's connection cache until this.
#[allow(dead_code, reason = "a bench that cleans up in `Drop`, which cannot await, calls `remove_database` instead")]
pub async fn discard_database(db_name: &str) {
	weftdb::DATABASES.lock().await.retain(|_, info| info.name() != db_name);
	weftdb::clear_connection_cache_by_name(db_name).await;
	remove_database(db_name);
}
