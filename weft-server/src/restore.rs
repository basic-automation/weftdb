//! `weft-server restore` — put a backup back on disk while nothing serves it (1.0 plan
//! PAC-13).
//!
//! The server already has one restore path, the drill route (`POST
//! /api/v1/storage/restore/drill`, [`crate::backup_routes`]): it restores a backup into a
//! throwaway sibling directory and verifies it, *while the store is live*, which is what
//! makes a drill safe to run on a working machine. This command is the other half of
//! C19's single restore entry point — the half an actual loss needs: a backup directory,
//! a machine, no server running, and a root that does not exist yet.
//!
//! What it does, in order:
//!
//! 1. Reads the backup's `MANIFEST.json` and picks the mode from it, so an operator
//!    cannot ask for the wrong one: a whole-store manifest
//!    ([`weftdb::WHOLE_STORE_MANIFEST_FORMAT`]) restores frames *and* control plane with
//!    [`weftdb::restore_store`]; a control-plane manifest restores the four databases
//!    with [`weftdb::restore_control_plane`].
//! 2. Restores into the fresh root `--into` names. The library does the durable work:
//!    the restore intent marker (C-7) is written **first**, every file is verified
//!    against the manifest, and the root is published by renaming a `.restoring-*`
//!    staging sibling, so a restore that fails leaves no half-store behind.
//! 3. With `--adopt`, opens the restored root once as `--database`/`--subject` (default:
//!    the scope in the environment, exactly as the server would), which runs
//!    restore-mode recovery — the frames the manifest carried are adopted into the index
//!    through their own descriptors and the intent marker is removed — prints what that
//!    recovery did, and shuts the store down cleanly. Adoption is idempotent, so an
//!    operator who skips this flag loses nothing: the server's first open of the root
//!    does the same work.
//!
//! # Exit status (plan C-7)
//!
//! The status is part of the contract, so this command exits the process itself rather
//! than letting the runtime print an error and return 1:
//!
//! - [`crate::settings::CONFIG_EXIT_CODE`] (78) — the command line or the destination is
//!   wrong: an unparseable flag, a missing value, a `--into` root that already exists.
//!   Nothing was written.
//! - [`crate::settings::STORE_EXIT_CODE`] (65) — the *backup* is what needs the
//!   operator: not a backup directory, an unfinished (staging-named) backup, a backup
//!   changed after it was taken, a manifest this release does not know, a frame that does
//!   not verify, or a backup taken by a newer WeftDB whose write floor this build cannot
//!   honour. The same code covers a restore that failed partway and an adopted root that
//!   would not open: the C-7 table gives an incomplete restore 65, and the destination is
//!   then either untouched or a directory whose next open retries.
//!
//! Both constants are the codes the server's startup already uses, so a supervisor sees
//! one vocabulary; the operator CLI slice (PAC-7) folds this command into its table
//! without changing a number.

use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat};
use serde::Serialize;
use weftdb::{is_complete_backup, is_staging_name, restore_control_plane, restore_store, BackupManifest, RestoreIntent, SnapshotReport};

use crate::{
	settings::{CONFIG_EXIT_CODE, STORE_EXIT_CODE}, store_open
};

/// The argument word that selects this command: `weft-server restore …`. It is a constant
/// so `main.rs`, the tests and the docs name one string.
pub const SUBCOMMAND: &str = "restore";

/// The help text, printed by `weft-server restore --help` and by any usage error.
pub const USAGE: &str = r#"usage: weft-server restore <BACKUP_DIR> --into <NEW_ROOT> [OPTIONS]

Restore a backup taken by this server (the backup daemon's directory under
WEFT_BACKUP_DIR, or one a POST /api/v1/storage/backup wrote) into a store root
that does not exist yet. Nothing may be serving that root: this is the offline
path, the one you run when the store is gone.

  <BACKUP_DIR>            a backup directory: MANIFEST.json, the four control-plane
                          databases, and for a whole-store backup segments/ too
  -i, --into <NEW_ROOT>   the root to publish the restored store into; it must NOT exist
      --adopt             open the restored root once, so frame adoption runs now and the
                          restore intent marker is cleared, then stop
  -d, --database <NAME>   scope database for --adopt (default: $WEFT_STORE_DATABASE or "default")
  -s, --subject <NAME>    scope subject for --adopt (default: $WEFT_STORE_SUBJECT or "default")
      --json              print the report as JSON instead of lines
  -h, --help              print this help

Exit status: 0 restored; 78 the command line or the destination; 65 the backup needs the
operator (unreadable, unfinished, changed, newer than this build) or the restore failed.
Nothing is ever written to an existing root, and a restore that fails publishes nothing.

Serve what it restored with WEFT_SEGMENT_STORE_ROOT=<NEW_ROOT> weft-server."#;

/// A parsed `weft-server restore` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
	/// The backup directory to read.
	pub backup: PathBuf,
	/// The fresh root to publish the restored store into.
	pub into: PathBuf,
	/// Whether to open the restored root once and complete adoption.
	pub adopt: bool,
	/// The scope database for `--adopt`; `None` takes the environment's.
	pub database: Option<String>,
	/// The scope subject for `--adopt`; `None` takes the environment's.
	pub subject: Option<String>,
	/// Whether to print the report as JSON.
	pub json: bool,
}

/// What `weft-server restore …` asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
	/// `--help` or `-h` anywhere in the command line: print [`USAGE`] and succeed.
	Help,
	/// A restore to run.
	Restore(Request),
}

/// A restore that did not happen, carrying the exit status plan C-7 gives the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
	/// What the operator reads on stderr: the reason, and usually the fix.
	pub message: String,
	/// The process exit status: [`CONFIG_EXIT_CODE`] or [`STORE_EXIT_CODE`].
	pub exit_code: i32,
}

impl std::fmt::Display for Refused {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(&self.message)
	}
}

impl std::error::Error for Refused {}

/// A refusal about the command line or the destination: the operator has to fix
/// something other than the backup, so plan C-7's `EX_CONFIG`.
fn usage_error(message: impl Into<String>) -> Refused {
	Refused { message: message.into(), exit_code: CONFIG_EXIT_CODE }
}

/// A refusal about the backup, or a restore that failed while writing it: plan C-7's
/// `EX_DATAERR` class, the one an operator acts on rather than retries.
fn backup_refused(why: impl std::fmt::Display) -> Refused {
	Refused { message: why.to_string(), exit_code: STORE_EXIT_CODE }
}

/// One restored control-plane file, in the report's own shape so the JSON keys are the
/// CLI's contract and not a library struct's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RestoredFile {
	/// The file's name, one of [`weftdb::CONTROL_PLANE_FILES`].
	pub name: String,
	/// The user tables verified readable in it.
	pub tables: usize,
	/// The rows verified readable across those tables.
	pub rows: i64,
	/// Its size on disk in bytes.
	pub bytes: u64,
}

/// What the optional first open (`--adopt`) did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Adoption {
	/// The scope the restored root was opened as.
	pub database: String,
	/// The scope subject it was opened as.
	pub subject: String,
	/// Whether the root carried a restore intent marker, i.e. whether this open was the
	/// restore-mode recovery or an ordinary open of a control-plane restore.
	pub restore_mode: bool,
	/// Orphan frames adopted into the index (D-S16, R8).
	pub adopted: usize,
	/// Orphan frames journaled `held` until TAG-9 binds series keys.
	pub held: usize,
	/// Rows quarantined by the pass.
	pub quarantined: usize,
	/// Rows moved to suspect quarantine.
	pub suspected: usize,
	/// Files under `segments/` nothing claimed.
	pub orphans: usize,
	/// Whether any suspect record names a frame version this build predates.
	pub incompatible_store: bool,
}

/// What a completed restore put on disk. This is the `--json` document, so its field
/// names are the CLI's output contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
	/// `"whole-store"` or `"control-plane"`, chosen by the backup's manifest.
	pub mode: &'static str,
	/// The backup directory that was read.
	pub backup: PathBuf,
	/// The root the store was published into.
	pub root: PathBuf,
	/// The restore intent marker the whole-store restore published with (C-7). Absent for
	/// a control-plane restore, which carries no frames to adopt.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub intent: Option<RestoreIntent>,
	/// How many frames the whole-store backup carried and the restore verified.
	pub frames: usize,
	/// The rows verified readable across the restored control plane.
	pub rows: i64,
	/// The bytes those files hold.
	pub bytes: u64,
	/// The per-file verification, in [`weftdb::CONTROL_PLANE_FILES`] order.
	pub files: Vec<RestoredFile>,
	/// Set when `--adopt` ran; `None` means adoption waits for the first open.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub adoption: Option<Adoption>,
}

/// Run `weft-server restore …`. `args` is the command line *after* the `restore` word.
///
/// # Process exit
///
/// A refusal ends the process with [`Refused::exit_code`] (78 or 65) here, after writing
/// the reason to stderr: the status is what a supervisor or a restore script reads, and
/// letting the error bubble to the runtime would flatten every one of them to 1. It
/// follows [`crate::store_open::spawn_exit_on_poison`] in doing this inside the library,
/// where the codes are defined.
///
/// # Errors
///
/// Only an unexpected failure of the report's own JSON encoding; every refusal of the
/// restore exits the process instead.
pub async fn run(args: impl Iterator<Item = String>) -> anyhow::Result<()> {
	let request = match parse(args) {
		Ok(Invocation::Help) => {
			eprintln!("{USAGE}");
			return Ok(());
		}
		Ok(Invocation::Restore(request)) => request,
		Err(refused) => exit_refused(&refused),
	};
	match execute(&request).await {
		Ok(report) => {
			emit(&report, request.json);
			Ok(())
		}
		Err(refused) => exit_refused(&refused),
	}
}

/// Print the refusal the way every `weft-server` CLI failure prints, then leave with its
/// code.
fn exit_refused(refused: &Refused) -> ! {
	eprintln!("weft-server restore: {}", refused.message);
	std::process::exit(refused.exit_code)
}

/// Write the report: the JSON document, or one fact per line for a human reading a
/// terminal.
fn emit(report: &Report, json: bool) {
	if json {
		let text = serde_json::to_string_pretty(report).expect("a restore report always serializes");
		println!("{text}");
		return;
	}
	println!("restored a {} backup from {} into {}", report.mode, report.backup.display(), report.root.display());
	if let Some(intent) = &report.intent {
		println!("  store {} taken {} ({}), restore started {}", intent.store_uuid, utc_millis(intent.backup_created_ms), intent.format, utc_millis(intent.restore_started_ms));
	}
	for file in &report.files {
		println!("  {:<18} {:>4} table(s) {:>12} row(s) {:>12} bytes", file.name, file.tables, file.rows, file.bytes);
	}
	println!("  {} frame(s) verified, {} row(s) and {} bytes of control plane", report.frames, report.rows, report.bytes);
	match &report.adoption {
		Some(adoption) if adoption.restore_mode => println!("  adopted {} frame(s) into the index ({} held for series keys), {} quarantined, {} suspect, opened as {}/{}", adoption.adopted, adoption.held, adoption.quarantined, adoption.suspected, adoption.database, adoption.subject),
		Some(adoption) => println!("  opened as {}/{}: nothing to adopt (a control-plane restore carries no frames)", adoption.database, adoption.subject),
		None => println!("  adoption runs at the first open of the restored root; add --adopt to run it now"),
	}
	println!("serve it with WEFT_SEGMENT_STORE_ROOT={} weft-server", report.root.display());
}

/// `millis` as an ISO-8601 UTC second stamp with milliseconds, so a report line can be
/// read without a clock at hand and compared against a backup directory name.
fn utc_millis(millis: u64) -> String {
	DateTime::from_timestamp_millis(millis.try_into().unwrap_or(i64::MAX)).map_or_else(|| format!("{millis}ms"), |when| when.to_rfc3339_opts(SecondsFormat::Millis, true))
}

/// Parse the command line after `restore`.
///
/// # Errors
///
/// [`Refused`] with [`CONFIG_EXIT_CODE`] for a flag this command does not take, a missing
/// value, a duplicated argument, a positional beyond the one backup directory, or a scope
/// flag without `--adopt`.
pub fn parse(args: impl Iterator<Item = String>) -> Result<Invocation, Refused> {
	let mut backup: Option<String> = None;
	let mut into: Option<String> = None;
	let mut adopt = false;
	let mut database: Option<String> = None;
	let mut subject: Option<String> = None;
	let mut json = false;
	let mut args = args.peekable();
	// No argument at all is a question, not an error: `weft-server restore` prints the help.
	if args.peek().is_none() {
		return Ok(Invocation::Help);
	}
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--help" | "-h" => return Ok(Invocation::Help),
			"--into" | "-i" => {
				let value = value_of(&arg, &mut args)?;
				if into.is_some() {
					return Err(usage_error(format!("{arg} given twice; a restore has one destination")));
				}
				into = Some(value);
			}
			"--database" | "-d" => database = Some(value_of(&arg, &mut args)?),
			"--subject" | "-s" => subject = Some(value_of(&arg, &mut args)?),
			"--adopt" => adopt = true,
			"--json" => json = true,
			other => {
				if other.starts_with('-') {
					return Err(usage_error(format!("unknown option {other:?} for `weft-server restore`; try --help")));
				}
				if backup.is_some() {
					return Err(usage_error(format!("a restore takes one backup directory; {other:?} is a second one (there is no `restore --into <a> <b>`)")));
				}
				backup = Some(other.to_string());
			}
		}
	}
	let Some(backup) = backup else {
		return Err(usage_error(format!("no backup directory given; usage: weft-server restore <BACKUP_DIR> --into <NEW_ROOT>{}", if into.is_some() { "" } else { " (and --into is missing too)" })));
	};
	let Some(into) = into else {
		return Err(usage_error("--into <NEW_ROOT> is required: a restore never overwrites a root, so say where to publish the store"));
	};
	if (database.is_some() || subject.is_some()) && !adopt {
		return Err(usage_error("--database and --subject name the scope to open, which only --adopt does; add --adopt or drop them"));
	}
	Ok(Invocation::Restore(Request { backup: PathBuf::from(backup), into: PathBuf::from(into), adopt, database, subject, json }))
}

/// The value that follows an option word, or a refusal naming the option.
fn value_of(option: &str, args: &mut impl Iterator<Item = String>) -> Result<String, Refused> {
	args.next().ok_or_else(|| usage_error(format!("{option} needs a value")))
}

/// Run the restore `request` asks for. Split out from [`run`] so a test can assert the
/// refusal — message and exit code — without ending a process.
///
/// # Errors
///
/// The [`Refused`] the exit status is chosen from.
pub async fn execute(request: &Request) -> Result<Report, Refused> {
	if request.into.exists() {
		return Err(usage_error(format!("refusing to restore into {}: a restore publishes a store under a fresh name, so the destination must not exist. Move the old root aside, or name another destination", request.into.display())));
	}
	let manifest = BackupManifest::read(&request.backup).await.map_err(|error| backup_refused(format!("{error:#}")))?;
	let Some(manifest) = manifest else {
		return Err(no_manifest(&request.backup).await);
	};
	let whole = manifest.is_whole_store();
	let (root, files, frames, intent) = if whole {
		let restored = restore_store(&request.backup, &request.into).await.map_err(|error| backup_refused(restore_failed(&error, &request.into)))?;
		let intent = restored.intent.clone();
		(restored.root, restored.restored, restored.frames, Some(intent))
	} else {
		let restored = restore_control_plane(&request.backup, &request.into).await.map_err(|error| backup_refused(restore_failed(&error, &request.into)))?;
		(restored.root, restored.restored, 0, None)
	};
	let files: Vec<RestoredFile> = files.iter().map(RestoredFile::from_report).collect();
	let rows = files.iter().map(|file| file.rows).sum();
	let bytes = files.iter().map(|file| file.bytes).sum();
	let mut report = Report { mode: if whole { "whole-store" } else { "control-plane" }, backup: request.backup.clone(), root, intent, frames, rows, bytes, files, adoption: None };
	if request.adopt {
		let (database, subject) = match (request.database.clone(), request.subject.clone()) {
			(Some(database), Some(subject)) => (database, subject),
			(database, subject) => {
				let (env_database, env_subject) = store_open::store_scope_from_env();
				(database.unwrap_or(env_database), subject.unwrap_or(env_subject))
			}
		};
		report.adoption = Some(adopt(&report.root, &database, &subject, whole).await?);
	}
	Ok(report)
}

/// The reason a backup has no `MANIFEST.json`, worded for the case the operator is
/// actually in.
async fn no_manifest(backup: &Path) -> Refused {
	let name = backup.file_name().and_then(|name| name.to_str()).unwrap_or_default();
	if is_staging_name(name) {
		return backup_refused(format!("{} is one of a backup's own staging directories — its name starts with {} — so it holds a backup that never finished. Name the finished `backup-…` directory", backup.display(), weftdb::STAGING_PREFIXES.join(", ")));
	}
	match is_complete_backup(backup).await {
		Ok(true) => backup_refused(format!("{} is a backup from before `MANIFEST.json`: it has the four control-plane files but nothing recorded to check them against, so this command cannot verify what it would restore. Take a fresh backup of the store and restore that", backup.display())),
		Ok(false) => backup_refused(format!("{} holds no MANIFEST.json and not the four control-plane files either: name a backup directory — a `backup-…` directory the server's backup wrote — not a store root or a half-copied one", backup.display())),
		Err(error) => backup_refused(format!("checking {} failed: {error}", backup.display())),
	}
}

/// A failure inside the restore itself, worded with the promise the staging rename
/// makes: the destination is either the finished store or absent.
fn restore_failed(error: &anyhow::Error, into: &Path) -> String {
	format!("{error:#}\n  nothing was published at {}; the staging sibling a partial restore leaves (a name starting with {:?}) is removed by the next restore or by the backup daemon's sweep", into.display(), weftdb::backup::RESTORING_PREFIX)
}

/// Open the restored root once so restore-mode recovery adopts the frames now, then stop
/// cleanly. Adoption is idempotent, so this is a convenience and a verification, not a
/// required step.
async fn adopt(root: &Path, database: &str, subject: &str, whole: bool) -> Result<Adoption, Refused> {
	let restore_mode = whole && read_intent(root).await;
	let store = crate::store_open::open_scoped_with_env_options(root, database, subject).await.map_err(|error| backup_refused(format!("the restore published {root} but opening it failed: {error:#}\n  the destination is intact; the next open retries adoption, which is idempotent", root = root.display())))?;
	let recovery = store.recovery_report();
	let adoption = Adoption { database: database.to_string(), subject: subject.to_string(), restore_mode, adopted: recovery.adopted, held: recovery.held, quarantined: recovery.quarantined, suspected: recovery.suspected, orphans: recovery.orphans, incompatible_store: recovery.incompatible_store };
	let shutdown = store.shutdown().await;
	// The store is closed and its root lock released before this command reports, so a
	// `restore --into` of a second root in the same process cannot meet a live lock.
	drop(store);
	if let Err(error) = shutdown {
		return Err(backup_refused(format!("the restored root {root} opened and adopted, but its clean shutdown failed: {error:#}\n  the store is usable; open it again (the server) and shut it down cleanly to clear the flag", root = root.display())));
	}
	Ok(adoption)
}

/// Whether the root carries a restore intent marker. A marker that will not parse is
/// reported as absent here — the open that follows reads it and fails loudly, which is
/// the library's job, not this report's.
async fn read_intent(root: &Path) -> bool {
	weftdb::read_restore_intent(root).await.ok().flatten().is_some()
}

impl RestoredFile {
	fn from_report(report: &SnapshotReport) -> Self {
		Self { name: report.dest.file_name().and_then(|name| name.to_str()).unwrap_or_default().to_string(), tables: report.tables, rows: report.rows, bytes: report.bytes }
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The command line after `restore`, as the shell would hand it over.
	fn argv(words: &[&str]) -> Vec<String> {
		words.iter().map(|word| (*word).to_string()).collect()
	}

	/// A request with everything defaulted except the two paths.
	fn request(backup: &Path, into: &Path) -> Request {
		Request { backup: backup.to_path_buf(), into: into.to_path_buf(), adopt: false, database: None, subject: None, json: false }
	}

	#[test]
	fn a_bare_restore_word_is_a_question() {
		assert_eq!(parse(argv(&[]).into_iter()), Ok(Invocation::Help), "`weft-server restore` alone asks what it takes");
		assert_eq!(parse(argv(&["--help"]).into_iter()), Ok(Invocation::Help));
		assert_eq!(parse(argv(&["-h"]).into_iter()), Ok(Invocation::Help));
		assert_eq!(parse(argv(&["/backups/nightly", "--json", "-h"]).into_iter()), Ok(Invocation::Help), "--help wins wherever it sits");
	}

	#[test]
	fn a_full_request_parses() {
		let parsed = parse(argv(&["/backups/nightly", "--into", "/srv/weft", "--adopt", "--database", "prod", "--subject", "line-3", "--json"]).into_iter());
		assert_eq!(parsed, Ok(Invocation::Restore(Request { backup: PathBuf::from("/backups/nightly"), into: PathBuf::from("/srv/weft"), adopt: true, database: Some("prod".into()), subject: Some("line-3".into()), json: true })));
	}

	#[test]
	fn the_short_destination_spelling_is_the_same_request() {
		assert_eq!(parse(argv(&["b", "-i", "i"]).into_iter()), parse(argv(&["b", "--into", "i"]).into_iter()));
	}

	#[test]
	fn the_short_scope_spellings_are_what_the_help_advertises() {
		let long = parse(argv(&["b", "-i", "i", "--adopt", "--database", "prod", "--subject", "line-3"]).into_iter());
		let short = parse(argv(&["b", "-i", "i", "--adopt", "-d", "prod", "-s", "line-3"]).into_iter());
		assert_eq!(short, long, "the help prints `-d, --database` and `-s, --subject`, so both must parse");
		let Invocation::Restore(request) = short.expect("the short spelling parses") else {
			panic!("a scope flag is not a help request");
		};
		assert_eq!(request.database.as_deref(), Some("prod"));
		assert_eq!(request.subject.as_deref(), Some("line-3"));
		assert!(request.adopt, "a scope flag still needs --adopt, which this command line gives it");
	}

	#[test]
	fn a_missing_backup_directory_names_what_is_missing() {
		let err = parse(argv(&["--into", "/srv/weft"]).into_iter()).expect_err("a restore needs a backup to read");
		assert_eq!(err.exit_code, CONFIG_EXIT_CODE);
		assert!(err.message.contains("no backup directory"), "got: {err}");
	}

	#[test]
	fn a_missing_destination_is_refused_because_nothing_is_ever_overwritten() {
		let err = parse(argv(&["/backups/nightly"]).into_iter()).expect_err("a restore needs somewhere to publish");
		assert_eq!(err.exit_code, CONFIG_EXIT_CODE);
		assert!(err.message.contains("--into <NEW_ROOT> is required"), "got: {err}");
		assert!(err.message.contains("never overwrites"), "the refusal should say why: {err}");
	}

	#[test]
	fn a_second_positional_is_refused_rather_than_ignored() {
		let err = parse(argv(&["/backups/nightly", "/backups/older", "--into", "/srv/weft"]).into_iter()).expect_err("there is no two-directory restore");
		assert_eq!(err.exit_code, CONFIG_EXIT_CODE);
		assert!(err.message.contains("second one"), "got: {err}");
	}

	#[test]
	fn an_unknown_option_names_itself() {
		let err = parse(argv(&["--restor", "x"]).into_iter()).expect_err("`--restor` is a typo, not a backup directory");
		assert_eq!(err.exit_code, CONFIG_EXIT_CODE);
		assert!(err.message.contains("--restor"), "got: {err}");
		assert!(err.message.contains("--help"), "the refusal should point at the help: {err}");
	}

	#[test]
	fn an_option_without_its_value_is_refused() {
		for flag in ["--into", "--database", "--subject"] {
			let err = parse(argv(&["/backups/nightly", flag]).into_iter()).expect_err(&format!("{flag} needs a value"));
			assert_eq!(err.exit_code, CONFIG_EXIT_CODE, "{flag}");
			assert!(err.message.contains(flag), "{flag}: got {err}");
		}
	}

	#[test]
	fn a_scope_flag_without_adopt_is_refused() {
		for flag in ["--database prod", "--subject line-3"] {
			let mut words = vec!["/backups/nightly", "--into", "/srv/weft"];
			words.extend(flag.split(' '));
			let err = parse(argv(&words).into_iter()).expect_err("a scope with nothing to open it is a typo");
			assert_eq!(err.exit_code, CONFIG_EXIT_CODE, "{flag}");
			assert!(err.message.contains("--adopt"), "{flag}: got {err}");
		}
	}

	#[test]
	fn a_repeated_destination_is_refused() {
		let err = parse(argv(&["b", "--into", "/a", "-i", "/b"]).into_iter()).expect_err("one destination per restore");
		assert_eq!(err.exit_code, CONFIG_EXIT_CODE);
		assert!(err.message.contains("twice"), "got: {err}");
	}

	#[test]
	fn the_clock_stamp_is_utc_with_milliseconds() {
		assert_eq!(utc_millis(0), "1970-01-01T00:00:00.000Z");
		assert_eq!(utc_millis(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
	}

	#[test]
	fn the_json_report_carries_the_commands_own_key_names() {
		let report = Report { mode: "control-plane", backup: PathBuf::from("/backups/nightly"), root: PathBuf::from("/srv/weft"), intent: None, frames: 0, rows: 7, bytes: 4096, files: vec![RestoredFile { name: "catalog.db".into(), tables: 2, rows: 7, bytes: 4096 }], adoption: None };
		let value = serde_json::to_value(&report).expect("a report serializes");
		for key in ["mode", "backup", "root", "frames", "rows", "bytes", "files"] {
			assert!(value.get(key).is_some(), "the JSON document lost {key}: {value}");
		}
		assert!(value.get("intent").is_none(), "a control-plane report says nothing about an intent it never published");
		assert!(value.get("adoption").is_none(), "no --adopt, no adoption section");
		assert_eq!(value["files"][0]["name"], "catalog.db");
		assert_eq!(value["files"][0]["tables"], 2);
	}

	#[tokio::test]
	async fn an_existing_destination_is_refused_before_the_backup_is_read() {
		let dir = tempfile::TempDir::new().expect("tempdir");
		let into = dir.path().join("live");
		std::fs::create_dir(&into).expect("create");
		std::fs::write(into.join("keep.txt"), b"untouched").expect("write");
		let err = execute(&request(&dir.path().join("backup"), &into)).await.expect_err("a restore never publishes into a root that exists");
		assert_eq!(err.exit_code, CONFIG_EXIT_CODE);
		assert!(err.message.contains("must not exist"), "got: {err}");
		assert_eq!(std::fs::read_dir(&into).expect("lists").count(), 1, "the destination was not written to");
	}

	#[tokio::test]
	async fn an_empty_directory_is_not_a_backup() {
		let dir = tempfile::TempDir::new().expect("tempdir");
		let backup = dir.path().join("backup-2026-11-01");
		std::fs::create_dir(&backup).expect("create");
		let err = execute(&request(&backup, &dir.path().join("fresh"))).await.expect_err("nothing to restore");
		assert_eq!(err.exit_code, STORE_EXIT_CODE);
		assert!(err.message.contains("MANIFEST.json"), "got: {err}");
		assert!(!dir.path().join("fresh").exists(), "a refused restore publishes nothing");
	}

	#[tokio::test]
	async fn a_staging_directory_is_refused_as_an_unfinished_backup() {
		let dir = tempfile::TempDir::new().expect("tempdir");
		let staging = dir.path().join(".partial-backup-2026-11-01T00-00-00Z");
		std::fs::create_dir(&staging).expect("create");
		let err = execute(&request(&staging, &dir.path().join("fresh"))).await.expect_err("a staging directory is not a backup");
		assert_eq!(err.exit_code, STORE_EXIT_CODE);
		assert!(err.message.contains("never finished"), "got: {err}");
		assert!(!dir.path().join("fresh").exists(), "a refused restore publishes nothing");
	}

	#[tokio::test]
	async fn a_pre_manifest_backup_is_refused_by_name() {
		let dir = tempfile::TempDir::new().expect("tempdir");
		let backup = dir.path().join("backup-2026-09-01");
		std::fs::create_dir(&backup).expect("create");
		for name in weftdb::CONTROL_PLANE_FILES {
			std::fs::write(backup.join(name), []).expect("write");
		}
		let err = execute(&request(&backup, &dir.path().join("fresh"))).await.expect_err("a pre-manifest backup cannot be verified");
		assert_eq!(err.exit_code, STORE_EXIT_CODE);
		assert!(err.message.contains("before `MANIFEST.json`"), "got: {err}");
		assert!(!dir.path().join("fresh").exists(), "a refused restore publishes nothing");
	}

	#[tokio::test]
	async fn a_manifest_this_build_cannot_read_is_refused() {
		let dir = tempfile::TempDir::new().expect("tempdir");
		let backup = dir.path().join("backup-2026-11-01");
		std::fs::create_dir(&backup).expect("create");
		std::fs::write(backup.join(weftdb::BACKUP_MANIFEST), b"{\"format\":\"weft-backup/99\"}").expect("write");
		let err = execute(&request(&backup, &dir.path().join("fresh"))).await.expect_err("an unknown manifest format is not a restore");
		assert_eq!(err.exit_code, STORE_EXIT_CODE);
		assert!(!err.message.is_empty(), "the operator gets the library's reason");
		assert!(!dir.path().join("fresh").exists(), "a refused restore publishes nothing");
	}
}
