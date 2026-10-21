//! Robustness track **ROB-2** (release plan C12, C22): a panic while the process is
//! writing the control plane ends the process instead of being answered.
//!
//! [`weftdb::exec::control_plane_write`] marks the region — weftdb's own test
//! `every_op_runs_inside_the_control_plane_write_scope` proves every op of every
//! segment-index transaction runs inside it — and [`weft_server::panic_hook`] is the hook
//! that reads the mark and aborts. This test checks both halves from outside the library:
//! a panic **inside** the region kills the process with a signal a supervisor can see and
//! says why on stderr, and a panic **outside** it still only ends its own task, because
//! ROB-3's catch layer (wave 6) is the only layer that catches panics and unwinding must
//! stay reachable for it.
//!
//! The abort ends whatever process runs it, so each case runs in a child: this test binary
//! re-executed with one test selected, exactly as `poison_exit.rs` does.
//!
//! Run with `cargo test -p weft-server --test control_plane_panic`.

use weft_server::panic_hook::install;

/// Set only in the child: which panic to raise.
const CHILD_MODE: &str = "WEFT_TEST_PANIC_CHILD_MODE";
/// The child panics inside a control-plane write.
const INSIDE: &str = "inside";
/// The child panics in a task that is writing nothing.
const OUTSIDE: &str = "outside";
/// What the child panics with, inside the region.
const INJECTED_INSIDE: &str = "injected panic in the middle of a control-plane write";
/// What the child panics with, outside it.
const INJECTED_OUTSIDE: &str = "injected panic in a task that is not writing the control plane";
/// What the child prints when a panic outside the region left it running.
const SURVIVED: &str = "panic child: still running after the panic";

/// The child's body: install the server's panic hook, then panic in one of the two places.
/// In a normal test run the variable is unset and this does nothing.
#[tokio::test]
async fn control_plane_panic_child() {
	let mode = std::env::var(CHILD_MODE).unwrap_or_default();
	if mode != INSIDE && mode != OUTSIDE {
		return;
	}
	install();
	if mode == INSIDE {
		// The same future the store's writes run in: yielded through, so the mark is one
		// the hook reads from a later poll on a possibly different worker thread.
		weftdb::exec::control_plane_write(async {
			tokio::task::yield_now().await;
			panic!("{INJECTED_INSIDE}");
		})
		.await;
		return;
	}
	let task = tokio::spawn(async { panic!("{INJECTED_OUTSIDE}") });
	let error = task.await.expect_err("the task aborts").into_panic();
	let shown = error.downcast_ref::<String>().cloned().unwrap_or_else(|| format!("{error:?}"));
	assert!(shown.contains(INJECTED_OUTSIDE), "the panic that ended the task: {shown}");
	println!("{SURVIVED}");
}

/// Run [`control_plane_panic_child`] in a child process panicking in `mode`.
async fn run_child(mode: &str) -> std::process::Output {
	let exe = std::env::current_exe().expect("finds the test binary");
	let mut child = tokio::process::Command::new(exe);
	child.args(["control_plane_panic_child", "--exact", "--nocapture", "--test-threads=1"]).env(CHILD_MODE, mode);
	child.output().await.expect("runs the child")
}

#[tokio::test]
async fn a_panic_inside_a_control_plane_write_aborts_the_process() {
	let out = run_child(INSIDE).await;
	let stdout = String::from_utf8_lossy(&out.stdout);
	let stderr = String::from_utf8_lossy(&out.stderr);
	assert!(!out.status.success(), "the child did not die: {out:?}");
	#[cfg(unix)]
	{
		use std::os::unix::process::ExitStatusExt as _;
		assert_eq!(out.status.signal(), Some(6), "it died by SIGABRT (6), not by a status a supervisor might mistake for a clean exit: {out:?}");
	}
	assert!(stderr.contains(INJECTED_INSIDE), "the panic itself is on stderr: {stderr}");
	assert!(stderr.contains("the control plane was being written when this panic happened") && stderr.contains("aborting the process"), "and so is the reason it died: {stderr}");
	assert!(!stdout.contains(SURVIVED), "the child never got past the panic: {stdout}");
}

#[tokio::test]
async fn a_panic_outside_a_control_plane_write_only_ends_its_task() {
	let out = run_child(OUTSIDE).await;
	let stdout = String::from_utf8_lossy(&out.stdout);
	let stderr = String::from_utf8_lossy(&out.stderr);
	assert!(out.status.success(), "the process lives, so ROB-3 can still catch this case: {out:?}");
	assert!(stderr.contains(INJECTED_OUTSIDE), "the panic is still reported by the chained hook: {stderr}");
	assert!(!stderr.contains("the control plane was being written when this panic happened"), "the abort path is not taken: {stderr}");
	assert!(stdout.contains(SURVIVED), "the process kept running: {stdout}");
}
