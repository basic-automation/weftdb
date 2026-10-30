//! `weft-server keygen` — mint an API key and its keys-file entry (SEC-6).
//!
//! The keys file ([`crate::auth`]) holds hashes, never keys, so a key
//! cannot be added to it by hand: the operator would have to hash a
//! self-chosen secret correctly, and a self-chosen secret is the weak
//! kind. This command is the one place a plaintext key exists: it mints
//! 32 random bytes ([`crate::auth::generate_api_key`]), prints the key **once**, and
//! writes only its hash — appended to the keys file when `--keys-file`
//! names one (created if absent, at mode `0600` on Unix, because the
//! file it sits beside in a deployment is a credential store even
//! though its own contents are hashes), or printed as the JSON entry
//! to paste when it does not.
//!
//! The command takes no store and opens nothing: it is run before the
//! server starts, against the file the server will mount.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::{
	auth::{generate_api_key, role_from_name, KeyStore}, routes::Role
};

/// The first argv word that selects this command instead of serving.
pub const SUBCOMMAND: &str = "keygen";

/// What the command accepts, printed by `--help` and by every usage error.
pub const USAGE: &str = "\
weft-server keygen [--id <ID>] [--role <ROLE>] [--keys-file <PATH>]

Mint a new WeftDB API key. The key itself is printed ONCE, at the top of the
output: store it where you keep credentials. Only its SHA-256 hash is written
to the keys file, so the file can be mounted and backed up without holding a
working credential.

  --id <ID>             the key's id in the keys file: one word naming the
                        principal (default: operator)
  --role <ROLE>         one of the frozen roles: anonymous, reader, writer,
                        declarer, maintainer, admin (default: admin). A key's
                        role grants its scopes on every aspect alike.
  --keys-file <PATH>    append the new entry to this keys file, creating it
                        (mode 0600) if it does not exist. Without it, the
                        entry is printed as JSON and nothing is written.
  -h, --help            print this help

The server reads the keys file once at startup (WEFT_KEYS_FILE names it), so
restart the server — or wait for key reload (SEC-7) — for a new key to take
effect.";

/// The id a key is minted under when `--id` is absent.
const DEFAULT_ID: &str = "operator";

/// What one run was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
	/// The new key's id in the keys file.
	pub id: String,
	/// The role the new key holds.
	pub role: Role,
	/// The keys file to append to; [`None`] prints the entry instead.
	pub keys_file: Option<PathBuf>,
}

/// What the process was asked for: the help text, or one minting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
	/// `--help`: print [`USAGE`] and exit successfully.
	Help,
	/// Mint one key.
	Generate(Request),
}

/// Read `argv` (starting with the subcommand word, as `main` hands it
/// over) into an [`Invocation`], in the shape of
/// [`crate::import_rows_mode::parse`]: an unknown word or option is an
/// error naming what was wrong, never a silent default.
///
/// # Errors
///
/// A one-line usage message for a missing subcommand word, an unknown
/// option, a missing value, a blank id, or a role outside the frozen
/// six (or `anonymous`: a key that grants nothing is a mistake, as the
/// keys-file loader also holds).
pub fn parse<I: IntoIterator<Item = String>>(arguments: I) -> Result<Invocation, String> {
	let mut arguments = arguments.into_iter().peekable();
	match arguments.next() {
		Some(word) if word == SUBCOMMAND => {}
		Some(word) => return Err(format!("there is no `{word}` command here; this parser serves `{SUBCOMMAND}`\n\n{USAGE}")),
		None => return Err(format!("`{SUBCOMMAND}` needs its subcommand word\n\n{USAGE}")),
	}
	let mut id: Option<String> = None;
	let mut role: Option<Role> = None;
	let mut keys_file: Option<PathBuf> = None;
	let mut help = false;
	while let Some(argument) = arguments.next() {
		match argument.as_str() {
			"-h" | "--help" => help = true,
			"--id" => {
				let value = value_of(&mut arguments, "--id")?;
				if value.trim().is_empty() || value.chars().any(char::is_whitespace) {
					return Err(format!("--id must be one non-blank word naming the principal, not {value:?}\n\n{USAGE}"));
				}
				id = Some(value);
			}
			"--role" => {
				let value = value_of(&mut arguments, "--role")?;
				let parsed = role_from_name(&value).ok_or_else(|| format!("--role {value:?} is not one of the frozen roles (anonymous, reader, writer, declarer, maintainer, admin)\n\n{USAGE}"))?;
				if parsed == Role::Anonymous {
					return Err(format!("--role anonymous would mint a key that grants nothing beyond the public probes; no key is needed for those\n\n{USAGE}"));
				}
				role = Some(parsed);
			}
			"--keys-file" => keys_file = Some(PathBuf::from(value_of(&mut arguments, "--keys-file")?)),
			other if other.starts_with('-') => return Err(format!("unknown option `{other}`\n\n{USAGE}")),
			other => return Err(format!("`{other}` is not an argument {SUBCOMMAND} takes; it mints one key per run\n\n{USAGE}")),
		}
	}
	if help {
		return Ok(Invocation::Help);
	}
	Ok(Invocation::Generate(Request { id: id.unwrap_or_else(|| DEFAULT_ID.to_string()), role: role.unwrap_or(Role::Admin), keys_file }))
}

/// The value that follows an option, or a message naming the option
/// that lost it.
fn value_of<I: Iterator<Item = String>>(arguments: &mut I, option: &str) -> Result<String, String> {
	arguments.next().filter(|value| !value.starts_with('-')).ok_or_else(|| format!("`{option}` needs a value\n\n{USAGE}"))
}

/// One keys-file entry for `request`, carrying `key_hash`: the exact
/// object [`KeyStore::parse`] reads back.
fn entry_json(request: &Request, key_hash: &str) -> Value {
	json!({ "id": request.id, "role": request.role.as_str(), "key_hash": key_hash })
}

/// Append `entry` to the keys file at `path`, creating the document
/// when the file does not exist yet. An existing file must parse as a
/// keys file and must not already hold the entry's id or hash —
/// redefining a principal by appending is exactly what the loader
/// refuses, so the tool refuses it first, with the same message.
///
/// # Errors
///
/// An unreadable or malformed existing file, a duplicate id or hash,
/// or a write that fails.
pub fn append_to_keys_file(path: &std::path::Path, entry: &Value) -> Result<(), String> {
	let id = entry["id"].as_str().unwrap_or_default();
	let mut document = if path.exists() {
		let text = std::fs::read_to_string(path).map_err(|e| format!("{}: cannot read the keys file: {e}", path.display()))?;
		let store = KeyStore::parse(&text)?;
		if store.contains_id(id) {
			return Err(format!("{} already holds a key with id {id:?}; pick another --id rather than redefine a principal", path.display()));
		}
		serde_json::from_str::<Value>(&text).map_err(|e| format!("{}: the keys file is not a keys document: {e}", path.display()))?
	} else {
		json!({ "version": 1, "keys": [] })
	};
	let keys = document["keys"].as_array_mut().ok_or_else(|| format!("{}: the keys file's `keys` is not a list", path.display()))?;
	if keys.iter().any(|known| known["key_hash"] == entry["key_hash"]) {
		return Err("the keys file already holds this key's hash under another id; one key must name one principal".to_string());
	}
	keys.push(entry.clone());
	let rendered = serde_json::to_string_pretty(&document).map_err(|e| format!("cannot render the keys file: {e}"))?;
	write_keys_file(path, &(rendered + "\n"))
}

/// Write the keys file, creating it at mode `0600` on Unix when it is
/// new: the hashes are not credentials, but the file belongs to the
/// credential store and a deployment should not have to chmod it by
/// hand to keep it that way.
fn write_keys_file(path: &std::path::Path, contents: &str) -> Result<(), String> {
	#[cfg(unix)]
	{
		use std::{fs::OpenOptions, io::Write as _, os::unix::fs::OpenOptionsExt as _};
		let mut options = OpenOptions::new();
		options.write(true).create(true).truncate(true);
		if !path.exists() {
			options.mode(0o600);
		}
		let mut file = options.open(path).map_err(|e| format!("{}: cannot write the keys file: {e}", path.display()))?;
		file.write_all(contents.as_bytes()).map_err(|e| format!("{}: cannot write the keys file: {e}", path.display()))?;
		return Ok(());
	}
	#[cfg(not(unix))]
	{
		std::fs::write(path, contents).map_err(|e| format!("{}: cannot write the keys file: {e}", path.display()))
	}
}

/// The command's whole job: parse, mint, record, report. The key is
/// printed first and labelled as shown-once, before anything that
/// could be mistaken for it.
///
/// # Errors
///
/// Any parse failure, a minting failure, or whatever
/// [`append_to_keys_file`] refuses.
pub fn run<I: IntoIterator<Item = String>>(arguments: I) -> anyhow::Result<()> {
	let invocation = parse(arguments).map_err(anyhow::Error::msg)?;
	match invocation {
		Invocation::Help => {
			println!("{USAGE}");
			Ok(())
		}
		Invocation::Generate(request) => {
			let generated = generate_api_key().map_err(anyhow::Error::msg)?;
			let entry = entry_json(&request, &generated.key_hash);
			match &request.keys_file {
				Some(path) => append_to_keys_file(path, &entry).map_err(anyhow::Error::msg)?,
				None => {}
			}
			println!("API key for `{}` ({} role) — shown once, store it now:", request.id, request.role.as_str());
			println!();
			println!("    {}", generated.token);
			println!();
			match &request.keys_file {
				Some(path) => println!("Its hash is recorded in {} — restart the server for the key to take effect.", path.display()),
				None => {
					println!("Add this entry to your keys file (WEFT_KEYS_FILE):");
					println!();
					println!("{}", serde_json::to_string_pretty(&entry)?);
				}
			}
			Ok(())
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::auth::format_key_hash;

	fn parse_ok(arguments: &[&str]) -> Invocation {
		parse(arguments.iter().map(ToString::to_string)).expect("parses")
	}

	#[test]
	fn defaults_are_the_operator_admin_key() {
		let Invocation::Generate(request) = parse_ok(&["keygen"]) else { panic!("expected a minting") };
		assert_eq!(request.id, "operator");
		assert_eq!(request.role, Role::Admin);
		assert_eq!(request.keys_file, None);
	}

	#[test]
	fn a_full_invocation_parses() {
		let Invocation::Generate(request) = parse_ok(&["keygen", "--id", "ingest-1", "--role", "writer", "--keys-file", "/etc/weft/keys.json"]) else { panic!("expected a minting") };
		assert_eq!(request.id, "ingest-1");
		assert_eq!(request.role, Role::Writer);
		assert_eq!(request.keys_file, Some(PathBuf::from("/etc/weft/keys.json")));
		assert_eq!(parse_ok(&["keygen", "--help"]), Invocation::Help);
	}

	#[test]
	fn bad_invocations_are_refused() {
		for arguments in [&["keygen", "--role", "superuser"][..], &["keygen", "--role", "anonymous"][..], &["keygen", "--id"][..], &["keygen", "--id", "two words"][..], &["keygen", "--bogus"][..], &["keygen", "extra"][..], &["keygn"][..]] {
			assert!(parse(arguments.iter().map(ToString::to_string)).is_err(), "{arguments:?}");
		}
	}

	/// The round trip that matters: an entry this tool appends is an
	/// entry the server loads, and the key it was minted from
	/// authenticates under the id and role the operator asked for.
	#[test]
	fn an_appended_entry_loads_and_authenticates() {
		let dir = tempfile::tempdir().expect("tempdir");
		let path = dir.path().join("keys.json");
		let request = Request { id: "ingest-1".to_string(), role: Role::Writer, keys_file: Some(path.clone()) };
		let token = "weft_a_key_the_test_chose";
		append_to_keys_file(&path, &entry_json(&request, &format_key_hash(token))).expect("append creates the file");
		let store = KeyStore::load(&path).expect("the server loads what the tool wrote");
		assert_eq!(store.authenticate(token).map(|p| (p.id, p.role)), Some(("ingest-1".to_string(), Role::Writer)));

		// A second key appends beside the first; a duplicate id does not.
		let second = Request { id: "reader-1".to_string(), role: Role::Reader, keys_file: Some(path.clone()) };
		append_to_keys_file(&path, &entry_json(&second, &format_key_hash("weft_another"))).expect("append");
		assert_eq!(KeyStore::load(&path).expect("loads").len(), 2);
		assert!(append_to_keys_file(&path, &entry_json(&request, &format_key_hash("weft_a_third"))).is_err(), "the id is taken");
	}

	#[cfg(unix)]
	#[test]
	fn a_created_keys_file_is_owner_only() {
		use std::os::unix::fs::PermissionsExt as _;
		let dir = tempfile::tempdir().expect("tempdir");
		let path = dir.path().join("keys.json");
		let request = Request { id: "operator".to_string(), role: Role::Admin, keys_file: None };
		append_to_keys_file(&path, &entry_json(&request, &format_key_hash("weft_x"))).expect("append");
		let mode = std::fs::metadata(&path).expect("metadata").permissions().mode();
		assert_eq!(mode & 0o777, 0o600, "a new keys file is 0600");
	}
}
