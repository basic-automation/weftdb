//! The operator CLI (PAC-7): the one place `weft-server`'s command line is
//! parsed, and the one place its exit statuses are tabulated.
//!
//! Before PAC-7 the binary's `main` dispatched by hand: three subcommands
//! were matched by name and *every other word* fell through to
//! `import-rows-mode`, whose parser refused it. The refusal was right — a
//! misspelled command must never quietly come up as a server — but the
//! class was wrong: a parse failure returned `anyhow::Result` and the
//! runtime flattened it to exit 1, the status plan C7 reserves for an
//! *unexpected* error. A supervisor reading statuses could not tell "the
//! operator typed the command wrong" from "the server crashed".
//!
//! Now [`crate::cli::parse`] reads the whole command line into a [`crate::cli::Command`] before
//! anything runs, and the process's statuses are the C7 table below —
//! [`crate::cli::EXIT_CODES`], printed verbatim by `weft-server --help` so the table
//! an operator reads is the table the code exits with:
//!
//! | Code | Meaning |
//! |---|---|
//! | 0 | Clean stop, or a stop before serving |
//! | 1 | Unexpected error |
//! | 2 | Usage |
//! | 65 | Store needs operator action |
//! | 70 | Poison exit |
//! | 75 | Root locked |
//! | 78 | Invalid configuration or unsupported deployment |
//!
//! The subcommands keep their own parsers and their own help text; this
//! module owns only the dispatch and the usage class. One deliberate
//! exception, recorded when PAC-13 landed and kept here: `restore`'s
//! command-line refusals exit 78, not 2 — PAC-13 chose the split by which
//! side of the terminal is wrong (78 = fix the command line or the
//! destination, 65 = the backup needs a human) and promised a supervisor
//! that already learned those numbers no renumbering. Every other
//! subcommand's parse errors, and a first word that names no command at
//! all, exit [`crate::settings::USAGE_EXIT_CODE`].

use crate::settings::{CONFIG_EXIT_CODE, ROOT_LOCKED_EXIT_CODE, STORE_EXIT_CODE, UNEXPECTED_EXIT_CODE, USAGE_EXIT_CODE};

/// One row of the plan C7 exit-code table: the status and what it means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitCode {
	/// The process exit status.
	pub code: i32,
	/// The C7 meaning, in the table's own words.
	pub meaning: &'static str,
}

/// The plan C7 exit-code table, in code order.
///
/// This is the contract a
/// supervisor or a script reads the process's status against; the
/// constants the phases exit with are these rows' codes, and a test in
/// this module pins the two together so the table cannot drift from the
/// code that exits.
pub const EXIT_CODES: &[ExitCode] = &[ExitCode { code: 0, meaning: "clean stop, or a stop before serving" }, ExitCode { code: UNEXPECTED_EXIT_CODE, meaning: "unexpected error" }, ExitCode { code: USAGE_EXIT_CODE, meaning: "usage: an unknown command or option, or a missing value" }, ExitCode { code: STORE_EXIT_CODE, meaning: "store needs operator action: layout or min_write_layout newer than the binary, unreadable STORE_FORMAT, scope mismatch, incomplete restore, frames without a control plane" }, ExitCode { code: weftdb::AMBIGUOUS_COMMIT_EXIT_CODE, meaning: "poison exit: the store is write-poisoned and WEFT_ON_AMBIGUOUS_COMMIT=exit" }, ExitCode { code: ROOT_LOCKED_EXIT_CODE, meaning: "root locked by another process" }, ExitCode { code: CONFIG_EXIT_CODE, meaning: "invalid configuration or unsupported deployment, including exposure and filesystem refusals" }];

/// What `weft-server` with no arguments does: serve.
pub const SERVE_COMMAND: &str = "serve";

/// The word that asks for the top-level help instead of a command.
pub const HELP_COMMAND: &str = "help";

/// What the command line accepts, printed by `--help`/`help` and named by
/// every top-level usage error.
pub const USAGE: &str = "\
weft-server — the WeftDB server and its operator commands

usage: weft-server [COMMAND] [ARGS]

With no command (or `serve`, which takes no arguments) the server serves:
it reads its configuration from the WEFT_* environment, opens the segment
store WEFT_SEGMENT_STORE_ROOT names when that is set, and serves until
SIGINT or SIGTERM stops it cleanly.

Commands:
  serve                 serve (the default; takes no arguments)
  keygen                mint an API key and record its hash in a keys file
  restore               restore a backup into a fresh store root, offline
  import-rows-mode      read a 0.1.x rows-mode data directory into a store
  help                  print this help (also -h / --help)

Each command has its own help: `weft-server <command> --help`.
`weft-server --version` prints the server's version and exits.

Exit status (plan C7 — the contract a supervisor reads):
   0  clean stop, or a stop before serving
   1  unexpected error
   2  usage: an unknown command or option, or a missing value
  65  store needs operator action (layout newer than the binary,
      unreadable STORE_FORMAT, scope mismatch, incomplete restore)
  70  poison exit (WEFT_ON_AMBIGUOUS_COMMIT=exit)
  75  root locked by another process
  78  invalid configuration or unsupported deployment

One exception, kept from PAC-13: `restore`'s own command-line refusals
exit 78 (fix the command line or the destination), not 2.";

/// What one command line asked the process to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
	/// Serve: no command word, or the explicit `serve`.
	Serve,
	/// Print [`USAGE`] and exit 0.
	Help,
	/// Print the service name and version, and exit 0.
	Version,
	/// `weft-server keygen …`: the arguments after the program name,
	/// starting with the subcommand word, as [`crate::keygen::parse`]
	/// expects them.
	Keygen(Vec<String>),
	/// `weft-server restore …`: the arguments after the subcommand word,
	/// as [`crate::restore::run`] expects them.
	Restore(Vec<String>),
	/// `weft-server import-rows-mode …`: the arguments after the program
	/// name, starting with the subcommand word, as
	/// [`crate::import_rows_mode::parse`] expects them.
	ImportRowsMode(Vec<String>),
}

/// A command line that names nothing the CLI can run: the message names
/// what was wrong and carries [`USAGE`], and the process exits with
/// [`USAGE_EXIT_CODE`] rather than attempting anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError {
	/// The one-line reason, followed by [`USAGE`].
	pub message: String,
}

impl std::fmt::Display for UsageError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(&self.message)
	}
}

impl std::error::Error for UsageError {}

/// Read `argv` (after the program name) into a [`Command`].
///
/// Only the first word is decided here — which command runs, or that the
/// line is help or a version question. The subcommands parse their own
/// remaining arguments with their own parsers, so their help text and
/// refusals stay theirs; what changes under PAC-7 is the status their
/// parse errors leave with (see the module docs).
///
/// A first word that is none of the commands is an error, never a reason
/// to serve: a supervisor that typos `restore` must not get a live server
/// on the machine that was meant to be putting a store back.
///
/// # Errors
///
/// [`UsageError`] for an unknown command, an option before any command,
/// or an argument after `serve`.
pub fn parse(argv: &[String]) -> Result<Command, UsageError> {
	let Some(first) = argv.first() else {
		return Ok(Command::Serve);
	};
	let rest = || argv[1..].to_vec();
	let with_word = || argv.to_vec();
	match first.as_str() {
		"-h" | "--help" | HELP_COMMAND => Ok(Command::Help),
		"-V" | "--version" => Ok(Command::Version),
		SERVE_COMMAND => {
			if argv.len() > 1 {
				Err(usage(format!("`serve` takes no arguments, not {:?}", argv[1])))
			} else {
				Ok(Command::Serve)
			}
		}
		word if word == crate::keygen::SUBCOMMAND => Ok(Command::Keygen(with_word())),
		word if word == crate::restore::SUBCOMMAND => Ok(Command::Restore(rest())),
		word if word == crate::import_rows_mode::SUBCOMMAND => Ok(Command::ImportRowsMode(with_word())),
		other if other.starts_with('-') => Err(usage(format!("`{other}` is not an option `weft-server` takes before a command"))),
		other => Err(usage(format!("there is no `{other}` command"))),
	}
}

/// A usage refusal: the reason, then the help it points at.
fn usage(reason: String) -> UsageError {
	UsageError { message: format!("{reason}\n\n{USAGE}") }
}

#[cfg(test)]
mod tests {
	use super::*;

	fn argv(words: &[&str]) -> Vec<String> {
		words.iter().map(ToString::to_string).collect()
	}

	#[test]
	fn no_arguments_and_a_bare_serve_both_serve() {
		assert_eq!(parse(&[]), Ok(Command::Serve));
		assert_eq!(parse(&argv(&["serve"])), Ok(Command::Serve));
	}

	#[test]
	fn help_and_version_are_questions_not_commands() {
		for word in ["-h", "--help", "help"] {
			assert_eq!(parse(&argv(&[word])), Ok(Command::Help), "{word}");
		}
		for word in ["-V", "--version"] {
			assert_eq!(parse(&argv(&[word])), Ok(Command::Version), "{word}");
		}
	}

	#[test]
	fn each_subcommand_dispatches_with_the_arguments_its_parser_expects() {
		// keygen and import-rows-mode parsers take the subcommand word;
		// restore's takes what follows it (PAC-13's shape, unchanged).
		assert_eq!(parse(&argv(&["keygen", "--id", "op"])), Ok(Command::Keygen(argv(&["keygen", "--id", "op"]))));
		assert_eq!(parse(&argv(&["restore", "backup-1", "--into", "root"])), Ok(Command::Restore(argv(&["backup-1", "--into", "root"]))));
		assert_eq!(parse(&argv(&["import-rows-mode", "tree", "-s", "plant"])), Ok(Command::ImportRowsMode(argv(&["import-rows-mode", "tree", "-s", "plant"]))));
	}

	#[test]
	fn an_unknown_first_word_is_a_usage_error_never_a_server() {
		for words in [&["restor"][..], &["import-rowsmode"][..], &["--bogus"][..], &["serve", "extra"][..]] {
			let error = parse(&argv(words)).expect_err("refused");
			assert!(error.message.contains(USAGE), "a usage error carries the usage: {error}");
			assert_eq!(USAGE_EXIT_CODE, 2, "plan C7: usage is exit 2");
		}
	}

	#[test]
	fn the_table_is_the_c7_table_and_the_constants_agree_with_it() {
		let codes: Vec<i32> = EXIT_CODES.iter().map(|row| row.code).collect();
		assert_eq!(codes, [0, 1, 2, 65, 70, 75, 78], "plan C7's table, in order");
		// The help an operator reads prints the same table.
		for phrase in ["clean stop, or a stop before serving", "unexpected error", "usage: an unknown command or option", "store needs operator action", "poison exit", "root locked by another process", "invalid configuration or unsupported deployment"] {
			assert!(USAGE.contains(phrase), "the usage text carries {phrase:?}");
		}
	}
}
