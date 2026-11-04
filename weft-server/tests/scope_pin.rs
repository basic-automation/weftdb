//! The store-scope pin (FRE-12b) at the process boundary: a `weft-server` whose
//! `WEFT_STORE_DATABASE`/`WEFT_STORE_SUBJECT` name a namespace the store root does not
//! record must refuse the start with the operator-action exit (plan C7's 65), because
//! serving the wrong scope is not a failure — it answers every query with an empty
//! namespace, which is indistinguishable from a lost store.
//!
//! The three ways out are in the message the process prints, so an operator who hits it
//! from a systemd unit has them without shell access to the box.

use std::{
	io::{BufRead, BufReader}, path::Path, process::{Child, Command, Stdio}, sync::mpsc, thread, time::{Duration, Instant}
};

use weft_server::settings::{Tier, VARS};

/// How long a server may take to start listening, or to give up.
const DEADLINE: Duration = Duration::from_secs(60);

/// The scope the store at `root` is created under by these tests.
const STORE_SCOPE: (&str, &str) = ("market", "BTCUSD");

/// Create a store at `root` under [`STORE_SCOPE`], then release the root so a server can
/// hold it. The scope lands in the root's `STORE_FORMAT` marker, which is what the pin
/// measures the environment against.
async fn create_store(root: &Path) {
	let store = weftdb::SegmentStore::open_scoped(root, STORE_SCOPE.0, STORE_SCOPE.1).await.expect("creates a store");
	store.shutdown().await.expect("shuts down cleanly");
	drop(store);
}

/// Spawn `weft-server` at `root` on an ephemeral port with `extra` set, every optional
/// daemon and exporter off, and both output streams piped. Test-harness channels the
/// runner may carry are unset: since PAC-5b a server refuses to start with one set.
fn spawn_server(root: &Path, extra: &[(&str, &str)]) -> Child {
	let mut cmd = Command::new(env!("CARGO_BIN_EXE_weft-server"));
	cmd.env("WEFT_SEGMENT_STORE_ROOT", root).env("WEFT_SERVER_ADDR", "127.0.0.1:0").env_remove("WEFT_STORE_DATABASE").env_remove("WEFT_STORE_SUBJECT");
	for var in ["WEFT_RECONCILE_INTERVAL_SECS", "WEFT_BACKUP_INTERVAL_SECS", "OTEL_EXPORTER_OTLP_ENDPOINT"] {
		cmd.env_remove(var);
	}
	for spec in VARS.iter().filter(|spec| spec.tier == Tier::Test) {
		cmd.env_remove(spec.name);
	}
	for (name, value) in extra {
		cmd.env(name, value);
	}
	cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("spawns weft-server")
}

/// A server's output lines, collected on a thread so a full pipe never blocks it, and
/// the exit status once it gives up. Panics if it is still running at the deadline.
fn wait_for_exit(child: &mut Child) -> (Option<i32>, Vec<String>) {
	let stdout = drain_lines(child.stdout.take().expect("stdout is piped"));
	let stderr = drain_lines(child.stderr.take().expect("stderr is piped"));
	let deadline = Instant::now() + DEADLINE;
	let status = loop {
		if let Some(status) = child.try_wait().expect("polls the server") {
			break status;
		}
		assert!(Instant::now() < deadline, "a refused start must be immediate, but the server was still running after {DEADLINE:?}");
		thread::sleep(Duration::from_millis(50));
	};
	(status.code(), [stdout.join().expect("reads stdout"), stderr.join().expect("reads stderr")].concat())
}

fn drain_lines(stream: impl std::io::Read + Send + 'static) -> thread::JoinHandle<Vec<String>> {
	thread::spawn(move || BufReader::new(stream).lines().map_while(Result::ok).collect())
}

/// Forward one pipe of the server's output into `lines`, line by line, on its own thread.
///
/// A pipe left unread fills its buffer and blocks the server, so both streams are
/// drained from the moment the process starts whatever the test then waits for.
fn forward_lines(stream: Option<impl std::io::Read + Send + 'static>, lines: mpsc::Sender<String>) {
	let Some(stream) = stream else { return };
	thread::spawn(move || {
		for line in BufReader::new(stream).lines().map_while(Result::ok) {
			let _ = lines.send(line);
		}
	});
}

/// A running server, killed on drop so a failing assertion never leaks it.
struct Running(Child);

impl Drop for Running {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

/// Start a server at `root` with `extra` and return once it is listening. Panics with the
/// captured output if it exits first, which is how a refused start shows up here.
fn start_listening(root: &Path, extra: &[(&str, &str)]) -> Running {
	let mut child = spawn_server(root, extra);
	// Both streams are forwarded into one channel: a start that refuses writes its
	// sentence to stderr and a start that succeeds writes its listening line to
	// stdout, and the failure message has to carry whichever of them arrived.
	let (lines, received) = mpsc::channel();
	forward_lines(child.stdout.take(), lines.clone());
	forward_lines(child.stderr.take(), lines.clone());
	drop(lines);
	let running = Running(child);
	let deadline = Instant::now() + DEADLINE;
	let mut seen: Vec<String> = Vec::new();
	loop {
		match received.recv_timeout(Duration::from_millis(100)) {
			Ok(line) => {
				if line.contains("listening on") {
					return running;
				}
				seen.push(line);
			}
			Err(mpsc::RecvTimeoutError::Timeout) => {}
			// The senders are gone, so the process is done: this is the refused start,
			// and the refusal is the message.
			Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the server exited before listening; its output:\n{}", seen.join("\n")),
		}
		assert!(Instant::now() < deadline, "the server did not start listening within {DEADLINE:?}; its output:\n{}", seen.join("\n"));
	}
}

#[tokio::test]
async fn a_scope_the_store_does_not_record_refuses_the_start() {
	let dir = tempfile::tempdir().expect("tempdir");
	let root = dir.path().join("store");
	create_store(&root).await;

	// The default scope: the store holds market/BTCUSD, and this server would serve
	// default/default out of the same root and call it empty.
	let (code, output) = wait_for_exit(&mut spawn_server(&root, &[]));
	assert_eq!(code, Some(weft_server::settings::STORE_EXIT_CODE), "plan C7's operator-action class: {output:?}");
	let text = output.join("\n");
	assert!(text.contains("records the scope market/BTCUSD"), "the message names the scope the store records: {text}");
	assert!(text.contains("configured for scope default/default"), "and the scope this start asked for: {text}");
	assert!(text.contains("WEFT_STORE_SCOPE_PIN=off"), "and how to serve this root under this scope anyway: {text}");
}

#[tokio::test]
async fn naming_the_stores_own_scope_starts_it() {
	let dir = tempfile::tempdir().expect("tempdir");
	let root = dir.path().join("store");
	create_store(&root).await;
	let server = start_listening(&root, &[("WEFT_STORE_DATABASE", STORE_SCOPE.0), ("WEFT_STORE_SUBJECT", STORE_SCOPE.1)]);
	drop(server);
}

#[tokio::test]
async fn a_root_that_holds_no_store_yet_starts_under_any_scope() {
	// The pin compares against a recorded scope. A fresh root has none, so the first
	// start creates the store under the scope the environment names and records it.
	let dir = tempfile::tempdir().expect("tempdir");
	let server = start_listening(&dir.path().join("fresh"), &[]);
	drop(server);
}

#[tokio::test]
async fn turning_the_pin_off_serves_the_other_scope() {
	let dir = tempfile::tempdir().expect("tempdir");
	let root = dir.path().join("store");
	create_store(&root).await;
	let server = start_listening(&root, &[("WEFT_STORE_SCOPE_PIN", "off")]);
	drop(server);
}

#[tokio::test]
async fn a_pin_value_that_is_not_on_or_off_refuses_the_start() {
	let dir = tempfile::tempdir().expect("tempdir");
	let (code, output) = wait_for_exit(&mut spawn_server(&dir.path().join("store"), &[("WEFT_STORE_SCOPE_PIN", "sometimes")]));
	assert_eq!(code, Some(weft_server::settings::CONFIG_EXIT_CODE), "a value the server cannot honour stops it (C7's config class): {output:?}");
	let text = output.join("\n");
	assert!(text.contains("WEFT_STORE_SCOPE_PIN"), "naming the variable: {text}");
	assert!(text.contains("on") && text.contains("off"), "and the values it accepts: {text}");
}
