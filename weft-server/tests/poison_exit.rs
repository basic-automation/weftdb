//! `WEFT_ON_AMBIGUOUS_COMMIT=exit` (release plan C-1): the `weftdb` library never exits
//! the process, so the server does. Once its store is write-poisoned it logs why and exits
//! with status 70, for a supervisor to restart it and the restart's recovery to settle the
//! commit. The store is opened as the server's startup opens it
//! (`store_open::open_from_env`, which reads the variable), and the poison is set through
//! weftdb's `fault-injection` hook, in a child process (this test binary re-executed),
//! since the exit ends whatever process it runs in.
//!
//! Run with `cargo test -p weft-server --features fault-injection --test poison_exit`.

use std::time::Duration;

use weft_server::store_open::{open_from_env, spawn_exit_on_poison, OnPoison, AMBIGUOUS_COMMIT_ENV, AMBIGUOUS_COMMIT_EXIT_CODE};
use weftdb::SegmentStore;

/// Set only in the child: the store root it opens.
const CHILD_ROOT: &str = "WEFT_TEST_POISON_EXIT_CHILD_ROOT";

/// What the child prints when it is still running after the poison.
const STILL_RUNNING: &str = "poison exit child: still running after the poison";

/// The child's body: open the store as the server does, poison it as an ambiguous COMMIT
/// would, and wait (long enough to be killed by the exit under `exit`, briefly
/// otherwise). Reaching the end means the server did not exit. In a normal test run the
/// variable is unset and this does nothing.
#[tokio::test]
async fn poison_exit_child() {
	let Some(root) = std::env::var_os(CHILD_ROOT) else { return };
	let store = open_from_env(root).await.expect("opens");
	tokio::time::sleep(Duration::from_millis(50)).await;
	store.inject_poison("segment_index transaction failed with an ambiguous COMMIT: COMMIT: I/O error (Other): sync");
	let wait = if OnPoison::from_env() == OnPoison::Exit { Duration::from_secs(30) } else { Duration::from_millis(500) };
	tokio::time::sleep(wait).await;
	println!("{STILL_RUNNING}");
}

/// Run [`poison_exit_child`] in a child process over `root`, with
/// `WEFT_ON_AMBIGUOUS_COMMIT` set to `on_poison` (or unset).
async fn run_child(root: &std::path::Path, on_poison: Option<&str>) -> std::process::Output {
	let exe = std::env::current_exe().expect("finds the test binary");
	let mut child = tokio::process::Command::new(exe);
	child.args(["poison_exit_child", "--exact", "--nocapture", "--test-threads=1"]).env(CHILD_ROOT, root);
	match on_poison {
		Some(value) => child.env(AMBIGUOUS_COMMIT_ENV, value),
		None => child.env_remove(AMBIGUOUS_COMMIT_ENV),
	};
	child.output().await.expect("runs the child")
}

#[tokio::test]
async fn a_poisoned_store_exits_the_server_with_status_70() {
	let dir = tempfile::tempdir().expect("tempdir");
	let out = run_child(dir.path(), Some("exit")).await;
	let stderr = String::from_utf8_lossy(&out.stderr);
	assert_eq!(out.status.code(), Some(AMBIGUOUS_COMMIT_EXIT_CODE), "the child exits with status 70: {out:?}");
	assert!(stderr.contains("is write-poisoned by segment_index transaction failed with an ambiguous COMMIT") && stderr.contains("exiting with status 70 (WEFT_ON_AMBIGUOUS_COMMIT=exit)"), "it says why: {stderr}");
	assert!(!String::from_utf8_lossy(&out.stdout).contains(STILL_RUNNING), "it did not outlive the poison");

	// The restart opens the store and accepts writes again.
	let store = SegmentStore::open(dir.path()).await.expect("the restart opens the store");
	let poisoned = store.poisoned();
	drop(store);
	assert_eq!(poisoned, None);
}

/// Without `WEFT_ON_AMBIGUOUS_COMMIT=exit` (unset, or `poison`) the server keeps running
/// on a poisoned store: the startup starts no exit task.
#[tokio::test]
async fn without_exit_a_poisoned_store_keeps_the_server_running() {
	for on_poison in [None, Some("poison")] {
		let dir = tempfile::tempdir().expect("tempdir");
		let out = run_child(dir.path(), on_poison).await;
		assert!(out.status.success(), "{on_poison:?}: the child keeps running and passes: {out:?}");
		assert!(String::from_utf8_lossy(&out.stdout).contains(STILL_RUNNING), "{on_poison:?}: {out:?}");
	}
}

/// The exit task holds the store's poison watches, not the store: a store that drops
/// unpoisoned ends the task without exiting, releases its root, and the root reopens.
#[tokio::test]
async fn the_exit_task_lets_the_store_drop() {
	let dir = tempfile::tempdir().expect("tempdir");
	let store = SegmentStore::open(dir.path()).await.expect("opens");
	let watcher = spawn_exit_on_poison(&store);
	tokio::time::sleep(Duration::from_millis(50)).await;
	assert!(!watcher.is_finished(), "a healthy store keeps the task waiting");
	drop(store);
	tokio::time::timeout(Duration::from_secs(10), watcher).await.expect("the task ends once the store drops").expect("without panicking");
	let reopened = SegmentStore::open(dir.path()).await.expect("the root was released");
	drop(reopened);
}
