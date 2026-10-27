//! `weft-server import-rows-mode` — read a 0.1.x data directory into a Storage v2 store.
//!
//! FRE-14c's operator route. Before it the only way to move a 0.1.x deployment's rows was
//! the library call `weftdb::rows_mode::import_into`, which meant writing a program, and
//! the 0.1.x-to-1.0 story in the README stopped at "the readable leg exists". This is the
//! same call behind one command, so an operator migrates with the binary they already ship.
//!
//! Two decisions are baked into the route, and both come from what `import_into` can and
//! cannot promise:
//!
//! * **The store's scope comes from the tree, not from a flag.** 0.1.x named a database by
//!   its top directory, and [`ScopeMismatch`](weftdb::rows_mode::RowsModeError::ScopeMismatch)
//!   refuses an import whose tree and store disagree. Guessing the scope from
//!   `WEFT_STORE_DATABASE` would have made the common case — a migrated tree, whose
//!   database is its directory name — refuse on every run, so the command opens the store
//!   under the database the tree names and the subject `--subject` names, and prints the
//!   two variables the server needs to serve what it just wrote.
//! * **One subject per run, and a store root per subject.** A v1 server opens one scope
//!   ([`crate::store_open::store_scope_from_env`]), so a tree holding several subjects
//!   migrates as several runs, each into the root that subject's server will open. The
//!   command says so rather than writing a tree's subjects into one root that no server can
//!   read in full.
//!
//! The import is not quiet about what it cannot carry: the tree's queue, pattern stores and
//! compression dictionaries stay behind, and every one of them is named with a reason in the
//! record `import_into` leaves in the store and in this command's own output.

use std::{num::NonZeroUsize, path::PathBuf};

use weftdb::rows_mode::{self, ImportRecord, TIMESTAMP_UNIT};

use crate::store_open;

/// The first argv word that selects this command instead of serving.
pub const SUBCOMMAND: &str = "import-rows-mode";

/// The rows-per-segment the import seals with when `--rows-per-segment` is absent:
/// `weftdb`'s own batch size, so the CLI and the library default cannot drift apart.
pub const DEFAULT_ROWS_PER_SEGMENT: usize = rows_mode::DEFAULT_ROWS_PER_SEGMENT;

/// What the command accepts, printed by `--help` and by every usage error.
pub const USAGE: &str = "\
weft-server import-rows-mode <TREE> --subject <SUBJECT> [OPTIONS]

Read one 0.1.x subject's rows out of a rows-mode data directory (the layout 0.1.x
wrote: `metadata.db` at the top, `<subject>/<aspect>/measurements.db` below it) and
seal them into a Storage v2 store. The store takes the tree's database name and the
subject you name, so the migrated tree reads back under its own name.

A 0.1.x database's queue, pattern stores and compression dictionaries stay behind:
each one is listed, with its reason, in the run's output and in the record the store
keeps. A tree is read once — the second run refuses rather than doubling the rows.

  <TREE>                        the 0.1.x database directory to read
  -s, --subject <SUBJECT>       the 0.1.x subject directory to read; required, because a
                                v1 server opens one subject at a time
  -d, --database <DATABASE>     the database to write (default: the tree's directory name)
      --store <DIR>             the store directory to write into
                                (default: $WEFT_SEGMENT_STORE_ROOT)
  -n, --rows-per-segment <N>    rows per sealed segment (default: 10000)
      --json                    print the import record as JSON instead of the summary
  -h, --help                    print this help

The store directory is $WEFT_SEGMENT_STORE_ROOT unless --store names it. One subject
per run: a tree of several subjects migrates as several runs, each into the store root
that subject's server opens.";

/// What one run was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
	/// The 0.1.x database directory to read.
	pub tree: PathBuf,
	/// The 0.1.x subject directory whose aspects this run reads.
	pub subject: String,
	/// The database to write; [`None`] takes the tree's directory name.
	pub database: Option<String>,
	/// The store root to write; [`None`] takes `$WEFT_SEGMENT_STORE_ROOT`.
	pub store: Option<PathBuf>,
	/// Rows per sealed segment.
	pub rows_per_segment: NonZeroUsize,
	/// Print the record as JSON rather than the line summary.
	pub json: bool,
}

/// What the process was asked for: the server, one import, or the help text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
	/// No subcommand: serve, exactly as `weft-server` always has.
	Serve,
	/// `--help` before any subcommand: print [`USAGE`] and exit successfully.
	Help,
	/// Read a tree into a store.
	Import(Request),
}

/// Read `argv` (after the program name) into an [`Invocation`].
///
/// A first word that is neither the subcommand nor a help flag is an error rather than a
/// reason to start serving: the server takes no other arguments, and a supervisor that
/// typos `import-rowsmode` must not come up as a server instead of migrating.
///
/// # Errors
///
/// A one-line usage message naming what was wrong, for an unknown command, an unknown
/// option, a missing value, a missing `<TREE>` or `--subject`, a zero `--rows-per-segment`,
/// or a subject or database name that could not be a scope name.
pub fn parse<I: IntoIterator<Item = String>>(arguments: I) -> Result<Invocation, String> {
	let mut command = None::<String>;
	let mut tree = None::<PathBuf>;
	let mut subject = None::<String>;
	let mut database = None::<String>;
	let mut store = None::<PathBuf>;
	let mut rows_per_segment = None::<NonZeroUsize>;
	let mut json = false;
	let mut help = false;

	let mut arguments = arguments.into_iter().peekable();
	if arguments.peek().is_none() {
		return Ok(Invocation::Serve);
	}

	while let Some(argument) = arguments.next() {
		if argument == "-h" || argument == "--help" {
			help = true;
			continue;
		}
		if command.is_none() && !argument.starts_with('-') {
			// The first word names the command, and there is one. Refusing it here rather
			// than at the end keeps the message about the word the operator actually typed:
			// `weft-server plant` must not read as "import plant", and a misspelled
			// `import-rowsmode` must not come up as a server instead of migrating.
			if argument != SUBCOMMAND {
				return Err(format!("there is no `{argument}` command\n\n{USAGE}"));
			}
			command = Some(argument);
			continue;
		}
		if command.is_none() {
			return Err(format!("`{argument}` needs a command: `weft-server` with no arguments serves\n\n{USAGE}"));
		}
		match argument.as_str() {
			"--json" => json = true,
			value if value == "-s" || value == "--subject" => subject = Some(value_of(&mut arguments, value)?),
			value if value == "-d" || value == "--database" => database = Some(value_of(&mut arguments, value)?),
			value if value == "--store" => store = Some(PathBuf::from(value_of(&mut arguments, value)?)),
			value if value == "-n" || value == "--rows-per-segment" => {
				let given = value_of(&mut arguments, value)?;
				rows_per_segment = Some(given.parse::<usize>().ok().and_then(NonZeroUsize::new).ok_or_else(|| format!("`{value}` wants a positive whole number of rows, not {given:?}"))?);
			}
			other if other.starts_with('-') => return Err(format!("unknown option `{other}`\n\n{USAGE}")),
			other => {
				if tree.is_some() {
					return Err(format!("there is a second path `{other}`; this command reads one tree\n\n{USAGE}"));
				}
				tree = Some(PathBuf::from(other));
			}
		}
	}

	if command.is_none() {
		return if help { Ok(Invocation::Help) } else { Err(format!("there is no command to run; `weft-server` with no arguments serves\n\n{USAGE}")) };
	}
	let Some(tree) = tree else {
		if help {
			return Ok(Invocation::Help);
		}
		return Err(format!("{SUBCOMMAND} needs the 0.1.x directory to read\n\n{USAGE}"));
	};
	let subject = subject.filter(|value| !value.trim().is_empty()).ok_or_else(|| format!("{SUBCOMMAND} needs --subject: a v1 server opens one subject at a time\n\n{USAGE}"))?;
	if database.as_deref().is_some_and(|value| value.trim().is_empty()) {
		return Err(format!("--database is set but empty; leave it out to take the tree's directory name\n\n{USAGE}"));
	}

	Ok(Invocation::Import(Request { tree, subject, database, store, rows_per_segment: rows_per_segment.unwrap_or_else(|| NonZeroUsize::new(DEFAULT_ROWS_PER_SEGMENT).unwrap_or(NonZeroUsize::MIN)), json }))
}

/// The value that follows an option, or a message naming the option that lost it.
fn value_of<I: Iterator<Item = String>>(arguments: &mut I, option: &str) -> Result<String, String> {
	arguments.next().filter(|value| !value.starts_with('-')).ok_or_else(|| format!("`{option}` needs a value\n\n{USAGE}"))
}

/// Run one import and report it.
///
/// # Errors
///
/// `$WEFT_SEGMENT_STORE_ROOT` unset and no `--store` given, or whatever
/// [`discover`](rows_mode::discover), the store's open, or
/// [`import_into`](rows_mode::import_into) reports — including the refusals that keep a
/// tree from being read twice into one store.
pub async fn import(request: &Request) -> anyhow::Result<ImportRecord> {
	let root = request.store.clone().map_or_else(store_open::store_root_from_env, Ok)?;
	// Read the tree first: it names the database, and `import_into` re-reads it per aspect
	// anyway, so this pass costs one catalog walk and buys the refusal below.
	let tree = rows_mode::discover(&request.tree).await?;
	let database = request.database.clone().unwrap_or_else(|| tree.database.clone());
	if tree.aspects.iter().all(|aspect| aspect.subject != request.subject) {
		// `import_into` would say the same thing about the missing `measurements.db`; saying
		// it here names the subjects the tree does hold, which is what an operator needs
		// after pointing at the wrong directory.
		let mut subjects = tree.subjects();
		subjects.sort();
		anyhow::bail!("{} holds no subject {:?}; it holds {}", request.tree.display(), request.subject, if subjects.is_empty() { "no subject at all".to_string() } else { subjects.join(", ") });
	}
	let store = store_open::open_scoped_with_env_options(&root, &database, &request.subject).await?;
	Ok(rows_mode::import_into(&store, &request.tree, &request.subject, request.rows_per_segment).await?)
}

/// The command's whole job: parse, import, report.
///
/// # Errors
///
/// Any parse or [`import`] failure, which the caller prints and exits non-zero on.
pub async fn run<I: IntoIterator<Item = String>>(arguments: I) -> anyhow::Result<()> {
	let invocation = parse(arguments).map_err(anyhow::Error::msg)?;
	match invocation {
		Invocation::Serve => Ok(()),
		Invocation::Help => {
			println!("{USAGE}");
			Ok(())
		}
		Invocation::Import(request) => {
			let record = import(&request).await?;
			if request.json {
				println!("{}", serde_json::to_string_pretty(&record)?);
			} else {
				print!("{}", summary(&record));
			}
			Ok(())
		}
	}
}

/// The operator's summary of a finished import: what moved, what stayed, and how to serve
/// what is now in the store.
#[must_use]
pub fn summary(record: &ImportRecord) -> String {
	use std::fmt::Write as _;
	let mut out = String::new();
	let _ = writeln!(out, "read {} as 0.1.x database {database:?}, subject {subject:?} ({TIMESTAMP_UNIT})", record.source, database = record.database, subject = record.subject);
	for aspect in &record.aspects {
		let span = match (aspect.first_timestamp, aspect.last_timestamp) {
			(Some(first), Some(last)) => format!("{first}..{last} {TIMESTAMP_UNIT}"),
			_ => "empty".to_string(),
		};
		let duplicates = if aspect.duplicate_timestamps == 0 { String::new() } else { format!(", {} sharing a timestamp", aspect.duplicate_timestamps) };
		let _ = writeln!(out, "  {aspect}: {rows} row(s) into {segments} segment(s) in {encoding}, {span}{duplicates}", aspect = aspect.aspect, rows = aspect.rows, segments = aspect.segments, encoding = aspect.encoding);
	}
	// One 0.1.x subject directory refuses the same kind of file seven times over, and a
	// tree of three subjects therefore printed twenty-odd lines that all end in the same
	// sentence. That buries the two lines an operator acts on, so a reason is printed once
	// with the paths it covers. The record the store keeps, and `--json`, stay complete.
	let mut grouped: Vec<(&str, Vec<&str>)> = Vec::new();
	for stayer in &record.not_carried {
		match grouped.iter_mut().find(|(reason, _)| *reason == stayer.reason) {
			Some((_, paths)) => paths.push(stayer.path.as_str()),
			None => grouped.push((stayer.reason.as_str(), vec![stayer.path.as_str()])),
		}
	}
	for (because, paths) in grouped {
		if paths.len() == 1 {
			let _ = writeln!(out, "  stays behind: {} — {because}", paths[0]);
			continue;
		}
		// The reason is the half an operator reads; a list of twenty paths is only the
		// evidence, so it goes under it, capped, with `--json` named for the rest.
		let _ = writeln!(out, "  stays behind {} file(s): {because}", paths.len());
		let shown = paths.iter().take(5).copied().collect::<Vec<_>>().join(", ");
		let _ = if paths.len() > 5 { writeln!(out, "      {shown}, and {} more (`--json` lists every one)", paths.len() - 5) } else { writeln!(out, "      {shown}") };
	}
	let _ = writeln!(out, "imported {} row(s) into {} segment(s) at {}", record.rows(), record.segments(), record.imported_at);
	let _ = writeln!(out, "serve it with WEFT_SEGMENT_STORE_ROOT set and WEFT_STORE_DATABASE={database} WEFT_STORE_SUBJECT={subject}", database = record.database, subject = record.subject);
	out
}

#[cfg(test)]
mod tests {
	use weftdb::rows_mode::{ImportedAspect, NotCarried};

	use super::*;

	#[test]
	fn no_arguments_serves() {
		assert_eq!(parse(std::iter::empty()), Ok(Invocation::Serve));
	}

	#[test]
	fn the_subcommand_and_its_minimum_are_a_request() {
		let parsed = parse(["import-rows-mode".to_string(), "/var/lib/legacy/v010_legacy".to_string(), "--subject".to_string(), "plant".to_string()]).expect("the minimum invocation parses");
		let Invocation::Import(request) = parsed else { panic!("expected an import, got {parsed:?}") };
		assert_eq!(request.tree, PathBuf::from("/var/lib/legacy/v010_legacy"));
		assert_eq!(request.subject, "plant");
		assert_eq!(request.database, None, "the tree's own name is the default database");
		assert_eq!(request.store, None, "$WEFT_SEGMENT_STORE_ROOT is the default store");
		assert_eq!(request.rows_per_segment.get(), DEFAULT_ROWS_PER_SEGMENT);
		assert!(!request.json);
	}

	#[test]
	fn every_option_has_a_long_and_short_form_that_lands_on_the_request() {
		let Invocation::Import(request) = parse(["import-rows-mode".to_string(), "tree".to_string(), "-s".to_string(), "plant".to_string(), "-d".to_string(), "v010_legacy".to_string(), "--store".to_string(), "/var/lib/weft".to_string(), "-n".to_string(), "500".to_string(), "--json".to_string()]).expect("a full invocation parses") else { panic!("expected an import") };
		assert_eq!(request.subject, "plant");
		assert_eq!(request.database.as_deref(), Some("v010_legacy"));
		assert_eq!(request.store, Some(PathBuf::from("/var/lib/weft")));
		assert_eq!(request.rows_per_segment.get(), 500);
		assert!(request.json);
	}

	#[test]
	fn a_subject_is_required_because_a_server_opens_one_at_a_time() {
		let error = parse(["import-rows-mode".to_string(), "tree".to_string()]).expect_err("no subject is not a request");
		assert!(error.contains("--subject"), "{error}");
		assert!(error.contains(USAGE), "a usage error carries the usage: {error}");
	}

	#[test]
	fn a_blank_subject_is_no_subject() {
		let error = parse(["import-rows-mode".to_string(), "tree".to_string(), "--subject".to_string(), "  ".to_string()]).expect_err("a blank scope name is not a subject");
		assert!(error.contains("--subject"), "{error}");
	}

	#[test]
	fn the_tree_is_required() {
		let error = parse(["import-rows-mode".to_string(), "--subject".to_string(), "plant".to_string()]).expect_err("no tree is not a request");
		assert!(error.contains("needs the 0.1.x directory"), "{error}");
	}

	#[test]
	fn two_trees_are_one_too_many() {
		let error = parse(["import-rows-mode".to_string(), "one".to_string(), "two".to_string(), "--subject".to_string(), "plant".to_string()]).expect_err("a second path is not a second subject");
		assert!(error.contains("second path"), "{error}");
	}

	#[test]
	fn a_zero_row_segment_count_is_refused_not_defaulted() {
		let error = parse(["import-rows-mode".to_string(), "tree".to_string(), "--subject".to_string(), "plant".to_string(), "--rows-per-segment".to_string(), "0".to_string()]).expect_err("zero rows per segment would seal nothing");
		assert!(error.contains("positive whole number"), "{error}");
		assert!(parse(["import-rows-mode".to_string(), "tree".to_string(), "--subject".to_string(), "plant".to_string(), "--rows-per-segment".to_string(), "not-a-number".to_string()]).is_err());
	}

	#[test]
	fn an_option_that_lost_its_value_names_itself() {
		let error = parse(["import-rows-mode".to_string(), "tree".to_string(), "--subject".to_string()]).expect_err("a dangling option is not a request");
		assert!(error.contains("--subject"), "{error}");
		let error = parse(["import-rows-mode".to_string(), "tree".to_string(), "--subject".to_string(), "--store".to_string(), "/x".to_string()]).expect_err("an option is not a subject");
		assert!(error.contains("--subject"), "{error}");
	}

	#[test]
	fn an_unknown_option_is_not_ignored() {
		let error = parse(["import-rows-mode".to_string(), "tree".to_string(), "--subject".to_string(), "plant".to_string(), "--subject2".to_string(), "x".to_string()]).expect_err("an unknown option must not silently take its value as a positional");
		assert!(error.contains("unknown option"), "{error}");
	}

	#[test]
	fn another_command_is_refused_rather_than_served() {
		// A supervisor that typos the subcommand must not come up as a server instead of
		// migrating; the server takes no other arguments, so nothing legitimate is refused.
		let error = parse(["import-rowsmode".to_string(), "tree".to_string()]).expect_err("a near-miss command is not serving");
		assert!(error.contains("there is no `import-rowsmode` command"), "{error}");
	}

	#[test]
	fn help_before_a_command_prints_the_usage() {
		assert_eq!(parse(["--help".to_string()]), Ok(Invocation::Help));
		assert_eq!(parse(["-h".to_string()]), Ok(Invocation::Help));
	}

	#[test]
	fn an_empty_database_falls_back_to_the_tree_rather_than_an_empty_scope() {
		let error = parse(["import-rows-mode".to_string(), "tree".to_string(), "--subject".to_string(), "plant".to_string(), "--database".to_string(), "".to_string()]).expect_err("an empty database would open a scope nothing reads");
		assert!(error.contains("--database"), "{error}");
	}

	#[test]
	fn the_summary_names_everything_the_operator_has_to_know() {
		let record = ImportRecord { source: "/var/lib/legacy/v010_legacy".to_string(), database: "v010_legacy".to_string(), subject: "plant".to_string(), timestamp_unit: TIMESTAMP_UNIT.to_string(), rows_per_segment: 10_000, aspects: vec![ImportedAspect { aspect: "price".to_string(), rows: 12_345, segments: 2, encoding: "ScaledI64".to_string(), first_timestamp: Some(1_577_836_800_000), last_timestamp: Some(1_577_923_200_000), duplicate_timestamps: 3 }], not_carried: vec![NotCarried { path: "plant/price/dictionaries".to_string(), reason: "compression dictionaries are rebuilt by v1's seal".to_string() }], imported_at: "2026-10-27T00:00:00Z".to_string() };
		let summary = summary(&record);
		assert!(summary.contains("read /var/lib/legacy/v010_legacy as 0.1.x database \"v010_legacy\", subject \"plant\" (millis)"), "{summary}");
		assert!(summary.contains("price: 12345 row(s) into 2 segment(s) in ScaledI64"), "{summary}");
		assert!(summary.contains("1577836800000..1577923200000 millis"), "{summary}");
		assert!(summary.contains("3 sharing a timestamp"), "{summary}");
		assert!(summary.contains("stays behind: plant/price/dictionaries — compression dictionaries are rebuilt by v1's seal"), "{summary}");
		assert!(summary.contains("WEFT_STORE_DATABASE=v010_legacy WEFT_STORE_SUBJECT=plant"), "the summary has to say how to serve what it wrote: {summary}");
	}

	#[test]
	fn an_empty_series_is_reported_as_empty_rather_than_at_the_epoch() {
		let record = ImportRecord { source: "tree".to_string(), database: "d".to_string(), subject: "s".to_string(), timestamp_unit: TIMESTAMP_UNIT.to_string(), rows_per_segment: 1, aspects: vec![ImportedAspect { aspect: "quiet".to_string(), rows: 0, segments: 0, encoding: "F64".to_string(), first_timestamp: None, last_timestamp: None, duplicate_timestamps: 0 }], not_carried: vec![], imported_at: "2026-10-27T00:00:00Z".to_string() };
		assert!(summary(&record).contains("quiet: 0 row(s) into 0 segment(s) in F64, empty"), "{}", summary(&record));
	}
}
