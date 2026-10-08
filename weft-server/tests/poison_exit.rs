//! `WEFT_ON_AMBIGUOUS_COMMIT=exit` (release plan C-1): the `weftdb` library never exits
//! the process, so the server does. Once its store is write-poisoned it logs why and exits
//! with status 70, for a supervisor to restart it and the restart's recovery to settle the
//! commit. The poison is set through weftdb's `fault-injection` hook, in a child process
//! (this test binary re-executed), since the exit ends whatever process it runs in.
//!
//! Run with `cargo test -p weft-server --features fault-injection --test poison_exit`.

use std::{sync::Arc, time::Duration};

use weft_server::store_open::{spawn_exit_on_poison, AMBIGUOUS_COMMIT_EXIT_CODE};
use weftdb::SegmentStore;

/// Set only in the child: the store root it opens.
const CHILD_ROOT: &str = "WEFT_TEST_POISON_EXIT_CHILD_ROOT";

/// The child's body: open a store, start the exit-on-poison task, poison the store as an
/// ambiguous COMMIT would, and wait. Reaching the end means the server did not exit. In a
/// normal test run the variable is unset and this does nothing.
#[tokio::test]
async fn poison_exit_child() {
	let Some(root) = std::env::var_os(CHILD_ROOT) else { return };
	let store = Arc::new(SegmentStore::open(root).await.expect("opens"));
	let watcher = spawn_exit_on_poison(Arc::clone(&store));
	tokio::time::sleep(Duration::from_millis(50)).await;
	assert!(!watcher.is_finished(), "a healthy store keeps the server running");
	store.inject_poison("segment_index transaction failed with an ambiguous COMMIT: COMMIT: I/O error (Other): sync");
	tokio::time::sleep(Duration::from_secs(30)).await;
	panic!("the server kept running on a poisoned store");
}

#[tokio::test]
async fn a_poisoned_store_exits_the_server_with_status_70() {
	let dir = tempfile::tempdir().expect("tempdir");
	let exe = std::env::current_exe().expect("finds the test binary");
	let out = tokio::process::Command::new(exe).args(["poison_exit_child", "--exact", "--nocapture", "--test-threads=1"]).env(CHILD_ROOT, dir.path()).output().await.expect("runs the child");
	let stderr = String::from_utf8_lossy(&out.stderr);
	assert_eq!(out.status.code(), Some(AMBIGUOUS_COMMIT_EXIT_CODE), "the child exits with status 70: {out:?}");
	assert!(stderr.contains("is write-poisoned by segment_index transaction failed with an ambiguous COMMIT") && stderr.contains("exiting with status 70 (WEFT_ON_AMBIGUOUS_COMMIT=exit)"), "it says why: {stderr}");

	// The restart opens the store and accepts writes again.
	let store = SegmentStore::open(dir.path()).await.expect("the restart opens the store");
	let poisoned = store.poisoned();
	drop(store);
	assert_eq!(poisoned, None);
}
