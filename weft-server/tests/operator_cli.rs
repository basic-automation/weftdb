#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]
//! PAC-7's operator CLI, end to end against the binary itself: the
//! dispatch, the plan C7 exit-code contract, and the store identity
//! reported at startup.
//!
//! The statuses are the contract a supervisor reads, so they are asserted
//! on the process, not on the parser: a usage error exits 2 (not the
//! runtime's flattened 1), help and `--version` exit 0, and a server that
//! opens a store prints the store's identity — root, `store_uuid`, scope
//! and the previous shutdown's cleanliness — before its listening banner.

use std::{
	io::{BufRead, BufReader, Read}, process::{Command, Stdio}, sync::mpsc, thread, time::{Duration, Instant}
};

/// How long a server may take to start listening.
const DEADLINE: Duration = Duration::from_secs(60);

/// Variables the surrounding test harness may itself set, removed from
/// every child so the assertions are about the test's own settings.
const NEUTRALIZED: &[&str] = &["WEFT_SEGMENT_STORE_ROOT", "WEFT_STORE_DATABASE", "WEFT_STORE_SUBJECT", "WEFT_RECONCILE_INTERVAL_SECS", "WEFT_BACKUP_INTERVAL_SECS", "WEFT_BACKUP_KEEP", "WEFT_BACKUP_DIR", "OTEL_EXPORTER_OTLP_ENDPOINT", "WEFT_GPU_CALIBRATE", "WEFT_DATA_DIR", "SKIP_SLOW_TESTS", "WEFT_VERBOSE", "WEFT_REQUIRE_GPU", "WEFT_FAULT"];

/// A `weft-server` invocation with the harness's environment neutralized.
fn invocation(args: &[&str]) -> Command {
	let mut cmd = Command::new(env!("CARGO_BIN_EXE_weft-server"));
	cmd.args(args);
	for name in NEUTRALIZED {
		cmd.env_remove(*name);
	}
	cmd
}

/// Run the binary to completion and return (exit code, stdout, stderr).
fn run(args: &[&str]) -> (Option<i32>, String, String) {
	let output = invocation(args).stdin(Stdio::null()).output().expect("runs weft-server");
	(output.status.code(), String::from_utf8_lossy(&output.stdout).into_owned(), String::from_utf8_lossy(&output.stderr).into_owned())
}

#[test]
fn help_and_version_exit_zero_and_name_the_contract() {
	for args in [&["--help"][..], &["-h"][..], &["help"][..]] {
		let (code, stdout, _) = run(args);
		assert_eq!(code, Some(0), "{args:?} exits 0");
		assert!(stdout.contains("keygen") && stdout.contains("restore") && stdout.contains("import-rows-mode") && stdout.contains("openapi"), "the help names every command: {stdout}");
		assert!(stdout.contains("65") && stdout.contains("store needs operator action"), "the help prints the C7 table: {stdout}");
	}
	let (code, stdout, _) = run(&["--version"]);
	assert_eq!(code, Some(0), "--version exits 0");
	assert!(stdout.starts_with("weft-server "), "the version line names the service: {stdout}");
}

#[test]
fn a_command_line_that_names_nothing_exits_with_usage_status_2() {
	for args in [&["restor"][..], &["import-rowsmode"][..], &["--bogus"][..], &["serve", "extra"][..], &["keygen", "--bogus"][..], &["keygen", "--role", "superuser"][..], &["import-rows-mode", "tree"][..]] {
		let (code, _, stderr) = run(args);
		assert_eq!(code, Some(2), "{args:?} is a usage error (plan C7), not an unexpected error: {stderr}");
		assert!(!stderr.is_empty(), "{args:?} says what was wrong");
	}
}

#[test]
fn startup_reports_the_store_identity_before_the_banner() {
	let dir = tempfile::TempDir::new().expect("tempdir");
	let root = dir.path().join("store");
	for run_no in 0..2 {
		let mut cmd = invocation(&[]);
		cmd.env("WEFT_SERVER_ADDR", "127.0.0.1:0").env("WEFT_SEGMENT_STORE_ROOT", &root);
		let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("spawns weft-server");
		let stderr = drain(child.stderr.take().expect("stderr is piped"));
		let (lines, received) = mpsc::channel::<String>();
		let stdout = child.stdout.take().expect("stdout is piped");
		thread::spawn(move || {
			for line in BufReader::new(stdout).lines().map_while(Result::ok) {
				let _ = lines.send(line);
			}
		});
		let mut identity: Option<String> = None;
		let deadline = Instant::now() + DEADLINE;
		loop {
			match received.recv_timeout(Duration::from_millis(100)) {
				Ok(line) => {
					let banner = line.contains("listening on");
					if line.starts_with("segment store:") && line.contains("store ") {
						identity = Some(line);
					}
					if banner {
						break;
					}
				}
				Err(mpsc::RecvTimeoutError::Disconnected) => break,
				Err(mpsc::RecvTimeoutError::Timeout) => assert!(Instant::now() <= deadline, "the server reached its banner within the deadline"),
			}
		}
		let identity = identity.unwrap_or_else(|| panic!("run {run_no}: the identity line precedes the banner"));
		assert!(identity.contains(&root.display().to_string()), "run {run_no}: the root: {identity}");
		assert!(identity.contains("scope default/default"), "run {run_no}: the scope: {identity}");
		assert!(identity.contains("layout 2"), "run {run_no}: the layout: {identity}");
		if run_no == 0 {
			assert!(identity.contains("created"), "the first run creates the store: {identity}");
			assert!(identity.contains("previous shutdown not clean"), "nothing shut it down before: {identity}");
		} else {
			assert!(identity.contains("opened"), "the second run reopens the store the first created: {identity}");
			assert!(identity.contains("previous shutdown not clean"), "the harness kills its children, so no clean shutdown was recorded: {identity}");
		}
		// Extract the UUID so the second run can be checked against it.
		let uuid = identity.split("(store ").nth(1).and_then(|rest| rest.split(',').next()).unwrap_or_default().to_string();
		assert!(uuid.len() == 36 && uuid.matches('-').count() == 4, "run {run_no}: the identity carries the store_uuid: {identity}");
		let _ = child.kill();
		let _ = child.wait();
		let _ = stderr.join();
		if run_no == 0 {
			// Stash the first run's UUID in the root's sibling for the second run's check.
			std::fs::write(dir.path().join("first-uuid"), &uuid).expect("stash");
		} else {
			let first = std::fs::read_to_string(dir.path().join("first-uuid")).expect("read back");
			assert_eq!(uuid, first, "the same store reports the same identity across restarts");
		}
	}
}

/// Read `stream` to its end on a thread, so the server never blocks on a
/// full pipe.
fn drain(mut stream: impl Read + Send + 'static) -> thread::JoinHandle<String> {
	thread::spawn(move || {
		let mut text = String::new();
		let _ = stream.read_to_string(&mut text);
		text
	})
}
