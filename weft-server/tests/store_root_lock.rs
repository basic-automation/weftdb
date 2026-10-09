//! A store root belongs to one process (docs/design/crash-consistency.md section 5.5,
//! OPEN step 1). A second `weft-server` pointed at the same `WEFT_SEGMENT_STORE_ROOT`
//! must fail fast, naming the process that holds the root, instead of racing the
//! first server's frames or failing deep inside Turso.

use std::{
	io::{BufRead, BufReader, Read}, path::Path, process::{Child, Command, ExitStatus, Stdio}, sync::mpsc, thread, time::{Duration, Instant}
};

/// How long a server may take to start listening, or to give up on a held root.
const DEADLINE: Duration = Duration::from_secs(60);

/// Spawn a `weft-server` serving the store at `root` on an ephemeral port, with every
/// optional daemon and exporter off and both output streams piped.
fn spawn_server(root: &Path) -> Child {
	let mut cmd = Command::new(env!("CARGO_BIN_EXE_weft-server"));
	cmd.env("WEFT_SEGMENT_STORE_ROOT", root).env("WEFT_SERVER_ADDR", "127.0.0.1:0");
	for var in ["WEFT_RECONCILE_INTERVAL_SECS", "WEFT_BACKUP_INTERVAL_SECS", "OTEL_EXPORTER_OTLP_ENDPOINT"] {
		cmd.env_remove(var);
	}
	cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("spawns weft-server")
}

/// Read `stream` to its end on a thread, so the server never blocks on a full pipe. The
/// handle yields the text once the server has exited.
fn drain(mut stream: impl Read + Send + 'static) -> thread::JoinHandle<String> {
	thread::spawn(move || {
		let mut text = String::new();
		let _ = stream.read_to_string(&mut text);
		text
	})
}

/// A running server, killed on drop so a failing assertion never leaks it.
struct Running(Child);

impl Drop for Running {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

/// Start a server at `root` and return once it is listening.
fn start_listening(root: &Path) -> Running {
	let mut child = spawn_server(root);
	let stdout = child.stdout.take().expect("stdout is piped");
	let stderr = drain(child.stderr.take().expect("stderr is piped"));
	let (lines, listening) = mpsc::channel();
	thread::spawn(move || {
		for line in BufReader::new(stdout).lines().map_while(Result::ok) {
			let _ = lines.send(line);
		}
	});
	let mut running = Running(child);
	let deadline = Instant::now() + DEADLINE;
	loop {
		match listening.recv_timeout(Duration::from_millis(100)) {
			Ok(line) if line.contains("listening on") => return running,
			Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
			Err(mpsc::RecvTimeoutError::Disconnected) => {
				let status = running.0.wait().expect("waits for the server");
				panic!("the server exited before listening ({status}): {}", stderr.join().unwrap_or_default());
			}
		}
		assert!(Instant::now() < deadline, "the server did not start listening within {DEADLINE:?}");
	}
}

/// Run a server at `root` that is expected to give up, and return its exit status and
/// stderr. Panics if it is still running at the deadline.
fn run_to_exit(root: &Path) -> (ExitStatus, String) {
	let mut child = spawn_server(root);
	drop(drain(child.stdout.take().expect("stdout is piped")));
	let stderr = drain(child.stderr.take().expect("stderr is piped"));
	let mut child = Running(child);
	let deadline = Instant::now() + DEADLINE;
	let status = loop {
		if let Some(status) = child.0.try_wait().expect("polls the server") {
			break status;
		}
		assert!(Instant::now() < deadline, "a second server on a held root must fail fast, but it was still running after {DEADLINE:?}");
		thread::sleep(Duration::from_millis(50));
	};
	(status, stderr.join().expect("reads stderr"))
}

#[test]
fn a_second_server_on_the_same_root_fails_fast_naming_the_holder() {
	let dir = tempfile::tempdir().expect("tempdir");
	let first = start_listening(dir.path());

	let (status, stderr) = run_to_exit(dir.path());
	assert!(!status.success(), "the second server must not start: {stderr}");
	let holder = format!("in use by pid {}", first.0.id());
	assert!(stderr.contains(&holder), "the error names the process holding the root ({holder:?}): {stderr}");

	// The OS drops the lock with its holder, however it exits: no stale LOCK to clear.
	drop(first);
	let third = start_listening(dir.path());
	drop(third);
}
