//! What the server says about the environment it was handed (1.0 plan PAC-5a).
//!
//! Before any startup phase reads a setting, the process checks its own environment
//! against the variable registry in `weft_server::settings`: a `WEFT_` name no crate reads
//! names itself, with the nearest real spelling when there is one, and a test harness's own
//! channel says what it is. The server starts either way — a warning is not a decision, and
//! refusing to start on an unknown variable is PAC-5b's change to make, not this slice's.
//!
//! `settings::report` writes to stderr, ahead of the tracing subscriber, so these
//! assertions read stderr; a reader's own warning about a value it cannot use comes from
//! tracing and lands on stdout.

use std::{
	io::{BufRead, BufReader, Read}, process::{Child, Command, Stdio}, sync::mpsc, thread, time::{Duration, Instant}
};

/// How long a server may take to start listening.
const DEADLINE: Duration = Duration::from_secs(60);

/// How long after the listening banner the request-guard line is allowed to take. The guard
/// reports the address it is guarding, which it only knows once the bind happened, so it is
/// printed after the banner; the grace is only a faster failure for the case where it never
/// comes at all.
const GUARD_GRACE: Duration = Duration::from_secs(10);

/// Variables the surrounding test harness may itself set, or that would start a background
/// task, removed from every child so the assertions are about the test's own settings.
const NEUTRALIZED: &[&str] = &["WEFT_RECONCILE_INTERVAL_SECS", "WEFT_BACKUP_INTERVAL_SECS", "WEFT_BACKUP_KEEP", "WEFT_BACKUP_DIR", "OTEL_EXPORTER_OTLP_ENDPOINT", "WEFT_GPU_CALIBRATE", "TEST_DATA_DIR", "WEFT_DATA_DIR", "SKIP_SLOW_TESTS", "WEFT_VERBOSE", "WEFT_REQUIRE_GPU", "WEFT_FAULT"];

/// What a started server wrote, on each stream.
struct Started {
	stdout: String,
	stderr: String,
}

impl Started {
	/// The lines `settings::report` prints, which is what this test file is about.
	fn complaints(&self) -> Vec<&str> {
		self.stderr.lines().filter(|line| line.starts_with("weft-server: ")).collect()
	}
}

/// Start a server with every variable in [`NEUTRALIZED`] unset and `extra` set, on an
/// ephemeral port, and return everything it wrote, having killed it once its listening
/// banner arrived.
fn start_server(extra: &[(&str, &str)]) -> Started {
	let mut cmd = Command::new(env!("CARGO_BIN_EXE_weft-server"));
	cmd.env("WEFT_SERVER_ADDR", "127.0.0.1:0");
	for name in NEUTRALIZED {
		cmd.env_remove(*name);
	}
	cmd.envs(extra.iter().copied());
	let mut child: Child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("spawns weft-server");
	let stderr = drain(child.stderr.take().expect("stderr is piped"));
	let (lines, received) = mpsc::channel::<String>();
	let stdout = child.stdout.take().expect("stdout is piped");
	thread::spawn(move || {
		for line in BufReader::new(stdout).lines().map_while(Result::ok) {
			let _ = lines.send(line);
		}
	});
	let mut stdout_lines: Vec<String> = Vec::new();
	let deadline = Instant::now() + DEADLINE;
	let mut listened = false;
	let mut guarded = false;
	let mut guard_deadline: Option<Instant> = None;
	// The listening banner is not the last line startup writes: the request guard reports
	// after it, because it needs the address the socket actually got. Killing the child at
	// the banner — which an earlier revision of this harness did — races it and loses that
	// line on slower runners, where macOS CI failed. So the child is killed at the guard
	// line, the last line before the server serves, and collection continues to EOF after it.
	loop {
		match received.recv_timeout(Duration::from_millis(100)) {
			Ok(line) => {
				let banner = line.contains("listening on");
				let guard_line = line.contains("request guard:");
				stdout_lines.push(line);
				if banner && !listened {
					listened = true;
					guard_deadline = Some(Instant::now() + GUARD_GRACE);
				}
				if guard_line && !guarded {
					guarded = true;
					let _ = child.kill();
				}
			}
			Err(mpsc::RecvTimeoutError::Disconnected) => break,
			Err(mpsc::RecvTimeoutError::Timeout) => {
				let now = Instant::now();
				match guard_deadline {
					Some(until) => {
						if now > until {
							break;
						}
					}
					None => {
						if now > deadline {
							break;
						}
					}
				}
			}
		}
	}
	let _ = child.wait();
	let stderr = stderr.join().expect("the stderr reader finishes once the server exits");
	assert!(listened, "the server reached its listening banner, so the report changed nothing about startup: {stderr}");
	assert!(guarded, "and reached the request-guard line, the last line it prints before serving: {stderr}");
	Started { stdout: stdout_lines.join("\n"), stderr }
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

/// The mistake this registry exists for: a name one letter off, which the process would
/// otherwise ignore completely.
#[test]
fn a_misspelled_variable_names_itself_and_the_nearest_real_one() {
	let mut extra: Vec<(&str, &str)> = vec![("WEFT_BACKUP_DIRS", "/var/backups"), ("WEFT_RECONCIL_INTERVAL_SECS", "60"), ("WEFT_ON_AMBIGUOUS_COMIT", "exit")];
	let mut expected: Vec<(&str, &str)> = vec![("WEFT_BACKUP_DIRS", "WEFT_BACKUP_DIR"), ("WEFT_RECONCIL_INTERVAL_SECS", "WEFT_RECONCILE_INTERVAL_SECS"), ("WEFT_ON_AMBIGUOUS_COMIT", "WEFT_ON_AMBIGUOUS_COMMIT")];
	// A lowercase `weft_server_addr` is a mistake worth naming everywhere, because the
	// registry's lookup is case-sensitive and it matches nothing. But it can only be *set*
	// where the environment is case-sensitive: a Windows environment block keeps one entry per
	// name whatever its spelling, so setting it there would overwrite `WEFT_SERVER_ADDR` — and
	// with it the port this harness binds — instead of adding a second variable. The
	// case-insensitive suggestion is still covered, by `settings::tests`, which never touches
	// the environment.
	if cfg!(unix) {
		extra.push(("weft_server_addr", "0.0.0.0:9000"));
		expected.push(("weft_server_addr", "WEFT_SERVER_ADDR"));
	}
	let started = start_server(&extra);
	for (mistyped, intended) in &expected {
		let line = format!("weft-server: {mistyped} is not a WeftDB variable; did you mean {intended}?");
		assert!(started.stderr.contains(&line), "{line:?} is one of the lines it prints: {stderr}", stderr = started.stderr);
	}
	assert_eq!(started.complaints().len(), expected.len(), "one line per mistyped name, and no others: {stderr}", stderr = started.stderr);
}

/// A name in our namespace that is nobody's variable is reported as itself. Guessing would
/// send the operator to change a setting that was never the problem.
#[test]
fn an_unknown_variable_with_no_near_spelling_is_reported_without_a_guess() {
	let started = start_server(&[("WEFT_LOG_LEVEL", "debug")]);
	assert_eq!(started.complaints(), ["weft-server: WEFT_LOG_LEVEL is not a WeftDB variable — no setting of it changes anything, and nothing in this build reads it"], "it says so, and does not guess: {stderr}", stderr = started.stderr);
}

/// A test harness's channel that leaked into a deployment: registered, real, and not
/// configuration.
#[test]
fn a_test_channel_set_on_a_real_server_says_so() {
	let started = start_server(&[("WEFT_TEST_SWAP_MATRIX", "swap:seal")]);
	assert_eq!(started.complaints().len(), 1, "one line: {stderr}", stderr = started.stderr);
	assert!(started.complaints()[0].contains("WEFT_TEST_SWAP_MATRIX is a test-harness channel"), "it says what it is: {stderr}", stderr = started.stderr);
	assert!(started.complaints()[0].contains("not configuration"), "and what it is not: {stderr}", stderr = started.stderr);
}

/// The point an operator cares about: a correctly configured server has nothing to say
/// about the environment.
#[test]
fn a_configured_environment_prints_nothing_about_the_environment() {
	let backup_dir = tempfile::tempdir().expect("tempdir");
	let started = start_server(&[("WEFT_BACKUP_DIR", backup_dir.path().to_str().expect("utf-8 path")), ("WEFT_BACKUP_KEEP", "3"), ("WEFT_ON_AMBIGUOUS_COMMIT", "exit"), ("WEFT_MAX_INTERPOLATE_POINTS", "5000"), ("WEFT_ALLOW_ANY_HOST", "1")]);
	assert!(started.complaints().is_empty(), "a named-and-registered setting produces no complaint, and neither does any other: {stderr}", stderr = started.stderr);
	assert!(started.stdout.contains("at most 5000 output points per request"), "the setting itself still does what it always did: {stdout}", stdout = started.stdout);
	assert!(started.stdout.contains("request guard: off"), "and so does the next one: {stdout}", stdout = started.stdout);
}

/// The registry describes reads; it does not add one or move one. A value an existing
/// reader cannot use still comes from that reader, in the place it always printed.
#[test]
fn reading_a_registered_variable_still_configures_the_thing_it_always_configured() {
	let store_root = tempfile::tempdir().expect("tempdir");
	let started = start_server(&[("WEFT_SEGMENT_STORE_ROOT", store_root.path().to_str().expect("utf-8 path")), ("WEFT_ON_AMBIGUOUS_COMMIT", "nonsense")]);
	assert!(started.complaints().is_empty(), "WEFT_ON_AMBIGUOUS_COMMIT is registered, so the registry says nothing about it being set: {stderr}", stderr = started.stderr);
	assert!(started.stdout.contains("WEFT_ON_AMBIGUOUS_COMMIT is neither `poison` nor `exit`"), "its reader does, as it always did: {stdout}", stdout = started.stdout);
	assert!(started.stdout.contains("segment store: created at"), "and the store opened anyway: {stdout}", stdout = started.stdout);
}
